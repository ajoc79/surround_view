use crate::{
    gpu::{Composer, Surface, View, extent},
    media::Frame,
    native::{Source, decode_preferred},
};
use eframe::egui_wgpu::wgpu;
use std::{
    fs::OpenOptions,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
};

const SIZE: [u32; 2] = [4096, 2048];
const RATE: u32 = 30;

pub struct Job {
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    result: Receiver<Result<String, String>>,
}

impl Job {
    pub fn start(
        input: PathBuf,
        output: PathBuf,
        device: wgpu::Device,
        queue: wgpu::Queue,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU32::new(0));
        let (sender, result) = mpsc::channel();
        let worker_stop = stop.clone();
        let worker_progress = progress.clone();
        thread::spawn(move || {
            let result = convert(
                input,
                &output,
                device,
                queue,
                &worker_stop,
                &worker_progress,
            )
            .map(|()| format!("Saved {}", output.display()));
            let _ = sender.send(result);
        });
        Self {
            stop,
            progress,
            result,
        }
    }

    pub fn result(&self) -> Option<Result<String, String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("The export worker stopped".into())),
        }
    }

    pub fn progress(&self) -> f32 {
        self.progress.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn run_headless(input: PathBuf, output: PathBuf) -> Result<(), String> {
    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .map_err(|error| format!("No wgpu adapter: {error}"))?;
    let info = adapter.get_info();
    eprintln!("wgpu adapter: {} ({:?})", info.name, info.device_type);
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|error| error.to_string())?;
    let stop = AtomicBool::new(false);
    convert(input, &output, device, queue, &stop, &AtomicU32::new(0))
}

struct Readback {
    target: Surface,
    buffer: wgpu::Buffer,
}

impl Readback {
    fn new(gpu: &Composer) -> Self {
        let target = Surface::new(&gpu.device, SIZE);
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("360° export readback"),
            size: u64::from(SIZE[0]) * u64::from(SIZE[1]) * 4,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self { target, buffer }
    }

    fn write_frames(
        &self,
        gpu: &mut Composer,
        frame: &Frame,
        count: u64,
        writer: &mut impl Write,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        if count == 0 {
            return Ok(());
        }
        if stop.load(Ordering::Relaxed) {
            return Err("Export cancelled".into());
        }
        gpu.upload(frame);
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        gpu.render(&self.target, View::Panorama, &mut encoder);
        encoder.copy_texture_to_buffer(
            self.target.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(SIZE[0] * 4),
                    rows_per_image: Some(SIZE[1]),
                },
            },
            extent(SIZE),
        );
        let index = gpu.queue.submit([encoder.finish()]);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.buffer
            .map_async(wgpu::MapMode::Read, .., move |result| {
                let _ = sender.send(result);
            });
        gpu.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: None,
            })
            .map_err(|error| error.to_string())?;
        receiver
            .recv()
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let result = {
            let data = self.buffer.get_mapped_range(..);
            (0..count).try_for_each(|_| {
                if stop.load(Ordering::Relaxed) {
                    return Err("Export cancelled".to_owned());
                }
                writer
                    .write_all(&data)
                    .map_err(|e| format!("MP4 encoder pipe failed: {e}"))
            })
        };
        self.buffer.unmap();
        result
    }
}

fn convert(
    input: PathBuf,
    output: &Path,
    device: wgpu::Device,
    queue: wgpu::Queue,
    stop: &AtomicBool,
    progress: &AtomicU32,
) -> Result<(), String> {
    if output.exists() {
        return Err("Choose a new output file. The selected path already exists.".into());
    }
    let source = Source::probe(input)?;
    if device.limits().max_texture_dimension_2d < SIZE[0] {
        return Err("The wgpu adapter cannot render a 4096 × 2048 panorama".into());
    }
    let mut gpu = Composer::new(device, queue)?;
    let renderer = Readback::new(&gpu);
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = tempfile::Builder::new()
        .prefix(".surround-export-")
        .tempfile_in(parent)
        .map_err(|error| format!("Cannot stage the MP4: {error}"))?;
    let movie = staging.into_temp_path();
    let mut encoder = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostdin",
            "-y",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            &format!("{}x{}", SIZE[0], SIZE[1]),
            "-framerate",
            &RATE.to_string(),
            "-i",
            "pipe:0",
            "-an",
            "-vf",
            "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "20",
            "-colorspace",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_primaries",
            "bt709",
            "-threads",
            "4",
            "-f",
            "mp4",
        ])
        .arg(&movie)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Cannot start the FFmpeg MP4 encoder: {error}"))?;
    let mut pipe = encoder.stdin.take().expect("piped encoder stdin");
    let errors = encoder.stderr.take().expect("piped encoder stderr");
    let log = thread::spawn(move || {
        let mut message = String::new();
        let _ = errors.take(128 * 1024).read_to_string(&mut message);
        message
    });
    let end_frame = (source.duration * f64::from(RATE)).ceil() as u64;
    let mut written = 0_u64;
    let mut previous: Option<Frame> = None;
    let mut fault = None;
    let mut next = |frame: Frame| {
        if let Some(last) = previous.replace(frame) {
            let next_pts = previous.as_ref().expect("next decoded frame").pts;
            let boundary = ((next_pts * f64::from(RATE) - 1e-6).ceil() as u64)
                .min(end_frame)
                .max(written);
            if let Err(error) =
                renderer.write_frames(&mut gpu, &last, boundary - written, &mut pipe, stop)
            {
                fault = Some(error);
                return false;
            }
            written = boundary;
            progress.store((written * 990 / end_frame) as u32, Ordering::Relaxed);
        }
        !stop.load(Ordering::Relaxed)
    };
    let decoded = decode_preferred(&source, 0.0, stop, &mut next);
    let finish = (|| {
        if stop.load(Ordering::Relaxed) {
            return Err("Export cancelled".into());
        }
        if let Some(error) = fault {
            return Err(error);
        }
        if let Some(warning) = decoded? {
            eprintln!("{warning}");
        }
        let frame = previous.ok_or("No decodable video frames")?;
        renderer.write_frames(&mut gpu, &frame, end_frame - written, &mut pipe, stop)
    })();
    drop(pipe);
    if finish.is_err() {
        let _ = encoder.kill();
    }
    let status = encoder.wait().map_err(|e| e.to_string());
    let details = log.join().unwrap_or_default();
    if let Err(error) = finish {
        return Err(if details.trim().is_empty() {
            error
        } else {
            format!("{error}\n{}", details.trim())
        });
    }
    if !status?.success() {
        return Err(format!("MP4 encoding failed: {}", details.trim()));
    }
    if stop.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    inject_spherical(&movie).map_err(|e| format!("Cannot write 360° video metadata: {e}"))?;
    if stop.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    movie
        .persist_noclobber(output)
        .map_err(|e| format!("Cannot save {}: {e}", output.display()))?;
    progress.store(1000, Ordering::Relaxed);
    Ok(())
}

