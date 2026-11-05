//! Local HTTPS proxy for the iDRAC6 web interface.
//!
//! Browsers no longer speak TLS 1.0, so we accept a modern TLS connection on
//! `127.0.0.1` and forward the raw bytes over a legacy TLS 1.0 connection to
//! the iDRAC. No HTTP is parsed: the iDRAC builds its redirects from the
//! `Host` header, which the browser sets to `localhost:<port>`, so links keep
//! pointing at the proxy.
//!
//! The local side uses a self-signed certificate that is generated once and
//! stored in the config directory, so the browser only asks to trust it
//! once.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::ssl::{SslAcceptor, SslMethod, SslStream};
use openssl::x509::extension::{BasicConstraints, ExtendedKeyUsage, SubjectAlternativeName};
use openssl::x509::{X509, X509NameBuilder};

/// The iDRAC6 web server (Appweb 2.4) stalls for minutes when it gets more
/// than a handful of simultaneous TLS connections, and browsers open six at
/// once. Connections beyond this limit wait for a free slot.
const MAX_UPSTREAM: usize = 2;
/// A keep-alive connection with no traffic for this long after the iDRAC
/// last answered gives up its slot when another connection is waiting.
const YIELD_AFTER: Duration = Duration::from_secs(2);
/// Close connections that have been idle this long in any case.
const MAX_IDLE: Duration = Duration::from_secs(60);

/// Counting semaphore limiting upstream connections.
struct Slots {
    used: Mutex<usize>,
    freed: Condvar,
    waiting: AtomicUsize,
}

impl Slots {
    fn acquire(&self, stop: &AtomicBool) -> Option<SlotGuard<'_>> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let mut used = self.used.lock().unwrap();
        while *used >= MAX_UPSTREAM {
            if stop.load(Ordering::SeqCst) {
                self.waiting.fetch_sub(1, Ordering::SeqCst);
                return None;
            }
            used = self.freed.wait_timeout(used, Duration::from_millis(500)).unwrap().0;
        }
        *used += 1;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        Some(SlotGuard(self))
    }
}

struct SlotGuard<'a>(&'a Slots);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap() -= 1;
        self.0.freed.notify_one();
    }
}

/// A running proxy for one iDRAC.
pub struct WebProxy {
    pub local_port: u16,
    pub target: String,
    stop: Arc<AtomicBool>,
    pub active: Arc<AtomicUsize>,
}

