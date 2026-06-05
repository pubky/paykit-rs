# Paykit SDK Architecture

## Goal

Describe the Paykit SDK layer that sits above Paykit Library.

Paykit Library remains the stateless Rust implementation of Paykit Protocol
wire formats, Pubky storage helpers, Encrypted Link transport helpers, and
structural validation. Paykit SDK is the durable runtime that wallets, payment
processors, and apps can use when they need local state, recovery behavior,
contact/payment workflows, and ergonomic platform APIs.

The SDK should be product-neutral and payment-method-neutral. It should support
payment methods through adapters without baking any one method into the SDK.

## Design Principles

- Keep `paykit-lib` stateless. It validates and sends protocol objects, but it
  does not own local history, lifecycle state, scheduling, retries, or contact
  policy.
- Make the SDK stateful and crash-safe. It owns durable event logs, derived
  views, snapshots, retries, recovery state, and app-facing getters.
- Preserve the private stream. Receive all Private Application Messages in
  order and persist them before advancing the Encrypted Link checkpoint.
- Own the Pubky integration needed by Paykit. Since Pubky is Paykit's only
  transport/storage backend, apps should not need a separate Pubky integration
  just to use Paykit SDK.
- Keep product profile/contact UX separate. The SDK can own small Paykit-facing
  profile/contact records and shared paths, but it should not own app screens,
  social graph semantics, or product-specific profile schemas.
- Keep payment execution separate. Payment adapters provide receiving details,
  endpoint compatibility, payment-target construction, and method-specific
  endpoint state.
- Prefer typed records at the SDK boundary. Apps should not have to parse raw
  JSON or reason about Encrypted Link snapshots directly for normal workflows.
- Expose low-level escape hatches where useful, but make the safe durable path
  the default.

## Architecture

The system should have three main layers:

- Paykit Protocol / `paykit-lib`: stateless wire types, Pubky path helpers,
  Encrypted Link send/receive helpers, parsers, serializers, and structural
  validation.
- Paykit SDK runtime: durable state, private stream routing, lifecycle
  derivation, endpoint publication, contact payment resolution, retries,
  recovery, Pubky session/capability handling, Pubky-backed Paykit
  profile/contact metadata, and app-facing APIs.
- Payment adapter layer: receiving-detail generation, endpoint compatibility,
  payment-target construction, method/provider state, and activity records.

The SDK may still accept narrow platform hooks for secure session persistence,
auth UI/Ring session handoff, custom profile/contact storage, scheduling, and
logging. Those hooks should not require each app to reimplement Pubky Paykit
logic.

The SDK should depend on `paykit-lib`, not replace it. Platform apps should
prefer SDK bindings for normal product workflows and use `paykit-lib` bindings
only for low-level protocol operations.

## Crate Layout

The SDK crate uses this layout:

```text
paykit-sdk/
  Cargo.toml
  src/
    lib.rs
    config.rs
    error.rs
    runtime.rs
    adapters.rs
    storage.rs
    identity.rs
    endpoints.rs
    contacts.rs
    linked_peers.rs
    private_stream.rs
    private_lists.rs
    payment_requests.rs
    receipts.rs
    endpoint_reservations.rs
    backup.rs
```

Module responsibilities:

- `runtime`: owns `PaykitSdk`, coordinates adapters, storage, and workflow
  modules.
- `config`: product-neutral policy knobs such as fallback behavior, retry
  limits, stale cache rules, and future message retention limits.
- `adapters`: payment-method adapter plus narrow platform hook for live Pubky
  session access.
- `storage`: durable records, transaction interface, migrations, and in-memory
  test storage.
- `identity`: SDK-owned Pubky session capability state and identity
  refresh/import/export workflows.
- `endpoints`: public Payment Endpoint publication, cleanup, and remote public
  Payment List reads.
- `contacts`: contact payment resolution types. SDK-owned Paykit-facing
  contact/profile records, shared namespace handling, and optional custom
  path/schema hooks are future scope.
