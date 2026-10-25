//! Isochronous streaming engine.
//!
//! Capture (EP 0x81) and playback (EP 0x01) run as rings of async libusb transfers,
//! serviced by one realtime event thread. Playback packet sizes follow capture packet
//! sizes (implicit feedback, as in Linux and proto/ftu_stream.c). Audio moves through
//! the shared-memory rings in `shm`, converted between packed S24_3LE and f32.
//!
//! All transfer callbacks run on the event thread, so `Engine` state needs no locking.
//! The control thread only touches atomics.

use crate::pacing::{Clock, Pacer};
use crate::rt;
use crate::shm::{ClockAnchor, Shm, ShmRegion, CHANNELS, RING_FRAMES};
use crate::usb::{ClockSource, Device, UsbError, EP_CAP, EP_PLAY};
use libusb1_sys as ffi;
use libusb1_sys::constants::*;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FRAME_BYTES: usize = CHANNELS * 3;
const MAX_PACKET: usize = 312; // endpoint wMaxPacketSize, 13 frames
const MAX_FRAMES_PER_PACKET: usize = MAX_PACKET / FRAME_BYTES;
// Queue geometry. One packet per 125 us microframe.
// Capture: 24 transfers x 8 packets = 24 ms in flight, completing every 1 ms. The queue
// depth only protects against stalls; latency comes from the 1 ms completion step.
// Playback: 16 transfers x 8 packets = 16 ms in flight. This is the output lead over
// capture, and the plugin's safety offset must cover it (plugin/src/Driver.cpp).
// libusb schedules every submit at least 4 ms ahead, so the queue minus 4 ms is the
// slack for event-thread stalls. 8 ms (4 ms slack) failed with kIOReturnIsoTooOld.
const IN_PACKETS: usize = 8;
const IN_XFERS: usize = 24;
const OUT_PACKETS: usize = 8;
const OUT_XFERS: usize = 16;
// A capture callback gap longer than this may have outlasted the playback queue's slack
// (12 ms), skipping playback microframes and shifting the lead for real. The raw error
// is erratic while the backlog of completions drains, then steady (see Pacer::stalled).
const STALL_RESYNC_US: u64 = 10_000;
// Name of libusb's darwin run-loop thread (libusb/os/darwin_usb.c).
const LIBUSB_EVENT_THREAD: &str = "org.libusb.device-hotplug";
const FIRST_SIZES: usize = 48;
const S24_SCALE: f32 = 8_388_608.0;

#[derive(Clone, Copy, Debug)]
pub struct StreamConfig {
    pub sample_rate: u32,
    pub clock: ClockSource,
    /// Time between submitting capture and playback, so the device runs capture this
    /// long before it receives any playback data. The vendor kext starts playback 10 USB
    /// frames (10 ms) after capture (kInitialInputFramesToSkip, docs/protocol.md).
    pub playback_delay: Duration,
}

/// Playback start delay the daemon uses unless the `playback_delay_ms` test hook
/// changes it.
pub const PLAYBACK_DELAY: Duration = Duration::from_millis(10);

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub cap_frames: u64,
    pub play_frames: u64,
    pub packet_errors: u64,
    pub fallback_packets: u64,
}

struct Engine {
    shm: *const Shm,
    stopping: Arc<AtomicBool>,
    inflight: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
    // Test hook: milliseconds the next capture callback sleeps, to reproduce stalls.
    stall_ms: Arc<AtomicU32>,
    pace: Pacer,
    // Host time of the previous capture callback, for stall detection.
    last_cb_ticks: u64,
    ns_per_tick: f64,
    // True while the writer is behind the read position, so a stretch of missing
    // packets counts as one underrun.
    starved: bool,
    // Frame counts of the first capture packets after start, logged once.
    first_sizes: [u16; FIRST_SIZES],
    first_n: usize,

    // Clock smoothing (sample time -> host ticks).
    clock: Clock,

    stats: Stats,
}

// SAFETY: the engine moves to the event thread once and is used only there. The shm
// pointer targets a region kept alive by the owning Stream.
unsafe impl Send for Engine {}

impl Engine {
    fn shm(&self) -> &Shm {
        // SAFETY: the region outlives the engine (both owned by Stream, engine freed first).
        unsafe { &*self.shm }
    }

