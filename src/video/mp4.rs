//! MP4 export of recordings, using an installed FFmpeg.
//!
//! A recording is decoded with the normal DVC decoder and the pictures are
//! piped to `ffmpeg` as raw video at a constant frame rate. The iDRAC only
//! sends what changed, so still stretches repeat the last picture; the
//! video keeps the real timing of the recording.
//!
//! idrac6-rs does not ship FFmpeg. [`find`] looks for it on `PATH` (and
//! next to the program on Windows); `IDRAC6_FFMPEG` points to a specific
//! binary.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use super::Framebuffer;
use super::dvc::DvcDecoder;
use super::playback::Recording;

/// Frames per second of the exported video.
pub const FPS: u64 = 15;
/// How long the last picture stays on screen at the end of the video.
const HOLD_END_MS: u64 = 1000;
/// Largest picture size; the iDRAC6 goes up to 1280x1024.
const MAX_W: usize = 1920;
const MAX_H: usize = 1200;

/// A usable FFmpeg.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    pub path: PathBuf,
    encoder: Encoder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoder {
    X264,
    OpenH264,
    MediaFoundation,
    Mpeg4,
}

impl Encoder {
    /// In order of preference.
    const ALL: [(Encoder, &'static str); 4] = [
        (Encoder::X264, "libx264"),
        (Encoder::OpenH264, "libopenh264"),
        (Encoder::MediaFoundation, "h264_mf"),
        (Encoder::Mpeg4, "mpeg4"),
    ];

    fn args(self) -> &'static [&'static str] {
        match self {
            Encoder::X264 => &["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-tune", "stillimage"],
            Encoder::OpenH264 => &["-c:v", "libopenh264", "-b:v", "3M"],
            Encoder::MediaFoundation => &["-c:v", "h264_mf", "-b:v", "3M"],
            Encoder::Mpeg4 => &["-c:v", "mpeg4", "-q:v", "3"],
        }
    }
}

/// Why FFmpeg cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// Not installed (or `IDRAC6_FFMPEG` points nowhere).
    NotFound,
    /// Installed, but without an H.264 or MPEG-4 encoder.
    NoEncoder(PathBuf),
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Missing::NotFound => write!(f, "FFmpeg is not installed, so MP4 videos cannot be made"),
            Missing::NoEncoder(p) => write!(f, "{} has no H.264 or MPEG-4 encoder, so MP4 videos cannot be made", p.display()),
        }
    }
}

impl std::error::Error for Missing {}

#[cfg_attr(not(windows), allow(unused_mut))]
fn command(path: &Path) -> Command {
    let mut c = Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console window pops up.
        c.creation_flags(0x0800_0000);
    }
    c
}

fn candidates() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("IDRAC6_FFMPEG").filter(|p| !p.is_empty()) {
        return vec![p.into()];
    }
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        let p = dir.join(exe);
        if p.is_file() {
            v.push(p);
        }
    }
    // winget installs a link here, but a running program does not see
    // the PATH change until it is restarted.
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(local).join(r"Microsoft\WinGet\Links\ffmpeg.exe");
        if p.is_file() {
            v.push(p);
        }
    }
    v.push(exe.into());
    v
}

/// Look for FFmpeg and an encoder it has. Takes a moment (runs it).
pub fn find() -> Result<Ffmpeg, Missing> {
    let mut found = None;
    for path in candidates() {
        let Ok(out) = command(&path).args(["-hide_banner", "-encoders"]).stdin(Stdio::null()).stderr(Stdio::null()).output() else { continue };
        if !out.status.success() {
            continue;
        }
        let list = String::from_utf8_lossy(&out.stdout);
        let has = |name: &str| list.lines().any(|l| l.split_whitespace().nth(1) == Some(name));
        if let Some(&(encoder, _)) = Encoder::ALL.iter().find(|(_, n)| has(n)) {
            return Ok(Ffmpeg { path, encoder });
        }
        found = Some(path);
    }
    Err(found.map_or(Missing::NotFound, Missing::NoEncoder))
}

/// A finished video.
#[derive(Debug, Clone)]
pub struct Done {
    pub path: PathBuf,
    pub duration: Duration,
    pub bytes: u64,
}

/// The export was cancelled.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// An export running in the background. Dropping it cancels it.
pub struct Export {
    pub out: PathBuf,
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<Done>>,
    result: Option<Result<Done, String>>,
    cancelled: bool,
}

