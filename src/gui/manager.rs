//! Main window: the list of saved iDRACs.

use std::collections::HashMap;
use std::sync::Arc;

use winit::dpi::LogicalSize;
use winit::window::{Window, WindowAttributes};

use super::playback::PlaybackWindow;
use super::{ConsoleWindow, Shell, WindowUi};
use crate::config::{Config, Instance};
use crate::proxy::WebProxy;

pub struct ManagerWindow {
    config: Config,
    /// Running web proxies keyed by instance index.
    proxies: HashMap<usize, Arc<WebProxy>>,
    editing: Option<(Option<usize>, Instance)>,
    confirm_delete: Option<usize>,
    /// Password prompt for instances without a saved password: which
    /// instance, what to open afterwards, and the typed password.
    prompt: Option<(usize, Open, String)>,
    error: Option<String>,
}

#[derive(Clone, Copy)]
enum Open {
    Console,
    BootCaptures,
}

impl ManagerWindow {
    pub fn new() -> Self {
        Self {
            config: Config::load(),
            proxies: HashMap::new(),
            editing: None,
            confirm_delete: None,
            prompt: None,
            error: None,
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.config.save() {
            self.error = Some(format!("Could not save settings: {e:#}"));
        }
    }

    fn open(&mut self, idx: usize, what: Open, password: String, shell: &mut Shell) {
        let mut inst = self.config.instances[idx].clone();
        inst.password = password;
        match what {
            Open::Console => shell.open.push(Box::new(ConsoleWindow::new(inst))),
            Open::BootCaptures => shell.open.push(Box::new(PlaybackWindow::new(inst))),
        }
    }

    fn open_or_prompt(&mut self, idx: usize, what: Open, shell: &mut Shell) {
        let pw = self.config.instances[idx].password.clone();
        if pw.is_empty() {
            self.prompt = Some((idx, what, String::new()));
        } else {
            self.open(idx, what, pw, shell);
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
                        if self.config.instances[i].host != inst.host || self.config.instances[i].https_port != inst.https_port {
                            self.proxies.remove(&i);
                        }
                        self.config.instances[i] = inst;
                    }
                    None => self.config.instances.push(inst),
                }
                self.save();
            }
            Some(false) => self.editing = None,
            None => {}
        }
    }
}

impl WindowUi for ManagerWindow {
    fn attributes(&self) -> WindowAttributes {
        super::base_attributes().with_title("iDRAC6 Console Manager").with_inner_size(LogicalSize::new(760.0, 420.0))
    }

    fn is_main(&self) -> bool {
        true
    }

    fn ui(&mut self, root: &mut egui::Ui, _window: &Window, shell: &mut Shell) {
        let ctx = root.ctx().clone();
        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("iDRAC6 Console Manager");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Add iDRAC").clicked() {
                        self.editing = Some((None, Instance::default()));
                    }
                });
            });
            ui.add_space(4.0);
        });
        if let Some(err) = self.error.clone() {
            egui::Panel::bottom("err").show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, err);
                    if ui.small_button("Dismiss").clicked() {
                        self.error = None;
                    }
                });
            });
        }
        egui::CentralPanel::default().show(root, |ui| {
            if self.config.instances.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.label("No iDRACs yet.");
                    if ui.button("Add your first iDRAC").clicked() {
                        self.editing = Some((None, Instance::default()));
                    }
                });
                return;
            }
            let mut action: Option<(usize, &'static str)> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("instances").num_columns(3).striped(true).spacing([16.0, 10.0]).show(ui, |ui| {
                    for (i, inst) in self.config.instances.iter().enumerate() {
                        ui.vertical(|ui| {
                            ui.set_min_width(200.0);
                            ui.strong(inst.title());
                            ui.weak(format!("{}@{}{}", inst.username, inst.host, if inst.https_port == 443 { String::new() } else { format!(":{}", inst.https_port) }));
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Console").on_hover_text("Open the virtual console").clicked() {
                                action = Some((i, "console"));
                            }
                            let running = self.proxies.contains_key(&i);
                            let label = if running { "Stop web proxy" } else { "Web UI" };
                            if ui.button(label).on_hover_text("Open the iDRAC web interface through a local TLS proxy").clicked() {
                                action = Some((i, "proxy"));
                            }
                            if ui.button("Boot captures").on_hover_text("Replay the server's last boot cycles recorded by the iDRAC").clicked() {
                                action = Some((i, "captures"));
                            }
                            if let Some(p) = self.proxies.get(&i) {
                                let url = p.url();
                                ui.hyperlink_to(&url, &url);
                                let n = p.active.load(std::sync::atomic::Ordering::Relaxed);
                                if n > 0 {
                                    ui.weak(format!("{n} open"));
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Edit").clicked() {
                                action = Some((i, "edit"));
                            }
                            if ui.button("Remove").clicked() {
                                action = Some((i, "remove"));
                            }
                        });
                        ui.end_row();
                    }
                });
            });
            match action {
                Some((i, "console")) => self.open_or_prompt(i, Open::Console, shell),
                Some((i, "captures")) => self.open_or_prompt(i, Open::BootCaptures, shell),
                Some((i, "proxy")) => self.toggle_proxy(i),
                Some((i, "edit")) => self.editing = Some((Some(i), self.config.instances[i].clone())),
                Some((i, "remove")) => self.confirm_delete = Some(i),
                _ => {}
            }
        });

        self.edit_dialog(&ctx);

        if let Some(i) = self.confirm_delete {
            let mut close = false;
            egui::Modal::new(egui::Id::unique("del")).show(&ctx, |ui| {
                ui.label(format!("Remove {}?", self.config.instances[i].title()));
                ui.horizontal(|ui| {
                    if ui.button("Remove").clicked() {
                        self.proxies.clear();
                        self.config.instances.remove(i);
                        self.save();
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
                self.open(i, what, pw, shell);
            } else if keep {
                self.prompt = Some((i, what, pw));
            }
        }
    }
}
