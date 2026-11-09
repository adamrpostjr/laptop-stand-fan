//! Linux: hwmon / sysfs (world-readable, no root).
//!
//! Sensors are found by driver **name**, never by `hwmonN` number (the
//! numbers change between boots). Preference order:
//! 1. `k10temp` / `zenpower` (AMD): `Tctl`, else `Tdie`, else the first input
//! 2. `coretemp` (Intel): `Package id 0`, else the hottest core
//! 3. thermal zones of type `x86_pkg_temp`, `cpu-thermal`, `cpu_thermal`, `soc_thermal`
//!
//! Generic `acpitz` zones are skipped: they're often a motherboard sensor, not
//! the CPU. If nothing matches, the caller falls back to the load estimate.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Reading, Source};

pub struct SysfsTemp {
    path: PathBuf,
    desc: String,
}

impl Source for SysfsTemp {
    fn describe(&self) -> String {
        self.desc.clone()
    }

    fn read(&mut self) -> Result<Reading> {
        let milli: i64 = read_trim(&self.path)?
            .parse()
            .with_context(|| format!("bad value in {}", self.path.display()))?;
        let c = milli as f32 / 1000.0;
        if !(-40.0..=150.0).contains(&c) {
            bail!("implausible reading {c} °C from {}", self.path.display());
        }
        Ok(Reading {
            celsius: c,
            estimated: false,
        })
    }
}

fn read_trim(p: &Path) -> Result<String> {
    Ok(fs::read_to_string(p)
        .with_context(|| format!("read {}", p.display()))?
        .trim()
        .to_string())
}

fn sorted_dirs(dir: &str) -> Vec<PathBuf> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// `(label, input path)` for every `tempN_input` of a hwmon device.
fn temp_inputs(hwmon: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<_> = sorted_dirs(&hwmon.to_string_lossy())
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_string();
            let idx = name
                .strip_prefix("temp")?
                .strip_suffix("_input")?
                .to_string();
            let label = read_trim(&hwmon.join(format!("temp{idx}_label"))).unwrap_or_default();
            Some((label, p))
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

pub fn detect() -> Option<SysfsTemp> {
    let hwmons: Vec<(String, PathBuf)> = sorted_dirs("/sys/class/hwmon")
        .into_iter()
        .filter_map(|p| Some((read_trim(&p.join("name")).ok()?, p)))
        .collect();

    let pick = |driver: &str, labels: &[&str]| -> Option<SysfsTemp> {
        let (_, dir) = hwmons.iter().find(|(n, _)| n == driver)?;
        let inputs = temp_inputs(dir);
        let (label, path) = labels
            .iter()
            .find_map(|want| inputs.iter().find(|(l, _)| l == want))
            .or_else(|| inputs.first())?;
        Some(SysfsTemp {
            desc: format!(
                "{driver} {} ({})",
                if label.is_empty() { "temp" } else { label },
                path.display()
            ),
            path: path.clone(),
        })
    };

    let found = pick("k10temp", &["Tctl", "Tdie"])
        .or_else(|| pick("zenpower", &["Tctl", "Tdie"]))
        .or_else(|| pick("coretemp", &["Package id 0"]));
    if let Some(s) = found.filter(|s| s.path.exists()) {
        return Some(s);
    }

    for z in sorted_dirs("/sys/class/thermal") {
        let Ok(kind) = read_trim(&z.join("type")) else {
            continue;
        };
        if matches!(
            kind.as_str(),
            "x86_pkg_temp" | "cpu-thermal" | "cpu_thermal" | "soc_thermal"
        ) {
            return Some(SysfsTemp {
                desc: format!("thermal zone {kind} ({})", z.join("temp").display()),
                path: z.join("temp"),
            });
        }
    }
    None
}
