# Feature parity checklist

Phase 1 deliverable from `plan.md`. Source: Fast Track Ultra User Guide (2007), available from M-Audio's support site. Not included here. Page numbers refer to the guide's own numbering.

Each row needs evidence before it counts as understood:

- **Linux:** covered by the Linux driver. Ref IDs point to the pinned links in `protocol.md`.
- **Static:** where the behavior was found in the vendor drivers. K = Mac kext, D = Mac daemon, P = Mac control panel. Details and addresses in `protocol.md`.
- **Capture:** a USB capture that confirms the request and its encoding.
- **Done:** implemented and tested in the new driver.

Legend: blank = not started, `?` = partial or unclear, `✓` = done, `n/a` = not applicable.

## Audio streaming

| ID | Feature | Manual | Linux | Static | Capture | Done |
| --- | --- | --- | --- | --- | --- | --- |
| S1 | 8 inputs: analog 1 to 6, S/PDIF L/R | p.1, p.10 | ✓ L1 | | ✓ prototype | ✓ Core Audio 8 in (input 1 and S/PDIF loop tested) |
| S2 | 8 outputs: analog 1 to 6, S/PDIF L/R | p.1, p.9 | ✓ L1 | | ✓ prototype (DAW 1, S/PDIF loopback) | ✓ Core Audio 8 out (output 1 and S/PDIF loop tested) |
| S3 | 24-bit sample format | p.2 | ✓ L1 | | ✓ | ✓ |
| S4 | Sample rate 44.1 kHz (default) | p.8, p.11 | ✓ L1 | ✓ K | ✓ | ✓ Core Audio |
| S5 | Sample rate 48 kHz | p.8, p.11 | ✓ L1 | ✓ K | ✓ | ✓ Core Audio, 8-hour soak |
| S6 | Sample rate 88.2 kHz | p.8, p.11 | ✓ L1 | ✓ K | ✓ | ✓ Core Audio |
| S7 | Sample rate 96 kHz | p.8, p.11 | ✓ L1 | ✓ K | ✓ | ✓ Core Audio |
| S8 | Sample rate change from the host (DAW or control panel) | p.8 | ✓ L1 | ✓ K | ✓ either EP sets both | ✓ Core Audio nominal rate (Audio MIDI Setup, DAW, panel) |

## Operating modes

These likely change the USB interface layout (alt settings or channel count). The driver must detect each one.

| ID | Feature | Manual | Linux | Static | Capture | Done |
| --- | --- | --- | --- | --- | --- | --- |
| M1 | Bus-powered mode: 4×4 (inputs 1/2, outputs 1/2, S/PDIF I/O) | p.2, p.5 | | ✓ D | ✓ status bit0 = 1 | ? power mode shown, mixer resent on change; channels not disabled |
| M2 | USB 1.1 mode: 2×2, 44.1 and 48 kHz only | p.2, p.3 | | ✓ K (2ch alt 1, 48k) | | ? S/PDIF clock refused on USB 1.1; 2×2 streaming not implemented |
| M3 | AC-3/DTS passthrough on S/PDIF out, mutes all analog I/O | p.5 | | ✓ K, D | | |
| M4 | Device connected but powered off (control panel shows status) | p.15 | | ✓ P ("Hardware Connected" / "Not Connected") | | ? panel shows connected; plugin IsAlive on unplug (not on hardware yet) |

## Clock

| ID | Feature | Manual | Linux | Static | Capture | Done |
| --- | --- | --- | --- | --- | --- | --- |
| C1 | Clock source: internal | p.12 | | ✓ K (iface 2 alt 1) | ✓ status bit13 = 0 | ✓ |
| C2 | Clock source: external (S/PDIF) | p.12 | | ✓ K (iface 2 alt 2) | ✓ status bit13 = 1 | ✓ S/PDIF loopback, 120 s |
| C3 | External lock: device follows incoming S/PDIF rate | p.8, p.12 | | ✓ K | | ? daemon follows the status rate; Core Audio does not; no source tested |
| C4 | External, no signal: device falls back to 48 kHz | p.12 | | ✓ P (message text only) | | |
| C5 | External signal status readback (locked / no signal, detected rate) | p.12 | | ✓ K, D | ? bit12 stayed 0 | ? lock from bit 12, untested without a source |
| C6 | S/PDIF input only active when clock is external | p.12 | | ? K | ✓ | ✓ panel hides S/PDIF in on internal clock |

## MIDI

| ID | Feature | Manual | Linux | Static | Capture | Done |
| --- | --- | --- | --- | --- | --- | --- |
| D1 | MIDI in, 1 port (16 channels) | p.1, p.5 | ✓ L1 | | ✓ loopback; SysEx bugs | ? Apple's class driver (firmware SysEx bugs) |
| D2 | MIDI out, 1 port (16 channels) | p.1, p.5 | ✓ L1 | | ✓ loopback; SysEx bugs | ? Apple's class driver (firmware SysEx bugs) |

## Monitor mixer

16 sources × 4 stereo buses. Sources 1 to 8 are hardware inputs (analog 1 to 6, S/PDIF L/R). Sources 9 to 16 are DAW returns. Buses map to outputs 1/2, 3/4, 5/6 and 7/8 (S/PDIF). Headphone 1 follows bus 1/2, headphone 2 follows bus 3/4.

Rows marked "host?" may be computed in the control panel and sent as raw matrix gains. Static analysis decides.

