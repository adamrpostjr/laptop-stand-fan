//! `fanstand run`: the background loop.
//!
//! Every `interval` (default 2 s) it reads the CPU temperature and writes it
//! to the stand (write without response). About every 30 s it reads RPM and
//! status back, which logs a status line and also detects a dead link.
//!
//! Any error (sleep/wake, out of range, Bluetooth toggled off, stand
//! rebooted) ends the session. We then reconnect with backoff
//! (2 → 4 → 8 → 16 → 30 s), forever. While we're away the stand runs its own
//! failsafe (safe speed after its timeout), so missing updates is safe.

use std::convert::Infallible;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use fanstand_protocol::Permille;
use fanstand_protocol::auth::Key;
use log::{info, warn};

use crate::stand::Stand;
use crate::temp::{self, Source};

const OP_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_EVERY: u32 = 15; // × interval ≈ 30 s
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub struct Options {
    pub device: Option<String>,
    pub scan_timeout: Duration,
    pub interval: Duration,
    pub force_estimate: bool,
}

pub async fn run(key: Key, opt: Options) -> Result<()> {
    let mut src = temp::detect(opt.force_estimate);
    let first = src.read();
    match &first {
        Ok(r) if r.estimated => warn!(
            "temperature source: {} (now {:.1} °C ESTIMATED)",
            src.describe(),
            r.celsius
        ),
        Ok(r) => info!(
            "temperature source: {} (now {:.1} °C)",
            src.describe(),
            r.celsius
        ),
        Err(e) => warn!(
            "temperature source: {} (first read failed: {e:#})",
            src.describe()
        ),
    }

    // One Ctrl+C future for the whole run. A fresh `ctrl_c()` per iteration
    // misses a signal that arrives while we're sleeping in backoff.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut backoff = Duration::from_secs(2);
    loop {
        let end = tokio::select! {
            _ = &mut ctrl_c => break,
            end = session(&key, &opt, src.as_mut()) => end,
        };
        let wait = match end {
            SessionEnd::LinkLost(e) => {
                info!("link lost ({e:#}); reconnecting");
                backoff = Duration::from_secs(2);
                Duration::from_millis(500)
            }
            SessionEnd::ConnectFailed(e) => {
                warn!("{e:#}; retrying in {}s", backoff.as_secs());
                let w = backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                w
            }
        };
        tokio::select! {
            _ = &mut ctrl_c => break,
            _ = tokio::time::sleep(wait) => {}
        }
    }
    info!("stopping");
    Ok(())
}

async fn timed<T>(what: &str, f: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(OP_TIMEOUT, f)
        .await
        .map_err(|_| anyhow!("{what}: timed out"))?
}

/// How a session ended, which decides the retry policy in [`run`].
enum SessionEnd {
    /// Worked, then dropped (sleep, range, stand reboot): reconnect right away.
    LinkLost(anyhow::Error),
    /// Never got going (not found, auth, Bluetooth off): back off.
    ConnectFailed(anyhow::Error),
}

/// One connection, from scan to link loss. Never returns while healthy.
async fn session(key: &Key, opt: &Options, src: &mut dyn Source) -> SessionEnd {
    let connect = tokio::time::timeout(
        opt.scan_timeout + Duration::from_secs(40),
        Stand::connect(opt.device.as_deref(), Some(key), opt.scan_timeout),
    );
    let s = match connect.await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return SessionEnd::ConnectFailed(e),
        Err(_) => return SessionEnd::ConnectFailed(anyhow!("connect: timed out")),
    };
    let fw = timed("read version", s.firmware_version())
        .await
        .unwrap_or_default();
    info!(
        "connected to {} (firmware {fw}), sending temperature every {:?}",
        s.name, opt.interval
    );

    let mut tick = tokio::time::interval(opt.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut n: u32 = 0;
    let result: Result<Infallible> = async {
        loop {
            tick.tick().await;
            if !timed("link check", s.is_connected()).await? {
                bail!("disconnected");
            }
            let reading = match src.read() {
                Ok(r) => {
                    timed("send temperature", s.send_temperature(r.deci_celsius())).await?;
                    Some(r)
                }
                Err(e) => {
                    warn!("temperature read failed: {e:#} (stand falls back to its safe speed if this persists)");
                    None
                }
            };
            if n.is_multiple_of(STATUS_EVERY) {
                let rpm = timed("read rpm", s.rpm()).await?;
                let st = timed("read status", s.status()).await?;
                let d = timed("read debug", s.debug()).await?;
                let cpu = reading.map_or("?".into(), |r| r.to_string());
                info!(
                    "cpu {cpu} | stand: duty {} rpm {}/{} | {st}",
                    Permille(d.duty_permille),
                    rpm[0],
                    rpm[1]
                );
                if st.any_stalled() {
                    warn!("stand reports a STALLED fan ({st}); it is running both fans at 100%");
                }
            }
            n = n.wrapping_add(1);
        }
    }
    .await;
    s.disconnect().await;
    let Err(e) = result;
    if n > 0 {
        SessionEnd::LinkLost(e)
    } else {
        SessionEnd::ConnectFailed(e)
    }
}
