//! Logging. Interactive commands log to stderr (`RUST_LOG` overrides).
//! The Windows service logs to:
//! - a rotating file, `%ProgramData%\fanstand\logs\service.log` (1 MiB × 4),
//!   everything at info and above;
//! - the Windows Event Log (Application, source "FanStand"): warnings,
//!   errors, and info records sent with `target: EVENT` (start/stop,
//!   overrides): the things worth seeing in Event Viewer.

/// `info!(target: EVENT, ...)` also goes to the Windows Event Log.
pub const EVENT: &str = "event";

pub fn init_console() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("fanstand=info,event=info"),
    )
    .format_target(false)
    .init();
}

#[cfg(windows)]
pub use service_log::{init_service, log_dir};

#[cfg(windows)]
mod service_log {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use log::{Level, LevelFilter, Log, Metadata, Record};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::EventLog::{
        EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
        REPORT_EVENT_TYPE, RegisterEventSourceW, ReportEventW,
    };
    use windows::core::{HSTRING, PCWSTR};

    use super::EVENT;
    use crate::service::EVENT_SOURCE;

    const MAX_BYTES: u64 = 1024 * 1024;
    const KEEP: usize = 3; // service.log.1 .. .3

    pub fn log_dir() -> PathBuf {
        crate::key::program_data().join("logs")
    }

    struct FileSink {
        path: PathBuf,
        file: Option<File>,
        len: u64,
    }

    impl FileSink {
        fn open(path: PathBuf) -> Self {
            let file = OpenOptions::new().create(true).append(true).open(&path).ok();
            let len = file
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .map_or(0, |m| m.len());
            Self { path, file, len }
        }

        fn write(&mut self, line: &str) {
            if self.len + line.len() as u64 > MAX_BYTES {
                self.rotate();
            }
            if let Some(f) = &mut self.file
                && f.write_all(line.as_bytes()).is_ok()
            {
                self.len += line.len() as u64;
            }
        }

        fn rotate(&mut self) {
            self.file = None; // close before renaming
            let name = |i: usize| self.path.with_extension(format!("log.{i}"));
            let _ = std::fs::remove_file(name(KEEP));
            for i in (1..KEEP).rev() {
                let _ = std::fs::rename(name(i), name(i + 1));
            }
            let _ = std::fs::rename(&self.path, name(1));
            *self = Self::open(self.path.clone());
        }
    }

    struct ServiceLogger {
        file: Mutex<FileSink>,
        /// Event source handle as an integer (HANDLE isn't Send/Sync).
        event_source: Option<isize>,
    }

    impl Log for ServiceLogger {
        fn enabled(&self, m: &Metadata) -> bool {
            m.level() <= Level::Info
                && (m.target() == EVENT
                    || m.target().starts_with("fanstand")
                    || m.level() <= Level::Warn)
        }

        fn log(&self, r: &Record) {
            if !self.enabled(r.metadata()) {
                return;
            }
            let msg = r.args().to_string();
            let now = humantime_utc();
            let line = format!("[{now} {:<5}] {msg}\r\n", r.level());
            self.file
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .write(&line);

            let kind: Option<REPORT_EVENT_TYPE> = match r.level() {
                Level::Error => Some(EVENTLOG_ERROR_TYPE),
                Level::Warn => Some(EVENTLOG_WARNING_TYPE),
                _ if r.target() == EVENT => Some(EVENTLOG_INFORMATION_TYPE),
                _ => None,
            };
            if let (Some(kind), Some(h)) = (kind, self.event_source) {
                let text = HSTRING::from(msg);
                // Event IDs 1..3 map to "%1" in the message file we register
                // (EventLogMessages.dll), so Event Viewer shows our text.
                let id = match r.level() {
                    Level::Error => 3,
                    Level::Warn => 2,
                    _ => 1,
                };
                unsafe {
                    let _ = ReportEventW(
                        HANDLE(h as *mut _),
                        kind,
                        0,
                        id,
                        None,
                        0,
                        Some(&[PCWSTR(text.as_ptr())]),
                        None,
                    );
                }
            }
        }

        fn flush(&self) {
            if let Some(f) = &mut self.file.lock().unwrap_or_else(|p| p.into_inner()).file {
                let _ = f.flush();
            }
        }
    }

    pub fn init_service() {
        let dir = log_dir();
        let _ = std::fs::create_dir_all(&dir);
        let event_source = unsafe { RegisterEventSourceW(None, &HSTRING::from(EVENT_SOURCE)) }
            .ok()
            .map(|h| h.0 as isize);
        let logger = ServiceLogger {
            file: Mutex::new(FileSink::open(dir.join("service.log"))),
            event_source,
        };
        if log::set_boxed_logger(Box::new(logger)).is_ok() {
            log::set_max_level(LevelFilter::Info);
        }
        std::panic::set_hook(Box::new(|p| log::error!("panic: {p}")));
    }

    /// "2026-11-09 13:23:48Z" without pulling in a date crate.
    fn humantime_utc() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let (days, rem) = (secs / 86_400, secs % 86_400);
        // Civil-from-days (Howard Hinnant's algorithm).
        let z = days as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
            rem / 3600,
            rem / 60 % 60,
            rem % 60
        )
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn timestamp_shape() {
            let t = super::humantime_utc();
            assert_eq!(t.len(), 20, "{t}");
            assert!(t.starts_with("20") && t.ends_with('Z'));
        }
    }
}
