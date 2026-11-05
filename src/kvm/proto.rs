//! Wire format of the iDRAC6 virtual console ("AVSP", Avocent).
//!
//! See `docs/protocol.md` for the full description. In short:
//!
//! * Both sockets start with an `APCP` handshake that tells the client
//!   whether to switch to TLS.
//! * Every message has an 8 byte header followed by the payload. On the
//!   command channel the header is `"BEEF" type:u16 length:u16`; on the video
//!   channel the server only fills the low byte of the type (byte 5). The
//!   length always counts the header too.
//! * All integers are big endian.

// Complete protocol definitions are kept even where the client does not
// use them yet; they document the wire format.
#![allow(dead_code)]

use std::io::{Read, Write};

use anyhow::{Result, bail};

pub const HEADER_LEN: usize = 8;
/// The Java client refuses anything larger, so do we.
pub const MAX_MESSAGE_LEN: usize = 15000;

/// Message type codes.
pub mod ty {
    // client -> server, command channel
    pub const LOGIN_V1: u16 = 0x0100;
    pub const LOGIN_V2: u16 = 0x0102;
    pub const KEYBOARD: u16 = 0x0200;
    pub const MOUSE_ABS: u16 = 0x0201;
    pub const MOUSE_ORIGIN: u16 = 0x0202;
    pub const KEYBOARD_LED_REQUEST: u16 = 0x0204;
    pub const FOCUS: u16 = 0x0208;
    pub const MOUSE_REL: u16 = 0x0209;
    pub const MOUSE_ACCEL: u16 = 0x020a;
    pub const SCREEN_REFRESH: u16 = 0x0301;
    pub const SET_DISPLAY_AREA: u16 = 0x0302;
    pub const SCALE_1TO1: u16 = 0x0304;
    pub const VIDEO_ENABLE: u16 = 0x030e;
    pub const KEEPALIVE: u16 = 0x0400;
    pub const SET_BANDWIDTH: u16 = 0x0402;
    pub const SET_COLOR_DEPTH: u16 = 0x0403;
    pub const SHARING_RESPONSE: u16 = 0x0413;
    pub const GET_SERVERS: u16 = 0x0420;
    pub const SET_POWER: u16 = 0x0424;

    // client -> server, video channel
    pub const VIDEO_ACK: u16 = 0x0000;
    pub const VIDEO_AUTH: u16 = 0x0001;

    // server -> client, command channel
    pub const KEYBOARD_LED: u16 = 0x8100;
    pub const MOUSE_ACK: u16 = 0x8101;
    pub const MOUSE_ACCEL_RESPONSE: u16 = 0x8102;
    pub const INPUT_RESOLUTION: u16 = 0x8200;
    pub const DISPLAY_RESOLUTION: u16 = 0x8201;
    pub const VIDEO_SETUP: u16 = 0x8202;
    pub const SCALING: u16 = 0x8203;
    pub const COLOR_MODE: u16 = 0x8204;
    pub const LOGIN_RESPONSE_V1: u16 = 0x8300;
    pub const USER_DISCONNECT_PENDING: u16 = 0x8301;
    pub const PROTOCOL_VERSION: u16 = 0x8302;
    pub const REQUEST_CANCELLED: u16 = 0x8304;
    pub const LOGIN_RESPONSE_V2: u16 = 0x8305;
    pub const SHARING_REQUEST: u16 = 0x8401;
    pub const USER_PRIVILEGES: u16 = 0x8420;
    pub const SERVER_NAMES: u16 = 0x8430;
    pub const SELECTED_SERVER: u16 = 0x8431;
    pub const SERVER_STATUS: u16 = 0x8432;
    pub const REQUEST_CREDENTIALS: u16 = 0x8434;
    pub const NOOP: u16 = 0xffff;

