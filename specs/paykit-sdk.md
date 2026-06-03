# Paykit SDK Plan

Status: planning / discussion only
Date: 2026-06-02

## Goal

Define the future Paykit SDK layer that sits above Paykit Library.

Paykit Library remains the stateless Rust implementation of Paykit Protocol
wire formats, Pubky storage helpers, Encrypted Link transport helpers, and
structural validation. Paykit SDK is the higher-level integration layer that
wallets, payment processors, and apps can use when they need durable local
state, stream routing, recovery behavior, contact/payment workflows, and
ergonomic platform APIs.

The SDK should be product-neutral. This plan was informed by existing mobile
and core app integrations that already duplicate higher-level Pubky and Paykit
runtime behavior, but those integrations are examples rather than scope owners.

## Integration Inputs

The repeated integration patterns are:

- Pubky session import/export and restore
- Pubky profile and contact fetch/cache workflows
- public Payment Endpoint publishing and discovery
- private Payment List publication and resolution
- Encrypted Link snapshot storage and recovery
- receipt access indexing and receipt retrieval
- contact payment resolution across private and public Paykit data

Those reusable behaviors are good candidates for a shared SDK/runtime instead
of remaining duplicated in each app.

## Why This Layer Exists

Private Application Messages share one ordered Encrypted Link stream. The
low-level library should expose that stream without hiding messages in internal
buffers or making lifecycle decisions. A wallet or processor still needs a
durable runtime that can:

- persist received Private Application Messages before advancing its local checkpoint
- route messages into protocol lanes
- derive local Payment Request state
- dedupe replays
- recover after crashes or missing snapshots
- manage Encrypted Link snapshots and recovery attempts
- publish and refresh public and private Payment Lists
- resolve contact Payment Endpoints
- expose convenient app-facing getters without losing unrelated message kinds

Those responsibilities are stateful and belong above Paykit Library.

## Relationship To Paykit Components

### Paykit Library

Paykit Library should provide:

- Pubky Routing reads and writes
- Payment Endpoint validation and storage helpers
- Private Payment List serialization and parsing
- Payment Request, Receipt Access, Receipt, and Payment Proof wire types
- Encrypted Link handshake, restore, send, receive, and snapshot helpers
- full Private Application Message stream receive API
- stateless parsers for known Private Application Message kinds
- stateless structural validation
- send helpers for known protocol messages

Paykit Library should not own:

- durable event history
- local database schema
- payer/payee lifecycle state
- contact address books
- profile storage schemas
- payment-provider balances
- payment execution
- recurring payment scheduling
- UI state
- notification policy
- retry queues beyond narrow transport helper behavior

### Paykit SDK

Paykit SDK should provide:

- durable Encrypted Link state storage
- durable Private Application Message event log
- crash-safe receive/checkpoint flow
- typed lane routing over the raw Private Application Message stream
- SDK-level typed getters that preserve unrelated message lanes
- Payment Request lifecycle state
- outbound Event Message preparation, persistence, and retry
- receipt access indexing and receipt retrieval helpers
- public Payment Endpoint publishing and cleanup workflows
- Private Payment List publishing, refresh, and latest-state indexing
- contact payment resolution across private and public Paykit data
- Pubky identity/profile/contact adapter integration needed for Paykit workflows
- recovery behavior for stale or lost Encrypted Link state
- optional scheduling hooks for recurring Payment Requests
- payment adapter interfaces
- platform bindings for app integrations

## Layering

The SDK plan should keep three layers separate:

- Pubky identity layer: generic Pubky session, Ring auth, profile/contact/follow,
  file fetch, and key helpers. These may come from a Pubky SDK or from the app.
  Paykit SDK should use this layer for Paykit workflows, but should not own the
  full Pubky profile/contact product model.
- Paykit SDK runtime: Payment Endpoint publication, Private Payment Lists,
  Encrypted Link state, private stream routing, contact payment resolution,
  backup records, Receipts, and Payment Requests.
- Payment adapter: receiving-detail generation, method/provider state,
  payment execution, activity records, feature flags, and UI.

## Candidate SDK Scope

### Pubky Session And Auth Runtime

Existing platform integrations wrap Paykit session management and Pubky auth
helpers in app services. The SDK should consume a Pubky identity layer or
caller-provided identity adapter, then expose the Paykit-facing runtime shape:

- initialize Paykit and Pubky-backed runtime state
- import/export Pubky session secrets
- sign out and force local sign out
- report current Pubky public key and auth state
- report whether the session is public-only or private-link-capable
- expose whether a local Pubky secret key is available for Encrypted Links
- restore a saved session on app start
- refresh a session when a locally managed Pubky secret key is available
- avoid persisting stale session secrets until import succeeds
- provide a serial execution boundary around stateful FFI calls where platform
  bindings require it

