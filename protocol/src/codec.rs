//! Wire encodings for the GATT characteristics (everything except the fan
//! curve, which lives in [`crate::curve`]).

/// Why a characteristic value or stored blob was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    WrongLength,
    OutOfRange,
    /// Curve temperatures not strictly increasing, or duty decreasing.
    NotSorted,
    UnknownValue,
    BadHeader,
    BadChecksum,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::WrongLength => "wrong length",
            Self::OutOfRange => "value out of range",
            Self::NotSorted => "curve not sorted (temps must increase, duty must not decrease)",
            Self::UnknownValue => "unknown value",
            Self::BadHeader => "bad header",
            Self::BadChecksum => "bad checksum",
        })
    }
}

impl core::error::Error for DecodeError {}

// ---------------------------------------------------------------- Mode

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    Manual = 0,
    Auto = 1,
}

impl Mode {
    pub const fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(Self::Manual),
            1 => Ok(Self::Auto),
            _ => Err(DecodeError::UnknownValue),
        }
    }
    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        match b {
            [v] => Self::from_u8(*v),
            _ => Err(DecodeError::WrongLength),
        }
    }
    pub const fn encode(self) -> [u8; 1] {
        [self as u8]
    }
}

// ---------------------------------------------------------------- Manual speed

pub fn decode_percent(b: &[u8]) -> Result<u8, DecodeError> {
    match b {
        [v] if *v <= 100 => Ok(*v),
        [_] => Err(DecodeError::OutOfRange),
        _ => Err(DecodeError::WrongLength),
    }
}

// ---------------------------------------------------------------- Temperature

/// Lowest accepted temperature, deci-°C (−40.0 °C).
pub const TEMP_MIN_DC: i16 = -400;
/// Highest accepted temperature, deci-°C (150.0 °C).
pub const TEMP_MAX_DC: i16 = 1500;

pub const fn validate_temperature(dc: i16) -> Result<i16, DecodeError> {
    if dc < TEMP_MIN_DC || dc > TEMP_MAX_DC {
        Err(DecodeError::OutOfRange)
    } else {
        Ok(dc)
    }
}

pub fn decode_temperature(b: &[u8]) -> Result<i16, DecodeError> {
    let arr: [u8; 2] = b.try_into().map_err(|_| DecodeError::WrongLength)?;
    validate_temperature(i16::from_le_bytes(arr))
}

pub const fn encode_temperature(dc: i16) -> [u8; 2] {
    dc.to_le_bytes()
}

// ---------------------------------------------------------------- RPM

pub fn encode_rpm(rpm: [u16; 2]) -> [u8; 4] {
    let a = rpm[0].to_le_bytes();
    let b = rpm[1].to_le_bytes();
    [a[0], a[1], b[0], b[1]]
}

pub fn decode_rpm(b: &[u8]) -> Result<[u16; 2], DecodeError> {
    match b {
        [a0, a1, b0, b1] => Ok([
            u16::from_le_bytes([*a0, *a1]),
            u16::from_le_bytes([*b0, *b1]),
        ]),
        _ => Err(DecodeError::WrongLength),
    }
}

// ---------------------------------------------------------------- Status

/// Status bitflags (one byte on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status(pub u8);

impl Status {
    pub const FAN1_STALLED: u8 = 1 << 0;
    pub const FAN2_STALLED: u8 = 1 << 1;
    /// Auto mode with no temperature update within the failsafe timeout:
    /// fans are at the safe speed.
    pub const FAILSAFE: u8 = 1 << 2;
    /// Auto mode and the temperature is fresh.
    pub const TEMP_FRESH: u8 = 1 << 3;
    pub const CALIBRATING: u8 = 1 << 4;

    pub const fn contains(self, flag: u8) -> bool {
        self.0 & flag == flag
    }
    pub fn set(&mut self, flag: u8, on: bool) {
        if on { self.0 |= flag } else { self.0 &= !flag }
    }
    pub const fn any_stalled(self) -> bool {
        self.0 & (Self::FAN1_STALLED | Self::FAN2_STALLED) != 0
    }
}