- `linked_peers`: Encrypted Link establishment, restore, recovery, and
  per-counterparty runtime state.
- `private_stream`: ordered Private Application Message intake, persistence,
  parsing, dedupe, and current derived view rebuilds.
- `private_lists`: local and remote Private Payment List publication, caching,
  latest-state derivation, and size policy.
- `payment_requests`: Payment Request event state derivation, outbound event
  queueing, and proof correlation.
- `receipts`: Receipt Access event indexing, Encrypted Receipt retrieval,
  decryption, and retrieval state.
- `endpoint_reservations`: optional receiving-detail reservation records for
  Private Payment List sharing.
- `backup`: versioned export/import of SDK-managed state.

Additional modules should be added only when they have concrete implementation:

- `scheduler`: optional recurring Payment Request scheduling integration.
- `telemetry`: structured logs and redaction helpers.

If platform bindings become large, add separate crates:

```text
paykit-sdk-ffi/
paykit-sdk-react-native/
```

Those should expose SDK workflows directly. They can eventually replace most
app usage of low-level `paykit-ffi`, while keeping `paykit-ffi` available for
protocol-level integrations.

## Core Runtime Object

The SDK should expose a single runtime object per local Pubky identity:

```rust
pub struct PaykitSdk<S, K, P, C> {
    storage: S,
    pubky: K,
    payment: P,
    config: PaykitSdkConfig,
    clock: C,
}
```

The runtime should be cheap to construct but stateful in behavior. It should not
hide durable state in memory only. Any operation that changes link progress,
outbound queues, derived state, or publication status must persist through the
storage adapter.

The SDK should also provide a boxed/dynamic adapter mode for FFI:

```rust
pub struct PaykitSdkHandle {
    // boxed adapters, storage, and runtime locks for platform bindings
}
```

## Integration Interfaces

The SDK should own Pubky-backed Paykit behavior. Integration interfaces are for
state, platform auth/session persistence, and payment-method-specific behavior
that the SDK cannot provide generically.

### StorageAdapter

`StorageAdapter` is required. It must support atomic updates or an equivalent
crash-recovery contract.

```rust
#[async_trait]
pub trait StorageAdapter {
    async fn transaction<T>(
        &self,
        f: impl FnOnce(&mut dyn StorageTransaction) -> Result<T> + Send,
    ) -> Result<T>;
}
```

The Rust SDK storage model supports records for:

- identity state
- linked peer state
- Encrypted Link snapshots and handshake snapshots
- endpoint publication records
- endpoint reservation records
- outbound Private Application Message records and retry state
- raw Private Application Message stream items
- Event Message dedupe indexes
- receipt records
- recovery/fail-closed markers

The SDK should ship an in-memory storage implementation for tests and examples.
Production apps should provide a durable implementation.

### PubkySessionProvider

Required for loading and storing local Pubky session material.

```rust
pub trait PubkySessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>>;
    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>>;
    async fn clear_session_access(&self) -> Result<()>;
}
```

`PubkySessionAccess` provides the live authenticated `PubkySession`, Pubky
client for counterparty homeserver access, and optional `PubkyLocalSecretKey`
needed for Encrypted Links. The SDK derives and persists public `IdentityState`
from that access during initialization.

`load_public_storage` lets contact resolution fetch public Payment Endpoints
without requiring authenticated session access. Implementations can reuse the
Pubky client from `PubkySessionAccess` when they have one, or provide a separate
public-storage client when only unauthenticated reads are available.

The SDK derives Paykit capability from the session access:

- `SignedOut`
- `PublicOnly`: public Pubky operations may work, but Encrypted Links cannot be
  established because the local secret key is unavailable.
- `PrivateLinkCapable`: public operations and Encrypted Links can work.

Ring-authenticated sessions often produce the `PublicOnly` case. The SDK should
surface this as a clear state instead of retrying private operations that cannot
succeed.

