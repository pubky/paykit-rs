# Mobile Integration

Use the generated Swift/Kotlin SDK, not a new app-side Paykit protocol layer.
Verify platform argument labels and types in the installed package. The examples
below show call order and arguments as pseudocode, not complete platform code.
Canonical sources: [FFI guide](https://github.com/pubky/paykit-rs/blob/master/paykit-ffi/README.md),
[Rust binding declarations](https://github.com/pubky/paykit-rs/tree/master/paykit-ffi/src),
and [Swift declarations](https://github.com/pubky/paykit-rs/blob/master/paykit-ffi/bindings/ios/paykit.swift).

For custom chat bindings or an apparent gap in platform support, first read
[Integration Boundaries](boundaries.md). For recovering broken peers, use
[Link Recovery](recovery.md); the lifecycle is not specific to Swift or Kotlin.

## Setup and Persistence

```text
config = defaultConfig("example-wallet/wallet")
capabilities = requiredSessionCapabilities(config)
sdk = PaykitSdk.withPaymentAdapter(stateStore, sessionProvider, paymentAdapter, config)
await sdk.initialize()
status = await sdk.identityStatus() // optional until identity state exists
```

- `SdkStateBlobStore` stores the entire opaque `SdkStateBlob` with a revision.
  `saveStateBlobAtomically(blob, expectedRevision)` must reject stale revisions;
  a null expected revision means the store must still be absent. Atomically
  replace the blob and revision together. Return a new revision after a successful
  save. Do not overwrite after a conflict or treat decode/storage errors as an
  empty store. See the [storage contract](https://github.com/pubky/paykit-rs/blob/master/paykit-ffi/src/storage.rs).
- Protect state and secrets at rest. Keep one long-lived handle per active
  identity/receiver. If multiple handles or processes share storage, serialize
  identity lifecycle, restore, and public sync operations as well as implementing
  the store's atomic revision check.
- `SdkPubkySessionProvider` loads current session access; it is not a mandate to
  implement an identity manager. Persist the exported grant/PoP secret, stable
  client ID, and receiver Noise secret securely. The Pubky identity secret is
  optional. Keep the live access returned by bootstrap usable immediately.
- If external auth must survive process loss, persist the complete
  `PubkyAuthRequest.saveState()` result and restore with `resumeAuth`; the
  authorization URL alone is insufficient. Do not log either secret-bearing value.
- A remembered identity with `liveSessionAvailable == false` can mean temporarily
  locked session storage, not sign-out. Preserve local state and restore access.
- Android requires the package's native initialization before network use. Follow
  [Android Initialization](https://github.com/pubky/paykit-rs/blob/master/paykit-ffi/README.md#android-initialization).
  For plain JVM tests, use `PaykitPublicKeys` and `PaykitSdkDefaults` constants
  rather than invoking native-backed key/config functions unnecessarily.

## Publish Private Reservations

On private-payment enrollment, explicitly call
`publishPaykitReceiverMarker(capabilities)` to advertise this receiver's Noise
public key. The remote receiver must publish its marker too. Before the first
list sync, call `ensureLinkWithPeer(counterparty, counterpartyReceiverPath,
maxAdvanceSteps)` and inspect the report. List sync requires a linked peer or a
persisted, resumable handshake; it does not start the handshake itself.

Provide plain `PrivatePaymentListReservationUpdateInput` records containing the
counterparty key, exact remote receiver path, and a list of
`PrivatePaymentEndpointReservationInput` values. Each reservation contains
`reservationId`, `identifier`, `payload`, `expiresAt`, and `attribution`.

```text
report = await sdk.syncPrivatePaymentListsWithReservationsAndProcessOutbound(
    updates,
    clearUnlistedLinkedPeers
)
```

An empty reservations list explicitly clears that target's Private Payment List.
For adapter callbacks, `UseCurrentReceivingDetails` requests ordinary adapter
details; `Reservations` with an empty list requests an empty list. Do not collapse
those responses into one empty/default value.

Set `clearUnlistedLinkedPeers` only when the supplied set is complete for the
receivers being retained; do not enable cleanup for a partial update. For
`syncContactPrivatePaymentLists`, the keep set comes from saved contacts and
their receiver paths. Removing a private list is not disconnecting its Noise link.

`failedToQueue` means there is no queued update for that target. `failedToDeliver`
reports send or cleanup failure after queueing; inspect its nested error and
continue durable work with `processPendingPrivateMessages` when appropriate.
Queueing during a handshake does not guarantee immediate delivery.

## Resolve a Payment

```text
prepared = await sdk.prepareAndResolvePrivateContactPayment(
    counterparty,
    counterpartyReceiverPath,
    amount,                         // PaymentAmountContext or nil/null
    afterPrivatePaymentListVersion, // nil/null until a list was consumed
    maxAdvanceSteps
)
resolution = prepared.resolution
```

Use `resolution.status`, `resolution.state`, and `resolution.payableEndpoints`.
Keep `resolution.privatePaymentListVersion` as the freshness token if consumed.
Retain the link/receive/outbound reports for diagnosing partial progress. This
call never resolves public endpoints; a public flow uses
`resolvePublicContactPayment(counterparty, counterpartyReceiverPath, amount)`.

Endpoint payloads and targets use `PaymentPayload`; call `exportText()` at the
wallet integration boundary, not for routine UI/logging. Wallet-specific
payability and execution remain the payment adapter/application's responsibility.

## Backups and Errors

`SdkStateBlob` is live local persistence; `exportBackupString()` is a separate
SDK backup. Preserve session/key material separately in secure storage. Restore
SDK backups with `restoreBackupString` after restoring appropriate session access.
Neither exporting a backup nor retaining the seed replaces live atomic storage.

Compare `backupStateRevision()` before and after mutating workflows to schedule
the app's backup, including when an operation throws after persisting progress.
Run the after-check in a `finally`/equivalent cleanup path; if it cannot be read,
conservatively mark the backup dirty. This fingerprint excludes transient leases.
It is not `stateRevision()` and must not be used as the store's CAS revision.

Use structured error categories/codes where available. Some batch reports carry
only a generic operation-failed code, not the underlying cause; inspect peer/queue
state as described in [Link Recovery](recovery.md) instead of parsing debug text
or blindly retrying. `PrivateOperationError`
offers `redactedContext` for ordinary logging; `exportDebugDetails()` is for
explicit diagnostics and can reveal sensitive data. Reports may contain
per-target failures even when the outer call succeeds.
