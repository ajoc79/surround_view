use crate::media::Frame;
use serde::Deserialize;
use wgpu::util::DeviceExt;

#[derive(Deserialize)]
struct Camera {
    tile: [u32; 2],
    lens_parameters: Lens,
    ground_homography: [[f32; 3]; 3],
    blend_camera_center_metres: [f32; 3],
    outward_normal: [f32; 2],
}

#[derive(Deserialize)]
struct Lens {
    fx: f32,
    fy: f32,
    cx: f32,
    cy: f32,
    xi: f32,
    k1: f32,
    k2: f32,
    p1: f32,
    p2: f32,
}

impl Lens {
    fn radial_limit_squared(&self) -> f32 {
        let a = 5.0 * self.k2;
        let b = 3.0 * self.k1;
        if a == 0.0 {
            return if b < 0.0 { -1.0 / b } else { f32::MAX };
        }
        let root = (b * b - 4.0 * a).sqrt();
        [(-b - root) / (2.0 * a), (-b + root) / (2.0 * a)]
            .into_iter()
            .filter(|r| *r > 0.0)
            .fold(f32::MAX, f32::min)
    }
}

#[derive(Deserialize)]
struct Vehicle {
    width_m: f32,
    length_m: f32,
}

#[derive(Deserialize)]
struct Calibration {
    schema_version: u32,
    native_camera_size: [u32; 2],
    vehicle: Vehicle,
    lens_model: String,
    cameras: [Camera; 4],
}

#[derive(Deserialize)]
struct Settings {
    output_width: u32,
    output_height: u32,
    ground_x_half_extent_m: f32,
    ground_y_half_extent_m: f32,
    blend_cosine_power: f32,
    blend_edge_fade_px: f32,
    minimum_brightness: f32,
    weight_epsilon: f32,
    native_border_px: f32,
    background_rgb: [u8; 3],
    vehicle_fill_rgb: [u8; 3],
}

pub enum View {
    Ground,
    Perspective {
        yaw: f32,
        pitch: f32,
        fov: f32,
    },
    #[cfg(not(target_arch = "wasm32"))]
    Panorama,
}

pub struct Surface {
    #[cfg(not(target_arch = "wasm32"))]
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub size: [u32; 2],
}

impl Surface {
    pub fn new(device: &wgpu::Device, size: [u32; 2]) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Surround render target"),
            size: extent(size),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            texture,
            view,
            size,
        }
    }
}

pub fn extent([width, height]: [u32; 2]) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

pub struct Composer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub ground_size: [u32; 2],
    pipeline: wgpu::RenderPipeline,
    settings: wgpu::Buffer,
    view: wgpu::Buffer,
    sampler: wgpu::Sampler,
    source: Option<(wgpu::Texture, wgpu::BindGroup)>,
}

