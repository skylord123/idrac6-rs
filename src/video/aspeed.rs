//! Decoder for the ASPEED AST2050 compressed video used by the iDRAC6
//! virtual console (packet type 134).
//!
//! The format is described in `docs/video-codec.md`.

// Complete protocol definitions are kept even where the client does not
// use them yet; they document the wire format.
#![allow(dead_code)]

use anyhow::{Result, bail};

use super::Framebuffer;

/// Length of the per-message sub-header in front of the compressed data.
pub const HEADER_LEN: usize = 12;

/// The 12 byte header at the start of every ASPEED video message payload.
///
/// Byte layout (big endian), as parsed by `com.avocent.kvm.b.a.eb`:
///
/// | bytes | field |
/// |-------|-------|
/// | 0..4  | unused by the client |
/// | 4..6  | frame height in pixels |
/// | 6..8  | frame width in pixels |
/// | 8     | flags: bit 0 = first message of a frame, bits 1 and 2 = unknown (parsed, never used) |
/// | 9     | unknown (parsed, never used) |
/// | 10    | unknown, low nibble parsed, never used |
/// | 11    | unused |
/// | 12..  | compressed data |
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AspeedFrameHeader {
    pub width: u16,
    pub height: u16,
    /// Raw flag byte (payload byte 8).
    pub flags: u8,
    /// Bit 0 of the flags: this message starts a new frame.
    pub frame_start: bool,
    /// Payload byte 9 (meaning unknown).
    pub byte9: u8,
    /// Payload byte 10 (meaning unknown).
    pub byte10: u8,
}

/// Parse the sub-header of an ASPEED video message payload (the bytes after
/// the 8 byte `BEEF` header). A payload shorter than [`HEADER_LEN`] gives an
/// all-zero header (which is not a frame start).
pub fn parse_header(payload: &[u8]) -> AspeedFrameHeader {
    if payload.len() < HEADER_LEN {
        return AspeedFrameHeader::default();
    }
    let be16 = |i: usize| u16::from_be_bytes([payload[i], payload[i + 1]]);
    AspeedFrameHeader {
        height: be16(4),
        width: be16(6),
        flags: payload[8],
        frame_start: payload[8] & 1 != 0,
        byte9: payload[9],
        byte10: payload[10],
    }
}

/// Whether the frame data collected so far already contains the end of the
/// frame. There is no length field in the header, so this scans the
/// bitstream (Huffman parsing only, no IDCT or colour conversion) looking
/// for the FRAME_END code. Use it to decode a frame without waiting for the
/// next frame-start message. It returns false for truncated or corrupt data.
pub fn frame_complete(hdr: &AspeedFrameHeader, data_so_far: &[u8]) -> bool {
    let t = tables();
    matches!(
        run_frame(t, None, hdr.width as usize, hdr.height as usize, data_so_far),
        Ok(FrameOutcome { reached_end: true, .. })
    )
}

/// A complete frame ready for [`AspeedDecoder::decode_frame`].
#[derive(Clone, Debug, Default)]
pub struct AssembledFrame {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// Joins video messages into frames the way the Java client does: data is
/// collected from a frame-start message plus all following messages, and
/// the frame is released when the next frame-start message arrives.
/// Messages received before the first frame start are dropped.
#[derive(Default)]
pub struct FrameAssembler {
    current: Option<AssembledFrame>,
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one message payload. Returns the previous frame when this
    /// message starts a new one.
    pub fn push(&mut self, payload: &[u8]) -> Option<AssembledFrame> {
        if payload.len() < HEADER_LEN {
            return None;
        }
        let hdr = parse_header(payload);
        let body = &payload[HEADER_LEN..];
        if hdr.frame_start {
            let next = AssembledFrame { width: hdr.width as usize, height: hdr.height as usize, data: body.to_vec() };
            self.current.replace(next)
        } else {
            if let Some(cur) = self.current.as_mut() {
                cur.data.extend_from_slice(body);
            }
            None
        }
    }

