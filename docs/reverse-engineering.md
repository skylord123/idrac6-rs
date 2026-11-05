# How the protocol was recovered

idrac6-rs contains no Dell or Avocent code. The protocols were worked out
from the viewer that every iDRAC6 serves, and re-implemented from scratch.
You can reproduce this with the script in `tools/re/`.

## Getting the viewer

The web UI's "Launch Virtual Console" button downloads `viewer.jnlp`, which
points at jars on the iDRAC itself:

| Jar | Contents |
|---|---|
| `/software/avctKVM.jar` | The viewer, Java (~490 classes) |
| `/software/avctKVMIOLinux64.jar` | `libavctKVMIO.so`: native keyboard grabbing only |
| `/software/avctVMLinux64.jar` | `libavmlinux.so`: physical CD/floppy access for Virtual Media |

All video decoding and protocol handling is in Java. The native libraries
only grab the local keyboard and read physical drives. Their keyboard
table showed that the wire key codes are USB HID usage ids.

## Decompiling

```sh
tools/re/decompile.sh 192.168.1.120
```

This downloads the jars into `re/`, decompiles them with
[CFR](https://github.com/leibnitz27/cfr), and decrypts the strings. The
classes are obfuscated: names are single letters, and every string literal
is XOR-encrypted with a per-class 5-byte key and decoded in the static
initializer (Zelix KlassMaster style). `ZDump.java` loads each class and
reads the decrypted string fields by reflection. `zsub.py` substitutes them
back into the source, so log messages such as `"Client random in
request: "` become readable again. Those messages made it easy to name
things.

## Where things live

| Package | What |
|---|---|
| `com.avocent.kvm.b.k` | APCP handshake and SSL setup |
| `com.avocent.kvm.b.p` | Session: sockets, login, video channel, power, refresh |
| `com.avocent.kvm.b.m` | Incoming message dispatch |
| `com.avocent.kvm.b.a.*` | One class per message; `e()` returns its name |
| `com.avocent.kvm.b.g`, `.f` | Mouse and keyboard senders |
| `com.avocent.kvm.a.a.a`, `.c` | ASPEED (AST2000/2050) video decoder |
| `com.avocent.idrac.kvm.*` | iDRAC-specific UI: menus, power, sharing dialogs |
| `com.avocent.vm.*` | Virtual Media |

## Verifying

- The video decoder was checked against the original. A Java harness
  generated random valid streams covering every block type, ran them
  through the jar's own decoder, and compared the output pixel by pixel
  with the Rust port: about 5.8 million pixels over 119 frames, with no
  differences.
- The APCP handshakes and TLS setup were checked against a real iDRAC6.
- The message layouts have unit tests built from the decompiled
  serializers.
