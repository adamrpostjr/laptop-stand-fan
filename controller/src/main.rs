//! `fanstand`: desktop controller for the BLE cooling laptop stand.

mod key;
mod run;
mod stand;
mod temp;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fanstand_protocol::calibration::{STEPS, duty_at_step};
use fanstand_protocol::codec::decode_rpm;
use fanstand_protocol::curve::CurvePoint;
use fanstand_protocol::diag::{DebugInfo, reset_reason_name};
use fanstand_protocol::{CalResult, Command, Config, FanCurve, Mode, Status, uuids};
use futures::StreamExt;
use uuid::Uuid;

use stand::Stand;

#[derive(Parser)]
#[command(
    name = "fanstand",
    version,
    about = "Control the BLE cooling laptop stand"
)]
struct Cli {
    /// Stand to use (name or ID from `fanstand scan`); default: first found
    #[arg(long, global = true)]
    device: Option<String>,
    /// Scan timeout in seconds
    #[arg(long, global = true, default_value_t = 10)]
    timeout: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the shared secret (then build the firmware with it)
    Keygen {
        #[arg(long)]
        force: bool,
    },
    /// List stands in range
    Scan,
    /// Show RPM, mode, flags, settings and diagnostics
    Status,
    /// Live RPM/status (notifications) until Ctrl+C
    Watch,
    /// auto | manual
    SetMode { mode: ModeArg },
    /// Fixed speed in percent (also switches to manual mode)
    SetSpeed {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
    },
    /// Fan curve as TEMP:DUTY points, e.g. `40:20 55:40 70:70 85:100`
    SetCurve {
        #[arg(required = true, num_args = 1..=8)]
        points: Vec<String>,
    },
    /// Safety settings (only the given ones change)
    SetConfig {
        /// Failsafe speed % (30-100)
        #[arg(long)]
        safe: Option<u8>,
        /// Minimum duty % (0-60)
        #[arg(long)]
        min: Option<u8>,
        /// Failsafe timeout s (5-600)
        #[arg(long)]
        timeout_s: Option<u16>,
    },
    /// Run minimum-duty calibration (~110 s) and show the result
    Calibrate {
        /// Abort a running calibration instead
        #[arg(long)]
        abort: bool,
    },
    /// Send one temperature reading in °C (testing; the stand treats it as fresh for the failsafe timeout)
    SendTemp { celsius: f32 },
    /// Background loop: send CPU temperature every few seconds, reconnect forever
    Run {
        /// Seconds between temperature updates
        #[arg(long, default_value_t = 2)]
        interval: u64,
        /// Use the CPU-load estimate even if a sensor exists (testing)
        #[arg(long)]
        estimate: bool,
    },
    /// Show which CPU temperature source is used and a few readings (no stand needed)
    Temp {
        #[arg(long)]
        estimate: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Auto,
    Manual,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("fanstand=info"))
        .format_target(false)
        .init();
    let timeout = Duration::from_secs(cli.timeout);
    let dev = cli.device.as_deref();

    match cli.cmd {
        Cmd::Keygen { force } => {
            let (path, hex) = key::generate(force)?;
            println!("Key written to {}", path.display());
            println!("Build + flash the firmware with it (from firmware/):");
            println!("  FANSTAND_KEY={hex} cargo run --release");
            println!(
                "Copy that file to your other computers' config dir to let them control the stand."
            );
        }
        Cmd::Run { interval, estimate } => {
            let k = key::load()?;
            run::run(
                k,
                run::Options {
                    device: cli.device.clone(),
                    scan_timeout: timeout,
                    interval: Duration::from_secs(interval.max(1)),
                    force_estimate: estimate,
                },
            )
            .await?;
        }
        Cmd::Temp { estimate } => {
            let mut src = temp::detect(estimate);
            println!("source: {}", src.describe());
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                match src.read() {
                    Ok(r) => println!(
                        "  {:.1} °C{}",
                        r.celsius,
                        if r.estimated { "  (ESTIMATED)" } else { "" }
                    ),
                    Err(e) => println!("  read failed: {e:#}"),
                }
            }
        }
        Cmd::Scan => {
            let adapter = stand::adapter().await?;
            let found = stand::scan(&adapter, timeout, true).await?;
            if found.is_empty() {
                println!("no stands found");
            }
            for f in found {
                println!("{:<10} {:<40} rssi {:?}", f.name, f.id, f.rssi);
            }
        }
        Cmd::Status => {
            let s = Stand::connect(dev, None, timeout).await?;
            let r = print_status(&s).await;
            s.disconnect().await;
            r?;
        }
        Cmd::Watch => {
            let s = Stand::connect(dev, None, timeout).await?;
            let r = watch(&s).await;
            s.disconnect().await;
            r?;
        }
        cmd => {
            // everything below writes → authenticate
            let k = key::load()?;
            let s = Stand::connect(dev, Some(&k), timeout).await?;
            let r = write_cmd(&s, cmd).await;
            s.disconnect().await;
            r?;
        }
    }
    Ok(())
}

