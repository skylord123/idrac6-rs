//! SSH to the iDRAC, in the user's own terminal.
//!
//! The iDRAC6 only offers the `ssh-rsa` host key type, which OpenSSH no
//! longer allows by default, so plain `ssh root@idrac` fails. `idrac6 ssh`
//! runs OpenSSH with that re-enabled, the saved user name and port, and
//! (through `SSH_ASKPASS`) the saved password. The GUI opens a terminal
//! window running it.
//!
//! The iDRAC's SSH shell is SM-CLP (`/admin1->`), with `racadm` commands
//! and `console com2` for the server's serial console (serial over LAN),
//! which `--serial` starts directly. Ctrl+\ ends the serial console.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{self, Instance};

/// Set (to the iDRAC's name or address) when idrac6 runs as ssh's
/// password helper.
const ASKPASS_ENV: &str = "IDRAC6_ASKPASS_FOR";

/// Options the iDRAC6 needs on top of OpenSSH's defaults. Its firmware
/// offers only `ssh-rsa` host keys; key exchange and ciphers are fine with
/// the defaults, but older firmware may only have the SHA-1 group 14 key
/// exchange. Both are still supported by current OpenSSH (only turned
/// off), so naming them never makes ssh refuse to start.
const LEGACY: [&str; 2] = ["HostKeyAlgorithms=+ssh-rsa", "KexAlgorithms=+diffie-hellman-group14-sha1"];

/// What the user gets: the SM-CLP shell, or the serial console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Shell,
    Serial,
}

/// Where to find OpenSSH's client.
pub fn find_ssh() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("IDRAC6_SSH") {
        return Some(p.into());
    }
    let name = if cfg!(windows) { "ssh.exe" } else { "ssh" };
    #[cfg(windows)]
    {
        // Windows' own OpenSSH client (an optional feature, on by default).
        if let Some(root) = std::env::var_os("SystemRoot") {
            let p = PathBuf::from(root).join("System32").join("OpenSSH").join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    which(name)
}

/// Find `name` on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// How to install OpenSSH's client on this system.
pub fn install_hint() -> &'static str {
    if cfg!(windows) {
        "Install the OpenSSH Client: Settings → System → Optional features → Add a feature → OpenSSH Client."
    } else {
        "Install the OpenSSH client, e.g. sudo apt install openssh-client."
    }
}

/// The ssh command line for `inst`.
fn ssh_args(inst: &Instance, mode: Mode) -> Vec<String> {
    let known_hosts = config::config_dir().join("ssh_known_hosts");
    let mut a = Vec::new();
    for o in LEGACY {
        a.extend(["-o".to_string(), o.to_string()]);
    }
    a.extend([
        // The iDRAC's own host keys, kept apart from the user's.
        "-o".into(),
        // Quoted: the option takes a list separated by spaces, and the
        // settings directory can contain them.
        format!("UserKnownHostsFile=\"{}\"", known_hosts.display()),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        // Keys from the user's agent would only use up login attempts.
        "-o".into(),
        "PubkeyAuthentication=no".into(),
        "-o".into(),
        "PreferredAuthentications=password,keyboard-interactive".into(),
        // A wrong saved password is tried once, not until the iDRAC
        // blocks this address (5 failures a minute by default).
        "-o".into(),
        "NumberOfPasswordPrompts=1".into(),
        "-p".into(),
        inst.ssh_port.to_string(),
        "-l".into(),
        inst.username.clone(),
    ]);
    if mode == Mode::Serial {
        a.push("-t".into());
    }
    // A host starting with "-" is not an option.
    a.push("--".into());
    a.push(inst.host.clone());
    if mode == Mode::Serial {
        a.extend(["console".into(), "com2".into()]);
    }
    a
}

/// The ssh process for `inst`, with the saved password (if there is one)
/// given through idrac6 as `SSH_ASKPASS`.
fn command(inst: &Instance, mode: Mode) -> Result<Command> {
    let Some(ssh) = find_ssh() else {
        bail!("ssh was not found. {}", install_hint());
    };
    std::fs::create_dir_all(config::config_dir()).ok();
    let mut cmd = Command::new(ssh);
    cmd.args(ssh_args(inst, mode));
    if saved_password(inst).is_some() {
        let exe = std::env::current_exe().context("finding idrac6's own path")?;
        cmd.env("SSH_ASKPASS", exe).env("SSH_ASKPASS_REQUIRE", "force").env(ASKPASS_ENV, lookup_key(inst));
    }
    Ok(cmd)
}

