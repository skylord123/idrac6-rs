//! Virtual Media panel inside the console window.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::kvm::session::KvmSession;
use crate::vmedia::{DriveKind, VmConfig, VmEvent, VmImage, VmSession};

enum State {
    Idle,
    /// Waiting for the KVM channel to hand out VM credentials.
    WaitingCredentials,
    Connecting,
    Connected(Arc<VmSession>),
}

pub struct VmPanel {
    pub open: bool,
    state: State,
    tx: Sender<VmEvent>,
    rx: Receiver<VmEvent>,
    session_rx: Option<Receiver<anyhow::Result<VmSession>>>,
    mapped: HashMap<DriveKind, Mapped>,
    read_only: bool,
    preempt: bool,
    forced_ro: bool,
    locked: bool,
    log: Vec<String>,
}

struct Mapped {
    path: PathBuf,
    read: u64,
    written: u64,
}

impl VmPanel {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            open: false,
            state: State::Idle,
            tx,
            rx,
            session_rx: None,
            mapped: HashMap::new(),
            read_only: true,
            preempt: false,
            forced_ro: false,
            locked: false,
            log: Vec::new(),
        }
    }

    fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        log::info!("virtual media: {s}");
        self.log.push(s);
        if self.log.len() > 50 {
            self.log.remove(0);
        }
    }

    /// Called by the console window when the KVM channel delivered the VM
    /// credentials.
    pub fn credentials(&mut self, host: &str, port: u16, user: String, password: String, notify: Arc<dyn Fn() + Send + Sync>) {
        if !matches!(self.state, State::WaitingCredentials) {
            return;
        }
        self.state = State::Connecting;
        let cfg = VmConfig { host: host.to_string(), port, username: user, password, preempt: self.preempt };
        let (tx, rx) = mpsc::channel();
        let events = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(VmSession::connect(cfg, events));
            notify();
        });
        self.session_rx = Some(rx);
    }

    pub fn disconnect(&mut self) {
        // A login still in progress: dropping its receiver drops the
        // session it returns, which disconnects it.
        self.session_rx = None;
        if let State::Connected(s) = std::mem::replace(&mut self.state, State::Idle) {
            // A map/unmap thread may still hold a reference, so ask the
            // worker to close; dropping the last reference joins it.
            match Arc::try_unwrap(s) {
                Ok(s) => s.disconnect(),
                Err(s) => s.request_disconnect(),
            }
        }
        // Late events from the old session must not reach a new one.
        (self.tx, self.rx) = mpsc::channel();
        self.mapped.clear();
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.session_rx {
            if let Ok(r) = rx.try_recv() {
                self.session_rx = None;
                match r {
                    Ok(s) => self.state = State::Connected(Arc::new(s)),
                    Err(e) => {
                        self.state = State::Idle;
                        self.note(format!("Connection failed: {e:#}"));
                    }
                }
            }
        }
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                VmEvent::Connected { drives } => self.note(format!("Connected ({} virtual drive slots)", drives.len())),
                VmEvent::Mapped { kind, path } => {
                    self.note(format!("Mapped {} as {}", path.display(), kind_label(kind)));
                    self.mapped.insert(kind, Mapped { path, read: 0, written: 0 });
                }
                VmEvent::Unmapped { kind, reason } => {
                    self.mapped.remove(&kind);
                    self.note(match reason {
                        Some(r) => format!("{} unmapped: {r}", kind_label(kind)),
                        None => format!("{} unmapped", kind_label(kind)),
                    });
                }
                VmEvent::Stats { kind, read_bytes, write_bytes } => {
                    if let Some(m) = self.mapped.get_mut(&kind) {
                        m.read = read_bytes;
                        m.written = write_bytes;
                    }
                }
                VmEvent::ReadOnlyForced(b) => self.forced_ro = b,
                VmEvent::LockedToKvm(b) => self.locked = b,
                VmEvent::Error(e) => self.note(format!("Error: {e}")),
                VmEvent::Disconnected(r) => {
                    self.note(format!("Disconnected: {r}"));
                    self.state = State::Idle;
                    self.mapped.clear();
                }
            }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, kvm: Option<&KvmSession>) {
        self.poll();
        if !self.open {
            return;
        }
        let mut open = self.open;
        egui::Window::new("Virtual Media").open(&mut open).resizable(true).default_width(460.0).show(ctx, |ui| {
            match &self.state {
                State::Idle => {
                    ui.label("Attach an ISO or disk image from this computer to the server as a USB CD/DVD or disk.");
                    ui.checkbox(&mut self.preempt, "Take over if another user has Virtual Media open");
                    let enabled = kvm.is_some();
                    if ui.add_enabled(enabled, egui::Button::new("Connect Virtual Media")).clicked() {
                        if let Some(k) = kvm {
                            k.request_vm_credentials();
                            self.state = State::WaitingCredentials;
                            self.note("Requesting virtual media credentials…");
                        }
                    }
                    if !enabled {
                        ui.weak("Connect the console first.");
                    }
                }
                State::WaitingCredentials | State::Connecting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Connecting…");
                    });
                }
                State::Connected(s) => {
                    let s = s.clone();
                    if self.forced_ro {
                        ui.colored_label(ui.visuals().warn_fg_color, "The iDRAC forces all drives read-only.");
                    }
                    if self.locked {
                        ui.weak("Virtual Media is locked to the console session and closes with it.");
                    }
                    egui::Grid::new("vm").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
                        for kind in [DriveKind::Cd, DriveKind::Floppy] {
                            ui.strong(kind_label(kind));
                            if let Some(m) = self.mapped.get(&kind) {
                                ui.vertical(|ui| {
                                    ui.label(m.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
                                    ui.weak(format!("read {} · written {}", human(m.read), human(m.written)));
                                });
                                if ui.button("Unmap").clicked() {
                                    let (s, tx) = (s.clone(), self.tx.clone());
                                    thread::spawn(move || {
                                        if let Err(e) = s.unmap(kind) {
                                            let _ = tx.send(VmEvent::Error(format!("{e:#}")));
                                        }
                                    });
                                }
                            } else {
                                ui.weak("not mapped");
                                if ui.button("Map image…").clicked() {
                                    let dialog = match kind {
                                        DriveKind::Cd => rfd::FileDialog::new().add_filter("CD/DVD image", &["iso"]).add_filter("All files", &["*"]),
                                        DriveKind::Floppy => rfd::FileDialog::new().add_filter("Disk image", &["img", "ima", "bin", "raw"]).add_filter("All files", &["*"]),
                                    };
                                    if let Some(path) = dialog.pick_file() {
                                        let image = VmImage { path, kind, read_only: self.read_only || kind == DriveKind::Cd };
                                        let (s, tx) = (s.clone(), self.tx.clone());
                                        thread::spawn(move || {
                                            if let Err(e) = s.map(image) {
                                                let _ = tx.send(VmEvent::Error(format!("{e:#}")));
                                            }
                                        });
                                    }
                                }
                            }
                            ui.end_row();
                        }
                    });
                    ui.checkbox(&mut self.read_only, "Map disk images read-only");
                    ui.horizontal(|ui| {
                        if ui.button("USB reset").on_hover_text("Re-plugs the virtual USB devices. Also resets the virtual keyboard and mouse.").clicked() {
                            if let Err(e) = s.usb_reset() {
                                self.note(format!("USB reset failed: {e:#}"));
                            }
                        }
                        if ui.button("Disconnect").clicked() {
                            self.disconnect();
                            self.note("Disconnected");
                        }
                    });
                }
            }
            if !self.log.is_empty() {
                ui.separator();
                egui::ScrollArea::vertical().max_height(120.0).stick_to_bottom(true).show(ui, |ui| {
                    for l in &self.log {
                        ui.weak(l);
                    }
                });
            }
        });
        self.open = open;
    }
}

fn kind_label(k: DriveKind) -> &'static str {
    match k {
        DriveKind::Cd => "Virtual CD/DVD",
        DriveKind::Floppy => "Virtual floppy / disk",
    }
}

fn human(b: u64) -> String {
    const U: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 3 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}
