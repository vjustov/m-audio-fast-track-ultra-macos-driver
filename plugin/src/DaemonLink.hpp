// Connection from the plugin to ftu-daemon: the XPC calls (hello, set_rate), the mapped
// shared memory and presence pushes. Separate from Driver.cpp so it can be tested
// against a fake daemon (plugin/tests/DaemonLinkTests.cpp).

#pragma once

#include <dispatch/dispatch.h>
#include <os/log.h>
#include <xpc/xpc.h>

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <functional>
#include <memory>
#include <mutex>
#include <thread>

#include "ftu_shm.h"

namespace ftu {

constexpr const char* kServiceName = "local.ftu.daemon";
// Upper bounds for waiting on the daemon. These waits run on coreaudiod threads, some
// inside HAL configuration changes, so a stuck daemon must not stall them for long.
// The daemon restarts the stream for a rate change in 0.1 to 0.25 s.
constexpr int64_t kReplyTimeoutMs = 300;
constexpr int kRateWaitMs = 1000;
// After the daemon goes away, launchd restarts it within about 0.1 s. Retry the
// connection this often, for up to kReconnectTries attempts.
constexpr int kReconnectIntervalMs = 100;
constexpr int kReconnectTries = 50;

inline os_log_t Log()
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
    // Creates a connection to the daemon, not yet resumed. The plugin uses the launchd
    // service; tests pass a connection to an in-process fake.
    using Connector = std::function<xpc_connection_t()>;

    explicit DaemonLink(Connector connect = DefaultConnector())
        : connect_(std::move(connect))
    {
    }

    static Connector DefaultConnector()
    {
        return [] { return xpc_connection_create_mach_service(kServiceName, nullptr, 0); };
    }

    const ftu_shm_t* Shm() const
    {
        return shm_.load(std::memory_order_acquire);
    }

    // Counts successful connections. Each one maps a new shared region whose positions
    // and generation start over, so it marks a new timeline. Realtime-safe.
    uint64_t Epoch() const
    {
        return epoch_.load(std::memory_order_acquire);
    }

    // Connects if needed and moves the daemon to `rate` on a background queue, so the
    // caller (StartIO) never waits on the daemon. IO plays silence and timestamps fall
    // back to the host clock until the daemon streams at `rate`. Rate 0 only connects.
    void PrepareAsync(uint32_t rate)
    {
        if (rate != 0) {
            wantedRate_.store(rate);
        }
        dispatch_async(Queue(), ^{
            std::lock_guard<std::mutex> lock(mutex_);
            if (!Shm() || stale_.load(std::memory_order_acquire)) {
                Connect();
            }
            const ftu_shm_t* s = Shm();
            // On S/PDIF clock the source sets the rate and the daemon ignores requests.
            if (rate != 0 && s && s->clock_source.load() == 0 && s->sample_rate.load() != rate) {
                RequestRateLocked(rate);
            }
        });
    }

    // Called with the device's presence when the daemon reports it: on connect and on
    // each push. Set once, before the first connection.
    void SetPresenceHandler(std::function<void(bool)> f)
    {
        onPresence_ = std::move(f);
    }

    // Called after the link connected again on its own because the daemon went away
    // while connected. IO is then on the new daemon's timeline. Set once, before the
    // first connection.
    void SetReconnectHandler(std::function<void()> f)
    {
        onReconnect_ = std::move(f);
    }

    // Asks the daemon to stream at `rate` and waits until it does, at most about
    // kReplyTimeoutMs + kRateWaitMs. Not realtime-safe.
    bool RequestRate(uint32_t rate)
    {
        wantedRate_.store(rate);
        std::lock_guard<std::mutex> lock(mutex_);
        return RequestRateLocked(rate);
    }

private:
    static dispatch_queue_t Queue()
    {
        static dispatch_queue_t q = dispatch_queue_create("local.ftu.plugin.daemon", DISPATCH_QUEUE_SERIAL);
        return q;
    }

    // Sends `msg` and waits up to kReplyTimeoutMs for the reply. Returns the reply
    // (caller releases) or nullptr on timeout, in which case the connection is marked
    // stale so the next call reconnects. A late reply is dropped by the handler.
    xpc_object_t SendWithTimeout(xpc_object_t msg)
    {
        struct Pending
        {
            dispatch_semaphore_t done = dispatch_semaphore_create(0);
            std::atomic<xpc_object_t> reply{nullptr};
            std::atomic<bool> abandoned{false};
            ~Pending()
            {
                if (xpc_object_t r = reply.load()) {
                    xpc_release(r);
                }
                dispatch_release(done);
            }
        };
        auto p = std::make_shared<Pending>();
        xpc_connection_send_message_with_reply(conn_, msg, ReplyQueue(), ^(xpc_object_t r) {
            if (!p->abandoned.load()) {
                p->reply.store(xpc_retain(r));
            }
            dispatch_semaphore_signal(p->done);
        });
        if (dispatch_semaphore_wait(p->done, dispatch_time(DISPATCH_TIME_NOW, kReplyTimeoutMs * NSEC_PER_MSEC)) != 0) {
            p->abandoned.store(true);
            stale_.store(true, std::memory_order_release);
            os_log_error(Log(), "no reply from %{public}s within %lld ms", kServiceName, kReplyTimeoutMs);
            return nullptr;
        }
        return xpc_retain(p->reply.load());
    }

