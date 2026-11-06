//! Server health, power and the System Event Log, read from the iDRAC6
//! web interface's data API (`/data?get=...`), plus the actions behind the
//! web UI's power control, identify and log pages. See `docs/web-api.md`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::kvm::proto::PowerAction;
use crate::web::{WebClient, xml_elements, xml_unescape, xml_value};

/// How healthy a sensor (or the whole server) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Health {
    #[default]
    Unknown,
    Ok,
    Warning,
    Critical,
}

impl Health {
    /// From the iDRAC's `sensorStatus` / `severity` strings.
    pub fn parse(s: &str) -> Health {
        let s = s.trim().to_ascii_lowercase();
        if s == "normal" || s == "ok" || s == "good" {
            Health::Ok
        } else if s.contains("warn") || s == "non-critical" || s == "noncritical" {
            Health::Warning
        } else if s.contains("critical") || s.contains("fail") || s.contains("recoverable") {
            Health::Critical
        } else {
            Health::Unknown
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Health::Unknown => "Unknown",
            Health::Ok => "Normal",
            Health::Warning => "Warning",
            Health::Critical => "Critical",
        }
    }

    /// The worst of several, ignoring unknowns unless there is nothing else.
    pub fn worst(items: impl IntoIterator<Item = Health>) -> Health {
        items.into_iter().max().unwrap_or(Health::Unknown)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorKind {
    Temperature,
    Fan,
    Voltage,
    PowerSupply,
    PowerDraw,
    Intrusion,
    Battery,
    Other,
}

impl SensorKind {
    /// The `sensorid` values the iDRAC6 uses.
    fn from_id(id: u32) -> SensorKind {
        match id {
            1 => SensorKind::Temperature,
            2 => SensorKind::Voltage,
            3 => SensorKind::PowerDraw,
            4 => SensorKind::Fan,
            5 => SensorKind::Intrusion,
            8 => SensorKind::PowerSupply,
            41 => SensorKind::Battery,
            _ => SensorKind::Other,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            SensorKind::Temperature => "Temperatures",
            SensorKind::Fan => "Fans",
            SensorKind::Voltage => "Voltages",
            SensorKind::PowerSupply => "Power supplies",
            SensorKind::PowerDraw => "Power consumption",
            SensorKind::Intrusion => "Chassis intrusion",
            SensorKind::Battery => "Batteries",
            SensorKind::Other => "Other",
        }
    }

    /// Display order on the health page.
    pub const ORDER: [SensorKind; 8] = [
        SensorKind::Temperature,
        SensorKind::Fan,
        SensorKind::PowerSupply,
        SensorKind::PowerDraw,
        SensorKind::Voltage,
        SensorKind::Battery,
        SensorKind::Intrusion,
        SensorKind::Other,
    ];
}

/// One sensor reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    pub kind: SensorKind,
    pub name: String,
    pub health: Health,
    /// Reading as the iDRAC formats it ("25", "Good", "Present", ...).
    pub reading: String,
    /// "C", "RPM", "Watts", or empty for discrete sensors.
    pub units: String,
    /// Warning and failure limits (`None` where the iDRAC says N/A).
    pub min_warning: Option<String>,
    pub max_warning: Option<String>,
    pub min_failure: Option<String>,
    pub max_failure: Option<String>,
    /// Extra facts (power supplies: rating, type, firmware).
    pub detail: Option<String>,
}

impl Sensor {
    /// Reading with its unit, e.g. "25 °C", "3480 RPM", "259 W".
    pub fn reading_text(&self) -> String {
        match self.units.as_str() {
            "" => self.reading.clone(),
            "C" => format!("{} °C", self.reading),
            "Watts" => format!("{} W", self.reading),
            "Amps" => format!("{} A", self.reading),
            "Volts" => format!("{} V", self.reading),
            u => format!("{} {u}", self.reading),
        }
    }

