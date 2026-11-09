//! Fallback: a temperature **estimated** from CPU load, for machines where no
//! reliable sensor can be read without admin rights or drivers.
//!
//! `estimate = IDLE_C + (FULL_C - IDLE_C) × load`, smoothed (the real
//! temperature lags load by seconds). It's deliberately on the warm side: a
//! wrong guess should mean more airflow, not less.

use anyhow::{Result, anyhow};
use sysinfo::{CpuRefreshKind, RefreshKind, System};

use super::{Reading, Source};

const IDLE_C: f32 = 45.0;
const FULL_C: f32 = 90.0;
/// EMA weight per reading (readings every ~2 s → ~6 s time constant).
const ALPHA: f32 = 0.3;

pub struct LoadEstimate {
    /// `None` if the OS refused CPU statistics; `read()` then errors and the
    /// stand falls back to its safe speed.
    sys: Option<System>,
    smoothed: Option<f32>,
}

impl LoadEstimate {
    pub fn new() -> Self {
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_usage()),
        )
        .inspect_err(|e| log::error!("CPU statistics unavailable: {e}"))
        .ok();
        // The first usage sample is meaningless; prime it now so the first read() is valid
        // (reads are ≥ MINIMUM_CPU_UPDATE_INTERVAL apart in practice).
        if let Some(s) = sys.as_mut() {
            s.refresh_cpu_usage();
        }
        Self {
            sys,
            smoothed: None,
        }
    }
}

impl Source for LoadEstimate {
    fn describe(&self) -> String {
        format!(
            "ESTIMATE from CPU load ({IDLE_C:.0} °C idle → {FULL_C:.0} °C at 100%): no CPU temperature sensor readable without admin"
        )
    }

    fn read(&mut self) -> Result<Reading> {
        let sys = self
            .sys
            .as_mut()
            .ok_or_else(|| anyhow!("CPU statistics unavailable"))?;
        sys.refresh_cpu_usage();
        let load = (sys.global_cpu_usage() / 100.0).clamp(0.0, 1.0);
        let target = IDLE_C + (FULL_C - IDLE_C) * load;
        let v = match self.smoothed {
            None => target,
            Some(s) => s + ALPHA * (target - s),
        };
        self.smoothed = Some(v);
        Ok(Reading {
            celsius: v,
            estimated: true,
        })
    }
}
