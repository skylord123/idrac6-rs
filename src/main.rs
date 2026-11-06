// Windows release builds are windowed programs without a console, unless
// built with the `console` feature (idrac6-cli.exe).
#![cfg_attr(all(windows, not(debug_assertions), not(feature = "console")), windows_subsystem = "windows")]
//! idrac6: native virtual console, virtual media and web UI proxy for
//! Dell iDRAC6.

mod config;
mod gui;
mod health;
mod kvm;
mod mount;
mod proxy;
mod ssh;
mod tls;
mod video;
mod vmedia;
mod web;

const USAGE: &str = "\
idrac6 - virtual console and web UI proxy for Dell iDRAC6

USAGE:
    idrac6                      Open the console manager (GUI)
    idrac6 console <name|host> Open the console of a saved iDRAC directly
    idrac6 captures <name|host> Replay the boot recordings of a saved iDRAC
    idrac6 play <file>          Replay a console recording (.dvcrec) or a
                                boot capture file
    idrac6 export <file> [<out.mp4>]
                                Convert a recording to an MP4 video (needs
                                FFmpeg)
    idrac6 mount <name|host> <image> [--read-write]
                                Attach an .iso (CD/DVD) or .img (disk) as
                                virtual media until Enter is pressed
    idrac6 ssh <name|host> [--serial]
                                SSH to a saved iDRAC's command line, or with
                                --serial to the server's serial console
                                (Ctrl+\\ ends it)
    idrac6 proxy <host> [--port <local-port>] [--https-port <port>]
                                Run only the web UI proxy, without a GUI
    idrac6 --help | --version

ENVIRONMENT:
    RUST_LOG=debug              More logging (also: info, trace)
    IDRAC6_FFMPEG=<path>        FFmpeg to use for MP4 videos
    IDRAC6_SSH=<path>           ssh to use
    IDRAC6_CONFIG_DIR=<dir>     Use a different settings directory

On Windows, run these as idrac6-cli (idrac6-cli.exe); idrac6.exe has no
console.
";

