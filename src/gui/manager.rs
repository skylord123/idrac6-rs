//! Main window: the list of saved iDRACs, with live status, a picture of
//! each server's screen, and power control.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use winit::dpi::LogicalSize;
use winit::event_loop::EventLoopProxy;
use winit::window::{Window, WindowAttributes, WindowId};

use super::monitor::{Monitor, MonitorState};
use super::playback::PlaybackWindow;
use super::{ConsoleWindow, Shell, UserEvent, WindowUi};
use crate::config::{Config, Instance};
use crate::health::Health;
use crate::kvm::proto::PowerAction;
use crate::proxy::WebProxy;
use crate::web::WebClient;

const THUMB: egui::Vec2 = egui::vec2(168.0, 126.0);

pub struct ManagerWindow {
    config: Config,
    /// Running web proxies keyed by instance index.
    proxies: HashMap<usize, Arc<WebProxy>>,
    /// Status pollers, one per instance (same order as `config.instances`).
    monitors: Vec<Option<Monitor>>,
    /// Preview textures keyed by instance index, with the preview sequence
    /// number they show.
    thumbs: HashMap<usize, (u64, egui::TextureHandle)>,
    editing: Option<(Option<usize>, Instance)>,
    confirm_delete: Option<usize>,
    confirm_power: Option<(usize, PowerAction)>,
    /// Password prompt for instances without a saved password: which
    /// instance, what to do afterwards, and the typed password.
    prompt: Option<(usize, Open, String)>,
    error: Option<String>,
    /// Short-lived confirmation at the bottom of the window.
    note: Option<(String, Instant)>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone, Copy)]
enum Open {
    Console,
    BootCaptures,
    Power(PowerAction),
    Identify(u8),
}

