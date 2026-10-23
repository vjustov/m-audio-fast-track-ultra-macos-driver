// XPC listener for ftu-daemon. Written in C because the XPC API takes blocks.
//
// Protocol: the client sends {"op": "hello"} and gets back
// {"version": FTU_SHM_VERSION, "shm": <xpc_shmem of the shared region>}.
// Only coreaudiod's user (_coreaudiod) and the daemon's own user may connect.

#include <dispatch/dispatch.h>
#include <string.h>
#include <unistd.h>
#include <xpc/xpc.h>

#include "../../shared/ftu_shm.h"

#define COREAUDIOD_UID 202

static void* g_addr;
static size_t g_len;
static xpc_connection_t g_listener;

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
