//! Background status polling for the manager window: power state, sensor
//! health, power draw and a console preview picture for each iDRAC, plus
//! the power and identify actions. One thread and one reused web login per
//! iDRAC.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::health::{self, Status};
use crate::kvm::proto::PowerAction;
use crate::web::{LoginRefused, WebClient};

/// How often status and previews are refreshed while live status is on.
pub const STATUS_EVERY: Duration = Duration::from_secs(30);
pub const PREVIEW_EVERY: Duration = Duration::from_secs(60);
/// Log out after this long without requests when live status is off.
const IDLE_LOGOUT: Duration = Duration::from_secs(120);
/// Wait this long after the iDRAC said it has too many sessions.
const BUSY_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct MonitorState {
    pub status: Option<Status>,
    pub status_at: Option<Instant>,
    /// Why the last status request failed.
    pub error: Option<String>,
    /// Polling stopped until the settings change (wrong password).
    pub paused: Option<String>,
    pub preview: Option<image::RgbImage>,
    /// Bumped whenever `preview` changes.
    pub preview_seq: u64,
    pub preview_at: Option<Instant>,
    /// A request is under way.
    pub busy: bool,
    /// Result of the last power or identify action, for the user.
    pub action_result: Option<(String, bool)>,
}

enum Cmd {
    Refresh,
    Power(PowerAction),
    Identify(u8),
    Live { status: bool, previews: bool },
    Password(String),
    Stop,
}

pub struct Monitor {
    pub client: Arc<WebClient>,
    pub state: Arc<Mutex<MonitorState>>,
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(client: Arc<WebClient>, live: bool, previews: bool, notify: Arc<dyn Fn() + Send + Sync>) -> Monitor {
        let state = Arc::new(Mutex::new(MonitorState::default()));
        let (tx, rx) = mpsc::channel();
        let (c, st) = (client.clone(), state.clone());
        let thread = thread::Builder::new()
            .name(format!("monitor-{}", client.host))
            .spawn(move || Worker { client: c, state: st, notify, live, previews, next_status: Instant::now(), next_preview: Instant::now() }.run(rx))
            .expect("spawn monitor");
        Monitor { client, state, tx, thread: Some(thread) }
    }

    pub fn refresh(&self) {
        let _ = self.tx.send(Cmd::Refresh);
    }

    pub fn power(&self, a: PowerAction) {
        let _ = self.tx.send(Cmd::Power(a));
    }

    pub fn identify(&self, seconds: u8) {
        let _ = self.tx.send(Cmd::Identify(seconds));
    }

    pub fn set_live(&self, status: bool, previews: bool) {
        let _ = self.tx.send(Cmd::Live { status, previews });
    }

    pub fn set_password(&self, pw: &str) {
        let _ = self.tx.send(Cmd::Password(pw.to_string()));
    }

