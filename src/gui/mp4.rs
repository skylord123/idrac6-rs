//! Shared pieces for MP4 videos: the "FFmpeg is needed" dialog, the export
//! progress and the "Video saved" dialog.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::video::mp4::{self, Export, Ffmpeg, Missing};

const DOWNLOAD_URL: &str = "https://ffmpeg.org/download.html";

/// What the user chose in [`FfmpegDialog`].
pub enum Choice {
    /// FFmpeg was found after "Check again".
    Found(Ffmpeg),
    /// The dialog's alternative button (e.g. record .dvcrec instead).
    Alternative,
    Close,
}

/// Explains that FFmpeg is missing and how to install it.
pub struct FfmpegDialog {
    missing: Missing,
    /// Extra line about what still works without FFmpeg.
    note: Option<&'static str>,
    /// Label of an alternative action button.
    alternative: Option<&'static str>,
    /// "Check again" did not find it.
    still_missing: bool,
}

impl FfmpegDialog {
    pub fn new(missing: Missing) -> Self {
        Self { missing, note: None, alternative: None, still_missing: false }
    }

    pub fn note(mut self, note: &'static str) -> Self {
        self.note = Some(note);
        self
    }

    pub fn alternative(mut self, label: &'static str) -> Self {
        self.alternative = Some(label);
        self
    }

    /// Show the dialog; `Some` when it should close.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Choice> {
        let mut choice = None;
        egui::Modal::new(egui::Id::unique("ffmpeg-missing")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            ui.heading("FFmpeg is needed for MP4 videos");
            ui.add_space(6.0);
            match &self.missing {
                Missing::NotFound => {
                    ui.label("idrac6 uses FFmpeg to make MP4 videos, and FFmpeg is not installed on this computer.");
                }
                Missing::NoEncoder(p) => {
                    ui.label(format!(
                        "idrac6 uses FFmpeg to make MP4 videos, but the FFmpeg at {} can't write H.264 or MPEG-4 video. Install a full FFmpeg build.",
                        p.display()
                    ));
                }
            }
            ui.add_space(8.0);
            install_help(ui);
            if let Some(n) = self.note {
                ui.add_space(8.0);
                ui.label(n);
            }
            if self.still_missing {
                ui.add_space(8.0);
                let hint = if cfg!(windows) {
                    "Still not found. If you just installed it, restart idrac6 so it sees the new PATH."
                } else {
                    "Still not found. Make sure the ffmpeg command works in a terminal."
                };
                ui.colored_label(ui.visuals().warn_fg_color, hint);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Check again").clicked() {
                    match mp4::find() {
                        Ok(ff) => choice = Some(Choice::Found(ff)),
                        Err(m) => {
                            self.missing = m;
                            self.still_missing = true;
                        }
                    }
                }
                if ui.button("FFmpeg website").on_hover_text(DOWNLOAD_URL).clicked() {
                    let _ = open::that(DOWNLOAD_URL);
                }
                if let Some(label) = self.alternative {
                    if ui.button(label).clicked() {
                        choice = Some(Choice::Alternative);
                    }
                }
                if ui.button("Close").clicked() {
                    choice = Some(Choice::Close);
                }
            });
        });
        choice
    }
}

fn install_help(ui: &mut egui::Ui) {
    let (intro, cmd, after) = if cfg!(windows) {
        (
            "Install it from a terminal (PowerShell) with:",
            "winget install Gyan.FFmpeg",
            Some("Or download it from the FFmpeg website and put ffmpeg.exe next to idrac6.exe or on your PATH."),
        )
    } else if cfg!(target_os = "macos") {
        ("Install it with Homebrew:", "brew install ffmpeg", None)
    } else {
        ("Install it with your package manager, for example:", "sudo apt install ffmpeg", Some("(Fedora: sudo dnf install ffmpeg, Arch: sudo pacman -S ffmpeg)"))
    };
    ui.label(intro);
    ui.horizontal(|ui| {
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .corner_radius(4.0)
            .show(ui, |ui| ui.monospace(cmd));
        if ui.small_button("Copy").clicked() {
            ui.ctx().copy_text(cmd.to_string());
        }
    });
    if let Some(a) = after {
        ui.weak(a);
    }
}

/// Look for FFmpeg; on failure, return the dialog to show.
pub fn require() -> Result<Ffmpeg, FfmpegDialog> {
    mp4::find().map_err(FfmpegDialog::new)
}

/// `path` with the `.mp4` extension, unless it already has it.
pub fn with_mp4_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")) {
        path
    } else {
        let mut s = path.into_os_string();
        s.push(".mp4");
        s.into()
    }
}

/// A compact progress line for an export: bar and Cancel. Returns true
/// when Cancel was clicked.
pub fn progress_line(ui: &mut egui::Ui, label: &str, job: &Export) -> bool {
    let p = job.progress();
    let bar = egui::ProgressBar::new(p).desired_width(140.0).desired_height(14.0).text(format!("{label} {:.0}%", p * 100.0));
    ui.add(bar).on_hover_text(format!("Writing {}", job.out.display()));
    ui.small_button("Cancel").on_hover_text("Stop making the video").clicked()
}

/// Size and length line, e.g. "2:31, 4.2 MB".
pub fn describe(len: Duration, bytes: u64) -> String {
    let s = len.as_secs();
    format!("{}:{:02}, {:.1} MB", s / 60, s % 60, bytes as f64 / 1_000_000.0)
}

/// Buttons to open a saved file and its folder.
pub fn open_buttons(ui: &mut egui::Ui, path: &Path) {
    if ui.button("Open video").on_hover_text("Play it in your video player").clicked() {
        let _ = open::that(path);
    }
    if ui.button("Show folder").clicked() {
        if let Some(dir) = path.parent() {
            let _ = open::that(dir);
        }
    }
}