The SDK should provide high-level import/export/sign-out APIs itself. The
provider is the narrow platform hook for secure storage and auth-session handoff,
not a separate Pubky SDK that integrators must use.

### Paykit Profile And Contact Namespace

The SDK should provide default Pubky-backed Paykit-facing profile and contact
metadata so different Paykit apps can interoperate. This belongs in the SDK, not
in `paykit-lib` core protocol validation.

The default namespace must not pollute the public Payment Endpoint root. Public
Payment Endpoints currently live directly under `/pub/paykit/v0/`, so SDK
profile/contact records should use a reserved Paykit path such as:

- a reserved subdirectory under `/pub/paykit/v0/`, or
- an unversioned Paykit-level path under `/pub/paykit/`.

The versioned option keeps SDK metadata beside the current protocol version.
The unversioned option may be better if profile/contact records should remain
stable across Paykit protocol versions. Either way, the chosen path must be
reserved so it cannot collide with Payment Endpoint Identifier files.

The exact path policy should be configurable:

- default shared Paykit SDK profile/contact paths for apps that want
  cross-Paykit interoperability
- custom app paths for products that already have profile/contact storage
- adapter-only mode for apps that want Paykit SDK to consume profile/contact
  data without writing any Pubky profile/contact records

Custom profile/contact hooks are optional escape hatches. The default path for
most integrators should be: configure Paykit SDK, provide secure storage and a
payment adapter, and let the SDK handle Pubky-backed Paykit profile/contact
reads and writes.

Default profile records can be public because they are display metadata. Contact
records need more care because they can reveal a social/payment graph. The SDK
should support shared Paykit contacts, but should also allow apps to keep saved
contacts local, encrypted, or app-specific when privacy policy requires it.

Schemas should be small, versioned, and Paykit-facing:

- profile display name and image pointer
- normalized Pubky public key
- optional app/profile source metadata
- contact public key
- optional local display snapshot
- optional Paykit capability summary

The SDK should not standardize product profile pages, social graph semantics,
contact grouping, or UI behavior.

### PaymentAdapter

Required for endpoint publication, endpoint selection, and payment-target
building.

```rust
pub trait PaymentAdapter {
    async fn current_receiving_details(
        &self,
        scope: ReceivingDetailScope,
    ) -> Result<Vec<ReceivingDetail>>;

    async fn reserve_receiving_details(
        &self,
        request: &PaymentEndpointReservationRequest,
    ) -> Result<Option<Vec<PaymentEndpointReservation>>>;

    async fn release_receiving_detail_reservation(
        &self,
        release: &PaymentEndpointReservationRelease,
    ) -> Result<()>;

    async fn select_payment_endpoint(
        &self,
        request: &PaymentEndpointSelectionRequest,
    ) -> Result<PaymentEndpointSelection>;

    async fn build_payment_target(
        &self,
        endpoint: &PaymentEndpointCandidate,
    ) -> Result<PaymentTarget>;
}
```

Endpoint selection requests include all discovered candidates, each candidate's
source, and optional amount context. The adapter returns evaluations and may
select one candidate from that batch.

The adapter owns payment-method-specific endpoint details:

- receiving-detail generation
- network or method metadata
- balances, quote policy, fees, and route policy
- method-specific payload parsing beyond basic Paykit compatibility

Payment execution and settlement detection stay with the integrating
application or payment provider. Future SDK APIs may accept execution results
from those systems.

### Payment Endpoint Reservations

Some payment methods need contact-scoped receiving details. The current SDK lets
payment adapters reserve receiving details for Private Payment List sharing. The
SDK queues the Private Payment List and stores linked reservation records in one
storage transaction. Reservation records keep lifecycle metadata and a payload
hash; they do not store the raw reserved endpoint payload.

When an adapter returns reservations, those reservations are the complete
Private Payment List to share for that counterparty. Adapters that need mixed
reserved and ordinary entries should include both as returned reservations.

