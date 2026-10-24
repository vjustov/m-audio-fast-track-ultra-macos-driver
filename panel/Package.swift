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
        )
    ]
)
