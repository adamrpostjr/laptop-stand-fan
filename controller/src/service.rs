//! Windows service: `fanstand service install|uninstall|start|stop|status`.
//!
//! The service runs `fanstand.exe service run` as LocalSystem. It owns the
//! BLE link (the same loop as `fanstand run`), serves the desktop app over a
//! named pipe (see [`crate::pipe`]), and drops/reconnects the link around
//! sleep via SCM power events.
//!
//! `service install` (elevated) copies this exe to
//! `%ProgramFiles%\FanStand\`, installs the key machine-wide, registers the
//! Event Log source and creates an auto-start service that restarts itself on
//! failure. The MSI does the same things declaratively; use one or the other.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{error, info};
use windows_service::service::{
    PowerEventParam, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl,
    ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::logging::EVENT;
use crate::run::{self, Power};

pub const SERVICE_NAME: &str = "FanStand";
pub const EVENT_SOURCE: &str = "FanStand";
const DISPLAY_NAME: &str = "FanStand cooling stand";
const DESCRIPTION: &str = "Sends the CPU temperature to the Bluetooth cooling laptop stand \
    so its fans follow the fan curve. Serves the FanStand app.";
const EVENTLOG_KEY: &str = r"SYSTEM\CurrentControlSet\Services\EventLog\Application\FanStand";
/// Ships with .NET Framework 4 (part of Windows): message N is just "%1",
/// so Event Viewer shows our text without a custom message DLL.
const EVENT_MESSAGE_FILE: &str =
    r"%SystemRoot%\Microsoft.NET\Framework64\v4.0.30319\EventLogMessages.dll";

// ------------------------------------------------------------------ running

define_windows_service!(ffi_service_main, service_main);

/// Entry point for `fanstand service run`. Blocks until the service stops.
pub fn dispatch() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context(
        "`service run` is started by Windows; use `fanstand service install` / `fanstand run` instead",
    )
}

fn service_main(_args: Vec<OsString>) {
    crate::logging::init_service();
    if let Err(e) = run_service() {
        error!("service failed: {e:#}");
    }
}

fn run_service() -> Result<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stop_tx = Mutex::new(Some(stop_tx));
    let (senders, inputs) = run::inputs();
    let power = senders.power.clone();

    let handler = move |c: ServiceControl| match c {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(tx) = stop_tx.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = tx.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::PowerEvent(p) => {
            match p {
                PowerEventParam::Suspend => {
                    let _ = power.send(Power::Suspend);
                }
                PowerEventParam::ResumeAutomatic | PowerEventParam::ResumeSuspend => {
                    let _ = power.send(Power::Resume);
                }
                _ => {}
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let set = |state, accept, code| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(code),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::POWER_EVENT,
        0,
    )?;
    info!(target: EVENT, "FanStand service {} started", env!("CARGO_PKG_VERSION"));

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .context("starting async runtime")
        .and_then(|rt| {
            rt.block_on(async move {
                let shared = run::new_shared();
                let pipe = tokio::spawn(crate::pipe::serve(shared.clone(), senders.requests.clone()));
                let r = run::run(
                    run::Options {
                        device: None,
                        scan_timeout: Duration::from_secs(10),
                        interval: Duration::from_secs(2),
                        force_estimate: false,
                    },
                    shared,
                    inputs,
                    async {
                        let _ = stop_rx.await;
                    },
                )
                .await;
                pipe.abort();
                drop(senders);
                r
            })
        });

    let code = match &result {
        Ok(()) => {
            info!(target: EVENT, "FanStand service stopped");
            0
        }
        Err(e) => {
            error!("FanStand service stopped: {e:#}");
            1
        }
    };
    log::logger().flush();
    set(ServiceState::Stopped, ServiceControlAccept::empty(), code)?;
    result
}

// ------------------------------------------------------------ management

fn manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access).map_err(elevation_hint)
}

/// Access denied → tell the user how to elevate.
fn elevation_hint(e: windows_service::Error) -> anyhow::Error {
    if let windows_service::Error::Winapi(io) = &e
        && io.raw_os_error() == Some(5)
    {
        return anyhow::anyhow!(
            "access denied: run this from an elevated terminal (e.g. `sudo fanstand service ...`)"
        );
    }
    e.into()
}

pub fn install_dir() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from)
        .join("FanStand")
}

