//! Shared memory between the daemon and the HAL plugin.
//! Mirror of `shared/ftu_shm.h`. Keep both in sync and bump `VERSION` on any change.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const MAGIC: u32 = 0x3155_5446; // "FTU1"
pub const VERSION: u32 = 7;
pub const CHANNELS: usize = 8;
pub const RING_FRAMES: usize = 16384;
const RING_SAMPLES: usize = RING_FRAMES * CHANNELS;

#[repr(C)]
pub struct Shm {
    pub magic: u32,
    pub version: u32,
    pub channels: u32,
    pub ring_frames: u32,

    pub running: AtomicU32,
    pub sample_rate: AtomicU32,
    pub generation: AtomicU64,

    clock_seq: AtomicU64,
    anchor_sample_time: AtomicU64,
    anchor_host_time: AtomicU64,
    host_ticks_per_frame: AtomicU64, // f64 bits

    pub cap_write_pos: AtomicU64,
    pub play_read_pos: AtomicU64,
    pub play_write_pos: AtomicU64,

    pub play_underruns: AtomicU64,
    pub usb_errors: AtomicU64,

    pub plugin_late_writes: AtomicU64,
    pub plugin_short_reads: AtomicU64,
    pub plugin_input_jumps: AtomicU64,
    pub plugin_output_jumps: AtomicU64,
    pub lead_frames_added: AtomicU64,
    pub lead_frames_dropped: AtomicU64,

    /// 0 internal, 1 S/PDIF: the clock the running stream uses.
    pub clock_source: AtomicU32,
    /// 1 while the device is plugged in, 0 while it is not. Unchanged over system sleep.
    pub device_present: AtomicU32,

    pub fallback_packets: AtomicU64,
    pub capture_errors: AtomicU64,
    pub playback_errors: AtomicU64,
    pub fifo_overflows: AtomicU64,
    pub callback_stalls: AtomicU64,
    pub max_callback_gap_us: AtomicU64,
    pub lead_jumps: AtomicU64,

    capture: [f32; RING_SAMPLES],
    playback: [f32; RING_SAMPLES],
}

// Must match sizeof(ftu_shm_t). Header is 216 bytes.
const _: () = assert!(std::mem::size_of::<Shm>() == 216 + 2 * RING_SAMPLES * 4);

/// The stream health counters, read together. All count since the stream started.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counters {
    pub play_underruns: u64,
    pub usb_errors: u64,
    pub plugin_late_writes: u64,
    pub plugin_short_reads: u64,
    pub plugin_input_jumps: u64,
    pub plugin_output_jumps: u64,
    pub lead_frames_added: u64,
    pub lead_frames_dropped: u64,
    pub lead_jumps: u64,
    pub fallback_packets: u64,
    pub capture_errors: u64,
    pub playback_errors: u64,
    pub fifo_overflows: u64,
    pub callback_stalls: u64,
}

impl std::fmt::Display for Counters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "playback underruns {}, usb errors {}, plugin late writes {}, short reads {}, input jumps {}, output jumps {}, lead frames added {}, dropped {}, lead jumps {}, fallback packets {}, capture errors {}, playback errors {}, fifo overflows {}, stalls {}",
            self.play_underruns,
            self.usb_errors,
            self.plugin_late_writes,
            self.plugin_short_reads,
            self.plugin_input_jumps,
            self.plugin_output_jumps,
            self.lead_frames_added,
            self.lead_frames_dropped,
            self.lead_jumps,
            self.fallback_packets,
            self.capture_errors,
            self.playback_errors,
            self.fifo_overflows,
            self.callback_stalls
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ClockAnchor {
    pub sample_time: u64,
    pub host_time: u64,
    pub host_ticks_per_frame: f64,
}

/// Owns a page-aligned anonymous shared mapping holding one `Shm`.
/// MAP_SHARED so the region can later be handed to the plugin (xpc_shmem_create).
pub struct ShmRegion {
    ptr: *mut Shm,
    len: usize,
}

