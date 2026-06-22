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
- `PaykitSdk.initialize`, `identityStatus`, and `signOut` — app-facing
  account/session lifecycle for the current Paykit runtime.
- `PaykitSdk.stateRevision` — return the platform SDK state revision so
  apps can detect when SDK-managed state changed.
- `PubkySessionAccess` — opaque Pubky session access material. Use its
  explicit export methods only when persisting or loading platform-protected
  session state.
- `defaultConfig()` — return default `PaykitSdkConfig`.
- `defaultPubkyClientConfig()` — return default `PubkyClientConfig`.
- `requiredSessionCapabilities(config)` — return Pubky capabilities required by
  a config.
- `coreSessionCapabilities()` — return the core Paykit Pubky capability scope.

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
- `PaykitSdk.currentPrivatePaymentList` — inspect the latest cached Private
  Payment List view for one counterparty.
- `PaykitSdk.prepareAndResolveContactPayment` — app-facing payment setup:
  refresh live session capability, ensure or advance private link state,
  receive private messages, process pending outbound messages, then resolve
  private-first with optional public fallback.
- `PaykitSdk.resolveContactPayment` — resolve payable private and optional
  public Payment Endpoints into adapter-built payment targets.
- `PaykitSdk.resolvePrivateContactPayment` and
  `resolvePublicContactPayment` — source-specific resolution helpers for apps
  that want to avoid mixed private/public results.

Private endpoint payloads and payment targets use `PaymentPayload`, so raw
payment-method data is exported only through explicit payload methods.
The reservation callback returns `ReceivingDetailReservationResponse`:
`UseCurrentReceivingDetails` means the SDK should call regular current
receiving details, while `Reservations` means use exactly the supplied list,
including an empty list.

For direct reservation publication, pass one
`PrivatePaymentListReservationUpdate` per counterparty. An empty reservation
list means "publish an empty Private Payment List for this counterparty".
Helpers that both queue and attempt delivery return
`PrivatePaymentListDeliveryReport` with `queued`, `cleared`,
`failedToQueue`, and `failedToDeliver` groups.

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

- `PubkySessionBootstrap` — create/import Pubky sessions and auth flows.
- `PubkyAuthRequest` — pending external auth-flow handle.
- `derivePubkySecretKey(seed, runtimeLabel)` — derive an app/runtime-specific
  Pubky secret key from a 64-byte wallet seed.
- `pubkyPublicKeyFromSecret(localSecretKey)` — derive a Pubky public key.
- `parsePubkyAuthUrl(authUrl)` — inspect a Pubky auth URL.
- `resolvePubkyUrl(uri)` and `parsePubkyResource(uri)` — Pubky URI helpers.

### Profiles and Contacts

- `PaykitSdk.publishPaykitProfile` / `fetchPaykitProfile` — write and read
  public Paykit Profiles.
- `PaykitSdk.deletePaykitProfile` — remove this identity's Paykit Profile.
- `PaykitSdk.publishPaykitBlob`, `uploadProfileAvatar`,
  `deletePaykitBlob`, `fetchPubkyFile`, and `fetchPubkyText` — publish profile
  blobs and read public Pubky resources.
- `PaykitSdk.saveContact`, `contactRecord`, `contactRecords`, and
  `removeContact` — manage local Contact Records.
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

`PaykitSdk.exportBackupString` and `restoreBackupString` are text-form
wrappers for platforms that prefer a single encoded SDK backup string.
`PaykitSdk.stateRevision` lets apps compare the platform state revision
before and after SDK-mutating workflows to mark app backups dirty.
`encodeSdkStateBlobSnapshot` and `decodeSdkStateBlobSnapshot` are convenience
helpers for apps that store the opaque state blob and revision in one platform
record.

These are opaque binding objects. Use their explicit export methods only at
platform secure-storage or backup boundaries.

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

React Native initializes this from the native module and returns a platform
error if initialization fails.

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
