# Mac driver 2.3.3: inventory

Phase 2, first pass. Source: `Fast_Track_Ultra_Driver_2_3_3_Mac_83637.dmg`, extracted to `vendor/extracted/mac-2.3.3/`. Vendor files are not in the repository; see the README's legal notes.

Internal codename for the device: **Ripley** (kext class names, firmware header).

## Package contents

| Path (under `Payload/`) | Type | Role |
| --- | --- | --- |
| `Library/Extensions/M-AudioFastTrackUltra.kext` | Kext 2.4.2, x86_64 | Audio driver for 10.9+ |
| `System/Library/Extensions/M-AudioFastTrackUltra.kext` | Kext 2.3.2, i386 + x86_64 | Audio driver for older systems. Same classes. |
| `Library/Application Support/Avid/{1813D6CE-…}/LaunchdDaemon` | i386, Obj-C + C++ | Device server. Holds the device model and all parameter logic. |
| `Library/Application Support/Avid/{1813D6CE-…}/DeviceInterface.dylib` | i386, C++ | Client IPC library. Talks to the daemon. |
| `Library/Application Support/Avid/{1813D6CE-…}/firmware.dfu` | DFU image, 26,552 bytes | Device firmware |
| `Library/LaunchAgents/com.m-audio.fasttrackultra.helper.plist` | Plist | Starts `LaunchdDaemon start`, keep-alive |
| `Applications/M-Audio Fast Track Ultra Control Panel.app` | i386, C++, Carbon/OpenGL UI | Control panel. Skins and strings in XML resources. |
| `Library/PreferencePanes/M-Audio Fast Track Ultra.prefPane` | Universal bundle | Trampoline that opens the control panel |
| `Scripts/InstallDefaultPrefsTool` | i386 | Writes default device prefs at install time |

All binaries are stripped of exports but keep local symbols. C++ and Obj-C names are readable: 879 in the kext, 1,777 in the dylib, 4,731 in the daemon.

## Architecture

```
Control Panel ──AADI_* API──▶ DeviceInterface.dylib ──IPC──▶ LaunchdDaemon ──IOUserClient──▶ kext ──▶ USB
                                                                   │
                                                          PersistentDeviceManager
                                                          (saves and restores state)
```

- **Kext:** streaming plus a thin pass-through. Its user client (`MaudioShDrRipleyUsbDriverUserClient`) exposes `UcUsbClassRequestSet`, `UsbClassRequestGet` and `UcGetExternalClockStatus`. It also handles sample rate, including following an external rate (`SetSampleRateFromUsbFrameListHandler`).
- **DeviceInterface.dylib:** a generic parameter API (`AADI_FindDevices`, `AADI_ConnectDevice`, `AADI_GetParamValue`, `AADI_SetParamValue`, `AADI_RequestParamValues`). It contains no protocol knowledge.
- **LaunchdDaemon:** the brain. It turns high-level parameters into USB class requests and sends them through the kext.

The protocol mapping we need lives almost entirely in `LaunchdDaemon`. That is the primary target for decompilation.

## Daemon device model

Classes relevant to the checklist (`docs/features.md`):

| Class | Notable methods | Checklist rows |
| --- | --- | --- |
| `LFastTrackUltraDevice` | `FetchMeters`, `FetchDeviceStatus`, `Fetch/SetCurrentPowerMode`, `Fetch/SetStartupPowerMode`, `SetIsInAC3Mode`, `SendChannelLevels`, `SendFXSendsLevel`, `SendFXReturnsLevels`, `SendEffectsParam`, `MapChannelToMixerNode`, `StartTimer`/`TimerCallback` | M1, M3, L1 to L3, X1, X6, X10, E1 to E11 |
| `LStereoOutWithStereoInBalance` | `CalcMixerNodeDB`, `SetIsInAC3Mode` | X2 to X5, X7 |
| `LChannel` | `SetLevelsCached` | X1 to X5 |
| `LFXBus`, `LEffectSettings`, `LFXReturns` | `SendPhysicalParams`, `CalcSingleFXReturnDB` | X6, X8, X10, E1 to E11 |
| `LMeters` | `Fetch`, `RequestAll`, `StoreInCache` | L1 to L3 |
| `LPowerMode` | | M1 |
| `LVersions` | | H4 |
| `LAudioStatus`, `LDriver`, `LEngineInterface` | | S4 to S8, C1 to C6 |
| `PersistentDeviceManager`, `LDeviceInstancePersistantStore` | | Restore state on connect |
| `FirmwareLoader_DFU`, `FirmwareLoader_Cypress` | `CompleteLoad`, `EnsureLoader`, `SetFirmwareMode` | Firmware update |

Early inferences from names only. Decompilation must confirm each one.

- **Pan, balance and output faders are host-side math.** `CalcMixerNodeDB` and `MapChannelToMixerNode` suggest the daemon folds pan, mute, solo and bus level into the unit 5 matrix gains that Linux exposes.
- **Meters are polled on a timer** (`StartTimer`, `TimerCallback`, `FetchMeters`).
- **Power mode is software-visible.** There is a current and a startup power mode, not just a hardware sense.
- **AC-3 mode changes mixer behavior** (`SetIsInAC3Mode` on both the device and the output class).
- **The vendor stack restores state from the host.** The daemon persists settings and pushes them on connect, so our driver must do the same.
- Some methods (`SendTunerMode`, `SendDimMode`, `SendMonoMode`, `SendLLMState`, `SendLongButtonPressTime`) look like a shared M-Audio framework. They may be no-ops on this device.

## Firmware

`firmware.dfu` has a standard DFU suffix:

| Field | Value |
| --- | --- |
| Header string | `RIPLEY USB 2.0 FIRMWARE COPYRIGHT AVID 2006-2008` |
| idVendor | `0x0763` |
| idProduct | `0x2080` |
| bcdDevice | `0x0151` (firmware 1.51) |
| bcdDFU | `0x0100` |

Open question: does the daemon flash it automatically when the device reports an older version, or only on request? Matters only if your unit runs old firmware. Normal operation should not need it.

## Kext details

- Matches `IOUSBDevice` with `idVendor 0x0763`, `idProduct 0x2080`. The 8R (`0x2081`) is not listed.
- MIDI on interface 3 is left to Apple's class driver, with an `Out of spec MPS OK` override.
- Stream classes: `MaudioShDrRipleyUsbEngine`, `MaudioShDrRipleyUsbStream`, `MaudioShDrRipleyUsbInterfaceAsyncLinkedRead/Write`.
- Uses an adaptive-read sample rate variance (`GetAdaptiveReadEvenSampleRateVariance`). That fits the implicit feedback scheme Linux describes.

## Next steps

1. Decompile `LFastTrackUltraDevice` in `LaunchdDaemon`. Extract the unit, control and channel numbers for meters, power mode, device status, AC-3 mode and effects.
2. Decompile `CalcMixerNodeDB` and `CalcSingleFXReturnDB` to recover the pan and level math.
3. Recover the parameter ID table (`SetFirstParamID` across modules) to map control panel controls to daemon parameters.
4. Decompile `UcGetExternalClockStatus` and the clock source path in the kext.

---

Generated with Claude AI — please review before distribution.
