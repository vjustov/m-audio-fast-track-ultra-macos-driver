//! Control service: owns the mixer state, answers XPC control requests, and pushes
//! changes to the device from the service loop.
//!
//! Requests and replies are JSON strings carried in the XPC "ctl" op (xpc_shim.c):
//!   {"get": "state"}                       -> the mixer state (mixer.rs)
//!   {"get": "meters"}                      -> latest meter snapshot
//!   {"get": "status"}                      -> connection, streaming, rate, power, firmware
//!   {"put": {state}}                       -> replaces the whole state (settings file restore)
//!   {"set": [["bus1.in1.level", "-6"], ..]} -> {"ok": true} or {"error": ".."}; all or nothing
//!   {"reset": true}                        -> defaults

use crate::mixer::{Ctl, Mixer};
use crate::usb::Device;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// Each GET_MEM returns the snapshot taken at the previous one, so the poll period is
// also the meters' age.
const METER_PERIOD: Duration = Duration::from_millis(20);
/// Polls kept for the peak window: 3 × 20 ms covers a client reading every 33 ms
/// (the panel's 30 Hz) without losing short peaks, and no reader resets it for another.
const PEAK_POLLS: usize = 3;

struct Meters {
    words: [i16; 27],
    /// Magnitudes from the last `PEAK_POLLS` polls, newest last.
    recent: Vec<[u16; 26]>,
    at: Instant,
}

impl Meters {
    fn peak(&self) -> [u16; 26] {
        let mut p = [0u16; 26];
        for r in &self.recent {
            for (a, b) in p.iter_mut().zip(r) {
                *a = (*a).max(*b);
            }
        }
        p
    }
}

#[derive(Default)]
struct Status {
    connected: bool,
    sample_rate: Option<u32>, // Some while streaming
    firmware: Option<String>,
}

pub struct Control {
    mixer: Mutex<Mixer>,
    path: Option<PathBuf>,
    dirty: Mutex<bool>,
    wake: Condvar,
    meters: Mutex<Option<Meters>>,
    status: Mutex<Status>,
}

static CONTROL: OnceLock<Control> = OnceLock::new();

/// `~/Library/Application Support/FastTrackUltra/mixer.json` for the daemon's user.
fn state_path() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer to static storage or null; read immediately.
    let home = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned()
    };
    Some(PathBuf::from(home).join("Library/Application Support/FastTrackUltra/mixer.json"))
}

fn load(path: &Option<PathBuf>) -> Mixer {
    let Some(p) = path else { return Mixer::default() };
    match std::fs::read_to_string(p) {
        Ok(s) => match serde_json::from_str::<Mixer>(&s) {
            Ok(mut m) => {
                m.normalize();
                eprintln!("ftu-daemon: mixer state loaded from {}", p.display());
                m
            }
            Err(e) => {
                let bad = p.with_extension("json.bad");
                let _ = std::fs::rename(p, &bad);
                eprintln!("ftu-daemon: {} unreadable ({e}), moved to {}, using defaults", p.display(), bad.display());
                Mixer::default()
            }
        },
        Err(_) => Mixer::default(),
    }
}

