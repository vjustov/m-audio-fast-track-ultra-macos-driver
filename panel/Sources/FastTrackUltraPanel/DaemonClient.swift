// XPC client for ftu-daemon's "ctl" op. Requests and replies are JSON strings;
// see daemon/src/ctl.rs for the request set.

import Foundation
import XPC

final class DaemonClient {
    static let service = "local.ftu.daemon"

    private let queue = DispatchQueue(label: "ftu.panel.xpc")
    private var conn: xpc_connection_t?

    /// Call on `queue` only.
    private func connection() -> xpc_connection_t {
        if let c = conn {
            return c
        }
        let c = xpc_connection_create_mach_service(Self.service, queue, 0)
        xpc_connection_set_event_handler(c) { [weak self] event in
            // Interrupted or invalid: build a fresh connection on the next request.
            if xpc_get_type(event) == XPC_TYPE_ERROR {
                self?.conn = nil
            }
        }
        xpc_connection_resume(c)
        conn = c
        return c
    }

    /// Sends one request. `reply` runs on the main queue with the decoded reply, or nil
    /// if the daemon could not be reached.
    func request(_ body: [String: Any], reply: @escaping (Any?) -> Void) {
        guard let data = try? JSONSerialization.data(withJSONObject: body),
              let json = String(data: data, encoding: .utf8)
        else {
            reply(nil)
            return
        }
        queue.async {
            let msg = xpc_dictionary_create(nil, nil, 0)
            xpc_dictionary_set_string(msg, "op", "ctl")
            xpc_dictionary_set_string(msg, "request", json)
            xpc_connection_send_message_with_reply(self.connection(), msg, self.queue) { r in
                var result: Any?
                if xpc_get_type(r) == XPC_TYPE_DICTIONARY, let p = xpc_dictionary_get_string(r, "response") {
                    result = try? JSONSerialization.jsonObject(with: Data(String(cString: p).utf8))
                }
                DispatchQueue.main.async { reply(result) }
            }
        }
    }
}