    /// Take the frame being collected, if any (for example when the
    /// session closes).
    pub fn take(&mut self) -> Option<AssembledFrame> {
        self.current.take()
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Block codes (top 4 bits of the bit buffer at a block boundary).
#[allow(dead_code)] // the 2 colour codes are only matched through ranges
mod code {
    pub const JPEG_NO_SKIP: u32 = 0;
    pub const LOW_JPEG_NO_SKIP: u32 = 4;
    pub const VQ_NO_SKIP_1_COLOR: u32 = 5;
    pub const VQ_NO_SKIP_2_COLOR: u32 = 6;
    pub const VQ_NO_SKIP_4_COLOR: u32 = 7;
    pub const JPEG_SKIP: u32 = 8;
    pub const FRAME_END: u32 = 9;
    pub const LOW_JPEG_SKIP: u32 = 12;
    pub const VQ_SKIP_1_COLOR: u32 = 13;
    pub const VQ_SKIP_2_COLOR: u32 = 14;
    pub const VQ_SKIP_4_COLOR: u32 = 15;
}

/// Bits taken by a block code.
const CODE_BITS: u32 = 4;
/// Bits taken by a block code plus an 8 bit x and 8 bit y position.
const SKIP_CODE_BITS: u32 = 20;
/// Upper bound on blocks per frame, so corrupt data cannot loop forever
/// (a 2048x2048 screen has 65536 blocks).
const MAX_BLOCKS_PER_FRAME: usize = 1 << 20;

/// Decoder for ASPEED video frames.
///
/// All decoding state is per frame (the Java client resets it for every
/// frame), so one decoder can be reused for the whole session.
pub struct AspeedDecoder {
    tables: &'static Tables,
}

impl Default for AspeedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AspeedDecoder {
    pub fn new() -> Self {
        Self { tables: tables() }
    }

    /// Decode one complete assembled frame.
    ///
    /// `width`/`height` come from the frame's first message header and
    /// `data` is the concatenated compressed data of all its messages (see
    /// [`FrameAssembler`]). `fb` is updated in place and resized when the
    /// resolution changes. Returns the bounding box `(x, y, w, h)` of the
    /// blocks that were drawn, or `None` if nothing changed.
    ///
    /// A frame that runs out of data before FRAME_END is not an error (the
    /// Java client does the same and keeps what was decoded). Unknown block
    /// codes and corrupt Huffman data return an error; blocks decoded before
    /// that point stay in `fb`.
    pub fn decode_frame(
        &mut self,
        width: usize,
        height: usize,
        data: &[u8],
        fb: &mut Framebuffer,
    ) -> Result<Option<(usize, usize, usize, usize)>> {
        if width < 8 || height < 8 || width > 4096 || height > 4096 {
            bail!("unsupported video resolution {width}x{height}");
        }
        fb.resize(width, height);
        let out = run_frame(self.tables, Some(fb), width, height, data)?;
        Ok(out.dirty.map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0)))
    }
}

struct FrameOutcome {
    reached_end: bool,
    /// (x0, y0, x1, y1), exclusive end, clipped to the frame.
    dirty: Option<(usize, usize, usize, usize)>,
}

/// Bit reader over little-endian 32-bit words, MSB first. Mirrors the
/// `codebuf`/`newbuf`/`newbits` scheme of the reference decoder so that
/// `codebuf` always holds the next 32 bits of the stream. Reads past the
/// end return zero bits.
struct BitReader<'a> {
    data: &'a [u8],
    words: usize,
    /// Index of the next word to load.
    next: usize,
    codebuf: u32,
    newbuf: u32,
    newbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut r = BitReader { data, words: data.len() / 4, next: 0, codebuf: 0, newbuf: 0, newbits: 32 };
        r.codebuf = r.load();
        r.newbuf = r.load();
        r
    }

    fn load(&mut self) -> u32 {
        let i = self.next;
        self.next += 1;
        if i < self.words {
            let b = &self.data[i * 4..i * 4 + 4];
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            0
        }
    }

    /// The next `n` bits (1..=16) without consuming them.
    #[inline]
    fn peek(&self, n: u32) -> u32 {
        (self.codebuf >> (32 - n)) & 0xFFFF
    }

    /// Consume `n` bits (1..=31).
    #[inline]
    fn skip(&mut self, n: u32) {
        if self.newbits <= n {
            let rb = self.load();
            self.codebuf = (self.codebuf << n) | ((self.newbuf | rb.wrapping_shr(self.newbits)) >> (32 - n));
            self.newbuf = rb.wrapping_shl(n - self.newbits);
            self.newbits = 32 + self.newbits - n;
        } else {
            self.codebuf = (self.codebuf << n) | (self.newbuf >> (32 - n));
            self.newbuf <<= n;
            self.newbits -= n;
        }
    }

    /// Read an `n` bit (1..=15) JPEG magnitude value ("getKbits").
    #[inline]
    fn signed(&mut self, n: u32) -> i16 {
        let mut v = self.peek(n) as u16;
        if v & (1 << (n - 1)) == 0 {
            v = v.wrapping_sub(((1u32 << n) - 1) as u16);
        }
        self.skip(n);
        v as i16
    }

    /// Words consumed so far, as the reference decoder counts them (two
    /// words are preloaded).
    fn words_used(&self) -> usize {
        self.next
    }
}

/// Canonical Huffman table in the reference decoder's representation.
struct Huffman {
    /// Smallest code of each length (1..=16); 0xFFFF when the length is unused.
    min_code: [u32; 17],
    /// Largest code of each length; 0 when unused.
    max_code: [u32; 17],
    /// Index into `values` of the first code of each length.
    offset: [usize; 17],
    values: Vec<u8>,
}

impl Huffman {
    fn new(bits: &[u8; 17], values: &[u8]) -> Self {
        let mut t = Huffman { min_code: [0; 17], max_code: [0; 17], offset: [0; 17], values: values.to_vec() };
        let mut code = 0u32;
        let mut idx = 0usize;
        for len in 1..=16 {
            let count = bits[len] as u32;
            t.offset[len] = idx;
            idx += count as usize;
            t.min_code[len] = code;
            code += count;
            t.max_code[len] = code.wrapping_sub(1) & 0xFFFF;
            code *= 2;
            if count == 0 {
                t.min_code[len] = 0xFFFF;
                t.max_code[len] = 0;
            }
        }
        t
    }

    /// Decode one symbol. `None` when no code length matches (the reference
    /// decoder then consumes nothing).
    #[inline]
    fn decode(&self, br: &mut BitReader) -> Option<u8> {
        for len in 1..=16u32 {
            let c = br.peek(len);
            let l = len as usize;
            if c <= self.max_code[l] && c >= self.min_code[l] {
                br.skip(len);
                let v = self.values.get(self.offset[l] + (c - self.min_code[l]) as usize).copied().unwrap_or(0);
                return Some(v);
            }
        }
        None
    }
}