The SDK may provide optional Pubky Ring auth helpers:

- start/cancel/complete a `pubkyauth://` relay flow
- parse auth URLs for display
- approve auth URLs when the app has the local Pubky secret key
- expose nonce/callback correlation helpers for apps that use callback URLs

The SDK should not force a specific identity provider. Apps should be able to
bring a session secret, a Pubky keypair, or an external authenticator.

A Ring-authenticated session may be valid for public Paykit operations while
lacking the local secret key needed for Encrypted Links. That capability
distinction should be first-class so apps can report "public Paykit only"
instead of retrying private flows that cannot succeed.

### Pubky Key Material

Some integrations derive Pubky Ed25519 secret keys from seed material. This can
be useful, but it is sensitive and product-specific.

The SDK should define the identity input boundary:

- accept an imported Pubky session secret
- accept a caller-managed Pubky secret key when needed for Encrypted Links
- validate that a stored secret key matches the active Pubky public key
- expose backup/restore metadata for locally managed versus externally managed
  sessions

The SDK should not silently derive identity keys from seed material by default.
Identity key derivation policy belongs to the integrating app unless Paykit
explicitly standardizes it as a protocol/product requirement. If the helper
remains useful, it should be an opt-in identity adapter, not the default SDK
behavior.

### Pubky Profiles

Existing app integrations duplicate profile fetch, profile write, cached
display metadata, avatar upload, and fallback behavior. The SDK should provide
adapter-backed helpers for the Paykit workflows that need profile context:

- fetch a Pubky profile by public key
- normalize and validate Pubky public keys
- support profile cache records for display name and image URI
- resolve an app profile first and fall back to `pubky.app` profile data when
  configured
- write a caller-provided profile payload to a configured Pubky path
- upload/fetch public profile blobs when configured with a storage path policy
- clear profile cache on sign out

Core Pubky profile behavior may belong in a Pubky SDK. Paykit SDK should keep
profile paths and profile schema configurable. Product profile paths, staging
paths, avatar compression, and signup flows stay product policy unless Paykit
adopts them explicitly.

### Pubky Contacts

App contact systems can combine public Pubky follows, app-owned contact
storage, profile snapshots, and Paykit private sharing state. The SDK should
provide adapter-backed helpers for Paykit workflows that need contact context:

- normalize, validate, compare, and redact Pubky public keys
- reject self-contact and duplicate contact additions
- list contacts from a configured Pubky contacts path
- discover remote `pubky.app` follows for import
- fetch contact profiles concurrently with placeholder fallback policy
- save, update, and remove contact profile snapshots at a configured path
- expose contact import candidates and merge decisions
- notify Paykit private state when saved contacts are added, removed, or pruned

Core Pubky follow/contact behavior may belong in a Pubky SDK. Paykit SDK should
not own contact UI, grouping/sorting presentation, localized validation
messages, route decisions, or onboarding screens.

### Pubky File And Image Resolution

Platform integrations often fetch `pubky://` resources and follow simple image
descriptor indirection. The SDK may provide reusable file helpers when Paykit
workflows need them:

- resolve or fetch `pubky://` resources
- fetch UTF-8 text or raw bytes
- optionally follow profile/image descriptor JSON with a `src` field
- expose cache keys or cache invalidation signals for profile media

General `pubky://` file fetching may also live in a Pubky SDK. Paykit SDK
should not own platform image rendering, Coil/SwiftUI image loaders,
memory/disk cache sizing, image clipping, or loading/error UI.

### Public Payment Endpoint Runtime

Apps, wallets, and payment processors build and publish public Payment
Endpoints from payment-adapter state. The SDK should own the reusable Paykit
workflow:

- parse Payment Endpoint payloads
- serialize endpoint payloads from adapter-provided receiving details
- validate Payment Endpoint Identifiers and configured supported methods
- determine the current desired public Payment List from adapter capabilities
- publish desired endpoints and remove stale managed endpoints
- remove all managed endpoints when public sharing is disabled
- fetch a counterparty's public Payment List
- filter endpoints through an adapter-provided compatibility checker
- build a contact payment launch target from compatible endpoints
- build method-specific payment launch data when the payment adapter supports it
- refresh expiring Payment Endpoint Payloads before publication
- clear cached endpoint metadata after a payment settles

The SDK should not generate payment receiving details directly. It should ask a
payment adapter for receiving details, network or method metadata,
payment-method availability, and endpoint usability.