impl core::fmt::Display for Status {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const NAMES: [(u8, &str); 5] = [
            (Status::FAN1_STALLED, "FAN1_STALLED"),
            (Status::FAN2_STALLED, "FAN2_STALLED"),
            (Status::FAILSAFE, "FAILSAFE"),
            (Status::TEMP_FRESH, "TEMP_FRESH"),
            (Status::CALIBRATING, "CALIBRATING"),
        ];
        let mut first = true;
        for (bit, name) in NAMES {
            if self.contains(bit) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("-")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- Config

/// Accepted range for the failsafe safe speed, percent.
pub const SAFE_SPEED_RANGE: core::ops::RangeInclusive<u8> = 30..=100;
/// Accepted range for the minimum duty, percent.
pub const MIN_DUTY_RANGE: core::ops::RangeInclusive<u8> = 0..=60;
/// Accepted range for the failsafe timeout, seconds.
pub const FAILSAFE_TIMEOUT_RANGE: core::ops::RangeInclusive<u16> = 5..=600;

/// Safety-related tunables (the `CONFIG` characteristic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Duty used when Auto mode has no fresh temperature.
    pub safe_speed_pct: u8,
    /// Lowest duty ever commanded outside calibration (found by calibration).
    pub min_duty_pct: u8,
    /// Seconds without a temperature update before failsafe engages.
    pub failsafe_timeout_s: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            safe_speed_pct: 60,
            min_duty_pct: 20,
            failsafe_timeout_s: 30,
        }
    }
}

impl Config {
    pub const ENCODED_LEN: usize = 4;

    pub fn validate(self) -> Result<Self, DecodeError> {
        if SAFE_SPEED_RANGE.contains(&self.safe_speed_pct)
            && MIN_DUTY_RANGE.contains(&self.min_duty_pct)
            && FAILSAFE_TIMEOUT_RANGE.contains(&self.failsafe_timeout_s)
        {
            Ok(self)
        } else {
            Err(DecodeError::OutOfRange)
        }
    }

    pub fn encode(&self) -> [u8; 4] {
        let t = self.failsafe_timeout_s.to_le_bytes();
        [self.safe_speed_pct, self.min_duty_pct, t[0], t[1]]
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        match b {
            [s, m, t0, t1] => Self {
                safe_speed_pct: *s,
                min_duty_pct: *m,
                failsafe_timeout_s: u16::from_le_bytes([*t0, *t1]),
            }
            .validate(),
            _ => Err(DecodeError::WrongLength),
        }
    }

    pub const fn failsafe_timeout_ms(&self) -> u64 {
        self.failsafe_timeout_s as u64 * 1000
    }
}

// ---------------------------------------------------------------- Command

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    StartCalibration = 0x01,
    AbortCalibration = 0x02,
}

impl Command {
    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        match b {
            [0x01] => Ok(Self::StartCalibration),
            [0x02] => Ok(Self::AbortCalibration),
            [_] => Err(DecodeError::UnknownValue),
            _ => Err(DecodeError::WrongLength),
        }
    }
    pub const fn encode(self) -> [u8; 1] {
        [self as u8]
    }
}

// ---------------------------------------------------------------- Calibration result

/// The `CAL_RESULT` characteristic: `[state, value]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalResult {
    /// No calibration since boot. `[0, 0]`
    None,
    /// `[1, progress %]`
    Running { progress_pct: u8 },
    /// `[2, min duty %]`. The new min duty is already applied and saved.
    Ok { min_duty_pct: u8 },
    /// `[3, code]`. Min duty unchanged.
    Failed(CalErrorCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CalErrorCode {
    Aborted = 1,
    Fan1NotSpinning = 2,
    Fan2NotSpinning = 3,
    /// RPM barely changes between 0 % and 100 % (PWM wire not connected?).
    NoPwmResponse = 4,
    /// The fans need more than the max allowed min duty to keep spinning.
    MinDutyTooHigh = 5,
    /// Refused: a fan is flagged as stalled.
    StalledAtStart = 6,
}

impl CalErrorCode {
    pub const fn from_u8(v: u8) -> Result<Self, DecodeError> {
        Ok(match v {
            1 => Self::Aborted,
            2 => Self::Fan1NotSpinning,
            3 => Self::Fan2NotSpinning,
            4 => Self::NoPwmResponse,
            5 => Self::MinDutyTooHigh,
            6 => Self::StalledAtStart,
            _ => return Err(DecodeError::UnknownValue),
        })
    }
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::Fan1NotSpinning => "fan 1 not spinning at 100% (check tach wiring)",
            Self::Fan2NotSpinning => "fan 2 not spinning at 100% (check tach wiring)",
            Self::NoPwmResponse => "fan speed does not respond to PWM (check PWM wiring)",
            Self::MinDutyTooHigh => "fans need a higher min duty than allowed",
            Self::StalledAtStart => "a fan is stalled; refusing to calibrate",
        }
    }
}

