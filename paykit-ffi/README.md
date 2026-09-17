# Paykit FFI

UniFFI bindings for [paykit-sdk](../paykit-sdk/), targeting iOS (Swift) and
Android (Kotlin).

The generated bindings expose the SDK runtime foundation: configuration, Pubky
session bootstrap helpers, opaque SDK state storage callbacks, payment adapter
callbacks, identity initialization/sign-out, SDK backup/restore, public Payment
Endpoint sync, Paykit Profile and Contact Record workflows, and public Pubky
read helpers. Product workflows such as private links, payment requests,
receipts, and contact payment resolution belong on the SDK surface rather than
on low-level `paykit-lib` protocol bindings.

## Exported Surface

### Runtime

- `PaykitSdk` — stateful SDK runtime handle.
- `SdkStateBlobStore` — platform callback interface for opaque SDK state
  blob load/save.
- `SdkPubkySessionProvider` — platform callback interface for live Pubky
  session access and public storage availability.
- `SdkPaymentAdapter` — platform callback interface for receiving details,
  endpoint reservation cleanup, payable endpoint ordering, and payment target
  construction.
- `PaykitSdk.initialize`, `identityStatus`, `signOut`, and
  `forgetSessionAccess` — app-facing account/session lifecycle for the current
  Paykit runtime. `signOut` revokes the Pubky grant and preserves local state if
  live access or remote revocation is unavailable; `forgetSessionAccess`
  performs explicit local-only cleanup.
- `PaykitSdk.stateRevision` — return the platform SDK state revision so
  apps can detect when SDK-managed state changed.
- `PaykitSdk.backupStateRevision` — fingerprint the backup contents without
  transient operation leases, so empty polls do not trigger app backups.
- `PubkySessionAccess` — opaque Pubky session access material. Use its
  explicit export methods only when persisting or loading platform-protected
  session state.
- `defaultConfig(receiverPath)` — return the default `PaykitSdkConfig` policy
  for an explicit Paykit receiver path.
- `defaultPubkyClientConfig()` — return default `PubkyClientConfig`.
- `requiredSessionCapabilities(config)` — return Pubky capabilities required by
  a config.

FFI methods accept raw z32 Pubky public keys and `pubky...` app-key strings.
App-facing records return `pubky...` strings. Use
`normalizePubkyPublicKey`, `rawPubkyPublicKey`, and
`redactedPubkyPublicKey` at app boundaries instead of hand-rolled conversion.
Android also ships pure Kotlin `PaykitPublicKeys` helpers, and Swift ships
pure `PaykitPublicKeys` helpers, for app code and unit tests that should not
load the native UniFFI library just to format keys.

### Public Payment Endpoints

- `PaykitSdk.withPaymentAdapter` — create a runtime with payment adapter
  callbacks.
- `paykitReceiverPaths` — list a Pubky identity's Paykit receiver paths before
  scoped public Payment List reads.
- `publishPaykitReceiverMarker`, `removePaykitReceiverMarker`, and
  `paykitReceiverMarker` — publish or inspect a lightweight public receiver
  discovery marker.
- `PaykitSdk.syncPublicEndpoints` — publish current public receiving details
  and remove stale SDK-managed public Payment Endpoints.
- `PaykitSdk.syncPublicEndpointsWithReceivingDetails` — publish explicit
  public receiving details without relying on adapter-side mutable state.
- `EndpointSyncReport` — published, removed, and failed endpoint changes.

The payment adapter returns receiving details and candidate ids. Apps execute
payments outside Paykit; Paykit SDK only routes, records, and validates the
Paykit-side workflow.

### Private Links and Stream Processing

- `PaykitSdk.initiateLinkWithPeer`, `acceptLinkWithPeer`, and
  `advanceLinkHandshake` — establish Encrypted Links with counterparties.
- `PaykitSdk.linkedPeers`, `blockPeer`, and `unblockPeer` — inspect and
  manage local peer state.
- `PaykitSdk.receivePrivateMessages` and
  `receivePrivateMessagesFromLinkedPeers` — receive and checkpoint private
  stream data.
