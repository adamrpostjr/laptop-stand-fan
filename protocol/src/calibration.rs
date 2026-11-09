//! Minimum-duty calibration.
//!
//! These fans don't stop at 0 % PWM. Below some duty, the speed stops
//! following PWM and settles at an RPM floor. This routine finds the lowest
//! duty that still controls the speed:
//!
//! 1. Spin up at 100 % for [`SPINUP_MS`].
//! 2. Step down from 100 % to 0 % in [`STEP_PCT`] steps, holding each step for
//!    [`STEP_MS`] and sampling the smoothed RPM at the end of the hold.
//! 3. Analyse the samples, using the slower fan at each step:
//!    - At 100 %, each fan must read at least [`MIN_TOP_RPM`] (otherwise the
//!      tach or the fan is faulty).
//!    - The 100 % reading must clearly exceed the 0 % reading (otherwise PWM
//!      is not reaching the fans).
//!    - If the fans really stop at 0 % (floor < stall threshold), the result
//!      is the lowest duty where every step at or above it reads ≥
//!      [`STOP_CASE_RPM`], plus a [`STEP_PCT`] margin.
//!    - Otherwise the result is the lowest duty where every step at or above
//!      it reads noticeably above the floor.
//!
//! The run takes about 5 s + 21 × 5 s ≈ 110 s. Stall detection is paused
//! while it runs.

use crate::codec::{CalErrorCode, MIN_DUTY_RANGE};

pub const SPINUP_MS: u64 = 5_000;
pub const STEP_MS: u64 = 5_000;
pub const STEP_PCT: u8 = 5;
pub const STEPS: usize = (100 / STEP_PCT as usize) + 1; // 100, 95, ... 0
pub const MIN_TOP_RPM: u16 = 400;
/// Fans that stop at 0 %: required RPM to count as "reliably spinning".
pub const STOP_CASE_RPM: u16 = 300;
/// Stall threshold used to tell "stops at 0 %" from "has a floor".
pub const FLOOR_STOP_RPM: u16 = 200;
/// "Responds to PWM" needs at least this much above the floor...
pub const RESPONSE_MIN_RPM: u16 = 30;
/// ...or this percentage of the 0→100 % span, whichever is larger.
pub const RESPONSE_SPAN_PCT: u16 = 5;

pub const TOTAL_MS: u64 = SPINUP_MS + STEPS as u64 * STEP_MS;

