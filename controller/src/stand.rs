//! Connection to the stand over BLE (btleplug), with typed reads and writes.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use fanstand_protocol::auth::{Key, NONCE_LEN, compute_response};
use fanstand_protocol::calibration::STEPS;
use fanstand_protocol::codec::{decode_percent, decode_rpm, encode_percent, encode_temperature};
use fanstand_protocol::diag::{DebugInfo, att_error, decode_cal_table};
use fanstand_protocol::{CalResult, Command, Config, DecodeError, FanCurve, Mode, Status, uuids};
use futures::{Stream, StreamExt};
use log::debug;
use uuid::Uuid;

pub const SERVICE: Uuid = Uuid::from_u128(uuids::SERVICE);

pub struct Found {
    pub peripheral: Peripheral,
    pub name: String,
    pub id: String,
    pub rssi: Option<i16>,
}

pub async fn adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("Bluetooth unavailable")?;
    manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Bluetooth adapter found"))
}

/// Scan for stands for up to `timeout`, stopping early once one is found
/// unless `all` is set.
pub async fn scan(adapter: &Adapter, timeout: Duration, all: bool) -> Result<Vec<Found>> {
    start_scan(adapter).await?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<Found> = Vec::new();
    // Devices already ruled out: don't re-query their properties every poll
    // (each query is a D-Bus round trip on Linux).
    let mut rejected = HashSet::new();
    loop {
        for p in adapter.peripherals().await? {
            let id = p.id();
            if rejected.contains(&id) {
                continue;
            }
            let Some(props) = p.properties().await? else {
                continue;
            };
            debug!(
                "scan: {id} name={:?} services={:?} rssi={:?}",
                props.local_name, props.services, props.rssi
            );
            // We match the service in-app (Linux scans unfiltered, see
            // `start_scan`). The name is a fallback for when the service list
            // isn't filled in yet; `Stand::open` verifies the service anyway.
            let named = props.local_name.as_deref() == Some(uuids::DEVICE_NAME);
            if !props.services.contains(&SERVICE) && !named {
                // A device may not have advertised its name yet; only rule
                // out ones that already told us something else.
                if props.local_name.is_some() || !props.services.is_empty() {
                    rejected.insert(id);
                }
                continue;
            }
            let id = id.to_string();
            match found.iter_mut().find(|f| f.id == id) {
                Some(f) => f.rssi = props.rssi.or(f.rssi),
                None => found.push(Found {
                    name: props.local_name.unwrap_or_else(|| "?".into()),
                    id,
                    rssi: props.rssi,
                    peripheral: p,
                }),
            }
        }
        if (!found.is_empty() && !all) || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = adapter.stop_scan().await;
    Ok(found)
}

/// Start discovery. "Already in progress" (another app is scanning, or BlueZ
/// is still stopping our previous session) is fine: results still arrive.
async fn start_scan(adapter: &Adapter) -> Result<()> {
    // Linux: no UUID filter. BlueZ 5.87 segfaults in its filtered-discovery
    // path (device_found_callback); we match the service ourselves instead.
    let filter = || ScanFilter {
        services: if cfg!(target_os = "linux") {
            vec![]
        } else {
            vec![SERVICE]
        },
    };
    for _ in 0..3 {
        match adapter.start_scan(filter()).await {
            Ok(()) => return Ok(()),
            Err(e) if e.to_string().to_ascii_lowercase().contains("in progress") => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => bail!("could not start BLE scan (is Bluetooth on?): {e}"),
        }
    }
    Ok(()) // someone else's discovery is still running; use it
}

pub struct Stand {
    p: Peripheral,
    chars: Vec<Characteristic>,
    pub name: String,
}

impl Stand {
    /// Find, connect, discover services and (if `key` is given) authenticate.
    pub async fn connect(
        device: Option<&str>,
        key: Option<&Key>,
        timeout: Duration,
    ) -> Result<Self> {
        let adapter = adapter().await?;
        let mut found = scan(&adapter, timeout, device.is_some()).await?;
        if let Some(d) = device {
            found.retain(|f| f.id.eq_ignore_ascii_case(d) || f.name == d);
        }
        let f = found.into_iter().next().ok_or_else(|| {
            anyhow!("no stand found within {timeout:?} (powered? in range? already connected elsewhere?)")
        })?;
        Self::open(f.peripheral, f.name, key).await
    }

    pub async fn open(p: Peripheral, name: String, key: Option<&Key>) -> Result<Self> {
        let was_connected = p.is_connected().await.unwrap_or(false);
        let r = async {
            if !was_connected {
                p.connect_with_timeout(Duration::from_secs(15))
                    .await
                    .context("connect failed")?;
            }
            p.discover_services_with_timeout(Duration::from_secs(15))
                .await
                .context("service discovery failed")?;
            let chars: Vec<_> = p.characteristics().into_iter().collect();
            if !chars.iter().any(|c| c.service_uuid == SERVICE) {
                bail!("device has no fan stand service");
            }
            let s = Self {
                p: p.clone(),
                chars,
                name,
            };
            if let Some(k) = key {
                s.authenticate(k).await?;
            }
            Ok(s)
        }
        .await;
        if r.is_err() && !was_connected {
            // Cancel the connection we started. Otherwise BlueZ keeps a
            // pending connect alive and every retry fails with "In Progress".
            let _ = p.disconnect().await;
        }
        r
    }

    fn chr(&self, uuid: u128) -> Result<&Characteristic> {
        let u = Uuid::from_u128(uuid);
        self.chars
            .iter()
            .find(|c| c.uuid == u)
            .ok_or_else(|| anyhow!("characteristic {u} missing (old firmware?)"))
    }

    pub async fn read(&self, uuid: u128) -> Result<Vec<u8>> {
        Ok(self.p.read(self.chr(uuid)?).await?)
    }

    /// Read and decode with one of the protocol codecs.
    async fn get<T>(
        &self,
        uuid: u128,
        decode: impl FnOnce(&[u8]) -> Result<T, DecodeError>,
    ) -> Result<T> {
        Ok(decode(&self.read(uuid).await?)?)
    }

    async fn write(&self, uuid: u128, data: &[u8]) -> Result<()> {
        self.p
            .write(self.chr(uuid)?, data, WriteType::WithResponse)
            .await
            .map_err(|e| anyhow!("{}", explain_att(&e.to_string())))
    }

    async fn authenticate(&self, key: &Key) -> Result<()> {
        let nonce: [u8; NONCE_LEN] = self
            .read(uuids::AUTH_CHALLENGE)
            .await?
            .try_into()
            .map_err(|_| anyhow!("bad challenge length"))?;
        self.write(uuids::AUTH_RESPONSE, &compute_response(key, &nonce))
            .await
            .context("authentication rejected (does this key match the one the firmware was built with?)")
    }

    pub async fn is_connected(&self) -> Result<bool> {
        Ok(self.p.is_connected().await?)
    }

    pub async fn disconnect(&self) {
        let _ = self.p.disconnect().await;
    }

    // ------------------------------------------------------------ typed reads

    pub async fn firmware_version(&self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.read(uuids::FIRMWARE_VERSION).await?).into_owned())
    }
    pub async fn mode(&self) -> Result<Mode> {
        self.get(uuids::MODE, Mode::decode).await
    }
    pub async fn manual_pct(&self) -> Result<u8> {
        self.get(uuids::MANUAL_SPEED, decode_percent).await
    }
    pub async fn rpm(&self) -> Result<[u16; 2]> {
        self.get(uuids::FAN_RPM, decode_rpm).await
    }
    pub async fn status(&self) -> Result<Status> {
        self.get(uuids::STATUS, Status::decode).await
    }
    pub async fn curve(&self) -> Result<FanCurve> {
        self.get(uuids::FAN_CURVE, FanCurve::decode).await
    }
    pub async fn config(&self) -> Result<Config> {
        self.get(uuids::CONFIG, Config::decode).await
    }
    pub async fn cal_result(&self) -> Result<CalResult> {
        self.get(uuids::CAL_RESULT, CalResult::decode).await
    }
    pub async fn cal_table(&self) -> Result<[[u16; 2]; STEPS]> {
        self.get(uuids::CAL_TABLE, decode_cal_table).await
    }
    pub async fn debug(&self) -> Result<DebugInfo> {
        self.get(uuids::DEBUG, DebugInfo::decode).await
    }

    // ------------------------------------------------------------ typed writes

    pub async fn set_mode(&self, m: Mode) -> Result<()> {
        self.write(uuids::MODE, &m.encode()).await
    }
    pub async fn set_manual_pct(&self, pct: u8) -> Result<()> {
        self.write(uuids::MANUAL_SPEED, &encode_percent(pct)).await
    }
    pub async fn set_curve(&self, c: &FanCurve) -> Result<()> {
        let (b, n) = c.encode();
        self.write(uuids::FAN_CURVE, &b[..n]).await
    }
    pub async fn set_config(&self, c: &Config) -> Result<()> {
        self.write(uuids::CONFIG, &c.encode()).await
    }
    pub async fn command(&self, c: Command) -> Result<()> {
        self.write(uuids::COMMAND, &c.encode()).await
    }
    /// Without response: cheap, and fine to lose one (the next comes in 2 s).
    pub async fn send_temperature(&self, temp_dc: i16) -> Result<()> {
        self.p
            .write(
                self.chr(uuids::TEMPERATURE)?,
                &encode_temperature(temp_dc),
                WriteType::WithoutResponse,
            )
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------ notifications

    pub async fn subscribe(&self, uuid: u128) -> Result<()> {
        Ok(self.p.subscribe(self.chr(uuid)?).await?)
    }
    pub async fn notifications(&self) -> Result<impl Stream<Item = ValueNotification> + use<>> {
        Ok(self.p.notifications().await?.boxed())
    }
}

