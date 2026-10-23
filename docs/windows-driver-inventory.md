# Windows driver 6.1.10: inventory

Phase 3, first pass. Source: `FastTrackUltra_6_1_10_Win_83292.zip`, extracted to `vendor/extracted/win-6.1.10/`. Vendor files are not in the repository.

The ZIP holds one InstallShield `setup.exe`. It wraps two MSI packages (`MSI32`, `MSI64`) and two VC2005 redistributables. 7-Zip (`7zz`) unpacks all of them.

## Package contents

From `ReadMeTXT` and file types. Names are the MSI stream names.

| MSI stream | Installed name | Version | Role |
| --- | --- | --- | --- |
| `StreamingSYS` / `StreamingINF` | `MAudioFastTrackUltra.sys` | 5.10.0.5107 | Kernel audio driver (x86 and x64) |
| `DfuSYS` / `DfuINF` | `MAudioFastTrackUltra_DFU.sys` | 1.2.0.2 | Kernel driver for firmware update mode |
| `DriverInterfaceEXE` | `DriverInterface.exe` | 1.0.6 | Device server. Same role as the Mac `LaunchdDaemon`. |
| `DeviceInterface32DLL` / `64DLL` | `DeviceInterface.dll` | 1.0.1 | Client IPC library |
| `PanelEXE` | `Panel.exe` | 1.1.5 | Control panel |
| `AudioDevMonServiceEXE` | `AudioDevMon.exe` | 1.0.1 | Service that watches for firmware update requests |
| `FirmwareUpdaterEXE` | `FirmwareUpdater.exe` | 1.1.0 | Firmware flasher |
| `FirmwareImageDFU` | `FastTrackUltra_1.51.dfu` | 1.51 | Firmware image |
| `Asio32DLL` / `Asio64DLL` | `MAudioFastTrackUltra_Asio*.dll` | 5.2.1 | ASIO driver |
| `ProToolsMDF32DLL` | `MAudioFastTrackUltra_ProTools.dll` | 5.2.1 | Pro Tools plugin |
| `DriverReplacerEXE`, `IniFileManagerDLL` | | | Install helpers |

## Findings

- **Same code base as the Mac driver.** `DriverInterface.exe` carries RTTI for the same classes: `LFastTrackUltraDevice`, `LUSBAudioClassRequestTargetV1`, `LMeters`, `LFXBus`, `LFXReturns`, `LPowerMode`, `LVersions`, and the rest. It is the Windows build of the Mac daemon. Windows binaries carry no symbol names, so the Mac build stays the primary target.
- **Firmware is identical.** `FirmwareImageDFU` matches the Mac `firmware.dfu` byte for byte (version 1.51).
- **DFU mode has its own USB ID.** `DfuINF` matches `USB\VID_0763&PID_2880`. The device re-enumerates as `0763:2880` to take a firmware update. `DriverInterface.exe` has an `LDFUDetach` class for that switch.
- **Audio driver match.** `StreamingINF` binds to `USB\VID_0763&PID_2080` and its interface 0. Like the Mac kext, it does not list the 8R (`0x2081`).

## Not examined

- `MAudioFastTrackUltra.sys` disassembly. The Mac kext already answered the clock-source and USB 1.1 questions it might have settled.
- `LVersions` in either build, for the version readout.

---

Generated with Claude AI — please review before distribution.
