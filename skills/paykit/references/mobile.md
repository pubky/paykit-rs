# Mobile Integration

Use generated Swift/Kotlin SDK declarations from the installed artifact. The
camelCase calls below have been checked against FFI source and generated Swift;
argument types/labels must still match the application's pin. No receiver path
argument is present. Read [Identity and State](identity-state.md) for the shared
architecture and [Link Recovery](recovery.md) for workers/errors.

## Shared-State Setup

Swift-shaped call-order example; the app supplies the providers. Set each
`appCapabilities` flag to `true` only for a feature the app supports; this example
starts with all flags disabled:

```text
config = defaultConfig(appId: "example-wallet")
sessionCapabilities = requiredSessionCapabilities()
appCapabilities = PaykitAppCapabilities(
    privatePayments: false,
    paymentRequests: false,
    receipts: false,
    outgoingPayments: false
)
sdk = PaykitSdk.withPaymentAdapterAndPubkySharedState(
    sessionProvider: sessionProvider,
    paymentAdapter: paymentAdapter,
    config: config
)
status = await sdk.initialize()
registry = await sdk.publishPaykitApp(
    displayName: "Example Wallet",
    capabilities: appCapabilities
)
```

Use `sessionCapabilities` (a `String`) when authorizing the Pubky session behind
`sessionProvider`. The `appCapabilities` record advertises supported features in
the App Registry; it does not grant session permissions.

This constructor has no `stateStore` parameter or `SdkStateBlobStore` callbacks.
`withPaymentAdapterAndPubkySharedStateAndClientConfig` adds `pubkyClient`.
State-backed calls require active grant access and current Paykit key material;
shared-state reads are not an offline cache. `identityStatus()` is optional
before initialization; use its `capability`, not an obsolete
`liveSessionAvailable` field. Missing/locked access does not authorize deletion.

`PubkySessionAccess(clientId:sessionSecret:localSecretKey:paykitIdentitySecretKey:)`
accepts an optional root key and optional delegated/current-generation key.
`PubkyLocalSecretKey.derivePaykitIdentitySecretKey(keyGeneration:)` derives a
key; `PaykitIdentitySecretKey(bytes:keyGeneration:)` imports one. The root-only
fallback is generation 1, not automatic detection of the current generation.
Persist credentials, client ID, and key/generation in secure storage.

Use separate grants for independently restored sessions: restoring the same
grant replaces its existing bearer. Return bootstrap's live `sessionAccess`
with the same client configuration instead of immediately restoring it again.
For pending external auth, persist all of `PubkyAuthRequest.saveState()` and
restore via `resumeAuth`; the URL alone omits the proof-of-possession key. Do
not log either. Once completion consumes an approval, a later exchange failure
or cancellation requires a new auth request.

Callbacks must not reenter the same SDK handle while it awaits them. Android
must call `PaykitAndroid.initialize(applicationContext)` before networking;
plain JVM tests can use `PaykitPublicKeys` and `PaykitSdkDefaults` constants.
Keep one long-lived handle per active app/identity. Switch identities with a new
handle and the correct backing, never the preceding identity's blob.

## Reservation Publication and Resolution

Call `ensureLinkWithPeer(counterparty:maxAdvanceSteps:)` before initial private
list publication. Pass `PrivatePaymentListReservationUpdateInput(counterparty,
reservations)` with no remote app/path argument. Each
`PrivatePaymentEndpointReservationInput` contains `reservationId`, `identifier`,
`payload`, `expiresAt`, and `attribution`. Use
`syncPrivatePaymentListsWithReservationsAndProcessOutbound(updates:clearUnlistedLinkedPeers:)`.
An empty reservation list clears this app's list for that counterparty;
`UseCurrentReceivingDetails` and `Reservations([])` are distinct adapter responses.
Only enable unlisted-peer cleanup for a complete keep set for this app.

Inspect `failedToQueue` and `failedToDeliver`: neither means rollback. An
unconfirmed shared-state write may have committed; earlier work can still be
pending. Retain submitted and previously unresolved wallet reservations; FFI does
not expose durable reservation records. Retry unchanged intent with the same
complete inputs and reservation IDs while the details remain valid; retries may
publish again.
Do not replay an older update over a newer intended list. Release only after
successful SDK-requested adapter cancellation or adapter-confirmed safe expiry or
cleanup, not merely a failed sync or superseding list. Cancellation callbacks must
be idempotent. See [Payment Workflows](workflows.md) for updates and consumption.

```text
prepared = await sdk.prepareAndResolvePrivateContactPayment(
    counterparty: counterparty,
    amount: amount,
    afterPrivatePaymentListVersion: consumedVersion,
    maxAdvanceSteps: maxAdvanceSteps
)
```

`amount` is optional `PaymentAmountContext`; `consumedVersion` is optional
`UInt64` in Swift. Inspect `prepared.resolution.status`, `.state`,
`.privatePaymentListVersion`, and `.payableEndpoints`, plus preparation reports.
Public resolution is `resolvePublicContactPayment(counterparty:amount:)`, with
no private fallback. For a Payment Request, use the request-specific resolution
APIs rather than replacing its destination constraints with contact resolution.
Export `PaymentPayload` text only at the wallet boundary, never routine logging.

## Callback Storage and Backup Boundaries

Use `withPaymentAdapter(stateStore:sessionProvider:paymentAdapter:config:)` only
when intentionally selecting custom/local storage. Every runtime for an identity
must resolve to the same logical state; independent local stores are not shared
state. `saveStateBlobAtomically(blob, expectedRevision)` must atomically store
blob/revision, reject stale revisions, and require absence for a null revision.
Return a new nonempty revision never reused for another blob; reject decode
errors rather than loading empty state. Protect these plaintext blobs at rest.

`exportBackupString()` is a separate, unencrypted hex export, not the live Pubky
resource. Encrypt it at the app backup boundary. Compare `backupStateRevision()`
before/after mutations, including a `finally` path after failures, to schedule
backup; failed comparison means conservatively dirty. This excludes transient
leases and is not `stateRevision()` or a storage CAS token. Restore/recovery
constraints and key handling are in [Identity and State](identity-state.md#backups-and-restore).

Android callback-supplied blob, payment-payload, and reservation-attribution
wrappers implement `AutoCloseable`; export needed values and close wrappers
before returning. Swift uses ARC. Treat explicitly exported private fields as
sensitive even when generated descriptions are redacted.
`PrivateOperationError.category`/`code` support branching; `redactedContext` is
for normal diagnostics. `exportDebugDetails()` is sensitive, explicit diagnostics
only. Generic batch codes do not prove retryability.

Sources: [constructors/backups](https://github.com/pubky/paykit-rs/blob/master/paykit-ffi/src/sdk.rs),
[session callbacks](https://github.com/pubky/paykit-rs/blob/master/paykit-ffi/src/session.rs),
[storage contract](https://github.com/pubky/paykit-rs/blob/master/paykit-ffi/src/storage.rs),
[generated Swift](https://github.com/pubky/paykit-rs/blob/master/paykit-ffi/bindings/ios/paykit.swift).
