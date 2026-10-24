//! Device bearer tokens for the app API.
//!
//! Each device gets one random 256-bit token. Only its SHA-256 hash is stored,
//! as `API_TOKEN_<DEVICE>` in `.env`; the raw token is shown once by
//! `just api-token <device>` and lives only on the device.

use sha2::{Digest, Sha256};

/// SHA-256 of a raw token.
pub type TokenHash = [u8; 32];

const TOKEN_BYTES: usize = 32;

/// Env key holding a device's token hash, e.g. device "praj_phone" → "API_TOKEN_PRAJ_PHONE".
pub fn api_token_env_key(device: &str) -> String {
    format!("API_TOKEN_{}", device.to_uppercase())
}

/// Device names become env key suffixes, so they are limited to ASCII letters, digits and `_`.
pub fn validate_device_name(device: &str) -> Result<(), String> {
    if !device.is_empty()
        && device
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        Ok(())
    } else {
        Err(format!(
            "device name {device:?} must be non-empty and use only ASCII letters, digits and '_'"
        ))
    }
}

/// A new random token, hex-encoded (64 characters).
pub fn mint_api_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)?;
    Ok(to_hex(&bytes))
}

pub fn hash_api_token(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

pub fn token_hash_hex(hash: &TokenHash) -> String {
    to_hex(hash)
}

/// Parses the 64-hex-character value stored in `API_TOKEN_<DEVICE>`.
pub fn parse_token_hash(hex: &str) -> Result<TokenHash, String> {
    let hex = hex.trim();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("expected a 64-character hex SHA-256 hash".to_string());
    }
    let mut hash = [0u8; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| "expected a 64-character hex SHA-256 hash".to_string())?;
    }
    Ok(hash)
}

/// Compares two hashes without an early exit, so timing does not reveal how many bytes match.
pub fn token_hashes_match(a: &TokenHash, b: &TokenHash) -> bool {
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_token_round_trips_through_stored_hash() {
        let token = mint_api_token().unwrap();
        assert_eq!(token.len(), 64);
        assert_ne!(token, mint_api_token().unwrap());

        let stored = token_hash_hex(&hash_api_token(&token));
        let parsed = parse_token_hash(&stored).unwrap();
        assert!(token_hashes_match(&parsed, &hash_api_token(&token)));
        assert!(!token_hashes_match(&parsed, &hash_api_token("other")));
    }

    #[test]
    fn rejects_malformed_stored_hash() {
        for bad in ["", "abc", &"g".repeat(64), &"a".repeat(63), &"é".repeat(32)] {
            assert!(parse_token_hash(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn device_names_are_env_key_safe() {
        assert!(validate_device_name("praj_phone2").is_ok());
        assert_eq!(api_token_env_key("praj_phone"), "API_TOKEN_PRAJ_PHONE");
        for bad in ["", "praj-phone", "praj phone", "ipad.1"] {
            assert!(validate_device_name(bad).is_err(), "{bad:?}");
        }
    }
}
