// Shared memory layout between the helper daemon (Rust) and the HAL plugin (C++).
// The Rust mirror is daemon/src/shm.rs. Keep both in sync and bump FTU_SHM_VERSION on any change.
//
// Timeline: one sample clock, the device's. "Sample time" N means the N-th frame the
// device has captured since the stream started. Rings are indexed by sample time
// modulo FTU_RING_FRAMES. Samples are 32-bit float, interleaved, 8 channels.
//
// Capture:  the daemon writes frames [.., cap_write_pos). Readers stay behind it.
// Playback: the writer (plugin) fills frames ahead of play_read_pos and advances
//           play_write_pos. The daemon reads at play_read_pos, sends those frames to
//           the device, and zeroes them. Frames past play_write_pos play as silence.
// Clock:    a seqlock-protected anchor maps sample time to mach_absolute_time().

#ifndef FTU_SHM_H
#define FTU_SHM_H

#include <stdint.h>

#ifdef __cplusplus
#include <atomic>
#define FTU_ATOMIC(T) std::atomic<T>
#else
#define FTU_ATOMIC(T) _Atomic T
#endif

#define FTU_SHM_MAGIC 0x31555446u  // "FTU1"
#define FTU_SHM_VERSION 3u
#define FTU_CHANNELS 8u
#define FTU_RING_FRAMES 16384u  // power of two; 341 ms at 48 kHz, 170 ms at 96 kHz

typedef struct {
    // Header, written once by the daemon.
    uint32_t magic;
    uint32_t version;
    uint32_t channels;
    uint32_t ring_frames;

    // Stream state. running: 1 while the USB streams run.
    FTU_ATOMIC(uint32_t) running;
    FTU_ATOMIC(uint32_t) sample_rate;
    FTU_ATOMIC(uint64_t) generation;  // bumped on every stream restart; sample time resets to 0

    // Clock anchor (seqlock: odd while being written).
    FTU_ATOMIC(uint64_t) clock_seq;
    FTU_ATOMIC(uint64_t) anchor_sample_time;
    FTU_ATOMIC(uint64_t) anchor_host_time;      // mach_absolute_time() ticks at anchor_sample_time
    FTU_ATOMIC(uint64_t) host_ticks_per_frame;  // IEEE 754 double bits, smoothed

    // Positions in sample time.
    FTU_ATOMIC(uint64_t) cap_write_pos;
    FTU_ATOMIC(uint64_t) play_read_pos;
    FTU_ATOMIC(uint64_t) play_write_pos;  // written by the playback writer: frames filled up to here

    // Counters for diagnostics.
    FTU_ATOMIC(uint64_t) play_underruns;  // times an active writer fell behind play_read_pos
    FTU_ATOMIC(uint64_t) usb_errors;

    // Written by the plugin.
    FTU_ATOMIC(uint64_t) plugin_late_writes;  // output cycles that arrived after play_read_pos
    FTU_ATOMIC(uint64_t) plugin_short_reads;  // input cycles not yet captured (zero-filled)

    // Clock the running stream uses: 0 internal, 1 S/PDIF. Written by the daemon at stream start.
    FTU_ATOMIC(uint32_t) clock_source;

    uint8_t reserved[44];

    float capture[FTU_RING_FRAMES * FTU_CHANNELS];
    float playback[FTU_RING_FRAMES * FTU_CHANNELS];
} ftu_shm_t;

#endif