fn inject_spherical(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let length = file.metadata()?.len();
    let mut offset = 0;
    let mut media_data = false;
    while offset + 8 <= length {
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let mut size = u64::from(u32::from_be_bytes(
            header[..4].try_into().expect("atom size"),
        ));
        let mut header_len = 8;
        if size == 1 {
            file.read_exact(&mut header)?;
            size = u64::from_be_bytes(header);
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut header)?;
            header_len = 16;
        }
        if size < header_len || size > length - offset {
            return Err(atom_error());
        }
        if &header[4..] == b"mdat" {
            media_data = true;
        }
        if &header[4..] == b"moov" {
            // FFmpeg writes the index after the media. Chunk offsets stay valid when moov grows.
            if !media_data || offset + size != length || size > 256 * 1024 * 1024 {
                return Err(atom_error());
            }
            file.seek(SeekFrom::Start(offset + header_len))?;
            let mut movie = vec![0; (size - header_len) as usize];
            file.read_exact(&mut movie)?;
            let patched = update_atom(&movie, b"trak", |track| {
                let index = update_atom(track, b"mdia", |media| {
                    update_atom(media, b"minf", |info| {
                        update_atom(info, b"stbl", |table| {
                            update_atom(table, b"stsd", |descriptions| {
                                let prefix = descriptions.get(..8).ok_or_else(atom_error)?;
                                let items = update_atom(&descriptions[8..], b"avc1", video_entry)?;
                                Ok([prefix, &items].concat())
                            })
                        })
                    })
                })?;
                Ok([index, legacy_spherical()].concat())
            })?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&atom(b"moov", &patched))?;
            return file.sync_all();
        }
        offset += size;
    }
    Err(atom_error())
}

fn atom_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Unexpected MP4 atom layout")
}

fn atom(name: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    [
        ((payload.len() + 8) as u32).to_be_bytes().as_slice(),
        name,
        payload,
    ]
    .concat()
}

fn update_atom(
    data: &[u8],
    name: &[u8; 4],
    modify: impl FnOnce(&[u8]) -> io::Result<Vec<u8>>,
) -> io::Result<Vec<u8>> {
    let mut offset = 0;
    while let Some(head) = data.get(offset..offset + 8) {
        let size = u32::from_be_bytes(head[..4].try_into().expect("atom size")) as usize;
        if size < 8 || size > data.len() - offset {
            return Err(atom_error());
        }
        let end = offset + size;
        if &head[4..] == name {
            return Ok([
                &data[..offset],
                &atom(name, &modify(&data[offset + 8..end])?),
                &data[end..],
            ]
            .concat());
        }
        offset = end;
    }
    Err(atom_error())
}

fn video_entry(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.len() < 78 {
        return Err(atom_error());
    }
    let st3d = atom(b"st3d", &[0; 5]); // FullBox header + monoscopic mode.
    let header = atom(b"svhd", b"\0\0\0\0Surround View\0");
    let pose = atom(b"prhd", &[0; 16]);
    let bounds = atom(b"equi", &[0; 20]); // Full equirectangular image, no cropping.
    let projection = atom(b"proj", &[pose, bounds].concat());
    let spherical = atom(b"sv3d", &[header, projection].concat());
    Ok([data, &st3d, &spherical].concat())
}

fn legacy_spherical() -> Vec<u8> {
    let uuid = [
        0xff, 0xcc, 0x82, 0x63, 0xf8, 0x55, 0x4a, 0x93, 0x88, 0x14, 0x58, 0x7a, 0x02, 0x52, 0x1f,
        0xdd,
    ];
    let metadata = br#"<rdf:SphericalVideo xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:GSpherical="http://ns.google.com/videos/1.0/spherical/"><GSpherical:Spherical>true</GSpherical:Spherical><GSpherical:Stitched>true</GSpherical:Stitched><GSpherical:StitchingSoftware>Surround View</GSpherical:StitchingSoftware><GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType><GSpherical:StereoMode>mono</GSpherical:StereoMode><GSpherical:SourceCount>4</GSpherical:SourceCount></rdf:SphericalVideo>"#;
    atom(b"uuid", &[uuid.as_slice(), metadata].concat())
}
