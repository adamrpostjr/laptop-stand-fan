//! macOS: sysinfo "components", which need no sudo:
//! - Apple Silicon: IOHID temperature sensors (`kHIDPage_AppleVendor`), e.g.
//!   "PMU tdie1..N" (CPU die). Some generations name them "pACC/eACC MTR Temp Sensor".
//! - Intel: SMC keys `TCXC` / `TC0P` ("CPU PECI", "CPU Proximity").
//!
//! We take the hottest CPU-ish sensor, preferring die sensors.

use anyhow::{Result, bail};
use sysinfo::Components;

use super::{Reading, Source};

/// Label fragments (lower-case), best first.
const PREFERRED: &[&[&str]] = &[&["tdie"], &["pacc", "eacc", "cpu"]];

pub struct MacSensors {
    components: Components,
    pattern: &'static [&'static str],
}

fn matches(label: &str, pattern: &[&str]) -> bool {
    let l = label.to_ascii_lowercase();
    pattern.iter().any(|p| l.contains(p)) && !l.contains("battery")
}

pub fn detect() -> Option<MacSensors> {
    let components = Components::new_with_refreshed_list().ok()?;
    for pattern in PREFERRED {
        let ok = components
            .iter()
            .any(|c| matches(c.label(), pattern) && c.temperature().is_some_and(|t| t > 0.0));
        if ok {
            return Some(MacSensors {
                components,
                pattern,
            });
        }
    }
    None
}

impl Source for MacSensors {
    fn describe(&self) -> String {
        let labels: Vec<_> = self
            .components
            .iter()
            .filter(|c| matches(c.label(), self.pattern))
            .map(|c| c.label().to_string())
            .collect();
        format!("macOS sensors, hottest of: {}", labels.join(", "))
    }

    fn read(&mut self) -> Result<Reading> {
        self.components.refresh(false);
        let t = self
            .components
            .iter()
            .filter(|c| matches(c.label(), self.pattern))
            .filter_map(|c| c.temperature())
            .filter(|t| (1.0..=150.0).contains(t))
            .fold(f32::NAN, f32::max);
        if t.is_nan() {
            bail!("no plausible CPU sensor reading");
        }
        Ok(Reading {
            celsius: t,
            estimated: false,
        })
    }
}
