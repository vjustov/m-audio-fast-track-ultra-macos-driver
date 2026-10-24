// HAL plugin for the M-Audio Fast Track Ultra.
//
// Presents one 8-in/8-out device to Core Audio. All USB work happens in ftu-daemon;
// this plugin only moves samples between Core Audio and the daemon's shared-memory
// rings (shared/ftu_shm.h) and reports the device clock from the daemon's anchor.
// Keep it thin: it runs inside coreaudiod's driver host.

#include <aspl/Driver.hpp>

#include <CoreAudio/AudioServerPlugIn.h>
#include <mach/mach_time.h>
#include <os/log.h>
#include <xpc/xpc.h>

#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstring>
#include <mutex>
#include <thread>
#include <vector>

#include "ftu_shm.h"

#include <cstddef>

// Layout must match the Rust mirror (daemon/src/shm.rs asserts the same size).
static_assert(offsetof(ftu_shm_t, capture) == 216, "ftu_shm_t header layout");
static_assert(offsetof(ftu_shm_t, plugin_input_jumps) % 8 == 0, "ftu_shm_t counter alignment");
static_assert(sizeof(ftu_shm_t) == 216 + 2 * FTU_RING_FRAMES * FTU_CHANNELS * sizeof(float),
    "ftu_shm_t size");

namespace {

constexpr const char* kServiceName = "local.ftu.daemon";
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

os_log_t Log()
{
    static os_log_t log = os_log_create("local.ftu.plugin", "driver");
    return log;
}

// Connection to ftu-daemon and the mapped shared memory.
// The mapping is never unmapped while the plugin lives: realtime threads may hold the
// pointer. A reconnect replaces it and leaks the old mapping (one per daemon restart).
class DaemonLink
{
public:
    const ftu_shm_t* Shm() const
    {
        return shm_.load(std::memory_order_acquire);
    }

    // Not realtime-safe. Called from StartIO.
    void EnsureConnected()
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (Shm() && !stale_.load(std::memory_order_acquire)) {
            return;
        }
        Connect();
    }

    // Asks the daemon to stream at `rate` and waits until it does. Not realtime-safe.
    bool RequestRate(UInt32 rate)
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (!Shm() || stale_.load(std::memory_order_acquire)) {
            Connect();
        }
        const ftu_shm_t* s = Shm();
        if (!s || !conn_) {
            return false;
        }
        if (s->sample_rate.load() == rate && s->running.load()) {
            return true;
        }
        const uint64_t gen = s->generation.load();

        xpc_object_t msg = xpc_dictionary_create(nullptr, nullptr, 0);
        xpc_dictionary_set_string(msg, "op", "set_rate");
        xpc_dictionary_set_uint64(msg, "rate", rate);
        xpc_object_t reply = xpc_connection_send_message_with_reply_sync(conn_, msg);
        xpc_release(msg);
        const bool ok = xpc_get_type(reply) == XPC_TYPE_DICTIONARY && xpc_dictionary_get_bool(reply, "ok");
        xpc_release(reply);
        if (!ok) {
            os_log_error(Log(), "daemon refused rate %u", rate);
            return false;
        }

