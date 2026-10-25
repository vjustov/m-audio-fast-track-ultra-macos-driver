//! The service loop: holds the shared memory, serves XPC and keeps the device streaming
//! through rate changes, clock changes, stream failures, replugs and system sleep.
//!
//! The decisions (when a running stream must stop, and what the next start uses) live
//! in `Watch` and `after_stop`, free of I/O. The loop itself (`serve`) runs against a
//! `Backend`: `System` in the daemon, a scripted fake device in the tests.

use crate::ctl::{self, Control, DeviceSync, Store};
use crate::shm::{Counters, Shm, ShmRegion};
use crate::stream::{Stats, Stream, StreamConfig};
use crate::usb::{ClockSource, Device, Usb, UsbError};
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
    fn ftu_xpc_presence(present: libc::c_int);
    fn ftu_power_start() -> libc::c_int;
    fn ftu_power_sleep_pending() -> libc::c_int;
    fn ftu_power_released();
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
    /// The system is going to sleep: release the device until it wakes.
    Sleep,
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
        // A replug or power cycle also stops the stream, and the device may lose its
        // mixer state over sleep.
        Stop::Failed | Stop::Sleep => (rate, true),
    }
}

/// A running stream, as the service loop sees it.
pub trait Running {
    type Dev;
    /// The rate the stream actually runs at.
    fn rate(&self) -> u32;
    fn failed(&self) -> bool;
    fn inject_stall(&self, ms: u32);
    fn log_counters(&self, log: &mut CounterLog);
    fn stop(self, dev: &mut Self::Dev) -> Stats;
}

/// What the service loop needs from the device and the process around it.
pub trait Backend {
    type Dev: Usb;
    type Stream: Running<Dev = Self::Dev>;
    /// The device, or None while it is unplugged.
    fn open(&mut self) -> Option<Self::Dev>;
    fn start(&mut self, dev: &mut Self::Dev, cfg: StreamConfig) -> Result<Self::Stream, UsbError>;
    /// A rate asked for over XPC since the last call, or 0.
    fn requested_rate(&mut self) -> u32;
    /// The system announced sleep and waits for the device to be released.
    fn sleep_pending(&self) -> bool;
    /// The device is closed: sleep may go ahead.
    fn released_for_sleep(&mut self);
    /// Publishes whether the device is plugged in.
    fn set_present(&mut self, present: bool);
    /// Waits `d`. Returns false to end the loop (tests only).
    fn pause(&mut self, d: Duration) -> bool;
    /// How long a service pass waits for a mixer change while streaming.
    fn service_period(&self) -> Duration {
        SERVICE_PERIOD
    }
}

/// The daemon's backend: libusb, the XPC listener and IOKit sleep notifications.
struct System {
    shm: Arc<ShmRegion>,
}

impl Running for Stream {
    type Dev = Device;
    fn rate(&self) -> u32 {
        self.shm().sample_rate.load(Ordering::Acquire)
    }
    fn failed(&self) -> bool {
        Stream::failed(self)
    }
    fn inject_stall(&self, ms: u32) {
        Stream::inject_stall(self, ms)
    }
    fn log_counters(&self, log: &mut CounterLog) {
        log.maybe_log(self.shm())
    }
    fn stop(self, dev: &mut Device) -> Stats {
        Stream::stop(self, dev)
    }
}

