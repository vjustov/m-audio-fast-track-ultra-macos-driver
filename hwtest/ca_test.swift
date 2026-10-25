// ca_test: end-to-end check through Core Audio. Plays a sine on one output channel of
// the "Fast Track Ultra" device and records all 8 inputs to a WAV file.
// Usage: swift hwtest/ca_test.swift <seconds> <out.wav> [channel 1-8] [dBFS] [sample rate]
import AVFoundation
import CoreAudio

func deviceID(named name: String) -> AudioDeviceID? {
    var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDevices,
                                          mScope: kAudioObjectPropertyScopeGlobal,
                                          mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size)
    var ids = [AudioDeviceID](repeating: 0, count: Int(size) / MemoryLayout<AudioDeviceID>.size)
    AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &ids)
    for id in ids {
        var nameAddr = AudioObjectPropertyAddress(mSelector: kAudioObjectPropertyName,
                                                  mScope: kAudioObjectPropertyScopeGlobal,
                                                  mElement: kAudioObjectPropertyElementMain)
        var cf: Unmanaged<CFString>?
        var sz = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
        if AudioObjectGetPropertyData(id, &nameAddr, 0, nil, &sz, &cf) == noErr,
           let s = cf?.takeRetainedValue() as String?, s == name {
            return id
        }
    }
    return nil
}

let args = CommandLine.arguments
let seconds = args.count > 1 ? Double(args[1])! : 5
let outPath = args.count > 2 ? args[2] : "ca_capture.wav"
let channel = args.count > 3 ? Int(args[3])! : 1
let dbfs = args.count > 4 ? Double(args[4])! : -20
let wantRate = args.count > 5 ? Double(args[5])! : 0

guard var dev = deviceID(named: "Fast Track Ultra") else { print("device not found"); exit(1) }
let engine = AVAudioEngine()
for unit in [engine.outputNode.audioUnit!, engine.inputNode.audioUnit!] {
    let r = AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0,
                                 &dev, UInt32(MemoryLayout<AudioDeviceID>.size))
    if r != noErr { print("set device failed: \(r)"); exit(1) }
}

// The engine reports 0 Hz until it reconfigures for the new device; ask the device.
func nominalRate(_ id: AudioDeviceID) -> Double {
    var addr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate,
                                          mScope: kAudioObjectPropertyScopeGlobal,
                                          mElement: kAudioObjectPropertyElementMain)
    var r: Float64 = 0
    var sz = UInt32(MemoryLayout<Float64>.size)
    AudioObjectGetPropertyData(id, &addr, 0, nil, &sz, &r)
    return r
}
if wantRate > 0 && nominalRate(dev) != wantRate {
    var addr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate,
                                          mScope: kAudioObjectPropertyScopeGlobal,
                                          mElement: kAudioObjectPropertyElementMain)
    var r = Float64(wantRate)
    let st = AudioObjectSetPropertyData(dev, &addr, 0, nil, UInt32(MemoryLayout<Float64>.size), &r)
    let t0 = Date()
    while nominalRate(dev) != wantRate && Date().timeIntervalSince(t0) < 5 { Thread.sleep(forTimeInterval: 0.05) }
    print("set rate \(wantRate): status \(st), now \(nominalRate(dev)) after \(String(format: "%.2f", Date().timeIntervalSince(t0))) s")
}
let rate = nominalRate(dev)
// More than 2 channels needs an explicit layout, or AVAudioFormat returns nil.
let layout = AVAudioChannelLayout(layoutTag: kAudioChannelLayoutTag_DiscreteInOrder | 8)!
let fmt = AVAudioFormat(standardFormatWithSampleRate: rate, channelLayout: layout)
let amp = Float(pow(10, dbfs / 20))
var phase = 0.0
let step = 2 * Double.pi * 997 / rate
let source = AVAudioSourceNode(format: fmt) { _, _, frames, abl in
    let bufs = UnsafeMutableAudioBufferListPointer(abl)
    for f in 0..<Int(frames) {
        let v = amp * Float(sin(phase))
        phase += step
        for (c, b) in bufs.enumerated() {
            b.mData!.assumingMemoryBound(to: Float.self)[f] = (c == channel - 1) ? v : 0
        }
    }
    return noErr
}
engine.attach(source)
engine.connect(source, to: engine.outputNode, format: fmt)

engine.prepare()
// Hardware-side format of the input, valid once the engine is prepared on the device.
let hwIn = engine.inputNode.inputFormat(forBus: 0)
let inFmt = AVAudioFormat(standardFormatWithSampleRate: hwIn.sampleRate,
                          channelLayout: AVAudioChannelLayout(layoutTag: kAudioChannelLayoutTag_DiscreteInOrder | hwIn.channelCount)!)
let inChannels = Int(inFmt.channelCount)
var recorded = [[Float]](repeating: [], count: inChannels)
let lock = NSLock()
engine.inputNode.installTap(onBus: 0, bufferSize: 4096, format: inFmt) { buf, _ in
    lock.lock()
    for c in 0..<inChannels {
        recorded[c].append(contentsOf: UnsafeBufferPointer(start: buf.floatChannelData![c], count: Int(buf.frameLength)))
    }
    lock.unlock()
}

try engine.start()
print("playing 997 Hz at \(dbfs) dBFS on output \(channel), recording \(inChannels) inputs at \(rate) Hz")
Thread.sleep(forTimeInterval: seconds)
engine.stop()

// 32-bit float WAV, interleaved.
lock.lock()
let frames = recorded.first?.count ?? 0
var data = Data()
func le<T: FixedWidthInteger>(_ v: T) { withUnsafeBytes(of: v.littleEndian) { data.append(contentsOf: $0) } }
data.append(contentsOf: Array("RIFF".utf8)); le(UInt32(36 + frames * inChannels * 4))
data.append(contentsOf: Array("WAVEfmt ".utf8)); le(UInt32(16)); le(UInt16(3)); le(UInt16(inChannels))
le(UInt32(rate)); le(UInt32(Int(rate) * inChannels * 4)); le(UInt16(inChannels * 4)); le(UInt16(32))
data.append(contentsOf: Array("data".utf8)); le(UInt32(frames * inChannels * 4))
for f in 0..<frames { for c in 0..<inChannels { le(recorded[c][f].bitPattern) } }
lock.unlock()
try data.write(to: URL(fileURLWithPath: outPath))
let captured = frames
print("captured \(captured) frames (\(String(format: "%.1f", Double(captured) / seconds)) per second)")