impl ManagerWindow {
    pub fn new() -> Self {
        let config = Config::load();
        let n = config.instances.len();
        Self {
            config,
            proxies: HashMap::new(),
            monitors: (0..n).map(|_| None).collect(),
            thumbs: HashMap::new(),
            editing: None,
            confirm_delete: None,
            confirm_power: None,
            prompt: None,
            error: None,
            note: None,
            notify: None,
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.config.save() {
            self.error = Some(format!("Could not save settings: {e:#}"));
        }
    }

    /// Start a status poller for every instance that has none yet.
    fn ensure_monitors(&mut self) {
        let Some(notify) = self.notify.clone() else { return };
        self.monitors.resize_with(self.config.instances.len(), || None);
        let s = self.config.settings.clone();
        for (i, m) in self.monitors.iter_mut().enumerate() {
            if m.is_none() {
                let inst = &self.config.instances[i];
                let client = Arc::new(WebClient::new(&inst.host, inst.https_port, &inst.username, &inst.password));
                *m = Some(Monitor::start(client, s.live_status, s.live_status && s.screen_previews, notify.clone()));
            }
        }
    }

    fn monitor(&self, i: usize) -> Option<&Monitor> {
        self.monitors.get(i).and_then(Option::as_ref)
    }

    fn apply_live_settings(&mut self) {
        let s = self.config.settings.clone();
        for m in self.monitors.iter().flatten() {
            m.set_live(s.live_status, s.live_status && s.screen_previews);
        }
        if !(s.live_status && s.screen_previews) {
            self.thumbs.clear();
        }
    }

    fn act(&mut self, idx: usize, what: Open, password: String, shell: &mut Shell) {
        let mut inst = self.config.instances[idx].clone();
        if inst.password.is_empty() {
            // Typed into the prompt: let the poller use it for this run too.
            if let Some(m) = self.monitor(idx) {
                m.set_password(&password);
            }
        }
        inst.password = password;
        match what {
            Open::Console => shell.open.push(Box::new(ConsoleWindow::new(inst))),
            Open::BootCaptures => shell.open.push(Box::new(PlaybackWindow::new(inst))),
            Open::Power(a) => {
                if let Some(m) = self.monitor(idx) {
                    m.power(a);
                }
            }
            Open::Identify(secs) => {
                if let Some(m) = self.monitor(idx) {
                    m.identify(secs);
                }
            }
        }
    }

    fn act_or_prompt(&mut self, idx: usize, what: Open, shell: &mut Shell) {
        let pw = self.config.instances[idx].password.clone();
        let known = self.monitor(idx).is_some_and(|m| m.client.has_password());
        if pw.is_empty() && !known {
            self.prompt = Some((idx, what, String::new()));
        } else if pw.is_empty() {
            // Entered earlier in this run.
            let pw = self.monitor(idx).map(|m| m.client.password()).unwrap_or_default();
            self.act(idx, what, pw, shell);
        } else {
            self.act(idx, what, pw, shell);
        }
    }

    fn toggle_proxy(&mut self, idx: usize) {
        if self.proxies.remove(&idx).is_some() {
            return;
        }
        let inst = &self.config.instances[idx];
        match WebProxy::start(&inst.host, inst.https_port, inst.proxy_port, &crate::config::config_dir()) {
            Ok(p) => {
                let url = p.url();
                self.proxies.insert(idx, Arc::new(p));
                if let Err(e) = open::that(&url) {
                    self.error = Some(format!("Proxy running at {url}, but the browser could not be opened: {e}"));
                }
            }
            Err(e) => self.error = Some(format!("Could not start the web proxy: {e:#}")),
        }
    }

    fn remove(&mut self, i: usize) {
        self.proxies.clear();
        self.thumbs.clear();
        if i < self.monitors.len() {
            self.monitors.remove(i);
        }
        self.config.instances.remove(i);
        self.save();
    }

    fn edit_dialog(&mut self, ctx: &egui::Context) {
        let Some((idx, inst)) = self.editing.as_mut() else { return };
        let mut done: Option<bool> = None;
        let title = if idx.is_some() { "Edit iDRAC" } else { "Add iDRAC" };
        egui::Modal::new(egui::Id::unique("edit")).show(ctx, |ui| {
            ui.heading(title);
            ui.add_space(6.0);
            egui::Grid::new("edit-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut inst.name).hint_text("optional").desired_width(240.0));
                ui.end_row();
                ui.label("Address");
                ui.add(egui::TextEdit::singleline(&mut inst.host).hint_text("192.168.1.120 or idrac.example").desired_width(240.0));
                ui.end_row();
                ui.label("HTTPS port");
                ui.add(egui::DragValue::new(&mut inst.https_port).range(1..=65535));
                ui.end_row();
                ui.label("Username");
                ui.add(egui::TextEdit::singleline(&mut inst.username).desired_width(240.0));
                ui.end_row();
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut inst.password).password(true).desired_width(240.0));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut inst.save_password, "Save password (stored in plain text)");
                ui.end_row();
                ui.label("Console");
                ui.checkbox(&mut inst.view_only, "Open in view-only mode (no keyboard or mouse)");
                ui.end_row();
                ui.label("Web proxy port");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut inst.proxy_port).range(0..=65535));
                    ui.weak("0 = pick a free port");
                });
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let ok = !inst.host.trim().is_empty() && !inst.username.trim().is_empty();
                if ui.add_enabled(ok, egui::Button::new("Save")).clicked() {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    done = Some(false);
                }
            });
        });
        match done {
            Some(true) => {
                let (idx, mut inst) = self.editing.take().unwrap();
                inst.host = inst.host.trim().to_string();
                inst.username = inst.username.trim().to_string();
                match idx {
                    Some(i) => {
                        let old = &self.config.instances[i];
                        if old.host != inst.host || old.https_port != inst.https_port {
                            self.proxies.remove(&i);
                        }
                        if old.host != inst.host || old.https_port != inst.https_port || old.username != inst.username || old.password != inst.password {
                            // New login details: start over with a new poller.
                            self.monitors[i] = None;
                            self.thumbs.remove(&i);
                        }
                        self.config.instances[i] = inst;
                    }
                    None => {
                        self.config.instances.push(inst);
                        self.monitors.push(None);
                    }
                }
                self.save();
            }
            Some(false) => self.editing = None,
            None => {}
        }
    }

    fn update_thumb(&mut self, ctx: &egui::Context, i: usize, seq_now: u64) {
        if self.thumbs.get(&i).is_some_and(|(seq, _)| *seq == seq_now) {
            return;
        }
        let ci = {
            let Some(m) = self.monitor(i) else { return };
            let s = m.state.lock().unwrap();
            match &s.preview {
                Some(img) => egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw()),
                None => {
                    drop(s);
                    self.thumbs.remove(&i);
                    return;
                }
            }
        };
        match self.thumbs.get_mut(&i) {
            Some((seq, t)) => {
                t.set(ci, egui::TextureOptions::LINEAR);
                *seq = seq_now;
            }
            None => {
                let t = ctx.load_texture(format!("thumb-{i}"), ci, egui::TextureOptions::LINEAR);
                self.thumbs.insert(i, (seq_now, t));
            }
        }
    }

    /// One server's card. Returns what the user clicked.
    fn row(&mut self, ui: &mut egui::Ui, i: usize) -> Option<Action> {
        let mut action = None;
        let inst = self.config.instances[i].clone();
        let st = self.monitor(i).map(|m| {
            let s = m.state.lock().unwrap();
            (snapshot(&s), s.preview_seq)
        });
        if let Some(seq) = st.as_ref().map(|(_, seq)| *seq) {
            self.update_thumb(ui.ctx(), i, seq);
        }
        let st = st.map(|(s, _)| s);
        let live = self.config.settings.live_status;
        let previews = live && self.config.settings.screen_previews;
        let status = st.as_ref().and_then(|s| s.status.clone());
        egui::Frame::group(ui.style()).inner_margin(10.0).corner_radius(6.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                if previews {
                    if self.thumbnail(ui, i, st.as_ref(), status.as_ref()) {
                        action = Some(Action::Console);
                    }
                    ui.add_space(6.0);
                }
                ui.vertical(|ui| {
                    // Title line with status on the right.
                    ui.horizontal(|ui| {
                        let health = status.as_ref().map_or(Health::Unknown, |s| s.health);
                        let (dot, tip) = health_dot(ui, health, status.as_ref(), st.as_ref());
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 5.0, dot);
                        resp.on_hover_text(tip);
                        ui.label(egui::RichText::new(inst.title()).strong().size(16.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            status_line(ui, live, &inst, st.as_ref());
                        });
                    });
                    let mut sub = format!("{}@{}{}", inst.username, inst.host, if inst.https_port == 443 { String::new() } else { format!(":{}", inst.https_port) });
                    if let Some(s) = &status {
                        if !s.model.is_empty() {
                            sub.push_str(&format!("  ·  {}", s.model));
                        }
                        if !s.service_tag.is_empty() {
                            sub.push_str(&format!("  ·  Service tag {}", s.service_tag));
                        }
                    }
                    ui.weak(sub);
                    if let Some(p) = self.proxies.get(&i) {
                        ui.horizontal(|ui| {
                            ui.weak("Web UI proxy:");
                            let url = p.url();
                            ui.hyperlink_to(&url, &url);
                            let n = p.active.load(Ordering::Relaxed);
                            if n > 0 {
                                ui.weak(format!("({n} open)"));
                            }
                        });
                    }
                    ui.add_space(if previews { 26.0 } else { 6.0 });
                    ui.horizontal(|ui| {
                        if ui.button("Console").on_hover_text("Open the virtual console").clicked() {
                            action = Some(Action::Console);
                        }
                        let running = self.proxies.contains_key(&i);
                        let label = if running { "Stop web proxy" } else { "Web UI" };
                        if ui.button(label).on_hover_text("Open the iDRAC web interface through a local TLS proxy").clicked() {
                            action = Some(Action::Proxy);
                        }
                        if ui.button("Boot captures").on_hover_text("Replay the server's last boot cycles recorded by the iDRAC").clicked() {
                            action = Some(Action::Captures);
                        }
                        let power_on = status.as_ref().and_then(|s| s.power_on);
                        ui.menu_button("Power", |ui| {
                            match power_on {
                                Some(true) => ui.weak("The server is on"),
                                Some(false) => ui.weak("The server is off"),
                                None => ui.weak("Power state unknown"),
                            };
                            ui.separator();
                            for a in PowerAction::ALL {
                                let possible = match (a, power_on) {
                                    (PowerAction::On, Some(true)) => false,
                                    (PowerAction::On, _) => true,
                                    (_, Some(false)) => false,
                                    _ => true,
                                };
                                if ui.add_enabled(possible, egui::Button::new(a.label())).clicked() {
                                    action = Some(Action::Power(a));
                                }
                            }
                        });
                        ui.menu_button("More", |ui| {
                            if ui.button("Blink identify LED (1 minute)").on_hover_text("Find the server in the rack by its blinking blue light").clicked() {
                                action = Some(Action::Identify(60));
                            }
                            if ui.button("Stop blinking").clicked() {
                                action = Some(Action::Identify(0));
                            }
                            ui.separator();
                            if ui.add_enabled(live, egui::Button::new("Refresh status")).clicked() {
                                action = Some(Action::Refresh);
                            }
                            if ui.button("Edit…").clicked() {
                                action = Some(Action::Edit);
                            }
                            if ui.button("Remove…").clicked() {
                                action = Some(Action::Remove);
                            }
                        });
                    });
                });
            });
        });
        action
    }

    /// The screen preview. Returns true when clicked.
    fn thumbnail(&self, ui: &mut egui::Ui, i: usize, st: Option<&MonitorState>, status: Option<&crate::health::Status>) -> bool {
        let (rect, resp) = ui.allocate_exact_size(THUMB, egui::Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, egui::Color32::from_gray(12));
        let mut tip = None;
        if let Some((_, t)) = self.thumbs.get(&i) {
            let [w, h] = t.size();
            let s = (THUMB.x / w as f32).min(THUMB.y / h as f32);
            let r = egui::Rect::from_center_size(rect.center(), egui::vec2(w as f32 * s, h as f32 * s));
            painter.image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            tip = Some(t.clone());
        } else {
            let text = match (st, status) {
                (Some(s), _) if s.paused.is_some() => "No preview",
                (_, Some(s)) if s.power_on == Some(false) => "Server is off",
                (Some(s), _) if s.error.is_some() => "No preview",
                (Some(s), _) if s.busy => "Loading…",
                _ => "Loading…",
            };
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), egui::Color32::from_gray(140));
        }
        if resp.hovered() {
            painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.5, ui.visuals().selection.stroke.color), egui::StrokeKind::Inside);
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let resp = match tip {
            Some(t) => {
                let age = st.and_then(|s| s.preview_at).map(ago).unwrap_or_default();
                resp.on_hover_ui(|ui| {
                    // The iDRAC's preview is small; show it pixel for pixel.
                    let [w, h] = t.size();
                    let ppp = ui.ctx().pixels_per_point();
                    ui.image(egui::load::SizedTexture::new(t.id(), egui::vec2(w as f32 / ppp, h as f32 / ppp)));
                    ui.weak(format!("Screen {age}. Click to open the console."));
                })
            }
            None => resp.on_hover_text("Click to open the console"),
        };
        resp.clicked()
    }

    fn power_dialog(&mut self, ctx: &egui::Context, shell: &mut Shell) {
        let Some((i, a)) = self.confirm_power else { return };
        let mut close = false;
        let mut go = false;
        egui::Modal::new(egui::Id::unique("power-confirm")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading("Power control");
            ui.add_space(4.0);
            ui.label(format!("Send \"{}\" to {}?", a.label(), self.config.instances[i].title()));
            let warn = match a {
                PowerAction::Off | PowerAction::Cycle => Some("The server loses power immediately, without shutting down its operating system."),
                PowerAction::Reset => Some("The server restarts immediately, without shutting down its operating system."),
                PowerAction::GracefulShutdown => Some("Asks the operating system to shut down, like a short press of the power button."),
                PowerAction::On => None,
            };
            if let Some(w) = warn {
                ui.weak(w);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(a.label()).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if go {
            self.confirm_power = None;
            self.act_or_prompt(i, Open::Power(a), shell);
        } else if close {
            self.confirm_power = None;
        }
    }

    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Options", |ui| {
            let mut changed = false;
            changed |= ui
                .checkbox(&mut self.config.settings.live_status, "Live status")
                .on_hover_text("Show each server's power state, health, temperature and power use, updated every 30 seconds")
                .changed();
            ui.add_enabled_ui(self.config.settings.live_status, |ui| {
                changed |= ui
                    .checkbox(&mut self.config.settings.screen_previews, "Screen previews")
                    .on_hover_text("Show a small picture of each server's screen, updated every minute")
                    .changed();
            });
            if changed {
                self.apply_live_settings();
                self.save();
            }
            ui.separator();
            if ui.add_enabled(self.config.settings.live_status, egui::Button::new("Refresh all")).clicked() {
                for m in self.monitors.iter().flatten() {
                    m.refresh();
                }
            }
        });
    }
}

