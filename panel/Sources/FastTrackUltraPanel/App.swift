import AppKit
import CoreAudio
import SwiftUI

@main
struct PanelApp: App {
    @StateObject private var store = Store()

    init() {
        Snapshot.scheduleIfRequested()
    }

    var body: some Scene {
        // One window: the panel controls a single device, so a second copy (⌘N) would
        // only duplicate it.
        Window("Fast Track Ultra Control Panel", id: "main") {
            ContentView().environmentObject(store).environmentObject(store.meterStore)
        }
        .windowResizability(.contentSize)
        .commands {
            CommandGroup(replacing: .appInfo) { AboutCommand() }
            CommandGroup(replacing: .newItem) {
                Button("Open Preset…") { store.openPreset() }.keyboardShortcut("o")
                Button("Save Preset As…") { store.savePresetAs() }.keyboardShortcut("s", modifiers: [.command, .shift])
            }
            CommandGroup(before: .toolbar) {
                ForEach(tabNames.indices, id: \.self) { i in
                    Button(tabNames[i]) { store.tab = i }
                        .keyboardShortcut(KeyEquivalent(Character(String(i + 1))))
                }
                Divider()
            }
        }

        Window("About Fast Track Ultra", id: "about") {
            AboutView().environmentObject(store).frame(width: 620, height: 330)
        }
        .windowResizability(.contentSize)
    }
}

/// App menu > About opens the about window (it was a tab in the vendor panel).
struct AboutCommand: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("About Fast Track Ultra") { openWindow(id: "about") }
    }
}

/// Development aid: FTU_SNAPSHOT=out.png [FTU_TAB=n] writes the window to a PNG and quits.
/// The app captures its own window, so no screen recording permission is needed.
enum Snapshot {
    static func scheduleIfRequested() {
        guard let path = ProcessInfo.processInfo.environment["FTU_SNAPSHOT"] else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.5) {
            if let view = NSApp.windows.first(where: { $0.isVisible })?.contentView?.superview,
               let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) {
                view.cacheDisplay(in: view.bounds, to: rep)
                try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
            }
            // Menu bar listing next to the image, to check commands and shortcuts.
            let menus = (NSApp.mainMenu?.items ?? []).map { top in
                ([top.title] + (top.submenu?.items ?? []).filter { !$0.isSeparatorItem }.map { item in
                    let mods = item.keyEquivalentModifierMask
                    let key = item.keyEquivalent.isEmpty ? "" : " " + (mods.contains(.shift) ? "⇧" : "") + (mods.contains(.command) ? "⌘" : "") + item.keyEquivalent.uppercased()
                    return "  " + item.title + key
                }).joined(separator: "\n")
            }
            try? menus.joined(separator: "\n").write(toFile: path + ".menus.txt", atomically: true, encoding: .utf8)
            NSApp.terminate(nil)
        }
    }
}

/// Sample rate changes go through Core Audio so the HAL plugin, the daemon and every
/// app agree on the rate.
enum CoreAudioRate {
    static let deviceName = "Fast Track Ultra"

    private static func device() -> AudioObjectID? {
        var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDevices,
                                              mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size) == noErr else { return nil }
        var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &ids) == noErr else { return nil }
        return ids.first { id in
            var nameAddr = AudioObjectPropertyAddress(mSelector: kAudioObjectPropertyName,
                                                      mScope: kAudioObjectPropertyScopeGlobal,
                                                      mElement: kAudioObjectPropertyElementMain)
            var name: Unmanaged<CFString>?
            var n = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
            guard AudioObjectGetPropertyData(id, &nameAddr, 0, nil, &n, &name) == noErr, let s = name?.takeRetainedValue() else { return false }
            return (s as String) == deviceName
        }
    }

    /// Returns an error message, or nil on success.
    static func set(_ rate: Double) -> String? {
        guard let id = device() else { return "\(deviceName) not found in Core Audio" }
        var addr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate,
                                              mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
        var r = rate
        let err = AudioObjectSetPropertyData(id, &addr, 0, nil, UInt32(MemoryLayout<Double>.size), &r)
        return err == noErr ? nil : "Core Audio refused the rate (error \(err))"
    }
}
