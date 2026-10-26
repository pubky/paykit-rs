// swift-tools-version:5.5

import PackageDescription

let tag = "v0.1.1-rc2"
let checksum = "886ef57dc75938f2713d2a7e649c6557ea6ba91afed92ef6030554b015d89322"
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
            sources: ["paykit.swift", "PaykitPublicKeys.swift", "PaykitRedaction.swift"]
        ),
        .binaryTarget(
            name: "PaykitFFI",
            url: url,
            checksum: checksum
        ),
    ]
)
