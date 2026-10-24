// Mixer controls built from native macOS controls, so they get the system look,
// keyboard control and VoiceOver support.

import SwiftUI

/// Vendor fader law (`MixerUtils::FaderPositionToGain`, docs/protocol.md). Position 1 is
/// 0 dB, 0.375 is -20 dB, the bottom approaches -75 dB, and 0 is off.
enum FaderLaw {
    static func db(_ x: Double) -> Double? {
        if x <= 0.001 { return nil }
        if x > 0.375 { return 20 - 40 * pow(0.5, (x - 0.375) / 0.625) }
        return -75 * pow(0.266667, x / 0.375)
    }

    static func position(_ db: Double?) -> Double {
        guard let db, db > -75 else { return 0 }
        if db >= -20 { return min(1, 0.375 + 0.625 * log((20 - db) / 40) / log(0.5)) }
        return max(0, 0.375 * log(-db / 75) / log(0.266667))
    }
}

func dbText(_ db: Double?) -> String {
    guard let db else { return "-∞" }
    return db > -0.05 ? "0" : String(format: "%.0f", db)
}

/// Tracks a drag locally so only the dragged control redraws. `set(value, commit)` gets
/// commit false during a drag and true once on release, or on every keyboard or
/// VoiceOver step, which arrive without a drag.
struct DragState {
    var value: Double?
    var editing = false

    mutating func change(_ v: Double, _ set: (Double, Bool) -> Void) {
        if editing {
            value = v
            set(v, false)
        } else {
            set(v, true)
        }
    }

    mutating func editingChanged(_ editing: Bool, _ set: (Double, Bool) -> Void) {
        self.editing = editing
        if !editing, let v = value {
            set(v, true)
            value = nil
        }
    }
}

/// Vertical level fader drawn in the system slider's style. A rotated system slider
/// lagged behind the pointer, so this one handles the drag itself; VoiceOver and the
/// keyboard still get a real slider through `accessibilityRepresentation`.
struct Fader: View {
    var name: String
    var db: Double?
    var height: CGFloat = 200
    var set: (Double?, Bool) -> Void
    @State private var dragPosition: Double?

    private let knob: CGFloat = 20

    var body: some View {
        let pos = dragPosition ?? FaderLaw.position(db)
        let shown = dragPosition.map(FaderLaw.db) ?? db
        let travel = height - knob
        ZStack(alignment: .bottom) {
            Capsule().fill(Color.secondary.opacity(0.3)).frame(width: 4)
            Capsule().fill(Color.accentColor).frame(width: 4, height: pos > 0 ? knob / 2 + travel * pos : 0)
            Circle()
                .fill(Color.white)
                .shadow(color: .black.opacity(0.35), radius: 1, y: 0.5)
                .frame(width: knob, height: knob)
                .offset(y: -travel * pos)
        }
        .frame(width: 24, height: height)
        .contentShape(Rectangle())
        .gesture(DragGesture(minimumDistance: 0)
            .onChanged { v in
                let p = min(1, max(0, 1 - (v.location.y - knob / 2) / travel))
                dragPosition = p
                set(FaderLaw.db(p), false)
            }
            .onEnded { _ in
                if let p = dragPosition { set(FaderLaw.db(p), true) }
                dragPosition = nil
            })
        .accessibilityRepresentation {
            Slider(value: Binding(get: { FaderLaw.position(db) }, set: { set(FaderLaw.db($0), true) }), in: 0...1) {
                Text(name)
            }
            .accessibilityValue(shown == nil ? "off" : "\(dbText(shown)) dB")
        }
        .help("\(name): \(dbText(shown)) dB")
    }
}

/// Small horizontal slider with its value printed underneath. Used for pan, sends,
/// returns and effect parameters.
struct MiniSlider: View {
    var name: String
    var value: Double
    var range: ClosedRange<Double> = 0...1
    var label: String
    var width: CGFloat = 44
    /// Values within this distance of the range's centre snap to the centre (pan).
    var snapToCenter: Double = 0
    var set: (Double, Bool) -> Void
    @State private var drag = DragState()

    var body: some View {
        let center = (range.lowerBound + range.upperBound) / 2
        let snap = { (v: Double) in abs(v - center) < snapToCenter ? center : v }
        VStack(spacing: 0) {
            Slider(value: Binding(get: { drag.value ?? value }, set: { drag.change(snap($0), set) }),
                   in: range, onEditingChanged: { drag.editingChanged($0, set) })
                .controlSize(.mini)
                .frame(width: width)
                .accessibilityLabel(name)
                .accessibilityValue(label)
            Text(label).font(.system(size: 10)).monospacedDigit().lineLimit(1).foregroundStyle(.secondary)
        }
        .help("\(name): \(label)")
    }
}

enum MeterTap {
    case input(Int), daw(Int), output(Int)
}

/// Peak meter as a system gauge, reading the device's 16-bit meter words. The fill
/// follows the fader law so 0 dB is full and -20 dB sits at 37.5 %.
struct MeterBar: View {
    @EnvironmentObject var meterStore: MeterStore
    var tap: MeterTap
    var width: CGFloat = 44

    private var word: Int? {
        guard let m = meterStore.meters else { return nil }
        switch tap {
        case .input(let i): return m.in[i]
        case .daw(let i): return m.daw[i]
        case .output(let i): return m.out[i]
        }
    }

    var body: some View {
        let w = abs(word ?? 0)
        let db = w == 0 ? nil : 20 * log10(Double(w) / 32768)
        Gauge(value: FaderLaw.position(db)) { EmptyView() }
            .gaugeStyle(.linearCapacity)
            .tint(Gradient(colors: [.green, .green, .yellow, .red]))
            .frame(width: width)
            .accessibilityLabel("Level")
            .accessibilityValue(db.map { String(format: "%.0f dB", $0) } ?? "silent")
    }
}

/// Button-style toggle, as in Logic's mixer: solo, mute and bypass.
struct SmallToggle: View {
    var title: String
    var on: Bool
    var tint: Color = .accentColor
    var set: (Bool) -> Void

    var body: some View {
        Toggle(title, isOn: Binding(get: { on }, set: set))
            .toggleStyle(.button)
            .controlSize(.small)
            // Colour only when on; an off button in its tint colour reads as active.
            .tint(on ? tint : Color.primary)
            .fixedSize()
    }
}
