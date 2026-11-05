# idrac6-rs

A small native app that brings back remote control of Dell **iDRAC6**
servers (PowerEdge R610, R710, T610 and friends), without Java.

- **Virtual console**: see the server's screen, use keyboard and mouse,
  send Ctrl-Alt-Del and other macros, power control, screenshots.
- **Virtual Media**: attach an ISO or disk image from your computer to the
  server as a USB CD/DVD or disk.
- **Web UI proxy**: open the iDRAC web interface in a modern browser. The
  proxy talks the iDRAC's TLS 1.0 for you.
- One window to manage all your iDRACs. Runs on Linux and Windows.
- Written in Rust. A single executable, no Java, starts instantly.

## Why

iDRAC6 is old. Its web server only speaks TLS 1.0, which browsers refuse
(`ERR_SSL_VERSION_OR_CIPHER_MISMATCH`). Its console is a Java Web Start
app that current Java versions no longer run. idrac6-rs reimplements both
the console protocol and the video codec natively, so you only need this
one program.

## Install

Download the latest build from the
[releases page](https://github.com/skylord123/idrac6-rs/releases):

- **Ubuntu / Debian**: `idrac6-amd64.deb`. Install it with
  `sudo apt install ./idrac6-amd64.deb`, then start *iDRAC6 Console* from
  your applications menu.
- **Windows**: `idrac6-windows-x86_64.zip`. Unzip and run `idrac6.exe`.
- **Other Linux**: `idrac6-linux-x86_64.tar.gz`. Extract and run
  `./idrac6`. Needs a desktop with OpenGL (X11 or Wayland).

Or build it yourself (Rust 1.85 or newer):

```sh
git clone https://github.com/skylord123/idrac6-rs
cd idrac6-rs
cargo build --release            # uses the system OpenSSL
cargo build --release --features vendored-openssl   # builds OpenSSL in (Windows, portable builds)
./target/release/idrac6
```

On Linux the system OpenSSL build needs `libssl-dev` (Debian/Ubuntu) or
`openssl-devel` (Fedora). On Windows use `--features vendored-openssl`,
which needs Perl and NASM.

To build the Debian package (binary, menu entry and icons):

```sh
cargo install cargo-deb
cargo deb --features vendored-openssl    # -> target/debian/idrac6_*.deb
```

## Usage

Start `idrac6`, click **Add iDRAC**, and enter the address, user and
password (the same ones you use for the iDRAC web page).

### Virtual console

Click **Console**. The app logs in to the web interface, gets a console
session token, and connects. Logging in takes a few seconds; the iDRAC6 is
slow.

- **Keyboard** goes to the server while the mouse pointer is over the
  screen. Keys are sent by physical position, so the server's keyboard
  layout applies, like with a real USB keyboard.
- Keys your desktop keeps for itself (Alt-Tab, the Windows key, Ctrl-Alt-Fn
  on Linux) can be sent from **Keyboard → Macros**. **Keyboard → Linux
  SysRq** has the magic SysRq combinations.
- If a key seems stuck on the server, use **Keyboard → Release all keys**.
  Keys are released automatically when the window loses focus.
- **Power** has On, Off, Reset, Power Cycle and Graceful Shutdown. Each
  one asks for confirmation.
- **View** switches between fit-to-window and 1:1, and has full screen.
- **File → Save screenshot** saves a PNG.
- **Video → Bandwidth limit** helps on slow links.
- If someone else is using the console, you can ask them to share it. If
  you are the one using it and someone asks, you can allow, allow view
  only, or reject.

You can also open a saved console directly, for example from a desktop
shortcut: `idrac6 console "R710 lab"` (needs a saved password).

### Virtual Media

In the console window open **Virtual Media**, click **Connect Virtual
Media**, then **Map image…** next to *Virtual CD/DVD* (an `.iso`) or
*Virtual floppy / disk* (an `.img`). The server sees a USB drive it can
boot from (choose it in the boot menu with F11). Keep the console window
open while the image is in use.

### Web interface

Click **Web UI**. This starts a local proxy and opens
`https://localhost:<port>/` in your browser. The first time, the browser
warns about the certificate. It's a self-signed certificate made by
idrac6-rs for `localhost`; accept it once.

You can also run only the proxy, without a GUI (for example on a jump
host):

```sh
idrac6 proxy 192.168.1.120 --port 8443
```

The proxy allows at most two connections to the iDRAC at a time. The
iDRAC6 web server locks up for minutes when a browser opens its usual six
at once.

## Settings

Saved iDRACs live in `config.toml`:

| OS | Location |
|---|---|
| Linux | `~/.config/idrac6-rs/` |
| Windows | `%APPDATA%\idrac6-rs\config\` |

Passwords are stored **in plain text** in that file when "Save password"
is ticked. On Linux the file is only readable by your user. Untick it to
be asked for the password each time instead. The proxy's certificate and
key are stored in the same directory.

Set `RUST_LOG=debug` for detailed logs, or `IDRAC6_CONFIG_DIR` to use
another settings directory.

## Troubleshooting

- **"Login failed: too many web sessions"**: the iDRAC6 allows only a few
  web sessions. Log out elsewhere or wait a few minutes for old sessions
  to expire.
- **"The virtual console is already in use"**: someone (or a crashed
  viewer) has it open. Use *Ask the current user to share*, or wait for the
  old session to time out.
- **Black screen with "No signal"**: the server is off or its screen is
  asleep. Move the mouse or press a key.
- **Mouse pointer out of sync**: the iDRAC6 sends absolute positions; set
  the server OS mouse acceleration to "none" if the pointers drift apart.
  iDRAC settings → Virtual Console → *Mouse mode* can also help.

## How it works

The iDRAC6 Java viewer was decompiled and its protocols re-implemented. The
docs describe the formats:

- [docs/protocol.md](docs/protocol.md): console protocol (APCP, AVSP, input, power)
- [docs/video-codec.md](docs/video-codec.md): the ASPEED AST2050 video codec
- [docs/virtual-media.md](docs/virtual-media.md): Virtual Media protocol
- [docs/reverse-engineering.md](docs/reverse-engineering.md): how this was done and how to reproduce it

Code layout:

```text
src/
  main.rs        command line
  gui/           windows (winit + egui): manager, console, virtual media
  kvm/           console protocol: messages, session, keyboard map
  video/aspeed.rs  ASPEED video decoder
  vmedia/        Virtual Media protocol and image serving
  web.rs         iDRAC web login and viewer.jnlp
  proxy.rs       HTTPS -> TLS 1.0 web proxy
  tls.rs         legacy TLS settings
```

## Status

New and still being tested. The web proxy has been tested against an
iDRAC6 on a PowerEdge R710. The video decoder matches the original Java
decoder pixel for pixel. The console and Virtual Media were built from the
reverse-engineered protocol and need more real-world testing. Bug reports
with a `RUST_LOG=debug` log are very welcome.

Not supported: iDRAC7 and newer (they use different, HTML5 consoles),
physical CD drives and smart cards for Virtual Media, and the older Avocent
DVC video codec that iDRAC6 does not use.

## License

MIT. Not affiliated with or endorsed by Dell or Vertiv/Avocent.
