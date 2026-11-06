//! Wire format of the iDRAC6 Virtual Media ("AVMP") protocol.
//!
//! See the module documentation in `mod.rs` for the protocol description.
//! Everything here is pure encode/decode so it can be unit tested without a
//! socket.

// Complete protocol definitions are kept even where the client does not
// use them yet; they document the wire format.
#![allow(dead_code)]

use std::io::{self, Read};

/// "APCP" session-setup magic (shared with the KVM channel).
pub const APCP_MAGIC: &[u8; 4] = b"APCP";
/// "AVMP" packet magic used for every packet after session setup.
pub const AVMP_MAGIC: &[u8; 4] = b"AVMP";
/// Size of the common AVMP header: magic(4) + total length(4) + type(2).
pub const HEADER_LEN: usize = 10;

/// APCP session request "application" byte for Virtual Media (KVM uses 3/4).
pub const APCP_APP_VMEDIA: u8 = 2;
/// Connection capability bits returned by the appliance in the APCP reply.
pub const CAP_TCP_CLEARTEXT: u32 = 1;
pub const CAP_TCP_RC4: u32 = 2;
pub const CAP_SSL_ANONYMOUS: u32 = 4;
pub const CAP_SSL_CERT: u32 = 8;

/// Packet types (u16 at offset 8).
pub mod ty {
    // Client -> appliance
    pub const LOGIN: u16 = 0x0100; // 256
    pub const LOGIN_CERT: u16 = 0x0101; // 257 (not used by iDRAC)
    pub const LOGIN_RANDOM: u16 = 0x0102; // 258 (not used by iDRAC)
    pub const GET_VDISK_INFO: u16 = 512;
    pub const VDISK_REQUEST: u16 = 528; // map CD/DVD
    pub const VDISK_REQUEST2: u16 = 529; // map floppy / removable disk
    pub const VCARD_REQUEST: u16 = 531; // smart card (unsupported)
    pub const VDISK_RELEASE: u16 = 544; // unmap
    pub const VDISK_SET_ENABLE: u16 = 560;
    pub const VDISK_READ_DATA: u16 = 768;
    pub const VCARD_DATA_BLOCK: u16 = 769;
    pub const VDISK_ALTERNATE_TOC_DATA: u16 = 800;
    pub const HEARTBEAT: u16 = 1024;
    pub const CLIENT_STATUS: u16 = 1040;
    pub const USB_RESET: u16 = 1056;
    pub const CLIENT_CONFIGURATION_OPTION: u16 = 1072;
    // Appliance -> client
    pub const LOGIN_STATUS: u16 = 0x8100; // 33024
    pub const DISCONNECT: u16 = 33040;
    pub const DISCONNECT_CANCEL: u16 = 33056;
    pub const VDISK_INFO: u16 = 33280;
    pub const VDISK_REQUEST_RELEASE: u16 = 33344;
    pub const VDISK_READ: u16 = 33536;
    pub const VCARD_XFER_BLOCK: u16 = 33537;
    pub const VDISK_WRITE: u16 = 33552;
    pub const VDISK_GET_ALTERNATE_TOC_DATA: u16 = 33568;
    pub const DEVICE_STATUS: u16 = 33808;
    pub const DEVICE_CONFIGURATION_OPTION: u16 = 33840;
}

/// Disk id meaning "all drives" (used by GET_VDISK_INFO and USB_RESET).
pub const ALL_DRIVES: u16 = 0xFFFF;

/// CLIENT_STATUS values.
pub const CLIENT_STATUS_OK: u32 = 0;
pub const CLIENT_STATUS_READ_ERROR: u32 = 2;
pub const CLIENT_STATUS_WRITE_ERROR: u32 = 3;

/// Remote (server side) virtual drive types reported in VDISK_INFO.
pub const REMOTE_TYPE_FLOPPY: u8 = 1;
pub const REMOTE_TYPE_CD: u8 = 2;
/// Removable disk / USB key style drive. The Java client calls this
/// "floppy mode": when an image is mapped on it, only the first MBR
/// partition is exposed.
pub const REMOTE_TYPE_REMOVABLE: u8 = 4;

/// Remote drive status in VDISK_INFO.
pub const DRIVE_STATUS_IDLE: u8 = 0;

/// The fixed READ TOC (format 0) blob the Java client sends for every ISO
/// image: one data track starting at LBA 0, lead-out at 74:12:60 MSF.
pub const ISO_TOC: [u8; 20] = [
    0x00, 0x12, 0x01, 0x01, // TOC length 18, first track 1, last track 1
    0x00, 0x14, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, // track 1: ADR/CTL 0x14, LBA 0
    0x00, 0x14, 0xAA, 0x00, 0x00, 0x4A, 0x0C, 0x3C, // lead-out (0xAA), MSF 74:12:60
];

