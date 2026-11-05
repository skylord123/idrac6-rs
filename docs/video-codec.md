# ASPEED video codec

Decoder for the ASPEED (AST2000/AST2050) video stream used by iDRAC6.

The iDRAC6 virtual console sends video as "ASpeed JPEG Video" packets
(AVSP message type 134, or 0x8606 = 34310 for the secondary form). This
module is a port of the decoder in Dell/Avocent's `avctKVM.jar`
(`com.avocent.kvm.a.a.a` / `com.avocent.kvm.a.a.c`, which logs itself as
"AST2000 Video Decode").

## Packet layout and frame assembly

Each video message payload (the bytes after the 8 byte `BEEF` header)
starts with a 12 byte sub-header, see `AspeedFrameHeader`. A frame is
split over several messages. The first message of a frame has bit 0 of
payload byte 8 set; the data of the following messages (without their
12 byte sub-header) is appended to it. The Java client hands a frame to
the decoder only when the first message of the *next* frame arrives
(`kvm/b/m.java`); `FrameAssembler` implements exactly that rule. The
client also counts a Video Ack (see the session layer) once per frame
start.

## Bitstream

The frame data is read as little-endian 32-bit words, and bits are taken
MSB first from each word. The stream is a sequence of blocks, each 8x8
pixels (the decoder is always in YUV 4:4:4 mode; the 16x16 4:2:0 mode of
the ASPEED reference decoder is never enabled by the iDRAC client). Each
block starts with a 4 bit code:

| code | name                 | meaning |
|------|----------------------|---------|
| 0    | JPEG_NO_SKIP         | JPEG block at the next position, normal quant tables |
| 8    | JPEG_SKIP            | 8 bit x, 8 bit y block position follow, then a JPEG block |
| 4    | LOW_JPEG_NO_SKIP     | JPEG block using the "advanced" (low quality) quant tables |
| 12   | LOW_JPEG_SKIP        | position, then a low-quality JPEG block |
| 5    | VQ_NO_SKIP_1_COLOR   | solid block from the VQ colour cache |
| 13   | VQ_SKIP_1_COLOR      | position, then a solid VQ block |
| 6    | VQ_NO_SKIP_2_COLOR   | 2 colours, 1 bit per pixel |
| 14   | VQ_SKIP_2_COLOR      | position, then 2 colours |
| 7    | VQ_NO_SKIP_4_COLOR   | 4 colours, 2 bits per pixel |
| 15   | VQ_SKIP_4_COLOR      | position, then 4 colours |
| 9    | FRAME_END            | end of frame |

"SKIP" codes move to an explicit block position (blocks that are not
sent keep their previous contents); "NO_SKIP" codes use the block after
the previous one, wrapping at the right edge and at the bottom.

A JPEG block is three baseline-JPEG data units (Y, Cb, Cr) using the
standard JPEG Huffman tables, a floating point dequantisation and the
AAN integer IDCT, followed by a table driven YCbCr to RGB conversion.
A VQ block first lists its colours (each either a 2 bit index into a
4 entry cache, or a new 24 bit YCbCr value stored in the cache), then
the per-pixel cache selectors.

The decoder state (DC predictors, VQ cache) is reset at every frame, so
frames are independent apart from the pixels left over in the
framebuffer.

Notes on the original:
* The quantisation tables are fixed in the Java client (luma table 4,
  chroma table 7, low-quality tables 0, scale 16) and are not read from
  the stream. Payload bytes 9 and 10 are parsed by the client but never
  used.
* The `swapRedAndBlue` system property of the Java client only applies
  to the palette based DVC decoders, not to this one. Output is RGB.
* The Java code never writes pixel 0 of the frame (an off-by-one in its
  YUV conversion); this port writes it.
