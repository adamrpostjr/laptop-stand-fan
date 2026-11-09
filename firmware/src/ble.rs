//! BLE GATT server (NimBLE via `esp32-nimble`).
//!
//! - **Reads** come from the control loop's published [`Snapshot`] and are
//!   open to anyone in range.
//! - **Writes** are validated here with the shared `protocol` codecs, then
//!   sent to the control loop as `Request`s over a channel. Nothing here
//!   waits on the control loop, and the control loop never waits on this.
//! - **Auth:** every write needs this connection to have passed the HMAC
//!   challenge first. Rejections use application ATT error codes (0x80+),
//!   never "Insufficient Authentication", which would trigger OS pairing.
//! - **Disconnect:** state is kept and advertising restarts.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use esp_idf_svc::sys;
use esp32_nimble::utilities::BleUuid;
use esp32_nimble::{BLEAdvertisementData, BLEDevice, NimbleProperties, OnWriteArgs};
use fanstand_protocol::auth::{Key, Nonce, verify_response};
use fanstand_protocol::codec::{decode_percent, decode_temperature, encode_rpm};
use fanstand_protocol::diag::{att_error, encode_cal_table};
use fanstand_protocol::{CalResult, Command, Config, DecodeError, FanCurve, Mode, Status, uuids};
use log::{info, warn};

use crate::control::Request;
use crate::state::{self, Shared};

const PREFERRED_MTU: u16 = 185;
const NOTIFY_POLL: Duration = Duration::from_millis(250);
const RPM_NOTIFY_MS: u64 = 1000;

#[derive(Clone, Copy)]
struct ConnAuth {
    nonce: Nonce,
    authed: bool,
}

type AuthMap = Arc<Mutex<HashMap<u16, ConnAuth>>>;

fn uuid(u: u128) -> BleUuid {
    BleUuid::from_uuid128(uuids::to_le_bytes(u))
}

fn new_nonce() -> Nonce {
    let mut n = [0u8; 16];
    // Hardware RNG (true random while the radio is on).
    unsafe { sys::esp_fill_random(n.as_mut_ptr().cast(), n.len()) };
    n
}

/// Shared write-path logic: auth check → decode/validate → enqueue.
#[derive(Clone)]
struct Writer {
    auth: AuthMap,
    tx: Sender<Request>,
    /// `None` = `no-auth` test build: every connection may write.
    key: Option<Key>,
}

impl Writer {
    fn is_authed(&self, conn: u16) -> bool {
        self.key.is_none()
            || self
                .auth
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&conn)
                .is_some_and(|c| c.authed)
    }

    fn guarded(
        &self,
        args: &mut OnWriteArgs,
        name: &str,
        decode: impl FnOnce(&[u8]) -> Result<Request, DecodeError>,
    ) {
        let conn = args.desc().conn_handle();
        if !self.is_authed(conn) {
            warn!("BLE: unauthenticated write to {name} rejected");
            args.reject_with_error_code(att_error::NOT_AUTHENTICATED);
            return;
        }
        match decode(args.recv_data()) {
            Ok(req) => {
                let _ = self.tx.send(req);
            }
            Err(e) => {
                warn!("BLE: invalid {name} write {:02x?}: {e}", args.recv_data());
                args.reject_with_error_code(att_error::INVALID_VALUE);
            }
        }
    }
}

