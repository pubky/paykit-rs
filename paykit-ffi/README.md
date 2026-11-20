# Paykit wallets for Kotlin and Swift

`WalletEnrollment` prepares a payer application for the signer and keeps the selected approval on the device. Both languages use the same native implementation. The Kotlin package is `com.synonym.paykit.v2`. The Swift module is `PaykitV2`.

Use a host-owned, protected store with process-wide and cross-process writer exclusion for each physical namespace. Implement key reads and an atomic, durable commit that verifies the exact owner in the same transaction. Report an unknown outcome when a commit may have applied. If acquisition may have applied before its answer was lost, report an unknown outcome. A release can be retried with the original token. It must leave a newer owner intact. A process exit must release its ownership. Keep signing material and the protected approval out of system backups and logs.

1. Save a random 16-byte enrollment reference, then call `beginEnrollment` for its App ID and public record path.
2. Call `prepareEnrollment` with the original Pubky authentication request and secret. Give the returned signed app request to the signer.
3. Call `acceptApproval` with the user's selected identity and the original signed companion claim. It verifies the identity, the request, certificate and scope keys before committing the original result. An interrupted host reopens the same reference.

Cancelling a Swift task or Kotlin coroutine stops its wait. A host commit already in progress retains ownership until it finishes. Reopen after that commit to observe its durable result.

## Install and reopen the approved payer

Create a `WalletHost` with the same protected store, the user's authenticated Homeserver session, the host's payment method and its supported Payment Endpoint Identifiers. The store also provides ordered key scans and conditional deletes for the installed Switch app. Call `completeEnrollment` with the original enrollment reference. If the process stops during installation, use the same reference again. After installation, `load` opens the same App ID and keys. Shut down the existing writer before opening another handle for the same physical store.

If an install or load fails after acquiring its writer, the call waits for release before returning the original failure. A release that cannot be confirmed returns a storage error, and the next opener retries that exact owner. An enrollment still waiting for approval only reads its enrollment reference.

`payments` reads the original attempts and their stored stages. `resumePayments` uses the SDK's paying order and the method's lookup of the original attempt ID, including after a lost result. The method must keep that same ID as its idempotency key. Return a definitive refusal only when submission was excluded. Persist a method outcome before returning it, and keep method callbacks independent of the wallet lock. `requestReceipts` reads the original seller's signed receipt references.

Use `backup` to export the installed wallet’s SDK backup. Store its bytes and App ID together in protected storage. Use `load` to reopen the installed state on the same device. For a retained grant, `reconcileAllowance` reports usage only from the original authority and complete account-wide method history. An unknown result leaves spending held.

A refused operation carries a `PaymentRefusal`. Use `kind()` to choose the next action and `reason()` for display. For example, `IdentityUnavailable` means the wallet needs a fresh identity before it can resume a payment. A `Divergent` refusal identifies the affected records and versions. `attemptedFailure()` exposes the original refusal when an attempt failed before dispatch. A client operation can also carry a typed `switchRefusal`, including the generation requiring recovery. `KeptBatchesAhead` identifies the earlier batch and its answer window. The separate `keptBatch` field identifies a batch retained for this call. Keep an unrecognized refusal held.

## Receive and acknowledge

Call `receive` on startup and after a feed wake. It returns one bounded report. Use `more` to decide when to read another page. If `stopped` is set, inspect the reported event. Persist or show each duty before acknowledging its ID with `acknowledgeEvents`. This acknowledges the processed prefix for Paykit. Each other protocol in a shared app remains responsible for its own duties.

After an interruption, `unacknowledgedEvents` lists surviving Paykit duties in order. After a failed receive, keep those events and wait for a successful receive before acknowledging through the last duty in that report. `recoveryRequired` returns the current signed service notice across reopen even after acknowledging delivery. A clearance event names the generation that was held. The receive cursor can already be in the next generation.

## Pay a one-time request

After reading the feed, show the request from `requests` with its payee, amount, payment reference, selling label and status. Pass its original `RequestId` to `payRequest` when the user chooses to pay. The result includes the method outcome or typed failure and any failures for individual public Payment Endpoints. Inspect `refusal.kind()` to show a quote, earlier payment doubt, or other confirmation. Pass back the same refusal only after the user confirms it.

For a divergent request, show the original record, collection, version and warnings returned by the refusal or `divergentRecords`. After the user confirms that observation, call `confirmDivergent` with its `observation`. Run `receive` until its waiting writes are drained before trying the payment again. If the write changed, show the new observation and ask again.

A one-time obligation can appear in `reconcile` with a provider-loss boundary and an empty attempt list. Show the original obligation and boundary, including that another wallet may already have paid it at the old service. Only if the user states it was unpaid, pass that boundary’s `payment` to `confirmNotPaidBeforeLoss`. A `LossChanged` refusal requires a fresh report and a new decision. The statement applies only to that obligation and loss. Other attempt uncertainties still require their original outcomes.

After an interrupted payment, shut down the old handle, reopen the same App ID and use `resumePayments` to look up the original attempt. Use `reconcile` for obligations with an unknown outcome and show the remaining doubt before any user-confirmed resolution. `requestReceipts` returns the signed seller receipt after it arrives. On a newly restored store that lookup can be held until the first inventory completes. `notices` and the payee deletion notices each have their own acknowledgement, separate from feed duties.

## Generate both languages

From the workspace root, build the generator and native library from this same checkout. Set `OUT` to a scratch directory. The commands below use the standard Cargo target directory. Substitute your configured target directory when it differs.

```sh
cargo build --locked --release -p paykit-ffi --features bindgen
paykit-ffi/generate-bindings.sh target/release/paykit-bindgen \
    target/release/libpaykit_v2.dylib "$OUT"
```

On Linux use `libpaykit_v2.so` for generation. Use the generated Swift support files and Kotlin source together with their matching native library. Default descriptions redact approval requests, protected store values, payment journal entries and their related payment data. Read typed fields explicitly when displaying a payment to the user.

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

The test issuer and enrolled checkout use Scripted by default. Set `PUBKY_SWITCH_REQUIRE_DB=1` and `PUBKY_SWITCH_TEST_DATABASE_URL` to run the signer, checkout, paid-wallet and recovery fixtures against the PostgreSQL-backed service.

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

To exercise a full checkout from the host, build the host library with `--features bindgen,test-wallet` and regenerate the bindings. Supply `-PcheckoutTest=true` to the Kotlin command. For the Swift 6 command, add `-D PAYKIT_CHECKOUT_TEST` and `paykit-ffi/tests/hosts/CheckoutConsumer.swift`. Both consumers enroll with a local shop, acknowledge a recovery notice across reopen, and cancel the payment call after the host method has durably charged it. The reopened wallet uses the original attempt to obtain the signed seller receipt. The same programs also exercise a divergent request with a held receipt and a payment whose only doubt is a provider loss.