    pub fn numeric(&self) -> Option<f64> {
        self.reading.trim().parse().ok()
    }
}

fn opt(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|v| !v.is_empty() && *v != "N/A").map(xml_unescape)
}

/// Parse the `<sensortype>` lists returned for temperatures, fans,
/// voltages, powerSupplies, intrusion, batteries and systemLevel.
pub fn parse_sensors(xml: &str) -> Vec<Sensor> {
    let mut out = Vec::new();
    for group in xml_elements(xml, "sensortype") {
        let kind = SensorKind::from_id(xml_value(group, "sensorid").and_then(|v| v.parse().ok()).unwrap_or(0));
        for s in xml_elements(group, "sensor") {
            let status = xml_value(s, "sensorStatus").unwrap_or("");
            let mut sensor = Sensor {
                kind,
                name: xml_unescape(xml_value(s, "name").or(xml_value(s, "location")).unwrap_or("")),
                health: Health::parse(status),
                reading: xml_unescape(xml_value(s, "reading").or(xml_value(s, "onlineStatus")).unwrap_or("")),
                units: xml_value(s, "units").unwrap_or("").to_string(),
                min_warning: opt(xml_value(s, "minWarning")),
                max_warning: opt(xml_value(s, "maxWarning")),
                min_failure: opt(xml_value(s, "minFailure")),
                max_failure: opt(xml_value(s, "maxFailure")),
                detail: None,
            };
            if kind == SensorKind::PowerSupply {
                // "PS 1 Status" -> "PS 1"
                if let Some(n) = sensor.name.strip_suffix(" Status") {
                    sensor.name = n.to_string();
                }
                let mut d = Vec::new();
                if let Some(w) = opt(xml_value(s, "maxWattage")) {
                    d.push(format!("{w} W"));
                }
                if let Some(t) = opt(xml_value(s, "type")) {
                    d.push(t);
                }
                if let Some(f) = opt(xml_value(s, "fwVersion")) {
                    d.push(format!("firmware {f}"));
                }
                sensor.detail = (!d.is_empty()).then(|| d.join(", "));
            }
            out.push(sensor);
        }
    }
    out
}

/// What the manager window shows for each server.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub power_on: Option<bool>,
    /// Worst sensor state.
    pub health: Health,
    /// Sensors that are not normal, for the tooltip.
    pub problems: Vec<String>,
    /// Inlet (ambient) temperature in °C.
    pub ambient_c: Option<f64>,
    /// Current power draw in watts.
    pub watts: Option<f64>,
    /// "PowerEdge R710"
    pub model: String,
    pub service_tag: String,
}

/// Everything [`Status`] needs, in one request.
pub const STATUS_FIELDS: &str = "pwState,sysDesc,svcTag,temperatures,fans,voltages,powerSupplies,intrusion,batteries,systemLevel";
/// Every sensor group.
pub const SENSOR_FIELDS: &str = "temperatures,fans,voltages,powerSupplies,intrusion,batteries,systemLevel";

