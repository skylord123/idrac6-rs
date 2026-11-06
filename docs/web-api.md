# iDRAC6 web data API

The iDRAC6 web pages load their data from a small XML API on the same
HTTPS server. idrac6-rs uses it for the manager's live status, the Health
window, power control from the manager and the identify LED
(`src/health.rs`). Everything here was read from the web UI's pages
(`sysSummaryData.html`, `powermonitor.html`, `powercontrol.html`,
`sel.html`, `troubleShootIdentify.html`) on firmware 1.50.

All requests need a logged-in session (see [protocol.md](protocol.md),
section 1). A request with an expired or unknown session cookie gets
`HTTP 401`; log in again and repeat it.

The iDRAC6 web server is slow and handles one request at a time. A login
takes about a second, so idrac6-rs keeps one session per iDRAC open while
it polls and logs out when done.

## Reading: `GET /data?get=<names>`

`<names>` is a comma-separated list. The answer is one XML document with
the requested values and `<status>ok</status>`:

```text
GET /data?get=pwState,sysDesc,svcTag
<root><pwState>1</pwState><sysDesc>PowerEdge R710</sysDesc><svcTag>ABC1234</svcTag><status>ok</status></root>
```

| Name | Value |
|---|---|
| `pwState` | 1 on, 0 off |
| `sysDesc`, `sysRev` | Model and revision |
| `svcTag` | Service tag |
| `hostName`, `osName` | As reported by the OS (needs OpenManage on the server) |
| `biosVer`, `fwVersion` | BIOS and iDRAC firmware versions |
| `v4IPAddr`, `macAddr` | iDRAC network address |
| `temperatures`, `fans`, `voltages`, `powerSupplies`, `intrusion`, `batteries`, `systemLevel` | Sensor lists, see below |
| `powermonitordata` | Power statistics, see below |
| `eventLogEntries` | System Event Log, see below |
| `bootCapFileData` | Boot capture list, see [video-dvc.md](video-dvc.md) |
| `consolepreview[auto <millis>]` | Takes a console screenshot, see below |

### Sensors

Each sensor group is a `<sensortype>` with a `sensorid` and a list of
`<sensor>` elements:

```xml
<sensortype><sensorid>1</sensorid>
<thresholdSensorList><sensor>
  <sensorStatus>Normal</sensorStatus>
  <name>System Board Ambient Temp</name>
  <reading>25</reading><units>C</units>
  <minWarning>8</minWarning><maxWarning>42</maxWarning>
  <minFailure>3</minFailure><maxFailure>47</maxFailure>
</sensor></thresholdSensorList></sensortype>
```

| `sensorid` | Group | List element |
|---|---|---|
| 1 | Temperatures (`C`) | `thresholdSensorList` |
| 2 | Voltages (discrete, reading "Good") | `discreteSensorList` |
| 3 | System Level: power draw (`Watts`) | `thresholdSensorList` |
| 4 | Fans (`RPM`) | `thresholdSensorList` |
| 5 | Chassis intrusion | `discreteSensorList` |
| 8 | Power supplies | `psSensorList` |
| 41 | CMOS battery | `discreteSensorList` |

Thresholds that do not apply are `N/A`. `sensorStatus` is `Normal`,
`Warning` or `Critical`. Power supply entries have `location` instead
of `name`, and `onlineStatus`, `maxWattage`, `inputWattage`, `type` and
`fwVersion` instead of a reading.

### Power statistics

`powermonitordata` holds:

| Name | Meaning |
|---|---|
| `ampReading1`..`4` | Current per power supply in amps (0.0 if absent) |
| `pcAveLm`, `pcAveLh`, `pcAveDay`, `pcAveLw` | Average watts over the last minute, hour, day, week |
| `pcMax*`, `pcMin*` | Highest and lowest watts in the same windows, with times in `pcMaxPt*` and `pcMinPt*` |
| `ptsReadingc1` | Energy used in kWh since `ptsStc1` |
| `ptsReadingw1`, `ptsFtw1` | Peak power in watts and when |
| `ptsReadinga1`, `ptsFta1` | Peak current in amps and when |
| `ptsStmp1` | When peak tracking started |
| `hInstantaneous`, `hPeak` | Headroom (watts below the supply limit) |

The current draw is the `systemLevel` sensor.

### System Event Log

```xml
<eventLogEntries><eventLogEntry>
  <severity>Critical</severity>
  <dateTime>Mon Sep 08 2025 23:40:13</dateTime>
  <dateTimeOrder>20250908234013.000000</dateTimeOrder>
  <description>PS 1 Status: Power Supply sensor for PS 1, input lost was asserted</description>
</eventLogEntry>...</eventLogEntries>
```

Entries come oldest first. `dateTime` is `System Boot` for events logged
before the clock was set. `GET /csvdata?get=eventLogEntriesCSV` returns
the log as CSV (severity, date, description), as the web UI's *Save As*
does.

### Console preview

The summary page's screen preview takes two requests:

```text
GET /data?get=consolepreview[auto <millis>]       -> <status>ok</status>
GET /capconsole/scapture0.png?<millis>
```

The first makes the iDRAC grab the screen; this takes a few seconds. The
PNG is 700×525 with the picture in the top left corner and white padding
around it. `consolepreview[manual <millis>]` is the web UI's refresh
button.

## Changing: `GET /data?set=<name:value,...>`

Answers `<status>ok</status>` on success.

| Assignment | Action |
|---|---|
| `pwState:<n>` | Power control: 0 off, 1 on, 2 power cycle, 3 reset (warm boot), 4 NMI, 5 graceful shutdown |
| `IdentifyEnable:1,IdentifyTimeout:<s>` | Blink the front identify LED for 1-255 s (0 = until turned off) |
| `IdentifyEnable:0,IdentifyTimeout:0` | Stop blinking |
| `clearSEL:1` | Clear the System Event Log |
