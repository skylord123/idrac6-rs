//! TLS helpers for talking to iDRAC6.
//!
//! iDRAC6 only speaks SSLv3/TLS 1.0 with old cipher suites and ships a
//! self-signed (and usually expired) certificate. Modern defaults refuse all
//! of that, so this module builds an OpenSSL connector with the security
//! level dropped to 0 and certificate verification disabled. It is only
//! ever used for connections *to* the iDRAC.

use std::io::{Read, Write};
use std::net::TcpStream;

use anyhow::{Context, Result};
use openssl::ssl::{SslConnector, SslMethod, SslOptions, SslStream, SslVerifyMode, SslVersion};

/// Build a connector that accepts anything the iDRAC6 offers.
pub fn legacy_connector() -> Result<SslConnector> {
    let mut b = SslConnector::builder(SslMethod::tls_client())?;
    // SECLEVEL=0 re-enables TLS 1.0, SHA1/MD5 signatures, small DH groups and
    // anonymous / RC4 / 3DES suites (where the OpenSSL build still has them).
    // Plain RSA key exchange comes first: the iDRAC6 CPU needs ~6 s for a
    // DHE handshake but only ~0.6 s for AES128-SHA.
    b.set_cipher_list("AES128-SHA:AES256-SHA:DES-CBC3-SHA:RC4-SHA:RC4-MD5:ALL:COMPLEMENTOFALL:@SECLEVEL=0")
        .context("setting legacy cipher list")?;
    b.set_min_proto_version(None)?;
    // iDRAC6 never negotiates above TLS 1.0, and some firmware closes the
    // connection when it sees a newer ClientHello version.
    b.set_max_proto_version(Some(SslVersion::TLS1))?;
    b.set_verify(SslVerifyMode::NONE);
    // Old firmware does not support secure renegotiation.
    b.set_options(SslOptions::ALLOW_UNSAFE_LEGACY_RENEGOTIATION);
    Ok(b.build())
}

/// Wrap an already-connected socket in a legacy TLS session.
pub fn wrap<S: Read + Write + std::fmt::Debug>(stream: S, host: &str) -> Result<SslStream<S>> {
    let conn = legacy_connector()?;
    let mut cfg = conn.configure()?;
    cfg.set_verify_hostname(false);
    cfg.set_use_server_name_indication(false);
    let s = cfg
        .connect(host, stream)
        .map_err(|e| anyhow::anyhow!("TLS handshake with {host} failed: {e}"))?;
    log::debug!(
        "TLS to {host}: {} {}",
        s.ssl().version_str(),
        s.ssl().current_cipher().map(|c| c.name()).unwrap_or("?")
    );
    Ok(s)
}

/// Connect a TCP socket and wrap it in legacy TLS.
pub fn connect(host: &str, port: u16) -> Result<SslStream<TcpStream>> {
    use std::net::ToSocketAddrs;
    let addr = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?
        .next()
        .with_context(|| format!("no address for {host}"))?;
    let tcp = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(10))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    tcp.set_nodelay(true).ok();
    wrap(tcp, host)
}
