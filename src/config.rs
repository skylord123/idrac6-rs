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
    /// Stored in plain text in the config file when `save_password` is set.
    pub password: String,
    pub save_password: bool,
    /// Fixed local port for the web UI proxy. 0 picks a free port.
    pub proxy_port: u16,
    /// Open consoles with keyboard and mouse input turned off.
    pub view_only: bool,
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
    #[serde(rename = "instance")]
    pub instances: Vec<Instance>,
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
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                log::error!("ignoring unreadable config {}: {e}", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let mut c = self.clone();
        for i in &mut c.instances {
            if !i.save_password {
                i.password.clear();
            }
        }
        let path = config_file();
        std::fs::create_dir_all(config_dir()).ok();
        let text = toml::to_string_pretty(&c)?;
        write_private(&path, text.as_bytes()).with_context(|| format!("writing {}", path.display()))
    }
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
