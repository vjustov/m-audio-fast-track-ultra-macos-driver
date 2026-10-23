# ftu-daemon

Helper daemon for the M-Audio Fast Track Ultra. It owns the USB device and moves audio between the device and the HAL plugin through shared memory. Phase 7 of `docs/plan.md`.

## Status

The streaming core works. The XPC service (`src/xpc_shim.c`) and the launchd plist (`../packaging/`) are built but not yet tested end to end with the plugin. Mixer control is not built yet.

| Part | File | State |
| --- | --- | --- |
| Device access, UAC1 requests | `src/usb.rs` | Open, claim, alt settings, sample rate, meters |
| Isochronous engine | `src/stream.rs` | 8×8 S24 at all rates, implicit feedback, realtime event thread |
| Shared memory | `src/shm.rs`, `../shared/ftu_shm.h` | f32 rings indexed by device sample time, seqlock clock anchor |
| Device clock | `src/stream.rs` (`update_clock`) | Second-order DLL, 0.5 Hz bandwidth. Estimate stays within about ±10 Hz of 48 kHz, 1 s readings |
| XPC service | `src/xpc_shim.c` | Hands the shared memory to `coreaudiod`'s user and the owner. Untested end to end |
| Test harness | `src/main.rs` (`test`) | Stands in for the plugin: tone into the playback ring, capture ring to WAV |

## Build and test

```sh
cargo build --release
# Tone on DAW 1, record all inputs. --spdif uses the internal S/PDIF loopback
# (route DAW 1 to output 7 first, see docs/protocol.md).
target/release/ftu-daemon test --spdif --seconds 20 --record ../captures/daemon-loopback.wav
python3 -I ../tools/check_loopback.py ../captures/daemon-loopback.wav 7 997 -20
```

Results so far (`captures/daemon-loopback-48k*.txt`): 30 s and 20 s runs through the S/PDIF loopback returned the tone at -20.00 dBFS with no glitches after the first 1.5 s, which is the S/PDIF loop locking. No USB errors, no underruns after startup.

## Design notes

- **One clock.** Sample time is the count of frames the device has captured. Both rings are indexed by it, and the clock anchor maps it to `mach_absolute_time()`. The plugin derives its zero timestamps from the anchor.
- **Playback ring contract.** The writer fills frames ahead of `play_read_pos` and advances `play_write_pos`. The daemon zeroes each frame after sending it, so a stalled writer produces silence, not a repeated buffer.
- **Queue depth.** 12 transfers × 32 microframes per direction (48 ms). Smaller queues failed in the prototype. Latency tuning is Phase 8.
- **Process model.** Will run as a LaunchDaemon with `UserName` set to the user, so `coreaudiod` can reach its XPC service without root.

---

Generated with Claude AI — please review before distribution.
