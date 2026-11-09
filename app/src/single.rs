//! One app per user session. A second launch signals the first (named event)
//! to show its window, then exits.

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent,
    WaitForSingleObject,
};
use windows::core::w;

/// Auto-reset event, set when another launch wants the window shown.
pub struct ShowSignal(isize);

impl ShowSignal {
    /// Blocks until signalled.
    pub fn wait(&self) {
        unsafe {
            WaitForSingleObject(HANDLE(self.0 as *mut _), INFINITE);
        }
    }
}

/// `Some` if we're the first instance; otherwise pokes the first and returns `None`.
pub fn claim() -> Option<ShowSignal> {
    unsafe {
        // Leaked on purpose: held for the life of the process.
        let _mutex = CreateMutexW(None, true, w!("Local\\FanStandApp")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\FanStandAppShow")) {
                let _ = SetEvent(ev);
            }
            return None;
        }
        let ev = CreateEventW(None, false, false, w!("Local\\FanStandAppShow")).ok()?;
        Some(ShowSignal(ev.0 as isize))
    }
}
