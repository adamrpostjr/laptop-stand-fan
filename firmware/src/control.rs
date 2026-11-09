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
use fanstand_protocol::calibration::{CalTable, STEP_MS, STEP_PCT, TOTAL_MS};
use fanstand_protocol::codec::{ConfigPatch, ConfigRanges};
use fanstand_protocol::diag::DebugInfo;
use fanstand_protocol::{
    CalResult, Command, Config, Controller, DeciCelsius, FanCurve, Mode, Permille, Settings,
};
use log::{error, info, warn};

use crate::fan::Fan;
use crate::state::Shared;
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
    /// Whole config (BLE `CONFIG` write).
    SetConfig(Config),
    /// Some fields (console `safe 70`).
    PatchConfig(ConfigPatch),
    Command(Command),
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
                    info!(
                        "calibration started (~{} s): fans 100%, then {STEP_PCT}% lower every {} s",
                        TOTAL_MS / 1000,
                        STEP_MS / 1000
                    )
                }
                CalResult::Ok { min_duty_pct } => {
                    print!("{}", CalTable(ctl.cal_samples()));
                    info!("calibration OK: min duty = {min_duty_pct}% (saved)");
                }
                CalResult::Failed(code) => {
                    print!("{}", CalTable(ctl.cal_samples()));
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
        let raw = [0, 1].map(|i| hw.tach[i].as_ref().map_or(0, |t| t.raw_rpm()));
        let temp_age_ms = ctl.ms_since_temperature(now);

        // ---- publish for BLE (never blocks: skip this tick if BLE holds the lock).
        // Fields set once at boot (reset reason, no-auth flag) are left alone.
        if let Ok(mut snap) = shared.try_lock() {
            snap.settings = *ctl.settings();
            snap.rpm = rpm;
            snap.status = out.status;
            snap.cal = cal;
            snap.cal_table = *ctl.cal_samples();
            let d = &mut snap.debug;
            d.uptime_s = (now / 1000) as u32;
            d.duty_permille = out.duty_permille;
            d.target_permille = out.target_permille;
            d.temp_dc = out.temp_dc;
            d.temp_age_s = temp_age_ms.map(|ms| (ms / 1000).min(u16::MAX as u64 - 1) as u16);
            d.raw_rpm = raw;
            d.flags = (d.flags & DebugInfo::FLAG_NO_AUTH)
                | if out.in_grace {
                    DebugInfo::FLAG_GRACE
                } else {
                    0
                };
            d.free_heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            d.tach_high_samples = [0, 1].map(|i| hw.tach[i].as_ref().map_or(0, |t| t.probe_high()));
        }

        // ---- periodic log
        if log_enabled && now >= next_log {
            next_log = now + LOG_PERIOD_MS;
            let mode = match ctl.settings().mode {
                Mode::Manual => "manual",
                Mode::Auto => "auto",
            };
            info!(
                "duty {} (target {}) | rpm {:4} / {:4} (raw {:4} / {:4}) | {mode} | temp {}{} | flags {}{}",
                Permille(out.duty_permille),
                Permille(out.target_permille),
                rpm[0],
                rpm[1],
                raw[0],
                raw[1],
                out.temp_dc
                    .map_or("none".into(), |t| DeciCelsius(t).to_string()),
                temp_age_ms.map_or(String::new(), |ms| format!(" ({}s ago)", ms / 1000)),
                out.status,
                if out.in_grace { " (grace)" } else { "" },
            );
        }

        let _ = watchdog.feed();
        std::thread::sleep(TICK);
    }
}

fn handle(ctl: &mut Controller, req: Request, now: u64, log_enabled: &mut bool) {
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
        Request::SetConfig(c) => ctl.set_config(c).map_err(|_| ConfigRanges.to_string()),
        Request::PatchConfig(p) => {
            let cfg = ctl.settings().config;
            cfg.apply(p)
                .and_then(|c| ctl.set_config(c))
                .map_err(|_| ConfigRanges.to_string())
        }
        Request::Command(Command::StartCalibration) => {
            ctl.start_calibration(now).map_err(|e| e.describe().into())
        }
        Request::Command(Command::AbortCalibration) => {
            ctl.abort_calibration();
            Ok(())
        }
        Request::ResetDefaults => ctl
            .update_settings(|s| *s = Settings::default())
            .map_err(|e| e.to_string()),
        Request::PrintStatus => {
            print_status(ctl, now);
            Ok(())
        }
        Request::PrintHelp => {
            crate::console::print_help();
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
    println!("firmware   {}", crate::FIRMWARE_VERSION);
    println!("settings   {}", ctl.settings());
    println!("status     {}", ctl.status_at(now));
    println!("cal        {:?}", ctl.cal_result());
    let table = ctl.cal_samples();
    if table.iter().any(|s| s[0] != 0 || s[1] != 0) {
        print!("{}", CalTable(table));
    }
}
