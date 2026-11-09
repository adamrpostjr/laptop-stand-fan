//! The background loop, shared by `fanstand run` and the Windows service.
//!
//! Every `interval` (default 2 s) it reads the CPU temperature and writes it
//! to the stand (write without response), then reads RPM, status and duty
//! back into the shared [`Snapshot`] that the desktop app displays. About
//! every 30 s it also logs a status line.
//!
//! Any error (out of range, Bluetooth toggled off, stand rebooted) ends the
//! session. We then reconnect with backoff (2 → 4 → 8 → 16 → 30 s), forever.
//! While we're away the stand runs its own failsafe (safe speed after its
//! timeout), so missing updates is safe.
//!
//! Sleep/wake: on [`Power::Suspend`] we let go of the stand and wait; on
//! [`Power::Resume`] we reconnect immediately instead of waiting out a
//! backoff that started before the machine slept.

use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use fanstand_ipc::{CpuTemp, Link, Request, Response, Snapshot, StandState};
use fanstand_protocol::{Mode, Permille, Status};
use log::{info, warn};
use tokio::sync::{mpsc, oneshot};

use crate::stand::Stand;
use crate::temp::{self, Source};

const OP_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_EVERY: u32 = 15; // × interval ≈ 30 s
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// If we never hear "resume" after "suspend", carry on anyway after this.
const SUSPEND_MAX: Duration = Duration::from_secs(15 * 60);

pub struct Options {
    pub device: Option<String>,
    pub scan_timeout: Duration,
    pub interval: Duration,
    pub force_estimate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Suspend,
    Resume,
}

pub type Shared = Arc<Mutex<Snapshot>>;
pub type Reply = oneshot::Sender<Response>;

/// Everything that can poke the loop from outside.
pub struct Inputs {
    /// Commands from the desktop app (snapshots are answered from [`Shared`]
    /// directly and never reach the loop).
    pub requests: mpsc::Receiver<(Request, Reply)>,
    pub power: mpsc::UnboundedReceiver<Power>,
}

/// Senders matching [`Inputs`]. Keep them alive as long as the loop runs.
pub struct InputSenders {
    pub requests: mpsc::Sender<(Request, Reply)>,
    pub power: mpsc::UnboundedSender<Power>,
}

pub fn inputs() -> (InputSenders, Inputs) {
    let (rq_tx, rq_rx) = mpsc::channel(8);
    let (pw_tx, pw_rx) = mpsc::unbounded_channel();
    (
        InputSenders {
            requests: rq_tx,
            power: pw_tx,
        },
        Inputs {
            requests: rq_rx,
            power: pw_rx,
        },
    )
}

pub fn new_shared() -> Shared {
    Arc::new(Mutex::new(Snapshot {
        service_version: env!("CARGO_PKG_VERSION").into(),
        ..Default::default()
    }))
}

/// Poison-tolerant: a panic elsewhere must not take the fan loop down.
pub fn lock(s: &Shared) -> MutexGuard<'_, Snapshot> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

fn set_link(shared: &Shared, link: Link, err: Option<String>) {
    let mut s = lock(shared);
    s.link = link;
    if link != Link::Connected {
        s.stand = None;
    }
    if err.is_some() || link == Link::Connected {
        s.last_error = err;
    }
}

pub async fn run(
    opt: Options,
    shared: Shared,
    mut inputs: Inputs,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
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
    {
        let mut s = lock(&shared);
        s.temp_source = src.describe();
        s.cpu = first.ok().map(cpu_temp);
    }

    // One shutdown future for the whole run, so a signal that arrives while
    // we're sleeping in backoff isn't missed.
    tokio::pin!(shutdown);
    let mut backoff = BACKOFF_MIN;
    'outer: loop {
        set_link(&shared, Link::Connecting, None);
        let end = tokio::select! {
            _ = &mut shutdown => break,
            end = session(&opt, src.as_mut(), &shared, &mut inputs) => end,
        };
        let wait = match end {
            SessionEnd::LinkLost(e) => {
                info!("link lost ({e:#}); reconnecting");
                set_link(&shared, Link::Retrying, Some(format!("{e:#}")));
                backoff = BACKOFF_MIN;
                Duration::from_millis(500)
            }
            SessionEnd::ConnectFailed(e) => {
                warn!("{e:#}; retrying in {}s", backoff.as_secs());
                set_link(&shared, Link::Retrying, Some(format!("{e:#}")));
                let w = backoff;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                w
            }
            SessionEnd::Suspended => {
                info!("system suspending: released the stand (it runs its failsafe speed)");
                set_link(&shared, Link::Suspended, None);
                SUSPEND_MAX
            }
            SessionEnd::Reconnect => {
                info!("reconnect requested");
                backoff = BACKOFF_MIN;
                Duration::ZERO
            }
        };

        // Wait, but stay responsive to shutdown, power events and the app.
        let sleep = tokio::time::sleep(wait);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut shutdown => break 'outer,
                _ = &mut sleep => break,
                Some(p) = inputs.power.recv() => match p {
                    Power::Suspend => set_link(&shared, Link::Suspended, None),
                    Power::Resume => {
                        info!("system resumed: reconnecting now");
                        backoff = BACKOFF_MIN;
                        break;
                    }
                },
                Some((req, reply)) = inputs.requests.recv() => {
                    if req == Request::Reconnect {
                        backoff = BACKOFF_MIN;
                        let _ = reply.send(Response::Ok);
                        break;
                    }
                    let _ = reply.send(not_connected(&shared, req));
                }
            }
        }
    }
    info!("stopping");
    Ok(())
}

fn not_connected(shared: &Shared, req: Request) -> Response {
    match req {
        Request::Snapshot => Response::Snapshot(Box::new(lock(shared).clone())),
        _ => Response::Error {
            message: "the stand isn't connected right now".into(),
        },
    }
}