async fn write_cmd(s: &Stand, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::SetMode { mode } => {
            s.set_mode(match mode {
                ModeArg::Auto => Mode::Auto,
                ModeArg::Manual => Mode::Manual,
            })
            .await?;
            println!("ok");
        }
        Cmd::SetSpeed { percent } => {
            s.set_manual_pct(percent).await?;
            s.set_mode(Mode::Manual).await?;
            println!("manual {percent}%");
        }
        Cmd::SetCurve { points } => {
            let curve = parse_curve(&points)?;
            s.set_curve(&curve).await?;
            println!("curve set: {}", fmt_curve(&curve));
        }
        Cmd::SetConfig {
            safe,
            min,
            timeout_s,
        } => {
            let mut c = s.config().await?;
            if let Some(v) = safe {
                c.safe_speed_pct = v;
            }
            if let Some(v) = min {
                c.min_duty_pct = v;
            }
            if let Some(v) = timeout_s {
                c.failsafe_timeout_s = v;
            }
            let c = c
                .validate()
                .context("safe 30-100, min 0-60, timeout 5-600")?;
            s.set_config(&c).await?;
            println!("{}", fmt_config(&c));
        }
        Cmd::Calibrate { abort } => {
            if abort {
                s.command(Command::AbortCalibration).await?;
                println!("abort sent");
            } else {
                calibrate(s).await?;
            }
        }
        Cmd::SendTemp { celsius } => {
            let dc = (celsius * 10.0).round() as i16;
            s.send_temperature(dc).await?;
            // write-without-response gives no feedback; read back via DEBUG
            tokio::time::sleep(Duration::from_millis(400)).await;
            let d = s.debug().await?;
            println!("sent {celsius:.1} °C; stand sees {}", fmt_temp(&d));
        }
        _ => unreachable!(),
    }
    Ok(())
}

async fn print_status(s: &Stand) -> Result<()> {
    let (fw, mode, manual, rpm, st) = (
        s.firmware_version().await?,
        s.mode().await?,
        s.manual_pct().await?,
        s.rpm().await?,
        s.status().await?,
    );
    let (curve, cfg, cal, d) = (
        s.curve().await?,
        s.config().await?,
        s.cal_result().await?,
        s.debug().await?,
    );
    println!("stand      {} (firmware {fw})", s.name);
    println!(
        "rpm        fan1 {:>4}  fan2 {:>4}   (raw {} / {})",
        rpm[0], rpm[1], d.raw_rpm[0], d.raw_rpm[1]
    );
    println!(
        "duty       {:.1}%  (target {:.1}%){}",
        d.duty_permille as f32 / 10.0,
        d.target_permille as f32 / 10.0,
        if d.flags & DebugInfo::FLAG_GRACE != 0 {
            "  [stall grace]"
        } else {
            ""
        }
    );
    match mode {
        Mode::Auto => println!("mode       auto"),
        Mode::Manual => println!("mode       manual {manual}%"),
    }
    println!("flags      {}", fmt_status(st));
    println!("temp       {}", fmt_temp(&d));
    println!(
        "tach line  fan1 {}  fan2 {}",
        fmt_probe(d.tach_high_samples[0], rpm[0]),
        fmt_probe(d.tach_high_samples[1], rpm[1])
    );
    println!("curve      {}", fmt_curve(&curve));
    println!("config     {}", fmt_config(&cfg));
    println!("calibrate  {}", fmt_cal(cal));
    println!(
        "uptime     {}  (last reset: {})  free heap {} KB{}",
        fmt_uptime(d.uptime_s),
        reset_reason_name(d.reset_reason),
        d.free_heap / 1024,
        if d.flags & DebugInfo::FLAG_NO_AUTH != 0 {
            "  ** NO-AUTH TEST BUILD **"
        } else {
            ""
        }
    );
    Ok(())
}