/// Lookup tables, built once.
struct Tables {
    dc_luma: Huffman,
    dc_chroma: Huffman,
    ac_luma: Huffman,
    ac_chroma: Huffman,
    /// Dequantisation tables with the AAN scale factors folded in:
    /// 0 = luma, 1 = chroma, 2 = low-quality luma, 3 = low-quality chroma.
    quant: [[f32; 64]; 4],
    /// Range limit table ("rlimit_table").
    range: [u8; 1408],
    cr_to_r: [i32; 256],
    cb_to_b: [i32; 256],
    cr_to_g: [i32; 256],
    cb_to_g: [i32; 256],
    y: [i32; 256],
}

fn tables() -> &'static Tables {
    static T: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    T.get_or_init(Tables::build)
}

// Quant table selection hard-coded by the Java client (`com.avocent.kvm.a.a.h`).
const LUMA_TABLE: usize = 4;
const CHROMA_TABLE: usize = 7;
const LOW_TABLE: usize = 0;
const QUANT_SCALE: u32 = 16;

impl Tables {
    fn build() -> Tables {
        const AAN: [f32; 8] = [1.0, 1.3870399, 1.306563, 1.1758755, 1.0, 0.78569496, 0.5411961, 0.27589938];
        let quant_table = |src: &[u8; 64]| {
            let mut q = [0f32; 64];
            for (n, out) in q.iter_mut().enumerate() {
                // The zigzag in set_quant_table/load_quant_table cancels out.
                let v = (src[n] as u32 * 16 / QUANT_SCALE).clamp(1, 255);
                *out = v as f32 * (AAN[n / 8] * AAN[n % 8]);
            }
            q
        };
        let mut range = [0u8; 1408];
        for i in 0..256 {
            range[256 + i] = i as u8;
        }
        range[512..896].fill(255);
        for i in 0..128 {
            range[1280 + i] = i as u8;
        }
        let fix = |d: f64| (d * 65536.0 + 0.5) as i32;
        let (mut cr_to_r, mut cb_to_b, mut cr_to_g, mut cb_to_g, mut y) = ([0; 256], [0; 256], [0; 256], [0; 256], [0; 256]);
        for i in 0..256 {
            let c = i as i32 - 128;
            cr_to_r[i] = (fix(1.597656) * c + 32768) >> 16;
            cb_to_b[i] = (fix(2.015625) * c + 32768) >> 16;
            cr_to_g[i] = -fix(0.8125) * c;
            cb_to_g[i] = -fix(0.390625) * c + 32768;
            y[i] = (fix(1.164) * (i as i32 - 16) + 32768) >> 16;
        }
        Tables {
            dc_luma: Huffman::new(&DC_LUM_BITS, &DC_LUM_VALS),
            dc_chroma: Huffman::new(&DC_CHR_BITS, &DC_CHR_VALS),
            ac_luma: Huffman::new(&AC_LUM_BITS, &AC_LUM_VALS),
            ac_chroma: Huffman::new(&AC_CHR_BITS, &AC_CHR_VALS),
            quant: [
                quant_table(&QUANT_LUMA[LUMA_TABLE]),
                quant_table(&QUANT_CHROMA[CHROMA_TABLE]),
                quant_table(&QUANT_LUMA[LOW_TABLE]),
                quant_table(&QUANT_CHROMA[LOW_TABLE]),
            ],
            range,
            cr_to_r,
            cb_to_b,
            cr_to_g,
            cb_to_g,
            y,
        }
    }

    #[inline]
    fn limit(&self, idx: i32) -> u32 {
        if idx < 0 { 0 } else { *self.range.get(idx as usize).unwrap_or(&255) as u32 }
    }

    #[inline]
    fn rgb(&self, y: u8, cb: u8, cr: u8) -> u32 {
        let yy = 256 + self.y[y as usize];
        let b = self.limit(yy + self.cb_to_b[cb as usize]);
        let g = self.limit(yy + ((self.cb_to_g[cb as usize] + self.cr_to_g[cr as usize]) >> 16));
        let r = self.limit(yy + self.cr_to_r[cr as usize]);
        (r << 16) | (g << 8) | b
    }
}

/// Per-frame decoding state.
struct FrameState {
    /// DC predictors for Y, Cb, Cr.
    dc: [i16; 3],
    /// VQ colour cache (24 bit Y/Cb/Cr).
    vq_colors: [u32; 4],
    /// Cache slots used by the current VQ block's colours.
    vq_index: [usize; 4],
}

impl FrameState {
    fn new() -> Self {
        FrameState { dc: [0; 3], vq_colors: [0x008080, 0xFF8080, 0x808080, 0xC08080], vq_index: [0, 1, 2, 3] }
    }
}