fn put_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Start an AVMP packet: magic, placeholder length, type. Call
/// [`finish`] after appending the body.
fn begin(ty: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(64);
    b.extend_from_slice(AVMP_MAGIC);
    put_u32(&mut b, 0);
    put_u16(&mut b, ty);
    b
}
fn finish(mut b: Vec<u8>) -> Vec<u8> {
    let len = b.len() as u32;
    b[4..8].copy_from_slice(&len.to_be_bytes());
    b
}

// ---------------------------------------------------------------------------
// Session setup (APCP)
// ---------------------------------------------------------------------------

/// APCP session request (53 bytes).
pub fn apcp_request(app: u8, requested_caps: u32, random: &[u8; 32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(53);
    b.extend_from_slice(APCP_MAGIC);
    put_u32(&mut b, 53);
    put_u16(&mut b, 0x0100); // message type: session request
    put_u16(&mut b, 0);
    b.push(app);
    b.push(0);
    b.push(0);
    b.push(0);
    put_u32(&mut b, requested_caps);
    b.push(32);
    b.extend_from_slice(random);
    b
}

/// Parsed APCP session setup reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSetup {
    pub version_major: u8,
    pub version_minor: u8,
    pub capabilities: u32,
    pub tcp_port: u16,
    pub random: Vec<u8>,
}

/// Read and parse the 53-byte APCP session setup reply.
pub fn read_apcp_reply<R: Read>(r: &mut R) -> io::Result<SessionSetup> {
    let mut b = [0u8; 53];
    r.read_exact(&mut b)?;
    parse_apcp_reply(&b)
}

pub fn parse_apcp_reply(b: &[u8]) -> io::Result<SessionSetup> {
    if b.len() < 53 || &b[0..4] != APCP_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad APCP header"));
    }
    if be16(b, 8) != 0x8100 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected APCP message type"));
    }
    // 0 magic, 4 length, 8 type, 10 reserved, 12 major, 13 minor,
    // 14 capabilities, 18 tcp port, 20 random length, 21 random[32]
    let n = (b[20] as usize).min(32);
    Ok(SessionSetup {
        version_major: b[12],
        version_minor: b[13],
        capabilities: be32(b, 14),
        tcp_port: be16(b, 18),
        random: b[21..21 + n].to_vec(),
    })
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

/// Username/password login (type 256, 153 bytes).
///
/// Layout after the 10-byte header: pad(2), username length(1),
/// username(96, NUL padded), password(32, NUL padded, no length byte),
/// RIP id(8, zero for iDRAC), port(1), cascade channel(1), reserved(1),
/// preempt flag(1).
pub fn login(user: &str, password: &str, preempt: bool) -> Vec<u8> {
    let mut b = begin(ty::LOGIN);
    put_u16(&mut b, 0);
    let u = user.as_bytes();
    let u = &u[..u.len().min(96)];
    b.push(u.len() as u8);
    b.extend_from_slice(u);
    b.resize(b.len() + (96 - u.len()), 0);
    let p = password.as_bytes();
    let p = &p[..p.len().min(32)];
    b.extend_from_slice(p);
    b.resize(b.len() + (32 - p.len()), 0);
    b.extend_from_slice(&[0u8; 8]); // RIP id
    b.push(0); // port number (0: TcpPortSpecifiedOnPath moves it to the TCP port)
    b.push(0); // cascade channel
    b.push(0);
    b.push(preempt as u8);
    let out = finish(b);
    debug_assert_eq!(out.len(), 153);
    out
}

/// Parsed LOGIN_STATUS reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStatus {
    pub status: u8,
    pub can_reject_preemption: bool,
    /// User currently holding the channel (for "in use" statuses).
    pub username: String,
}

impl LoginStatus {
    pub fn describe(&self) -> String {
        login_status_text(self.status, &self.username)
    }
}

pub fn login_status_text(status: u8, user: &str) -> String {
    match status {
        0 => "OK".into(),
        1 => "invalid user name".into(),
        2 => "invalid password".into(),
        3 => "access to the virtual media channel denied".into(),
        4 => "virtual media channel is in use".into(),
        5 => "virtual media channel not found".into(),
        6 => format!("virtual media is in use by {user}"),
        8 => "all virtual media channels are in use".into(),
        11 => "virtual media is in use by another user".into(),
        22 => "login failed at the authentication server".into(),
        23 => "authentication server certificate invalid or expired".into(),
        n => format!("login failed (status {n})"),
    }
}

/// Parse a LOGIN_STATUS packet (full packet including the header).
pub fn parse_login_status(p: &[u8]) -> io::Result<LoginStatus> {
    if p.len() < 15 || &p[0..4] != AVMP_MAGIC || be16(p, 8) != ty::LOGIN_STATUS {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad login status packet"));
    }
    let n = p[14] as usize;
    let name = p.get(15..15 + n).unwrap_or(&[]);
    Ok(LoginStatus {
        status: p[12],
        can_reject_preemption: p[13] & 1 != 0,
        username: String::from_utf8_lossy(name).into_owned(),
    })
}

