// The daemon's replies, as recorded in shared/fixtures, must decode with the panel's
// types, and a state the panel saves and sends back ("put") must keep every field.

import Foundation
import XCTest

@testable import FastTrackUltraPanel

final class ContractTests: XCTestCase {
    private func fixture(_ name: String) throws -> Data {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .appendingPathComponent("../../../shared/fixtures/\(name)")
            .standardizedFileURL
        return try Data(contentsOf: url)
    }

    /// JSON as plain objects with nulls removed: Swift omits nil optionals, and the
    /// daemon reads a missing optional as null.
    private func normalized(_ data: Data) throws -> NSObject {
        func strip(_ v: Any) -> Any {
            if let d = v as? [String: Any] {
                return d.filter { !($0.value is NSNull) }.mapValues(strip)
            }
            if let a = v as? [Any] {
                return a.map(strip)
            }
            return v
        }
        return strip(try JSONSerialization.jsonObject(with: data)) as! NSObject
    }

    func testMixerStateDecodesAndRoundTrips() throws {
        let data = try fixture("mixer-state.json")
        let state = try JSONDecoder().decode(MixerState.self, from: data)
        XCTAssertEqual(state.buses.count, 4)
        XCTAssertEqual(state.buses[0][0].level, -6)
        XCTAssertEqual(state.buses[0][0].pan, 0.25)
        XCTAssertEqual(state.fx.program, 6)
        XCTAssertEqual(state.fx.bypass, [false, true])
        XCTAssertEqual(state.fx.programs?.count, 8)
        XCTAssertEqual(state.fx.post_fader, true)
        XCTAssertEqual(state.fx.programs?[5].feedback, 0.25, "plate keeps the feedback set before switching to delay")
        XCTAssertEqual(state.outputs[0].level, -1.5)
        let again = try JSONEncoder().encode(state)
        XCTAssertEqual(try normalized(again), try normalized(data), "the panel would drop or change fields on put")
    }

    func testStatusDecodes() throws {
        let st = try JSONDecoder().decode(DeviceStatus.self, from: fixture("status.json"))
        XCTAssertTrue(st.connected)
        XCTAssertTrue(st.streaming)
        XCTAssertEqual(st.sample_rate, 48000)
        XCTAssertEqual(st.clock, "internal")
        XCTAssertEqual(st.power, "psu")
        XCTAssertEqual(st.firmware, "1.51")
    }

    func testMetersDecode() throws {
        let m = try JSONDecoder().decode(Meters.self, from: fixture("meters.json"))
        XCTAssertEqual(m.in.count, 8)
        XCTAssertEqual(m.in[0], 3276)
        XCTAssertEqual(m.daw.count, 8)
        XCTAssertEqual(m.out.count, 8)
        XCTAssertEqual(m.status, 0x0100)
    }
}