impl Control {
    /// Loads the saved state. Call once before the XPC listener starts.
    pub fn init() -> &'static Control {
        CONTROL.get_or_init(|| {
            let path = state_path();
            Control {
                mixer: Mutex::new(load(&path)),
                path,
                dirty: Mutex::new(true),
                wake: Condvar::new(),
                meters: Mutex::new(None),
                status: Mutex::new(Status::default()),
            }
        })
    }

    fn save(&self, m: &Mixer) {
        let Some(p) = &self.path else { return };
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(p.parent().unwrap())?;
            let tmp = p.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
            std::fs::rename(&tmp, p)
        };
        if let Err(e) = write() {
            eprintln!("ftu-daemon: saving {} failed: {e}", p.display());
        }
    }

    fn changed(&self, m: &Mixer) {
        self.save(m);
        *self.dirty.lock().unwrap() = true;
        self.wake.notify_all();
    }

    fn handle(&self, req: &str) -> Value {
        let req: Value = match serde_json::from_str(req) {
            Ok(v) => v,
            Err(e) => return json!({ "error": format!("bad request: {e}") }),
        };
        if let Some(what) = req.get("get").and_then(Value::as_str) {
            return match what {
                "state" => serde_json::to_value(&*self.mixer.lock().unwrap()).unwrap(),
                "meters" => self.meters_json(),
                "status" => self.status_json(),
                _ => json!({ "error": format!("unknown get '{what}'") }),
            };
        }
        if let Some(list) = req.get("set").and_then(Value::as_array) {
            let mut m = self.mixer.lock().unwrap();
            let mut next = m.clone();
            for item in list {
                let pair = item.as_array().map(|a| (a.first().and_then(Value::as_str), a.get(1).and_then(Value::as_str)));
                let (Some((Some(path), Some(value))), true) = (pair, item.as_array().is_some_and(|a| a.len() == 2)) else {
                    return json!({ "error": "set expects [[path, value], ...] with string values" });
                };
                if let Err(e) = next.set(path, value) {
                    return json!({ "error": e });
                }
            }
            if next != *m {
                *m = next;
                self.changed(&m);
            }
            return json!({ "ok": true });
        }
        if let Some(state) = req.get("put") {
            let mut next: Mixer = match serde_json::from_value(state.clone()) {
                Ok(m) => m,
                Err(e) => return json!({ "error": format!("bad state: {e}") }),
            };
            next.normalize();
            let mut m = self.mixer.lock().unwrap();
            *m = next;
            self.changed(&m);
            return json!({ "ok": true });
        }
        if req.get("reset").and_then(Value::as_bool) == Some(true) {
            let mut m = self.mixer.lock().unwrap();
            *m = Mixer::default();
            self.changed(&m);
            return json!({ "ok": true });
        }
        json!({ "error": "unknown request" })
    }

    fn status_json(&self) -> Value {
        let st = self.status.lock().unwrap();
        let word = self.meters.lock().unwrap().as_ref().map(|m| m.words[26] as u16);
        json!({
            "connected": st.connected,
            "streaming": st.sample_rate.is_some(),
            "sample_rate": st.sample_rate,
            "clock": "internal",
            // Status bit 0: 1 = bus powered, 0 = PSU (docs/protocol.md).
            "power": word.map(|w| if w & 1 != 0 { "usb" } else { "psu" }),
            "firmware": st.firmware,
            "driver": env!("CARGO_PKG_VERSION"),
        })
    }

    fn meters_json(&self) -> Value {
        match &*self.meters.lock().unwrap() {
            Some(mt) => {
                let w = mt.peak();
                json!({
                    "in": &w[0..8],
                    "daw": &w[8..16],
                    "out": &w[16..24],
                    "extra": &w[24..26],
                    "status": mt.words[26] as u16,
                    "age_ms": mt.at.elapsed().as_millis() as u64,
                })
            }
            None => json!({ "error": "no meters yet (device not connected?)" }),
        }
    }

    /// Called from the service loop while a device is open. Sends state changes that
    /// are not yet on the device (`sent` caches what is), polls the meters, and waits
    /// up to `timeout` for the next change.
    pub fn service(&self, dev: &Device, sent: &mut HashMap<(u8, u8, u8), Vec<u8>>, timeout: Duration) {
        let poll_due = self.meters.lock().unwrap().as_ref().map_or(true, |m| m.at.elapsed() >= METER_PERIOD);
        if poll_due {
            if let Ok(words) = dev.read_meters_raw() {
                let mut meters = self.meters.lock().unwrap();
                let mut recent = meters.as_ref().map_or_else(Vec::new, |m| m.recent.clone());
                let mut mag = [0u16; 26];
                for (m, w) in mag.iter_mut().zip(&words) {
                    *m = w.unsigned_abs();
                }
                recent.push(mag);
                if recent.len() > PEAK_POLLS {
                    recent.remove(0);
                }
                let prev = meters.replace(Meters { words, recent, at: Instant::now() });
                drop(meters);
                // Power mode change (status bits 0 and 1): the DSP restarts, so resend everything.
                if prev.is_some_and(|p| (p.words[26] ^ words[26]) & 0x3 != 0) {
                    eprintln!("ftu-daemon: power mode changed (status {:#06x}), restoring mixer", words[26] as u16);
                    sent.clear();
                }
            }
        }
        let pending = std::mem::take(&mut *self.dirty.lock().unwrap());
        if pending || sent.is_empty() {
            let controls: Vec<Ctl> = self.mixer.lock().unwrap().controls();
            let mut n = 0;
            let mut failed = false;
            for c in controls {
                if sent.get(&c.key()) == Some(&c.data) {
                    continue;
                }
                match dev.set_cur(c.unit, c.control, c.channel, &c.data) {
                    Ok(()) => {
                        sent.insert(c.key(), c.data);
                        n += 1;
                    }
                    Err(e) => {
                        eprintln!("ftu-daemon: unit {} control {} channel {}: {e}", c.unit, c.control, c.channel);
                        failed = true;
                        break;
                    }
                }
            }
            if n > 20 {
                eprintln!("ftu-daemon: wrote {n} mixer controls");
            }
            if failed {
                // Retry on a later pass, without spinning.
                let mut dirty = self.dirty.lock().unwrap();
                *dirty = true;
                let _ = self.wake.wait_timeout(dirty, timeout);
                return;
            }
        }
        let dirty = self.dirty.lock().unwrap();
        if !*dirty {
            let _ = self.wake.wait_timeout(dirty, timeout.min(METER_PERIOD));
        }
    }

    pub fn device_opened(&self, dev: &Device) {
        let mut st = self.status.lock().unwrap();
        st.connected = true;
        st.firmware = dev.firmware_version();
    }

    /// `Some(rate)` while the stream runs, `None` when it stops.
    pub fn set_streaming(&self, rate: Option<u32>) {
        self.status.lock().unwrap().sample_rate = rate;
    }

    /// Called when the device goes away: meters are no longer valid.
    pub fn device_lost(&self) {
        self.meters.lock().unwrap().take();
        *self.status.lock().unwrap() = Status::default();
    }
}

/// XPC entry point (xpc_shim.c). Returns a JSON reply to be freed with `ftu_ctl_free`.
#[no_mangle]
pub extern "C" fn ftu_ctl_request(req: *const c_char) -> *mut c_char {
    let reply = match (CONTROL.get(), req.is_null()) {
        (Some(c), false) => {
            // SAFETY: non-null NUL-terminated string from xpc_dictionary_get_string.
            let s = unsafe { CStr::from_ptr(req) }.to_string_lossy();
            c.handle(&s)
        }
        _ => json!({ "error": "control not ready" }),
    };
    CString::new(reply.to_string()).unwrap_or_default().into_raw()
}

#[no_mangle]
pub extern "C" fn ftu_ctl_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: allocated by CString::into_raw in ftu_ctl_request.
        drop(unsafe { CString::from_raw(s) });
    }
}
