// Panel layout, after the vendor Fast Track Ultra control panel: one monitor tab per
// output pair, then settings, meters and about. Presets and status bars follow the
// Ultra 8R panel.

import AppKit
import SwiftUI

let busNames = ["Monitor 1/2", "Monitor 3/4", "Monitor 5/6", "Monitor 7/8"]
let tabNames = busNames + ["Device", "Meters"]
let sourceLabels = ["1", "2", "3", "4", "5", "6", "1", "2", "1", "2", "3", "4", "5", "6", "7", "8"]

func panText(_ p: Double) -> String {
    if abs(p) < 0.005 { return "C" }
    return String(format: "%@%.0f", p < 0 ? "L" : "R", abs(p) * 100)
}

struct ContentView: View {
    @EnvironmentObject var store: Store

    var body: some View {
        VStack(spacing: 0) {
            PresetsBar().padding(.horizontal, 10).padding(.vertical, 6)
            if let st = store.state {
                Picker("Section", selection: $store.tab) {
                    ForEach(tabNames.indices, id: \.self) { Text(tabNames[$0]).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
                .padding(.bottom, 8)
                Divider()
                Group {
                    switch store.tab {
                    case 0..<4: MonitorView(bus: store.tab, state: st)
                    case 4: SettingsView(state: st)
                    default: MetersView()
                    }
                }
                .frame(maxHeight: .infinity, alignment: .top)
            } else {
                Spacer()
                Text(store.reachable ? "Loading…" : "ftu-daemon is not running. Install it with packaging/install.sh.")
                    .foregroundStyle(.secondary)
                Spacer()
            }
            StatusBar()
        }
        .frame(minWidth: 1010, minHeight: 620)
    }
}

struct PresetsBar: View {
    @EnvironmentObject var store: Store

    var body: some View {
        HStack {
            Text("PRESETS").font(.caption).foregroundStyle(.secondary)
            Button("Load…") { store.openPreset() }
            Button("Save as…") { store.savePresetAs() }
            Spacer()
            if let m = store.message {
                Text(m).font(.caption).foregroundStyle(.secondary)
            }
        }
        .controlSize(.small)
    }
}

struct StatusBar: View {
    @EnvironmentObject var store: Store

    var body: some View {
        HStack(spacing: 14) {
            if !store.reachable {
                Text("Daemon not running")
            } else if let s = store.status {
                Text(s.connected ? "Hardware Connected" : "Hardware Not Connected")
                Divider().frame(height: 12)
                Text(s.streaming ? "Streaming \(rateText(s.sample_rate))" : "Not Streaming")
                if let p = s.power {
                    Divider().frame(height: 12)
                    Text(p == "usb" ? "USB bus power (4×4 mode)" : "Power supply")
                }
            }
            Spacer()
        }
        .font(.system(size: 11))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 10).padding(.vertical, 4)
        .background(.bar)
    }
}

func rateText(_ hz: Int?) -> String {
    guard let hz else { return "" }
    return hz % 1000 == 0 ? "\(hz / 1000) kHz" : String(format: "%.1f kHz", Double(hz) / 1000)
}

/// Strips that carry no signal: S/PDIF in needs the external clock (alt 2), and bus
/// power silences analog 3 to 6 (docs/protocol.md, Hardware observations).
func sourceActive(_ s: Int, _ status: DeviceStatus?) -> Bool {
    if (s == 6 || s == 7) && status?.clock != "spdif" { return false }
    if (2...5).contains(s) && status?.power == "usb" { return false }
    return true
}

func busActive(_ b: Int, _ status: DeviceStatus?) -> Bool {
    !((b == 1 || b == 2) && status?.power == "usb")
}

struct MonitorView: View {
    @EnvironmentObject var store: Store
    let bus: Int
    let state: MixerState

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            GroupBox {
                VStack(spacing: 2) {
                    // Centered over their strips: three pairs of analog inputs, one S/PDIF pair.
                    HStack(spacing: 0) {
                        Text("Analog").frame(width: 3 * pairWidth)
                        Text("S/PDIF").frame(width: pairWidth)
                    }
                    .font(.caption.bold())
                    HStack(spacing: 0) { ForEach(0..<4) { p in PairView(bus: bus, pair: p, state: state) } }
                }
            } label: { Text("Hardware Inputs").font(.caption.bold()) }
            GroupBox {
                VStack(spacing: 2) {
                    Text(" ").font(.caption.bold())
                    HStack(spacing: 0) { ForEach(4..<8) { p in PairView(bus: bus, pair: p, state: state) } }
                }
            } label: { Text("Software Returns").font(.caption.bold()) }
            GroupBox {
                VStack(spacing: 2) {
                    Text(" ").font(.caption.bold()) // matches the Analog / S/PDIF header row
                    MasterView(bus: bus, state: state)
                }
            } label: { Text("Master Out").font(.caption.bold()) }
        }
        .padding(8)
    }
}

