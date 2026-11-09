//! CPU temperature, cross-platform, no admin rights or drivers.
//!
//! | OS      | Source                                   | Notes |
//! |---------|------------------------------------------|-------|
//! | Linux   | hwmon `k10temp`/`zenpower`/`coretemp`    | exact CPU sensor |
//! | macOS   | IOHID (Apple Silicon) / SMC (Intel)      | via sysinfo, no sudo |
//! | Windows | PDH `\Thermal Zone Information`          | ACPI zone, *not* the die; guarded |
//! | any     | **estimate from CPU load**               | always labelled ESTIMATED |
//!
//! Measured sources on Windows are wrapped in [`StuckGuard`]: a value that
//! doesn't move at all for 10 minutes is a placeholder, so we switch to the
//! estimate (and say so in the log).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

mod estimate;

use std::time::{Duration, Instant};

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub celsius: f32,
    pub estimated: bool,
}

impl Reading {
    pub fn deci_celsius(&self) -> i16 {
        (self.celsius * 10.0).round().clamp(-400.0, 1500.0) as i16
    }
}

pub trait Source: Send {
    /// Human-readable description, e.g. "k10temp Tctl (/sys/class/hwmon/hwmon6)".
    fn describe(&self) -> String;
    fn read(&mut self) -> Result<Reading>;
}

/// Pick the best source on this machine. `force_estimate` is for testing.
pub fn detect(force_estimate: bool) -> Box<dyn Source> {
    if !force_estimate {
        #[cfg(target_os = "linux")]
        if let Some(s) = linux::detect() {
            return Box::new(s);
        }
        #[cfg(target_os = "macos")]
        if let Some(s) = macos::detect() {
            return Box::new(s);
        }
        #[cfg(target_os = "windows")]
        if let Some(s) = windows::detect() {
            return Box::new(StuckGuard::new(Box::new(s), Duration::from_secs(600)));
        }
    }
    Box::new(estimate::LoadEstimate::new())
}

/// Falls back to the load estimate if the inner source fails repeatedly or
/// reports exactly the same value for `stuck_after`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct StuckGuard {
    inner: Box<dyn Source>,
    fallback: Option<estimate::LoadEstimate>,
    last: Option<(f32, Instant)>,
    failures: u32,
    stuck_after: Duration,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl StuckGuard {
    pub fn new(inner: Box<dyn Source>, stuck_after: Duration) -> Self {
        Self {
            inner,
            fallback: None,
            last: None,
            failures: 0,
            stuck_after,
        }
    }

    fn give_up(&mut self, why: &str) {
        log::warn!(
            "{} looks unreliable ({why}); switching to a temperature ESTIMATED from CPU load",
            self.inner.describe()
        );
        self.fallback = Some(estimate::LoadEstimate::new());
    }
}

impl Source for StuckGuard {
    fn describe(&self) -> String {
        match &self.fallback {
            Some(f) => f.describe(),
            None => format!(
                "{} (falls back to an estimate if it looks stuck)",
                self.inner.describe()
            ),
        }
    }

    fn read(&mut self) -> Result<Reading> {
        if let Some(f) = self.fallback.as_mut() {
            return f.read();
        }
        match self.inner.read() {
            Ok(r) => {
                self.failures = 0;
                match self.last {
                    Some((v, since)) if (v - r.celsius).abs() < 0.05 => {
                        if since.elapsed() >= self.stuck_after {
                            self.give_up(&format!(
                                "stuck at {v:.1} °C for {} min",
                                self.stuck_after.as_secs() / 60
                            ));
                            return self.read();
                        }
                    }
                    _ => self.last = Some((r.celsius, Instant::now())),
                }
                Ok(r)
            }
            Err(e) => {
                self.failures += 1;
                if self.failures >= 5 {
                    self.give_up(&format!("{e:#}"));
                    return self.read();
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Always reads the same value, like a placeholder thermal zone.
    struct Constant(f32);
    impl Source for Constant {
        fn describe(&self) -> String {
            "constant".into()
        }
        fn read(&mut self) -> Result<Reading> {
            Ok(Reading {
                celsius: self.0,
                estimated: false,
            })
        }
    }

    #[test]
    fn deci_celsius_rounds_and_clamps() {
        assert_eq!(
            Reading {
                celsius: 45.66,
                estimated: false
            }
            .deci_celsius(),
            457
        );
        assert_eq!(
            Reading {
                celsius: 999.0,
                estimated: false
            }
            .deci_celsius(),
            1500
        );
        assert_eq!(
            Reading {
                celsius: -99.0,
                estimated: true
            }
            .deci_celsius(),
            -400
        );
    }

    #[test]
    fn stuck_value_switches_to_estimate() {
        let mut g = StuckGuard::new(Box::new(Constant(27.8)), Duration::ZERO);
        assert!(!g.read().unwrap().estimated); // first reading starts the clock
        assert!(g.read().unwrap().estimated); // same value, 0 s allowed → estimate
        assert!(g.describe().contains("ESTIMATE"));
    }

    struct Failing;
    impl Source for Failing {
        fn describe(&self) -> String {
            "failing".into()
        }
        fn read(&mut self) -> Result<Reading> {
            anyhow::bail!("nope")
        }
    }

    #[test]
    fn repeated_failures_switch_to_estimate() {
        let mut g = StuckGuard::new(Box::new(Failing), Duration::from_secs(600));
        for _ in 0..4 {
            assert!(g.read().is_err());
        }
        assert!(g.read().unwrap().estimated);
    }
}