/// Decode (or with `fb == None`, just parse) one frame.
fn run_frame(
    t: &Tables,
    mut fb: Option<&mut Framebuffer>,
    width: usize,
    height: usize,
    data: &[u8],
) -> Result<FrameOutcome> {
    let blocks_x = width / 8;
    let blocks_y = height / 8;
    if blocks_x == 0 || blocks_y == 0 {
        bail!("unsupported video resolution {width}x{height}");
    }
    let words = data.len() / 4;
    let mut br = BitReader::new(data);
    let mut st = FrameState::new();
    let (mut bx, mut by) = (0usize, 0usize);
    let mut dirty: Option<(usize, usize, usize, usize)> = None;
    let mut ycc = [[0u8; 64]; 3];

    for _ in 0..MAX_BLOCKS_PER_FRAME {
        let cb = br.codebuf;
        let kind = cb >> 28;
        let skip_pos = |bx: &mut usize, by: &mut usize, br: &mut BitReader| {
            *bx = ((cb >> 20) & 0xFF) as usize;
            *by = ((cb >> 12) & 0xFF) as usize;
            br.skip(SKIP_CODE_BITS);
        };
        let drawn = match kind {
            code::FRAME_END => {
                return Ok(FrameOutcome { reached_end: true, dirty });
            }
            code::JPEG_NO_SKIP | code::LOW_JPEG_NO_SKIP => {
                br.skip(CODE_BITS);
                jpeg_block(t, &mut br, &mut st, if kind == code::LOW_JPEG_NO_SKIP { 2 } else { 0 }, &mut ycc)?;
                true
            }
            code::JPEG_SKIP | code::LOW_JPEG_SKIP => {
                skip_pos(&mut bx, &mut by, &mut br);
                jpeg_block(t, &mut br, &mut st, if kind == code::LOW_JPEG_SKIP { 2 } else { 0 }, &mut ycc)?;
                true
            }
            code::VQ_NO_SKIP_1_COLOR..=code::VQ_NO_SKIP_4_COLOR => {
                br.skip(CODE_BITS);
                vq_block(&mut br, &mut st, kind - code::VQ_NO_SKIP_1_COLOR, &mut ycc);
                true
            }
            code::VQ_SKIP_1_COLOR..=code::VQ_SKIP_4_COLOR => {
                skip_pos(&mut bx, &mut by, &mut br);
                vq_block(&mut br, &mut st, kind - code::VQ_SKIP_1_COLOR, &mut ycc);
                true
            }
            other => bail!("unknown ASPEED block code {other} at block ({bx}, {by})"),
        };

        if drawn {
            let (x0, y0) = (bx * 8, by * 8);
            if x0 < width && y0 < height {
                let (x1, y1) = ((x0 + 8).min(width), (y0 + 8).min(height));
                if let Some(fb) = fb.as_deref_mut() {
                    for row in 0..(y1 - y0) {
                        let line = &mut fb.pixels[(y0 + row) * width + x0..(y0 + row) * width + x1];
                        for (col, px) in line.iter_mut().enumerate() {
                            let i = row * 8 + col;
                            *px = t.rgb(ycc[0][i], ycc[1][i], ycc[2][i]);
                        }
                    }
                }
                dirty = Some(match dirty {
                    None => (x0, y0, x1, y1),
                    Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
                });
            }
        }

        // Advance to the next block position.
        bx += 1;
        if bx >= blocks_x {
            bx = 0;
            by += 1;
            if by >= blocks_y {
                by = 0;
            }
        }

        // Out of data: the reference decoder stops once it has loaded more
        // words than the frame holds.
        if br.words_used() > words {
            return Ok(FrameOutcome { reached_end: false, dirty });
        }
    }
    bail!("ASPEED frame has too many blocks")
}

/// Decode a VQ block. `kind` is 0, 1 or 2 for 1, 2 or 4 colours; this is
/// also the number of selector bits per pixel.
fn vq_block(br: &mut BitReader, st: &mut FrameState, kind: u32, ycc: &mut [[u8; 64]; 3]) {
    let colors = 1usize << kind;
    for n in 0..colors {
        let cb = br.codebuf;
        let slot = ((cb >> 29) & 3) as usize;
        st.vq_index[n] = slot;
        if (cb >> 31) & 1 == 0 {
            // Reuse the cached colour: 1 bit flag + 2 bit slot.
            br.skip(3);
        } else {
            // New colour: flag, slot, 24 bit YCbCr.
            st.vq_colors[slot] = (cb >> 5) & 0xFF_FFFF;
            br.skip(27);
        }
    }
    for i in 0..64 {
        let sel = if kind == 0 {
            0
        } else {
            let s = br.peek(kind) as usize;
            br.skip(kind);
            s
        };
        let c = st.vq_colors[st.vq_index[sel & 3]];
        ycc[0][i] = (c >> 16) as u8;
        ycc[1][i] = (c >> 8) as u8;
        ycc[2][i] = c as u8;
    }
}

/// Decode a JPEG block (Y, Cb, Cr data units). `q` selects the quant
/// tables: 0 = normal, 2 = low quality.
fn jpeg_block(t: &Tables, br: &mut BitReader, st: &mut FrameState, q: usize, ycc: &mut [[u8; 64]; 3]) -> Result<()> {
    let coef = data_unit(br, &t.dc_luma, &t.ac_luma, &mut st.dc[0])?;
    idct(&coef, &t.quant[q], &t.range, &mut ycc[0]);
    let coef = data_unit(br, &t.dc_chroma, &t.ac_chroma, &mut st.dc[1])?;
    idct(&coef, &t.quant[q + 1], &t.range, &mut ycc[1]);
    let coef = data_unit(br, &t.dc_chroma, &t.ac_chroma, &mut st.dc[2])?;
    idct(&coef, &t.quant[q + 1], &t.range, &mut ycc[2]);
    Ok(())
}

