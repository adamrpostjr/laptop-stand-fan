//! App-level authentication: HMAC-SHA256 challenge-response over a 32-byte
//! pre-shared key.
//!
//! 1. The client reads `AUTH_CHALLENGE` and gets a random 16-byte nonce. The
//!    stand creates a new one for every connection and after every attempt.
//! 2. The client writes `HMAC-SHA256(key, DOMAIN ‖ nonce)` (32 bytes) to
//!    `AUTH_RESPONSE`.
//! 3. If it matches, the connection is authenticated until it disconnects,
//!    and writes to the settings/temperature characteristics are accepted.
//!
//! The link itself is not encrypted, so anyone in range can *read* RPM and
//! status. The key never goes over the air, and a captured response can't be
//! replayed because the nonce changes.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 16;
pub const RESPONSE_LEN: usize = 32;
pub const DOMAIN: &[u8] = b"fanstand-auth-v1";

pub type Key = [u8; KEY_LEN];
pub type Nonce = [u8; NONCE_LEN];

fn mac(key: &Key, nonce: &Nonce) -> Hmac<Sha256> {
    let mut m =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(DOMAIN);
    m.update(nonce);
    m
}

/// What the client writes to `AUTH_RESPONSE`.
pub fn compute_response(key: &Key, nonce: &Nonce) -> [u8; RESPONSE_LEN] {
    let out = mac(key, nonce).finalize().into_bytes();
    let mut r = [0u8; RESPONSE_LEN];
    r.copy_from_slice(&out);
    r
}

/// Constant-time check of a client's response.
pub fn verify_response(key: &Key, nonce: &Nonce, response: &[u8]) -> bool {
    response.len() == RESPONSE_LEN && mac(key, nonce).verify_slice(response).is_ok()
}

const fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse a 64-hex-digit key. `const` so the firmware can parse a build-time
/// `env!` and fail the build if the key is malformed.
pub const fn parse_key_hex(s: &str) -> Option<Key> {
    let b = s.as_bytes();
    if b.len() != KEY_LEN * 2 {
        return None;
    }
    let mut out = [0u8; KEY_LEN];
    let mut i = 0;
    while i < KEY_LEN {
        let (Some(hi), Some(lo)) = (hex_val(b[2 * i]), hex_val(b[2 * i + 1])) else {
            return None;
        };
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(out)
}

/// Lower-case hex form of a key (inverse of [`parse_key_hex`]).
pub struct KeyHex<'a>(pub &'a Key);

impl core::fmt::Display for KeyHex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq<const N: usize>() -> [u8; N] {
        core::array::from_fn(|i| i as u8)
    }

    #[test]
    fn known_vector() {
        // python3: hmac.new(bytes(range(32)), b'fanstand-auth-v1' + bytes(range(16)), sha256)
        let expected =
            parse_key_hex("10e28affe9f5f1f735eb3a42e8b007e5573934c89fb14fc1b5b6c09a9de790cf")
                .unwrap();
        assert_eq!(compute_response(&seq(), &seq()), expected);
    }

    #[test]
    fn verify() {
        let (k, n) = (seq::<32>(), seq::<16>());
        let r = compute_response(&k, &n);
        assert!(verify_response(&k, &n, &r));
        let mut bad = r;
        bad[31] ^= 1;
        assert!(!verify_response(&k, &n, &bad));
        assert!(!verify_response(&k, &n, &r[..31]));
        assert!(!verify_response(&k, &n, &[]));
        let mut n2 = n;
        n2[0] ^= 1;
        assert!(
            !verify_response(&k, &n2, &r),
            "replay with different nonce accepted"
        );
        let mut k2 = k;
        k2[0] ^= 1;
        assert!(!verify_response(&k2, &n, &r), "wrong key accepted");
    }

    #[test]
    fn hex_parsing() {
        const K: Option<Key> =
            parse_key_hex("000102030405060708090a0b0c0d0e0f101112131415161718191A1B1C1D1E1F");
        assert_eq!(K, Some(seq()));
        assert_eq!(parse_key_hex(""), None);
        assert_eq!(parse_key_hex(&"0".repeat(63)), None);
        assert_eq!(parse_key_hex(&"0".repeat(65)), None);
        assert_eq!(parse_key_hex(&"g".repeat(64)), None);
        let k: Key = seq();
        assert_eq!(parse_key_hex(&std::format!("{}", KeyHex(&k))), Some(k));
    }
}
