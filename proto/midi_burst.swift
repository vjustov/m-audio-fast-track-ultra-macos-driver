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

print("notes before: \(roundTrips(3))/3")
// N note-ons in one MIDIPacket. Each becomes one 4-byte USB-MIDI event, so N events = 4N bytes on the bulk endpoint.
for n in [8, 12, 15, 16, 17, 20, 32] {
    lock.lock(); received.removeAll(); lock.unlock()
    var msg: [UInt8] = []
    for i in 0..<n { msg += [0x90, UInt8(20 + i), 1] }
    _ = send(msg); usleep(600000)
    lock.lock(); let got = received; lock.unlock()
    var off: [UInt8] = []
    for i in 0..<n { off += [0x80, UInt8(20 + i), 0] }
    for c in stride(from: 0, to: off.count, by: 12) { _ = send(Array(off[c..<min(c + 12, off.count)])); usleep(5000) }
    usleep(200000)
    let ok = roundTrips(3)
    print("\(n) notes in one packet (\(4 * n) USB bytes): sent \(msg.count), received \(got.count), exact=\(got == msg), notes after=\(ok)/3")
    if ok == 0 { print("MIDI stopped"); break }
}
