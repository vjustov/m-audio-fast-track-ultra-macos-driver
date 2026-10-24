// soak: long-running dropout check through Core Audio. Plays a 997 Hz sine on one
// output of the "Fast Track Ultra" device and checks one input for breaks in the sine,
// live, so hours of audio need no WAV file. Loop the output back to the input: the
// device's own S/PDIF loop (outputs and inputs 7/8, needs the S/PDIF clock), or a cable.
// Prints one line per minute and every glitch with its time and kind: a run of exact
// zeros (dropout), a phase jump (samples skipped or repeated), or a blip.
// Build: swiftc -O proto/soak.swift -o proto/build/soak
// Usage: soak <minutes> [output 1-8] [input 1-8] [dBFS] [dump.raw]   (defaults: 7 7 -20)
// dump.raw gets 4096 float32 samples around each glitch (2048 before, 2048 after).
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

func nominalRate(_ id: AudioDeviceID) -> Double {
    var addr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate,
                                          mScope: kAudioObjectPropertyScopeGlobal,
                                          mElement: kAudioObjectPropertyElementMain)
    var r: Float64 = 0
    var sz = UInt32(MemoryLayout<Float64>.size)
    AudioObjectGetPropertyData(id, &addr, 0, nil, &sz, &r)
    return r
}

let args = CommandLine.arguments
guard args.count > 1, let minutes = Double(args[1]) else {
    print("usage: soak <minutes> [output 1-8] [input 1-8] [dBFS]")
    exit(2)
}
let outCh = args.count > 2 ? Int(args[2])! : 7
let inCh = args.count > 3 ? Int(args[3])! : 7
let dbfs = args.count > 4 ? Double(args[4])! : -20
let dumpPath = args.count > 5 ? args[5] : nil

guard var dev = deviceID(named: "Fast Track Ultra") else { print("device not found"); exit(1) }
let engine = AVAudioEngine()
for unit in [engine.outputNode.audioUnit!, engine.inputNode.audioUnit!] {
    let r = AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0,
                                 &dev, UInt32(MemoryLayout<AudioDeviceID>.size))
    if r != noErr { print("set device failed: \(r)"); exit(1) }
}
let rate = nominalRate(dev)

// Processor overloads reported by the HAL (the IO cycle missed its deadline).
var overloads = 0
var overloadAddr = AudioObjectPropertyAddress(mSelector: kAudioDeviceProcessorOverload,
                                              mScope: kAudioObjectPropertyScopeGlobal,
                                              mElement: kAudioObjectPropertyElementMain)
AudioObjectAddPropertyListenerBlock(dev, &overloadAddr, DispatchQueue.main) { _, _ in overloads += 1 }

// More than 2 channels needs an explicit layout, or AVAudioFormat returns nil.
let layout = AVAudioChannelLayout(layoutTag: kAudioChannelLayoutTag_DiscreteInOrder | 8)!
let fmt = AVAudioFormat(standardFormatWithSampleRate: rate, channelLayout: layout)
let amp = Float(pow(10, dbfs / 20))
let step = 2 * Double.pi * 997 / rate
var phase = 0.0
let source = AVAudioSourceNode(format: fmt) { _, _, frames, abl in
    let bufs = UnsafeMutableAudioBufferListPointer(abl)
    for f in 0..<Int(frames) {
        let v = amp * Float(sin(phase))
        phase += step
        if phase > 2 * Double.pi { phase -= 2 * Double.pi }
        for (c, b) in bufs.enumerated() {
            b.mData!.assumingMemoryBound(to: Float.self)[f] = (c == outCh - 1) ? v : 0
        }
    }
    return noErr
}
engine.attach(source)
engine.connect(source, to: engine.outputNode, format: fmt)
engine.prepare()

// A clean sine satisfies x[n] = 2cos(w) x[n-1] - x[n-2] whatever its delay, phase or
// gain. A dropout, repeat or skip breaks it. The residual is checked against the
// received amplitude, so the loop's gain does not matter.
struct Glitch {
    var frame: Int
    var window: [Float]  // 4096 samples, the glitch at index 2048
}

struct Checker {
    let k = Float(2 * cos(step))
    var x1: Float = 0, x2: Float = 0
    var frames = 0
    var sumSq: Double = 0
    var peak: Float = 0
    var glitches = 0
    var lastGlitch = -1_000_000
    var ring = [Float](repeating: 0, count: 4096)
    var pending: [Int] = []   // glitch frames still collecting their "after" half
    var done: [Glitch] = []

    mutating func feed(_ p: UnsafePointer<Float>, _ n: Int, armed: Bool) {
        // 5% of the amplitude. A one-sample slip leaves a residual of about 13%
        // (2 sin(w/2), w = 0.13 rad); 50 Hz hum on an analog loop stays below 2%.
        let limit = max(peak, 1e-4) * 0.05
        for i in 0..<n {
            let x = p[i]
            let r = x - k * x1 + x2
            if armed && frames >= 2 && abs(r) > limit && frames - lastGlitch > Int(rate / 100) {
                glitches += 1
                pending.append(frames)
                lastGlitch = frames
            }
            ring[frames & 4095] = x
            if let g = pending.first, frames - g == 2047 {
                pending.removeFirst()
                let w = (0..<4096).map { ring[(frames - 4095 + $0) & 4095] }
                done.append(Glitch(frame: g, window: w))
            }
            x2 = x1
            x1 = x
            sumSq += Double(x * x)
            peak = max(peak * 0.99999, abs(x))
            frames += 1
        }
    }
}