impl CalResult {
    pub const fn encode(self) -> [u8; 2] {
        match self {
            Self::None => [0, 0],
            Self::Running { progress_pct } => [1, progress_pct],
            Self::Ok { min_duty_pct } => [2, min_duty_pct],
            Self::Failed(code) => [3, code as u8],
        }
    }
    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        match b {
            [0, _] => Ok(Self::None),
            [1, p] => Ok(Self::Running { progress_pct: *p }),
            [2, d] => Ok(Self::Ok { min_duty_pct: *d }),
            [3, c] => Ok(Self::Failed(CalErrorCode::from_u8(*c)?)),
            [_, _] => Err(DecodeError::UnknownValue),
            _ => Err(DecodeError::WrongLength),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_codec() {
        assert_eq!(Mode::decode(&[0]), Ok(Mode::Manual));
        assert_eq!(Mode::decode(&[1]), Ok(Mode::Auto));
        assert_eq!(Mode::decode(&[2]), Err(DecodeError::UnknownValue));
        assert_eq!(Mode::decode(&[]), Err(DecodeError::WrongLength));
        assert_eq!(Mode::decode(&[1, 0]), Err(DecodeError::WrongLength));
        assert_eq!(Mode::Auto.encode(), [1]);
    }

    #[test]
    fn percent_codec() {
        assert_eq!(decode_percent(&[0]), Ok(0));
        assert_eq!(decode_percent(&[100]), Ok(100));
        assert_eq!(decode_percent(&[101]), Err(DecodeError::OutOfRange));
        assert_eq!(decode_percent(&[]), Err(DecodeError::WrongLength));
    }

    #[test]
    fn temperature_codec() {
        assert_eq!(decode_temperature(&encode_temperature(456)), Ok(456));
        assert_eq!(decode_temperature(&encode_temperature(-400)), Ok(-400));
        assert_eq!(decode_temperature(&encode_temperature(1500)), Ok(1500));
        assert_eq!(
            decode_temperature(&encode_temperature(1501)),
            Err(DecodeError::OutOfRange)
        );
        assert_eq!(
            decode_temperature(&encode_temperature(-401)),
            Err(DecodeError::OutOfRange)
        );
        // 45.6 °C = 456 = 0x01C8, little-endian
        assert_eq!(encode_temperature(456), [0xC8, 0x01]);
        assert_eq!(decode_temperature(&[1]), Err(DecodeError::WrongLength));
    }

    #[test]
    fn rpm_codec() {
        let e = encode_rpm([1550, 0x1234]);
        assert_eq!(e, [0x0E, 0x06, 0x34, 0x12]);
        assert_eq!(decode_rpm(&e), Ok([1550, 0x1234]));
        assert_eq!(decode_rpm(&e[..3]), Err(DecodeError::WrongLength));
    }

    #[test]
    fn status_flags() {
        let mut s = Status::default();
        assert!(!s.any_stalled());
        s.set(Status::FAN2_STALLED, true);
        s.set(Status::FAILSAFE, true);
        assert!(s.any_stalled());
        assert_eq!(s.0, 0b0000_0110);
        assert_eq!(std::format!("{s}"), "FAN2_STALLED|FAILSAFE");
        s.set(Status::FAN2_STALLED, false);
        assert!(!s.any_stalled());
        assert_eq!(std::format!("{}", Status(0)), "-");
    }

    #[test]
    fn config_codec_and_validation() {
        let c = Config::default();
        assert_eq!(Config::decode(&c.encode()), Ok(c));
        assert_eq!(c.encode(), [60, 20, 30, 0]);
        assert_eq!(
            Config::decode(&[29, 20, 30, 0]),
            Err(DecodeError::OutOfRange)
        );
        assert_eq!(
            Config::decode(&[101, 20, 30, 0]),
            Err(DecodeError::OutOfRange)
        );
        assert_eq!(
            Config::decode(&[60, 61, 30, 0]),
            Err(DecodeError::OutOfRange)
        );
        assert_eq!(
            Config::decode(&[60, 20, 4, 0]),
            Err(DecodeError::OutOfRange)
        );
        assert_eq!(
            Config::decode(&[60, 20, 0x59, 0x02]),
            Err(DecodeError::OutOfRange)
        ); // 601 s
        assert_eq!(
            Config::decode(&[60, 20, 0x58, 0x02]).map(|c| c.failsafe_timeout_s),
            Ok(600)
        );
        assert_eq!(Config::decode(&[60, 20, 30]), Err(DecodeError::WrongLength));
    }

    #[test]
    fn command_codec() {
        assert_eq!(Command::decode(&[1]), Ok(Command::StartCalibration));
        assert_eq!(Command::decode(&[2]), Ok(Command::AbortCalibration));
        assert_eq!(Command::decode(&[9]), Err(DecodeError::UnknownValue));
    }

    #[test]
    fn cal_result_codec() {
        for r in [
            CalResult::None,
            CalResult::Running { progress_pct: 42 },
            CalResult::Ok { min_duty_pct: 15 },
            CalResult::Failed(CalErrorCode::NoPwmResponse),
        ] {
            assert_eq!(CalResult::decode(&r.encode()), Ok(r));
        }
        assert_eq!(CalResult::decode(&[3, 99]), Err(DecodeError::UnknownValue));
    }
}