/// The ATT error code in an OS error message ("... ATT error: 0x81" on BlueZ;
/// other platforms print the code as a standalone hex token). btleplug has no
/// typed ATT error, so the text is all we get.
fn att_code(msg: &str) -> Option<u8> {
    let hex_byte = |s: &str| {
        let h: String = s.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        (h.len() == 2)
            .then(|| u8::from_str_radix(&h, 16).ok())
            .flatten()
    };
    let lower = msg.to_ascii_lowercase();
    if let Some(i) = lower.find("att error: 0x") {
        return hex_byte(&lower[i + "att error: 0x".len()..]);
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find_map(|tok| hex_byte(tok.strip_prefix("0x")?))
}

/// Add a readable hint for the stand's own ATT error codes.
fn explain_att(msg: &str) -> String {
    match att_code(msg).and_then(att_error::describe) {
        Some(hint) => format!("{msg} ({hint})"),
        None => msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn att_code_parsing() {
        assert_eq!(
            att_code("Operation failed with ATT error: 0x82"),
            Some(att_error::AUTH_FAILED)
        );
        assert_eq!(
            att_code("ProtocolError(0x81)"),
            Some(att_error::INVALID_VALUE)
        );
        assert_eq!(att_code("handle 0x8000 timed out"), None); // not a byte
        assert_eq!(att_code("no code here"), None);
        assert!(explain_att("ATT error: 0x80").ends_with("(not authenticated)"));
        assert_eq!(explain_att("ATT error: 0x0e"), "ATT error: 0x0e");
    }
}