/// Huffman-decode one 8x8 data unit. Returns coefficients in natural
/// (row-major) order.
fn data_unit(br: &mut BitReader, dc: &Huffman, ac: &Huffman, pred: &mut i16) -> Result<[i16; 64]> {
    let mut zz = [0i16; 64];
    if let Some(size) = dc.decode(br) {
        if size == 0 {
            zz[0] = *pred;
        } else {
            if size > 15 {
                bail!("bad DC size {size}");
            }
            zz[0] = pred.wrapping_add(br.signed(size as u32));
            *pred = zz[0];
        }
    }
    let mut k = 1usize;
    // Each iteration either consumes bits or advances k, but bound it anyway.
    let mut guard = 0;
    while k <= 63 {
        guard += 1;
        if guard > 1024 {
            bail!("corrupt AC data");
        }
        match ac.decode(br) {
            Some(sym) => {
                let size = (sym & 0xF) as u32;
                let run = (sym >> 4) as usize;
                if size == 0 {
                    if run == 0 {
                        break; // end of block
                    }
                    if run == 15 {
                        k += 16; // 16 zeros
                    }
                } else {
                    k += run;
                    if k > 63 {
                        bail!("AC coefficient index out of range");
                    }
                    zz[k] = br.signed(size);
                    k += 1;
                }
            }
            None => k += 1,
        }
    }
    let mut out = [0i16; 64];
    for (i, o) in out.iter_mut().enumerate() {
        *o = zz[ZIGZAG[i] as usize];
    }
    Ok(out)
}

#[inline]
fn mul8(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b) >> 8
}

/// Dequantise and inverse-DCT one block (AAN algorithm, as in the
/// reference decoder: float dequantisation, integer butterflies).
fn idct(coef: &[i16; 64], q: &[f32; 64], range: &[u8; 1408], out: &mut [u8; 64]) {
    let mut ws = [0i32; 64];
    let dq = |i: usize| (coef[i] as f32 * q[i]) as i32;
    for c in 0..8 {
        if (1..8).all(|r| coef[c + 8 * r] == 0) {
            let v = dq(c);
            for r in 0..8 {
                ws[c + 8 * r] = v;
            }
            continue;
        }
        let (t0, t1, t2, t3) = (dq(c), dq(c + 16), dq(c + 32), dq(c + 48));
        let t10 = t0.wrapping_add(t2);
        let t11 = t0.wrapping_sub(t2);
        let t13 = t1.wrapping_add(t3);
        let t12 = mul8(t1.wrapping_sub(t3), 362).wrapping_sub(t13);
        let (e0, e3, e1, e2) = (t10.wrapping_add(t13), t10.wrapping_sub(t13), t11.wrapping_add(t12), t11.wrapping_sub(t12));
        let (t4, t5, t6, t7) = (dq(c + 8), dq(c + 24), dq(c + 40), dq(c + 56));
        let z13 = t6.wrapping_add(t5);
        let z10 = t6.wrapping_sub(t5);
        let z11 = t4.wrapping_add(t7);
        let z12 = t4.wrapping_sub(t7);
        let o7 = z11.wrapping_add(z13);
        let o11 = mul8(z11.wrapping_sub(z13), 362);
        let z5 = mul8(z10.wrapping_add(z12), 473);
        let o10 = mul8(z12, 277).wrapping_sub(z5);
        let o12 = mul8(z10, -669).wrapping_add(z5);
        let o6 = o12.wrapping_sub(o7);
        let o5 = o11.wrapping_sub(o6);
        let o4 = o10.wrapping_add(o5);
        ws[c] = e0.wrapping_add(o7);
        ws[c + 56] = e0.wrapping_sub(o7);
        ws[c + 8] = e1.wrapping_add(o6);
        ws[c + 48] = e1.wrapping_sub(o6);
        ws[c + 16] = e2.wrapping_add(o5);
        ws[c + 40] = e2.wrapping_sub(o5);
        ws[c + 32] = e3.wrapping_add(o4);
        ws[c + 24] = e3.wrapping_sub(o4);
    }
    let lim = |v: i32| range[((384i32.wrapping_add(v >> 3)) & 0x3FF) as usize];
    for r in 0..8 {
        let w = &ws[r * 8..r * 8 + 8];
        let t10 = w[0].wrapping_add(w[4]);
        let t11 = w[0].wrapping_sub(w[4]);
        let t13 = w[2].wrapping_add(w[6]);
        let t12 = mul8(w[2].wrapping_sub(w[6]), 362).wrapping_sub(t13);
        let (e0, e3, e1, e2) = (t10.wrapping_add(t13), t10.wrapping_sub(t13), t11.wrapping_add(t12), t11.wrapping_sub(t12));
        let z13 = w[5].wrapping_add(w[3]);
        let z10 = w[5].wrapping_sub(w[3]);
        let z11 = w[1].wrapping_add(w[7]);
        let z12 = w[1].wrapping_sub(w[7]);
        let o7 = z11.wrapping_add(z13);
        let o11 = mul8(z11.wrapping_sub(z13), 362);
        let z5 = mul8(z10.wrapping_add(z12), 473);
        let o10 = mul8(z12, 277).wrapping_sub(z5);
        let o12 = mul8(z10, -669).wrapping_add(z5);
        let o6 = o12.wrapping_sub(o7);
        let o5 = o11.wrapping_sub(o6);
        let o4 = o10.wrapping_add(o5);
        let o = &mut out[r * 8..r * 8 + 8];
        o[0] = lim(e0.wrapping_add(o7));
        o[7] = lim(e0.wrapping_sub(o7));
        o[1] = lim(e1.wrapping_add(o6));
        o[6] = lim(e1.wrapping_sub(o6));
        o[2] = lim(e2.wrapping_add(o5));
        o[5] = lim(e2.wrapping_sub(o5));
        o[4] = lim(e3.wrapping_add(o4));
        o[3] = lim(e3.wrapping_sub(o4));
    }
}

// ---------------------------------------------------------------------------
// Tables (from com.avocent.kvm.a.a.c)
// ---------------------------------------------------------------------------