/// Two strips, 2 pt apart, plus 3 pt padding on each side.
let stripWidth: CGFloat = 48
let pairWidth: CGFloat = 2 * stripWidth + 2 + 6

/// Spoken and tooltip name for a source: "Input 1", "S/PDIF In L", "DAW 3".
func sourceTitle(_ s: Int) -> String {
    switch s {
    case 6: return "S/PDIF In L"
    case 7: return "S/PDIF In R"
    case 0..<6: return "Input \(s + 1)"
    default: return "DAW \(s - 7)"
    }
}

/// Two channel strips with their link button between the upper and lower halves.
struct PairView: View {
    @EnvironmentObject var store: Store
    let bus: Int
    let pair: Int
    let state: MixerState

    var body: some View {
        let active = sourceActive(2 * pair, store.status)
        VStack(spacing: 4) {
            HStack(spacing: 2) { upper(2 * pair); upper(2 * pair + 1) }
            SmallToggle(title: "Link", on: state.link[pair]) { store.setLink(pair: pair, $0) }
            HStack(spacing: 2) { lower(2 * pair); lower(2 * pair + 1) }
        }
        .padding(.horizontal, 3)
        .disabled(!active || !busActive(bus, store.status))
        .opacity(active && busActive(bus, store.status) ? 1 : 0.4)
    }

    @ViewBuilder private func upper(_ s: Int) -> some View {
        let send = state.buses[bus][s]
        VStack(spacing: 4) {
            Text(sourceLabels[s]).font(.caption.bold())
            MeterBar(tap: s < 8 ? .input(s) : .daw(s - 8))
            MiniSlider(name: "\(sourceTitle(s)) \(state.link[pair] ? "balance" : "pan")", value: send.pan, range: -1...1,
                       label: state.link[pair] ? "bal " + panText(send.pan) : panText(send.pan), snapToCenter: 0.08) {
                store.setPan(bus: bus, source: s, $0, commit: $1)
            }
            SmallToggle(title: "Solo", on: send.solo, tint: .yellow) { store.setSolo(bus: bus, source: s, $0) }
            SmallToggle(title: "Mute", on: send.mute, tint: .red) { store.setMute(bus: bus, source: s, $0) }
        }
        .frame(width: stripWidth)
    }

    @ViewBuilder private func lower(_ s: Int) -> some View {
        let send = state.buses[bus][s]
        VStack(spacing: 4) {
            Fader(name: "\(sourceTitle(s)) level", db: send.level) { store.setLevel(bus: bus, source: s, $0, commit: $1) }
            Text(dbText(send.level)).font(.system(size: 10)).monospacedDigit()
            Text("Send").font(.system(size: 10)).foregroundStyle(.secondary)
            MiniSlider(name: "\(sourceTitle(s)) effect send", value: FaderLaw.position(state.fx.sends[s]),
                       label: dbText(state.fx.sends[s])) {
                store.setSend(source: s, FaderLaw.db($0), commit: $1)
            }
        }
        .frame(width: stripWidth)
    }
}

struct MasterView: View {
    @EnvironmentObject var store: Store
    let bus: Int
    let state: MixerState

