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
- `PaykitSdk.backupStateRevision` — fingerprint backup contents without
  transient operation leases, so empty polls do not trigger app backups.
- `PaykitSdk.observedBackupStateRevision` - return optional paired storage and
  backup revisions from a completed shared-state operation, without I/O. This
  is historical metadata, not current remote state or authorization. Callback
  storage returns no observation. Retain fresh `backupStateRevision` as the
  fallback, schedule conservatively after failures, and discard cached pairs
  on identity, key, session, or runtime reset.
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
- `paykitAuthorizerSessionCapabilities` - capability string for identity
  authorizers: `/pub/paykit/:rw,/pub/paykit-authority/v0/current-key.json:rw`.
  Ordinary apps keep `/pub/paykit/:rw`, without authority-path write access.
- `publishPaykitNoiseKeyAuthorization` - sign and publish the active Noise key
  before private app publication or delegation. Requires the local Pubky secret
  and separate authority-path write access.
- `paykitNoiseKeyAuthorization` - fetch and verify an identity's current signed
  routing key, X25519 static key (`noiseStaticPublicKey`), and generation without
  updating peer pins. Registry key fields are unsigned discovery metadata.
- `publishPaykitApp` — publish this app's registry entry before endpoint sync.
- `rotatePaykitIdentityKey` — re-encrypt shared state with the next Paykit key
  generation and require fresh Encrypted Links while preserving durable history.
  Requires authorizer access and publishes the replacement signed key; retry
  interruptions with the same keys. Shared-state backup recovery has the same
  authorizer requirement.
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
  observe, and remove recovery markers. Active links retain their markers;
  explicit removal requires a blocked peer.

