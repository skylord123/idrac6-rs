//! A live virtual console session: one command socket and one video socket,
//! each driven by its own thread.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use openssl::ssl::SslStream;

use super::proto::{self, ApcpChannel, FrameReader, Message, ty};
use crate::video::Framebuffer;
use crate::video::aspeed::{self, AspeedDecoder};

/// Everything needed to open a console. Normally filled in from the JNLP
/// file the iDRAC web UI hands out (see [`crate::web::ConsoleInfo`]).
#[derive(Clone, Debug)]
pub struct KvmParams {
    pub host: String,
    /// Command (keyboard/mouse) port, JNLP `kmport`.
    pub kmport: u16,
    /// Video port, JNLP `vport`.
    pub vport: u16,
    /// One-time session tokens, JNLP `user` / `passwd`.
    pub user: String,
    pub passwd: String,
    /// JNLP `apcp=1`: do the APCP handshake (always the case on iDRAC6).
    pub apcp: bool,
    /// JNLP `version=2`.
    pub version: u8,
    /// Ask to share an existing session instead of failing when one exists.
    pub shared: bool,
}

/// Things the GUI needs to know about.
#[derive(Debug, Clone)]
pub enum KvmEvent {
    Status(String),
    Running,
    LoginFailed(String),
    ViewOnly,
    Resolution(u16, u16),
    Leds(proto::Leds),
    PowerState(proto::ServerInfo),
    SharingRequest(proto::SharingRequest),
    VideoState(VideoState),
    /// Current mouse acceleration setting (see [`proto::MouseAccel`]).
    MouseAccel(u8),
    /// Credentials for the virtual media channel (answer to
    /// [`KvmSession::request_vm_credentials`]).
    VmCredentials { user: String, password: String },
    /// The iDRAC announced why it is about to end the session (see
    /// `docs/protocol.md`, message `0x8301`). Followed by `Disconnected`.
    DisconnectReason(u8),
    Disconnected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoState {
    Active,
    Calibrating,
    NoSignal,
    OutOfRange,
    Stopped(u8),
}

/// State shared between the session threads and the GUI.
pub struct Shared {
    pub fb: Mutex<Framebuffer>,
    /// Incremented after every decoded frame.
    pub frame_seq: AtomicU64,
    pub bytes_in: AtomicU64,
    pub frames: AtomicU64,
    pub server_id: Mutex<Option<String>>,
    /// Console recording under way, if any. Shared with the GUI, which
    /// passes the same one on to the next session after a reconnect.
    pub recorder: Recorder,
}

pub type Recorder = Arc<Mutex<Option<crate::video::playback::Writer>>>;

type Notify = Arc<dyn Fn() + Send + Sync>;

pub struct KvmSession {
    cmd_tx: Sender<Vec<u8>>,
    pub shared: Arc<Shared>,
    pub events: Receiver<KvmEvent>,
    stop: Arc<AtomicBool>,
}

impl KvmSession {
    /// Start connecting in the background. Progress and errors arrive as
    /// [`KvmEvent`]s; `notify` is called whenever there is something new
    /// (used to wake up the GUI).
    pub fn start(params: KvmParams, notify: Notify) -> KvmSession {
        Self::start_with_recorder(params, notify, Recorder::default())
    }

    /// Like [`KvmSession::start`], recording video into `recorder` while it
    /// holds a writer.
    pub fn start_with_recorder(params: KvmParams, notify: Notify, recorder: Recorder) -> KvmSession {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            fb: Mutex::new(Framebuffer::default()),
            frame_seq: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            server_id: Mutex::new(None),
            recorder,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Ctx {
            params,
            events: ev_tx,
            notify,
            shared: shared.clone(),
            stop: stop.clone(),
            cmd_tx: cmd_tx.clone(),
        };
        thread::Builder::new()
            .name("kvm-command".into())
            .spawn(move || {
                let r = command_thread(&ctx, cmd_rx);
                ctx.stop.store(true, Ordering::SeqCst);
                let msg = match r {
                    Ok(()) => "Disconnected.".to_string(),
                    Err(e) => format!("{e:#}"),
                };
                log::info!("console session ended: {msg}");
                ctx.send(KvmEvent::Disconnected(msg));
            })
            .expect("spawn kvm thread");
        KvmSession { cmd_tx, shared, events: ev_rx, stop }
    }

