# ftu-daemon

Helper daemon for the M-Audio Fast Track Ultra. It owns the USB device and moves audio between the device and the HAL plugin through shared memory. Phase 7 of `docs/plan.md`.

## Status

Streaming, the XPC service and mixer control work end to end with the HAL plugin. See `docs/plan.md` for the current status.

| Part | File | State |
| --- | --- | --- |
| Device access, UAC1 requests | `src/usb.rs` | Open, claim, alt settings, sample rate, meters |
| Isochronous engine | `src/stream.rs` | 8×8 S24 at all rates, implicit feedback, realtime event thread and libusb run-loop thread |
| Shared memory | `src/shm.rs`, `../shared/ftu_shm.h` | f32 rings indexed by device sample time, seqlock clock anchor |
| Lead control, packet sizing | `src/pacing.rs` (`Pacer`) | Pure logic, tested against a simulated device |
| Device clock | `src/pacing.rs` (`Clock`), published by `src/stream.rs` | Second-order DLL, 0.5 Hz bandwidth. Estimate stays within about ±10 Hz of 48 kHz, 1 s readings |
| XPC service | `src/xpc_shim.c` | Hands the shared memory to `coreaudiod`'s user and the owner. Rate requests, mixer control |
| Mixer state | `src/mixer.rs` | Matrix with pan, link, mute and solo; effect; returns; output gain. Vendor pan and balance laws |
| Control service | `src/ctl.rs` | JSON requests over XPC, persistence, writes changes to the device, meter polling |
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

## Mixer control

The firmware cannot report its own state (GET returns canned values), so the daemon keeps it. It saves every change to `~/Library/Application Support/FastTrackUltra/mixer.json` and writes the whole state to the device after every connect, power cycle and power mode change. Only changed controls are sent otherwise.

```sh
D="/Library/Application Support/FastTrackUltra/ftu-daemon"
"$D" ctl get                       # full state as JSON
"$D" ctl meters                    # peak meters in dBFS, polled every 20 ms
"$D" ctl status                    # connected, port, streaming, rate, clock, power, firmware
"$D" ctl clock spdif               # or internal; saved in device.json, restarts the stream
"$D" ctl set bus1.in1.level -6 bus1.in1.pan 0.5
"$D" ctl reset                     # defaults: DAW 1-8 to outputs 1-8, inputs off
"$D" ctl stall 20                  # test hook: stalls the USB event thread once for 20 ms
```

| Path | Value |
| --- | --- |
| `bus<1-4>.<src>.level` | dB from -128 to 0, or `off`. Bus 1 = outputs 1/2, bus 4 = 7/8 (S/PDIF) |
| `bus<1-4>.<src>.pan` | -1 (left) to 1 (right). Linked pairs use it as balance |
| `bus<1-4>.<src>.mute`, `.solo` | `on` or `off`. Solo is per bus |
| `link.<src>` | `on` or `off`, for the pair the source belongs to |
| `fx.program` | `room1`, `room2`, `room3`, `hall1`, `hall2`, `plate`, `delay`, `echo` |
| `fx.volume`, `fx.duration`, `fx.feedback` | 0 to 1 |
| `fx.send.<src>` | dB or `off`. One send per source, shared by all buses |
| `fx.return.<1-4>` | dB or `off`, effect return into outputs 1 to 4 |
| `fx.bypass.<1-2>` | `on` silences the effect return on bus 1 or 2. No firmware bypass request is known |
| `link.out<1-8>` | Links an output pair's faders in clients. No effect on the device |
| `out<1-8>.level`, `out<1-8>.mute` | Output gain after the mixer (unit 9) |

On S/PDIF clock the stream runs on capture alternate setting 2 at the rate the device reports in status bits 8 to 9. Rate requests from Core Audio are ignored, and the stream restarts when the reported rate changes for 500 ms. Core Audio does not follow the source's rate yet. `status` reports `clock_locked` from bit 12 while on S/PDIF clock. That path is untested: this unit's receiver locks to its own S/PDIF output.

Sources are `in1` to `in8` (7 and 8 are S/PDIF) and `daw1` to `daw8`. Several pairs in one `set` apply together or not at all. Clients can also replace the whole state with `{"put": state}` (used for settings files). `get meters` returns the peak of the last 60 ms (three 20 ms polls), so several clients can read without interfering.

## Design notes

- **One clock.** Sample time is the count of frames the device has captured. Both rings are indexed by it, and the clock anchor maps it to `mach_absolute_time()`. The plugin derives its zero timestamps from the anchor.
- **Playback ring contract.** The writer fills frames ahead of `play_read_pos` and advances `play_write_pos`. The daemon zeroes each frame after sending it, so a stalled writer produces silence, not a repeated buffer.
- **Queue depth.** Capture: 24 transfers × 8 microframes. Playback: 16 × 8 (16 ms), with a control loop that holds the playback lead constant. An 8 ms playback queue failed with `kIOReturnIsoTooOld`. libusb schedules each submit at least 4 ms ahead, so the playback queue survives event-thread stalls of up to 12 ms.
- **Lead control.** The lead is sampled once per playback transfer and smoothed over 20 ms. Errors over four packets are fixed in one jump, smaller ones outside a two-packet band by one frame at a time. After a start, and after a callback gap over 10 ms, the smoothed error follows the raw error for 24 transfers, so the jump is exact. The start-up jump corrects any error, so the round-trip latency is the same after every start.
- **Two realtime threads.** On macOS libusb receives completions on its own CFRunLoop thread (`org.libusb.device-hotplug`) and then wakes the event thread. Both run with a time constraint policy. With the run-loop thread at normal priority, callback gaps of 3 to 20 ms happened about ten times a minute.
- **Counters.** The daemon logs its stream counters (underruns, USB errors, lead corrections and jumps, fallback packets, stalls, max callback gap since the previous line) to `~/Library/Logs/ftu-daemon.log` at most once a second when they change, and each lead jump with its size.
- **Process model.** A LaunchDaemon with `UserName` set to the user, so `coreaudiod` can reach its XPC service without root.

---

Generated with Claude AI — please review before distribution.
