//! Decoder for the Avocent "DVC" video stream, which the iDRAC6 actually
//! sends on the video channel (message types 0x81..0x83).
//!
//! The format is described in `docs/video-dvc.md`. In short: each Video
//! Packet carries a 12 byte sub-header (size, begin/end-of-frame flags,
//! checksum) followed by a byte stream of run-length opcodes that draw
//! pixels left to right, top to bottom. Opcodes may span packet
//! boundaries, so packets are queued and decoded once the packet that ends
//! the frame (or update) has arrived, which is also when the original
//! viewer repaints.
//!
//! This is a port of `com.avocent.kvm.d.b` / `d.c` / `d.d` / `d.e` and the
//! video model `com.avocent.kvm.c.i` / `c.j` from the iDRAC6 viewer,
//! including its edge-case behaviour, verified pixel for pixel against the
//! original on a capture from a real iDRAC6.

use std::collections::VecDeque;

use anyhow::Result;

use super::Framebuffer;

/// Length of the sub-header at the start of every Video Packet payload.
pub const HEADER_LEN: usize = 12;

/// The model refuses sizes above this (`com.avocent.kvm.c.i.b`).
const MODEL_MAX: usize = 1600;
/// The decoder ignores packets announcing sizes outside 1..=4096.
const DECODER_MAX: usize = 4096;
/// Opaque black, the model's initial and cleared colour.
const BLACK: u32 = 0xFF00_0000;
/// Decode anyway once this much data is queued without an end-of-frame
/// packet, so a misbehaving server cannot make us buffer forever.
const MAX_QUEUED: usize = 4 * 1024 * 1024;

/// Parsed Video Packet sub-header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DvcHeader {
    /// Bytes 4..6.
    pub height: u16,
    /// Bytes 6..8.
    pub width: u16,
    /// Byte 8 bit 0: first packet of a frame (drawing restarts at pixel 0).
    pub begin: bool,
    /// Byte 8 bit 1: last packet of a frame (the screen should be redrawn).
    pub end: bool,
    /// Bytes 10..12, only meaningful with `end`: 16 bit checksum of the whole
    /// screen, 0 = not provided.
    pub checksum: u16,
}

pub fn parse_header(p: &[u8]) -> DvcHeader {
    if p.len() < HEADER_LEN {
        return DvcHeader::default();
    }
    let flags = p[8];
    let end = flags & 2 != 0;
    DvcHeader {
        height: u16::from_be_bytes([p[4], p[5]]),
        width: u16::from_be_bytes([p[6], p[7]]),
        begin: flags & 1 != 0,
        end,
        checksum: if end { u16::from_be_bytes([p[10], p[11]]) } else { 0 },
    }
}

/// How "make pixel" opcodes are coloured; chosen by the message type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 0x81: two bytes per pixel, 15 bit RGB (`d.c`).
    Rgb15,
    /// 0x82: one byte per pixel, 7 bit palette index (`d.e`).
    Palette,
    /// 0x83: one byte per pixel, 7 bit grey level (`d.d`).
    Grey,
}

impl Kind {
    fn from_type(t: u8) -> Kind {
        match t {
            0x82 => Kind::Palette,
            0x83 => Kind::Grey,
            _ => Kind::Rgb15,
        }
    }
}