    /// Queue a raw message for the command socket.
    pub fn send(&self, msg: Vec<u8>) {
        let _ = self.cmd_tx.send(msg);
    }

    /// A handle to the command queue, for senders on other threads (e.g.
    /// typing text). Sending fails once the session has ended.
    pub fn sender(&self) -> Sender<Vec<u8>> {
        self.cmd_tx.clone()
    }

    /// Tell the iDRAC whether our window has the input focus, like the Java
    /// viewer does. Keyboard input may be ignored until focus is set.
    pub fn set_focus(&self, focused: bool) {
        self.send(proto::focus(focused));
        self.send(proto::keyboard_led_request());
    }

    pub fn key(&self, usage: u16, pressed: bool) {
        self.send(proto::key(usage, pressed));
    }

    /// Press and release a key combination (modifiers first).
    pub fn key_combo(&self, usages: &[u16]) {
        for &u in usages {
            self.key(u, true);
        }
        for &u in usages.iter().rev() {
            self.key(u, false);
        }
    }

    pub fn mouse(&self, x: u16, y: u16, buttons: u8, wheel: i16) {
        self.send(proto::mouse_abs(x, y, buttons, wheel));
    }

    /// Ask the iDRAC for the one-time virtual media credentials. The answer
    /// arrives as [`KvmEvent::VmCredentials`].
    pub fn request_vm_credentials(&self) {
        self.send(crate::vmedia::proto::kvm_credentials_request().to_vec());
    }

    pub fn refresh(&self) {
        self.send(proto::screen_refresh());
    }

