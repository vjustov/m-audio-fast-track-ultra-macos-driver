// HAL plugin for the M-Audio Fast Track Ultra.
//
// Presents one 8-in/8-out device to Core Audio. All USB work happens in ftu-daemon;
// this plugin only moves samples between Core Audio and the daemon's shared-memory
// rings (shared/ftu_shm.h) and reports the device clock from the daemon's anchor.
// Keep it thin: it runs inside coreaudiod's driver host.

#include <aspl/Driver.hpp>

#include <CoreAudio/AudioServerPlugIn.h>
#include <dispatch/dispatch.h>
#include <mach/mach_time.h>
#include <os/log.h>
#include <xpc/xpc.h>

#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstring>
#include <functional>
#include <mutex>
#include <thread>
#include <vector>

#include "DaemonLink.hpp"
#include "ftu_shm.h"

#include <cstddef>

// Layout must match the Rust mirror (daemon/src/shm.rs asserts the same size).
static_assert(offsetof(ftu_shm_t, capture) == 216, "ftu_shm_t header layout");
static_assert(offsetof(ftu_shm_t, plugin_input_jumps) % 8 == 0, "ftu_shm_t counter alignment");
static_assert(sizeof(ftu_shm_t) == 216 + 2 * FTU_RING_FRAMES * FTU_CHANNELS * sizeof(float),
    "ftu_shm_t size");

namespace {

using ftu::DaemonLink;
using ftu::Log;

constexpr UInt32 kDefaultRate = 48000;
constexpr UInt32 kRates[] = {44100, 48000, 88200, 96000};
constexpr UInt32 kZeroTimeStampPeriod = 2048;

// The daemon keeps 16 ms of playback queued ahead of capture (daemon/src/stream.rs,
// OUT_XFERS x OUT_PACKETS), so Core Audio must write output at least that far ahead.
// Capture arrives in 1 ms steps. 24 ms covers both with margin for wakeup jitter.
UInt32 SafetyOffsetFor(UInt32 rate)
{
    return rate * 24 / 1000;
}

// Device latency per direction, in frames, beyond the safety offset: the round trip
// measured through Core Audio with hwtest/latency on 2026-10-25 (clean starts, before
// the 10 ms playback start delay), split between input and output. For comparison the
// vendor kext reports 28 + 28 at 44.1 and 48 kHz, 43 + 43 at 88.2 and 45 + 45 at 96.
// Provisional: re-measure with the current daemon; "unreported" should read about 0.
UInt32 LatencyFor(UInt32 rate, aspl::Direction dir)
{
    UInt32 roundTrip;
    switch (rate) {
    case 44100:
        roundTrip = 62;
        break;
    case 88200:
        roundTrip = 86;
        break;
    case 96000:
        roundTrip = 91;
        break;
    default:
        roundTrip = 63;
        break;
    }
    const UInt32 input = roundTrip / 2;
    return dir == aspl::Direction::Input ? input : roundTrip - input;
}

bool ValidRate(Float64 rate)
{
    for (UInt32 r : kRates) {
        if (rate == r) {
            return true;
        }
    }
    return false;
}

// Current device rate, shared by the device, its streams and the IO handler.
struct RateState
{
    std::atomic<UInt32> rate{kDefaultRate};
};
constexpr UInt64 kSeedBase = 1000;  // keeps daemon seeds apart from libASPL's synthetic clock

// Identifies the daemon timeline IO is on: a stream generation within one connection.
// A restarted daemon counts generations from 0 again, so the connection count is part
// of it. Realtime-safe.
UInt64 TimelineId(const DaemonLink& link, const ftu_shm_t* s)
{
    return (link.Epoch() << 32) + s->generation.load(std::memory_order_acquire);
}

struct ClockAnchor
{
    uint64_t sampleTime;
    uint64_t hostTime;
    double ticksPerFrame;
};

// Seqlock read of the daemon's clock anchor. Realtime-safe.
bool ReadClock(const ftu_shm_t* s, ClockAnchor& out)
{
    for (int i = 0; i < 16; i++) {
        const uint64_t s1 = s->clock_seq.load(std::memory_order_acquire);
        if (s1 & 1) {
            continue;
        }
        out.sampleTime = s->anchor_sample_time.load(std::memory_order_relaxed);
        out.hostTime = s->anchor_host_time.load(std::memory_order_relaxed);
        const uint64_t bits = s->host_ticks_per_frame.load(std::memory_order_relaxed);
        std::memcpy(&out.ticksPerFrame, &bits, sizeof(bits));
        std::atomic_thread_fence(std::memory_order_acquire);
        if (s->clock_seq.load(std::memory_order_relaxed) == s1) {
            return s1 != 0 && out.ticksPerFrame > 0;
        }
    }
    return false;
}

bool Running(const ftu_shm_t* s)
{
    return s && s->running.load(std::memory_order_acquire) != 0;
}

AudioStreamBasicDescription FloatFormat(UInt32 rate)
{
    AudioStreamBasicDescription f = {};
    f.mSampleRate = rate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    f.mBitsPerChannel = 32;
    f.mChannelsPerFrame = FTU_CHANNELS;
    f.mBytesPerFrame = FTU_CHANNELS * sizeof(float);
    f.mFramesPerPacket = 1;
    f.mBytesPerPacket = f.mBytesPerFrame;
    return f;
}

// A stream whose format always follows the device rate. Core Audio may also change the
// rate by setting a stream format; that is forwarded to the device.
class FtuStream : public aspl::Stream
{
public:
    FtuStream(std::shared_ptr<const aspl::Context> context,
        std::shared_ptr<aspl::Device> device,
        const aspl::StreamParameters& params,
        std::shared_ptr<RateState> rate)
        : aspl::Stream(std::move(context), device, params)
        , device_(device)
        , rate_(std::move(rate))
    {
    }

