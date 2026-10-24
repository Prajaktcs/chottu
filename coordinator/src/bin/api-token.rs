//! `just api-token <device>`: mint an app API token for one device.
//!
//! Prints the raw token once (enter it in the app) and the `.env` line holding
//! its Argon2id hash, which the server checks it against. The raw token is never stored.

use anyhow::{bail, Result};
use chotu_common::api_token::{api_token_env_key, hash_api_token, mint_api_token};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(device), None) = (args.next(), args.next()) else {
        bail!("usage: just api-token <device>");
    };

    let token = mint_api_token(&device).map_err(anyhow::Error::msg)?;
    let hash = hash_api_token(&token).map_err(anyhow::Error::msg)?;
    println!("Token for {device} (shown once; enter it in the app):");
    println!();
    println!("  {token}");
    println!();
    println!("Add this line to .env, list {device:?} under api.devices in config.yaml, then restart Chotu.");
    println!("Keep the single quotes: the hash contains `$`.");
    println!();
    println!("  {}='{hash}'", api_token_env_key(&device));
    Ok(())
}
