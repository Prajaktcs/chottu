//! `just api-token <device>`: mint an app API token for one device.
//!
//! Prints the raw token once (enter it in the app) and the hashed `.env` line
//! the server checks it against. The raw token is never stored.

use anyhow::{anyhow, bail, Result};
use chotu_common::api_token::{
    api_token_env_key, hash_api_token, mint_api_token, token_hash_hex, validate_device_name,
};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(device), None) = (args.next(), args.next()) else {
        bail!("usage: just api-token <device>");
    };
    validate_device_name(&device).map_err(anyhow::Error::msg)?;

    let token = mint_api_token().map_err(|e| anyhow!("failed to read OS randomness: {e}"))?;
    let hash = token_hash_hex(&hash_api_token(&token));
    println!("Token for {device} (shown once; enter it in the app):");
    println!();
    println!("  {token}");
    println!();
    println!("Add this line to .env, list {device:?} under api.devices in config.yaml, then restart Chotu:");
    println!();
    println!("  {}={hash}", api_token_env_key(&device));
    Ok(())
}