/// The 128 colours of the 7 bit palette (`com.avocent.kvm.d.h`).
#[rustfmt::skip]
const PALETTE: [u32; 128] = [
    0x000000, 0x000046, 0x00007f, 0x0000bf, 0x004600, 0x004646, 0x00467f, 0x0046bf,
    0x007f00, 0x007f46, 0x007f7f, 0x007fbf, 0x00bf00, 0x00bf46, 0x00bf7f, 0x00bfbf,
    0x460000, 0x460046, 0x46007f, 0x4600bf, 0x464600, 0x464646, 0x46467f, 0x4646bf,
    0x467f00, 0x467f46, 0x467f7f, 0x467fbf, 0x46bf00, 0x46bf46, 0x46bf7f, 0x46bfbf,
    0x7f0000, 0x7f0046, 0x7f007f, 0x7f00bf, 0x7f4600, 0x7f4646, 0x7f467f, 0x7f46bf,
    0x7f7f00, 0x7f7f46, 0x7f7f7f, 0x7f7fbf, 0x7fbf00, 0x7fbf46, 0x7fbf7f, 0x7fbfbf,
    0xbf0000, 0xbf0046, 0xbf007f, 0xbf00bf, 0xbf4600, 0xbf4646, 0xbf467f, 0xbf46bf,
    0xbf7f00, 0xbf7f46, 0xbf7f7f, 0xbf7fbf, 0xbfbf00, 0xbfbf46, 0xbfbf7f, 0xbfbfbf,
    0x0000ff, 0x4600ff, 0x7f00ff, 0xbf00ff, 0x0046ff, 0x4646ff, 0x7f46ff, 0xbf46ff,
    0x007fff, 0x467fff, 0x7f7fff, 0xbf7fff, 0x00bfff, 0x46bfff, 0x7fbfff, 0xbfbfff,
    0x00ff00, 0x00ff46, 0x00ff7f, 0x00ffbf, 0x46ff00, 0x46ff46, 0x46ff7f, 0x46ffbf,
    0x7fff00, 0x7fff46, 0x7fff7f, 0x7fffbf, 0xbfff00, 0xbfff46, 0xbfff7f, 0xbfffbf,
    0xff0000, 0xff0046, 0xff007f, 0xff00bf, 0xff4600, 0xff4646, 0xff467f, 0xff46bf,
    0xff7f00, 0xff7f46, 0xff7f7f, 0xff7fbf, 0xffbf00, 0xffbf46, 0xffbf7f, 0xffbfbf,
    0x00ffff, 0x46ffff, 0x7fffff, 0xbfffff, 0xff00ff, 0xff46ff, 0xff7fff, 0xffbfff,
    0xffff00, 0xffff46, 0xffff7f, 0xffffbf, 0xffffff, 0x5f5f5f, 0x9f9f9f, 0xdfdfdf,
];

fn rgb15(v: u32) -> u32 {
    let r = (v & 0x7C00) >> 7;
    let g = (v & 0x3E0) >> 2;
    let b = (v & 0x1F) << 3;
    BLACK | r << 16 | g << 8 | b
}

struct Packet {
    kind: Kind,
    hdr: DvcHeader,
    data: Vec<u8>,
}

/// The screen the opcodes draw into (`com.avocent.kvm.c.j`). Pixels are
/// kept as ARGB like the original, because some opcodes compare whole
/// values and "no pixel" (0) differs from opaque black (0xFF000000).
struct Model {
    width: usize,
    height: usize,
    /// Pixel count `width * height` (`i.g`), may differ from `px.len()`.
    count: i64,
    px: Vec<u32>,
    /// Drawing position (`i.i`), -1 before the first frame.
    pos: i64,
}

impl Model {
    fn new() -> Self {
        Model { width: 820, height: 620, count: 820 * 620, px: vec![BLACK; 820 * 620], pos: -1 }
    }

    /// `j.b(w, h)` + `i.b(w, h)`.
    fn resize(&mut self, w: usize, h: usize) {
        self.px = vec![BLACK; w * h];
        if !(w == self.width && h == self.height || w > MODEL_MAX || h > MODEL_MAX) {
            self.width = w;
            self.height = h;
            self.count = (w * h) as i64;
        }
    }

    fn len(&self) -> i64 {
        self.px.len() as i64
    }

    /// `i.c(n)`: move the drawing position.
    fn set_pos(&mut self, n: i64) {
        self.pos = n.min(self.count);
    }

    /// `j.a(n)`: read a pixel, 0 outside the screen.
    fn get(&self, n: i64) -> u32 {
        if n < 0 || n >= self.len() { 0 } else { self.px[n as usize] }
    }

