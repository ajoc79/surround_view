#[cfg(target_arch = "wasm32")]
use crate::browser as backend;
#[cfg(not(target_arch = "wasm32"))]
use crate::{export, native as backend};
use crate::{
    gpu::{Composer, Surface, View},
    media::{Event, Frame, QUEUE_LIMIT, VideoInfo, present},
};
use eframe::{egui, egui_wgpu};
use std::{collections::VecDeque, time::Duration};

pub struct Player {
    gpu: Composer,
    state: egui_wgpu::RenderState,
    surface: Surface,
    texture: egui::TextureId,
    video: Option<backend::Video>,
    info: Option<VideoInfo>,
    frames: VecDeque<Frame>,
    has_frame: bool,
    dirty: bool,
    playing: bool,
    ended: bool,
    position: f64,
    previous_time: Option<f64>,
    scrub: Option<f64>,
    status: Option<String>,
    perspective: bool,
    yaw: f32,
    pitch: f32,
    fov: f32,
    #[cfg(not(target_arch = "wasm32"))]
    path: Option<std::path::PathBuf>,
    #[cfg(not(target_arch = "wasm32"))]
    export: Option<export::Job>,
}

impl Player {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        #[cfg(not(target_arch = "wasm32"))] initial_file: Option<std::path::PathBuf>,
    ) -> Result<Self, String> {
        let state = cc
            .wgpu_render_state
            .clone()
            .ok_or("A wgpu renderer is required")?;
        #[cfg(target_arch = "wasm32")]
        state.device.set_device_lost_callback(|reason, detail| {
            if reason != wgpu::DeviceLostReason::Destroyed {
                crate::web_error(&format!(
                    "WebGPU device lost. Reload the page or check hardware acceleration. {detail}"
                ));
            }
        });
        let gpu = Composer::new(state.device.clone(), state.queue.clone())?;
        let surface = Surface::new(&state.device, gpu.ground_size);
        let texture = state.renderer.write().register_native_texture(
            &state.device,
            &surface.view,
            egui_wgpu::wgpu::FilterMode::Linear,
        );
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(23, 26, 33);
        visuals.window_fill = visuals.panel_fill;
        visuals.selection.bg_fill = egui::Color32::from_rgb(33, 111, 119);
        cc.egui_ctx.set_visuals(visuals);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(12.0, 10.0);
        cc.egui_ctx.set_style(style);

        let player = Self {
            gpu,
            state,
            surface,
            texture,
            video: None,
            info: None,
            frames: VecDeque::new(),
            has_frame: false,
            dirty: false,
            playing: false,
            ended: false,
            position: 0.0,
            previous_time: None,
            scrub: None,
            status: None,
            perspective: false,
            yaw: 0.0,
            pitch: -0.24,
            fov: 1.1,
            #[cfg(not(target_arch = "wasm32"))]
            path: None,
            #[cfg(not(target_arch = "wasm32"))]
            export: None,
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut player = player;
            if let Some(path) = initial_file {
                player.open_native(path);
            }
            Ok(player)
        }
        #[cfg(target_arch = "wasm32")]
        Ok(player)
    }

    fn reset(&mut self) {
        self.info = None;
        self.frames.clear();
        self.has_frame = false;
        self.ended = false;
        self.playing = true;
        self.position = 0.0;
        self.previous_time = None;
        self.scrub = None;
        self.status = None;
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn open_native(&mut self, path: std::path::PathBuf) {
        self.reset();
        self.path = Some(path.clone());
        self.video = Some(backend::Video::new(path, 0.0));
    }

    fn open(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("MPEG transport stream", &["ts"])
            .pick_file()
        {
            self.open_native(path);
        }
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(video) = &mut self.video {
                video.choose_file();
            } else {
                self.video = Some(backend::Video::new());
            }
        }
    }

    fn seek(&mut self, position: f64) {
        let Some(info) = &self.info else {
            return;
        };
        if !position.is_finite() || !(0.0..info.duration).contains(&position) {
            self.status = Some("Requested time is outside the video".into());
            return;
        }
        if let Some(video) = &mut self.video {
            self.position = position;
            self.frames.clear();
            self.has_frame = false;
            self.ended = false;
            self.previous_time = None;
            self.status = None;
            video.seek(position);
        }
    }

    fn receive(&mut self, now: f64) {
        for _ in 0..QUEUE_LIMIT + 2 {
            if self.frames.len() >= QUEUE_LIMIT {
                break;
            }
            let Some(event) = self.video.as_mut().and_then(backend::Video::poll) else {
                break;
            };
            match event {
                Event::Opened(info) => self.info = Some(info),
                Event::Frame(frame) => self.frames.push_back(frame),
                Event::Warning(message) => self.status = Some(message),
                Event::Failed(error) => {
                    self.status = Some(error);
                    self.playing = false;
                    self.ended = true;
                }
                Event::End => self.ended = true,
            }
        }
        if !self.has_frame
            && let Some(frame) = self.frames.pop_front()
        {
            self.position = frame.pts;
            self.gpu.upload(&frame);
            self.has_frame = true;
            self.dirty = true;
            self.previous_time = None;
        }
        let elapsed = self
            .previous_time
            .replace(now)
            .map_or(0.0, |previous| (now - previous).max(0.0));
        if self.playing
            && self.has_frame
            && self.scrub.is_none()
            && let Some(info) = &self.info
        {
            let horizon = if self.ended {
                info.duration
            } else {
                self.frames.back().map_or(self.position, |frame| frame.pts)
            };
            self.position = (self.position + elapsed).min(horizon).min(info.duration);
            if let Some(frame) = present(&mut self.frames, self.position) {
                self.gpu.upload(&frame);
                self.dirty = true;
            }
            if self.ended && self.position >= info.duration {
                self.playing = false;
            }
        }
    }

    fn update_image(&mut self) {
        let size = if self.perspective {
            [1280, 720]
        } else {
            self.gpu.ground_size
        };
        if self.surface.size != size {
            self.surface = Surface::new(&self.state.device, size);
            self.state
                .renderer
                .write()
                .update_egui_texture_from_wgpu_texture(
                    &self.state.device,
                    &self.surface.view,
                    egui_wgpu::wgpu::FilterMode::Linear,
                    self.texture,
                );
            self.dirty = true;
        }
        if !self.dirty || !self.has_frame {
            return;
        }
        let view = if self.perspective {
            View::Perspective {
                yaw: self.yaw,
                pitch: self.pitch,
                fov: self.fov,
            }
        } else {
            View::Ground
        };
        let mut encoder = self
            .state
            .device
            .create_command_encoder(&Default::default());
        self.gpu.render(&self.surface, view, &mut encoder);
        self.state.queue.submit([encoder.finish()]);
        self.dirty = false;
    }
}

