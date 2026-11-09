//! The complete fan-control state machine. It's pure: the firmware feeds it
//! time, RPM and BLE events, and applies the duty it returns.
//!
//! Each tick computes a target duty, in priority order:
//! 1. **Calibration** running → the calibration step's duty (applied immediately).
//! 2. **Any fan stalled** → 100 % (applied immediately).
//! 3. **Manual** → `max(manual %, min duty)`.
//! 4. **Auto, fresh temperature** → `max(curve(smoothed temp), min duty)`.
//! 5. **Auto, stale or no temperature** (failsafe) → `max(safe speed, min duty)`.
//!    This is also the state at boot until the first temperature arrives.
//!
//! Cases 3–5 go through a slew limiter (fast up, slow down). Boot, and any
//! increase of ≥ 20 points in the applied duty, start a 5 s grace period
//! during which stall detection is paused.

use crate::calibration::{Calibration, STEPS};
use crate::codec::{
    CalErrorCode, CalResult, Config, DecodeError, Mode, Status, validate_temperature,
};
use crate::curve::FanCurve;
use crate::led::LedPattern;
use crate::ramp::SlewLimiter;
use crate::settings::Settings;
use crate::stall::{StallDetector, StallParams};
use crate::temperature::TempTracker;
use crate::{FAN_COUNT, pct_to_permille};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlParams {
    pub slew_up_permille_per_s: u16,
    pub slew_down_permille_per_s: u16,
    pub grace_ms: u64,
    /// A change of at least this much in applied duty starts a grace period.
    pub big_change_permille: u16,
    pub stall: StallParams,
    /// Temperature EMA shift (2 → weight ¼).
    pub temp_smoothing_shift: u8,
}

impl Default for ControlParams {
    fn default() -> Self {
        Self {
            slew_up_permille_per_s: 100,  // 10 %/s
            slew_down_permille_per_s: 20, // 2 %/s
            grace_ms: 5_000,
            big_change_permille: 200,
            stall: StallParams::default(),
            temp_smoothing_shift: 2,
        }
    }
}

/// Result of one control tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    /// Duty to apply to the PWM output, permille.
    pub duty_permille: u16,
    /// Target before slew limiting, permille (for logging).
    pub target_permille: u16,
    pub status: Status,
    pub led: LedPattern,
    /// Smoothed temperature, deci-°C (for logging).
    pub temp_dc: Option<i16>,
    pub in_grace: bool,
}

#[derive(Debug, Clone)]
pub struct Controller {
    settings: Settings,
    params: ControlParams,
    temp: TempTracker,
    slew: SlewLimiter,
    stall: [StallDetector; FAN_COUNT],
    grace_until_ms: u64,
    grace_ref_permille: u16,
    last_tick_ms: Option<u64>,
    applied_permille: u16,
    calibration: Option<Calibration>,
    cal_result: CalResult,
    cal_samples: [[u16; FAN_COUNT]; STEPS],
    dirty: bool,
}

impl Controller {
    pub fn new(settings: Settings) -> Self {
        Self::with_params(settings, ControlParams::default())
    }

    pub fn with_params(settings: Settings, params: ControlParams) -> Self {
        Self {
            settings,
            params,
            temp: TempTracker::new(params.temp_smoothing_shift),
            slew: SlewLimiter::new(
                params.slew_up_permille_per_s,
                params.slew_down_permille_per_s,
            ),
            stall: [StallDetector::new(); FAN_COUNT],
            grace_until_ms: 0,
            grace_ref_permille: 0,
            last_tick_ms: None,
            applied_permille: 0,
            calibration: None,
            cal_result: CalResult::None,
            cal_samples: [[0; FAN_COUNT]; STEPS],
            dirty: false,
        }
    }

    // ------------------------------------------------------------ settings

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// `true` once after any persisted setting changed (including min duty
    /// from a finished calibration). The firmware then writes NVS.
    pub fn take_dirty(&mut self) -> bool {
        core::mem::take(&mut self.dirty)
    }

    /// The one way settings change: apply `f` to a copy, validate it, and
    /// mark dirty only if something actually changed.
    pub fn update_settings(&mut self, f: impl FnOnce(&mut Settings)) -> Result<(), DecodeError> {
        let mut next = self.settings;
        f(&mut next);
        let next = next.validate()?;
        if next != self.settings {
            self.settings = next;
            self.dirty = true;
        }
        Ok(())
    }