    /// `j.b(c)`: draw one pixel.
    fn put(&mut self, c: u32) {
        if self.pos == -1 {
            self.pos = 0;
        }
        if self.pos >= 0 && self.pos < self.len() {
            self.px[self.pos as usize] = c;
        }
        self.pos += 1;
    }

    /// `j.c(src, n)`: copy `n` pixels from `src` (overlapping, forwards).
    fn copy_from(&mut self, src: i64, mut n: i64) {
        let len = self.len();
        if self.pos > len {
            return;
        }
        if src + n >= len {
            n = len - src;
        }
        if self.pos + n >= len {
            n = len - self.pos;
        }
        for k in 0..n.max(0) {
            let (d, s) = (self.pos + k, src + k);
            if d >= 0 && s >= 0 && d < len && s < len {
                self.px[d as usize] = self.px[s as usize];
            }
        }
        self.pos += n;
    }

    /// `j.d(c, n)`: fill `n` pixels.
    fn fill(&mut self, c: u32, mut n: i64) {
        let len = self.len();
        if self.pos >= len {
            return;
        }
        if self.pos + n >= len {
            n = len - self.pos;
        }
        if n <= 0 || self.pos < 0 || n > len {
            return;
        }
        let s = self.pos as usize;
        self.px[s..s + n as usize].fill(c);
        self.pos += n;
    }

    /// `j.a(int[])`: draw a run of pixels; dropped (without advancing) if it
    /// would reach the end of the buffer.
    fn put_run(&mut self, run: &[u32]) {
        if self.pos == -1 {
            self.pos = 0;
        }
        let n = run.len() as i64;
        if self.pos >= self.len() || self.pos + n >= self.len() {
            return;
        }
        if self.pos >= 0 {
            let s = self.pos as usize;
            self.px[s..s + run.len()].copy_from_slice(run);
        }
        self.pos += n;
    }

    /// Screen checksum as verified by `kvm/b/q.r()`.
    fn checksum(&self, kind: Kind) -> u16 {
        let n = (self.count.max(0) as usize).min(self.px.len());
        let mut sum: u32 = 0;
        for &p in &self.px[..n] {
            let c = p & 0xFF_FFFF;
            match kind {
                Kind::Rgb15 => {
                    if c != 0 {
                        sum = (sum + ((c & 0xFF0000) >> 9 | (c & 0xFF00) >> 6 | (c & 0xFF) >> 3)) & 0xFFFF;
                    }
                }
                _ => {
                    // Index of the colour in the palette (last match), 0 if none.
                    let idx = PALETTE.iter().rposition(|&q| q == c).unwrap_or(0) as u32;
                    sum = (sum + idx) & 0xFFFF;
                }
            }
        }
        sum as u16
    }
}

/// Signals that the packet queue ran dry in the middle of an opcode.
struct Starved;

/// Stateful DVC decoder. Feed it every Video Packet in order.
pub struct DvcDecoder {
    model: Model,
    queue: VecDeque<Packet>,
    queued_bytes: usize,
    /// Packet being decoded and the read offset into its data.
    cur: Option<Packet>,
    off: usize,
    /// A begin-of-frame packet was fetched; reset the position before the
    /// next opcode (`w.g`).
    restart: bool,
    /// Seen the first begin-of-frame packet; earlier packets are dropped.
    synced: bool,
    kind: Kind,
    /// Frames completed during the current `push_packet` call.
    completed: bool,
    /// Number of end-of-frame checksums that did not match.
    pub checksum_errors: u64,
}

impl Default for DvcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DvcDecoder {
    pub fn new() -> Self {
        DvcDecoder {
            model: Model::new(),
            queue: VecDeque::new(),
            queued_bytes: 0,
            cur: None,
            off: 0,
            restart: false,
            synced: false,
            kind: Kind::Rgb15,
            completed: false,
            checksum_errors: 0,
        }
    }

    /// Forget everything, e.g. after reconnecting.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Screen size the server announced.
    #[allow(dead_code)]
    pub fn size(&self) -> (usize, usize) {
        (self.model.width, self.model.height)
    }