    // Replies arrive here, never on Queue(), which may be blocked waiting for them.
    static dispatch_queue_t ReplyQueue()
    {
        static dispatch_queue_t q = dispatch_queue_create("local.ftu.plugin.reply", DISPATCH_QUEUE_SERIAL);
        return q;
    }

    bool RequestRateLocked(uint32_t rate)
    {
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
        xpc_object_t reply = SendWithTimeout(msg);
        xpc_release(msg);
        if (!reply) {
            return false;
        }
        const bool ok = xpc_get_type(reply) == XPC_TYPE_DICTIONARY && xpc_dictionary_get_bool(reply, "ok");
        xpc_release(reply);
        if (!ok) {
            os_log_error(Log(), "daemon refused rate %u", rate);
            return false;
        }

        // The daemon restarts the stream: wait for a new generation at the new rate.
        for (int i = 0; i < kRateWaitMs / 10; i++) {
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
    // The daemon went away: connect to its successor without waiting for a client to
    // start IO or change the rate, which a playing client never does. Until then IO
    // reads the old region, whose `running` flag the old daemon left set.
    void Reconnect(int triesLeft)
    {
        bool done = false;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (!stale_.load(std::memory_order_acquire)) {
                return;  // a call on another path connected first
            }
            Connect();
            const ftu_shm_t* s = Shm();
            done = !stale_.load(std::memory_order_acquire) && s;
            // A new daemon starts at its saved rate, which may not be Core Audio's.
            const uint32_t rate = wantedRate_.load();
            if (done && rate != 0 && s->clock_source.load() == 0 && s->sample_rate.load() != rate) {
                RequestRateLocked(rate);
            }
        }
        if (done) {
            os_log(Log(), "reconnected after the daemon went away");
            if (onReconnect_) {
                onReconnect_();
            }
        } else if (triesLeft > 1) {
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, kReconnectIntervalMs * NSEC_PER_MSEC), Queue(), ^{
                Reconnect(triesLeft - 1);
            });
        } else {
            os_log_error(Log(), "daemon did not come back; next StartIO retries");
        }
    }

    void Connect()
    {
        // Errors from connections this call replaces must not mark the new one stale.
        const uint64_t attempt = ++attempts_;
        if (conn_) {
            xpc_connection_cancel(conn_);
            xpc_release(conn_);
            conn_ = nullptr;
        }
        conn_ = connect_();
        if (!conn_) {
            os_log_error(Log(), "cannot create connection to %{public}s", kServiceName);
            return;
        }
        xpc_connection_set_event_handler(conn_, ^(xpc_object_t event) {
            if (xpc_get_type(event) == XPC_TYPE_ERROR) {
                if (attempts_.load() != attempt) {
                    return;
                }
                // Daemon exited or restarted: its new shared region is a different one.
                const bool wasLive = !stale_.exchange(true, std::memory_order_acq_rel);
                if (wasLive && shm_.load(std::memory_order_acquire)) {
                    dispatch_async(Queue(), ^{
                        Reconnect(kReconnectTries);
                    });
                }
                return;
            }
            const char* op = xpc_dictionary_get_string(event, "op");
            if (op && std::strcmp(op, "presence") == 0) {
                Presence(xpc_dictionary_get_bool(event, "present"));
            }
        });
        xpc_connection_resume(conn_);

        xpc_object_t msg = xpc_dictionary_create(nullptr, nullptr, 0);
        xpc_dictionary_set_string(msg, "op", "hello");
        xpc_object_t reply = SendWithTimeout(msg);
        xpc_release(msg);
        if (!reply) {
            return;
        }
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
        shm_.store(shm, std::memory_order_release);
        epoch_.fetch_add(1, std::memory_order_acq_rel);
        stale_.store(false, std::memory_order_release);
        os_log(Log(), "connected to %{public}s", kServiceName);
        Presence(shm->device_present.load(std::memory_order_acquire) != 0);
    }

    // Hands presence to the handler off the caller's thread, which may hold mutex_ or
    // be an XPC handler: the handler notifies the HAL.
    void Presence(bool present)
    {
        if (!onPresence_) {
            return;
        }
        auto f = onPresence_;
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
            f(present);
        });
    }

    Connector connect_;
    std::mutex mutex_;
    std::function<void(bool)> onPresence_;
    std::function<void()> onReconnect_;
    xpc_connection_t conn_ = nullptr;
    std::atomic<const ftu_shm_t*> shm_{nullptr};
    std::atomic<bool> stale_{false};
    std::atomic<uint64_t> epoch_{0};
    std::atomic<uint64_t> attempts_{0};
    std::atomic<uint32_t> wantedRate_{0};  // Core Audio's last rate, 0 if none yet
};

} // namespace ftu