pub fn install(key_file: Option<&Path>) -> Result<()> {
    let mgr = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;

    // 1. Key: --key file, else this user's key. Validate before anything.
    let key_src = match key_file {
        Some(p) => p.to_path_buf(),
        None => crate::key::key_path()?,
    };
    let hex = crate::key::read_key_file(&key_src).context("pass the key with --key <file>")?;
    let key_dst = crate::key::install_machine_key(&hex)?;
    println!("key        {} (SYSTEM + Administrators only)", key_dst.display());

    // 2. If an old copy is running, stop it so the exe can be replaced.
    if let Ok(svc) = mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP) {
        stop_and_wait(&svc)?;
    }

    // 3. Binary.
    let dir = install_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let exe = dir.join("fanstand.exe");
    let me = std::env::current_exe()?;
    if !same_file(&me, &exe) {
        std::fs::copy(&me, &exe).with_context(|| format!("copying to {}", exe.display()))?;
        // The desktop app, if it was built next to us.
        let gui = me.with_file_name("fanstand-app.exe");
        if gui.exists() {
            std::fs::copy(&gui, dir.join("fanstand-app.exe"))?;
        }
    }
    println!("binary     {}", exe.display());

    // 4. Event Log source.
    register_event_source()?;
    println!("event log  Application / {EVENT_SOURCE}");

    // 5. Service.
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec!["service".into(), "run".into()],
        // Bluetooth support service: don't start before the radio stack.
        dependencies: vec![windows_service::service::ServiceDependency::Service("bthserv".into())],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP;
    let svc = match mgr.open_service(SERVICE_NAME, access) {
        Ok(svc) => {
            svc.change_config(&info)?;
            svc
        }
        Err(_) => mgr.create_service(&info, access).map_err(elevation_hint)?,
    };
    svc.set_description(DESCRIPTION)?;
    svc.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            restart_after(5),
            restart_after(15),
            restart_after(60),
        ]),
    })?;
    svc.set_failure_actions_on_non_crash_failures(true)?;
    svc.start::<&str>(&[])?;
    println!("service    {SERVICE_NAME} installed and started (auto-start, restarts on failure)");
    println!("logs       {}", crate::logging::log_dir().display());
    Ok(())
}

fn restart_after(secs: u64) -> ServiceAction {
    ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: Duration::from_secs(secs),
    }
}

pub fn uninstall(purge: bool) -> Result<()> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    match mgr.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(svc) => {
            stop_and_wait(&svc)?;
            svc.delete()?;
            println!("service    removed");
        }
        Err(e) => println!("service    not installed ({})", elevation_hint(e)),
    }
    let _ = delete_event_source();
    if purge {
        let _ = std::fs::remove_dir_all(crate::key::program_data());
        let _ = std::fs::remove_dir_all(install_dir());
        println!("removed    key, logs and {}", install_dir().display());
    } else {
        println!(
            "kept       key and logs in {} (use --purge to remove)",
            crate::key::program_data().display()
        );
    }
    Ok(())
}

pub fn start() -> Result<()> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let svc = mgr
        .open_service(SERVICE_NAME, ServiceAccess::START)
        .map_err(elevation_hint)?;
    svc.start::<&str>(&[])?;
    println!("started");
    Ok(())
}

pub fn stop() -> Result<()> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let svc = mgr
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP)
        .map_err(elevation_hint)?;
    stop_and_wait(&svc)?;
    println!("stopped");
    Ok(())
}

pub fn status() -> Result<()> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    match mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(svc) => println!("service    {:?}", svc.query_status()?.current_state),
        Err(_) => println!("service    not installed"),
    }
    match fanstand_ipc::call(&fanstand_ipc::Request::Snapshot) {
        Ok(fanstand_ipc::Response::Snapshot(s)) => {
            println!("link       {:?}", s.link);
            if let Some(e) = &s.last_error {
                println!("last error {e}");
            }
            if let Some(c) = s.cpu {
                println!("cpu        {:.1} °C{}", c.celsius, if c.estimated { " (estimated)" } else { "" });
            }
            if let Some(st) = &s.stand {
                println!(
                    "stand      {} | rpm {} / {} | duty {:.1}% | {:?}{} | {}",
                    st.name,
                    st.rpm[0],
                    st.rpm[1],
                    st.duty_pct,
                    st.mode,
                    if st.mode == fanstand_ipc::Mode::Manual {
                        format!(" {}%", st.manual_pct)
                    } else {
                        String::new()
                    },
                    st.flags
                );
            }
        }
        Ok(other) => println!("pipe       unexpected reply {other:?}"),
        Err(e) => println!("pipe       {e}"),
    }
    Ok(())
}

fn stop_and_wait(svc: &windows_service::service::Service) -> Result<()> {
    if svc.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    let _ = svc.stop();
    for _ in 0..100 {
        if svc.query_status()?.current_state == ServiceState::Stopped {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("service did not stop within 10 s")
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// ------------------------------------------------------------ event source

fn register_event_source() -> Result<()> {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
        RegCloseKey, RegCreateKeyExW, RegSetValueExW,
    };
    use windows::core::HSTRING;

    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(EVENTLOG_KEY),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()
        .context("registering the Event Log source")?;
        let file: Vec<u16> = EVENT_MESSAGE_FILE.encode_utf16().chain([0]).collect();
        let file_bytes = std::slice::from_raw_parts(file.as_ptr().cast::<u8>(), file.len() * 2);
        let r1 = RegSetValueExW(key, &HSTRING::from("EventMessageFile"), None, REG_EXPAND_SZ, Some(file_bytes));
        let types = 7u32.to_le_bytes(); // error | warning | information
        let r2 = RegSetValueExW(key, &HSTRING::from("TypesSupported"), None, REG_DWORD, Some(&types));
        let _ = RegCloseKey(key);
        r1.ok()?;
        r2.ok()?;
    }
    Ok(())
}

fn delete_event_source() -> Result<()> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RegDeleteKeyW};
    use windows::core::HSTRING;
    unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, &HSTRING::from(EVENTLOG_KEY)).ok()? };
    Ok(())
}