const ZIGZAG: [u8; 64] = [
    0, 1, 5, 6, 14, 15, 27, 28, 2, 4, 7, 13, 16, 26, 29, 42,
    3, 8, 12, 17, 25, 30, 41, 43, 9, 11, 18, 24, 31, 40, 44, 53,
    10, 19, 23, 32, 39, 45, 52, 54, 20, 22, 33, 38, 46, 51, 55, 60,
    21, 34, 37, 47, 50, 56, 59, 61, 35, 36, 48, 49, 57, 58, 62, 63,
];

const DC_LUM_BITS: [u8; 17] = [
    0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0,
    0,
];

const DC_LUM_VALS: [u8; 12] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11,
];

const DC_CHR_BITS: [u8; 17] = [
    0, 0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0,
    0,
];

const DC_CHR_VALS: [u8; 12] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11,
];

const AC_LUM_BITS: [u8; 17] = [
    0, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1,
    125,
];

const AC_LUM_VALS: [u8; 162] = [
    1, 2, 3, 0, 4, 17, 5, 18, 33, 49, 65, 6, 19, 81, 97, 7,
    34, 113, 20, 50, 129, 145, 161, 8, 35, 66, 177, 193, 21, 82, 209, 240,
    36, 51, 98, 114, 130, 9, 10, 22, 23, 24, 25, 26, 37, 38, 39, 40,
    41, 42, 52, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72, 73,
    74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105,
    106, 115, 116, 117, 118, 119, 120, 121, 122, 131, 132, 133, 134, 135, 136, 137,
    138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166, 167,
    168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197,
    198, 199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218, 225, 226,
    227, 228, 229, 230, 231, 232, 233, 234, 241, 242, 243, 244, 245, 246, 247, 248,
    249, 250,
];

const AC_CHR_BITS: [u8; 17] = [
    0, 0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2,
    119,
];

const AC_CHR_VALS: [u8; 162] = [
    0, 1, 2, 3, 17, 4, 5, 33, 49, 6, 18, 65, 81, 7, 97, 113,
    19, 34, 50, 129, 8, 20, 66, 145, 161, 177, 193, 9, 35, 51, 82, 240,
    21, 98, 114, 209, 10, 22, 36, 52, 225, 37, 241, 23, 24, 25, 26, 38,
    39, 40, 41, 42, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72,
    73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104,
    105, 106, 115, 116, 117, 118, 119, 120, 121, 122, 130, 131, 132, 133, 134, 135,
    136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165,
    166, 167, 168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195,
    196, 197, 198, 199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218,
    226, 227, 228, 229, 230, 231, 232, 233, 234, 242, 243, 244, 245, 246, 247, 248,
    249, 250,
];

const QUANT_LUMA: [[u8; 64]; 8] = [
    [
        20, 13, 12, 20, 30, 50, 63, 76, 15, 15, 17, 23, 32, 72, 75, 68,
        17, 16, 20, 30, 50, 71, 86, 70, 17, 21, 27, 36, 63, 108, 100, 77,
        22, 27, 46, 70, 85, 136, 128, 96, 30, 43, 68, 80, 101, 130, 141, 115,
        61, 80, 97, 108, 128, 151, 150, 126, 90, 115, 118, 122, 140, 125, 128, 123,
    ],
    [
        17, 12, 10, 17, 26, 43, 55, 66, 13, 13, 15, 20, 28, 63, 65, 60,
        15, 14, 17, 26, 43, 62, 75, 61, 15, 18, 24, 31, 55, 95, 87, 67,
        19, 24, 40, 61, 74, 119, 112, 84, 26, 38, 60, 70, 88, 113, 123, 100,
        53, 70, 85, 95, 112, 132, 131, 110, 78, 100, 103, 107, 122, 109, 112, 108,
    ],
    [
        14, 9, 9, 14, 21, 36, 46, 55, 10, 10, 12, 17, 23, 52, 54, 49,
        12, 11, 14, 21, 36, 51, 62, 50, 12, 15, 19, 26, 46, 78, 72, 56,
        16, 19, 33, 50, 61, 98, 93, 69, 21, 31, 49, 58, 73, 94, 102, 83,
        44, 58, 70, 78, 93, 109, 108, 91, 65, 83, 86, 88, 101, 90, 93, 89,
    ],
    [
        11, 7, 7, 11, 17, 28, 36, 43, 8, 8, 10, 13, 18, 41, 43, 39,
        10, 9, 11, 17, 28, 40, 49, 40, 10, 12, 15, 20, 36, 62, 57, 44,
        12, 15, 26, 40, 48, 78, 74, 55, 17, 25, 39, 46, 58, 74, 81, 66,
        35, 46, 56, 62, 74, 86, 86, 72, 51, 66, 68, 70, 80, 71, 74, 71,
    ],
    [
        9, 6, 5, 9, 13, 22, 28, 34, 6, 6, 7, 10, 14, 32, 33, 30,
        7, 7, 9, 13, 22, 32, 38, 31, 7, 9, 12, 16, 28, 48, 45, 34,
        10, 12, 20, 31, 38, 61, 57, 43, 13, 19, 30, 36, 45, 58, 63, 51,
        27, 36, 43, 48, 57, 68, 67, 56, 40, 51, 53, 55, 63, 56, 57, 55,
    ],
    [
        6, 4, 3, 6, 9, 15, 19, 22, 4, 4, 5, 7, 9, 21, 22, 20,
        5, 4, 6, 9, 15, 21, 25, 21, 5, 6, 8, 10, 19, 32, 30, 23,
        6, 8, 13, 21, 25, 40, 38, 28, 9, 13, 20, 24, 30, 39, 42, 34,
        18, 24, 29, 32, 38, 45, 45, 37, 27, 34, 35, 36, 42, 37, 38, 37,
    ],
    [
        3, 2, 1, 3, 4, 7, 9, 11, 2, 2, 2, 3, 4, 10, 11, 10,
        2, 2, 3, 4, 7, 10, 12, 10, 2, 3, 4, 5, 9, 16, 15, 11,
        3, 4, 6, 10, 12, 20, 19, 14, 4, 6, 10, 12, 15, 19, 21, 17,
        9, 12, 14, 16, 19, 22, 22, 18, 13, 17, 17, 18, 21, 18, 19, 18,
    ],
    [
        2, 1, 1, 2, 3, 5, 6, 7, 1, 1, 1, 2, 3, 7, 7, 6,
        1, 1, 2, 3, 5, 7, 8, 7, 1, 2, 2, 3, 6, 10, 10, 7,
        2, 2, 4, 7, 8, 13, 12, 9, 3, 4, 6, 8, 10, 13, 14, 11,
        6, 8, 9, 10, 12, 15, 15, 12, 9, 11, 11, 12, 14, 12, 12, 12,
    ],
];

