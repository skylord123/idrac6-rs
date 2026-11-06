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

## Boot capture recordings

iDRAC6 records the console during the last three server boots ("Boot
Capture", web UI page `lastboot.html`). The recordings use the same DVC
codec. `src/video/playback.rs` parses and plays them.

### Web endpoints

| Request | Result |
|---|---|
| `GET /data?get=bootCapFileData` | XML: `bootCapEnabled`, `bootCapCount`, `bootCapFname0..2` (e.g. `bootcapture_2`, or `No Recording Available`), `bootCapDate0..2`, `bootSortDate0..2` (Unix time). Index 0 is the latest. |
| `GET /data?type=jnlp&get=captureFile(<idx>@<ip>@<ipv6fmt>@ @<millis>)` | JNLP for `com.avocent.idrac.kvm.Main` with `url=https://<ip>:443/captures/<fname>`, `user`, `passwd`, `maxFileLength=2097152`, `title`. |
| `GET /captures/<fname>` | The recording itself, at most 2 MiB. It needs neither a session cookie nor the JNLP tokens. |

The viewer maps the JNLP argument `url` to its `dvcurl` property, which
switches it to playback mode. The file is fetched with a plain HTTP GET
(the `user`/`passwd` tokens are unused), and at most `length` bytes
(default 2 MiB) are read.

### File format

A sequence of records, big endian, read from offset 0 (it is not a ring
buffer):

```text
len32                  total record length including these 4 bytes, >= 20
"DE AD BE EF"
timestamp32            milliseconds since recording start
width16  height16
flags8                 bit 0 begin of frame, bit 1 end of frame, bit 2 unknown (ignored)
x8 x8 x8
data[len - 20]         DVC opcode stream, same as a Video Packet after its 12 byte sub-header
```

The reader (`com.avocent.kvm.d.a.a.q`, mode 3) stops at a record shorter
than 20 bytes or one that runs past the end of the file. Files are usually
filled up to exactly 2 MiB. In practice every record has both begin and
end set, so each record is one complete screen update. The text-mode boot
screens report 752×413, and the size changes when the OS switches mode.
Records are decoded by the 15 bit colour decoder (`d.c`, i.e. as type
`0x81`), with no checksum.

### Console recordings

idrac6-rs writes console recordings (`.dvcrec`) in the same format: each
live `0x81` Video Packet becomes one record, with its sub-header's width,
height and begin/end flags and the time since recording started. They can
be longer than 2 MiB; the player reads the whole file. Packets of other
types are not recorded (iDRAC6 only sends `0x81`).

### Timing and controls

`com.avocent.kvm.d.a.b` sleeps `timestamp - previous timestamp` before
each record, capped at 1000 ms, so long idle periods in a recording play
back in one second. A 460 s recording plays in about 370 s.

| Control | Java behaviour |
|---|---|
| Play | Decode continuously at the recorded pace. |
| Pause | Stop decoding. |
| Step | While paused, decode up to the next completed frame. |
| Rewind | Reset decoder and screen to black and restart from the first record, paused. |
| Replay | Rewind, then play. |
| Frame: | Counter of completed frames since the last rewind. |

`playback::Player` provides the same operations (`advance_to`, `step`,
`rewind`, `frames_shown`) and leaves the clock to the caller.