impl Composer {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Result<Self, String> {
        let calibration: Calibration =
            serde_json::from_str(include_str!("../config/calibration.json"))
                .map_err(|error| format!("Invalid calibration: {error}"))?;
        let settings: Settings = serde_json::from_str(include_str!("../config/render.json"))
            .map_err(|error| format!("Invalid render settings: {error}"))?;
        if calibration.schema_version != 1
            || calibration.lens_model != "unified_sphere_radial"
            || calibration.native_camera_size.contains(&0)
            || calibration.cameras.iter().enumerate().any(|(i, cam)| {
                cam.tile != [(i % 2) as u32, (i / 2) as u32]
                    || cam.lens_parameters.p1 != 0.0
                    || cam.lens_parameters.p2 != 0.0
            })
        {
            return Err("Unsupported surround-view calibration".into());
        }
        let [nw, nh] = calibration.native_camera_size.map(|v| v as f32);
        let mut data = vec![
            [
                settings.ground_x_half_extent_m,
                settings.ground_y_half_extent_m,
                calibration.vehicle.width_m * 0.5,
                calibration.vehicle.length_m * 0.5,
            ],
            [
                nw,
                nh,
                settings.native_border_px,
                settings.minimum_brightness / 255.0,
            ],
            [
                settings.blend_cosine_power,
                settings.blend_edge_fade_px,
                settings.weight_epsilon,
                0.0,
            ],
            color(settings.background_rgb),
            color(settings.vehicle_fill_rgb),
        ];
        for cam in calibration.cameras {
            let l = cam.lens_parameters;
            data.push([l.fx, l.fy, l.cx, l.cy]);
            data.push([l.xi, l.k1, l.k2, l.radial_limit_squared()]);
            for [x, y, z] in cam.ground_homography {
                data.push([x, y, z, 0.0]);
            }
            data.push([
                cam.blend_camera_center_metres[0],
                cam.blend_camera_center_metres[1],
                cam.outward_normal[0],
                cam.outward_normal[1],
            ]);
        }
        let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Calibration and render settings"),
            contents: bytemuck::cast_slice(&data),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let view = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Current projection and orientation"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shader = device.create_shader_module(wgpu::include_wgsl!("surround.wgsl"));
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Surround-view composition"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        Ok(Self {
            device,
            queue,
            ground_size: [settings.output_width, settings.output_height],
            pipeline,
            settings: params,
            view,
            sampler,
            source: None,
        })
    }

    pub fn upload(&mut self, frame: &Frame) {
        let size = extent([frame.width, frame.height]);
        if self
            .source
            .as_ref()
            .is_none_or(|(texture, _)| texture.size() != size)
        {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Decoded camera mosaic"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bindings = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Surround-view inputs"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.settings.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.view.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.source = Some((texture, bindings));
        }
        if let Some((source, _)) = &self.source {
            self.queue.write_texture(
                source.as_image_copy(),
                &frame.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame.width * 4),
                    rows_per_image: Some(frame.height),
                },
                size,
            );
        }
    }

    pub fn render(&self, target: &Surface, projection: View, encoder: &mut wgpu::CommandEncoder) {
        let [width, height] = target.size.map(|v| v as f32);
        let settings = match projection {
            View::Ground => [0.0, 0.0, 0.0, 0.0],
            View::Perspective { yaw, pitch, fov } => [1.0, yaw, pitch, fov],
            #[cfg(not(target_arch = "wasm32"))]
            View::Panorama => [2.0, 0.0, 0.0, 0.0],
        };
        let data = [settings, [width, height, 0.0, 0.0]];
        self.queue
            .write_buffer(&self.view, 0, bytemuck::cast_slice(&data));
        let mut render = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Render surround view"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        if let Some((_, bindings)) = &self.source {
            render.set_pipeline(&self.pipeline);
            render.set_bind_group(0, bindings, &[]);
            render.draw(0..3, 0..1);
        }
    }
}

fn color([r, g, b]: [u8; 3]) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    #[test]
    #[ignore = "requires a wgpu adapter and Python with the reference requirements"]
    fn ground_projection_matches_python() {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        let mut gpu = Composer::new(device, queue).unwrap();
        let [width, height] = [640, 360];
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend([
                    48 + (x * 3 % 134) as u8,
                    65 + (y * 5 % 130) as u8,
                    40 + ((x + y) % 200) as u8,
                    255,
                ]);
            }
        }
        let staging = tempfile::tempdir().unwrap();
        let input = staging.path().join("source.rgba");
        let expected = staging.path().join("expected.rgb");
        fs::write(&input, &rgba).unwrap();
        let python = std::env::var_os("SURROUND_REFERENCE_PYTHON").unwrap_or("python3".into());
        let status = Command::new(python)
            .args([
                "-B",
                "-c",
                r#"
import json, sys
from pathlib import Path
import numpy as np
from surround_view.geometry import compose_frame
load = lambda name: json.loads(Path(f'config/{name}.json').read_text())
frame = np.fromfile(sys.argv[1], dtype=np.uint8).reshape(360, 640, 4)
ground, _ = compose_frame(frame[:, :, :3], load('calibration'), load('render'))
ground.tofile(sys.argv[2])
"#,
            ])
            .arg(input)
            .arg(&expected)
            .status()
            .unwrap();
        assert!(status.success());

        gpu.upload(&Frame {
            pts: 0.0,
            width,
            height,
            rgba,
        });
        let surface = Surface::new(&gpu.device, gpu.ground_size);
        let [w, h] = surface.size;
        let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let copied = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(row) * u64::from(h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        gpu.render(&surface, View::Ground, &mut encoder);
        encoder.copy_texture_to_buffer(
            surface.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &copied,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            extent(surface.size),
        );
        gpu.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        copied.map_async(wgpu::MapMode::Read, .., move |result| {
            tx.send(result).unwrap()
        });
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        rx.recv().unwrap().unwrap();
        let received = copied.get_mapped_range(..);
        let expected = fs::read(expected).unwrap();
        assert_eq!(expected.len(), (w * h * 3) as usize);
        let differences = received
            .chunks_exact(row as usize)
            .flat_map(|line| line[..(w * 4) as usize].chunks_exact(4))
            .zip(expected.chunks_exact(3))
            .filter(|(actual, expected)| (0..3).any(|i| actual[i].abs_diff(expected[i]) > 2))
            .count();
        assert!(
            differences < (w * h / 100) as usize,
            "{differences} pixels differ by more than two levels"
        );
        eprintln!(
            "{differences} / {} pixels differ by more than two levels",
            w * h
        );
    }
}
