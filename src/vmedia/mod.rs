//! iDRAC6 Virtual Media: map a local ISO/IMG file as a virtual CD/DVD or
//! floppy / removable disk on the server.
//!
//! The wire protocol is described in `docs/virtual-media.md`.

pub mod image;
pub mod proto;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use image::Image;
use proto::{Incoming, RemoteDrive};

/// Kind of virtual drive to map an image on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriveKind {
    /// Virtual CD/DVD, backed by an ISO image (2048-byte blocks, read-only).
    Cd,
    /// Virtual floppy / removable disk, backed by an IMG raw disk image
    /// (512-byte blocks). The iDRAC picks the first free floppy (type 1) or
    /// removable-disk (type 4) slot, exactly like the Java client.
    Floppy,
}

/// Connection parameters.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// iDRAC address (JNLP `ip` argument / the host the user configured).
    pub host: String,
    /// TCP port: the JNLP `kmport` argument (5900 on iDRAC6).
    pub port: u16,
    /// Virtual media user name from the KVM credentials reply (type 33844).
    pub username: String,
    /// Virtual media password from the KVM credentials reply (type 33844).
    pub password: String,
    /// If the virtual media channel is held by another user, take it over.
    pub preempt: bool,
}

/// An image to map.
#[derive(Debug, Clone)]
pub struct VmImage {
    pub path: PathBuf,
    pub kind: DriveKind,
    /// Ignored for `DriveKind::Cd` (always read-only).
    pub read_only: bool,
}

/// Events reported by the session thread.
#[derive(Debug, Clone)]
pub enum VmEvent {
    /// Logged in; the server's virtual drive slots.
    Connected { drives: Vec<RemoteDrive> },
    Mapped { kind: DriveKind, path: PathBuf },
    /// `reason` is set when the iDRAC (not the user) caused the unmap.
    Unmapped { kind: DriveKind, reason: Option<String> },
    /// Cumulative transfer counters of a mapped drive.
    Stats { kind: DriveKind, read_bytes: u64, write_bytes: u64 },
    /// The iDRAC forces all drives read-only (or lifts it).
    ReadOnlyForced(bool),
    /// Virtual media is locked to the KVM session (closes with it).
    LockedToKvm(bool),
    Error(String),
    /// Session ended (iDRAC disconnect, network error, or `disconnect()`).
    Disconnected(String),
}

enum Cmd {
    Map(VmImage, Sender<Result<()>>),
    Unmap(DriveKind, Sender<Result<()>>),
    UsbReset,
    Disconnect,
}

/// A live virtual media session. Dropping it disconnects.
pub struct VmSession {
    cmds: Sender<Cmd>,
    socket: TcpStream,
    thread: Option<JoinHandle<()>>,
}

/// Plain or TLS transport.
enum Conn {
    Plain(TcpStream),
    Tls(Box<openssl::ssl::SslStream<TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(b),
            Conn::Tls(s) => s.read(b),
        }
    }
}
impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(b),
            Conn::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

const READ_TIMEOUT: Duration = Duration::from_millis(50);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
const MAP_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest VDISK_READ_DATA payload sent in one packet.
const MAX_READ_CHUNK: u32 = 1 << 20;
/// Largest read request served; bigger ones get a read error.
const MAX_READ_TOTAL: u64 = 16 << 20;