pub fn start(shared: Shared, tx: Sender<Request>, key: Option<Key>, version: &str) -> Result<()> {
    let device = BLEDevice::take();
    BLEDevice::set_device_name(uuids::DEVICE_NAME).map_err(|e| anyhow!("{e:?}"))?;
    if let Err(e) = device.set_preferred_mtu(PREFERRED_MTU) {
        warn!("BLE: set MTU failed: {e:?}");
    }
    let advertising = device.get_advertising();
    let server = device.get_server();
    let auth: AuthMap = Arc::default();
    let w = Writer {
        auth: auth.clone(),
        tx,
        key,
    };

    // ---- connection lifecycle
    {
        let auth = auth.clone();
        server.on_connect(move |server, desc| {
            info!(
                "BLE: connected {:?} (handle {})",
                desc.address(),
                desc.conn_handle()
            );
            auth.lock().unwrap_or_else(|e| e.into_inner()).insert(
                desc.conn_handle(),
                ConnAuth {
                    nonce: new_nonce(),
                    authed: false,
                },
            );
            // Keep advertising so a second device (or a laptop whose old link
            // hasn't timed out yet) can still connect.
            if server.connected_count() < sys::CONFIG_BT_NIMBLE_MAX_CONNECTIONS as usize {
                let _ = advertising.lock().start();
            }
        });
    }
    {
        let auth = auth.clone();
        server.on_disconnect(move |desc, reason| {
            info!("BLE: disconnected {:?} ({reason:?})", desc.address());
            auth.lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&desc.conn_handle());
        });
    }
    server.advertise_on_disconnect(true);

    let service = server.create_service(uuid(uuids::SERVICE));
    let mut svc = service.lock();
    let rw = NimbleProperties::READ | NimbleProperties::WRITE;

    // ---- MODE
    {
        let (s, w) = (shared.clone(), w.clone());
        svc.create_characteristic(uuid(uuids::MODE), rw)
            .lock()
            .on_read(move |c, _| {
                c.set_value(&state::read(&s).settings.mode.encode());
            })
            .on_write(move |a| w.guarded(a, "mode", |b| Mode::decode(b).map(Request::SetMode)));
    }
    // ---- MANUAL_SPEED
    {
        let (s, w) = (shared.clone(), w.clone());
        svc.create_characteristic(uuid(uuids::MANUAL_SPEED), rw)
            .lock()
            .on_read(move |c, _| {
                c.set_value(&[state::read(&s).settings.manual_pct]);
            })
            .on_write(move |a| {
                w.guarded(a, "manual speed", |b| {
                    decode_percent(b).map(Request::SetManual)
                })
            });
    }
    // ---- TEMPERATURE (write / write-without-response)
    {
        let w = w.clone();
        svc.create_characteristic(
            uuid(uuids::TEMPERATURE),
            NimbleProperties::WRITE | NimbleProperties::WRITE_NO_RSP,
        )
        .lock()
        .on_write(move |a| {
            w.guarded(a, "temperature", |b| {
                decode_temperature(b).map(Request::Temperature)
            })
        });
    }
    // ---- FAN_CURVE
    {
        let (s, w) = (shared.clone(), w.clone());
        svc.create_characteristic(uuid(uuids::FAN_CURVE), rw)
            .lock()
            .on_read(move |c, _| {
                let (buf, n) = state::read(&s).settings.curve.encode();
                c.set_value(&buf[..n]);
            })
            .on_write(move |a| {
                w.guarded(a, "curve", |b| FanCurve::decode(b).map(Request::SetCurve))
            });
    }
    // ---- CONFIG
    {
        let (s, w) = (shared.clone(), w.clone());
        svc.create_characteristic(uuid(uuids::CONFIG), rw)
            .lock()
            .on_read(move |c, _| {
                c.set_value(&state::read(&s).settings.config.encode());
            })
            .on_write(move |a| {
                w.guarded(a, "config", |b| Config::decode(b).map(Request::SetConfig))
            });
    }
    // ---- COMMAND
    {
        let w = w.clone();
        svc.create_characteristic(uuid(uuids::COMMAND), NimbleProperties::WRITE)
            .lock()
            .on_write(move |a| {
                w.guarded(a, "command", |b| {
                    Command::decode(b).map(|c| match c {
                        Command::StartCalibration => Request::StartCalibration,
                        Command::AbortCalibration => Request::AbortCalibration,
                    })
                })
            });
    }
    // ---- FAN_RPM, STATUS, CAL_RESULT (read + notify)
    let rpm_chr = svc.create_characteristic(
        uuid(uuids::FAN_RPM),
        NimbleProperties::READ | NimbleProperties::NOTIFY,
    );
    {
        let s = shared.clone();
        rpm_chr
            .lock()
            .set_value(&encode_rpm([0, 0]))
            .on_read(move |c, _| {
                c.set_value(&encode_rpm(state::read(&s).rpm));
            });
    }
    let status_chr = svc.create_characteristic(
        uuid(uuids::STATUS),
        NimbleProperties::READ | NimbleProperties::NOTIFY,
    );
    {
        let s = shared.clone();
        status_chr.lock().set_value(&[0]).on_read(move |c, _| {
            c.set_value(&[state::read(&s).status.0]);
        });
    }
    let cal_chr = svc.create_characteristic(
        uuid(uuids::CAL_RESULT),
        NimbleProperties::READ | NimbleProperties::NOTIFY,
    );
    {
        let s = shared.clone();
        cal_chr
            .lock()
            .set_value(&CalResult::None.encode())
            .on_read(move |c, _| {
                c.set_value(&state::read(&s).cal.encode());
            });
    }
    // ---- FIRMWARE_VERSION
    svc.create_characteristic(uuid(uuids::FIRMWARE_VERSION), NimbleProperties::READ)
        .lock()
        .set_value(version.as_bytes());
    // ---- AUTH_CHALLENGE / AUTH_RESPONSE
    {
        let auth = auth.clone();
        svc.create_characteristic(uuid(uuids::AUTH_CHALLENGE), NimbleProperties::READ)
            .lock()
            .on_read(move |c, desc| {
                let mut map = auth.lock().unwrap_or_else(|e| e.into_inner());
                let entry = map.entry(desc.conn_handle()).or_insert(ConnAuth {
                    nonce: new_nonce(),
                    authed: false,
                });
                c.set_value(&entry.nonce);
            });
    }
    {
        let (auth_r, auth_w) = (auth.clone(), auth.clone());
        let no_auth = key.is_none();
        svc.create_characteristic(uuid(uuids::AUTH_RESPONSE), rw)
            .lock()
            .on_read(move |c, desc| {
                let ok = no_auth
                    || auth_r
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&desc.conn_handle())
                        .is_some_and(|a| a.authed);
                c.set_value(&[ok as u8]);
            })
            .on_write(move |a| {
                let Some(key) = key else { return }; // no-auth build: accept anything
                let conn = a.desc().conn_handle();
                let mut map = auth_w.lock().unwrap_or_else(|e| e.into_inner());
                let entry = map.entry(conn).or_insert(ConnAuth {
                    nonce: new_nonce(),
                    authed: false,
                });
                let ok = verify_response(&key, &entry.nonce, a.recv_data());
                // Fresh nonce after every attempt: a response can't be replayed.
                entry.nonce = new_nonce();
                if ok {
                    entry.authed = true;
                    info!("BLE: {:?} authenticated", a.desc().address());
                } else {
                    warn!("BLE: {:?} failed authentication", a.desc().address());
                    a.reject_with_error_code(att_error::AUTH_FAILED);
                }
            });
    }
    // ---- DEBUG, CAL_TABLE
    {
        let s = shared.clone();
        svc.create_characteristic(uuid(uuids::DEBUG), NimbleProperties::READ)
            .lock()
            .on_read(move |c, _| {
                c.set_value(&state::read(&s).debug.encode());
            });
    }
    {
        let s = shared.clone();
        svc.create_characteristic(uuid(uuids::CAL_TABLE), NimbleProperties::READ)
            .lock()
            .on_read(move |c, _| {
                c.set_value(&encode_cal_table(&state::read(&s).cal_table));
            });
    }
    drop(svc);

    // ---- advertising: flags + 128-bit service UUID + name = 31 bytes
    advertising
        .lock()
        .set_data(
            BLEAdvertisementData::new()
                .name(uuids::DEVICE_NAME)
                .add_service_uuid(uuid(uuids::SERVICE)),
        )
        .map_err(|e| anyhow!("advertising data: {e:?}"))?;
    advertising
        .lock()
        .start()
        .map_err(|e| anyhow!("advertising start: {e:?}"))?;
    info!(
        "BLE: advertising as '{}' (auth {})",
        uuids::DEVICE_NAME,
        if key.is_some() {
            "required"
        } else {
            "DISABLED: no-auth test build"
        }
    );

    // ---- notifier thread: RPM every 1 s, STATUS / CAL_RESULT on change
    std::thread::Builder::new()
        .name("ble-notify".into())
        .stack_size(4096)
        .spawn(move || {
            let mut last_status: Option<Status> = None;
            let mut last_cal: Option<CalResult> = None;
            let mut since_rpm = 0u64;
            loop {
                std::thread::sleep(NOTIFY_POLL);
                let snap = state::read(&shared);
                since_rpm += NOTIFY_POLL.as_millis() as u64;
                if since_rpm >= RPM_NOTIFY_MS {
                    since_rpm = 0;
                    rpm_chr.lock().set_value(&encode_rpm(snap.rpm)).notify();
                }
                if last_status != Some(snap.status) {
                    last_status = Some(snap.status);
                    status_chr.lock().set_value(&[snap.status.0]).notify();
                }
                if last_cal != Some(snap.cal) {
                    last_cal = Some(snap.cal);
                    cal_chr.lock().set_value(&snap.cal.encode()).notify();
                }
            }
        })?;
    Ok(())
}
