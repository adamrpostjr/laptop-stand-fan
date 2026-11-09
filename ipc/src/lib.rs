//! Service ↔ desktop app protocol.
//!
//! The service owns the BLE link to the stand. The app talks to it over a
//! local named pipe: one JSON object per line, one response per request.
//! The pipe's ACL admits only SYSTEM, administrators and interactively
//! logged-on users, so the app never needs the stand's key.

use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\fanstand";
/// Longest request line the service accepts.
pub const MAX_LINE: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "req", rename_all = "snake_case")]
pub enum Request {
    Snapshot,
    /// Fan curve follows the CPU temperature.
    SetAuto,
    /// Fixed speed; also switches the stand to manual mode.
    SetManual { pct: u8 },
    /// Drop the link and reconnect now (skips any backoff wait).
    Reconnect,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "resp", rename_all = "snake_case")]
pub enum Response {
    Snapshot(Box<Snapshot>),
    Ok,
    Error { message: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    #[default]
    Starting,
    /// Scanning / connecting / authenticating.
    Connecting,
    Connected,
    /// Waiting before the next attempt (see `Snapshot::last_error`).
    Retrying,
    /// The machine is going to sleep; we let go of the stand.
    Suspended,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub service_version: String,
    pub link: Link,
    pub last_error: Option<String>,
    /// e.g. "Windows ACPI thermal zones, hottest (...)".
    pub temp_source: String,
    pub cpu: Option<CpuTemp>,
    /// Present once we've read the stand at least once on this connection.
    pub stand: Option<StandState>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CpuTemp {
    pub celsius: f32,
    /// From CPU load, not a sensor.
    pub estimated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StandState {
    pub name: String,
    pub firmware: String,
    pub rpm: [u16; 2],
    pub duty_pct: f32,
    pub target_pct: f32,
    pub mode: Mode,
    pub manual_pct: u8,
    /// Status flags as text ("ok", "FAILSAFE TEMP_FRESH", ...).
    pub flags: String,
    pub stalled: [bool; 2],
    pub failsafe: bool,
    pub calibrating: bool,
    pub uptime_s: u32,
    /// Milliseconds since the Unix epoch when this was read.
    pub updated_ms: u64,
}

pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("IPC types always serialize");
    s.push('\n');
    s
}

pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

/// Blocking one-shot client: connect, send `req`, read the reply.
#[cfg(windows)]
pub fn call(req: &Request) -> std::io::Result<Response> {
    use std::io::{BufRead, BufReader, Write};
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut tries = 0;
    let pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(PIPE_NAME)
        {
            Ok(p) => break p,
            // All instances busy: the service is between accepts.
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => return Err(e),
        }
    };
    (&pipe).write_all(encode(req).as_bytes())?;
    let mut line = String::new();
    BufReader::new(&pipe).read_line(&mut line)?;
    decode(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_stable() {
        assert_eq!(encode(&Request::SetManual { pct: 40 }), "{\"req\":\"set_manual\",\"pct\":40}\n");
        assert_eq!(encode(&Request::Snapshot), "{\"req\":\"snapshot\"}\n");
        assert_eq!(decode::<Request>("{\"req\":\"set_auto\"}\r\n").unwrap(), Request::SetAuto);
    }

    #[test]
    fn snapshot_roundtrip() {
        let s = Snapshot {
            service_version: "0.1.0".into(),
            link: Link::Connected,
            cpu: Some(CpuTemp { celsius: 55.5, estimated: false }),
            stand: Some(StandState {
                name: "FanStand".into(),
                firmware: "0.1.0".into(),
                rpm: [1190, 1203],
                duty_pct: 60.0,
                target_pct: 60.0,
                mode: Mode::Auto,
                manual_pct: 50,
                flags: "TEMP_FRESH".into(),
                stalled: [false, false],
                failsafe: false,
                calibrating: false,
                uptime_s: 100,
                updated_ms: 1,
            }),
            ..Default::default()
        };
        let r = Response::Snapshot(Box::new(s));
        assert_eq!(decode::<Response>(&encode(&r)).unwrap(), r);
    }

    #[test]
    fn junk_is_an_error() {
        assert!(decode::<Request>("{\"req\":\"format_c\"}").is_err());
        assert!(decode::<Request>("not json").is_err());
    }
}
