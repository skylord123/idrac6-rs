//! iDRAC6 Boot Capture recordings.
//!
//! The iDRAC6 records the console during the last three boots. The web UI
//! (`lastboot.html`) plays them with the Java viewer in "playback" mode. A
//! recording is a plain sequence of DVC video records, described in
//! `docs/video-dvc.md` ("Boot capture recordings"). This module parses one
//! and turns each record into a regular Video Packet payload for
//! [`super::dvc::DvcDecoder`], so playback reuses the live decoder.
//!
//! Port of `com.avocent.kvm.d.a.a` (reader, mode 3), `com.avocent.kvm.d.f`
//! (record) and `com.avocent.kvm.d.a.b` (timing) from the iDRAC6 viewer.

use std::time::Duration;

use anyhow::{Result, bail};

use super::Framebuffer;
use super::dvc::{self, DvcDecoder};

/// Length prefix + `DEADBEEF` header in front of every record's data.
pub const RECORD_HEADER_LEN: usize = 20;
/// The Java viewer never sleeps longer than this between two records; long
/// pauses in the recording are cut short.
pub const MAX_DELAY_MS: u64 = 1000;
/// The web UI's `maxFileLength`: the viewer refuses to read more than this.
pub const MAX_FILE_LEN: usize = 2 * 1024 * 1024;

/// The Video Packet type the records are decoded as. Playback always uses
/// the 15 bit colour decoder (`com.avocent.kvm.d.c`).
pub const MSG_TYPE: u8 = 0x81;

const MAGIC: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

/// One recorded video packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecFrame {
    /// When to show this record, in milliseconds from the start of playback,
    /// with gaps capped at [`MAX_DELAY_MS`] like the Java viewer does.
    pub at_ms: u64,
    /// Raw timestamp from the record (milliseconds since recording start).
    pub timestamp_ms: u32,
    pub width: u16,
    pub height: u16,
    /// First packet of a frame.
    pub begin: bool,
    /// Last packet of a frame: the screen is complete after this record.
    pub end: bool,
    /// Raw flags byte. Bit 2 is set on a few records (possibly full-screen
    /// refreshes); the viewer ignores it.
    pub flags: u8,
    /// Video Packet type for the decoder, always [`MSG_TYPE`].
    pub msg_type: u8,
    /// A complete Video Packet payload (12 byte DVC sub-header + data), as
    /// [`DvcDecoder::push_packet`] expects it.
    pub payload: Vec<u8>,
}

/// A parsed recording.
#[derive(Debug, Clone, Default)]
pub struct Recording {
    pub frames: Vec<RecFrame>,
    /// True when the data ended in the middle of a record (the rest was
    /// ignored, as the Java reader does).
    pub truncated: bool,
}

impl Recording {
    /// How long the viewer waits before showing record `i` (capped at
    /// [`MAX_DELAY_MS`]).
    #[allow(dead_code)]
    pub fn delay_before(&self, i: usize) -> Duration {
        match (i.checked_sub(1).and_then(|p| self.frames.get(p)), self.frames.get(i)) {
            (Some(a), Some(b)) => Duration::from_millis(b.at_ms - a.at_ms),
            _ => Duration::ZERO,
        }
    }

    /// Total playback time with the viewer's capped delays.
    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.frames.last().map_or(0, |f| f.at_ms))
    }

}

/// Parse a boot capture file.
///
/// Like the Java reader this stops at the first record with a length below
/// 20 or one that runs past the end of the data. A file that does not start
/// with a valid record is rejected.
pub fn parse(data: &[u8]) -> Result<Recording> {
    let data = &data[..data.len().min(MAX_FILE_LEN)];
    let mut rec = Recording::default();
    let mut off = 0usize;
    let mut last_ts: Option<u32> = None;
    let mut at_ms = 0u64;
    while off + 4 <= data.len() {
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        if len < RECORD_HEADER_LEN {
            break;
        }
        let Some(r) = data.get(off..off.saturating_add(len)) else {
            rec.truncated = true;
            break;
        };
        if r[4..8] != MAGIC {
            log::debug!("boot capture record at {off} has no DEADBEEF marker");
        }
        let ts = u32::from_be_bytes([r[8], r[9], r[10], r[11]]);
        let width = u16::from_be_bytes([r[12], r[13]]);
        let height = u16::from_be_bytes([r[14], r[15]]);
        let flags = r[16];
        // Java (com.avocent.kvm.d.a.b): sleep (ts - previous) if positive,
        // at most 1000 ms. The first record plays immediately.
        if let Some(prev) = last_ts {
            let d = (ts as i64 - prev as i64).clamp(0, MAX_DELAY_MS as i64);
            at_ms += d as u64;
        }
        last_ts = Some(ts);
        let begin = flags & 1 != 0;
        let end = flags & 2 != 0;
        let body = &r[RECORD_HEADER_LEN..];
        // Synthesize the live Video Packet sub-header the decoder expects:
        // height at 4..6, width at 6..8, begin/end flags at 8, no checksum.
        let mut payload = Vec::with_capacity(dvc::HEADER_LEN + body.len());
        payload.extend_from_slice(&[0; 4]);
        payload.extend_from_slice(&height.to_be_bytes());
        payload.extend_from_slice(&width.to_be_bytes());
        payload.push(flags & 3);
        payload.extend_from_slice(&[0; 3]);
        payload.extend_from_slice(body);
        rec.frames.push(RecFrame { at_ms, timestamp_ms: ts, width, height, begin, end, flags, msg_type: MSG_TYPE, payload });
        off += len;
    }
    if rec.frames.is_empty() {
        bail!("not a boot capture recording (no valid records)");
    }
    Ok(rec)
}