- `PaykitSdk.processOutboundPrivateMessages` and
  `processPendingPrivateMessages` — send queued private messages.
- `PaykitSdk.*EncryptedLinkRecoveryMarker*` methods — inspect, publish,
  observe, and remove recovery markers.

Private operation errors expose stable category/code fields and redacted
context. Raw diagnostic details require an explicit debug export method.

### Private Payment Lists and Payment Resolution

- `PaykitSdk.enqueuePrivatePaymentList` — queue current private receiving
  details for one counterparty receiver.
- `PaykitSdk.enqueuePrivatePaymentListWithReceivingDetails` — queue an
  explicit complete private list for one counterparty receiver.
- `PaykitSdk.clearPrivatePaymentList` — queue an empty private list for one
  counterparty receiver.
- `PaykitSdk.clearPrivatePaymentListAndProcessOutbound` — queue an empty
  private list and attempt delivery for that counterparty receiver.
- `PaykitSdk.syncContactPrivatePaymentLists` — queue current private lists
  for saved contacts and optionally clear linked peers that are no longer
  saved contacts.
- `PaykitSdk.syncContactPrivatePaymentListsAndProcessOutbound` — queue
  contact private lists and attempt outbound delivery in one app-facing call.
- `PaykitSdk.syncPrivatePaymentListsWithReservationsAndProcessOutbound` —
  queue reservation-backed private lists supplied by the app and attempt
  delivery, with per-counterparty-receiver queue and delivery failures.
- `PaykitSdk.currentPrivatePaymentList` — inspect the latest cached Private
  Payment List view for one counterparty receiver.
- `PaykitSdk.prepareAndResolvePrivateContactPayment` — app-facing private
  payment setup:
  refresh live session access, ensure or advance private link state,
  drain currently available private send/receive work for the peer, then
  resolve only the counterparty's Private Payment List.
- `PaykitSdk.resolvePrivateContactPayment` — resolve only payable private
  Payment Endpoints into adapter-built private payment targets.
- `PaykitSdk.resolvePublicContactPayment` — independently resolve only payable
  public Payment Endpoints into adapter-built public payment targets. It does
  not inspect or mutate Encrypted Link state.

The bindings do not expose a mixed public/private resolution call, a source
discriminator, or implicit fallback between the two payment modes.

Private endpoint payloads and payment targets use `PaymentPayload`, so raw
payment-method data is exported only through explicit payload methods.
The reservation callback returns `ReceivingDetailReservationResponse`:
`UseCurrentReceivingDetails` means the SDK should call regular current
receiving details, while `Reservations` means use exactly the supplied list,
including an empty list.

For direct reservation publication, pass one
`PrivatePaymentListReservationUpdateInput` per counterparty receiver. An empty
reservation list means "publish an empty Private Payment List for this
counterparty receiver".
Helpers that both queue and attempt delivery return
`PrivatePaymentListDeliveryReport` with `queued`, `cleared`,
`failedToQueue`, and `failedToDeliver` groups. A peer whose Encrypted Link is
`LINKING` can appear in `queued`; the message remains eligible for a later
outbound worker run after the link becomes `LINKED`.

### Public File Downloads

`fetchPubkyFileBounded(uri, maxBytes)` enforces a caller-selected byte limit while
reading successful response bodies, before returning bytes across FFI. The
effective limit is the smaller of `maxBytes` and 5 MiB. It returns
`nil` in Swift or `null` in Kotlin for missing files and throws for oversized
bodies and truncation the transport can detect, such as a Content-Length
shortfall or an incomplete chunked body. A close-delimited body has no declared
length, so an early connection close can return partial bytes successfully.
Zero permits only an empty body. Transport buffers and the current chunk are
additional memory. Image decode, pixel, cache and request-timeout limits remain
the app's responsibility. HTTP error bodies remain unbounded inside the current
Pubky client before Paykit regains control. Closing that gap through this path
requires a Pubky client API change. This is not complete response-size protection.
A hostile homeserver can bypass the limit by returning an HTTP error status.
A Pubky request timeout limits that request's duration, not its memory use.
`fetchPubkyFile` limits successful bodies to 5 MiB.