pub const fn duty_at_step(i: usize) -> u8 {
    100 - (i as u8) * STEP_PCT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalTick {
    pub duty_permille: u16,
    /// `Some` on the tick the calibration finishes.
    pub finished: Option<Result<u8, CalErrorCode>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Calibration {
    started_ms: u64,
    /// `None` = spin-up phase.
    step: Option<usize>,
    step_started_ms: u64,
    samples: [[u16; 2]; STEPS],
}

impl Calibration {
    pub fn start(now_ms: u64) -> Self {
        Self {
            started_ms: now_ms,
            step: None,
            step_started_ms: now_ms,
            samples: [[0; 2]; STEPS],
        }
    }

    pub fn samples(&self) -> &[[u16; 2]; STEPS] {
        &self.samples
    }

    pub fn progress_pct(&self, now_ms: u64) -> u8 {
        let el = now_ms.saturating_sub(self.started_ms).min(TOTAL_MS);
        (el * 100 / TOTAL_MS) as u8
    }

    pub fn tick(&mut self, now_ms: u64, rpm: [u16; 2]) -> CalTick {
        match self.step {
            None => {
                if now_ms.saturating_sub(self.step_started_ms) >= SPINUP_MS {
                    self.step = Some(0);
                    self.step_started_ms = now_ms;
                }
            }
            Some(i) => {
                if now_ms.saturating_sub(self.step_started_ms) >= STEP_MS {
                    self.samples[i] = rpm;
                    if i + 1 == STEPS {
                        return CalTick {
                            duty_permille: 1000,
                            finished: Some(analyse(&self.samples)),
                        };
                    }
                    self.step = Some(i + 1);
                    self.step_started_ms = now_ms;
                }
            }
        }
        let pct = self.step.map_or(100, duty_at_step);
        CalTick {
            duty_permille: crate::pct_to_permille(pct),
            finished: None,
        }
    }
}

/// `samples[i]` = `[fan1, fan2]` RPM at duty `duty_at_step(i)`.
pub fn analyse(samples: &[[u16; 2]; STEPS]) -> Result<u8, CalErrorCode> {
    let top = samples[0];
    if top[0] < MIN_TOP_RPM {
        return Err(CalErrorCode::Fan1NotSpinning);
    }
    if top[1] < MIN_TOP_RPM {
        return Err(CalErrorCode::Fan2NotSpinning);
    }
    let m = |i: usize| samples[i][0].min(samples[i][1]);
    let top = m(0);
    let floor = m(STEPS - 1);
    if top <= floor.saturating_add(RESPONSE_MIN_RPM) {
        return Err(CalErrorCode::NoPwmResponse);
    }

    let (threshold, margin) = if floor < FLOOR_STOP_RPM {
        (STOP_CASE_RPM, STEP_PCT)
    } else {
        let span = top - floor;
        (
            floor + RESPONSE_MIN_RPM.max(span * RESPONSE_SPAN_PCT / 100),
            0,
        )
    };

    // Scan down from 100 %; stop at the first step below threshold.
    let mut last_good = 0;
    for i in 0..STEPS {
        if m(i) >= threshold {
            last_good = i;
        } else {
            break;
        }
    }
    let min = (duty_at_step(last_good) + margin).min(100);
    if min > *MIN_DUTY_RANGE.end() {
        return Err(CalErrorCode::MinDutyTooHigh);
    }
    Ok(min)
}

/// Prints a calibration table (duty | fan1 rpm | fan2 rpm), one row per step.
pub struct CalTable<'a>(pub &'a [[u16; 2]; STEPS]);

impl core::fmt::Display for CalTable<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, " duty | fan1 rpm | fan2 rpm")?;
        for (i, s) in self.0.iter().enumerate() {
            writeln!(f, "{:4}% | {:8} | {:8}", duty_at_step(i), s[0], s[1])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build samples from an RPM-vs-duty model.
    fn model(f1: impl Fn(u8) -> u16, f2: impl Fn(u8) -> u16) -> [[u16; 2]; STEPS] {
        let mut s = [[0; 2]; STEPS];
        for (i, v) in s.iter_mut().enumerate() {
            let d = duty_at_step(i);
            *v = [f1(d), f2(d)];
        }
        s
    }

    /// Typical PWM fan: floor of 450 RPM below `knee` %, linear to 1550 at 100 %.
    fn floored(knee: u8) -> impl Fn(u8) -> u16 {
        move |d| {
            if d <= knee {
                450
            } else {
                450 + (1100 * (d - knee) as u32 / (100 - knee) as u32) as u16
            }
        }
    }

    #[test]
    fn step_table() {
        assert_eq!(STEPS, 21);
        assert_eq!(duty_at_step(0), 100);
        assert_eq!(duty_at_step(20), 0);
    }

    #[test]
    fn finds_knee_for_floored_fans() {
        // knee at 20 %: 25 % is the first step clearly above the floor
        assert_eq!(analyse(&model(floored(20), floored(20))), Ok(25));
        assert_eq!(analyse(&model(floored(0), floored(0))), Ok(5));
    }

    #[test]
    fn slower_fan_decides() {
        assert_eq!(analyse(&model(floored(10), floored(30))), Ok(35));
    }

    #[test]
    fn fans_that_stop_get_margin() {
        // stops below 15 %, 400 RPM at 15 %
        let f = |d: u8| {
            if d < 15 {
                0
            } else {
                400 + (d as u16 - 15) * 13
            }
        };
        assert_eq!(analyse(&model(f, f)), Ok(20));
    }

    #[test]
    fn detects_dead_tach() {
        assert_eq!(
            analyse(&model(|_| 0, floored(20))),
            Err(CalErrorCode::Fan1NotSpinning)
        );
        assert_eq!(
            analyse(&model(floored(20), |_| 100)),
            Err(CalErrorCode::Fan2NotSpinning)
        );
    }

    #[test]
    fn detects_no_pwm_response() {
        // PWM line floating → fans at full speed regardless of duty
        assert_eq!(
            analyse(&model(|_| 1550, |_| 1540)),
            Err(CalErrorCode::NoPwmResponse)
        );
    }

    #[test]
    fn rejects_too_high_min_duty() {
        assert_eq!(
            analyse(&model(floored(70), floored(70))),
            Err(CalErrorCode::MinDutyTooHigh)
        );
    }

    #[test]
    fn state_machine_sequence() {
        let mut c = Calibration::start(0);
        let mut t = 0;
        let mut duties = std::vec::Vec::new();
        let result = loop {
            let r = c.tick(t, [900, 900]);
            if let Some(res) = r.finished {
                break res;
            }
            if duties.last() != Some(&r.duty_permille) {
                duties.push(r.duty_permille);
            }
            t += 100;
            assert!(t < TOTAL_MS + 1000, "did not finish");
        };
        // 100 % (spin-up + step 0) then 95..0
        assert_eq!(duties.len(), STEPS);
        assert_eq!(duties[0], 1000);
        assert_eq!(*duties.last().unwrap(), 0);
        // rpm passed was constant → no PWM response
        assert_eq!(result, Err(CalErrorCode::NoPwmResponse));
        assert!(t >= TOTAL_MS);
    }

    #[test]
    fn state_machine_records_rpm_per_step() {
        let mut c = Calibration::start(0);
        let fan = floored(20);
        let mut duty = 1000u16;
        let mut t = 0;
        loop {
            let pct = (duty / 10) as u8;
            let r = c.tick(t, [fan(pct), fan(pct)]);
            if let Some(res) = r.finished {
                assert_eq!(res, Ok(25));
                break;
            }
            duty = r.duty_permille;
            t += 100;
        }
        assert_eq!(c.samples()[0], [1550, 1550]);
        assert_eq!(c.samples()[20], [450, 450]);
        assert_eq!(c.progress_pct(t), 100);
    }
}