### Contact Payment Resolution

Apps often need to answer questions like "can this contact be paid?" and
"start a payment to this contact" by trying counterparty-specific private
Payment Lists first and then public Payment Lists. The SDK should provide this
as a shared workflow:

- resolve a contact's current private Payment List when a Linked Peer exists
- use cached private endpoints when a link is stale but cached data is still
  acceptable under configured policy
- wait for private recovery only for a configured bounded recovery window
- fall back to public Payment Endpoints when a counterparty-specific private
  Payment List is unavailable or disabled
- return a structured result: opened payment target, no endpoint, unsupported
  endpoint, stale/recovery pending, or error
- expose "has payable endpoint" checks for contact lists and previews

The SDK should let apps configure fallback policy. Some products may prefer
private-only behavior for saved contacts; others may allow public fallback.
When private recovery does not complete inside the configured window, the SDK
should return either public fallback or a structured stale/recovery result
instead of blocking payment resolution indefinitely.

### Encrypted Link Runtime

App integrations currently have to manage Encrypted Link handles, snapshots,
handshake snapshots, restore/retry behavior, and recovery markers. The SDK
should own this runtime:

- decide deterministic initiator/responder role from the two public keys
- establish or restore Encrypted Links for Linked Peers
- persist Encrypted Link snapshots after successful receive/send progress
- persist handshake snapshots while a handshake is incomplete
- validate snapshot recipient before restore
- close/drop stale active handles
- count stale link failures and fail closed after a configured threshold
- restart handshakes only when link/session context is truly lost
- keep link recovery state per counterparty
- publish and read recovery markers when recovery is needed
- avoid broad private storage purge unless the policy proves it is safe for the
  current contact set

This should be implemented above Paykit Library because it is stateful and
requires local durable storage.

### Durable Private Stream Intake

The SDK should use `EncryptedLink::receive_private_application_messages` as the
durable receive path.

This requires a `StorageAdapter` with transaction semantics, or an explicit
crash-recovery contract that provides equivalent checkpoint safety.

For each receive cycle:

1. Receive the full Private Application Message batch from Paykit Library.
2. Persist each received private stream item whose plaintext is syntactically
   valid JSON, with stream order, raw JSON payload, parsed kind when available,
   parse result, and current link identity.
3. Persist dedupe/index updates derived from those messages.
4. Persist the advanced Encrypted Link snapshot only after the messages and
   derived state are durable, preferably in the same transaction/checkpoint
   boundary.
5. Expose typed views/getters from the SDK's own durable store.

If the SDK persists messages but crashes before persisting the advanced
snapshot, replay is acceptable and should be deduped. If the SDK persists the
snapshot without persisting messages, it may lose events, so this order is not
allowed.

### Stream Routing

The SDK should route Private Application Messages by Private Message Kind.

Initial lanes:

- `paykit.private_payment_list`: Latest-State Message lane
- `paykit.receipt_access`: Event Message lane
- `paykit.payment_request`: Payment Request Event Message lane
- `paykit.payment_request_acceptance`: Payment Request Event Message lane
- `paykit.payment_request_rejection`: Payment Request Event Message lane
- `paykit.payment_request_cancellation`: Payment Request Event Message lane
- `paykit.payment_proof`: Payment Request Event Message lane

Unknown private stream items with valid Private Application Message headers but
unsupported kinds should be retained in the durable log unless a caller
explicitly configures a discard policy. This keeps future protocol messages
recoverable after an SDK upgrade.

### Private Payment List Runtime

Apps that support private sharing publish private endpoint maps per saved
contact and keep the newest remote endpoint map cached. The SDK should own:

- building a complete Private Payment List from adapter-provided endpoints
- enforcing pubky-noise message size limits before send
- dropping lower-priority endpoints under a configured policy when the payload
  is too large
- hashing the local payload to skip redundant sends
- publishing removal tombstones or empty lists when private sharing is disabled
  or a contact is removed
- caching the latest valid remote Private Payment List per Linked Peer
- preserving older raw messages in the durable log for audit/debug
- making clear that Private Payment Lists publish endpoints only; Payment
  References come from Payment Requests, Payment Proofs, and Receipts

The SDK should not reintroduce the removed `reference` field or any
product-specific Private Payment List payload shape.

### Single-Use Endpoint Reservation

Some payment methods use contact-scoped or single-use receiving details. This
can be reusable, but it must be adapter-driven.

The SDK should provide an optional endpoint reservation subsystem:

- reserve an adapter-provided receiving detail for a contact
- remember current and historical contact assignments for attribution
- prevent reserved receiving details from being reused as general receive
  details
- restore reservation ceilings from backup so restored integrations do not
  reuse old reserved identifiers
- rotate contact-specific receiving details after use
- reconcile reserved identifier ceilings with the payment adapter
- map method-specific settlement identifiers back to a contact when possible

The SDK should not own receiving-detail generation, payment-provider state,
method-specific routing or quote policy, or settlement detection directly.
Those are payment adapter responsibilities.

### Payment Request Runtime

The SDK should derive Payment Request state from persisted Event Messages.

For outbound Payment Request events, the SDK should serialize and persist the
exact event payload before sending so retries can reuse the same Event ID and
payload.

It should handle:

- request proposal storage
- explicit acceptance
- rejection
- unilateral cancellation
- Payment Proof indexing
- dedupe by Event ID
- detection of conflicting reused Event IDs
- detection of conflicting repeated Payment Request terms
- proof/request correlation validation
- sender role validation
- proposal expiry handling before acceptance
- recurring request schedule eligibility
- pause of automatic execution when local history is missing or inconsistent

Paykit Library can validate event shape and stateless proof/request correlation,
but the SDK decides whether an event is allowed in the current lifecycle state.

### Receipt Runtime

The SDK should treat Receipt Access as an Event Message lane.

It should:

- persist every valid Receipt Access message in send order
- reconcile repeated Receipt IDs
- fetch Encrypted Receipts from Receipt Location, paired with the Receipt Access
  sender/issuer context, when requested or configured
- decrypt Receipts with Receipt Decryption Key
- index Receipts by Receipt ID, Payment Reference, counterparty,
  Payment Request ID, and Billing Period when available
- retry sending Receipt Access when storing the Receipt succeeded but sending
  the access message failed
- avoid logging raw Receipt Decryption Keys

### Backup And Restore

Apps may need to back up Pubky session state, private link snapshots, private
contact endpoint cache, recovery markers, and reserved endpoint metadata. The
SDK should make this first-class:

- export SDK-managed non-payment-provider state in versioned backup records
- restore SDK state without assuming the active payment-provider session is
  already valid
- validate restored Encrypted Link and handshake snapshots against the expected
  counterparty before use
- separate secret state from non-secret cache state where platform storage
  requires it
- emit "backup state changed" notifications for app backup systems
- fail closed when restored state is inconsistent

The SDK should not own the app's backup transport, cloud provider, encryption
container, or seed/secret backup policy.

### Activity And Contact Attribution

Integrations may annotate activity with Pubky contact keys and use private
endpoint reservations to map incoming payments back to contacts. The SDK should
own only the reusable correlation helpers:

- normalize contact public keys for activity metadata
- map known private settlement identifiers to contacts
- map reserved private receiving details to contacts
- expose correlation metadata that an app can store on activity records

The SDK should not own the app activity database, transaction replacement
policy, activity screens, or display copy.

## Clearly Out Of Scope

The following should stay in a payment adapter, product app, or integrating app:

- payment execution and settlement detection
- payment-provider lifecycle, balances, fee/quote policy, route policy, and
  method-specific state
- method-specific payment payload or URI parsing beyond adapter-provided
  compatibility checks
- receiving-detail generation and identifier ownership
- reusable receiving-detail caching outside Paykit contact reservations
- selected payment-method UI policy
- fiat display, exchange-rate handling, and amount-entry UX
- product feature flags and onboarding screens
- settings screens and localized error messages
- navigation, QR scanning, clipboard handling, share sheets, and toasts
- profile/contact list presentation, grouping, sorting UI, and search UI
- avatar compression, platform image rendering, and cache sizing
- signup policy unless Paykit standardizes it
- identity key derivation policy unless Paykit standardizes it
- app backup transport, cloud sync, and encrypted backup containers
- destructive storage purge policy that is not proven safe for all current
  Linked Peers

## Suggested SDK Adapter Boundary

The first SDK should be usable by wallets, payment processors, and apps without
importing product-specific types. It should depend on small interfaces supplied
by the integrating app.

Suggested adapters:

- `StorageAdapter`: durable local storage with transaction/checkpoint semantics
- `Clock`: current time for expiry, retry, and schedule decisions
- `PubkyIdentityAdapter`: active session, public key, local secret key
  availability, public-only/private-link-capable state, session
  import/export/sign-out
- `PubkyProfileAdapter`: optional app profile paths and profile serialization
- `PaymentEndpointProvider`: current public/private receiving details
- `EndpointCompatibilityChecker`: validates whether an endpoint is payable by
  the payment adapter on the current network or method