impl WebProxy {
    /// Start proxying `https://127.0.0.1:<local_port>` to `host:port`.
    /// `local_port = 0` picks a free port.
    pub fn start(host: &str, port: u16, local_port: u16, cert_dir: &Path) -> Result<WebProxy> {
        let (cert, key) = load_or_create_cert(cert_dir)?;
        let mut acc = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
        acc.set_certificate(&cert)?;
        acc.set_private_key(&key)?;
        let acceptor = Arc::new(acc.build());

        let listener = TcpListener::bind(("127.0.0.1", local_port)).with_context(|| format!("binding 127.0.0.1:{local_port}"))?;
        let local_port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let host = host.to_string();
        log::info!("web proxy https://localhost:{local_port} -> {host}:{port}");

        let slots = Arc::new(Slots { used: Mutex::new(0), freed: Condvar::new(), waiting: AtomicUsize::new(0) });
        let (stop2, active2, host2) = (stop.clone(), active.clone(), host.clone());
        thread::Builder::new().name(format!("proxy-{local_port}")).spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(client) = conn else { continue };
                let (acceptor, host, active) = (acceptor.clone(), host2.clone(), active2.clone());
                let stop = stop2.clone();
                let slots = slots.clone();
                thread::spawn(move || {
                    active.fetch_add(1, Ordering::SeqCst);
                    if let Err(e) = handle(client, &acceptor, &host, port, &stop, &slots) {
                        log::debug!("proxy connection: {e:#}");
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        })?;
        Ok(WebProxy { local_port, target: format!("{host}:{port}"), stop, active })
    }

    pub fn url(&self) -> String {
        format!("https://localhost:{}/", self.local_port)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake up the accept loop.
        let _ = TcpStream::connect(("127.0.0.1", self.local_port));
    }
}

impl Drop for WebProxy {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle(client: TcpStream, acceptor: &SslAcceptor, host: &str, port: u16, stop: &AtomicBool, slots: &Slots) -> Result<()> {
    client.set_nodelay(true).ok();
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut down = acceptor.accept(client).map_err(|e| anyhow::anyhow!("browser TLS handshake: {e}"))?;
    let Some(_slot) = slots.acquire(stop) else { return Ok(()) };
    let mut up = crate::tls::connect(host, port)?;
    pump(&mut down, &mut up, stop, &slots.waiting)
}

/// Copy bytes both ways between two TLS streams until either side closes.
///
/// OpenSSL streams can't be read and written from two threads at once, so a
/// single thread alternates between short, timed reads on each side.
fn pump(a: &mut SslStream<TcpStream>, b: &mut SslStream<TcpStream>, stop: &AtomicBool, waiting: &AtomicUsize) -> Result<()> {
    let mut buf = vec![0u8; 32 * 1024];
    let mut last_traffic = Instant::now();
    // True between the browser sending something and the iDRAC answering.
    // The iDRAC often thinks for over a second before replying, and such a
    // connection must not be closed to make room for others.
    let mut awaiting_reply = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let idle = last_traffic.elapsed();
        if idle > MAX_IDLE || (!awaiting_reply && idle > YIELD_AFTER && waiting.load(Ordering::SeqCst) > 0) {
            // Closing an idle keep-alive connection is normal HTTP; the
            // browser opens a new one when it needs it.
            let _ = a.get_ref().shutdown(Shutdown::Both);
            let _ = b.get_ref().shutdown(Shutdown::Both);
            return Ok(());
        }
        // Back off from 2ms to 50ms timeouts while the connection is idle.
        let t = Duration::from_millis(if idle < Duration::from_millis(100) { 2 } else { 50 });
        a.get_ref().set_read_timeout(Some(t))?;
        b.get_ref().set_read_timeout(Some(t))?;
        let mut moved = false;
        for dir in 0..2 {
            let (src, dst) = if dir == 0 { (&mut *a, &mut *b) } else { (&mut *b, &mut *a) };
            match src.read(&mut buf) {
                Ok(0) => {
                    let _ = dst.shutdown();
                    let _ = dst.get_ref().shutdown(Shutdown::Both);
                    return Ok(());
                }
                Ok(n) => {
                    dst.write_all(&buf[..n])?;
                    dst.flush()?;
                    moved = true;
                    awaiting_reply = dir == 0;
                }
                Err(e) if crate::kvm::proto::is_timeout(&e) => {}
                Err(e) => {
                    // A peer that drops TCP without close_notify shows up
                    // here; treat it as a normal close.
                    log::trace!("proxy read: {e}");
                    let _ = dst.get_ref().shutdown(Shutdown::Both);
                    return Ok(());
                }
            }
        }
        if moved {
            last_traffic = Instant::now();
        }
    }
    Ok(())
}

fn load_or_create_cert(dir: &Path) -> Result<(X509, PKey<Private>)> {
    let cert_path: PathBuf = dir.join("proxy-cert.pem");
    let key_path: PathBuf = dir.join("proxy-key.pem");
    if let (Ok(c), Ok(k)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        if let (Ok(c), Ok(k)) = (X509::from_pem(&c), PKey::private_key_from_pem(&k)) {
            return Ok((c, k));
        }
    }
    let (cert, key) = generate_cert()?;
    std::fs::create_dir_all(dir).ok();
    std::fs::write(&cert_path, cert.to_pem()?)?;
    write_private(&key_path, &key.private_key_to_pem_pkcs8()?)?;
    Ok((cert, key))
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)?;
        Ok(())
    }
}

fn generate_cert() -> Result<(X509, PKey<Private>)> {
    let key = PKey::from_rsa(Rsa::generate(2048)?)?;
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_text("CN", "idrac6-rs local proxy")?;
    let name = name.build();
    let mut b = X509::builder()?;
    b.set_version(2)?;
    let mut serial = BigNum::new()?;
    serial.rand(128, MsbOption::MAYBE_ZERO, false)?;
    let serial = serial.to_asn1_integer()?;
    b.set_serial_number(&serial)?;
    b.set_subject_name(&name)?;
    b.set_issuer_name(&name)?;
    b.set_pubkey(&key)?;
    let (from, to) = (Asn1Time::days_from_now(0)?, Asn1Time::days_from_now(3650)?);
    b.set_not_before(&from)?;
    b.set_not_after(&to)?;
    b.append_extension(BasicConstraints::new().build()?)?;
    b.append_extension(ExtendedKeyUsage::new().server_auth().build()?)?;
    let san = SubjectAlternativeName::new().dns("localhost").ip("127.0.0.1").build(&b.x509v3_context(None, None))?;
    b.append_extension(san)?;
    b.sign(&key, MessageDigest::sha256())?;
    Ok((b.build(), key))
}