fn main() {
    // Running as ssh's password helper?
    ssh::askpass_hook();
    // Set OpenSSL up before anything else touches it, without its exit-time
    // cleanup (background threads may still be using TLS when we exit).
    openssl::init();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,winit=warn,egui_glow=warn,glutin=warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if windowed_cli(args.first().map(String::as_str)) {
        return;
    }
    let r = match args.first().map(String::as_str) {
        None => gui::run(Box::new(gui::ManagerWindow::new())),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("idrac6 {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("proxy") => run_proxy(&args[1..]),
        Some(cmd @ ("console" | "captures")) => {
            let want = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: idrac6 {cmd} <name|host>"));
            want.and_then(|want| {
                let inst = find_instance(want)?;
                if cmd == "console" {
                    gui::run(Box::new(gui::ConsoleWindow::new(inst)))
                } else {
                    gui::run(Box::new(gui::PlaybackWindow::new(inst)))
                }
            })
        }
        Some("play") => match args.get(1) {
            Some(f) => gui::run(Box::new(gui::PlaybackWindow::from_file(f.into()))),
            None => Err(anyhow::anyhow!("usage: idrac6 play <file>")),
        },
        Some("export") => match (args.get(1), args.get(2)) {
            (Some(f), out) => export(f.as_ref(), out.map(Into::into)),
            _ => Err(anyhow::anyhow!("usage: idrac6 export <file> [<out.mp4>]")),
        },
        Some("ssh") => (|| {
            let serial = args.iter().any(|a| a == "--serial");
            let wait = args.iter().any(|a| a == "--wait-on-error");
            let Some(want) = args[1..].iter().find(|a| !a.starts_with("--")) else {
                anyhow::bail!("usage: idrac6 ssh <name|host> [--serial]");
            };
            let inst = find_saved(want)?;
            let code = ssh::run(&inst, if serial { ssh::Mode::Serial } else { ssh::Mode::Shell }, wait)?;
            std::process::exit(code)
        })(),
        Some("mount") => (|| {
            let (Some(want), Some(image)) = (args.get(1), args.get(2)) else {
                anyhow::bail!("usage: idrac6 mount <name|host> <image> [--read-write]");
            };
            let inst = find_instance(want)?;
            mount::run(inst, image.into(), None, args.iter().any(|a| a == "--read-write"))
        })(),
        Some(other) => Err(anyhow::anyhow!("unknown command '{other}'\n\n{USAGE}")),
    };
    if let Err(e) = r {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// The windowed Windows build can't print or read the terminal, so
/// commands that need it point to idrac6-cli.exe. Returns true when it
/// did.
fn windowed_cli(cmd: Option<&str>) -> bool {
    let windowed = cfg!(all(windows, not(debug_assertions), not(feature = "console")));
    let terminal_only = matches!(cmd, Some("-h" | "--help" | "help" | "-V" | "--version" | "proxy" | "export" | "mount" | "ssh"));
    if !(windowed && terminal_only) {
        return false;
    }
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title("idrac6")
        .set_description("idrac6.exe has no console. For command-line use run idrac6-cli.exe, which is installed next to it, for example:\n\nidrac6-cli --help")
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    true
}

/// `idrac6 export`: recording file to MP4.
fn export(file: &std::path::Path, out: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    use std::io::Write;
    let out = out.unwrap_or_else(|| file.with_extension("mp4"));
    if out.exists() {
        anyhow::bail!("{} already exists", out.display());
    }
    let ff = video::mp4::find().map_err(|e| anyhow::anyhow!("{e}. Install FFmpeg (https://ffmpeg.org/download.html) or set IDRAC6_FFMPEG."))?;
    let data = std::fs::read(file).map_err(|e| anyhow::anyhow!("could not read {}: {e}", file.display()))?;
    let rec = video::playback::parse(&data)?;
    let done = video::mp4::encode(&ff, &rec, &out, &mut |p| {
        eprint!("\rWriting {}: {:3.0}%", out.display(), p * 100.0);
        let _ = std::io::stderr().flush();
        true
    })?;
    eprintln!();
    let s = done.duration.as_secs();
    println!("Saved {} ({}:{:02}, {:.1} MB)", done.path.display(), s / 60, s % 60, done.bytes as f64 / 1e6);
    Ok(())
}

/// Look up a saved iDRAC by name or address.
fn find_saved(want: &str) -> anyhow::Result<config::Instance> {
    config::Config::load()
        .instances
        .into_iter()
        .find(|i| i.name.eq_ignore_ascii_case(want) || i.host == want)
        .ok_or_else(|| anyhow::anyhow!("no saved iDRAC called '{want}'"))
}

/// Look up a saved iDRAC by name or address; it must have a saved password.
fn find_instance(want: &str) -> anyhow::Result<config::Instance> {
    let inst = find_saved(want)?;
    if inst.password.is_empty() {
        anyhow::bail!("'{want}' has no saved password; open it from the manager instead");
    }
    Ok(inst)
}

fn run_proxy(args: &[String]) -> anyhow::Result<()> {
    let mut host = None;
    let mut local_port = 8443u16;
    let mut https_port = 443u16;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => local_port = it.next().and_then(|v| v.parse().ok()).ok_or_else(|| anyhow::anyhow!("--port needs a number"))?,
            "--https-port" => https_port = it.next().and_then(|v| v.parse().ok()).ok_or_else(|| anyhow::anyhow!("--https-port needs a number"))?,
            h if host.is_none() && !h.starts_with('-') => host = Some(h.to_string()),
            other => anyhow::bail!("unexpected argument '{other}'"),
        }
    }
    let host = host.ok_or_else(|| anyhow::anyhow!("missing iDRAC host\n\n{USAGE}"))?;
    let p = proxy::WebProxy::start(&host, https_port, local_port, &config::config_dir())?;
    println!("Proxying {} -> {}", p.url(), p.target);
    println!("Open the URL in your browser and accept the self-signed certificate. Ctrl+C to stop.");
    loop {
        std::thread::park();
    }
}
