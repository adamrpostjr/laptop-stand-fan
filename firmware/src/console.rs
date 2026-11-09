//! Serial debug console over USB (`espflash monitor`). Type a command and press Enter.
//!
//! It runs in its own thread and only sends parsed commands to the control
//! loop over a channel, so a stuck console can never block fan control.
//! Anyone with the USB cable is trusted, so the console needs no auth.

use std::io::Read;
use std::sync::mpsc::Sender;
use std::time::Duration;

use fanstand_protocol::calibration::TOTAL_MS;
use fanstand_protocol::codec::{ConfigPatch, ConfigRanges};
use fanstand_protocol::{Command, FanCurve, Mode};

use crate::control::Request;

pub fn print_help() {
    println!(
        "commands:
  status                 settings + last calibration table
  auto | manual <0-100>  switch mode (manual also sets the speed)
  temp <C>               inject a CPU temperature (Auto mode test), e.g. temp 55.5
  curve <t:d> ...        e.g. curve 40:20 55:40 70:70 85:100
  safe | min | timeout   config, e.g. `safe 70` ({ConfigRanges})
  cal | abort            run/abort minimum-duty calibration (~{} s)
  defaults               reset all settings
  log                    toggle the 1 s status log",
        TOTAL_MS / 1000
    );
}

/// One console line → the requests it stands for.
pub fn parse(line: &str) -> Result<Vec<Request>, String> {
    let mut it = line.split_whitespace();
    let cmd = it.next().unwrap_or("");
    let arg = it.next();
    let num = |a: Option<&str>| -> Result<u16, String> {
        a.ok_or("missing number")?
            .parse::<u16>()
            .map_err(|e| e.to_string())
    };
    let pct = |a| -> Result<u8, String> { u8::try_from(num(a)?).map_err(|e| e.to_string()) };
    let patch = |p: ConfigPatch| vec![Request::PatchConfig(p)];
    Ok(match cmd {
        "help" | "?" => vec![Request::PrintHelp],
        "status" => vec![Request::PrintStatus],
        "auto" => vec![Request::SetMode(Mode::Auto)],
        "manual" => vec![
            Request::SetManual(pct(arg)?),
            Request::SetMode(Mode::Manual),
        ],
        "temp" => {
            let c: f32 = arg.ok_or("missing °C")?.parse().map_err(|_| "bad number")?;
            vec![Request::Temperature((c * 10.0).round() as i16)]
        }
        "curve" => vec![Request::SetCurve(
            FanCurve::parse(arg.into_iter().chain(it)).map_err(|e| e.to_string())?,
        )],
        "safe" => patch(ConfigPatch {
            safe_speed_pct: Some(pct(arg)?),
            ..Default::default()
        }),
        "min" => patch(ConfigPatch {
            min_duty_pct: Some(pct(arg)?),
            ..Default::default()
        }),
        "timeout" => patch(ConfigPatch {
            failsafe_timeout_s: Some(num(arg)?),
            ..Default::default()
        }),
        "cal" | "calibrate" => vec![Request::Command(Command::StartCalibration)],
        "abort" => vec![Request::Command(Command::AbortCalibration)],
        "defaults" => vec![Request::ResetDefaults],
        "log" => vec![Request::ToggleLog],
        "" => vec![],
        other => return Err(format!("unknown command '{other}' (try 'help')")),
    })
}

/// Spawn the console reader thread.
pub fn spawn(tx: Sender<Request>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("console".into())
        .stack_size(6 * 1024)
        .spawn(move || run(tx))
        .map(|_| ())
}

fn run(tx: Sender<Request>) {
    let mut line = String::new();
    let mut buf = [0u8; 32];
    let mut stdin = std::io::stdin();
    loop {
        // The UART VFS stdin doesn't block (no UART driver installed), so poll it.
        let n = match stdin.read(&mut buf) {
            Ok(n) if n > 0 => n,
            _ => {
                std::thread::sleep(Duration::from_millis(30));
                continue;
            }
        };
        for &b in &buf[..n] {
            match b {
                b'\r' | b'\n' => {
                    if !line.trim().is_empty() {
                        println!();
                        match parse(line.trim()) {
                            Ok(reqs) => {
                                for r in reqs {
                                    if tx.send(r).is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => println!("error: {e}"),
                        }
                    }
                    line.clear();
                }
                0x08 | 0x7f => {
                    if line.pop().is_some() {
                        print!("\x08 \x08");
                    }
                }
                b if (b.is_ascii_graphic() || b == b' ') && line.len() < 128 => {
                    line.push(b as char);
                    print!("{}", b as char); // espflash monitor doesn't echo locally
                }
                _ => {}
            }
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}
