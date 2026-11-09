//! FanStand desktop app: a tray icon plus a small window showing both fans'
//! RPM, the CPU temperature and the duty, with a manual override.
//!
//! It never talks Bluetooth itself. Everything goes through the FanStand
//! service over `\\.\pipe\fanstand` (see the `fanstand-ipc` crate), so it
//! needs neither admin rights nor the stand's key.
//!
//! - Closing the window hides it to the tray; "Quit" in the tray menu exits.
//! - `--minimized` starts in the tray (used by the autostart entry).
//! - Starting it again while it runs just brings the window forward.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod icon;
#[cfg(windows)]
mod single;

#[cfg(windows)]
fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    // Build helper for the installer: `fanstand-app --write-icon <path.ico>`.
    if let Some(i) = args.iter().position(|a| a == "--write-icon") {
        let path = args.get(i + 1).expect("--write-icon <path>");
        std::fs::write(path, icon::ico(icon::Tone::Ok)).expect("writing icon");
        return Ok(());
    }
    let minimized = args.iter().any(|a| a == "--minimized");
    let Some(show_signal) = single::claim() else {
        return Ok(()); // the running copy was told to show itself
    };
    app::run(minimized, show_signal)
}

#[cfg(not(windows))]
fn main() {
    eprintln!("fanstand-app is Windows-only; use the `fanstand` CLI on this OS");
    std::process::exit(1);
}
