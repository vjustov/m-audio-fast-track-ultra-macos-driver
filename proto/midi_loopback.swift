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

// 1. Latency: single note-ons, one at a time.
var lat: [Double] = []
for i in 0..<20 {
    lock.lock(); received.removeAll(); times.removeAll(); lock.unlock()
    let t0 = send([0x90, UInt8(60 + i), 100])
    let deadline = Date().addingTimeInterval(0.5)
    while Date() < deadline { lock.lock(); let n = received.count; lock.unlock(); if n >= 3 { break }; usleep(200) }
    lock.lock()
    if received == [0x90, UInt8(60 + i), 100], let t1 = times.first { lat.append(ms(t1 - t0)) }
    lock.unlock()
    _ = send([0x80, UInt8(60 + i), 0]); usleep(20000)
}
print("note round trips: \(lat.count)/20 ok" + (lat.isEmpty ? "" : String(format: ", latency min %.2f ms, max %.2f ms", lat.min()!, lat.max()!)))

// 2. Byte-exact burst: all 16 channels, notes, CCs, pitch bend, program change, plus a SysEx.
usleep(100000)
lock.lock(); received.removeAll(); lock.unlock()
var expected: [UInt8] = []
for ch in 0..<16 {
    let c = UInt8(ch)
    let msgs: [[UInt8]] = [[0x90 | c, 64, 127], [0xB0 | c, 7, UInt8(ch * 8)], [0xE0 | c, 0x7F, 0x40], [0xC0 | c, UInt8(ch)], [0x80 | c, 64, 0]]
    for m in msgs { _ = send(m); expected += m; usleep(2000) }
}
let sysex: [UInt8] = [0xF0, 0x7D] + (0..<64).map { UInt8($0) } + [0xF7]
_ = send(sysex); expected += sysex
usleep(500000)
lock.lock(); let got = received; lock.unlock()
print("burst: sent \(expected.count) bytes, received \(got.count) bytes, match: \(got == expected)")
if got != expected, let i = zip(got, expected).enumerated().first(where: { $0.element.0 != $0.element.1 })?.offset {
    print("first mismatch at byte \(i)")
}

// 3. SysEx alone, several sizes, longer wait.
for size in [4, 16, 64, 256] {
    usleep(200000)
    lock.lock(); received.removeAll(); lock.unlock()
    let msg: [UInt8] = [0xF0, 0x7D] + (0..<size).map { UInt8($0 & 0x7F) } + [0xF7]
    _ = send(msg)
    usleep(1500000)
    lock.lock(); let r = received; lock.unlock()
    print("sysex \(msg.count) bytes via MIDISend: received \(r.count), match: \(r == msg)")
}