    // server -> client, video channel (8 bit type)
    pub const VIDEO_DVC: u8 = 0x80;
    /// Avocent DVC "Video Packet" types used by iDRAC6.
    pub const VIDEO_DVC_FIRST: u8 = 0x81;
    pub const VIDEO_DVC_LAST: u8 = 0x83;
    pub const VIDEO_CONNECT_STATUS: u8 = 0x84;
    pub const VIDEO_STOPPED: u8 = 0x85;
    pub const VIDEO_ASPEED: u8 = 0x86;
    pub const VIDEO_TEXT: u8 = 0x87;
}

/// A received message: its type and payload (header stripped).
#[derive(Debug, Clone)]
pub struct Message {
    pub ty: u16,
    pub payload: Vec<u8>,
}

/// Build a command-channel message: `"BEEF" ty len payload`.
pub fn beef(ty: u16, payload: &[u8]) -> Vec<u8> {
    let len = (HEADER_LEN + payload.len()) as u16;
    let mut v = Vec::with_capacity(len as usize);
    v.extend_from_slice(b"BEEF");
    v.extend_from_slice(&ty.to_be_bytes());
    v.extend_from_slice(&len.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Build a video-channel message with the short header `0 0 0 0 1 ty len`.
pub fn video_short(ty: u8, payload: &[u8]) -> Vec<u8> {
    let len = (HEADER_LEN + payload.len()) as u16;
    let mut v = vec![0, 0, 0, 0, 1, ty];
    v.extend_from_slice(&len.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Incremental frame parser that works with sockets using read timeouts.
pub struct FrameReader {
    buf: Vec<u8>,
    start: usize,
    /// When true the type is the 16 bit field at bytes 4..6, otherwise only
    /// byte 5 (video channel).
    wide_type: bool,
}

impl FrameReader {
    pub fn new(wide_type: bool) -> Self {
        Self { buf: Vec::with_capacity(32 * 1024), start: 0, wide_type }
    }

    /// Read whatever is available (blocking until the socket's read timeout)
    /// and return `Ok(false)` on EOF.
    pub fn fill<R: Read>(&mut self, r: &mut R) -> Result<bool> {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 64 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let old = self.buf.len();
        self.buf.resize(old + 16 * 1024, 0);
        match r.read(&mut self.buf[old..]) {
            Ok(0) => {
                self.buf.truncate(old);
                Ok(false)
            }
            Ok(n) => {
                self.buf.truncate(old + n);
                Ok(true)
            }
            Err(e) if is_timeout(&e) => {
                self.buf.truncate(old);
                Ok(true)
            }
            Err(e) => {
                self.buf.truncate(old);
                Err(e.into())
            }
        }
    }

    /// Pop the next complete message, if any.
    pub fn next(&mut self) -> Result<Option<Message>> {
        let avail = &self.buf[self.start..];
        if avail.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = u16::from_be_bytes([avail[6], avail[7]]) as usize;
        if !(HEADER_LEN..=MAX_MESSAGE_LEN).contains(&len) {
            bail!("bad message length {len} (header {:02x?})", &avail[..HEADER_LEN]);
        }
        if avail.len() < len {
            return Ok(None);
        }
        let ty = if self.wide_type {
            u16::from_be_bytes([avail[4], avail[5]])
        } else {
            avail[5] as u16
        };
        let payload = avail[HEADER_LEN..len].to_vec();
        self.start += len;
        Ok(Some(Message { ty, payload }))
    }
}

/// True for the errors a socket read timeout produces on Linux and Windows.
pub fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

// ---------------------------------------------------------------------------
// APCP handshake
// ---------------------------------------------------------------------------

/// Which channel an APCP request opens.
#[derive(Clone, Copy, Debug)]
pub enum ApcpChannel {
    Command = 3,
    Video = 4,
}

/// Capabilities the client offers: 1 = plain TCP, 4 = SSL.
const APCP_CAPS_PLAIN: u32 = 1;
const APCP_CAPS_SSL: u32 = 4;

/// What the server answered.
#[derive(Debug)]
pub struct ApcpReply {
    pub use_ssl: bool,
    /// Non-zero when the server wants us to reconnect to another port.
    pub redirect_port: u16,
}

pub fn apcp_request(channel: ApcpChannel) -> Vec<u8> {
    let random: [u8; 32] = rand::random();
    let mut v = Vec::with_capacity(53);
    v.extend_from_slice(b"APCP");
    v.extend_from_slice(&53u32.to_be_bytes());
    v.extend_from_slice(&0x0100u16.to_be_bytes()); // message type: request
    v.extend_from_slice(&0u16.to_be_bytes());
    v.push(channel as u8);
    v.extend_from_slice(&[0, 0, 0]);
    v.extend_from_slice(&(APCP_CAPS_PLAIN | APCP_CAPS_SSL).to_be_bytes());
    v.push(32);
    v.extend_from_slice(&random);
    v
}

/// Send the APCP request and parse the reply. The stream must be plain TCP.
pub fn apcp_handshake<S: Read + Write>(s: &mut S, channel: ApcpChannel) -> Result<ApcpReply> {
    s.write_all(&apcp_request(channel))?;
    s.flush()?;
    let mut r = [0u8; 53];
    s.read_exact(&mut r)
        .map_err(|e| anyhow::anyhow!("no APCP reply from the iDRAC ({e})"))?;
    if &r[0..4] != b"APCP" {
        bail!("bad APCP reply header {:02x?}", &r[0..4]);
    }
    let msg_type = u16::from_be_bytes([r[8], r[9]]);
    if msg_type != 0x8100 {
        bail!("unexpected APCP reply type {msg_type:#06x}");
    }
    let caps = u32::from_be_bytes([r[14], r[15], r[16], r[17]]);
    let port = u16::from_be_bytes([r[18], r[19]]);
    let use_ssl = if caps & APCP_CAPS_PLAIN != 0 {
        false
    } else if caps & APCP_CAPS_SSL != 0 {
        true
    } else {
        bail!("server refused the console connection (APCP capabilities {caps:#x})");
    };
    Ok(ApcpReply { use_ssl, redirect_port: port })
}

// ---------------------------------------------------------------------------
// Client -> server messages
// ---------------------------------------------------------------------------

fn put_padded(v: &mut Vec<u8>, s: &str, width: usize) {
    let b = s.as_bytes();
    let n = b.len().min(width);
    v.push(n as u8);
    v.extend_from_slice(&b[..n]);
    v.extend(std::iter::repeat_n(0, width - n));
}

/// Login request (v2 adds the share-mode byte).
///
/// `user`/`password` are the one-time tokens from the JNLP file, not the real
/// iDRAC credentials.
pub fn login(user: &str, password: &str, port: u8, channel: u8, client_random: u32, shared: bool, v2: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(210);
    put_padded(&mut p, user, 96);
    put_padded(&mut p, password, 96);
    p.extend_from_slice(&[0u8; 8]); // RIP id, unused on iDRAC
    p.push(port);
    p.push(channel);
    p.extend_from_slice(&client_random.to_be_bytes());
    if v2 {
        p.push(if shared { 1 } else { 0 });
    }
    beef(if v2 { ty::LOGIN_V2 } else { ty::LOGIN_V1 }, &p)
}

/// First message on the video socket: proves it belongs to our session.
pub fn video_auth(client_random: u32, session_id: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(8);
    p.extend_from_slice(&client_random.to_be_bytes());
    p.extend_from_slice(&session_id.to_be_bytes());
    video_short(ty::VIDEO_AUTH as u8, &p)
}

pub fn video_ack(frames: u8) -> Vec<u8> {
    beef(ty::VIDEO_ACK, &[frames, 0, 0, 0, 0, 0, 0, 0])
}

pub fn keepalive() -> Vec<u8> {
    beef(ty::KEEPALIVE, &[0; 8])
}

pub fn get_servers() -> Vec<u8> {
    beef(ty::GET_SERVERS, &[0; 8])
}

pub fn video_enable(on: bool) -> Vec<u8> {
    beef(ty::VIDEO_ENABLE, &[on as u8, 1, 0, 0, 0, 0, 0, 0])
}

pub fn set_display_area(w: u16, h: u16) -> Vec<u8> {
    let mut p = [0u8; 8];
    p[0..2].copy_from_slice(&w.to_be_bytes());
    p[2..4].copy_from_slice(&h.to_be_bytes());
    beef(ty::SET_DISPLAY_AREA, &p)
}

pub fn scale_1to1() -> Vec<u8> {
    beef(ty::SCALE_1TO1, &[0; 8])
}

pub fn screen_refresh() -> Vec<u8> {
    beef(ty::SCREEN_REFRESH, &[0; 8])
}

/// Key press or release. `usage` is a USB HID keyboard usage id.
pub fn key(usage: u16, pressed: bool) -> Vec<u8> {
    let u = usage.to_be_bytes();
    beef(ty::KEYBOARD, &[0, if pressed { 0 } else { 1 }, u[0], u[1], 0, 0, 0, 0])
}

/// Mouse button bits.
pub mod buttons {
    pub const LEFT: u8 = 1;
    pub const RIGHT: u8 = 2;
    pub const MIDDLE: u8 = 4;
}

/// Absolute mouse state. `x`/`y` are in remote screen pixels, `buttons` is
/// the button state after the event and `wheel` is +1 (away from the user)
/// or -1 (towards). The server derives press/release from the button state.
pub fn mouse_abs(x: u16, y: u16, buttons: u8, wheel: i16) -> Vec<u8> {
    let mut p = [0u8; 8];
    p[1] = buttons;
    p[2..4].copy_from_slice(&x.to_be_bytes());
    p[4..6].copy_from_slice(&y.to_be_bytes());
    p[6..8].copy_from_slice(&wheel.to_be_bytes());
    beef(ty::MOUSE_ABS, &p)
}

pub fn focus(has_focus: bool) -> Vec<u8> {
    beef(ty::FOCUS, &[has_focus as u8, 0, 0, 0, 0, 0, 0, 0])
}

pub fn keyboard_led_request() -> Vec<u8> {
    beef(ty::KEYBOARD_LED_REQUEST, &[0; 8])
}

pub fn mouse_accel(value: u8) -> Vec<u8> {
    beef(ty::MOUSE_ACCEL, &[value, 0, 0, 0, 0, 0, 0, 0])
}

/// Bandwidth limit, 0 = unlimited .. 4 = lowest (see [`Bandwidth`]).
pub fn set_bandwidth(level: u8) -> Vec<u8> {
    beef(ty::SET_BANDWIDTH, &[level.min(4), 0, 0, 0, 0, 0, 0, 0])
}

/// Colour depth request.
pub fn set_color_depth(depth: u8, color: bool) -> Vec<u8> {
    beef(ty::SET_COLOR_DEPTH, &[0, depth, color as u8, 0, 0, 0, 0, 0])
}

/// Answer to a session sharing request from another user.
pub fn sharing_response(request_id: u16, answer: SharingAnswer) -> Vec<u8> {
    let r = request_id.to_be_bytes();
    beef(ty::SHARING_RESPONSE, &[answer as u8, 0, r[0], r[1], 0, 0, 0, 0])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharingAnswer {
    Reject = 1,
    Approve = 2,
    ReadOnly = 3,
}

/// Server power actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    On = 1,
    Off = 2,
    Cycle = 3,
    Reset = 4,
    GracefulShutdown = 5,
}

impl PowerAction {
    pub const ALL: [PowerAction; 5] = [Self::On, Self::GracefulShutdown, Self::Off, Self::Reset, Self::Cycle];

    pub fn label(self) -> &'static str {
        match self {
            Self::On => "Power On",
            Self::Off => "Power Off",
            Self::Cycle => "Power Cycle",
            Self::Reset => "Reset (warm boot)",
            Self::GracefulShutdown => "Graceful Shutdown",
        }
    }
}

pub fn set_power(server_id: &str, action: PowerAction) -> Vec<u8> {
    let mut p = Vec::with_capacity(server_id.len() + 3);
    p.extend_from_slice(&(server_id.len() as u16).to_be_bytes());
    p.extend_from_slice(server_id.as_bytes());
    p.push(action as u8);
    beef(ty::SET_POWER, &p)
}

/// Video bandwidth limits offered by the original viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bandwidth {
    Unlimited = 0,
    Mbit10 = 1,
    Mbit1 = 2,
    Kbit512 = 3,
    Kbit256 = 4,
}

impl Bandwidth {
    pub const ALL: [Bandwidth; 5] = [Self::Unlimited, Self::Mbit10, Self::Mbit1, Self::Kbit512, Self::Kbit256];
    pub fn label(self) -> &'static str {
        match self {
            Self::Unlimited => "Unlimited",
            Self::Mbit10 => "10 Mbit/s",
            Self::Mbit1 => "1 Mbit/s",
            Self::Kbit512 => "512 kbit/s",
            Self::Kbit256 => "256 kbit/s",
        }
    }
}

// ---------------------------------------------------------------------------
// Server -> client messages
// ---------------------------------------------------------------------------

fn be16(p: &[u8], o: usize) -> u16 {
    p.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).unwrap_or(0)
}
fn be32(p: &[u8], o: usize) -> u32 {
    p.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct LoginResponse {
    pub status: u8,
    /// Echoed back in the video channel auth message.
    pub session_id: u32,
    pub flags: u16,
    pub message: String,
}

impl LoginResponse {
    pub fn parse(p: &[u8], v2: bool) -> Self {
        if v2 {
            let n = p.get(10).copied().unwrap_or(0) as usize;
            Self {
                status: p.first().copied().unwrap_or(0xff),
                session_id: be32(p, 4),
                flags: be16(p, 8),
                message: String::from_utf8_lossy(p.get(11..11 + n).unwrap_or(&[])).into(),
            }
        } else {
            let n = p.get(8).copied().unwrap_or(0) as usize;
            Self {
                status: p.first().copied().unwrap_or(0xff),
                session_id: be32(p, 4),
                flags: p.get(3).copied().unwrap_or(0) as u16,
                message: String::from_utf8_lossy(p.get(9..9 + n).unwrap_or(&[])).into(),
            }
        }
    }
    /// Video is multiplexed on the command socket instead of a second one.
    pub fn video_on_command_channel(&self) -> bool {
        self.flags & 1 != 0
    }
    pub fn view_only(&self) -> bool {
        self.flags & 0x20 != 0 && self.flags & 0x40 != 0
    }
    pub fn failure_reason(&self) -> Option<&'static str> {
        Some(match self.status {
            0 => return None,
            1 | 2 => "Login failed: the session token was rejected. Log in again.",
            3 => "Access denied: this user may not use the virtual console.",
            4 | 12 => "The virtual console is already in use by another user.",
            8 => "All virtual console channels are in use.",
            15 => "The current console user denied the sharing request.",
            16 => "The sharing request timed out.",
            _ => "Login failed.",
        })
    }
}

