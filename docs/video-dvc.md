# DVC video codec

The iDRAC6 sends its video with Avocent's "DVC" codec. It is implemented in
`src/video/dvc.rs` and ported from `com.avocent.kvm.d.b` (base), `d.c`,
`d.d` and `d.e` (colour variants), and the video model
`com.avocent.kvm.c.i`/`c.j` of the iDRAC6 viewer. The decoder in the
viewer that logs itself as ASPEED (`docs/video-codec.md`) exists, but the
iDRAC6 does not use it.

## Video Packet messages

The video arrives on the video channel as "Video Packet" messages. The
message type selects how pixel colours are coded:

| Type | Java decoder | Pixel opcode |
|---|---|---|
| `0x81` (also `0x8601` on the command channel) | `d.c` | 15 bit RGB, 2 bytes. This is what iDRAC6 sends. |
| `0x82` (`0x8602`) | `d.e` | 7 bit index into a fixed 128 colour palette, 1 byte |
| `0x83` (`0x8603`) | `d.d` | 7 bit grey level (value × 2), 1 byte |

Every payload (the bytes after the 8 byte message header) starts with a
12 byte sub-header, followed by the opcode stream:

| Offset | Size | Meaning |
|---|---|---|
| 0 | 4 | unused |
| 4 | 2 | screen height |
| 6 | 2 | screen width |
| 8 | 1 | flags: bit 0 = begin of frame, bit 1 = end of frame |
| 9 | 1 | unused |
| 10 | 2 | with the end flag: 16 bit checksum of the screen, 0 = none |
| 12 | … | opcode stream |

A frame (a full screen or an update) is one or more packets from a
begin-flag packet to an end-flag packet. On the live iDRAC6 a full
1280×1024 screen comes as 8184 byte packets, and small updates as single
17 to 177 byte packets with both flags set. **Opcodes may cross packet
boundaries**: the decoder keeps reading in the next packet of the same
frame. Packets received before the first begin-flag packet are ignored.

## Drawing

The decoder keeps the screen as a `width × height` pixel array and a drawing
position, the pixel index in row-major order. A begin-flag packet sets the
position to 0. After the end-flag packet the screen is complete and is
repainted. A width/height that differs from the current one (and is within
1..4096) reallocates the screen as black. The model refuses sizes above
1600, which the original then mishandles.

Each opcode byte's top three bits select the operation:

| Bits | Operation |
|---|---|
| `000nnnnn` | **Skip**: leave `n` pixels unchanged (advance the position). |
| `010nnnnn` | **Copy above**: copy `n` pixels from one row up (overlapping, forwards). Ignored on the first row. |
| `001nnnnn` | **Copy left**: repeat the pixel left of the position `n` times. Before position 0 the colour is 0. |
| `011cbbbb` | **Two-colour mask**, see below. |
| `1xxxxxxx` | **Pixel**: one pixel. For `0x81` the colour is `(xxxxxxx << 8) \| next_byte` as RGB555. For `0x82`/`0x83` it is `xxxxxxx` as palette index or grey level. |

### Run lengths

`n` starts with the opcode's low 5 bits. While the next byte has the same
top three bits, up to four more bytes are consumed. Each adds 5 more bits:
`n |= (byte & 0x1f) << (5 * k)` for k = 1..4. The `0x82`/`0x83` variants add
2 to every run length.

### Two-colour mask

The two colours are A, the pixel left of the position, and B, the nearest
pixel before that (scanning backwards) whose value differs from A, or A
itself if there is none. The opcode's low four bits are four pixels, MSB
first (0 = A, 1 = B). If bit 4 (`c`) is set, mask bytes follow: each gives
7 more pixels (bits 6..0), and its bit 7 says whether another mask byte
follows. A run of pixels that would reach the end of the screen buffer is
dropped without advancing the position.

Pixels are compared including the model's alpha channel. Drawn pixels are
opaque (`0xFF000000 | rgb`), but "copy left" at position 0 and reads
outside the screen give 0. The port keeps ARGB internally to reproduce
this exactly.

### RGB555

`r = (v & 0x7c00) >> 7`, `g = (v & 0x3e0) >> 2`, `b = (v & 0x1f) << 3`, so
the low three bits of each channel are always zero.

### Checksum

When an end-flag packet carries a non-zero checksum, the viewer sums over
all screen pixels whose RGB is non-zero, as a 16 bit wrapping sum. For
`0x81` it adds the RGB555 value, `((p & 0xff0000) >> 9) | ((p & 0xff00) >>
6) | ((p & 0xff) >> 3)`. For the palette and grey variants it adds the
pixel's palette index. A mismatch is only logged, unless the viewer is set
to request a full refresh. The live iDRAC6 sends 0 (no checksum).

## Session handling

- The viewer acknowledges every Video Packet, not every frame: for each
  one it sends a Video Ack (`BEEF 0000 0010 | 01 00…`) on the video
  channel (`kvm/b/m.java` `a(fb)` → `p.v()` with a threshold of 1).
- A type `0x03` message (8 bytes) arrives on the video channel before the
  first Video Packet. The viewer has no handler for it and ignores it.
- Text mode (`0x87`), palette (`0x88`) and font table (`0x89`) messages
  belong to other Avocent products. The DVC decoder does not use them.

## Verification

A capture of 96 Video Packets from a live iDRAC6 (an Unraid text console,
1280×1024) was decoded by the original Java `d.c` decoder, driven exactly
like `kvm/b/q.f()` drives it, and by the Rust port. All 79 completed
frames have identical RGB contents.
