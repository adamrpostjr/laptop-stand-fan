//! Tachometer input. Each fan gets one PCNT unit that counts rising edges.
//!
//! The tach outputs are open-collector, and there are no external resistors,
//! so we enable the ESP32's internal pull-up (~45 kΩ). The PCNT glitch filter
//! drops short noise spikes. A real tach pulse is milliseconds long
//! (1550 RPM × 2 / 60 ≈ 52 Hz).

use anyhow::Result;
use core::time::Duration;
use esp_idf_svc::hal::gpio::{AnyInputPin, InputPin};
use esp_idf_svc::hal::pcnt::PcntUnitDriver;
use esp_idf_svc::hal::pcnt::config::{
    ChannelConfig, ChannelEdgeAction, GlitchFilterConfig, UnitConfig,
};
use esp_idf_svc::sys;
use fanstand_protocol::rpm::{PULSES_PER_REV, RpmSmoother, pulses_to_rpm};
use log::{info, warn};

/// Glitch filter width. The ESP32 hardware maximum is 1023 APB cycles ≈ 12.8 µs.
const GLITCH_NS: u64 = 10_000;
const GLITCH_FALLBACK_NS: u64 = 1_000;

pub struct Tach<'d> {
    unit: PcntUnitDriver<'d>,
    gpio: i32,
    smoother: RpmSmoother,
    last_sample_ms: u64,
    raw_rpm: u16,
    /// High samples so far in the current window.
    probe_acc: u8,
    /// High samples in the last window (see [`Tach::probe_level`]).
    last_probe_high: u8,
}

impl<'d> Tach<'d> {
    pub fn new(pin: impl InputPin + 'd, now_ms: u64) -> Result<Self> {
        let gpio = pin.pin() as i32;
        let mut unit = PcntUnitDriver::new(&UnitConfig {
            low_limit: -1,
            high_limit: i16::MAX as i32,
            ..Default::default()
        })?;

        let filter = |ns| GlitchFilterConfig {
            max_glitch: Duration::from_nanos(ns),
            ..Default::default()
        };
        if let Err(e) = unit.set_glitch_filter(Some(&filter(GLITCH_NS))) {
            warn!(
                "GPIO{gpio}: {GLITCH_NS} ns glitch filter rejected ({e}); using {GLITCH_FALLBACK_NS} ns"
            );
            unit.set_glitch_filter(Some(&filter(GLITCH_FALLBACK_NS)))?;
        }

        unit.add_channel(
            Some(pin),
            Option::<AnyInputPin>::None,
            &ChannelConfig::default(),
        )?
        // count rising edges only: 2 per revolution
        .set_edge_action(ChannelEdgeAction::Increase, ChannelEdgeAction::Hold)?;

        // The PCNT driver routes the pin but doesn't guarantee a pull-up, so set it explicitly.
        sys::esp!(unsafe {
            sys::gpio_set_pull_mode(gpio, sys::gpio_pull_mode_t_GPIO_PULLUP_ONLY)
        })?;

        unit.enable()?;
        unit.clear_count()?;
        unit.start()?;
        info!("tach on GPIO{gpio}: PCNT, pull-up on, glitch filter on");

        Ok(Self {
            unit,
            gpio,
            smoother: RpmSmoother::new(1),
            last_sample_ms: now_ms,
            raw_rpm: 0,
            probe_acc: 0,
            last_probe_high: 0,
        })
    }

    /// Read and reset the pulse count, then update the RPM. Call about once per second.
    pub fn sample(&mut self, now_ms: u64) -> u16 {
        let window = now_ms.saturating_sub(self.last_sample_ms) as u32;
        self.last_sample_ms = now_ms;
        let pulses = match self
            .unit
            .get_count()
            .and_then(|c| self.unit.clear_count().map(|_| c))
        {
            Ok(c) => c.max(0) as u32,
            Err(e) => {
                warn!("GPIO{}: PCNT read failed: {e}", self.gpio);
                0
            }
        };
        self.raw_rpm = pulses_to_rpm(pulses, window, PULSES_PER_REV);
        self.last_probe_high = self.probe_acc;
        self.probe_acc = 0;
        self.smoother.update(self.raw_rpm)
    }

    /// Sample the raw logic level (called every control tick). This tells a
    /// toggling tach line apart from one stuck high (disconnected) or stuck low.
    pub fn probe_level(&mut self) {
        let high = unsafe { sys::gpio_get_level(self.gpio) } != 0;
        self.probe_acc = self.probe_acc.saturating_add(high as u8);
    }

    pub fn probe_high(&self) -> u8 {
        self.last_probe_high
    }

    pub fn raw_rpm(&self) -> u16 {
        self.raw_rpm
    }
}