/// One entry of the AvailableServerNames / ServerStatusUpdate messages.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub id: String,
    pub power_state: u16,
    pub status: u16,
    pub name: String,
}

fn parse_server(p: &[u8], mut o: usize) -> (ServerInfo, usize) {
    let mut s = ServerInfo::default();
    let n = be16(p, o) as usize;
    o += 2;
    s.id = String::from_utf8_lossy(p.get(o..o + n).unwrap_or(&[])).into();
    o += n;
    s.power_state = be16(p, o) & 3;
    o += 2;
    s.status = be16(p, o);
    o += 2;
    let n = be16(p, o) as usize;
    o += 2;
    s.name = String::from_utf8_lossy(p.get(o..o + n).unwrap_or(&[])).into();
    o += n;
    (s, o)
}

pub fn parse_server_names(p: &[u8]) -> Vec<ServerInfo> {
    let count = be16(p, 0) as usize;
    let mut o = 2;
    let mut v = Vec::new();
    for _ in 0..count.min(64) {
        if o >= p.len() {
            break;
        }
        let (s, no) = parse_server(p, o);
        v.push(s);
        o = no;
    }
    v
}

pub fn parse_server_status(p: &[u8]) -> ServerInfo {
    parse_server(p, 0).0
}

pub fn parse_selected_server(p: &[u8]) -> String {
    let n = be16(p, 0) as usize;
    String::from_utf8_lossy(p.get(2..2 + n).unwrap_or(&[])).into()
}

