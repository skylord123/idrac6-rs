# idrac6-rs

A small native app that brings back remote control of Dell **iDRAC6**
servers (PowerEdge R610, R710, T610 and friends), without Java.

- **Virtual console**: see the server's screen, use keyboard and mouse,
  send Ctrl-Alt-Del and other macros, power control, screenshots.
- **Boot captures**: replay the iDRAC's recordings of the last boots.
- **Recording**: record a console session as an MP4 video or a raw
  recording, and export recordings and boot captures as MP4.
- **Virtual Media**: attach an ISO or disk image from your computer to the
  server as a USB CD/DVD or disk.
- **Web UI proxy**: open the iDRAC web interface in a modern browser. The
  proxy talks the iDRAC's TLS 1.0 for you.
- **Health**: sensors, power use, the System Event Log, and who is
  logged in to the iDRAC (with a button to close leftover sessions).
- **SSH and serial console**: open the iDRAC's command line, or the
  server's serial console (serial over LAN), in your terminal with one
  click.
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

- **Windows**: `idrac6-windows-x86_64-setup.exe` installs idrac6 with a
  Start menu entry (no admin rights needed; it installs for your user
  unless you choose all users). Or take `idrac6-windows-x86_64.zip`,
  unzip it anywhere and run `idrac6.exe`.

  For the command line (`export`, `proxy`, `mount`, `ssh` and the others
  below) use `idrac6-cli.exe`, which comes with both: `idrac6.exe` is a
  windowed program and can't print to a terminal. Write `idrac6-cli`
  wherever this README says `idrac6` on the command line. The installer
  can add it to your PATH.
- **Ubuntu / Debian**: `idrac6-amd64.deb`. Install it with
  `sudo apt install ./idrac6-amd64.deb`, then start *iDRAC6 Console* from
  your applications menu.
- **Fedora / openSUSE**: `idrac6-x86_64.rpm`. Install it with
  `sudo dnf install ./idrac6-x86_64.rpm` (or `sudo zypper install`).
- **Other Linux**: `idrac6-linux-x86_64.tar.gz`. Extract it and run
  `idrac6/idrac6`. Needs a desktop with OpenGL (X11 or Wayland).

`SHA256SUMS.txt` has the checksums of all files.