impl eframe::App for Player {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_arch = "wasm32")]
        if self
            .video
            .as_mut()
            .is_some_and(backend::Video::take_opened_file)
        {
            self.reset();
        }
        let now = ctx.input(|input| input.time);
        self.receive(now);

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(job) = &self.export
            && let Some(result) = job.result()
        {
            self.status = Some(result.unwrap_or_else(|error| error));
            self.export = None;
        }

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("Surround View");
                if ui.button("Open TS…").clicked() {
                    self.open();
                }
                ui.separator();
                self.dirty |= ui
                    .selectable_value(&mut self.perspective, false, "Top view")
                    .changed();
                self.dirty |= ui
                    .selectable_value(&mut self.perspective, true, "360° view")
                    .changed();
                #[cfg(not(target_arch = "wasm32"))]
                if ui
                    .add_enabled(
                        self.info.is_some() && self.export.is_none(),
                        egui::Button::new("Export 360° MP4…"),
                    )
                    .clicked()
                    && let (Some(source), Some(target)) = (
                        self.path.clone(),
                        rfd::FileDialog::new()
                            .add_filter("MPEG-4 video", &["mp4"])
                            .set_file_name("surround-360.mp4")
                            .save_file(),
                    )
                {
                    self.playing = false;
                    self.status = None;
                    self.export = Some(export::Job::start(
                        source,
                        target,
                        self.state.device.clone(),
                        self.state.queue.clone(),
                    ));
                }
            });
            if let Some(info) = &self.info {
                ui.label(
                    egui::RichText::new(format!(
                        "{}  ·  {} × {}",
                        info.name, info.width, info.height
                    ))
                    .weak(),
                );
            }
            ui.add_space(8.0);
        });

        egui::TopBottomPanel::bottom("playback").show(ctx, |ui| {
            ui.add_space(8.0);
            if let Some(duration) = self.info.as_ref().map(|info| info.duration) {
                ui.horizontal(|ui| {
                    let label = if self.playing { "Pause" } else { "Play" };
                    if ui.button(label).clicked()
                        || (ctx.input(|i| i.key_pressed(egui::Key::Space))
                            && !ctx.wants_keyboard_input())
                    {
                        if !self.playing && self.ended && self.position >= duration {
                            self.seek(0.0);
                        }
                        self.playing = !self.playing;
                        self.previous_time = None;
                    }
                    ui.monospace(format!(
                        "{} / {}",
                        timestamp(self.scrub.unwrap_or(self.position)),
                        timestamp(duration)
                    ));
                    let mut value = self.scrub.unwrap_or(self.position);
                    ui.spacing_mut().slider_width = (ui.available_width() - 10.0).max(60.0);
                    let slider =
                        ui.add(egui::Slider::new(&mut value, 0.0..=duration).show_value(false));
                    if slider.dragged() {
                        self.scrub = Some(value);
                    } else if slider.drag_stopped() || slider.changed() {
                        self.scrub = None;
                        self.seek(value.min((duration - 0.001).max(0.0)));
                    }
                });
            }
            if let Some(status) = &self.status {
                ui.label(status);
            }
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(job) = &self.export {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::ProgressBar::new(job.progress())
                            .desired_width(260.0)
                            .show_percentage()
                            .text("Exporting 360° MP4"),
                    );
                    if ui.button("Cancel export").clicked() {
                        job.cancel();
                    }
                });
            }
            ui.add_space(8.0);
        });

        self.update_image();
        egui::CentralPanel::default().show(ctx, |ui| {
            let area = ui.available_rect_before_wrap();
            if !self.has_frame {
                ui.centered_and_justified(|ui| {
                    if !self.ended && (self.playing || self.info.is_some()) {
                        ui.spinner();
                    } else {
                        ui.label("Open a four-camera MPEG-TS recording");
                    }
                });
                return;
            }
            let original = egui::vec2(self.surface.size[0] as f32, self.surface.size[1] as f32);
            let room = area.size() - egui::vec2(16.0, 16.0);
            let scale = (room.x / original.x).min(room.y / original.y).max(0.0);
            let rect = egui::Rect::from_center_size(area.center(), original * scale);
            let sense = if self.perspective {
                egui::Sense::drag()
            } else {
                egui::Sense::hover()
            };
            let response = ui.allocate_rect(rect, sense);
            ui.painter().image(
                self.texture,
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            if self.perspective {
                if response.dragged() {
                    let delta = ctx.input(|input| input.pointer.delta());
                    self.yaw = (self.yaw - delta.x * 0.005).rem_euclid(std::f32::consts::TAU);
                    self.pitch = (self.pitch + delta.y * 0.005).clamp(-1.48, 1.48);
                    self.dirty = true;
                }
                if response.hovered() {
                    let scroll = ctx.input(|input| input.smooth_scroll_delta.y);
                    if scroll != 0.0 {
                        self.fov = (self.fov * (-scroll * 0.002).exp()).clamp(0.4, 1.75);
                        self.dirty = true;
                    }
                }
                response.on_hover_text("Drag to look around · Scroll to zoom");
            }
        });

        if self.dirty
            || (self.video.is_some() && !self.ended && (self.playing || !self.has_frame))
            || (self.playing && self.has_frame)
        {
            ctx.request_repaint_after(Duration::from_millis(12));
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.export.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        #[cfg(target_arch = "wasm32")]
        if self.video.is_some() {
            ctx.request_repaint_after(Duration::from_millis(60));
        }
    }
}

fn timestamp(time: f64) -> String {
    let tenths = (time.max(0.0) * 10.0) as u64;
    format!(
        "{:02}:{:02}.{}",
        tenths / 600,
        tenths / 10 % 60,
        tenths % 10
    )
}