    /// Feed one Video Packet (message type 0x81..0x83) payload, i.e. the bytes
    /// after the 8 byte message header. Decodes as far as possible into `fb`
    /// (resizing it on resolution changes). Returns `Ok(true)` when a frame or
    /// update was completed and the screen should be redrawn.
    pub fn push_packet(&mut self, msg_type: u8, payload: &[u8], fb: &mut Framebuffer) -> Result<bool> {
        if payload.len() < HEADER_LEN {
            anyhow::bail!("short video packet ({} bytes)", payload.len());
        }
        let hdr = parse_header(payload);
        let data = payload[HEADER_LEN..].to_vec();
        self.queued_bytes += data.len();
        self.queue.push_back(Packet { kind: Kind::from_type(msg_type), hdr, data });
        // Opcodes may continue in the next packet, so only decode once the
        // end of the frame is here (the original blocks waiting instead).
        if !hdr.end && self.queued_bytes < MAX_QUEUED {
            return Ok(false);
        }
        self.completed = false;
        self.run();
        self.queued_bytes = self.queue.iter().map(|p| p.data.len()).sum();
        if self.completed {
            self.export(fb);
        }
        Ok(self.completed)
    }

    /// Copy the model into the output framebuffer (dropping alpha).
    fn export(&self, fb: &mut Framebuffer) {
        let m = &self.model;
        fb.resize(m.width, m.height);
        let n = fb.pixels.len().min(m.px.len());
        for (d, s) in fb.pixels[..n].iter_mut().zip(&m.px[..n]) {
            *d = s & 0xFF_FFFF;
        }
    }

    /// Fetch the next packet (`kvm/b/q.f()`). Returns false when none is
    /// queued.
    fn fetch(&mut self) -> bool {
        loop {
            if let Some(p) = self.cur.take() {
                if p.hdr.end {
                    // The frame that just ended is complete.
                    self.completed = true;
                    if p.hdr.checksum != 0 {
                        let got = self.model.checksum(p.kind);
                        if got != p.hdr.checksum {
                            self.checksum_errors += 1;
                            log::debug!("DVC checksum mismatch: expected {:#06x}, got {got:#06x}", p.hdr.checksum);
                        }
                    }
                }
            }
            let Some(p) = self.queue.pop_front() else { return false };
            self.kind = p.kind;
            // `b.a(hb, bof, eof)`: adopt the packet and its frame size.
            let (w, h) = (p.hdr.width as usize, p.hdr.height as usize);
            if (w != self.model.width || h != self.model.height) && (1..=DECODER_MAX).contains(&w) && (1..=DECODER_MAX).contains(&h) {
                self.model.resize(w, h);
            }
            self.restart = p.hdr.begin;
            self.off = 0;
            let begin = p.hdr.begin;
            self.cur = Some(p);
            if !self.synced && !begin {
                // Not in sync yet: skip packets until a frame starts.
                continue;
            }
            if begin {
                self.synced = true;
            }
            return true;
        }
    }

    fn cur_len(&self) -> usize {
        self.cur.as_ref().map_or(0, |p| p.data.len())
    }

    fn cur_end(&self) -> bool {
        self.cur.as_ref().is_some_and(|p| p.hdr.end)
    }

    fn byte(&self, i: usize) -> Option<u8> {
        self.cur.as_ref().and_then(|p| p.data.get(i).copied())
    }

    /// `w.f()`: start drawing the new frame at pixel 0.
    fn restart_frame(&mut self) {
        self.model.set_pos(0);
    }

    /// Run opcodes until the queue is empty.
    fn run(&mut self) {
        loop {
            if self.cur.is_none() || self.off >= self.cur_len() {
                if !self.fetch() {
                    return;
                }
            }
            if self.restart {
                self.restart_frame();
                self.restart = false;
            }
            let Some(op) = self.byte(self.off) else { continue };
            self.off += 1;
            let r = match op & 0xE0 {
                0x00 => self.op_skip(op),
                0x40 => self.op_copy_above(op),
                0x20 => self.op_copy_left(op),
                0x60 => self.op_two_colour(op),
                _ => self.op_pixel(op),
            };
            if r.is_err() {
                // Ran out of data mid-opcode (only with a malformed stream);
                // drop the rest of it like a truncated packet.
                return;
            }
            if self.restart {
                self.restart_frame();
                self.restart = false;
            }
        }
    }

