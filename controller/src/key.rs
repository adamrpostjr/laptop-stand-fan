//! The BLE shared secret: 32 random bytes as 64 hex digits.
//!
//! Lookup order: the `FANSTAND_KEY` env var, then `<config dir>/fanstand/key`:
//! - Linux:   `~/.config/fanstand/key`
//! - macOS:   `~/Library/Application Support/fanstand/key`
//! - Windows: `%APPDATA%\fanstand\key`
//!
//! then, on Windows, the machine-wide key the service uses:
//! `%ProgramData%\fanstand\private\key`, readable only by SYSTEM and
//! Administrators (`fanstand service install` or the MSI puts it there).
//!
//! The same key is compiled into the firmware (`FANSTAND_KEY` at build time).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use fanstand_protocol::auth::{KEY_LEN, Key, KeyHex, parse_key_hex};

pub fn key_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no per-user config directory on this OS")?
        .join("fanstand")
        .join("key"))
}

/// `%ProgramData%\fanstand` (Windows only).
#[cfg(windows)]
pub fn program_data() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join("fanstand")
}

#[cfg(windows)]
pub fn machine_key_path() -> PathBuf {
    program_data().join("private").join("key")
}

pub fn load() -> Result<Key> {
    if let Ok(hex) = std::env::var("FANSTAND_KEY") {
        return parse_key_hex(hex.trim()).context("FANSTAND_KEY is not 64 hex digits");
    }
    let mut paths = vec![];
    // LocalSystem has a config dir too (systemprofile); harmless to look.
    if let Ok(p) = key_path() {
        paths.push(p);
    }
    #[cfg(windows)]
    paths.push(machine_key_path());
    for path in &paths {
        match std::fs::read_to_string(path) {
            Ok(hex) => {
                return parse_key_hex(hex.trim())
                    .with_context(|| format!("{} is not 64 hex digits", path.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
    let tried: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
    bail!(
        "no key found (looked in {}). Copy the key from the machine you flashed from, or run `fanstand keygen` and reflash",
        tried.join(", ")
    )
}

/// Read and validate a key file, returning the normalised hex.
pub fn read_key_file(path: &std::path::Path) -> Result<String> {
    let hex = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let k = parse_key_hex(hex.trim())
        .with_context(|| format!("{} is not 64 hex digits", path.display()))?;
    Ok(KeyHex(&k).to_string())
}

/// Write the machine-wide key and lock it down to SYSTEM + Administrators.
/// Needs an elevated process.
#[cfg(windows)]
pub fn install_machine_key(hex: &str) -> Result<PathBuf> {
    let path = machine_key_path();
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Lock the folder before the key lands in it: protected DACL (no
    // inheritance from ProgramData, which lets Users read), full control
    // for SYSTEM and Administrators only. Well-known SIDs, so this works
    // on non-English Windows too.
    let status = std::process::Command::new("icacls")
        .arg(dir)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"])
        .stdout(std::process::Stdio::null())
        .status()
        .context("running icacls")?;
    if !status.success() {
        bail!("icacls could not restrict {} ({status})", dir.display());
    }
    std::fs::write(&path, format!("{hex}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
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
    let hex = KeyHex(&k).to_string();
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
