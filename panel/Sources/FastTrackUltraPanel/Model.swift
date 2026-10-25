// Mirrors the daemon's JSON (daemon/src/mixer.rs and ctl/api.rs) and keeps the panel in
// sync with it. Edits update the local copy at once and reach the daemon in batches.

import AppKit
import Foundation
import SwiftUI

struct Send: Codable, Equatable {
    var level: Double?
    var pan: Double
    var mute: Bool
    var solo: Bool
}

/// Settings one effect program remembers (daemon: mixer.rs FxSettings).
struct FxSettings: Codable, Equatable {
    var volume: Double
    var duration: Double
    var feedback: Double
}

struct Fx: Codable, Equatable {
    var program: Int
    var volume: Double
    var duration: Double
    var feedback: Double
    /// Per program, restored on a program change. Optional: older daemons lack it.
    var programs: [FxSettings]?
    var sends: [Double?]
    var returns: [Double?]
    var bypass: [Bool]
}

struct Output: Codable, Equatable {
    var level: Double?
    var mute: Bool
}

struct MixerState: Codable, Equatable {
    var buses: [[Send]]
    var link: [Bool]
    var fx: Fx
    var outputs: [Output]
    var out_link: [Bool]
}

struct Meters: Decodable {
    var `in`: [Int]
    var daw: [Int]
    var out: [Int]
    var status: Int
}

struct DeviceStatus: Decodable, Equatable {
    var connected: Bool
    var streaming: Bool
    var sample_rate: Int?
    /// Clock setting: "internal" or "spdif".
    var clock: String
    /// On S/PDIF clock while streaming: whether the device reports a valid signal.
    var clock_locked: Bool?
    var port: String?
    var power: String?
    var startup_power: String?
    var firmware: String?
    var driver: String
}

let programs = ["Room 1", "Room 2", "Room 3", "Hall 1", "Hall 2", "Plate", "Delay", "Echo"]

/// Source index 0..15 to the daemon's name: in1..in8, daw1..daw8.
func sourceName(_ s: Int) -> String { s < 8 ? "in\(s + 1)" : "daw\(s - 7)" }

private func dbValue(_ db: Double?) -> String { db.map { String(format: "%.2f", $0) } ?? "off" }
private func onOff(_ b: Bool) -> String { b ? "on" : "off" }

/// Meter readings, kept apart from `Store` so their 30 Hz updates redraw only the
/// meters instead of every control in the window.
@MainActor
final class MeterStore: ObservableObject {
    @Published var meters: Meters?
}

@MainActor
final class Store: ObservableObject {
    @Published var state: MixerState?
    /// Selected tab: 0 to 3 monitor pairs, 4 Device, 5 Meters. Shared with the View menu.
    @Published var tab = Int(ProcessInfo.processInfo.environment["FTU_TAB"] ?? "") ?? 0
    let meterStore = MeterStore()
    @Published var status: DeviceStatus?
    @Published var reachable = true
    @Published var message: String?

    private let client = DaemonClient()
    private var pending: [(String, String)] = []
    private var flushScheduled = false
    private var lastEdit = Date.distantPast
    private var timers: [Timer] = []