#[derive(Debug, Clone)]
pub struct SharingRequest {
    pub id: u16,
    pub kind: u8,
    pub user: String,
}

pub fn parse_sharing_request(p: &[u8]) -> SharingRequest {
    SharingRequest {
        id: be16(p, 0),
        kind: p.get(2).copied().unwrap_or(0),
        user: p.get(3..).unwrap_or(&[]).iter().filter(|&&c| c != 0).map(|&c| c as char).collect(),
    }
}

/// Keyboard LED state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Leds {
    pub num: bool,
    pub caps: bool,
    pub scroll: bool,
}

pub fn parse_leds(p: &[u8]) -> Leds {
    let b = p.first().copied().unwrap_or(0);
    Leds { num: b & 1 != 0, caps: b & 2 != 0, scroll: b & 4 != 0 }
}

pub fn parse_resolution(p: &[u8]) -> (u16, u16) {
    (be16(p, 0), be16(p, 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beef_header() {
        let m = keepalive();
        assert_eq!(&m[..8], b"BEEF\x04\x00\x00\x10");
        assert_eq!(m.len(), 16);
    }

    #[test]
    fn login_layout() {
        let m = login("1804289383", "846930886", 1, 0, 0x01020304, false, true);
        assert_eq!(m.len(), 217);
        assert_eq!(&m[4..8], &[0x01, 0x02, 0x00, 217]);
        assert_eq!(m[8], 10);
        assert_eq!(&m[9..19], b"1804289383");
        assert_eq!(m[8 + 97], 9);
        assert_eq!(&m[8 + 194..8 + 202], &[0; 8]);
        assert_eq!(m[210], 1);
        assert_eq!(m[211], 0);
        assert_eq!(&m[212..216], &[1, 2, 3, 4]);
        assert_eq!(m[216], 0);
    }

    #[test]
    fn video_auth_layout() {
        let m = video_auth(7, 9);
        assert_eq!(m, vec![0, 0, 0, 0, 1, 1, 0, 16, 0, 0, 0, 7, 0, 0, 0, 9]);
    }

    #[test]
    fn frame_reader_splits() {
        let mut data = keepalive();
        data.extend(beef(0x8305, &[1, 2, 3]));
        let mut fr = FrameReader::new(true);
        let mut cur = std::io::Cursor::new(data);
        assert!(fr.fill(&mut cur).unwrap());
        let a = fr.next().unwrap().unwrap();
        assert_eq!(a.ty, ty::KEEPALIVE);
        let b = fr.next().unwrap().unwrap();
        assert_eq!(b.ty, 0x8305);
        assert_eq!(b.payload, vec![1, 2, 3]);
        assert!(fr.next().unwrap().is_none());
    }

    #[test]
    fn apcp_request_layout() {
        let r = apcp_request(ApcpChannel::Command);
        assert_eq!(r.len(), 53);
        assert_eq!(&r[..4], b"APCP");
        assert_eq!(r[12], 3);
        assert_eq!(&r[16..20], &[0, 0, 0, 5]);
        assert_eq!(r[20], 32);
    }

    #[test]
    fn power_layout() {
        let m = set_power("abc", PowerAction::Cycle);
        assert_eq!(&m[4..8], &[0x04, 0x24, 0, 14]);
        assert_eq!(&m[8..], &[0, 3, b'a', b'b', b'c', 3]);
    }
}
