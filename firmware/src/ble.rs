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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use esp_idf_svc::sys;
use esp32_nimble::utilities::BleUuid;
use esp32_nimble::utilities::mutex::Mutex as NimbleMutex;
use esp32_nimble::{
    BLEAdvertisementData, BLECharacteristic, BLEDevice, BLEService, NimbleProperties, OnWriteArgs,
};
use fanstand_protocol::auth::{Key, Nonce, verify_response};
use fanstand_protocol::codec::{decode_percent, decode_temperature, encode_percent, encode_rpm};
use fanstand_protocol::diag::{att_error, encode_cal_table};
use fanstand_protocol::{CalResult, Command, Config, DecodeError, FanCurve, Mode, Status, uuids};
use log::{info, warn};

use crate::control::Request;
use crate::state::{self, Shared, Snapshot, lock};

const PREFERRED_MTU: u16 = 185;
const NOTIFY_POLL: Duration = Duration::from_millis(250);
const RPM_NOTIFY_MS: u64 = 1000;

type Chr = Arc<NimbleMutex<BLECharacteristic>>;

#[derive(Clone, Copy)]
struct ConnAuth {
    nonce: Nonce,
    authed: bool,
}

impl ConnAuth {
    fn fresh() -> Self {
        Self {
            nonce: new_nonce(),
            authed: false,
        }
    }
}

type AuthMap = Arc<Mutex<HashMap<u16, ConnAuth>>>;

