//! Fan PWM: LEDC at 25 kHz on GPIO15, shared by both fans. Non-inverted:
//! a higher duty means faster fans.

use anyhow::Result;
use esp_idf_svc::hal::gpio::OutputPin;
use esp_idf_svc::hal::ledc::config::{Resolution, TimerConfig};
use esp_idf_svc::hal::ledc::{LedcChannel, LedcDriver, LedcTimer, LedcTimerDriver};
use esp_idf_svc::hal::units::FromValueType;

pub const PWM_FREQ_KHZ: u32 = 25;

pub struct Fan<'d> {
    channel: LedcDriver<'d>,
    max_duty: u32,
    permille: u16,
}

impl<'d> Fan<'d> {
    /// Configures the timer and channel and sets `initial_permille` straight away.
    pub fn new<T, C>(
        timer: T,
        channel: C,
        pin: impl OutputPin + 'd,
        initial_permille: u16,
    ) -> Result<Self>
    where
        T: LedcTimer + 'd,
        C: LedcChannel<SpeedMode = T::SpeedMode> + 'd,
    {
        // 80 MHz APB / 25 kHz = 3200 counts, so up to 11 bits are possible. 10 bits = 1024 steps.
        let timer = LedcTimerDriver::new(
            timer,
            &TimerConfig::new()
                .frequency(PWM_FREQ_KHZ.kHz().into())
                .resolution(Resolution::Bits10),
        )?;
        let channel = LedcDriver::new(channel, timer, pin)?;
        let max_duty = channel.get_max_duty();
        let mut fan = Self {
            channel,
            max_duty,
            permille: u16::MAX,
        };
        fan.set_permille(initial_permille)?;
        Ok(fan)
    }

    pub fn set_permille(&mut self, permille: u16) -> Result<()> {
        let p = permille.min(1000);
        if p != self.permille {
            let duty = (self.max_duty as u64 * p as u64 / 1000) as u32;
            self.channel.set_duty(duty)?;
            self.permille = p;
        }
        Ok(())
    }
}
