# Paykit SDK Plan

Status: planning / implementation design
Date: 2026-06-03

## Goal

Define the future Paykit SDK layer that sits above Paykit Library.

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
  endpoint compatibility, payment execution, settlement detection, and
  method-specific state.
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
  payment execution, settlement detection, method/provider state, and activity
  records.

The SDK may still accept narrow platform hooks for secure session persistence,
auth UI/Ring session handoff, custom profile/contact storage, scheduling, and
logging. Those hooks should not require each app to reimplement Pubky Paykit
logic.

The SDK should depend on `paykit-lib`, not replace it. Platform apps should
prefer SDK bindings for normal product workflows and use `paykit-lib` bindings
only for low-level protocol operations.

## Crate Layout

The first implementation should add a new Rust crate:

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
    reservations.rs
    backup.rs
    scheduler.rs
    telemetry.rs
```

Suggested module responsibilities:

- `runtime`: owns `PaykitSdk`, coordinates adapters, storage, and workflow
  modules.
- `config`: product-neutral policy knobs such as fallback behavior, retry
  limits, stale cache rules, and message retention limits.
- `adapters`: payment-method adapters plus narrow platform hooks for session
  persistence, scheduling, reservations, and logging.
- `storage`: durable records, transaction interface, migrations, and in-memory
  test storage.
- `identity`: SDK-owned Pubky session capability state and identity
  refresh/import/export workflows.
- `endpoints`: public Payment Endpoint publication, cleanup, and remote public
  Payment List reads.
- `contacts`: SDK-owned Paykit-facing contact/profile records, shared namespace
  handling, and optional custom path/schema hooks.
- `linked_peers`: Encrypted Link establishment, restore, recovery, and
  per-counterparty runtime state.
- `private_stream`: ordered Private Application Message intake, persistence,
  routing, dedupe, and derived view rebuild.
- `private_lists`: local and remote Private Payment List publication, caching,
  latest-state derivation, and size policy.
- `payment_requests`: Payment Request event state machine, outbound event
  queueing, proof correlation, and scheduling hooks.
- `receipts`: Receipt Access event indexing, Encrypted Receipt retrieval,
  decryption, and retry of access messages.
- `reservations`: optional contact-scoped or single-use receiving-detail
  reservation records.
- `backup`: versioned export/import of SDK-managed state.
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

`StorageTransaction` should support records for:

- identity/session state
- linked peer state
- Encrypted Link snapshots and handshake snapshots
- raw private stream items
- parsed event records
- dedupe indexes
- derived current views
- outbound messages and retry state
- endpoint publication records
- endpoint reservations
- receipt records
- backup metadata
- recovery/fail-closed markers

The SDK should ship an in-memory storage implementation for tests and examples.
Production apps should provide a durable implementation.

### PubkySessionProvider

Required for loading and storing local Pubky session material.

```rust
pub trait PubkySessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>>;
    async fn clear_session_access(&self) -> Result<()>;
}
```

`PubkySessionAccess` provides the live authenticated `PubkySession`, Pubky
client for counterparty homeserver access, and optional `PubkyLocalSecretKey`
needed for Encrypted Links. The SDK derives and persists public `IdentityState`
from that access during initialization.

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

Required for endpoint publication and payment execution workflows.

```rust
pub trait PaymentAdapter {
    async fn current_receiving_details(
        &self,
        scope: ReceivingDetailScope,
    ) -> Result<Vec<ReceivingDetail>>;

    async fn is_endpoint_payable(
        &self,
        endpoint: &PaymentEndpointCandidate,
    ) -> Result<EndpointCompatibility>;

    async fn build_payment_target(
        &self,
        endpoint: &PaymentEndpointCandidate,
    ) -> Result<PaymentTarget>;

    async fn execute_payment_request(
        &self,
        request: &PaymentRequestExecution,
    ) -> Result<PaymentExecutionResult>;
}
```

The adapter owns payment-method-specific details:

- receiving-detail generation
- network or method metadata
- balances, quote policy, fees, and route policy
- payment execution
- settlement detection
- method-specific payload parsing beyond basic Paykit compatibility

### EndpointReservationAdapter

Optional. Used when payment methods need contact-scoped or single-use receiving
details.

```rust
pub trait EndpointReservationAdapter {
    async fn reserve(
        &self,
        contact: PubkyPublicKey,
        method: PaymentEndpointIdentifier,
    ) -> Result<ReservedReceivingDetail>;

