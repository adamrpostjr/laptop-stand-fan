//! Temperature freshness (input to the failsafe) and smoothing.

#[derive(Debug, Clone, Copy)]
pub struct TempTracker {
    last_update_ms: Option<u64>,
    /// Smoothed temperature, deci-°C × 16.
    smoothed_x16: i32,
    shift: u8,
}

impl TempTracker {
    /// `shift = 2`: each sample has weight ¼. With 2 s updates the time
    /// constant is about 8 s, which filters CPU-temperature spikes.
    pub const fn new(shift: u8) -> Self {
        Self {
            last_update_ms: None,
            smoothed_x16: 0,
            shift,
        }
    }

    /// Record a new reading. If the previous reading was stale (or none),
    /// the filter restarts from this value instead of averaging with old data.
    pub fn on_temperature(&mut self, now_ms: u64, temp_dc: i16, timeout_ms: u64) {
        let x = temp_dc as i32 * 16;
        if self.is_fresh(now_ms, timeout_ms) {
            self.smoothed_x16 += (x - self.smoothed_x16) >> self.shift;
        } else {
            self.smoothed_x16 = x;
        }
        self.last_update_ms = Some(now_ms);
    }

    /// `true` if a reading arrived within `timeout_ms`. Never fresh before
    /// the first reading.
    pub fn is_fresh(&self, now_ms: u64, timeout_ms: u64) -> bool {
        self.last_update_ms
            .is_some_and(|t| now_ms.saturating_sub(t) <= timeout_ms)
    }

    /// Smoothed temperature in deci-°C, if any reading was ever received.
    pub fn smoothed(&self) -> Option<i16> {
        self.last_update_ms.map(|_| {
            let v = self.smoothed_x16;
            // round half away from zero
            let r = if v >= 0 { (v + 8) / 16 } else { (v - 8) / 16 };
            r as i16
        })
    }

    pub fn ms_since_update(&self, now_ms: u64) -> Option<u64> {
        self.last_update_ms.map(|t| now_ms.saturating_sub(t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TO: u64 = 30_000;

    #[test]
    fn not_fresh_before_first_reading() {
        let t = TempTracker::new(2);
        assert!(!t.is_fresh(0, TO));
        assert_eq!(t.smoothed(), None);
    }

    #[test]
    fn freshness_window() {
        let mut t = TempTracker::new(2);
        t.on_temperature(1000, 500, TO);
        assert!(t.is_fresh(1000, TO));
        assert!(t.is_fresh(31_000, TO));
        assert!(!t.is_fresh(31_001, TO));
    }

    #[test]
    fn smoothing() {
        let mut t = TempTracker::new(2);
        t.on_temperature(0, 400, TO);
        assert_eq!(t.smoothed(), Some(400));
        t.on_temperature(2000, 800, TO);
        assert_eq!(t.smoothed(), Some(500)); // 400 + (800-400)/4
        for i in 2..40 {
            t.on_temperature(i * 2000, 800, TO);
        }
        assert_eq!(t.smoothed(), Some(800));
    }

    #[test]
    fn stale_resets_filter() {
        let mut t = TempTracker::new(2);
        t.on_temperature(0, 400, TO);
        t.on_temperature(100_000, 900, TO);
        assert_eq!(t.smoothed(), Some(900));
    }

    #[test]
    fn negative_temps() {
        let mut t = TempTracker::new(2);
        t.on_temperature(0, -105, TO);
        assert_eq!(t.smoothed(), Some(-105));
    }
}