The payment adapter creates reservations before the SDK can persist linked
records. Adapters should make reserved details idempotent, expiring, or safe to
abandon if the process stops before durable queueing. The SDK releases returned
reservations on SDK-side validation or queueing failure.

Reservation IDs are counterparty-scoped idempotency keys for SDK reservation
records, not for Private Payment List delivery. Requeueing the same reservation
details may queue another latest-state Private Payment List and update the
record to the latest outbound message id. Idempotent repeats preserve the
original reservation attribution, expiry, and creation time; adapters that want
new metadata should use a new reservation id.

Single-use Payment Request reservations are not part of the current SDK shape.
They need more request-specific context before they should be exposed.

### Future Scheduling

Recurring Payment Request scheduling is future SDK scope. The SDK should derive
eligibility and durable state, while the integrating app/runtime may still own
the actual timer service.

### Logger And Clock

The SDK should accept:

- `Clock`: deterministic current-time source for expiry, retries, and tests.
- `Logger`: structured logging with redaction for secrets, Receipt Decryption
  Keys, raw private payloads, and session material.

## Storage Model

The SDK storage model can be implemented by each platform, but the logical
records should be stable.

### IdentityState

Tracks local Pubky identity state:

- local public key
- capability state
- whether local secret key is available
- last successful initialization time
- sign-out generation

### LinkedPeerRecord

One record per counterparty:

- counterparty public key
- relationship state: unknown, known, linking, linked, recovery required, blocked
- in-progress handshake role: initiator or responder
- last sync time
- last private receive time
- current recovery marker state
- failure counters
- policy overrides

### EncryptedLinkState

One record per linked peer:

- active link snapshot
- handshake snapshot
- snapshot recipient public key
- read/write progress metadata
- last persisted checkpoint time
- snapshot generation

Snapshots are opaque `paykit-lib` snapshots. The SDK validates the expected
counterparty before restoring them.

### PrivateStreamItem

Append-only raw private stream item:

- local identity
- counterparty
- stream sequence number assigned by the SDK
- receive batch id
- raw UTF-8 payload
- parsed `version`
- parsed `kind`
- known Paykit kind, when recognized
- parse status: valid, malformed recognized message, unknown kind, invalid JSON
- parse error, when available
- received time

This is the source of truth for private protocol-derived state.

### EventDedupRecord

Tracks Event Message idempotency:

- counterparty
- event id
- event kind
- payload hash of the exact stored payload
- first stream item id
- duplicate stream item ids
- conflict status

Conflicting reused Event IDs must fail closed for the affected derived state.

### PrivatePaymentListView

Latest-state view per counterparty:

- latest valid stream item id
- current Payment Endpoint map
- stale/recovery status
- last refresh time

Malformed newer Private Payment List messages do not replace the last valid
view. They remain in the raw log.

### EndpointPublicationRecord

Tracks SDK-managed public Payment Endpoint publication:

- Payment Endpoint Identifier
- last payload the SDK tried to publish
- publication status: desired, published, pending removal, removed, failed
- last status update time
- last error, when available

Future versions may add payload hashes, separate attempt/confirmation times,
and retry counters if the SDK needs change detection or richer retry policy.

### EndpointReservationRecord

Tracks optional contact-scoped receiving details:

- reservation id
- counterparty public key
- Payment Endpoint Identifier
- payload hash
- latest outbound message id used to queue the reservation for sharing
- attribution metadata
- reservation expiry, when provided by the payment adapter

### PaymentRequestRecord

Derived record per Payment Request:

- payment request id
- proposer/payee counterparty
- current local role: payer or payee
- immutable terms
- proposal event id
- proposal expiry state
- lifecycle state
- accepted/rejected/canceled event ids
- latest Payment Proof records
- recurrence schedule metadata
- automatic execution status
- recovery/fail-closed status

SDK lifecycle states should be explicit and local:

- `proposed`
- `proposal_expired`
- `accepted`
- `rejected`
- `canceled`
- `proof_submitted`
- `active_recurring`
- `paused`
- `recovery_required`
- `invalid_conflict`