impl Export {
    /// Load the recording with `load` and write it to `out` as MP4. `notify`
    /// is called as progress is made and when it is done.
    pub fn start(
        ff: Ffmpeg,
        load: impl FnOnce() -> Result<Recording> + Send + 'static,
        out: PathBuf,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Export {
        let (tx, rx) = mpsc::channel();
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (p, c, o) = (progress.clone(), cancel.clone(), out.clone());
        std::thread::Builder::new()
            .name("mp4-export".into())
            .spawn(move || {
                let r = load().and_then(|rec| {
                    encode(&ff, &rec, &o, &mut |done| {
                        let permille = (done * 1000.0) as u32;
                        if p.swap(permille, Ordering::Relaxed) != permille {
                            notify();
                        }
                        !c.load(Ordering::Relaxed)
                    })
                });
                let _ = tx.send(r);
                notify();
            })
            .expect("spawn export thread");
        Export { out, progress, cancel, rx, result: None, cancelled: false }
    }

    /// 0.0 to 1.0.
    pub fn progress(&self) -> f32 {
        self.progress.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn was_cancelled(&self) -> bool {
        self.cancelled
    }

    /// The result once it is done.
    pub fn poll(&mut self) -> Option<&Result<Done, String>> {
        if self.result.is_none() {
            if let Ok(r) = self.rx.try_recv() {
                self.result = Some(r.map_err(|e| format!("{e:#}")));
            }
        }
        self.result.as_ref()
    }
}

impl Drop for Export {
    /// Cancel and wait a moment for FFmpeg to be stopped and the partial
    /// file removed.
    fn drop(&mut self) {
        if self.result.is_none() {
            self.cancel.store(true, Ordering::Relaxed);
            let _ = self.rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

/// Size of the video: the largest picture in the recording, rounded up to
/// even numbers (H.264 needs that).
fn video_size(rec: &Recording) -> (usize, usize) {
    let w = rec.frames.iter().map(|f| f.width as usize).filter(|&w| w <= MAX_W).max().unwrap_or(0);
    let h = rec.frames.iter().map(|f| f.height as usize).filter(|&h| h <= MAX_H).max().unwrap_or(0);
    let (w, h) = if w == 0 || h == 0 { (640, 480) } else { (w, h) };
    ((w + 1) & !1, (h + 1) & !1)
}

/// Time of each record from the start, in real milliseconds (no gaps cut
/// short, unlike the player).
fn record_times(rec: &Recording) -> Vec<u64> {
    let first = rec.frames.first().map_or(0, |f| f.timestamp_ms);
    let mut last = 0u64;
    rec.frames
        .iter()
        .map(|f| {
            last = last.max(f.timestamp_ms.saturating_sub(first) as u64);
            last
        })
        .collect()
}

/// Length of the video [`encode`] makes from `rec`.
pub fn video_duration(rec: &Recording) -> Duration {
    Duration::from_millis(record_times(rec).last().copied().unwrap_or(0) + HOLD_END_MS)
}

/// Write `rec` to `out` as MP4. `progress` gets 0.0..1.0 and returns false
/// to cancel. The file only appears at `out` when it is complete.
pub fn encode(ff: &Ffmpeg, rec: &Recording, out: &Path, progress: &mut dyn FnMut(f64) -> bool) -> Result<Done> {
    let (w, h) = video_size(rec);
    let times = record_times(rec);
    let total_ms = times.last().copied().unwrap_or(0) + HOLD_END_MS;
    let frames = total_ms * FPS / 1000 + 1;
    let part = part_path(out);

    let mut child = command(&ff.path)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", &format!("{w}x{h}"), "-framerate", &FPS.to_string(), "-i", "-"])
        // Drop repeated pictures (the screen is mostly still) instead of
        // encoding them, keeping one a second; the video gets a variable
        // frame rate.
        .args(["-vf", &format!("mpdecimate=hi=1:lo=1:frac=0:max={FPS}"), "-fps_mode", "vfr"])
        .args(ff.encoder.args())
        .args(["-pix_fmt", "yuv420p", "-color_range", "tv", "-colorspace", "smpte170m", "-color_primaries", "smpte170m", "-color_trc", "smpte170m"])
        .args(["-movflags", "+faststart", "-f", "mp4"])
        .arg(&part)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", ff.path.display()))?;
    let (errors, errors_thread) = collect_stderr(&mut child);
    let mut stdin = child.stdin.take().expect("piped stdin");

    let r = (|| -> Result<()> {
        let mut decoder = DvcDecoder::new();
        let mut fb = Framebuffer::default();
        let mut canvas = Canvas::new(w, h);
        // The last picture drawn, to skip frames that change nothing.
        let mut shown = Framebuffer::default();
        let mut next = 0;
        for n in 0..frames {
            let t = n * 1000 / FPS;
            let mut changed = false;
            while next < rec.frames.len() && times[next] <= t {
                let f = &rec.frames[next];
                match decoder.push_packet(f.msg_type, &f.payload, &mut fb) {
                    Ok(done) => changed |= done,
                    Err(e) => log::debug!("mp4 export: record {next}: {e:#}"),
                }
                next += 1;
            }
            if changed && (fb.width, fb.height, &fb.pixels) != (shown.width, shown.height, &shown.pixels) {
                canvas.draw(&fb);
                shown.clone_from(&fb);
            }
            stdin.write_all(&canvas.yuv).map_err(|e| anyhow!("FFmpeg stopped taking frames: {e}"))?;
            if n % FPS == 0 && !progress(n as f64 / frames as f64) {
                return Err(Cancelled.into());
            }
        }
        Ok(())
    })();
    drop(stdin);
    if r.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().context("waiting for FFmpeg")?;
    if let Some(t) = errors_thread {
        let _ = t.join();
    }
    let failed = || {
        let msg = errors.lock().unwrap().trim().to_string();
        anyhow!("FFmpeg failed ({status}){}", if msg.is_empty() { String::new() } else { format!(": {msg}") })
    };
    // FFmpeg quitting early shows up as a broken pipe; its own message
    // says why.
    let r = match r {
        Err(e) if !e.is::<Cancelled>() && !status.success() => Err(failed()),
        r => r,
    };
    let finish = r.and_then(|()| {
        if !status.success() {
            return Err(failed());
        }
        std::fs::rename(&part, out).with_context(|| format!("saving {}", out.display()))?;
        Ok(())
    });
    if let Err(e) = finish {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    progress(1.0);
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(Done { path: out.to_path_buf(), duration: Duration::from_millis(total_ms), bytes })
}

/// Where the video is written until it is complete.
fn part_path(out: &Path) -> PathBuf {
    let mut name = out.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    out.with_file_name(name)
}

/// Keep the last bit of FFmpeg's error output.
/// Collect the end of `child`'s stderr; join the thread to have all of it.
fn collect_stderr(child: &mut Child) -> (Arc<Mutex<String>>, Option<std::thread::JoinHandle<()>>) {
    let buf = Arc::new(Mutex::new(String::new()));
    let mut thread = None;
    if let Some(mut err) = child.stderr.take() {
        let b = buf.clone();
        thread = Some(std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut s = b.lock().unwrap();
                s.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if s.len() > 2000 {
                    let cut = s.len() - 2000;
                    let cut = (cut..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
                    s.drain(..cut);
                }
            }
        }));
    }
    (buf, thread)
}

/// The video picture: the screen scaled to fit, centred on black.
struct Canvas {
    w: usize,
    h: usize,
    /// `bgr0` pixels.
    bytes: Vec<u8>,
    /// The same as `yuv420p` (BT.601, limited range), what FFmpeg gets.
    yuv: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Canvas {
        let mut c = Canvas { w, h, bytes: vec![0; w * h * 4], yuv: vec![0; w * h * 3 / 2] };
        c.to_yuv();
        c
    }

    fn draw(&mut self, fb: &Framebuffer) {
        self.draw_rgb(fb);
        self.to_yuv();
    }

    /// `w` and `h` are even.
    fn to_yuv(&mut self) {
        let w = self.w;
        let (y_plane, uv) = self.yuv.split_at_mut(w * self.h);
        let (u_plane, v_plane) = uv.split_at_mut(w * self.h / 4);
        let rgb = |b: &[u8]| (b[2] as i32, b[1] as i32, b[0] as i32);
        // Two picture rows give one row of chroma.
        for (((rows, ys), us), vs) in
            self.bytes.chunks_exact(w * 8).zip(y_plane.chunks_exact_mut(w * 2)).zip(u_plane.chunks_exact_mut(w / 2)).zip(v_plane.chunks_exact_mut(w / 2))
        {
            for (p, y) in rows.chunks_exact(4).zip(ys.iter_mut()) {
                let (r, g, b) = rgb(p);
                *y = (16 + ((66 * r + 129 * g + 25 * b + 128) >> 8)) as u8;
            }
            let (top, bottom) = rows.split_at(w * 4);
            for (((t, bt), u), v) in top.chunks_exact(8).zip(bottom.chunks_exact(8)).zip(us.iter_mut()).zip(vs.iter_mut()) {
                let (mut r, mut g, mut b) = (0, 0, 0);
                for q in [&t[..4], &t[4..], &bt[..4], &bt[4..]] {
                    let c = rgb(q);
                    r += c.0;
                    g += c.1;
                    b += c.2;
                }
                let (r, g, b) = (r / 4, g / 4, b / 4);
                *u = (128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8)) as u8;
                *v = (128 + ((112 * r - 94 * g - 18 * b + 128) >> 8)) as u8;
            }
        }
    }

    fn draw_rgb(&mut self, fb: &Framebuffer) {
        let (sw, sh) = (fb.width, fb.height);
        if sw == 0 || sh == 0 || fb.pixels.len() < sw * sh {
            return;
        }
        self.bytes.fill(0);
        let s = (self.w as f64 / sw as f64).min(self.h as f64 / sh as f64);
        let dw = ((sw as f64 * s).round() as usize).clamp(1, self.w);
        let dh = ((sh as f64 * s).round() as usize).clamp(1, self.h);
        let (ox, oy) = ((self.w - dw) / 2, (self.h - dh) / 2);
        if dw == sw && dh == sh {
            for y in 0..sh {
                let row = &fb.pixels[y * sw..(y + 1) * sw];
                let o = ((oy + y) * self.w + ox) * 4;
                for (d, p) in self.bytes[o..o + sw * 4].chunks_exact_mut(4).zip(row) {
                    d.copy_from_slice(&p.to_le_bytes());
                }
            }
            return;
        }
        // Bilinear scaling.
        let taps = |dst: usize, src: usize| -> Vec<(usize, usize, u32)> {
            (0..dst)
                .map(|i| {
                    let x = ((i as f64 + 0.5) * src as f64 / dst as f64 - 0.5).clamp(0.0, (src - 1) as f64);
                    let x0 = x.floor() as usize;
                    (x0, (x0 + 1).min(src - 1), ((x - x0 as f64) * 256.0) as u32)
                })
                .collect()
        };
        let (xs, ys) = (taps(dw, sw), taps(dh, sh));
        let mix = |a: u32, b: u32, f: u32| -> u32 {
            let ch = |p: u32, sh: u32| (p >> sh) & 0xff;
            let m = |sh: u32| ((ch(a, sh) * (256 - f) + ch(b, sh) * f) >> 8) << sh;
            m(0) | m(8) | m(16)
        };
        for (y, &(y0, y1, fy)) in ys.iter().enumerate() {
            let (r0, r1) = (&fb.pixels[y0 * sw..(y0 + 1) * sw], &fb.pixels[y1 * sw..(y1 + 1) * sw]);
            let o = ((oy + y) * self.w + ox) * 4;
            for (d, &(x0, x1, fx)) in self.bytes[o..o + dw * 4].chunks_exact_mut(4).zip(&xs) {
                let p = mix(mix(r0[x0], r0[x1], fx), mix(r1[x0], r1[x1], fx), fy);
                d.copy_from_slice(&p.to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::playback::{encode_record, parse};

    fn rec(records: &[(u32, u16, u16)]) -> Recording {
        let data: Vec<u8> = records.iter().flat_map(|&(ts, w, h)| encode_record(ts, w, h, true, true, &[0x80])).collect();
        parse(&data).unwrap()
    }

    #[test]
    fn size_is_the_largest_picture_rounded_to_even() {
        assert_eq!(video_size(&rec(&[(0, 720, 400), (10, 1023, 767)])), (1024, 768));
    }

    #[test]
    fn keeps_real_timing() {
        let r = rec(&[(5000, 8, 2), (5100, 8, 2), (65_100, 8, 2)]);
        assert_eq!(record_times(&r), vec![0, 100, 60_100]);
        assert_eq!(video_duration(&r), Duration::from_millis(61_100));
    }

    #[test]
    fn canvas_fits_and_centres() {
        let mut c = Canvas::new(8, 8);
        let fb = Framebuffer { width: 4, height: 2, pixels: vec![0x00ff_0000; 8] };
        c.draw(&fb);
        // Black is Y 16, red is Y 82 / V 240.
        assert_eq!((c.yuv[0], c.yuv[2 * 8], c.yuv[64 + 16 + 4]), (16, 82, 240));
        // Scaled 2x to 8x4, centred vertically: rows 0-1 and 6-7 black.
        let px = |x: usize, y: usize| u32::from_le_bytes(c.bytes[(y * 8 + x) * 4..][..4].try_into().unwrap());
        assert_eq!(px(0, 0), 0);
        assert_eq!(px(0, 2), 0x00ff_0000);
        assert_eq!(px(7, 5), 0x00ff_0000);
        assert_eq!(px(7, 6), 0);
    }

    #[test]
    fn missing_ffmpeg_is_reported() {
        // SAFETY: tests touching IDRAC6_FFMPEG run in this one test only.
        unsafe { std::env::set_var("IDRAC6_FFMPEG", "/nonexistent/ffmpeg") };
        let r = find();
        unsafe { std::env::remove_var("IDRAC6_FFMPEG") };
        assert_eq!(r.unwrap_err(), Missing::NotFound);
    }
}
