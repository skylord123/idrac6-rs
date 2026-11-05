//! Virtual console window.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent};
use winit::event_loop::EventLoopProxy;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

use super::vmedia::VmPanel;
use super::{Shell, UserEvent, WindowUi};
use crate::config::Instance;
use crate::kvm::keymap::{self, Macro};
use crate::kvm::proto::{self, Bandwidth, PowerAction, SharingAnswer, buttons};
use crate::kvm::session::{KvmEvent, KvmSession, VideoState};
use crate::web::{ConsoleInfo, WebSession};

enum Phase {
    /// Logging in to the web UI and fetching the console tokens.
    LoggingIn(Receiver<anyhow::Result<(WebSession, ConsoleInfo)>>),
    Connected(KvmSession),
    Failed(String),
}

pub struct ConsoleWindow {
    inst: Instance,
    phase: Phase,
    web: Option<Arc<WebSession>>,
    info: Option<ConsoleInfo>,
    window_id: Option<WindowId>,
    proxy: Option<EventLoopProxy<UserEvent>>,
    status: String,
    view_only: bool,
    resolution: Option<(u16, u16)>,
    leds: proto::Leds,
    video_state: Option<VideoState>,
    power: Option<proto::ServerInfo>,
    sharing: Option<proto::SharingRequest>,
    in_use_retry: bool,
    /// Connect with the "share" flag once the login finishes.
    pending_shared: bool,

    texture: Option<egui::TextureHandle>,
    shown_seq: u64,
    fit: bool,
    fullscreen: bool,
    capture_keys: bool,
    pressed: BTreeSet<u16>,
    hovering_video: bool,
    mouse: Option<(u16, u16)>,
    mouse_buttons: u8,
    wheel_acc: f32,

    confirm_power: Option<PowerAction>,
    message: Option<String>,
    show_stats: bool,
    stats: Stats,
    vm: VmPanel,
}

#[derive(Default)]
struct Stats {
    last: Option<(Instant, u64, u64)>,
    fps: f32,
    kbps: f32,
}

fn macros() -> &'static [Macro] {
    static M: OnceLock<Vec<Macro>> = OnceLock::new();
    M.get_or_init(keymap::macros)
}

fn sysrq() -> &'static [Macro] {
    static M: OnceLock<Vec<Macro>> = OnceLock::new();
    M.get_or_init(keymap::sysrq_macros)
}

impl ConsoleWindow {
    pub fn new(inst: Instance) -> Self {
        Self {
            phase: Phase::Failed(String::new()),
            inst,
            web: None,
            info: None,
            window_id: None,
            proxy: None,
            status: "Starting…".into(),
            view_only: false,
            resolution: None,
            leds: Default::default(),
            video_state: None,
            power: None,
            sharing: None,
            in_use_retry: false,
            pending_shared: false,
            texture: None,
            shown_seq: 0,
            fit: true,
            fullscreen: false,
            capture_keys: true,
            pressed: BTreeSet::new(),
            hovering_video: false,
            mouse: None,
            mouse_buttons: 0,
            wheel_acc: 0.0,
            confirm_power: None,
            message: None,
            show_stats: false,
            stats: Stats::default(),
            vm: VmPanel::new(),
        }
    }

    fn repaint(&self) {
        if let (Some(p), Some(id)) = (&self.proxy, self.window_id) {
            let _ = p.send_event(UserEvent::Repaint(id, Duration::ZERO));
        }
    }

    fn notifier(&self) -> Arc<dyn Fn() + Send + Sync> {
        let p = self.proxy.clone().expect("proxy");
        let id = self.window_id.expect("window id");
        Arc::new(move || {
            let _ = p.send_event(UserEvent::Repaint(id, Duration::ZERO));
        })
    }

    /// Log in to the web UI and fetch the JNLP in the background.
    fn start_login(&mut self) {
        let (tx, rx) = mpsc::channel();
        let inst = self.inst.clone();
        let notify = self.notifier();
        thread::spawn(move || {
            let r = (|| {
                let web = WebSession::login(&inst.host, inst.https_port, &inst.username, &inst.password)?;
                match web.console_info() {
                    Ok(info) => Ok((web, info)),
                    Err(e) => {
                        web.logout();
                        Err(e)
                    }
                }
            })();
            let _ = tx.send(r);
            notify();
        });
        self.status = format!("Logging in to {}…", self.inst.host);
        self.phase = Phase::LoggingIn(rx);
    }