- `EndpointReservationAdapter`: reserves, rotates, and checks single-use
  receiving details
- `PaymentExecutor`: optional execution hook for Payment Requests
- `Scheduler`: optional recurring Payment Request scheduling hook
- `Logger`: structured logging with redaction

## Suggested API Shape

This is illustrative only.

```rust
struct PaykitSdk {
    // storage, Pubky session access, clock, and payment adapter hooks
}

impl PaykitSdk {
    async fn initialize(&mut self) -> Result<InitializationReport>;
    async fn restore_session(&mut self, session_secret: String) -> Result<PublicKey>;
    async fn sign_out(&mut self) -> Result<()>;

    async fn load_own_profile(&mut self) -> Result<Option<ProfileRecord>>;
    async fn load_contacts(&mut self) -> Result<Vec<ContactRecord>>;
    async fn add_contact(&mut self, public_key: PublicKey) -> Result<ContactRecord>;
    async fn remove_contact(&mut self, public_key: PublicKey) -> Result<()>;

    async fn publish_public_payment_endpoints(&mut self) -> Result<EndpointSyncReport>;
    async fn clear_public_payment_endpoints(&mut self) -> Result<EndpointSyncReport>;

    async fn sync_counterparty(&mut self, counterparty: PublicKey) -> Result<SyncReport>;
    async fn prepare_saved_contacts(&mut self, contacts: Vec<PublicKey>) -> Result<SyncReport>;

    async fn resolve_contact_payment(
        &mut self,
        counterparty: PublicKey,
    ) -> Result<ContactPaymentResolution>;

    async fn get_current_private_payment_list(
        &self,
        counterparty: PublicKey,
    ) -> Result<Option<PrivatePaymentList>>;

    async fn list_receipt_access(
        &self,
        counterparty: PublicKey,
    ) -> Result<Vec<ReceiptAccessRecord>>;

    async fn list_payment_requests(
        &self,
        counterparty: PublicKey,
        filter: PaymentRequestFilter,
    ) -> Result<Vec<PaymentRequestRecord>>;

    async fn accept_payment_request(
        &mut self,
        payment_request_id: PaymentRequestId,
    ) -> Result<()>;

    async fn reject_payment_request(
        &mut self,
        payment_request_id: PaymentRequestId,
        reason: Option<String>,
    ) -> Result<()>;

    async fn cancel_payment_request(
        &mut self,
        payment_request_id: PaymentRequestId,
        reason: Option<String>,
    ) -> Result<()>;

    async fn record_payment_proof(
        &mut self,
        payment_request_id: PaymentRequestId,
        proof: PaymentProofInput,
    ) -> Result<()>;

    async fn export_backup_state(&self) -> Result<Option<SdkBackupState>>;
    async fn restore_backup_state(&mut self, backup: Option<SdkBackupState>) -> Result<()>;
}
```

## Storage Model

The SDK storage model should be implementation-specific, but it should be able
to represent:

- local Pubky identity state
- session backup state
- profile cache records
- contact records and contact profile snapshots
- imported-contact candidates
- public endpoint publication records
- counterparties / Linked Peers
- Encrypted Link snapshots
- Encrypted Link handshake snapshots
- recovery markers and recovery attempts
- raw Private Application Messages
- parsed private event records
- current Private Payment List view
- private endpoint publication records and payload hashes
- endpoint reservation records and historical assignments
- outbound message queue and retry state
- Payment Request records
- Payment Proof records
- Receipt Access records
- Receipt retrieval/decryption status
- recovery/fail-closed status

The raw Private Application Message table/log is the source of truth for
derived protocol state. Derived views can be rebuilt from it.

Storage implementations should be able to commit raw messages, derived
indexes/dedupe state, and advanced Encrypted Link snapshots atomically, or
provide recovery semantics that prevent the snapshot from advancing durably
ahead of persisted messages.

## Open Questions

1. Which storage interface should the first SDK target: embedded database,
   caller-provided trait, or both?
2. Should Paykit standardize seed-derived Pubky keys, or should that remain
   entirely app policy?
3. Which profile/contact paths should be default versus caller-configured?
4. Should contact payment resolution default to private-first with bounded
   recovery and public fallback, or should fallback be app-configured?
5. How much of endpoint reservation should be generalized now versus left as a
   payment adapter feature until more integrations need it?
6. How should the SDK limit stored unknown Private Application Messages, so
   future protocol messages are not lost but local storage cannot grow forever?
7. How much scheduling should the SDK own for recurring Payment Requests versus
   delegating to app/runtime schedulers?