/// What a card's buttons ask for.
#[derive(Clone, Copy)]
enum Action {
    Console,
    Proxy,
    Captures,
    Power(PowerAction),
    Identify(u8),
    Refresh,
    Edit,
    Remove,
}

/// Copy of the parts of a monitor's state the UI needs (so the lock is not
/// held while drawing). The preview picture is left out; it is turned into
/// a texture only when it changes.
fn snapshot(s: &MonitorState) -> MonitorState {
    MonitorState {
        status: s.status.clone(),
        status_at: s.status_at,
        error: s.error.clone(),
        paused: s.paused.clone(),
        preview: None,
        preview_seq: s.preview_seq,
        preview_at: s.preview_at,
        busy: s.busy,
        action_result: s.action_result.clone(),
    }
}

fn ago(t: Instant) -> String {
    let s = t.elapsed().as_secs();
    match s {
        0..=9 => "just now".into(),
        10..=59 => format!("{s} s ago"),
        _ => format!("{} min ago", s / 60),
    }
}

fn health_dot(ui: &egui::Ui, h: Health, status: Option<&crate::health::Status>, st: Option<&MonitorState>) -> (egui::Color32, String) {
    let color = match h {
        Health::Ok => egui::Color32::from_rgb(70, 190, 90),
        Health::Warning => egui::Color32::from_rgb(230, 170, 40),
        Health::Critical => egui::Color32::from_rgb(220, 60, 60),
        Health::Unknown => ui.visuals().weak_text_color(),
    };
    let mut tip = match (status, st.and_then(|s| s.error.as_ref())) {
        (_, Some(e)) => e.clone(),
        (Some(_), None) => format!("Health: {}", h.label()),
        (None, None) => "Health unknown".into(),
    };
    if let Some(s) = status {
        for p in &s.problems {
            tip.push('\n');
            tip.push_str(p);
        }
    }
    if let Some(t) = st.and_then(|s| s.status_at) {
        tip.push_str(&format!("\nUpdated {}", ago(t)));
    }
    (color, tip)
}

