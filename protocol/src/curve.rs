//! Fan curve: up to 8 (°C, duty %) points with linear interpolation.
//!
//! Wire format: `n` × `[temp_c: u8, duty_pct: u8]`, 1 ≤ n ≤ 8, so 2..=16 bytes.
//!
//! Validation (enforced by every constructor, so a `FanCurve` is always valid):
//! - 1..=8 points
//! - temperatures ≤ [`MAX_CURVE_TEMP_C`] and strictly increasing
//! - duty ≤ 100 % and **non-decreasing**, so a hotter CPU never gets
//!   less airflow

use crate::codec::DecodeError;

pub const MAX_POINTS: usize = 8;
pub const MAX_ENCODED_LEN: usize = MAX_POINTS * 2;
pub const MAX_CURVE_TEMP_C: u8 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CurvePoint {
    pub temp_c: u8,
    pub duty_pct: u8,
}

impl CurvePoint {
    pub const fn new(temp_c: u8, duty_pct: u8) -> Self {
        Self { temp_c, duty_pct }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanCurve {
    points: [CurvePoint; MAX_POINTS],
    len: u8,
}

impl Default for FanCurve {
    /// Quiet at idle, full speed at 85 °C.
    fn default() -> Self {
        Self::new(&[
            CurvePoint::new(40, 20),
            CurvePoint::new(50, 30),
            CurvePoint::new(65, 50),
            CurvePoint::new(75, 75),
            CurvePoint::new(85, 100),
        ])
        .expect("default curve is valid")
    }
}

impl FanCurve {
    pub fn new(points: &[CurvePoint]) -> Result<Self, DecodeError> {
        if points.is_empty() || points.len() > MAX_POINTS {
            return Err(DecodeError::WrongLength);
        }
        for p in points {
            if p.temp_c > MAX_CURVE_TEMP_C || p.duty_pct > 100 {
                return Err(DecodeError::OutOfRange);
            }
        }
        for w in points.windows(2) {
            if w[1].temp_c <= w[0].temp_c || w[1].duty_pct < w[0].duty_pct {
                return Err(DecodeError::NotSorted);
            }
        }
        let mut arr = [CurvePoint::default(); MAX_POINTS];
        arr[..points.len()].copy_from_slice(points);
        Ok(Self {
            points: arr,
            len: points.len() as u8,
        })
    }

    pub fn points(&self) -> &[CurvePoint] {
        &self.points[..self.len as usize]
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        if b.is_empty() || !b.len().is_multiple_of(2) || b.len() > MAX_ENCODED_LEN {
            return Err(DecodeError::WrongLength);
        }
        let mut pts = [CurvePoint::default(); MAX_POINTS];
        let n = b.len() / 2;
        for (i, c) in b.chunks_exact(2).enumerate() {
            pts[i] = CurvePoint::new(c[0], c[1]);
        }
        Self::new(&pts[..n])
    }

    /// Returns the buffer and the number of valid bytes in it.
    pub fn encode(&self) -> ([u8; MAX_ENCODED_LEN], usize) {
        let mut out = [0u8; MAX_ENCODED_LEN];
        for (i, p) in self.points().iter().enumerate() {
            out[2 * i] = p.temp_c;
            out[2 * i + 1] = p.duty_pct;
        }
        (out, self.len as usize * 2)
    }

    /// Duty (permille) for a temperature in deci-°C. Clamps to the first/last
    /// point outside the curve and interpolates linearly in between.
    pub fn duty_permille_at(&self, temp_dc: i16) -> u16 {
        let pts = self.points();
        let t = temp_dc as i32;
        let first = pts[0];
        if t <= first.temp_c as i32 * 10 {
            return first.duty_pct as u16 * 10;
        }
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (ta, tb) = (a.temp_c as i32 * 10, b.temp_c as i32 * 10);
            if t <= tb {
                let (da, db) = (a.duty_pct as i32 * 10, b.duty_pct as i32 * 10);
                // Round to nearest; tb > ta is guaranteed by validation.
                let num = (db - da) * (t - ta);
                let span = tb - ta;
                return (da + (num + span / 2) / span) as u16;
            }
        }
        pts[pts.len() - 1].duty_pct as u16 * 10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(points: &[(u8, u8)]) -> Result<FanCurve, DecodeError> {
        let v: std::vec::Vec<_> = points.iter().map(|&(t, d)| CurvePoint::new(t, d)).collect();
        FanCurve::new(&v)
    }

    #[test]
    fn validation() {
        assert!(c(&[(50, 50)]).is_ok());
        assert_eq!(c(&[]), Err(DecodeError::WrongLength));
        assert_eq!(
            c(&[
                (1, 1),
                (2, 2),
                (3, 3),
                (4, 4),
                (5, 5),
                (6, 6),
                (7, 7),
                (8, 8),
                (9, 9)
            ]),
            Err(DecodeError::WrongLength)
        );
        assert!(
            c(&[
                (1, 1),
                (2, 2),
                (3, 3),
                (4, 4),
                (5, 5),
                (6, 6),
                (7, 7),
                (8, 8)
            ])
            .is_ok()
        );
        assert_eq!(c(&[(121, 50)]), Err(DecodeError::OutOfRange));
        assert_eq!(c(&[(50, 101)]), Err(DecodeError::OutOfRange));
        assert_eq!(c(&[(50, 50), (50, 60)]), Err(DecodeError::NotSorted));
        assert_eq!(c(&[(60, 50), (50, 60)]), Err(DecodeError::NotSorted));
        assert_eq!(c(&[(50, 60), (60, 50)]), Err(DecodeError::NotSorted));
        assert!(c(&[(50, 60), (60, 60)]).is_ok()); // flat segment allowed
    }

    #[test]
    fn codec_roundtrip() {
        let curve = FanCurve::default();
        let (buf, n) = curve.encode();
        assert_eq!(n, 10);
        assert_eq!(&buf[..4], &[40, 20, 50, 30]);
        assert_eq!(FanCurve::decode(&buf[..n]), Ok(curve));
        assert_eq!(FanCurve::decode(&buf[..3]), Err(DecodeError::WrongLength));
        assert_eq!(FanCurve::decode(&[]), Err(DecodeError::WrongLength));
        assert_eq!(FanCurve::decode(&[0u8; 18]), Err(DecodeError::WrongLength));
        assert_eq!(
            FanCurve::decode(&[60, 50, 50, 60]),
            Err(DecodeError::NotSorted)
        );
    }

    #[test]
    fn interpolation() {
        let curve = c(&[(40, 20), (60, 60), (80, 100)]).unwrap();
        assert_eq!(curve.duty_permille_at(-400), 200);
        assert_eq!(curve.duty_permille_at(400), 200);
        assert_eq!(curve.duty_permille_at(500), 400);
        assert_eq!(curve.duty_permille_at(600), 600);
        assert_eq!(curve.duty_permille_at(605), 610);
        assert_eq!(curve.duty_permille_at(700), 800);
        assert_eq!(curve.duty_permille_at(800), 1000);
        assert_eq!(curve.duty_permille_at(1500), 1000);
        // Rounding: 40→60 °C maps 200→600‰, so 0.1 °C = 2‰.
        assert_eq!(curve.duty_permille_at(401), 202);
    }

    #[test]
    fn interpolation_is_monotonic() {
        let curve = FanCurve::default();
        let mut prev = 0;
        for t in -400..=1500 {
            let d = curve.duty_permille_at(t);
            assert!(d >= prev, "non-monotonic at {t}");
            assert!(d <= 1000);
            prev = d;
        }
    }

    #[test]
    fn single_point_curve_is_flat() {
        let curve = c(&[(50, 70)]).unwrap();
        assert_eq!(curve.duty_permille_at(0), 700);
        assert_eq!(curve.duty_permille_at(1000), 700);
    }
}
