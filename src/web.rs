//! Talking to the iDRAC6 web server: log in, fetch the virtual console
//! launch file (JNLP) and log out again.
//!
//! The console itself does not take the real username/password. The web UI
//! hands out one-time tokens inside `viewer.jnlp`, so opening a console
//! means: log in to the web UI, download the JNLP, log out, then connect to
//! the KVM ports with the tokens.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::kvm::session::KvmParams;

/// A minimal HTTP/1.1 response.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Send one request over a fresh legacy-TLS connection.
pub fn request(host: &str, port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Response> {
    let mut s = crate::tls::connect(host, port)?;
    s.get_ref().set_read_timeout(Some(Duration::from_secs(30)))?;
    let host_hdr = if port == 443 { host.to_string() } else { format!("{host}:{port}") };
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host_hdr}\r\nUser-Agent: idrac6-rs\r\nAccept: */*\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    if let Some(b) = body {
        s.write_all(b)?;
    }
    s.flush()?;
    let mut raw = Vec::new();
    match s.read_to_end(&mut raw) {
        Ok(_) => {}
        // Appweb often drops the TLS connection without a close_notify.
        Err(e) if !raw.is_empty() => log::debug!("read ended with {e}"),
        Err(e) => return Err(e.into()),
    }
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<Response> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").context("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).context("bad status line")?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        body = dechunk(&body);
    }
    Ok(Response { status, headers, body })
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = b.windows(2).position(|w| w == b"\r\n") {
        let size_str = String::from_utf8_lossy(&b[..eol]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
        b = &b[eol + 2..];
        if size == 0 || size > b.len() {
            break;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[(size + 2).min(b.len())..];
    }
    out
}

/// Percent-encode a form value the way the iDRAC login page does.
fn form_escape(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn xml_value<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim())
}

/// A logged-in web session.
pub struct WebSession {
    pub host: String,
    pub port: u16,
    cookie: String,
    st2: Option<String>,
}

impl WebSession {
    pub fn login(host: &str, port: u16, user: &str, password: &str) -> Result<WebSession> {
        let body = format!("user={}&password={}", form_escape(user), form_escape(password));
        let r = request(
            host,
            port,
            "POST",
            "/data/login",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            Some(body.as_bytes()),
        )
        .context("logging in to the iDRAC web interface")?;
        if r.status != 200 {
            bail!("iDRAC login returned HTTP {}", r.status);
        }
        let text = r.text();
        log::debug!("login response: {text}");
        if xml_value(&text, "status") != Some("ok") {
            bail!("iDRAC login failed: {}", xml_value(&text, "errorMessage").unwrap_or(&text));
        }
        match xml_value(&text, "authResult") {
            Some("0") => {}
            Some("1") => bail!("Login failed: wrong username or password."),
            Some("4") => bail!("Login failed: this user is not allowed to log in."),
            Some("5") => bail!("Login failed: too many web sessions are open on the iDRAC. Close some or wait for them to time out."),
            other => bail!("Login failed (authResult {}).", other.unwrap_or("?")),
        }
        let cookie = r
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .filter_map(|(_, v)| v.split(';').next())
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("; ");
        if cookie.is_empty() {
            bail!("iDRAC login did not return a session cookie");
        }
        // Newer iDRAC6 firmware (2.8x+) returns index.html?ST1=..,ST2=.. and
        // wants ST2 echoed back as a header on data requests.
        let st2 = xml_value(&text, "forwardUrl").and_then(|u| {
            u.split(|c| c == '?' || c == ',' || c == '&')
                .find_map(|kv| kv.strip_prefix("ST2="))
                .map(str::to_string)
        });
        Ok(WebSession { host: host.into(), port, cookie, st2 })
    }

    pub fn get(&self, path: &str) -> Result<Response> {
        let mut h: Vec<(&str, &str)> = vec![("Cookie", &self.cookie)];
        if let Some(st2) = &self.st2 {
            h.push(("ST2", st2));
        }
        request(&self.host, self.port, "GET", path, &h, None)
    }

    /// Download the virtual console launch file and extract the session
    /// parameters from it.
    pub fn console_info(&self) -> Result<ConsoleInfo> {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = format!("/viewer.jnlp({}@0@idrac@{millis})", self.host);
        let r = self.get(&path)?;
        if r.status != 200 {
            bail!("downloading viewer.jnlp returned HTTP {}", r.status);
        }
        ConsoleInfo::from_jnlp(&r.text())
    }

    /// The boot recordings the iDRAC keeps (up to three, newest first).
    pub fn boot_captures(&self) -> Result<Vec<BootCapture>> {
        let r = self.get("/data?get=bootCapFileData")?;
        if r.status != 200 {
            bail!("listing boot captures returned HTTP {}", r.status);
        }
        let xml = r.text();
        if xml_value(&xml, "bootCapEnabled") == Some("0") {
            bail!("Boot capture is disabled on this iDRAC.");
        }
        let count: usize = xml_value(&xml, "bootCapCount").and_then(|v| v.parse().ok()).unwrap_or(0);
        Ok((0..count.min(3))
            .filter_map(|i| {
                let file = xml_value(&xml, &format!("bootCapFname{i}"))?.to_string();
                let date = xml_value(&xml, &format!("bootCapDate{i}")).unwrap_or("").to_string();
                (!file.is_empty() && !file.contains(' ')).then_some(BootCapture { file, date })
            })
            .collect())
    }

    /// Download one boot recording (at most 2 MiB).
    pub fn download_boot_capture(&self, c: &BootCapture) -> Result<Vec<u8>> {
        let r = self.get(&format!("/captures/{}", c.file))?;
        if r.status != 200 {
            bail!("downloading {} returned HTTP {}", c.file, r.status);
        }
        Ok(r.body)
    }

    /// End the session. Returns false if the iDRAC could not be reached
    /// (the session then stays open on the iDRAC until it times out).
    pub fn logout(&self) -> bool {
        match self.get("/data/logout") {
            Ok(_) => true,
            Err(e) => {
                log::debug!("logout failed: {e:#}");
                false
            }
        }
    }
}

/// A boot recording stored on the iDRAC.
#[derive(Debug, Clone)]
pub struct BootCapture {
    /// File name under `/captures/`, e.g. `bootcapture_2`.
    pub file: String,
    /// When it was recorded, as the iDRAC formats it.
    pub date: String,
}

/// The arguments the Java viewer would have been started with.
#[derive(Debug, Clone)]
pub struct ConsoleInfo {
    pub args: HashMap<String, String>,
}

impl ConsoleInfo {
    pub fn from_jnlp(xml: &str) -> Result<ConsoleInfo> {
        let mut args = HashMap::new();
        let mut rest = xml;
        while let Some(i) = rest.find("<argument>") {
            rest = &rest[i + "<argument>".len()..];
            let Some(end) = rest.find("</argument>") else { break };
            let arg = &rest[..end];
            if let Some((k, v)) = arg.split_once('=') {
                args.insert(k.trim().to_string(), v.trim().to_string());
            }
            rest = &rest[end..];
        }
        if !args.contains_key("user") || !args.contains_key("passwd") {
            if xml.contains("<jnlp") {
                bail!("viewer.jnlp has no session tokens; does this user have virtual console privileges?");
            }
            bail!("the iDRAC did not return a viewer.jnlp (session expired or console disabled?)");
        }
        Ok(ConsoleInfo { args })
    }

    fn get<T: std::str::FromStr>(&self, k: &str, default: T) -> T {
        self.args.get(k).and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    pub fn kvm_params(&self, host: &str, shared: bool) -> KvmParams {
        KvmParams {
            // Use the address the user configured (it may be a DNS name or a
            // forwarded address) rather than the IP inside the JNLP.
            host: host.to_string(),
            kmport: self.get("kmport", 5900),
            vport: self.get("vport", 5900),
            user: self.args.get("user").cloned().unwrap_or_default(),
            passwd: self.args.get("passwd").cloned().unwrap_or_default(),
            apcp: self.get("apcp", 1u8) == 1,
            version: self.get("version", 2u8),
            shared,
        }
    }

    pub fn vm_privilege(&self) -> bool {
        self.args.get("vmprivilege").map(|v| v == "true").unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JNLP: &str = r#"<jnlp codebase="https://192.168.1.120:443" spec="1.0+">
 <application-desc main-class="com.avocent.idrac.kvm.Main">
   <argument>ip=192.168.1.120</argument>
   <argument>vmprivilege=true</argument>
   <argument>user=1804289383</argument>
   <argument>passwd=846930886</argument>
   <argument>kmport=5900</argument>
   <argument>vport=5900</argument>
   <argument>apcp=1</argument>
   <argument>version=2</argument>
 </application-desc></jnlp>"#;

    #[test]
    fn parses_jnlp() {
        let c = ConsoleInfo::from_jnlp(JNLP).unwrap();
        let p = c.kvm_params("idrac.local", false);
        assert_eq!(p.user, "1804289383");
        assert_eq!(p.passwd, "846930886");
        assert_eq!(p.kmport, 5900);
        assert!(p.apcp);
        assert_eq!(p.version, 2);
        // We always connect to the host the user configured (it may be a
        // DNS name or a forwarded address), not the IP inside the JNLP.
        assert_eq!(p.host, "idrac.local");
        assert!(c.vm_privilege());
    }

    #[test]
    fn xml_values() {
        let x = "<root><bootCapCount>2</bootCapCount><bootCapFname0>bootcapture_2</bootCapFname0></root>";
        assert_eq!(xml_value(x, "bootCapCount"), Some("2"));
        assert_eq!(xml_value(x, "bootCapFname0"), Some("bootcapture_2"));
        assert_eq!(xml_value(x, "missing"), None);
    }

    #[test]
    fn parses_chunked() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap();
        assert_eq!(r.body, b"abcde");
    }

    #[test]
    fn escapes_form() {
        assert_eq!(form_escape("a b&c=d"), "a%20b%26c%3Dd");
    }
}