unsafe impl Send for ShmRegion {}
unsafe impl Sync for ShmRegion {}

impl ShmRegion {
    pub fn new() -> std::io::Result<Self> {
        let len = std::mem::size_of::<Shm>();
        // SAFETY: anonymous mapping, checked for failure. Zero-filled by the kernel,
        // which is a valid initial state for every field.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_SHARED,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let ptr = ptr as *mut Shm;
        // SAFETY: freshly mapped, exclusively owned here.
        unsafe {
            (*ptr).magic = MAGIC;
            (*ptr).version = VERSION;
            (*ptr).channels = CHANNELS as u32;
            (*ptr).ring_frames = RING_FRAMES as u32;
        }
        Ok(Self { ptr, len })
    }

    pub fn get(&self) -> &Shm {
        // SAFETY: valid for the lifetime of self. All shared fields are atomics;
        // the rings are accessed through raw pointers below.
        unsafe { &*self.ptr }
    }

    pub fn as_ptr(&self) -> *mut Shm {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        // SAFETY: mapping created in new() with this length.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

impl Shm {
    pub fn counters(&self) -> Counters {
        let r = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Counters {
            play_underruns: r(&self.play_underruns),
            usb_errors: r(&self.usb_errors),
            plugin_late_writes: r(&self.plugin_late_writes),
            plugin_short_reads: r(&self.plugin_short_reads),
            plugin_input_jumps: r(&self.plugin_input_jumps),
            plugin_output_jumps: r(&self.plugin_output_jumps),
            lead_frames_added: r(&self.lead_frames_added),
            lead_frames_dropped: r(&self.lead_frames_dropped),
            lead_jumps: r(&self.lead_jumps),
            fallback_packets: r(&self.fallback_packets),
            capture_errors: r(&self.capture_errors),
            playback_errors: r(&self.playback_errors),
            fifo_overflows: r(&self.fifo_overflows),
            callback_stalls: r(&self.callback_stalls),
        }
    }

    /// Pointer to the 8 interleaved capture samples of a frame at `sample_time`.
    pub fn capture_frame(&self, sample_time: u64) -> *mut f32 {
        let i = (sample_time as usize % RING_FRAMES) * CHANNELS;
        // SAFETY: i + CHANNELS <= RING_SAMPLES. Concurrent access is coordinated by
        // cap_write_pos (release by writer, acquire by reader).
        unsafe { (self.capture.as_ptr() as *mut f32).add(i) }
    }

    pub fn playback_frame(&self, sample_time: u64) -> *mut f32 {
        let i = (sample_time as usize % RING_FRAMES) * CHANNELS;
        // SAFETY: as above, coordinated by play_write_pos and play_read_pos.
        unsafe { (self.playback.as_ptr() as *mut f32).add(i) }
    }

    pub fn publish_clock(&self, a: ClockAnchor) {
        let seq = self.clock_seq.load(Ordering::Relaxed);
        self.clock_seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        self.anchor_sample_time.store(a.sample_time, Ordering::Relaxed);
        self.anchor_host_time.store(a.host_time, Ordering::Relaxed);
        self.host_ticks_per_frame.store(a.host_ticks_per_frame.to_bits(), Ordering::Relaxed);
        self.clock_seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    pub fn read_clock(&self) -> Option<ClockAnchor> {
        for _ in 0..16 {
            let s1 = self.clock_seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let a = ClockAnchor {
                sample_time: self.anchor_sample_time.load(Ordering::Relaxed),
                host_time: self.anchor_host_time.load(Ordering::Relaxed),
                host_ticks_per_frame: f64::from_bits(self.host_ticks_per_frame.load(Ordering::Relaxed)),
            };
            std::sync::atomic::fence(Ordering::Acquire);
            if self.clock_seq.load(Ordering::Relaxed) == s1 {
                return (s1 != 0).then_some(a);
            }
        }
        None
    }
}
