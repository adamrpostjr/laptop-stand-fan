//! Slew-rate limiter for the fan duty. It has separate up and down rates:
//! speeding up is fast, slowing down is gentle and quiet.

#[derive(Debug, Clone, Copy)]
pub struct SlewLimiter {
    /// Current output in milli-permille (1/1000 ‰) so tiny `dt`s still make progress.
    current_milli: Option<u32>,
    up_permille_per_s: u16,
    down_permille_per_s: u16,
}

impl SlewLimiter {
    pub const fn new(up_permille_per_s: u16, down_permille_per_s: u16) -> Self {
        Self {
            current_milli: None,
            up_permille_per_s,
            down_permille_per_s,
        }
    }

    /// Current output in permille, or `None` before the first step.
    pub fn current(&self) -> Option<u16> {
        self.current_milli.map(|c| ((c + 500) / 1000) as u16)
    }

    /// Set the output immediately, bypassing the rate limit.
    pub fn jump_to(&mut self, target: u16) -> u16 {
        let t = target.min(1000);
        self.current_milli = Some(t as u32 * 1000);
        t
    }

    /// Move toward `target` by at most the configured rate × `dt_ms`.
    /// The first call jumps straight to `target`.
    pub fn step(&mut self, target: u16, dt_ms: u32) -> u16 {
        let Some(cur) = self.current_milli else {
            return self.jump_to(target);
        };
        let target_m = target.min(1000) as u32 * 1000;
        let dt = dt_ms.min(60_000);
        // ‰/s × ms = milli-‰
        let next = if target_m > cur {
            cur.saturating_add(self.up_permille_per_s as u32 * dt)
                .min(target_m)
        } else {
            cur.saturating_sub(self.down_permille_per_s as u32 * dt)
                .max(target_m)
        };
        self.current_milli = Some(next);
        self.current().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_step_jumps() {
        let mut s = SlewLimiter::new(100, 20);
        assert_eq!(s.current(), None);
        assert_eq!(s.step(600, 100), 600);
    }

    #[test]
    fn ramps_up_at_rate() {
        let mut s = SlewLimiter::new(100, 20); // 10 %/s up
        s.jump_to(200);
        for _ in 0..10 {
            s.step(1000, 100);
        }
        assert_eq!(s.current(), Some(300)); // +100‰ after 1 s
        for _ in 0..100 {
            s.step(1000, 100);
        }
        assert_eq!(s.current(), Some(1000)); // never overshoots
    }

    #[test]
    fn ramps_down_slower() {
        let mut s = SlewLimiter::new(100, 20); // 2 %/s down
        s.jump_to(1000);
        s.step(0, 1000);
        assert_eq!(s.current(), Some(980));
        s.step(0, 10_000);
        assert_eq!(s.current(), Some(780));
    }

    #[test]
    fn small_dt_accumulates() {
        let mut s = SlewLimiter::new(1, 1); // 1‰/s: 0.01‰ per 10 ms
        s.jump_to(0);
        for _ in 0..100 {
            s.step(1000, 10);
        }
        assert_eq!(s.current(), Some(1));
    }

    #[test]
    fn clamps_target() {
        let mut s = SlewLimiter::new(1000, 1000);
        assert_eq!(s.jump_to(5000), 1000);
        assert_eq!(s.step(5000, 1000), 1000);
    }
}
