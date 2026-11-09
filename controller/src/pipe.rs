//! Named-pipe server for the desktop app (`\\.\pipe\fanstand`).
//!
//! Snapshots are answered straight from the shared state; commands are
//! forwarded to the BLE loop, which owns the connection.
//!
//! Security: the pipe's DACL allows SYSTEM and Administrators full access and
//! *interactively logged-on* users read/write. Network logons (and so remote
//! pipe clients) get nothing. The first instance is created with
//! FILE_FLAG_FIRST_PIPE_INSTANCE so nobody can squat the name before us.

use std::ffi::c_void;

use anyhow::{Context, Result};
use fanstand_ipc::{MAX_LINE, Request, Response};
use log::{debug, warn};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{mpsc, oneshot};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::core::HSTRING;

use crate::run::{Reply, Shared, lock};

/// SY, BA: full; IU (interactive users): read + write.
const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

struct Security {
    attrs: SECURITY_ATTRIBUTES,
}
// The descriptor is leaked (lives for the process) and only read by the OS.
unsafe impl Send for Security {}
unsafe impl Sync for Security {}

fn security() -> Result<Security> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(SDDL),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .context("building the pipe's security descriptor")?;
    }
    Ok(Security {
        attrs: SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        },
    })
}

fn create(sec: &Security, first: bool) -> std::io::Result<NamedPipeServer> {
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first).reject_remote_clients(true);
    unsafe {
        opts.create_with_security_attributes_raw(
            fanstand_ipc::PIPE_NAME,
            &sec.attrs as *const SECURITY_ATTRIBUTES as *mut c_void,
        )
    }
}

pub async fn serve(shared: Shared, commands: mpsc::Sender<(Request, Reply)>) -> Result<()> {
    let sec = security()?;
    let mut server = create(&sec, true).context(
        "creating the app pipe (is another FanStand service or `fanstand run --serve` already running?)",
    )?;
    loop {
        server.connect().await?;
        let client = server;
        server = create(&sec, false)?;
        let (shared, commands) = (shared.clone(), commands.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(client, shared, commands).await {
                debug!("pipe client: {e:#}");
            }
        });
    }
}

async fn handle(
    pipe: NamedPipeServer,
    shared: Shared,
    commands: mpsc::Sender<(Request, Reply)>,
) -> Result<()> {
    let (rd, mut wr) = tokio::io::split(pipe);
    let mut rd = BufReader::new(rd.take(64 * 1024)); // per-connection cap
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let resp = if line.len() > MAX_LINE {
            Response::Error {
                message: "request too long".into(),
            }
        } else {
            match fanstand_ipc::decode::<Request>(&line) {
                Ok(Request::Snapshot) => Response::Snapshot(Box::new(lock(&shared).clone())),
                Ok(req) => {
                    let (tx, rx) = oneshot::channel();
                    match commands.send((req, tx)).await {
                        Ok(()) => rx.await.unwrap_or(Response::Error {
                            message: "service is shutting down".into(),
                        }),
                        Err(_) => Response::Error {
                            message: "service is shutting down".into(),
                        },
                    }
                }
                Err(e) => {
                    warn!("bad request on the app pipe: {e}");
                    Response::Error {
                        message: format!("bad request: {e}"),
                    }
                }
            }
        };
        wr.write_all(fanstand_ipc::encode(&resp).as_bytes()).await?;
        wr.flush().await?;
    }
}
