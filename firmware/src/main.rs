//! Firmware for the BLE cooling laptop stand (ESP32 DevKit, WROOM-32D).
//!
//! Pins (fixed by the hardware; don't change):
//! - GPIO15: 25 kHz PWM to both fans (strapping pin, so configured first)
//! - GPIO16: fan 1 tach (open-collector, internal pull-up)
//! - GPIO19: fan 2 tach (open-collector, internal pull-up). The original spec
//!   said GPIO18, but the wire is soldered to GPIO19 (found 2026-11-08).
//! - GPIO2:  status LED
//!
//! Boot order is chosen so the fans are under control as early as possible:
//! 1. PWM at the default safe speed, before anything else (even logging).
//! 2. Settings from NVS. The defaults are used if NVS is missing or broken;
//!    nothing here can stop the fans.
//! 3. Tach, LED, watchdog, console, BLE, then the control loop. A BLE
//!    start-up failure is logged and the fans keep running without it.

mod ble;
mod console;
mod control;
mod fan;
mod state;
mod storage;
mod tach;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::task::watchdog::{TWDTConfig, TWDTDriver};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use fanstand_protocol::auth::{Key, parse_key_hex};
use fanstand_protocol::{Config, Controller, Settings, pct_to_permille};
use log::{error, info, warn};

#[cfg(not(feature = "no-auth"))]
pub const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(feature = "no-auth")]
pub const FIRMWARE_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-noauth");

/// BLE shared secret, baked in at build time from `FANSTAND_KEY` (64 hex
/// digits; `fanstand keygen` creates one). A malformed key fails the build.
#[cfg(not(feature = "no-auth"))]
const AUTH_KEY: Option<Key> = match parse_key_hex(env!(
    "FANSTAND_KEY",
    "set FANSTAND_KEY to the 64-hex-digit key from `fanstand keygen` (or build with --features no-auth for nRF Connect testing)"
)) {
    Some(k) => Some(k),
    None => panic!("FANSTAND_KEY must be exactly 64 hex digits"),
};
/// Test builds for nRF Connect: anyone in range can change settings.
#[cfg(feature = "no-auth")]
const AUTH_KEY: Option<Key> = None;

fn main() -> anyhow::Result<()> {
    sys::link_patches();
    let boot = Instant::now();
    let p = Peripherals::take()?;

    // 1. PWM first. Until now the fans have run at 100 % on their internal
    //    PWM pull-up; bring them to the default safe speed immediately.
    let fan = fan::Fan::new(
        p.ledc.timer0,
        p.ledc.channel0,
        p.pins.gpio15,
        pct_to_permille(Config::default().safe_speed_pct),
    )?;

    esp_idf_svc::log::EspLogger::initialize_default();
    info!("fanstand firmware {FIRMWARE_VERSION}");
    let reset_code = unsafe { sys::esp_reset_reason() } as u8;
    info!(
        "reset reason: {}",
        fanstand_protocol::diag::reset_reason_name(reset_code)
    );
    info!(
        "PWM 25 kHz on GPIO15 at {}% (boot default)",
        Config::default().safe_speed_pct
    );

    // 2. Settings
    let storage = match EspDefaultNvsPartition::take()
        .map_err(anyhow::Error::from)
        .and_then(storage::Storage::new)
    {
        Ok(s) => Some(s),
        Err(e) => {
            error!("NVS unavailable ({e}); running with defaults, changes won't persist");
            None
        }
    };
    let settings = storage
        .as_ref()
        .map_or_else(Settings::default, |s| s.load());
    info!("settings: {}", control::describe(&settings));

    // 3. Tach, LED
    let now_ms = boot.elapsed().as_millis() as u64;
    let tach = [
        tach::Tach::new(p.pins.gpio16, now_ms)
            .inspect_err(|e| {
                error!("fan 1 tach init failed: {e} (will read 0 RPM → stall protection)")
            })
            .ok(),
        tach::Tach::new(p.pins.gpio19, now_ms)
            .inspect_err(|e| {
                error!("fan 2 tach init failed: {e} (will read 0 RPM → stall protection)")
            })
            .ok(),
    ];
    let mut led = PinDriver::output(p.pins.gpio2)?;
    led.set_high()?;

    // 4. Task watchdog on the control loop (reboots after 5 s without a feed)
    let mut twdt = TWDTDriver::new(
        p.twdt,
        &TWDTConfig {
            duration: Duration::from_secs(5),
            panic_on_trigger: true,
            ..Default::default()
        },
    )?;
    let watchdog = twdt.watch_current_task()?;

    // 5. Console (separate thread; talks to the loop via a channel)
    let (tx, rx) = mpsc::channel();
    let ble_tx = tx.clone();
    if let Err(e) = console::spawn(tx) {
        warn!("console unavailable: {e}");
    }
    info!("type 'help' + Enter for console commands");

    // 6. BLE
    let shared = state::new_shared(settings, reset_code, AUTH_KEY.is_none());
    if AUTH_KEY.is_none() {
        warn!("*** no-auth test build: anyone in BLE range can change settings ***");
    }
    if let Err(e) = ble::start(shared.clone(), ble_tx, AUTH_KEY, FIRMWARE_VERSION) {
        error!("BLE start failed: {e}; fans keep running without BLE");
    }

    // 7. Control loop (never returns)
    let hw = control::Hardware {
        fan,
        tach,
        led,
        storage,
    };
    control::run(hw, Controller::new(settings), rx, watchdog, boot, shared)
}