    pub fn set_mode(&mut self, mode: Mode) {
        let _ = self.update_settings(|s| s.mode = mode); // can't fail
    }

    pub fn set_manual_pct(&mut self, pct: u8) -> Result<(), DecodeError> {
        self.update_settings(|s| s.manual_pct = pct)
    }

    pub fn set_curve(&mut self, curve: FanCurve) {
        let _ = self.update_settings(|s| s.curve = curve); // curves are valid by construction
    }

    pub fn set_config(&mut self, config: Config) -> Result<(), DecodeError> {
        self.update_settings(|s| s.config = config)
    }

    // ------------------------------------------------------------ inputs

    pub fn on_temperature(&mut self, now_ms: u64, temp_dc: i16) -> Result<(), DecodeError> {
        let t = validate_temperature(temp_dc)?;
        self.temp
            .on_temperature(now_ms, t, self.settings.config.failsafe_timeout_ms());
        Ok(())
    }

    /// Start a calibration run. Refused while a fan is stalled.
    pub fn start_calibration(&mut self, now_ms: u64) -> Result<(), CalErrorCode> {
        if self.stall.iter().any(|s| s.is_stalled()) {
            self.cal_result = CalResult::Failed(CalErrorCode::StalledAtStart);
            return Err(CalErrorCode::StalledAtStart);
        }
        if self.calibration.is_none() {
            self.calibration = Some(Calibration::start(now_ms));
            self.cal_result = CalResult::Running { progress_pct: 0 };
        }
        Ok(())
    }

    pub fn abort_calibration(&mut self) {
        if self.calibration.take().is_some() {
            self.cal_result = CalResult::Failed(CalErrorCode::Aborted);
        }
    }

    pub fn is_calibrating(&self) -> bool {
        self.calibration.is_some()
    }

    pub fn cal_result(&self) -> CalResult {
        self.cal_result
    }

    /// RPM table of the current or most recent calibration (zeros if none).
    /// `[i]` is the reading at `calibration::duty_at_step(i)`.
    pub fn cal_samples(&self) -> &[[u16; FAN_COUNT]; STEPS] {
        &self.cal_samples
    }

    pub fn ms_since_temperature(&self, now_ms: u64) -> Option<u64> {
        self.temp.ms_since_update(now_ms)
    }

    // ------------------------------------------------------------ tick

    /// Run one control step. Call at a steady rate (e.g. every 100 ms) with
    /// the latest smoothed RPM of each fan.
    pub fn tick(&mut self, now_ms: u64, rpm: [u16; FAN_COUNT]) -> Output {
        let first = self.last_tick_ms.is_none();
        let dt_ms = self
            .last_tick_ms
            .map_or(0, |l| now_ms.saturating_sub(l))
            .min(60_000) as u32;
        self.last_tick_ms = Some(now_ms);
        if first {
            self.grace_until_ms = now_ms + self.params.grace_ms;
        }
        let in_grace = now_ms < self.grace_until_ms;

        // Stall detection uses the duty applied on the previous tick.
        if self.calibration.is_none() && !first {
            for (det, &r) in self.stall.iter_mut().zip(rpm.iter()) {
                det.update(
                    &self.params.stall,
                    now_ms,
                    r,
                    self.applied_permille,
                    in_grace,
                );
            }
        }
        let stalled = self.stall.iter().any(|s| s.is_stalled());

        let cfg = self.settings.config;
        let min = pct_to_permille(cfg.min_duty_pct);
        let fresh = self.temp.is_fresh(now_ms, cfg.failsafe_timeout_ms());

        let (target, immediate) = if let Some(cal) = self.calibration.as_mut() {
            let t = cal.tick(now_ms, rpm);
            self.cal_samples = *cal.samples();
            self.cal_result = CalResult::Running {
                progress_pct: cal.progress_pct(now_ms),
            };
            if let Some(res) = t.finished {
                self.calibration = None;
                self.cal_result = match res {
                    Ok(d) => {
                        // d is within MIN_DUTY_RANGE (checked by `analyse`)
                        let _ = self.update_settings(|s| s.config.min_duty_pct = d);
                        CalResult::Ok { min_duty_pct: d }
                    }
                    Err(e) => CalResult::Failed(e),
                };
            }
            (t.duty_permille, true)
        } else if stalled {
            (1000, true)
        } else {
            let base = match self.settings.mode {
                Mode::Manual => pct_to_permille(self.settings.manual_pct),
                Mode::Auto => match (fresh, self.temp.smoothed()) {
                    (true, Some(t)) => self.settings.curve.duty_permille_at(t),
                    _ => pct_to_permille(cfg.safe_speed_pct),
                },
            };
            (base.max(min), false)
        };

        let applied = if immediate || first {
            self.slew.jump_to(target)
        } else {
            self.slew.step(target, dt_ms)
        };

        // Grace only for big *increases* (spin-up lag). The reference follows
        // the duty down, so a slow ramp-down never re-arms grace and delays
        // stall detection.
        if first || applied < self.grace_ref_permille {
            self.grace_ref_permille = applied;
        } else if applied - self.grace_ref_permille >= self.params.big_change_permille {
            self.grace_until_ms = now_ms + self.params.grace_ms;
            self.grace_ref_permille = applied;
        }
        self.applied_permille = applied;

        let status = self.status_at(now_ms);
        let led = if status.any_stalled() {
            LedPattern::FastBlink
        } else if status.contains(Status::CALIBRATING) {
            LedPattern::DoubleBlink
        } else if status.contains(Status::FAILSAFE) {
            LedPattern::SlowBlink
        } else {
            LedPattern::Steady
        };

        Output {
            duty_permille: applied,
            target_permille: target,
            status,
            led,
            temp_dc: self.temp.smoothed(),
            in_grace: now_ms < self.grace_until_ms,
        }
    }

