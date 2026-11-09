//! Diagnostics over BLE (so the stand can be debugged without USB serial):
//! the `DEBUG` and `CAL_TABLE` characteristics, plus the app-level ATT
//! error codes.

use crate::calibration::STEPS;
use crate::codec::{DecodeError, decode_rpm, encode_rpm};

/// ATT application error codes (0x80–0x9F) returned by rejected writes.
///
/// "Not authenticated" deliberately does *not* use ATT 0x05 (Insufficient
/// Authentication): that code makes BlueZ, Windows and macOS start OS-level
/// pairing on their own.
pub mod att_error {
    /// Write needs a successful `AUTH_RESPONSE` on this connection first.
    pub const NOT_AUTHENTICATED: u8 = 0x80;
    /// Value failed validation (range, length, curve order...).
    pub const INVALID_VALUE: u8 = 0x81;
    /// Wrong HMAC in `AUTH_RESPONSE`.
    pub const AUTH_FAILED: u8 = 0x82;

    /// Meaning of one of our codes, for client error messages.
    pub const fn describe(code: u8) -> Option<&'static str> {
        match code {
            NOT_AUTHENTICATED => Some("not authenticated"),
            INVALID_VALUE => Some("value rejected by the stand: out of range or invalid"),
            AUTH_FAILED => Some("wrong key"),
            _ => None,
        }
    }
}

/// Live diagnostics, 24 bytes LE (the firmware asks for a 185-byte MTU;
/// at the default MTU, clients fall back to a long read).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DebugInfo {
    pub uptime_s: u32,
    /// ESP-IDF `esp_reset_reason_t`; see [`reset_reason_name`].
    pub reset_reason: u8,
    pub duty_permille: u16,
    pub target_permille: u16,
    /// Smoothed temperature, deci-°C.
    pub temp_dc: Option<i16>,
    pub temp_age_s: Option<u16>,
    /// Unsmoothed RPM of the last 1 s window.
    pub raw_rpm: [u16; 2],
    pub flags: u8,
    pub free_heap: u32,
    /// Tach line probe: how many of the last ~10 samples (one per 100 ms
    /// control tick) read logic high. ~5 = toggling, 10 = stuck high
    /// (nothing pulling it low), 0 = stuck low.
    pub tach_high_samples: [u8; 2],
}

impl DebugInfo {
    pub const LEN: usize = 24;
    pub const FLAG_GRACE: u8 = 1 << 0;
    /// Firmware built with the `no-auth` feature (test builds only).
    pub const FLAG_NO_AUTH: u8 = 1 << 1;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&self.uptime_s.to_le_bytes());
        b[4] = self.reset_reason;
        b[5..7].copy_from_slice(&self.duty_permille.to_le_bytes());
        b[7..9].copy_from_slice(&self.target_permille.to_le_bytes());
        b[9..11].copy_from_slice(&self.temp_dc.unwrap_or(i16::MIN).to_le_bytes());
        b[11..13].copy_from_slice(&self.temp_age_s.unwrap_or(u16::MAX).to_le_bytes());
        b[13..15].copy_from_slice(&self.raw_rpm[0].to_le_bytes());
        b[15..17].copy_from_slice(&self.raw_rpm[1].to_le_bytes());
        b[17] = self.flags;
        b[18..22].copy_from_slice(&self.free_heap.to_le_bytes());
        b[22] = self.tach_high_samples[0];
        b[23] = self.tach_high_samples[1];
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        if b.len() != Self::LEN {
            return Err(DecodeError::WrongLength);
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let temp = i16::from_le_bytes([b[9], b[10]]);
        let age = u16_at(11);
        Ok(Self {
            uptime_s: u32_at(0),
            reset_reason: b[4],
            duty_permille: u16_at(5),
            target_permille: u16_at(7),
            temp_dc: (temp != i16::MIN).then_some(temp),
            temp_age_s: (age != u16::MAX).then_some(age),
            raw_rpm: [u16_at(13), u16_at(15)],
            flags: b[17],
            free_heap: u32_at(18),
            tach_high_samples: [b[22], b[23]],
        })
    }
}

/// Names for ESP-IDF's `esp_reset_reason_t` (stable across IDF 5.x).
pub const fn reset_reason_name(code: u8) -> &'static str {
    match code {
        1 => "power-on",
        2 => "external pin",
        3 => "software",
        4 => "panic",
        5 => "interrupt watchdog",
        6 => "TASK WATCHDOG (control loop hung)",
        7 => "other watchdog",
        8 => "deep sleep",
        9 => "BROWNOUT (check 5 V supply)",
        10 => "SDIO",
        _ => "unknown",
    }
}

pub const CAL_TABLE_LEN: usize = STEPS * 4;

pub fn encode_cal_table(t: &[[u16; 2]; STEPS]) -> [u8; CAL_TABLE_LEN] {
    let mut b = [0u8; CAL_TABLE_LEN];
    for (chunk, row) in b.chunks_exact_mut(4).zip(t) {
        chunk.copy_from_slice(&encode_rpm(*row));
    }
    b
}

pub fn decode_cal_table(b: &[u8]) -> Result<[[u16; 2]; STEPS], DecodeError> {
    if b.len() != CAL_TABLE_LEN {
        return Err(DecodeError::WrongLength);
    }
    let mut t = [[0u16; 2]; STEPS];
    for (row, chunk) in t.iter_mut().zip(b.chunks_exact(4)) {
        *row = decode_rpm(chunk)?;
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_roundtrip() {
        let d = DebugInfo {
            uptime_s: 123_456,
            reset_reason: 6,
            duty_permille: 600,
            target_permille: 650,
            temp_dc: Some(-55),
            temp_age_s: Some(3),
            raw_rpm: [1100, 1090],
            flags: DebugInfo::FLAG_GRACE,
            free_heap: 150_000,
            tach_high_samples: [5, 10],
        };
        assert_eq!(DebugInfo::decode(&d.encode()), Ok(d));
        let none = DebugInfo {
            temp_dc: None,
            temp_age_s: None,
            ..d
        };
        assert_eq!(DebugInfo::decode(&none.encode()), Ok(none));
        assert_eq!(DebugInfo::decode(&[0; 23]), Err(DecodeError::WrongLength));
    }

    #[test]
    fn cal_table_roundtrip() {
        let mut t = [[0u16; 2]; STEPS];
        for (i, s) in t.iter_mut().enumerate() {
            *s = [i as u16 * 70, 1550 - i as u16 * 50];
        }
        assert_eq!(decode_cal_table(&encode_cal_table(&t)), Ok(t));
        assert_eq!(CAL_TABLE_LEN, 84);
    }
}
