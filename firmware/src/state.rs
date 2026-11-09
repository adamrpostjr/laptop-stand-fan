//! The snapshot the control loop publishes for the BLE side.
//!
//! The control loop only ever `try_lock`s this, so BLE can't block fan
//! control. If the lock is busy, that tick's update is skipped and the next
//! one (100 ms later) publishes instead.

use std::sync::{Arc, Mutex};

use fanstand_protocol::calibration::STEPS;
use fanstand_protocol::diag::DebugInfo;
use fanstand_protocol::{CalResult, Settings, Status};

#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub settings: Settings,
    pub rpm: [u16; 2],
    pub status: Status,
    pub cal: CalResult,
    pub cal_table: [[u16; 2]; STEPS],
    pub debug: DebugInfo,
}

pub type Shared = Arc<Mutex<Snapshot>>;

pub fn new_shared(settings: Settings, reset_reason: u8, no_auth: bool) -> Shared {
    Arc::new(Mutex::new(Snapshot {
        settings,
        rpm: [0; 2],
        status: Status::default(),
        cal: CalResult::None,
        cal_table: [[0; 2]; STEPS],
        debug: DebugInfo {
            reset_reason,
            flags: if no_auth { DebugInfo::FLAG_NO_AUTH } else { 0 },
            ..Default::default()
        },
    }))
}

/// Lock for the BLE side (NimBLE task / notifier thread). Recovers from poisoning.
pub fn read(shared: &Shared) -> Snapshot {
    *shared.lock().unwrap_or_else(|e| e.into_inner())
}
