// latency: measures the round trip through Core Audio and compares it with what the
// device reports. Plays a short 2 kHz burst every half second on one output of the
// "Fast Track Ultra" device and finds it on one input by cross-correlation, in device
// sample time: (input sample time of the burst) - (output sample time it was written at).
// Loop the output back to the input with a cable. A DAW aligns recordings using the
// reported latencies, so they should add up to the measured round trip.
// Build: swiftc -O proto/latency.swift -o proto/build/latency
// Usage: latency [output 1-8] [input 1-8] [bursts] [rate]   (defaults: 1 1 20, current rate)
import CoreAudio
import Foundation

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

func property<T: BitwiseCopyable>(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector,
                 _ scope: AudioObjectPropertyScope, _ initial: T) -> T {
    var addr = AudioObjectPropertyAddress(mSelector: sel, mScope: scope, mElement: kAudioObjectPropertyElementMain)
    var v = initial
    var sz = UInt32(MemoryLayout<T>.size)
    AudioObjectGetPropertyData(obj, &addr, 0, nil, &sz, &v)
    return v
}

/// Latency of the first stream in a scope (kAudioStreamPropertyLatency).
func streamLatency(_ dev: AudioDeviceID, _ scope: AudioObjectPropertyScope) -> UInt32 {
    var addr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyStreams, mScope: scope,
                                          mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    AudioObjectGetPropertyDataSize(dev, &addr, 0, nil, &size)
    var ids = [AudioStreamID](repeating: 0, count: Int(size) / MemoryLayout<AudioStreamID>.size)
    AudioObjectGetPropertyData(dev, &addr, 0, nil, &size, &ids)
    guard let s = ids.first else { return 0 }
    return property(s, kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal, UInt32(0))
}

let args = CommandLine.arguments
let outCh = args.count > 1 ? Int(args[1])! : 1
let inCh = args.count > 2 ? Int(args[2])! : 1
let bursts = args.count > 3 ? Int(args[3])! : 20
let wantRate = args.count > 4 ? Float64(args[4])! : 0
let channels = 8

guard let dev = deviceID(named: "Fast Track Ultra") else { print("device not found"); exit(1) }
if wantRate > 0 {
    var addr = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyNominalSampleRate,
                                          mScope: kAudioObjectPropertyScopeGlobal,
                                          mElement: kAudioObjectPropertyElementMain)
    var r = wantRate
    let st = AudioObjectSetPropertyData(dev, &addr, 0, nil, UInt32(MemoryLayout<Float64>.size), &r)
    if st != noErr { print("set rate failed: \(st)"); exit(1) }
    sleep(2)
}
let rate = property(dev, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, Float64(0))
let period = Int(rate / 2)

// Burst: 32 samples of 2 kHz under a Hann window.
let burstLen = 32
let template: [Float] = (0..<burstLen).map { n in
    let w = 0.5 - 0.5 * cos(2 * Double.pi * Double(n) / Double(burstLen - 1))
    return Float(0.5 * w * sin(2 * Double.pi * 2000 * Double(n) / rate))
}

// Input samples by device sample time, filled on the IO thread and analyzed per period.
let capLen = period * 4
var cap = [Float](repeating: 0, count: capLen)
var capEnd: Int64 = -1   // input sample time just past the last stored sample
var firstOut: Int64 = -1 // first burst written, so analysis starts with a complete one
let lock = NSLock()

