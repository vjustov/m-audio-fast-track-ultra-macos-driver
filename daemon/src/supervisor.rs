//! The service loop: holds the shared memory, serves XPC and keeps the device streaming
//! through rate changes, clock changes, stream failures and replugs.
//!
//! The decisions (when a running stream must stop, and what the next start uses) live
//! in `Watch` and `after_stop`, free of I/O, so they can be tested.

use crate::ctl::{self, Control, DeviceSync, Store};
use crate::shm::{Shm, ShmRegion};
use crate::stream::{Stream, StreamConfig};
use crate::usb::{ClockSource, Device};
use crate::{wall_clock, SERVICE_NAME};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_RATE: u32 = 48000;
// The status word can lag a restart, so on S/PDIF clock a rate mismatch must last this
// long before the stream follows the source.
const SPDIF_FOLLOW_DELAY: Duration = Duration::from_millis(500);
// Service pass length while streaming: how often requests and the status are checked.
const SERVICE_PERIOD: Duration = Duration::from_millis(50);

extern "C" {
    fn ftu_xpc_listen(name: *const libc::c_char, addr: *mut libc::c_void, len: usize, ctl: *const libc::c_void) -> libc::c_int;
    fn ftu_xpc_take_requested_rate() -> u32;
}

/// Why a running stream stops.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stop {
    /// Restart at this rate: a client asked for it, or the S/PDIF source changed.
    Rate(u32),
    /// The clock source setting changed.
    Clock,
    /// The stream failed (USB error, replug, power cycle).
    Failed,
}

/// Decides once per service pass whether the running stream must stop.
pub struct Watch {
    clock: ClockSource,
    // The rate the service asked for, which requests are compared with.
    rate: u32,
    // The rate the stream actually runs at (on S/PDIF clock, the source's).
    streaming_rate: u32,
    // On S/PDIF clock: when the device started reporting another rate.
    mismatch_since: Option<Instant>,
}

impl Watch {
    pub fn new(clock: ClockSource, rate: u32, streaming_rate: u32) -> Self {
        Self { clock, rate, streaming_rate, mismatch_since: None }
    }

    /// `requested` is a rate asked for over XPC since the last pass (0 for none), and
    /// `status_rate` the rate in the device status word, if read.
    pub fn check(&mut self, failed: bool, requested: u32, clock_change: bool, status_rate: Option<u32>, now: Instant) -> Option<Stop> {
        if failed {
            return Some(Stop::Failed);
        }
        // On S/PDIF clock the source sets the rate, so requests are dropped.
        if requested != 0 && requested != self.rate && self.clock == ClockSource::Internal {
            return Some(Stop::Rate(requested));
        }
        if clock_change {
            return Some(Stop::Clock);
        }
        if self.clock == ClockSource::Spdif {
            match status_rate {
                Some(r) if r != self.streaming_rate => {
                    let since = *self.mismatch_since.get_or_insert(now);
                    if now.duration_since(since) >= SPDIF_FOLLOW_DELAY {
                        return Some(Stop::Rate(r));
                    }
                }
                _ => self.mismatch_since = None,
            }
        }
        None
    }
}

/// What the next start uses after `stop`: the rate, and whether the mixer must be
/// resent because the device may have reset.
pub fn after_stop(stop: Stop, rate: u32, streaming_rate: u32) -> (u32, bool) {
    match stop {
        // Keep the current rate, so leaving S/PDIF does not jump.
        Stop::Clock => (streaming_rate, false),
        Stop::Rate(r) => (r, false),
        // A replug or power cycle also stops the stream.
        Stop::Failed => (rate, true),
    }
}