The SDK may expose product-friendly summaries, but it should not claim generic
settlement finality unless the payment adapter confirms it.

### ReceiptAccessRecord And ReceiptRecord

Receipt Access records:

- event id
- receipt id
- sender/issuer counterparty
- Receipt Location path
- Receipt Decryption Key
- optional Payment Request ID
- optional Billing Period
- retrieval state, timestamps, and last retrieval error

Receipt records:

- receipt id
- payment reference
- optional Payment Request ID
- optional Billing Period
- issuer context
- recipient public key
- optional Payment Endpoint Identifier
- optional Payment Amount
- caller-defined Receipt Metadata
- retrieval/decryption time
- Receipt Access Event ID and key hash used for retrieval

Receipt Decryption Keys must be redacted in logs and debug output.

### OutboundPrivateMessageRecord

Durable outbound Private Application Message queue:

- outbound id
- counterparty
- Private Message Kind
- exact raw JSON payload, including Event ID when the message kind has one
- send status
- attempt count
- created, updated, attempted, and sent timestamps
- last error

The SDK should use one generic outbound Private Application Message record type
for all Private Application Message kinds. Event Messages are processed as FIFO
per counterparty/Encrypted Link. Private Payment Lists use latest-state
semantics, so older unsent lists may be superseded by a newer complete list.
Send workers must claim the next sendable message through storage before
sending it, and in-progress claims must expire so a crashed worker can be
retried without letting two workers advance the same link at once.

Event Message retries must reuse the same Event ID and exact payload.

## Storage And Checkpoint Invariant

The most important SDK invariant is:

```text
Persist raw received messages and derived indexes before durably saving the
advanced Encrypted Link snapshot.
```

For each receive cycle:

1. Claim the per-counterparty peer link operation lease.
2. Restore or establish the Encrypted Link.
3. Receive the full batch from `paykit-lib`.
4. Persist every received Private Application Message plaintext and parse enough
   to identify version/kind when the payload is valid JSON.
5. In one transaction, insert raw stream items, update Event Message dedupe
   records, and then save the advanced link snapshot.
6. Release the lease.

If the app crashes after messages are stored but before the snapshot is stored,
replay is acceptable and must be deduped. If the snapshot is stored without the
messages, events may be lost; the SDK must not allow that.

## Runtime Locks

The SDK needs local runtime locks to prevent concurrent operations from racing
the same durable state.

Recommended locks:

- identity lock: serializes import/export/sign-out and capability refresh.
- public endpoint lock: serializes publication and cleanup of local public
  Payment Endpoints.
- storage-backed peer link operation lease: serializes Encrypted Link restore,
  handshake, send, receive, and snapshot updates per counterparty.
- outbound queue claim/lock: serializes retry workers per counterparty.
- reservation transaction: stores reservation records and the outbound message
  that shares them atomically. Existing reservation IDs with the same
  counterparty, Payment Endpoint Identifier, and payload hash are idempotent;
  conflicting existing details and duplicate IDs in the same batch are rejected.
  The idempotency applies to reservation records, while Private Payment List
  delivery remains latest-state and may queue another outbound message.

These are local SDK/runtime locks, not protocol messages. They can be in-memory
with durable recovery markers where needed. If a platform can run multiple
processes against the same storage, lock ownership must be storage-backed.
Lease expiry makes a stale operation reclaimable by another worker; durable
writes still check the currently stored lease id so an old holder cannot commit
after a newer lease has replaced it.

## Workflows

### Initialize SDK

1. Load SDK config.
2. Load identity state from storage.
3. Load Pubky session access through `PubkySessionProvider`.
4. Derive signed-out, public-only, or private-link-capable state.
5. Load peer records and recovery markers.
6. Start optional retry workers only after storage is ready.
7. Return an initialization report with capability and recovery state.

### Import Or Restore Pubky Session

