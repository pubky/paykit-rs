// swift-tools-version:5.5

import PackageDescription

let tag = "v0.1.0-rc14"
let checksum = "be8bfdb7ce17689c64d994be3d743fdd8ec29abc03e36854fa6d152d32b15429"
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
