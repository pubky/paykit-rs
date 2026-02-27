// swift-tools-version:5.5
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let tag = "v0.0.1"
let checksum = "ab9778e7e2ccfee1263809a258919e48ac390bf17938bd7c2e9c7af4465f03ce"
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
            path: "./bindings/ios",
            sources: ["paykit.swift"]
        ),
        .binaryTarget(
            name: "PaykitFFI",
            url: url,
            checksum: checksum
        )
    ]
)