1. Acquire identity lock.
2. Import or restore the Pubky session through SDK-owned Pubky logic.
3. Persist resulting session access through SDK storage/session hooks.
4. Detect capability: public-only or private-link-capable.
5. Persist identity state and capability.
6. If identity changed, mark old peer/link state inactive instead of silently
   reusing it.
7. Return current identity status.

### Publish Public Payment Endpoints

1. Ask `PaymentAdapter` for current public receiving details.
2. Convert receiving details into Payment Endpoint payloads.
3. Validate identifiers and payloads through `paykit-lib`.
4. Persist desired endpoint state.
5. Publish desired endpoints.
6. Remove stale managed endpoints.
7. Persist confirmed/pending/failed state.
8. Return an `EndpointSyncReport`.

The SDK should not remove endpoints it did not create unless explicitly
configured to manage the whole Paykit public namespace.

### Establish Encrypted Link

1. Ensure the identity is private-link-capable.
2. Start an initiator or responder Encrypted Link Handshake through
   `paykit-lib`.
3. Persist the handshake snapshot, role, and `linking` peer state.
4. Advance the stored handshake on retry/poll cycles.
5. When the handshake is pending, replace the stored handshake snapshot.
6. When the handshake completes, persist the active link snapshot, clear the
   handshake snapshot/role, and mark the peer `linked`.
7. If restore or handshake advancement fails, mark the peer recovery-required
   and stop private automation for that peer.

### Publish Private Payment List

1. Ensure the identity is private-link-capable.
2. Ensure the counterparty has an active Encrypted Link snapshot.
3. Ask `PaymentAdapter` for Private Payment List reservations.
4. If reservations are returned, build the complete Private Payment List and
   persist the outbound record plus linked reservation records atomically.
5. If reservations are not returned, ask `PaymentAdapter` for private receiving
   details scoped to the counterparty and queue the list normally.
6. Release adapter reservations when SDK-side validation or queueing fails
   before durable queueing.
7. Let the outbound Private Application Message worker send through Encrypted
   Link.
8. Persist send result and updated link snapshot.

Private Payment Lists publish endpoints only. Payment References come from
Payment Requests, Payment Proofs, and Receipts.

### Receive Private Stream

1. Claim the peer link operation lease.
2. Restore or establish Encrypted Link.
3. Receive ordered Private Application Messages through `paykit-lib`.
4. Persist raw messages and parse results.
5. Update Event Message dedupe records.
6. Persist the updated Encrypted Link snapshot in the same transaction.
7. Index Receipt Access events. Private Payment List views are derived on read
   from stored stream items.
8. Return a receive report.

Future receive routing should extend the same raw stream log to Payment
Requests, Payment Proofs, and any other Event Message kinds.

### Resolve Contact Payment

Input:

- counterparty public key
- desired amount/asset, if known
- supported endpoint policy
- fallback policy

Flow:

1. Load contact/profile display metadata if configured.
2. Check Linked Peer and cached private Payment List.
3. If private state is stale and recovery is possible, run bounded recovery.
4. If private endpoints are available, pass them to `PaymentAdapter` as one
   batch with amount context when known.
5. If no private endpoint is payable and public fallback is allowed, fetch
   public Payment List and pass those candidates as a second batch.
6. Return a structured result:
   - `Payable`
   - `NoEndpoint`
   - `UnsupportedEndpoint`
   - `PrivateRecoveryPending`
   - `PublicOnlySession`

The SDK should not block indefinitely waiting for private recovery.

### Send Payment Request

Payee flow:

1. Build immutable Payment Request terms.
2. Validate terms structurally.
3. Generate Event ID and Payment Request ID.
4. Serialize exact event payload through `paykit-lib`.
5. Persist outbound event and local derived request state.
6. Send over Encrypted Link.
7. Persist send status.

Payer receive flow:

1. Receive and persist raw event.
2. Validate structural shape.
3. Check sender role.
4. Check duplicate/conflicting Event ID.
5. Check duplicate/conflicting Payment Request ID terms.
6. Check proposal expiry.
7. Derive local state and expose it to the app.