    UInt32 GetLatency() const override
    {
        return LatencyFor(rate_->rate.load(), GetDirection());
    }

    AudioStreamBasicDescription GetPhysicalFormat() const override
    {
        return FloatFormat(rate_->rate.load());
    }

    AudioStreamBasicDescription GetVirtualFormat() const override
    {
        return FloatFormat(rate_->rate.load());
    }

    std::vector<AudioStreamRangedDescription> GetAvailablePhysicalFormats() const override
    {
        return AllFormats();
    }

    std::vector<AudioStreamRangedDescription> GetAvailableVirtualFormats() const override
    {
        return AllFormats();
    }

protected:
    OSStatus SetPhysicalFormatImpl(const AudioStreamBasicDescription& format) override
    {
        return FollowRate(format);
    }

    OSStatus SetVirtualFormatImpl(const AudioStreamBasicDescription& format) override
    {
        return FollowRate(format);
    }

private:
    static std::vector<AudioStreamRangedDescription> AllFormats()
    {
        std::vector<AudioStreamRangedDescription> v;
        for (UInt32 r : kRates) {
            AudioStreamRangedDescription d = {};
            d.mFormat = FloatFormat(r);
            d.mSampleRateRange.mMinimum = r;
            d.mSampleRateRange.mMaximum = r;
            v.push_back(d);
        }
        return v;
    }

    OSStatus FollowRate(const AudioStreamBasicDescription& format)
    {
        if (!ValidRate(format.mSampleRate)) {
            return kAudioDeviceUnsupportedFormatError;
        }
        if (auto dev = device_.lock(); dev && format.mSampleRate != rate_->rate.load()) {
            return dev->SetNominalSampleRateAsync(format.mSampleRate);
        }
        return kAudioHardwareNoError;
    }

