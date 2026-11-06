//! Player for the boot recordings the iDRAC6 keeps and for console
//! recordings saved by idrac6-rs (same format).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use winit::dpi::LogicalSize;
use winit::event_loop::EventLoopProxy;
use winit::window::{Window, WindowAttributes, WindowId};

use super::mp4::{Choice, FfmpegDialog};
use super::{Shell, UserEvent, WindowUi};
use crate::config::Instance;
use crate::video::Framebuffer;
use crate::video::mp4::{self as video_mp4, Done, Export, Ffmpeg};
use crate::video::playback::{self, Player};
use crate::web::{BootCapture, WebSession};

enum State {
    Listing(Receiver<anyhow::Result<Vec<BootCapture>>>),
    Loading(Vec<BootCapture>, Receiver<anyhow::Result<Vec<u8>>>),
    Playing(Vec<BootCapture>),
    Failed(String),
}

pub struct PlaybackWindow {
    /// Boot captures of this iDRAC, or
    inst: Option<Instance>,
    /// a recording file.
    file: Option<PathBuf>,
    state: State,
    selected: usize,
    player: Option<Player>,
    fb: Framebuffer,
    texture: Option<egui::TextureHandle>,
    dirty: bool,
    playing: bool,
    /// Playback position.
    pos: Duration,
    last_tick: Option<Instant>,
    speed: f32,
    window_id: Option<WindowId>,
    proxy: Option<EventLoopProxy<UserEvent>>,
    message: Option<String>,
    pending_load: Option<usize>,
    /// Export MP4 in progress.
    export: Option<Export>,
    ffmpeg_dialog: Option<FfmpegDialog>,
    exported: Option<Done>,
}

impl PlaybackWindow {
    pub fn new(inst: Instance) -> Self {
        Self::make(Some(inst), None)
    }

    /// Play a recording file (a console recording or a downloaded boot
    /// capture).
    pub fn from_file(path: PathBuf) -> Self {
        Self::make(None, Some(path))
    }

    fn make(inst: Option<Instance>, file: Option<PathBuf>) -> Self {
        Self {
            inst,
            file,
            state: State::Failed(String::new()),
            selected: 0,
            player: None,
            fb: Framebuffer::default(),
            texture: None,
            dirty: false,
            playing: false,
            pos: Duration::ZERO,
            last_tick: None,
            speed: 1.0,
            window_id: None,
            proxy: None,
            message: None,
            pending_load: None,
            export: None,
            ffmpeg_dialog: None,
            exported: None,
        }
    }

    fn notifier(&self) -> Arc<dyn Fn() + Send + Sync> {
        let (p, id) = (self.proxy.clone().expect("proxy"), self.window_id.expect("window"));
        Arc::new(move || {
            let _ = p.send_event(UserEvent::Repaint(id, Duration::ZERO));
        })
    }

