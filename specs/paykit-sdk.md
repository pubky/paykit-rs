# Paykit SDK Architecture

## Goal

Describe the Paykit SDK layer that sits above Paykit Library.

Paykit Library remains the stateless Rust implementation of Paykit Protocol
wire formats, Pubky storage helpers, Encrypted Link transport helpers, and
structural validation. Paykit SDK is the durable runtime that wallets, payment
processors, and apps can use when they need durable state, recovery behavior,
contact/payment workflows, and ergonomic platform APIs.

The SDK should be product-neutral and payment-method-neutral. It should support
payment methods through adapters without baking any one method into the SDK.

## Shared Identity Model

Paykit private communication belongs to the Pubky identity, not to one app.
Apps sharing an identity use one logical set of Encrypted Links, private stream
checkpoints, outbound queues, Payment Requests, Receipts, recovery state, and
backup data. Each SDK handle is configured with a `PaykitAppId` so app-owned
public Payment Endpoints and private messages remain attributable to their
source. The App ID does not create a separate private channel.

One public Paykit App Registry lists the apps participating in the identity,
their display names and capabilities, the identity-wide Noise public key, and
optional default-app preferences. Public Payment Endpoints are stored per app.
Private Payment Lists use Latest-State Message semantics per app and their
current views are aggregated for payment resolution.

App Registry key fields are unsigned discovery metadata. Key authority is the
`PaykitNoiseKeyAuthorization` at `/pub/paykit-authority/v0/current-key.json`.
The Pubky identity signs its owner, Ed25519 routing key, X25519 static key
(`noise_static_public_key`, hex-encoded), and generation. Verify the signature and
expected owner; retain the highest verified authorization in shared state and
backups, rejecting lower generations or different routing/static keys at the
same generation. Missing, malformed, or conflicting records block private
communication without a registry fallback. Public-only contacts need no Noise authorization.

