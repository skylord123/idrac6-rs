# iDRAC6 virtual console protocol

This describes the protocol the iDRAC6 Java viewer (`avctKVM.jar`, Avocent
"AVSP") uses, as implemented in `src/kvm/`. Everything was recovered from
the decompiled viewer; see [reverse-engineering.md](reverse-engineering.md)
for how. All integers are big endian unless stated otherwise.

## Overview

1. Log in to the web UI (`POST /data/login`).
2. Download `viewer.jnlp(...)`. Its `<argument>` list holds the console
   parameters, including the one-time session tokens `user` and `passwd`.
3. Open the **command** channel on `kmport` (5900): APCP handshake, TLS,
   login with the tokens.
4. Open the **video** channel on `vport` (5900): APCP handshake, TLS, video
   auth.
5. Exchange messages until one side closes. Log out of the web UI.

Keyboard, mouse, power and control messages travel on the command channel.
Compressed video comes back on the video channel.

## 1. Web login and JNLP

```text
POST /data/login            user=<user>&password=<password>
  -> <root><status>ok</status><authResult>0</authResult>
           <forwardUrl>index.html?ST1=..,ST2=..</forwardUrl></root>
```

| `authResult` | Meaning |
|---|---|
| 0 | OK |
| 1 | Wrong username or password |
| 4 | User may not log in |
| 5 | Too many web sessions |

The session is the `_appwebSessionId_` cookie. Newer firmware also wants
the `ST2` value echoed in an `ST2` request header.

```text
GET /viewer.jnlp(<host>@0@<title>@<epoch-millis>)
```

The JNLP's arguments look like this:

```text
ip=192.168.1.120  vmprivilege=true  user=1804289383  passwd=846930886
kmport=5900  vport=5900  apcp=1  version=2  title=...  helpurl=...
```

`user`/`passwd` are single-use tokens, not the real credentials. `GET
/data/logout` ends the web session. iDRAC6 allows only a few concurrent
web sessions, so log out when done.

The iDRAC6 web server speaks TLS 1.0 only. It also offers DHE suites, but a
DHE handshake takes about 6 s on its CPU, so prefer plain RSA (`AES128-SHA`
takes about 0.6 s).

## 2. APCP handshake

Both channels start in clear text with a 53 byte request:

```text
"APCP" len32=53 type16=0x0100 0x0000 channel8 0 0 0 caps32 rndlen8=32 rnd[32]
```

- `channel`: 3 = command, 4 = video. Virtual media uses 2.
- `caps`: what the client accepts. 1 = plain TCP, 4 = SSL; the viewer sends 5.

The reply has the same size:

```text
"APCP" len32=53 type16=0x8100 0x0001 x8 x8 caps32 port16 rndlen8 rnd[32]
```

If `caps` has bit 0 set, the connection stays plain. If it has bit 2 set,
start TLS on the same socket. The iDRAC6 answers 4, so KVM is always TLS.
A non-zero `port` means "reconnect to this port" (without another APCP).

The TLS session is plain TLS 1.0 with an expired self-signed certificate.
The viewer enables anonymous DH, RC4, DES and NULL suites; in practice the
iDRAC picks `AES256-SHA` or `AES128-SHA`.

## 3. Message framing

Every message has an 8 byte header followed by the payload:

```text
command channel: "BEEF" type16 length16 | payload
video   channel:  ?? ?? ?? ?? ?? type8 length16 | payload
```

`length` includes the header. The client rejects lengths below 16 or
above 15000. Most fixed-size client messages have an 8 byte payload (total
length 16).

On the video channel the server only fills the low byte of the type. The
first message the client sends there uses the header `00 00 00 00 01
type8 length16`.

## 4. Command channel: login

Login request, type `0x0102` (v2; v1 is `0x0100` without the last byte),
total length 217:

```text
userlen8  user[96]      JNLP "user" token, NUL padded
passlen8  pass[96]      JNLP "passwd" token
rip[8]                  zero on iDRAC
port8 = 1
channel8 = 0
client_random32         any value; repeated in the video auth
share8                  0 = exclusive, 1 = ask to share an existing session
```

Login response, type `0x8305` (v2) or `0x8300` (v1):

```text
v2: status8 x8 x8 x8 session_id32 flags16 msglen8 msg[...]
v1: status8 x8 x8 flags8 session_id32 msglen8 msg[...]
```

| status | Meaning |
|---|---|
| 0 | OK |
| 1, 2 | Bad login (expired or used token) |
| 3 | Access denied |
| 4, 12 | Console in use (retry with `share=1` to ask the user) |
| 8 | All channels in use |
| 15 | Sharing request denied |
| 16 | Sharing request timed out |