/// Runs forever: holds the shared memory, serves XPC, and keeps the device streaming.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let shm = Arc::new(ShmRegion::new()?);
    // Shared with the XPC handlers for the life of the process.
    let control: &'static Control = Box::leak(Box::new(Control::new(Store::user())));
    let name = std::ffi::CString::new(SERVICE_NAME)?;
    let ctx = control as *const Control as *const libc::c_void;
    // SAFETY: the region and the control live until process exit.
    if unsafe { ftu_xpc_listen(name.as_ptr(), shm.as_ptr() as *mut libc::c_void, shm.len(), ctx) } != 0 {
        return Err("XPC listener failed (is the service registered with launchd?)".into());
    }
    eprintln!("ftu-daemon: serving {SERVICE_NAME}, shared memory {} bytes", shm.len());

    let mut rate = DEFAULT_RATE;
    let mut sync = DeviceSync::default();
    loop {
        let Ok(mut dev) = Device::open() else {
            control.device_lost();
            sync.clear();
            // Keep honouring rate requests while the device is away.
            // SAFETY: plain C function, no arguments.
            let r = unsafe { ftu_xpc_take_requested_rate() };
            if r != 0 {
                rate = r;
            }
            std::thread::sleep(Duration::from_secs(2));
            continue;
        };
        control.device_opened(&dev);
        // Restore the mixer before audio starts.
        sync.service(control, &dev, Duration::ZERO);
        control.take_clock_change();
        let clock = if dev.link_speed() == "USB 2.0" { control.clock() } else { ClockSource::Internal };
        let mut cfg = StreamConfig { sample_rate: rate, clock };
        if clock == ClockSource::Spdif {
            // Select alternate setting 2 first, so the status word reports the S/PDIF rate.
            if let Err(e) = dev.claim_streaming(clock) {
                eprintln!("ftu-daemon: selecting S/PDIF clock failed: {e}");
            }
            std::thread::sleep(Duration::from_millis(100));
            cfg.sample_rate = device_rate(&dev).unwrap_or(rate);
        }
        let stream = match Stream::start(&mut dev, shm.clone(), cfg) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ftu-daemon: start failed: {e}");
                sync.clear();
                drop(dev);
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let streaming_rate = stream.shm().sample_rate.load(Ordering::Acquire);
        eprintln!("ftu-daemon: streaming at {streaming_rate} Hz, clock {}", clock.name());
        control.set_streaming(Some(streaming_rate), Some(clock));

        let mut watch = Watch::new(clock, rate, streaming_rate);
        let mut log = CounterLog::new();
        let stop = loop {
            log.maybe_log(stream.shm());
            // SAFETY: as above.
            let requested = unsafe { ftu_xpc_take_requested_rate() };
            let stall = control.take_stall();
            if stall > 0 {
                stream.inject_stall(stall);
            }
            let status_rate = control.status_word().map(ctl::status_rate);
            if let Some(stop) = watch.check(stream.failed(), requested, control.take_clock_change(), status_rate, Instant::now()) {
                break stop;
            }
            sync.service(control, &dev, SERVICE_PERIOD);
        };

        control.set_streaming(None, None);
        let stats = stream.stop(&mut dev);
        match stop {
            Stop::Clock => eprintln!("ftu-daemon: switching to {} clock", control.clock().name()),
            Stop::Rate(r) => eprintln!("ftu-daemon: switching to {r} Hz"),
            Stop::Failed => eprintln!("ftu-daemon: stream stopped ({stats:?}), restarting"),
        }
        let clear;
        (rate, clear) = after_stop(stop, rate, streaming_rate);
        if clear {
            sync.clear();
        }
        // Restart right away. The handle is dropped and the device reopened, which also
        // notices a replug.
    }
}

/// Rate from the device status word. The meter read returns the previous snapshot, so
/// read twice.
fn device_rate(dev: &Device) -> Option<u32> {
    dev.read_meters_raw().ok()?;
    dev.read_meters_raw().ok().map(|w| ctl::status_rate(w[26] as u16))
}

/// Logs the stream counters when any changes, at most once a second, with the
/// wall-clock time so events can be matched with a client's log.
struct CounterLog {
    last: Instant,
    reported: [u64; 15],
}

impl CounterLog {
    fn new() -> Self {
        Self { last: Instant::now(), reported: [0; 15] }
    }