    var body: some View {
        let o = 2 * bus
        let hasFx = bus < 2 // the effect returns only reach outputs 1 to 4
        VStack(spacing: 4) {
            HStack(spacing: 2) {
                ForEach([o, o + 1], id: \.self) { x in
                    VStack(spacing: 4) {
                        Text("\(x + 1)").font(.caption.bold())
                        MeterBar(tap: .output(x))
                        // Invisible stand-ins keep the faders level with the input strips.
                        MiniSlider(name: "", value: 0, label: " ") { _, _ in }.hidden()
                        SmallToggle(title: "Solo", on: false) { _ in }.hidden()
                        SmallToggle(title: "Mute", on: state.outputs[x].mute, tint: .red) { store.setOutMute(output: x, $0) }
                    }
                    .frame(width: stripWidth)
                }
            }
            SmallToggle(title: "Link", on: state.out_link[bus]) { store.setOutLink(pair: bus, $0) }
            HStack(spacing: 2) {
                ForEach([o, o + 1], id: \.self) { x in
                    VStack(spacing: 4) {
                        Fader(name: "Output \(x + 1) level", db: state.outputs[x].level) { store.setOutLevel(output: x, $0, commit: $1) }
                        Text(dbText(state.outputs[x].level)).font(.system(size: 10)).monospacedDigit()
                        Text("Return").font(.system(size: 10)).foregroundStyle(.secondary).opacity(hasFx ? 1 : 0)
                        if hasFx {
                            MiniSlider(name: "Effect return to output \(x + 1)", value: FaderLaw.position(state.fx.returns[x]),
                                       label: dbText(state.fx.returns[x])) {
                                store.setReturn(output: x, FaderLaw.db($0), commit: $1)
                            }
                        }
                    }
                    .frame(width: stripWidth)
                }
            }
            if hasFx {
                SmallToggle(title: "Bypass", on: state.fx.bypass[bus], tint: .orange) { store.setBypass(bus: bus, $0) }
                    .help("Silences the effect return on this pair")
            }
        }
        .disabled(!busActive(bus, store.status))
        .opacity(busActive(bus, store.status) ? 1 : 0.4)
    }
}

struct SettingsView: View {
    @EnvironmentObject var store: Store
    let state: MixerState
    @State private var rateError: String?

    private func fxRow(_ title: String, _ key: String, _ v: Double) -> some View {
        LabeledContent(title) {
            HStack {
                Slider(value: Binding(get: { v }, set: { store.setFx(key, $0) }), in: 0...1)
                    .frame(width: 260)
                    .accessibilityLabel("Effect \(key)")
                    .accessibilityValue(String(Int((v * 127).rounded())))
                Text(String(Int((v * 127).rounded()))).monospacedDigit().frame(width: 32, alignment: .trailing)
            }
        }
    }

    private var spdif: Bool { store.status?.clock == "spdif" }

    /// S/PDIF in (sources 7 and 8) audible on bus 4, which feeds S/PDIF out.
    private var spdifLoop: Bool {
        state.buses[3][6...7].contains { $0.level != nil && !$0.mute }
    }

    var body: some View {
        Form {
            Section {
                Picker("Clock source", selection: Binding(
                    get: { store.status?.clock ?? "internal" },
                    set: { store.setClock($0) }
                )) {
                    Text("Internal").tag("internal")
                    Text("External (S/PDIF)").tag("spdif")
                }
                .disabled(store.status?.port == "USB 1.1")
                Picker("Sample rate", selection: Binding(
                    get: { store.status?.sample_rate ?? 0 },
                    set: { r in rateError = CoreAudioRate.set(Double(r)) }
                )) {
                    ForEach([44100, 48000, 88200, 96000], id: \.self) { Text(rateText($0)).tag($0) }
                }
                .disabled(spdif)
                if let e = rateError { Text(e).foregroundStyle(.red) }
                if spdif, store.status?.clock_locked == false {
                    Text("No S/PDIF signal. Connect a source to S/PDIF in.").foregroundStyle(.red)
                }
                if spdif, spdifLoop {
                    Text("S/PDIF in is routed to outputs 7/8 (S/PDIF out). On S/PDIF clock this can feed back.")
                        .foregroundStyle(.orange)
                }
            } header: {
                Text("Audio")
            } footer: {
                Text(spdif
                     ? "The S/PDIF source sets the sample rate."
                     : "The sample rate follows Core Audio. S/PDIF input needs the S/PDIF clock.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Effect") {
                Picker("Algorithm", selection: Binding(get: { state.fx.program }, set: { store.setProgram($0) })) {
                    ForEach(programs.indices, id: \.self) { Text(programs[$0]).tag($0) }
                }
                fxRow("Duration", "duration", state.fx.duration)
                fxRow("Feedback", "feedback", state.fx.feedback)
                fxRow("Volume", "volume", state.fx.volume)
            }
        }
        .formStyle(.grouped)
        .frame(maxWidth: 640)
        .frame(maxWidth: .infinity)
    }
}

struct MetersView: View {
    @EnvironmentObject var store: Store

