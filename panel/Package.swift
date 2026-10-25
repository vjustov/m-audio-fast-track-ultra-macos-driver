// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "FastTrackUltraPanel",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "FastTrackUltraPanel",
            path: "Sources/FastTrackUltraPanel",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        // Decodes the daemon's JSON fixtures (shared/fixtures, checked on the daemon side
        // by ctl::api::tests::contract_fixtures_match) with the panel's types.
        .testTarget(
            name: "FastTrackUltraPanelTests",
            dependencies: ["FastTrackUltraPanel"],
            path: "Tests/FastTrackUltraPanelTests",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
