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
use crate::shm::{ClockAnchor, Shm, ShmRegion, CHANNELS};
use crate::usb::{ClockSource, Device, UsbError, EP_CAP, EP_PLAY};
use libusb1_sys as ffi;
use libusb1_sys::constants::*;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FRAME_BYTES: usize = CHANNELS * 3;
const MAX_PACKET: usize = 312; // endpoint wMaxPacketSize, 13 frames
const MAX_FRAMES_PER_PACKET: usize = MAX_PACKET / FRAME_BYTES;
// 12 transfers x 32 microframes = 48 ms queued per direction. 8 ms was not enough
// (protocol.md, streaming prototype).
const PACKETS_PER_XFER: usize = 32;
const NUM_XFERS: usize = 12;
const FIFO_LEN: usize = 4096;
const S24_SCALE: f32 = 8_388_608.0;
const DLL_B: f64 = 0.0178;
const DLL_C: f64 = 0.000158;

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
    rate: u32,

    // Frame counts of completed capture packets, consumed by playback packets.
    fifo: [u16; FIFO_LEN],
    fifo_head: usize,
    fifo_tail: usize,
    nominal_acc: f64,

    cap_pos: u64,
    play_pos: u64,

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

    fn next_frame_count(&mut self) -> usize {
        if self.fifo_tail != self.fifo_head {
            let n = self.fifo[self.fifo_tail % FIFO_LEN] as usize;
            self.fifo_tail += 1;
            return n;
        }
        self.stats.fallback_packets += 1;
        self.nominal_acc += self.rate as f64 / 8000.0;
        let n = self.nominal_acc as usize;
        self.nominal_acc -= n as f64;
        n
    }

    /// Fills a playback transfer from the playback ring and zeroes what it consumed.
    fn prepare_out(&mut self, t: *mut ffi::libusb_transfer) {
        let shm = self.shm;
        // SAFETY: t is an iso transfer allocated with PACKETS_PER_XFER descriptors and a
        // buffer of MAX_PACKET * PACKETS_PER_XFER bytes; frames <= MAX_FRAMES_PER_PACKET.
        unsafe {
            let shm = &*shm;
            let write_pos = shm.play_write_pos.load(Ordering::Acquire);
            let mut offset = 0usize;
            for i in 0..PACKETS_PER_XFER {
                let frames = self.next_frame_count().min(MAX_FRAMES_PER_PACKET);
                // write_pos 0 means no writer yet (startup prefill): silence, not an underrun.
                if write_pos != 0 && write_pos < self.play_pos + frames as u64 {
                    shm.play_underruns.fetch_add(1, Ordering::Relaxed);
                }
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
                    continue;
                }
                let frames = (d.actual_length as usize / FRAME_BYTES).min(MAX_FRAMES_PER_PACKET);
                if self.fifo_head - self.fifo_tail < FIFO_LEN {
                    self.fifo[self.fifo_head % FIFO_LEN] = frames as u16;
                    self.fifo_head += 1;
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
    /// Transfer completions arrive every 4 ms with scheduling jitter. Loop bandwidth
    /// 0.5 Hz: omega = 2*pi*0.5*0.004, b = sqrt(2)*omega, c = omega^2.
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
        // SAFETY: t is a completed transfer owned by this engine.
        let r = unsafe { ffi::libusb_submit_transfer(t) };
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
    if unsafe { (*t).status } == LIBUSB_TRANSFER_COMPLETED {
        e.consume_in(t);
    } else {
        e.stats.packet_errors += 1;
    }
    e.resubmit(t, "capture");
}

extern "system" fn out_cb(t: *mut ffi::libusb_transfer) {
    // SAFETY: as in in_cb.
    let e = unsafe { &mut *((*t).user_data as *mut Engine) };
    unsafe {
        if (*t).status != LIBUSB_TRANSFER_COMPLETED {
            e.stats.packet_errors += 1;
        }
        for i in 0..(*t).num_iso_packets as usize {
            if (*(*t).iso_packet_desc.as_ptr().add(i)).status != LIBUSB_TRANSFER_COMPLETED {
                e.stats.packet_errors += 1;
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
    thread: Option<JoinHandle<(Box<Engine>, Transfers)>>,
    shm: Arc<ShmRegion>,
    last_stats: Stats,
}

impl Stream {
    /// Claims the streaming interfaces, sets the rate and starts both directions.
    pub fn start(dev: &mut Device, shm: Arc<ShmRegion>, cfg: StreamConfig) -> Result<Self, UsbError> {
        dev.claim_streaming(cfg.clock)?;
        if cfg.clock == ClockSource::Internal {
            dev.set_sample_rate(cfg.sample_rate)?;
        }
        let rate = dev.sample_rate()?;

        let s = shm.get();
        s.running.store(0, Ordering::Release);
        s.generation.fetch_add(1, Ordering::AcqRel);
        s.sample_rate.store(rate, Ordering::Release);
        s.cap_write_pos.store(0, Ordering::Release);
        s.play_read_pos.store(0, Ordering::Release);
        s.play_write_pos.store(0, Ordering::Release);

        let stopping = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let inflight = Arc::new(AtomicUsize::new(0));
        let mut engine = Box::new(Engine {
            shm: shm.as_ptr(),
            stopping: stopping.clone(),
            inflight: inflight.clone(),
            failed: failed.clone(),
            rate,
            fifo: [0; FIFO_LEN],
            fifo_head: 0,
            fifo_tail: 0,
            nominal_acc: 0.0,
            cap_pos: 0,
            play_pos: 0,
            clock_init: false,
            anchor_sample: 0,
            anchor_host: 0.0,
            tpf: 0.0,
            stats: Stats::default(),
        });

        let handle = dev.handle();
        let ctx = dev.ctx() as usize;
        let ep = engine.as_mut() as *mut Engine;
        let mut xfers = Transfers(Vec::with_capacity(2 * NUM_XFERS));
        // SAFETY: transfers are allocated, filled and submitted before the event thread
        // starts; libusb owns them while in flight. Buffers live in `xfers`.
        unsafe {
            for (endpoint, cb) in [(EP_CAP, in_cb as ffi::libusb_transfer_cb_fn), (EP_PLAY, out_cb)] {
                for _ in 0..NUM_XFERS {
                    let t = ffi::libusb_alloc_transfer(PACKETS_PER_XFER as c_int);
                    let mut buf = vec![0u8; MAX_PACKET * PACKETS_PER_XFER];
                    ffi::libusb_fill_iso_transfer(
                        t,
                        handle,
                        endpoint,
                        buf.as_mut_ptr(),
                        buf.len() as c_int,
                        PACKETS_PER_XFER as c_int,
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
                        return Err(UsbError { op: "initial submit", code: r });
                    }
                    inflight.fetch_add(1, Ordering::AcqRel);
                    xfers.0.push((t, buf));
                }
            }
        }
        s.running.store(1, Ordering::Release);

        let thread_stopping = stopping.clone();
        let thread = std::thread::Builder::new()
            .name("ftu-usb".into())
            .spawn(move || {
                // Transfers complete every 4 ms; the work per completion is small.
                if !rt::make_realtime(4_000_000, 1_000_000) {
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

        Ok(Self { stopping, failed, thread: Some(thread), shm, last_stats: Stats::default() })
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