    var body: some View {
        VStack(spacing: 16) {
            row("Hardware Inputs", MeterTap.input, labels: ["1", "2", "3", "4", "5", "6", "S/PDIF L", "S/PDIF R"])
            row("Software Returns", MeterTap.daw, labels: (1...8).map(String.init))
            row("Outputs", MeterTap.output, labels: ["1", "2", "3", "4", "5", "6", "S/PDIF L", "S/PDIF R"])
            Spacer()
        }
        .padding(16)
    }

    private func row(_ title: String, _ tap: @escaping (Int) -> MeterTap, labels: [String]) -> some View {
        GroupBox(title) {
            HStack(spacing: 18) {
                ForEach(0..<8) { i in
                    VStack {
                        MeterBar(tap: tap(i), width: 70)
                        Text(labels[i]).font(.caption2)
                    }
                    .frame(width: 70)
                }
            }
            .padding(6)
        }
    }
}

/// The vendor panel's about tab: status, versions and support links. Its bootloader,
/// hardware model and GUID fields have no known source (the device has no serial string).
struct AboutView: View {
    @EnvironmentObject var store: Store

    private static let repo = "https://github.com/vjustov/m-audio-fast-track-ultra-macos-driver"
    private var panelVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "dev"
    }

    var body: some View {
        let s = store.status
        VStack(alignment: .leading, spacing: 12) {
            Text("Fast Track Ultra Control Panel").font(.title2.bold())
            HStack(alignment: .top, spacing: 12) {
                GroupBox("Status") {
                    Grid(alignment: .leading, verticalSpacing: 4) {
                        field("daemon", store.reachable ? "running" : "not running")
                        field("device", s?.connected == true ? "connected" : "not connected")
                        field("streaming", s.map { $0.streaming ? "yes, \(rateText($0.sample_rate))" : "no" } ?? "n/a")
                        field("port", s?.port ?? "n/a")
                        field("power (current)", powerText(s?.power))
                        field("power (startup)", powerText(s?.startup_power))
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                GroupBox("Versions") {
                    Grid(alignment: .leading, verticalSpacing: 4) {
                        field("panel", panelVersion)
                        field("driver", s?.driver ?? "n/a")
                        field("firmware", s?.firmware ?? "n/a")
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                GroupBox("Support") {
                    VStack(alignment: .leading, spacing: 4) {
                        Link("Source code", destination: URL(string: Self.repo)!)
                        Link("Report a problem", destination: URL(string: Self.repo + "/issues")!)
                        Link("Documentation", destination: URL(string: Self.repo + "/tree/main/docs")!)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .font(.system(size: 11))
            if s?.power == "usb" {
                Text("The device is running on USB bus power. Only analog inputs and outputs 1 and 2 "
                     + "and S/PDIF are available. Connect the power adapter to use all channels.")
                    .font(.system(size: 11))
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
            Text("Community driver for macOS. Not affiliated with M-Audio or Avid.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(20)
    }

    private func field(_ k: String, _ v: String) -> some View {
        GridRow {
            Text(k).foregroundStyle(.secondary).gridColumnAlignment(.trailing)
            Text(v).textSelection(.enabled)
        }
    }

    private func powerText(_ p: String?) -> String {
        switch p {
        case "usb": return "bus powered"
        case "psu": return "power supply"
        default: return "n/a"
        }
    }
}
