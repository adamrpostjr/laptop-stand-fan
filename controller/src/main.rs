//! `fanstand`: desktop controller for the BLE cooling laptop stand.

mod key;
mod logging;
#[cfg(windows)]
mod pipe;
mod run;
#[cfg(windows)]
mod service;
mod stand;
mod temp;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use fanstand_protocol::calibration::{CalTable, STEP_MS, STEP_PCT, TOTAL_MS};
use fanstand_protocol::codec::{ConfigPatch, ConfigRanges, decode_rpm};
use fanstand_protocol::curve::MAX_POINTS;
use fanstand_protocol::diag::{DebugInfo, reset_reason_name};
use fanstand_protocol::{CalResult, Command, DeciCelsius, FanCurve, Mode, Permille, Status, uuids};
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
        #[arg(required = true, num_args = 1..=MAX_POINTS)]
        points: Vec<String>,
    },
    /// Safety settings; only the given ones change
    #[command(after_help = ConfigRanges.to_string())]
    SetConfig {
        /// Failsafe speed %
        #[arg(long)]
        safe: Option<u8>,
        /// Minimum duty %
        #[arg(long)]
        min: Option<u8>,
        /// Failsafe timeout in seconds
        #[arg(long)]
        timeout_s: Option<u16>,
    },
    /// Run minimum-duty calibration (about two minutes) and show the result
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
        /// Also serve the FanStand app over the local pipe (like the service does)
        #[cfg(windows)]
        #[arg(long)]
        serve: bool,
    },
    /// Install / control the Windows service (install, uninstall, start, stop, status)
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Show which CPU temperature source is used and a few readings (no stand needed)
    Temp {
        #[arg(long)]
        estimate: bool,
    },
}

#[cfg(windows)]
#[derive(Subcommand)]
enum ServiceCmd {
    /// Copy fanstand to Program Files, install the key machine-wide and start the service (elevated)
    Install {
        /// Key file to install (default: this user's key)
        #[arg(long)]
        key: Option<std::path::PathBuf>,
    },
    /// Stop and remove the service (elevated)
    Uninstall {
        /// Also delete the machine key, logs and the installed binaries
        #[arg(long)]
        purge: bool,
    },
    /// Install or replace the machine-wide key the service uses (elevated)
    SetKey { file: std::path::PathBuf },
    /// Start the service (elevated)
    Start,
    /// Stop the service; the stand falls back to its safe speed (elevated)
    Stop,
    /// Service state plus what it currently sees
    Status,
    /// Started by Windows; not for interactive use
    #[command(hide = true)]
    Run,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Auto,
    Manual,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The service dispatcher takes over this thread and sets up its own
    // logging and runtime, so it must not run inside ours.
    #[cfg(windows)]
    if let Cmd::Service { cmd } = &cli.cmd {
        return match cmd {
            ServiceCmd::Run => service::dispatch(),
            ServiceCmd::Install { key } => service::install(key.as_deref()),
            ServiceCmd::Uninstall { purge } => service::uninstall(*purge),
            ServiceCmd::SetKey { file } => {
                let hex = key::read_key_file(file)?;
                let path = key::install_machine_key(&hex).map_err(|e| {
                    e.context("run this from an elevated terminal (e.g. `sudo fanstand service set-key ...`)")
                })?;
                println!("key installed at {} (the service picks it up on its next connect)", path.display());
                Ok(())
            }
            ServiceCmd::Start => service::start(),
            ServiceCmd::Stop => service::stop(),
            ServiceCmd::Status => service::status(),
        };
    }
    logging::init_console();
    tokio::runtime::Runtime::new()?.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
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
        Cmd::Run {
            interval,
            estimate,
            #[cfg(windows)]
            serve,
        } => {
            key::load()?; // fail fast; the loop reloads it on every connect
            let shared = run::new_shared();
            let (senders, inputs) = run::inputs();
            #[cfg(windows)]
            if serve {
                let (sh, tx) = (shared.clone(), senders.requests.clone());
                tokio::spawn(async move {
                    if let Err(e) = pipe::serve(sh, tx).await {
                        log::error!("app pipe: {e:#}");
                    }
                });
            }
            run::run(
                run::Options {
                    device: cli.device.clone(),
                    scan_timeout: timeout,
                    interval: Duration::from_secs(interval.max(1)),
                    force_estimate: estimate,
                },
                shared,
                inputs,
                async {
                    let _ = tokio::signal::ctrl_c().await;
                },
            )
            .await?;
            drop(senders);
        }
        #[cfg(windows)]
        Cmd::Service { .. } => unreachable!("handled in main"),
        Cmd::Temp { estimate } => {
            let mut src = temp::detect(estimate);
            println!("source: {}", src.describe());
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                match src.read() {
                    Ok(r) => println!("  {r}"),
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
            let curve = FanCurve::parse(points.iter().map(String::as_str))
                .map_err(|e| anyhow::anyhow!("invalid curve: {e}"))?;
            s.set_curve(&curve).await?;
            println!("curve set: {curve}");
        }
        Cmd::SetConfig {
            safe,
            min,
            timeout_s,
        } => {
            let patch = ConfigPatch {
                safe_speed_pct: safe,
                min_duty_pct: min,
                failsafe_timeout_s: timeout_s,
            };
            let c = s
                .config()
                .await?
                .apply(patch)
                .with_context(|| format!("valid: {ConfigRanges}"))?;
            s.set_config(&c).await?;
            println!("{c}");
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
            s.send_temperature(temp::to_deci_celsius(celsius)).await?;
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
        "duty       {}  (target {}){}",
        Permille(d.duty_permille),
        Permille(d.target_permille),
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
    println!("flags      {st}");
    println!("temp       {}", fmt_temp(&d));
    println!(
        "tach line  fan1 {}  fan2 {}",
        fmt_probe(d.tach_high_samples[0], rpm[0]),
        fmt_probe(d.tach_high_samples[1], rpm[1])
    );
    println!("curve      {curve}");
    println!("config     {cfg}");
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
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            _ = &mut ctrl_c => break,
            v = n.next() => {
                let Some(v) = v else { bail!("disconnected") };
                if v.uuid == st_u {
                    status = Status::decode(&v.value)?;
                    println!("status → {status}");
                } else if v.uuid == rpm_u {
                    let r = decode_rpm(&v.value)?;
                    let d = s.debug().await?;
                    println!("rpm {:>4} / {:>4}  duty {:>6}  temp {}  [{status}]",
                        r[0], r[1], Permille(d.duty_permille).to_string(), fmt_temp(&d));
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
    println!(
        "calibrating: fans go to 100%, then {STEP_PCT}% lower every {} s (~{} s). Ctrl+C aborts.",
        STEP_MS / 1000,
        TOTAL_MS / 1000
    );
    let cal_u = Uuid::from_u128(uuids::CAL_RESULT);
    let mut last_progress = None;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let result = loop {
        tokio::select! {
            _ = &mut ctrl_c => {
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
    print!("{}", CalTable(&s.cal_table().await?));
    println!("result: {}", fmt_cal(result));
    if let CalResult::Failed(_) = result {
        bail!("calibration failed");
    }
    Ok(())
}

// ---------------------------------------------------------------- formatting

fn fmt_temp(d: &DebugInfo) -> String {
    match (d.temp_dc, d.temp_age_s) {
        (Some(t), Some(a)) => format!("{} ({a} s ago)", DeciCelsius(t)),
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
