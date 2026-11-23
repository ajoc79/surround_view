//! Packetized Annex B access units from the H.264 program in a 188-byte MPEG-TS file.
use std::collections::VecDeque;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

const PACKET_SIZE: usize = 188;
const MAX_PES: usize = 16 * 1024 * 1024;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct AccessUnit {
    data: Vec<u8>,
    pub pts: f64,
    pub offset: f64,
    pub key: bool,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl AccessUnit {
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.data)
    }
}

#[derive(Default)]
struct Section(Vec<u8>);

impl Section {
    fn push(&mut self, payload: &[u8], start: bool) -> Option<&[u8]> {
        let payload = if start {
            let pointer = *payload.first()? as usize;
            self.0.clear();
            payload.get(pointer + 1..)?
        } else if self.0.is_empty() {
            return None;
        } else {
            payload
        };
        if self.0.len() + payload.len() > 4096 {
            self.0.clear();
            return None;
        }
        self.0.extend_from_slice(payload);
        let header = self.0.get(..3)?;
        let size = (usize::from(header[1] & 15) << 8) | usize::from(header[2]);
        self.0.get(..3 + size)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct Demux {
    pmt: Option<u16>,
    video: Option<u16>,
    pat: Section,
    program: Section,
    pes: Vec<u8>,
    pes_offset: f64,
    continuity: Option<u8>,
    pending: VecDeque<AccessUnit>,
    problems: u32,
    codec: String,
    parameters: [Vec<u8>; 2],
    timestamp: Option<i64>,
    earliest_pts: Option<i64>,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl Demux {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new() -> Self {
        Self {
            pmt: None,
            video: None,
            pat: Section::default(),
            program: Section::default(),
            pes: Vec::new(),
            pes_offset: 0.0,
            continuity: None,
            pending: VecDeque::new(),
            problems: 0,
            codec: String::new(),
            parameters: Default::default(),
            timestamp: None,
            earliest_pts: None,
        }
    }

    pub fn push(&mut self, bytes: &[u8], offset: f64) {
        if !bytes.len().is_multiple_of(PACKET_SIZE) {
            self.problems += 1;
        }
        for (i, packet) in bytes.chunks_exact(PACKET_SIZE).enumerate() {
            self.packet(packet, offset + (i * PACKET_SIZE) as f64);
        }
    }

    pub fn next(&mut self) -> Option<AccessUnit> {
        self.pending.pop_front()
    }

    pub fn flush(&mut self) {
        self.finish_pes();
    }

    pub fn start(&self) -> f64 {
        self.earliest_pts
            .map_or(f64::NAN, |value| value as f64 / 90_000.0)
    }

    pub fn codec(&self) -> String {
        self.codec.clone()
    }

    pub fn problems(&mut self) -> u32 {
        std::mem::take(&mut self.problems)
    }

    pub fn seek(&mut self, reference: f64) {
        self.pes.clear();
        self.pending.clear();
        self.continuity = None;
        self.problems = 0;
        self.timestamp = Some((reference * 90_000.0).round() as i64);
    }
}

impl Demux {
    fn packet(&mut self, packet: &[u8], offset: f64) {
        if packet[0] != 0x47 {
            self.problems += 1;
            self.pes.clear();
            self.continuity = None;
            return;
        }
        let pid = (u16::from(packet[1] & 31) << 8) | u16::from(packet[2]);
        let start = packet[1] & 0x40 != 0;
        let control = packet[3] >> 4;
        if control & 1 == 0 {
            return;
        }
        let payload_offset = if control & 2 != 0 {
            5 + usize::from(packet[4])
        } else {
            4
        };
        let Some(payload) = packet.get(payload_offset..).filter(|p| !p.is_empty()) else {
            return;
        };
        if pid == 0 {
            if let Some(section) = self.pat.push(payload, start)
                && section.first() == Some(&0)
                && let Some(program) = section
                    .get(8..section.len().saturating_sub(4))
                    .and_then(|data| data.chunks_exact(4).find(|item| item[..2] != [0, 0]))
            {
                self.pmt = Some((u16::from(program[2] & 31) << 8) | u16::from(program[3]));
            }
            return;
        }
        if Some(pid) == self.pmt && self.video.is_none() {
            if let Some(section) = self.program.push(payload, start)
                && section.first() == Some(&2)
                && section.len() >= 16
            {
                let program_length =
                    (usize::from(section[10] & 15) << 8) | usize::from(section[11]);
                let mut cursor = 12 + program_length;
                while cursor + 5 <= section.len() - 4 {
                    let stream = &section[cursor..cursor + 5];
                    if stream[0] == 0x1b {
                        self.video = Some((u16::from(stream[1] & 31) << 8) | u16::from(stream[2]));
                        break;
                    }
                    cursor += 5 + ((usize::from(stream[3] & 15) << 8) | usize::from(stream[4]));
                }
            }
            return;
        }
        if Some(pid) != self.video {
            return;
        }
        let continuity = packet[3] & 15;
        if self.continuity == Some(continuity) {
            return;
        }
        if packet[1] & 0x80 != 0 || control & 0x0c != 0 {
            self.problems += 1;
            self.pes.clear();
            self.continuity = None;
            return;
        }
        if let Some(previous) = self.continuity
            && continuity != (previous + 1) % 16
        {
            self.problems += 1;
            self.pes.clear();
        }
        self.continuity = Some(continuity);
        if start {
            self.finish_pes();
            self.pes_offset = offset;
        }
        if start || !self.pes.is_empty() {
            if self.pes.len() + payload.len() > MAX_PES {
                self.problems += 1;
                self.pes.clear();
            } else {
                self.pes.extend_from_slice(payload);
            }
        }
    }

    fn finish_pes(&mut self) {
        let packet = std::mem::take(&mut self.pes);
        if packet.is_empty() {
            return;
        }
        match self.access_unit(&packet) {
            Ok(Some(unit)) => self.pending.push_back(unit),
            Ok(None) => {}
            Err(()) => self.problems += 1,
        }
    }

    fn access_unit(&mut self, packet: &[u8]) -> Result<Option<AccessUnit>, ()> {
        if packet.len() < 14 || packet[..3] != [0, 0, 1] || packet[7] & 0x80 == 0 || packet[8] < 5 {
            return Err(());
        }
        let declared = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
        let end = if declared == 0 {
            packet.len()
        } else {
            6 + declared
        };
        let data = packet.get(9 + usize::from(packet[8])..end).ok_or(())?;
        let raw = decode_pts(&packet[9..14]).ok_or(())?;
        let ticks = if let Some(last) = self.timestamp {
            let wrap = 1_i64 << 33;
            let delta = (raw - last + wrap / 2).rem_euclid(wrap) - wrap / 2;
            last + delta
        } else {
            raw
        };
        self.timestamp = Some(ticks);
        self.earliest_pts = Some(
            self.earliest_pts
                .map_or(ticks, |previous| previous.min(ticks)),
        );
        let mut key = false;
        let mut picture = false;
        let mut parameter_present = [false; 2];
        for nal in annex_b(data) {
            match nal[0] & 31 {
                1..=4 => picture = true,
                5 => {
                    picture = true;
                    key = true;
                }
                7 | 8 => {
                    let i = usize::from((nal[0] & 31) - 7);
                    if i == 0 && nal.len() >= 4 {
                        self.codec = format!("avc1.{:02X}{:02X}{:02X}", nal[1], nal[2], nal[3]);
                    }
                    self.parameters[i] = nal.to_vec();
                    parameter_present[i] = true;
                }
                _ => {}
            }
        }
        if !picture {
            return Ok(None);
        }
        let mut chunk = Vec::with_capacity(data.len() + 64);
        if key && !parameter_present.into_iter().all(|present| present) {
            for parameter in &self.parameters {
                if !parameter.is_empty() {
                    chunk.extend_from_slice(&[0, 0, 0, 1]);
                    chunk.extend_from_slice(parameter);
                }
            }
        }
        chunk.extend_from_slice(data);
        Ok(Some(AccessUnit {
            data: chunk,
            pts: ticks as f64 / 90_000.0,
            offset: self.pes_offset,
            key,
        }))
    }
}

fn decode_pts(bytes: &[u8]) -> Option<i64> {
    if bytes.len() != 5 || bytes[0] & bytes[2] & bytes[4] & 1 != 1 {
        return None;
    }
    Some(
        (i64::from(bytes[0] & 0x0e) << 29)
            | (i64::from(bytes[1]) << 22)
            | (i64::from(bytes[2] & 0xfe) << 14)
            | (i64::from(bytes[3]) << 7)
            | i64::from(bytes[4] >> 1),
    )
}

fn annex_b(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        loop {
            let start = data.windows(3).position(|bytes| bytes == [0, 0, 1])? + 3;
            let bytes = &data[start..];
            let end = bytes
                .windows(3)
                .position(|bytes| bytes == [0, 0, 1])
                .unwrap_or(bytes.len());
            let nal = &bytes[..end];
            // Leave the next start code for the following iteration.
            data = &bytes[end..];
            let end = nal
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |index| index + 1);
            if end != 0 {
                return Some(&nal[..end]);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_handles_three_and_four_byte_prefixes() {
        let units: Vec<_> = annex_b(&[0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65, 0x80, 0]).collect();
        assert_eq!(units, [vec![0x67, 0x42], vec![0x65, 0x80]]);
    }

    #[test]
    fn preserves_mpeg_timestamp_precision() {
        assert_eq!(decode_pts(&[0x21, 0, 1, 0, 1]), Some(0));
        assert_eq!(decode_pts(&[0x21, 0, 1, 0x9e, 0xfd]), Some(20_350));
        assert_eq!(decode_pts(&[0x21, 0, 0, 0, 1]), None);
    }
}

#[cfg(test)]
mod recording_tests {
    use super::*;
    use std::{
        fs::File,
        io::{BufReader, Read},
    };

    #[test]
    #[ignore = "requires SURROUND_TEST_TS pointing to the recorder sample"]
    fn recorder_h264_and_seek() {
        let path = std::env::var_os("SURROUND_TEST_TS").expect("SURROUND_TEST_TS");
        let mut reader = BufReader::new(File::open(path).unwrap());
        let mut demux = Demux::new();
        let mut packet = [0; PACKET_SIZE];
        let mut offset = 0.0;
        let mut count = 0;
        let mut keyframes = Vec::new();
        while reader.read_exact(&mut packet).is_ok() {
            demux.push(&packet, offset);
            offset += PACKET_SIZE as f64;
            while let Some(mut unit) = demux.next() {
                if unit.key {
                    keyframes.push((unit.pts, unit.offset));
                }
                assert!(!unit.take().is_empty());
                count += 1;
            }
        }
        demux.flush();
        while demux.next().is_some() {
            count += 1;
        }
        assert_eq!(demux.codec(), "avc1.42C029");
        assert_eq!(demux.start(), 0.0);
        assert!((440..=442).contains(&count), "decoded {count} access units");
        assert!(
            keyframes
                .iter()
                .any(|(time, _)| (time - 4.400822).abs() < 0.001)
        );
        assert!(demux.problems() > 0);

        use std::io::{Seek, SeekFrom};
        let (pts, offset) = keyframes
            .iter()
            .rev()
            .find(|(pts, _)| *pts <= 5.0)
            .copied()
            .unwrap();
        demux.seek(pts);
        reader.seek(SeekFrom::Start(offset as u64)).unwrap();
        let mut read_offset = offset;
        let first = loop {
            reader.read_exact(&mut packet).unwrap();
            demux.push(&packet, read_offset);
            read_offset += PACKET_SIZE as f64;
            if let Some(unit) = demux.next() {
                break unit;
            }
        };
        assert!(first.key);
        assert_eq!(first.pts, pts);
    }
}
