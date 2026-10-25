//! Isochronous streaming engine.
//!
//! Capture (EP 0x81) and playback (EP 0x01) run as rings of async libusb transfers,
//! serviced by one realtime event thread. Playback packet sizes follow capture packet
//! sizes (implicit feedback, as in Linux and proto/ftu_stream.c). Audio moves through
//! the shared-memory rings in `shm`, converted between packed S24_3LE and f32.
//!
//! All transfer callbacks run on the event thread, so `Engine` state needs no locking.
//! The control thread only touches atomics.

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
// Lead control: smoothing per playback transfer (1 ms), and the minimum number of
// transfers between corrections.
const LEAD_ALPHA: f64 = 0.05;
const LEAD_HOLD: u32 = 20;
// Smoothed lead error, in packets, that triggers a one-frame correction, and the error
// that is fixed in one jump instead.
const LEAD_BAND_PACKETS: f64 = 2.0;
const LEAD_JUMP_PACKETS: f64 = 4.0;
// A capture callback gap longer than this may have outlasted the playback queue's slack
// (12 ms), skipping playback microframes and shifting the lead for real. The raw error
// is erratic while the backlog of completions drains, then steady.
const STALL_RESYNC_US: u64 = 10_000;
// Playback transfers after such a stall during which the smoothed error just follows
// the raw error, so one jump fixes the shift instead of a series chasing a lagging
// average. More than OUT_XFERS, since the whole queue may complete in the backlog.
const LEAD_SETTLE: u32 = 24;
// Name of libusb's darwin run-loop thread (libusb/os/darwin_usb.c).
const LIBUSB_EVENT_THREAD: &str = "org.libusb.device-hotplug";
const FIFO_LEN: usize = 4096;
const S24_SCALE: f32 = 8_388_608.0;
// Clock loop for 1 ms updates, 0.5 Hz bandwidth: omega = 2*pi*0.5*0.001,
// b = sqrt(2)*omega, c = omega^2.
const DLL_B: f64 = 0.004443;
const DLL_C: f64 = 0.00000987;