    async fn rotate_after_use(
        &self,
        reservation_id: ReservationId,
    ) -> Result<Option<ReservedReceivingDetail>>;
}
```

### SchedulerAdapter

Optional. Used for recurring Payment Requests.

```rust
pub trait SchedulerAdapter {
    async fn schedule(&self, job: ScheduledPaymentJob) -> Result<()>;
    async fn cancel(&self, job_id: ScheduledJobId) -> Result<()>;
}
```

The SDK derives scheduling eligibility, but the integrating app/runtime may own
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
- session backup metadata
- whether local secret key is available
- last successful initialization time
- sign-out generation

### LinkedPeerRecord

One record per counterparty:

- counterparty public key
- relationship state: unknown, known, linked, recovery required, blocked
- initiator/responder role
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
- raw JSON payload
- parsed `version`
- parsed `kind`
- known Paykit kind, when recognized
- parse status: valid, malformed recognized message, unknown kind, invalid JSON
- parse error, when available
- received time

This is the source of truth for private protocol-derived state.

### EventDedupRecord

Tracks Event Message idempotency:

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

Tracks local public and private endpoint publication:

- scope: public or counterparty-private
- Payment Endpoint Identifier
- payload hash
- publication status: desired, published, pending removal, removed, failed
- last attempted time
- last confirmed time
- retry count

### EndpointReservationRecord

Tracks optional contact-scoped receiving details:

- reservation id
- counterparty public key
- Payment Endpoint Identifier
- adapter-provided receiving detail id
- payload hash
- state: active, used, rotated, retired
- attribution metadata
- backup generation

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
- retrieval state

Receipt records:

- receipt id
- decrypted receipt payload
- payment reference
- optional Payment Request ID
- optional Billing Period
- issuer context
- retrieval/decryption time

Receipt Decryption Keys must be redacted in logs and debug output.

### OutboundMessageRecord

Durable outbound queue:

- outbound id
- counterparty
- message kind
- event id, when applicable
- exact serialized payload
- payload hash
- send status
- retry count
- next retry time
- last error

Event Message retries must reuse the same Event ID and exact payload.

### UnknownPrivateMessageRetention

Unknown valid Private Application Messages should be retained by default. The
SDK config should define:

- maximum retained unknown messages per counterparty
- maximum retained bytes per counterparty
- retention duration
- whether discard is allowed

Discard policy should never apply to recognized Paykit Event Messages unless
the app explicitly purges all SDK state.

## Storage And Checkpoint Invariant

The most important SDK invariant is:

```text
Persist raw received messages and derived indexes before durably saving the
advanced Encrypted Link snapshot.
```

For each receive cycle:

1. Acquire the per-counterparty private-stream lock.
2. Restore or establish the Encrypted Link.
3. Receive the full batch from `paykit-lib`.
4. Parse every syntactically valid JSON Private Application Message enough to
   identify version/kind.
5. In one transaction, insert raw stream items, update dedupe records, update
   derived views, and then save the advanced link snapshot.
6. Release the lock.

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
- peer link lock: serializes Encrypted Link restore, handshake, send, receive,
  and snapshot updates per counterparty.
- private stream lock: serializes receive/checkpoint work per counterparty.
- outbound queue lock: serializes retry workers per counterparty.
- reservation lock: serializes contact-scoped receiving-detail reservation and
  rotation per counterparty/method.

These are local SDK/runtime locks, not protocol messages. They can be in-memory
with durable recovery markers where needed. If a platform can run multiple
processes against the same storage, lock ownership must be storage-backed.

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
4. Compare desired payload hashes with `EndpointPublicationRecord`.
5. Publish changed endpoints.
6. Remove stale managed endpoints.
7. Persist confirmed/pending/failed state.
8. Return an `EndpointSyncReport`.

The SDK should not remove endpoints it did not create unless explicitly
configured to manage the whole Paykit public namespace.

### Publish Private Payment List

1. Ensure the identity is private-link-capable.
2. Ensure or restore Linked Peer.
3. Ask `PaymentAdapter` for private receiving details scoped to the counterparty.
4. Apply reservation policy if enabled.
5. Build a complete Private Payment List.
6. Enforce the pubky-noise message size limit.
7. Skip send when payload hash matches the last published payload.
8. Persist outbound record and exact payload.
9. Send through Encrypted Link.
10. Persist send result and updated link snapshot.

Private Payment Lists publish endpoints only. Payment References come from
Payment Requests, Payment Proofs, and Receipts.

### Receive Private Stream

1. Acquire peer link/private stream locks.
2. Restore or establish Encrypted Link.
3. Receive ordered Private Application Messages through `paykit-lib`.
4. Persist raw messages and parse results.
5. Route recognized messages:
   - Private Payment List updates latest-state view.
   - Receipt Access appends event records.
   - Payment Request messages update lifecycle state.
   - Payment Proof messages update proof records.
6. Detect duplicate Event IDs and conflicting payloads.
7. Persist updated snapshot after messages and derived state.
8. Return a receive report.

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
4. If private endpoints are available, ask `PaymentAdapter` for compatibility.
5. If no private endpoint is payable and public fallback is allowed, fetch
   public Payment List and check compatibility.
6. Return a structured result:
   - `Payable`
   - `NoEndpoint`
   - `UnsupportedEndpoint`
   - `PrivateRecoveryPending`
   - `PublicOnlySession`
   - `Error`

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
7. Index by Receipt ID, Payment Reference, Payment Request ID, Billing Period,
   counterparty, and issuer.

### Backup And Restore

Backup should include SDK-managed state:

- identity backup metadata, when safe
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

1. Load backup into storage under a restore transaction.
2. Validate local identity compatibility.
3. Validate every link snapshot recipient.
4. Mark peers requiring resync.
5. Do not execute automatic payments until private stream and request state are
   consistent.

## Public SDK API Shape

The exact API will evolve, but the SDK should expose operations like:

```rust
impl PaykitSdk {
    async fn initialize(&mut self) -> Result<InitializationReport>;
    async fn identity_status(&self) -> Result<IdentityStatus>;
    async fn import_session(&mut self, session_secret: String) -> Result<IdentityStatus>;
    async fn sign_out(&mut self) -> Result<()>;