Before private app publication or delegation, the identity authorizer calls
`publish_paykit_noise_key_authorization` with the current Paykit key, Pubky identity
secret, and `PAYKIT_AUTHORIZER_SESSION_CAPABILITIES`:
`/pub/paykit/:rw,/pub/paykit-authority/v0/current-key.json:rw`.
Ordinary apps receive only the Paykit secret and `PAYKIT_SESSION_CAPABILITIES`
(`/pub/paykit/:rw`), never authority-path write access. Rotation requires authorizer
access and existing authorization; its publication order and retries follow
[shared-state recovery](#backup-and-restore).

Authorization has no expiry. Freshness relies on the homeserver serving current
contents, enforcing separate capabilities, and fencing writes after lock loss.
Generation pins do not detect a stale first read or rollback of shared state.
Device revocation requires revoking its Pubky grant and distributing replacement
keys only to remaining apps; rotation cannot revoke a Pubky identity secret holder.

The app or binding layer provides live Pubky session access. Handles for apps
sharing an identity use the same encrypted Pubky-hosted SDK state. Homeserver
write locks serialize whole-state transactions, and prepared Noise sends
couple exact ciphertext with the advanced Encrypted Link snapshot before
publication.

## Design Principles

- Keep `paykit-lib` stateless. It validates and sends protocol objects, but it
  does not own local history, lifecycle state, scheduling, retries, or contact
  policy.
- Make the SDK stateful and crash-safe. It owns durable event logs, derived
  views, snapshots, retries, recovery state, and app-facing getters.
- Treat private state as identity-wide. App IDs attribute endpoints and
  messages; they do not partition Encrypted Links or private streams.
- Preserve the private stream. Receive all Private Application Messages in
  order and persist them before advancing the Encrypted Link checkpoint.
- Own the Pubky integration needed by Paykit. Since Pubky is Paykit's only
  transport/storage backend, apps should not need a separate Pubky integration
  just to use Paykit SDK.
- Keep product profile/contact UX separate. The SDK can own small Paykit-facing
  profile/contact records and shared paths, but it should not own app screens,
  social graph semantics, or product-specific profile schemas.
- Keep payment execution separate. Payment adapters provide receiving details,
  payable endpoint ordering, payment-target construction, and method-specific
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
- Paykit SDK runtime: durable state, private stream routing, Event ID dedupe,
  Payment Request and Allowance lifecycle derivation, endpoint publication,
  contact payment resolution, retries, recovery, Pubky session bootstrap and
  capability handling, Pubky-backed Paykit profile/contact metadata, and
  app-facing lifecycle/history views and APIs.
- Payment adapter layer: receiving-detail generation, payable endpoint
  ordering, payment-target construction, method/provider state, and activity
  records.

The SDK may still accept narrow platform hooks for secure session persistence,
auth UI, scheduling, and logging. Those hooks
should not require each app to reimplement Pubky Paykit logic or to depend on a
separate shared-runtime coordinator.

The SDK should depend on `paykit-lib`, not replace it. Platform apps should
prefer SDK bindings for normal product workflows and use `paykit-lib` bindings
only for low-level protocol operations.

## Implemented SDK Scope

The current Rust SDK implementation covers:

- the `PaykitSdk` runtime facade
- the storage adapter contract and in-memory test storage
- Pubky identity status tracking and explicit sign-out
- public Payment Endpoint sync
- Paykit App Registry publication and app-attributed public endpoints
- Encrypted Link setup, private stream intake, outbound private queueing,
  retries, and recovery marker workflows
- Private Payment List publication, caching, and contact payment resolution
- Payment Endpoint Reservations for contact-scoped receiving details
- Payment Request and Allowance lifecycle state, Receipt Access indexing,
  receipt issuance, and receipt retrieval
- Paykit-facing profile/contact helpers
- encrypted Pubky-hosted identity-wide SDK state with locked updates
- crash-safe prepared Noise send and receive checkpointing
- SDK backup/export/restore validation

The workspace also exposes Swift and Kotlin SDK bindings through `paykit-ffi`.
First-party durable mobile storage helpers, payment execution, settlement
confirmation, product UI, app backup transport, background synchronization,
and recurring payment scheduling
remain separate implementation areas unless they are explicitly listed above.

## Crate Layout

The SDK crate uses this layout:

```text
paykit-sdk/
  Cargo.toml
  src/
    lib.rs
    config.rs
    error.rs
    identity.rs
    pubky_session.rs
    domain/
      adapters/
      allowances/
      contacts/
      endpoints/
      endpoint_reservations/
      linked_peers/
      outbound_private/
      payment_requests/
      private_lists/
      private_stream/
      receipts/
      records.rs
      publication.rs
      recovery.rs
    runtime/
      mod.rs
      app_registry.rs
      app_removal.rs
      backup.rs
      allowances.rs
      contacts.rs
      encrypted_links.rs
      outbound_private.rs
      payment_requests.rs
      payment_resolution.rs
      payment_resolution/
      private_lists.rs
      private_stream.rs
      profiles.rs
      public_endpoints.rs
      receipts/
        mod.rs
        issuance.rs
        retrieval.rs
      recovery.rs
      reservation_cleanup.rs
    storage/
      mod.rs
      in_memory.rs
      pubky_shared.rs
      queue.rs
      records.rs
      state_blob.rs
    backup/
      mod.rs
      validation/
```

Module responsibilities:

- `runtime`: owns `PaykitSdk` and the SDK workflows that coordinate adapters,
  storage, Pubky, and Paykit Library calls.
- `domain`: SDK-facing types, records, and pure derivation helpers.
- `storage`: durable records, transaction interface, queue helpers, and
  in-memory test storage.
- `backup`: versioned export/import of SDK-managed state and restore
  validation.
- `config`: product-neutral policy knobs such as recovery behavior, endpoint
  publication scope, and retry limits.
- `identity`: SDK-owned Pubky identity and live-session state,
  and local Pubky key helpers.
- `pubky_session`: Pubky signup, signin, session import, auth handoff, and
  `pubky://` normalization helpers.

Additional modules should be added only when they have concrete implementation:

- `scheduler`: optional recurring Payment Request scheduling integration.
- `telemetry`: structured logs and redaction helpers.

Platform bindings live in `paykit-ffi`. Additional wrapper packages can sit on
top of that binding layer when a platform needs a more idiomatic API.

## Core Runtime Object

The SDK should expose a runtime object per app integration. Multiple handles
for one identity participate in the same logical private state:

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

`StorageAdapter` is required. It presents the authoritative logical SDK state
to the runtime and must support atomic updates or an equivalent crash-recovery
contract. Apps sharing one identity must use the same logical state backing;
separate app-local adapters are not safe substitutes for shared state.

```rust
#[async_trait]
pub trait StorageAdapter {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>>;
}
```

The Rust SDK storage model supports records for:

- identity state
- linked peer state
- Contact Records and cached Paykit Profiles
- Encrypted Link snapshots and handshake snapshots
- endpoint publication records
- endpoint reservation records
- per-App operation leases and Payment Request execution claims
- outbound Private Application Message records and retry state
- raw Private Application Message stream items
- Event Message dedupe indexes
- receipt records
- recovery/fail-closed markers

Allowance V1 requires the SDK storage model to retain:

- lifecycle evidence in the private stream, outbound messages, and Event Message
  dedupe indexes, from which Allowance lifecycle views are derived
- Payment Request-to-Allowance associations and their selection revision history
- semantic payment keys shared by manual and automatic execution, manual-only
  and deferred decisions, usage reservations, and payment outcome history
- evaluation-time watermarks and accounting recovery/fail-closed state

Admission must check current lifecycle, association revision, occurrence exclusion,
and capacity, then atomically persist its reservation and watermark. Outcome
updates must be idempotent. These are SDK/runtime requirements under
[Allowance V1](allowances.md#component-responsibilities); lifecycle views need not
be stored separately from their durable evidence.

The SDK ships in-memory storage for tests and examples and encrypted
Pubky-hosted storage for serialized shared-identity runtimes. Custom adapters
remain available for app-owned durable storage.

`PubkySharedStateStorage` stores one encrypted blob at
`/pub/paykit/v0/shared-state.bin`. The inner state uses the SDK state-blob
codec. The outer envelope uses XChaCha20-Poly1305 with a fresh random nonce, a
key derived from the current identity-wide Paykit secret, and the Pubky public
key plus key generation as associated data. Reads reject invalid,
undecryptable, wrong-generation, or internally inconsistent state. Encrypted
blobs are limited to 64 MiB; whole-state transactions require additional
working memory.
Encryption does not hide the resource's existence, size, or update timing, and
does not by itself detect a homeserver replay of an older valid blob.

Each transaction acquires a renewable WebDAV file lock, fetches and decrypts the
latest blob, applies the existing `StorageTransaction`, validates and encrypts
changed state, then writes with the lock token and releases the lock. Contended
transactions can return a storage conflict. Multi-step operations may already
have committed work, so callers must inspect durable records before retrying
commands that create requests or payments. A storage instance also fails closed
if a resource it previously observed disappears.

Changed-state commits compact obsolete Private Payment Lists per counterparty
and App ID. They retain the latest valid inbound list (including an empty clear),
pending or prepared sends, reservation evidence, and the latest outbound intent,
publication, and attempted send. Event Message history and deduplication records
are not pruned, so the 64 MiB limit still applies to growing payment history.

Before each state PUT, the adapter publishes an empty, uniquely named marker
under `/pub/paykit/v0/shared-state-writes/`. A confirmed PUT or an explicit PUT
rejection for lock expiry (412), rate limiting (429), or storage quota (507)
removes that attempt's marker, provided cleanup succeeds. All other failures
are treated as uncertain writes; these and cancellation leave the marker.
Marker cleanup failure does not undo a confirmed commit. A transaction finding
pending markers holds and renews the state lock for five minutes, removes the
observed markers, then loads fresh state. Read-only transactions and key rotation
use the same barrier. Cancelling the wait leaves the markers for the next caller.
Competing runtimes that cannot acquire the lock while markers exist receive
`SharedStateBusy`, including on reads. They should back off for recovery, not
immediately retry; cancellation can restart the wait, so there is no fixed deadline.

This cooldown is a best-effort mitigation for Pubky 0.14's missing commit-time
lock enforcement, not a production concurrency guarantee. A write delayed beyond
the cooldown can still overwrite newer state; a successful readback does not end
uncertainty.
Normal writes add marker publication and cleanup requests but no timed delay.

### PubkySessionProvider

Required for loading live Pubky session access for the shared Paykit identity.

```rust
pub trait PubkySessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>>;
    async fn revoke_session_access(&self, access: &PubkySessionAccess) -> Result<()>;
    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>>;
    async fn clear_session_access(&self) -> Result<()>;
}
```

`PubkySessionAccess` provides the live authenticated `PubkySession`, Pubky
client for counterparty homeserver access, and an optional
`PubkyLocalSecretKey` and `PaykitIdentitySecretKey`. When an explicit Paykit
secret is absent, the SDK derives generation 1 from the local Pubky secret. A
holder of that Pubky secret can derive any later generation explicitly, while
delegated apps receive only the current generation-specific Paykit secret.
Independent Noise and shared-state keys are derived from the Paykit secret.
This lets delegated apps use private Paykit without receiving the Pubky root
secret. Public-only session access can use public workflows but cannot
establish or advance Encrypted Links. The SDK derives and persists public
`IdentityState` from that access during initialization.

Paykit KDF v1 MUST use BLAKE3's `derive_key(context, key_material)`
operation (not keyed hashing), with the exact UTF-8 context strings below:

```text
paykit_secret = derive_key("paykit/identity-secret", pubky_secret || u64_be(generation))
noise_secret  = derive_key("paykit/noise", paykit_secret)
state_key     = derive_key("paykit/shared-state", paykit_secret)
```

`pubky_secret` is the raw 32-byte Pubky secret, not its seed, mnemonic, or text
encoding. `generation` MUST be positive and encoded as exactly eight big-endian
bytes, making the first input 40 bytes. Each output and each second-stage input
is exactly 32 bytes. Rotation uses the next generation derived from the same
Pubky secret, never a previous Paykit secret or randomly chosen replacement.
The Noise public key is the Pubky keypair public key derived from `noise_secret`.
Delegated apps import the authorizer-derived secret through a trusted channel;
they cannot verify this derivation without the Pubky root. The SDK checks it
when the root is available.

The hash mode, context strings, input order, encoding, and lengths above define
Paykit KDF v1. Changing any of them requires a new explicitly selected derivation
scheme version, not a key-generation increment. Generations rotate keys within
this fixed scheme; they do not change its transcript.

Canonical vectors (hex), using `pubky_secret = 09` repeated 32 times:

| Generation | Paykit secret | Noise secret | Shared-state key |
| --- | --- | --- | --- |
| 1 | `5ac67d4f33e44a52a647e6515811095c128ef51f23010e6d841d41ba8c0743c4` | `0e00cd53a2a7156f7e9720c406d05332920d11164d913b717601bba0a13b6bfb` | `501c714e7f1b8f44b51177b410e328c7e9df7e392ca6a972cc02189de1e30764` |
| 2 | `f3a1d35856bb4bdf769d6cfb3c943debd3871a62d4c14e4d7b4a7755c7b965c4` | `4882e20c237469fddab74a9fdf9271c493bb1357b00d110e8b9b89a7e25f12a2` | `9b3008988de501585c74df37d0bc8602ac67ea61f4a2c05834cfd0595a79faa4` |

If `load_session_access` returns `None`, no live session access is currently
available. Ordinary refreshes must preserve the shared Paykit state and block
Pubky-backed workflows until session access is available again. Explicit
`sign_out` validates the identity, revokes this application's live Pubky grant,
and clears its local session access without deleting shared Paykit state. If
live access or remote revocation is unavailable, it preserves local access so
the operation can be retried. `forget_session_access` is the explicit local-only
escape hatch and does not invalidate other persisted copies of the grant. An
application that should also withdraw its published payment capability calls
`remove_paykit_app` while authenticated before signing out. Explicit app
removal requires app-owned Payment Requests to be canceled or otherwise
terminal and private financial events and Receipt Access delivery to be
complete. Any previously shared Private Payment Lists must be cleared first.
Unanswered identity-addressed requests are not owned by an app and remain
available to other apps.

`load_public_storage` lets contact resolution fetch public Payment Endpoints
without requiring authenticated session access. Implementations can reuse the
Pubky client from `PubkySessionAccess` when they have one, or provide a separate
public-storage client when only unauthenticated reads are available.

Identity status reports the last initialized public key and live-session
availability directly. A public key with no live session identifies the stored
state but Pubky-backed workflows must wait. A matching live session can run
public operations. Encrypted Link workflows also require access to the matching
Paykit identity secret, supplied directly or derived from the local Pubky
secret for the required generation. Session access without an explicit Paykit
secret falls back to generation 1; after rotation, the current derived key must
be supplied explicitly. Transient absence of live session access and explicit
app sign-out must preserve private SDK state. If session access changes to
another identity, the SDK rejects the existing state backing without modifying
it; the caller must select the shared state backing for the new identity.

The SDK should provide high-level initialization, backup/export/restore, and
sign-out APIs itself. The provider is the narrow platform hook for secure
storage and auth-session handoff, not a separate Pubky SDK or identity product
that integrators must use.

Rust integrations can use `PubkySessionBootstrap` to create or import the live
grant-session access consumed by the provider. Each bootstrap is constructed
with a stable, app-owned Pubky client ID. It covers common Pubky account/session
workflows: signup, signin, grant-secret import, auth handoff start/resume/approve
helpers, and `pubky://` resource normalization. All session and auth operations
require Pubky grants. Full SDK runtime auth should use
`PAYKIT_SESSION_CAPABILITIES` as the expected scope for auth
start/resume/approve, completion, and session import. The required scope covers
the identity-wide Paykit public and private paths. Authorizers instead use
`PAYKIT_AUTHORIZER_SESSION_CAPABILITIES` as specified in the
[Shared Identity Model](#shared-identity-model). Private-capable auth
completion and session import also supply either the matching
`PubkyLocalSecretKey` or the delegated Paykit identity secret.
Pending external grant auth also owns a client proof-of-possession key that is
not recoverable from its authorization URL. Apps that need an unapproved
request to survive process loss must securely persist the complete state
returned by `PubkyAuthRequest::save_state` and resume with that state. Pubky
relay approvals are consumed when read, so cancellation or credential-exchange
failure after approval retrieval requires a new auth request. Apps must delete
saved state after completion, expiry, or abandonment.
`PubkyLocalSecretKey` also provides Pubky-compatible BIP39 seed and
mnemonic helpers plus public-key-from-secret helpers. Apps intentionally using
the same Pubky identity material derive the same initial Paykit identity key.
Exported session secrets contain both the signed grant and proof-of-possession
key. Those secrets, pending auth state, and auth URLs are secret-bearing values
and must be stored or displayed only for their intended flow.
Bindings should wrap these helpers so mobile apps do not need a second Pubky
SDK dependency for ordinary Paykit onboarding.

Applications can attach an app-defined companion claim to a Pubky Auth
approval. The integrator supplies the claim query parameter, claim type,
expected capability, and serialized unsigned payload. The SDK owns request
validation, request-bound identity signing, companion channel derivation,
XSalsa20-Poly1305 transport, relay delivery, and grant authorization. The
companion message must be accepted by the relay before the Pubky grant is
approved. Bitkit's watch-only account claim is one application of this generic
operation. The shared protocol is specified in
[`pubky-auth-companion-claims.md`](pubky-auth-companion-claims.md).

### Paykit Profile And Contacts

The SDK provides default Pubky-backed Paykit-facing profile metadata so
different Paykit apps can interoperate. This belongs in the SDK, not in
`paykit-lib` core protocol validation.

Public profile and contact paths are identity-wide:

- profile record: `/pub/paykit/profile.json`
- Paykit blobs: `/pub/paykit/blobs/...`
- public contact markers: `/pub/paykit/contacts/...`

The identity-wide Paykit App Registry is stored at
`/pub/paykit/v0/app-registry.json`. Public Payment Endpoints are stored under
`/pub/paykit/v0/apps/{app_id}/endpoints/...`, so app ownership remains explicit
without partitioning private communication.

Apps sharing this state are mutually trusted: they hold the same Paykit key
material and broad Pubky write access. App IDs attribute work and scope SDK
commands; they do not isolate a malicious app from another app's state.

The registry uses this closed-world JSON shape:

```json
{
  "version": 1,
  "kind": "paykit.app_registry",
  "key_generation": 1,
  "noise_public_key": "<z32-public-key>",
  "apps": {
    "bitkit": {
      "display_name": "Bitkit",
      "capabilities": {
        "private_payments": true,
        "payment_requests": true,
        "receipts": true,
        "outgoing_payments": true
      }
    }
  },
  "default_app_id": "bitkit",
  "default_apps_by_endpoint": {
    "btc-lightning-bolt11": "bitkit"
  }
}
```

App IDs are stable path-safe identifiers. Defaults may only refer to registered
apps. Registries are limited to 64 KiB, 64 applications, and 256
endpoint-specific defaults. Registry updates replace the complete document;
the SDK compares content revisions under a homeserver write lock and uses bounded
refetch/merge retries so one app cannot overwrite another app's newer registration.

`noise_public_key` may be omitted while only public-capable apps are
registered. The first private-capable app initializes it from the current
Paykit identity secret. Key rotation advances `key_generation`, replaces the
Noise public key, re-encrypts shared state, clears old Encrypted Link
snapshots, and preserves non-cryptographic history. Private capabilities cannot
be registered without a Noise public key.
Persist the replacement key before starting rotation. If rotation fails after
re-encrypting state but before publishing the registry, retry with that same
replacement key; generating another replacement cannot recover that commit.

An app must successfully publish its registry entry before creating
app-attributed private work. Removal preflight reports outstanding requests,
private delivery, Receipt issuance, and shared Private Payment Lists.

`image_uri` may point at the Paykit blob prefix or another public image
location. The SDK can publish/delete Paykit blobs under the identity-wide blob
prefix and can fetch public `pubky://` files referenced by profile metadata as
bytes or UTF-8 text. Image decoding, resizing, platform cache integration, and
UI rendering stay in the app/bindings layer. These helpers are not a generic
Pubky file-management layer.

Paykit Profile has a small shared display core plus an application-defined
`extra` JSON object for additional public identity fields. The SDK stores and
returns `extra`, but does not assign protocol meaning to those fields. Writers
replace the complete identity-wide Paykit Profile document at its fetched
revision. A concurrent replacement fails so the writer can reload first.

This is separate from the Pubky app profile namespace:

- Pubky app profile: `/pub/pubky.app/profile.json`
- Pubky app follows: `/pub/pubky.app/follows/`

The SDK can expose read-only helpers for Pubky app profile and follows so
Paykit apps do not reimplement basic Pubky reads. It must not write Pubky app
profile or follows data; those remain owned by Pubky app/product flows.

For contact display, the SDK should expose a resolver that tries Paykit Profile
first and can fall back to Pubky Profile when no Paykit Profile exists. This
lets apps reuse a common display fallback while keeping the two namespaces
separate. Malformed Paykit Profile data should surface as an error instead of
being silently hidden by fallback.

Default profile records can be public because they are display metadata.
Contacts need more care because they can reveal a social/payment graph. The SDK
keeps saved contacts in shared private SDK state by default. Public contact
markers under `/pub/paykit/contacts/` are opt-in through SDK policy and explicit
runtime calls.
One Contact Record represents one Pubky identity. Paykit Apps participating in
that identity are discovered through its Paykit App Registry rather than stored
as separate contacts.

Profile JSON may ignore unknown fields so the public profile schema can grow
without breaking older SDKs. Private Paykit protocol messages remain
closed-world unless their spec says otherwise.

Paykit schemas should be small, versioned, and Paykit-facing:

- profile display name and image pointer
- normalized Pubky public key
- contact public key
- optional public contact marker

The SDK should not standardize product profile pages, Pubky app follows
semantics, contact grouping, or UI behavior.

### PaymentAdapter

Required for endpoint publication, endpoint selection, and payment-target
building. Public and private values use distinct types and callbacks; they are
never combined into one candidate batch.

```rust
pub trait PaymentAdapter {
    async fn current_public_receiving_details(
        &self,
    ) -> Result<Vec<PublicReceivingDetail>>;

    async fn select_public_payment_endpoints(
        &self,
        request: &PublicPaymentEndpointSelectionRequest,
    ) -> Result<Vec<PublicPaymentEndpointCandidate>>;

    async fn build_public_payment_target(
        &self,
        endpoint: &PublicPaymentEndpointCandidate,
    ) -> Result<PaymentTarget>;

    async fn current_private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PrivateReceivingDetail>>;

    async fn reserve_private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Option<Vec<PrivatePaymentEndpointReservation>>>;

    async fn cancel_private_receiving_detail_reservation(
        &self,
        cancellation: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()>;

    async fn select_private_payment_endpoints(
        &self,
        request: &PrivatePaymentEndpointSelectionRequest,
    ) -> Result<Vec<PrivatePaymentEndpointCandidate>>;

    async fn build_private_payment_target(
        &self,
        endpoint: &PrivatePaymentEndpointCandidate,
    ) -> Result<PaymentTarget>;
}
```

Each endpoint selection request includes candidates from exactly one payment
mode and optional amount context. The adapter returns payable candidates in the
order it wants payment execution to try them. Public APIs cannot receive
private candidates, and private APIs cannot receive public candidates.

The adapter owns payment-method-specific endpoint details:

- receiving-detail generation
- network or method metadata
- balances, quote policy, fees, and route policy
- method-specific payload parsing beyond basic Paykit compatibility

Payment execution and settlement detection stay with the integrating
application or payment provider. SDK APIs can accept execution results from
those systems when Paykit needs to record them.

### Payment Endpoint Reservations

Some payment methods need contact-scoped receiving details. The SDK lets payment
adapters reserve receiving details for Private Payment List sharing. The SDK
queues the Private Payment List and stores linked reservation records in one
storage transaction. Reservation records keep lifecycle metadata and a payload
hash; they do not store the raw reserved endpoint payload.

When an adapter returns reservations, those reservations are the complete
Private Payment List to share for that counterparty. Adapters that need mixed
reserved and ordinary entries should include both as returned reservations.

The payment adapter creates reservations before the SDK can persist linked
records. Adapters should make reserved details idempotent, expiring, or safe to
abandon if the process stops before durable queueing. After acquiring the peer
lease, the SDK cancels unpersisted reservations on validation or queueing failure.
If lease acquisition fails, caller-supplied reservations remain caller-owned:
retry or release them without canceling reservations another worker has queued.
Any adapter that returns reservations must explicitly implement reservation
cancellation; cleanup must not be silently treated as successful.

Reservation IDs are scoped by counterparty and owner App ID. They are
idempotency keys for SDK reservation records, not for Private Payment List
delivery. Requeueing the same reservation details may queue another
latest-state Private Payment List and update the record to the latest outbound
message id. Idempotent repeats preserve the original reservation attribution,
expiry, and creation time; adapters that want new metadata should use a new
reservation id.

When cleanup starts canceling a persisted reservation through the payment
adapter, the SDK marks the reservation as cancellation-started in storage before
calling the adapter. Reservation IDs with cancellation-started records must not be
reused for new Private Payment Lists until cleanup removes the record.
App-removal cleanup checks its App lease when claiming cancellation, before
adapter dispatch, and before deleting the reservation. If ownership changes
after dispatch, cancellation may already have happened externally; the record
remains for the current worker to reconcile through idempotent cancellation.

Single-use Payment Request reservations are outside the SDK shape until the
request-specific context is defined.

### Recurring Scheduling

The SDK derives durable Recurring Payment Request lifecycle state and exposes
accepted requests through `active_recurring_payment_requests`. It does not
calculate due Billing Periods, run timers, authorize or execute payments,
validate payment-method-specific settlement, or decide service entitlements.
Those responsibilities belong to the integrating application/runtime. See
[Recurring Payment Requests And Subscriptions](payment-requests.md#recurring-payment-requests-and-subscriptions).

### Logger And Clock

The SDK should accept:

- `Clock`: deterministic current-time source for expiry, retries, and tests.
- `Logger`: structured logging with redaction for secrets, Receipt Decryption
  Keys, raw private payloads, and session material.

## Storage Model

The SDK storage model can be implemented by each platform, but the logical
records should be stable.

### IdentityState

Tracks the Pubky identity associated with the logical SDK state:

- last initialized Pubky public key
- last successful initialization time

### LinkedPeerRecord

One record per counterparty identity:

- counterparty public key
- relationship state: not linked, linking, linked, recovery required, blocked
- in-progress handshake role: initiator or responder
- last sync time
- last private receive time
- current recovery marker state
- failure counters
- policy overrides

Private and payment state is scoped by counterparty Pubky key. App IDs inside
messages preserve source and payment ownership without creating separate links.

### EncryptedLinkState

One record per linked peer in the current Rust SDK:

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
- raw UTF-8 payload, or a retained invalid-frame marker when plaintext bytes
  are not UTF-8
- parsed `version`
- parsed `kind`
- parsed source `app_id`
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
Recognized malformed Event Messages with a parseable Event ID also contribute
dedupe evidence. This records ID usage, not successful protocol validation, and
does not make a malformed message eligible for lifecycle or Receipt Access
processing. Changing payload bytes requires a fresh Event ID. Backup restore
validates current stream metadata and required indexes against the same rules
as live intake; stale metadata or missing indexes are rejected without changing
the destination state. Backups from unreleased development formats are not
migrated. Raw unsupported messages retained in the current format remain
available for audit and fail-closed Allowance correlation.

### PrivatePaymentListView

Latest-state view per counterparty and source Paykit App:

- source App ID
- latest valid stream item id
- current Payment Endpoint map
- last refresh time

Malformed newer Private Payment List messages do not replace the last valid
view. They remain in the raw log.

### EndpointPublicationRecord

Tracks SDK-managed public Payment Endpoint publication:

- owner App ID
- Payment Endpoint Identifier
- last payload the SDK tried to publish
- shared `PublicationStatus`: pending publication, published, pending removal,
  removed, failed
- last status update time
- last error, when available

Additional endpoint fields should be added only when the SDK needs change
detection or richer retry policy.

### EndpointReservationRecord

Tracks optional contact-scoped receiving details:

- reservation id
- counterparty public key
- owner App ID
- Payment Endpoint Identifier
- payload hash
- latest outbound message id used to queue the reservation for sharing
- attribution metadata
- reservation expiry, when provided by the payment adapter
- cancellation-started timestamp, when adapter cleanup is in progress

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

SDK lifecycle states should be explicit and local:

- `proposed`
- `proposal_expired`
- `accepted`
- `rejected`
- `canceled`
- `proof_submitted`
- `active_recurring`
- `recovery_required`
- `invalid_conflict`

The SDK may expose product-friendly summaries, but it should not claim generic
settlement finality unless the payment adapter confirms it.

`PaymentRequestFilter` supports product screens without requiring callers to
know every counterparty in advance:

- optional counterparty
- optional local role
- optional lifecycle states, where an empty list means all states
- optional recurring/one-time filter
- inbound-only mode for received Payment Requests

### ReceiptAccessRecord And ReceiptRecord

Receipt issuance records:

- counterparty that should receive Receipt Access
- issuing App ID
- receipt id and Receipt Access Event ID
- payment reference and optional Payment Request correlation fields
- Encrypted Receipt JSON
- exact Receipt Access JSON
- local issuance status, timestamps, outbound message id, and last error

Receipt Access records:

- event id
- receipt id
- sender/issuer counterparty
- sender/issuer App ID
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
- issuer App ID
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
- source App ID
- Private Message Kind
- exact raw JSON payload, including Event ID when the message kind has one
- send status
- attempt count
- created, updated, attempted, and sent timestamps
- optional durable receipt confirmation timestamp (`confirmed_at`)
- last error

The SDK should use one generic outbound Private Application Message record type
for all Private Application Message kinds. Event Messages are processed as FIFO
per counterparty Encrypted Link. Private Payment Lists use latest-state
semantics per source App ID, so older lists never sent by the same app may be
superseded by a newer complete list.
Send workers must claim the next sendable message through storage before
sending it. A stale `Sending` queue head can be reclaimed after the lease
timeout, but the SDK must retry that same message before later private messages
advance the Encrypted Link. `Sending` and `Failed` messages must not be
superseded by newer lists: either may have reached the homeserver. Their prepared
ciphertext must be retried at the queue head until delivery is checkpointed or
the link is recovered.

Event Message retries must reuse the same Event ID, App ID, and exact payload.
`Sent` records publication; `confirmed_at` separately records the authenticated
counterparty's durable receipt. Unconfirmed events become eligible for retry
after the outbound retry backoff and are requeued in original order
when the link recovers. Waiting for confirmation does not block newer messages.
Delivery Confirmations share this queue but are not themselves confirmed or
periodically replayed after publication; duplicate events queue them again.
They are identity-owned obligations and remain sendable after the confirming
App signs out or is removed. Removing an App still requires its published
unconfirmed events to be resolved first.

Sending through Pubky is not atomic with SDK storage. The SDK first prepares a
send without mutating live Noise state, then stores the exact ciphertext and
advanced Encrypted Link snapshot together. Only after that transaction succeeds
does it acknowledge the local Noise transition and publish the ciphertext. A
crash or uncertain publication retries those exact bytes before later messages
can advance the link. Non-retryable link-state failures still mark the peer
recovery-required before automatic private sends continue. SDK records expose
local outbound status so apps can distinguish queued intent from checkpointed
send state. The status is not an acknowledgement from the counterparty.
Superseded reservation cleanup failures are reported as local cleanup failures;
they do not change whether the current outbound message was sent or failed.

## Storage And Checkpoint Invariant

The most important SDK invariant is:

```text
Persist raw received messages and derived indexes before durably saving the
advanced Encrypted Link snapshot.
```

For each receive cycle:

1. Claim the per-counterparty peer link operation lease.
2. Restore or establish the Encrypted Link.
3. Prepare the next message without advancing the live Noise state.
4. Persist the received Private Application Message plaintext and parse enough
   to identify version, kind, and source App ID when the payload is valid JSON.
5. In one transaction, insert raw stream items, update Event Message dedupe
   records, queue their Delivery Confirmations (or apply received confirmations),
   and save the advanced link snapshot.
6. Acknowledge the prepared receive, then repeat until no message is available.
7. Release the lease.

If the app crashes after messages are stored but before the snapshot is stored,
replay is acceptable and must be deduped. If the snapshot is stored without the
messages, events may be lost; the SDK must not allow that.

## Runtime Locks

The SDK needs local runtime locks to prevent concurrent operations from racing
the same durable state.

Recommended locks:

- identity lock: serializes import/export/sign-out and session refresh.
- storage-backed App operation lease: serializes publication and cleanup of one
  App's public Payment Endpoints across devices and runtimes.
- storage-backed peer link operation lease: serializes Encrypted Link restore,
  handshake, send, receive, and snapshot updates per counterparty identity.
- outbound queue claim/lock: serializes retry workers per counterparty identity.
- reservation transaction: stores reservation records and the outbound message
  that shares them atomically. Existing reservation IDs with the same
  counterparty, Payment Endpoint Identifier, and payload hash are idempotent;
  cancellation-started records, conflicting existing details, and duplicate IDs in
  the same batch are rejected.
  The idempotency applies to reservation records, while Private Payment List
  delivery remains latest-state and may queue another outbound message.

These are SDK/runtime coordination mechanisms, not protocol messages. A lock
that protects only one runtime handle can be in-memory. Any lock protecting
identity-wide state across apps or processes must be enforced by the shared
storage backing. Lease expiry makes a stale operation reclaimable by another
worker; durable writes still check the stored lease id so an earlier holder
cannot commit after a newer lease has replaced it.
App workflows check their current lease before remote publication or cleanup
and renew it when at most half its lifetime remains;
renewal cannot revive a lease replaced by another worker.

The local lease does not make an already-started remote write safe if its
holder is suspended past expiry. Homeserver write locks must fence writes through
commit so an expired holder cannot replace newer shared state. Prepared sends
ensure a retry publishes the same ciphertext. Complete, durable file publication
is a separate homeserver requirement; locks do not provide storage crash safety.

The Rust SDK implementation provides storage-backed per-peer leases for
Encrypted Link work and per-App leases for public endpoint sync and App
removal. It also serializes `initialize`, `sign_out`, and
`forget_session_access` calls on one runtime instance. Homeserver-backed shared
storage extends the durable leases across devices and processes.

## Workflows

### Initialize SDK

1. Load SDK config.
2. Load identity state from storage.
3. Load Pubky session access through `PubkySessionProvider`.
4. Record whether the stored identity has matching live session access.
5. Load peer records and recovery markers.
6. Start optional retry workers only after storage is ready.
7. Return identity status with the persisted public key and current capability.

### Import Or Restore Pubky Session

1. Acquire identity lock.
2. Import or restore the Pubky session through SDK-owned Pubky logic.
3. Persist resulting session access through SDK storage/session hooks.
4. Validate session access and current Paykit identity key availability when
   private capability is expected.
5. Persist identity state.
6. If identity changed, reject the current state backing without modifying it;
   select the shared state backing for the new identity before retrying.
7. Return current identity status.

### Publish Public Payment Endpoints

1. Claim the configured App's shared operation lease.
2. Ask `PaymentAdapter` for the App's current public receiving details.
3. Convert receiving details into Payment Endpoint payloads.
4. Validate identifiers and payloads through `paykit-lib`.
5. Persist all pending publication and removal records in one transaction.
6. Publish pending endpoints and remove stale managed endpoints.
7. Persist each confirmed or failed result while verifying the lease.
8. Release the lease and return an `EndpointSyncReport`.

The SDK should not remove endpoints it did not create for the configured App ID
unless explicitly configured to manage that app's complete endpoint namespace.

### Establish Encrypted Link

1. Require matching live session access and local routing/static keys and
   generation matching the identity's current signed authorization.
2. Fetch and verify peer authorization, check its owner and generation pin,
   persist it, and use its signed routing key, never the App Registry key.
3. Start an initiator or responder Encrypted Link Handshake through
   `paykit-lib`.
4. Persist the handshake snapshot, role, and `linking` peer state.
5. Advance the stored handshake on retry/poll cycles, checking the peer's
   authenticated X25519 static key against the signed record whenever available.
6. When the handshake is pending, replace the stored handshake snapshot.
7. Completion requires a matching static key before persisting the active link
   snapshot, clearing the handshake snapshot/role, and marking the peer `linked`.
8. On restore, repeat local authorization and peer fetch/verify/pin checks.
   Require the snapshot's routing key and any authenticated peer static key to
   match; completed links require the static key before send or receive.
   Snapshot validity alone is not identity authorization.
9. Failed snapshot restore or protocol/key mismatch requires peer recovery and
   stops private automation. Transient advancement failures may remain `linking`
   for retry.

Every Encrypted Link setup, send, and receive operation fetches current
authorization; retained pins are not a substitute for this check.

### Encrypted Link Recovery Markers

When one side can no longer trust its local Encrypted Link state, the SDK fails
closed locally and publishes an Encrypted Link Recovery Marker through Pubky
public storage when matching live session access is available. The marker is
not sent over the broken link. Its attempt ID also identifies the peer's current
stream generation, so the marker remains published after recovery completes.

Marker privacy rules:

- derive marker paths per identity pair from the local identity-wide Noise
  secret key and the counterparty routing key from its verified, pinned signed
  authorization
- keep marker payloads minimal: version, kind, recovery attempt ID, and creation
  time
- do not include Payment Endpoints, Payment References, message counts, peer
  display metadata, payment state, or detailed recovery stages

SDK behavior:

1. Persist and publish a local attempt ID before the first handshake. Rotate it
   when local state requires a fresh connection; retries reuse the saved ID.
2. Observe the counterparty marker before trusting cached private payment state
   when matching live session access is available. Cached state can still be
   listed for the same persisted identity without live session access, but it
   should be treated as previously received local state rather than freshly
   verified private state.
3. If a new counterparty attempt ID is observed, mark the peer
   recovery-required, clear active link/handshake snapshots, and pause private
   automation.
4. Bind handshake and message paths to both peers' attempt IDs, ordered by Pubky
   identity. Snapshots retain this context. Following a remote attempt does not
   rotate the local ID, and repeated observations do not reset the connection.
5. Check markers during handshake startup, before and after advancement, and
   before send/receive or `ensure_link_with_peer` reuses a completed link.
   Validate snapshot attempt IDs against the tracked peer state before restore
   and in the handshake checkpoint transaction. Keep markers after completion;
   explicit removal is allowed only for blocked peers. Remote timestamps are not
   compared with local clocks.
6. Publish/remove markers under a renewable Pubky write lock, rechecking peer
   operation ownership after acquiring the lock.

Recovery does not delete old streams. Their paths cannot overlap a replacement
connection, including when an old write finishes late. Retiring these files is
separate from relinking; pending application events still require replay and
deduplication on the replacement link.

A marker read is not a lock on the counterparty. The remote peer can recover
after the last check, including just before or after a local checkpoint. The
next successful marker check observes the new attempt; late writes remain on
the retired stream. Link completion and message publication do not prove remote
receipt.

### Publish Private Payment List

1. Ensure matching live session access is available.
2. Ensure the counterparty has an active Encrypted Link snapshot.
3. Ask `PaymentAdapter` for Private Payment List reservations.
4. If reservations are returned, build the configured app's complete Private
   Payment List and
   persist the outbound record plus linked reservation records atomically.
5. If reservations are not returned, ask `PaymentAdapter` for private receiving
   details scoped to the counterparty and queue the list normally.
6. After acquiring the peer lease, cancel unpersisted adapter reservations when
   SDK-side validation or queueing fails.
7. Let the identity-wide outbound Private Application Message worker send
   through the Encrypted Link.
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

Receive routing should extend the same raw stream log to Payment Requests,
Payment Proofs, Allowance lifecycle events, and any other Event Message kinds.
Allowance lifecycle/history views derive from retained events on the exact
Encrypted Link and preserve invalid or unresolved evidence for recovery.

### Resolve Public Payment

`resolve_public_contact_payment` fetches the counterparty App Registry and the
public Payment Endpoints of every registered app, passes app-attributed
`PublicPaymentEndpointCandidate` values to the public adapter callback, and
returns `PublicContactPaymentResolution`. Invalid or unavailable endpoint data
from one app is reported as an app-specific failure while valid sibling-app
endpoints remain available. Resolution inspects every registered app while
bounding the aggregate to 256 endpoints and 4 MiB of payload data. An app whose
complete list cannot fit the remaining aggregate budget receives a structured
`ResourceLimit` failure. If no registered app can be loaded, the result is
`Unavailable`.
It does not inspect or mutate Linked Peer, Encrypted Link, or Private Payment
List state.

Payment Request resolution restricted to a `required_app_id` loads only that
app's public endpoints. Failure to load it returns `Unavailable`, regardless
of other apps' endpoints.

### Resolve Private Payment

`resolve_private_contact_payment` checks only Linked Peer and cached Private
Payment List state across source apps. When an active link exists and no cached endpoint is
available, it may try the private refresh/recovery path. It passes only
`PrivatePaymentEndpointCandidate` values to the private adapter callback and
returns `PrivateContactPaymentResolution`, including private state:

- `Available`
- `NoPrivateEndpoint`
- `RecoveryPending`

The private resolution input accepts an optional
`after_private_payment_list_version`. The private result carries the
`private_payment_list_version` from the same local Private Payment List
aggregated snapshot as its endpoints. When the available version is not newer than the
input version, resolution returns `WaitingForUpdatedPaymentList` with no
payable endpoints. These versions are opaque local freshness tokens scoped to
one SDK state and counterparty; they are not the
serialized Private Application Message schema version.

For a Payment Request restricted to one app, freshness is scoped to that app's
list. Updates from other apps do not advance the request's list version.

The application owns consumption. Submitting, pending, or uncertain payment
execution should persist the returned version as consumed before another
payment is resolved for that peer. Applications should serialize this handoff
per counterparty. Using one endpoint consumes every endpoint returned with
that version. A later resolution includes candidates only from application
lists updated after the consumed version; another application's unchanged
list does not become fresh merely because one application published an update.

`prepare_and_resolve_private_contact_payment` may first advance the Encrypted
Link and drain currently available private send/receive work, then invokes the
same private-only resolution with the optional consumed version. It never
falls back to public Payment Endpoints.

Payment Request-aware public and private resolution load the request amount
from durable derived state and filter candidates before adapter selection.
Only accepted Payment Endpoint Identifiers are eligible, and a non-null
`required_app_id` limits candidates to that payee App.

When request terms include `payment_endpoints`, private resolution uses those
immutable destinations instead of a Private Payment List. The result has no
list version, and a consumed-list version does not suppress it. Public
resolution returns no candidates for such requests. Apps must use the
request-aware resolver and retain their per-request payment execution guards;
they must not substitute a generic contact resolution when no specified endpoint
is payable.

Both public and private result statuses use `Payable`, `NoEndpoint`, and
`UnsupportedEndpoint`; private resolution additionally uses
`WaitingForUpdatedPaymentList`. Public and private statuses remain distinct
enum and result types. When a result is `Payable`, its ordered endpoints each
include an adapter-built `PaymentTarget`. The application explicitly chooses
the payment mode; the SDK does not combine candidates, results, or fallback
policy.

### Send Payment Request

Payee flow:

1. Build immutable Payment Request terms, optionally requiring one Paykit App
   to handle the payment.
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
7. Derive local state and expose valid requests as actionable. A non-null
   `required_app_id` limits resolution to Payment Endpoints owned by that
   payee App. Any compatible local payer App may claim execution.

### Claim And Respond To A Payment Request

Before preparing or executing a payment, a payer App claims the request in
identity-wide shared state. Exactly one App can hold the claim. The App queues
acceptance before it starts payment execution, and acceptance does not release
the claim.

`claim_and_accept_payment_request` combines the ownership check and acceptance
queue update atomically. Apps that claim a request before preparing payment keep
that earlier claim; the combined operation revalidates it. Success means the
acceptance is saved, not delivered, and does not authorize payment execution.

For outbound lifecycle events:

1. Load local Payment Request state.
2. Check whether the local role may send this event.
3. Check current lifecycle state.
4. Serialize and persist exact outbound payload.
5. Send and retry using the same Event ID and payload.

Cancellation is unilateral. Acceptance and rejection are payer-only. An open
one-time request may be explicitly released after acceptance while unresolved;
another compatible App can then claim it without sending another acceptance.
For an open recurring request, the claim covers the subscription and may be
released for another local payer App to handle future or unpaid periods. A
targeted request remains limited to Payment Endpoints owned by its required
payee App after release. Claims are never released automatically because an
uncertain failure may have completed the payment.

One-time proof submission removes the execution claim. Recurring proof
submission retains it. A proof permanently completes its one-time request or
billing period; later Apps cannot submit another proof for that completed work.
Derived records include local outbound delivery status for queued lifecycle
events. Apps should not treat outbound status as counterparty acceptance or
settlement confirmation.

### Record Payment Proof

1. Load a known Payment Request with a valid prior Acceptance and a lifecycle
   state that permits a Payment Proof.
2. Ask `PaymentAdapter` for execution result or caller-supplied proof data,
   including the App ID whose endpoint was paid.
3. Validate stateless proof/request correlation through `paykit-lib`, including
   any required App ID from the request.
4. Persist proof event before sending.
5. Send Payment Proof.
6. Derive local state from the retained lifecycle events: preserve `canceled`
   after Cancellation, retain `active_recurring` for an ongoing Recurring Payment
   Request, and use `proof_submitted` for a non-canceled one-time request.

A proof after Cancellation may report only an execution the payer durably
recorded as past its irreversible boundary before observing Cancellation. It
does not reopen the request or authorize another payment. Apply the
[Payment Proof validation rules](payment-requests.md#paykitpayment_proof).
Reporting past execution does not require the payee's originating App to remain
registered. New claims and execution still require its current authorization.

The SDK should not mark a payment as settled unless the payment adapter provides
settlement confirmation.

### Issue Receipts

1. Prepare receipt issuance and persist the Encrypted Receipt payload plus exact
   Receipt Access JSON before network side effects.
2. Store the Encrypted Receipt at its Receipt Location on the issuer homeserver.
3. Queue Receipt Access through the normal outbound private message queue.
4. Retry from durable issuance state if storage or queueing fails.
5. Treat outbound status as local delivery checkpoint state, not counterparty
   acknowledgement.

Receipt IDs are unique per issuer identity and Receipt Location is derived from
the Receipt ID. `prepare_receipt_issuance` may generate a Receipt ID and return
it to the caller; retries should then call `process_receipt_issuance` with that
ID from the same shared SDK state. The
one-call `issue_receipt` helper requires the draft to already contain a
caller-provided Receipt ID so repeating the same call cannot create a second
receipt after a partial failure.

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

- public identity state
- peer records, including the highest verified signed Noise key authorizations
- Encrypted Link snapshots
- handshake snapshots
- Private Payment List cache
- endpoint publication records
- endpoint reservations
- Contact Records, including user labels, cached Paykit Profiles, public
  contact marker status/timestamps, and marker errors
- raw private stream log or checkpointed subset
- outbound queue
- recovery markers

For Allowance V1, backups must also retain lifecycle evidence together with
association/decision history and selection revisions, semantic payment keys,
usage reservations, outcome history, evaluation-time watermarks, and accounting
recovery state.

If the authoritative SDK state and every backup copy are lost, the SDK cannot
safely reconstruct private runtime state from encrypted message slots alone.
Public Payment Endpoints and Paykit Profiles can be rediscovered, but Encrypted
Link snapshots/counters, private stream history, Event Message dedupe records,
Receipt Access keys, outbound queues, Contact Records, and Payment
Request/Allowance/Receipt history require the durable shared state. Recovery without it
means fresh initialization, republishing public state, relinking peers, and
receiving fresh private data from counterparties.

Backup export is an optional portable snapshot of the same logical SDK state.
It is not the live cross-app synchronization mechanism, and sign-out does not
delete either the shared state or caller-managed backup copies.

rc59 persisted integration data is unsupported in rc60: there is no in-place
upgrade or migration for SDK state blobs, Pubky shared-state payloads, Rust or
FFI backups (including encoded backup strings), or FFI state-blob snapshots.
rc60 testing requires fresh/reset Paykit state, not importing rc59 data. A
decode failure must not silently reset state. Resetting cannot recover private
history or establish accounting freshness. Recovery is not an rc59 migration.

Allowance lifecycle and accounting evidence is also private SDK-managed state.
Relinking or receiving fresh lifecycle messages cannot restore missing selection
or usage history. Allowance V1 requires wallet reconciliation of successful and
unresolved payments before admission can resume; missing accounting must not be
reconstructed as zero usage from lifecycle messages or Payment Proofs.

Backup should not include:

- app cloud transport details
- product-specific profile/contact data outside SDK Contact Records
- payment-provider secrets unless explicitly provided by the payment adapter
- wallet seed material

Normal restore flow:

1. Require an otherwise empty SDK state backing, then validate the local
   identity. A portable backup must not replace newer shared state.
2. Validate backup record shape plus every link snapshot recipient.
3. Preserve valid active Encrypted Link snapshots and in-progress handshake
   snapshots so the SDK can catch up from the restored checkpoint.
4. Mark peers recovery-required only when no safe restored checkpoint exists,
   the peer was already recovery-required, or outbound private work needs a link
   snapshot that is missing.
5. Load backup into storage under a restore transaction.
6. Do not execute automatic payments until private stream and request state are
   consistent.
7. Republish participating App Registry entries before those apps create new
   app-attributed work.

For Allowance V1, restored or recovery-incomplete execution state must remain
ineligible until wallet reconciliation establishes that no later successful or
unresolved payment is missing.

Backup restore preserves history, derived records, and peer authorization pins.
Checkpoints resume only after [current signed-key checks](#establish-encrypted-link);
missing or unsafe checkpoints pause private automation until relink. Concurrent
multi-app updates use homeserver-enforced write locks and crash-safe
prepared Noise operations.

`recover_shared_state_from_backup` requires a matching trusted backup, an active
`PAYKIT_AUTHORIZER_SESSION_CAPABILITIES` session, the Pubky identity secret,
the current Paykit key, its next-generation successor derived from that secret,
and an existing protected signed authorization.

Before replacement, recovery verifies the record's signature, owner, routing/static
keys, and generation against the current key or the exact replacement on retry.
Missing, malformed, or conflicting authorization fails before state replacement.
It cannot bootstrap missing authority; normal publication requires readable SDK state.

Under the shared-state lock, recovery checks App Registry key/generation
consistency, not key authority, and rejects healthy current state or unknown
generations. It commits restored state encrypted with the replacement key, then
updates the App Registry, then publishes replacement signed authorization.
Errors do not roll back commits. Retry with the exact same keys and backup;
valid replacement state, including later app progress, survives publication retries.
Only absent state or corrupt state with a readable current-generation header can
be replaced, while the registry still identifies the current key. Unreadable headers
and corrupt replacement-generation state are rejected to prevent Noise state reuse.
Recovery discards Noise checkpoints and prepared sends, preserves backup history,
and requires relinking and wallet reconciliation before execution. State newer
than the backup cannot be reconstructed. Callers must securely persist the
replacement key before recovery and distribute it to authorized apps afterward.

## Public SDK API Shape

The Rust SDK exposes initialization, identity status, public endpoint
sync, linked peer handshakes, private stream receive, Private Payment List
derivation/publication, Paykit Profile publication/fetching, read-only Pubky
profile/follows helpers, Contact Record CRUD/profile refresh, contact
payment resolution, outbound Private
Application Message processing, Receipt Access indexing/retrieval, Payment
Request lifecycle derivation plus checked outbound lifecycle queueing, optional
Payment Endpoint Reservation records, and backup/export/restore for SDK-managed
state. Use `paykit-sdk` rustdoc for exact signatures.

The main public method families cover initialization/sign-out, session
bootstrap, public endpoint sync, profile and blob helpers, Contact Records,
Pubky profile/follows reads, contact payment resolution, linked peer setup,
private stream receive, outbound private delivery, Private Payment Lists,
Payment Requests, Receipts, and SDK backup/export/restore.

Allowance V1 requires SDK API families for lifecycle proposal,
inspection, acceptance, rejection and End; candidate evaluation and persisted
selection; durable manual/automatic admission and outcome reporting; and
selection/usage history, backup and reconciliation. The SDK coordinates this
evidence while the wallet owns consent, candidate priority, local safeguards,
execution and settlement validation. See
[component responsibilities](allowances.md#component-responsibilities).

`ReceiptDraftBuilder` is the ergonomic way to create `ReceiptDraft` values for
SDK calls. It can generate a Receipt ID before `issue_receipt`, or leave it
empty when callers want the two-step `prepare_receipt_issuance` flow to create
and return the ID first.

## Platform Binding Shape

The workspace exposes first-class Swift and Kotlin SDK bindings through
`paykit-ffi`. See [paykit-sdk-bindings.md](paykit-sdk-bindings.md) for the
binding-specific API plan.

Recommended approach:

- Rust SDK crate owns the runtime and core state machine.
- FFI crate exposes an opaque SDK handle.
- Platform wrappers provide ergonomic Swift/Kotlin APIs.
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

Allowance V1 also requires binding APIs for lifecycle operations, candidate
evaluation and selection, durable admission and outcome reporting, history
inspection, and accounting recovery. These surfaces follow the SDK
responsibilities above; they do not move payment execution into the bindings.

Bindings should not expose:

- raw Receipt Decryption Keys in debug output
- unredacted raw private messages in logs
- implicit event-loss-prone typed getters that bypass durable storage

## Error Model

SDK errors should be structured:

- `ConcurrentUpdate`: a storage revision changed or a required lock was busy;
  reread affected state before retrying, accounting for earlier committed steps
- `SharedStateBusy`: the shared-state lock is busy with an unconfirmed write;
  back off for recovery rather than immediately retrying, including for reads
- `Storage`: durable storage failure
- `Identity`: Pubky session/key/capability failure
- `Transport`: Pubky or Encrypted Link transport failure
- `NotFound`: required local or Pubky resource is missing
- `Protocol`: invalid Paykit message, conflict, or unsupported version
- `Policy`: operation blocked by configuration or privacy policy
- `PaymentAdapter`: payment adapter failure
- `RecoveryRequired`: local state is inconsistent and automatic execution is
  blocked until recovery completes

Errors should include machine-readable codes and short redacted context. UI copy
belongs to the app.

## Policy Configuration

`PaykitSdkConfig` includes:

- required local Paykit App ID
- public endpoint management scope
- public contact sharing policy, defaulting to private Contact Records

Additional policy configuration can add:

- stale private cache policy
- outbound retry policy beyond lease expiry
- unknown message retention policy
- receipt auto-retrieval policy
- recurring Payment Request scheduling policy
- endpoint reservation policy
- log redaction level

## What Moves From Existing App/Core Integrations

These are good candidates to move into Paykit SDK:

- Pubky identity and live-session tracking for Paykit workflows
- public Payment Endpoint sync and stale endpoint cleanup
- Private Payment List publish/fetch/cache
- Encrypted Link snapshot and handshake runtime
- stale link recovery markers and private refresh/recovery policy
- ordered private stream receive/persist/route
- Paykit profile publishing/fetching and Contact Records
- contact payment resolution and payable endpoint checks
- endpoint reservation records and attribution helpers
- SDK-managed backup records
- Receipt Access indexing and retrieval helpers
- Payment Request lifecycle state
- Allowance lifecycle derivation and durable selection/admission evidence
- Allowance usage accounting, outcome history, watermarks, and recovery

These should stay outside Paykit SDK:

- payment-provider node/runtime state
- receiving-detail generation internals
- payment execution and settlement detection
- Allowance consent, candidate priority, local safeguards, and payment scheduling
- balances, fees, quotes, and route policy
- product profile/contact UI
- localized copy and navigation
- app backup transport and cloud sync
- payment-provider seed/secret derivation policy
- payment execution ownership and product-level app selection policy

## Test Plan

Core tests:

- storage transaction commits and rollbacks
- receive persists raw messages before snapshot
- replay after crash dedupes events
- conflicting Event IDs fail closed
- malformed recognized messages remain auditable
- unknown valid Private Application Messages are retained
- Private Payment List latest valid message wins per source App ID
- stale private link reports recovery or uses public endpoints only when the
  resolution request includes them
- private-capable session access requires current Paykit identity key material
- key rotation preserves durable history while clearing old Encrypted Link
  snapshots and rejecting wrong-generation shared state
- missing live session access blocks private operations without clearing cached
  private state
- outbound retries reuse exact Event ID and payload
- Payment Request role/lifecycle checks
- Receipt Access dedupe and receipt retrieval
- profile serialization and Contact Record storage
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
- Public contact marker discovery and richer contact-sharing policy.
- App-level policies for when payment resolution should include public Payment
  Endpoints.
- Reservation lifecycle hooks beyond Private Payment List queueing.
- Recurring Payment Request scheduling ownership between the SDK and
  app/runtime schedulers.
- Unknown Private Application Message retention defaults for mobile storage
  budgets.
- higher-level platform wrapper/package policy on top of the SDK bindings
- Multi-device synchronization and recovery policy on top of the
  identity-wide shared-state resource.
- Revocable Pubky credentials and secure Paykit key distribution across
  authorized applications and devices.

## Durable Allowance payment accounting

The runtime exposes one wallet-coordinated ledger through
`allowance_accounting_state` and `reconcile_allowance_accounting`. A missing
ledger does not mean zero usage: both automatic and manual admission require
explicit complete wallet reconciliation first. The wallet must reconcile its
external execution/idempotency records and every payment path. Empty recovered
history attests no prior payments when initializing; it never clears existing
evidence. Only explicit outcome reports resolve already retained uncertainty.
Payment Proofs and Receipts never construct, commit, or release this accounting.

`evaluate_allowance_candidates` returns advisory static/time/lifecycle results.
`select_allowance` persists exactly one choice. The atomic
`accept_payment_request_automatically` operation persists that choice and queues
ordinary Acceptance after current wallet preflight checks. For one-time requests,
current shared capacity is checked before queuing Acceptance; insufficient
capacity leaves the request proposed with its ordinary manual response flow.
Recurring Acceptance defers capacity checks until a payment is due. Acceptance
consumes no capacity, and reservation always rechecks it atomically. Existing
manual Acceptance, Rejection, and Cancellation operations
serialize with the same ledger and recheck current request state before queuing.
Automatic Acceptance requires the current app's execution claim, as manual
Acceptance does.
A manual response excludes conflicting automatic handling; it can revoke an
unissued preparation but never releases a Submitted or Unknown attempt.

`reserve_automatic_payment` and `reserve_manual_payment` share one transaction
key: actual local Pubky key, counterparty Pubky key, canonical Payment Request ID,
and normalized UTC Billing Period instants when recurring. App ID and Allowance
ID are excluded from this key. Automatic reservation reruns shared exact decimal,
period, lifetime, and trusted-time checks using
complete successful and unresolved automatic usage. Manual payments consume no
Allowance capacity but exclude automatic payment of the same occurrence.
Automatic execution requires the actual asset and amount to equal the requested
Payment Amount and excludes explicitly priced endpoints. Manual conversion or
explicitly priced payments, including fixed same-asset rail prices, rely on
`local_enabled` attesting the wallet's independent validation of the actual amount
against the conversion policy, required quote, endpoint precision, upward
rounding, and payment deadlines. The wallet retains its selected quote separately.
Unpriced same-asset manual payments require the requested amount.
Manual attempts retain the verified actual Payment Amount;
handoff must use that same asset and numerically equal amount. Reconciliation
must preserve this actual amount as immutable execution evidence.
The trusted-time watermark advances even when evaluation is blocked.

A Ready reservation has status Prepared and grants no execution authority.
Immediately before execution the wallet calls `begin_payment_execution` with
fresh endpoint, actual amount, scheduling, local enablement, and private checks.
Only Ready with status Submitted authorizes a wallet handoff. The SDK rechecks
current lifecycle and the effective association revision in that transaction.
Reservation and handoff require the current app's execution claim and outgoing
payment capability. An unresolved attempt prevents releasing that claim.
The wallet must use the returned attempt ID for external idempotency. All apps
coordinate admission through the same identity-wide ledger. SDK storage and
external settlement cannot commit atomically: a crash after handoff issuance requires external
reconciliation, never a second execution or timeout-based release. The wallet
reports definitive pre-settlement failure, verified success, or Unknown through
`record_payment_outcome`; verified success cannot be undone by refunds or replay.
When a pre-handoff check blocks, the wallet must abort execution and report
verified failure before releasing or replacing its reservation.

`defer_payment_occurrence` records a temporary endpoint or policy failure for
later full reconsideration. It cannot relabel unresolved execution as retryable.
`mark_payment_manual_only` is sticky. Explicit future-only
`authorize_allowance_reassociation` appends a user authorization and revision;
the accepted replacement need not be active yet. Authorization validates static
matching and the current trusted-time watermark; admission and handoff still
require current eligibility. Old usage and attempts remain on the original
Allowance, earlier occurrences and other requests remain independent, and
manual-only decisions survive. Background
matching cannot change the persisted choice or provide this user authorization.

The current SDK backup schema and both platform storage envelopes remain
version 1; see [Backup And Restore](#backup-and-restore) for supported persisted formats.
Restore rejects a backup that would discard or change retained Allowance or
Payment Request lifecycle or conflict evidence for the same payer identity. Rejection
leaves all current state unchanged; use a backup retaining that history.
This includes Payment Request Cancellation, malformed lifecycle messages, and
Event ID conflicts with other message kinds. Payment-history reconciliation
cannot restore forgotten lifecycle authority or make a canceled request payable.
Restore requires an empty destination, apart from initialized identity metadata;
it never replaces live shared state. Restored accounting invalidates prepared
handoffs and blocks new admission until complete wallet reconciliation. Paykit key rotation retains
payment history and imposes the same recovery block. Sign-out and local session
clearing leave the shared ledger intact. Switching identities accesses that
identity's separate state; missing history requires complete wallet
reconciliation before further admission. Restored identifiers must have canonical
lowercase UUID-v4 spelling, and malformed or inconsistent accounting fails closed.