### Payment Requests

- `PaykitSdk.proposePaymentRequest`, `acceptPaymentRequest`,
  `rejectPaymentRequest`, `cancelPaymentRequest`, and `submitPaymentProof` —
  queue Payment Request lifecycle events through the SDK outbound stream.
- `PaykitSdk.paymentRequests`, `paymentRequestsWith`,
  `receivedPaymentRequestsFrom`, `listPaymentRequests`,
  `activeRecurringPaymentRequests`, and `actionableReceivedPaymentRequests` —
  inspect SDK-derived Payment Request records.
- `PaymentReference` — redacted Payment Reference object with explicit text
  export for payment execution or display.

Returned records reflect local stream and outbound queue state. Outbound
statuses still indicate whether a queued event has been sent.

### Allowances

- `PaykitSdk.proposeAllowance`, `acceptAllowance`, `rejectAllowance`, and
  `endAllowance` queue Allowance lifecycle events through the SDK outbound
  stream.
- `PaykitSdk.listAllowances` and `getAllowance` inspect SDK-derived Allowance
  records without reimplementing lifecycle derivation on the platform.
- `AllowanceTerms` and its nested range, period, and limit objects validate
  immutable Allowance authority before proposal and expose private fields only
  through explicit getters. Their Swift `description`/`debugDescription` is
  routed to the redacted Rust formatting; the Kotlin wrappers expose no fields
  through `toString()`.

Allowance Terms and every value returned by their getters are sensitive private
state. Do not include them in ordinary Swift/Kotlin logs, reflection output, or
diagnostics. Returned records describe consent-message state and history health;
they do not determine payment eligibility or authorize payment execution.
Existing Swift `PaykitSdkProtocol` mocks and Kotlin `PaykitSdkInterface`
implementations must add the six Allowance methods when adopting these bindings.

### Allowance payment accounting

The accounting APIs use the SDK's shared matching and exact decimal rules. They
never execute a wallet payment. Use one coordinated SDK runtime and executor for
the local identity; sessions, capabilities, key rotation, endpoint freshness,
trusted time, user consent, and wallet idempotency remain the caller's responsibility.

1. Read `allowanceAccountingState`. Before first admission, reconcile the complete
   wallet journal through `reconcileAllowanceAccounting`. Supply an empty history
   only when the wallet has established that no previous manual or automatic
   payment exists. Initialization is an explicit completeness attestation.
2. Call `evaluateAllowanceCandidates` with a scoped Payment Request and trusted
   UTC time. Apply wallet priority or obtain user choice among eligible candidates.
   `selectAllowance` persists one choice with its expected revision; candidate
   discovery itself never chooses or combines Allowances. Candidate checks cover
   static matching and active time; actual capacity is checked at admission.
3. `acceptPaymentRequestAutomatically` atomically persists selection and queues
   ordinary Acceptance after SDK and wallet checks. Recurring Acceptance reserves
   no amount or count. Each scheduled Billing Period needs its own later admission.
4. Call `reserveAutomaticPayment` with the occurrence, expected association
   revision, and fresh `PaymentExecutionChecks`. The checks include the actual
   validated `AccountingAmount`, selected Payment Endpoint, trusted UTC time, and
   wallet attestations for endpoint usability, local safeguards, and recurrence.
   `Ready` with status `Prepared` holds capacity but does not permit execution.
5. Immediately before wallet execution, call `beginPaymentExecution` with that
   attempt ID and fresh checks. Only a new `Ready` response with `Submitted`
   status permits handoff. Use its stable attempt ID as the external wallet's
   idempotency key. An admission or handoff `Blocked` result is a successful
   durable decision: its updated trusted-time watermark must remain saved.
6. Report wallet-verified settlement through `recordPaymentOutcome`. `Succeeded`
   commits usage; verified terminal `Failed` releases it. Timeouts, missing
   callbacks, or uncertain settlement are `Unknown` and keep capacity reserved.
   A crash between handoff and settlement requires external reconciliation.