    pub fn status_at(&self, now_ms: u64) -> Status {
        let mut s = Status::default();
        s.set(Status::FAN1_STALLED, self.stall[0].is_stalled());
        s.set(Status::FAN2_STALLED, self.stall[1].is_stalled());
        let auto = self.settings.mode == Mode::Auto;
        let fresh = self
            .temp
            .is_fresh(now_ms, self.settings.config.failsafe_timeout_ms());
        s.set(Status::FAILSAFE, auto && !fresh);
        s.set(Status::TEMP_FRESH, auto && fresh);
        s.set(Status::CALIBRATING, self.calibration.is_some());
        s
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::curve::CurvePoint;

    const TICK: u64 = 100;

    /// Simulated stand: ticks the controller and models fan RPM from duty.
    struct Sim {
        c: Controller,
        now: u64,
        out: Output,
        /// Per-fan health; a dead fan reads 0 RPM.
        alive: [bool; 2],
        /// Periodic temperature feed (deci-°C), every 2 s, if `Some`.
        temp: Option<i16>,
    }

    fn fan_rpm(duty: u16) -> u16 {
        // floor 450 RPM, linear to 1550 at 100 %
        450 + (1100 * duty as u32 / 1000) as u16
    }

    impl Sim {
        fn new(settings: Settings) -> Self {
            let mut c = Controller::new(settings);
            let out = c.tick(0, [0, 0]);
            Self {
                c,
                now: 0,
                out,
                alive: [true; 2],
                temp: None,
            }
        }
        fn run_s(&mut self, secs: u64) {
            let end = self.now + secs * 1000;
            while self.now < end {
                self.now += TICK;
                if let Some(t) = self.temp
                    && self.now.is_multiple_of(2000)
                {
                    self.c.on_temperature(self.now, t).unwrap();
                }
                let r = fan_rpm(self.out.duty_permille);
                let rpm = [
                    if self.alive[0] { r } else { 0 },
                    if self.alive[1] { r } else { 0 },
                ];
                self.out = self.c.tick(self.now, rpm);
            }
        }
        fn duty_pct(&self) -> u16 {
            self.out.duty_permille / 10
        }
    }

    fn auto_settings() -> Settings {
        Settings {
            curve: FanCurve::new(&[CurvePoint::new(40, 20), CurvePoint::new(80, 100)]).unwrap(),
            ..Settings::default()
        }
    }

    #[test]
    fn boots_into_failsafe_at_safe_speed() {
        let sim = Sim::new(Settings::default());
        assert_eq!(sim.out.duty_permille, 600);
        assert!(sim.out.status.contains(Status::FAILSAFE));
        assert!(!sim.out.status.contains(Status::TEMP_FRESH));
        assert_eq!(sim.out.led, LedPattern::SlowBlink);
        assert!(sim.out.in_grace);
    }

    #[test]
    fn auto_follows_curve_with_slew() {
        let mut sim = Sim::new(auto_settings());
        sim.temp = Some(600); // 60 °C → 60 %
        sim.run_s(60);
        assert_eq!(sim.duty_pct(), 60);
        assert!(sim.out.status.contains(Status::TEMP_FRESH));
        assert!(!sim.out.status.contains(Status::FAILSAFE));
        assert_eq!(sim.out.led, LedPattern::Steady);

        // jump to 80 °C → 100 %, ramps at 10 %/s, smoothed temp lags
        sim.temp = Some(800);
        sim.run_s(1);
        assert!(sim.duty_pct() <= 70);
        sim.run_s(60);
        assert_eq!(sim.duty_pct(), 100);

        // cool down: ramps at ≤ 2 %/s
        sim.temp = Some(400);
        sim.run_s(5);
        assert!(sim.duty_pct() >= 89, "dropped too fast: {}", sim.duty_pct());
        sim.run_s(120);
        assert_eq!(sim.duty_pct(), 20);
    }

    #[test]
    fn min_duty_clamps_curve_and_manual() {
        let mut s = auto_settings();
        s.config.min_duty_pct = 35;
        let mut sim = Sim::new(s);
        sim.temp = Some(300);
        sim.run_s(60);
        assert_eq!(sim.duty_pct(), 35);
        sim.c.set_mode(Mode::Manual);
        sim.c.set_manual_pct(0).unwrap();
        sim.run_s(30);
        assert_eq!(sim.duty_pct(), 35);
    }

    #[test]
    fn failsafe_after_timeout_and_recovery() {
        let mut sim = Sim::new(auto_settings());
        sim.temp = Some(400); // 20 %
        sim.run_s(60);
        assert_eq!(sim.duty_pct(), 20);

        sim.temp = None; // controller app went away
        sim.run_s(29);
        assert!(!sim.out.status.contains(Status::FAILSAFE));
        assert_eq!(sim.duty_pct(), 20);
        sim.run_s(2);
        assert!(sim.out.status.contains(Status::FAILSAFE));
        sim.run_s(10);
        assert_eq!(sim.duty_pct(), 60);

        sim.temp = Some(400); // recovers
        sim.run_s(2);
        assert!(!sim.out.status.contains(Status::FAILSAFE));
        assert!(sim.out.status.contains(Status::TEMP_FRESH));
        sim.run_s(60);
        assert_eq!(sim.duty_pct(), 20);
    }

    #[test]
    fn manual_ignores_missing_temperature() {
        let mut s = Settings::default();
        s.mode = Mode::Manual;
        s.manual_pct = 45;
        let mut sim = Sim::new(s);
        sim.run_s(120);
        assert_eq!(sim.duty_pct(), 45);
        assert_eq!(sim.out.status, Status(0));
    }

    #[test]
    fn stall_goes_full_speed_and_recovers() {
        let mut s = Settings::default();
        s.mode = Mode::Manual;
        s.manual_pct = 40;
        let mut sim = Sim::new(s);
        sim.run_s(20);
        sim.alive[1] = false;
        sim.run_s(10);
        assert!(!sim.out.status.any_stalled());
        sim.run_s(2);
        assert!(sim.out.status.contains(Status::FAN2_STALLED));
        assert!(!sim.out.status.contains(Status::FAN1_STALLED));
        assert_eq!(sim.out.duty_permille, 1000);
        assert_eq!(sim.out.led, LedPattern::FastBlink);

        // stays stalled while dead
        sim.run_s(120);
        assert!(sim.out.status.contains(Status::FAN2_STALLED));
        assert_eq!(sim.out.duty_permille, 1000);

        // fan freed: 30 s of good RPM clears it, back to manual 40 %
        sim.alive[1] = true;
        sim.run_s(31);
        assert!(!sim.out.status.any_stalled());
        sim.run_s(60);
        assert_eq!(sim.duty_pct(), 40);
    }

    #[test]
    fn boot_grace_prevents_false_stall() {
        // fans read 0 for the first 4.9 s (spin-up), then fine
        let mut c = Controller::new(Settings::default());
        let mut t = 0;
        while t < 4_900 {
            let o = c.tick(t, [0, 0]);
            assert!(!o.status.any_stalled());
            t += TICK;
        }
        for _ in 0..200 {
            let o = c.tick(t, [900, 900]);
            assert!(!o.status.any_stalled());
            t += TICK;
        }
    }

    #[test]
    fn no_stall_detection_without_grace_expiry_after_big_change() {
        let mut s = Settings::default();
        s.mode = Mode::Manual;
        s.manual_pct = 20;
        s.config.min_duty_pct = 0;
        let mut c = Controller::new(s);
        let mut t = 0;
        for _ in 0..100 {
            c.tick(t, [500, 500]);
            t += TICK;
        }
        // big jump, fans read 0 for 4 s while "spinning up"
        c.set_manual_pct(100).unwrap();
        for _ in 0..40 {
            let o = c.tick(t, [0, 0]);
            assert!(o.in_grace || !o.status.any_stalled());
            t += TICK;
        }
        for _ in 0..200 {
            let o = c.tick(t, [1500, 1500]);
            assert!(!o.status.any_stalled());
            t += TICK;
        }
    }

    #[test]
    fn calibration_sets_min_duty_and_persists() {
        let mut sim = Sim::new(auto_settings());
        sim.temp = Some(500);
        sim.run_s(10);
        sim.c.take_dirty();
        sim.c.start_calibration(sim.now).unwrap();
        sim.run_s(1);
        assert!(sim.out.status.contains(Status::CALIBRATING));
        assert_eq!(sim.out.led, LedPattern::DoubleBlink);
        assert_eq!(sim.out.duty_permille, 1000);
        assert!(matches!(sim.c.cal_result(), CalResult::Running { .. }));
        sim.run_s(115);
        assert!(!sim.c.is_calibrating());
        // linear model from 0 %: floor 450, 5 % reads 505 = threshold
        assert_eq!(sim.c.cal_result(), CalResult::Ok { min_duty_pct: 5 });
        assert_eq!(sim.c.settings().config.min_duty_pct, 5);
        // full table kept after the run, including the final 0 % row
        assert_eq!(sim.c.cal_samples()[0], [1550, 1550]);
        assert_eq!(sim.c.cal_samples()[STEPS - 1], [450, 450]);
        assert!(sim.c.take_dirty());
        assert!(!sim.c.take_dirty());
    }

    #[test]
    fn calibration_abort_and_refusal() {
        let mut sim = Sim::new(Settings::default());
        sim.c.start_calibration(sim.now).unwrap();
        sim.run_s(3);
        sim.c.abort_calibration();
        assert_eq!(sim.c.cal_result(), CalResult::Failed(CalErrorCode::Aborted));
        assert_eq!(sim.c.settings().config.min_duty_pct, 20);

        sim.alive = [false, true];
        sim.run_s(20);
        assert!(sim.out.status.contains(Status::FAN1_STALLED));
        assert_eq!(
            sim.c.start_calibration(sim.now),
            Err(CalErrorCode::StalledAtStart)
        );
    }

    #[test]
    fn ramp_down_does_not_delay_stall_detection() {
        let mut s = Settings::default();
        s.mode = Mode::Manual;
        s.manual_pct = 100;
        let mut sim = Sim::new(s);
        sim.run_s(10);
        sim.c.set_manual_pct(20).unwrap(); // 80 s ramp at 2 %/s
        sim.alive[0] = false;
        sim.run_s(11);
        assert!(sim.out.status.contains(Status::FAN1_STALLED));
    }

    #[test]
    fn dirty_tracking() {
        let mut c = Controller::new(Settings::default());
        assert!(!c.take_dirty());
        c.set_mode(Mode::Auto); // unchanged
        assert!(!c.take_dirty());
        c.set_mode(Mode::Manual);
        assert!(c.take_dirty());
        assert_eq!(c.set_manual_pct(101), Err(DecodeError::OutOfRange));
        assert!(!c.take_dirty());
        let bad = Config {
            safe_speed_pct: 10,
            ..Config::default()
        };
        assert_eq!(c.set_config(bad), Err(DecodeError::OutOfRange));
        assert!(!c.take_dirty());
        c.set_config(Config {
            safe_speed_pct: 70,
            ..Config::default()
        })
        .unwrap();
        assert!(c.take_dirty());
        assert_eq!(c.on_temperature(0, 2000), Err(DecodeError::OutOfRange));
    }

    #[test]
    fn survives_u32_ms_wraparound() {
        // 50 days of uptime
        let start = 50 * 24 * 3600 * 1000u64;
        let mut c = Controller::new(auto_settings());
        let mut t = start;
        for i in 0..600 {
            if i % 20 == 0 {
                c.on_temperature(t, 600).unwrap();
            }
            c.tick(t, [900, 900]);
            t += TICK;
        }
        let o = c.tick(t, [900, 900]);
        assert!(o.status.contains(Status::TEMP_FRESH));
        assert_eq!(o.duty_permille, 600);
    }
}
