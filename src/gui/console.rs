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
use crate::kvm::typer::{self, Speed, TypeJob};
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
    focused: bool,
    mouse_accel: Option<u8>,
    bandwidth: Bandwidth,
    /// Single cursor mode: hide our pointer and send relative movement.
    single_cursor: bool,
    /// The pointer is currently captured for single cursor mode.
    grabbed: bool,
    want_grab: Option<bool>,
    rel: (f64, f64),
    rel_sent: Instant,
    show_about: bool,

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
    /// "Type text" dialog: the text to type.
    type_text: Option<String>,
    type_speed: Speed,
    typing: Option<TypeJob>,
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

fn alt_f() -> &'static [Macro] {
    static M: OnceLock<Vec<Macro>> = OnceLock::new();
    M.get_or_init(|| keymap::function_key_macros(false))
}

fn ctrl_alt_f() -> &'static [Macro] {
    static M: OnceLock<Vec<Macro>> = OnceLock::new();
    M.get_or_init(|| keymap::function_key_macros(true))
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
            focused: true,
            mouse_accel: None,
            bandwidth: Bandwidth::Unlimited,
            single_cursor: false,
            grabbed: false,
            want_grab: None,
            rel: (0.0, 0.0),
            rel_sent: Instant::now(),
            show_about: false,
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
            type_text: None,
            type_speed: Speed::Normal,
            typing: None,
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

    fn open_type_dialog(&mut self) {
        let clip = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
        self.type_text = Some(clip);
    }

    fn start_typing(&mut self, text: &str) {
        self.release_all_keys();
        let Some(s) = self.session() else { return };
        let job = TypeJob::start(s.sender(), text, self.type_speed, self.notifier());
        self.typing = Some(job);
    }

    fn type_dialog(&mut self, ctx: &egui::Context) {
        let Some(text) = self.type_text.as_mut() else { return };
        let mut go = false;
        let mut close = false;
        egui::Modal::new(egui::Id::unique("type-text")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Type text");
            ui.label("The text is typed on the server as keystrokes. Paste with Ctrl+V or edit it below.");
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                ui.add(egui::TextEdit::multiline(text).font(egui::TextStyle::Monospace).desired_rows(8).desired_width(f32::INFINITY).hint_text("Text to type"));
            });
            let chars = typer::normalize(text).chars().filter(|&c| typer::stroke(c).is_some()).count();
            let lines = text.lines().count().max(1);
            ui.weak(format!("{chars} character{}, {lines} line{}", plural(chars), plural(lines)));
            let bad = typer::untypeable(text);
            if !bad.is_empty() {
                let list: String = bad.iter().take(12).map(|c| if c.is_control() { format!("U+{:04X} ", *c as u32) } else { format!("{c} ") }).collect();
                ui.colored_label(ui.visuals().warn_fg_color, format!("Skipped (no key on a US keyboard): {}", list.trim_end()));
            }
            if self.leds.caps {
                ui.colored_label(ui.visuals().warn_fg_color, "Caps Lock is on at the server, so letters may come out in the wrong case.");
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Speed");
                for sp in Speed::ALL {
                    ui.radio_value(&mut self.type_speed, sp, sp.label());
                }
            });
            ui.weak("Typed as on a US keyboard; the server's layout decides what appears.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(chars > 0, egui::Button::new("Type")).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if go {
            let t = self.type_text.take().unwrap_or_default();
            self.start_typing(&t);
        } else if close {
            self.type_text = None;
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
                KvmEvent::Running => {
                    self.status = "Connected".into();
                    if let Phase::Connected(s) = &self.phase {
                        s.set_focus(self.focused);
                    }
                }
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
                KvmEvent::MouseAccel(v) => {
                    log::info!("iDRAC mouse acceleration: {:?}", proto::MouseAccel::from_u8(v));
                    self.mouse_accel = Some(v);
                }
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
                if ui.add_enabled(self.session().is_some(), egui::Button::new("Refresh")).clicked() {
                    if let Some(s) = self.session() {
                        s.refresh();
                    }
                }
                ui.separator();
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
            });
            ui.add_enabled_ui(self.can_input(), |ui| {
                ui.menu_button("Keyboard", |ui| {
                    if ui.button("Type text…").on_hover_text("Type text (e.g. from the clipboard) as keystrokes").clicked() {
                        self.open_type_dialog();
                    }
                    ui.separator();
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
                        ui.separator();
                        for (label, list) in [("Alt-F?", alt_f()), ("Ctrl-Alt-F?", ctrl_alt_f())] {
                            ui.menu_button(label, |ui| {
                                for m in list {
                                    if ui.button(m.label).clicked() {
                                        self.send_macro(m);
                                    }
                                }
                            });
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
                ui.menu_button("Tools", |ui| {
                    if ui
                        .checkbox(&mut self.single_cursor, "Single cursor")
                        .on_hover_text("Hide the local pointer and move the server's pointer directly. Click the screen to start, press F9 to release.")
                        .clicked()
                        && !self.single_cursor
                    {
                        self.want_grab = Some(false);
                    }
                    ui.menu_button("Mouse acceleration", |ui| {
                        ui.weak("Match the server's operating system");
                        for a in proto::MouseAccel::ALL {
                            let selected = self.mouse_accel == Some(a as u8);
                            if ui.radio(selected, a.label()).clicked() {
                                if let Some(s) = self.session() {
                                    s.send(proto::mouse_accel(a as u8));
                                }
                                self.mouse_accel = Some(a as u8);
                            }
                        }
                    });
                    ui.menu_button("Bandwidth limit", |ui| {
                        for b in Bandwidth::ALL {
                            if ui.radio(self.bandwidth == b, b.label()).clicked() {
                                if let Some(s) = self.session() {
                                    s.send(proto::set_bandwidth(b as u8));
                                }
                                self.bandwidth = b;
                            }
                        }
                    });
                    ui.separator();
                    ui.checkbox(&mut self.show_stats, "Statistics");
                });
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About").clicked() {
                    self.show_about = true;
                }
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
            if let Some(m) = failed {
                ui.centered_and_justified(|ui| {
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
                });
                return;
            }
            // No picture yet (connecting, no signal, out of range, screen
            // asleep). The area still takes keyboard and mouse input, so a
            // key press or click can wake the server's screen.
            let resp = ui.allocate_rect(ui.max_rect(), egui::Sense::click_and_drag());
            self.hovering_video = resp.hovered() || resp.dragged();
            if self.can_input() {
                let (w, h) = self.resolution.map(|(w, h)| (w as f32, h as f32)).unwrap_or((1024.0, 768.0));
                self.pointer_input(ui, &resp, w, h);
            }
            let wake = "Press a key or click here to wake the server's screen.";
            let (text, spinner) = match self.video_state {
                Some(VideoState::NoSignal) => (format!("No video signal from the server.\n{wake}"), false),
                Some(VideoState::OutOfRange) => (
                    format!("The iDRAC reports the video mode is out of range.\n{wake}\nIf it stays like this, lower the server's resolution (iDRAC6 supports up to 1280×1024 at 60 Hz)."),
                    false,
                ),
                Some(VideoState::Calibrating) => ("The iDRAC is calibrating the video signal…".to_string(), true),
                Some(VideoState::Stopped(c)) => (format!("Video is blocked or stopped by the iDRAC (code {c})."), false),
                _ => (self.status.clone(), true),
            };
            let rect = resp.rect;
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(rect.height() / 3.0);
                    if spinner {
                        ui.spinner();
                    }
                    ui.label(egui::RichText::new(text).color(egui::Color32::from_gray(200)));
                });
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
            self.pointer_input(ui, &resp, tw, th);
        }
        if let (Some(VideoState::NoSignal | VideoState::OutOfRange), Some(_)) = (self.video_state, &self.texture) {
            let text = if self.video_state == Some(VideoState::NoSignal) { "No signal" } else { "Out of range" };
            ui.painter().text(resp.rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(24.0), egui::Color32::YELLOW);
        }
    }

    /// Route pointer input to absolute or single cursor (relative) mode.
    fn pointer_input(&mut self, ui: &egui::Ui, resp: &egui::Response, tw: f32, th: f32) {
        if self.grabbed {
            self.relative_buttons(ui);
        } else if self.single_cursor && resp.clicked() {
            self.want_grab = Some(true);
        } else {
            self.mouse_input(ui, resp, tw, th);
        }
    }

    /// Buttons and wheel while the pointer is captured.
    fn relative_buttons(&mut self, ui: &egui::Ui) {
        let (primary, secondary, middle, scroll) = ui.input(|i| {
            (
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
        let mut b = 0;
        if primary {
            b |= buttons::LEFT;
        }
        if secondary {
            b |= buttons::RIGHT;
        }
        if middle {
            b |= buttons::MIDDLE;
        }
        self.wheel_acc += scroll;
        let mut wheel = 0i16;
        while self.wheel_acc >= 40.0 {
            wheel += 1;
            self.wheel_acc -= 40.0;
        }
        while self.wheel_acc <= -40.0 {
            wheel -= 1;
            self.wheel_acc += 40.0;
        }
        if b != self.mouse_buttons || wheel != 0 {
            self.mouse_buttons = b;
            self.flush_relative(true, wheel);
        }
    }

    /// Send accumulated relative motion. Rate-limited to 100 messages/s
    /// unless `force` (button or wheel change).
    fn flush_relative(&mut self, force: bool, wheel: i16) {
        let (dx, dy) = (self.rel.0.trunc(), self.rel.1.trunc());
        let moved = dx != 0.0 || dy != 0.0;
        if !force && (!moved || self.rel_sent.elapsed() < Duration::from_millis(10)) {
            return;
        }
        self.rel.0 -= dx;
        self.rel.1 -= dy;
        self.rel_sent = Instant::now();
        let clamp = |v: f64| v.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        let b = self.mouse_buttons;
        if let Some(s) = self.session() {
            s.send(proto::mouse_rel(clamp(dx), clamp(dy), b, wheel));
        }
    }

    fn apply_grab(&mut self, window: &Window) {
        use winit::window::CursorGrabMode;
        let Some(grab) = self.want_grab.take() else { return };
        if grab && !self.grabbed && self.can_input() {
            let ok = window.set_cursor_grab(CursorGrabMode::Locked).or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined));
            if let Err(e) = ok {
                log::warn!("could not capture the pointer: {e}");
            }
            window.set_cursor_visible(false);
            self.grabbed = true;
            self.rel = (0.0, 0.0);
            if let Some(s) = self.session() {
                s.send(proto::mouse_origin());
            }
        } else if !grab && self.grabbed {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.grabbed = false;
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

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
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
        if self.session().is_none() && self.grabbed {
            self.want_grab = Some(false);
        }
        self.apply_grab(window);
        if self.grabbed {
            self.flush_relative(false, 0);
            if self.rel.0.abs() >= 1.0 || self.rel.1.abs() >= 1.0 {
                ctx.request_repaint_after(Duration::from_millis(10));
            }
        }
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
                if let Some(job) = &self.typing {
                    ui.separator();
                    let done = job.done();
                    ui.add(egui::ProgressBar::new(done as f32 / job.total.max(1) as f32).desired_width(140.0).text(format!("Typing {done}/{}", job.total)));
                    if ui.small_button("Stop").clicked() {
                        job.cancel();
                    }
                }
                if self.grabbed {
                    ui.separator();
                    ui.strong("Single cursor: press F9 to release the pointer");
                } else if self.can_input() && self.capture_keys {
                    ui.separator();
                    ui.weak(if self.hovering_video { "Keyboard goes to the server" } else { "Point at the screen to type" });
                }
            });
        });
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(egui::Color32::BLACK)).show(root, |ui| self.video(ui));
        self.dialogs(&ctx);
        self.type_dialog(&ctx);
        if self.typing.as_ref().is_some_and(|j| j.finished()) {
            self.typing = None;
        }
        if self.show_about {
            egui::Modal::new(egui::Id::unique("about")).show(&ctx, |ui| {
                ui.heading(format!("idrac6 {}", env!("CARGO_PKG_VERSION")));
                ui.label("Virtual console, virtual media and web UI proxy for Dell iDRAC6.");
                ui.hyperlink("https://github.com/skylord123/idrac6-rs");
                ui.weak("MIT licensed. Not affiliated with Dell or Vertiv/Avocent.");
                if ui.button("Close").clicked() {
                    self.show_about = false;
                }
            });
        }
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
        let modal_open = self.confirm_power.is_some() || self.sharing.is_some() || self.message.is_some() || self.show_about || self.type_text.is_some();
        // Over the Virtual Media window the keyboard belongs to egui.
        if !self.capture_keys || !self.can_input() || modal_open {
            return false;
        }
        let PhysicalKey::Code(code) = ev.physical_key else { return false };
        let pressed = ev.state == ElementState::Pressed;
        if self.grabbed && code == KeyCode::F9 {
            if pressed {
                self.want_grab = Some(false);
                self.repaint();
            }
            return true;
        }
        if !self.grabbed && !self.hovering_video && !self.pressed.contains(&keymap::hid_usage(code).unwrap_or(0)) {
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

    fn raw_mouse_motion(&mut self, dx: f64, dy: f64) -> bool {
        if !self.grabbed {
            return false;
        }
        self.rel.0 += dx;
        self.rel.1 += dy;
        self.flush_relative(false, 0);
        true
    }

    fn focus_changed(&mut self, focused: bool) {
        self.focused = focused;
        if !focused && self.grabbed {
            self.want_grab = Some(false);
        }
        if let Some(s) = self.session() {
            s.set_focus(focused);
        }
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