`deferPaymentOccurrence` records a temporary private reason; reconsideration must
repeat all checks. `markPaymentManualOnly` is sticky and background matching never
clears it. A user-authorized manual payment must use `reserveManualPayment`, the
same handoff/outcome flow, and the same semantic occurrence key. It consumes no
Allowance capacity but cannot duplicate an unresolved or successful automatic
payment. Calls directly to a separate wallet executor would bypass this guarantee.

For an explicit user-approved replacement on a recurring request, call
`authorizeAllowanceReassociation` with the expected revision, a future Billing
Period boundary, and the stable UUID-v4 authorization reference. Future occurrences
use the new revision; previous attempts and their usage remain attributed to the
original Allowance. Reassociation never clears a manual-only decision or converts
an unresolved payment into a new execution opportunity.

`AllowanceAccountingHistory` contains typed associations, occurrences, attempts,
and per-Allowance watermarks. Treat every record and value accessed through
`AccountingAmount` getters as private wallet data. Rust debug formatting and the
amount object's native default formatting are redacted; platform record fields
remain explicit data and must not be logged or included in generated descriptions.

The unreleased state and backup blob formats remain version 1 and may evolve
directly during development. Previous development data is unsupported; no migration
is provided. Decode failure never falls back to empty state. The platform
`saveStateBlobAtomically` callback must durably save the whole blob and enforce
its expected revision before acknowledging success.
Preserve opaque blobs with caller-managed encryption in storage and backups.

After restore or private-state loss, accounting remains blocked until complete
wallet reconciliation. A stale but internally valid backup can omit later spend:
restoring its ledger and watermark does not establish freshness. Reconcile every
payment path and the executor's durable idempotency journal, then submit the
complete recovered history and verified outcomes with the expected ledger
revision. Reconciliation merges retained evidence; it never resets usage from
Allowance lifecycle events, Payment Proofs, timeouts, or missing files. Unresolved
attempts remain reserved, and prepared tokens from an earlier recovery epoch
cannot be used as fresh handoff permits.

The Swift and Kotlin `AllowanceBindingsCompile` fixtures exercise all accounting
methods and typed records. They are compile-only surface checks, not a payment
workflow to execute in sequence. SDK protocol mocks must implement these methods
when adopting this binding version.