    fn connect(&mut self, shared: bool) {
        let Some(info) = &self.info else { return };
        let params = info.kvm_params(&self.inst.host, shared);
        let session = KvmSession::start(params, self.notifier());
        self.phase = Phase::Connected(session);
    }

    fn session(&self) -> Option<&KvmSession> {
        match &self.phase {
            Phase::Connected(s) if !s.is_closed() => Some(s),
            _ => None,
        }
    }

    fn can_input(&self) -> bool {
        self.session().is_some() && !self.view_only
    }

    fn release_all_keys(&mut self) {
        let keys: Vec<u16> = std::mem::take(&mut self.pressed).into_iter().collect();
        if let Some(s) = self.session() {
            for k in keys {
                s.key(k, false);
            }
        }
    }

    fn send_macro(&self, m: &Macro) {
        if let Some(s) = self.session() {
            s.key_combo(&m.keys);
        }
    }

    fn poll(&mut self) {
        if let Phase::LoggingIn(rx) = &self.phase {
            match rx.try_recv() {
                Ok(Ok((web, info))) => {
                    self.web = Some(Arc::new(web));
                    self.info = Some(info);
                    self.status = "Connecting to the virtual console…".into();
                    self.connect(self.pending_shared);
                }
                Ok(Err(e)) => {
                    self.phase = Phase::Failed(format!("{e:#}"));
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.phase = Phase::Failed("login thread died".into()),
            }
        }
        let mut events = Vec::new();
        if let Phase::Connected(s) = &self.phase {
            while let Ok(e) = s.events.try_recv() {
                events.push(e);
            }
        }
        for e in events {
            match e {
                KvmEvent::Status(s) => self.status = s,
                KvmEvent::Running => self.status = "Connected".into(),
                KvmEvent::LoginFailed(r) => {
                    self.in_use_retry = r.contains("already in use");
                    self.phase = Phase::Failed(r);
                }
                KvmEvent::ViewOnly => self.view_only = true,
                KvmEvent::Resolution(w, h) => self.resolution = Some((w, h)),
                KvmEvent::Leds(l) => self.leds = l,
                KvmEvent::PowerState(p) => self.power = Some(p),
                KvmEvent::SharingRequest(r) => self.sharing = Some(r),
                KvmEvent::VideoState(v) => self.video_state = Some(v),
                KvmEvent::VmCredentials { user, password } => {
                    let port = self.info.as_ref().map(|i| i.kvm_params(&self.inst.host, false).kmport).unwrap_or(5900);
                    let notify = self.notifier();
                    self.vm.credentials(&self.inst.host, port, user, password, notify);
                }
                KvmEvent::Disconnected(m) => {
                    if !matches!(self.phase, Phase::Failed(_)) {
                        self.phase = Phase::Failed(m);
                    }
                }
            }
        }
    }

    fn update_texture(&mut self, ctx: &egui::Context) {
        let Some(s) = self.session() else { return };
        let seq = s.shared.frame_seq.load(Ordering::Acquire);
        if seq == self.shown_seq && self.texture.is_some() {
            return;
        }
        let fb = s.shared.fb.lock().unwrap();
        if fb.width == 0 || fb.height == 0 {
            return;
        }
        let mut px = Vec::with_capacity(fb.pixels.len());
        px.extend(fb.pixels.iter().map(|&p| egui::Color32::from_rgb((p >> 16) as u8, (p >> 8) as u8, p as u8)));
        let img = egui::ColorImage::new([fb.width, fb.height], px);
        drop(fb);
        self.shown_seq = seq;
        match &mut self.texture {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("console", img, egui::TextureOptions::LINEAR)),
        }
    }

