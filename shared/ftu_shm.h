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
#define FTU_SHM_VERSION 6u
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
    // IO cycles that did not start where the previous one ended (same stream generation):
    // the HAL's timeline skipped or repeated frames.
    FTU_ATOMIC(uint64_t) plugin_input_jumps;
    FTU_ATOMIC(uint64_t) plugin_output_jumps;
    // Playback packets the daemon lengthened or shortened by one frame to pull the
    // playback lead back to its target. Each one shifts output timing by one frame.
    FTU_ATOMIC(uint64_t) lead_frames_added;
    FTU_ATOMIC(uint64_t) lead_frames_dropped;

    // Clock the running stream uses: 0 internal, 1 S/PDIF. Written by the daemon at stream start.
    FTU_ATOMIC(uint32_t) clock_source;
    uint32_t pad0;

    // Stream diagnostics, written by the daemon's USB thread.
    FTU_ATOMIC(uint64_t) fallback_packets;       // playback packets sized without a capture packet
    FTU_ATOMIC(uint64_t) capture_errors;         // failed capture transfers or packets
    FTU_ATOMIC(uint64_t) playback_errors;        // failed playback transfers or packets
    FTU_ATOMIC(uint64_t) fifo_overflows;         // capture packet sizes dropped (size FIFO full)
    FTU_ATOMIC(uint64_t) callback_stalls;        // capture callbacks more than 3 ms apart
    FTU_ATOMIC(uint64_t) max_callback_gap_us;    // longest gap between capture callbacks
    FTU_ATOMIC(uint64_t) lead_jumps;             // large lead errors fixed by moving the read position

    float capture[FTU_RING_FRAMES * FTU_CHANNELS];
    float playback[FTU_RING_FRAMES * FTU_CHANNELS];
} ftu_shm_t;

#endif
