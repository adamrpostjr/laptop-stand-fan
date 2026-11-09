//! GATT UUIDs for the fan stand's custom service.
//!
//! All UUIDs share a random 128-bit base (`29daXXXX-3c6c-4a9d-be74-2d57f6f02892`)
//! with a 16-bit discriminator in bytes 2..4.
//!
//! UUIDs are `u128`s in the usual big-endian textual order, so
//! `uuid::Uuid::from_u128(SERVICE)` gives the textual form. [`to_be_bytes`]
//! is for stacks that want bytes.

const BASE: u128 = 0x29da_0000_3c6c_4a9d_be74_2d57_f6f0_2892;

const fn make(short: u16) -> u128 {
    BASE | ((short as u128) << 96)
}

/// The fan stand primary service (advertised; the controller scans for it).
pub const SERVICE: u128 = make(0x0001);

/// `u8`, read/write. 0 = manual, 1 = auto. See [`crate::Mode`].
pub const MODE: u128 = make(0x0101);
/// `u8` 0..=100, read/write.
pub const MANUAL_SPEED: u128 = make(0x0102);
/// `i16` LE, 0.1 °C units. Write / write-without-response.
pub const TEMPERATURE: u128 = make(0x0103);
/// 1..=8 × (`u8` °C, `u8` duty %), read/write. See [`crate::FanCurve`].
pub const FAN_CURVE: u128 = make(0x0104);
/// 2 × `u16` LE (fan 1, fan 2), read + notify every 1 s.
pub const FAN_RPM: u128 = make(0x0105);
/// `u8` bitflags, read + notify. See [`crate::Status`].
pub const STATUS: u128 = make(0x0106);
/// UTF-8 string, read.
pub const FIRMWARE_VERSION: u128 = make(0x0107);
/// 4 bytes: safe speed %, min duty %, failsafe timeout s (`u16` LE). read/write.
/// See [`crate::Config`].
pub const CONFIG: u128 = make(0x0108);
/// `u8` command, write. See [`crate::Command`].
pub const COMMAND: u128 = make(0x0109);
/// 16-byte nonce, read. Fresh per connection and after every auth attempt.
pub const AUTH_CHALLENGE: u128 = make(0x010A);
/// Write the 32-byte HMAC response. Read returns `[1]` if this connection is
/// authenticated, `[0]` otherwise.
pub const AUTH_RESPONSE: u128 = make(0x010B);
/// 2 bytes, read + notify. See [`crate::CalResult`].
pub const CAL_RESULT: u128 = make(0x010C);
/// 22 bytes of live diagnostics, read. See [`crate::diag::DebugInfo`].
pub const DEBUG: u128 = make(0x010D);
/// Last calibration table, 21 × (fan1, fan2) `u16` LE RPM (100 % → 0 %), read.
pub const CAL_TABLE: u128 = make(0x010E);

/// Every characteristic UUID, for iteration/tests.
pub const ALL_CHARACTERISTICS: [u128; 14] = [
    MODE,
    MANUAL_SPEED,
    TEMPERATURE,
    FAN_CURVE,
    FAN_RPM,
    STATUS,
    FIRMWARE_VERSION,
    CONFIG,
    COMMAND,
    AUTH_CHALLENGE,
    AUTH_RESPONSE,
    CAL_RESULT,
    DEBUG,
    CAL_TABLE,
];

/// Device name in the advertisement (the controller filters on [`SERVICE`], not this).
pub const DEVICE_NAME: &str = "FanStand";

/// Big-endian byte form (same order as the textual UUID).
pub const fn to_be_bytes(uuid: u128) -> [u8; 16] {
    uuid.to_be_bytes()
}

/// Little-endian byte form (the order used on the air / by NimBLE's `ble_uuid128_t`).
pub const fn to_le_bytes(uuid: u128) -> [u8; 16] {
    uuid.to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_unique_and_share_base() {
        let mut all = std::vec::Vec::from(ALL_CHARACTERISTICS);
        all.push(SERVICE);
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n);
        for u in all {
            assert_eq!(u & !(0xffffu128 << 96), BASE);
        }
    }

    #[test]
    fn service_textual_form() {
        // 29da0001-3c6c-4a9d-be74-2d57f6f02892
        assert_eq!(
            to_be_bytes(SERVICE),
            [
                0x29, 0xda, 0x00, 0x01, 0x3c, 0x6c, 0x4a, 0x9d, 0xbe, 0x74, 0x2d, 0x57, 0xf6, 0xf0,
                0x28, 0x92
            ]
        );
        assert_eq!(to_le_bytes(SERVICE)[0], 0x92);
    }
}