MP4 videos need [FFmpeg](https://ffmpeg.org/download.html), which is not
included: `sudo apt install ffmpeg` on Ubuntu/Debian, `winget install
Gyan.FFmpeg` on Windows (or put `ffmpeg.exe` next to `idrac6.exe`).
Everything else works without it, and idrac6-rs tells you when it's
missing.

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

The RPM is built the same way with `cargo install cargo-generate-rpm`,
`cargo build --release --features vendored-openssl` and `cargo
generate-rpm`. On Windows, `cargo build --release --features
vendored-openssl,console` builds the command-line program; copy it to
`target\release\idrac6-cli.exe` before building `idrac6.exe` (without
`console`). The Windows installer needs
[Inno Setup](https://jrsoftware.org/isinfo.php) 6.3 or newer: build the
release executable, then run `iscc /DAppVersion=X.Y.Z
packaging\windows\idrac6.iss` (it is written to `target\installer\`).

### Releases

Pushing a tag `vX.Y.Z` (matching the version in `Cargo.toml`) makes the
CI workflow test everything, build all the packages above on Linux and
Windows, check that the installer installs, and publish them on a GitHub
release with checksums. Every other push and pull request builds the same
packages as downloadable workflow artifacts.

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

The **Sessions** tab (also **More → Sessions…**) lists everyone logged
in to the iDRAC: web, virtual console, virtual media, SSH and RACADM,
with user and address. **Close** logs a session out, for example one left
by a browser that was closed without logging out or a viewer that
crashed. The iDRAC6 allows only a few sessions at a time.

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
- **File → Record video (MP4)…** records the screen as an MP4 video until
  you stop it (click the red *REC* timer). It is recorded as a `.dvcrec`
  first and turned into the MP4 when you stop, which takes a few seconds;
  if that fails, the `.dvcrec` is kept. Needs FFmpeg.
- **File → Record raw (.dvcrec)…** records the iDRAC's own screen data:
  small files that play in idrac6-rs (**Open recording…** in the main
  window or `idrac6 play <file>`), where long idle stretches play back in
  a second, and can be exported to MP4 later.
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

**Export MP4…** in the player saves the recording as an MP4 video, with
its real timing (the player shortens long pauses; the video doesn't).
From the command line: `idrac6 export recording.dvcrec [video.mp4]`.

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

### SSH and serial console

**More → SSH command line** opens a terminal logged in to the iDRAC's
command line (SM-CLP, with `racadm`). **More → Serial console** connects
straight to the server's serial port (COM2, serial over LAN); press
**Ctrl+\\** to end it. Both are also in the console's **Tools** menu.

The iDRAC6 only offers an old host key type (`ssh-rsa`) that current
OpenSSH turns off, so plain `ssh` fails. idrac6 runs your `ssh` with it
re-enabled, the saved user name and SSH port (set under **More → Edit**),
and the saved password. The iDRAC's host key is kept in
`ssh_known_hosts` in the settings directory. Logging in takes about 10
seconds.

The serial console only shows what the server sends to its serial port,
so a blank screen usually means the server isn't using it. Three things
need to be set up:

1. **iDRAC**: serial over LAN on (the default). Check with `racadm
   getconfig -g cfgIpmiSol` (`cfgIpmiSolEnable=1`) and `racadm getconfig
   -g cfgSerial` (`cfgSerialCom2RedirEnable=1`). Note the baud rate,
   usually 115200.
2. **BIOS** (F2 at boot → Serial Communication): *Serial Communication*
   "On with Console Redirection via COM2", *External Serial Connector*
   "Serial Device1", *Failsafe Baud Rate* 115200, *Remote Terminal Type*
   VT100/VT220, *Redirection After Boot* enabled. The BIOS screens and
   boot menu then also appear on the serial console.
3. **The OS**: a login prompt (getty) on the COM2 port, which Linux calls
   `ttyS1`, at the same baud rate. To also see boot messages, add
   `console=tty0 console=ttyS1,115200n8` to the kernel command line.

On **Unraid**, add the login prompt by appending these lines to
`/boot/config/go` (Unraid rebuilds `/etc` on every boot), and run them
once to start it right away:

```sh
grep -q '^[^#].*agetty.*ttyS1' /etc/inittab || echo 's1:12345:respawn:/sbin/agetty -L 115200 ttyS1 vt100' >> /etc/inittab
telinit q
```

For boot messages, add `console=tty0 console=ttyS1,115200n8` to the
`append` line under *Unraid OS* in Main → Flash → Syslinux Configuration,
and reboot.

The kernel log shows which port the BIOS redirects to: `dmesg | grep
SPCR` prints e.g. `console: uart,io,0x2f8,115200`; 0x2f8 is `ttyS1`,
0x3f8 is `ttyS0`. Or open the serial console and run
`echo hello > /dev/ttyS1` on the server; if nothing appears, try
`/dev/ttyS0`.

It needs OpenSSH's `ssh`: on Linux the `openssh-client` package, on
Windows the built-in "OpenSSH Client" optional feature. On Linux the
desktop's terminal is used (COSMIC Terminal, GNOME Console or Terminal,
Konsole, Xfce Terminal, and others); set another one under **Options →
Terminal for SSH**, e.g. `kitty` or `xterm -e`. On Windows it opens a
console window.

From a terminal:

```sh
idrac6 ssh myserver           # the iDRAC's command line
idrac6 ssh myserver --serial  # the serial console
```

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

Set `RUST_LOG=debug` for detailed logs, `IDRAC6_CONFIG_DIR` to use
another settings directory, `IDRAC6_FFMPEG` to use a specific FFmpeg
binary, or `IDRAC6_SSH` to use a specific `ssh`.

## Troubleshooting

- **"Login failed: too many web sessions"**: the iDRAC6 allows only a few
  web sessions. Close leftover ones under **More → Sessions…**, log out
  elsewhere, or wait a few minutes for old sessions to expire.
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
  video/mp4.rs   MP4 export through FFmpeg
  video/aspeed.rs  ASPEED video decoder
  vmedia/        Virtual Media protocol and image serving
  web.rs         iDRAC web login and viewer.jnlp
  health.rs      status, sensors, power and event log from the web API
  proxy.rs       HTTPS -> TLS 1.0 web proxy
  ssh.rs         SSH and serial console in a terminal
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
