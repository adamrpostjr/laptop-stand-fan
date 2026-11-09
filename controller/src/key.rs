//! The BLE shared secret: 32 random bytes as 64 hex digits.
//!
//! Lookup order: the `FANSTAND_KEY` env var, then `<config dir>/fanstand/key`:
//! - Linux:   `~/.config/fanstand/key`
//! - macOS:   `~/Library/Application Support/fanstand/key`
//! - Windows: `%APPDATA%\fanstand\key`
//!
//! The same key is compiled into the firmware (`FANSTAND_KEY` at build time).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use fanstand_protocol::auth::{KEY_LEN, Key, parse_key_hex};

pub fn key_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no per-user config directory on this OS")?
        .join("fanstand")
        .join("key"))
}

pub fn load() -> Result<Key> {
    if let Ok(hex) = std::env::var("FANSTAND_KEY") {
        return parse_key_hex(hex.trim()).context("FANSTAND_KEY is not 64 hex digits");
    }
    let path = key_path()?;
    let hex = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no key at {} (copy the key from the machine you flashed from, or run `fanstand keygen` and reflash)",
            path.display()
        )
    })?;
    parse_key_hex(hex.trim()).with_context(|| format!("{} is not 64 hex digits", path.display()))
}

pub fn generate(force: bool) -> Result<(PathBuf, String)> {
    let path = key_path()?;
    if path.exists() && !force {
        bail!(
            "{} already exists. The firmware is built with it; use --force only if you will reflash",
            path.display()
        );
    }
    let mut k = [0u8; KEY_LEN];
    getrandom::fill(&mut k).map_err(|e| anyhow::anyhow!("OS RNG failed: {e}"))?;
    let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(path.parent().unwrap())?;
    write_private(&path, &hex)?;
    Ok((path, hex))
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, hex: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    writeln!(f, "{hex}")?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, hex: &str) -> Result<()> {
    // %APPDATA% is already per-user (ACL'd to the account).
    std::fs::write(path, format!("{hex}\n"))?;
    Ok(())
}
