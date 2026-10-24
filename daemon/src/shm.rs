//! Shared memory between the daemon and the HAL plugin.
//! Mirror of `shared/ftu_shm.h`. Keep both in sync and bump `VERSION` on any change.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const MAGIC: u32 = 0x3155_5446; // "FTU1"
pub const VERSION: u32 = 3;
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

    /// 0 internal, 1 S/PDIF: the clock the running stream uses.
    pub clock_source: AtomicU32,

    _reserved: [u8; 44],

    capture: [f32; RING_SAMPLES],
    playback: [f32; RING_SAMPLES],
}

// Must match sizeof(ftu_shm_t). Header is 168 bytes.
const _: () = assert!(std::mem::size_of::<Shm>() == 168 + 2 * RING_SAMPLES * 4);

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
