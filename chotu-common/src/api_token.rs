//! Device bearer tokens for the app API.
//!
//! Each device gets one token, `<device>.<64 hex chars>` (256 random bits).
//! Only its Argon2id hash (a PHC string, OWASP-recommended parameters) is
//! stored, as `API_TOKEN_<DEVICE>` in `.env`; the raw token is shown once by
//! `just api-token <device>` and lives only on the device.

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;

/// A parsed Argon2id PHC string.
pub type TokenHash = PasswordHash;

const SECRET_BYTES: usize = 32;

/// Separates the device name from the secret. Never valid in a device name.
const SEPARATOR: char = '.';

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

/// A new random token for `device`: `<device>.<64 hex chars>`.
pub fn mint_api_token(device: &str) -> Result<String, String> {
    validate_device_name(device)?;
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| format!("failed to read OS randomness: {e}"))?;
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{device}{SEPARATOR}{secret}"))
}

/// The device a presented token claims to be. Says nothing about whether it is valid.
pub fn token_device(token: &str) -> Option<&str> {
    token
        .split_once(SEPARATOR)
        .map(|(device, _)| device)
        .filter(|device| validate_device_name(device).is_ok())
}

/// Argon2id hash of a token, as the PHC string stored in `.env`. Salted at random.
pub fn hash_api_token(token: &str) -> Result<String, String> {
    let hash: PasswordHash = Argon2::default()
        .hash_password(token.as_bytes())
        .map_err(|e| format!("failed to hash token: {e}"))?;
    Ok(hash.to_string())
}

/// Parses the PHC string stored in `API_TOKEN_<DEVICE>`. Only Argon2id is accepted.
pub fn parse_token_hash(stored: &str) -> Result<TokenHash, String> {
    let hash = PasswordHash::new(stored.trim())
        .map_err(|_| "expected an Argon2id PHC string from `just api-token`".to_string())?;
    if hash.algorithm.as_str() != "argon2id" || hash.hash.is_none() {
        return Err("expected an Argon2id PHC string from `just api-token`".to_string());
    }
    Ok(hash)
}

/// True when `token` matches `hash`. Uses the parameters stored in the hash and
/// compares in constant time. CPU- and memory-hard: call off the async executor.
pub fn verify_api_token(token: &str, hash: &TokenHash) -> bool {
    Argon2::default()
        .verify_password(token.as_bytes(), hash)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_token_verifies_against_its_stored_hash_only() {
        let token = mint_api_token("alex_phone").unwrap();
        assert!(token.starts_with("alex_phone."));
        assert_eq!(token.len(), "alex_phone.".len() + 64);
        assert_ne!(token, mint_api_token("alex_phone").unwrap());
        assert_eq!(token_device(&token), Some("alex_phone"));

        let stored = hash_api_token(&token).unwrap();
        assert!(stored.starts_with("$argon2id$v=19$"), "{stored}");
        assert_ne!(stored, hash_api_token(&token).unwrap(), "salt is random");

        let parsed = parse_token_hash(&stored).unwrap();
        assert!(verify_api_token(&token, &parsed));
        assert!(!verify_api_token("alex_phone.other", &parsed));
    }

    #[test]
    fn rejects_stored_values_that_are_not_argon2id() {
        let sha256_hex = "a".repeat(64);
        let argon2i =
            "$argon2i$v=19$m=16,t=2,p=1$c29tZXNhbHQ$9sTbSlTio3Biev89thdrlKKiCaYsjjYVJxGAL3swxpQ";
        for bad in ["", "abc", sha256_hex.as_str(), argon2i] {
            assert!(parse_token_hash(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn device_names_are_env_key_safe() {
        assert!(validate_device_name("praj_phone2").is_ok());
        assert_eq!(api_token_env_key("praj_phone"), "API_TOKEN_PRAJ_PHONE");
        for bad in ["", "praj-phone", "praj phone", "ipad.1"] {
            assert!(validate_device_name(bad).is_err(), "{bad:?}");
            assert!(mint_api_token(bad).is_err(), "{bad:?}");
        }
        assert_eq!(token_device("no-separator"), None);
        assert_eq!(token_device("bad-name.secret"), None);
    }
}
