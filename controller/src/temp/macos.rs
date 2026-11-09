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
    /// Indices of the CPU sensors we use (only these are refreshed).
    cpu: Vec<usize>,
}

fn matches(label: &str, pattern: &[&str]) -> bool {
    let l = label.to_ascii_lowercase();
    pattern.iter().any(|p| l.contains(p)) && !l.contains("battery")
}

pub fn detect() -> Option<MacSensors> {
    let components = Components::new_with_refreshed_list().ok()?;
    for pattern in PREFERRED {
        let cpu: Vec<usize> = components
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                matches(c.label(), pattern) && c.temperature().is_some_and(|t| t > 0.0)
            })
            .map(|(i, _)| i)
            .collect();
        if !cpu.is_empty() {
            return Some(MacSensors { components, cpu });
        }
    }
    None
}

impl Source for MacSensors {
    fn describe(&self) -> String {
        let labels: Vec<_> = self
            .cpu
            .iter()
            .filter_map(|&i| self.components.get(i))
            .map(|c| c.label())
            .collect();
        format!("macOS sensors, hottest of: {}", labels.join(", "))
    }

    fn read(&mut self) -> Result<Reading> {
        let mut t = f32::NAN;
        for (i, c) in self.components.iter_mut().enumerate() {
            if self.cpu.contains(&i) {
                c.refresh();
                if let Some(v) = c.temperature().filter(|v| (1.0..=150.0).contains(v)) {
                    t = t.max(v);
                }
            }
        }
        if t.is_nan() {
            bail!("no plausible CPU sensor reading");
        }
        Ok(Reading {
            celsius: t,
            estimated: false,
        })
    }
}