    /// Log in, run `f` with the web session, log out.
    fn with_web<T: Send + 'static>(&self, f: impl FnOnce(&WebSession) -> anyhow::Result<T> + Send + 'static) -> Receiver<anyhow::Result<T>> {
        let (tx, rx) = mpsc::channel();
        let inst = self.inst.clone().expect("instance");
        let notify = self.notifier();
        thread::spawn(move || {
            let r = WebSession::login(&inst.host, inst.https_port, &inst.username, &inst.password).and_then(|w| {
                let r = f(&w);
                let _ = w.logout();
                r
            });
            let _ = tx.send(r);
            notify();
        });
        rx
    }

    fn load_file(&mut self, path: PathBuf) {
        let (tx, rx) = mpsc::channel();
        let notify = self.notifier();
        let p = path.clone();
        thread::spawn(move || {
            let _ = tx.send(std::fs::read(&p).map_err(|e| anyhow::anyhow!("Could not read {}: {e}", p.display())));
            notify();
        });
        self.file = Some(path);
        self.player = None;
        self.texture = None;
        self.state = State::Loading(Vec::new(), rx);
    }

    fn open_dialog(&mut self) {
        let mut d = rfd::FileDialog::new().add_filter("Recordings", &["dvcrec"]).add_filter("All files", &["*"]);
        if let Some(dir) = self.file.as_ref().and_then(|f| f.parent()) {
            d = d.set_directory(dir);
        }
        if let Some(p) = d.pick_file() {
            self.load_file(p);
        }
    }

    fn title(&self) -> String {
        match (&self.inst, &self.file) {
            (Some(i), _) => format!("{} - Boot Capture", i.title()),
            (None, Some(f)) => format!("{} - Recording", f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            (None, None) => "Recording".into(),
        }
    }

    fn list(&mut self) {
        self.state = State::Listing(self.with_web(|w| w.boot_captures()));
    }

    fn load(&mut self, list: Vec<BootCapture>, idx: usize) {
        let c = list[idx].clone();
        self.selected = idx;
        self.player = None;
        self.texture = None;
        self.state = State::Loading(list, self.with_web(move |w| w.download_boot_capture(&c)));
    }

    fn poll(&mut self) {
        match std::mem::replace(&mut self.state, State::Failed(String::new())) {
            State::Listing(rx) => match rx.try_recv() {
                Ok(Ok(list)) if list.is_empty() => self.state = State::Failed("This iDRAC has no boot recordings yet.".into()),
                Ok(Ok(list)) => self.load(list, 0),
                Ok(Err(e)) => self.state = State::Failed(format!("{e:#}")),
                Err(_) => self.state = State::Listing(rx),
            },
            State::Loading(list, rx) => match rx.try_recv() {
                Ok(Ok(data)) => match playback::parse(&data) {
                    Ok(rec) => {
                        self.player = Some(Player::new(rec));
                        self.fb = Framebuffer::default();
                        self.pos = Duration::ZERO;
                        self.playing = true;
                        self.last_tick = None;
                        self.state = State::Playing(list);
                    }
                    Err(e) => self.state = State::Failed(format!("Could not read the recording: {e:#}")),
                },
                Ok(Err(e)) => self.state = State::Failed(format!("{e:#}")),
                Err(_) => self.state = State::Loading(list, rx),
            },
            other => self.state = other,
        }
    }

    /// Advance the clock and decode what is due.
    fn tick(&mut self, ctx: &egui::Context) {
        let Some(p) = self.player.as_mut() else { return };
        let now = Instant::now();
        if self.playing {
            if let Some(last) = self.last_tick {
                self.pos += (now - last).mul_f32(self.speed);
            }
            self.last_tick = Some(now);
            match p.advance_to(self.pos, &mut self.fb) {
                Ok(changed) => self.dirty |= changed,
                Err(e) => log::warn!("playback decode: {e:#}"),
            }
            if p.finished() {
                self.playing = false;
            } else if let Some(next) = p.next_at() {
                let wait = next.saturating_sub(self.pos).div_f32(self.speed);
                ctx.request_repaint_after(wait.min(Duration::from_millis(100)));
            }
        } else {
            self.last_tick = None;
        }
    }

    fn seek(&mut self, t: Duration) {
        let Some(p) = self.player.as_mut() else { return };
        if t < self.pos {
            p.rewind(&mut self.fb);
        }
        let _ = p.advance_to(t, &mut self.fb);
        self.pos = t;
        self.dirty = true;
    }

    fn update_texture(&mut self, ctx: &egui::Context) {
        if !self.dirty || self.fb.width == 0 {
            return;
        }
        self.dirty = false;
        let px = self.fb.pixels.iter().map(|&p| egui::Color32::from_rgb((p >> 16) as u8, (p >> 8) as u8, p as u8)).collect();
        let img = egui::ColorImage::new([self.fb.width, self.fb.height], px);
        match &mut self.texture {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("playback", img, egui::TextureOptions::LINEAR)),
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let State::Playing(list) = &self.state else { return };
        let list = list.clone();
        let Some(p) = self.player.as_ref() else { return };
        let total = p.recording.duration();
        let finished = p.finished();
        let frame = p.frames_shown();
        ui.horizontal(|ui| {
            let label = if self.playing { "Pause" } else if finished { "Replay" } else { "Play" };
            if ui.button(label).clicked() {
                if finished {
                    self.seek(Duration::ZERO);
                }
                self.playing = !self.playing || finished;
            }
            if ui.add_enabled(!finished, egui::Button::new("Step")).clicked() {
                self.playing = false;
                if let Some(p) = self.player.as_mut() {
                    let _ = p.step(&mut self.fb);
                    self.pos = p.next_at().map(|t| t.saturating_sub(Duration::from_millis(1))).unwrap_or(total);
                }
                self.dirty = true;
            }
            if ui.button("Rewind").clicked() {
                self.playing = false;
                self.seek(Duration::ZERO);
            }
            ui.separator();
            egui::ComboBox::from_id_salt("speed").selected_text(format!("{}×", self.speed)).width(60.0).show_ui(ui, |ui| {
                for s in [0.5f32, 1.0, 2.0, 4.0, 8.0] {
                    ui.selectable_value(&mut self.speed, s, format!("{s}×"));
                }
            });
            ui.separator();
            ui.label(format!("Frame: {frame}"));
            ui.label(format!("{} / {}", mmss(self.pos.min(total)), mmss(total)));
            ui.separator();
            if list.is_empty() {
                if ui.button("Open…").on_hover_text("Open another recording file").clicked() {
                    self.open_dialog();
                }
            } else {
                egui::ComboBox::from_id_salt("rec").selected_text(rec_label(&list[self.selected], self.selected)).show_ui(ui, |ui| {
                    for (i, c) in list.iter().enumerate() {
                        if ui.selectable_label(i == self.selected, rec_label(c, i)).clicked() && i != self.selected {
                            self.pending_load = Some(i);
                        }
                    }
                });
            }
            if ui.button("Save screenshot…").clicked() {
                self.screenshot();
            }
            if ui.button("Copy").on_hover_text("Copy the current picture to the clipboard").clicked() {
                if let Err(e) = super::clipboard::copy_image(self.fb.width, self.fb.height, &self.fb.pixels) {
                    self.message = Some(format!("Could not copy the picture: {e:#}"));
                }
            }
            ui.separator();
            let len = self.player.as_ref().map(|p| video_mp4::video_duration(&p.recording)).unwrap_or_default();
            if ui
                .add_enabled(self.export.is_none(), egui::Button::new("Export MP4…"))
                .on_hover_text(format!(
                    "Save the whole recording as an MP4 video ({}). The video keeps the real timing, so it can be longer than here, where long pauses are shortened. Needs FFmpeg.",
                    mmss(len)
                ))
                .clicked()
            {
                self.export_mp4(None);
            }
        });
        if let Some(job) = &self.export {
            let mut cancel = false;
            ui.horizontal(|ui| {
                let name = job.out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                cancel = ui.button("Cancel").on_hover_text("Stop making the video").clicked();
                let p = job.progress();
                ui.add(egui::ProgressBar::new(p).text(format!("Exporting {name}: {:.0}%", p * 100.0)));
            });
            if cancel {
                if let Some(job) = self.export.as_mut() {
                    job.cancel();
                }
            }
        }
        let mut secs = self.pos.min(total).as_secs_f32();
        ui.spacing_mut().slider_width = ui.available_width();
        let slider = egui::Slider::new(&mut secs, 0.0..=total.as_secs_f32().max(0.1)).show_value(false);
        if ui.add(slider).changed() {
            self.seek(Duration::from_secs_f32(secs));
        }
    }

    /// Export MP4: check for FFmpeg, ask where, start.
    fn export_mp4(&mut self, ff: Option<Ffmpeg>) {
        let ff = match ff.map(Ok).unwrap_or_else(super::mp4::require) {
            Ok(ff) => ff,
            Err(d) => {
                self.ffmpeg_dialog = Some(d);
                return;
            }
        };
        let Some(rec) = self.player.as_ref().map(|p| p.recording.clone()) else { return };
        let mut d = rfd::FileDialog::new().add_filter("MP4 video", &["mp4"]);
        match (&self.file, &self.inst, &self.state) {
            (Some(f), _, _) => {
                let stem = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "recording".into());
                d = d.set_file_name(format!("{stem}.mp4"));
                if let Some(dir) = f.parent() {
                    d = d.set_directory(dir);
                }
            }
            (None, Some(i), State::Playing(list)) => {
                let date = list.get(self.selected).map(|c| c.date.trim().to_string()).unwrap_or_default();
                d = d.set_file_name(format!("{}-boot-{}.mp4", super::console::file_safe(i.title()), super::console::file_safe(&date)));
            }
            _ => d = d.set_file_name("recording.mp4"),
        }
        let Some(out) = d.save_file() else { return };
        let out = super::mp4::with_mp4_extension(out);
        self.export = Some(Export::start(ff, move || Ok(rec), out, self.notifier()));
    }

    fn poll_export(&mut self) {
        let Some(job) = self.export.as_mut() else { return };
        let Some(r) = job.poll().cloned() else { return };
        let cancelled = job.was_cancelled();
        self.export = None;
        match r {
            Ok(done) => self.exported = Some(done),
            Err(_) if cancelled => {}
            Err(e) => self.message = Some(format!("Could not make the MP4 video: {e}")),
        }
    }

    fn screenshot(&mut self) {
        if self.fb.width == 0 {
            return;
        }
        let rgb: Vec<u8> = self.fb.pixels.iter().flat_map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8]).collect();
        let name = if self.inst.is_some() { "boot-capture.png" } else { "recording.png" };
        let Some(path) = rfd::FileDialog::new().set_file_name(name).add_filter("PNG image", &["png"]).save_file() else { return };
        self.message = Some(match image::RgbImage::from_raw(self.fb.width as u32, self.fb.height as u32, rgb).map(|i| i.save(&path)) {
            Some(Ok(())) => format!("Saved {}", path.display()),
            _ => "Could not save the screenshot.".into(),
        });
    }
}

