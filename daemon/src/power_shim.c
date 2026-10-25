// System sleep notifications for ftu-daemon. Written in C because IOKit's power API is
// callback based.
//
// Before sleep the daemon stops the stream and releases the device, as the vendor kext
// stops its engine (docs/protocol.md, Streaming engine). The service loop polls
// ftu_power_sleep_pending() and calls ftu_power_released() once the device is closed;
// sleep is allowed then, or after RELEASE_TIMEOUT_MS if the loop is stuck. After wake
// the flag clears and the loop reopens the device.

#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOMessage.h>
#include <IOKit/pwr_mgt/IOPMLib.h>
#include <pthread.h>
#include <stdatomic.h>
#include <unistd.h>

#define RELEASE_TIMEOUT_MS 3000

static io_connect_t g_root;
static _Atomic int g_sleep_pending;
static _Atomic int g_released;

int ftu_power_sleep_pending(void) {
    return atomic_load(&g_sleep_pending);
}

void ftu_power_released(void) {
    atomic_store(&g_released, 1);
}

static void on_power(void* refcon, io_service_t service, natural_t type, void* arg) {
    (void)refcon;
    (void)service;
    switch (type) {
    case kIOMessageCanSystemSleep:
        // Idle sleep may go ahead; the WillSleep that follows releases the device.
        IOAllowPowerChange(g_root, (long)arg);
        break;
    case kIOMessageSystemWillSleep:
        atomic_store(&g_released, 0);
        atomic_store(&g_sleep_pending, 1);
        for (int waited = 0; waited < RELEASE_TIMEOUT_MS && !atomic_load(&g_released); waited += 10) {
            usleep(10 * 1000);
        }
        IOAllowPowerChange(g_root, (long)arg);
        break;
    case kIOMessageSystemHasPoweredOn:
        atomic_store(&g_sleep_pending, 0);
        break;
    default:
        break;
    }
}

static void* power_thread(void* unused) {
    (void)unused;
    IONotificationPortRef port = NULL;
    io_object_t notifier;
    g_root = IORegisterForSystemPower(NULL, &port, on_power, &notifier);
    if (!g_root) {
        return NULL;
    }
    CFRunLoopAddSource(CFRunLoopGetCurrent(), IONotificationPortGetRunLoopSource(port), kCFRunLoopDefaultMode);
    CFRunLoopRun();
    return NULL;
}

// Starts listening on a thread of its own. Returns 0 on success.
int ftu_power_start(void) {
    pthread_t t;
    if (pthread_create(&t, NULL, power_thread, NULL) != 0) {
        return -1;
    }
    pthread_detach(t);
    return 0;
}