/// Steps through a recording, like the Java playback controls.
///
/// Timing is left to the caller: show record `next_index()` once
/// `recording.frames[i].at_ms` has elapsed since playback started (or use
/// [`Recording::delay_before`]).
pub struct Player {
    pub recording: Recording,
    decoder: DvcDecoder,
    next: usize,
    /// Completed frames shown so far (the viewer's "Frame:" counter).
    frames_shown: usize,
}

impl Player {
    pub fn new(recording: Recording) -> Self {
        Self { recording, decoder: DvcDecoder::new(), next: 0, frames_shown: 0 }
    }


    #[allow(dead_code)]
    pub fn next_index(&self) -> usize {
        self.next
    }

    pub fn frames_shown(&self) -> usize {
        self.frames_shown
    }

    pub fn finished(&self) -> bool {
        self.next >= self.recording.frames.len()
    }

    /// Playback position of the next record.
    pub fn next_at(&self) -> Option<Duration> {
        self.recording.frames.get(self.next).map(|f| Duration::from_millis(f.at_ms))
    }

    /// Decode the next record. Returns `Ok(true)` when a frame was completed
    /// and `fb` should be shown, `Ok(false)` at the end or mid-frame.
    pub fn advance(&mut self, fb: &mut Framebuffer) -> Result<bool> {
        let Some(f) = self.recording.frames.get(self.next) else { return Ok(false) };
        self.next += 1;
        let done = self.decoder.push_packet(f.msg_type, &f.payload, fb)?;
        if done {
            self.frames_shown += 1;
        }
        Ok(done)
    }

    /// "Step": decode records until one frame is complete (or the end).
    pub fn step(&mut self, fb: &mut Framebuffer) -> Result<bool> {
        while !self.finished() {
            if self.advance(fb)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Decode every record whose time is at or before `t` (catching up after
    /// a slow repaint). Returns true if anything new was completed.
    pub fn advance_to(&mut self, t: Duration, fb: &mut Framebuffer) -> Result<bool> {
        let mut any = false;
        while self.next_at().is_some_and(|at| at <= t) {
            any |= self.advance(fb)?;
        }
        Ok(any)
    }

    /// "Rewind": back to the first record with a black screen.
    pub fn rewind(&mut self, fb: &mut Framebuffer) {
        self.decoder = DvcDecoder::new();
        self.next = 0;
        self.frames_shown = 0;
        fb.pixels.iter_mut().for_each(|p| *p = 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ts: u32, w: u16, h: u16, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut v = ((RECORD_HEADER_LEN + data.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(&MAGIC);
        v.extend_from_slice(&ts.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[flags, 0, 0, 0]);
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn parses_records_and_timing() {
        let mut f = record(100, 8, 2, 3, &[0x80]);
        f.extend(record(150, 8, 2, 3, &[0x81]));
        f.extend(record(9000, 8, 2, 7, &[0x82]));
        f.extend([0, 0, 0, 0]); // padding stops the reader
        let r = parse(&f).unwrap();
        assert_eq!(r.frames.len(), 3);
        assert_eq!(r.frames.iter().map(|f| f.at_ms).collect::<Vec<_>>(), vec![0, 50, 1050]);
        assert!(!r.truncated);
        let p = &r.frames[0].payload;
        assert_eq!(dvc::parse_header(p), dvc::DvcHeader { height: 2, width: 8, begin: true, end: true, checksum: 0 });
        assert_eq!(&p[dvc::HEADER_LEN..], &[0x80]);
        assert_eq!(r.frames[2].flags, 7);
        assert_eq!(r.delay_before(2), Duration::from_millis(1000));
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let mut f = record(0, 8, 2, 3, &[0x80]);
        let mut second = record(10, 8, 2, 3, &[0x80, 0x80, 0x80]);
        second.truncate(22);
        f.extend(second);
        let r = parse(&f).unwrap();
        assert_eq!(r.frames.len(), 1);
        assert!(r.truncated);
    }

    #[test]
    fn rejects_non_recordings() {
        assert!(parse(b"<html>404</html>").is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn player_steps_and_rewinds() {
        let mut f = record(0, 8, 2, 3, &[0xFC, 0x00, 0x23]);
        f.extend(record(40, 8, 2, 3, &[0x80]));
        let mut p = Player::new(parse(&f).unwrap());
        let mut fb = Framebuffer::default();
        assert!(p.step(&mut fb).unwrap());
        assert_eq!(p.frames_shown(), 1);
        assert!(p.advance_to(Duration::from_millis(40), &mut fb).unwrap());
        assert!(p.finished());
        p.rewind(&mut fb);
        assert_eq!(p.next_index(), 0);
        assert_eq!(p.frames_shown(), 0);
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
        for _ in 0..300 {
            let n = (next() % 600) as usize;
            let mut v: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            if n >= 8 && next() % 2 == 0 {
                // A plausible length prefix makes the parser go further.
                let len = (20 + next() % 64).to_be_bytes();
                v[..4].copy_from_slice(&len);
            }
            if let Ok(r) = parse(&v) {
                let mut p = Player::new(r);
                let mut fb = Framebuffer::default();
                while !p.finished() {
                    let _ = p.advance(&mut fb);
                }
            }
        }
    }
}