impl VmSession {
    /// Connect and log in. Blocks until login succeeds or fails.
    pub fn connect(cfg: VmConfig, events: Sender<VmEvent>) -> Result<VmSession> {
        let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port))
            .with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
        let socket = tcp.try_clone()?;
        let mut conn = handshake(tcp, &cfg)?;
        socket.set_read_timeout(Some(READ_TIMEOUT))?;
        conn.write_all(&proto::get_vdisk_info())?;
        conn.flush()?;

        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("vmedia".into())
            .spawn(move || {
                let mut w = Worker::new(conn, rx, events.clone());
                let reason = match w.run() {
                    Ok(r) => r,
                    Err(e) => format!("connection lost: {e:#}"),
                };
                w.fail_pending(&reason);
                let _ = events.send(VmEvent::Disconnected(reason));
            })?;
        Ok(VmSession { cmds: tx, socket, thread: Some(thread) })
    }

    /// Map an image. Returns once the iDRAC has been asked to attach it (a
    /// later `DEVICE_STATUS` error arrives as `VmEvent::Unmapped`).
    pub fn map(&self, image: VmImage) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.cmds.send(Cmd::Map(image, tx)).map_err(|_| anyhow!("virtual media session is closed"))?;
        match rx.recv_timeout(MAP_TIMEOUT + Duration::from_secs(1)) {
            Ok(r) => r,
            Err(_) => bail!("timed out waiting for the iDRAC"),
        }
    }

    pub fn unmap(&self, kind: DriveKind) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.cmds.send(Cmd::Unmap(kind, tx)).map_err(|_| anyhow!("virtual media session is closed"))?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| anyhow!("virtual media session is closed"))?
    }

    /// Reset the virtual USB device on the server (affects all virtual
    /// media and may briefly affect virtual keyboard/mouse).
    pub fn usb_reset(&self) -> Result<()> {
        self.cmds.send(Cmd::UsbReset).map_err(|_| anyhow!("virtual media session is closed"))
    }

    /// Unmap everything and close the connection.
    pub fn disconnect(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.cmds.send(Cmd::Disconnect);
        if let Some(t) = self.thread.take() {
            // Give the worker a moment to send releases, then force close.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.socket.shutdown(Shutdown::Both);
            let _ = t.join();
        }
    }
}

impl Drop for VmSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// APCP setup, optional TLS, and login (with preemption retry).
fn handshake(mut tcp: TcpStream, cfg: &VmConfig) -> Result<Conn> {
    let random: [u8; 32] = rand::random();
    tcp.write_all(&proto::apcp_request(proto::APCP_APP_VMEDIA, proto::CAP_TCP_CLEARTEXT, &random))?;
    let setup = proto::read_apcp_reply(&mut tcp).context("reading virtual media session setup")?;
    log::debug!("vmedia APCP setup: {setup:?}");
    let mut conn = match setup.capabilities {
        c if c & proto::CAP_TCP_CLEARTEXT != 0 => Conn::Plain(tcp),
        c if c & proto::CAP_SSL_ANONYMOUS != 0 => Conn::Tls(Box::new(crate::tls::wrap(tcp, &cfg.host)?)),
        0 => bail!("the iDRAC offers no virtual media connection type (is virtual media disabled?)"),
        c => bail!("unsupported virtual media connection type {c}"),
    };

    let mut preempt = false;
    for _ in 0..2 {
        conn.write_all(&proto::login(&cfg.username, &cfg.password, preempt))?;
        conn.flush()?;
        let pkt = read_packet_blocking(&mut conn).context("reading virtual media login status")?;
        let st = proto::parse_login_status(&pkt)?;
        match st.status {
            0 => return Ok(conn),
            6 | 11 if cfg.preempt && !preempt => preempt = true,
            _ => bail!("virtual media login failed: {}", st.describe()),
        }
    }
    bail!("virtual media login failed: channel still in use after preemption")
}

fn read_packet_blocking<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut hdr = [0u8; proto::HEADER_LEN];
    r.read_exact(&mut hdr)?;
    let len = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    if !(proto::HEADER_LEN..=1 << 20).contains(&len) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad packet length"));
    }
    let mut p = hdr.to_vec();
    p.resize(len, 0);
    r.read_exact(&mut p[proto::HEADER_LEN..])?;
    Ok(p)
}

struct MappedDrive {
    kind: DriveKind,
    image: Image,
    read_bytes: u64,
    write_bytes: u64,
    stats_dirty: bool,
}

struct PendingMap {
    image: VmImage,
    reply: Sender<Result<()>>,
    deadline: Instant,
    /// Ask for the drive table again at this time (the iDRAC was still
    /// bringing up its virtual USB device).
    retry_at: Option<Instant>,
}

struct Worker<S: Read + Write> {
    conn: S,
    cmds: Receiver<Cmd>,
    events: Sender<VmEvent>,
    buf: proto::PacketBuf,
    drives: Vec<RemoteDrive>,
    mapped: HashMap<u16, MappedDrive>,
    pending: Option<PendingMap>,
    connected_sent: bool,
    last_sent: Instant,
    last_stats: Instant,
}

