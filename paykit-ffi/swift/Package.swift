// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "PaykitV2",
    platforms: [.iOS(.v15)],
    products: [.library(name: "PaykitV2", targets: ["PaykitV2"])],
    targets: [
        .binaryTarget(name: "PaykitV2FFI", path: "PaykitV2FFI.xcframework"),
        .target(name: "PaykitV2", dependencies: ["PaykitV2FFI"])
    ],
    swiftLanguageVersions: [.v5]
)
