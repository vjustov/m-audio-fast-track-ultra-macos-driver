# Fast Track Ultra USB protocol

Working spec for the device's USB interface. Every fact carries a source tag so we know how much to trust it.

Source tags:

- **[Linux]** Linux kernel `snd-usb-audio`. Written by people who ran the real device, but only covers what Linux exposes.
- **[UAC1]** Standard USB Audio Class 1.0 behavior that the Linux code relies on implicitly.
- **[Mac]** / **[Win]** Vendor driver static analysis (Phases 2 and 3). Not started.
- **[Capture]** Confirmed on hardware (Phase 5). Not started.

## Linux references

All links are pinned to mainline commit [`734bef548a96`](https://github.com/torvalds/linux/tree/734bef548a96e8774d6eef5dfa2026c6edc4e80b/sound/usb) (2026-10-23).

| Ref | What | Link |
| --- | --- | --- |
| L1 | Interfaces, endpoints, formats, rates | [`quirks-table.h#L1705-L1760`](https://github.com/torvalds/linux/blob/734bef548a96e8774d6eef5dfa2026c6edc4e80b/sound/usb/quirks-table.h#L1705-L1760) |
| L2 | Implicit feedback flag | [`quirks.c#L2357-L2360`](https://github.com/torvalds/linux/blob/734bef548a96e8774d6eef5dfa2026c6edc4e80b/sound/usb/quirks.c#L2357-L2360) |
| L3 | Mixer matrix, effect and send/return controls | [`mixer_quirks.c#L1262-L1544`](https://github.com/torvalds/linux/blob/734bef548a96e8774d6eef5dfa2026c6edc4e80b/sound/usb/mixer_quirks.c#L1262-L1544) |
| L4 | FTU mixer enabled for both USB IDs | [`mixer_quirks.c#L4463-L4465`](https://github.com/torvalds/linux/blob/734bef548a96e8774d6eef5dfa2026c6edc4e80b/sound/usb/mixer_quirks.c#L4463-L4465) |
| L5 | Effect value range overrides | [`mixer.c#L1116-L1133`](https://github.com/torvalds/linux/blob/734bef548a96e8774d6eef5dfa2026c6edc4e80b/sound/usb/mixer.c#L1116-L1133) |
| L6 | Channel number = mask bit + 1 | [`mixer.c#L1551-L1564`](https://github.com/torvalds/linux/blob/734bef548a96e8774d6eef5dfa2026c6edc4e80b/sound/usb/mixer.c#L1551-L1564) |

## Device identity

| Model | VID:PID | Source |
| --- | --- | --- |
| Fast Track Ultra | `0763:2080` | [Linux] L1, L4 |
| Fast Track Ultra 8R | `0763:2081` | [Linux] L1, L4 |

Both models share the same Linux code. The device reports a vendor-specific class, so no standard macOS driver claims it. [Linux] L1 uses `USB_DEVICE_VENDOR_SPEC`.

## Interfaces

| Interface | Role | Notes | Source |
| --- | --- | --- | --- |
| 0 | Control | Vendor class `0xFF`, with UAC1 class-specific descriptors inside. They describe only four terminals (IT 1 line in, OT 2 USB, IT 3 USB, OT 4 line out). Units 5 to 9 are undescribed. | [Capture] |
| 1 | Playback | See Streaming. | [Linux] L1 |
| 2 | Capture | See Streaming. | [Linux] L1 |
| 3 | MIDI | Standard USB MIDI class. No quirks. | [Linux] L1 |

## Streaming

| Property | Playback | Capture | Source |
| --- | --- | --- | --- |
| Interface / alt setting | 1 / 1 | 2 / 1 | [Linux] L1 |
| Endpoint | `0x01` (OUT) | `0x81` (IN) | [Linux] L1 |
| Endpoint attributes | `0x09`: isochronous, adaptive | `0x05`: isochronous, async | [Linux] L1 |
| Endpoint attributes, as described | `0x05`: async, plus a 3-byte feedback IN endpoint `0x82` (attributes `0x01`) | `0x05`: async | [Capture] |
| Channels | 8 | 8 | [Linux] L1 |
| Sample format | 24-bit signed, little endian, packed 3 bytes | same | [Linux] L1 |
| Sample rates | 44100, 48000, 88200, 96000 | same | [Linux] L1 |

### Clocking

- Playback uses implicit feedback. The host sizes outgoing packets to match the sample counts in incoming capture packets. The capture stream must run whenever playback runs. [Linux] L2
- Sample rate is set per endpoint with the UAC1 sampling frequency control. [Linux] L1 sets `UAC_EP_CS_ATTR_SAMPLE_RATE`. Request format [UAC1]:
  - `bmRequestType = 0x22`, `bRequest = 0x01` (SET_CUR)
  - `wValue = 0x0100`, `wIndex = endpoint address`
  - Data: 3 bytes, rate in Hz, little endian

Unknown: whether setting one endpoint sets both, and how the clock source (internal / S/PDIF) is selected.

## Control requests

All mixer and effect controls are UAC1 class requests to interface 0. [Linux] L3, L6

| Field | SET_CUR | GET_CUR |
| --- | --- | --- |
| `bmRequestType` | `0x21` | `0xA1` |
| `bRequest` | `0x01` | `0x81` |
| `wValue` | `(control << 8) \| channel` | same |
| `wIndex` | `(unit << 8) \| 0` | same |

`channel` is 1-based. `0` addresses the whole unit. [Linux] L6

Value types:

- **S16 dB:** 2 bytes, signed, little endian, in 1/256 dB steps. [UAC1]
- **U8:** 1 byte, unsigned.

### Unit 5: monitor mixer matrix

| Control | Channel | Meaning | Type | Range | Source |
| --- | --- | --- | --- | --- | --- |
| 1 to 8 | 1 to 16 | Gain from source `channel` to output `control` | S16 dB | Device reported (GET_MIN/MAX/RES). Unknown. | [Linux] L3 |
| 9 | 1 to 16 | Effect send level for source `channel` | S16 dB | Device reported. Unknown. | [Linux] L3 |

Sources 1 to 8 are hardware inputs (analog 1 to 6, S/PDIF L/R). Sources 9 to 16 are DAW returns 1 to 8. Outputs 1 to 8 are analog 1 to 6, then S/PDIF L/R. [Linux] L3 control names, matched against the manual.

Linux exposes one send per source. The manual shows sends on both the 1/2 and 3/4 monitor tabs. Either the device has one shared send or Linux covers only part of it.

### Unit 6: effect processor

| Control | Channel | Meaning | Type | Range | Source |
| --- | --- | --- | --- | --- | --- |
| 1 | 0 | Program | 2 bytes: program, `0x00` | 0 Room 1, 1 Room 2, 2 Room 3, 3 Hall 1, 4 Hall 2, 5 Plate, 6 Delay, 7 Echo | [Linux] L3 |
| 2 | 0 | Effect volume | U8 | `0x00` to `0x7F` | [Linux] L3, L5 |
| 3 | 0 | Duration | S16 | `0x0000` to `0x7F00`, step `0x0100` | [Linux] L3, L5 |
| 4 | 0 | Feedback | U8 | `0x00` to `0x7F` | [Linux] L3, L5 |

### Unit 7: effect return

| Control | Channel | Meaning | Type | Range | Source |
| --- | --- | --- | --- | --- | --- |
| 7 | 1 to 4 | Effect return level into output `channel` | S16 dB | Device reported. Unknown. | [Linux] L3 |

Returns only reach outputs 1 to 4, which matches the manual: only buses 1/2 and 3/4 use the DSP.

## Mac driver findings

Static analysis of the Mac 2.3.3 package. Addresses refer to these binaries:

| Ref | Binary | SHA-256 |
| --- | --- | --- |
| D | `LaunchdDaemon` (i386) | `a7b0958cbde941d84db4bd820c28e2a1ae69e800bd339e4d89ef99dcfacde41f` |
| K | `M-AudioFastTrackUltra.kext` 2.4.2 (x86_64) | `6db44b81b1752d71e8d4763792c3f262726cf782da39a18f241d07d774a49c06` |

"Observed" means read directly from decompiled code or assembly. "Inferred" means it follows from the code but has not been read literally.

### Confirmations of the Linux spec

| Topic | Mac evidence | Agrees with Linux |
| --- | --- | --- |
| Matrix gain request | `SendMixerRequest` D:`0x23d1c` builds `wValue = (control << 8) \| channel`, `wIndex = unit << 8`, 2 bytes, SET_CUR `0x01` / GET_CUR `0x81`. Mixer unit created as unit 5 with 16 inputs (D:`0x567cc`). | Yes |
| Effect parameters | `SendEffectsParam` D:`0x5cc06`: SET_CUR, `wIndex 0x0600`, `wValue 0x0100` to `0x0400`. | Yes, with one difference below |
| Effect send | `SendFXSendsLevel` D:`0x5cae8`: one mixer node per source (8 hardware, 8 DAW), node 8 of that source's row. Consistent with control 9. | Yes |
| Sample rate | K:`0x48f0` assembly: `bmRequestType 0x22`, `bRequest 0x01`, `wValue 0x0100`, `wIndex` = endpoint address, 3 bytes. | Yes |

So the effect send is **one shared send per source**, not one per bus. The manual's two monitor tabs show the same control.

### Gain encoding

`LDevice::ConvertToFirmwareLevel` D:`0x2c128` (observed):

- Input in dB is clamped to `[-128, max]` and multiplied by 256, giving S16 in 1/256 dB.
- Anything at or below about -128 dB is sent as `0x8000`, meaning off.
- The daemon caches the last value per node and skips unchanged writes. A cache value of `0x7FFF` means "unknown, always send".

### Effect parameter encoding

From `SendEffectsParam` D:`0x5cc06` (observed). The control panel works in 0.0 to 1.0 and the daemon scales:

| Control | wValue | Bytes | Encoding |
| --- | --- | --- | --- |
| Program | `0x0100` | 1 | Program number 0 to 7 |
| Volume | `0x0200` | 1 | `value × 127` |
| Duration | `0x0300` | 2 | `value × 32512` (`0x7F00`) |
| Feedback | `0x0400` | 1 | `value × 127` |

Difference from Linux: the Mac driver sends the program as 1 byte. Linux sends 2.

### Effect return (conflict with Linux)

`SendFXReturnsLevels` D:`0x5c9a2` with `SendVolumeRequest` D:`0x23db0` (observed):

- Feature unit 7, `wValue = 0x0200 | channel` (UAC1 volume control), `wIndex 0x0700`, S16 dB.
- Two channels per output pair: pair `n` uses channels `2n` and `2n + 1` of the unit's channel table.

Linux uses control 7 on the same unit. The Mac driver uses control 2. Settle on hardware.

### Feature units 8 and 9

Created in the device constructor D:`0x567cc` next to unit 7. `InitializeHardware` D:`0x5c7b8` sets every channel of both to 0 dB at startup. Their purpose is unknown. They are likely DAW playback and capture trims.

### Meters and device status

`GetMixerData` K:`0xd66` assembly and `FetchMeters` D:`0x5ccf8` (observed):

| Field | Value |
| --- | --- |
| `bmRequestType` | `0xA1` |
| `bRequest` | `0x85` (UAC1 GET_MEM) |
| `wValue` | `0x0100` |
| `wIndex` | `0x0504` |
| `wLength` | 54 |

`wIndex` low byte is `0x04`, not the control interface `0x00`. It is a literal in both binaries.

Reply layout: 27 signed 16-bit little-endian words. `LMeters::StoreInCache` D:`0x585e2` converts each meter to `|v| / 32768`, a linear 0.0 to 1.0 value.

| Words | Meaning | Confidence |
| --- | --- | --- |
| 0 to 7 | Hardware inputs 1 to 8 (analog 1 to 6, S/PDIF L/R) | Inferred from cache layout |
| 8 to 15 | DAW returns 1 to 8 | Inferred |
| 16 to 23 | Outputs 1 to 8 | Inferred |
| 24 to 25 | Effect return L/R (inferred). Read 0 with reverb active on hardware, so likely wrong. | Inferred, contradicted |
| 26 | Device status word | Observed (`FetchDeviceStatus` D:`0x5cd40`) |

Status word bits:

| Bits | Meaning | Source |
| --- | --- | --- |
| 0 | Current power mode. Set means mode 0. | D:`0x5b91e` |
| 1 | Startup power mode. Set means mode 0. | D:`0x5b8e0` |
| 8 to 9 | Device sample rate: 0 = 44.1, 1 = 48, 2 = 88.2, 3 = 96 kHz | K:`0xdb2`, K:`0xe52` |
| 12 | Set means the external clock is not valid | K:`0xe52` |
| 13 | Watched by the daemon timer, which posts a notification when it changes. Meaning unknown. | D:`0x5c1f2` |

Power mode 1 = bus powered, 0 = PSU (see Hardware observations). Power mode is read-only: `LPowerMode` rejects writes (D:`0x4fdd8`).

Polling: the kext reads this block every 1,000 ms (K:`0xe52`). The daemon also polls it on its own timer for meters.

### Clock source

- The kext exposes a Core Audio clock selector with two values: `'int '` (Internal) and `'spdf'` (S/PDIF). K:`0x113a`
- With `'spdf'` selected, the kext timer reads status bits 8 to 9 and switches the engine to the device-reported rate if it differs. It reports "clock valid" from status bit 12. K:`0xe52`
- Changing the selector calls `SetDeviceClockSource` K:`0xfa0`. That calls `SetConfiguration` through the engine vtable (slot `0xca8`, resolved from `__ZTV21MaudioUsbRipleyEngine`), which re-selects alternate settings.
- **The clock source is chosen by alternate setting.** No control request is involved. `SetAlternateSetting` K:`0x20c6` (assembly, observed): on a high-speed link with a source other than `'int '`, the second stream interface gets alternate setting **2** instead of 1.
- The stream interface table `kStreamInterface` K:`0x107a0` lists interface 1 as output and interface 2 as input. So external clock means **capture interface 2, alternate setting 2**. That mapping is inferred from the table order.

Alternate setting table for high speed, `kHighSpeedAltInfo` K:`0x10820` (observed, 28-byte entries):

| Interface | Alt | Channels | Max rate | Use |
| --- | --- | --- | --- | --- |
| 1 | 0 | 0 | | Playback idle |
| 2 | 0 | 0 | | Capture idle |
| 1 | 1 | 8 | 96 kHz | Playback |
| 2 | 1 | 8 | 96 kHz | Capture, internal clock |
| 2 | 2 | 8 | 96 kHz | Capture, S/PDIF clock |

Linux only ever selects alternate setting 1. It probably cannot sync to S/PDIF.

Our driver follows the kext: S/PDIF clock selects alternate setting 2, takes the rate from status bits 8 to 9, and reports lock from bit 12. Rate following and the lock report are untested without an external source (see Clock and sample rate tests).

### USB 1.1 mode

When the link is not high speed, the kext switches to `kFullSpeedAltInfo` K:`0x108e0` (observed). The high-speed flag is set in `CreateAudioInterfaceServices` K:`0x1d8c`.

| Interface | Alt | Channels | Max rate |
| --- | --- | --- | --- |
| 1 | 0 | 0 | |
| 2 | 0 | 0 | |
| 1 | 1 | 2 | 48 kHz |
| 2 | 1 | 2 | 48 kHz |

There is no alternate setting 2 at full speed, so external clock is unavailable on USB 1.1.

### AC-3 passthrough

- AC-3 is a host-side stream format (`'cac3'`) on the same alternate setting 1. K:`0x17fe`, K:`0x1890`, K:`0x1f32`
- Selecting it makes the kext re-apply its clock settings (`SetAC3State` K:`0x10fa`).
- The daemon saves the mixer state and switches the output pairs into AC-3 mode (`SetIsInAC3Mode` D:`0x5bcb2`). No AC-3-specific USB request was found.

### Version readout

No USB request is involved. (D = daemon, P = control panel binary, defined in the next section.)

- `LVersions` exposes two string parameters: driver version (param 0) and firmware version (param 1). Both are read from driver properties (`LVersions::GetParamValueLocal` D:`0x5b0f6`).
- The firmware version is the USB device descriptor's `bcdDevice`, formatted as BCD digits (`-[IORegistryUSBDeviceEntry GetFirmwareVersionString]` D:`0x62630`). `0x0151` displays as 1.51. It shows "n/a" if the property is missing.

### Fader, pan and balance math

Split across three places: the control panel turns UI positions into linear gains, the daemon stores per-channel and per-output gains, and the daemon combines them into one matrix gain per node.

Extra binary for this section:

| Ref | Binary | SHA-256 |
| --- | --- | --- |
| P | Control panel `Panel` (i386) | `f1f052f25cd2d75fb6430d882a06ea7b7d297dbe922906c1ad510ee254f341f6` |

**Fader position to gain** (`MixerUtils::FaderPositionToGain` P:`0x9b350`, assembly, observed). `x` runs from 0.0 to 1.0:

```
x <= 1.19209e-7  ->  silence
x >  0.375       ->  dB = 20 - 40 * 0.5 ^ ((x - 0.375) / 0.625)
otherwise        ->  dB = -75 * 0.266667 ^ (x / 0.375)
gain = 10 ^ (dB / 20)
```

Top of the fader is 0 dB. Position 0.375 is -20 dB. The bottom approaches -75 dB. Both branches meet at -20 dB. `GainToFaderPosition` P:`0x9b22c` is the inverse.

**Pan, for mono sources** (`MixerUtils::PanToStereoGain` P:`0x9b4ca`, assembly, observed). Constant power, -3 dB at center. `p` runs from 0.0 (left) to 1.0 (right):

```
0.4999 <= p < 0.50394  ->  L = R = 0.70710678
p >= 0.9999            ->  L = 0, R = 1
p <= 0.0001            ->  L = 1, R = 0
otherwise              ->  L = cos(p * 1.5708), R = sin(p * 1.5708)
```

**Balance, for linked stereo pairs** (`MixerUtils::BalanceToStereoGain` P:`0x9b41e`, assembly, observed):

```
0.4999 <= b < 0.50394  ->  L = R = 1
b < 0.5                ->  L = 1, R = FaderPositionToGain(2 * b)
otherwise              ->  L = FaderPositionToGain(2 * (1 - b)), R = 1
```

**Combining** (`MixerUtils::CalcLeftAndRightGains` P:`0x9ad7c`): `L = panL * faderGain`, `R = panR * faderGain`. Zero gains are floored to 1e-10 so the later dB conversion stays finite.

**Daemon side:**

- `LChannel::SetLevelsCached` D:`0x2b388` stores each channel as a two-element dB array `[toL, toR]`. With pan state 0 it is `[level, level - 200]`, state 1 is `[level - 200, level]`, state 2 is `[level, level]`.
- Each output pair holds a four-element dB balance matrix. The default is `[0, -200, -200, 0]`, which is identity (`LStereoOutWithStereoInBalance::LoadParamCacheDefaults` D:`0x5703a`).
- `LMixer8::CalcSingleMixerNodeDB` D:`0x4670c` combines them through `CalcMixerNodeDB` D:`0x573c2` (assembly, observed):

```
node_dB = 20 * log10( 10^((a + c) / 20) + 10^((b + d) / 20) )
```

   Here `a` and `b` come from the channel array and `c` and `d` from the balance matrix. Mute and solo force the node to -200 dB. Which array element feeds which argument is inferred; the decompiler loses track of the stack slots.
- The node dB then goes through `ConvertToFirmwareLevel` to the unit 5 request.

- **No-op features:** tuner, dim, mono, low-latency monitoring and button settings are stubs on this device (D:`0x5b886` to `0x5b8d6`, 10 bytes each).

## Hardware observations

Unit: firmware 1.51 (`bcdDevice 0x0151`), high speed, on macOS 26.7.1. Raw dumps are in `captures/`. Only GET requests were sent.

### Descriptors

- All three audio interfaces are vendor class `0xFF`. That is why macOS binds no class driver and Linux needs a quirk.
- Configuration: bus powered, `bMaxPower` 498 mA.
- Playback alt 1 is described as async with explicit feedback on `0x82`. Linux ignores this and uses implicit feedback from `0x81` (L2). Either may work. Implicit feedback is the proven path.
- Format type I on all streaming alts: 8 channels, 3-byte subframes, 24 bits, rates 44100, 48000, 88200, 96000.
- Capture interface 2 has alt 1 and alt 2 with identical descriptors. Alt 2 exists, which fits the S/PDIF clock finding.
- AC header `wTotalLength` is 52. The descriptors that follow total 53 bytes. Firmware bug, harmless.
- MIDI is interface 3 (class `0x01`, subclass 3) with bulk endpoints `0x07` and `0x87`.
- DFU is interface 4 (class `0xFE`). Functional descriptor: download only, detach timeout 2000 ms, transfer size 1024, `bcdDFU` 1.10.

### GET behavior

- Units 5 to 9 answer GET_CUR/MIN/MAX/RES. Every control, valid or not, returns CUR 0, MIN -32768, MAX 0, RES 1. An unknown unit (`0x20`) stalls.
- The firmware does not report real state or ranges through GET. Readback must come from host-side cache, as the vendor daemon does.
- Effect unit 6 GET_CUR returns all zeros.
- Endpoint GET_CUR sample rate works: 48000 on both `0x01` and `0x81` at power-on.
- Meter GET_MEM works. The first read after power-on returns stale memory. Later reads are valid.
- Status word at power-on: `0x0103`. Rate bits read 1 (48 kHz), matching the endpoint readback. Power mode bits read 1 and 1 with the unit on USB bus power only (no PSU).
- After plugging in the PSU while running: status `0x0100`. Both power bits dropped to 0. The device kept the same USB address, so it did not re-enumerate. **1 = bus powered, 0 = PSU.** Bit 1 followed bit 0 without a replug, so the daemon's "startup power mode" name does not mean "mode at enumeration". Its real meaning is unknown.
- The first meter read after the PSU change was stale memory again, and the next showed near full scale on inputs 1 to 6. This looks like the DSP and analog stage restarting.
- On PSU, meters for outputs 1 to 6 mirror inputs 1 to 6, and outputs 7/8 mirror inputs 1/2. On bus power only outputs 1/2 showed signal. This fits the manual's 4×4 bus-power mode (p.2, p.5): the firmware silences analog 3 to 6 itself.
- With no host commands sent, meters for outputs 1 and 2 mirror inputs 1 and 2. The power-on mixer routes input 1 to output 1 and input 2 to output 2.

### Meter read timing

- GET_MEM returns the snapshot taken at the previous GET_MEM, not a live one. This explains the "stale memory" on the first read after power-on.
- To read current meters: send one GET_MEM, discard it, then send another.
- A mixer change shows up in the meters within about 0.3 s (`captures/meter-latency.txt`).

### SET tests

Signal: a steady tone into input 1 from an external player. Outputs unconnected. Logs: `captures/set-*.txt`.

- **Unit 5 gain law confirmed.** Output 1 / source 1 at 0, -6, -12, -20, -40 dB measured 0.0, -6.0, -12.0, -20.0, -40.1 dB relative to input 1. Encoding is dB × 256. `-32768` silences the node.
- **No gain above 0 dB.** +1 dB acts as 0 dB. +3, +6, +12, +127 dB and `0x7FFF` all silence the node. Clamp to 0 dB on the host. This matches GET_MAX 0.
- GET_CUR still returns `0000` after any SET. GET is canned.
- **Effect send works.** Unit 5 control 9, channel 1, at 0 dB puts reverb on outputs 1 to 4: left on 1 and 3, right on 2 and 4. Outputs 7/8 get no return, which matches the manual. Control 9 at OFF removes it after the reverb tail (several seconds on Hall 1).
- **Effect volume works.** Unit 6 control 2 at 127 and 64 gave about 23,500 on output 1. 16 gave 11,800, 0 gave 2,900. 0 does not fully silence the return.
- **Unit 7 shows nothing in the meters.** Controls 1, 2, 3, 7 and 8, channels 0 to 16, at OFF and -20 dB: no change on outputs 1 to 4. So neither the Mac (control 2) nor the Linux (control 7) mapping changes the return level in the output meters.
- Units 8 and 9 (controls 1, 2, channels 0 to 4): no change. Units 10 to 12 stall. Unit 6 controls 5 to 9: accepted, no change. Unit 5 sources 0 and 17 to 20: accepted, no change.
- Meter words 24 and 25 stayed 0 with the reverb active, so they are not the effect return meters. Their meaning is unknown.
- **Listening test overrides the meters for unit 7** (`captures/listen-unit7-2.txt`). On headphone 1, setting unit 7 channels 0 to 8 to OFF cut the reverb sharply, and 0 dB brought it back. This held for control 2 (Mac) and control 7 (Linux). So unit 7 is the effect return level, and the output meters are tapped before it.
- Unit 7 by ear, one change at a time (`captures/listen-unit7-3.txt`, `captures/listen-unit7-ctl1.txt`):
  - Channel 1 cuts the left side (output 1). Channel 2 cuts the right side (output 2).
  - Channel 3 cuts the left side of headphone 2 (output 3). Channel 4 cuts the right side (output 4). Confirmed on headphone 2 (`captures/listen-unit7-ch3.txt`, `captures/listen-unit7-ch4.txt`).
  - Channel 0 (master): no change.
  - Control 1 at a nonzero value mutes both sides. This matches UAC1 mute.
  - Controls 3 and 9: no change. The firmware does not ignore the selector. Controls 2 and 7 are both volume.
- **Implementation:** use the Mac mapping. Unit 7, control 2, channel = output 1 to 4, S16 dB × 256. Use control 1 for mute if needed.
- A USB unplug with the PSU connected does not reset the device. Mixer state and meter memory survive. Only a full power cycle resets it.

### Clock and sample rate tests

Logs: `captures/clock-alt2.txt`, `captures/clock-alt2-b.txt`. Interface 2 was claimed and switched with SET_INTERFACE. No streaming ran.

- **Sample rate:** SET_CUR on either endpoint (`0x01` or `0x81`) changes both. All four rates work. Status bits 8 to 9 read 0, 1, 2, 3 for 44.1, 48, 88.2, 96 kHz. Confirmed.
- **Clock source:** selecting interface 2 alt 2 sets status bit 13 (`0x2100`). Alt 1 or alt 0 clears it. **Bit 13 = external (S/PDIF) clock selected.**
- **S/PDIF input gating:** in alt 1, meter words 6 and 7 (S/PDIF in L/R) read 0. In alt 2 they carry signal. This confirms the manual's C6. After alt 2 → alt 0, the S/PDIF input stayed active until alt 1 was selected.
- **Rate under external clock:** SET_CUR 44100 in alt 2 was ignored. Rate stayed 48000. The host cannot set the rate while on S/PDIF clock.
- **Bit 12 stayed 0** in alt 2 with nothing connected to S/PDIF in. Expected 1 ("external clock invalid").
- **Internal S/PDIF loopback** (`captures/clock-loopback.txt`). With nothing plugged into the S/PDIF jacks, S/PDIF in tracks S/PDIF out exactly in alt 2. Moving source 1 from output 7 to output 8 moved it from S/PDIF in L to R. -12 dB on output 7 showed as -12 dB on S/PDIF in L. The receiver locks to the device's own transmitter, which explains bit 12 = 0. Whether this is in the hardware or the firmware is unknown.
- Consequence: bit 12 and the C4 fallback to 48 kHz cannot be tested on this unit without an external S/PDIF source that breaks the loop. Routing S/PDIF in back to S/PDIF out in the mixer while on external clock would create a feedback loop. The driver should avoid that route.
- A rate change causes a short transient on the inputs (meter spikes to full scale).

### Effect presets by ear

Log: `captures/listen-presets-3.txt`. Metronome into input 1, headphone 2 (effect only). Duration 16256, feedback 64, volume 127.

| Program | Name (Linux/Mac) | Heard |
| --- | --- | --- |
| 0 | Room 1 | Discrete repeats, delay-like |
| 1 | Room 2 | Same as Room 1 |
| 2 | Room 3 | Same as Room 1 |
| 3 | Hall 1 | Reverb |
| 4 | Hall 2 | Reverb-like |
| 5 | Plate | Delay-like |
| 6 | Delay | Delay |
| 7 | Echo | Delay, slightly different from 6 |

- Every program value is accepted and changes the sound. The 1-byte write works.
- Rooms sounding like delay on clicks fits short early reflections at half duration. The three rooms may differ only in size, which a click track hides. Names stay as Linux and the Mac daemon give them.

- **Duration confirmed by ear** (`captures/listen-duration.txt`). On Delay, unit 6 control 3 at 0, 8128, 16256, 32512 (2 bytes LE) gave a longer repeat gap at each step.

- **Feedback confirmed by ear** (`captures/listen-feedback.txt`). On Delay, unit 6 control 4 at 0, 32, 64, 100, 127 (1 byte) gave more repeats at each step.

### Feature units 8 and 9 by ear

Dry metronome from input 1 routed to output 3, effect off. Logs: `captures/listen-unit8*.txt`, `captures/listen-unit9*.txt`.

- **Unit 8:** controls 1 and 2, channels 0 to 16, at mute and OFF: no audible change, tested twice. Unit 8 is not in the input-to-monitor path. Inferred: a trim on the DAW playback or capture path. Testing it needs streaming.
- **Unit 9 is a per-output gain after the mixer** (`captures/listen-unit9-2.txt`):
  - Control 2, channel 3, at -20 dB: output 3 quieter. At OFF: cut out.
  - Control 1, channel 3, nonzero: cut out (UAC1 mute).
  - Not visible in the output meters, so like unit 7 it sits after the meter tap.
  - Channel 3 = output 3 is confirmed. Channels 1 to 8 = outputs 1 to 8 is inferred.
- The Mac daemon only sets units 8 and 9 to 0 dB at startup (`InitializeHardware`). The vendor output fader (X7) is probably folded into the unit 5 node gains on the host. Unit 9 gives our driver a direct option for output volume and mute.

### Streaming prototype

Tool: `proto/ftu_stream.c` (libusb, `make -C proto`). Logs: `captures/stream-*.txt`, `captures/loopback-*.txt`.

- **Isochronous streaming works from user space on macOS 26 / M3 Max.** No kext, no entitlement. libusb claims interfaces 1 and 2 because no driver binds the vendor-class interfaces.
- Setup: claim interfaces 1 and 2, select alt 1 on both (alt 2 on interface 2 for S/PDIF clock), then SET_CUR the rate on `0x81`.
- **Implicit feedback works.** Each playback packet carries as many frames as the matching capture packet. At 48 kHz, capture packets carry 5, 6 or 7 frames, about 91% of them 6. Totals stayed balanced: capture and playback frame counts matched exactly over 60 s.
- All four rates stream with zero transfer errors. Packet sizes: 44.1 kHz 5 or 6 frames; 88.2 kHz 10 to 12; 96 kHz 11 to 13 (13 × 24 = 312 bytes, the endpoint maximum).
- **Queue depth matters.** With 8 transfers of 8 packets (8 ms queued), a resubmit failed with `LIBUSB_ERROR_OTHER` after about 3 s. With 12 transfers of 32 packets (48 ms queued), runs were stable. The HAL plugin path needs a realtime thread and a similar margin.
- **Playback lands where expected.** A -30 dBFS tone on DAW channel 1 read 1036 on meter word 8 (32767 × 10^(-30/20) = 1036). The power-on mixer routes DAW 1 to output 1.
- **Bit-level check through the internal S/PDIF loopback.** DAW 1 routed to output 7, S/PDIF clock selected, S/PDIF in L recorded. A 997 Hz tone at -20 dBFS came back at -20.00 dBFS.
  - Run 1, 60 s: one dropout of 43 zero samples at 12.36 s and a phase jump at 13.87 s.
  - Run 2, 60 s: clean.
  - Both runs: a glitch in the first second while the S/PDIF loop locks.
  - Inferred cause of the run 1 dropout: a host scheduling stall. Phase 8 work.

### MIDI

Test: 5-pin cable from MIDI Out to MIDI In, CoreMIDI scripts in `proto/midi_*.swift`. Logs: `captures/midi-*.txt`.

- macOS binds Apple's USB MIDI class driver to interface 3. CoreMIDI shows "Fast Track Ultra" with one source and one destination. No driver work is needed for basic MIDI.
- Note round trips: 20/20, latency 1.7 to 4.7 ms.
- Channel messages are byte-exact on all 16 channels: note on/off, poly pressure, CC, program change, channel pressure, pitch bend (304 bytes).
- **Firmware bug 1:** a SysEx whose total length leaves remainder 1 when divided by 3 (4, 7, 19 bytes) comes back with junk after `F7`, for example `F7 00 F7 00`. Lengths 5, 6, 8, 9, 11, 15, 27 are exact. 35 bytes came back 2 bytes long.
- **Firmware bug 2: any MIDI OUT bulk transfer over 64 bytes stops all MIDI** until a full power cycle (USB and PSU). Control requests keep working. Confirmed with note bursts in one packet (`captures/midi-burst.txt`): 16 USB-MIDI events (64 bytes) pass byte-exact, 17 events (68 bytes) kill MIDI. The 51-byte SysEx is also 17 events. The 35-byte SysEx is 12. Plan in `docs/firmware.md`.
- **Driver consequence:** Apple's class driver packs as many events as CoreMIDI hands it into one transfer. A DAW sending a dense burst (a chord with CCs, a SysEx dump) can kill MIDI. Workaround: the helper daemon owns interface 3 and caps each bulk OUT transfer at 64 bytes (16 events). On macOS that means detaching Apple's driver, which libusb can only do with root or the device-capture entitlement. Root is acceptable for personal use.

## Still open

- MIDI: which direction adds the SysEx junk bytes. Deferred to Phase 7: once the daemon owns interface 3 it sees raw USB-MIDI packets both ways.

- Bit 12 behavior with a truly absent S/PDIF signal (see Clock tests)
- C4 fallback to 48 kHz with no external signal
- Meaning of meter words 24 and 25
- Meaning of status bit 1 ("startup power mode"). It tracked bit 0 on a live PSU change.
- DSP bypass. No dedicated request found. Possibly done by zeroing effect returns.
- Unit 8 purpose (needs streaming). Unit 9 channels other than 3
- Exact mapping of channel and balance array elements in `CalcSingleMixerNodeDB`
- Native range for unit 5 is -128 to 0 dB (confirmed). Unit 7 range unknown

---

Generated with Claude AI — please review before distribution.