    fn maybe_log(&mut self, s: &Shm) {
        if self.last.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last = Instant::now();
        let mut now = [
            &s.play_underruns,
            &s.usb_errors,
            &s.plugin_late_writes,
            &s.plugin_short_reads,
            &s.plugin_input_jumps,
            &s.plugin_output_jumps,
            &s.lead_frames_added,
            &s.lead_frames_dropped,
            &s.lead_jumps,
            &s.fallback_packets,
            &s.capture_errors,
            &s.playback_errors,
            &s.fifo_overflows,
            &s.callback_stalls,
            &s.max_callback_gap_us,
        ]
        .map(|c| c.load(Ordering::Relaxed));
        // The max gap is since the previous line, so it does not trigger one.
        if now[..14] == self.reported[..14] {
            return;
        }
        now[14] = s.max_callback_gap_us.swap(0, Ordering::Relaxed);
        eprintln!(
            "ftu-daemon: {} playback underruns {}, usb errors {}, plugin late writes {}, short reads {}, input jumps {}, output jumps {}, lead frames added {}, dropped {}, lead jumps {}, fallback packets {}, capture errors {}, playback errors {}, fifo overflows {}, stalls {}, max gap {} us, lead {} frames",
            wall_clock(),
            now[0],
            now[1],
            now[2],
            now[3],
            now[4],
            now[5],
            now[6],
            now[7],
            now[8],
            now[9],
            now[10],
            now[11],
            now[12],
            now[13],
            now[14],
            s.play_read_pos.load(Ordering::Relaxed) as i64 - s.cap_write_pos.load(Ordering::Relaxed) as i64
        );
        self.reported = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Instant {
        // A fixed origin keeps the tests independent of when they run.
        static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *ORIGIN.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    #[test]
    fn a_failed_stream_stops_first() {
        let mut w = Watch::new(ClockSource::Internal, 48000, 48000);
        assert_eq!(w.check(true, 44100, true, None, at(0)), Some(Stop::Failed));
    }

    #[test]
    fn rate_requests_restart_on_internal_clock_only() {
        let mut w = Watch::new(ClockSource::Internal, 48000, 48000);
        assert_eq!(w.check(false, 0, false, None, at(0)), None);
        assert_eq!(w.check(false, 48000, false, None, at(0)), None);
        assert_eq!(w.check(false, 96000, false, None, at(0)), Some(Stop::Rate(96000)));

        let mut w = Watch::new(ClockSource::Spdif, 48000, 48000);
        assert_eq!(w.check(false, 96000, false, Some(48000), at(0)), None);
    }

    #[test]
    fn a_rate_request_wins_over_a_clock_change() {
        let mut w = Watch::new(ClockSource::Internal, 48000, 48000);
        assert_eq!(w.check(false, 44100, true, None, at(0)), Some(Stop::Rate(44100)));
        assert_eq!(w.check(false, 0, true, None, at(0)), Some(Stop::Clock));
    }

    #[test]
    fn spdif_follows_the_source_after_500_ms() {
        let mut w = Watch::new(ClockSource::Spdif, 48000, 48000);
        assert_eq!(w.check(false, 0, false, Some(44100), at(0)), None);
        assert_eq!(w.check(false, 0, false, Some(44100), at(499)), None);
        assert_eq!(w.check(false, 0, false, Some(44100), at(500)), Some(Stop::Rate(44100)));
    }

    #[test]
    fn a_brief_spdif_mismatch_is_ignored() {
        let mut w = Watch::new(ClockSource::Spdif, 48000, 48000);
        assert_eq!(w.check(false, 0, false, Some(44100), at(0)), None);
        assert_eq!(w.check(false, 0, false, Some(48000), at(300)), None);
        assert_eq!(w.check(false, 0, false, Some(44100), at(600)), None);
        assert_eq!(w.check(false, 0, false, None, at(900)), None);
        assert_eq!(w.check(false, 0, false, Some(44100), at(1000)), None);
        assert_eq!(w.check(false, 0, false, Some(44100), at(1500)), Some(Stop::Rate(44100)));
    }

    #[test]
    fn internal_clock_ignores_the_status_rate() {
        let mut w = Watch::new(ClockSource::Internal, 48000, 48000);
        assert_eq!(w.check(false, 0, false, Some(96000), at(0)), None);
        assert_eq!(w.check(false, 0, false, Some(96000), at(5000)), None);
    }

    #[test]
    fn next_start_after_each_stop() {
        // Leaving S/PDIF keeps the source's rate.
        assert_eq!(after_stop(Stop::Clock, 48000, 44100), (44100, false));
        assert_eq!(after_stop(Stop::Rate(96000), 48000, 48000), (96000, false));
        // A failure may mean the device reset, so the mixer is resent.
        assert_eq!(after_stop(Stop::Failed, 48000, 48000), (48000, true));
    }
}