#[derive(Clone, Copy, Debug)]
pub struct StreamConfig {
    pub sample_rate: u32,
    pub clock: ClockSource,
}

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
    rate: u32,

    // Frame counts of completed capture packets, consumed by playback packets.
    fifo: [u16; FIFO_LEN],
    fifo_head: usize,
    fifo_tail: usize,
    nominal_acc: f64,

    cap_pos: u64,
    play_pos: u64,
    // Playback position minus capture position that the stream should hold: the
    // prefilled playback queue. Drift away from it means the device consumes at a
    // different rate than we send, or that fallback packets were used.
    target_lead: i64,
    // Lead error smoothed over about 20 ms, sampled once per playback transfer. The raw
    // error swings by about one transfer either way as capture transfers complete
    // between playback transfers, so it cannot be acted on directly.
    lead_err_avg: f64,
    // Playback transfers since the last correction, and the correction (-1, 0, 1)
    // the next packet applies.
    since_correction: u32,
    correction: i64,
    // Transfers left in the settle period after a stall (LEAD_SETTLE).
    settle: u32,
    // Host time of the previous capture callback, for stall detection.
    last_cb_ticks: u64,
    ns_per_tick: f64,
    // True while the writer is behind the read position, so a stretch of missing
    // packets counts as one underrun.
    starved: bool,

    // Clock smoothing (sample time -> host ticks).
    clock_init: bool,
    anchor_sample: u64,
    anchor_host: f64,
    tpf: f64,

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

    /// Decides once per playback transfer whether to correct the lead. With implicit
    /// feedback the rates match, so the lead only moves after fallback packets or lost
    /// capture packets. Each correction shifts output timing by one frame, so they are
    /// limited to one per LEAD_HOLD transfers, and only when the smoothed error leaves
    /// a band of two packets.
    fn plan_correction(&mut self) {
        if self.cap_pos == 0 {
            return;
        }
        let err = (self.play_pos as i64 - self.cap_pos as i64 - self.target_lead) as f64;
        if self.settle > 0 {
            self.settle -= 1;
            self.lead_err_avg = err;
            self.since_correction = 0;
            return;
        }
        self.lead_err_avg += LEAD_ALPHA * (err - self.lead_err_avg);
        self.since_correction += 1;
        let per_packet = (self.rate / 8000) as f64;
        if self.since_correction < LEAD_HOLD {
            return;
        }
        if self.lead_err_avg.abs() > LEAD_JUMP_PACKETS * per_packet {
            // A disturbance moved the lead by many frames at once, and the output has
            // already jumped. Fix it in one step: skip ahead in the playback ring
            // (zeroing what is skipped) or step back (replaying zeroed frames, a short
            // silence). One glitch instead of one per correction.
            let jump = self.lead_err_avg.round() as i64;
            if jump < 0 {
                for f in 0..(-jump) as u64 {
                    let p = self.shm().playback_frame(self.play_pos + f);
                    // SAFETY: frames ahead of play_pos; the writer is far ahead of them.
                    unsafe { std::ptr::write_bytes(p, 0, CHANNELS) };
                }
            }
            eprintln!(
                "ftu-daemon: {} lead jump {jump} frames (error now {err}, smoothed {:.1})",
                crate::wall_clock(),
                self.lead_err_avg
            );
            self.play_pos = (self.play_pos as i64 - jump) as u64;
            self.lead_err_avg -= jump as f64;
            self.since_correction = 0;
            self.shm().lead_jumps.fetch_add(1, Ordering::Relaxed);
        } else if self.lead_err_avg.abs() > LEAD_BAND_PACKETS * per_packet {
            self.correction = if self.lead_err_avg > 0.0 { -1 } else { 1 };
            self.since_correction = 0;
        }
    }

    /// Frames for the next playback packet: the size of the matching capture packet
    /// (implicit feedback), plus a pending lead correction.
    fn next_frame_count(&mut self) -> usize {
        let mut n = if self.fifo_tail != self.fifo_head {
            let n = self.fifo[self.fifo_tail % FIFO_LEN] as usize;
            self.fifo_tail += 1;
            n
        } else {
            self.stats.fallback_packets += 1;
            self.shm().fallback_packets.fetch_add(1, Ordering::Relaxed);
            self.nominal_acc += self.rate as f64 / 8000.0;
            let n = self.nominal_acc as usize;
            self.nominal_acc -= n as f64;
            n
        };
        match std::mem::take(&mut self.correction) {
            -1 => {
                n = n.saturating_sub(1);
                self.shm().lead_frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
            1 => {
                n += 1;
                self.shm().lead_frames_added.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        n.clamp(1, MAX_FRAMES_PER_PACKET)
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
                let active = write_pos != 0 && write_pos + RING_FRAMES as u64 > self.play_pos;
                let short = active && write_pos < self.play_pos + frames as u64;
                if short && !self.starved {
                    shm.play_underruns.fetch_add(1, Ordering::Relaxed);
                }
                self.starved = short;
                let out = (*t).buffer.add(offset);
                for f in 0..frames {
                    let src = shm.playback_frame(self.play_pos + f as u64);
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
                self.play_pos += frames as u64;
                self.stats.play_frames += frames as u64;
                (*(*t).iso_packet_desc.as_mut_ptr().add(i)).length = (frames * FRAME_BYTES) as u32;
                offset += frames * FRAME_BYTES;
            }
            (*t).length = offset as c_int;
            shm.play_read_pos.store(self.play_pos, Ordering::Release);
        }
    }

    /// Copies completed capture packets into the capture ring and records their sizes.
    fn consume_in(&mut self, t: *mut ffi::libusb_transfer) {
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
                if self.fifo_head - self.fifo_tail < FIFO_LEN {
                    self.fifo[self.fifo_head % FIFO_LEN] = frames as u16;
                    self.fifo_head += 1;
                } else {
                    shm.fifo_overflows.fetch_add(1, Ordering::Relaxed);
                }
                let src = ffi::libusb_get_iso_packet_buffer_simple(t, i as u32);
                for f in 0..frames {
                    let dst = shm.capture_frame(self.cap_pos + f as u64);
                    for c in 0..CHANNELS {
                        let p = src.add(f * FRAME_BYTES + c * 3);
                        let v = ((*p as i32) << 8 | (*p.add(1) as i32) << 16 | (*p.add(2) as i32) << 24) >> 8;
                        *dst.add(c) = v as f32 / S24_SCALE;
                    }
                }
                self.cap_pos += frames as u64;
                self.stats.cap_frames += frames as u64;
            }
            shm.cap_write_pos.store(self.cap_pos, Ordering::Release);
        }
        self.update_clock(rt::now_ticks());
    }

    /// Tracks host time per device frame with a second-order delay-locked loop.
    /// Capture completions arrive every 1 ms with scheduling jitter.
    fn update_clock(&mut self, now: u64) {
        let now = now as f64;
        if !self.clock_init {
            if self.cap_pos == 0 {
                return;
            }
            self.clock_init = true;
            self.anchor_sample = self.cap_pos;
            self.anchor_host = now;
            self.tpf = 1e9 / self.rate as f64 / rt::ns_per_tick();
        } else {
            let frames = (self.cap_pos - self.anchor_sample) as f64;
            if frames <= 0.0 {
                return;
            }
            let predicted = self.anchor_host + frames * self.tpf;
            let err = now - predicted;
            self.anchor_sample = self.cap_pos;
            self.anchor_host = predicted + DLL_B * err;
            self.tpf += DLL_C * err / frames;
        }
        self.shm().publish_clock(ClockAnchor {
            sample_time: self.anchor_sample,
            host_time: self.anchor_host as u64,
            host_ticks_per_frame: self.tpf,
        });
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
    if e.last_cb_ticks != 0 {
        let gap_us = ((now - e.last_cb_ticks) as f64 * e.ns_per_tick / 1000.0) as u64;
        if gap_us > 3000 {
            e.shm().callback_stalls.fetch_add(1, Ordering::Relaxed);
        }
        if gap_us > STALL_RESYNC_US {
            e.settle = LEAD_SETTLE;
        }
        e.shm().max_callback_gap_us.fetch_max(gap_us, Ordering::Relaxed);
    }
    let stall = e.stall_ms.swap(0, Ordering::Relaxed);
    if stall > 0 {
        std::thread::sleep(Duration::from_millis(stall as u64));
    }
    e.last_cb_ticks = now;
    if unsafe { (*t).status } == LIBUSB_TRANSFER_COMPLETED {
        e.consume_in(t);
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
            rate,
            fifo: [0; FIFO_LEN],
            fifo_head: 0,
            fifo_tail: 0,
            nominal_acc: 0.0,
            cap_pos: 0,
            play_pos: 0,
            target_lead: (OUT_XFERS * OUT_PACKETS) as i64 * rate as i64 / 8000,
            starved: false,
            lead_err_avg: 0.0,
            since_correction: 0,
            correction: 0,
            settle: 0,
            last_cb_ticks: 0,
            ns_per_tick: rt::ns_per_tick(),
            clock_init: false,
            anchor_sample: 0,
            anchor_host: 0.0,
            tpf: 0.0,
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