    /// Stop polling and log out. Returns the thread to wait for.
    pub fn stop(mut self) -> Option<JoinHandle<()>> {
        let _ = self.tx.send(Cmd::Stop);
        self.thread.take()
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

struct Worker {
    client: Arc<WebClient>,
    state: Arc<Mutex<MonitorState>>,
    notify: Arc<dyn Fn() + Send + Sync>,
    live: bool,
    previews: bool,
    next_status: Instant,
    next_preview: Instant,
}

impl Worker {
    fn update(&self, f: impl FnOnce(&mut MonitorState)) {
        f(&mut self.state.lock().unwrap());
        (self.notify)();
    }

    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        loop {
            let paused = self.state.lock().unwrap().paused.is_some() || !self.client.has_password();
            let next = if self.live && !paused {
                let mut n = self.next_status;
                if self.previews {
                    n = n.min(self.next_preview);
                }
                n.saturating_duration_since(Instant::now())
            } else {
                Duration::from_secs(10)
            };
            match rx.recv_timeout(next) {
                Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Cmd::Refresh) => {
                    self.update(|s| s.paused = None);
                    self.next_status = Instant::now();
                    self.next_preview = Instant::now();
                    self.poll(true);
                }
                Ok(Cmd::Power(a)) => self.action(&format!("{}", a.label()), |c| health::set_power(c, a)),
                Ok(Cmd::Identify(secs)) => {
                    let what = if secs == 0 { "Stop blinking the identify LED".to_string() } else { format!("Blink the identify LED for {secs} s") };
                    self.action(&what, |c| health::identify(c, secs))
                }
                Ok(Cmd::Live { status, previews }) => {
                    let start = (status && !self.live) || (previews && !self.previews);
                    self.live = status;
                    self.previews = previews;
                    if !previews {
                        self.update(|s| {
                            s.preview = None;
                            s.preview_seq += 1;
                        });
                    }
                    if start {
                        self.next_status = Instant::now();
                        self.next_preview = Instant::now();
                    }
                }
                Ok(Cmd::Password(pw)) => {
                    self.client.set_password(&pw);
                    self.update(|s| s.paused = None);
                    self.next_status = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.live && !paused {
                        self.poll(false);
                    } else if self.client.logged_in() && self.client.idle_for() > IDLE_LOGOUT {
                        self.client.logout();
                    }
                }
            }
        }
        self.client.logout();
    }

    /// Fetch whatever is due (everything when `force`).
    fn poll(&mut self, force: bool) {
        if !self.client.has_password() {
            return;
        }
        let now = Instant::now();
        if force || now >= self.next_status {
            self.next_status = now + STATUS_EVERY;
            self.update(|s| s.busy = true);
            match health::fetch_status(&self.client) {
                Ok(st) => self.update(|s| {
                    s.status = Some(st);
                    s.status_at = Some(Instant::now());
                    s.error = None;
                    s.busy = false;
                }),
                Err(e) => {
                    self.failed(e);
                    return;
                }
            }
        }
        if self.previews && (force || now >= self.next_preview) {
            self.next_preview = now + PREVIEW_EVERY;
            self.update(|s| s.busy = true);
            match health::fetch_preview(&self.client) {
                Ok(img) => self.update(|s| {
                    s.preview = Some(img);
                    s.preview_seq += 1;
                    s.preview_at = Some(Instant::now());
                    s.busy = false;
                }),
                Err(e) => {
                    log::debug!("console preview of {}: {e:#}", self.client.host);
                    self.update(|s| s.busy = false);
                }
            }
        }
    }

    fn failed(&mut self, e: anyhow::Error) {
        log::info!("status of {}: {e:#}", self.client.host);
        let refused = e.downcast_ref::<LoginRefused>().copied();
        let msg = match refused {
            Some(r) => r.to_string(),
            None => format!("Unreachable: {e:#}"),
        };
        match refused {
            // Retrying a wrong password could get the address blocked.
            Some(LoginRefused::BadCredentials | LoginRefused::NotAllowed) => self.update(|s| {
                s.paused = Some(msg.clone());
                s.error = Some(msg);
                s.busy = false;
            }),
            Some(LoginRefused::TooManySessions) => {
                self.next_status = Instant::now() + BUSY_BACKOFF;
                self.next_preview = self.next_status;
                self.update(|s| {
                    s.error = Some(msg);
                    s.busy = false;
                });
            }
            None => self.update(|s| {
                s.error = Some(msg);
                s.busy = false;
            }),
        }
    }

    fn action(&mut self, what: &str, f: impl FnOnce(&WebClient) -> anyhow::Result<()>) {
        self.update(|s| {
            s.busy = true;
            s.action_result = None;
        });
        let r = f(&self.client);
        let result = match &r {
            Ok(()) => (format!("{what}: sent to {}.", self.client.host), true),
            Err(e) => (format!("{what} failed: {e:#}"), false),
        };
        self.update(|s| {
            s.action_result = Some(result);
            s.busy = false;
        });
        // Show the new power state soon.
        self.next_status = Instant::now() + Duration::from_secs(5);
    }
}
