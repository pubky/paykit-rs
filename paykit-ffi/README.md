# Paykit wallets for Kotlin and Swift

`WalletEnrollment` prepares a wallet application for the signer and keeps the selected approval on the device. Both languages use the same native implementation. The Kotlin package is `com.synonym.paykit`. The Swift module is `Paykit`.

Use a host-owned, protected store with process-wide and cross-process writer exclusion for each physical namespace. Implement key reads and an atomic, durable commit that verifies the exact owner in the same transaction. Report an unknown outcome when a commit may have applied. If acquisition may have applied before its answer was lost, report an unknown outcome. A release can be retried with the original token. It must leave a newer owner intact. A process exit must release its ownership. Keep signing material and the protected approval out of system backups and logs.

1. Save a random 16-byte enrollment reference, then call `beginEnrollment` for its App ID and public record path.
2. Call `prepareEnrollment` with the original Pubky authentication request, secret and a nonempty `WalletScopes`. For a payer, use `WalletScopes(pay = true)` in Kotlin or `WalletScopes(pay: true)` in Swift. The SDK adds required scopes, including contacts for a payer. Choose either `request` for all sales or `requestLabels` for labelled sales. Use prefixes such as `paykit-cap` in `extensions`. A retry with a different effective scope set returns `EnrollmentConflict` (Paykit spec 3.1, 3.2). Give the returned signed app request to the signer.
3. Call `acceptApproval` with the user's selected identity and the original signed companion claim. It verifies the identity, the request, certificate and scope keys before committing the original result. An interrupted host reopens the same reference.

## Threads, waiting and the payment method

Every wallet call runs on Paykit's own threads. The thread that calls only waits for the answer, so any thread can call, the main thread included. An open wallet runs its calls one at a time, in the order they arrive. A read waits behind a call that is running, such as a payment waiting for the host's `pay`. Cancelling a Kotlin coroutine stops its wait. A cancelled Swift task keeps waiting for the answer. Neither stops the call: it runs to its end, and its result is in the store for the next read. The one exception is `feedWait`, which changes nothing: a stopped Kotlin wait ends it. A commit in progress keeps the store's writer until it finishes.

Paykit calls the host's store, Homeserver and payment method from its own threads, never the main thread. Do not move a callback to the main thread and wait for it there. Never call the wallet from inside a callback. A synchronous callback that calls the wallet on its own thread gets `Reentrant`. A callback that waits on another thread that calls the wallet hangs the wallet, because that call waits for the callback. Store calls and the method's synchronous calls (`selectEndpoints`, `transfer`, `settlementId`) must answer at local speed, with no network and no user interface. A synchronous call that throws never crashes the wallet: a thrown `selectEndpoints` pays no endpoint of that request, a thrown `transfer` reads as an amount the method can't tell before paying, which the user confirms, and a thrown `settlementId` as no settlement ID. That is not retried: a proof of a payment that names no request is then dropped for good, since the payee's feed moves past it and a resend of it is dropped as already processed, and a requested proof skips the duplicate checks and asks the user whether it is credited already. Never throw `settlementId` for a fault that may pass. Paykit waits at most 10 seconds for a lookup (`payment`, `allowanceHistory`, `verify`); after that the lookup counts as not available now. It also waits at most 10 seconds for a Homeserver call; a write past that limit has an unknown outcome, as for any transport timeout.

`pay` saves the attempt ID durably, submits the payment with that ID as its idempotency key, and answers. If the payment is not settled yet, answer `Pending`; never wait for settlement. Answer with a refusal only if nothing was submitted and nothing will be. A thrown error counts as possibly sent. Paykit waits at most 30 seconds for `pay`, a fixed limit for now. After that it stops waiting and records the payment as possibly sent, and the attempt stays open. Your `pay` keeps running to its end. When it answers, the wallet that is open for that store resumes its payments by itself. A paid or failed answer is kept and recorded by that resume, or by the next wallet opened in the same process if none is open, and the resume is retried until it records it; the host calls nothing, and reads `payments()` again to see it. After any other answer that resume asks `payment`. If the process ended first, the next `resumePayments` asks `payment` with the attempt ID. While `pay` runs, Paykit doesn't ask `payment` about that attempt; a `pay` that hasn't answered after four times the limit no longer counts as running, and Paykit asks `payment` again, so a null answered while `pay` is still submitting lets the payment be made twice. A kept answer, with its proof, and the record of a `pay` that never answers stay in memory until a wallet of that app opens and records it, or the process ends.

`payment` answers for every attempt ID that `pay` was called with, after a restart too. Answer `Pending`, never null, from the moment `pay` starts until the payment has settled. Null means `pay` was never called with that ID. Paykit reads null as never sent, so the payment can be made again. A method whose declaration has no history can't look a payment up: opening, reading, receiving, backing up and reconciling still work, and paying is refused with `PaymentMethodWithoutLookup`.

