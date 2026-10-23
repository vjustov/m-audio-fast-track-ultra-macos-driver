# Firmware patching: side project

Exploratory. Not needed for the driver. The driver works around the firmware bugs listed below.

## Goal

Fix the MIDI bugs in the device firmware, and learn enough about the image to patch it safely. Shipping a patched firmware is out of scope until a recovery path is proven.

## Why

Firmware bugs found on hardware (`docs/protocol.md`, MIDI section; logs in `captures/midi-*.txt`):

- A SysEx whose total length leaves remainder 1 when divided by 3 (4, 7, 19 bytes) comes back with junk bytes after `F7` (`F7 00 F7 00`). Other lengths are exact. Which direction adds the junk (USB to DIN, or DIN to USB) is unknown.
- Any MIDI OUT bulk transfer over 64 bytes (more than 16 USB-MIDI events) stops all MIDI until a full power cycle. Confirmed with note bursts: 16 events pass, 17 kill it (`captures/midi-burst.txt`). The endpoint descriptor says 512 bytes. The firmware likely sizes its buffer for full-speed packets (64 bytes).
- Other candidates, low priority: GET requests that return canned values instead of real state, and the AC header `wTotalLength` off by one.

## What we know

| Fact | Value | Source |
| --- | --- | --- |
| Running version | 1.51 (`bcdDevice 0x0151`) | Device descriptor |
| Newest shipped version | 1.51. Mac 2.3.3 and Windows 6.1.10 carry byte-identical images. | `docs/windows-driver-inventory.md` |
| Image | `firmware.dfu`, 26,552 bytes | Mac package |
| Header | Magic `RD`, 2 bytes, then `RIPLEY USB 2.0 FIRMWARE COPYRIGHT AVID 2006-2008` | Offset `0x00` |
| Suffix | Standard DFU suffix: VID `0x0763`, PID `0x2080`, `bcdDevice 0x0151`, `bcdDFU 0x0100`, CRC | Last 16 bytes |
| Encryption or compression | Probably neither. Entropy is 3.1 to 6.2 bits per byte across 4 KB blocks. | Measured |
| Data at `0x36` | A table of descending 16-bit LE words starting at `0x7FFF`. Inferred: a gain or log lookup table. | Hex dump |
| DFU interface | Interface 4, class `0xFE` subclass 1 | Config descriptor |
| DFU attributes | Download only (`bmAttributes 0x01`). No upload, so the current flash cannot be read back over DFU. Not manifestation tolerant. | DFU functional descriptor |
| DFU limits | Detach timeout 2000 ms, transfer size 1024, DFU 1.1 | DFU functional descriptor |
| DFU mode ID | `0763:2880` after detach | Windows `DfuINF` |
| Vendor update code | Mac daemon: `FirmwareLoader_DFU`, `FirmwareLoader_Cypress`, `CompleteLoad`, `EnsureLoader`, `SetFirmwareMode`. Windows: `LDFUDetach`, `FirmwareUpdater.exe`, `AudioDevMon.exe`. | Symbol and RTTI scan |

## Hardware

Chips on the board, read by the user (2026-10-23):

