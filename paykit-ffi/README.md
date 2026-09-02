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
- `PaykitSdk.withPubkySharedState` and
  `PaykitSdk.withPaymentAdapterAndPubkySharedState` — use the first-party
  encrypted Pubky state instead of platform blob callbacks.
- `SdkStateBlobStore` — platform callback interface for opaque SDK state
  blob load/save.
- `SdkPubkySessionProvider` — platform callback interface for live Pubky
  session access and public storage availability.
- `SdkPaymentAdapter` — platform callback interface for receiving details,
  endpoint reservation cleanup, payable endpoint ordering, and payment target
  construction.
- `PaykitSdk.initialize`, `identityStatus`, `signOut`, and
  `forgetSessionAccess` — app-facing account/session lifecycle for the current
  Paykit runtime. `signOut` revokes the Pubky grant and leaves identity-wide
  Paykit state intact; `forgetSessionAccess` performs local-only cleanup.
- `PaykitSdk.stateRevision` — return the latest observed SDK state revision so
  apps can detect when SDK-managed state changed.
- `PubkySessionAccess` — opaque Pubky session access material. Use its
  explicit export methods only when persisting or loading platform-protected
  session state.
- `defaultConfig(appId)` — return the default `PaykitSdkConfig` policy for one
  Paykit App ID.
- `defaultPubkyClientConfig()` — return default `PubkyClientConfig`.
- `requiredSessionCapabilities()` — return the Pubky capabilities required by
  the SDK.

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
- `paykitAppRegistry` — fetch an identity's public Paykit App Registry.
- `publishPaykitApp` — publish this app's registry entry before endpoint sync.
- `rotatePaykitIdentityKey` — re-encrypt shared state with the next Paykit key
  generation and require fresh Encrypted Links while preserving durable history.
- `removePaykitApp` — remove this app's public Payment Endpoints and registry
  entry after its active Payment Requests and pending private financial work
  are complete.
- `setDefaultPaykitApp` and `setDefaultPaykitAppForEndpoint` — maintain
  identity-wide and per-endpoint app preferences.
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
  details for one counterparty.
- `PaykitSdk.enqueuePrivatePaymentListWithReceivingDetails` — queue an
  explicit complete private list for one counterparty.
- `PaykitSdk.clearPrivatePaymentList` — queue an empty private list for one
  counterparty.
- `PaykitSdk.clearPrivatePaymentListAndProcessOutbound` — queue an empty
  private list and attempt delivery for that counterparty.
- `PaykitSdk.syncContactPrivatePaymentLists` — queue current private lists
  for saved contacts and optionally clear linked peers that are no longer
  saved contacts.
- `PaykitSdk.syncContactPrivatePaymentListsAndProcessOutbound` — queue
  contact private lists and attempt outbound delivery in one app-facing call.
- `PaykitSdk.syncPrivatePaymentListsWithReservationsAndProcessOutbound` —
  queue reservation-backed private lists supplied by the app and attempt
  delivery, with per-counterparty queue and delivery failures.
- `PaykitSdk.currentPrivatePaymentLists` — inspect the latest cached Private
  Payment List view from each app for one counterparty.
- `PaykitSdk.prepareAndResolvePrivateContactPayment` — app-facing private
  payment setup:
  refresh live session access, ensure or advance private link state,
  drain currently available private send/receive work for the peer, then
  resolve only the counterparty's Private Payment List.
- `PaykitSdk.resolvePrivateContactPayment` — resolve only payable private
  Payment Endpoints into adapter-built private payment targets.
- `PaykitSdk.resolvePublicContactPayment` — independently resolve only payable
  public Payment Endpoints into adapter-built public payment targets. It does
  not inspect or mutate Encrypted Link state. The result includes per-app load
  failures when one registered app publishes invalid or unavailable data.

The bindings do not expose a mixed public/private resolution call, a source
discriminator, or implicit fallback between the two payment modes.

Private endpoint payloads and payment targets use `PaymentPayload`, so raw
payment-method data is exported only through explicit payload methods.
The reservation callback returns `PrivateReceivingDetailReservationResponse`:
`UseCurrentReceivingDetails` means the SDK should call regular current
receiving details, while `Reservations` means use exactly the supplied list,
including an empty list.