    fn update_stats(&mut self) {
        let Some(s) = self.session() else { return };
        let now = Instant::now();
        let frames = s.shared.frames.load(Ordering::Relaxed);
        let bytes = s.shared.bytes_in.load(Ordering::Relaxed);
        match self.stats.last {
            Some((t, f0, b0)) if now - t >= Duration::from_secs(1) => {
                let dt = (now - t).as_secs_f32();
                self.stats.fps = (frames - f0) as f32 / dt;
                self.stats.kbps = (bytes - b0) as f32 * 8.0 / 1000.0 / dt;
                self.stats.last = Some((now, frames, bytes));
            }
            None => self.stats.last = Some((now, frames, bytes)),
            _ => {}
        }
    }

    fn screenshot(&mut self) {
        let Some(s) = self.session() else { return };
        let (w, h, rgb) = {
            let fb = s.shared.fb.lock().unwrap();
            let rgb: Vec<u8> = fb.pixels.iter().flat_map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8]).collect();
            (fb.width as u32, fb.height as u32, rgb)
        };
        if w == 0 {
            self.message = Some("There is no picture to save yet.".into());
            return;
        }
        let name = format!("{}-{}.png", self.inst.title().replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_"), timestamp());
        let Some(path) = rfd::FileDialog::new().set_file_name(&name).add_filter("PNG image", &["png"]).save_file() else { return };
        let r = image::RgbImage::from_raw(w, h, rgb).map(|img| img.save(&path));
        self.message = Some(match r {
            Some(Ok(())) => format!("Saved {}", path.display()),
            Some(Err(e)) => format!("Could not save the screenshot: {e}"),
            None => "Could not save the screenshot.".into(),
        });
    }

    fn menu(&mut self, ui: &mut egui::Ui, window: &Window, shell: &mut Shell) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Save screenshot…").clicked() {
                    self.screenshot();
                }
                ui.separator();
                if ui.button("Reconnect").clicked() {
                    self.reconnect(false);
                }
                if ui.button("Close").clicked() {
                    shell.close_me = true;
                }
            });
            ui.menu_button("View", |ui| {
                ui.radio_value(&mut self.fit, true, "Fit to window");
                ui.radio_value(&mut self.fit, false, "Actual size (1:1)");
                ui.separator();
                if ui.checkbox(&mut self.fullscreen, "Full screen").clicked() {
                    window.set_fullscreen(self.fullscreen.then_some(Fullscreen::Borderless(None)));
                }
                if let Some((w, h)) = self.resolution {
                    if ui.button(format!("Resize window to {w}×{h}")).clicked() {
                        let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(w as u32, h as u32 + 60));
                    }
                }
                ui.separator();
                ui.checkbox(&mut self.show_stats, "Statistics");
            });
            ui.add_enabled_ui(self.can_input(), |ui| {
                ui.menu_button("Keyboard", |ui| {
                    ui.checkbox(&mut self.capture_keys, "Send keystrokes to server")
                        .on_hover_text("When off, keys go to this window instead (useful to type in menus).");
                    if ui.button("Release all keys").on_hover_text("Fixes keys that appear stuck").clicked() {
                        self.release_all_keys();
                    }
                    ui.separator();
                    ui.menu_button("Macros", |ui| {
                        for m in macros() {
                            if ui.button(m.label).clicked() {
                                self.send_macro(m);
                            }
                        }
                    });
                    ui.menu_button("Linux SysRq", |ui| {
                        for m in sysrq() {
                            if ui.button(m.label).clicked() {
                                self.send_macro(m);
                            }
                        }
                    });
                    ui.separator();
                    for (label, key) in [("Toggle Num Lock", keymap::usage::NUM_LOCK), ("Toggle Caps Lock", keymap::usage::CAPS_LOCK), ("Toggle Scroll Lock", keymap::usage::SCROLL_LOCK)] {
                        if ui.button(label).clicked() {
                            if let Some(s) = self.session() {
                                s.key_combo(&[key]);
                            }
                        }
                    }
                });
                ui.menu_button("Power", |ui| {
                    if let Some(p) = &self.power {
                        ui.weak(format!("Server is {}", power_label(p.power_state)));
                        ui.separator();
                    }
                    for a in PowerAction::ALL {
                        if ui.button(a.label()).clicked() {
                            self.confirm_power = Some(a);
                        }
                    }
                });
            });
            let vm_ok = self.session().is_some() && self.info.as_ref().is_some_and(|i| i.vm_privilege());
            if ui.add_enabled(vm_ok, egui::Button::new("Virtual Media")).on_disabled_hover_text("Needs a connected console and the Virtual Media privilege").clicked() {
                self.vm.open = true;
            }
            ui.add_enabled_ui(self.session().is_some(), |ui| {
                ui.menu_button("Video", |ui| {
                    if ui.button("Refresh screen").clicked() {
                        if let Some(s) = self.session() {
                            s.refresh();
                        }
                    }
                    ui.menu_button("Bandwidth limit", |ui| {
                        for b in Bandwidth::ALL {
                            if ui.button(b.label()).clicked() {
                                if let Some(s) = self.session() {
                                    s.send(proto::set_bandwidth(b as u8));
                                }
                            }
                        }
                    });
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                led(ui, "Scroll", self.leds.scroll);
                led(ui, "Caps", self.leds.caps);
                led(ui, "Num", self.leds.num);
                if self.view_only {
                    ui.colored_label(ui.visuals().warn_fg_color, "View only");
                }
            });
        });
    }

    fn reconnect(&mut self, shared: bool) {
        if let Phase::Connected(s) = &self.phase {
            s.close();
        }
        self.texture = None;
        self.shown_seq = 0;
        self.view_only = false;
        self.pressed.clear();
        // Session tokens are single use, so always fetch fresh ones.
        if let Some(w) = self.web.take() {
            thread::spawn(move || w.logout());
        }
        self.pending_shared = shared;
        self.start_login();
    }

    fn video(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        let Some(tex) = &self.texture else {
            let failed = match &self.phase {
                Phase::Failed(m) if !m.is_empty() => Some(m.clone()),
                _ => None,
            };
            ui.centered_and_justified(|ui| {
                match failed {
                    Some(m) => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(avail.y / 3.0);
                            ui.colored_label(ui.visuals().error_fg_color, m);
                            ui.add_space(8.0);
                            if ui.button("Try again").clicked() {
                                self.reconnect(false);
                            }
                            if self.in_use_retry && ui.button("Ask the current user to share").clicked() {
                                self.reconnect(true);
                            }
                        });
                    }
                    None => {
                        let text = match self.video_state {
                            Some(VideoState::NoSignal) => "No video signal from the server".to_string(),
                            Some(VideoState::OutOfRange) => "The iDRAC reports the server's video mode is out of range.\nLower the resolution or refresh rate on the server (iDRAC6 supports up to 1280×1024 at 60 Hz).".to_string(),
                            Some(VideoState::Calibrating) => "The iDRAC is calibrating the video signal…".to_string(),
                            Some(VideoState::Stopped(c)) => format!("Video is blocked or stopped by the iDRAC (code {c})."),
                            _ => self.status.clone(),
                        };
                        ui.vertical_centered(|ui| {
                            ui.add_space(avail.y / 3.0);
                            ui.spinner();
                            ui.label(text);
                        });
                    }
                }
            });
            return;
        };
        let [tw, th] = tex.size();
        let (tw, th) = (tw as f32, th as f32);
        let ppp = ui.ctx().pixels_per_point();
        let size = if self.fit {
            let s = (avail.x / tw).min(avail.y / th);
            egui::vec2(tw * s, th * s)
        } else {
            egui::vec2(tw / ppp, th / ppp)
        };
        let sized = egui::load::SizedTexture::new(tex.id(), size);
        let image = egui::Image::new(sized).sense(egui::Sense::click_and_drag());
        let resp = if self.fit {
            ui.centered_and_justified(|ui| ui.add(image)).inner
        } else {
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| ui.add(image)).inner
        };
        self.hovering_video = resp.hovered() || resp.dragged();
        if self.can_input() {
            self.mouse_input(ui, &resp, tw, th);
        }
        if let (Some(VideoState::NoSignal | VideoState::OutOfRange), Some(_)) = (self.video_state, &self.texture) {
            let text = if self.video_state == Some(VideoState::NoSignal) { "No signal" } else { "Out of range" };
            ui.painter().text(resp.rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(24.0), egui::Color32::YELLOW);
        }
    }

    fn mouse_input(&mut self, ui: &egui::Ui, resp: &egui::Response, tw: f32, th: f32) {
        if self.session().is_none() {
            return;
        }
        let rect = resp.rect;
        let (pos, primary, secondary, middle, scroll) = ui.input(|i| {
            (
                i.pointer.latest_pos(),
                i.pointer.primary_down(),
                i.pointer.secondary_down(),
                i.pointer.middle_down(),
                i.raw.events.iter().fold(0.0f32, |acc, e| match e {
                    egui::Event::MouseWheel { unit, delta, .. } => {
                        acc + delta.y
                            * match unit {
                                egui::MouseWheelUnit::Point => 1.0,
                                egui::MouseWheelUnit::Line => 40.0,
                                egui::MouseWheelUnit::Page => 120.0,
                            }
                    }
                    _ => acc,
                }),
            )
        });
        let active = resp.hovered() || resp.dragged();
        let mut b = 0;
        if active {
            if primary {
                b |= buttons::LEFT;
            }
            if secondary {
                b |= buttons::RIGHT;
            }
            if middle {
                b |= buttons::MIDDLE;
            }
        }
        let Some(p) = pos else { return };
        if !active && b == self.mouse_buttons {
            return;
        }
        let x = ((p.x - rect.min.x) / rect.width() * tw).clamp(0.0, tw - 1.0) as u16;
        let y = ((p.y - rect.min.y) / rect.height() * th).clamp(0.0, th - 1.0) as u16;
        let mut wheel = 0i16;
        if active {
            self.wheel_acc += scroll;
            // One wheel notch is ~50 points in egui-winit.
            while self.wheel_acc >= 40.0 {
                wheel += 1;
                self.wheel_acc -= 40.0;
            }
            while self.wheel_acc <= -40.0 {
                wheel -= 1;
                self.wheel_acc += 40.0;
            }
        }
        if Some((x, y)) != self.mouse || b != self.mouse_buttons || wheel != 0 {
            if let Some(s) = self.session() {
                s.mouse(x, y, b, wheel);
            }
            self.mouse = Some((x, y));
            self.mouse_buttons = b;
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(a) = self.confirm_power {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("power")).show(ctx, |ui| {
                ui.heading("Power control");
                ui.label(format!("Really send \"{}\" to {}?", a.label(), self.inst.title()));
                ui.horizontal(|ui| {
                    if ui.button(a.label()).clicked() {
                        if let Some(s) = self.session() {
                            if let Err(e) = s.power(a) {
                                self.message = Some(format!("{e:#}"));
                            }
                        }
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.confirm_power = None;
            }
        }
        if let Some(r) = self.sharing.clone() {
            let mut answer = None;
            egui::Modal::new(egui::Id::unique("share")).show(ctx, |ui| {
                ui.heading("Session sharing request");
                let who = if r.user.is_empty() { "Another user".to_string() } else { r.user.clone() };
                ui.label(format!("{who} wants to share this console session."));
                ui.horizontal(|ui| {
                    if ui.button("Allow").clicked() {
                        answer = Some(SharingAnswer::Approve);
                    }
                    if ui.button("Allow view only").clicked() {
                        answer = Some(SharingAnswer::ReadOnly);
                    }
                    if ui.button("Reject").clicked() {
                        answer = Some(SharingAnswer::Reject);
                    }
                });
            });
            if let Some(a) = answer {
                if let Some(s) = self.session() {
                    s.send(proto::sharing_response(r.id, a));
                }
                self.sharing = None;
            }
        }
        if let Some(m) = self.message.clone() {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("msg")).show(ctx, |ui| {
                ui.label(m);
                if ui.button("OK").clicked() {
                    close = true;
                }
            });
            if close {
                self.message = None;
            }
        }
    }
}

fn power_label(state: u16) -> &'static str {
    match state {
        1 => "on",
        2 => "off",
        _ => "in an unknown power state",
    }
}

fn led(ui: &mut egui::Ui, name: &str, on: bool) {
    let c = if on { egui::Color32::from_rgb(80, 220, 100) } else { ui.visuals().weak_text_color() };
    ui.colored_label(c, name);
}

fn timestamp() -> String {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    s.to_string()
}

impl WindowUi for ConsoleWindow {
    fn attributes(&self) -> WindowAttributes {
        super::base_attributes()
            .with_title(format!("{} - Virtual Console", self.inst.title()))
            .with_inner_size(LogicalSize::new(1040.0, 830.0))
    }

    fn created(&mut self, window: &Window, proxy: &EventLoopProxy<UserEvent>) {
        self.window_id = Some(window.id());
        self.proxy = Some(proxy.clone());
        self.start_login();
    }

    fn ui(&mut self, root: &mut egui::Ui, window: &Window, shell: &mut Shell) {
        let ctx = root.ctx().clone();
        self.poll();
        self.update_texture(&ctx);
        self.update_stats();
        egui::Panel::top("menu").show(root, |ui| self.menu(ui, window, shell));
        egui::Panel::bottom("status").show(root, |ui| {
            ui.horizontal(|ui| {
                let state = match &self.phase {
                    Phase::LoggingIn(_) => self.status.clone(),
                    Phase::Connected(s) if !s.is_closed() => self.status.clone(),
                    Phase::Connected(_) => "Disconnected".into(),
                    Phase::Failed(_) => "Disconnected".into(),
                };
                ui.label(state);
                if let Some((w, h)) = self.resolution {
                    ui.separator();
                    ui.label(format!("{w}×{h}"));
                }
                if self.show_stats {
                    ui.separator();
                    ui.label(format!("{:.1} fps, {:.0} kbit/s", self.stats.fps, self.stats.kbps));
                }
                if self.can_input() && self.capture_keys {
                    ui.separator();
                    ui.weak(if self.hovering_video { "Keyboard goes to the server" } else { "Point at the screen to type" });
                }
            });
        });
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(egui::Color32::BLACK)).show(root, |ui| self.video(ui));
        self.dialogs(&ctx);
        let kvm = match &self.phase {
            Phase::Connected(s) if !s.is_closed() => Some(s),
            _ => None,
        };
        self.vm.ui(&ctx, kvm);
        if self.show_stats && self.session().is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn raw_key(&mut self, ev: &KeyEvent) -> bool {
        // Keys go to the server while the pointer is over the video and no
        // dialog is open, like a KVM switch.
        let modal_open = self.confirm_power.is_some() || self.sharing.is_some() || self.message.is_some();
        // Over the Virtual Media window the keyboard belongs to egui.
        if !self.capture_keys || !self.can_input() || modal_open {
            return false;
        }
        let PhysicalKey::Code(code) = ev.physical_key else { return false };
        let pressed = ev.state == ElementState::Pressed;
        if !self.hovering_video && !self.pressed.contains(&keymap::hid_usage(code).unwrap_or(0)) {
            return false;
        }
        let Some(u) = keymap::hid_usage(code) else { return code != KeyCode::Fn };
        if pressed {
            self.pressed.insert(u);
        } else {
            self.pressed.remove(&u);
        }
        if let Some(s) = self.session() {
            s.key(u, pressed);
        }
        true
    }

    fn focus_changed(&mut self, focused: bool) {
        if !focused {
            // Otherwise e.g. Alt stays pressed on the server after Alt-Tab.
            self.release_all_keys();
            self.mouse_buttons = 0;
        }
        self.repaint();
    }

    fn close_requested(&mut self) -> bool {
        self.release_all_keys();
        self.vm.disconnect();
        if let Phase::Connected(s) = &self.phase {
            s.close();
        }
        // The web session is logged out in Drop.
        true
    }
}

impl Drop for ConsoleWindow {
    fn drop(&mut self) {
        // Log out synchronously: a background thread would be killed when
        // the app exits, leaving the session open on the iDRAC, which only
        // allows a few at a time.
        if let Some(w) = self.web.take() {
            w.logout();
        }
    }
}