/// "On · 25 °C · 259 W", or why there is no status.
fn status_line(ui: &mut egui::Ui, live: bool, inst: &Instance, st: Option<&MonitorState>) {
    if !live {
        return;
    }
    let Some(st) = st else { return };
    if let Some(p) = &st.paused {
        ui.colored_label(ui.visuals().error_fg_color, "Login failed").on_hover_text(format!("{p}\nFix the password with More → Edit."));
        return;
    }
    if st.busy {
        ui.spinner();
    }
    match (&st.status, &st.error) {
        (_, Some(e)) => {
            ui.colored_label(ui.visuals().warn_fg_color, "Unreachable").on_hover_text(e);
        }
        (Some(s), None) => {
            // Right-to-left layout: add the parts in reverse.
            if let Some(w) = s.watts {
                ui.label(format!("{w:.0} W")).on_hover_text("Current power use");
                ui.weak("·");
            }
            if let Some(t) = s.ambient_c {
                ui.label(format!("{t:.0} °C")).on_hover_text("Inlet (ambient) temperature");
                ui.weak("·");
            }
            match s.power_on {
                Some(true) => ui.colored_label(egui::Color32::from_rgb(70, 190, 90), "On"),
                Some(false) => ui.weak("Off"),
                None => ui.weak("Power unknown"),
            };
        }
        (None, None) if inst.password.is_empty() && !st.busy => {
            ui.weak("Status needs the password").on_hover_text("Save the password (More → Edit), or open the console once to enter it.");
        }
        (None, None) => {}
    }
}