For direct reservation publication, pass one
`PrivatePaymentListReservationUpdateInput` per counterparty. An empty
reservation list means "publish an empty Private Payment List for this
counterparty".
Helpers that both queue and attempt delivery return
`PrivatePaymentListDeliveryReport` with `queued`, `cleared`,
`failedToQueue`, and `failedToDeliver` groups. A peer whose Encrypted Link is
`LINKING` can appear in `queued`; the message remains eligible for a later
outbound worker run after the link becomes `LINKED`.

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
- `PaykitSdk.resolvePrivatePaymentRequest`, `resolvePublicPaymentRequest`, and
  `prepareAndResolvePrivatePaymentRequest` — resolve using the request amount
  while enforcing its accepted endpoint identifiers and required payee App.

Returned records reflect local stream and outbound queue state. Outbound
statuses still indicate whether a queued event has been sent.
`actionableReceivedPaymentRequests` includes every request that still needs a
payer response. A required Paykit App constrains the payee endpoint, not the
payer app that responds.

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
  64-byte BIP39 seed using the Pubky Core/Ring convention.
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
    expectedCapabilities: "/pub/paykit/:rw",
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
    expectedCapabilities = "/pub/paykit/:rw",
    localSecretKey = identityKey,
    claim = claim,
)
```

### Profiles and Contacts

- `PaykitSdk.publishPaykitProfile` / `fetchPaykitProfile` — write and read
  public Paykit Profiles. Updates pass the revision returned by the preceding
  fetch or publication.
- `PaykitSdk.deletePaykitProfile(revision)` — remove the fetched profile only
  if its revision is still current.
- `PaykitSdk.publishPaykitBlob`, `uploadProfileAvatar`,
  `deletePaykitBlob`, `fetchPubkyFile`, and `fetchPubkyText` — publish profile
  blobs and read public Pubky resources with caller-provided size limits.
- `PaykitSdk.saveContact`, `contactRecord`, `contactRecords`, and
  `removeContact` — manage Contact Records. Each contact is one Pubky
  identity.
- `PaykitSdk.fetchPubkyProfile` and bounded `fetchPubkyFollows` — read Pubky
  app profile and follow data.
- `PaykitSdk.resolveProfile` and `currentProfile` — resolve profile display
  metadata for another identity or the current identity.
- `PaykitSdk.publishPublicContact`, `removePublicContact`, and
  `syncPublicContactMarkers` — opt-in Public Contact Marker workflows.

`PaykitProfile.extraJson` is a JSON object string for application-defined
identity profile fields without exposing an FFI JSON value model.

### State and Secret Blobs

- `SdkStateBlob` — internal identity-wide SDK runtime state. Store it in the
  shared durable backing used by every runtime for that Pubky identity.
- `SdkBackupBlob` — SDK backup/export payload for app-controlled
  backup flows.
- `PubkyLocalSecretKey` — local Pubky secret key bytes.
- `PaykitIdentitySecretKey` — rotatable identity-wide Paykit secret plus key
  generation.

Any generation can be derived from `PubkyLocalSecretKey` by passing its
generation number. Delegated apps can instead load `PaykitIdentitySecretKey`
without receiving the Pubky root secret. After rotation, every remaining app
must persist the replacement Paykit secret;
it cannot be re-derived from the unchanged Pubky key.

`PaykitSdk.exportBackupString` and `restoreBackupString` are text-form
wrappers for platforms that prefer a single encoded SDK backup string.
`PaykitSdk.stateRevision` returns the latest revision observed by the selected
storage mode. Callback storage can compare it before and after SDK-mutating
workflows to mark app backups dirty.
`encodeSdkStateBlobSnapshot` and `decodeSdkStateBlobSnapshot` are convenience
helpers for apps that store the opaque state blob and revision in one platform
record.

These are opaque binding objects. Use their explicit export methods only at
the shared-state storage or backup boundary.

## Mobile Workflow Guide

The app usually keeps one long-lived `PaykitSdk` handle for the current Paykit
identity. On startup:

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

Apps that share one identity-wide Pubky state can instead construct the handle
with `withPaymentAdapterAndPubkySharedState`. This mode does not use
`SdkStateBlobStore` callbacks. It requires active session access with current
Paykit identity key material for every operation. Independent runtimes use
homeserver ETag preconditions, so stale writes fail instead of replacing newer
state and the caller can retry the SDK operation.

Use `identityStatus` to gate product actions. `publicKey` identifies the last
initialized identity when known. `SignedOut` means Pubky-backed workflows must
wait even though the identity and its Paykit state remain available. With
callback storage, `PublicOnly` permits public workflows and
`PrivateLinkCapable` also permits Encrypted Link workflows. Pubky shared-state
storage requires `PrivateLinkCapable` access because decrypting state requires
the Paykit identity secret.

When callback storage is selected, `SdkStateBlobStore` must persist every blob
save atomically. Every runtime for the same Pubky identity must resolve to the
same logical blob. A protected device-local blob is suitable only while one
process owns the runtime. If the app also stores the SDK blob inside a larger
app backup record, compare `stateRevision` before and after SDK-mutating
workflows and mark the app backup dirty when it changes.

State-store callbacks run while the SDK holds its per-handle storage lock. They
must only load or save the blob and must not call back into that SDK handle.
Session-provider and payment-adapter callbacks are also synchronous boundaries:
none of these callbacks may call back into the same SDK handle while the
originating SDK operation is waiting for it.

Generated Android wrappers for callback-supplied SDK blobs, payment payloads,
and reservation attribution implement `AutoCloseable`. Callback implementations
should export the values they need and close those wrappers before returning;
Swift releases the equivalent objects through ARC.

Each successful changed blob write must return a new non-empty opaque revision
that is never reused for another state blob. Reusing any earlier revision makes
ABA stale-writer detection unsafe. Reusing the expected revision is rejected by
the Rust adapter directly.

When switching identities, select or create the state backing for the new
Pubky key, construct a new `PaykitSdk` handle with that store and session, then
call `initialize`. Do not reuse the previous identity's blob. Reopening the
previous store restores that identity without deleting its state.

```text
before = sdk.stateRevision()
report = sdk.syncPublicEndpointsWithReceivingDetails(details)
after = sdk.stateRevision()