// Describes a glitch window: longest run of exact zeros, and the sine's phase just
// before and just after the glitch, as a jump in samples (modulo one 997 Hz period).
func describe(_ g: Glitch) -> String {
    let w = g.window
    var zeros = 0, run = 0
    for v in w { run = v == 0 ? run + 1 : 0; zeros = max(zeros, run) }
    // Least-squares phase of a 997 Hz sine over 256 samples.
    func phaseAt(_ from: Int) -> Double {
        var c = 0.0, s = 0.0
        for i in 0..<256 {
            let a = step * Double(from + i)
            c += Double(w[from + i]) * cos(a)
            s += Double(w[from + i]) * sin(a)
        }
        return atan2(c, s)
    }
    let before = phaseAt(2048 - 512), after = phaseAt(2048 + 256)
    var d = after - before
    while d > Double.pi { d -= 2 * Double.pi }
    while d < -Double.pi { d += 2 * Double.pi }
    let jump = d / step
    let kind = zeros >= 4 ? "dropout (\(zeros) zero samples)"
        : abs(jump) >= 0.5 ? "phase jump"
        : "blip"
    return String(format: "%@, phase jump %+.1f samples", kind, jump)
}

let hwIn = engine.inputNode.inputFormat(forBus: 0)
let inFmt = AVAudioFormat(standardFormatWithSampleRate: hwIn.sampleRate,
                          channelLayout: AVAudioChannelLayout(layoutTag: kAudioChannelLayoutTag_DiscreteInOrder | hwIn.channelCount)!)
var checker = Checker()
let lock = NSLock()
let start = Date()
// Ignore the first 3 s: the S/PDIF receiver takes about 1.5 s to lock.
let settle = 3.0
engine.inputNode.installTap(onBus: 0, bufferSize: 4096, format: inFmt) { buf, _ in
    lock.lock()
    checker.feed(buf.floatChannelData![inCh - 1], Int(buf.frameLength), armed: Date().timeIntervalSince(start) > settle)
    lock.unlock()
}

// The engine stops when Core Audio reconfigures it (default device change, sleep).
// Log it and restart, so a long soak survives and shows where it happened.
var restarts = 0
NotificationCenter.default.addObserver(forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main) { _ in
    restarts += 1
    print(String(format: "  engine reconfigured at %.1f s, restarting", Date().timeIntervalSince(start)))
    do { try engine.start() } catch { print("  restart failed: \(error)") }
}

try engine.start()
print("soak: \(minutes) min, 997 Hz at \(dbfs) dBFS, output \(outCh) -> input \(inCh), \(rate) Hz")
setvbuf(stdout, nil, _IOLBF, 0)

let clock = DateFormatter()
clock.dateFormat = "HH:mm:ss.SSS"
var lastFrames = 0
var lastGlitches = 0
var lastTime = Date()
var totalGlitches = 0
let end = start.addingTimeInterval(minutes * 60)
let timer = Timer(timeInterval: 60, repeats: true) { _ in
    let now = Date()
    lock.lock()
    let frames = checker.frames
    let rms = checker.sumSq / Double(max(frames - lastFrames, 1))
    checker.sumSq = 0
    let events = checker.done
    checker.done = []
    totalGlitches = checker.glitches
    lock.unlock()
    let measured = Double(frames - lastFrames) / now.timeIntervalSince(lastTime)
    let level = 10 * log10(rms + 1e-20)
    for g in events {
        let at = start.addingTimeInterval(Double(g.frame) / rate)
        print(String(format: "  glitch at %.3f s (%@): %@", Double(g.frame) / rate, clock.string(from: at), describe(g)))
        if let path = dumpPath {
            let data = g.window.withUnsafeBufferPointer { Data(buffer: $0) }
            if let h = FileHandle(forWritingAtPath: path) {
                h.seekToEndOfFile()
                h.write(data)
                h.closeFile()
            } else {
                FileManager.default.createFile(atPath: path, contents: data)
            }
        }
    }
    print(String(format: "t=%5.0f min  input %6.1f dBFS  glitches %d (total %d)  overloads %d  restarts %d  rate %.1f Hz",
                 now.timeIntervalSince(start) / 60, level, totalGlitches - lastGlitches, totalGlitches, overloads, restarts, measured))
    if level < -60 { print("  input is silent: check the loop (S/PDIF clock, or the cable) and the mixer route") }
    lastFrames = frames
    lastGlitches = totalGlitches
    lastTime = now
    if now >= end {
        engine.stop()
        print("done: \(totalGlitches) glitches, \(overloads) overloads in \(Int(minutes)) min")
        exit(totalGlitches == 0 ? 0 : 1)
    }
}
RunLoop.main.add(timer, forMode: .common)
RunLoop.main.run()