    std::weak_ptr<aspl::Device> device_;
    std::shared_ptr<RateState> rate_;
};

class FtuDevice : public aspl::Device
{
public:
    FtuDevice(std::shared_ptr<const aspl::Context> context,
        const aspl::DeviceParameters& params,
        std::shared_ptr<DaemonLink> link,
        std::shared_ptr<RateState> rate)
        : aspl::Device(std::move(context), params)
        , link_(std::move(link))
        , rate_(std::move(rate))
    {
    }

    void AddFtuStream(std::shared_ptr<FtuStream> stream)
    {
        streams_.push_back(stream);
        AddStreamAsync(stream);
    }

    UInt32 GetSafetyOffset() const override
    {
        return SafetyOffsetFor(rate_->rate.load());
    }

protected:
    // Runs inside a HAL configuration change, off the realtime thread.
    OSStatus SetNominalSampleRateImpl(Float64 rate) override
    {
        if (!ValidRate(rate)) {
            return kAudioDeviceUnsupportedFormatError;
        }
        if (!link_->RequestRate(static_cast<UInt32>(rate))) {
            return kAudioHardwareUnspecifiedError;
        }
        rate_->rate.store(static_cast<UInt32>(rate));
        const OSStatus st = aspl::Device::SetNominalSampleRateImpl(rate);
        // Sent after the configuration change returns: calling back into the HAL from
        // inside it re-enters the plug-in command gate. The device lives as long as the
        // driver host, so `this` stays valid.
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
            for (auto& s : streams_) {
                s->NotifyPropertiesChanged({kAudioStreamPropertyPhysicalFormat, kAudioStreamPropertyVirtualFormat, kAudioStreamPropertyLatency});
            }
            NotifyPropertyChanged(kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeInput);
            NotifyPropertyChanged(kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeOutput);
        });
        return st;
    }

    // Zero timestamps come from the device clock (daemon anchor). Without a running
    // daemon, fall back to libASPL's host-clock timestamps so clients keep working.
    OSStatus GetZeroTimeStampImpl(UInt32 clientID,
        Float64* outSampleTime,
        UInt64* outHostTime,
        UInt64* outSeed) override
    {
        const ftu_shm_t* s = link_->Shm();
        ClockAnchor a;
        // A daemon still at another rate (StartIO moves it asynchronously) would give
        // timestamps at the wrong rate.
        if (Running(s) && s->sample_rate.load(std::memory_order_acquire) == rate_->rate.load() && ReadClock(s, a)) {
            const double now = static_cast<double>(mach_absolute_time());
            const double sampleNow =
                static_cast<double>(a.sampleTime) + (now - a.hostTime) / a.ticksPerFrame;
            const double zts = std::floor(sampleNow / kZeroTimeStampPeriod) * kZeroTimeStampPeriod;
            *outSampleTime = zts;
            *outHostTime = static_cast<UInt64>(
                static_cast<double>(a.hostTime) + (zts - a.sampleTime) * a.ticksPerFrame);
            *outSeed = kSeedBase + TimelineId(*link_, s);
            return kAudioHardwareNoError;
        }
        return aspl::Device::GetZeroTimeStampImpl(clientID, outSampleTime, outHostTime, outSeed);
    }

private:
    std::shared_ptr<DaemonLink> link_;
    std::shared_ptr<RateState> rate_;
    std::vector<std::shared_ptr<FtuStream>> streams_;
};

class Handler : public aspl::ControlRequestHandler, public aspl::IORequestHandler
{
public:
    Handler(std::shared_ptr<DaemonLink> link, std::shared_ptr<RateState> rate)
        : link_(std::move(link))
        , rate_(std::move(rate))
    {
    }

    OSStatus OnStartIO() override
    {
        // A new IO session starts a new timeline: do not count its first cycle as a jump.
        in_.Reset();
        out_.Reset();
        // The daemon may be at another rate (its default, or a previous session's).
        // Core Audio's nominal rate wins, except on S/PDIF clock. Done in the
        // background: StartIO holds up every client of the device while it runs.
        link_->PrepareAsync(rate_->rate.load());
        os_log(Log(), "StartIO");
        return kAudioHardwareNoError;
    }

