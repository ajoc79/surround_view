use crate::media::{Event, Frame, QUEUE_LIMIT, VideoInfo, valid_dimensions};
use serde::Deserialize;
use std::{
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};

#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    start: f64,
}

#[derive(Deserialize)]
struct Probe {
    streams: Vec<Stream>,
    format: Format,
}

#[derive(Deserialize)]
struct Format {
    format_name: String,
    duration: Option<String>,
}

#[derive(Deserialize)]
struct Stream {
    codec_name: String,
    width: u32,
    height: u32,
    duration: Option<String>,
    start_time: Option<String>,
}

impl Source {
    pub fn probe(path: PathBuf) -> Result<Self, String> {
        let path = path
            .canonicalize()
            .map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
        if !path.is_file() {
            return Err("Select a local MPEG-TS video file".into());
        }
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name,width,height,duration,start_time:format=format_name,duration",
                "-of",
                "json",
            ])
            .arg(&path)
            .output()
            .map_err(|e| format!("Cannot start ffprobe. Install FFmpeg and add it to PATH. {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "Could not open the video: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let probe: Probe = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
        let stream = probe.streams.first().ok_or("No video stream")?;
        if stream.codec_name != "h264"
            || !probe.format.format_name.split(',').any(|s| s == "mpegts")
        {
            return Err("Select an H.264 MPEG-TS recording".into());
        }
        if !valid_dimensions(stream.width, stream.height) {
            return Err(format!(
                "Unsupported camera mosaic: {} × {}",
                stream.width, stream.height
            ));
        }
        let duration = stream
            .duration
            .as_deref()
            .or(probe.format.duration.as_deref())
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d > 0.0)
            .ok_or("Could not determine MPEG-TS video duration")?;
        let start = stream
            .start_time
            .as_deref()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0.0);
        Ok(Self {
            path,
            width: stream.width,
            height: stream.height,
            duration,
            start,
        })
    }

    fn info(&self) -> VideoInfo {
        VideoInfo {
            name: self
                .path
                .file_name()
                .unwrap_or(self.path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            duration: self.duration,
            width: self.width,
            height: self.height,
        }
    }
}

pub struct Video {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    events: Receiver<Event>,
}

impl Video {
    pub fn new(path: PathBuf, position: f64) -> Self {
        let (send, events) = mpsc::sync_channel(QUEUE_LIMIT);
        let stop = Arc::new(AtomicBool::new(false));
        {
            let path = path.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                if let Err(error) = Self::read(path, position, &stop, &send) {
                    send_event(&send, &stop, Event::Failed(error));
                }
            });
        }
        Self { path, stop, events }
    }

    pub fn seek(&mut self, position: f64) {
        *self = Self::new(self.path.clone(), position);
    }

    pub fn poll(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    fn read(
        path: PathBuf,
        position: f64,
        stop: &AtomicBool,
        send: &SyncSender<Event>,
    ) -> Result<(), String> {
        let source = Source::probe(path)?;
        if !send_event(send, stop, Event::Opened(source.info())) {
            return Ok(());
        }
        if let Some(warning) = decode_preferred(&source, position, stop, |frame| {
            send_event(send, stop, Event::Frame(frame))
        })? {
            send_event(send, stop, Event::Warning(warning));
        }
        send_event(send, stop, Event::End);
        Ok(())
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn send_event(send: &SyncSender<Event>, stop: &AtomicBool, mut event: Event) -> bool {
    while !stop.load(Ordering::Relaxed) {
        match send.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Full(value)) => {
                event = value;
                thread::sleep(Duration::from_millis(5));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
    false
}

enum DecodeLog {
    Timestamp(u64, f64, bool),
    Damaged,
}

fn parse_log(line: &str) -> Option<DecodeLog> {
    if line.contains("PES packet size mismatch")
        || line.contains("corrupt input packet")
        || line.contains("corrupt decoded frame")
    {
        return Some(DecodeLog::Damaged);
    }
    let (_, fields) = line.split_once("] n:")?;
    let sequence = fields.split_whitespace().next()?.parse().ok()?;
    let (_, pts) = fields.split_once("pts_time:")?;
    let pts = pts.split_whitespace().next()?.parse::<f64>().ok()?;
    let valid_size = fields
        .split_whitespace()
        .any(|field| field.starts_with("s:"));
    Some(DecodeLog::Timestamp(sequence, pts, valid_size))
}

pub fn decode_preferred(
    source: &Source,
    position: f64,
    stop: &AtomicBool,
    mut emit: impl FnMut(Frame) -> bool,
) -> Result<Option<String>, String> {
    let mut delivered = false;
    match decode(source, position, true, stop, |frame| {
        delivered = true;
        emit(frame)
    }) {
        Err(_) if !delivered && !stop.load(Ordering::Relaxed) => {
            let warning = decode(source, position, false, stop, emit)?;
            Ok(warning.or_else(|| Some("Using software H.264 decoding".into())))
        }
        result => result,
    }
}

// ffprobe exposes transport keyframes and source timestamps without decoding pixels.
fn preceding_keyframe(source: &Source, position: f64, stop: &AtomicBool) -> f64 {
    let Ok(mut probe) = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_packets",
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "csv=p=0",
        ])
        .arg(&source.path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return 0.0;
    };
    let stdout = probe.stdout.take().expect("piped ffprobe packets");
    let mut keyframe = 0.0_f64;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut fields = line.split(',');
        let Some(pts) = fields.next().and_then(|field| field.parse::<f64>().ok()) else {
            continue;
        };
        if fields.next().is_some_and(|flags| flags.starts_with('K')) {
            let time = pts - source.start;
            if time > position {
                break;
            }
            keyframe = keyframe.max(time);
        }
    }
    let _ = probe.kill();
    let _ = probe.wait();
    (keyframe - 0.005).max(0.0)
}