// ---------------------------------------------------------------------------
// Client -> appliance packets
// ---------------------------------------------------------------------------

/// Packet with only a disk id (GET_VDISK_INFO, VDISK_RELEASE, USB_RESET,
/// HEARTBEAT).
pub fn disk_cmd(ty: u16, disk: u16) -> Vec<u8> {
    let mut b = begin(ty);
    put_u16(&mut b, disk);
    finish(b)
}

pub fn get_vdisk_info() -> Vec<u8> {
    disk_cmd(ty::GET_VDISK_INFO, ALL_DRIVES)
}
pub fn heartbeat() -> Vec<u8> {
    disk_cmd(ty::HEARTBEAT, 0)
}
pub fn release(disk: u16) -> Vec<u8> {
    disk_cmd(ty::VDISK_RELEASE, disk)
}
pub fn usb_reset() -> Vec<u8> {
    disk_cmd(ty::USB_RESET, ALL_DRIVES)
}

fn count_byte(blocks: u32) -> u8 {
    if blocks > 255 { 0xFF } else { blocks as u8 }
}

/// Map a CD/DVD (VDISK_REQUEST, 24 + toc bytes). CDs are always read-only.
pub fn map_cd(disk: u16, block_size: u32, blocks: u32, toc: &[u8]) -> Vec<u8> {
    let mut b = begin(ty::VDISK_REQUEST);
    put_u16(&mut b, disk);
    put_u32(&mut b, block_size);
    put_u32(&mut b, blocks);
    b.push(1); // read only
    b.push(count_byte(blocks));
    put_u16(&mut b, toc.len() as u16);
    b.extend_from_slice(toc);
    finish(b)
}

/// Map a floppy / removable disk (VDISK_REQUEST2, 26 bytes).
#[allow(clippy::too_many_arguments)]
pub fn map_disk(
    disk: u16,
    block_size: u32,
    blocks: u32,
    read_only: bool,
    cylinders: u16,
    heads: u8,
    sectors: u8,
) -> Vec<u8> {
    let mut b = begin(ty::VDISK_REQUEST2);
    put_u16(&mut b, disk);
    put_u32(&mut b, block_size);
    put_u32(&mut b, blocks);
    b.push(read_only as u8);
    b.push(count_byte(blocks));
    put_u16(&mut b, cylinders);
    b.push(heads);
    b.push(sectors);
    finish(b)
}

/// VDISK_READ_DATA (20 + data bytes): one chunk of a read reply.
pub fn read_data(disk: u16, start: u32, blocks: u32, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(20 + data.len());
    b.extend_from_slice(AVMP_MAGIC);
    put_u32(&mut b, 0);
    put_u16(&mut b, ty::VDISK_READ_DATA);
    put_u16(&mut b, disk);
    put_u32(&mut b, start);
    put_u32(&mut b, blocks);
    b.extend_from_slice(data);
    finish(b)
}

/// CLIENT_STATUS (16 bytes): sent after a read or write completes.
pub fn client_status(disk: u16, status: u32) -> Vec<u8> {
    let mut b = begin(ty::CLIENT_STATUS);
    put_u16(&mut b, disk);
    put_u32(&mut b, status);
    finish(b)
}

/// CLIENT_CONFIGURATION_OPTION "reserve KVM" (21 bytes).
pub fn reserve_kvm(reserve: bool) -> Vec<u8> {
    let mut b = begin(ty::CLIENT_CONFIGURATION_OPTION);
    put_u16(&mut b, 0xFFFF);
    put_u32(&mut b, 1);
    put_u32(&mut b, 1);
    b.push(reserve as u8);
    finish(b)
}

// ---------------------------------------------------------------------------
// Appliance -> client packets
// ---------------------------------------------------------------------------

/// One remote virtual drive slot from VDISK_INFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteDrive {
    pub drive_type: u8,
    pub status: u8,
    pub capabilities: u16,
}

/// Decoded appliance packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    VDiskInfo(Vec<RemoteDrive>),
    DeviceStatus { disk: u16, status: i32 },
    Read { disk: u16, start: u32, blocks: u32, blocking_factor: u32 },
    /// `data` holds exactly `blocks * block_size` bytes once the caller
    /// slices it; here it is everything after offset 20.
    Write { disk: u16, start: u32, blocks: u32, data: Vec<u8> },
    Disconnect { reason: u8, status: u8, timeout: u8 },
    DisconnectCancel,
    RequestRelease { disk: u16 },
    ConfigOption { option: u32, value: u8 },
    Other(u16),
}

pub fn packet_type(p: &[u8]) -> u16 {
    be16(p, 8)
}

