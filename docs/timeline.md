# Timeline

A running log of findings, issues and loose ends, so work can pick up across sessions. Newest first.

`plan.md` holds the plan and the verified status. This file holds the trail: what was tried, what broke, what was half done when a session ended.

## How to use it

- Read **Open threads** and the latest entry before starting work.
- Add an entry at the end of each session, or when something surprising turns up. Use absolute dates, and start items with the local time (HH:MM) when it is known.
- Each entry has up to four parts: **Did**, **Found**, **Issues**, **Left off**. Skip the empty ones.
- Link evidence: commits, files, `captures/`, log lines.
- When a thread is resolved, update **Open threads** and say so in that day's entry. Don't edit old entries.

## Open threads

- **Next hardware session: one install, then the runner.** Everything below marked "not on hardware" ships in one install: `make`, then `sudo packaging/install.sh` (restarts coreaudiod; check its CPU afterwards). Then `make hwcheck` (first time: `python3 -I hwtest/run.py --compare-delay` to see the 10 ms playback delay's effect). By hand afterwards: a rate switch and a clock switch from the panel, unplug and replug during playback (the device should go not alive and come back), sleep and wake during playback, `pkill -x ftu-daemon` during playback (clients must not hang), mixer restore after a power cycle. Record results here.
- **Latency.** The start-up correction is committed. Round trip repeats across starts at 48, 88.2 and 96 kHz; after switching down from 88.2 or 96 kHz it was 5 to 12 frames higher (device side, per the simulator). Not on hardware: a 10 ms playback start delay as in the vendor kext (`ctl playback-delay` changes it for comparison), and per-rate latency reported by the plugin (`LatencyFor` in `Driver.cpp`, provisional values from the 2026-10-25 measurements).
- **coreaudiod spin.** Trigger unknown. Only a reboot clears it. Check coreaudiod CPU after every install.
- **External clock rate following.** Core Audio does not follow an S/PDIF source's rate. Lock loss and C4 fallback untested. Needs a real external source.
- **Hardening.** Not on hardware: sleep releases the device and wake reopens it (IOKit notification, `power_shim.c`); unplug and replug set the plugin device not alive and alive again (`device_present`, XPC push). Drift check not started. Soaks so far are 48 kHz only.
- **Residual glitch after a stall.** Each injected stall of 15 ms or more leaves one small phase jump (about 6 samples) roughly 0.7 s after the lead jump. Daemon and plugin counters rule out our positions (2026-10-25 analysis). Likely cause found in simulation: the clock loop took the late backlog callbacks as timing, moved about 40 frames after a 20 ms stall and settled over 2 s, crossing zero near 0.7 s. Fixed in `pacing.rs` (untrusted callbacks only advance the prediction; a trusted one over 2 ms off re-anchors). Not verified on hardware: run a stall injection with the soak on the new build. Also watch for `capture packet of N frames` lines (device FIFO moves).
- **Daemon refactors need a hardware run.** `stream.rs` drives `pacing.rs` (lead controller, packet sizing, clock); the service loop moved to `supervisor.rs`; `ctl.rs` split into `ctl/{api,store,sync}.rs` with no global; counters are a struct. Behaviour is meant to be identical apart from the clock fix, but it has only run in unit tests. Covered by the session plan above.
- **Packet-size logging.** The daemon logs the first 48 capture packet sizes per start (`first capture packets`) and any packet outside the rate's size pattern (`capture packet of N frames`). Read them after the rate matrix and the stall test.
- **Plugin hang fixes written, not installed or tested.** Plugin: every daemon call has a 300 ms reply timeout and the rate wait is capped at 1 s; StartIO connects and sets the rate on a background queue and returns at once; zero timestamps use the daemon clock only when the daemon runs at Core Audio's rate (host clock otherwise, also on S/PDIF clock at a mismatched rate); format and safety-offset notifications are sent after the configuration change returns. Daemon: `ctl` requests run on their own queue. Covered by the session plan above.
- **Parity gaps found in code.** FX sends mode (X11), per-program effect settings (E12), AC-3 passthrough (M3).
- **Test setup still in place.** Cable from output 1 to input 1, and `bus1.in1.mute on` in the mixer to stop feedback. Unmute when the cable comes out. Default system output is set to headphones so other audio stays off the device.
- **Branch.** All of this is on `external-clock`, not merged into `main`.
- **Volume keys.** The plugin exposes no volume or mute control, so macOS ignores the keys. Two options offered 2026-10-24 02:39, no decision yet: hardware gain (plugin forwards a volume and mute control over XPC, daemon sets unit 9 on outputs 1/2) or software gain in the plugin.

## 2026-10-25

Times are local (CEST until 03:00, CET after), from commit times, file times and `~/Library/Logs/ftu-daemon.log`.

**Did**
- 23:08 (2026-10-24) to 06:11 8-hour soak at 48 kHz, output 1 to input 1: 0 glitches, 0 overloads, no lead jumps, 2 callback gaps over 3 ms.
- 23:01 (2026-10-24) Installed libusb's CFRunLoop thread at realtime priority. 23:02 to 23:07 5-minute check soak: 0 glitches, 0 callback gaps over 3 ms. 07:30 committed (`9c8e3c2`).
- 07:34 Wrote `proto/latency.swift`: round trip in device sample time from a burst and cross-correlation. Usage `latency [out] [in] [bursts] [rate]`; the rate argument sets Core Audio's nominal rate first. 07:34:07 to 07:34:44 rate sweep 44.1, 48, 88.2, 96, 48 kHz. Uncommitted.
- 07:35 Changed the daemon to correct the start-up lead error exactly: the stall settle period also runs at start, and the first decision jumps by the whole error. Uncommitted.
- 10:08 Installed the new daemon (restarts coreaudiod).
- 10:30 Updated `plan.md` Status and Next action and `daemon/README.md` (lead control, two realtime threads, counters, `ctl stall`). Uncommitted.
- 10:34 Rebooted. Daemon streaming at 48 kHz from 10:34 with 0 USB errors. At 10:48 coreaudiod was at 0% CPU, so the reboot cleared the spin.

**Found**
- 07:34 Round trip is 1.25 to 1.75 ms (67 frames at 44.1 kHz, 67 to 84 at 48, 115 at 88.2, 120 at 96). Steady within a session, different after each start.
- Cause: the start-up lead correction stopped anywhere inside the ±2-packet band. Daemon log at each start: `lead jump -31 frames (error now -48, smoothed -31.0)`, then 6 to 12 one-frame corrections. The lead starts one transfer short (48 frames at 48 kHz) every time. At 44.1, 88.2 and 96 kHz the first jump was -56 to -62 frames against an error of -88 to -96.
- 10:08:23 With the new build the first start logged `lead jump -48 frames (error now -48, smoothed -48.0)` and no one-frame corrections. Same at the 10:27:28 and 10:27:38 starts. Round trip not measured yet, because coreaudiod was spinning.
- The 8-hour soak spanned the DST change (03:00 back to 02:00), so it ended at 06:10 local time, not 07:10.
- 10:27 to 10:33 While coreaudiod spun, the daemon's USB thread was starved again: isochronous OUT submits failed with `0xe00002ee`, lead jumps of about -384 frames every few seconds (52 jumps, 13 frames dropped, 48 capture errors by 10:33:43). Same symptom as the 2026-10-24 selector deadlock.
- 10:53 to 10:55 Latency sweep on the new daemon, `latency 1 1 3 <rate>` cycling 44.1, 48, 88.2, 96 kHz three times (each run is a stream start, about 2 s). Round trip in frames per cycle: 44.1 kHz 59.27 / 70.95 / 68.95 (spread 4.00 in cycle 1); 48 kHz 62.97 ×3 (and once more at 10:53); 88.2 kHz 86.78 / 85.78 / 85.78; 96 kHz 90.82 ×3. Every start logged one exact lead jump, but the start-up error varied a lot (-48 to -288 frames). 48, 88.2 and 96 kHz now repeat. 44.1 kHz does not, by up to 11.7 frames (about two 5.5-frame packets). Not traced. coreaudiod 9% right after, 0% a minute later.
- 10:57 to 11:05 44.1 kHz traced. The round trip depends on the previous rate: switching down from 88.2 or 96 kHz adds 5 to 12 frames, at 44.1 and 48 kHz alike. Low-to-high switches and 44.1/48 swaps repeat to within about a frame. Matrix (frames): 44.1→48 62.98, 88.2→48 67.98, 96→48 69.98; 48→44.1 61.94 to 62.94, 88.2→44.1 66.95 to 69.94, 96→44.1 70.94 to 74.94; 88.2 and 96 kHz targets vary by at most 2. The daemon lead held 704 to 706 frames at 44.1 kHz, so the daemon is not the source. Hypothesis: after a rate change the device sends a few capture packets at the old size, and implicit feedback copies them into playback. Built a daemon that logs the first 48 capture packet sizes per start (`first capture packets` in the log). Not installed yet: needs `sudo`.
- Second pass on the Mac kext's streaming engine, written up under "Streaming engine" in `protocol.md`. Relevant to the above: the kext starts playback 10 ms after capture (`kInitialInputFramesToSkip`), pairs playback with capture packets by USB frame, corrects the start-up difference one frame per packet, and fills failed capture packets with silence. It reports latency 28+28 frames at 44.1/48, 43+43 at 88.2, 45+45 at 96 kHz.
- Code-only session (device away). Vendor daemon pass, in `protocol.md`: meter words 24/25 are the effect return meters in the vendor stack; status bit 1 is the startup power source; units 8 and 9 are only set to 0 dB at startup; bypass is host-side (unit 7 off); unit 7 uses the unit 5 encoding. New parity gaps in `features.md`: X11 FX sends mode (pre/post fader, panel 2.3.3) and E12 effect settings remembered per program (our daemon keeps one set).
- Plugin review for the coreaudiod spin (not proven as the trigger, since the spin survived removing our plugin). Risks found in `plugin/src/Driver.cpp`: (1) `SetNominalSampleRateImpl` runs inside the HAL's configuration change and blocks there on a synchronous XPC call with no timeout, then polls up to 3 s; (2) it calls `NotifyPropertiesChanged` from inside that configuration change, re-entering the HAL; (3) `OnStartIO` makes synchronous XPC calls with no timeout (`EnsureConnected`, `RequestRate`), so a slow daemon stalls StartIO for every client; (4) the daemon serves `hello`, `set_rate` and `ctl` on one serial queue. Correction, same day: `ctl` does no USB transfers (`Control::handle` only updates state and saves JSON; the service loop writes to the device), so (4) is a minor risk only. Not changed yet.
- Residual 6-sample jump after a stall: in the final stall-test build (2026-10-24 22:44 to 22:47) the stalls left no capture errors, one-frame corrections, fallback packets or plugin input/output jumps, so the daemon positions and plugin timeline stayed continuous. 6 frames is one packet at 48 kHz. Two candidates: the device re-centering its own FIFO after the skipped playback microframes, or the clock DLL being fed a burst of late completions after the stall. The instrumented build now also logs any capture packet outside floor..ceil(rate/8000) (`capture packet of N frames`), which would show a device FIFO move.
- Unit tests for the stream timing. Moved the lead controller, implicit-feedback packet sizing and the clock loop from `stream.rs` into `daemon/src/pacing.rs` (no libusb or shared memory), with a simulated device and event thread in its tests: microframe timing, 1/8000 size patterns, 8-packet transfers, libusb's 4 ms submit-ahead, lost capture when a stall outlasts the 24 ms queue. 10 new tests, 16 in the daemon. They check: one exact jump at start for all rates, a small start error corrected exactly, steady round trip, round trip independent of whole-ms start offsets, stalls of 10 ms change nothing, 15 to 100 ms recover with one jump to the same round trip, stale 96 kHz-sized packets after a switch do not move the round trip on the host side (so the 44.1/48 kHz shift after switching down is device-side), and the clock. Each guarded behaviour was removed once to confirm its test fails. Findings: the lead measurement is biased by up to one transfer when capture and playback start part-way through a millisecond (cannot happen with libusb on macOS, which schedules in whole frames); the clock loop issue above.
- Code-only work for the next hardware session. Sleep: `power_shim.c` (IOKit) asks the service loop to stop the stream and close the device before sleep and waits up to 3 s; after wake the loop reopens it and resends the mixer. Hot-plug: `device_present` in shared memory (version 7, the old pad field) and an XPC push to the plugin, which sets IsAlive; the device is never removed from the plugin's list, to stay out of coreaudiod's device-list reconciling seen in the spin stacks. The plugin also connects once at load to learn presence. The service loop runs against a `Backend` trait; a scripted fake device tests replug, sleep while streaming, sleep while unplugged and rate restarts (35 daemon tests). Playback is submitted 10 ms after capture (vendor kext behaviour; `ctl playback-delay MS` to compare). The plugin reports per-rate stream latency. `hwtest/run.py` (`make hwcheck`) runs the rate matrix, a stall test and a soak and fails on: round trip spread over 1 frame per rate, unreported latency over 2 frames, more than one lead jump or glitch per stall, any soak glitch, coreaudiod over 30% CPU. A dry run without the device first passed on silence (the latency tool returns 0 frames with exit status 0); the runner now requires each burst to come back (gain at least 0.1) and stops after a failed preflight.
- Design cleanup after a review (no audio-path change): `main.rs` 516 → 123 lines; service loop in `supervisor.rs` with the stop and restart decisions (`Watch`, `after_stop`) testable; `ctl.rs` split into `ctl/mod.rs` (shared state), `api.rs`, `store.rs`, `sync.rs`, and the XPC shim gets the `Control` as a context pointer instead of a global; `Counters` struct replaces the 15-element array; contract fixtures in `shared/fixtures/` checked by the daemon (`contract_fixtures_match`) and decoded by new panel tests (`panel/Tests`); top-level `Makefile` (`make`, `make test`); active hardware test tools moved from `proto/` to `hwtest/` (built to `hwtest/build/`; older entries say `proto/`). Tests: 31 daemon, 3 panel.
- AC-3 path decoded (`protocol.md`, AC-3 passthrough): 16-bit IEC 61937 words into the top bits of channels 7/8, other channels left at zero.
- 10:48 After the reboot, Spotlight (`mds`, `mdworker_shared`) was indexing and the load average was about 27. `ftu-daemon` showed 33% CPU in `ps`. No baseline to compare.

**Issues**
- 10:08 to 10:34 coreaudiod at about 100% CPU after an install restarted it. New clients hung: `proto/build/latency` blocked in its first rate change. A `sample` (`sudo sample coreaudiod 3`) showed one thread in `HALC_ShellPlugIn::ProxyObject_PropertiesChanged` → `HALC_ShellSimpleProxyList::Reconcile` waiting on a mach message, and a MIG server thread busy in `HALS_System::GetPropertyData` → `GetPlugInList` / `~BasicDeviceManagerList`. 10:16 sampled, then a plain `killall coreaudiod` came back spinning. It kept spinning with `FastTrackUltra.driver` removed, so our plugin does not sustain it. Telegram, Signal, Slack, Control Center and others ran at 60 to 85% CPU, reloading device lists in a loop. The `sample` matches the 2026-10-24 clock selector deadlock, which makes the selector a weaker suspect.
- Possible triggers from our side: `install.sh` restarts coreaudiod, and each rate change notifies every client. Many earlier installs and rate switches did not trigger it.
- Bisect: a session scratch script (not in the repo) moved the driver out of `/Library/Audio/Plug-Ins/HAL`, restarted coreaudiod, measured CPU for 30 s, then put it back and measured again. Result: spinning both with and without our driver.

**Left off**
- Start-up exact-correction change written, not measured, not committed. Reboot done at 10:34, coreaudiod is clean.
- Next: check coreaudiod CPU, then run `for r in 44100 48000 88200 96000 44100 48000 ...; do proto/build/latency 1 1 3 $r; done` and expect the same round trip per rate across starts. Then add per-rate device latency to the plugin (`kAudioDevicePropertyLatency`), and re-measure: the "unreported" column should go to about 0.

## 2026-10-24

**Did**
- 00:05 to 00:17 First install. Playback and recording verified through Core Audio (`7a6155c`, 00:13): a -20 dBFS tone read 3276 on the DAW 1 meter. 00:17 Ableton Live Lite recorded and played back.
- 00:18 to 00:43 Sample-rate switching from Core Audio at all four rates. Output safety offset cut from 64 ms to 24 ms (`040eca0`).
- 00:55 to 01:18 Mixer control in the daemon with a CLI (`d8831cd`). Verified with a metronome: level, pan law, link, mute, solo and the effect send on the meters; output gain and effect returns by ear; state restored after a full power cycle (01:14).
- 01:19 Reviewed screenshots of the vendor Fast Track Ultra panel and the Ultra 8R panel. Chose the Ultra layout (tabs per output pair) with the 8R's presets bar and status bar.
- 01:24 to 02:17 SwiftUI control panel (`3a33253`). 01:53 macOS HIG review (apple-design skill): fixed text under 10 pt, hit targets, tab count and capitalization. 01:57 app icon (`panel/icon/make_icon.swift`). 02:01 switched to native macOS controls.
- 02:26 to 02:35 Underrun counter fix, shared meter peaks, File and View menu commands, single main window (`af7060b`).
- 02:36 Proposed a hot-plug test without audible sound: a tone to S/PDIF out only, the user unplugs on printed cues while a watcher logs the daemon and Core Audio state. Not run.
- 13:39 to 13:40 Commits: volume keys note (`3299925`), About window (`6ef6d6a`), safe transfer free on failed start (`9c7b012`), external clock (S/PDIF) selection (`ef2affb`).
- 14:41 First run of the live dropout soak `proto/soak.swift` (committed 21:16, `7277053`). Usage `soak <minutes> [out] [in] [dBFS] [dump.raw]`; build with `swiftc -O proto/soak.swift -o proto/build/soak`.
- 13:40 About window shows status, versions and support links like the vendor panel's About tab (`6ef6d6a`).
- 13:40 Transfers are freed safely when a stream fails to start (`9c7b012`, `abort_start` in `stream.rs`).
- 14:41 to 21:14 The daemon log shows 13 stream stops and restarts, most adding 3 USB errors. Cause not recorded.
- 21:16 Lead controller fix committed (`8ee2652`).
- 21:16 to 22:16 1-hour soak, started right after the commits (daemon log from 21:16:44). Glitch bursts at 21:24:10 and 22:11:17.
- 22:23 to 23:01 Stall-injection runs (3 to 100 ms stalls every 15 s during a 4-minute soak): 22:23 to 22:27 on the old controller, 22:45 to 22:47 on the fix. 22:57 Stall test hook `ftu-daemon ctl stall MS`, a log line per lead jump, and a max callback gap that resets per log line (`f319e2f`).
- 23:08 Started the 8-hour soak (see 2026-10-25).

**Found**
- An 8 ms playback queue fails with `kIOReturnIsoTooOld`. libusb schedules each submit at least 4 ms ahead.
- 02:26 2,731 "underruns" on a 3-minute soak were one ring of intended silence counted per packet (2,731 × 6 frames = one 16,384-frame ring) after `install.sh` restarted coreaudiod. Counter fixed (`af7060b`).
- 01:38 Meters lagged the audio by 200 to 300 ms: the device returns the previous snapshot, and the daemon and the panel each polled every 100 ms. Now 20 ms in the daemon and 30 Hz in the panel.
- 02:08 to 02:14 Panel faders felt sluggish. Three causes, fixed in turn: 30 Hz meter updates redrew every control (meters moved to their own observable), every drag event changed the shared state (committed on release instead), and a rotated system `Slider` lagged the pointer (replaced by a drawn fader with `accessibilityRepresentation`).
- 02:33 Clearing the meter peak on every read let the panel steal the CLI's readings. Replaced by a 60 ms sliding window.
- 20:24 to 21:00 (15-minute runs, daemon reinstalled 20:24, 20:44, 20:54) One-frame output slips came from the lead controller sampling at different points of the transfer cycle and correcting about 3,000 times a second. Fixed (`8ee2652`, 21:16): 46 glitches per 15 minutes down to 5.
- 22:23 to 22:47 Remaining glitches were USB thread stalls. Up to 12 ms is harmless; from 15 ms the lead shifts about 345 frames. Reproduced with `ftu-daemon ctl stall MS`. Lead now re-seeds after a gap and fixes it in one jump (`f319e2f`, 22:57). Source was libusb's run-loop thread at normal priority.
- Vendor driver has no volume control, so keyboard volume keys never worked (`3299925`). 02:39 Our plugin has none either (`plugin/src/Driver.cpp`), so macOS ignores the keys.
- Before 13:39 The double alternate-setting selection was not the cause of the S/PDIF hang. Ruled out with `ftu-daemon test --preselect` and `--switch`. A controlled test (`spdif-hang-test.sh`: switch to S/PDIF with audio playing, watch coreaudiod CPU for 120 s) passed, so S/PDIF clock was re-enabled. The earlier S/PDIF hang was left-over state from the selector build.
- Before the one-jump fix, recovering from a disturbance by one frame per 20 ms cost about 85 glitches each.
- 22:16 1-hour soak: 8 glitches in two bursts (21:24:10 and 22:11:17), each a lead jump and a jump back about 60 ms later. No USB errors, fallback packets or FIFO overflows at those times.
- 22:27 Stall injection: stalls up to 12 ms gave no jumps. From 15 ms the raw lead error was about -345 frames at 48 kHz, the same for 20 to 100 ms. Stalls of 50 ms and more also drop audio (2048 zero samples), which no controller change can avoid.
- 21:14:47 The laptop slept once during a soak: a 274 ms callback gap, 3 USB errors, 24 capture and 144 playback errors, then the stream restarted on its own after wake.
- The soak always logs 1 underrun at the end: the writer stopping, not a fault.

**Issues**
- A Core Audio clock selector plus a 250 ms watcher thread deadlocked coreaudiod at load. Both removed. Notes under "Core Audio clock source selector" in `plan.md`. The machine was unusable until `packaging/uninstall.sh` and a reinstall of `af7060b` from a temporary worktree.
- Soak false positives: 50 Hz hum on the analog loop at a 1% threshold (raised to 5%), cable contact noise when the interface was moved, and system audio playing to the device while it was the default output.
- AVAudioEngine stops when the default device changes or the Mac sleeps. The soak now restarts the engine and logs it.
- `sudo` cannot run from the Claude session, not even with the `!` prefix (no terminal for the password). The user runs `sudo packaging/install.sh` in their own terminal. It restarts coreaudiod.
- 00:23 `install.sh` failed with `Bootstrap failed: 5: Input/output error`: `launchctl bootout` returns before the old job is gone. The script now waits and retries.
- 00:48 A request to save the conversation transcript into the repo was stopped by a safety classifier three times. Dropped.
- 01:35 The session has no screen recording permission, so `screencapture` fails. The panel writes its own window with `FTU_SNAPSHOT=out.png FTU_TAB=n`, plus the menu bar to `out.png.menus.txt`. SwiftUI `TabView` on macOS 26 rendered as a blank bar in those snapshots and was replaced.
- macOS restores a saved window frame, so a smaller minimum size only shows after deleting the `NSWindow Frame` default for `local.ftu.panel`.
- Killing the panel to rebuild it looked like a crash to the user. Say so before relaunching.
- Commits are SSH-signed through the Bitwarden agent. Signing fails with "agent refused operation" while Bitwarden is locked.

## 2026-10-23

Times are local (CET/CEST), from the session transcript and commit times.

**Did**
- Before 21:00 (earlier session, times not recorded): plan and feature checklist from the manual. Linux `snd-usb-audio` review pinned to `734bef548a96`. Static analysis of the Mac 2.3.3 daemon and kext and the Windows 6.1.10 driver with REA driving Hopper. Version readout and the vendor fader, pan and balance math.
- 21:10 Device connected (USB only). Descriptors dumped, GET probes, power mode checked with and without the PSU.
- 21:26 to 21:53 Mixer SET tests with a phone tone into input 1, read on the meters. Unit 7 (effect return) by ear on both headphone jacks.
- 21:56 to 22:13 Effect programs, duration and feedback by ear with a metronome; units 8 and 9 by ear. Spoken `say` cues, one step per run.
- 22:13 Started the streaming prototype (`proto/ftu_stream.c`). Clock alt 2 and the internal S/PDIF loopback tested.
- 22:20 to 22:59 MIDI loopback with a 5-pin cable (`proto/midi_*.swift`).
- 22:28 to 22:55 Firmware patching side project opened (`docs/firmware.md`), then paused.
- 23:04 to 23:27 Decided: C++ HAL plugin on libASPL, Rust daemon, LaunchDaemon running as the user (`UserName`). Built the daemon core, XPC service, plugin and packaging.
- 23:34 to 23:58 Restructured for GitHub (GPL-3.0, README, legal notes), committed with the personal identity, pushed to `github.com/vjustov/m-audio-fast-track-ultra-macos-driver` (`a8192be` to `fd2ddb1`).

**Found**
- Interfaces: 0 control (terminals only), 1 playback EP `0x01`, 2 capture EP `0x81` (alt 1 internal clock, alt 2 S/PDIF clock), 3 MIDI (Apple's class driver binds it), 4 DFU. The audio interfaces are vendor class `0xFF`.
- GET requests are canned: every control returns CUR 0, MIN -32768, MAX 0. The host has to keep the mixer state.
- Unit 5 gain is dB × 256, OFF = -32768, maximum 0 dB. +1 dB clamps, +3 dB or more mutes. Control 9 is the effect send, one per source.
- Unit 7 is the effect return (control 2 or 7 volume, control 1 mute, channels 1 to 4 = outputs 1 to 4). Unit 9 is a per-output gain after the mixer. Both sit after the meter tap. Unit 8 had no audible effect.
- GET_MEM meters return the snapshot from the previous read, so read twice.
- Status word: bits 0/1 power mode (1 = bus powered), bits 8 to 9 rate, bit 13 external clock selected. Bit 12 stayed 0 because S/PDIF in mirrors S/PDIF out inside the device in alt 2.
- All eight effect programs change the sound. Rooms 1 to 3, Hall 2 and Plate sound alike on a click track.
- Isochronous streaming works from user space on macOS 26 with libusb and implicit feedback, at all four rates.
- MIDI firmware bugs: any OUT bulk transfer over 64 bytes (17 or more events) stops MIDI until a full power cycle. SysEx whose length leaves remainder 1 when divided by 3 comes back with junk after `F7`.
- Chips on the board: Dream SAM3308B and Cirrus CS42448. A separate USB controller (likely Cypress FX2) is inferred, not confirmed.
- Avid's EULA bans reverse engineering except where the law allows it. Notes in `README.md`.

**Issues**
- Spoken cues inside muted gaps were cut off. Cues now play without inserted silence, one test step per run.
- pyusb could not be imported under `python3 -I`; `python3 -P` works.
- The first meter read after power-on or a PSU change returns stale memory.
- The company pre-commit hook requires signed commits. This repo signs with the personal SSH key (`~/.ssh/id_personal`), never the Parloa keys. `gh` is logged in as the Parloa account, so pushes go over SSH with the personal key.
- The personal key's passphrase was not in the agent and the Bitwarden agent refused `ssh-add`. A temporary `ssh-agent` loaded it from the Keychain (`ssh-add --apple-use-keychain`), removed after use.

---

Generated with Claude AI — please review before distribution.
