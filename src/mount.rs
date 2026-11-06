//! `idrac6 mount`: attach an image as virtual media without the GUI.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Instance;
use crate::kvm::session::{KvmEvent, KvmSession};
use crate::vmedia::{DriveKind, VmConfig, VmEvent, VmImage, VmSession};
use crate::web::WebSession;

pub fn run(inst: Instance, image: PathBuf, kind: Option<DriveKind>, read_write: bool) -> Result<()> {
    let kind = kind.unwrap_or_else(|| {
        let iso = image.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso"));
        if iso { DriveKind::Cd } else { DriveKind::Floppy }
    });
    std::fs::metadata(&image).with_context(|| format!("cannot read {}", image.display()))?;

    // Enter, end of input, Ctrl+C and SIGTERM all unmap and log out.
    let (stop_tx, stop_rx) = mpsc::channel();
    let tx = stop_tx.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = tx.send(());
    }) {
        log::warn!("cannot handle Ctrl+C: {e}");
    }
    eprintln!("Logging in to {}…", inst.host);
    let web = WebSession::login(&inst.host, inst.https_port, &inst.username, &inst.password)?;
    let result = (|| {
        let info = web.console_info()?;
        if !info.vm_privilege() {
            bail!("this user does not have the Virtual Media privilege");
        }
        let params = info.kvm_params(&inst.host, false);
        let port = params.kmport;
        // Virtual Media credentials are handed out on a console session.
        let kvm = KvmSession::start(params, Arc::new(|| {}));
        let (user, password) = wait_for_credentials(&kvm, &stop_rx)?;

        let (tx, rx) = mpsc::channel();
        let cfg = VmConfig { host: inst.host.clone(), port, username: user, password, preempt: false };
        let vm = VmSession::connect(cfg, tx)?;
        let img = VmImage { path: image.clone(), kind, read_only: !read_write };
        vm.map(img)?;
        eprintln!("Mapped {} as {:?}. Press Enter (or Ctrl+C) to unmap.", image.display(), kind);

        std::thread::spawn(move || {
            let _ = std::io::stdin().lock().read_line(&mut String::new());
            let _ = stop_tx.send(());
        });
        let mut last_print = Instant::now();
        loop {
            if stop_rx.recv_timeout(Duration::from_millis(200)).is_ok() {
                break;
            }
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    VmEvent::Stats { read_bytes, write_bytes, .. } if last_print.elapsed() > Duration::from_secs(2) => {
                        eprintln!("  read {read_bytes} bytes, written {write_bytes} bytes");
                        last_print = Instant::now();
                    }
                    VmEvent::Stats { .. } => {}
                    VmEvent::Disconnected(r) => bail!("virtual media disconnected: {r}"),
                    VmEvent::Unmapped { reason: Some(r), .. } => bail!("the iDRAC unmapped the image: {r}"),
                    other => eprintln!("  {other:?}"),
                }
            }
            if kvm.is_closed() {
                bail!("the console session closed");
            }
        }
        eprintln!("Unmapping…");
        vm.unmap(kind)?;
        vm.disconnect();
        kvm.close();
        Ok(())
    })();
    let _ = web.logout();
    result
}

fn wait_for_credentials(kvm: &KvmSession, stop: &mpsc::Receiver<()>) -> Result<(String, String)> {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut requested = false;
    while Instant::now() < deadline {
        if stop.try_recv().is_ok() {
            kvm.close();
            bail!("interrupted");
        }
        match kvm.events.recv_timeout(Duration::from_millis(250)) {
            Ok(KvmEvent::Running) if !requested => {
                kvm.request_vm_credentials();
                requested = true;
            }
            Ok(KvmEvent::VmCredentials { user, password }) => return Ok((user, password)),
            Ok(KvmEvent::LoginFailed(r)) | Ok(KvmEvent::Disconnected(r)) => bail!("{r}"),
            _ => {}
        }
    }
    bail!("timed out waiting for virtual media credentials from the iDRAC")
}