impl Backend for System {
    type Dev = Device;
    type Stream = Stream;
    fn open(&mut self) -> Option<Device> {
        Device::open().ok()
    }
    fn start(&mut self, dev: &mut Device, cfg: StreamConfig) -> Result<Stream, UsbError> {
        Stream::start(dev, self.shm.clone(), cfg)
    }
    fn requested_rate(&mut self) -> u32 {
        // SAFETY: plain C function, no arguments.
        unsafe { ftu_xpc_take_requested_rate() }
    }
    fn sleep_pending(&self) -> bool {
        // SAFETY: plain C function, no arguments.
        unsafe { ftu_power_sleep_pending() != 0 }
    }
    fn released_for_sleep(&mut self) {
        // SAFETY: plain C function, no arguments.
        unsafe { ftu_power_released() }
    }
    fn set_present(&mut self, present: bool) {
        self.shm.get().device_present.store(present as u32, Ordering::Release);
        // SAFETY: plain C function; the shim copies the value.
        unsafe { ftu_xpc_presence(present as libc::c_int) }
    }
    fn pause(&mut self, d: Duration) -> bool {
        std::thread::sleep(d);
        true
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
    // SAFETY: plain C function, no arguments.
    if unsafe { ftu_power_start() } != 0 {
        eprintln!("ftu-daemon: sleep notifications unavailable; the stream runs into sleep");
    }
    serve(control, &mut System { shm });
    Ok(())
}

/// Waits until the system has woken. Returns false if the backend ends the loop.
fn wait_for_wake(b: &mut impl Backend) -> bool {
    eprintln!("ftu-daemon: device released for sleep");
    b.released_for_sleep();
    while b.sleep_pending() {
        if !b.pause(Duration::from_millis(100)) {
            return false;
        }
    }
    eprintln!("ftu-daemon: awake, reopening the device");
    true
}

/// The service loop. Returns only when the backend's `pause` says so.
pub fn serve<B: Backend>(control: &Control, b: &mut B) {
    let mut rate = DEFAULT_RATE;
    let mut sync = DeviceSync::default();
    let mut present = None;
    loop {
        if b.sleep_pending() {
            if !wait_for_wake(b) {
                return;
            }
            sync.clear();
        }
        let Some(mut dev) = b.open() else {
            if present != Some(false) {
                present = Some(false);
                b.set_present(false);
                eprintln!("ftu-daemon: device not found, waiting for it");
            }
            control.device_lost();
            sync.clear();
            // Keep honouring rate requests while the device is away.
            let r = b.requested_rate();
            if r != 0 {
                rate = r;
            }
            if !b.pause(Duration::from_secs(2)) {
                return;
            }
            continue;
        };
        if present != Some(true) {
            present = Some(true);
            b.set_present(true);
        }
        control.device_opened(&dev);
        // Restore the mixer before audio starts.
        sync.service(control, &dev, Duration::ZERO);
        control.take_clock_change();
        let clock = if dev.link_speed() == "USB 2.0" { control.clock() } else { ClockSource::Internal };
        let mut cfg = StreamConfig { sample_rate: rate, clock, playback_delay: control.playback_delay() };
        if clock == ClockSource::Spdif {
            // Select alternate setting 2 first, so the status word reports the S/PDIF rate.
            if let Err(e) = dev.claim_streaming(clock) {
                eprintln!("ftu-daemon: selecting S/PDIF clock failed: {e}");
            }
            if !b.pause(Duration::from_millis(100)) {
                return;
            }
            cfg.sample_rate = device_rate(&dev).unwrap_or(rate);
        }
        let stream = match b.start(&mut dev, cfg) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ftu-daemon: start failed: {e}");
                sync.clear();
                drop(dev);
                if !b.pause(Duration::from_secs(1)) {
                    return;
                }
                continue;
            }
        };
        let streaming_rate = stream.rate();
        eprintln!("ftu-daemon: streaming at {streaming_rate} Hz, clock {}", clock.name());
        control.set_streaming(Some(streaming_rate), Some(clock));

        let mut watch = Watch::new(clock, rate, streaming_rate);
        let mut log = CounterLog::new();
        let stop = loop {
            stream.log_counters(&mut log);
            if b.sleep_pending() {
                break Stop::Sleep;
            }
            let requested = b.requested_rate();
            let stall = control.take_stall();
            if stall > 0 {
                stream.inject_stall(stall);
            }
            let status_rate = control.status_word().map(ctl::status_rate);
            if let Some(stop) = watch.check(stream.failed(), requested, control.take_clock_change(), status_rate, Instant::now()) {
                break stop;
            }
            sync.service(control, &dev, b.service_period());
        };

        control.set_streaming(None, None);
        let stats = stream.stop(&mut dev);
        match stop {
            Stop::Clock => eprintln!("ftu-daemon: switching to {} clock", control.clock().name()),
            Stop::Rate(r) => eprintln!("ftu-daemon: switching to {r} Hz"),
            Stop::Failed => eprintln!("ftu-daemon: stream stopped ({stats:?}), restarting"),
            Stop::Sleep => eprintln!("ftu-daemon: system going to sleep"),
        }
        let clear;
        (rate, clear) = after_stop(stop, rate, streaming_rate);
        if clear {
            sync.clear();
        }
        // Restart right away. The handle is dropped and the device reopened, which also
        // notices a replug. Before sleep the top of the loop waits for the wake.
        drop(dev);
    }
}