- **Dream SAM3308B**: audio DSP. The first reading was "SAM3808". A closer look confirmed SAM3308B. Datasheet: [docs.dream.fr](https://docs.dream.fr/pdf/Serie3000/SAM_Datasheets/SAM3308B.pdf), rev. 07/06/11.
- **Cirrus Logic CS42448**: 24-bit codec, 6 ADCs and 8 DACs, up to 192 kHz ([cirrus.com](https://cirrus.com/products/cs42448)). This matches the 6 analog inputs, and the 6 analog outputs plus spare DACs. It holds no firmware.

### SAM3308B architecture (from its datasheet)

- **P16:** Dream's own 16-bit control processor, carried over from the SAM97xx series. It runs the firmware. This is a proprietary instruction set. No public disassembler is known.
- **8 × P24:** 24-bit DSPs, each with 2k × 24 RAM (code and data) and 1k × 24 ROM. They run the mixer and effects. The P16 loads their code over the internal async bus.
- **Built-in 1k × 16 ROM** with a BIOS (UART, DataFlash, host I/O) and a debugger.
- **Firmware storage:** runs from external parallel ROM/Flash, or is downloaded at power-up into the 16k × 24 internal RAM from serial EEPROM, DataFlash, SmartMedia or a host.
- **ROM debugger:** pins STIN (34) and STOUT (35), 57.6 kbaud async serial. If STIN is high at power-up, the built-in debugger starts. **This is a likely recovery path,** since the debugger lives in mask ROM and a bad flash cannot erase it. Pin numbers are for the LQFP100 package.
- **On-chip MIDI UART** (MIDI_IN, MIDI_OUT). The DIN jacks may connect here or to a UART on the USB controller. Either way, the 64-byte bug sits on the USB side: the bulk endpoint buffer belongs to the USB controller's firmware.
- **The SAM3308B has no USB.** Its host interfaces are a serial slave port, UARTs and a parallel MPU port. A separate USB 2.0 controller must sit on the board. The Mac daemon's `FirmwareLoader_Cypress` class points to a Cypress EZ-USB FX2 (an 8051), but that is unconfirmed.

### What this means for the image

- **USB descriptors sit in the image verbatim** (offsets in `firmware.dfu`):
  - Device descriptor at `0x1EDE`.
  - Interface association descriptor at `0x1ED6`.
  - 35 of the 38 configuration sub-descriptors, including a 293-byte contiguous run from `0x1F1F`.
  - "Fast Track Ultra" in UTF-16LE at `0x1DCE`, "M-Audio" at `0x30A`.
  - The 9-byte configuration header is not stored verbatim. The firmware probably builds it at runtime. It holds the power attributes, which fits the bus/PSU behavior.
  - These are the anchors for disassembly. Code that reads `0x1EDE` is the GET_DESCRIPTOR handler, and the USB stack sits next to it.
- The 16-bit word patterns in `firmware.dfu` fit P16 code. The descending table at `0x36` fits DSP gain coefficients.
- The image holds the USB descriptors, so at least part of it belongs to the USB controller, not the SAM. It may be one combined image: USB controller code plus SAM P16 and P24 code that the controller loads into the SAM at power-up. Which byte ranges belong to which chip is unknown.
- If the USB controller is an FX2, its code is 8051. Free disassemblers (Ghidra, IDA) handle that. That would make the USB and MIDI-over-USB side the easy part to patch.
- Disassembling needs the P16 instruction set. Options: Dream's SDK or tools (not public), older SAM97xx documentation, or reverse engineering the encoding from known code paths (USB descriptor handling, the effect parameter writes).

Unknown:

- The USB controller chip. Likely a Cypress CY7C68013 (FX2) or CY7C68013A (FX2LP), 56 to 128 pins.
- Where the firmware is stored on the board. Look for a small 8-pin chip (a 24-series I2C EEPROM next to an FX2, or Atmel AT45DB DataFlash or 25-series SPI flash next to the SAM). If present, it can be read and written directly with a clip and an SPI programmer. That gives a full backup and a recovery path independent of DFU.
- Whether the SAM3308B ROM debugger pins (STIN 34, STOUT 35) reach test pads.
- What the `RD` header word and the 2 bytes after it mean (format tag, length or checksum).
- Whether the device verifies anything beyond the DFU suffix CRC.

## Risks

- **Bricking.** If the loader is in rewritable flash, a bad image can leave the unit dead with no USB recovery. Do not flash anything modified until a recovery path is known.
- **No read-back over USB.** DFU upload is not supported. A backup needs direct access to the flash chip, if there is one.
- **Proprietary instruction set.** The P16 has no public tooling. Disassembly may take the most effort of any step.
- **Hidden checks.** A checksum inside the image (beyond the DFU CRC) would reject or break a patched build.

## Plan

Each step stands alone. Stop at any point.

1. **Identify the processors.** DSP done: Dream SAM3308B (P16 control core, P24 DSP array). Remaining: the USB controller. Descriptors located in the image (see above). Remaining: find the firmware storage chip.
2. **Recover the vendor update protocol.** Decompile `FirmwareLoader_DFU` and `SetFirmwareMode` in the Mac daemon. Learn the detach sequence, any vendor-specific requests, and whether the whole file (header included) or only part of it is sent.
3. **Find the recovery path.** Two candidates: the SAM ROM debugger (STIN/STOUT serial, STIN high at power-up), and direct SPI access to the flash chip if one exists. No modified flash before one of them works.
4. **Rehearse with the stock image.** Reflash 1.51 with `dfu-util` and confirm the device comes back. This proves the tooling end to end with zero functional change.
5. **Locate the MIDI code.** Find the bulk endpoint `0x07`/`0x87` handlers and the USB-MIDI event packing. Find the 64-byte buffer and the remainder-1 SysEx bug in code.
6. **Patch, re-sign, flash.** Only after steps 3 and 4. Recompute any internal checksum and the DFU suffix CRC.

## Workaround in the driver (independent of this project)

- Take over interface 3 in the helper daemon instead of Apple's class driver, if needed, and split outgoing SysEx so no bulk transfer exceeds 64 bytes.
- Strip the junk `F7 00 ...` bytes after a SysEx end on input.
- The 64-byte limit is confirmed. The junk-byte direction is not.

---

Generated with Claude AI — please review before distribution.