    /// Read a run length (`b.g`): 5 bits from the opcode plus up to four
    /// continuation bytes with the same top three bits.
    fn run_length(&mut self, op: u8) -> Result<i64, Starved> {
        let class = op & 0xE0;
        let mut n = (op & 0x1F) as i64;
        let extra = if self.kind == Kind::Rgb15 { 0 } else { 2 };
        if self.off >= self.cur_len() {
            if self.cur_end() {
                return Ok(n + extra);
            }
            if !self.fetch() {
                return Err(Starved);
            }
        }
        let mut shift = 1;
        while shift < 5 {
            let Some(b) = self.byte(self.off) else { break };
            if b & 0xE0 != class {
                break;
            }
            self.off += 1;
            n |= ((b & 0x1F) as i64) << (shift * 5);
            shift += 1;
            if self.off >= self.cur_len() {
                if self.cur_end() {
                    break;
                }
                if !self.fetch() {
                    return Err(Starved);
                }
                if self.restart {
                    return Ok(extra);
                }
            }
        }
        Ok(n + extra)
    }

    /// 000xxxxx: leave `n` pixels unchanged.
    fn op_skip(&mut self, op: u8) -> Result<(), Starved> {
        let n = self.run_length(op)?;
        let p = self.model.pos + n;
        self.model.set_pos(p);
        Ok(())
    }

    /// 010xxxxx: copy `n` pixels from the line above.
    fn op_copy_above(&mut self, op: u8) -> Result<(), Starved> {
        let n = self.run_length(op)?;
        let src = self.model.pos - self.model.width as i64;
        if src >= 0 {
            self.model.copy_from(src, n);
        }
        Ok(())
    }

    /// 001xxxxx: repeat the pixel to the left `n` times.
    fn op_copy_left(&mut self, op: u8) -> Result<(), Starved> {
        let n = self.run_length(op)?;
        let left = self.model.pos - 1;
        let c = if left < 0 { 0 } else { self.model.get(left) };
        self.model.fill(c, n);
        Ok(())
    }

