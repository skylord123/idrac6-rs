#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! idrac6: native virtual console, virtual media and web UI proxy for
//! Dell iDRAC6.

mod config;
mod gui;
mod health;
mod kvm;
mod mount;
mod proxy;
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
    idrac6 mount <name|host> <image> [--read-write]
                                Attach an .iso (CD/DVD) or .img (disk) as
                                virtual media until Enter is pressed
    idrac6 proxy <host> [--port <local-port>] [--https-port <port>]
                                Run only the web UI proxy, without a GUI
    idrac6 --help | --version

ENVIRONMENT:
    RUST_LOG=debug              More logging (also: info, trace)
    IDRAC6_CONFIG_DIR=<dir>     Use a different settings directory
";

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,winit=warn,egui_glow=warn,glutin=warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
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

/// Look up a saved iDRAC by name or address; it must have a saved password.
fn find_instance(want: &str) -> anyhow::Result<config::Instance> {
    let inst = config::Config::load()
        .instances
        .into_iter()
        .find(|i| i.name.eq_ignore_ascii_case(want) || i.host == want)
        .ok_or_else(|| anyhow::anyhow!("no saved iDRAC called '{want}'"))?;
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