fn title(inst: &Instance, mode: Mode) -> String {
    match mode {
        Mode::Shell => format!("SSH: {}", inst.title()),
        Mode::Serial => format!("Serial console: {}", inst.title()),
    }
}

/// `idrac6 ssh`: connect in this terminal. Returns ssh's exit code.
pub fn run(inst: &Instance, mode: Mode, wait_on_error: bool) -> Result<i32> {
    let mut cmd = command(inst, mode)?;
    // Window title.
    print!("\x1b]0;{}\x07", title(inst, mode));
    println!("Connecting to {} ({}). The iDRAC takes about 10 seconds to log in.", inst.title(), inst.host);
    match mode {
        Mode::Shell => println!("Type help for the iDRAC's commands, exit to leave.\n"),
        Mode::Serial => {
            println!("Serial console (COM2). Press Ctrl+\\ to end it.");
            println!("Blank? Press Enter for a login prompt. If still nothing shows, the server's OS isn't using its");
            println!("serial port: it needs a getty on the COM2 port (usually ttyS1) at 115200 baud. See the README.\n");
        }
    }
    let status = cmd.status().context("running ssh")?;
    let code = status.code().unwrap_or(1);
    // 255 is ssh's own failure (refused, wrong password...); keep the
    // window open so the message can be read.
    if wait_on_error && code == 255 {
        println!("\nThe connection failed. Press Enter to close this window.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    Ok(code)
}

/// What the password helper (and a terminal opened from the manager)
/// uses to find the iDRAC again.
pub fn lookup_key(inst: &Instance) -> String {
    format!("{}@{}:{}", inst.username, inst.host, inst.https_port)
}

/// The password saved for the iDRAC with `key`.
fn saved_by_key(key: &str) -> Option<Instance> {
    config::Config::load().instances.into_iter().find(|i| lookup_key(i) == key && !i.password.is_empty())
}

/// The password saved for `inst` (not one only typed in this run, which
/// the helper process could not get).
fn saved_password(inst: &Instance) -> Option<String> {
    saved_by_key(&lookup_key(inst)).map(|i| i.password)
}

/// Whether ssh's `prompt` asks for the password of `inst` itself, e.g.
/// "root@192.168.1.120's password: ", and not of a jump host or for a
/// host key confirmation.
fn is_password_prompt_for(prompt: &str, inst: &Instance) -> bool {
    let p = prompt.to_ascii_lowercase();
    p.contains("password") && p.contains(&format!("{}@{}", inst.username, inst.host).to_ascii_lowercase())
}

/// Called first in `main`: when ssh runs idrac6 as its password helper,
/// answer and exit.
pub fn askpass_hook() {
    let Some(key) = std::env::var_os(ASKPASS_ENV) else { return };
    let prompt = std::env::args().nth(1).unwrap_or_default();
    match saved_by_key(&key.to_string_lossy()) {
        Some(inst) if is_password_prompt_for(&prompt, &inst) => {
            println!("{}", inst.password);
            std::process::exit(0);
        }
        _ => std::process::exit(1),
    }
}

/// Open a terminal window running `idrac6 ssh` for `inst`.
#[cfg(not(windows))]
pub fn open_terminal(inst: &Instance, mode: Mode, custom: &str) -> Result<()> {
    if find_ssh().is_none() {
        bail!("ssh was not found. {}", install_hint());
    }
    let exe = std::env::current_exe().context("finding idrac6's own path")?;
    let mut args = vec!["ssh".to_string(), "--wait-on-error".into()];
    if mode == Mode::Serial {
        args.push("--serial".into());
    }
    args.push(selector(inst));
    terminal::open(&exe, &args, custom)
}

/// Open a console window running ssh for `inst`. (idrac6.exe is a GUI
/// program without a console of its own, so `cmd` runs ssh directly.)
#[cfg(windows)]
pub fn open_terminal(inst: &Instance, mode: Mode, _custom: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x10;
    // cmd.exe would expand or end quoting at these.
    for v in [&inst.host, &inst.username] {
        if v.contains(['"', '%', '\r', '\n']) {
            bail!("the address and user name can't contain \" or %");
        }
    }
    let ssh = command(inst, mode)?;
    let quote = |a: &std::ffi::OsStr| win_quote(&a.to_string_lossy());
    let line = std::iter::once(quote(ssh.get_program())).chain(ssh.get_args().map(quote)).collect::<Vec<_>>().join(" ");
    let title: String = title(inst, mode).chars().filter(|c| c.is_alphanumeric() || " .:-_".contains(*c)).collect();
    let note = match mode {
        Mode::Shell => "Connecting. The iDRAC takes about 10 seconds to log in.",
        Mode::Serial => "Connecting to the serial console (COM2). Press Ctrl+\\ to end it. Blank? Press Enter; the server's OS must run a login prompt on its COM2 port (usually ttyS1) at 115200 baud.",
    };
    let mut cmd = Command::new("cmd.exe");
    cmd.raw_arg(format!("/c \"title {title} & echo {note} & {line} || pause\""));
    for (k, v) in ssh.get_envs() {
        if let Some(v) = v {
            cmd.env(k, v);
        }
    }
    cmd.creation_flags(CREATE_NEW_CONSOLE);
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("could not open a console window: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Quote one argument for a Windows command line, the way the C runtime
/// splits it again: quotes inside are escaped, and so are backslashes in
/// front of them.
#[cfg_attr(not(windows), allow(dead_code))]
fn win_quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"', '&', '|', '^', '(', ')', '<', '>']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// The argument `idrac6 ssh` finds `inst` by.
#[cfg_attr(windows, allow(dead_code))]
fn selector(inst: &Instance) -> String {
    // Names need not be unique; this picks out exactly this entry.
    lookup_key(inst)
}

mod terminal {
    use std::path::Path;
    use std::process::Command;

    use anyhow::{Result, bail};

    use super::*;

    /// Run `exe args` in a new terminal window.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn open(exe: &Path, args: &[String], custom: &str) -> Result<()> {
        let custom = custom.trim();
        if !custom.is_empty() {
            let mut parts = split(custom);
            let prog = parts.remove(0);
            return spawn(Command::new(&prog).args(parts).arg(exe).args(args), &prog);
        }
        platform(exe, args)
    }

    fn spawn(cmd: &mut Command, what: &str) -> Result<()> {
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("could not start {what}: {e}"))?;
        // Reap it in the background; terminals usually hand the window to
        // a server process and exit at once.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    /// Split a command line on spaces, keeping "quoted parts" together.
    pub(super) fn split(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quote = None;
        let mut any = false;
        for c in s.chars() {
            match (quote, c) {
                (None, '"' | '\'') => {
                    quote = Some(c);
                    any = true;
                }
                (Some(q), c) if c == q => quote = None,
                (None, c) if c.is_whitespace() => {
                    if any {
                        out.push(std::mem::take(&mut cur));
                        any = false;
                    }
                }
                (_, c) => {
                    cur.push(c);
                    any = true;
                }
            }
        }
        if any {
            out.push(cur);
        }
        out
    }

    #[cfg(windows)]
    fn platform(_exe: &Path, _args: &[String]) -> Result<()> {
        bail!("not used on Windows")
    }

    #[cfg(target_os = "macos")]
    fn platform(exe: &Path, args: &[String]) -> Result<()> {
        let line = std::iter::once(exe.display().to_string()).chain(args.iter().cloned()).map(|a| format!("'{}'", a.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
        let script = format!("tell application \"Terminal\" to do script \"{}\"", line.replace('\\', "\\\\").replace('"', "\\\""));
        spawn(Command::new("osascript").args(["-e", &script, "-e", "tell application \"Terminal\" to activate"]), "Terminal")
    }

    /// Terminal emulators, preferred first, and how each takes a command.
    #[cfg(all(unix, not(target_os = "macos")))]
    const TERMINALS: &[(&str, &[&str])] = &[
        ("cosmic-term", &["--command"]),
        ("ptyxis", &["--new-window", "--"]),
        ("kgx", &["--"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("xfce4-terminal", &["-x"]),
        ("mate-terminal", &["-x"]),
        ("terminator", &["-x"]),
        ("tilix", &["-e"]),
        ("wezterm", &["start", "--"]),
        ("kitty", &[]),
        ("alacritty", &["-e"]),
        ("foot", &[]),
        ("x-terminal-emulator", &["-e"]),
        ("xterm", &["-e"]),
    ];

    #[cfg(all(unix, not(target_os = "macos")))]
    fn platform(exe: &Path, args: &[String]) -> Result<()> {
        // The desktop's own terminal first.
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_uppercase();
        let own: &[&str] = if desktop.contains("COSMIC") {
            &["cosmic-term"]
        } else if desktop.contains("GNOME") || desktop.contains("UNITY") {
            &["ptyxis", "kgx", "gnome-terminal"]
        } else if desktop.contains("KDE") {
            &["konsole"]
        } else if desktop.contains("XFCE") {
            &["xfce4-terminal"]
        } else if desktop.contains("MATE") {
            &["mate-terminal"]
        } else {
            &[]
        };
        let order = own.iter().filter_map(|n| TERMINALS.iter().find(|(t, _)| t == n)).chain(TERMINALS.iter());
        for (name, pre) in order {
            if which(name).is_some() {
                return spawn(Command::new(name).args(*pre).arg(exe).args(args), name);
            }
        }
        bail!("no terminal program was found. Set one under Options → Terminal, e.g. \"xterm -e\".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_shell_and_serial() {
        let inst = Instance { host: "10.0.0.5".into(), username: "root".into(), ssh_port: 2222, ..Default::default() };
        let a = ssh_args(&inst, Mode::Shell);
        assert!(a.windows(2).any(|w| w == ["-o", "HostKeyAlgorithms=+ssh-rsa"]));
        assert!(a.windows(2).any(|w| w == ["-p", "2222"]));
        assert_eq!(&a[a.len() - 2..], ["--", "10.0.0.5"]);
        assert!(a.iter().any(|o| o == "NumberOfPasswordPrompts=1"));
        assert!(a.iter().any(|o| o.starts_with("UserKnownHostsFile=\"") && o.ends_with('"')));
        assert!(!a.contains(&"-t".to_string()));
        let s = ssh_args(&inst, Mode::Serial);
        assert_eq!(&s[s.len() - 5..], ["-t", "--", "10.0.0.5", "console", "com2"]);
    }

    #[test]
    fn answers_only_the_idracs_prompt() {
        let inst = Instance { host: "10.0.0.5".into(), username: "root".into(), ..Default::default() };
        assert!(is_password_prompt_for("root@10.0.0.5's password: ", &inst));
        assert!(!is_password_prompt_for("me@bastion.example's password: ", &inst));
        assert!(!is_password_prompt_for("Are you sure you want to continue connecting (yes/no)?", &inst));
        assert!(!is_password_prompt_for("Enter passphrase for key: root@10.0.0.5", &inst));
    }

    #[test]
    fn quotes_windows_arguments() {
        assert_eq!(win_quote("-p"), "-p");
        assert_eq!(win_quote(""), "\"\"");
        assert_eq!(win_quote(r"C:\Program Files\ssh.exe"), r#""C:\Program Files\ssh.exe""#);
        assert_eq!(
            win_quote(r#"UserKnownHostsFile="C:\Users\A B\known""#),
            r#""UserKnownHostsFile=\"C:\Users\A B\known\"""#
        );
        assert_eq!(win_quote(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(win_quote(r"dir with space\"), r#""dir with space\\""#);
    }

    #[test]
    fn splits_terminal_command() {
        assert_eq!(terminal::split("kitty -e"), ["kitty", "-e"]);
        assert_eq!(terminal::split(" \"/opt/My Term/term\"  --exec "), ["/opt/My Term/term", "--exec"]);
        assert_eq!(terminal::split("a '' b"), ["a", "", "b"]);
    }
}