Flags:
- bit 0: video is multiplexed on the command channel. Not seen on iDRAC6.
  The video message types then use `0x86xx`.
- bits 5 and 6 together: the session is view-only.

## 5. Video channel

After the APCP/TLS setup the client sends Video Channel Auth, type 1, with
the short header:

```text
00 00 00 00 01 01 00 10 | client_random32 session_id32
```

The server answers Video Connect Status (`0x84`). The client then sends,
on the **command** channel:

| Type | Message | Payload |
|---|---|---|
| `0x030e` | Video Enable | `01 01 00 ...` |
| `0x0302` | Set Display Area | `width16=1024 height16=768 ...` |
| `0x0304` | Scale 1:1 | zeros |

Video messages from the server:

| Type | Meaning |
|---|---|
| `0x86` | ASPEED compressed video, see [video-codec.md](video-codec.md) |
| `0x85` | Video stopped. byte0: 0 calibrating, 1 no signal, 2 out of range, 3/4 blocked by the local admin |
| `0x80`–`0x83`, `0x87`–`0x89` | Older Avocent DVC codec, text mode, palette and font table. Not used by iDRAC6 (AST2050). |

For every message that starts a new frame (and for video stopped), the
client sends Video Ack on the video channel: `"BEEF" 0000 0010 | 01 00 00
00 00 00 00 00`.

## 6. Command channel: client messages

All have an 8 byte payload unless stated otherwise.

| Type | Name | Payload |
|---|---|---|
| `0x0200` | Keyboard | `00 release8 usage16 00 00 00 00`. `release` is 0 for press, 1 for release. `usage` is a USB HID keyboard usage id (0x04 = A, 0xe0 = Left Ctrl, ...). |
| `0x0201` | Mouse (absolute) | `00 buttons8 x16 y16 wheel16`. Buttons: 1 left, 2 right, 4 middle. Coordinates are in remote pixels. `wheel` is +1/-1 (signed). |
| `0x0202` | Mouse origin | zeros |
| `0x0204` | Keyboard LED request | zeros |
| `0x0208` | Focus | `focus8` |
| `0x0209` | Mouse (relative) | same layout as absolute |
| `0x020a` | Mouse acceleration | `value8` |
| `0x0301` | Screen refresh | zeros |
| `0x0400` | Keep-alive | zeros. The viewer sends it every 10 s together with `0x0420`. |
| `0x0402` | Bandwidth limit | `level8`: 0 unlimited, 1 10 Mbit, 2 1 Mbit, 3 512 kbit, 4 256 kbit |
| `0x0403` | Colour depth | `x8 depth8 color8 x8` |
| `0x0413` | Sharing response | `answer8 00 request_id16`. Answers: 1 reject, 2 approve, 3 view-only. |
| `0x0420` | Get available servers | zeros |
| `0x0424` | Set power | `idlen16 id[idlen] action8`. Actions: 1 on, 2 off, 3 cycle, 4 reset, 5 graceful shutdown. `id` comes from `0x8430`. |
| `0x0427` | Request VM credentials | `00 00 00 1e 00 00 00 00` |

## 7. Command channel: server messages

| Type | Name | Payload |
|---|---|---|
| `0x8100` | Keyboard LEDs | `bits8`: 1 scroll, 2 num, 4 caps (PS/2 LED order) |
| `0x8101` | Mouse ack | `count8` |
| `0x8102` | Mouse acceleration | `value8` |
| `0x8200` | Input resolution | `w16 h16` |
| `0x8201` | Display resolution | `w16 h16`. The viewer asks for a screen refresh. |
| `0x8202` | Video setup data | ten u16 values (brightness, contrast, position, ...) |
| `0x8204` | Colour mode | `depth8 color8` |
| `0x8301` | Disconnect pending | `reason8`: 1 admin, 2 iDRAC reboot, 3 local user took over, 4 firmware upgrade, 5 last user left, 6 primary user went exclusive, 7 idle timeout |
| `0x8304` | Pending request cancelled | |
| `0x8401` | Sharing request | `id16 kind8 user[...]` |
| `0x8420` | User privileges | |
| `0x8430` | Available servers | `count16`, then per server: `idlen16 id power16 status16 namelen16 name` |
| `0x8431` | Selected server | `idlen16 id` |
| `0x8432` | Server status | one server entry as above |
| `0x8434` | VM credentials | `user[32] password[32]`, NUL padded |
