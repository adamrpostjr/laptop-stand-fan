//! Tachometer math: pulse counts → RPM, plus smoothing.

/// TL-C12C: 2 tach pulses per revolution.
pub const PULSES_PER_REV: u32 = 2;

/// RPM from `pulses` counted over `window_ms`.
pub fn pulses_to_rpm(pulses: u32, window_ms: u32, pulses_per_rev: u32) -> u16 {
    if window_ms == 0 || pulses_per_rev == 0 {
        return 0;
    }
    let rpm = pulses as u64 * 60_000 / (pulses_per_rev as u64 * window_ms as u64);
    rpm.min(u16::MAX as u64) as u16
}

/// Exponential moving average for RPM. `new = old + (x - old) / 2^shift`,
/// using fixed-point (×16) state so it settles exactly on the input.
#[derive(Debug, Clone, Copy)]
pub struct RpmSmoother {
    state_x16: Option<i32>,
    shift: u8,
}

impl RpmSmoother {
    /// `shift = 1` gives each new sample a weight of ½.
    pub const fn new(shift: u8) -> Self {
        Self {
            state_x16: None,
            shift,
        }
    }

    pub fn update(&mut self, raw: u16) -> u16 {
        let x = raw as i32 * 16;
        let s = match self.state_x16 {
            None => x,
            Some(s) => s + ((x - s) >> self.shift),
        };
        self.state_x16 = Some(s);
        self.value()
    }

    pub fn value(&self) -> u16 {
        self.state_x16
            .map_or(0, |s| ((s + 8) / 16).clamp(0, u16::MAX as i32) as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpm_math() {
        // 1550 RPM × 2 pulses / 60 s ≈ 51.67 pulses/s
        assert_eq!(pulses_to_rpm(52, 1000, PULSES_PER_REV), 1560);
        assert_eq!(pulses_to_rpm(51, 1000, PULSES_PER_REV), 1530);
        assert_eq!(pulses_to_rpm(0, 1000, PULSES_PER_REV), 0);
        assert_eq!(pulses_to_rpm(26, 500, PULSES_PER_REV), 1560);
        assert_eq!(pulses_to_rpm(10, 0, PULSES_PER_REV), 0);
        assert_eq!(pulses_to_rpm(u32::MAX, 1, 1), u16::MAX);
    }

    #[test]
    fn smoother_first_sample_passes_through() {
        let mut s = RpmSmoother::new(1);
        assert_eq!(s.value(), 0);
        assert_eq!(s.update(1200), 1200);
    }

    #[test]
    fn smoother_converges_both_ways() {
        let mut s = RpmSmoother::new(1);
        s.update(1500);
        assert_eq!(s.update(500), 1000);
        for _ in 0..20 {
            s.update(0);
        }
        assert_eq!(s.value(), 0);
        for _ in 0..20 {
            s.update(1550);
        }
        assert_eq!(s.value(), 1550);
    }
}