## Install and reopen the approved payer

Create a `WalletHost` with the same protected store, the user's authenticated Homeserver session, the host's payment method, the method's `MethodDeclaration` and the endpoints it pays, each an `EndpointSupport` with its Payment Endpoint Identifier and the fractional digits it supports for that endpoint's transfer amount. The declaration and endpoints are given once: a method whose guarantees change is given to a new `WalletHost`. The store also provides ordered key scans and conditional deletes for the installed Switch app. Call `completeEnrollment` with the original enrollment reference. If the process stops during installation, use the same reference again. After installation, `load` opens the same App ID and keys. Shut down the existing writer before opening another handle for the same physical store.

The optional `config` argument to `WalletHost` applies to enrollment, load and restore. Omit it for the SDK defaults. In `WalletConfig`, `orphanAfterS` sets the orphan age in seconds (the default is the rail's floor), `sellingLabel` selects the label for outgoing requests, and `resendBudget` sets the daily message budget per requester. List each extension kind the host handles in `extensionKinds`, such as `paykit-cap.offer`. Its prefix must be approved for the app to receive it. Set `publishesRecord` to true to publish the app record through the host's authenticated Homeserver session. Adding a public Payment Endpoint also requires the unlabelled request scope (Paykit spec 3.1, 3.3, 4, 7.2, 9, Switch spec 4.2).

If an install or load fails after acquiring its writer, the call waits for release before returning the original failure. A release that cannot be confirmed returns a storage error, and the next opener retries that exact owner. An enrollment still waiting for approval only reads its enrollment reference.

`payments` reads the original attempts and their stored stages. `resumePayments` uses the SDK's paying order and the method's lookup of the original attempt ID, including after a lost result. The method must keep that same ID as its idempotency key. Return a definitive refusal only when submission was excluded. Persist a method outcome before returning it. `requestReceipts` reads the original seller's signed receipt references.

Use `backup` to export the installed wallet’s SDK backup. Store its bytes and App ID together in protected storage. Use `load` to reopen the installed state on the same device. For a retained grant, `reconcileAllowance` reports usage only from the original authority and complete account-wide method history. An unknown result leaves spending held.

A failed operation says what to do next by its error case, and each of the six cases below carries a `WalletFailure` with a stable `code` to branch on and a `context` for logs. The context is fixed text, the code and what its case means, so it never carries what the service, the network, the payment method or the store said:

- `NeedsUser`: show it to the user. A held, blocked, may-be-paid, transfer or rate refusal goes ahead once the user confirms it and the same `failure.refusal` is passed back in `confirmedRefusals`; every other code names its own call.
- `RetryLater`: try again later, after `retryAfterMs` when it is set. The network, the service or a rule that can't be met now.
- `OutcomeUnknown`: the call may have taken effect. Reconcile before trying again; `keptBatch` names a Switch batch kept for this call, which the wallet sends again by itself.
- `NotSent`: no payment was made and no message went out, so the call is safe to make again once its cause is fixed. An attempt's own writes may have happened: an `attempt_failed` stores its attempt, closed as failed, and may mark a single-use endpoint used, which a retry then leaves out.
- `Permanent`: the answer is final, such as a rejected, cancelled or already paid request. Trying again gives the same answer.
- `Recovery`: the wallet as a whole stopped, for an alarm, a recovery notice, a refused certificate or its authority. Show it and follow the recovery steps. A hold on one record comes as `NeedsUser`, never as `Recovery`.

The enrollment, restore and store failures keep their own cases, and so do four that carry no `WalletFailure`: `Closed` (the wallet was shut down), `Reentrant` (a callback called the wallet), `PaymentMethodWithoutLookup` (the method can't look up a payment) and `RuntimeUnavailable` (the bindings' runtime couldn't start). Codes are fixed with the first packaged release; a code a host doesn't know is handled by its case. The "Wallet errors" table of the [host guide](../docs/paykit-host-guide.md#wallet-errors) lists every code, where it comes from, its case and what the host does.

A failure from the payment rules carries a `PaymentRefusal` in `failure.refusal`. Use `kind()` for its data. `reason()` is fixed text, the refusal's code and what its case means: the SDK's own refusal text can quote what the switch or the network said, so it never leaves the bindings. For example, `IdentityUnavailable` means the wallet needs a fresh identity before it can resume a payment. A `Divergent` refusal identifies the affected records and versions. `attemptedFailure()` exposes the original refusal when an attempt failed before dispatch. A failure from the Switch client can also carry a typed `switchRefusal`, including the generation requiring recovery. `KeptBatchesAhead` identifies the earlier batch and its answer window. The separate `keptBatch` field identifies a batch retained for this call. A refusal the bindings don't recognize has the code `unknown` and comes as `Permanent`: show it, and never retry or confirm it.

## Restore the wallet from a protected backup

Ask whether the source device is still in the user's possession or known to have been wiped. If its custody is unknown, have the signer revoke the old app and enroll a new one. For a device the user can account for, pass the original App ID, backup bytes and custody decision to `restore` (Switch spec §12.4). If a storage commit has an unknown result, use `load` on that store to learn what was installed.

1. Run the normal foreground `receive`. Follow additional pages and handle its recovery or availability notices. `restoreState` reports the current cursor verdict, any questioned registrations and whether reconciliation continues. After interruption, load the same App ID and read this state again.
2. Show the questioned registrations to the user and ask whether the original device ran after the backup. Call `confirmRestoredRegistrations` with the displayed numbers only if the user recognizes them. Otherwise, `refuseRestoredRegistrations` stops the app and requests revocation. Read the resulting alarm. If the question changed, show its current registrations before asking again.
3. Call `reconcile` and show any doubtful attempts. If `payerSkipped` is set, the payer pass still needs the proper certificate and record coverage. An uncertain attempt may already have paid. Use `resolveConfirmedAttempt` for the exact obligation and attempt only after the user chooses its outcome. An absent receipt alone cannot establish failure.
4. After the user reviews the results, call `endConfirmedReconciliation`. Reopen and check `restoreState` when interrupted. Each registered protocol must finish its own reconciliation. A request may still be held because the seller could have cancelled it since the backup. Show that request and its `heldAfterRestore` refusal before passing the user's confirmation to `payRequest`.

## Receive and acknowledge

Call `receive` on startup and after a feed wake. It returns one bounded report. Use `more` to decide when to read another page. If `stopped` is set, inspect the reported event. Persist or show each duty before acknowledging its ID with `acknowledgeEvents`. This acknowledges the processed prefix for Paykit. Each other protocol in a shared app remains responsible for its own duties.

`messageDelivery` can return no entry after retention expires or after a restore. A `Failed` delivery can follow a refused retry when the recipient accepted an earlier try and its answer was lost. Check the request or record before sending a replacement (Switch spec 11, 12.4).

After an interruption, `unacknowledgedEvents` lists surviving Paykit duties in order. After a failed receive, keep those events and wait for a successful receive before acknowledging through the last duty in that report. `recoveryRequired` returns the current signed service notice across reopen even after acknowledging delivery. A clearance event names the generation that was held. The receive cursor can already be in the next generation.

## Pay a one-time request

After reading the feed, show the request from `requests` with its payee, amount, payment reference, selling label and status. Pass its original `RequestId` to `payRequest` when the user chooses to pay. The result includes the method outcome or typed failure and any failures for individual public Payment Endpoints. Inspect `refusal.kind()` to show a quote, earlier payment doubt, or other confirmation. Pass back the same refusal only after the user confirms it.

For a divergent request, show the original record, collection, version and warnings returned by the refusal or `divergentRecords`. After the user confirms that observation, call `confirmDivergent` with its `observation`. Run `receive` until its waiting writes are drained before trying the payment again. If the write changed, show the new observation and ask again.

A one-time obligation can appear in `reconcile` with a provider-loss boundary and an empty attempt list. Show the original obligation and boundary, including that another wallet may already have paid it at the old service. Only if the user states it was unpaid, pass that boundary’s `payment` to `confirmNotPaidBeforeLoss`. A `LossChanged` refusal requires a fresh report and a new decision. The statement applies only to that obligation and loss. Other attempt uncertainties still require their original outcomes.

After an interrupted payment, shut down the old handle, reopen the same App ID and use `resumePayments` to look up the original attempt. Use `reconcile` for obligations with an unknown outcome and show the remaining doubt before any user-confirmed resolution. `requestReceipts` returns the signed seller receipt after it arrives. On a newly restored store that lookup can be held until the first inventory completes. `notices` have their own acknowledgement, separate from feed duties.

## Generate both languages

From the workspace root, build the generator and native library from this same checkout. Set `OUT` to a scratch directory. The commands below use the standard Cargo target directory. Substitute your configured target directory when it differs.

```sh
cargo build --locked --release -p paykit-ffi --features bindgen
paykit-ffi/generate-bindings.sh target/release/paykit-bindgen \
    target/release/libpaykit.dylib "$OUT"
```

Generate into a fresh `OUT` folder whenever the generated names change: the Swift commands below compile `"$OUT"/swift/*.swift`, and an older generated file left beside the new one would be compiled with it. Otherwise pass `Paykit.swift` and `WalletRedaction.swift` explicitly. On Linux use `libpaykit.so` for generation. Use the generated Swift support files and Kotlin source together with their matching native library. Default descriptions redact approval requests, protected store values, payment journal entries and their related payment data. Read typed fields explicitly when displaying a payment to the user.

### Android

Build each ABI required by the host with the Android NDK and `cargo-ndk`. For an arm64 wallet targeting API 28 and above, run the following command.

```sh
cargo ndk -t arm64-v8a -p 28 -o "$OUT/jniLibs" build --locked --release -p paykit-ffi --lib
```

Add `$OUT/kotlin` to a Kotlin Android library's sources and `$OUT/jniLibs` to its main JNI library sources. Package JNA's Android AAR and Kotlin coroutines in the consuming application. Keep the library name `paykit`. The enrollment and typed-error consumer in `tests/hosts/WalletConsumer.kt` uses these same Kotlin calls on the JVM.

### Apple

Build arm64 device and simulator archives from the same checkout. Use a fresh destination for the local Swift package.

```sh
cargo build --locked --release -p paykit-ffi --lib --target aarch64-apple-ios
cargo build --locked --release -p paykit-ffi --lib --target aarch64-apple-ios-sim
paykit-ffi/package-apple.sh "$OUT" \
    target/aarch64-apple-ios/release/libpaykit.a \
    target/aarch64-apple-ios-sim/release/libpaykit.a "$OUT/apple"
```

Add the resulting local package to Xcode and import `Paykit`. The generated UniFFI module builds in Swift 5 language mode and can be imported from a Swift 6 host. `tests/hosts/WalletConsumer.swift` exercises that combination.

The native enrollment cases use the actual signer, SQLite-backed device storage, Scripted and PostgreSQL. Set the Switch harness PostgreSQL environment to require the service case.

### Run the generated-language consumers

The test issuer and enrolled checkout use Scripted by default. Set `PUBKY_SWITCH_REQUIRE_DB=1` and `PUBKY_SWITCH_TEST_DATABASE_URL` to run the signer, checkout, paid-wallet and recovery fixtures against the PostgreSQL-backed service.

On macOS, use the host library and the sources generated above. Set `LIB` to the absolute directory containing `libpaykit.dylib`, and use an absolute `OUT` path. The Kotlin consumer runs with a local Gradle installation. The Swift consumer imports the generated module from a host compiled in Swift 6 mode.

```sh
cargo build --locked -p paykit-ffi --example wallet-test-issuer
export PAYKIT_TEST_ISSUER="$LIB/examples/wallet-test-issuer"
gradle -p paykit-ffi/tests/hosts -Pbindings="$OUT/kotlin" -PnativeLibraryDir="$LIB" run

swiftc -swift-version 5 -I "$OUT/swift" -Xcc "-fmodule-map-file=$OUT/swift/PaykitFFI.modulemap" \
    -emit-library -emit-module -module-name Paykit "$OUT"/swift/*.swift \
    -emit-module-path "$OUT/Paykit.swiftmodule" -o "$OUT/libPaykitSwift.dylib" -L "$LIB" -lpaykit
swiftc -swift-version 6 -parse-as-library -I "$OUT" -I "$OUT/swift" \
    -Xcc "-fmodule-map-file=$OUT/swift/PaykitFFI.modulemap" \
    paykit-ffi/tests/hosts/WalletConsumer.swift -L "$OUT" -lPaykitSwift -L "$LIB" -lpaykit \
    -o "$OUT/wallet-consumer"
DYLD_LIBRARY_PATH="$OUT:$LIB" "$OUT/wallet-consumer"
```

To exercise a full checkout from the host, build the host library with `--features bindgen,test-wallet` and regenerate the bindings. Supply `-PcheckoutTest=true` to the Kotlin command. For the Swift 6 command, add `-D PAYKIT_CHECKOUT_TEST`, `paykit-ffi/tests/hosts/CheckoutConsumer.swift`, `paykit-ffi/tests/hosts/ConsumerWallet.swift` and the eight group callers `paykit-ffi/tests/hosts/{Inbox,Payer,Payee,Lists,Public,Periods,Allowance,Executor}Consumer.swift`. After the checkouts, the same run hands one enrolled wallet to each group's caller. Both consumers enroll with a local shop, acknowledge a recovery notice across reopen, and stop waiting for the payment call after the host method has durably charged it. The payment is recorded when the method answers, and the reopened wallet uses the original attempt to obtain the signed seller receipt. The same programs also exercise a divergent request with a held receipt and a payment whose only doubt is a provider loss.

Run either checkout consumer with `PAYKIT_TEST_RESTORE=confirm` to restore a backup into a fresh store, review a registration made since the backup, and pay the original held request after confirmation. Use `refuse` to decline the registration and observe the compromise alarm, or `doubt` to close the wallet while the method's charge is still running and reconcile that possibly sent attempt. The test checkout waits 2 seconds for `pay` instead of 30.