async fn timed<T>(what: &str, f: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(OP_TIMEOUT, f)
        .await
        .map_err(|_| anyhow!("{what}: timed out"))?
}

/// How a session ended, which decides the retry policy in [`run`].
enum SessionEnd {
    /// Worked, then dropped (range, stand reboot): reconnect right away.
    LinkLost(anyhow::Error),
    /// Never got going (no key, not found, auth, Bluetooth off): back off.
    ConnectFailed(anyhow::Error),
    /// The machine is going to sleep.
    Suspended,
    /// The app asked us to start over.
    Reconnect,
}

/// One connection, from scan to link loss. Never returns while healthy.
async fn session(
    opt: &Options,
    src: &mut dyn Source,
    shared: &Shared,
    inputs: &mut Inputs,
) -> SessionEnd {
    // Loaded per attempt, so installing the key doesn't need a restart.
    let key = match crate::key::load() {
        Ok(k) => k,
        Err(e) => return SessionEnd::ConnectFailed(e),
    };
    let connect = tokio::time::timeout(
        opt.scan_timeout + Duration::from_secs(40),
        Stand::connect(opt.device.as_deref(), Some(&key), opt.scan_timeout),
    );
    let s = tokio::select! {
        // Don't hold a half-made connection across sleep.
        Some(Power::Suspend) = inputs.power.recv() => return SessionEnd::Suspended,
        r = connect => match r {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return SessionEnd::ConnectFailed(e),
            Err(_) => return SessionEnd::ConnectFailed(anyhow!("connect: timed out")),
        },
    };
    let fw = timed("read version", s.firmware_version())
        .await
        .unwrap_or_default();
    info!(
        "connected to {} (firmware {fw}), sending temperature every {:?}",
        s.name, opt.interval
    );
    set_link(shared, Link::Connected, None);

    let mut tick = tokio::time::interval(opt.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut n: u32 = 0;
    let mut ended = None;
    let result: Result<Infallible> = async {
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                Some(p) = inputs.power.recv() => {
                    if p == Power::Suspend {
                        ended = Some(SessionEnd::Suspended);
                        bail!("suspend");
                    }
                    continue;
                }
                Some((req, reply)) = inputs.requests.recv() => {
                    if req == Request::Reconnect {
                        let _ = reply.send(Response::Ok);
                        ended = Some(SessionEnd::Reconnect);
                        bail!("reconnect");
                    }
                    let resp = match handle(&s, req, shared).await {
                        Ok(r) => r,
                        Err(e) => Response::Error { message: format!("{e:#}") },
                    };
                    let _ = reply.send(resp);
                    // Show the effect right away rather than on the next tick.
                    refresh(&s, &fw, shared).await?;
                    continue;
                }
            }
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
            lock(shared).cpu = reading.map(cpu_temp);
            let (st, duty, rpm) = refresh(&s, &fw, shared).await?;
            if n.is_multiple_of(STATUS_EVERY) {
                let cpu = reading.map_or("?".into(), |r| r.to_string());
                info!("cpu {cpu} | stand: duty {duty} rpm {}/{} | {st}", rpm[0], rpm[1]);
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
    match ended {
        Some(end) => end,
        None if n > 0 => SessionEnd::LinkLost(e),
        None => SessionEnd::ConnectFailed(e),
    }
}

async fn handle(s: &Stand, req: Request, shared: &Shared) -> Result<Response> {
    match req {
        Request::Snapshot => return Ok(Response::Snapshot(Box::new(lock(shared).clone()))),
        Request::SetAuto => {
            timed("set mode", s.set_mode(Mode::Auto)).await?;
            info!(target: crate::logging::EVENT, "fan control set to auto by the desktop app");
        }
        Request::SetManual { pct } => {
            if pct > 100 {
                bail!("speed must be 0-100 %");
            }
            timed("set speed", s.set_manual_pct(pct)).await?;
            timed("set mode", s.set_mode(Mode::Manual)).await?;
            info!(target: crate::logging::EVENT, "fan speed overridden to {pct}% by the desktop app");
        }
        Request::Reconnect => unreachable!("handled by the caller"),
    }
    Ok(Response::Ok)
}

/// Read the stand's state into the snapshot.
async fn refresh(s: &Stand, fw: &str, shared: &Shared) -> Result<(Status, Permille, [u16; 2])> {
    let rpm = timed("read rpm", s.rpm()).await?;
    let st = timed("read status", s.status()).await?;
    let d = timed("read debug", s.debug()).await?;
    let mode = timed("read mode", s.mode()).await?;
    let manual_pct = timed("read manual speed", s.manual_pct()).await?;
    let updated_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    lock(shared).stand = Some(StandState {
        name: s.name.clone(),
        firmware: fw.to_string(),
        rpm,
        duty_pct: d.duty_permille as f32 / 10.0,
        target_pct: d.target_permille as f32 / 10.0,
        mode: match mode {
            Mode::Auto => fanstand_ipc::Mode::Auto,
            Mode::Manual => fanstand_ipc::Mode::Manual,
        },
        manual_pct,
        flags: st.to_string(),
        stalled: [
            st.contains(Status::FAN1_STALLED),
            st.contains(Status::FAN2_STALLED),
        ],
        failsafe: st.contains(Status::FAILSAFE),
        calibrating: st.contains(Status::CALIBRATING),
        uptime_s: d.uptime_s,
        updated_ms,
    });
    Ok((st, Permille(d.duty_permille), rpm))
}

fn cpu_temp(r: temp::Reading) -> CpuTemp {
    CpuTemp {
        celsius: r.celsius,
        estimated: r.estimated,
    }
}
