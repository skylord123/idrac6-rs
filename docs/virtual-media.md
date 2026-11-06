# Virtual Media protocol

iDRAC6 Virtual Media: map a local ISO/IMG file as a virtual CD/DVD or
floppy / removable disk on the server.

## Protocol (reverse engineered from `avctKVM.jar`, `com.avocent.vm.*`)

All integers are big-endian.

### 1. Getting credentials (on the KVM channel)

Virtual media does not use the web login or the JNLP `user`/`passwd`
tokens directly. The KVM viewer (`com.avocent.idrac.kvm.u`/`q`/`r`) sends a
*RequestCredentials* message on the already logged-in KVM **command**
channel and waits up to 30 s for the answer:

```text
request  (type 1063 = 0x0427): "BEEF" 04 27 00 10 | 00 00 00 1E 00 00 00 00
response (type 33844 = 0x8434): "BEEF" 84 34 len16 | user[32] password[32]   (NUL padded)
```

`proto::kvm_credentials_request` and `proto::parse_kvm_credentials`
build/parse these. The returned pair is a one-off virtual-media login.
The Java viewer then starts `VirtualMedia.main` with
`user=<u> password=<p> path=a:<HOST>,p:<KMPORT>,... id=Server1 application_name=avworks`.
The bundled `Options.properties` sets `TcpPortSpecifiedOnPath=true`, so
virtual media connects to **HOST:KMPORT** (the JNLP `kmport`, 5900 on
iDRAC6), the same port as the KVM.

### 2. Session setup (APCP, cleartext)

Client sends a 53-byte APCP request, identical to the KVM one except the
application byte is `2`:

```text
"APCP" len32=53 type16=0x0100 0x0000 app8=2 0 0 0 caps32=1 rndlen8=32 rnd[32]
```

The appliance replies with 53 bytes:

```text
"APCP" len32=53 type16=0x8100 0x0000 major8 minor8 caps32 tcpport16 rndlen8 rnd[32]
```

`caps` selects the transport: `1` = cleartext TCP (the default on iDRAC6;
observed on a live iDRAC6), `4` = anonymous SSL (when "Virtual Media
encryption" is enabled; TLS is then started on the same socket),
`2` (RC4) and `8` (client cert) are not supported by the Java client
either. The Java client always requests `1`.

### 3. Packets ("AVMP")

Every later packet in both directions has a 10-byte header:

```text
"AVMP" total_len32 (including header) type16 | body...
```

Most bodies start with `disk_id16` at offset 10 (`0xFFFF` = all drives).

### 4. Login

```text
LOGIN (256), 153 bytes: 0x0000 ulen8 user[96] password[32] rip[8]=0
                         port8=0 channel8=0 0 preempt8
LOGIN_STATUS (0x8100): 0x0000 status8 flags8 ulen8 user[96]
```

`status`: 0 ok, 1 bad user, 2 bad password, 3 access denied, 4 channel in
use, 5 channel not found, 6/11 in use by another user (can be preempted
by repeating LOGIN with `preempt=1` on the same socket), 8 all channels
in use, 22/23 authentication server errors.

### 5. Drive discovery and mapping

* `GET_VDISK_INFO (512)`: `disk=0xFFFF`. Sent right after login and before
  each map.
* `VDISK_INFO (33280)`: `0x0000 count16 { type8 status8 caps16 } * count`.
  Remote drive types: 1 floppy, 2 CD/DVD, 4 removable disk. Status 0 =
  idle (free), 1 attached, 2 disabled, 3 broken.
* A CD image (ISO) is mapped on the first idle drive of type 2:
  `VDISK_REQUEST (528)`: `disk16 block_size32=2048 blocks32 ro8=1
  min(blocks,255)8 toc_len16 toc[toc_len]`, where `blocks = ceil(size/2048)` (Dell's client sends `size/2048 - 1`, which hides the last 2 KiB of the image from the server)
  and the TOC is the fixed 20-byte blob in `proto::ISO_TOC`.
* A floppy/removable image (IMG) is mapped on the first idle drive of type
  1 or 4: `VDISK_REQUEST2 (529)`: `disk16 block_size32=512 blocks32 ro8
  min(blocks,255)8 cylinders16 heads8 sectors8`. Geometry comes from the
  image's boot sector (FAT BPB, or the first MBR partition). On a type-4
  drive only the first MBR partition is exposed (block offset = its LBA).
* `VDISK_RELEASE (544)`: unmap `disk`.
* `DEVICE_STATUS (33808)`: `disk16 status32`. 1/3/7 fine; 2 read failed,
  4 USB not connected, 5 already attached, 6 released, 8 mapping failed,
  other = unknown error. Errors make the client unmap the drive.
* `VDISK_REQUEST_RELEASE (33344)`: the iDRAC asks the client to release
  `disk`; the client unmaps it.

### 6. Block I/O

There is no SCSI on the wire; the iDRAC emulates the USB/SCSI device and
only asks for blocks:

* `VDISK_READ (33536)`: `disk16 start32 blocks32 blocking_factor32`.
  The client answers with one or more `VDISK_READ_DATA (768)` packets of
  at most `blocking_factor` blocks each: `disk16 start32 blocks32
  data[blocks*block_size]`, then `CLIENT_STATUS (1040)`: `disk16
  status32` (0 ok, 2 read error).
* `VDISK_WRITE (33552)`: `disk16 start32 blocks32 data[blocks*block_size]`.
  The client writes and answers `CLIENT_STATUS` (0 ok, 3 write error).

### 7. Housekeeping

* `HEARTBEAT (1024)`, 12 bytes: sent when nothing was sent for 10 s.
* `USB_RESET (1056)`: `disk=0xFFFF`, resets the virtual USB device.
* `CLIENT_CONFIGURATION_OPTION (1072)`: "reserve KVM" option (unused here).
* `DEVICE_CONFIGURATION_OPTION (33840)`: `0x0000 option32 pad[4] value8`:
  1 = drives forced read-only, 2 = locked to the KVM session,
  3 = reserve available.
* `DISCONNECT (33040)`: `0x0000 reason8 status8 timeout8`. Reasons:
  0 administrator, 1 idle timeout, 2 iDRAC reboot, 3 firmware upgrade,
  4 preempted by local user, 6 KVM closed while VM was locked to it.

## Usage

```ignore
// 1. On the logged-in KVM session: send proto::kvm_credentials_request(),
//    wait for message 33844 and parse it with proto::parse_kvm_credentials.
let (username, password) = /* from the KVM channel */;
let (tx, rx) = std::sync::mpsc::channel();
let vm = VmSession::connect(VmConfig { host, port: kmport, username, password, preempt: false }, tx)?;
vm.map(VmImage { path: "debian.iso".into(), kind: DriveKind::Cd, read_only: true })?;
// ... handle VmEvent from rx ...
vm.disconnect();
```
