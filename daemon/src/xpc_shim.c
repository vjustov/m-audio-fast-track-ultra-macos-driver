// XPC listener for ftu-daemon. Written in C because the XPC API takes blocks.
//
// Protocol:
//   {"op": "hello"}                -> {"version": FTU_SHM_VERSION, "shm": <xpc_shmem of the region>}
//   {"op": "set_rate", "rate": Hz} -> {"ok": bool}. The stream restarts at the new rate;
//                                     the client watches shm sample_rate and generation.
//   {"op": "ctl", "request": JSON}  -> {"response": JSON}. Mixer control, see ctl.rs.
// Only coreaudiod's user (_coreaudiod) and the daemon's own user may connect.

#include <dispatch/dispatch.h>
#include <stdatomic.h>
#include <string.h>
#include <unistd.h>
#include <xpc/xpc.h>

#include "../../shared/ftu_shm.h"

#define COREAUDIOD_UID 202

static void* g_addr;
static size_t g_len;
static xpc_connection_t g_listener;
// ctl requests run here, not on the listener queue. They only update state and save it
// to disk (the service loop does the USB writes), but coreaudiod's hello and set_rate
// should never queue behind a file write.
static dispatch_queue_t g_ctl_queue;
static _Atomic uint32_t g_requested_rate;

// Implemented in Rust (ctl.rs).
char* ftu_ctl_request(const char* request);
void ftu_ctl_free(char* reply);

static int valid_rate(uint64_t r) {
    return r == 44100 || r == 48000 || r == 88200 || r == 96000;
}

// Returns the last rate requested over XPC and clears it, or 0 if none is pending.
uint32_t ftu_xpc_take_requested_rate(void) {
    return atomic_exchange(&g_requested_rate, 0);
}

static void handle_message(xpc_connection_t peer, xpc_object_t msg) {
    if (xpc_get_type(msg) != XPC_TYPE_DICTIONARY) {
        return;
    }
    xpc_object_t reply = xpc_dictionary_create_reply(msg);
    if (!reply) {
        return;
    }
    const char* op = xpc_dictionary_get_string(msg, "op");
    if (op && strcmp(op, "hello") == 0) {
        xpc_object_t shm = xpc_shmem_create(g_addr, g_len);
        if (shm) {
            xpc_dictionary_set_value(reply, "shm", shm);
            xpc_release(shm);
        }
        xpc_dictionary_set_uint64(reply, "version", FTU_SHM_VERSION);
    } else if (op && strcmp(op, "set_rate") == 0) {
        uint64_t rate = xpc_dictionary_get_uint64(msg, "rate");
        int ok = valid_rate(rate);
        if (ok) {
            atomic_store(&g_requested_rate, (uint32_t)rate);
        }
        xpc_dictionary_set_bool(reply, "ok", ok);
    } else if (op && strcmp(op, "ctl") == 0) {
        xpc_retain(peer);
        xpc_retain(msg);
        dispatch_async(g_ctl_queue, ^{
            char* response = ftu_ctl_request(xpc_dictionary_get_string(msg, "request"));
            xpc_dictionary_set_string(reply, "response", response);
            ftu_ctl_free(response);
            xpc_connection_send_message(peer, reply);
            xpc_release(reply);
            xpc_release(msg);
            xpc_release(peer);
        });
        return;
    } else {
        xpc_dictionary_set_string(reply, "error", "unknown op");
    }
    xpc_connection_send_message(peer, reply);
    xpc_release(reply);
}

static void handle_peer(xpc_object_t obj) {
    if (xpc_get_type(obj) != XPC_TYPE_CONNECTION) {
        return;
    }
    xpc_connection_t peer = (xpc_connection_t)obj;
    uid_t uid = xpc_connection_get_euid(peer);
    if (uid != COREAUDIOD_UID && uid != getuid()) {
        xpc_connection_cancel(peer);
        return;
    }
    xpc_retain(peer);
    xpc_connection_set_event_handler(peer, ^(xpc_object_t msg) {
        if (xpc_get_type(msg) == XPC_TYPE_ERROR) {
            if (msg == XPC_ERROR_CONNECTION_INVALID) {
                xpc_release(peer);
            }
            return;
        }
        handle_message(peer, msg);
    });
    xpc_connection_resume(peer);
}

// Starts listening on the launchd-registered Mach service `name`.
// Returns 0 on success. Handlers run on a dispatch queue owned by XPC.
int ftu_xpc_listen(const char* name, void* addr, size_t len) {
    g_addr = addr;
    g_len = len;
    dispatch_queue_t q = dispatch_queue_create("ftu.xpc", DISPATCH_QUEUE_SERIAL);
    g_ctl_queue = dispatch_queue_create("ftu.xpc.ctl", DISPATCH_QUEUE_SERIAL);
    g_listener = xpc_connection_create_mach_service(name, q, XPC_CONNECTION_MACH_SERVICE_LISTENER);
    if (!g_listener) {
        return -1;
    }
    xpc_connection_set_event_handler(g_listener, ^(xpc_object_t obj) {
        handle_peer(obj);
    });
    xpc_connection_resume(g_listener);
    return 0;
}

// Client side, for `ftu-daemon ctl`: sends one ctl request and waits for the reply.
// Returns a malloc'd JSON string (free with free()) or NULL if the daemon is unreachable.
char* ftu_xpc_ctl_call(const char* name, const char* request) {
    xpc_connection_t conn = xpc_connection_create_mach_service(name, NULL, 0);
    if (!conn) {
        return NULL;
    }
    xpc_connection_set_event_handler(conn, ^(xpc_object_t e) {
        (void)e;
    });
    xpc_connection_resume(conn);
    xpc_object_t msg = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_string(msg, "op", "ctl");
    xpc_dictionary_set_string(msg, "request", request);
    xpc_object_t reply = xpc_connection_send_message_with_reply_sync(conn, msg);
    char* out = NULL;
    if (xpc_get_type(reply) == XPC_TYPE_DICTIONARY) {
        const char* r = xpc_dictionary_get_string(reply, "response");
        if (r) {
            out = strdup(r);
        }
    }
    xpc_release(reply);
    xpc_release(msg);
    xpc_connection_cancel(conn);
    xpc_release(conn);
    return out;
}
