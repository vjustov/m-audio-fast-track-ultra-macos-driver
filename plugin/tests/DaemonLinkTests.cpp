// Tests for DaemonLink (plugin/src/DaemonLink.hpp) against a fake ftu-daemon.
//
// The fake listens on an anonymous XPC connection, so no launchd service is needed: the
// link connects through an endpoint instead of the mach service name. It answers hello
// with its own shared memory and set_rate by changing the rate there, and each test can
// make it hang (keep a request unanswered), refuse to apply a rate, push presence or drop its clients.
//
// Run: make test (or plugin/build/daemon_link_tests).

#include "DaemonLink.hpp"

#include <sys/mman.h>

#include <chrono>
#include <cstdio>
#include <functional>
#include <vector>

using namespace std::chrono;
using ftu::DaemonLink;

namespace {

int failures = 0;

#define CHECK(cond)                                                          \
    do {                                                                     \
        if (!(cond)) {                                                       \
            std::fprintf(stderr, "  FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); \
            failures++;                                                      \
        }                                                                    \
    } while (0)

// Waits up to `ms` for `cond`.
bool Eventually(std::function<bool()> cond, int ms = 1000)
{
    const auto end = steady_clock::now() + milliseconds(ms);
    while (steady_clock::now() < end) {
        if (cond()) {
            return true;
        }
        std::this_thread::sleep_for(milliseconds(5));
    }
    return cond();
}

long ElapsedMs(steady_clock::time_point t0)
{
    return static_cast<long>(duration_cast<milliseconds>(steady_clock::now() - t0).count());
}

class FakeDaemon
{
public:
    std::atomic<bool> replyHello{true};
    std::atomic<bool> replyRate{true};
    std::atomic<bool> applyRate{true};
    std::atomic<int> hellos{0};

    FakeDaemon()
    {
        len_ = (sizeof(ftu_shm_t) + 16383) & ~size_t(16383);
        void* p = mmap(nullptr, len_, PROT_READ | PROT_WRITE, MAP_ANON | MAP_SHARED, -1, 0);
        shm_ = static_cast<ftu_shm_t*>(p);
        shm_->magic = FTU_SHM_MAGIC;
        shm_->version = FTU_SHM_VERSION;
        shm_->channels = FTU_CHANNELS;
        shm_->ring_frames = FTU_RING_FRAMES;
        shm_->running.store(1);
        shm_->sample_rate.store(48000);
        shm_->device_present.store(1);

        q_ = dispatch_queue_create("fake.daemon", DISPATCH_QUEUE_SERIAL);
        listener_ = xpc_connection_create(nullptr, q_);
        xpc_connection_set_event_handler(listener_, ^(xpc_object_t peer) {
            if (xpc_get_type(peer) != XPC_TYPE_CONNECTION) {
                return;
            }
            xpc_connection_t c = (xpc_connection_t)xpc_retain(peer);
            peers_.push_back(c);
            xpc_connection_set_event_handler(c, ^(xpc_object_t msg) {
                if (xpc_get_type(msg) == XPC_TYPE_DICTIONARY) {
                    Handle(c, msg);
                }
            });
            xpc_connection_resume(c);
        });
        xpc_connection_resume(listener_);
        endpoint_ = xpc_endpoint_create(listener_);
    }

    DaemonLink::Connector Connector()
    {
        xpc_endpoint_t ep = endpoint_;
        return [ep] { return xpc_connection_create_from_endpoint(ep); };
    }

    ftu_shm_t* Shm()
    {
        return shm_;
    }

    void PushPresence(bool present)
    {
        dispatch_sync(q_, ^{
            for (xpc_connection_t c : peers_) {
                xpc_object_t m = xpc_dictionary_create(nullptr, nullptr, 0);
                xpc_dictionary_set_string(m, "op", "presence");
                xpc_dictionary_set_bool(m, "present", present);
                xpc_connection_send_message(c, m);
                xpc_release(m);
            }
        });
    }

    // As if the daemon exited: every client sees its connection invalidated.
    void DropClients()
    {
        dispatch_sync(q_, ^{
            for (xpc_connection_t c : peers_) {
                xpc_connection_cancel(c);
                xpc_release(c);
            }
            peers_.clear();
        });
    }

private:
    void Handle(xpc_connection_t c, xpc_object_t msg)
    {
        const char* op = xpc_dictionary_get_string(msg, "op");
        if (!op) {
            return;
        }
        if (std::strcmp(op, "hello") == 0) {
            hellos++;
            if (!replyHello) {
                Hold(msg);
                return;
            }
            xpc_object_t r = xpc_dictionary_create_reply(msg);
            xpc_object_t shm = xpc_shmem_create(shm_, len_);
            xpc_dictionary_set_value(r, "shm", shm);
            xpc_release(shm);
            xpc_dictionary_set_uint64(r, "version", FTU_SHM_VERSION);
            xpc_connection_send_message(c, r);
            xpc_release(r);
        } else if (std::strcmp(op, "set_rate") == 0) {
            if (!replyRate) {
                Hold(msg);
                return;
            }
            const uint32_t rate = static_cast<uint32_t>(xpc_dictionary_get_uint64(msg, "rate"));
            xpc_object_t r = xpc_dictionary_create_reply(msg);
            xpc_dictionary_set_bool(r, "ok", true);
            xpc_connection_send_message(c, r);
            xpc_release(r);
            if (applyRate) {
                // The daemon restarts the stream: a new generation at the new rate.
                ftu_shm_t* s = shm_;
                dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 50 * NSEC_PER_MSEC), q_, ^{
                    s->sample_rate.store(rate);
                    s->generation.fetch_add(1);
                });
            }
        }
    }

    // A hung daemon keeps the request: XPC answers a request released without a reply
    // with an error at once, which would look like a refusal, not silence.
    void Hold(xpc_object_t msg)
    {
        held_.push_back(xpc_retain(msg));
    }

    std::vector<xpc_object_t> held_;  // only touched on q_
    size_t len_ = 0;
    ftu_shm_t* shm_ = nullptr;
    dispatch_queue_t q_;
    xpc_connection_t listener_;
    xpc_endpoint_t endpoint_;
    std::vector<xpc_connection_t> peers_;  // only touched on q_
};

// Each test gets its own fake and link. Both are leaked: XPC handlers may still run.
struct Fixture
{
    FakeDaemon* daemon = new FakeDaemon();
    DaemonLink* link = new DaemonLink(daemon->Connector());
    std::atomic<int> presence{-1};
    std::atomic<int> reconnects{0};

    Fixture()
    {
        link->SetPresenceHandler([this](bool p) { presence.store(p ? 1 : 0); });
        link->SetReconnectHandler([this] { reconnects++; });
    }

    bool Connect()
    {
        link->PrepareAsync(0);
        return Eventually([&] { return link->Shm() != nullptr; });
    }
};

void ConnectMapsTheSharedMemoryAndReportsPresence()
{
    Fixture f;
    CHECK(f.Connect());
    CHECK(f.link->Shm()->sample_rate.load() == 48000);
    CHECK(Eventually([&] { return f.presence.load() == 1; }));
}

void PresencePushesReachTheHandler()
{
    Fixture f;
    CHECK(f.Connect());
    CHECK(Eventually([&] { return f.presence.load() == 1; }));
    f.daemon->PushPresence(false);
    CHECK(Eventually([&] { return f.presence.load() == 0; }));
    f.daemon->PushPresence(true);
    CHECK(Eventually([&] { return f.presence.load() == 1; }));
}

void RateRequestWaitsForTheNewStream()
{
    Fixture f;
    CHECK(f.link->RequestRate(96000));
    CHECK(f.daemon->Shm()->sample_rate.load() == 96000);
    // Already there: no round trip to the daemon.
    const auto t0 = steady_clock::now();
    CHECK(f.link->RequestRate(96000));
    CHECK(ElapsedMs(t0) < 50);
}

void SilentDaemonTimesOutAndIsRetried()
{
    Fixture f;
    CHECK(f.Connect());
    f.daemon->replyRate = false;
    auto t0 = steady_clock::now();
    CHECK(!f.link->RequestRate(44100));
    const long ms = ElapsedMs(t0);
    CHECK(ms >= ftu::kReplyTimeoutMs - 20 && ms < ftu::kReplyTimeoutMs + 300);
    // The timeout marks the connection stale: the next call connects again.
    const int hellos = f.daemon->hellos.load();
    f.daemon->replyRate = true;
    CHECK(f.link->RequestRate(44100));
    CHECK(f.daemon->hellos.load() == hellos + 1);
}

void UnappliedRateGivesUpAfterTheWait()
{
    Fixture f;
    CHECK(f.Connect());
    f.daemon->applyRate = false;
    const auto t0 = steady_clock::now();
    CHECK(!f.link->RequestRate(88200));
    const long ms = ElapsedMs(t0);
    CHECK(ms >= ftu::kRateWaitMs - 50 && ms < ftu::kRateWaitMs + ftu::kReplyTimeoutMs + 300);
}

void SilentHelloLeavesTheLinkDisconnected()
{
    Fixture f;
    f.daemon->replyHello = false;
    const auto t0 = steady_clock::now();
    CHECK(!f.link->RequestRate(44100));
    CHECK(ElapsedMs(t0) < ftu::kReplyTimeoutMs + 300);
    CHECK(f.link->Shm() == nullptr);
    f.daemon->replyHello = true;
    CHECK(f.link->RequestRate(44100));
}

void DaemonRestartIsReconnectedWithoutACall()
{
    Fixture f;
    CHECK(f.Connect());
    const int hellos = f.daemon->hellos.load();
    const uint64_t epoch = f.link->Epoch();
    f.daemon->DropClients();
    // A playing client makes no call, so the link must connect again by itself.
    CHECK(Eventually([&] { return f.reconnects.load() == 1; }));
    CHECK(f.daemon->hellos.load() == hellos + 1);
    CHECK(f.link->Epoch() == epoch + 1);
}

void ReconnectRestoresTheRate()
{
    Fixture f;
    CHECK(f.link->RequestRate(96000));
    // The new daemon starts at its saved rate.
    f.daemon->Shm()->sample_rate.store(48000);
    f.daemon->DropClients();
    CHECK(Eventually([&] { return f.reconnects.load() == 1; }, 2000));
    CHECK(f.daemon->Shm()->sample_rate.load() == 96000);
}

void ReconnectRetriesUntilTheDaemonAnswers()
{
    Fixture f;
    CHECK(f.Connect());
    f.daemon->replyHello = false;
    f.daemon->DropClients();
    std::this_thread::sleep_for(milliseconds(ftu::kReplyTimeoutMs + 200));
    CHECK(f.reconnects.load() == 0);
    f.daemon->replyHello = true;
    CHECK(Eventually([&] { return f.reconnects.load() == 1; }, 2000));
    CHECK(f.link->Shm() != nullptr);
}

void ReplacedConnectionDoesNotMarkTheNewOneStale()
{
    Fixture f;
    CHECK(f.Connect());
    // A timeout replaces the connection; the old one's cancel error must not trigger
    // a reconnect of the new one.
    f.daemon->replyRate = false;
    CHECK(!f.link->RequestRate(44100));
    f.daemon->replyRate = true;
    CHECK(f.link->RequestRate(44100));
    const int hellos = f.daemon->hellos.load();
    std::this_thread::sleep_for(milliseconds(300));
    CHECK(f.daemon->hellos.load() == hellos);
    CHECK(f.reconnects.load() == 0);
}

} // namespace

int main()
{
    const std::pair<const char*, void (*)()> tests[] = {
        {"connect maps the shared memory and reports presence", ConnectMapsTheSharedMemoryAndReportsPresence},
        {"presence pushes reach the handler", PresencePushesReachTheHandler},
        {"rate request waits for the new stream", RateRequestWaitsForTheNewStream},
        {"silent daemon times out and is retried", SilentDaemonTimesOutAndIsRetried},
        {"unapplied rate gives up after the wait", UnappliedRateGivesUpAfterTheWait},
        {"silent hello leaves the link disconnected", SilentHelloLeavesTheLinkDisconnected},
        {"daemon restart is reconnected without a call", DaemonRestartIsReconnectedWithoutACall},
        {"reconnect restores the rate", ReconnectRestoresTheRate},
        {"reconnect retries until the daemon answers", ReconnectRetriesUntilTheDaemonAnswers},
        {"replaced connection does not mark the new one stale", ReplacedConnectionDoesNotMarkTheNewOneStale},
    };
    for (const auto& [name, fn] : tests) {
        const int before = failures;
        fn();
        std::printf("%s  %s\n", failures == before ? "ok  " : "FAIL", name);
    }
    std::printf("%zu tests, %d failed checks\n", std::size(tests), failures);
    return failures == 0 ? 0 : 1;
}