The SDK checks signed local/peer routing and static keys, retains peer generation
pins in shared state and backups, and fails closed without an unsigned registry
fallback. See [Establish Encrypted Link](../specs/paykit-sdk.md#establish-encrypted-link)
for setup, restore, and send/receive checks.

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

### Public File Downloads

`fetchPubkyFile(uri, maxBytes)` and `fetchPubkyText(uri, maxBytes)` require a
positive byte limit. `fetchPubkyFileBounded(uri, maxBytes)` additionally caps
the limit at 5 MiB and permits zero for an empty body. Missing files return
`nil`/`null`; oversized successful bodies fail before full buffering. Pubky
currently buffers HTTP error bodies before returning them to Paykit. Image decoding,
pixel and cache limits, and request timeouts remain app responsibilities.

### Payment Requests

- `PaykitSdk.proposePaymentRequest`, `acceptPaymentRequest`,
  `rejectPaymentRequest`, `cancelPaymentRequest`, and `submitPaymentProof` —
  queue Payment Request lifecycle events through the SDK outbound stream.
- `PaykitSdk.claimAndAcceptPaymentRequest` — atomically claim execution and queue
  acceptance. Keep the earlier preparation claim where needed; success confirms
  durable acceptance, not delivery or permission to execute a payment.
- `PaykitSdk.paymentRequests`, `paymentRequestsWith`,
  `receivedPaymentRequestsFrom`, `listPaymentRequests`,
  `activeRecurringPaymentRequests`, and `actionableReceivedPaymentRequests` —
  inspect SDK-derived Payment Request records.
- `PaymentReference` — redacted Payment Reference object with explicit text
  export for payment execution or display.
- `PaykitSdk.resolvePrivatePaymentRequest`, `resolvePublicPaymentRequest`, and
  `prepareAndResolvePrivatePaymentRequest` — resolve using the request amount
  while enforcing its accepted endpoint identifiers and required payee App.
  Request preparation retains fresh private intake and final validation, but
  leaves an otherwise idle queue of unclaimed, unprepared Delivery Confirmations
  durably pending. Callers service them through later outbound processing;
  preparation does not schedule a worker or guarantee a delivery time.

`PaymentRequestTerms.paymentEndpoints` optionally specifies fixed destinations
for the request. Use request-aware private resolution; it returns no list
version and never substitutes newer private lists or public endpoints.
Keep per-request payment execution guards even when the list version is absent.

Returned records reflect local stream and outbound queue state. Outbound
statuses indicate publication, not that the peer accepted the request or
executed payment. The SDK tracks durable receipt separately and retries
unconfirmed events across relinks without changing their Event IDs.
Run receive and outbound processing
to exchange confirmations; apps do not create these messages themselves.
`actionableReceivedPaymentRequests` includes every request that still needs a
payer response. A required Paykit App constrains the payee endpoint, not the
payer app that responds.

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
never execute a wallet payment. All apps use the identity's shared ledger and
coordinate handoffs through it; sessions, capabilities, key rotation, endpoint
freshness, trusted time, user consent, and wallet idempotency remain the caller's
responsibility.

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
   ordinary Acceptance after SDK and wallet checks. As with manual Acceptance,
   call `claimPaymentRequestForExecution` first to assign the request to this app.
   Recurring Acceptance reserves
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
   Reservation and handoff require this app's execution claim and outgoing
   payment capability; unresolved attempts keep the claim held.
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

Manual converted payments retain the wallet-verified actual amount. Before
reservation and handoff, `localEnabled` must attest that the wallet checked
conversion policy, any required quote, endpoint precision, rounding, and payment
deadlines. The wallet must retain its selected quote separately. Handoff must
use the reserved asset and amount. Automatic Allowance payments still require
the exact requested asset and amount.

For an explicit user-approved replacement on a recurring request, call
`authorizeAllowanceReassociation` with the expected revision, a future Billing
Period boundary, and the stable UUID-v4 authorization reference. Future occurrences
use the new revision; previous attempts and their usage remain attributed to the
original Allowance. Reassociation never clears a manual-only decision or converts
an unresolved payment into a new execution opportunity.

`AllowanceAccountingHistory` contains typed associations, occurrences, attempts,
and per-Allowance watermarks. Treat every record and value accessed through
`AccountingAmount` getters as private wallet data. Rust debug formatting and
native record descriptions are redacted. Explicitly accessed fields remain
private data and must not be logged.

The current state and backup blob formats remain version 1; see
[supported persisted formats](../specs/paykit-sdk.md#backup-and-restore).
Decode failure never falls back to empty state. The platform
`saveStateBlobAtomically` callback must durably save the whole blob and enforce
its expected revision before acknowledging success.
Preserve opaque blobs with caller-managed encryption in storage and backups.

Restore rejects backups that would discard retained Allowance or Payment Request
lifecycle evidence, including Cancellation and Event ID conflicts. Rejection
leaves current state unchanged. Restore requires empty storage apart from
initialized identity metadata; it cannot replace an existing shared state.

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

See the [SDK accounting contract](../specs/paykit-sdk.md#durable-allowance-payment-accounting).

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
  `deletePaykitBlob`, `fetchPubkyFile`, `fetchPubkyFileBounded`, and `fetchPubkyText` — publish profile
  blobs and read public Pubky resources with caller-provided size limits.
- `PaykitSdk.saveContact`, `contactRecord`, `contactRecords`, and
  `removeContact` — manage Contact Records. Each contact is one Pubky
  identity.
- `PaykitSdk.saveContacts(updates)` — save a batch in one atomic storage
  transaction after validating every update and the initialized identity. Results
  follow input order; duplicate keys are applied in order, with the last update
  winning in storage. Existing profile and Public Contact Marker metadata is
  preserved. Marker publication and unblocking peers remain separate operations.
  An empty batch still requires an initialized identity and leaves state unchanged.
- `PaykitSdk.saveContactsAndUnblockPeers(updates)` — explicitly add or restore
  contacts and unblock their blocked peers in the same atomic transaction. Other
  peers and existing links are unchanged. A busy blocked peer rejects the batch;
  unblocked peers need a fresh Encrypted Link. Use `saveContact` for label edits.
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
must use the replacement Paykit secret. Delegated apps receive it from the
identity's key-management layer; a root-key holder can derive it for that
generation.

Persist the replacement key before calling `rotatePaykitIdentityKey`. If the
call fails or is interrupted, retry with the same current and replacement keys:
shared state may already use the replacement even if the registry update or
replacement authorization publication has not completed.

`PaykitSdk.exportBackupString` and `restoreBackupString` are text-form
wrappers for platforms that prefer a single encoded SDK backup string.
`PaykitSdk.backupStateRevision` lets apps compare backup contents before and
after SDK-mutating workflows to mark app backups dirty. `stateRevision`
remains the selected storage mode's revision, including transient lease changes.
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

Use a separate Pubky grant for each independently restored session. Restoring
the same grant again replaces its existing bearer. When reconnecting through
`PubkySessionBootstrap.importSession`, return its live `sessionAccess` from the
provider with the same client configuration instead of restoring it again.

Apps that share one identity-wide Pubky state can instead construct the handle
with `withPaymentAdapterAndPubkySharedState`. This mode does not use
`SdkStateBlobStore` callbacks. It requires active session access with current
Paykit identity key material for every operation. Independent runtimes use
renewable homeserver write locks across bounded groups of state transactions.
All serving homeserver instances must run 0.15 or newer; updating these bindings
does not upgrade the server. Confirm deployment separately from the
`webdav-locks` capability, which does not identify the server version.
Each changed transaction is durably saved before returning. After contention
or an uncertain result, inspect durable request/payment records and resume
existing work; a multi-step operation may already have committed intent.
Apps with shared keys and write access are mutually trusted; App IDs do not
provide cryptographic isolation from other authorized apps.
An unconfirmed state write leaves a homeserver marker. The next operation waits
five minutes under a renewed lock before reloading state; cancelling restarts
that wait on the next attempt. A competing runtime that exhausts lock acquisition
retries receives `SharedStateBusy` (`shared_state_busy`), whether the holder is
doing normal work or waiting for recovery. Back off and keep the operation
pending instead of immediately retrying. Reads can also be
blocked; there is no fixed completion deadline.
This is a best-effort mitigation, not a safety guarantee. Homeserver 0.15 checks
lock ownership before publication, but a lost database transaction can still
release its lock while backend publication remains in flight. Normal successful
writes have no cooldown. See the [shared-state safety requirements](../paykit-sdk/README.md)
before deploying multiple writers.

Use `identityStatus` to gate product actions. `publicKey` identifies the last
initialized identity when known. `SignedOut` means Pubky-backed workflows must
wait even though the identity and its Paykit state remain available. With
callback storage, `PublicOnly` permits public workflows and
`PrivateLinkCapable` also permits Encrypted Link workflows. Pubky shared-state
storage requires `PrivateLinkCapable` access because decrypting state requires
the Paykit identity secret.

`PrivateLinkCapable` reports key/session availability, not signed authorization;
private setup also requires `publishPaykitNoiseKeyAuthorization` as described above.

When callback storage is selected, `SdkStateBlobStore` must persist every blob
save atomically. Every runtime for the same Pubky identity must resolve to the
same logical blob. A protected device-local blob is suitable only while one
process owns the runtime. If the app also stores the SDK blob inside a larger
app backup record, compare `backupStateRevision` before and after SDK-mutating
workflows and mark the app backup dirty when it changes, including when a
workflow fails after persisting progress. If the comparison fails, conservatively
mark the backup dirty. Do not use this fingerprint as the store's CAS revision.

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
before = sdk.backupStateRevision()
report = sdk.syncPublicEndpointsWithReceivingDetails(details)
after = sdk.backupStateRevision()

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
counterparty. `failedToQueue` reports a queueing failure, not a rollback guarantee;
inspect durable state before replacing reservations or retrying the workflow.
`failedToDeliver` means the SDK
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

Normal restore requires an otherwise empty SDK state backing. This prevents an older
app backup from replacing newer state written by another app sharing the same
identity. After restore, participating apps publish their App Registry entries
again before creating new app-attributed work. Restored links retain peer pins
and require the [signed-key checks](../specs/paykit-sdk.md#establish-encrypted-link).

For missing or corrupt Pubky shared state, use
`recoverSharedStateFromBackup(backupBlob, replacementKey)` with a matching trusted
backup, active `paykitAuthorizerSessionCapabilities()` session, Pubky identity
secret, current Paykit key and its derived successor, and existing protected
authorization. Missing, malformed, or conflicting authorization fails before
replacement. Persist the replacement key first; recovery commits state, updates
the App Registry, then publishes replacement authorization. See
[Backup And Restore](../specs/paykit-sdk.md#backup-and-restore) for same-key retries,
key distribution, relinking/reconciliation, and supported persisted formats.

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

The iOS script regenerates SwiftPM interfaces in `bindings/ios` and writes the
XCFramework and zip to `dist/ios`. It also updates the checksum in
`Package.swift`; keep that change only when preparing a release.

Android Kotlin bindings, JNI libraries, debug symbols, and the local Maven
publication are generated artifacts, not tracked sources. The Gradle project,
manifest, ProGuard rules, and `kotlin-manual` helpers remain tracked.

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