if after != before:
    markAppBackupDirty()
```

### Publish Receive Details

When receiving details change, publish public endpoints and, for saved
contacts, queue private lists:

```text
sdk.publishPaykitApp(displayName, capabilities)
sdk.syncPublicEndpointsWithReceivingDetails(publicDetails)

updates = [
    PrivatePaymentListReservationUpdateInput(
        counterparty,
        reservations: [
            PrivatePaymentEndpointReservationInput(
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

Publishing the app is required before it can create app-attributed private
work. Call `paykitAppRemovalBlockers` before de-registration to inspect active
requests, undelivered events, incomplete Receipt issuance, and Private Payment
Lists that must first be cleared.

An empty `reservations` list publishes an empty Private Payment List for that
counterparty. `failedToQueue` means the SDK did not persist an outbound
private message for that counterparty. `failedToDeliver` means the SDK
queued the message, then delivery or reservation cleanup failed; keep the state
and retry with `processPendingPrivateMessages`.

### Pay A Contact

For private contact payment UX, use the high-level private preparation call:

```text
prepared = sdk.prepareAndResolvePrivateContactPayment(
    counterparty,
    amount, // PaymentAmountContext or nil/null
    nil/null, // previous private list version when one was consumed
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
    amount // PaymentAmountContext or nil/null
)
```

No API combines these results or falls back from one mode to the other. The
application chooses which payment mode to present and invoke.

### Backup And Restore

The SDK backup is separate from the live shared-state blob. Store the backup
according to the product's recovery model:

```text
backupText = sdk.exportBackupString()
sdk.restoreBackupString(backupText)
```

Restore requires an otherwise empty SDK state backing. This prevents an older
app backup from replacing newer state written by another app sharing the same
identity. After restore, participating apps publish their App Registry entries
again before creating new app-attributed work.

Use `exportBackupString` after SDK state changes when the app wants the user to
recover Paykit private state after reinstall, sign-out, or device restore.
Without an SDK backup or live state blob, public Paykit data can be
rediscovered from Pubky, but private link checkpoints, private stream indexes,
receipt keys, queued outbound messages, and Contact Records are not
derivable from the Pubky public key alone.

### Error And Report Handling

- `PrivateOperationError.category` and `code` are for app branching.
  `redactedContext` is safe for normal UI/logging. Use `exportDebugDetails`
  only for explicit diagnostics.
- `EndpointSyncReport.failed` means public endpoint publication/removal was not
  fully applied. Keep local receiving details and retry sync later.
- `PrivatePaymentListDeliveryReport.failedToQueue` is a local persistence or
  validation problem for that counterparty; show or log it as a
  blocked update.
- `PrivatePaymentListDeliveryReport.failedToDeliver` is retryable workflow
  state unless the nested error says recovery is required. Keep the queued
  state and let the retry worker continue.
- Private resolution reports private availability and recovery state. Public
  resolution has a separate result and does not carry private-link state.

## Building

Always build all platforms together:

```bash
cd paykit-ffi
./build.sh all
```

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
├── bindings/ios/           # Generated Swift + XCFramework
└── bindings/android/       # Generated Kotlin + Android library
```