| ID | Feature | Manual | Linux | Static | Capture | Done |
| --- | --- | --- | --- | --- | --- | --- |
| X1 | Per source, per bus: level fader | p.10 | ✓ L3 | ✓ D | ✓ dB×256, max 0 dB | ✓ daemon |
| X2 | Per source, per bus: pan (host?) | p.10 | | ✓ P, D (host) | | ✓ daemon |
| X3 | Per source, per bus: mute (host?) | p.10 | | ? D (host) | | ✓ daemon |
| X4 | Per source, per bus: solo (host?) | p.10 | | ? D (host) | | ✓ daemon |
| X5 | Per source pair: link (host?) | p.10 | | ✓ P (balance law) | | ✓ daemon |
| X6 | Per source, buses 1/2 and 3/4 only: effect send level | p.10 | ? L3 | ✓ D (one per source) | ✓ u5 ctl9 | ✓ daemon |
| X7 | Per bus: output fader | p.11 | | | ✓ unit 9 ctl2 = per-output gain, ctl1 = mute (ch 3 tested) | ✓ daemon (unit 9) |
| X8 | Buses 1/2 and 3/4 only: DSP bypass | p.11 | | ✓ D (host: unit 7 return off, no device request) | | ✓ daemon (zeroes the return) |
| X9 | Mixer is independent of the recording path | p.10 | | | | ? expected from the hardware path; not tested explicitly |
| X10 | Effect return level into outputs 1 to 4 (not in manual) | n/a | ✓ L3 | ✓ D (ctl 2) | ✓ by ear: ctl 2 (or 7) ch 1-4 = out 1-4, ctl 1 = mute. Not in meters | ✓ daemon |
| X11 | FX sends mode: pre-fader or post-fader (Mix 1/2) (panel 2.3.3, not in manual) | n/a | | ✓ D, P: no effect on USB traffic (same send values, only resent after Mix 1/2); hardware send is pre-fader (meters, 2026-10-25) | | ✓ daemon, panel: our extension, off by default (unit tests) |

## DSP effects

Only sends from buses 1/2 and 3/4 reach the DSP. The manual gives no parameter ranges. Linux does, see `protocol.md`.

| ID | Feature | Manual | Linux | Static | Capture | Done |
| --- | --- | --- | --- | --- | --- | --- |
| E1 | Preset: Room 1 | p.13 | ✓ L3 | ✓ D | ✓ (selectable; rooms sound alike) | ✓ |
| E2 | Preset: Room 2 | p.13 | ✓ L3 | ✓ D | ✓ (selectable; rooms sound alike) | ✓ |
| E3 | Preset: Room 3 | p.13 | ✓ L3 | ✓ D | ✓ (selectable; rooms sound alike) | ✓ |
| E4 | Preset: Hall 1 | p.13 | ✓ L3 | ✓ D | ✓ | ✓ |
| E5 | Preset: Hall 2 | p.13 | ✓ L3 | ✓ D | ✓ (selectable; rooms sound alike) | ✓ |
| E6 | Preset: Plate | p.13 | ✓ L3 | ✓ D | ✓ (selectable; rooms sound alike) | ✓ |
| E7 | Preset: Delay | p.13 | ✓ L3 | ✓ D | ✓ (selectable; rooms sound alike) | ✓ |
| E8 | Preset: Echo | p.13 | ✓ L3 | ✓ D | ✓ (selectable; rooms sound alike) | ✓ |
| E9 | Parameter: Duration | p.13 | ✓ L5 | ✓ D | ✓ by ear | ✓ |
| E10 | Parameter: Feedback | p.13 | ✓ L5 | ✓ D | ✓ by ear | ✓ |
| E11 | Parameter: Volume | p.13 | ✓ L5 | ✓ D | ✓ | ✓ |
| E12 | Duration, feedback and volume remembered per program, restored on program change (not in manual) | n/a | | ✓ D (`LEffectSettings` per program) | | ✓ daemon, panel (unit tests) |

## Metering

| ID | Feature | Manual | Linux | Static | Capture | Done |
| --- | --- | --- | --- | --- | --- | --- |
| L1 | Per source meters on each monitor tab (16 sources) | p.10 | | ✓ D | | ✓ panel |
| L2 | Per bus output meters | p.11 | | ✓ D | | ✓ panel |
| L3 | Meters tab: all 8 hardware inputs and 8 outputs | p.14 | | ✓ D | ✓ (read twice) | ✓ panel |

## Host-side features

No USB traffic expected. Reimplement in the new control panel.

| ID | Feature | Manual | Linux | Static | Capture | Done |
| --- | --- | --- | --- | --- | --- | --- |
| H1 | Save mixer and routing settings to a file | p.13 | | | n/a | ✓ panel |
| H2 | Restore settings from a file | p.13 | | | n/a | ✓ panel |
| H3 | Flow tab: static signal flow graphic | p.14 | | | n/a | |
| H4 | About tab: hardware and driver versions | p.15 | | ✓ D (bcdDevice) | | ✓ panel |

## Out of scope

| Feature | Reason |
| --- | --- |
| Mic/instrument gain, 20 dB pad, front/rear switches, phantom power | Analog hardware controls |
| Headphone and main output knobs | Analog hardware controls |
| Inserts on channels 1 and 2 | Analog hardware |
| ASIO/WDM buffer size, ASIO direct monitoring, High Performance mode | Windows only. Core Audio buffer size is set by the host app. |

## Open questions for static analysis

- Pan, mute, solo and link (X2 to X5): computed on the host. Pan and balance laws are in `protocol.md`. Mute and solo force a node to -200 dB.
- Mixer and DSP state across power cycles: the driver restores it on connect, as the vendor daemon does (`PersistentDeviceManager`). Ours restores from `mixer.json`, verified after a full power cycle (2026-10-24).
- What are the native ranges and step sizes for matrix gains, sends and effect returns? E9 to E11 are known from Linux.

---

Generated with Claude AI — please review before distribution.
