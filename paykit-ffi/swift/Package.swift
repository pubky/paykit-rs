// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "Paykit",
    platforms: [.iOS(.v15)],
    products: [.library(name: "Paykit", targets: ["Paykit"])],
    targets: [
        .binaryTarget(name: "PaykitFFI", path: "PaykitFFI.xcframework"),
        .target(name: "Paykit", dependencies: ["PaykitFFI"])
    ],
    swiftLanguageVersions: [.v5]
)
