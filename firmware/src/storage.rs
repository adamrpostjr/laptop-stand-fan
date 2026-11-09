//! Settings persistence in NVS: one checksummed blob (format in `fanstand_protocol::settings`).

use anyhow::Result;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use fanstand_protocol::Settings;
use fanstand_protocol::settings::BLOB_LEN;
use log::{info, warn};

const NAMESPACE: &str = "fanstand";
const KEY: &str = "settings";

pub struct Storage {
    nvs: EspNvs<NvsDefault>,
}

impl Storage {
    pub fn new(partition: EspDefaultNvsPartition) -> Result<Self> {
        Ok(Self {
            nvs: EspNvs::new(partition, NAMESPACE, true)?,
        })
    }

    /// Stored settings, or the defaults if there are none or they're invalid.
    pub fn load(&self) -> Settings {
        let mut buf = [0u8; BLOB_LEN + 8];
        match self.nvs.get_blob(KEY, &mut buf) {
            Ok(Some(b)) => match Settings::from_blob(b) {
                Ok(s) => {
                    info!("settings loaded from NVS");
                    s
                }
                Err(e) => {
                    warn!("stored settings invalid ({e}); using defaults");
                    Settings::default()
                }
            },
            Ok(None) => {
                info!("no stored settings; using defaults");
                Settings::default()
            }
            Err(e) => {
                warn!("NVS read failed ({e}); using defaults");
                Settings::default()
            }
        }
    }

    pub fn save(&self, s: &Settings) -> Result<()> {
        self.nvs.set_blob(KEY, &s.to_blob())?;
        Ok(())
    }
}