fn rec_label(c: &BootCapture, i: usize) -> String {
    let which = match i {
        0 => " (latest)",
        _ => "",
    };
    format!("{}{which}", c.date.trim())
}

fn mmss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

impl WindowUi for PlaybackWindow {
    fn attributes(&self) -> WindowAttributes {
        super::base_attributes()
            .with_title(self.title())
            .with_inner_size(LogicalSize::new(900.0, 640.0))
    }

    fn created(&mut self, window: &Window, proxy: &EventLoopProxy<UserEvent>) {
        self.window_id = Some(window.id());
        self.proxy = Some(proxy.clone());
        match self.file.clone() {
            Some(f) => self.load_file(f),
            None => self.list(),
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, _window: &Window, _shell: &mut Shell) {
        let ctx = root.ctx().clone();
        self.poll();
        self.poll_export();
        if let Some(i) = self.pending_load.take() {
            if let State::Playing(list) = &self.state {
                let list = list.clone();
                self.load(list, i);
            }
        }
        self.tick(&ctx);
        self.update_texture(&ctx);
        egui::Panel::bottom("controls").show(root, |ui| {
            ui.add_space(4.0);
            self.controls(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(egui::Color32::BLACK)).show(root, |ui| {
            let status = match &self.state {
                State::Listing(_) => Some("Looking for boot recordings…".to_string()),
                State::Loading(..) if self.inst.is_none() => Some("Loading the recording…".to_string()),
                State::Loading(..) => Some("Downloading the recording…".to_string()),
                State::Failed(m) => Some(m.clone()),
                _ => None,
            };
            if let Some(s) = status {
                let failed = matches!(self.state, State::Failed(_));
                let file = self.inst.is_none();
                let rect = ui.max_rect();
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(rect.height() / 2.0 - 30.0);
                        if !failed {
                            ui.spinner();
                        }
                        ui.label(egui::RichText::new(s).color(egui::Color32::from_gray(200)));
                        if failed && file {
                            ui.add_space(8.0);
                            if ui.button("Open another file…").clicked() {
                                self.open_dialog();
                            }
                        }
                    });
                });
                return;
            }
            if let Some(t) = &self.texture {
                let [w, h] = t.size();
                let avail = ui.available_size();
                let s = (avail.x / w as f32).min(avail.y / h as f32);
                let sized = egui::load::SizedTexture::new(t.id(), egui::vec2(w as f32 * s, h as f32 * s));
                ui.centered_and_justified(|ui| ui.add(egui::Image::new(sized)));
            }
        });
        if let Some(d) = self.ffmpeg_dialog.as_mut() {
            if let Some(c) = d.show(&ctx) {
                self.ffmpeg_dialog = None;
                if let Choice::Found(ff) = c {
                    self.export_mp4(Some(ff));
                }
            }
        }
        if let Some(done) = self.exported.clone() {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("pb-exported")).show(&ctx, |ui| {
                ui.set_max_width(460.0);
                ui.heading("Video saved");
                ui.add_space(4.0);
                ui.label(format!("{}.", super::mp4::describe(done.duration, done.bytes)));
                ui.weak(done.path.display().to_string());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    super::mp4::open_buttons(ui, &done.path);
                    if ui.button("OK").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.exported = None;
            }
        }
        if let Some(m) = self.message.clone() {
            egui::Modal::new(egui::Id::unique("pb-msg")).show(&ctx, |ui| {
                ui.set_max_width(480.0);
                ui.label(m);
                if ui.button("OK").clicked() {
                    self.message = None;
                }
            });
        }
    }
}
