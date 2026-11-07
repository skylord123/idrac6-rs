//! Saved iDRAC instances.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Instance {
    /// Display name.
    pub name: String,
    /// IP address or hostname of the iDRAC.
    pub host: String,
    /// HTTPS port of the iDRAC web server.
    pub https_port: u16,
    pub username: String,
    /// Kept in the system keyring when `save_password` is set (or, where
    /// there is none, in plain text in the config file).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub password: String,
    pub save_password: bool,
    /// Fixed local port for the web UI proxy. 0 picks a free port.
    pub proxy_port: u16,
    /// Open consoles with keyboard and mouse input turned off.
    pub view_only: bool,
    /// SSH port of the iDRAC.
    pub ssh_port: u16,
}

impl Default for Instance {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            https_port: 443,
            username: "root".into(),
            password: String::new(),
            save_password: true,
            proxy_port: 0,
            view_only: false,
            ssh_port: 22,
        }
    }
}

impl Instance {
    pub fn title(&self) -> &str {
        if self.name.is_empty() { &self.host } else { &self.name }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
    #[serde(rename = "instance")]
    pub instances: Vec<Instance>,
}

/// App-wide options.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Poll power state and sensor health for the manager window.
    pub live_status: bool,
    /// Also fetch a small picture of each server's screen.
    pub screen_previews: bool,
    /// Terminal program for SSH, with the option that runs a command
    /// (e.g. "kitty" or "xterm -e"). Empty: pick one.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub terminal: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { live_status: true, screen_previews: true, terminal: String::new() }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("IDRAC6_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    directories::ProjectDirs::from("", "", "idrac6-rs")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

impl Config {
    pub fn load() -> Config {
        let path = config_file();
        let mut c: Config = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                // Set the file aside, or the next save would replace it with
                // an empty config and lose every saved iDRAC.
                let bad = path.with_extension("toml.bad");
                match std::fs::rename(&path, &bad) {
                    Ok(()) => log::error!("could not read the config {}, moved it to {}: {e}", path.display(), bad.display()),
                    Err(re) => log::error!("could not read the config {}: {e} (and could not move it aside: {re})", path.display()),
                }
                Config::default()
            }),
            Err(_) => Config::default(),
        };
        let mut plain_text = false;
        for i in c.instances.iter_mut().filter(|i| i.save_password) {
            if i.password.is_empty() {
                if let Some(pw) = keyring::get(i) {
                    i.password = pw;
                }
            } else {
                plain_text = true;
            }
        }
        // Move passwords saved by older versions (or while no keyring was
        // available) into the keyring.
        if plain_text && keyring::available() {
            if let Err(e) = c.save() {
                log::warn!("could not move passwords to the keyring: {e:#}");
            }
        }
        c
    }

    pub fn save(&self) -> Result<()> {
        let mut c = self.clone();
        let mut keep = Vec::new();
        for i in &mut c.instances {
            if i.save_password && !i.password.is_empty() {
                match keyring::set(i, &i.password) {
                    Ok(()) => {
                        keep.push(keyring::account(i));
                        i.password.clear();
                    }
                    // No keyring: fall back to the (private) config file.
                    Err(e) => log::warn!("{e:#}; saving the password in {}", config_file().display()),
                }
            } else {
                i.password.clear();
            }
        }
        keyring::forget_others(&keep);
        let path = config_file();
        std::fs::create_dir_all(config_dir()).ok();
        let text = toml::to_string_pretty(&c)?;
        write_private(&path, text.as_bytes()).with_context(|| format!("writing {}", path.display()))
    }
}

/// Saved passwords in the system's credential store: Secret Service
/// (GNOME Keyring, KWallet) on Linux, Credential Manager on Windows,
/// Keychain on macOS.
mod keyring {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    use anyhow::{Context, Result};

    use super::Instance;

    const SERVICE: &str = "idrac6-rs";

    pub fn account(i: &Instance) -> String {
        format!("{}@{}:{}", i.username, i.host, i.https_port)
    }

    /// Accounts this run has read or written, so entries of removed or
    /// changed iDRACs can be deleted.
    fn known() -> &'static Mutex<HashSet<String>> {
        static K: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        K.get_or_init(Default::default)
    }

    fn entry(account: &str) -> Option<keyring::Entry> {
        keyring::Entry::new(SERVICE, account).map_err(|e| log::debug!("keyring: {e}")).ok()
    }

    /// Whether a keyring service is reachable.
    pub fn available() -> bool {
        static A: OnceLock<bool> = OnceLock::new();
        *A.get_or_init(|| {
            let ok = entry("availability-check").is_some_and(|e| matches!(e.get_password(), Ok(_) | Err(keyring::Error::NoEntry)));
            if !ok {
                log::info!("no system keyring available; saved passwords go in the config file");
            }
            ok
        })
    }

    pub fn get(i: &Instance) -> Option<String> {
        if !available() {
            return None;
        }
        let acc = account(i);
        match entry(&acc)?.get_password() {
            Ok(pw) => {
                known().lock().unwrap().insert(acc);
                Some(pw)
            }
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                log::warn!("reading the password for {acc} from the keyring: {e}");
                None
            }
        }
    }

    pub fn set(i: &Instance, password: &str) -> Result<()> {
        anyhow::ensure!(available(), "no system keyring");
        let acc = account(i);
        let e = entry(&acc).context("no system keyring")?;
        if e.get_password().ok().as_deref() != Some(password) {
            e.set_password(password).with_context(|| format!("storing the password for {acc} in the keyring"))?;
        }
        known().lock().unwrap().insert(acc);
        Ok(())
    }

    /// Delete the entries this run knows about except `keep`.
    pub fn forget_others(keep: &[String]) {
        let stale: Vec<String> = known().lock().unwrap().iter().filter(|a| !keep.contains(a)).cloned().collect();
        for acc in stale {
            if let Some(e) = entry(&acc) {
                match e.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => {}
                    Err(err) => log::warn!("removing {acc} from the keyring: {err}"),
                }
            }
            known().lock().unwrap().remove(&acc);
        }
    }
}

/// Where saved passwords go, for the settings dialog.
pub fn password_storage() -> &'static str {
    if keyring::available() { "in the system keyring" } else { "in plain text in the settings file" }
}

/// Write a file only the current user can read (it may hold passwords).
fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