    /// 011cxxxx: two-colour mask. The colours are the pixel to the left and
    /// the nearest earlier pixel with a different value. 4 pixels from the
    /// opcode, then (if bit 4 is set) 7 pixels per following byte while that
    /// byte's top bit is set.
    fn op_two_colour(&mut self, op: u8) -> Result<(), Starved> {
        let m = &self.model;
        let left = m.get(m.pos - 1);
        // Scan backwards from the pixel to the left for a different value.
        // (Before the first frame the original starts at -2, i.e. no scan.)
        // Positions past the buffer read as 0, which then equals `left`, so
        // clamping the start to the buffer gives the same result.
        let start = if m.pos == -1 { -2 } else { m.pos - 1 };
        let mut other = left;
        if start >= 0 && !m.px.is_empty() {
            let end = (start as usize).min(m.px.len() - 1);
            if let Some(i) = m.px[..=end].iter().rposition(|&c| c != left) {
                other = m.px[i];
            }
        }
        let pick = |bit: bool| if bit { other } else { left };
        let run4 = [pick(op & 8 != 0), pick(op & 4 != 0), pick(op & 2 != 0), pick(op & 1 != 0)];
        self.model.put_run(&run4);
        if op & 0x10 == 0 {
            return Ok(());
        }
        loop {
            if self.off >= self.cur_len() {
                if self.cur_end() {
                    break;
                }
                if !self.fetch() {
                    return Err(Starved);
                }
                if self.restart {
                    break;
                }
            }
            let Some(b) = self.byte(self.off) else { break };
            self.off += 1;
            let run7 = [
                pick(b & 0x40 != 0),
                pick(b & 0x20 != 0),
                pick(b & 0x10 != 0),
                pick(b & 0x08 != 0),
                pick(b & 0x04 != 0),
                pick(b & 0x02 != 0),
                pick(b & 0x01 != 0),
            ];
            self.model.put_run(&run7);
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok(())
    }

    /// 1xxxxxxx: one pixel of an explicit colour.
    fn op_pixel(&mut self, op: u8) -> Result<(), Starved> {
        let c = match self.kind {
            Kind::Rgb15 => {
                if self.off >= self.cur_len() {
                    if !self.fetch() {
                        return Err(Starved);
                    }
                    if self.restart {
                        return Ok(());
                    }
                }
                let Some(lo) = self.byte(self.off) else { return Ok(()) };
                self.off += 1;
                rgb15(((op & 0x7F) as u32) << 8 | lo as u32)
            }
            Kind::Palette => BLACK | PALETTE[(op & 0x7F) as usize],
            Kind::Grey => {
                let g = ((op & 0x7F) as u32) << 1;
                BLACK | g << 16 | g << 8 | g
            }
        };
        self.model.put(c);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(w: u16, h: u16, begin: bool, end: bool, data: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; HEADER_LEN];
        p[4..6].copy_from_slice(&h.to_be_bytes());
        p[6..8].copy_from_slice(&w.to_be_bytes());
        p[8] = begin as u8 | (end as u8) << 1;
        p.extend_from_slice(data);
        p
    }

    #[test]
    fn header_fields() {
        let mut p = packet(1280, 1024, true, true, &[]);
        p[10] = 0x12;
        p[11] = 0x34;
        let h = parse_header(&p);
        assert_eq!((h.width, h.height, h.begin, h.end, h.checksum), (1280, 1024, true, true, 0x1234));
    }

    #[test]
    fn pixel_and_copy_left() {
        let mut d = DvcDecoder::new();
        let mut fb = Framebuffer::default();
        // red pixel (0x7C00), then copy-left 3, on an 8x2 screen
        let done = d.push_packet(0x81, &packet(8, 2, true, true, &[0xFC, 0x00, 0x23]), &mut fb).unwrap();
        assert!(done);
        assert_eq!((fb.width, fb.height), (8, 2));
        assert_eq!(&fb.pixels[..5], &[0xF80000, 0xF80000, 0xF80000, 0xF80000, 0]);
    }

    #[test]
    fn spans_packets() {
        // A 15 bit pixel split across two packets of the same frame.
        let mut d = DvcDecoder::new();
        let mut fb = Framebuffer::default();
        assert!(!d.push_packet(0x81, &packet(8, 2, true, false, &[0x80]), &mut fb).unwrap());
        assert!(d.push_packet(0x81, &packet(8, 2, false, true, &[0x1F]), &mut fb).unwrap());
        assert_eq!(fb.pixels[0], 0x0000F8);
    }

    #[test]
    fn copy_above_and_skip() {
        let mut d = DvcDecoder::new();
        let mut fb = Framebuffer::default();
        // white pixel, copy-left 3 (row 0 = 4 white), skip 0, copy-above 4
        let data = [0xFF, 0xFF, 0x23, 0x00, 0x44];
        d.push_packet(0x81, &packet(4, 2, true, true, &data), &mut fb).unwrap();
        assert!(fb.pixels.iter().take(7).all(|&p| p == 0xF8F8F8));
    }

    #[test]
    fn garbage_never_panics() {
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let mut d = DvcDecoder::new();
        let mut fb = Framebuffer::default();
        for i in 0..400 {
            let len = (next() % 600) as usize;
            let mut p: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if p.len() >= HEADER_LEN && i % 3 == 0 {
                p[4..6].copy_from_slice(&((next() % 64) as u16).to_be_bytes());
                p[6..8].copy_from_slice(&((next() % 64) as u16).to_be_bytes());
            }
            let _ = d.push_packet(0x81 + (next() % 3) as u8, &p, &mut fb);
        }
    }
}