pub fn parse_power_state(xml: &str) -> Option<bool> {
    match xml_value(xml, "pwState") {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

pub fn status_from(xml: &str) -> Status {
    let sensors = parse_sensors(xml);
    let problems = sensors
        .iter()
        .filter(|s| matches!(s.health, Health::Warning | Health::Critical))
        .map(|s| format!("{}: {} ({})", s.name, s.reading_text(), s.health.label()))
        .collect();
    let ambient_c = sensors
        .iter()
        .filter(|s| s.kind == SensorKind::Temperature)
        .find(|s| s.name.to_ascii_lowercase().contains("ambient") || s.name.to_ascii_lowercase().contains("inlet"))
        .or_else(|| sensors.iter().find(|s| s.kind == SensorKind::Temperature))
        .and_then(Sensor::numeric);
    let watts = sensors.iter().find(|s| s.kind == SensorKind::PowerDraw).and_then(Sensor::numeric);
    Status {
        power_on: parse_power_state(xml),
        health: Health::worst(sensors.iter().map(|s| s.health)),
        problems,
        ambient_c,
        watts,
        model: xml_value(xml, "sysDesc").map(xml_unescape).unwrap_or_default(),
        service_tag: xml_value(xml, "svcTag").map(xml_unescape).unwrap_or_default(),
    }
}

/// The system summary page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub power_on: Option<bool>,
    pub model: String,
    pub revision: String,
    pub service_tag: String,
    pub host_name: String,
    pub os: String,
    pub bios: String,
    pub firmware: String,
    pub idrac_ip: String,
    pub idrac_mac: String,
}

pub const SUMMARY_FIELDS: &str = "pwState,sysDesc,sysRev,hostName,osName,svcTag,biosVer,fwVersion,v4IPAddr,macAddr";

pub fn parse_summary(xml: &str) -> Summary {
    let v = |t| xml_value(xml, t).map(xml_unescape).unwrap_or_default();
    Summary {
        power_on: parse_power_state(xml),
        model: v("sysDesc"),
        revision: v("sysRev"),
        service_tag: v("svcTag"),
        host_name: v("hostName"),
        os: v("osName"),
        bios: v("biosVer"),
        firmware: v("fwVersion"),
        idrac_ip: v("v4IPAddr"),
        idrac_mac: v("macAddr"),
    }
}

/// Power monitoring statistics (`powermonitordata`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PowerStats {
    /// Current draw from the System Level sensor, in watts.
    pub watts: Option<f64>,
    /// Current per power supply, in amps.
    pub amps: Vec<(String, f64)>,
    /// Energy used since `energy_since`, in kWh.
    pub energy_kwh: Option<f64>,
    pub energy_since: String,
    pub peak_watts: Option<f64>,
    pub peak_watts_at: String,
    pub peak_amps: Option<f64>,
    pub peak_amps_at: String,
    /// Since when the peaks were recorded.
    pub peaks_since: String,
    /// Last minute, hour, day and week.
    pub history: Vec<PowerWindow>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PowerWindow {
    pub label: &'static str,
    pub average: Option<f64>,
    pub max: Option<f64>,
    pub max_at: String,
    pub min: Option<f64>,
    pub min_at: String,
}

pub const POWER_FIELDS: &str = "powermonitordata,systemLevel";

pub fn parse_power(xml: &str) -> PowerStats {
    let num = |t: &str| xml_value(xml, t).and_then(|v| v.trim().parse::<f64>().ok());
    let text = |t: &str| xml_value(xml, t).map(xml_unescape).unwrap_or_default();
    let watts = parse_sensors(xml).into_iter().find(|s| s.kind == SensorKind::PowerDraw).and_then(|s| s.numeric());
    let amps = (1..=4)
        .filter_map(|i| {
            let a = num(&format!("ampReading{i}"))?;
            // Absent supplies report 0.0.
            (a > 0.0).then(|| (format!("PS {i}"), a))
        })
        .collect();
    let history = [("Last minute", "Lm"), ("Last hour", "Lh"), ("Last day", "Day"), ("Last week", "Lw")]
        .into_iter()
        .map(|(label, k)| PowerWindow {
            label,
            average: num(&format!("pcAve{k}")),
            max: num(&format!("pcMax{k}")),
            max_at: text(&format!("pcMaxPt{k}")),
            min: num(&format!("pcMin{k}")),
            min_at: text(&format!("pcMinPt{k}")),
        })
        .collect();
    PowerStats {
        watts,
        amps,
        energy_kwh: num("ptsReadingc1"),
        energy_since: text("ptsStc1"),
        peak_watts: num("ptsReadingw1"),
        peak_watts_at: text("ptsFtw1"),
        peak_amps: num("ptsReadinga1"),
        peak_amps_at: text("ptsFta1"),
        peaks_since: text("ptsStmp1"),
        history,
    }
}

/// One System Event Log record.
#[derive(Debug, Clone, PartialEq)]
pub struct SelEntry {
    pub severity: Health,
    /// As the iDRAC formats it, or "System Boot" for events logged before
    /// the clock was set.
    pub date: String,
    pub description: String,
}

/// Parse `eventLogEntries`. Returned newest first (the iDRAC lists them
/// oldest first).
pub fn parse_sel(xml: &str) -> Vec<SelEntry> {
    let mut v: Vec<SelEntry> = xml_elements(xml, "eventLogEntry")
        .into_iter()
        .map(|e| SelEntry {
            severity: Health::parse(xml_value(e, "severity").unwrap_or("")),
            date: xml_unescape(xml_value(e, "dateTime").unwrap_or("")),
            description: xml_unescape(xml_value(e, "description").unwrap_or("")),
        })
        .collect();
    v.reverse();
    v
}

/// A login on the iDRAC (web, console, virtual media, SSH...).
#[derive(Debug, Clone, PartialEq)]
pub struct IdracSession {
    pub id: u32,
    pub user: String,
    pub ip: String,
    /// The kind, named like the web UI does ("Virtual Console", "SSH"...).
    pub kind: String,
    /// The iDRAC lets it be closed.
    pub killable: bool,
    /// The session idrac6 itself reads this list with.
    pub own: bool,
}

/// Parse `activeSessions`, oldest first. `own` is the reader's session.
pub fn parse_sessions(xml: &str, own: Option<u32>) -> Vec<IdracSession> {
    let mut v: Vec<IdracSession> = xml_elements(xml, "session")
        .into_iter()
        .filter_map(|e| {
            let id = xml_value(e, "sessionId")?.trim().parse().ok()?;
            let raw = xml_unescape(xml_value(e, "sessionType").unwrap_or(""));
            // Names from the web UI's Sessions page.
            let kind = match raw.as_str() {
                "webgui" => "Web".to_string(),
                "CLP" => "SSH / Telnet (SM-CLP)".to_string(),
                "racadm" => "RACADM".to_string(),
                "vKVM" => "Virtual Console".to_string(),
                "vMedia" => "Virtual Media".to_string(),
                _ => raw,
            };
            Some(IdracSession {
                id,
                user: xml_unescape(xml_value(e, "userName").unwrap_or("")),
                ip: xml_unescape(xml_value(e, "ipAddress").unwrap_or("")),
                kind,
                killable: xml_value(e, "isKillable").is_some_and(|k| k.trim() == "1") && Some(id) != own,
                own: Some(id) == own,
            })
        })
        .collect();
    v.sort_by_key(|s| s.id);
    v
}

/// The web UI's Sessions page has the reader's own session id in a
/// script: declared as `mySessionId = -1;`, then set with
/// `mySessionId = 189 ;`.
fn parse_own_session(html: &str) -> Option<u32> {
    html.match_indices("mySessionId")
        .filter_map(|(i, k)| {
            let rest = html[i + k.len()..].trim_start().strip_prefix('=')?.trim_start();
            let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            rest[..end].parse().ok()
        })
        .last()
}

/// `data?set=pwState:<n>` values (from the web UI's power control page).
pub fn power_state_value(a: PowerAction) -> u8 {
    match a {
        PowerAction::Off => 0,
        PowerAction::On => 1,
        PowerAction::Cycle => 2,
        PowerAction::Reset => 3,
        PowerAction::GracefulShutdown => 5,
    }
}

// --- Requests ------------------------------------------------------------

pub fn fetch_status(c: &WebClient) -> Result<Status> {
    Ok(status_from(&c.data(STATUS_FIELDS)?))
}

pub fn fetch_summary(c: &WebClient) -> Result<Summary> {
    Ok(parse_summary(&c.data(SUMMARY_FIELDS)?))
}

pub fn fetch_sensors(c: &WebClient) -> Result<Vec<Sensor>> {
    Ok(parse_sensors(&c.data(SENSOR_FIELDS)?))
}

pub fn fetch_power(c: &WebClient) -> Result<PowerStats> {
    Ok(parse_power(&c.data(POWER_FIELDS)?))
}

pub fn fetch_sel(c: &WebClient) -> Result<Vec<SelEntry>> {
    Ok(parse_sel(&c.data("eventLogEntries")?))
}

/// The System Event Log as CSV, the way the web UI's "Save As" saves it.
pub fn sel_csv(c: &WebClient) -> Result<Vec<u8>> {
    let r = c.get("/csvdata?get=eventLogEntriesCSV")?;
    if r.status != 200 {
        bail!("downloading the event log returned HTTP {}", r.status);
    }
    Ok(r.body)
}

pub fn clear_sel(c: &WebClient) -> Result<()> {
    c.set("clearSEL:1").map(drop)
}

pub fn fetch_sessions(c: &WebClient) -> Result<Vec<IdracSession>> {
    let xml = c.data("activeSessions")?;
    // Which one is ours; the list is still useful without it.
    let own = c.get("/ssninfo.html").ok().filter(|r| r.status == 200).and_then(|r| parse_own_session(&r.text()));
    Ok(parse_sessions(&xml, own))
}

/// Log a session out (the trash can on the web UI's Sessions page).
pub fn close_session(c: &WebClient, id: u32) -> Result<()> {
    c.set(&format!("killSession({id})")).map(drop)
}

/// Power control through the web interface (same as its Power page).
pub fn set_power(c: &WebClient, a: PowerAction) -> Result<()> {
    c.set(&format!("pwState:{}", power_state_value(a))).map(drop)
}

/// Blink the blue identify LED on the server's front panel for `seconds`
/// (1-255), or stop it with 0.
pub fn identify(c: &WebClient, seconds: u8) -> Result<()> {
    let on = u8::from(seconds > 0);
    c.set(&format!("IdentifyEnable:{on},IdentifyTimeout:{seconds}")).map(drop)
}

/// A fresh console preview picture (what the web UI's summary page shows),
/// with the blank border the iDRAC adds trimmed off.
pub fn fetch_preview(c: &WebClient) -> Result<image::RgbImage> {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    // Asks the iDRAC to grab the screen into /capconsole/scapture0.png.
    c.data(&format!("consolepreview[auto%20{t}]"))?;
    let r = c.get(&format!("/capconsole/scapture0.png?{t}"))?;
    if r.status != 200 {
        bail!("downloading the console preview returned HTTP {}", r.status);
    }
    let img = image::load_from_memory_with_format(&r.body, image::ImageFormat::Png).context("decoding the console preview")?.into_rgb8();
    Ok(trim_border(img))
}

/// The preview is padded to a fixed size with white on the right and
/// bottom; cut that off.
fn trim_border(img: image::RgbImage) -> image::RgbImage {
    let (w, h) = img.dimensions();
    let white = |x: u32, y: u32| img.get_pixel(x, y).0 == [255, 255, 255];
    let mut right = w;
    while right > 1 && (0..h).all(|y| white(right - 1, y)) {
        right -= 1;
    }
    let mut bottom = h;
    while bottom > 1 && (0..right).all(|x| white(x, bottom - 1)) {
        bottom -= 1;
    }
    if right == w && bottom == h {
        return img;
    }
    image::imageops::crop_imm(&img, 0, 0, right, bottom).to_image()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed responses from an iDRAC6 1.50 in a PowerEdge R710.
    const SENSORS: &str = "<root><pwState>1</pwState>
<powerOn></powerOn><sensortype><sensorid>1</sensorid>
<thresholdSensorList><sensor><sensorStatus>Normal</sensorStatus>
<name>System Board Ambient Temp</name>
<reading>25</reading>
<units>C</units>
<minWarning>8</minWarning>
<maxWarning>42</maxWarning>
<minFailure>3</minFailure>
<maxFailure>47</maxFailure>
</sensor></thresholdSensorList></sensortype><sensortype><sensorid>4</sensorid>
<thresholdSensorList><sensor><sensorStatus>Normal</sensorStatus>
<name>System Board FAN 1 RPM</name>
<reading>3480</reading>
<units>RPM</units>
<minWarning>N/A</minWarning>
<maxWarning>N/A</maxWarning>
<minFailure>720</minFailure>
<maxFailure>N/A</maxFailure>
</sensor></thresholdSensorList></sensortype><sensortype><sensorid>2</sensorid>
<discreteSensorList><sensor><sensorStatus>Critical</sensorStatus>
<name>PS 1 Voltage</name>
<reading>0V</reading>
</sensor></discreteSensorList></sensortype><sensortype><sensorid>8</sensorid>
<psSensorList><sensor><sensorStatus>Normal</sensorStatus>
<location>PS 2 Status</location>
<inputWattage>1080</inputWattage>
<fwVersion>08.13.00</fwVersion>
<maxWattage>870</maxWattage>
<type>AC</type>
<onlineStatus>Present</onlineStatus>
</sensor></psSensorList></sensortype><sensortype><sensorid>3</sensorid>
<thresholdSensorList><sensor><sensorStatus>Normal</sensorStatus>
<name>System Board System Level</name>
<reading>259</reading>
<units>Watts</units>
<minWarning>N/A</minWarning>
<maxWarning>917</maxWarning>
<minFailure>N/A</minFailure>
<maxFailure>966</maxFailure>
</sensor></thresholdSensorList></sensortype><status>ok</status>
</root>";

    #[test]
    fn parses_sensors() {
        let s = parse_sensors(SENSORS);
        assert_eq!(s.len(), 5);
        assert_eq!(s[0].kind, SensorKind::Temperature);
        assert_eq!(s[0].reading_text(), "25 °C");
        assert_eq!(s[0].max_warning.as_deref(), Some("42"));
        assert_eq!(s[1].kind, SensorKind::Fan);
        assert_eq!(s[1].min_warning, None);
        assert_eq!(s[1].min_failure.as_deref(), Some("720"));
        assert_eq!(s[2].health, Health::Critical);
        assert_eq!(s[3].kind, SensorKind::PowerSupply);
        assert_eq!(s[3].name, "PS 2");
        assert_eq!(s[3].reading, "Present");
        assert_eq!(s[3].detail.as_deref(), Some("870 W, AC, firmware 08.13.00"));
        assert_eq!(s[4].reading_text(), "259 W");
    }

    #[test]
    fn summarizes_status() {
        let st = status_from(SENSORS);
        assert_eq!(st.power_on, Some(true));
        assert_eq!(st.health, Health::Critical);
        assert_eq!(st.problems, vec!["PS 1 Voltage: 0V (Critical)".to_string()]);
        assert_eq!(st.ambient_c, Some(25.0));
        assert_eq!(st.watts, Some(259.0));
    }

    #[test]
    fn parses_power_and_summary() {
        let x = "<root><powermonitordata><pmReading>114</pmReading><ampReading1>1.0</ampReading1><ampReading2>1.3</ampReading2>
<ampReading3>0.0</ampReading3><ptsStc1>Thu Aug 19 2010 04:28:03 AM</ptsStc1><ptsReadingc1>162.9</ptsReadingc1>
<ptsReadingw1>424</ptsReadingw1><ptsFtw1>Thu Aug 21 2025 09:23:27 AM</ptsFtw1><pcAveLm>263</pcAveLm><pcMaxLw>417</pcMaxLw>
</powermonitordata><status>ok</status></root>";
        let p = parse_power(x);
        assert_eq!(p.amps, vec![("PS 1".to_string(), 1.0), ("PS 2".to_string(), 1.3)]);
        assert_eq!(p.energy_kwh, Some(162.9));
        assert_eq!(p.peak_watts, Some(424.0));
        assert_eq!(p.history[0].average, Some(263.0));
        assert_eq!(p.history[3].max, Some(417.0));
        assert_eq!(p.watts, None);

        let s = parse_summary("<root><pwState>0</pwState><sysDesc>PowerEdge R710</sysDesc><svcTag>ABC1234</svcTag></root>");
        assert_eq!(s.power_on, Some(false));
        assert_eq!(s.model, "PowerEdge R710");
        assert_eq!(s.service_tag, "ABC1234");
    }

    #[test]
    fn parses_sel_newest_first() {
        let x = "<root><eventLogEntries><eventLogEntry><severity>Normal</severity>
<dateTime>Thu Aug 19 2010 04:32:47</dateTime><dateTimeOrder>20100819043247.000000</dateTimeOrder>
<description>Log cleared</description></eventLogEntry><eventLogEntry><severity>Critical</severity>
<dateTime>System Boot</dateTime><description>PS 1 input lost &amp; restored</description></eventLogEntry>
</eventLogEntries><status>ok</status></root>";
        let e = parse_sel(x);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].severity, Health::Critical);
        assert_eq!(e[0].description, "PS 1 input lost & restored");
        assert_eq!(e[1].date, "Thu Aug 19 2010 04:32:47");
    }

    #[test]
    fn parses_sessions() {
        let xml = "<root><sessionList><session><sessionId>190</sessionId>\n<userName>root</userName>\n<ipAddress>192.0.2.82</ipAddress>\n<sessionType>webgui</sessionType>\n<consoleType>webgui</consoleType>\n<isKillable>1</isKillable>\n</session><session><sessionId>175</sessionId>\n<userName>admin</userName>\n<ipAddress>10.0.0.9</ipAddress>\n<sessionType>vKVM</sessionType>\n<isKillable>1</isKillable>\n</session><session><sessionId>12</sessionId><userName>root</userName><ipAddress>10.0.0.2</ipAddress><sessionType>CLP</sessionType><isKillable>0</isKillable></session></sessionList><status>ok</status></root>";
        let html = "var mySessionId = -1;        // 06/11/08 rbj\n<script>\n        mySessionId = 190 ;\n";
        let own = parse_own_session(html);
        assert_eq!(own, Some(190));
        let s = parse_sessions(xml, own);
        assert_eq!(s.iter().map(|s| s.id).collect::<Vec<_>>(), [12, 175, 190]);
        assert_eq!((s[0].kind.as_str(), s[0].killable), ("SSH / Telnet (SM-CLP)", false));
        assert_eq!((s[1].kind.as_str(), s[1].user.as_str(), s[1].ip.as_str(), s[1].killable, s[1].own), ("Virtual Console", "admin", "10.0.0.9", true, false));
        assert_eq!((s[2].kind.as_str(), s[2].killable, s[2].own), ("Web", false, true));
    }

    #[test]
    fn health_strings() {
        assert_eq!(Health::parse("Normal"), Health::Ok);
        assert_eq!(Health::parse("Warning"), Health::Warning);
        assert_eq!(Health::parse("Non-Critical"), Health::Warning);
        assert_eq!(Health::parse("Critical"), Health::Critical);
        assert_eq!(Health::parse("Non-Recoverable"), Health::Critical);
        assert_eq!(Health::parse(""), Health::Unknown);
        assert_eq!(Health::worst([Health::Ok, Health::Warning, Health::Unknown]), Health::Warning);
    }

    #[test]
    fn power_values_match_web_ui() {
        assert_eq!(power_state_value(PowerAction::Off), 0);
        assert_eq!(power_state_value(PowerAction::On), 1);
        assert_eq!(power_state_value(PowerAction::Cycle), 2);
        assert_eq!(power_state_value(PowerAction::Reset), 3);
        assert_eq!(power_state_value(PowerAction::GracefulShutdown), 5);
    }

    #[test]
    fn trims_white_border() {
        let mut img = image::RgbImage::from_pixel(10, 8, image::Rgb([255, 255, 255]));
        for y in 0..6 {
            for x in 0..7 {
                img.put_pixel(x, y, image::Rgb([0, 0, 0]));
            }
        }
        assert_eq!(trim_border(img).dimensions(), (7, 6));
    }
}