        // The daemon restarts the stream: wait for a new generation at the new rate.
        for (int i = 0; i < 300; i++) {
            if (s->generation.load() != gen && s->sample_rate.load() == rate && s->running.load()) {
                os_log(Log(), "rate changed to %u", rate);
                return true;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        os_log_error(Log(), "timed out waiting for rate %u", rate);
        return false;
    }

private:
    void Connect()
    {
        if (conn_) {
            xpc_connection_cancel(conn_);
            xpc_release(conn_);
            conn_ = nullptr;
        }
        conn_ = xpc_connection_create_mach_service(kServiceName, nullptr, 0);
        if (!conn_) {
            os_log_error(Log(), "cannot create connection to %{public}s", kServiceName);
            return;
        }
        xpc_connection_set_event_handler(conn_, ^(xpc_object_t event) {
            if (xpc_get_type(event) == XPC_TYPE_ERROR) {
                // Daemon exited or restarted: its new shared region is a different one.
                stale_.store(true, std::memory_order_release);
            }
        });
        xpc_connection_resume(conn_);

        xpc_object_t msg = xpc_dictionary_create(nullptr, nullptr, 0);
        xpc_dictionary_set_string(msg, "op", "hello");
        xpc_object_t reply = xpc_connection_send_message_with_reply_sync(conn_, msg);
        xpc_release(msg);

        if (xpc_get_type(reply) != XPC_TYPE_DICTIONARY) {
            os_log_error(Log(), "no reply from %{public}s (daemon not running?)", kServiceName);
            xpc_release(reply);
            return;
        }
        const uint64_t version = xpc_dictionary_get_uint64(reply, "version");
        xpc_object_t shmObj = xpc_dictionary_get_value(reply, "shm");
        void* addr = nullptr;
        const size_t len = shmObj ? xpc_shmem_map(shmObj, &addr) : 0;
        xpc_release(reply);

        if (version != FTU_SHM_VERSION || len < sizeof(ftu_shm_t) || !addr) {
            os_log_error(Log(), "bad handshake: version %llu, size %zu", version, len);
            return;
        }
        auto* shm = static_cast<const ftu_shm_t*>(addr);
        if (shm->magic != FTU_SHM_MAGIC || shm->channels != FTU_CHANNELS ||
            shm->ring_frames != FTU_RING_FRAMES) {
            os_log_error(Log(), "shared memory header mismatch");
            return;
        }
        stale_.store(false, std::memory_order_release);
        shm_.store(shm, std::memory_order_release);
        os_log(Log(), "connected to %{public}s", kServiceName);
    }

    std::mutex mutex_;
    xpc_connection_t conn_ = nullptr;
    std::atomic<const ftu_shm_t*> shm_{nullptr};
    std::atomic<bool> stale_{false};
};

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
        for (auto& s : streams_) {
            s->NotifyPropertiesChanged({kAudioStreamPropertyPhysicalFormat, kAudioStreamPropertyVirtualFormat});
        }
        NotifyPropertyChanged(kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeInput);
        NotifyPropertyChanged(kAudioDevicePropertySafetyOffset, kAudioObjectPropertyScopeOutput);
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
        if (Running(s) && ReadClock(s, a)) {
            const double now = static_cast<double>(mach_absolute_time());
            const double sampleNow =
                static_cast<double>(a.sampleTime) + (now - a.hostTime) / a.ticksPerFrame;
            const double zts = std::floor(sampleNow / kZeroTimeStampPeriod) * kZeroTimeStampPeriod;
            *outSampleTime = zts;
            *outHostTime = static_cast<UInt64>(
                static_cast<double>(a.hostTime) + (zts - a.sampleTime) * a.ticksPerFrame);
            *outSeed = kSeedBase + s->generation.load(std::memory_order_acquire);
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
        link_->EnsureConnected();
        // A new IO session starts a new timeline: do not count its first cycle as a jump.
        in_.Reset();
        out_.Reset();
        // The daemon may be at another rate (its default, or a previous session's).
        // Core Audio's nominal rate wins, except on S/PDIF clock, where the source
        // sets the rate and the daemon ignores requests.
        const ftu_shm_t* s = link_->Shm();
        const UInt32 want = rate_->rate.load();
        if (s && s->clock_source.load() == 0 && s->sample_rate.load() != want) {
            link_->RequestRate(want);
        }
        return kAudioHardwareNoError;
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
        if (in_.Jumped(s, t, frames)) {
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
        if (out_.Jumped(s, t, frames)) {
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

        bool Jumped(const ftu_shm_t* s, uint64_t t, UInt32 frames)
        {
            const uint64_t gen = s->generation.load(std::memory_order_relaxed);
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

    auto plugin = std::make_shared<aspl::Plugin>(context);
    plugin->AddDevice(device);

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
