# Fast Track Ultra driver for modern macOS

An open-source driver for the **M-Audio Fast Track Ultra** USB 2.0 audio interface on macOS on Apple Silicon. M-Audio's last driver supports OS X 10.7 and is an Intel kernel extension, so the interface has not worked on Apple Silicon Macs. This project re-creates that driver in user space, from a protocol reverse engineered for interoperability.

> Unofficial project. Not affiliated with, endorsed by, or supported by M-Audio, inMusic or Avid. "M-Audio" and "Fast Track Ultra" are trademarks of their owners and are used here only to name the hardware this driver supports.

## Status

Work in progress. Tested on macOS 26 (Tahoe) on an M3 Max with firmware 1.51.

| Area | State |
| --- | --- |
| USB protocol | Mapped and confirmed on hardware: mixer, effects, effect returns, output gain, clock source, sample rates, meters. See [`docs/protocol.md`](docs/protocol.md). |
| Audio streaming | Works from user space: 8 in, 8 out, 24-bit, 44.1 to 96 kHz, verified bit-level through the device's internal S/PDIF loopback. |
| Helper daemon (Rust) | Streaming core works. XPC service built. |
| HAL plugin (C++) | Built. Not yet tested inside Core Audio. |
| Control panel | Not started. |
| MIDI | Works through macOS's built-in class driver. Firmware bug: see Known issues. |

Progress against the original control panel's features is tracked in [`docs/features.md`](docs/features.md).

## How it works

```
Core Audio apps
      │
coreaudiod ── HAL plugin (plugin/, C++, libASPL)
                   │  shared memory: f32 ring buffers + device clock
                   │  handed over via XPC
              ftu-daemon (daemon/, Rust) ── libusb ── Fast Track Ultra
```

- **No kernel extension and no DriverKit.** The interface's audio interfaces use a vendor-specific USB class, so macOS binds no driver to them, and a user-space process can claim them with libusb.
- **The daemon** owns the USB device, runs the isochronous streams (playback timed from capture, "implicit feedback"), and tracks the device clock.
- **The plugin** runs inside `coreaudiod`. It only copies samples between Core Audio and the daemon's ring buffers and reports the device clock. Keeping it thin keeps a bug from taking down system audio.
- **The daemon runs as a LaunchDaemon under your user account,** not root, so `coreaudiod` can reach its XPC service.

## Requirements

- Apple Silicon Mac. Developed on macOS 26.
- Xcode command line tools, CMake, Rust (stable), and libusb (`brew install libusb cmake`).
- Python 3 with `pyusb` and `numpy`, only for the research tools in `tools/`.

## Build

```sh
git clone --recurse-submodules <this repository>
cd m-audio-fast-track-ultra-macos-driver

# Daemon
(cd daemon && cargo build --release)

# HAL plugin
cmake -S plugin -B plugin/build -DCMAKE_BUILD_TYPE=Release
cmake --build plugin/build
```

## Install

Installation writes to system folders and restarts `coreaudiod`, so all audio drops for a moment.

```sh
sudo packaging/install.sh     # daemon, LaunchDaemon plist, HAL plugin
sudo packaging/uninstall.sh   # removes all of it
```

The daemon logs to `~/Library/Logs/ftu-daemon.log`.

## Testing without Core Audio

The daemon has a test mode that stands in for the plugin. It plays a tone and records all inputs:

```sh
daemon/target/release/ftu-daemon test --seconds 10 --record /tmp/capture.wav
```

`--spdif` selects the S/PDIF clock, which turns on the device's internal S/PDIF loopback. Combined with `tools/check_loopback.py`, that gives a glitch check with no cables. Details are in [`daemon/README.md`](daemon/README.md).

## Known issues

- **MIDI freezes on transfers over 64 bytes.** Any USB-MIDI transfer to the device larger than 64 bytes (17 or more MIDI events, or a SysEx of about 48 bytes or more) stops all MIDI until the interface is power cycled. The bug is in the device firmware. A workaround, splitting transfers in the daemon, needs the daemon to run as root and is not built yet.
- **Extra bytes after some SysEx messages.** SysEx messages of certain lengths come back with stray bytes after the end marker. Firmware bug; harmless to most receivers.
- **High latency.** About 64 ms of output safety offset for now. Tuning is planned.
- **48 kHz only in the plugin.** The daemon supports all four rates. Rate switching from Core Audio is not wired up yet.

## Repository layout

| Path | Contents |
| --- | --- |
| `daemon/` | Rust helper daemon: USB, streaming, shared memory, XPC |
| `plugin/` | C++ HAL plugin (AudioServerPlugIn) built on libASPL |
| `shared/` | Shared-memory layout used by both |
| `packaging/` | LaunchDaemon plist template and install/uninstall scripts |
| `proto/` | Early prototypes: C streaming tool, CoreMIDI loopback tests |
| `tools/` | Python research tools: control requests, probes, loopback analysis |
| `captures/` | Logs and USB descriptor dumps from hardware tests (audio captures not committed) |
| `docs/` | Plan, feature checklist, protocol spec, driver inventories, firmware notes |
| `third_party/libASPL` | Submodule, [libASPL](https://github.com/gavv/libASPL) v3.1.2 (MIT) |

## Documentation

- [`docs/plan.md`](docs/plan.md): phases, decisions, risks, current status.
- [`docs/features.md`](docs/features.md): feature parity checklist with evidence per feature.
- [`docs/protocol.md`](docs/protocol.md): the USB protocol, with sources and hardware test results.
- [`docs/mac-driver-inventory.md`](docs/mac-driver-inventory.md), [`docs/windows-driver-inventory.md`](docs/windows-driver-inventory.md): structure of the vendor drivers.
- [`docs/firmware.md`](docs/firmware.md): notes toward patching the firmware's MIDI bugs. Exploratory.

## Legal notes

- **No vendor code, binaries, firmware or manuals are included.** The repository holds only original code and documentation of observed behavior. Vendor drivers and the manual are available from M-Audio's support site.
- **Interoperability.** The protocol was worked out from the Linux kernel driver (`snd-usb-audio`, linked by commit, not copied), from analysis of the vendor drivers, and from tests on hardware the author owns. The sole purpose is making the author's hardware work on a current OS.
- **License.** GPL-3.0, see [`LICENSE`](LICENSE). libASPL is MIT-licensed and compatible.

---

Generated with Claude AI — please review before distribution.