    pub fn power(&self, action: proto::PowerAction) -> Result<()> {
        let id = self.shared.server_id.lock().unwrap().clone();
        let Some(id) = id else {
            bail!("the iDRAC has not reported a server id yet; try again in a few seconds");
        };
        self.send(proto::set_power(&id, action));
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for KvmSession {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------

struct Ctx {
    params: KvmParams,
    events: Sender<KvmEvent>,
    notify: Notify,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    cmd_tx: Sender<Vec<u8>>,
}

impl Ctx {
    fn send(&self, e: KvmEvent) {
        let _ = self.events.send(e);
        (self.notify)();
    }
    fn status(&self, s: impl Into<String>) {
        self.send(KvmEvent::Status(s.into()));
    }
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// A socket that is either plain TCP or TLS over TCP.
enum Conn {
    Plain(TcpStream),
    Tls(Box<SslStream<TcpStream>>),
}

impl Conn {
    fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => s.get_ref(),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(b),
            Conn::Tls(s) => s.read(b),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(b),
            Conn::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a socket read blocks before we check the outgoing queue.
const POLL: Duration = Duration::from_millis(8);

fn open(host: &str, port: u16, apcp: Option<ApcpChannel>) -> Result<Conn> {
    use std::net::ToSocketAddrs;
    let addr = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?
        .next()
        .context("no address")?;
    let mut tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).with_context(|| format!("connecting to {host}:{port}"))?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    let use_ssl = match apcp {
        Some(ch) => {
            let reply = proto::apcp_handshake(&mut tcp, ch)?;
            log::debug!("APCP {ch:?} reply: {reply:?}");
            if reply.redirect_port != 0 {
                tcp = TcpStream::connect_timeout(&std::net::SocketAddr::new(addr.ip(), reply.redirect_port), CONNECT_TIMEOUT)?;
                tcp.set_nodelay(true).ok();
            }
            reply.use_ssl
        }
        // Without APCP the original client always used SSL.
        None => true,
    };
    let conn = if use_ssl {
        Conn::Tls(Box::new(crate::tls::wrap(tcp, host)?))
    } else {
        Conn::Plain(tcp)
    };
    conn.tcp().set_read_timeout(Some(POLL))?;
    Ok(conn)
}

fn write_all(c: &mut Conn, data: &[u8]) -> Result<()> {
    // Writes go through even though reads use a short timeout: the socket
    // has no write timeout, so this simply blocks until sent.
    c.write_all(data)?;
    c.flush()?;
    Ok(())
}

fn command_thread(ctx: &Ctx, outgoing: Receiver<Vec<u8>>) -> Result<()> {
    let p = &ctx.params;
    ctx.status(format!("Connecting to {}:{}…", p.host, p.kmport));
    let mut conn = open(&p.host, p.kmport, p.apcp.then_some(ApcpChannel::Command))?;
    ctx.status("Logging in…");

    let v2 = p.version >= 2;
    let client_random: u32 = rand::random::<u32>() % 10_000_000;
    // The Java client calls the login with (port = 1, channel = 0).
    write_all(&mut conn, &proto::login(&p.user, &p.passwd, 1, 0, client_random, p.shared, v2))?;

    let mut reader = FrameReader::new(true);
    let mut last_keepalive = Instant::now();
    let mut logged_in = false;
    let mut video: Option<VideoAssembler> = None;

    loop {
        if ctx.stopped() {
            return Ok(());
        }
        // Flush queued input.
        loop {
            match outgoing.try_recv() {
                Ok(m) => {
                    log::debug!("cmd >> {:02x?}", &m[..m.len().min(16)]);
                    write_all(&mut conn, &m)?
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if logged_in && last_keepalive.elapsed() >= Duration::from_secs(10) {
            write_all(&mut conn, &proto::keepalive())?;
            write_all(&mut conn, &proto::get_servers())?;
            last_keepalive = Instant::now();
        }
        if !reader.fill(&mut conn)? {
            bail!("The iDRAC closed the console connection.");
        }
        while let Some(m) = reader.next()? {
            log::trace!("cmd << {:#06x} ({} bytes)", m.ty, m.payload.len());
            match m.ty {
                ty::LOGIN_RESPONSE_V1 | ty::LOGIN_RESPONSE_V2 => {
                    let r = proto::LoginResponse::parse(&m.payload, m.ty == ty::LOGIN_RESPONSE_V2);
                    log::info!("login response: status={} flags={:#x} msg={:?}", r.status, r.flags, r.message);
                    if let Some(reason) = r.failure_reason() {
                        ctx.send(KvmEvent::LoginFailed(reason.into()));
                        bail!("{reason}");
                    }
                    logged_in = true;
                    if r.view_only() {
                        ctx.send(KvmEvent::ViewOnly);
                    }
                    if r.video_on_command_channel() {
                        start_video(&mut conn)?;
                        video = Some(VideoAssembler::new());
                    } else {
                        spawn_video_thread(ctx, client_random, r.session_id);
                    }
                    // Kick off the server-id discovery needed for power control.
                    write_all(&mut conn, &proto::keepalive())?;
                    write_all(&mut conn, &proto::get_servers())?;
                    ctx.send(KvmEvent::Running);
                }
                ty::DISPLAY_RESOLUTION => {
                    let (w, h) = proto::parse_resolution(&m.payload);
                    log::info!("display resolution {w}x{h}");
                    ctx.send(KvmEvent::Resolution(w, h));
                    write_all(&mut conn, &proto::screen_refresh())?;
                }
                ty::MOUSE_ACCEL_RESPONSE => ctx.send(KvmEvent::MouseAccel(m.payload.first().copied().unwrap_or(0))),
                ty::KEYBOARD_LED => ctx.send(KvmEvent::Leds(proto::parse_leds(&m.payload))),
                ty::SERVER_NAMES => {
                    if let Some(s) = proto::parse_server_names(&m.payload).into_iter().next() {
                        *ctx.shared.server_id.lock().unwrap() = Some(s.id.clone());
                        ctx.send(KvmEvent::PowerState(s));
                    }
                }
                ty::SERVER_STATUS => {
                    let s = proto::parse_server_status(&m.payload);
                    *ctx.shared.server_id.lock().unwrap() = Some(s.id.clone());
                    ctx.send(KvmEvent::PowerState(s));
                }
                ty::SELECTED_SERVER => {
                    *ctx.shared.server_id.lock().unwrap() = Some(proto::parse_selected_server(&m.payload));
                }
                ty::REQUEST_CREDENTIALS => match crate::vmedia::proto::parse_kvm_credentials(&m.payload) {
                    Some((user, password)) => ctx.send(KvmEvent::VmCredentials { user, password }),
                    None => log::warn!("unparseable virtual media credentials ({} bytes)", m.payload.len()),
                },
                ty::SHARING_REQUEST => ctx.send(KvmEvent::SharingRequest(proto::parse_sharing_request(&m.payload))),
                ty::USER_DISCONNECT_PENDING => {
                    let reason = m.payload.first().copied().unwrap_or(0);
                    ctx.send(KvmEvent::DisconnectReason(reason));
                    bail!("{}", disconnect_reason(reason));
                }
                t if t & 0xff00 == 0x8600 || t & 0xff00 == 0x8500 => {
                    // Video multiplexed onto the command channel.
                    if let Some(v) = video.as_mut() {
                        let msg = Message { ty: t & 0xff, payload: m.payload };
                        if let Some(reply) = v.handle(ctx, msg)? {
                            write_all(&mut conn, &reply)?;
                        }
                    }
                }
                other => log::debug!("ignoring command message {other:#06x} ({} bytes)", m.payload.len()),
            }
        }
    }
}

/// What the original client sends once video is flowing.
fn start_video(conn: &mut Conn) -> Result<()> {
    write_all(conn, &proto::video_enable(true))?;
    write_all(conn, &proto::set_display_area(1024, 768))?;
    write_all(conn, &proto::scale_1to1())?;
    Ok(())
}

fn disconnect_reason(code: u8) -> &'static str {
    match code {
        1 => "An administrator disconnected this session.",
        2 => "The iDRAC is rebooting.",
        3 => "The session was taken over by a local user.",
        4 => "The iDRAC firmware is being upgraded.",
        5 => "The last active user disconnected.",
        6 => "The primary user switched to exclusive mode.",
        7 => "The session timed out because it was idle.",
        _ => "The iDRAC ended the session.",
    }
}

fn spawn_video_thread(ctx: &Ctx, client_random: u32, session_id: u32) {
    let c = Ctx {
        params: ctx.params.clone(),
        events: ctx.events.clone(),
        notify: ctx.notify.clone(),
        shared: ctx.shared.clone(),
        stop: ctx.stop.clone(),
        cmd_tx: ctx.cmd_tx.clone(),
    };
    thread::Builder::new()
        .name("kvm-video".into())
        .spawn(move || {
            if let Err(e) = video_thread(&c, client_random, session_id) {
                if !c.stopped() {
                    log::error!("video channel: {e:#}");
                    c.send(KvmEvent::Disconnected(format!("Video connection lost: {e:#}")));
                }
            }
            c.stop.store(true, Ordering::SeqCst);
        })
        .expect("spawn video thread");
}

fn video_thread(ctx: &Ctx, client_random: u32, session_id: u32) -> Result<()> {
    let p = &ctx.params;
    let mut conn = open(&p.host, p.vport, p.apcp.then_some(ApcpChannel::Video)).context("opening video channel")?;
    write_all(&mut conn, &proto::video_auth(client_random, session_id))?;
    let mut reader = FrameReader::new(false);
    let mut asm = VideoAssembler::new();
    // Debug aid: IDRAC6_CAPTURE=<file> records every video message (8 byte
    // header + payload) so decoding problems can be reproduced offline.
    let mut capture = std::env::var_os("IDRAC6_CAPTURE").and_then(|p| match std::fs::File::create(&p) {
        Ok(f) => {
            log::info!("recording video messages to {}", std::path::Path::new(&p).display());
            Some(std::io::BufWriter::new(f))
        }
        Err(e) => {
            log::warn!("cannot create capture file: {e}");
            None
        }
    });
    loop {
        if ctx.stopped() {
            return Ok(());
        }
        if !reader.fill(&mut conn)? {
            bail!("The iDRAC closed the video connection.");
        }
        while let Some(m) = reader.next()? {
            ctx.shared.bytes_in.fetch_add(m.payload.len() as u64 + 8, Ordering::Relaxed);
            if let Some(f) = capture.as_mut() {
                let len = (m.payload.len() + proto::HEADER_LEN) as u16;
                let mut hdr = *b"BEEF\0\0\0\0";
                hdr[5] = m.ty as u8;
                hdr[6..8].copy_from_slice(&len.to_be_bytes());
                let _ = f.write_all(&hdr).and_then(|_| f.write_all(&m.payload)).and_then(|_| f.flush());
            }
            if m.ty == ty::VIDEO_CONNECT_STATUS as u16 {
                log::info!("video channel connected");
                for msg in [proto::video_enable(true), proto::set_display_area(1024, 768), proto::scale_1to1()] {
                    let _ = ctx.cmd_tx.send(msg);
                }
                continue;
            }
            if let Some(reply) = asm.handle(ctx, m)? {
                write_all(&mut conn, &reply)?;
            }
        }
    }
}

/// Collects ASPEED video packets into frames, decodes them and acks them.
struct VideoAssembler {
    decoder: AspeedDecoder,
    /// Avocent DVC codec: what iDRAC6 actually sends.
    dvc: crate::video::dvc::DvcDecoder,
    pending: Option<(aspeed::AspeedFrameHeader, Vec<u8>)>,
    last_state: Option<VideoState>,
}

impl VideoAssembler {
    fn new() -> Self {
        Self { decoder: AspeedDecoder::new(), dvc: crate::video::dvc::DvcDecoder::new(), pending: None, last_state: None }
    }

    fn set_state(&mut self, ctx: &Ctx, s: VideoState) {
        if self.last_state != Some(s) {
            self.last_state = Some(s);
            ctx.send(KvmEvent::VideoState(s));
        }
    }

    /// Handle one video message. Returns an ack to send back, if any.
    fn handle(&mut self, ctx: &Ctx, m: Message) -> Result<Option<Vec<u8>>> {
        match m.ty as u8 {
            t @ (ty::VIDEO_DVC_FIRST..=ty::VIDEO_DVC_LAST) => {
                self.set_state(ctx, VideoState::Active);
                let mut fb = ctx.shared.fb.lock().unwrap();
                let r = self.dvc.push_packet(t, &m.payload, &mut fb);
                drop(fb);
                if let Some(w) = ctx.shared.recorder.lock().unwrap().as_mut() {
                    if let Err(e) = w.packet(t, &m.payload) {
                        log::error!("recording: {e}");
                        (ctx.notify)();
                    }
                }
                match r {
                    Ok(true) => {
                        ctx.shared.frames.fetch_add(1, Ordering::Relaxed);
                        ctx.shared.frame_seq.fetch_add(1, Ordering::Release);
                        (ctx.notify)();
                    }
                    Ok(false) => {}
                    Err(e) => {
                        log::warn!("video decode failed: {e:#}");
                        let _ = ctx.cmd_tx.send(proto::screen_refresh());
                    }
                }
                // The Java viewer acknowledges every video packet.
                Ok(Some(proto::video_ack(1)))
            }
            ty::VIDEO_ASPEED => {
                let hdr = aspeed::parse_header(&m.payload);
                let data = m.payload.get(aspeed::HEADER_LEN..).unwrap_or(&[]);
                if hdr.frame_start {
                    // A new frame begins: the previous one is complete.
                    if let Some((h, buf)) = self.pending.take() {
                        self.decode(ctx, &h, &buf);
                    }
                    let mut buf = Vec::with_capacity(64 * 1024);
                    buf.extend_from_slice(data);
                    let complete = aspeed::frame_complete(&hdr, &buf);
                    if complete {
                        self.decode(ctx, &hdr, &buf);
                    } else {
                        self.pending = Some((hdr, buf));
                    }
                    self.set_state(ctx, VideoState::Active);
                    return Ok(Some(proto::video_ack(1)));
                } else if let Some((h, buf)) = self.pending.as_mut() {
                    buf.extend_from_slice(data);
                    if aspeed::frame_complete(h, buf) {
                        let (h, buf) = self.pending.take().unwrap();
                        self.decode(ctx, &h, &buf);
                    }
                }
                Ok(None)
            }
            ty::VIDEO_STOPPED => {
                let code = m.payload.first().copied().unwrap_or(0);
                let s = match code {
                    0 => VideoState::Calibrating,
                    1 => VideoState::NoSignal,
                    2 => VideoState::OutOfRange,
                    c => VideoState::Stopped(c),
                };
                log::info!("video stopped ({code})");
                self.set_state(ctx, s);
                Ok(Some(proto::video_ack(1)))
            }
            other => {
                log::debug!("ignoring video message {other:#04x} ({} bytes)", m.payload.len());
                Ok(None)
            }
        }
    }

    fn decode(&mut self, ctx: &Ctx, h: &aspeed::AspeedFrameHeader, data: &[u8]) {
        let mut fb = ctx.shared.fb.lock().unwrap();
        match self.decoder.decode_frame(h.width as usize, h.height as usize, data, &mut fb) {
            Ok(_) => {
                drop(fb);
                ctx.shared.frames.fetch_add(1, Ordering::Relaxed);
                ctx.shared.frame_seq.fetch_add(1, Ordering::Release);
                (ctx.notify)();
            }
            Err(e) => {
                log::warn!("frame decode failed: {e:#}");
                drop(fb);
                let _ = ctx.cmd_tx.send(proto::screen_refresh());
            }
        }
    }
}