fn ble_uuid(u: u128) -> BleUuid {
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
        self.key.is_none() || lock(&self.auth).get(&conn).is_some_and(|c| c.authed)
    }

    fn guarded(
        &self,
        args: &mut OnWriteArgs,
        name: &str,
        decode: fn(&[u8]) -> Result<Request, DecodeError>,
    ) {
        if !self.is_authed(args.desc().conn_handle()) {
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

/// A characteristic whose value is taken from the latest snapshot on every read.
fn readable(
    svc: &mut BLEService,
    uuid: u128,
    props: NimbleProperties,
    shared: &Shared,
    value: fn(&Snapshot) -> Vec<u8>,
) -> Chr {
    let chr = svc.create_characteristic(ble_uuid(uuid), props);
    let s = shared.clone();
    chr.lock().on_read(move |c, _| {
        c.set_value(&value(&state::read(&s)));
    });
    chr
}

/// Add the guarded write path to a characteristic.
fn writable(
    chr: &Chr,
    w: &Writer,
    name: &'static str,
    decode: fn(&[u8]) -> Result<Request, DecodeError>,
) {
    let w = w.clone();
    chr.lock().on_write(move |a| w.guarded(a, name, decode));
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
    let connected = Arc::new(AtomicUsize::new(0));
    let w = Writer {
        auth: auth.clone(),
        tx,
        key,
    };

    // ---- connection lifecycle
    {
        let (auth, connected) = (auth.clone(), connected.clone());
        server.on_connect(move |server, desc| {
            info!(
                "BLE: connected {:?} (handle {})",
                desc.address(),
                desc.conn_handle()
            );
            lock(&auth).insert(desc.conn_handle(), ConnAuth::fresh());
            connected.fetch_add(1, Ordering::Relaxed);
            // Keep advertising so a second device (or a laptop whose old link
            // hasn't timed out yet) can still connect.
            if server.connected_count() < sys::CONFIG_BT_NIMBLE_MAX_CONNECTIONS as usize {
                let _ = advertising.lock().start();
            }
        });
    }
    {
        let (auth, connected) = (auth.clone(), connected.clone());
        server.on_disconnect(move |desc, reason| {
            info!("BLE: disconnected {:?} ({reason:?})", desc.address());
            lock(&auth).remove(&desc.conn_handle());
            let _ =
                connected.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        });
    }
    server.advertise_on_disconnect(true);

    let service = server.create_service(ble_uuid(uuids::SERVICE));
    let mut svc = service.lock();
    let r = NimbleProperties::READ;
    let rw = NimbleProperties::READ | NimbleProperties::WRITE;
    let notify = NimbleProperties::READ | NimbleProperties::NOTIFY;

    // ---- settings (read: snapshot, write: guarded)
    let c = readable(&mut svc, uuids::MODE, rw, &shared, |s| {
        s.settings.mode.encode().into()
    });
    writable(&c, &w, "mode", |b| Mode::decode(b).map(Request::SetMode));

    let c = readable(&mut svc, uuids::MANUAL_SPEED, rw, &shared, |s| {
        encode_percent(s.settings.manual_pct).into()
    });
    writable(&c, &w, "manual speed", |b| {
        decode_percent(b).map(Request::SetManual)
    });

    let c = readable(&mut svc, uuids::FAN_CURVE, rw, &shared, |s| {
        let (buf, n) = s.settings.curve.encode();
        buf[..n].to_vec()
    });
    writable(&c, &w, "curve", |b| {
        FanCurve::decode(b).map(Request::SetCurve)
    });

    let c = readable(&mut svc, uuids::CONFIG, rw, &shared, |s| {
        s.settings.config.encode().into()
    });
    writable(&c, &w, "config", |b| {
        Config::decode(b).map(Request::SetConfig)
    });

    // ---- write-only
    let c = svc.create_characteristic(
        ble_uuid(uuids::TEMPERATURE),
        NimbleProperties::WRITE | NimbleProperties::WRITE_NO_RSP,
    );
    writable(&c, &w, "temperature", |b| {
        decode_temperature(b).map(Request::Temperature)
    });

    let c = svc.create_characteristic(ble_uuid(uuids::COMMAND), NimbleProperties::WRITE);
    writable(&c, &w, "command", |b| {
        Command::decode(b).map(Request::Command)
    });

    // ---- read + notify (the notifier thread pushes these)
    let rpm_chr = readable(&mut svc, uuids::FAN_RPM, notify, &shared, |s| {
        encode_rpm(s.rpm).into()
    });
    let status_chr = readable(&mut svc, uuids::STATUS, notify, &shared, |s| {
        s.status.encode().into()
    });
    let cal_chr = readable(&mut svc, uuids::CAL_RESULT, notify, &shared, |s| {
        s.cal.encode().into()
    });

    // ---- read-only
    svc.create_characteristic(ble_uuid(uuids::FIRMWARE_VERSION), r)
        .lock()
        .set_value(version.as_bytes());
    readable(&mut svc, uuids::DEBUG, r, &shared, |s| {
        s.debug.encode().into()
    });
    readable(&mut svc, uuids::CAL_TABLE, r, &shared, |s| {
        encode_cal_table(&s.cal_table).into()
    });

    // ---- AUTH_CHALLENGE / AUTH_RESPONSE
    {
        let auth = auth.clone();
        svc.create_characteristic(ble_uuid(uuids::AUTH_CHALLENGE), r)
            .lock()
            .on_read(move |c, desc| {
                let mut map = lock(&auth);
                c.set_value(
                    &map.entry(desc.conn_handle())
                        .or_insert_with(ConnAuth::fresh)
                        .nonce,
                );
            });
    }
    {
        let (w_read, auth) = (w.clone(), auth.clone());
        svc.create_characteristic(ble_uuid(uuids::AUTH_RESPONSE), rw)
            .lock()
            .on_read(move |c, desc| {
                c.set_value(&[w_read.is_authed(desc.conn_handle()) as u8]);
            })
            .on_write(move |a| {
                let Some(key) = key else { return }; // no-auth build: accept anything
                let mut map = lock(&auth);
                let entry = map
                    .entry(a.desc().conn_handle())
                    .or_insert_with(ConnAuth::fresh);
                let ok = verify_response(&key, &entry.nonce, a.recv_data());
                // Fresh nonce after every attempt: a response can't be replayed.
                entry.nonce = new_nonce();
                entry.authed |= ok;
                if ok {
                    info!("BLE: {:?} authenticated", a.desc().address());
                } else {
                    warn!("BLE: {:?} failed authentication", a.desc().address());
                    a.reject_with_error_code(att_error::AUTH_FAILED);
                }
            });
    }
    drop(svc);

    // ---- advertising: flags + 128-bit service UUID + name = 31 bytes
    advertising
        .lock()
        .set_data(
            BLEAdvertisementData::new()
                .name(uuids::DEVICE_NAME)
                .add_service_uuid(ble_uuid(uuids::SERVICE)),
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

    // ---- notifier thread: RPM every 1 s, STATUS / CAL_RESULT on change.
    // Idles while nobody is connected (no snapshot lock, no notify calls).
    std::thread::Builder::new()
        .name("ble-notify".into())
        .stack_size(4096)
        .spawn(move || {
            let mut last_status: Option<Status> = None;
            let mut last_cal: Option<CalResult> = None;
            let mut since_rpm = 0u64;
            loop {
                std::thread::sleep(NOTIFY_POLL);
                if connected.load(Ordering::Relaxed) == 0 {
                    (last_status, last_cal) = (None, None);
                    continue;
                }
                let snap = state::read(&shared);
                since_rpm += NOTIFY_POLL.as_millis() as u64;
                if since_rpm >= RPM_NOTIFY_MS {
                    since_rpm = 0;
                    rpm_chr.lock().set_value(&encode_rpm(snap.rpm)).notify();
                }
                if last_status != Some(snap.status) {
                    last_status = Some(snap.status);
                    status_chr.lock().set_value(&snap.status.encode()).notify();
                }
                if last_cal != Some(snap.cal) {
                    last_cal = Some(snap.cal);
                    cal_chr.lock().set_value(&snap.cal.encode()).notify();
                }
            }
        })?;
    Ok(())
}
