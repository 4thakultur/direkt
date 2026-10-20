//! Time-based one-time passwords (RFC 6238) for two-factor sign-in.
//!
//! Uses the parameters every authenticator app supports (Google
//! Authenticator, Microsoft Authenticator, Aegis, 1Password, …): HMAC-SHA1,
//! six digits, 30-second steps, and a 160-bit secret shown as base32.

use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use sha1::Sha1;

pub const DIGITS: usize = 6;
pub const STEP_SECS: i64 = 30;
/// Steps either side of now that are accepted, for clock drift.
const SKEW_STEPS: i64 = 1;
const SECRET_BYTES: usize = 20;
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
/// White modules around the code, as the QR specification requires.
const QR_QUIET_ZONE: usize = 4;

/// A new random shared secret, base32-encoded.
pub fn generate_secret() -> String {
    let mut secret = [0u8; SECRET_BYTES];
    getrandom::fill(&mut secret).expect("operating system RNG is unavailable");
    encode_base32(&secret)
}

/// RFC 4648 base32 without padding, as authenticator apps expect.
pub fn encode_base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Decode base32, ignoring case, spaces and padding.
pub fn decode_base32(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let value = BASE32
            .iter()
            .position(|&b| b as char == c.to_ascii_uppercase())?;
        buffer = (buffer << 5) | value as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// The 30-second step a Unix time falls in.
pub fn step_at(unix_secs: i64) -> i64 {
    unix_secs.div_euclid(STEP_SECS)
}

/// The code for one step (RFC 4226 dynamic truncation).
pub fn code_at(secret: &[u8], step: i64) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let value = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    format!(
        "{:0width$}",
        value % 10u32.pow(DIGITS as u32),
        width = DIGITS
    )
}

/// Whether `code` looks like an authenticator code (six digits, spaces
/// allowed), as opposed to a recovery code.
pub fn looks_like_code(code: &str) -> bool {
    let digits: Vec<char> = code.chars().filter(|c| !c.is_whitespace()).collect();
    digits.len() == DIGITS && digits.iter().all(char::is_ascii_digit)
}

/// The step whose code matches `code` near `unix_secs`, if any. Every
/// candidate is compared, so timing does not reveal which step matched.
pub fn verify(secret: &[u8], code: &str, unix_secs: i64) -> Option<i64> {
    if !looks_like_code(code) {
        return None;
    }
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let now = step_at(unix_secs);
    let mut matched = None;
    for step in now - SKEW_STEPS..=now + SKEW_STEPS {
        if constant_time_eq(code_at(secret, step).as_bytes(), code.as_bytes()) {
            matched = Some(step);
        }
    }
    matched
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The `otpauth://` link an authenticator app reads from the QR code.
pub fn otpauth_uri(secret_base32: &str, issuer: &str, account: &str) -> String {
    let issuer = utf8_percent_encode(issuer, NON_ALPHANUMERIC);
    let account = utf8_percent_encode(account, NON_ALPHANUMERIC);
    format!(
        "otpauth://totp/{issuer}:{account}?secret={secret_base32}&issuer={issuer}\
         &algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}"
    )
}

/// A QR code as an SVG path of dark modules, in module units.
pub struct QrSvg {
    /// Side length including the quiet zone.
    pub size: usize,
    pub path: String,
}

pub fn qr_svg(data: &str) -> Option<QrSvg> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let mut path = String::new();
    for (i, color) in code.to_colors().into_iter().enumerate() {
        if color == qrcode::Color::Dark {
            let (x, y) = (i % width + QR_QUIET_ZONE, i / width + QR_QUIET_ZONE);
            path.push_str(&format!("M{x} {y}h1v1h-1z"));
        }
    }
    Some(QrSvg {
        size: width + 2 * QR_QUIET_ZONE,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 6238 appendix B secret for SHA-1.
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn rfc_6238_test_vectors() {
        // The RFC lists eight digits; six-digit codes are their last six.
        for (time, code) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(code_at(RFC_SECRET, step_at(time)), code, "at {time}");
        }
    }

    #[test]
    fn base32_round_trips() {
        assert_eq!(encode_base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(decode_base32("mzxw 6ytb oi======").unwrap(), b"foobar");
        assert_eq!(decode_base32("MZ1"), None, "1 is not base32");
        let secret = generate_secret();
        assert_eq!(secret.len(), 32);
        assert_eq!(decode_base32(&secret).unwrap().len(), SECRET_BYTES);
    }

    #[test]
    fn verify_accepts_one_step_of_drift() {
        let now = 1_234_567_890;
        let step = step_at(now);
        assert_eq!(verify(RFC_SECRET, "005924", now), Some(step));
        assert_eq!(verify(RFC_SECRET, "005 924", now), Some(step));
        let late = code_at(RFC_SECRET, step - 1);
        assert_eq!(verify(RFC_SECRET, &late, now), Some(step - 1));
        let early = code_at(RFC_SECRET, step + 1);
        assert_eq!(verify(RFC_SECRET, &early, now), Some(step + 1));
        let stale = code_at(RFC_SECRET, step - 2);
        assert_eq!(verify(RFC_SECRET, &stale, now), None);
        assert_eq!(verify(RFC_SECRET, "00592", now), None);
        assert_eq!(verify(RFC_SECRET, "abcdef", now), None);
    }

    #[test]
    fn uri_escapes_the_label() {
        assert_eq!(
            otpauth_uri("ABC", "Direkt", "a b:c"),
            "otpauth://totp/Direkt:a%20b%3Ac?secret=ABC&issuer=Direkt&algorithm=SHA1&digits=6&period=30"
        );
    }

    #[test]
    fn qr_code_has_a_quiet_zone() {
        let qr = qr_svg(&otpauth_uri("ABC", "Direkt", "admin")).unwrap();
        assert!(qr.size >= 21 + 2 * QR_QUIET_ZONE);
        assert!(qr.path.starts_with("M4 4h1v1h-1z"), "finder pattern corner");
    }
}
