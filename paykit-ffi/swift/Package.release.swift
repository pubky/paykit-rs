// swift-tools-version: 5.9
import PackageDescription

// paykit-ffi/build.sh renders this at a release, with the release tag and the checksum of the
// archive attached to it, as the Package.swift at the repository root.
let tag = "@TAG@"
let checksum = "@CHECKSUM@"
let url = "https://github.com/pubky/paykit-rs/releases/download/\(tag)/Paykit.xcframework.zip"

let package = Package(
    name: "Paykit",
    platforms: [.iOS(.v15)],
    products: [.library(name: "Paykit", targets: ["Paykit"])],
    targets: [
        .binaryTarget(name: "PaykitFFI", url: url, checksum: checksum),
        .target(
            name: "Paykit",
            dependencies: ["PaykitFFI"],
            path: "paykit-ffi/bindings/ios",
            sources: ["Paykit.swift", "WalletRedaction.swift"]
        )
    ],
    swiftLanguageVersions: [.v5]
)