const QUANT_CHROMA: [[u8; 64]; 8] = [
    [
        31, 33, 45, 88, 185, 185, 185, 185, 33, 39, 48, 123, 185, 185, 185, 185,
        45, 48, 105, 185, 185, 185, 185, 185, 88, 123, 185, 185, 185, 185, 185, 185,
        185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185,
        185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185, 185,
    ],
    [
        27, 29, 39, 76, 160, 160, 160, 160, 29, 34, 42, 107, 160, 160, 160, 160,
        39, 42, 91, 160, 160, 160, 160, 160, 76, 107, 160, 160, 160, 160, 160, 160,
        160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160,
        160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160, 160,
    ],
    [
        22, 24, 32, 63, 133, 133, 133, 133, 24, 28, 34, 88, 133, 133, 133, 133,
        32, 34, 75, 133, 133, 133, 133, 133, 63, 88, 133, 133, 133, 133, 133, 133,
        133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133,
        133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133, 133,
    ],
    [
        18, 19, 26, 51, 108, 108, 108, 108, 19, 22, 28, 72, 108, 108, 108, 108,
        26, 28, 61, 108, 108, 108, 108, 108, 51, 72, 108, 108, 108, 108, 108, 108,
        108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108,
        108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108, 108,
    ],
    [
        13, 14, 19, 38, 80, 80, 80, 80, 14, 17, 21, 53, 80, 80, 80, 80,
        19, 21, 45, 80, 80, 80, 80, 80, 38, 53, 80, 80, 80, 80, 80, 80,
        80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80,
        80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80, 80,
    ],
    [
        9, 10, 13, 26, 55, 55, 55, 55, 10, 11, 14, 37, 55, 55, 55, 55,
        13, 14, 31, 55, 55, 55, 55, 55, 26, 37, 55, 55, 55, 55, 55, 55,
        55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55,
        55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55, 55,
    ],
    [
        4, 5, 6, 13, 27, 27, 27, 27, 5, 5, 7, 18, 27, 27, 27, 27,
        6, 7, 15, 27, 27, 27, 27, 27, 13, 18, 27, 27, 27, 27, 27, 27,
        27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27,
        27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27,
    ],
    [
        3, 3, 4, 8, 18, 18, 18, 18, 3, 3, 4, 12, 18, 18, 18, 18,
        4, 4, 10, 18, 18, 18, 18, 18, 8, 12, 18, 18, 18, 18, 18, 18,
        18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
        18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
    ],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Packs bits MSB first into little-endian 32-bit words, like the encoder.
    #[derive(Default)]
    pub(super) struct BitWriter {
        words: Vec<u32>,
        cur: u64,
        n: u32,
    }

    impl BitWriter {
        pub(super) fn put(&mut self, value: u32, bits: u32) {
            for i in (0..bits).rev() {
                self.cur = (self.cur << 1) | ((value >> i) & 1) as u64;
                self.n += 1;
                if self.n == 32 {
                    self.words.push(self.cur as u32);
                    self.cur = 0;
                    self.n = 0;
                }
            }
        }
        pub(super) fn finish(mut self) -> Vec<u8> {
            if self.n > 0 {
                self.words.push((self.cur << (32 - self.n)) as u32);
            }
            // Padding so the decoder's look-ahead stays inside the data.
            self.words.extend([0, 0]);
            self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
        }
    }

    #[test]
    fn bit_reader_reads_msb_first_across_words() {
        let mut w = BitWriter::default();
        w.put(0b1011, 4);
        w.put(0x1234_5, 20);
        w.put(0x7FF_FFFF, 27);
        w.put(0b1, 1);
        w.put(0xABCD, 16);
        let data = w.finish();
        let mut br = BitReader::new(&data);
        assert_eq!(br.peek(4), 0b1011);
        br.skip(4);
        assert_eq!(br.codebuf >> 12, 0x12345);
        br.skip(20);
        assert_eq!(br.codebuf >> 5, 0x7FF_FFFF);
        br.skip(27);
        assert_eq!(br.peek(1), 1);
        br.skip(1);
        assert_eq!(br.peek(16), 0xABCD);
    }

    #[test]
    fn signed_values_follow_jpeg_rules() {
        let mut w = BitWriter::default();
        w.put(0b101, 3); // +5
        w.put(0b010, 3); // -5
        w.put(0b0, 1); // -1
        let data = w.finish();
        let mut br = BitReader::new(&data);
        assert_eq!(br.signed(3), 5);
        assert_eq!(br.signed(3), -5);
        assert_eq!(br.signed(1), -1);
    }

    #[test]
    fn huffman_decodes_standard_dc_codes() {
        let t = tables();
        // Standard luminance DC: size 0 = "00", size 1 = "010", size 5 = "110".
        let mut w = BitWriter::default();
        w.put(0b00, 2);
        w.put(0b010, 3);
        w.put(0b110, 3);
        let data = w.finish();
        let mut br = BitReader::new(&data);
        assert_eq!(t.dc_luma.decode(&mut br), Some(0));
        assert_eq!(t.dc_luma.decode(&mut br), Some(1));
        assert_eq!(t.dc_luma.decode(&mut br), Some(5));
    }

    fn header(width: u16, height: u16, start: bool) -> Vec<u8> {
        let mut p = vec![0u8; HEADER_LEN];
        p[4..6].copy_from_slice(&height.to_be_bytes());
        p[6..8].copy_from_slice(&width.to_be_bytes());
        p[8] = start as u8;
        p
    }

    #[test]
    fn vq_frame_draws_solid_blocks() {
        // 16x8 screen: block 0 = new white colour in slot 1, block 1 = reuse slot 1.
        let mut w = BitWriter::default();
        w.put(code::VQ_NO_SKIP_1_COLOR, 4);
        w.put(1, 1);
        w.put(1, 2);
        w.put(0xFF8080, 24);
        w.put(code::VQ_NO_SKIP_1_COLOR, 4);
        w.put(0, 1);
        w.put(1, 2);
        w.put(code::FRAME_END, 4);
        let data = w.finish();
        let mut fb = Framebuffer::default();
        let mut d = AspeedDecoder::new();
        let r = d.decode_frame(16, 8, &data, &mut fb).unwrap();
        assert_eq!(r, Some((0, 0, 16, 8)));
        assert!(fb.pixels.iter().all(|&p| p == 0xFFFFFF), "{:x}", fb.pixels[0]);
        assert!(frame_complete(&parse_header(&header(16, 8, true)), &data));
    }

    #[test]
    fn skip_code_positions_block() {
        let mut w = BitWriter::default();
        w.put(code::VQ_SKIP_1_COLOR, 4);
        w.put(1, 8); // x
        w.put(2, 8); // y
        w.put(1, 1);
        w.put(0, 2);
        w.put(0xFF8080, 24);
        w.put(code::FRAME_END, 4);
        let data = w.finish();
        let mut fb = Framebuffer::default();
        let r = AspeedDecoder::new().decode_frame(32, 32, &data, &mut fb).unwrap();
        assert_eq!(r, Some((8, 16, 8, 8)));
        assert_eq!(fb.pixels[16 * 32 + 8], 0xFFFFFF);
        assert_eq!(fb.pixels[0], 0);
    }

    #[test]
    fn jpeg_flat_block_is_grey() {
        // DC size 0 everywhere and EOB: every component is 128 -> mid grey.
        let mut w = BitWriter::default();
        w.put(code::JPEG_NO_SKIP, 4);
        w.put(0b00, 2); // Y DC size 0
        w.put(0b1010, 4); // luma AC EOB
        w.put(0b00, 2); // Cb DC size 0
        w.put(0b00, 2); // chroma AC EOB
        w.put(0b00, 2); // Cr DC size 0
        w.put(0b00, 2); // chroma AC EOB
        w.put(code::FRAME_END, 4);
        let data = w.finish();
        let mut fb = Framebuffer::default();
        AspeedDecoder::new().decode_frame(8, 8, &data, &mut fb).unwrap();
        let p = fb.pixels[0];
        let (r, g, b) = (p >> 16, (p >> 8) & 0xFF, p & 0xFF);
        assert!(r == g && g == b && (120..=140).contains(&r), "{p:06x}");
    }

    #[test]
    fn assembler_releases_frame_on_next_start() {
        let mut a = FrameAssembler::new();
        let mut p1 = header(640, 480, true);
        p1.extend([1, 2, 3, 4]);
        let mut p2 = header(640, 480, false);
        p2.extend([5, 6, 7, 8]);
        assert!(a.push(&p2).is_none()); // before any frame start: dropped
        assert!(a.push(&p1).is_none());
        assert!(a.push(&p2).is_none());
        let f = a.push(&header(640, 480, true)).unwrap();
        assert_eq!((f.width, f.height), (640, 480));
        assert_eq!(f.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(a.push(&[1, 2, 3]).is_none());
        assert_eq!(parse_header(&[0; 3]), AspeedFrameHeader::default());
    }

    #[test]
    fn garbage_never_panics() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut d = AspeedDecoder::new();
        let mut fb = Framebuffer::default();
        for len in [0usize, 1, 3, 4, 7, 8, 64, 1000, 20000] {
            for _ in 0..20 {
                let data: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                for (w, h) in [(640, 480), (720, 400), (8, 8), (1, 1), (0, 0), (1023, 767)] {
                    let _ = d.decode_frame(w, h, &data, &mut fb);
                    let hdr = AspeedFrameHeader { width: w as u16, height: h as u16, ..Default::default() };
                    let _ = frame_complete(&hdr, &data);
                }
            }
        }
    }
}
