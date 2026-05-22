// swift-tools-version:5.5

import PackageDescription

let tag = "v0.1.0-rc9"
let checksum = "18acfcdac490a0beb47230d8cd5cbafcd164f230f2a1acac0757a2e64193b367"
let url = "https://github.com/pubky/paykit-rs/releases/download/\(tag)/Paykit.xcframework.zip"

let package = Package(
    name: "paykit",
    platforms: [
        .iOS(.v15),
        .macOS(.v12),
    ],
    products: [
        .library(
            name: "Paykit",
            targets: ["PaykitFFI", "Paykit"]),
    ],
    targets: [
        .target(
            name: "Paykit",
            dependencies: ["PaykitFFI"],
            path: "./paykit-ffi/bindings/ios",
            sources: ["paykit.swift"]
        ),
        .binaryTarget(
            name: "PaykitFFI",
            url: url,
            checksum: checksum
        ),
    ]
)
