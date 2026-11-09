//! Shared protocol and pure control logic for the BLE cooling laptop stand.
//!
//! This crate is `no_std` so the ESP32 firmware and the desktop controller can
//! both use it. Nothing in it touches hardware or a clock: every time-dependent
//! function takes `now_ms: u64` (monotonic milliseconds), so the logic can be
//! unit-tested on the host.
//!
//! Units used throughout:
//! - duty: **permille** (0..=1000) internally, **percent** (0..=100) on the wire
//! - temperature: **deci-°C** (0.1 °C, `i16`) on the wire and internally;
//!   fan-curve points use whole °C
//! - fan speed: RPM (`u16`)

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod auth;
pub mod calibration;
pub mod codec;
pub mod control;
pub mod crc;
pub mod curve;
pub mod diag;
pub mod led;
pub mod ramp;
pub mod rpm;
pub mod settings;
pub mod stall;
pub mod temperature;
pub mod uuids;

pub use codec::{CalResult, Command, Config, DecodeError, Mode, Status};
pub use control::{ControlParams, Controller, Output};
pub use curve::{CurvePoint, FanCurve};
pub use settings::Settings;

/// Number of fans on the stand.
pub const FAN_COUNT: usize = 2;

/// Convert a percentage (clamped to 100) to permille.
pub const fn pct_to_permille(pct: u8) -> u16 {
    let p = if pct > 100 { 100 } else { pct };
    p as u16 * 10
}

/// Convert permille to the nearest whole percent.
pub const fn permille_to_pct(permille: u16) -> u8 {
    let p = if permille > 1000 { 1000 } else { permille };
    ((p + 5) / 10) as u8
}

/// Displays a permille duty as a percentage with one decimal, e.g. `62.5%`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permille(pub u16);

impl core::fmt::Display for Permille {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}%", self.0 / 10, self.0 % 10)
    }
}

/// Displays deci-°C as `45.6 °C`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeciCelsius(pub i16);

impl core::fmt::Display for DeciCelsius {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let a = self.0.unsigned_abs();
        write!(f, "{sign}{}.{} °C", a / 10, a % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_display() {
        assert_eq!(std::format!("{}", Permille(625)), "62.5%");
        assert_eq!(std::format!("{}", Permille(1000)), "100.0%");
        assert_eq!(std::format!("{}", DeciCelsius(456)), "45.6 °C");
        assert_eq!(std::format!("{}", DeciCelsius(-5)), "-0.5 °C");
    }

    #[test]
    fn pct_permille_roundtrip() {
        for p in 0..=100u8 {
            assert_eq!(permille_to_pct(pct_to_permille(p)), p);
        }
        assert_eq!(pct_to_permille(150), 1000);
        assert_eq!(permille_to_pct(2000), 100);
        assert_eq!(permille_to_pct(254), 25);
        assert_eq!(permille_to_pct(255), 26);
    }
}