var procID: AudioDeviceIOProcID?
let status = AudioDeviceCreateIOProcIDWithBlock(&procID, dev, nil) { _, inData, inTime, outData, outTime in
    let ins = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: inData))
    let outs = UnsafeMutableAudioBufferListPointer(outData)
    // Output: interleaved, one buffer of 8 channels (the plugin's single stream).
    if let b = outs.first, let p = b.mData?.assumingMemoryBound(to: Float.self) {
        let frames = Int(b.mDataByteSize) / (channels * 4)
        let t0 = Int64(outTime.pointee.mSampleTime)
        for f in 0..<frames {
            let t = t0 + Int64(f)
            let k = Int(t % Int64(period))
            for c in 0..<channels { p[f * channels + c] = 0 }
            if k < burstLen {
                p[f * channels + outCh - 1] = template[k]
                if k == 0 && firstOut < 0 { firstOut = t }
            }
        }
    }
    if let b = ins.first, let p = b.mData?.assumingMemoryBound(to: Float.self) {
        let frames = Int(b.mDataByteSize) / (channels * 4)
        let t0 = Int64(inTime.pointee.mSampleTime)
        lock.lock()
        for f in 0..<frames {
            cap[Int((t0 + Int64(f)) % Int64(capLen))] = p[f * channels + inCh - 1]
        }
        capEnd = t0 + Int64(frames)
        lock.unlock()
    }
}
guard status == noErr, let procID else { print("IOProc failed: \(status)"); exit(1) }

let inLat = property(dev, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeInput, UInt32(0))
let outLat = property(dev, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput, UInt32(0))
let inSafety = property(dev, kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeInput, UInt32(0))
let outSafety = property(dev, kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeOutput, UInt32(0))
let inStream = streamLatency(dev, kAudioObjectPropertyScopeInput)
let outStream = streamLatency(dev, kAudioObjectPropertyScopeOutput)
let bufferFrames = property(dev, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal, UInt32(0))
print(String(format: "rate %.0f Hz, buffer %d frames", rate, bufferFrames))
print("reported: input latency \(inLat) + stream \(inStream), safety \(inSafety); output latency \(outLat) + stream \(outStream), safety \(outSafety)")

AudioDeviceStart(dev, procID)
defer { AudioDeviceStop(dev, procID); AudioDeviceDestroyIOProcID(dev, procID) }

// For each burst written at output sample time T, wait until input covers
// [T, T + period), then find the lag with the strongest correlation.
var results: [Double] = []
var next: Int64 = -1
let deadline = Date().addingTimeInterval(Double(bursts) * 0.5 + 5)
while results.count < bursts && Date() < deadline {
    usleep(20_000)
    lock.lock()
    if next < 0, firstOut >= 0 { next = firstOut + Int64(period) }
    guard next >= 0, capEnd >= next + Int64(period), capEnd - next <= Int64(capLen) else { lock.unlock(); continue }
    let w = (0..<period).map { cap[Int((next + Int64($0)) % Int64(capLen))] }
    lock.unlock()
    var best = 0, bestV: Float = 0
    var corr = [Float](repeating: 0, count: period - burstLen)
    for lag in 0..<(period - burstLen) {
        var acc: Float = 0
        for n in 0..<burstLen { acc += w[lag + n] * template[n] }
        corr[lag] = acc
        if abs(acc) > abs(bestV) { bestV = acc; best = lag }
    }
    // Parabolic interpolation around the peak for a sub-sample estimate.
    var lag = Double(best)
    if best > 0 && best < corr.count - 1 {
        let a = Double(corr[best - 1]), b = Double(corr[best]), c = Double(corr[best + 1])
        let d = a - 2 * b + c
        if d != 0 { lag += 0.5 * (a - c) / d }
    }
    let energy = template.reduce(0) { $0 + $1 * $1 }
    print(String(format: "burst %2d: %8.2f frames (%.3f ms), gain %+.2f", results.count + 1, lag, lag / rate * 1000, bestV / energy))
    results.append(lag)
    next += Int64(period)
}
guard !results.isEmpty else { print("no bursts measured"); exit(1) }
let mean = results.reduce(0, +) / Double(results.count)
let spread = (results.max()! - results.min()!)
let reported = Double(inLat + inStream + outLat + outStream)
print(String(format: "round trip %.2f frames (%.3f ms), spread %.2f; reported latency sum %.0f; unreported %.2f frames (%.3f ms)",
             mean, mean / rate * 1000, spread, reported, mean - reported, (mean - reported) / rate * 1000))