async fn watch(s: &Stand) -> Result<()> {
    s.subscribe(uuids::FAN_RPM).await?;
    s.subscribe(uuids::STATUS).await?;
    let mut n = s.notifications().await?;
    let (rpm_u, st_u) = (
        Uuid::from_u128(uuids::FAN_RPM),
        Uuid::from_u128(uuids::STATUS),
    );
    println!("watching {} (Ctrl+C to stop)", s.name);
    let mut status = s.status().await?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            v = n.next() => {
                let Some(v) = v else { bail!("disconnected") };
                if v.uuid == st_u {
                    status = Status(v.value.first().copied().unwrap_or(0));
                    println!("status → {}", fmt_status(status));
                } else if v.uuid == rpm_u {
                    let r = decode_rpm(&v.value)?;
                    let d = s.debug().await?;
                    println!("rpm {:>4} / {:>4}  duty {:5.1}%  temp {}  [{}]",
                        r[0], r[1], d.duty_permille as f32 / 10.0, fmt_temp(&d), fmt_status(status));
                }
            }
        }
    }
    Ok(())
}

async fn calibrate(s: &Stand) -> Result<()> {
    s.subscribe(uuids::CAL_RESULT).await?;
    let mut n = s.notifications().await?;
    s.command(Command::StartCalibration).await?;
    println!("calibrating: fans go to 100%, then step down 5% every 5 s (~110 s). Ctrl+C aborts.");
    let cal_u = Uuid::from_u128(uuids::CAL_RESULT);
    let mut last_progress = None;
    let result = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                s.command(Command::AbortCalibration).await?;
                bail!("aborted");
            }
            // poll as a fallback in case a notification is missed
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                let r = s.cal_result().await?;
                if !matches!(r, CalResult::Running { .. }) { break r; }
            }
            v = n.next() => {
                let Some(v) = v else { bail!("disconnected during calibration (it continues on the stand; check `fanstand status`)") };
                if v.uuid != cal_u { continue; }
                match CalResult::decode(&v.value)? {
                    CalResult::Running { progress_pct } => {
                        if last_progress != Some(progress_pct / 10) {
                            last_progress = Some(progress_pct / 10);
                            println!("  {progress_pct:>3}%");
                        }
                    }
                    other => break other,
                }
            }
        }
    };
    let table = s.cal_table().await?;
    println!(" duty | fan1 rpm | fan2 rpm");
    for (i, row) in table.iter().enumerate().take(STEPS) {
        println!("{:4}% | {:8} | {:8}", duty_at_step(i), row[0], row[1]);
    }
    println!("result: {}", fmt_cal(result));
    if let CalResult::Failed(_) = result {
        bail!("calibration failed");
    }
    Ok(())
}

// ---------------------------------------------------------------- formatting

fn parse_curve(points: &[String]) -> Result<FanCurve> {
    let pts = points
        .iter()
        .map(|p| {
            let (t, d) = p
                .split_once(':')
                .context("points look like 40:20 (°C:duty%)")?;
            Ok(CurvePoint::new(t.trim().parse()?, d.trim().parse()?))
        })
        .collect::<Result<Vec<_>>>()?;
    FanCurve::new(&pts).map_err(|e| anyhow::anyhow!("invalid curve: {e}"))
}

fn fmt_curve(c: &FanCurve) -> String {
    c.points()
        .iter()
        .map(|p| format!("{}°C→{}%", p.temp_c, p.duty_pct))
        .collect::<Vec<_>>()
        .join("  ")
}

fn fmt_config(c: &Config) -> String {
    format!(
        "safe speed {}%, min duty {}%, failsafe after {} s",
        c.safe_speed_pct, c.min_duty_pct, c.failsafe_timeout_s
    )
}

fn fmt_status(s: Status) -> String {
    if s.0 == 0 { "ok".into() } else { s.to_string() }
}

fn fmt_temp(d: &DebugInfo) -> String {
    match (d.temp_dc, d.temp_age_s) {
        (Some(t), Some(a)) => format!("{:.1} °C ({a} s ago)", t as f32 / 10.0),
        _ => "none received".into(),
    }
}

fn fmt_cal(c: CalResult) -> String {
    match c {
        CalResult::None => "not run since boot".into(),
        CalResult::Running { progress_pct } => format!("running ({progress_pct}%)"),
        CalResult::Ok { min_duty_pct } => format!("ok, min duty {min_duty_pct}% (saved)"),
        CalResult::Failed(code) => format!("FAILED: {}", code.describe()),
    }
}

/// Interpret the raw tach line probe (high samples out of ~10 per second).
fn fmt_probe(high: u8, rpm: u16) -> String {
    let what = match (high, rpm) {
        (_, r) if r > 0 => "pulsing",
        (0, _) => "STUCK LOW: held at GND (short, or wrong pin?)",
        (h, _) if h >= 10 => "STUCK HIGH: no signal reaching the pin (tach wire not connected?)",
        _ => "toggling but no pulses counted (firmware/PCNT problem)",
    };
    format!("{high:>2}/10 high ({what})")
}

fn fmt_uptime(s: u32) -> String {
    format!("{}h{:02}m{:02}s", s / 3600, s / 60 % 60, s % 60)
}
