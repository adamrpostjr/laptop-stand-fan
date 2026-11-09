//! The control loop. It runs on the main task every 100 ms and feeds the
//! task watchdog.
//!
//! All decisions are made by the pure `fanstand_protocol::Controller`. This
//! module moves data between it and the hardware, and handles `Request`s from
//! the console and BLE arriving over a channel, and publishes a [`Snapshot`]
//! for BLE reads with `try_lock`. Neither side can ever block this loop.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::gpio::{Output, PinDriver};
use esp_idf_svc::hal::task::watchdog::WatchdogSubscription;
use fanstand_protocol::calibration::{STEPS, duty_at_step};
use fanstand_protocol::diag::DebugInfo;
use fanstand_protocol::{CalResult, Config, Controller, FanCurve, Mode, Settings};
use log::{error, info, warn};

use crate::fan::Fan;
use crate::state::{Shared, Snapshot};
use crate::storage::Storage;
use crate::tach::Tach;

pub const TICK: Duration = Duration::from_millis(100);
const TACH_PERIOD_MS: u64 = 1000;
const LOG_PERIOD_MS: u64 = 1000;
/// Settings are written this long after the last change, so a burst of edits
/// becomes a single flash write.
const SAVE_DEBOUNCE_MS: u64 = 2000;

#[derive(Debug)]
pub enum Request {
    SetMode(Mode),
    SetManual(u8),
    Temperature(i16),
    SetCurve(FanCurve),
    SetConfig(Config),
    SetSafeSpeed(u8),
    SetMinDuty(u8),
    SetFailsafeTimeout(u16),
    StartCalibration,
    AbortCalibration,
    ResetDefaults,
    PrintStatus,
    PrintHelp,
    ToggleLog,
}

pub struct Hardware<'d> {
    pub fan: Fan<'d>,
    /// `None` if that PCNT unit failed to initialise (it then reads 0 RPM,
    /// which trips stall protection → 100 %).
    pub tach: [Option<Tach<'d>>; 2],
    pub led: PinDriver<'d, Output>,
    pub storage: Option<Storage>,
}

pub fn run(
    mut hw: Hardware<'_>,
    mut ctl: Controller,
    rx: Receiver<Request>,
    mut watchdog: WatchdogSubscription<'_>,
    boot: Instant,
    shared: Shared,
) -> ! {
    let now_ms = || boot.elapsed().as_millis() as u64;
    let mut next_tach = now_ms() + TACH_PERIOD_MS;
    let mut next_log = 0;
    let mut save_at: Option<u64> = None;
    let mut log_enabled = true;
    let mut rpm = [0u16; 2];
    let mut last_cal_result = ctl.cal_result();
    let mut last_status = None;

    loop {
        let now = now_ms();

        // ---- requests (console / BLE)
        while let Ok(req) = rx.try_recv() {
            handle(&mut ctl, req, now, &mut log_enabled);
        }

        // ---- tach (1 s window) + raw line-level probe every tick
        for t in hw.tach.iter_mut().flatten() {
            t.probe_level();
        }
        if now >= next_tach {
            for (i, t) in hw.tach.iter_mut().enumerate() {
                rpm[i] = t.as_mut().map_or(0, |t| t.sample(now));
            }
            next_tach = (next_tach + TACH_PERIOD_MS).max(now + 1);
        }

        // ---- control
        let out = ctl.tick(now, rpm);
        if let Err(e) = hw.fan.set_permille(out.duty_permille) {
            error!("PWM update failed: {e}");
        }
        let _ = hw.led.set_level(out.led.is_on(now).into());

        // ---- events worth logging immediately
        if last_status != Some(out.status) {
            if last_status.is_some() {
                info!("status → {}", out.status);
            }
            last_status = Some(out.status);
        }
        let cal = ctl.cal_result();
        if cal != last_cal_result {
            match cal {
                CalResult::Running { .. }
                    if !matches!(last_cal_result, CalResult::Running { .. }) =>
                {
                    info!("calibration started (~110 s): fans 100% then stepping down 5% every 5 s")
                }
                CalResult::Ok { min_duty_pct } => {
                    print_cal_table(ctl.cal_samples());
                    info!("calibration OK: min duty = {min_duty_pct}% (saved)");
                }
                CalResult::Failed(code) => {
                    print_cal_table(ctl.cal_samples());
                    warn!("calibration failed: {}", code.describe());
                }
                _ => {}
            }
            last_cal_result = cal;
        }

        // ---- persistence
        if ctl.take_dirty() {
            save_at = Some(now + SAVE_DEBOUNCE_MS);
        }
        if save_at.is_some_and(|t| now >= t) {
            save_at = None;
            match &hw.storage {
                Some(s) => match s.save(ctl.settings()) {
                    Ok(()) => info!("settings saved"),
                    Err(e) => error!("saving settings failed: {e}"),
                },
                None => warn!("no NVS; settings not persisted"),
            }
        }

        // ---- publish for BLE (never blocks: skip this tick if BLE holds the lock)
        if let Ok(mut snap) = shared.try_lock() {
            let raw = |i: usize| hw.tach[i].as_ref().map_or(0, |t| t.raw_rpm());
            let no_auth = snap.debug.flags & DebugInfo::FLAG_NO_AUTH;
            *snap = Snapshot {
                settings: *ctl.settings(),
                rpm,
                status: out.status,
                cal: ctl.cal_result(),
                cal_table: *ctl.cal_samples(),
                debug: DebugInfo {
                    uptime_s: (now / 1000) as u32,
                    reset_reason: snap.debug.reset_reason,
                    duty_permille: out.duty_permille,
                    target_permille: out.target_permille,
                    temp_dc: out.temp_dc,
                    temp_age_s: ctl
                        .ms_since_temperature(now)
                        .map(|ms| (ms / 1000).min(u16::MAX as u64 - 1) as u16),
                    raw_rpm: [raw(0), raw(1)],
                    flags: no_auth
                        | if out.in_grace {
                            DebugInfo::FLAG_GRACE
                        } else {
                            0
                        },
                    free_heap: unsafe { esp_idf_svc::sys::esp_get_free_heap_size() },
                    tach_high_samples: [0, 1]
                        .map(|i| hw.tach[i].as_ref().map_or(0, |t| t.probe_high())),
                },
            };
        }

        // ---- periodic log
        if log_enabled && now >= next_log {
            next_log = now + LOG_PERIOD_MS;
            let raw = |i: usize| hw.tach[i].as_ref().map_or(0, |t| t.raw_rpm());
            let temp = match (out.temp_dc, ctl.ms_since_temperature(now)) {
                (Some(t), Some(age)) => format!("{:.1}C ({}s ago)", t as f32 / 10.0, age / 1000),
                _ => "none".into(),
            };
            let mode = match ctl.settings().mode {
                Mode::Manual => format!("Manual {}%", ctl.settings().manual_pct),
                Mode::Auto => "Auto".into(),
            };
            info!(
                "duty {:5.1}% (target {:5.1}%) | rpm {:4} / {:4} (raw {:4} / {:4}) | {mode} | temp {temp} | flags {}{}",
                out.duty_permille as f32 / 10.0,
                out.target_permille as f32 / 10.0,
                rpm[0],
                rpm[1],
                raw(0),
                raw(1),
                out.status,
                if out.in_grace { " (grace)" } else { "" },
            );
        }

        let _ = watchdog.feed();
        std::thread::sleep(TICK);
    }
}