    void OnStopIO() override
    {
        os_log(Log(), "StopIO");
    }

    void OnReadClientInput(const std::shared_ptr<aspl::Client>& client,
        const std::shared_ptr<aspl::Stream>& stream,
        Float64 zeroTimestamp,
        Float64 timestamp,
        void* bytes,
        UInt32 bytesCount) override
    {
        const UInt32 frames = bytesCount / (FTU_CHANNELS * sizeof(float));
        auto* out = static_cast<float*>(bytes);
        const ftu_shm_t* s = link_->Shm();
        if (!Running(s) || timestamp < 0) {
            std::memset(bytes, 0, bytesCount);
            return;
        }
        const uint64_t t = static_cast<uint64_t>(timestamp);
        if (in_.Jumped(TimelineId(*link_, s), t, frames)) {
            const_cast<ftu_shm_t*>(s)->plugin_input_jumps.fetch_add(1, std::memory_order_relaxed);
        }
        const uint64_t written = s->cap_write_pos.load(std::memory_order_acquire);
        if (t + frames > written || written - t > FTU_RING_FRAMES) {
            // Not captured yet, or already overwritten.
            const_cast<ftu_shm_t*>(s)->plugin_short_reads.fetch_add(1, std::memory_order_relaxed);
            std::memset(bytes, 0, bytesCount);
            return;
        }
        CopyFromRing(s->capture, t, out, frames);
    }

    void OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>& stream,
        Float64 zeroTimestamp,
        Float64 timestamp,
        const void* bytes,
        UInt32 bytesCount) override
    {
        const UInt32 frames = bytesCount / (FTU_CHANNELS * sizeof(float));
        const ftu_shm_t* s = link_->Shm();
        if (!Running(s) || timestamp < 0) {
            return;
        }
        auto* shm = const_cast<ftu_shm_t*>(s);
        const uint64_t t = static_cast<uint64_t>(timestamp);
        if (out_.Jumped(TimelineId(*link_, s), t, frames)) {
            shm->plugin_output_jumps.fetch_add(1, std::memory_order_relaxed);
        }
        const uint64_t readPos = shm->play_read_pos.load(std::memory_order_acquire);
        if (t < readPos || t + frames > readPos + FTU_RING_FRAMES) {
            shm->plugin_late_writes.fetch_add(1, std::memory_order_relaxed);
            return;  // too late, or too far ahead
        }
        CopyToRing(shm->playback, t, static_cast<const float*>(bytes), frames);
        const uint64_t end = t + frames;
        if (shm->play_write_pos.load(std::memory_order_relaxed) < end) {
            shm->play_write_pos.store(end, std::memory_order_release);
        }
    }

private:
    // Checks that each IO cycle starts where the previous one ended. Several input
    // clients read the same cycle, so a repeat of the last start is not a jump.
    // IO thread only.
    struct Timeline
    {
        uint64_t generation = UINT64_MAX;
        uint64_t start = 0;
        uint64_t end = 0;

        void Reset()
        {
            generation = UINT64_MAX;
        }

        bool Jumped(uint64_t gen, uint64_t t, UInt32 frames)
        {
            const bool jumped = gen == generation && t != end && t != start;
            if (gen != generation || t != start) {
                generation = gen;
                start = t;
                end = t + frames;
            }
            return jumped;
        }
    };

    static void CopyFromRing(const float* ring, uint64_t t, float* out, UInt32 frames)
    {
        const uint32_t start = static_cast<uint32_t>(t % FTU_RING_FRAMES);
        const uint32_t first = std::min<uint32_t>(frames, FTU_RING_FRAMES - start);
        std::memcpy(out, ring + start * FTU_CHANNELS, first * FTU_CHANNELS * sizeof(float));
        std::memcpy(out + first * FTU_CHANNELS, ring, (frames - first) * FTU_CHANNELS * sizeof(float));
    }

    static void CopyToRing(float* ring, uint64_t t, const float* in, UInt32 frames)
    {
        const uint32_t start = static_cast<uint32_t>(t % FTU_RING_FRAMES);
        const uint32_t first = std::min<uint32_t>(frames, FTU_RING_FRAMES - start);
        std::memcpy(ring + start * FTU_CHANNELS, in, first * FTU_CHANNELS * sizeof(float));
        std::memcpy(ring, in + first * FTU_CHANNELS, (frames - first) * FTU_CHANNELS * sizeof(float));
    }

    std::shared_ptr<DaemonLink> link_;
    std::shared_ptr<RateState> rate_;
    Timeline in_;
    Timeline out_;
};