impl<S: Read + Write> Worker<S> {
    fn new(conn: S, cmds: Receiver<Cmd>, events: Sender<VmEvent>) -> Self {
        Worker {
            conn,
            cmds,
            events,
            buf: proto::PacketBuf::default(),
            drives: Vec::new(),
            mapped: HashMap::new(),
            pending: None,
            connected_sent: false,
            last_sent: Instant::now(),
            last_stats: Instant::now(),
        }
    }

    fn send(&mut self, pkt: &[u8]) -> io::Result<()> {
        self.conn.write_all(pkt)?;
        self.conn.flush()?;
        self.last_sent = Instant::now();
        Ok(())
    }

    fn event(&self, e: VmEvent) {
        let _ = self.events.send(e);
    }

    fn fail_pending(&mut self, why: &str) {
        if let Some(p) = self.pending.take() {
            let _ = p.reply.send(Err(anyhow!("{why}")));
        }
    }

    /// Main loop. Returns the disconnect reason.
    fn run(&mut self) -> Result<String> {
        let mut tmp = vec![0u8; 64 * 1024];
        loop {
            // Commands from the GUI.
            loop {
                match self.cmds.try_recv() {
                    Ok(Cmd::Map(img, reply)) => self.start_map(img, reply)?,
                    Ok(Cmd::Unmap(kind, reply)) => {
                        let r = self.unmap_kind(kind, None);
                        let _ = reply.send(r);
                    }
                    Ok(Cmd::UsbReset) => self.send(&proto::usb_reset())?,
                    Ok(Cmd::Disconnect) | Err(TryRecvError::Disconnected) => {
                        let ids: Vec<u16> = self.mapped.keys().copied().collect();
                        for id in ids {
                            let _ = self.send(&proto::release(id));
                            if let Some(m) = self.mapped.remove(&id) {
                                self.event(VmEvent::Unmapped { kind: m.kind, reason: None });
                            }
                        }
                        return Ok("disconnected".into());
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }

            if let Some(p) = &self.pending
                && Instant::now() > p.deadline
            {
                self.fail_pending("no free virtual drive on the iDRAC. Check that Virtual Media is set to Attach or Auto-attach (iDRAC web UI: System > Console/Media > Configuration) and that nobody else has it in use.");
            }
            if let Some(p) = self.pending.as_mut()
                && p.retry_at.is_some_and(|t| Instant::now() >= t)
            {
                p.retry_at = None;
                self.send(&proto::get_vdisk_info())?;
            }
            if self.last_sent.elapsed() >= HEARTBEAT_EVERY {
                self.send(&proto::heartbeat())?;
            }
            if self.last_stats.elapsed() >= Duration::from_millis(500) {
                self.last_stats = Instant::now();
                for m in self.mapped.values_mut().filter(|m| m.stats_dirty) {
                    m.stats_dirty = false;
                    let _ = self.events.send(VmEvent::Stats {
                        kind: m.kind,
                        read_bytes: m.read_bytes,
                        write_bytes: m.write_bytes,
                    });
                }
            }

            match self.conn.read(&mut tmp) {
                Ok(0) => return Ok("connection closed by the iDRAC".into()),
                Ok(n) => self.buf.extend(&tmp[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
            while let Some(pkt) = self.buf.next_packet()? {
                if let Some(reason) = self.handle(&pkt)? {
                    return Ok(reason);
                }
            }
        }
    }

    fn start_map(&mut self, img: VmImage, reply: Sender<Result<()>>) -> Result<()> {
        if self.pending.is_some() {
            let _ = reply.send(Err(anyhow!("another map request is in progress")));
            return Ok(());
        }
        if self.mapped.values().any(|m| m.kind == img.kind) {
            let _ = reply.send(Err(anyhow!("a {:?} image is already mapped; unmap it first", img.kind)));
            return Ok(());
        }
        // Refresh the drive table first, like the Java client.
        self.send(&proto::get_vdisk_info())?;
        self.pending = Some(PendingMap { image: img, reply, deadline: Instant::now() + MAP_TIMEOUT, retry_at: None });
        Ok(())
    }

    fn finish_map(&mut self, p: PendingMap) -> Result<()> {
        let r = self.do_map(&p.image);
        let ok = r.is_ok();
        let _ = p.reply.send(r);
        if ok {
            self.event(VmEvent::Mapped { kind: p.image.kind, path: p.image.path.clone() });
        }
        Ok(())
    }

    fn free_drive(&self, kind: DriveKind) -> Option<(u16, u8)> {
        self.drives.iter().enumerate().find_map(|(i, d)| {
            let fits = match kind {
                DriveKind::Cd => d.drive_type == proto::REMOTE_TYPE_CD,
                DriveKind::Floppy => {
                    d.drive_type == proto::REMOTE_TYPE_FLOPPY || d.drive_type == proto::REMOTE_TYPE_REMOVABLE
                }
            };
            (fits && d.status == proto::DRIVE_STATUS_IDLE && !self.mapped.contains_key(&(i as u16)))
                .then_some((i as u16, d.drive_type))
        })
    }

    fn do_map(&mut self, img: &VmImage) -> Result<()> {
        let (disk, remote_type) = self.free_drive(img.kind).ok_or_else(|| {
            anyhow!("no free virtual drive on the iDRAC (virtual media detached, or already in use)")
        })?;
        let image = match img.kind {
            DriveKind::Cd => Image::open_iso(&img.path),
            DriveKind::Floppy => {
                Image::open_img(&img.path, img.read_only, remote_type == proto::REMOTE_TYPE_REMOVABLE)
            }
        }
        .with_context(|| format!("opening {}", img.path.display()))?;
        let g = image.geometry;
        let pkt = if image.is_cd {
            proto::map_cd(disk, g.block_size, g.blocks, &proto::ISO_TOC)
        } else {
            proto::map_disk(disk, g.block_size, g.blocks, image.read_only, g.cylinders, g.heads, g.sectors)
        };
        self.send(&pkt)?;
        log::info!("vmedia: mapped {} on remote drive {disk} ({g:?})", img.path.display());
        self.mapped.insert(
            disk,
            MappedDrive {
                kind: img.kind,
                image,
                read_bytes: 0,
                write_bytes: 0,
                stats_dirty: false,
            },
        );
        Ok(())
    }

    fn unmap_kind(&mut self, kind: DriveKind, reason: Option<String>) -> Result<()> {
        let id = self
            .mapped
            .iter()
            .find(|(_, m)| m.kind == kind)
            .map(|(id, _)| *id)
            .ok_or_else(|| anyhow!("no {kind:?} image is mapped"))?;
        self.unmap_id(id, reason)?;
        Ok(())
    }

    fn unmap_id(&mut self, id: u16, reason: Option<String>) -> io::Result<()> {
        if let Some(m) = self.mapped.remove(&id) {
            self.send(&proto::release(id))?;
            self.event(VmEvent::Unmapped { kind: m.kind, reason });
        }
        Ok(())
    }

    /// Handle one packet. Returns `Some(reason)` when the session ends.
    fn handle(&mut self, pkt: &[u8]) -> Result<Option<String>> {
        match proto::decode(pkt)? {
            Incoming::VDiskInfo(drives) => {
                log::debug!("vmedia drives: {drives:?}");
                self.drives = drives;
                if !self.connected_sent {
                    self.connected_sent = true;
                    self.event(VmEvent::Connected { drives: self.drives.clone() });
                }
                if let Some(mut p) = self.pending.take() {
                    // Right after connecting in auto-attach mode the drives
                    // report "disabled" for a moment; wait for them.
                    let attaching = self.drives.iter().any(|d| d.status == proto::DRIVE_STATUS_DISABLED);
                    if self.free_drive(p.image.kind).is_none() && attaching && Instant::now() < p.deadline {
                        p.retry_at = Some(Instant::now() + Duration::from_secs(1));
                        self.pending = Some(p);
                    } else {
                        self.finish_map(p)?;
                    }
                }
            }
            Incoming::Read { disk, start, blocks, blocking_factor } => {
                self.handle_read(disk, start, blocks, blocking_factor)?;
            }
            Incoming::Write { disk, start, blocks, data } => self.handle_write(disk, start, blocks, &data)?,
            Incoming::DeviceStatus { disk, status } => {
                if let Some(msg) = proto::device_status_error(status) {
                    log::warn!("vmedia: drive {disk} status {status}: {msg}");
                    self.unmap_id(disk, Some(msg.to_string()))?;
                }
            }
            Incoming::RequestRelease { disk } => {
                self.unmap_id(disk, Some("released by the iDRAC".into()))?;
            }
            Incoming::ConfigOption { option, value } => match option {
                1 => self.event(VmEvent::ReadOnlyForced(value == 1)),
                2 => self.event(VmEvent::LockedToKvm(value == 1)),
                _ => log::debug!("vmedia config option {option} = {value}"),
            },
            Incoming::Disconnect { reason, .. } => {
                return Ok(Some(proto::disconnect_reason_text(reason).to_string()));
            }
            Incoming::DisconnectCancel => {}
            Incoming::Other(t) => log::debug!("vmedia: ignoring packet type {t}"),
        }
        Ok(None)
    }

    fn handle_read(&mut self, disk: u16, start: u32, blocks: u32, factor: u32) -> Result<()> {
        let Some(m) = self.mapped.get_mut(&disk) else {
            log::warn!("vmedia: read for unmapped drive {disk}");
            self.send(&proto::client_status(disk, proto::CLIENT_STATUS_READ_ERROR))?;
            return Ok(());
        };
        let bs = m.image.block_size().max(1);
        let kind = m.kind;
        // Real reads are tens of KB. Refuse anything absurd instead of
        // trying to serve it, including a range past block 2^32.
        let too_big = blocks as u64 * bs as u64 > MAX_READ_TOTAL;
        if too_big || start.checked_add(blocks).is_none() {
            log::warn!("vmedia: refusing read of {blocks} blocks at {start}");
            self.send(&proto::client_status(disk, proto::CLIENT_STATUS_READ_ERROR))?;
            self.event(VmEvent::Error(format!("{kind:?}: refused a read of {blocks} blocks at block {start}")));
            return Ok(());
        }
        // Answer in chunks of at most `factor` blocks (and at most
        // MAX_READ_CHUNK bytes), sending each one as soon as it is read.
        let factor = if factor == 0 { blocks } else { factor }.clamp(1, (MAX_READ_CHUNK / bs).max(1));
        let mut ok = true;
        let (mut s, mut left) = (start, blocks);
        while left > 0 {
            let n = left.min(factor);
            let Some(m) = self.mapped.get_mut(&disk) else { break };
            match m.image.read_blocks(s, n) {
                Ok(data) => {
                    m.read_bytes += data.len() as u64;
                    m.stats_dirty = true;
                    self.send(&proto::read_data(disk, s, n, &data))?;
                }
                Err(e) => {
                    log::warn!("vmedia: read of {n} blocks at {s} failed: {e}");
                    ok = false;
                    break;
                }
            }
            s += n;
            left -= n;
        }
        if ok {
            self.send(&proto::client_status(disk, proto::CLIENT_STATUS_OK))?;
        } else {
            self.send(&proto::client_status(disk, proto::CLIENT_STATUS_READ_ERROR))?;
            self.event(VmEvent::Error(format!("{kind:?}: read error at block {s}")));
        }
        Ok(())
    }

    fn handle_write(&mut self, disk: u16, start: u32, blocks: u32, data: &[u8]) -> Result<()> {
        let result = match self.mapped.get_mut(&disk) {
            None => Err(io::Error::other("write to unmapped drive")),
            Some(m) => {
                let len = blocks as usize * m.image.block_size() as usize;
                let r = if data.len() < len {
                    Err(io::Error::new(io::ErrorKind::InvalidData, "short write packet"))
                } else {
                    m.image.write_blocks(start, &data[..len])
                };
                if r.is_ok() {
                    m.write_bytes += len as u64;
                    m.stats_dirty = true;
                }
                r
            }
        };
        match result {
            Ok(()) => self.send(&proto::client_status(disk, proto::CLIENT_STATUS_OK))?,
            Err(e) => {
                log::warn!("vmedia: write at block {start} failed: {e}");
                self.send(&proto::client_status(disk, proto::CLIENT_STATUS_WRITE_ERROR))?;
                self.event(VmEvent::Error(format!("write failed: {e}")));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
