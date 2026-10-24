# M-Audio Fast Track Ultra on modern macOS

## Goal

Make the M-Audio Fast Track Ultra work on macOS Tahoe 26 on Apple Silicon (M3 Max), with full feature parity with the original vendor driver.

The last official macOS driver targets OS X 10.7 Lion. It is an Intel kernel extension and cannot load on Apple Silicon.

## Decisions so far

- **Audience:** personal use first. Source published under GPL-3.0. No App Store, no signed distribution, no Apple entitlement requests for now.
- **Scope:** copy all functionality of the vendor driver. A basic "audio in, audio out" driver is not the finish line.
- **Delivery vehicle:** a Core Audio HAL plugin (AudioServerPlugIn) plus a user-space helper daemon that owns the USB device. DriverKit stays an option if the project later goes public.
- **Languages (2026-10-23):** HAL plugin in C++ on libASPL (MIT). Helper daemon in Rust. Control panel later.
- **Daemon process model:** a LaunchDaemon in the system domain, running as the user via `UserName`, not root. `coreaudiod` lives in the system domain and cannot reach a per-user LaunchAgent's services. Running unprivileged means MIDI stays on Apple's class driver, so the 64-byte MIDI freeze is not worked around yet. Moving to root later is a plist change.
- **DriverKit not needed:** the Phase 6 prototype streams 8×8 at all rates from user space.

## Source material

| Source | Location | Use |
| --- | --- | --- |
| Mac driver 2.3.3 (OS X 10.7) | `Fast_Track_Ultra_Driver_2_3_3_Mac_83637.dmg` (M-Audio support site, not in repo) | Primary reverse engineering target. Kext for streaming, control panel app for features. |
| Windows driver 6.1.10 | `FastTrackUltra_6_1_10_Win_83292.zip` (M-Audio support site, not in repo) | Secondary target. Newer, may contain fixes or features missing from the Mac build. |
| Linux `snd-usb-audio` | Linux kernel source (`sound/usb/quirks-table.h`, `sound/usb/mixer_quirks.c`) | Cross-check. Believed to support USB IDs `0763:2080` and `0763:2081` with a dedicated mixer. To be verified. |
| User manual | M-Audio support site | Feature inventory. |

## Phases

### Phase 1: Feature inventory

Build the parity checklist before reading any code.

- List every user-facing feature from the manual and the control panel screenshots.
- Expected areas: per-channel mixer, routing matrix, DSP effects (reverb, delay), sample rates, clock source, buffer size, metering, firmware update.
- Record each feature as a row in `docs/features.md` with columns for "static evidence", "capture evidence" and "implemented".

**Exit criteria:** a checklist that a reviewer agrees covers the vendor control panel.

### Phase 2: Static analysis of the Mac package

- Unpack the DMG and the installer package in an isolated directory.
- Inventory the payload: kext, control panel app, helper tools, plists, firmware blobs.
- Control panel app: recover Objective-C classes and methods. Map each UI control to the code path that builds a USB request.
- Kext: recover the USB descriptors it expects, the interface and alt setting selection, endpoint usage, sample formats, clock and sync handling, and vendor control requests.
- Write findings into `docs/protocol.md`, one section per request.

**Exit criteria:** every Phase 1 feature has a candidate USB request, or is marked as unknown.

### Phase 3: Static analysis of the Windows package

- Unpack the ZIP. Identify the kernel driver (`.sys`), the ASIO DLL and the control panel.
- Diff its request set against the Mac findings. Note anything new or different.
- Check for an embedded firmware image or a firmware update routine.

**Exit criteria:** the unknowns from Phase 2 are resolved or narrowed. Differences between the two drivers are documented.

### Phase 4: Linux cross-check

- Read the Linux quirk entry and mixer code for the Fast Track Ultra.
- Compare against `docs/protocol.md`. Disagreements become test cases for Phase 5.

**Exit criteria:** a list of confirmed matches and open conflicts.

### Phase 5: Hardware confirmation (needs the device)

Static analysis produces candidates. Captures prove what each byte means.

Capture options, best first:

1. **Windows on an x86 PC** with the vendor driver, USBPcap and Wireshark. This is the only way to watch the vendor driver control the DSP. Windows on ARM (including Parallels on the M3) cannot load the 2013 x86 kernel driver.
2. **Linux on a spare PC or Raspberry Pi** with `usbmon` and Wireshark. Reliable streaming.
3. **ARM Linux in a UTM VM** on the M3 with USB passthrough. Control traffic captures fine. Audio streaming may glitch.

A Lion Mac is not useful here. Old macOS has no practical USB sniffer.

For each checklist row: toggle the control, capture, confirm the request matches the static finding.

**Exit criteria:** every checklist row has capture evidence or a documented reason it cannot be captured.

### Phase 6: User-space prototype

A command-line tool that talks to the device directly with libusb or the IOUSBHost framework.