    /// Runs the lead controller once per playback transfer and applies a jump to the
    /// playback ring.
    fn plan_correction(&mut self) {
        let Some(jump) = self.pace.plan_correction() else {
            return;
        };
        if jump.frames < 0 {
            for f in 0..(-jump.frames) as u64 {
                let p = self.shm().playback_frame(jump.from + f);
                // SAFETY: frames ahead of play_pos; the writer is far ahead of them.
                unsafe { std::ptr::write_bytes(p, 0, CHANNELS) };
            }
        }
        eprintln!(
            "ftu-daemon: {} lead jump {} frames (error now {}, smoothed {:.1})",
            crate::wall_clock(),
            jump.frames,
            jump.err,
            jump.smoothed
        );
        self.shm().lead_jumps.fetch_add(1, Ordering::Relaxed);
    }

    /// Frames for the next playback packet (implicit feedback plus a pending
    /// correction), counting fallback packets and corrections.
    fn next_frame_count(&mut self) -> usize {
        let p = self.pace.next_packet();
        if p.fallback {
            self.stats.fallback_packets += 1;
            self.shm().fallback_packets.fetch_add(1, Ordering::Relaxed);
        }
        match p.correction {
            -1 => {
                self.shm().lead_frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
            1 => {
                self.shm().lead_frames_added.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        p.frames
    }

    /// Fills a playback transfer from the playback ring and zeroes what it consumed.
    fn prepare_out(&mut self, t: *mut ffi::libusb_transfer) {
        let shm = self.shm;
        // SAFETY: t is an iso transfer with num_iso_packets descriptors and a buffer of
        // MAX_PACKET bytes per packet; frames <= MAX_FRAMES_PER_PACKET.
        unsafe {
            let shm = &*shm;
            let write_pos = shm.play_write_pos.load(Ordering::Acquire);
            self.plan_correction();
            let mut offset = 0usize;
            for i in 0..(*t).num_iso_packets as usize {
                let frames = self.next_frame_count().min(MAX_FRAMES_PER_PACKET);
                // Count each time playback runs dry once, not once per packet. A writer
                // that stops (an app ends playback, coreaudiod restarts) leaves write_pos
                // behind, and per-packet counting turned the following ring's worth of
                // intended silence into 2,731 "underruns" at 48 kHz.
                let active = write_pos != 0 && write_pos + RING_FRAMES as u64 > self.pace.play_pos;
                let short = active && write_pos < self.pace.play_pos + frames as u64;
                if short && !self.starved {
                    shm.play_underruns.fetch_add(1, Ordering::Relaxed);
                }
                self.starved = short;
                let out = (*t).buffer.add(offset);
                for f in 0..frames {
                    let src = shm.playback_frame(self.pace.play_pos + f as u64);
                    for c in 0..CHANNELS {
                        let s = *src.add(c);
                        *src.add(c) = 0.0;
                        let v = (s.clamp(-1.0, 1.0) * (S24_SCALE - 1.0)).round() as i32;
                        let p = out.add(f * FRAME_BYTES + c * 3);
                        *p = v as u8;
                        *p.add(1) = (v >> 8) as u8;
                        *p.add(2) = (v >> 16) as u8;
                    }
                }
                self.pace.played(frames);
                self.stats.play_frames += frames as u64;
                (*(*t).iso_packet_desc.as_mut_ptr().add(i)).length = (frames * FRAME_BYTES) as u32;
                offset += frames * FRAME_BYTES;
            }
            (*t).length = offset as c_int;
            shm.play_read_pos.store(self.pace.play_pos, Ordering::Release);
        }
    }

    /// Copies completed capture packets into the capture ring and records their sizes.
    /// `now` is the callback's start time; `trusted` says whether it can feed the clock.
    fn consume_in(&mut self, t: *mut ffi::libusb_transfer, now: u64, trusted: bool) {
        let shm = self.shm;
        // SAFETY: as in prepare_out; actual_length is bounded by the packet length.
        unsafe {
            let shm = &*shm;
            for i in 0..(*t).num_iso_packets as usize {
                let d = &*(*t).iso_packet_desc.as_ptr().add(i);
                if d.status != LIBUSB_TRANSFER_COMPLETED {
                    self.stats.packet_errors += 1;
                    shm.capture_errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let frames = (d.actual_length as usize / FRAME_BYTES).min(MAX_FRAMES_PER_PACKET);
                if self.first_n < FIRST_SIZES {
                    self.first_sizes[self.first_n] = frames as u16;
                    self.first_n += 1;
                    if self.first_n == FIRST_SIZES {
                        eprintln!("ftu-daemon: {} first capture packets {:?}", crate::wall_clock(), self.first_sizes);
                    }
                }
                // Packets of 7 then 5 frames at 48 kHz are normal while streaming (the
                // device's sample clock against USB frame timing), so packet sizes outside
                // the rate's pattern are not logged: at about 500 a second, writing them
                // from this thread stalled it (2026-10-25).
                let src = ffi::libusb_get_iso_packet_buffer_simple(t, i as u32);
                for f in 0..frames {
                    let dst = shm.capture_frame(self.pace.cap_pos + f as u64);
                    for c in 0..CHANNELS {
                        let p = src.add(f * FRAME_BYTES + c * 3);
                        let v = ((*p as i32) << 8 | (*p.add(1) as i32) << 16 | (*p.add(2) as i32) << 24) >> 8;
                        *dst.add(c) = v as f32 / S24_SCALE;
                    }
                }
                if !self.pace.capture_packet(frames) {
                    shm.fifo_overflows.fetch_add(1, Ordering::Relaxed);
                }
                self.stats.cap_frames += frames as u64;
            }
            shm.cap_write_pos.store(self.pace.cap_pos, Ordering::Release);
        }
        self.update_clock(now, trusted);
    }

    /// Feeds the clock loop and publishes its anchor.
    fn update_clock(&mut self, now: u64, trusted: bool) {
        if self.clock.update(self.pace.cap_pos, now as f64, trusted) {
            self.shm().publish_clock(ClockAnchor {
                sample_time: self.clock.anchor_sample,
                host_time: self.clock.anchor_host as u64,
                host_ticks_per_frame: self.clock.ticks_per_frame,
            });
        }
    }

    fn resubmit(&mut self, t: *mut ffi::libusb_transfer, what: &str) {
        if self.stopping.load(Ordering::Acquire) {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        // SAFETY: t is a completed transfer owned by this engine. A failed submit leaves it
        // idle, so retrying is safe; libusb reschedules it a few frames from now.
        let mut r = unsafe { ffi::libusb_submit_transfer(t) };
        for _ in 0..2 {
            if r >= 0 {
                break;
            }
            self.shm().usb_errors.fetch_add(1, Ordering::Relaxed);
            r = unsafe { ffi::libusb_submit_transfer(t) };
        }
        if r < 0 {
            eprintln!("{what} resubmit failed: {}", UsbError { op: "submit", code: r });
            self.shm().usb_errors.fetch_add(1, Ordering::Relaxed);
            self.failed.store(true, Ordering::Release);
            self.stopping.store(true, Ordering::Release);
            self.inflight.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

extern "system" fn in_cb(t: *mut ffi::libusb_transfer) {
    // SAFETY: user_data is the Engine pointer set at allocation; callbacks run only on
    // the event thread, so this is the only live &mut.
    let e = unsafe { &mut *((*t).user_data as *mut Engine) };
    let now = rt::now_ticks();
    // The first callback has no gap; the clock loop only anchors on it.
    let mut trusted = true;
    if e.last_cb_ticks != 0 {
        let gap_us = ((now - e.last_cb_ticks) as f64 * e.ns_per_tick / 1000.0) as u64;
        trusted = Clock::trusted_gap(Duration::from_micros(gap_us));
        if gap_us > 3000 {
            e.shm().callback_stalls.fetch_add(1, Ordering::Relaxed);
        }
        if gap_us > STALL_RESYNC_US {
            e.pace.stalled();
        }
        e.shm().max_callback_gap_us.fetch_max(gap_us, Ordering::Relaxed);
    }
    let stall = e.stall_ms.swap(0, Ordering::Relaxed);
    if stall > 0 {
        std::thread::sleep(Duration::from_millis(stall as u64));
    }
    e.last_cb_ticks = now;
    if unsafe { (*t).status } == LIBUSB_TRANSFER_COMPLETED {
        e.consume_in(t, now, trusted);
    } else {
        e.stats.packet_errors += 1;
        e.shm().capture_errors.fetch_add(1, Ordering::Relaxed);
    }
    e.resubmit(t, "capture");
}

extern "system" fn out_cb(t: *mut ffi::libusb_transfer) {
    // SAFETY: as in in_cb.
    let e = unsafe { &mut *((*t).user_data as *mut Engine) };
    unsafe {
        if (*t).status != LIBUSB_TRANSFER_COMPLETED {
            e.stats.packet_errors += 1;
            e.shm().playback_errors.fetch_add(1, Ordering::Relaxed);
        }
        for i in 0..(*t).num_iso_packets as usize {
            if (*(*t).iso_packet_desc.as_ptr().add(i)).status != LIBUSB_TRANSFER_COMPLETED {
                e.stats.packet_errors += 1;
                e.shm().playback_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    if !e.stopping.load(Ordering::Acquire) {
        e.prepare_out(t);
    }
    e.resubmit(t, "playback");
}

struct Transfers(Vec<(*mut ffi::libusb_transfer, Vec<u8>)>);
unsafe impl Send for Transfers {}

pub struct Stream {
    stopping: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    stall_ms: Arc<AtomicU32>,
    thread: Option<JoinHandle<(Box<Engine>, Transfers)>>,
    shm: Arc<ShmRegion>,
    last_stats: Stats,
}

/// Cleans up after a failed initial submit. Transfers already submitted still point at
/// `engine` and their buffers, so cancel them and wait until libusb hands them back
/// before freeing anything. `stopping` must be set so callbacks do not resubmit.
unsafe fn abort_start(ctx: *mut ffi::libusb_context, inflight: &AtomicUsize, xfers: Transfers, engine: Box<Engine>) {
    for (t, _) in &xfers.0 {
        ffi::libusb_cancel_transfer(*t);
    }
    let tv = libc::timeval { tv_sec: 0, tv_usec: 50_000 };
    let deadline = Instant::now() + Duration::from_secs(2);
    while inflight.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
        ffi::libusb_handle_events_timeout_completed(ctx, &tv, std::ptr::null_mut());
    }
    if inflight.load(Ordering::Acquire) > 0 {
        // Still owned by libusb: leaking is safer than freeing memory it may write.
        eprintln!("warning: {} transfers did not return after a failed start; leaking them", inflight.load(Ordering::Acquire));
        std::mem::forget(xfers);
        std::mem::forget(engine);
        return;
    }
    for (t, _buf) in xfers.0 {
        ffi::libusb_free_transfer(t);
    }
}

impl Stream {
    /// Claims the streaming interfaces, sets the rate and starts both directions.
    pub fn start(dev: &mut Device, shm: Arc<ShmRegion>, cfg: StreamConfig) -> Result<Self, UsbError> {
        dev.claim_streaming(cfg.clock)?;
        // On S/PDIF clock the device ignores rate requests. The caller passes the rate
        // the device reports in its status word, as the vendor kext does.
        let rate = match cfg.clock {
            ClockSource::Internal => {
                dev.set_sample_rate(cfg.sample_rate)?;
                dev.sample_rate()?
            }
            ClockSource::Spdif => cfg.sample_rate,
        };

        let s = shm.get();
        s.running.store(0, Ordering::Release);
        s.generation.fetch_add(1, Ordering::AcqRel);
        s.sample_rate.store(rate, Ordering::Release);
        s.clock_source.store(cfg.clock as u32, Ordering::Release);
        s.cap_write_pos.store(0, Ordering::Release);
        s.play_read_pos.store(0, Ordering::Release);
        s.play_write_pos.store(0, Ordering::Release);

        let stopping = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let inflight = Arc::new(AtomicUsize::new(0));
        let stall_ms = Arc::new(AtomicU32::new(0));
        let mut engine = Box::new(Engine {
            shm: shm.as_ptr(),
            stopping: stopping.clone(),
            inflight: inflight.clone(),
            failed: failed.clone(),
            stall_ms: stall_ms.clone(),
            pace: Pacer::new(rate, OUT_XFERS * OUT_PACKETS, MAX_FRAMES_PER_PACKET),
            starved: false,
            first_sizes: [0; FIRST_SIZES],
            first_n: 0,
            last_cb_ticks: 0,
            ns_per_tick: rt::ns_per_tick(),
            clock: Clock::new(rate, 1e9 / rate as f64 / rt::ns_per_tick()),
            stats: Stats::default(),
        });

        let handle = dev.handle();
        let ctx = dev.ctx() as usize;
        let ep = engine.as_mut() as *mut Engine;
        let mut xfers = Transfers(Vec::with_capacity(IN_XFERS + OUT_XFERS));
        // SAFETY: transfers are allocated, filled and submitted before the event thread
        // starts; libusb owns them while in flight. Buffers live in `xfers`.
        unsafe {
            let plan = [
                (EP_CAP, in_cb as ffi::libusb_transfer_cb_fn, IN_XFERS, IN_PACKETS),
                (EP_PLAY, out_cb, OUT_XFERS, OUT_PACKETS),
            ];
            for (endpoint, cb, count, packets) in plan {
                if endpoint == EP_PLAY && !cfg.playback_delay.is_zero() {
                    // Capture is in flight (24 ms queued), so a short wait loses nothing;
                    // its completions wait for the event thread.
                    std::thread::sleep(cfg.playback_delay);
                }
                for _ in 0..count {
                    let t = ffi::libusb_alloc_transfer(packets as c_int);
                    let mut buf = vec![0u8; MAX_PACKET * packets];
                    ffi::libusb_fill_iso_transfer(
                        t,
                        handle,
                        endpoint,
                        buf.as_mut_ptr(),
                        buf.len() as c_int,
                        packets as c_int,
                        cb,
                        ep as *mut std::ffi::c_void,
                        1000,
                    );
                    if endpoint == EP_CAP {
                        ffi::libusb_set_iso_packet_lengths(t, MAX_PACKET as u32);
                    } else {
                        (*ep).prepare_out(t);
                    }
                    let r = ffi::libusb_submit_transfer(t);
                    if r < 0 {
                        stopping.store(true, Ordering::Release);
                        ffi::libusb_free_transfer(t);
                        abort_start(ctx as *mut ffi::libusb_context, &inflight, xfers, engine);
                        dev.release_streaming();
                        return Err(UsbError { op: "initial submit", code: r });
                    }
                    inflight.fetch_add(1, Ordering::AcqRel);
                    xfers.0.push((t, buf));
                }
            }
        }
        s.running.store(1, Ordering::Release);

        // On macOS libusb receives every completion on its own run-loop thread first and
        // only then wakes ours, so a preempted run-loop thread stalls the stream as much
        // as a preempted ftu-usb thread. It runs at normal priority unless raised here.
        // It is created with the libusb context, so this repeats for each start.
        if rt::make_named_realtime(LIBUSB_EVENT_THREAD, 1_000_000, 300_000) == 0 {
            eprintln!("warning: could not make {LIBUSB_EVENT_THREAD} realtime");
        }

        let thread_stopping = stopping.clone();
        let thread = std::thread::Builder::new()
            .name("ftu-usb".into())
            .spawn(move || {
                // Transfers complete every 1 ms; the work per completion is small.
                if !rt::make_realtime(1_000_000, 300_000) {
                    eprintln!("warning: realtime scheduling refused");
                }
                let ctx = ctx as *mut ffi::libusb_context;
                let tv = libc::timeval { tv_sec: 0, tv_usec: 50_000 };
                while !thread_stopping.load(Ordering::Acquire) {
                    // SAFETY: ctx valid while the Device lives; Stream::stop joins first.
                    unsafe { ffi::libusb_handle_events_timeout_completed(ctx, &tv, std::ptr::null_mut()) };
                }
                // Drain: callbacks stop resubmitting once `stopping` is set.
                let deadline = Instant::now() + Duration::from_secs(2);
                while inflight.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
                    unsafe { ffi::libusb_handle_events_timeout_completed(ctx, &tv, std::ptr::null_mut()) };
                }
                (engine, xfers)
            })
            .expect("spawn event thread");

        Ok(Self { stopping, failed, stall_ms, thread: Some(thread), shm, last_stats: Stats::default() })
    }

    /// Test hook: makes the event thread sleep `ms` in its next capture callback.
    pub fn inject_stall(&self, ms: u32) {
        self.stall_ms.store(ms, Ordering::Relaxed);
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn shm(&self) -> &Shm {
        self.shm.get()
    }

    /// Stops both directions, frees the transfers and releases the interfaces.
    pub fn stop(mut self, dev: &mut Device) -> Stats {
        self.shutdown();
        dev.release_streaming();
        self.last_stats
    }

    fn shutdown(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.shm.get().running.store(0, Ordering::Release);
        if let Some(t) = self.thread.take() {
            if let Ok((engine, xfers)) = t.join() {
                self.last_stats = engine.stats;
                for (t, _buf) in xfers.0 {
                    // SAFETY: no longer in flight after the drain.
                    unsafe { ffi::libusb_free_transfer(t) };
                }
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.shutdown();
    }
}