pub fn decode(
    source: &Source,
    position: f64,
    hardware: bool,
    stop: &AtomicBool,
    mut emit: impl FnMut(Frame) -> bool,
) -> Result<Option<String>, String> {
    if !position.is_finite() || !(0.0..source.duration).contains(&position) {
        return Err("Requested time is outside the video".into());
    }
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "info",
        "-copyts",
        "-fflags",
        "+discardcorrupt",
        "-threads",
        "2",
    ]);
    if hardware {
        command.args(["-hwaccel", "auto"]);
    }
    if position > 0.0 {
        let origin = preceding_keyframe(source, position, stop);
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if origin > 0.0 {
            command.args(["-ss", &format!("{origin:.9}")]);
        }
    }
    command.arg("-i").arg(&source.path).args([
        "-map",
        "0:v:0",
        "-an",
        "-noautoscale",
        "-filter_threads",
        "2",
        "-vf",
        "format=rgba,showinfo=checksum=0",
        "-fps_mode",
        "passthrough",
        "-threads",
        "1",
        "-pix_fmt",
        "rgba",
        "-f",
        "rawvideo",
        "pipe:1",
    ]);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Cannot start FFmpeg: {error}"))?;
    let (log_tx, logs) = mpsc::channel();
    let stderr = child.stderr.take().expect("piped FFmpeg stderr");
    let expected_size = format!("s:{}x{}", source.width, source.height);
    let log_worker = thread::spawn(move || {
        let mut tail = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(mut event) = parse_log(&line) {
                if let DecodeLog::Timestamp(_, _, valid_size) = &mut event {
                    *valid_size = line.split_whitespace().any(|field| field == expected_size);
                }
                if log_tx.send(event).is_err() {
                    break;
                }
            }
            if !line.starts_with("[Parsed_showinfo") && !line.starts_with("frame=") {
                tail = line;
            }
        }
        tail
    });
    let mut stdout = child.stdout.take().expect("piped FFmpeg stdout");
    let frame_bytes = (source.width * source.height * 4) as usize;
    let mut count = 0;
    let mut damaged = false;
    let mut last = f64::NEG_INFINITY;
    let mut interrupted = false;
    let mut before_target = None;
    let mut delivered = false;
    let mut fault = None;

    // Consume each RGBA frame together with the corresponding showinfo timestamp.
    while !stop.load(Ordering::Relaxed) {
        let mut rgba = vec![0; frame_bytes];
        match stdout.read(&mut rgba[..1]) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                fault = Some(error.to_string());
                break;
            }
        }
        if let Err(error) = stdout.read_exact(&mut rgba[1..]) {
            fault = Some(format!("Truncated decoded frame: {error}"));
            break;
        }
        let (sequence, timestamp, dimensions_match) = loop {
            match logs.recv() {
                Ok(DecodeLog::Timestamp(sequence, pts, size)) => break (sequence, pts, size),
                Ok(DecodeLog::Damaged) => damaged = true,
                Err(_) => break (u64::MAX, 0.0, false),
            }
        };
        if sequence != count || !dimensions_match {
            fault = Some("FFmpeg video resolution changed or decoded timestamps were lost".into());
            break;
        }
        count += 1;
        let pts = (timestamp - source.start).max(0.0);
        if !timestamp.is_finite() || pts < last {
            damaged = true;
            continue;
        }
        last = pts;
        let frame = Frame {
            pts,
            width: source.width,
            height: source.height,
            rgba,
        };
        if pts + 0.001 < position {
            before_target = Some(frame);
            continue;
        }
        before_target = None;
        delivered = true;
        if !emit(frame) {
            interrupted = true;
            break;
        }
    }
    drop(stdout);
    if fault.is_some() || interrupted || stop.load(Ordering::Relaxed) {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|error| error.to_string());
    let tail = log_worker.join().unwrap_or_default();
    damaged |= logs
        .try_iter()
        .any(|line| matches!(line, DecodeLog::Damaged));
    if stop.load(Ordering::Relaxed) || interrupted {
        return Ok(None);
    }
    if let Some(error) = fault {
        return Err(error);
    }
    if !status?.success() || !last.is_finite() {
        return Err(format!("FFmpeg did not complete video decoding. {tail}"));
    }
    if !delivered && let Some(mut frame) = before_target {
        frame.pts = position;
        emit(frame);
    }
    Ok(damaged.then(|| "The recording contains damaged or discontinuous video packets.".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires FFmpeg and SURROUND_TEST_TS pointing to the recorder sample"]
    fn recorder_seek_and_bounds() {
        let path = std::env::var_os("SURROUND_TEST_TS").expect("SURROUND_TEST_TS");
        let source = Source::probe(path.into()).unwrap();
        let stop = AtomicBool::new(false);
        let mut found = Vec::new();
        decode(&source, 5.0, false, &stop, |frame| {
            found.push(frame.pts);
            found.len() < 3
        })
        .unwrap();
        assert_eq!(found.len(), 3);
        assert!((5.0..5.10).contains(&found[0]), "{found:?}");
        assert!(found.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            decode(&source, 60.0, false, &stop, |_| panic!(
                "out-of-range seek emitted a frame"
            ))
            .is_err()
        );
    }
}
