# idrac6-rs

A small native app that brings back remote control of Dell **iDRAC6**
servers (PowerEdge R610, R710, T610 and friends), without Java.

- **Virtual console**: see the server's screen, use keyboard and mouse,
  send Ctrl-Alt-Del and other macros, power control, screenshots.
- **Boot captures**: replay the iDRAC's recordings of the last boots.
- **Recording**: record a console session to a file and replay it.
- **Virtual Media**: attach an ISO or disk image from your computer to the
  server as a USB CD/DVD or disk.
- **Web UI proxy**: open the iDRAC web interface in a modern browser. The
  proxy talks the iDRAC's TLS 1.0 for you.
- **Health**: sensors, power use and the System Event Log.
- One window to manage all your iDRACs, with each server's power state,
  health, temperature, power use and a picture of its screen. Power
  control and the identify LED right from the list. Runs on Linux and
  Windows.
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

On Linux the build needs `libssl-dev` and `libdbus-1-dev` (Debian/Ubuntu)
or `openssl-devel` and `dbus-devel` (Fedora). On Windows use `--features vendored-openssl`,
which needs Perl and NASM.

To build the Debian package (binary, menu entry and icons):

```sh
cargo install cargo-deb
cargo deb --features vendored-openssl    # -> target/debian/idrac6_*.deb
```

## Usage

Start `idrac6`, click **Add iDRAC**, and enter the address, user and
password (the same ones you use for the iDRAC web page).

### The server list

Each iDRAC shows its power state, overall health (the coloured dot; hover
it for details), inlet temperature, power use and a small picture of its
screen, refreshed every 30 seconds (the picture every minute). Click the
picture to open the console; hover it to see it larger.

- **Power** turns the server on or off, resets it, power cycles it or
  asks its OS to shut down. Each one asks for confirmation.
- **More → Blink identify LED** makes the blue light on the server's front
  blink for a minute, to find it in the rack.
- **Options** turns the live status or the screen pictures off, for
  example on a slow link. idrac6-rs keeps one web session per iDRAC open
  for this and logs out when you quit.

### Health

Click **Health** for the server's details, every sensor with its limits,
power use over the last minute, hour, day and week with peaks and energy
used, and the System Event Log. The log can be filtered by severity,
searched, saved as CSV and cleared.

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
- **Keyboard → Type text…** types text on the server as keystrokes, for
  long passwords, recovery keys or commands. It opens with your clipboard
  text. Characters are typed as on a US keyboard; use the *Slow* speed for
  BIOS screens and boot loaders that drop keys.
- If a key seems stuck on the server, use **Keyboard → Release all keys**.
  Keys are released automatically when the window loses focus.
- **Power** has On, Off, Reset, Power Cycle and Graceful Shutdown. Each
  one asks for confirmation.
- **View** switches between fit-to-window and 1:1, and has full screen.
  **View → View only** stops all keyboard and mouse input, for watching a
  server without the risk of a stray key press. Tick *Open in view-only
  mode* in an iDRAC's settings to make that the default.
- **File → Save screenshot** saves a PNG; **File → Copy screenshot** puts
  it on the clipboard.
- **File → Start recording…** records the screen to a `.dvcrec` file until
  you stop it (click the red *REC* timer). Replay it with **Open
  recording…** in the main window or `idrac6 play <file>`. Recordings use
  the iDRAC's boot capture format, so long idle stretches play back in a
  second.
- **Tools → Mouse acceleration** should match the server's OS (Windows or
  Linux). A mismatch makes the server's pointer drift away from yours.
- **Tools → Single cursor** hides your pointer and moves the server's pointer
  directly, so there is only one pointer to follow. Click the screen to
  start; press **F9** to get your pointer back.
- **Tools → Bandwidth limit** helps on slow links; **Tools → Statistics**
  shows frame rate and bandwidth.
- If the server's screen is asleep ("No signal" / "Out of range"), point at
  the black area and press a key or click to wake it.
- If the connection drops (network trouble, the iDRAC rebooting or an
  idle timeout), the console reconnects by itself, waiting a little longer
  after each failed try. Turn this off under **Tools → Reconnect
  automatically**.
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

Without the GUI, for example from a script:

```sh
idrac6 mount "R710 lab" ubuntu.iso          # CD/DVD, read-only
idrac6 mount "R710 lab" disk.img --read-write
```

The image stays attached until you press Enter.

If you get "no free virtual drive", Virtual Media is probably set to
*Detached* on the iDRAC. Set it to *Auto-attach* under System → Console/Media
→ Configuration, or with `racadm config -g cfgRacVirtual -o
cfgVirMediaAttached 2`.

### Boot captures

iDRAC6 records the server's screen during its last three boots. Click
**Boot captures** to replay them, with play, pause, step, rewind, a seek
bar and playback speed. Handy for seeing a BIOS error or kernel panic that
scrolled past. From the command line: `idrac6 captures "R710 lab"`.
**Open…** in the player (or `idrac6 play <file>`) plays recording files.

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

When "Save password" is ticked, the password goes into the system
keyring: GNOME Keyring or KWallet on Linux (through the Secret Service),
Credential Manager on Windows. Passwords saved in `config.toml` by older
versions are moved there on the next start. Without a keyring service
(e.g. a minimal window manager), passwords are stored in plain text in
`config.toml`, which on Linux only your user can read. Untick "Save
password" to be asked each time instead. The proxy's certificate and
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
- [docs/video-dvc.md](docs/video-dvc.md): the Avocent DVC video codec iDRAC6 uses, and the boot capture file format
- [docs/video-codec.md](docs/video-codec.md): the ASPEED video codec (supported, but not used by iDRAC6)
- [docs/virtual-media.md](docs/virtual-media.md): Virtual Media protocol
- [docs/web-api.md](docs/web-api.md): the web interface's data API (status, sensors, power, event log)
- [docs/reverse-engineering.md](docs/reverse-engineering.md): how this was done and how to reproduce it

Code layout:

```text
src/
  main.rs        command line
  gui/           windows (winit + egui): manager, console, health, virtual media
  kvm/           console protocol: messages, session, keyboard map
  video/dvc.rs   DVC video decoder (what iDRAC6 sends)
  video/playback.rs  boot capture recordings
  video/aspeed.rs  ASPEED video decoder
  vmedia/        Virtual Media protocol and image serving
  web.rs         iDRAC web login and viewer.jnlp
  health.rs      status, sensors, power and event log from the web API
  proxy.rs       HTTPS -> TLS 1.0 web proxy
  tls.rs         legacy TLS settings
```

## Status

New and still being tested. Tested on an iDRAC6 in a PowerEdge R710: web
proxy, console video, keyboard, boot capture playback, and Virtual Media
(ISO read back byte for byte, writes to a disk image). The video and
playback decoders match the original Java viewer pixel for pixel on real
recordings. Single cursor mode was built from the reverse-engineered
protocol and needs more real-world testing, as do power control from the
server list and the identify LED (they send the same requests as the web
interface's buttons). Bug reports
with a `RUST_LOG=debug` log are very welcome (set `IDRAC6_CAPTURE=<file>`
to also record the video stream).

Not supported: iDRAC7 and newer (they use different, HTML5 consoles),
and physical CD drives and smart cards for Virtual Media.

## License

MIT. Not affiliated with or endorsed by Dell or Vertiv/Avocent.
