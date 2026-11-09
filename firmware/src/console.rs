//! Serial debug console over USB (`espflash monitor`). Type a command and press Enter.
//!
//! It runs in its own thread and only sends parsed commands to the control
//! loop over a channel, so a stuck console can never block fan control.
//! Anyone with the USB cable is trusted, so the console needs no auth.

use std::io::Read;
use std::sync::mpsc::Sender;
use std::time::Duration;

use fanstand_protocol::Mode;
use fanstand_protocol::curve::{CurvePoint, FanCurve, MAX_POINTS};

use crate::control::Request;

pub const HELP: &str = "\
commands:
  status                 settings + last calibration table
  auto | manual <0-100>  mode
  temp <C>               inject a CPU temperature (Auto mode test), e.g. temp 55.5
  curve <t:d> ...        e.g. curve 40:20 55:40 70:70 85:100
  safe <30-100>          failsafe speed %
  min <0-60>             minimum duty %
  timeout <5-600>        failsafe timeout s
  cal | abort            run/abort minimum-duty calibration (~110 s)
  defaults               reset all settings
  log                    toggle the 1 s status log";

pub fn parse(line: &str) -> Result<Request, String> {
    let mut it = line.split_whitespace();
    let cmd = it.next().unwrap_or("");
    let arg = it.next();
    let num = |a: Option<&str>| -> Result<u16, String> {
        a.ok_or("missing number")?
            .parse::<u16>()
            .map_err(|e| e.to_string())
    };
    let pct = |a| -> Result<u8, String> { u8::try_from(num(a)?).map_err(|e| e.to_string()) };
    Ok(match cmd {
        "help" | "?" => Request::PrintHelp,
        "status" => Request::PrintStatus,
        "auto" => Request::SetMode(Mode::Auto),
        "manual" => Request::SetManual(pct(arg)?),
        "temp" => {
            let c: f32 = arg.ok_or("missing °C")?.parse().map_err(|_| "bad number")?;
            Request::Temperature((c * 10.0).round() as i16)
        }
        "curve" => {
            let mut pts = [CurvePoint::default(); MAX_POINTS];
            let mut n = 0;
            for tok in arg.into_iter().chain(it) {
                let (t, d) = tok.split_once(':').ok_or("points look like 40:20")?;
                let p = pts.get_mut(n).ok_or("max 8 points")?;
                *p = CurvePoint::new(
                    t.parse().map_err(|_| "bad temp")?,
                    d.parse().map_err(|_| "bad duty")?,
                );
                n += 1;
            }
            Request::SetCurve(FanCurve::new(&pts[..n]).map_err(|e| e.to_string())?)
        }
        "safe" => Request::SetSafeSpeed(pct(arg)?),
        "min" => Request::SetMinDuty(pct(arg)?),
        "timeout" => Request::SetFailsafeTimeout(num(arg)?),
        "cal" | "calibrate" => Request::StartCalibration,
        "abort" => Request::AbortCalibration,
        "defaults" => Request::ResetDefaults,
        "log" => Request::ToggleLog,
        "" => return Err(String::new()),
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
                            Ok(cmd) => {
                                if tx.send(cmd).is_err() {
                                    return;
                                }
                            }
                            Err(e) if !e.is_empty() => println!("error: {e}"),
                            Err(_) => {}
                        }
                    }
                    line.clear();
                }
                0x08 | 0x7f => {
                    if line.pop().is_some() {
                        print!("\x08 \x08");
                    }
                }
                b if b.is_ascii_graphic() || b == b' ' => {
                    if line.len() < 128 {
                        line.push(b as char);
                        print!("{}", b as char); // espflash monitor doesn't echo locally
                    }
                }
                _ => {}
            }
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}