Review context: [candidate selection](https://github.com/pubky/paykit-rs/pull/136#discussion_r4004039601),
[deferral](https://github.com/pubky/paykit-rs/pull/136#discussion_r4004039605),
[future reassociation](https://github.com/pubky/paykit-rs/pull/136#discussion_r4004039611), and
[shared SDK support](https://github.com/pubky/paykit-rs/pull/136#discussion_r4004039617).

### Receipts

- `generateReceiptId` — create a caller-stable Receipt ID for retry-safe
  issuance.
- `PaykitSdk.prepareReceiptIssuance`, `issueReceipt`, and
  `processReceiptIssuance` — persist receipt issuance state, store the
  Encrypted Receipt, and queue Receipt Access.
- `PaykitSdk.issuedReceipts`, `issuedReceiptsTo`,
  `receiptIssuanceRecords`, `receiptAccess`, `receiptAccessFrom`,
  `receiptAccessRecords`, `retrieveReceipt`, `receipts`, `receiptsFrom`, and
  `receiptRecords` — inspect issued, indexed, and decrypted receipts.

Receipt Decryption Keys and encrypted payloads stay inside SDK-managed state.
Payment References are exposed through the redacted `PaymentReference`
object.

### Pubky Session Bootstrap

- `PubkySessionBootstrap.republishIdentity(publicKey)` - rebroadcast the newest
  existing signed PKARR identity record found on the configured networks or in
  their caches, without signing or changing it. Returns `true` when published or
  `false` when no record was found; resolution/publication failures remain errors.
  No secret key or restored session is needed. Reuse the bootstrap helper for
  its cache; the app owns scheduling, throttling and retries, and the Pubky client
  owns timeouts.
- `PubkySessionBootstrap(clientId)` — create/import grant sessions and grant
  auth flows for a stable app-owned Pubky client ID.
- `PubkyClientConfig.authRelayUrl` — select a local or private grant-auth
  relay; leave unset for Pubky's production default.
- `PubkyAuthRequest` — pending external auth-flow handle; call `saveState()` to
  persist its complete proof-of-possession state securely when an unapproved
  request must survive process loss. Once `complete()` fetches an approval,
  cancellation or a later exchange failure requires a new auth request.
- `PubkyAuthRequestState` — secret-bearing URL plus client key used by
  `resumeAuth`; delete it after completion, expiry, or abandonment.
- `pubkySecretKeyFromBip39Seed(seed)` — derive a Pubky secret key from a
  64-byte BIP39 seed using the Pubky/Ring convention.
- `pubkySecretKeyFromBip39Mnemonic(mnemonicPhrase)` — derive the same key from
  a BIP39 English mnemonic phrase.
- `pubkyPublicKeyFromSecret(localSecretKey)` — derive a Pubky public key.
- `parsePubkyAuthUrl(authUrl)` — inspect a Pubky auth URL.
- `PubkySessionBootstrap.approveAuthWithCompanionClaim(...)` — sign, encrypt,
  and relay an application-defined companion claim before approving the Pubky
  grant.
- `PubkyAuthCompanionClaim` — integrator-owned query parameter, claim type, and
  unsigned payload; no channel, signature, nonce, or secretbox primitives cross
  FFI.
- `resolvePubkyUrl(uri)` and `parsePubkyResource(uri)` — Pubky URI helpers.

The companion approval method throws
`PubkyAuthCompanionClaimApprovalError`, whose cases distinguish invalid auth
URLs, invalid claims or local keys, encryption failure, relay delivery failure,
and grant authorization failure. Relay delivery completes before grant
approval begins, so a relay or encryption failure does not authorize the
requesting server. The integrating application owns its payload serialization
and semantic validation; Paykit owns the common cryptographic transport and
approval ordering.

Swift integration shape:

```swift
let claim = PubkyAuthCompanionClaim(
    queryParameter: "x-bitkit-claim",
    claimType: "watch-only-account-v1",
    unsignedPayload: bitkitUnsignedClaim
)
try await bootstrap.approveAuthWithCompanionClaim(
    authUrl: authUrl,
    expectedCapabilities: "/pub/paykit/v0/bitkit/server/:rw",
    localSecretKey: identityKey,
    claim: claim
)
```

Kotlin integration shape:

```kotlin
val claim = PubkyAuthCompanionClaim(
    queryParameter = "x-bitkit-claim",
    claimType = "watch-only-account-v1",
    unsignedPayload = bitkitUnsignedClaim,
)
bootstrap.approveAuthWithCompanionClaim(
    authUrl = authUrl,
    expectedCapabilities = "/pub/paykit/v0/bitkit/server/:rw",
    localSecretKey = identityKey,
    claim = claim,
)
```

### Profiles and Contacts

- `PaykitSdk.publishPaykitProfile` / `fetchPaykitProfile` — write and read
  public Paykit Profiles.
- `PaykitSdk.deletePaykitProfile` — remove this identity's Paykit Profile.
- `PaykitSdk.publishPaykitBlob`, `uploadProfileAvatar`,
  `deletePaykitBlob`, `fetchPubkyFile`, `fetchPubkyFileBounded`, and `fetchPubkyText` — publish profile
  blobs and read public Pubky resources.
- `PaykitSdk.saveContact`, `contactRecord`, `contactRecords`, and
  `removeContact` — manage local Contact Records. Each contact is one Pubky
  identity with one or more Paykit receiver paths.
- `PaykitSdk.fetchPubkyProfile`, `fetchPubkyFollows`, and
  `resolveContactProfile` — read Pubky app profile/follow data and resolve
  contact display metadata.
- `PaykitSdk.resolveProfile` and `currentProfile` — profile-resolution
  aliases for non-contact and current-identity screens.
- `PaykitSdk.publishPublicContact`, `removePublicContact`, and
  `syncPublicContactMarkers` — opt-in Public Contact Marker workflows.

`PaykitProfile.extraJson` is a JSON object string so apps can carry
app-specific public profile fields without exposing an FFI JSON value model.

### State and Secret Blobs

- `SdkStateBlob` — internal SDK runtime state for platform durable
  storage. Store it encrypted or inside platform-protected storage.
- `SdkBackupBlob` — SDK backup/export payload for app-controlled
  backup flows.
- `PubkyLocalSecretKey` — local Pubky secret key bytes.
- `ReceiverNoiseSecretKey` — receiver-scoped Noise secret key bytes. Generate
  it once per receiver, persist it with session access, and reuse it when
  signing in, completing auth, or importing that session. It remains required
  when an external signer owns the Pubky identity secret.

`PaykitSdk.exportBackupString` and `restoreBackupString` are text-form
wrappers for platforms that prefer a single encoded SDK backup string.
`PaykitSdk.backupStateRevision` lets apps compare backup contents before and
after SDK-mutating workflows to mark app backups dirty. `stateRevision`
remains the platform storage revision, including transient lease changes.
`encodeSdkStateBlobSnapshot` and `decodeSdkStateBlobSnapshot` are convenience
helpers for apps that store the opaque state blob and revision in one platform
record.

These are opaque binding objects. Use their explicit export methods only at
platform secure-storage or backup boundaries.

## Mobile Workflow Guide

The app usually keeps one long-lived `PaykitSdk` handle for the current local
Paykit identity. On startup:

```text
sdk = PaykitSdk.withPaymentAdapter(
    stateStore,
    sessionProvider,
    paymentAdapter,
    config
)
sdk.initialize()
status = sdk.identityStatus()
```

Use `identityStatus` to gate product actions. A persisted identity can remain
visible while live session access is unavailable. A missing `publicKey` means
explicit sign-out; a present `publicKey` with `liveSessionAvailable == false`
means the identity is remembered but Pubky-backed workflows must wait.

`SdkStateBlobStore` must persist every blob save atomically. If the app stores
the SDK backup inside a larger app backup record, compare `backupStateRevision`
before and after SDK-mutating workflows and mark the app backup dirty when it
changes, including when a workflow fails after persisting progress. If the
comparison fails, conservatively mark the app backup dirty. Do not use this
fingerprint as the state store's compare-and-swap revision.

```text
before = sdk.backupStateRevision()
report = sdk.syncPublicEndpointsWithReceivingDetails(details)
after = sdk.backupStateRevision()

if after != before:
    markAppBackupDirty()
```

### Publish Receive Details

When receiving details change, publish public endpoints and, for saved local
contacts, queue private lists:

```text
sdk.syncPublicEndpointsWithReceivingDetails(publicDetails)

updates = [
    PrivatePaymentListReservationUpdateInput(
        counterparty,
        counterpartyReceiverPath,
        reservations: [
            PaymentEndpointReservationInput(
                reservationId,
                identifier,
                payload,
                expiresAt,
                attribution
            )
        ]
    )
]

report = sdk.syncPrivatePaymentListsWithReservationsAndProcessOutbound(
    updates,
    clearUnlistedLinkedPeers
)
```

An empty `reservations` list publishes an empty Private Payment List for that
counterparty receiver. `failedToQueue` means the SDK did not persist an outbound
private message for that counterparty receiver. `failedToDeliver` means the SDK
queued the message, then delivery or reservation cleanup failed; keep the state
and retry with `processPendingPrivateMessages`.

### Pay A Contact

For private contact payment UX, use the high-level private preparation call:

```text
prepared = sdk.prepareAndResolvePrivateContactPayment(
    counterparty,
    counterpartyReceiverPath,
    amount, // PaymentAmountContext or nil/null
    maxAdvanceSteps
)
```

This refreshes session access, advances or starts private link work when
possible, receives pending private messages, processes pending outbound
messages, and resolves only private endpoints. Use
`prepared.resolution.status` for the private payment outcome and
`prepared.resolution.state` for private-link recovery or availability state.

Public payment is a separate call and result type:

```text
resolution = sdk.resolvePublicContactPayment(
    counterparty,
    counterpartyReceiverPath,
    amount // PaymentAmountContext or nil/null
)
```

No API combines these results or falls back from one mode to the other. The
application chooses which payment mode to present and invoke.

### Backup And Restore

The SDK backup is separate from the live state blob. Store both according to
the product's backup model:

```text
backupText = sdk.exportBackupString()
sdk.restoreBackupString(backupText)
```

Use `exportBackupString` after SDK state changes when the app wants the user to
recover Paykit private state after reinstall, sign-out, or device restore.
Without an SDK backup or live state blob, public Paykit data can be
rediscovered from Pubky, but private link checkpoints, private stream indexes,
receipt keys, queued outbound messages, and local Contact Records are not
derivable from the Pubky public key alone.

### Error And Report Handling

- `PrivateOperationError.category` and `code` are for app branching.
  `redactedContext` is safe for normal UI/logging. Use `exportDebugDetails`
  only for explicit diagnostics.
- `EndpointSyncReport.failed` means public endpoint publication/removal was not
  fully applied. Keep local receiving details and retry sync later.
- `PrivatePaymentListDeliveryReport.failedToQueue` is a local persistence or
  validation problem for that counterparty receiver; show or log it as a
  blocked update.
- `PrivatePaymentListDeliveryReport.failedToDeliver` is retryable workflow
  state unless the nested error says recovery is required. Keep the queued
  state and let the retry worker continue.
- Contact payment resolution may return public Payment Endpoints while
  `privateState` reports private recovery or unavailable private state.
  Treat `status` as the general result and `privateState` as the private
  transport state.

## Building

Always build all platforms together:

```bash
cd paykit-ffi
./build.sh all
```

Build only iOS artifacts for local debugging:

```bash
cd paykit-ffi
./build_ios.sh
```

The iOS script regenerates the SwiftPM interface files in `bindings/ios` and
writes release artifacts to:

```text
dist/ios/Paykit.xcframework
dist/ios/Paykit.xcframework.zip
```

It also computes the zip checksum and updates the root `Package.swift`. The
generated XCFramework directory and zip are ignored and must not be committed.

Build only Android artifacts for local debugging:

```bash
cd paykit-ffi
./build_android.sh
```

The Android script regenerates ignored UniFFI Kotlin bindings, JNI libraries,
native debug symbols, and a local Maven publication:

```text
bindings/android/lib/src/main/kotlin/com/synonym/paykit/paykit.android.kt
bindings/android/lib/src/main/kotlin/com/synonym/paykit/paykit.common.kt
bindings/android/lib/src/main/jniLibs/**/libpaykit.so
bindings/android/native-debug-symbols.zip
```

The Gradle wrapper/configuration, `AndroidManifest.xml`, ProGuard files, and
`kotlin-manual` helper sources are tracked because Android consumers need them
at source checkout time.

Release builds use the same script with `-r`:

```bash
./build.sh -r all
./build.sh -r --rc all
```

Run focused checks:

```bash
cargo test -p paykit-ffi --all-features
cargo doc -p paykit-ffi --no-deps
```

## Android Initialization

Android apps must initialize the platform certificate verifier before Pubky
networking:

```kotlin
import com.synonym.paykit.PaykitAndroid

check(PaykitAndroid.initialize(applicationContext))
```

## Project Structure

```text
paykit-ffi/
├── src/lib.rs              # UniFFI SDK exports
├── build.sh                # Unified all-platform build script
├── build_ios.sh            # Internal iOS sub-build script
├── build_android.sh        # Internal Android sub-build script
├── bindings/ios/           # SwiftPM source/interface files
├── bindings/android/       # Android Gradle source/config files
└── dist/ios/               # Ignored generated XCFramework release artifacts
```