impl WindowUi for ManagerWindow {
    fn attributes(&self) -> WindowAttributes {
        super::base_attributes().with_title("iDRAC6 Console Manager").with_inner_size(LogicalSize::new(900.0, 600.0))
    }

    fn is_main(&self) -> bool {
        true
    }

    fn created(&mut self, window: &Window, proxy: &EventLoopProxy<UserEvent>) {
        let (p, id): (EventLoopProxy<UserEvent>, WindowId) = (proxy.clone(), window.id());
        self.notify = Some(Arc::new(move || {
            let _ = p.send_event(UserEvent::Repaint(id, Duration::ZERO));
        }));
        self.ensure_monitors();
    }

    fn ui(&mut self, root: &mut egui::Ui, _window: &Window, shell: &mut Shell) {
        let ctx = root.ctx().clone();
        self.ensure_monitors();
        // Results of power and identify actions.
        for m in self.monitors.iter().flatten() {
            if let Some((msg, ok)) = m.state.lock().unwrap().action_result.take() {
                if ok {
                    self.note = Some((msg, Instant::now()));
                } else {
                    self.error = Some(msg);
                }
            }
        }
        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("iDRAC6 Console Manager");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Add iDRAC").clicked() {
                        self.editing = Some((None, Instance::default()));
                    }
                    self.settings_menu(ui);
                });
            });
            ui.add_space(6.0);
        });
        let note = self.note.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(6)).map(|(m, t)| (m.clone(), *t));
        if self.error.is_some() || note.is_some() {
            egui::Panel::bottom("err").show(root, |ui| {
                ui.add_space(2.0);
                if let Some(err) = self.error.clone() {
                    ui.horizontal(|ui| {
                        ui.colored_label(ui.visuals().error_fg_color, err);
                        if ui.small_button("Dismiss").clicked() {
                            self.error = None;
                        }
                    });
                }
                if let Some((m, t)) = note {
                    ui.label(m);
                    ctx.request_repaint_after(Duration::from_secs(6).saturating_sub(t.elapsed()));
                }
                ui.add_space(2.0);
            });
        }
        egui::CentralPanel::default().show(root, |ui| {
            if self.config.instances.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.heading("No iDRACs yet");
                    ui.add_space(4.0);
                    ui.weak("Add one with its address and the login you use for its web page.");
                    ui.add_space(10.0);
                    if ui.button("Add your first iDRAC").clicked() {
                        self.editing = Some((None, Instance::default()));
                    }
                });
                return;
            }
            let mut action: Option<(usize, Action)> = None;
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.add_space(4.0);
                for i in 0..self.config.instances.len() {
                    if let Some(a) = self.row(ui, i) {
                        action = Some((i, a));
                    }
                    ui.add_space(6.0);
                }
            });
            match action {
                Some((i, Action::Console)) => self.act_or_prompt(i, Open::Console, shell),
                Some((i, Action::Captures)) => self.act_or_prompt(i, Open::BootCaptures, shell),
                Some((i, Action::Proxy)) => self.toggle_proxy(i),
                Some((i, Action::Power(a))) => self.confirm_power = Some((i, a)),
                Some((i, Action::Identify(s))) => self.act_or_prompt(i, Open::Identify(s), shell),
                Some((i, Action::Refresh)) => {
                    if let Some(m) = self.monitor(i) {
                        m.refresh();
                    }
                }
                Some((i, Action::Edit)) => self.editing = Some((Some(i), self.config.instances[i].clone())),
                Some((i, Action::Remove)) => self.confirm_delete = Some(i),
                None => {}
            }
        });

        self.edit_dialog(&ctx);
        self.power_dialog(&ctx, shell);

        if let Some(i) = self.confirm_delete {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("del")).show(&ctx, |ui| {
                ui.label(format!("Remove {}?", self.config.instances[i].title()));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Remove").clicked() {
                        self.remove(i);
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.confirm_delete = None;
            }
        }

        if let Some((i, what, mut pw)) = self.prompt.take() {
            let mut keep = true;
            let mut go = false;
            egui::Modal::new(egui::Id::unique("pw")).show(&ctx, |ui| {
                ui.label(format!("Password for {}@{}", self.config.instances[i].username, self.config.instances[i].host));
                let r = ui.add(egui::TextEdit::singleline(&mut pw).password(true));
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            });
            if go {
                self.act(i, what, pw, shell);
            } else if keep {
                self.prompt = Some((i, what, pw));
            }
        }

        // Keep the "updated N s ago" texts and spinners current.
        if self.config.settings.live_status {
            ctx.request_repaint_after(Duration::from_secs(5));
        }
    }
}

impl Drop for ManagerWindow {
    fn drop(&mut self) {
        // Log out of every iDRAC before the app exits: in parallel, without
        // waiting for requests under way, and giving up after a few seconds
        // on unreachable ones.
        let threads: Vec<_> = self
            .monitors
            .drain(..)
            .flatten()
            .flat_map(|m| {
                let c = m.client.clone();
                // The poller logs out when it stops, but may be in the
                // middle of a slow request; log out alongside it and wait
                // for both.
                let poller = m.stop();
                let logout = std::thread::spawn(move || c.shutdown());
                poller.into_iter().chain([logout])
            })
            .collect();
        // The iDRAC answers one request at a time, so a logout can wait
        // behind a slow screen capture.
        let deadline = Instant::now() + Duration::from_secs(15);
        for t in threads {
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}
