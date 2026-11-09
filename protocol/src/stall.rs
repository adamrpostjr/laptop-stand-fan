//! Per-fan stall detection.
//!
//! A fan is **stalled** when it reads `< rpm_threshold` for longer than
//! `stall_after_ms` while the commanded duty is `≥ min_duty_permille` and we
//! are not in a grace period. The grace period follows boot and large speed
//! changes; the controller manages it.
//!
//! The flag clears automatically once the fan reads `≥ rpm_threshold`
//! continuously for `clear_after_ms`, for example after something that blocked
//! the blades falls out. A dead fan stays flagged.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallParams {
    pub rpm_threshold: u16,
    pub min_duty_permille: u16,
    pub stall_after_ms: u64,
    pub clear_after_ms: u64,
}

impl Default for StallParams {
    fn default() -> Self {
        Self {
            rpm_threshold: 200,
            min_duty_permille: 200,
            stall_after_ms: 10_000,
            clear_after_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StallDetector {
    low_since_ms: Option<u64>,
    ok_since_ms: Option<u64>,
    stalled: bool,
}

impl StallDetector {
    pub const fn new() -> Self {
        Self {
            low_since_ms: None,
            ok_since_ms: None,
            stalled: false,
        }
    }

    pub const fn is_stalled(&self) -> bool {
        self.stalled
    }

    /// Feed one observation; returns the (possibly updated) stalled state.
    pub fn update(
        &mut self,
        p: &StallParams,
        now_ms: u64,
        rpm: u16,
        duty_permille: u16,
        in_grace: bool,
    ) -> bool {
        if self.stalled {
            if rpm >= p.rpm_threshold {
                let since = *self.ok_since_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) >= p.clear_after_ms {
                    self.stalled = false;
                    self.ok_since_ms = None;
                    self.low_since_ms = None;
                }
            } else {
                self.ok_since_ms = None;
            }
            return self.stalled;
        }

        if in_grace || duty_permille < p.min_duty_permille || rpm >= p.rpm_threshold {
            self.low_since_ms = None;
        } else {
            let since = *self.low_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) > p.stall_after_ms {
                self.stalled = true;
                self.ok_since_ms = None;
            }
        }
        self.stalled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        d: &mut StallDetector,
        from_s: u64,
        to_s: u64,
        rpm: u16,
        duty: u16,
        grace: bool,
    ) -> bool {
        let p = StallParams::default();
        let mut r = false;
        for t in from_s..=to_s {
            r = d.update(&p, t * 1000, rpm, duty, grace);
        }
        r
    }

    #[test]
    fn healthy_fan_never_stalls() {
        let mut d = StallDetector::new();
        assert!(!run(&mut d, 0, 100, 900, 500, false));
    }

    #[test]
    fn stalls_after_more_than_10s() {
        let mut d = StallDetector::new();
        assert!(!run(&mut d, 0, 10, 0, 500, false)); // exactly 10 s: not yet
        assert!(run(&mut d, 11, 11, 0, 500, false));
    }

    #[test]
    fn low_duty_does_not_stall() {
        let mut d = StallDetector::new();
        assert!(!run(&mut d, 0, 100, 0, 199, false));
        assert!(run(&mut d, 101, 112, 0, 200, false));
    }

    #[test]
    fn grace_resets_timer() {
        let mut d = StallDetector::new();
        assert!(!run(&mut d, 0, 8, 0, 500, false));
        assert!(!run(&mut d, 9, 9, 0, 500, true)); // grace resets the low timer
        assert!(!run(&mut d, 10, 20, 0, 500, false));
        assert!(run(&mut d, 21, 21, 0, 500, false));
    }

    #[test]
    fn brief_dip_does_not_stall() {
        let mut d = StallDetector::new();
        assert!(!run(&mut d, 0, 8, 150, 500, false));
        assert!(!run(&mut d, 9, 9, 800, 500, false));
        assert!(!run(&mut d, 10, 18, 150, 500, false));
    }

    #[test]
    fn clears_after_30s_of_good_rpm() {
        let mut d = StallDetector::new();
        assert!(run(&mut d, 0, 11, 0, 1000, false));
        assert!(run(&mut d, 12, 41, 1500, 1000, false)); // 29 s good
        assert!(!run(&mut d, 42, 42, 1500, 1000, false)); // 30 s good
    }

    #[test]
    fn dead_fan_stays_stalled_even_in_grace() {
        let mut d = StallDetector::new();
        assert!(run(&mut d, 0, 11, 0, 1000, false));
        assert!(run(&mut d, 12, 500, 0, 1000, true));
        // a recovery interrupted by a dip restarts the 30 s window
        assert!(run(&mut d, 501, 520, 1500, 1000, false));
        assert!(run(&mut d, 521, 521, 0, 1000, false));
        assert!(run(&mut d, 522, 550, 1500, 1000, false));
        assert!(!run(&mut d, 551, 552, 1500, 1000, false));
    }
}