    async fn sync_public_endpoints(&mut self) -> Result<EndpointSyncReport>;
    async fn clear_public_endpoints(&mut self) -> Result<EndpointSyncReport>;

    async fn sync_contact(&mut self, counterparty: PubkyPublicKey) -> Result<ContactSyncReport>;
    async fn sync_saved_contacts(&mut self, contacts: Vec<PubkyPublicKey>) -> Result<SyncReport>;

    async fn receive_private_messages(
        &mut self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivateReceiveReport>;

    async fn current_private_payment_list(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<Option<PrivatePaymentListView>>;

    async fn resolve_contact_payment(
        &mut self,
        request: ContactPaymentResolutionRequest,
    ) -> Result<ContactPaymentResolution>;

    async fn propose_payment_request(
        &mut self,
        counterparty: PubkyPublicKey,
        terms: PaymentRequestTermsInput,
    ) -> Result<PaymentRequestRecord>;

    async fn accept_payment_request(
        &mut self,
        id: PaymentRequestId,
    ) -> Result<PaymentRequestRecord>;

    async fn reject_payment_request(
        &mut self,
        id: PaymentRequestId,
        reason: Option<String>,
    ) -> Result<PaymentRequestRecord>;

    async fn cancel_payment_request(
        &mut self,
        id: PaymentRequestId,
        reason: Option<String>,
    ) -> Result<PaymentRequestRecord>;

    async fn record_payment_proof(
        &mut self,
        id: PaymentRequestId,
        proof: PaymentProofInput,
    ) -> Result<PaymentRequestRecord>;

    async fn list_payment_requests(
        &self,
        filter: PaymentRequestFilter,
    ) -> Result<Vec<PaymentRequestRecord>>;

    async fn list_receipt_access(
        &self,
        filter: ReceiptAccessFilter,
    ) -> Result<Vec<ReceiptAccessRecord>>;

    async fn retrieve_receipt(
        &mut self,
        receipt_id: ReceiptId,
    ) -> Result<ReceiptRecord>;

    async fn export_backup_state(&self) -> Result<SdkBackupState>;
    async fn restore_backup_state(&mut self, backup: SdkBackupState) -> Result<RestoreReport>;
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

`PaykitSdkConfig` should include:

- public endpoint management scope
- private sharing enabled/disabled
- public fallback policy
- private recovery wait duration and retry limits
- stale private cache policy
- outbound retry policy
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

## Implementation Build Order

1. Add `paykit-sdk` crate skeleton, error type, config, adapter traits, and
   in-memory test storage.
2. Implement durable storage records and transaction/checkpoint contract.
3. Implement identity capability state and public endpoint sync.
4. Implement Encrypted Link runtime, peer records, private stream intake, and
   checkpoint-safe persistence.
5. Implement Private Payment List latest-state derivation and contact payment
   resolution.
6. Implement outbound queue and retry model for private messages.
7. Implement Receipt Access indexing and Receipt retrieval.
8. Implement Payment Request lifecycle derivation and outbound lifecycle APIs.
9. Implement optional endpoint reservation subsystem.
10. Implement backup/export/restore for SDK-managed state.
11. Add FFI and platform wrappers.
12. Migrate one existing app integration behind the SDK and use that to refine
    adapters.

Each step should include unit tests for storage invariants and at least one
integration-style test using an in-memory adapter setup.

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

## Open Questions

1. Should the first storage implementation be caller-provided only, or should
   Paykit ship a SQLite-backed storage adapter?
2. Should the default Paykit SDK profile/contact namespace use a reserved
   subdirectory under `/pub/paykit/v0/`, or an unversioned Paykit-level path
   under `/pub/paykit/`?
3. What is the default public fallback policy for saved contacts?
4. How much endpoint reservation should be generalized in the first SDK version?
5. How much recurring Payment Request scheduling should the SDK own versus
   delegating to app/runtime schedulers?
6. What are the unknown Private Application Message retention defaults?
7. Should SDK bindings become the primary mobile API while `paykit-ffi` remains
   low-level protocol API?