fn handle(ctl: &mut Controller, req: Request, now: u64, log_enabled: &mut bool) {
    let cfg = ctl.settings().config;
    let res: Result<(), String> = match req {
        Request::SetMode(m) => {
            ctl.set_mode(m);
            Ok(())
        }
        Request::SetManual(p) => ctl.set_manual_pct(p).map_err(|e| e.to_string()),
        Request::Temperature(t) => ctl.on_temperature(now, t).map_err(|e| e.to_string()),
        Request::SetCurve(c) => {
            ctl.set_curve(c);
            Ok(())
        }
        Request::SetConfig(c) => ctl.set_config(c).map_err(|e| e.to_string()),
        Request::SetSafeSpeed(v) => ctl
            .set_config(Config {
                safe_speed_pct: v,
                ..cfg
            })
            .map_err(|_| "safe speed must be 30-100".into()),
        Request::SetMinDuty(v) => ctl
            .set_config(Config {
                min_duty_pct: v,
                ..cfg
            })
            .map_err(|_| "min duty must be 0-60".into()),
        Request::SetFailsafeTimeout(v) => ctl
            .set_config(Config {
                failsafe_timeout_s: v,
                ..cfg
            })
            .map_err(|_| "timeout must be 5-600 s".into()),
        Request::StartCalibration => ctl.start_calibration(now).map_err(|e| e.describe().into()),
        Request::AbortCalibration => {
            ctl.abort_calibration();
            Ok(())
        }
        Request::ResetDefaults => {
            let d = Settings::default();
            ctl.set_mode(d.mode);
            let _ = ctl.set_manual_pct(d.manual_pct);
            ctl.set_curve(d.curve);
            let _ = ctl.set_config(d.config);
            Ok(())
        }
        Request::PrintStatus => {
            print_status(ctl, now);
            Ok(())
        }
        Request::PrintHelp => {
            println!("{}", crate::console::HELP);
            Ok(())
        }
        Request::ToggleLog => {
            *log_enabled = !*log_enabled;
            println!("periodic log {}", if *log_enabled { "on" } else { "off" });
            Ok(())
        }
    };
    if let Err(e) = res {
        warn!("request rejected: {e}");
    }
}

fn print_status(ctl: &Controller, now: u64) {
    let cal_samples = ctl.cal_samples();
    let s = ctl.settings();
    println!("firmware   {}", crate::FIRMWARE_VERSION);
    println!("mode       {:?}, manual {}%", s.mode, s.manual_pct);
    let pts: Vec<String> = s
        .curve
        .points()
        .iter()
        .map(|p| format!("{}:{}", p.temp_c, p.duty_pct))
        .collect();
    println!("curve      {}", pts.join(" "));
    println!(
        "config     safe {}%, min duty {}%, failsafe timeout {} s",
        s.config.safe_speed_pct, s.config.min_duty_pct, s.config.failsafe_timeout_s
    );
    println!("status     {}", ctl.status_at(now));
    println!("cal        {:?}", ctl.cal_result());
    if cal_samples.iter().any(|s| s[0] != 0 || s[1] != 0) {
        print_cal_table(cal_samples);
    }
}

fn print_cal_table(samples: &[[u16; 2]; STEPS]) {
    println!(" duty | fan1 rpm | fan2 rpm");
    for (i, s) in samples.iter().enumerate() {
        println!("{:4}% | {:8} | {:8}", duty_at_step(i), s[0], s[1]);
    }
}

/// Used by the boot log.
pub fn describe(settings: &Settings) -> String {
    format!(
        "mode {:?}, manual {}%, safe {}%, min {}%, timeout {} s, {} curve points",
        settings.mode,
        settings.manual_pct,
        settings.config.safe_speed_pct,
        settings.config.min_duty_pct,
        settings.config.failsafe_timeout_s,
        settings.curve.points().len()
    )
}