/// Decode a complete packet (header included).
pub fn decode(p: &[u8]) -> io::Result<Incoming> {
    let short = || io::Error::new(io::ErrorKind::InvalidData, "short AVMP packet");
    if p.len() < HEADER_LEN {
        return Err(short());
    }
    let need = |n: usize| if p.len() < n { Err(short()) } else { Ok(()) };
    Ok(match packet_type(p) {
        ty::VDISK_INFO => {
            need(14)?;
            let n = be16(p, 12) as usize;
            need(14 + 4 * n)?;
            Incoming::VDiskInfo(
                (0..n)
                    .map(|i| RemoteDrive {
                        drive_type: p[14 + 4 * i],
                        status: p[15 + 4 * i],
                        capabilities: be16(p, 16 + 4 * i),
                    })
                    .collect(),
            )
        }
        ty::DEVICE_STATUS => {
            need(16)?;
            Incoming::DeviceStatus { disk: be16(p, 10), status: be32(p, 12) as i32 }
        }
        ty::VDISK_READ => {
            need(24)?;
            Incoming::Read {
                disk: be16(p, 10),
                start: be32(p, 12),
                blocks: be32(p, 16),
                blocking_factor: be32(p, 20),
            }
        }
        ty::VDISK_WRITE => {
            need(20)?;
            Incoming::Write {
                disk: be16(p, 10),
                start: be32(p, 12),
                blocks: be32(p, 16),
                data: p[20..].to_vec(),
            }
        }
        ty::DISCONNECT => {
            need(15)?;
            Incoming::Disconnect { reason: p[12], status: p[13], timeout: p[14] }
        }
        ty::DISCONNECT_CANCEL => Incoming::DisconnectCancel,
        ty::VDISK_REQUEST_RELEASE => {
            need(12)?;
            Incoming::RequestRelease { disk: be16(p, 10) }
        }
        ty::DEVICE_CONFIGURATION_OPTION => {
            need(21)?;
            Incoming::ConfigOption { option: be32(p, 12), value: p[20] }
        }
        t => Incoming::Other(t),
    })
}

pub fn disconnect_reason_text(reason: u8) -> &'static str {
    match reason {
        0 => "disconnected by an administrator",
        1 => "idle timeout",
        2 => "iDRAC is rebooting",
        3 => "iDRAC firmware upgrade pending",
        4 => "channel preempted by a local user",
        6 => "virtual console closed (virtual media was locked to it)",
        _ => "disconnected by the iDRAC",
    }
}

/// Device status codes from DEVICE_STATUS. `None` means "not an error".
pub fn device_status_error(status: i32) -> Option<&'static str> {
    // Like the Java client: only these codes are errors; 0 (seen right after
    // a successful map), 1, 3, 7 and anything unlisted are informational.
    match status {
        2 => Some("read failed"),
        4 => Some("mapping failed: the virtual USB device is not connected"),
        5 => Some("mapping failed: the drive is already attached"),
        6 => Some("drive released by the iDRAC"),
        8 => Some("mapping failed"),
        -1 => Some("unknown device error"),
        _ => None,
    }
}

/// Incremental AVMP packet reader: feed bytes, take complete packets.
/// Tolerates read timeouts in the middle of a packet.
#[derive(Default)]
pub struct PacketBuf {
    buf: Vec<u8>,
}

impl PacketBuf {
    pub fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pop the next complete packet, if any.
    pub fn next_packet(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = be32(&self.buf, 4) as usize;
        if len < HEADER_LEN || len > 64 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad AVMP packet length {len}"),
            ));
        }
        if self.buf.len() < len {
            return Ok(None);
        }
        let rest = self.buf.split_off(len);
        Ok(Some(std::mem::replace(&mut self.buf, rest)))
    }
}

// ---------------------------------------------------------------------------
// Helpers for the KVM channel (credentials for virtual media)
// ---------------------------------------------------------------------------

/// KVM "RequestCredentialsMessage" (type 1063 = 0x0427) to send on the KVM
/// command channel. The appliance answers with type 33844 (0x8434).
pub fn kvm_credentials_request() -> [u8; 16] {
    [b'B', b'E', b'E', b'F', 0x04, 0x27, 0x00, 0x10, 0, 0, 0, 30, 0, 0, 0, 0]
}

/// KVM message type of the credentials reply.
pub const KVM_CREDENTIALS_RESPONSE: u16 = 33844;

/// Parse the payload (bytes after the 8-byte BEEF header) of the KVM
/// credentials reply: user name and password, each 32 bytes NUL padded.
pub fn parse_kvm_credentials(payload: &[u8]) -> Option<(String, String)> {
    if payload.len() < 64 {
        return None;
    }
    let field = |b: &[u8]| {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into_owned()
    };
    Some((field(&payload[..32]), field(&payload[32..64])))
}