/// Rate from the device status word. The meter read returns the previous snapshot, so
/// read twice.
fn device_rate(dev: &impl Usb) -> Option<u32> {
    dev.read_meters_raw().ok()?;
    dev.read_meters_raw().ok().map(|w| ctl::status_rate(w[26] as u16))
}

/// Logs the stream counters when any changes, at most once a second, with the
/// wall-clock time so events can be matched with a client's log.
pub struct CounterLog {
    last: Instant,
    reported: Counters,
}

impl CounterLog {
    fn new() -> Self {
        Self { last: Instant::now(), reported: Counters::default() }
    }

    fn maybe_log(&mut self, s: &Shm) {
        if self.last.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last = Instant::now();
        let now = s.counters();
        if now == self.reported {
            return;
        }
        // The max gap is since the previous line, so it does not trigger one.
        let max_gap = s.max_callback_gap_us.swap(0, Ordering::Relaxed);
        let lead = s.play_read_pos.load(Ordering::Relaxed) as i64 - s.cap_write_pos.load(Ordering::Relaxed) as i64;
        eprintln!("ftu-daemon: {} {now}, max gap {max_gap} us, lead {lead} frames", wall_clock());
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

    // A scripted device and process for `serve`. Time advances one tick per service
    // pass, per pause and per open attempt; script commands run at their tick.
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Copy, Debug)]
    enum Cmd {
        Unplug,
        Plug,
        Rate(u32),
        Sleep,
        Wake,
        End,
    }

    #[derive(Default)]
    struct World {
        tick: u32,
        script: Vec<(u32, Cmd)>,
        plugged: bool,
        stream_ok: bool,
        sleep_pending: bool,
        requested: u32,
        end: bool,
        set_curs: u32,
        open_devices: i32,
        log: Vec<String>,
    }

    type W = Rc<RefCell<World>>;

    fn advance(w: &W) {
        let mut x = w.borrow_mut();
        x.tick += 1;
        let t = x.tick;
        let due: Vec<Cmd> = x.script.iter().filter(|&&(at, _)| at == t).map(|&(_, c)| c).collect();
        for c in due {
            match c {
                Cmd::Unplug => {
                    x.plugged = false;
                    x.stream_ok = false;
                }
                Cmd::Plug => x.plugged = true,
                Cmd::Rate(r) => x.requested = r,
                Cmd::Sleep => x.sleep_pending = true,
                Cmd::Wake => x.sleep_pending = false,
                Cmd::End => x.end = true,
            }
            x.log.push(format!("{c:?}"));
        }
        // A script without End must not hang the test.
        if t > 10_000 {
            x.end = true;
            x.stream_ok = false;
        }
    }

    struct FakeDev(W);

    impl Usb for FakeDev {
        fn link_speed(&self) -> &'static str {
            "USB 2.0"
        }
        fn firmware_version(&self) -> Option<String> {
            Some("1.51".into())
        }
        fn claim_streaming(&mut self, _: ClockSource) -> Result<(), UsbError> {
            Ok(())
        }
        fn read_meters_raw(&self) -> Result<[i16; 27], UsbError> {
            Ok([0; 27])
        }
        fn set_cur(&self, _: u8, _: u8, _: u8, _: &[u8]) -> Result<(), UsbError> {
            self.0.borrow_mut().set_curs += 1;
            Ok(())
        }
    }

    impl Drop for FakeDev {
        fn drop(&mut self) {
            self.0.borrow_mut().open_devices -= 1;
        }
    }

    struct FakeStream(W, u32);

    impl Running for FakeStream {
        type Dev = FakeDev;
        fn rate(&self) -> u32 {
            self.1
        }
        fn failed(&self) -> bool {
            !self.0.borrow().stream_ok
        }
        fn inject_stall(&self, _: u32) {}
        fn log_counters(&self, _: &mut CounterLog) {}
        fn stop(self, _: &mut FakeDev) -> Stats {
            self.0.borrow_mut().log.push("stop".into());
            Stats::default()
        }
    }

    struct Fake(W);

    impl Backend for Fake {
        type Dev = FakeDev;
        type Stream = FakeStream;
        fn open(&mut self) -> Option<FakeDev> {
            advance(&self.0);
            let mut x = self.0.borrow_mut();
            if !x.plugged || x.end {
                return None;
            }
            x.open_devices += 1;
            Some(FakeDev(self.0.clone()))
        }
        fn start(&mut self, _: &mut FakeDev, cfg: StreamConfig) -> Result<FakeStream, UsbError> {
            let mut x = self.0.borrow_mut();
            x.stream_ok = true;
            let line = format!("start {} after {} sets", cfg.sample_rate, x.set_curs);
            x.log.push(line);
            Ok(FakeStream(self.0.clone(), cfg.sample_rate))
        }
        fn requested_rate(&mut self) -> u32 {
            advance(&self.0);
            std::mem::take(&mut self.0.borrow_mut().requested)
        }
        fn sleep_pending(&self) -> bool {
            self.0.borrow().sleep_pending
        }
        fn released_for_sleep(&mut self) {
            let mut x = self.0.borrow_mut();
            let line = format!("released with {} open", x.open_devices);
            x.log.push(line);
        }
        fn set_present(&mut self, present: bool) {
            let mut x = self.0.borrow_mut();
            if !x.end {
                x.log.push(format!("present {present}"));
            }
        }
        fn pause(&mut self, _: Duration) -> bool {
            advance(&self.0);
            !self.0.borrow().end
        }
        fn service_period(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// Runs `serve` on a plugged-in fake until the script ends. Returns the log up to End.
    fn run_script(script: &[(u32, Cmd)]) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!("ftu-serve-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        let control = Control::new(Store::at(&dir));
        let w: W = Rc::new(RefCell::new(World { plugged: true, script: script.to_vec(), ..Default::default() }));
        serve(&control, &mut Fake(w.clone()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = w.borrow().log.clone();
        log.into_iter().take_while(|l| l != "End").collect()
    }

    fn sets_at(line: &str) -> u32 {
        line.split(' ').nth(3).unwrap().parse().unwrap()
    }

    #[test]
    fn a_replug_restarts_the_stream_and_resends_the_mixer() {
        let log = run_script(&[(5, Cmd::Unplug), (20, Cmd::Plug), (40, Cmd::End)]);
        let starts: Vec<&String> = log.iter().filter(|l| l.starts_with("start")).collect();
        assert_eq!(starts.len(), 2, "{log:?}");
        assert!(sets_at(starts[1]) > sets_at(starts[0]), "mixer not resent: {log:?}");
        // Presence changes once each way, however many times the open is retried.
        let presence: Vec<&String> = log.iter().filter(|l| l.starts_with("present")).collect();
        assert_eq!(presence, ["present true", "present false", "present true"], "{log:?}");
    }

    #[test]
    fn sleep_releases_the_device_and_wake_resumes() {
        let log = run_script(&[(5, Cmd::Sleep), (15, Cmd::Wake), (30, Cmd::End)]);
        let i = log.iter().position(|l| l.starts_with("released")).expect("not released");
        assert_eq!(log[i], "released with 0 open", "{log:?}");
        assert_eq!(log[i - 1], "stop", "{log:?}");
        assert_eq!(log[i + 1], "Wake", "nothing may happen until wake: {log:?}");
        let starts: Vec<&String> = log.iter().filter(|l| l.starts_with("start")).collect();
        assert_eq!(starts.len(), 2, "{log:?}");
        assert!(sets_at(starts[1]) > sets_at(starts[0]), "mixer not resent after wake: {log:?}");
        // Sleep is not an unplug: clients keep the device.
        assert_eq!(log.iter().filter(|l| l.starts_with("present")).count(), 1, "{log:?}");
    }

    #[test]
    fn sleep_while_unplugged_is_allowed_at_once() {
        let log = run_script(&[(2, Cmd::Unplug), (6, Cmd::Sleep), (12, Cmd::Wake), (14, Cmd::Plug), (30, Cmd::End)]);
        let sleep = log.iter().position(|l| l == "Sleep").unwrap();
        assert!(log[sleep + 1].starts_with("released"), "{log:?}");
        assert_eq!(log.iter().filter(|l| l.starts_with("start")).count(), 2, "{log:?}");
    }

    #[test]
    fn a_rate_request_restarts_without_resending_the_mixer() {
        let log = run_script(&[(5, Cmd::Rate(96000)), (20, Cmd::End)]);
        let starts: Vec<&String> = log.iter().filter(|l| l.starts_with("start")).collect();
        assert_eq!(starts.len(), 2, "{log:?}");
        assert!(starts[0].starts_with("start 48000") && starts[1].starts_with("start 96000"), "{log:?}");
        assert_eq!(sets_at(starts[0]), sets_at(starts[1]), "{log:?}");
    }
}