- Enumerate and claim the interfaces.
- Set the sample rate and clock source.
- Play a test tone and record input over isochronous endpoints.
- Drive every mixer, routing and DSP control from the checklist.

**Exit criteria:** every feature works from the CLI, with no Core Audio involved.

### Phase 7: macOS driver

- **Helper daemon:** owns the USB device, runs the isochronous streams, exposes audio through shared memory ring buffers and controls through XPC.
- **HAL plugin:** loads into `coreaudiod`, presents the device to Core Audio, reads and writes the shared ring buffers. The daemon exists because `coreaudiod` is sandboxed and cannot reliably open USB devices itself.
- **Control panel:** a small SwiftUI app for mixer, routing and DSP, talking to the daemon over XPC.

**Exit criteria:** the device shows up in Audio MIDI Setup and works in a DAW at all supported sample rates.

### Phase 8: Hardening

- Clock drift and sample rate tracking between USB and the Core Audio clock.
- Latency tuning and buffer size options.
- Sleep, wake and hot-plug.
- Persisting mixer and DSP state across reconnects.
- Long recording soak tests.

**Exit criteria:** a multi-hour recording session with no dropouts, and clean recovery after sleep and replug.

## Risks

- **Isochronous timing from user space.** The helper daemon must keep USB transfers fed with low jitter. If it cannot, DriverKit becomes necessary.
- **DSP effects may be undocumented in Linux.** Parity for the effects depends on the vendor drivers and on Windows captures.
- **Firmware.** If the device needs a firmware upload at power-on, the daemon must replicate it. Phases 2 and 3 will tell.
- **macOS changes.** Future macOS releases may tighten HAL plugin or USB access rules.

## Status (2026-10-23)

- Phases 1 to 4 done. See `docs/features.md`, `docs/protocol.md` and the two inventory docs.
- Phase 5 mostly done with the device on this Mac. Mixer, effects, effect return, output gain, clock, rates and meters are confirmed. Open items are listed at the end of `docs/protocol.md`.
- Phase 6 started. `proto/ftu_stream` streams 8×8 at all rates with implicit feedback from user space.
- Phase 7 started. `daemon/` (Rust) has the streaming core: isochronous engine, shared-memory rings, device clock. Verified through the S/PDIF loopback. See `daemon/README.md`.
- The XPC service, the C++ HAL plugin (`plugin/`, libASPL) and the install scripts (`packaging/`) are built and installed (2026-10-24).
  - Core Audio lists "Fast Track Ultra", 8 in / 8 out, 48 kHz. The plugin connects to the daemon over XPC.
  - Playback through Core Audio confirmed: a -20 dBFS tone from `proto/ca_test.swift` read 3276 on the DAW 1 meter, the exact expected value.
  - Recording through Core Audio confirmed: a phone metronome into input 1 came back as 10 clicks at 80.0 BPM, interval spread 0.48 ms, no dropouts, peak -29 dBFS. Inputs 2 to 6 stayed at their noise floors (`captures/ca-test-metronome.txt`).
  - DAW test passed (user, 2026-10-24): Ableton Live Lite recorded input 1 and played it back to both sides of the headphones.
  - Sample-rate switching from Core Audio works at 44.1, 48, 88.2 and 96 kHz. The plugin asks the daemon over XPC; the switch takes 0.1 to 0.25 s. Metronome recordings at each rate: click interval spread at most 0.03 ms, no dropouts (`captures/ca-rates.txt`).
  - Latency reduced from 64 ms to 24 ms output safety offset: capture transfers complete every 1 ms, 16 ms playback queue, a control loop holds the playback lead constant. An 8 ms queue failed with `kIOReturnIsoTooOld` (libusb schedules every submit at least 4 ms ahead).
  - 3-minute soak at 48 kHz on the 24 ms build: 240 clicks, interval spread 0.08 ms, no dropouts, no USB errors, late writes or short reads. The daemon log showed 2,731 playback underruns on that build. The counter did not move afterwards, so they most likely came from stream start, before the soak. Not yet explained.
  - Control panel (`panel/`, SwiftUI, 2026-10-24): layout after the vendor panel, native macOS controls. Tested by the user on the device.
  - Mixer control over XPC (2026-10-24): the daemon keeps the mixer, effect and output state, saves it, and restores it after a power cycle. `ftu-daemon ctl` drives it. Verified with a metronome on input 1: level, pan law, link, mute, solo, buses 2 and 4 and effect send on the meters; output gain, output mute and effect returns by ear; restore after a full power cycle on the meters. See `daemon/README.md`.

- Side project: firmware patching for the MIDI bugs. See `docs/firmware.md`.

## Next action

Phase 7: finish the control panel (menu bar commands, a single main window, external clock). Phase 8: the startup underruns, longer soaks and lower latency.

---

Generated with Claude AI — please review before distribution.
