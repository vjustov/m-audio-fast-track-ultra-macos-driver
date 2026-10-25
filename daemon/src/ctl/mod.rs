//! Control service: the mixer state and device status shared between the XPC control
//! API (`api`), the settings files (`store`) and the service loop, which pushes changes
//! to the device (`sync`).
//!
//! Locks are held only for a copy or an update, never across USB or file I/O except the
//! state save after a change, and never two at once except `status` then `meters` in
//! the status reply.

mod api;
mod store;
mod sync;

pub use store::Store;
pub use sync::DeviceSync;

use crate::mixer::Mixer;
use crate::usb::{ClockSource, Usb};
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

/// Device sample rate from status word bits 8 to 9 (docs/protocol.md).
pub fn status_rate(word: u16) -> u32 {
    [44100, 48000, 88200, 96000][(word >> 8 & 3) as usize]
}

struct Meters {
    words: [i16; 27],
    /// Magnitudes from the last `sync::PEAK_POLLS` polls, newest last.
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
    port: Option<&'static str>,
    clock: Option<ClockSource>, // Some while streaming
}

pub struct Control {
    store: Store,
    mixer: Mutex<Mixer>,
    /// Set when the mixer changed and the device needs the new controls.
    dirty: Mutex<bool>,
    wake: Condvar,
    meters: Mutex<Option<Meters>>,
    status: Mutex<Status>,
    /// Clock source setting, saved in device.json. The service loop picks up changes.
    clock: Mutex<ClockSource>,
    clock_changed: AtomicBool,
    stall_ms: AtomicU32,
    /// Test hook for comparing stream starts; not saved.
    playback_delay_ms: AtomicU32,
}

impl Control {
    /// Loads the saved state from `store`.
    pub fn new(store: Store) -> Self {
        Control {
            mixer: Mutex::new(store.load_mixer()),
            clock: Mutex::new(store.load_clock()),
            store,
            dirty: Mutex::new(true),
            wake: Condvar::new(),
            meters: Mutex::new(None),
            status: Mutex::new(Status::default()),
            clock_changed: AtomicBool::new(false),
            stall_ms: AtomicU32::new(0),
            playback_delay_ms: AtomicU32::new(crate::stream::PLAYBACK_DELAY.as_millis() as u32),
        }
    }

    /// Clock source setting. The stream may still be on the old one (see `take_clock_change`).
    pub fn clock(&self) -> ClockSource {
        *self.clock.lock().unwrap()
    }

    /// True once after the clock setting changed.
    pub fn take_clock_change(&self) -> bool {
        self.clock_changed.swap(false, Ordering::AcqRel)
    }

    /// Stall requested by the test hook, once.
    pub fn take_stall(&self) -> u32 {
        self.stall_ms.swap(0, Ordering::AcqRel)
    }

    /// Playback start delay for the next stream start.
    pub fn playback_delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.playback_delay_ms.load(Ordering::Acquire) as u64)
    }

    /// Latest device status word, from the meter poll.
    pub fn status_word(&self) -> Option<u16> {
        self.meters.lock().unwrap().as_ref().map(|m| m.words[26] as u16)
    }

    pub fn device_opened(&self, dev: &impl Usb) {
        let mut st = self.status.lock().unwrap();
        st.connected = true;
        st.firmware = dev.firmware_version();
        st.port = Some(dev.link_speed());
    }

    /// `Some(rate)` while the stream runs, `None` when it stops.
    pub fn set_streaming(&self, rate: Option<u32>, clock: Option<ClockSource>) {
        let mut st = self.status.lock().unwrap();
        st.sample_rate = rate;
        st.clock = clock;
    }

    /// Called when the device goes away: meters are no longer valid.
    pub fn device_lost(&self) {
        self.meters.lock().unwrap().take();
        *self.status.lock().unwrap() = Status::default();
    }

    /// Saves the new state and wakes the service loop to send it.
    fn changed(&self, m: &Mixer) {
        self.store.save_mixer(m);
        *self.dirty.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

/// XPC entry point (xpc_shim.c). `ctx` is the `Control` passed to `ftu_xpc_listen`.
/// Returns a JSON reply to be freed with `ftu_ctl_free`.
#[no_mangle]
pub extern "C" fn ftu_ctl_request(ctx: *const c_void, req: *const c_char) -> *mut c_char {
    let reply = if ctx.is_null() || req.is_null() {
        serde_json::json!({ "error": "control not ready" })
    } else {
        // SAFETY: ctx is the &'static Control given to ftu_xpc_listen; req is a non-null
        // NUL-terminated string from xpc_dictionary_get_string.
        let (c, s) = unsafe { (&*(ctx as *const Control), CStr::from_ptr(req).to_string_lossy()) };
        c.handle(&s)
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
