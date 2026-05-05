// swift-tools-version:5.5

import PackageDescription

let tag = "v0.1.0-rc4"
let checksum = "8c735bdfc532c6ff1624aeba3a5000fc08f5bef5a1b98be7ec141c345b548096"
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