std::shared_ptr<aspl::Driver> CreateDriver()
{
    auto context = std::make_shared<aspl::Context>();
    auto link = std::make_shared<DaemonLink>();
    auto rate = std::make_shared<RateState>();

    aspl::DeviceParameters params;
    params.Name = "Fast Track Ultra";
    params.Manufacturer = "M-Audio";
    params.DeviceUID = "local.ftu.device";
    params.ModelUID = "local.ftu.model";
    params.SampleRate = kDefaultRate;
    params.ChannelCount = FTU_CHANNELS;
    params.SafetyOffset = SafetyOffsetFor(kDefaultRate);
    params.ZeroTimeStampPeriod = kZeroTimeStampPeriod;
    params.EnableMixing = true;

    auto device = std::make_shared<FtuDevice>(context, params, link, rate);

    std::vector<AudioValueRange> rates;
    for (UInt32 r : kRates) {
        rates.push_back({static_cast<Float64>(r), static_cast<Float64>(r)});
    }
    device->SetAvailableSampleRatesAsync(rates);

    for (auto dir : {aspl::Direction::Input, aspl::Direction::Output}) {
        aspl::StreamParameters sp;
        sp.Direction = dir;
        sp.StartingChannel = 1;
        sp.Format = FloatFormat(kDefaultRate);
        device->AddFtuStream(std::make_shared<FtuStream>(context, device, sp, rate));
    }

    auto handler = std::make_shared<Handler>(link, rate);
    device->SetControlHandler(handler);
    device->SetIOHandler(handler);

    // Unplugged: the device stays listed but reports not alive, so clients notice. It
    // is never removed, which would make coreaudiod reconcile its device lists.
    std::weak_ptr<FtuDevice> weak = device;
    link->SetPresenceHandler([weak](bool present) {
        if (auto d = weak.lock()) {
            os_log(Log(), "device %{public}s", present ? "present" : "unplugged");
            const bool back = present && !d->GetIsAlive();
            d->SetIsAlive(present);
            // Going not alive stops the clients' IO, and coming back does not restart
            // it. A configuration change makes clients reconfigure and start again.
            if (back) {
                d->RequestConfigurationChange();
            }
        }
    });
    // The new daemon's shared memory starts a new timeline (the seed changes with it).
    link->SetReconnectHandler([weak] {
        if (auto d = weak.lock()) {
            d->RequestConfigurationChange();
        }
    });

    auto plugin = std::make_shared<aspl::Plugin>(context);
    plugin->AddDevice(device);
    // Learn the device's presence now rather than at the first StartIO.
    link->PrepareAsync(0);

    return std::make_shared<aspl::Driver>(context, plugin);
}

} // namespace

extern "C" void* FtuEntryPoint(CFAllocatorRef allocator, CFUUIDRef typeUUID)
{
    if (!CFEqual(typeUUID, kAudioServerPlugInTypeUUID)) {
        return nullptr;
    }
    static std::shared_ptr<aspl::Driver> driver = CreateDriver();
    return driver->GetReference();
}
