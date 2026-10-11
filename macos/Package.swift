// swift-tools-version: 6.2
// SwiftUI shell for macOS. The Rust engine is linked as a static library
// (crates/ffi → libgtt_ffi.a); build it first — `scripts/build-app.sh` does
// both and assembles GlobalTokenTracker.app.
import Foundation
import PackageDescription

let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
// Universal (lipo'd) lib from build-app.sh, else a plain host `cargo build --release`.
let rustLibDirs = ["target/universal-release", "target/release"]
    .map { root.deletingLastPathComponent().appendingPathComponent($0).path }
    .filter { FileManager.default.fileExists(atPath: $0) }

let package = Package(
    name: "GlobalTokenTracker",
    platforms: [.macOS(.v26)],
    targets: [
        .systemLibrary(name: "CGTT", path: "Sources/CGTT"),
        .executableTarget(
            name: "GlobalTokenTracker",
            dependencies: ["CGTT"],
            path: "Sources/GlobalTokenTracker",
            linkerSettings: [
                .unsafeFlags(rustLibDirs.flatMap { ["-L", $0] }),
                .linkedFramework("CoreServices"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("ServiceManagement"),
            ]
        ),
    ]
)
