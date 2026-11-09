//! Persistent settings and their NVS blob format.
//!
//! Blob layout (28 bytes, little-endian):
//! ```text
//! 0  'F' 'S'           magic
//! 2  version (=1)
//! 3  mode
//! 4  manual %
//! 5  safe speed %
//! 6  min duty %
//! 7  failsafe timeout s (u16)
//! 9  curve point count n
//! 10 16 bytes: n × (temp °C, duty %), rest zero
//! 26 CRC-16/CCITT-FALSE over bytes 0..26 (u16)
//! ```

use crate::codec::{Config, DecodeError, Mode};
use crate::crc::crc16_ccitt;
use crate::curve::{FanCurve, MAX_ENCODED_LEN};

pub const BLOB_LEN: usize = 28;
const MAGIC: [u8; 2] = *b"FS";
const VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub manual_pct: u8,
    pub curve: FanCurve,
    pub config: Config,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            manual_pct: 50,
            curve: FanCurve::default(),
            config: Config::default(),
        }
    }
}

impl Settings {
    pub fn to_blob(&self) -> [u8; BLOB_LEN] {
        let mut b = [0u8; BLOB_LEN];
        b[0..2].copy_from_slice(&MAGIC);
        b[2] = VERSION;
        b[3] = self.mode as u8;
        b[4] = self.manual_pct;
        b[5..9].copy_from_slice(&self.config.encode());
        let (curve, n) = self.curve.encode();
        b[9] = (n / 2) as u8;
        b[10..10 + n].copy_from_slice(&curve[..n]);
        let crc = crc16_ccitt(&b[..26]);
        b[26..28].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn from_blob(b: &[u8]) -> Result<Self, DecodeError> {
        if b.len() != BLOB_LEN {
            return Err(DecodeError::WrongLength);
        }
        if b[0..2] != MAGIC || b[2] != VERSION {
            return Err(DecodeError::BadHeader);
        }
        if crc16_ccitt(&b[..26]).to_le_bytes() != [b[26], b[27]] {
            return Err(DecodeError::BadChecksum);
        }
        let mode = Mode::from_u8(b[3])?;
        let manual_pct = crate::codec::decode_percent(&b[4..5])?;
        let config = Config::decode(&b[5..9])?;
        let n = b[9] as usize * 2;
        if n > MAX_ENCODED_LEN {
            return Err(DecodeError::WrongLength);
        }
        let curve = FanCurve::decode(&b[10..10 + n])?;
        Ok(Self {
            mode,
            manual_pct,
            curve,
            config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::CurvePoint;

    #[test]
    fn roundtrip_default() {
        let s = Settings::default();
        assert_eq!(Settings::from_blob(&s.to_blob()), Ok(s));
    }

    #[test]
    fn roundtrip_custom() {
        let s = Settings {
            mode: Mode::Manual,
            manual_pct: 77,
            curve: FanCurve::new(&[CurvePoint::new(30, 10), CurvePoint::new(90, 100)]).unwrap(),
            config: Config {
                safe_speed_pct: 80,
                min_duty_pct: 15,
                failsafe_timeout_s: 120,
            },
        };
        assert_eq!(Settings::from_blob(&s.to_blob()), Ok(s));
    }

    #[test]
    fn rejects_corruption() {
        let good = Settings::default().to_blob();
        for i in 0..BLOB_LEN {
            let mut b = good;
            b[i] ^= 0x01;
            assert!(Settings::from_blob(&b).is_err(), "byte {i} flip accepted");
        }
        assert_eq!(
            Settings::from_blob(&good[..27]),
            Err(DecodeError::WrongLength)
        );
        let mut b = good;
        b[2] = 2;
        assert_eq!(Settings::from_blob(&b), Err(DecodeError::BadHeader));
    }

    #[test]
    fn rejects_invalid_values_even_with_good_crc() {
        let mut b = Settings::default().to_blob();
        b[5] = 10; // safe speed 10 % < 30 % minimum
        let crc = crc16_ccitt(&b[..26]);
        b[26..28].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Settings::from_blob(&b), Err(DecodeError::OutOfRange));
    }
}
