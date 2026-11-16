// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "OrbKit",
    platforms: [.iOS("26.1")],
    products: [
        .library(name: "OrbKit", targets: ["OrbKit"]),
    ],
    targets: [
        // Built by scripts/build-xcframework.sh.
        .binaryTarget(name: "OrbFFI", path: "OrbFFI.xcframework"),
        .target(
            name: "OrbKit",
            dependencies: ["OrbFFI"],
            linkerSettings: [
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                // The embedding model's tokenizer has C++ in it.
                .linkedLibrary("c++"),
            ]
        ),
        .testTarget(name: "OrbKitTests", dependencies: ["OrbKit"]),
    ]
)
