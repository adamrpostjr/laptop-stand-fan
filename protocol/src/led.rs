//! Status LED patterns (pure: which pattern, and whether it's on at time t).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedPattern {
    /// Normal operation: steady on.
    Steady,
    /// Temperature failsafe (Auto mode, no fresh temperature): 1 Hz blink.
    SlowBlink,
    /// Fan stall: 5 Hz blink.
    FastBlink,
    /// Calibration running: two short blinks every 1.5 s.
    DoubleBlink,
}

impl LedPattern {
    pub fn is_on(self, now_ms: u64) -> bool {
        match self {
            Self::Steady => true,
            Self::SlowBlink => now_ms % 1000 < 500,
            Self::FastBlink => now_ms % 200 < 100,
            Self::DoubleBlink => {
                let t = now_ms % 1500;
                t < 100 || (200..300).contains(&t)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert!(LedPattern::Steady.is_on(12345));
        assert!(LedPattern::SlowBlink.is_on(0));
        assert!(!LedPattern::SlowBlink.is_on(500));
        assert!(LedPattern::FastBlink.is_on(1050));
        assert!(!LedPattern::FastBlink.is_on(1150));
        let d = LedPattern::DoubleBlink;
        assert!(d.is_on(50) && !d.is_on(150) && d.is_on(250) && !d.is_on(400) && d.is_on(1550));
    }
}
