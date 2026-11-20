# Paykit wallets for Kotlin and Swift

`WalletEnrollment` prepares a payer application for the signer and keeps the selected approval on the device. Both languages use the same native implementation. The Kotlin package is `com.synonym.paykit.v2`. The Swift module is `PaykitV2`.

Use a host-owned, protected store with process-wide and cross-process writer exclusion for each physical namespace. Implement key reads and an atomic, durable commit that verifies the exact owner in the same transaction. Report an unknown outcome when a commit may have applied. If acquisition may have applied before its answer was lost, report an unknown outcome. A release can be retried with the original token. It must leave a newer owner intact. A process exit must release its ownership. Keep signing material and the protected approval out of system backups and logs.

1. Save a random 16-byte enrollment reference, then call `beginEnrollment` for its App ID and public record path.
2. Call `prepareEnrollment` with the original Pubky authentication request and secret. Give the returned signed app request to the signer.
3. Call `acceptApproval` with the user's selected identity and the original signed companion claim. It verifies the identity, the request, certificate and scope keys before committing the original result. An interrupted host reopens the same reference.

Cancelling a Swift task or Kotlin coroutine stops its wait. A host commit already in progress retains ownership until it finishes. Reopen after that commit to observe its durable result.

## Install and reopen the approved payer

Create a `WalletHost` with the same protected store, the user's authenticated Homeserver session, the host's payment method and its supported Payment Endpoint Identifiers. The store also provides ordered key scans and conditional deletes for the installed Switch app. Call `completeEnrollment` with the original enrollment reference. If the process stops during installation, use the same reference again. After installation, `load` opens the same App ID and keys. Shut down the existing writer before opening another handle for the same physical store.

`payments` reads the original attempts and their stored stages. `resumePayments` uses the SDK's paying order and the method's lookup of the original attempt ID, including after a lost result. The method must keep that same ID as its idempotency key. Return a definitive refusal only when submission was excluded. Persist a method outcome before returning it, and keep method callbacks independent of the wallet lock. `requestReceipts` reads the original seller's signed receipt references.

Keep backups and their App IDs in protected storage. Before calling `restore`, ask whether the original device is in the user's possession or known to have been wiped. An unaccounted-for device requires revocation and a new enrollment (Switch spec 12.4). The SDK reads and reconciles the restored state under the original App ID. For a retained grant, `reconcileAllowance` reports usage only from the original authority and complete account-wide method history. An unknown result leaves spending held.

## Generate both languages

From the workspace root, build the generator and native library from this same checkout. Set `OUT` to a scratch directory. The commands below use the standard Cargo target directory. Substitute your configured target directory when it differs.

```sh
cargo build --locked --release -p paykit-ffi --features bindgen
paykit-ffi/generate-bindings.sh target/release/paykit-bindgen \
    target/release/libpaykit_v2.dylib "$OUT"
```

On Linux use `libpaykit_v2.so` for generation. Use the generated Swift support files and Kotlin source together with their matching native library. Generated descriptions redact the approval request and protected storage values.

### Android

Build each ABI required by the host with the Android NDK and `cargo-ndk`. For an arm64 wallet targeting API 28 and above, run the following command.

```sh
cargo ndk -t arm64-v8a -p 28 -o "$OUT/jniLibs" build --locked --release -p paykit-ffi --lib
```

Add `$OUT/kotlin` to a Kotlin Android library's sources and `$OUT/jniLibs` to its main JNI library sources. Package JNA's Android AAR and Kotlin coroutines in the consuming application. Keep the library name `paykit_v2`. The enrollment and typed-error consumer in `tests/hosts/WalletConsumer.kt` uses these same Kotlin calls on the JVM.

### Apple

Build arm64 device and simulator archives from the same checkout. Use a fresh destination for the local Swift package.

```sh
cargo build --locked --release -p paykit-ffi --lib --target aarch64-apple-ios
cargo build --locked --release -p paykit-ffi --lib --target aarch64-apple-ios-sim
paykit-ffi/package-apple.sh "$OUT" \
    target/aarch64-apple-ios/release/libpaykit_v2.a \
    target/aarch64-apple-ios-sim/release/libpaykit_v2.a "$OUT/apple"
```

Add the resulting local package to Xcode and import `PaykitV2`. The generated UniFFI module builds in Swift 5 language mode and can be imported from a Swift 6 host. `tests/hosts/WalletConsumer.swift` exercises that combination.

The native enrollment cases use the actual signer, SQLite-backed device storage, Scripted and PostgreSQL. Set the Switch harness PostgreSQL environment to require the service case.

### Run the generated-language consumers

On macOS, use the host library and the sources generated above. Set `LIB` to the absolute directory containing `libpaykit_v2.dylib`, and use an absolute `OUT` path. The Kotlin consumer runs with a local Gradle installation. The Swift consumer imports the generated module from a host compiled in Swift 6 mode.

```sh
cargo build --locked -p paykit-ffi --example wallet-test-issuer
export PAYKIT_TEST_ISSUER="$LIB/examples/wallet-test-issuer"
gradle -p paykit-ffi/tests/hosts -Pbindings="$OUT/kotlin" -PnativeLibraryDir="$LIB" run

swiftc -swift-version 5 -I "$OUT/swift" -Xcc "-fmodule-map-file=$OUT/swift/PaykitV2FFI.modulemap" \
    -emit-library -emit-module -module-name PaykitV2 "$OUT"/swift/*.swift \
    -emit-module-path "$OUT/PaykitV2.swiftmodule" -o "$OUT/libPaykitV2.dylib" -L "$LIB" -lpaykit_v2
swiftc -swift-version 6 -parse-as-library -I "$OUT" -I "$OUT/swift" \
    -Xcc "-fmodule-map-file=$OUT/swift/PaykitV2FFI.modulemap" \
    paykit-ffi/tests/hosts/WalletConsumer.swift -L "$OUT" -lPaykitV2 -L "$LIB" -lpaykit_v2 \
    -o "$OUT/wallet-consumer"
DYLD_LIBRARY_PATH="$OUT:$LIB" "$OUT/wallet-consumer"
```
