// midi_loopback: sends MIDI to the Fast Track Ultra's DIN out and checks it arrives on DIN in.
// Needs a 5-pin cable from MIDI Out to MIDI In. Run: swift proto/midi_loopback.swift
import CoreMIDI
import Foundation

func name(_ e: MIDIEndpointRef) -> String {
    var n: Unmanaged<CFString>?
    MIDIObjectGetStringProperty(e, kMIDIPropertyDisplayName, &n)
    return (n?.takeRetainedValue() as String?) ?? ""
}
func find(_ count: Int, _ get: (Int) -> MIDIEndpointRef) -> MIDIEndpointRef? {
    (0..<count).map(get).first { name($0).contains("Fast Track Ultra") }
}
guard let src = find(MIDIGetNumberOfSources(), MIDIGetSource),
      let dst = find(MIDIGetNumberOfDestinations(), MIDIGetDestination) else {
    print("Fast Track Ultra MIDI endpoints not found"); exit(1)
}

var received: [UInt8] = []
var times: [UInt64] = []
let lock = NSLock()
var client = MIDIClientRef(), inPort = MIDIPortRef(), outPort = MIDIPortRef()
MIDIClientCreateWithBlock("ftu-loopback" as CFString, &client, nil)
MIDIInputPortCreateWithBlock(client, "in" as CFString, &inPort) { list, _ in
    var p = list.pointee.packet
    for _ in 0..<list.pointee.numPackets {
        let bytes = withUnsafeBytes(of: p.data) { Array($0.prefix(Int(p.length))) }
        lock.lock(); received += bytes; times.append(mach_absolute_time()); lock.unlock()
        p = MIDIPacketNext(&p).pointee
    }
}
MIDIPortConnectSource(inPort, src, nil)
MIDIOutputPortCreate(client, "out" as CFString, &outPort)

func send(_ bytes: [UInt8]) -> UInt64 {
    var list = MIDIPacketList()
    let pkt = MIDIPacketListInit(&list)
    _ = MIDIPacketListAdd(&list, MemoryLayout<MIDIPacketList>.size, pkt, 0, bytes.count, bytes)
    let t = mach_absolute_time()
    MIDISend(outPort, dst, &list)
    return t
}
var tb = mach_timebase_info(); mach_timebase_info(&tb)
func ms(_ d: UInt64) -> Double { Double(d) * Double(tb.numer) / Double(tb.denom) / 1e6 }


func roundTrips(_ n: Int) -> Int {
    var ok = 0
    for i in 0..<n {
        lock.lock(); received.removeAll(); lock.unlock()
        _ = send([0x90, UInt8(60 + i), 100])
        let deadline = Date().addingTimeInterval(0.3)
        while Date() < deadline { lock.lock(); let c = received.count; lock.unlock(); if c >= 3 { break }; usleep(200) }
        lock.lock(); if received == [0x90, UInt8(60 + i), 100] { ok += 1 }; lock.unlock()
        _ = send([0x80, UInt8(60 + i), 0]); usleep(20000)
    }
    return ok
}
print("notes before: \(roundTrips(5))/5")
usleep(100000)
lock.lock(); received.removeAll(); lock.unlock()
var expected: [UInt8] = []
for ch in 0..<16 {
    let c = UInt8(ch)
    for m: [UInt8] in [[0x90 | c, 64, 127], [0xA0 | c, 64, 50], [0xB0 | c, 7, UInt8(ch * 8)], [0xC0 | c, UInt8(ch)], [0xD0 | c, 33], [0xE0 | c, 0x7F, 0x40], [0x80 | c, 64, 0]] {
        _ = send(m); expected += m; usleep(2000)
    }
}
usleep(500000)
lock.lock(); let got = received; lock.unlock()
print("channel-message burst: sent \(expected.count), received \(got.count), byte-exact: \(got == expected)")
print("notes after burst: \(roundTrips(5))/5")
lock.lock(); received.removeAll(); lock.unlock()
let sx: [UInt8] = [0xF0, 0x7D, 0x01, 0x02, 0x03, 0x04, 0xF7]
_ = send(sx); usleep(1000000)
lock.lock(); let gs = received; lock.unlock()
print("sysex 7 bytes: received \(gs.count) \(gs.map { String(format: "%02X", $0) }.joined(separator: " "))")
print("notes after sysex: \(roundTrips(5))/5")
