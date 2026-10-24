# Fast Track Ultra control panel

SwiftUI app that drives the mixer, effect and outputs through `ftu-daemon`'s XPC control service. The layout follows the vendor Fast Track Ultra panel: one tab per output pair, then Device and Meters. Presets and the status bar follow the Ultra 8R panel.

## Build and run

```sh
panel/build.sh
open "panel/build/Fast Track Ultra.app"
```

The daemon must be installed (`sudo packaging/install.sh`). The app talks to it over the `local.ftu.daemon` Mach service and needs no special permissions.

## Notes

- Controls are native macOS controls, except the level faders. Those are drawn by the app because a rotated system slider lagged behind the pointer. VoiceOver and the keyboard see a system slider through `accessibilityRepresentation`.
- While a slider is dragged, values go to the daemon at up to about 30 requests per second, but the shared state is updated only on release, so the window does not redraw on every mouse event.
- Meters poll at 30 Hz and live in their own observable object for the same reason.
- Sample rate changes go through Core Audio, so the HAL plugin, the daemon and every app agree.
- The fader law is the vendor's (`docs/protocol.md`, Fader, pan and balance math).
- `FTU_SNAPSHOT=out.png FTU_TAB=n` writes the window to a PNG and quits. Useful for checking layout without screen recording permission.
- The icon is drawn by `icon/make_icon.swift`; `build.sh` copies `icon/AppIcon.icns` into the bundle.

---

Generated with Claude AI — please review before distribution.