### Accept, Reject, Or Cancel Payment Request

For outbound lifecycle events:

1. Load local Payment Request state.
2. Check whether the local role may send this event.
3. Check current lifecycle state.
4. Serialize and persist exact outbound payload.
5. Send and retry using the same Event ID and payload.

Cancellation is unilateral. Acceptance and rejection are payer-only in v0.2.

### Record Payment Proof

1. Load accepted Payment Request.
2. Ask `PaymentAdapter` for execution result or caller-supplied proof data.
3. Validate stateless proof/request correlation through `paykit-lib`.
4. Persist proof event before sending.
5. Send Payment Proof.
6. Update local state to `proof_submitted`.

The SDK should not mark a payment as settled unless the payment adapter provides
settlement confirmation.

### Retrieve Receipts

1. Receive Receipt Access event.
2. Persist event and dedupe by Event ID.
3. Pair Receipt Location path with sender/issuer context.
4. Fetch Encrypted Receipt when requested or configured.
5. Decrypt with Receipt Decryption Key.
6. Verify Receipt ID/location correlation through `paykit-lib`.
7. Verify the decrypted receipt recipient matches the local Pubky identity.
8. Try newer Receipt Access records first, but fall back to older valid records
   for the same Receipt ID.
9. Index by Receipt ID, Payment Reference, Payment Request ID, Billing Period,
   counterparty, and issuer.

### Backup And Restore

Backup should include SDK-managed state:

- public identity/capability state
- peer records
- Encrypted Link snapshots
- handshake snapshots
- Private Payment List cache
- endpoint publication records
- endpoint reservations
- raw private stream log or checkpointed subset
- outbound queue
- recovery markers

Backup should not include:

- app cloud transport details
- product-specific profile/contact schema beyond adapter-owned records
- payment-provider secrets unless explicitly provided by the payment adapter
- seed material unless Paykit standardizes that policy

Restore flow:

1. Validate local identity compatibility before replacing storage state.
2. Validate backup record shape and every link snapshot recipient.
3. Load backup into storage under a restore transaction.
4. Mark peers requiring resync before committing restored private state.
5. Do not execute automatic payments until private stream and request state are
   consistent.

## Public SDK API Shape

The current Rust SDK exposes initialization, identity status, public endpoint
sync, linked peer handshakes, private stream receive, Private Payment List
derivation/publication, contact payment resolution, outbound Private
Application Message processing, Receipt Access indexing/retrieval, Payment
Request lifecycle derivation plus checked outbound lifecycle queueing, optional
Payment Endpoint Reservation records, and backup/export/restore for
SDK-managed state. Use `paykit-sdk` rustdoc for the exact current signatures.

Future SDK versions should extend the current surface with operations like:

```rust
impl PaykitSdk {
    async fn import_session(&mut self, session_secret: String) -> Result<IdentityStatus>;
    async fn sign_out(&mut self) -> Result<()>;

    async fn clear_public_endpoints(&mut self) -> Result<EndpointSyncReport>;

    async fn sync_contact(&mut self, counterparty: PubkyPublicKey) -> Result<ContactSyncReport>;
    async fn sync_saved_contacts(&mut self, contacts: Vec<PubkyPublicKey>) -> Result<SyncReport>;

    async fn list_payment_requests(
        &self,
        filter: PaymentRequestFilter,
    ) -> Result<Vec<PaymentRequestRecord>>;

    async fn list_receipt_access(
        &self,
        filter: ReceiptAccessFilter,
    ) -> Result<Vec<ReceiptAccessRecord>>;

    async fn retrieve_receipt(&self, receipt_id: ReceiptId) -> Result<ReceiptRecord>;
}
```

## Platform Binding Shape

The SDK should have first-class bindings for mobile and app integrations.

Recommended approach:

- Rust SDK crate owns the runtime and core state machine.
- FFI crate exposes an opaque SDK handle.
- Platform wrappers provide ergonomic Swift/Kotlin/React Native APIs.
- Apps provide adapter implementations through platform callbacks or through
  small platform-side adapter objects.

Platform bindings should expose:

- initialization and identity status
- endpoint sync
- contact sync and payment resolution
- private stream receive/sync
- Payment Request lifecycle APIs
- Receipt retrieval APIs
- backup/export/restore APIs
- structured reports and errors

Bindings should not expose:

- raw Receipt Decryption Keys in debug output
- unredacted raw private messages in logs
- implicit event-loss-prone typed getters that bypass durable storage

## Error Model

SDK errors should be structured:

- `Storage`: durable storage failure
- `Identity`: Pubky session/key/capability failure
- `Transport`: Pubky or Encrypted Link transport failure
- `Protocol`: invalid Paykit message, conflict, or unsupported version
- `Policy`: operation blocked by configured fallback or privacy policy
- `PaymentAdapter`: payment adapter failure
- `RecoveryRequired`: local state is inconsistent and automatic execution is
  paused

Errors should include machine-readable codes and short redacted context. UI copy
belongs to the app.

## Policy Configuration

Current `PaykitSdkConfig` includes:

- public endpoint management scope
- private sharing enabled/disabled
- public fallback policy
- private recovery wait duration
- peer link operation lease timeout
- outbound private send lease timeout

Future policy configuration may add:

- stale private cache policy
- outbound retry policy beyond lease expiry
- unknown message retention policy
- receipt auto-retrieval policy
- recurring Payment Request scheduling policy
- endpoint reservation policy
- log redaction level

## What Moves From Existing App/Core Integrations

These are good candidates to move into Paykit SDK:

- Pubky session capability tracking for Paykit workflows
- public-only versus private-link-capable state
- public Payment Endpoint sync and stale endpoint cleanup
- Private Payment List publish/fetch/cache
- Encrypted Link snapshot and handshake runtime
- stale link recovery markers and bounded recovery policy
- ordered private stream receive/persist/route
- contact payment resolution and payable endpoint checks
- endpoint reservation records and attribution helpers
- SDK-managed backup records
- Receipt Access indexing and retrieval helpers
- Payment Request lifecycle state

These should stay outside Paykit SDK:

- payment-provider node/runtime state
- receiving-detail generation internals
- payment execution and settlement detection
- balances, fees, quotes, and route policy
- product profile/contact UI
- localized copy and navigation
- app backup transport and cloud sync
- seed/secret derivation policy unless standardized

## Test Plan

Core tests:

- storage transaction commits and rollbacks
- receive persists raw messages before snapshot
- replay after crash dedupes events
- conflicting Event IDs fail closed
- malformed recognized messages remain auditable
- unknown valid Private Application Messages are retained
- Private Payment List latest valid message wins
- stale private link falls back according to policy
- public-only identity blocks private link operations clearly
- outbound retries reuse exact Event ID and payload
- Payment Request role/lifecycle checks
- Receipt Access dedupe and receipt retrieval
- backup/restore validates snapshot recipient and pauses unsafe automation

Platform tests:

- SDK handle lifecycle
- adapter callback errors
- redaction in debug/log output
- serialization parity for records and errors
- no nullable/optional protocol ambiguity in wrappers

## Later Design Areas

- Durable storage adapters, including whether Paykit should ship a
  SQLite-backed implementation.
- Default Paykit SDK profile/contact namespace under `/pub/paykit/v0/` or an
  unversioned Paykit-level path under `/pub/paykit/`.
- Public fallback and private recovery timeout policies for saved contacts
  versus one-off counterparties.
- Reservation lifecycle hooks beyond Private Payment List queueing.
- Recurring Payment Request scheduling ownership between the SDK and
  app/runtime schedulers.
- Unknown Private Application Message retention defaults for mobile storage
  budgets.
- SDK platform bindings as the primary mobile API, with `paykit-ffi` kept for
  low-level protocol integrations.