    init() {
        refreshState()
        refreshStatus()
        timers.append(Timer.scheduledTimer(withTimeInterval: 1.0 / 30, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refreshMeters() }
        })
        timers.append(Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.refreshStatus()
                // Pick up changes from other clients (the CLI), but not mid-drag.
                if let self, Date().timeIntervalSince(self.lastEdit) > 1.0 { self.refreshState() }
            }
        })
    }

    private func decode<T: Decodable>(_ type: T.Type, _ obj: Any?) -> T? {
        guard let obj, JSONSerialization.isValidJSONObject(obj),
              let data = try? JSONSerialization.data(withJSONObject: obj)
        else { return nil }
        return try? JSONDecoder().decode(T.self, from: data)
    }

    func refreshState() {
        client.request(["get": "state"]) { [weak self] r in
            guard let self else { return }
            if self.reachable != (r != nil) { self.reachable = r != nil }
            if let s = self.decode(MixerState.self, r), s != self.state, Date().timeIntervalSince(self.lastEdit) > 1.0 {
                self.state = s
            }
        }
    }

    func refreshStatus() {
        client.request(["get": "status"]) { [weak self] r in
            guard let self else { return }
            if self.reachable != (r != nil) { self.reachable = r != nil }
            let st = self.decode(DeviceStatus.self, r)
            if st != self.status { self.status = st }
        }
    }

    func refreshMeters() {
        client.request(["get": "meters"]) { [weak self] r in
            guard let self else { return }
            guard var m = self.decode(Meters.self, r) else {
                self.meterStore.meters = nil
                return
            }
            // Fall back by about 20 dB per second instead of dropping at once.
            if let old = self.meterStore.meters {
                let fall = { (new: [Int], prev: [Int]) in zip(new, prev).map { max($0, Int(Double($1) * 0.93)) } }
                m.in = fall(m.in, old.in)
                m.daw = fall(m.daw, old.daw)
                m.out = fall(m.out, old.out)
            }
            self.meterStore.meters = m
        }
    }

    // MARK: Sending

    private func send(_ path: String, _ value: String) {
        lastEdit = Date()
        pending.removeAll { $0.0 == path }
        pending.append((path, value))
        guard !flushScheduled else { return }
        flushScheduled = true
        // Coalesce fader and knob drags into at most ~30 requests per second.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.03) { [weak self] in
            guard let self else { return }
            self.flushScheduled = false
            let batch = self.pending.map { [$0.0, $0.1] }
            self.pending.removeAll()
            self.client.request(["set": batch]) { r in
                if let err = (r as? [String: Any])?["error"] as? String { self.message = err }
            }
        }
    }

    /// Source plus its stereo partner when the pair is linked.
    private func linked(_ s: Int) -> [Int] {
        guard let st = state, st.link[s / 2] else { return [s] }
        return [s, s ^ 1]
    }

    // Setters for continuous controls take `commit`. While a slider is being dragged it
    // passes false: the value goes to the daemon, but the published state (which would
    // redraw every strip) changes only once, on release.

    func setLevel(bus b: Int, source s: Int, _ db: Double?, commit: Bool = true) {
        for x in linked(s) {
            if commit { state?.buses[b][x].level = db }
            send("bus\(b + 1).\(sourceName(x)).level", dbValue(db))
        }
    }

    func setPan(bus b: Int, source s: Int, _ pan: Double, commit: Bool = true) {
        for x in linked(s) {
            if commit { state?.buses[b][x].pan = pan }
            send("bus\(b + 1).\(sourceName(x)).pan", String(format: "%.3f", pan))
        }
    }

    func setMute(bus b: Int, source s: Int, _ on: Bool) {
        for x in linked(s) {
            state?.buses[b][x].mute = on
            send("bus\(b + 1).\(sourceName(x)).mute", onOff(on))
        }
    }

    func setSolo(bus b: Int, source s: Int, _ on: Bool) {
        for x in linked(s) {
            state?.buses[b][x].solo = on
            send("bus\(b + 1).\(sourceName(x)).solo", onOff(on))
        }
    }

    /// Linking copies the left source's settings to the right one on every bus, so the
    /// pair starts out matched.
    func setLink(pair p: Int, _ on: Bool) {
        state?.link[p] = on
        send("link.\(sourceName(2 * p))", onOff(on))
        guard on, let st = state else { return }
        for b in st.buses.indices {
            let l = st.buses[b][2 * p]
            setLevel(bus: b, source: 2 * p, l.level)
            setPan(bus: b, source: 2 * p, l.pan)
            setMute(bus: b, source: 2 * p, l.mute)
            setSolo(bus: b, source: 2 * p, l.solo)
        }
        setSend(source: 2 * p, st.fx.sends[2 * p])
    }

    func setSend(source s: Int, _ db: Double?, commit: Bool = true) {
        for x in linked(s) {
            if commit { state?.fx.sends[x] = db }
            send("fx.send.\(sourceName(x))", dbValue(db))
        }
    }

    func setReturn(output o: Int, _ db: Double?, commit: Bool = true) {
        if commit { state?.fx.returns[o] = db }
        send("fx.return.\(o + 1)", dbValue(db))
    }

    func setBypass(bus b: Int, _ on: Bool) {
        state?.fx.bypass[b] = on
        send("fx.bypass.\(b + 1)", onOff(on))
    }

    private func outs(_ o: Int) -> [Int] {
        guard let st = state, st.out_link[o / 2] else { return [o] }
        return [o, o ^ 1]
    }

    func setOutLevel(output o: Int, _ db: Double?, commit: Bool = true) {
        for x in outs(o) {
            if commit { state?.outputs[x].level = db }
            send("out\(x + 1).level", dbValue(db))
        }
    }

    func setOutMute(output o: Int, _ on: Bool) {
        state?.outputs[o].mute = on
        send("out\(o + 1).mute", onOff(on))
    }

    func setOutLink(pair p: Int, _ on: Bool) {
        state?.out_link[p] = on
        send("link.out\(2 * p + 1)", onOff(on))
        if on, let l = state?.outputs[2 * p].level { setOutLevel(output: 2 * p, l) }
    }

    func setClock(_ c: String) {
        client.request(["clock": c]) { [weak self] r in
            guard let self else { return }
            if let err = (r as? [String: Any])?["error"] as? String { self.message = err }
            self.refreshStatus()
        }
    }

    func setProgram(_ i: Int) {
        // Mirror the daemon at once: keep the old program's settings, restore the new one's.
        if var fx = state?.fx, var p = fx.programs, p.indices.contains(fx.program), p.indices.contains(i) {
            p[fx.program] = FxSettings(volume: fx.volume, duration: fx.duration, feedback: fx.feedback)
            fx.volume = p[i].volume
            fx.duration = p[i].duration
            fx.feedback = p[i].feedback
            fx.programs = p
            state?.fx = fx
        }
        state?.fx.program = i
        send("fx.program", String(i))
    }

    func setFx(_ name: String, _ v: Double, commit: Bool = true) {
        if commit { switch name {
        case "volume": state?.fx.volume = v
        case "duration": state?.fx.duration = v
        default: state?.fx.feedback = v
        } }
        send("fx.\(name)", String(format: "%.4f", v))
    }

    // MARK: Settings files

    func openPreset() {
        let p = NSOpenPanel()
        p.allowedContentTypes = [.json]
        if p.runModal() == .OK, let url = p.url { load(from: url) }
    }

    func savePresetAs() {
        let p = NSSavePanel()
        p.allowedContentTypes = [.json]
        p.nameFieldStringValue = "Fast Track Ultra.json"
        if p.runModal() == .OK, let url = p.url { save(to: url) }
    }

    func save(to url: URL) {
        guard let st = state else { return }
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        do {
            try enc.encode(st).write(to: url)
            message = "Saved \(url.lastPathComponent)"
        } catch {
            message = "Save failed: \(error.localizedDescription)"
        }
    }

    func load(from url: URL) {
        guard let data = try? Data(contentsOf: url),
              let obj = try? JSONSerialization.jsonObject(with: data),
              decode(MixerState.self, obj) != nil
        else {
            message = "\(url.lastPathComponent) is not a Fast Track Ultra settings file"
            return
        }
        client.request(["put": obj]) { [weak self] r in
            guard let self else { return }
            if let err = (r as? [String: Any])?["error"] as? String {
                self.message = err
            } else {
                self.message = "Loaded \(url.lastPathComponent)"
                self.lastEdit = .distantPast
                self.refreshState()
            }
        }
    }
}
