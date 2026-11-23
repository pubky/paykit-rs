// swift-tools-version: 6.0
// Runs the generated Swift bindings and the test-wallet library on an iOS simulator.
// PAYKIT_APPLE_PACKAGE names the local package that `package-apple.sh` assembled from both
// archives built with `--features test-wallet`. From this directory:
//   PAYKIT_APPLE_PACKAGE="$OUT/apple" xcodebuild test -scheme PaykitWalletDevice-Package \
//       -destination "platform=iOS Simulator,name=iPhone 17"
import Foundation
import PackageDescription

let bindings = Context.environment["PAYKIT_APPLE_PACKAGE"] ?? "../../target/apple"

let package = Package(
    name: "PaykitWalletDevice",
    platforms: [.iOS(.v15)],
    dependencies: [.package(path: bindings)],
    targets: [
        .testTarget(
            name: "WalletOnDevice",
            dependencies: [
                .product(name: "Paykit", package: URL(fileURLWithPath: bindings).lastPathComponent)
            ],
            path: ".",
            exclude: ["android", "hosts/backup.rs", "hosts/build.gradle.kts", "hosts/checkout.rs",
                "hosts/CheckoutConsumer.kt", "hosts/issuer.rs", "hosts/service.rs",
                "hosts/settings.gradle.kts", "hosts/signer.rs", "hosts/WalletConsumer.kt",
                "hosts/SliceWallet.kt", "hosts/InboxConsumer.kt", "hosts/PayerConsumer.kt", "hosts/PayeeConsumer.kt", "hosts/ListsConsumer.kt", "hosts/PublicConsumer.kt", "hosts/PeriodsConsumer.kt", "hosts/AllowanceConsumer.kt", "hosts/ExecutorConsumer.kt"],
            sources: ["hosts/WalletConsumer.swift", "hosts/CheckoutConsumer.swift", "hosts/SliceWallet.swift", "hosts/InboxConsumer.swift", "hosts/PayerConsumer.swift", "hosts/PayeeConsumer.swift", "hosts/ListsConsumer.swift", "hosts/PublicConsumer.swift", "hosts/PeriodsConsumer.swift", "hosts/AllowanceConsumer.swift", "hosts/ExecutorConsumer.swift",
                "ios/WalletOnDevice.swift"],
            swiftSettings: [.define("PAYKIT_CHECKOUT_TEST"), .define("PAYKIT_DEVICE_TEST")]
        )
    ]
)
