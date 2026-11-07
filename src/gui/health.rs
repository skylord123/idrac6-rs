//! Health window: system overview, sensors, power use, the System Event
//! Log and the iDRAC's login sessions of one server.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use winit::dpi::LogicalSize;
use winit::event_loop::EventLoopProxy;
use winit::window::{Window, WindowAttributes, WindowId};

use super::{Shell, UserEvent, WindowUi};
use crate::config::Instance;
use crate::health::{self, Health, IdracSession, PowerStats, SelEntry, Sensor, SensorKind, Summary};
use crate::web::WebClient;

/// Sensors and power refresh this often while their tab is shown.
const REFRESH_EVERY: Duration = Duration::from_secs(30);

/// Data that is fetched in the background.
struct Load<T> {
    data: Option<T>,
    at: Option<Instant>,
    pending: Option<Receiver<anyhow::Result<T>>>,
    error: Option<String>,
}

impl<T> Default for Load<T> {
    fn default() -> Self {
        Self { data: None, at: None, pending: None, error: None }
    }
}

impl<T: Send + 'static> Load<T> {
    fn start(&mut self, client: &Arc<WebClient>, notify: &Arc<dyn Fn() + Send + Sync>, f: fn(&WebClient) -> anyhow::Result<T>) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (c, n) = (client.clone(), notify.clone());
        thread::spawn(move || {
            let _ = tx.send(f(&c));
            n();
        });
        self.pending = Some(rx);
    }

    fn poll(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(Ok(d)) => {
                self.data = Some(d);
                self.at = Some(Instant::now());
                self.error = None;
                self.pending = None;
            }
            Ok(Err(e)) => {
                self.error = Some(format!("{e:#}"));
                self.pending = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
        }
    }

    fn loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Never loaded. After a failure only Refresh tries again, so a wrong
    /// password does not turn into a stream of failed logins.
    fn missing(&self) -> bool {
        self.data.is_none() && self.error.is_none()
    }

    fn stale(&self) -> bool {
        self.error.is_none() && self.at.is_none_or(|t| t.elapsed() >= REFRESH_EVERY)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Sensors,
    Power,
    Log,
    Sessions,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogFilter {
    All,
    Problems,
    Critical,
}

pub struct HealthWindow {
    inst: Instance,
    client: Arc<WebClient>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    tab: Tab,
    summary: Load<Summary>,
    sensors: Load<Vec<Sensor>>,
    power: Load<PowerStats>,
    sel: Load<Vec<SelEntry>>,
    sessions: Load<Vec<IdracSession>>,
    /// Asking before logging a session out.
    confirm_close: Option<IdracSession>,
    log_filter: LogFilter,
    log_search: String,
    confirm_clear: bool,
    /// Clearing the log or saving it as CSV.
    log_job: Option<Receiver<anyhow::Result<String>>>,
    message: Option<String>,
}

impl HealthWindow {
    pub fn new(inst: Instance, client: Arc<WebClient>) -> Self {
        Self {
            inst,
            client,
            notify: None,
            tab: Tab::Overview,
            summary: Load::default(),
            sensors: Load::default(),
            power: Load::default(),
            sel: Load::default(),
            sessions: Load::default(),
            confirm_close: None,
            log_filter: LogFilter::All,
            log_search: String::new(),
            confirm_clear: false,
            log_job: None,
            message: None,
        }
    }

    /// Open on `tab` instead of the overview.
    pub fn with_tab(mut self, tab: Tab) -> Self {
        self.tab = tab;
        self
    }

    /// Fetch what the current tab shows, if it is missing or old.
    fn load_tab(&mut self, force: bool) {
        let Some(n) = self.notify.clone() else { return };
        let c = self.client.clone();
        match self.tab {
            Tab::Overview => {
                if force || self.summary.missing() {
                    self.summary.start(&c, &n, health::fetch_summary);
                }
                if force || self.sensors.stale() {
                    self.sensors.start(&c, &n, health::fetch_sensors);
                }
                if force || self.sel.missing() {
                    self.sel.start(&c, &n, health::fetch_sel);
                }
            }
            Tab::Sensors => {
                if force || self.sensors.stale() {
                    self.sensors.start(&c, &n, health::fetch_sensors);
                }
            }
            Tab::Power => {
                if force || self.power.stale() {
                    self.power.start(&c, &n, health::fetch_power);
                }
            }
            Tab::Log => {
                if force || self.sel.missing() {
                    self.sel.start(&c, &n, health::fetch_sel);
                }
            }
            Tab::Sessions => {
                if force || self.sessions.missing() {
                    self.sessions.start(&c, &n, health::fetch_sessions);
                }
            }
        }
    }

    fn loading(&self) -> bool {
        self.summary.loading() || self.sensors.loading() || self.power.loading() || self.sel.loading() || self.sessions.loading() || self.log_job.is_some()
    }

    fn tab_error(&self) -> Option<&String> {
        match self.tab {
            Tab::Overview => self.summary.error.as_ref().or(self.sensors.error.as_ref()),
            Tab::Sensors => self.sensors.error.as_ref(),
            Tab::Power => self.power.error.as_ref(),
            Tab::Log => self.sel.error.as_ref(),
            Tab::Sessions => self.sessions.error.as_ref(),
        }
    }

    fn tab_time(&self) -> Option<Instant> {
        match self.tab {
            Tab::Overview => self.summary.at,
            Tab::Sensors => self.sensors.at,
            Tab::Power => self.power.at,
            Tab::Log => self.sel.at,
            Tab::Sessions => self.sessions.at,
        }
    }

    fn overview(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.summary.data.clone() else {
            loading_note(ui, self.summary.loading());
            return;
        };
        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            section(ui, "System");
            egui::Grid::new("sys").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                let power = match s.power_on {
                    Some(true) => egui::RichText::new("On").color(health_color(ui, Health::Ok)),
                    Some(false) => egui::RichText::new("Off").weak(),
                    None => egui::RichText::new("Unknown").weak(),
                };
                ui.weak("Power");
                ui.label(power);
                ui.end_row();
                for (k, v) in [
                    ("Model", s.model.as_str()),
                    ("Revision", s.revision.as_str()),
                    ("Service tag", s.service_tag.as_str()),
                    ("Host name", s.host_name.as_str()),
                    ("Operating system", s.os.as_str()),
                    ("BIOS", s.bios.as_str()),
                    ("iDRAC firmware", s.firmware.as_str()),
                    ("iDRAC address", s.idrac_ip.as_str()),
                    ("iDRAC MAC", s.idrac_mac.as_str()),
                ] {
                    if !v.is_empty() {
                        ui.weak(k);
                        ui.label(v);
                        ui.end_row();
                    }
                }
            });

            let ui = &mut cols[1];
            section(ui, "Health");
            match &self.sensors.data {
                Some(sensors) => {
                    egui::Grid::new("groups").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                        for kind in SensorKind::ORDER {
                            let group: Vec<&Sensor> = sensors.iter().filter(|s| s.kind == kind).collect();
                            if group.is_empty() {
                                continue;
                            }
                            let h = Health::worst(group.iter().map(|s| s.health));
                            ui.horizontal(|ui| {
                                dot(ui, h);
                                ui.label(kind.title());
                            });
                            ui.label(group_summary(kind, &group, h));
                            ui.end_row();
                        }
                    });
                }
                None => loading_note(ui, self.sensors.loading()),
            }
        });
        ui.add_space(12.0);
        section(ui, "Latest events");
        match &self.sel.data {
            Some(events) if events.is_empty() => {
                ui.weak("The event log is empty.");
            }
            Some(events) => {
                for e in events.iter().take(6) {
                    event_row(ui, e);
                }
                ui.add_space(4.0);
                if ui.link(format!("Show all {} events", events.len())).clicked() {
                    self.tab = Tab::Log;
                }
            }
            None => loading_note(ui, self.sel.loading()),
        }
    }

    fn sensors_tab(&self, ui: &mut egui::Ui) {
        let Some(sensors) = &self.sensors.data else {
            loading_note(ui, self.sensors.loading());
            return;
        };
        for kind in SensorKind::ORDER {
            let group: Vec<&Sensor> = sensors.iter().filter(|s| s.kind == kind).collect();
            if group.is_empty() {
                continue;
            }
            ui.horizontal(|ui| {
                dot(ui, Health::worst(group.iter().map(|s| s.health)));
                ui.strong(kind.title());
            });
            ui.add_space(2.0);
            let thresholds = group.iter().any(|s| s.min_failure.is_some() || s.max_failure.is_some() || s.max_warning.is_some());
            const W: [f32; 4] = [300.0, 110.0, 170.0, 170.0];
            ui.horizontal(|ui| {
                cell(ui, W[0], |ui| ui.weak("Name"));
                cell(ui, W[1], |ui| ui.weak("Reading"));
                if thresholds {
                    cell(ui, W[2], |ui| ui.weak("Warning below / above"));
                    cell(ui, W[3], |ui| ui.weak("Failure below / above"));
                }
            });
            for (n, s) in group.iter().enumerate() {
                striped_row(ui, n, |ui| {
                    cell(ui, W[0], |ui| {
                        dot(ui, s.health);
                        ui.label(&s.name).on_hover_text(s.health.label())
                    });
                    let reading = egui::RichText::new(s.reading_text());
                    let reading = match s.health {
                        Health::Warning | Health::Critical => reading.color(health_color(ui, s.health)).strong(),
                        _ => reading,
                    };
                    cell(ui, W[1], |ui| ui.label(reading));
                    if kind == SensorKind::PowerSupply {
                        cell(ui, W[2] + W[3], |ui| ui.weak(s.detail.clone().unwrap_or_default()));
                    } else if thresholds {
                        cell(ui, W[2], |ui| ui.weak(range(&s.min_warning, &s.max_warning)));
                        cell(ui, W[3], |ui| ui.weak(range(&s.min_failure, &s.max_failure)));
                    }
                });
            }
            ui.add_space(14.0);
        }
    }

    fn power_tab(&self, ui: &mut egui::Ui) {
        let Some(p) = &self.power.data else {
            loading_note(ui, self.power.loading());
            return;
        };
        ui.horizontal(|ui| {
            if let Some(w) = p.watts {
                ui.label(egui::RichText::new(format!("{w:.0} W")).size(32.0).strong());
                ui.add_space(8.0);
            }
            ui.vertical(|ui| {
                ui.weak("Current power use");
                let amps: Vec<String> = p.amps.iter().map(|(n, a)| format!("{n}: {a:.1} A")).collect();
                if !amps.is_empty() {
                    ui.label(amps.join("    "));
                }
            });
        });
        ui.add_space(12.0);
        section(ui, "Power use over time");
        egui::Grid::new("pc").num_columns(4).striped(true).spacing([28.0, 6.0]).show(ui, |ui| {
            ui.weak("");
            ui.weak("Average");
            ui.weak("Highest");
            ui.weak("Lowest");
            ui.end_row();
            for h in &p.history {
                ui.label(h.label);
                ui.label(watts(h.average));
                ui.label(watts(h.max)).on_hover_text(&h.max_at);
                ui.label(watts(h.min)).on_hover_text(&h.min_at);
                ui.end_row();
            }
        });
        ui.add_space(12.0);
        section(ui, "Peaks and energy");
        egui::Grid::new("peaks").num_columns(3).spacing([28.0, 6.0]).show(ui, |ui| {
            if let Some(w) = p.peak_watts {
                ui.weak("Peak power");
                ui.label(format!("{w:.0} W"));
                ui.weak(format!("on {}", p.peak_watts_at));
                ui.end_row();
            }
            if let Some(a) = p.peak_amps {
                ui.weak("Peak current");
                ui.label(format!("{a:.1} A"));
                ui.weak(format!("on {}", p.peak_amps_at));
                ui.end_row();
            }
            if let Some(e) = p.energy_kwh {
                ui.weak("Energy used");
                ui.label(format!("{e:.1} kWh"));
                ui.weak(format!("since {}", p.energy_since));
                ui.end_row();
            }
        });
        if !p.peaks_since.is_empty() {
            ui.add_space(4.0);
            ui.weak(format!("Peaks recorded since {}.", p.peaks_since));
        }
    }

    fn log_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("filter")
                .selected_text(match self.log_filter {
                    LogFilter::All => "All events",
                    LogFilter::Problems => "Warnings and critical",
                    LogFilter::Critical => "Critical only",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.log_filter, LogFilter::All, "All events");
                    ui.selectable_value(&mut self.log_filter, LogFilter::Problems, "Warnings and critical");
                    ui.selectable_value(&mut self.log_filter, LogFilter::Critical, "Critical only");
                });
            ui.add(egui::TextEdit::singleline(&mut self.log_search).hint_text("Search").desired_width(200.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = self.log_job.is_some();
                let have = self.sel.data.as_ref().is_some_and(|e| !e.is_empty());
                if ui.add_enabled(!busy && have, egui::Button::new("Clear log…")).clicked() {
                    self.confirm_clear = true;
                }
                if ui.add_enabled(!busy && have, egui::Button::new("Save as CSV…")).clicked() {
                    self.save_csv();
                }
            });
        });
        ui.add_space(6.0);
        let Some(events) = &self.sel.data else {
            loading_note(ui, self.sel.loading());
            return;
        };
        let q = self.log_search.to_lowercase();
        let shown: Vec<&SelEntry> = events
            .iter()
            .filter(|e| match self.log_filter {
                LogFilter::All => true,
                LogFilter::Problems => matches!(e.severity, Health::Warning | Health::Critical),
                LogFilter::Critical => e.severity == Health::Critical,
            })
            .filter(|e| q.is_empty() || e.description.to_lowercase().contains(&q) || e.date.to_lowercase().contains(&q))
            .collect();
        ui.weak(if shown.len() == events.len() { format!("{} events, newest first", events.len()) } else { format!("{} of {} events", shown.len(), events.len()) });
        ui.separator();
        let row_h = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, row_h, shown.len(), |ui, range| {
            for e in &shown[range] {
                event_row(ui, e);
            }
        });
    }

    fn sessions_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("Everyone logged in to this iDRAC. The iDRAC6 allows only a few sessions at a time; close ones left over from closed browsers or viewers that crashed.");
        ui.add_space(8.0);
        let Some(list) = &self.sessions.data else {
            loading_note(ui, self.sessions.loading());
            return;
        };
        if list.is_empty() {
            ui.weak("No sessions.");
            return;
        }
        let mut close = None;
        egui::Grid::new("sessions").num_columns(5).striped(true).spacing([24.0, 8.0]).show(ui, |ui| {
            for h in ["Type", "User", "From", "ID", ""] {
                ui.label(egui::RichText::new(h).strong());
            }
            ui.end_row();
            for s in list {
                ui.label(&s.kind);
                ui.label(&s.user);
                ui.label(&s.ip);
                ui.weak(s.id.to_string());
                if s.own {
                    ui.weak("This app").on_hover_text("The session idrac6 uses to read this list");
                } else if s.killable {
                    if ui.button("Close").on_hover_text("Log this session out").clicked() {
                        close = Some(s.clone());
                    }
                } else {
                    ui.weak("Can't be closed");
                }
                ui.end_row();
            }
        });
        if close.is_some() {
            self.confirm_close = close;
        }
    }

    fn close_session(&mut self, id: u32) {
        let (tx, rx) = mpsc::channel();
        let (c, n) = (self.client.clone(), self.notify.clone());
        thread::spawn(move || {
            let _ = tx.send(health::close_session(&c, id).map(|()| format!("Session {id} was closed.")));
            if let Some(n) = n {
                n();
            }
        });
        self.log_job = Some(rx);
    }

    fn save_csv(&mut self) {
        let name = format!("{}-event-log.csv", self.inst.title().replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "_"));
        let Some(path) = rfd::FileDialog::new().set_file_name(&name).add_filter("CSV", &["csv"]).save_file() else { return };
        let (tx, rx) = mpsc::channel();
        let (c, n) = (self.client.clone(), self.notify.clone());
        thread::spawn(move || {
            let r = health::sel_csv(&c).and_then(|csv| {
                std::fs::write(&path, csv)?;
                Ok(format!("Saved {}", path.display()))
            });
            let _ = tx.send(r);
            if let Some(n) = n {
                n();
            }
        });
        self.log_job = Some(rx);
    }

    fn clear_log(&mut self) {
        let (tx, rx) = mpsc::channel();
        let (c, n) = (self.client.clone(), self.notify.clone());
        thread::spawn(move || {
            let _ = tx.send(health::clear_sel(&c).map(|()| "The System Event Log was cleared.".to_string()));
            if let Some(n) = n {
                n();
            }
        });
        self.log_job = Some(rx);
    }

    fn poll_job(&mut self) {
        let Some(rx) = &self.log_job else { return };
        match rx.try_recv() {
            Ok(r) => {
                let cleared = r.as_ref().is_ok_and(|m| m.contains("cleared"));
                let session = self.tab == Tab::Sessions;
                // A closed session just disappears from the list.
                if !(session && r.is_ok()) {
                    self.message = Some(match r {
                        Ok(m) => m,
                        Err(e) => format!("{e:#}"),
                    });
                }
                self.log_job = None;
                if cleared {
                    self.sel.data = None;
                    self.load_tab(true);
                }
                if session {
                    self.load_tab(true);
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.log_job = None,
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.confirm_clear {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("clear-sel")).show(ctx, |ui| {
                ui.set_max_width(380.0);
                ui.heading("Clear the event log?");
                ui.label(format!("This permanently deletes all {} entries in the System Event Log of {}.", self.sel.data.as_ref().map_or(0, Vec::len), self.inst.title()));
                ui.weak("Save it as CSV first if you may need it.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Clear log").clicked() {
                        self.clear_log();
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.confirm_clear = false;
            }
        }
        if let Some(s) = self.confirm_close.clone() {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("close-session")).show(ctx, |ui| {
                ui.set_max_width(400.0);
                ui.heading("Close this session?");
                ui.label(format!("{} session {} of {} from {}.", s.kind, s.id, s.user, s.ip));
                ui.weak("Whoever is using it is logged out. Unsaved changes in it are lost.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Close session").clicked() {
                        self.close_session(s.id);
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.confirm_close = None;
            }
        }
        if let Some(m) = self.message.clone() {
            egui::Modal::new(egui::Id::unique("health-msg")).show(ctx, |ui| {
                ui.label(m);
                if ui.button("OK").clicked() {
                    self.message = None;
                }
            });
        }
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(egui::RichText::new(title).strong().size(15.0));
    ui.add_space(4.0);
}

fn loading_note(ui: &mut egui::Ui, loading: bool) {
    ui.horizontal(|ui| {
        if loading {
            ui.spinner();
            ui.weak("Loading…");
        } else {
            ui.weak("Not available.");
        }
    });
}

fn health_color(ui: &egui::Ui, h: Health) -> egui::Color32 {
    match h {
        Health::Ok => egui::Color32::from_rgb(70, 190, 90),
        Health::Warning => egui::Color32::from_rgb(230, 170, 40),
        Health::Critical => egui::Color32::from_rgb(220, 60, 60),
        Health::Unknown => ui.visuals().weak_text_color(),
    }
}

fn dot(ui: &mut egui::Ui, h: Health) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, health_color(ui, h));
}

fn event_row(ui: &mut egui::Ui, e: &SelEntry) {
    ui.horizontal(|ui| {
        cell(ui, 86.0, |ui| {
            dot(ui, e.severity);
            ui.label(egui::RichText::new(e.severity.label()).color(health_color(ui, e.severity)))
        });
        cell(ui, 180.0, |ui| ui.weak(&e.date));
        ui.add(egui::Label::new(&e.description).truncate()).on_hover_text(&e.description);
    });
}

/// A fixed-width, left-aligned table cell.
fn cell<R>(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui) -> R) {
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(egui::vec2(width, h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(width);
        ui.set_max_width(width);
        add(ui);
    });
}

/// A table row with a faint background on every other row.
fn striped_row(ui: &mut egui::Ui, n: usize, add: impl FnOnce(&mut egui::Ui)) {
    let bg = ui.painter().add(egui::Shape::Noop);
    let rect = ui.horizontal(add).response.rect;
    if n % 2 == 0 {
        let fill = ui.visuals().faint_bg_color;
        ui.painter().set(bg, egui::Shape::rect_filled(rect.expand2(egui::vec2(4.0, 1.0)), 2.0, fill));
    }
}

fn range(lo: &Option<String>, hi: &Option<String>) -> String {
    match (lo, hi) {
        (None, None) => String::new(),
        (Some(l), None) => format!("{l} / –"),
        (None, Some(h)) => format!("– / {h}"),
        (Some(l), Some(h)) => format!("{l} / {h}"),
    }
}

fn watts(v: Option<f64>) -> String {
    v.map(|w| format!("{w:.0} W")).unwrap_or_else(|| "–".into())
}

/// One line per sensor group on the overview: "25 °C" / "5 fans, 3480–3600 RPM".
fn group_summary(kind: SensorKind, group: &[&Sensor], h: Health) -> String {
    let bad = group.iter().filter(|s| matches!(s.health, Health::Warning | Health::Critical)).count();
    let base = match kind {
        SensorKind::Temperature | SensorKind::PowerDraw if group.len() == 1 => group[0].reading_text(),
        SensorKind::Fan => {
            let rpm: Vec<f64> = group.iter().filter_map(|s| s.numeric()).collect();
            let (lo, hi) = rpm.iter().fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            if rpm.is_empty() {
                format!("{} fans", group.len())
            } else if lo == hi {
                format!("{} fans, {lo:.0} RPM", group.len())
            } else {
                format!("{} fans, {lo:.0}–{hi:.0} RPM", group.len())
            }
        }
        SensorKind::PowerSupply => format!("{} present", group.iter().filter(|s| s.reading.eq_ignore_ascii_case("present")).count()),
        SensorKind::Intrusion | SensorKind::Battery if group.len() == 1 => group[0].reading.clone(),
        _ => format!("{} sensors", group.len()),
    };
    match (bad, h) {
        (0, _) => base,
        (n, h) => format!("{base} ({n} {})", h.label().to_lowercase()),
    }
}

impl WindowUi for HealthWindow {
    fn attributes(&self) -> WindowAttributes {
        super::base_attributes().with_title(format!("{} - Health", self.inst.title())).with_inner_size(LogicalSize::new(900.0, 640.0))
    }

    fn created(&mut self, window: &Window, proxy: &EventLoopProxy<UserEvent>) {
        let (p, id): (EventLoopProxy<UserEvent>, WindowId) = (proxy.clone(), window.id());
        self.notify = Some(Arc::new(move || {
            let _ = p.send_event(UserEvent::Repaint(id, Duration::ZERO));
        }));
        self.load_tab(false);
    }

    fn ui(&mut self, root: &mut egui::Ui, _window: &Window, _shell: &mut Shell) {
        let ctx = root.ctx().clone();
        self.summary.poll();
        self.sensors.poll();
        self.power.poll();
        self.sel.poll();
        self.sessions.poll();
        self.poll_job();
        self.load_tab(false);
        egui::Panel::top("tabs").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (t, label) in [(Tab::Overview, "Overview"), (Tab::Sensors, "Sensors"), (Tab::Power, "Power"), (Tab::Log, "Event log"), (Tab::Sessions, "Sessions")] {
                    if ui.selectable_label(self.tab == t, egui::RichText::new(label).size(15.0)).clicked() && self.tab != t {
                        self.tab = t;
                        self.load_tab(false);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!self.loading(), egui::Button::new("Refresh")).clicked() {
                        self.load_tab(true);
                    }
                    if self.loading() {
                        ui.spinner();
                    } else if let Some(t) = self.tab_time() {
                        let s = t.elapsed().as_secs();
                        ui.weak(if s < 10 { "Updated just now".to_string() } else if s < 60 { format!("Updated {s} s ago") } else { format!("Updated {} min ago", s / 60) });
                    }
                });
            });
            ui.add_space(6.0);
        });
        if let Some(e) = self.tab_error().cloned() {
            egui::Panel::bottom("health-err").show(root, |ui| {
                ui.colored_label(ui.visuals().error_fg_color, e);
            });
        }
        egui::CentralPanel::default().show(root, |ui| match self.tab {
            Tab::Log => self.log_tab(ui),
            tab => {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.add_space(4.0);
                    match tab {
                        Tab::Overview => self.overview(ui),
                        Tab::Sensors => self.sensors_tab(ui),
                        Tab::Sessions => self.sessions_tab(ui),
                        _ => self.power_tab(ui),
                    }
                });
            }
        });
        self.dialogs(&ctx);
        // Live tabs refresh themselves.
        if matches!(self.tab, Tab::Sensors | Tab::Power | Tab::Overview) {
            ctx.request_repaint_after(Duration::from_secs(5));
        }
    }
}
