# Paykit SDK

Stateful Rust runtime for Paykit integrations.

`paykit-sdk` builds on `paykit-lib` and owns durable Paykit state for Pubky
identity status, public Payment Endpoint sync, Encrypted Link state, private
stream intake, Private Payment List derivation, contact payment resolution, and
outbound Private Application Message delivery. It also derives Payment Request
state, indexes Receipt Access events, retrieves/decrypts Encrypted Receipts,
tracks local receipt issuance, tracks optional Payment Endpoint Reservations,
manages Paykit-facing profile and Contact Records, and exports/restores
SDK-managed backup state.

This crate exposes the Rust SDK API. The workspace also ships Swift and Kotlin
SDK bindings through `paykit-ffi`, which should be the primary mobile/app
integration surface.

Payment execution, settlement detection, balances, route policy, product UI,
and app backup transport stay outside the SDK and are provided by application
or payment-adapter code.

Paykit private communication is identity-wide. Apps sharing one Pubky identity
use one logical set of Encrypted Links, private stream checkpoints, outbound
queues, requests, receipts, recovery state, and backup data. Each SDK handle is
configured with a `PaykitAppId` so public Payment Endpoints and private messages
remain attributable to the app that owns or produced them; the App ID does not
create a separate private channel.

The public Paykit App Registry lists the apps participating in an identity,
their coarse capabilities, the identity-wide Noise public key, and optional
default-app preferences. Public Payment Endpoints remain app-scoped under the
identity. Private Payment Lists use latest-state semantics per app and are
aggregated across apps when resolving a private payment.

Public-only apps may publish registry entries and public Payment Endpoints
before the identity-wide Noise key is initialized. Private capabilities require
identity-wide Paykit key material and a signed Noise key authorization.

### Noise Key Authorization

Before private app publication or delegation, the identity authorizer calls
`publish_paykit_noise_key_authorization`. It needs the current Paykit key,
the Pubky identity secret, and `PAYKIT_AUTHORIZER_SESSION_CAPABILITIES`:
`/pub/paykit/:rw,/pub/paykit-authority/v0/current-key.json:rw`.
Ordinary apps receive only the Paykit secret and `/pub/paykit/:rw`; they must
never receive write access to the authority path.

Encrypted Links trust the identity-signed routing/static keys and generation,
not unsigned App Registry key fields. The SDK pins peer authorizations in shared
state and backups and checks authenticated static keys before completed or
restored links carry messages. Missing, malformed, or conflicting authorization
fails closed. See the [Shared Identity Model](../specs/paykit-sdk.md#shared-identity-model)
for publication, generation, and trust rules.

Multiple app processes using the same identity must also use the same durable
SDK state. The SDK ships `PubkySharedStateStorage`, which stores that logical
state as one encrypted Pubky resource. Bounded SDK operations reuse a renewable
WebDAV write lock and the latest decrypted state across their transactions.
Each changed transaction is durably saved before returning; an operation error
does not roll back earlier checkpoints. Locks are released between outbound
messages and before wallet callbacks. Separate local state blobs are only
suitable when one process owns the runtime. Private sends durably couple the exact prepared
ciphertext with the advanced Encrypted Link snapshot before publication.

## Current Scope

Implemented in this Rust SDK crate:

- SDK runtime facade and atomic storage adapter contract
- encrypted identity-wide SDK state stored in Pubky with locked updates
- Pubky session bootstrap helpers, identity status tracking, and sign-out handling
- request-bound application-defined companion claims for Pubky Auth
- public Payment Endpoint sync
- Encrypted Link setup, crash-safe private stream intake and outbound retries,
  and recovery marker workflows
- Private Payment List publication/cache and contact payment resolution
- Payment Endpoint Reservations for contact-scoped receiving details
- Payment Request and Allowance lifecycle derivation, Receipt Access indexing,
  receipt issuance, and receipt retrieval
- Paykit-facing profile/contact helpers and SDK backup/restore

Not implemented in this crate yet:

- first-party durable mobile storage helpers
- payment execution, settlement confirmation, balances, fees, or route policy
- product UI/profile screens, localization, and app backup transport
- recurring payment scheduling

The SDK derives and persists Recurring Payment Request lifecycle state, but the
integrating application owns scheduling, payment authorization, execution,
settlement validation, and service-access policy. See
[Recurring Payment Requests And Subscriptions](../specs/payment-requests.md#recurring-payment-requests-and-subscriptions).

## Integration Shape

Apps construct `PaykitSdk` with three pieces:

- a `StorageAdapter` that provides atomic transactions over the identity's
  shared logical state
- a `PubkySessionProvider` that returns live Pubky session access and clears it
  during sign-out
- a `PaymentAdapter` that supplies receiving details, endpoint selection, and
  payment-target construction

`PubkySharedStateStorage` is the first-party shared implementation. It derives
an encryption key from the identity-wide Paykit secret and stores the complete
encrypted state at `/pub/paykit/v0/shared-state.bin`. Initial key material can
be derived from the Pubky secret or supplied separately to delegated apps.
Signing out clears the app's session access but leaves the encrypted resource
intact.

The session provider is only the boundary for live Pubky access. It is not a
requirement to use Ring or another wallet as an identity coordinator before an
app can use Paykit. The app or binding layer decides how session material is
created, stored, unlocked, or imported, then exposes the current access to the
SDK.

Typical startup:

```rust,no_run
use paykit_sdk::{PaykitSdk, PaykitSdkConfig};

# async fn example<S, K, P>(storage: S, pubky: K, payment: P) -> paykit_sdk::Result<()>
# where
#     S: paykit_sdk::StorageAdapter,
#     K: paykit_sdk::PubkySessionProvider,
#     P: paykit_sdk::PaymentAdapter,
# {
let config = PaykitSdkConfig::new("bitkit")?;
let sdk = PaykitSdk::new(storage, pubky, payment, config);
let status = sdk.initialize().await?;

if status.capability == paykit_sdk::PubkyIdentityCapability::PrivateLinkCapable {
    // Private workflows also require current signed Noise key authorization.
}
# Ok(())
# }
```

Common workflows:

- call `initialize` on startup to refresh identity status from the Pubky
  provider
- call `publish_paykit_app` before publishing this app's public Payment
  Endpoints or creating app-attributed private work
- call `sync_public_endpoints` after local receiving details change
- request and validate `PAYKIT_SESSION_CAPABILITIES` for full SDK
  auth/session handoff
- use `publish_paykit_profile` / `fetch_paykit_profile` for identity-wide
  Paykit Profile metadata, including application-defined public fields in
  `extra`; updates pass the fetched revision so concurrent edits fail explicitly
- use `publish_paykit_blob` / `delete_paykit_blob` for files under the
  identity-wide Paykit blob prefix
- use `fetch_pubky_file` / `fetch_pubky_text` with an explicit byte limit to
  load public `pubky://` files referenced by profile metadata
- use `fetch_pubky_file_bounded` for a caller-selected limit capped at 5 MiB,
  including zero for an empty body. Limits apply while reading successful
  responses; Pubky currently buffers error bodies before returning to Paykit.
- use bounded `fetch_pubky_profile` and call `fetch_pubky_follows` with an
  explicit entry limit for read-only Pubky app profile and follows data
- use `resolve_profile` when contact display should prefer Paykit
  Profile and fall back to Pubky Profile
- construct `PubkySessionBootstrap` with a stable, app-owned client ID and use
  it for grant-only Pubky signup, signin, session import, capability-checked
  auth handoff, and `pubky://` normalization flows before exposing live access
  through a `PubkySessionProvider`; exported session secrets contain the grant
  and proof-of-possession key, while pending auth state contains the
  secret-bearing URL and client key, so both belong in secure storage
- call `PubkyAuthRequest::save_state` when an unapproved external auth request
  must survive process loss, then pass that complete state to
  `PubkySessionBootstrap::resume_auth`; the authorization URL alone cannot
  restore the proof-of-possession key. Once completion fetches an approval,
  cancellation or a later credential-exchange failure requires a new auth
  request because Pubky relay approvals are consumed when read
- use `PubkySessionBootstrap::republish_identity` to rebroadcast an existing
  signed PKARR identity record, even before session restoration. It chooses the
  newest record found across the configured networks and cache without changing
  its timestamp, signature, homeserver or other records. Returns `true` when
  published or `false` when no record was found; operational failures return
  errors. Cached records can be used when discovery fails, but a reported newer
  invalid DHT item is not overwritten. Reuse the bootstrap client for its cache.
  Apps own scheduling, throttling and retries; no background task or persistent
  packet store is added.
- use `PubkySessionBootstrap::approve_auth_with_companion_claim` for a
  `pubkyauth://` request carrying an application-defined companion claim; the
  integrator supplies the query parameter, claim type, exact expected
  capabilities, and serialized unsigned payload, while the helper signs and
  encrypts that payload, delivers it to the derived relay channel, and only
  then approves the Pubky grant
- when deriving a Pubky key from identity seed material, use the Pubky
  Core/Ring-compatible BIP39 seed or mnemonic helpers; the Pubky secret can
  deterministically derive any generation-specific Paykit secret
- rotate identity-wide Paykit key material with
  `rotate_paykit_identity_key`; rotation preserves durable history, resets old
  Encrypted Link state, and uses the next generation derived from the Pubky
  secret. Delegated apps import that key from their authorizer. Every remaining
  authorized app must persist it before private work resumes
- call `receive_private_messages` before deriving Private Payment Lists,
  Payment Requests, Allowances, Receipt Access state, or resolving a private
  contact payment when the freshest private endpoints matter. Idle checks verify
  signed key authorization, recovery markers, and the next message slot without
  claiming a peer lease or rewriting shared state. Batch intake shares one state
  read and probes up to sixteen peers concurrently. Available messages are prepared
  read-only, then committed atomically with their checkpoint only if the link and
  authorization remain current and no peer lease intervenes. App authorization
  updates share the first message commit. Recovery reloads state under a lease;
  message processing remains one peer at a time
- list saved Payment Requests with `payment_requests` or `list_payment_requests`;
  all counterparties and filters use one shared-state read, without network intake
- use `propose_allowance`, `accept_allowance`, `reject_allowance`, and
  `end_allowance` for durable lifecycle intent; drain the normal outbound queue
  and use `allowance_record` or `list_allowances` for derived views
- call `resolve_private_contact_payment` for Private Payment List endpoints or
  `resolve_public_contact_payment` for public Payment Endpoints; each returns a
  source-specific result with ordered adapter-built `PaymentTarget` values;
  public results also report app-specific endpoint load failures without
  discarding valid endpoints from other registered apps, including a
  `ResourceLimit` failure when an app's complete list cannot fit the bounded
  aggregate;
  private results also include an opaque `private_payment_list_version`, and
  passing the last consumed version back prevents every endpoint from that
  Private Payment List from being reused
- call `prepare_and_resolve_private_contact_payment` when private payment setup
  should also advance the Encrypted Link and drain pending private work; it
  never reads or falls back to public Payment Endpoints, and accepts the same
  optional consumed Private Payment List version
- when paying a received Payment Request, use
  `resolve_private_payment_request`, `resolve_public_payment_request`, or
  `prepare_and_resolve_private_payment_request`; these use the request amount
  and enforce its accepted endpoint identifiers and required payee App before
  invoking the payment adapter. Request preparation still performs fresh private
  intake and final validation, but leaves an otherwise idle queue of unclaimed,
  unprepared Delivery Confirmations durable for later outbound processing.
  Callers must drive maintenance; preparation does not schedule a worker or
  guarantee when those confirmations will be delivered
- build receipt drafts with `ReceiptDraftBuilder`; call
  `prepare_receipt_issuance` before receipt network side effects, then
  `process_receipt_issuance`; use `issue_receipt` only when the draft already
  has a caller-provided Receipt ID
- call `linked_peers`, `pending_outbound_private_counterparties`,
  `receipt_access_from`, `receipts_from`, and `issued_receipts_to` to drive
  app-visible work queues
- call `process_outbound_private_messages` for one counterparty, or
  `process_pending_private_messages` from a broader retry worker
- call `sync_public_contact_markers` on startup if the app uses public contact
  markers
- call `sign_out` when the app wants to revoke its Pubky grant and clear its
  local session access; shared Paykit state remains available to other apps and
  to a later session
- call `forget_session_access` only for explicit local-only cleanup
- call `remove_paykit_app` before sign-out when the app should also withdraw
  its public Payment Endpoints and App Registry entry; removal requires the app
  to cancel or finish its active Payment Requests, undelivered private events,
  and incomplete Receipt issuance first

## Conversion Quotes

Set optional `conversion` and `payment_deadline` fields on Payment Request terms.
For `per_period` conversion, the payee calls `quote_payment_request` after
acceptance. The request record retains `conversion_quotes`; a payer selects an
unexpired quote, preserves its ID with the in-flight payment and submits
`conversion_quote_id` with the Payment Proof. New quotes do not revoke earlier
ones. Delayed proofs remain available for amount and payment-time validation.
See [Payment conversion and deadlines](../specs/payment-conversion.md) for rate
coverage, rounding, role and expiry rules. The SDK checks correlation, while the
application verifies settlement and decides each installment's paid/underpaid/late
status.

## Allowance Integration

An Allowance is shared consent for possible automatic handling; the SDK does
not interpret it as payment authorization by itself. A typical lifecycle uses
the same durable private queue as other Event Messages:

```rust
use paykit_lib::{AllowanceAmountRange, AllowanceId, AllowanceTerms};
use paykit_sdk::{AllowanceFilter, AllowanceLocalRole};

let terms = AllowanceTerms::builder("btc")
    .per_payment_amount(AllowanceAmountRange::new("0.0001", "0.01")?)
    .lifetime_amount_limit("0.10")
    .build()?;
let proposed = sdk
    .propose_allowance(
        counterparty.clone(),
        AllowanceLocalRole::Allower,
        terms,
    )
    .await?;
let allowance_id = AllowanceId::new(&proposed.allowance_id)?;

// Send queued events through process_outbound_private_messages. After the
// peer receives the proposal, it calls accept_allowance or reject_allowance.
// Either peer may later call end_allowance for accepted authority.
let current = sdk
    .allowance_record(&counterparty, &allowance_id)
    .await?;
let all = sdk.list_allowances(AllowanceFilter::default()).await?;
```

Ordinary one-time and Recurring Payment Requests carry no Allowance ID and keep
the existing proposal, Acceptance, Cancellation, endpoint-resolution, and
recurrence rules. Payment Proof may report the Allowance actually used through
an optional `allowance_id`; it remains informational and never updates usage.
The lifecycle APIs supply durable evidence, not a decision to pay.

Before automatic work, an integrating wallet must satisfy these requirements:

- automatic handling is locally enabled and wallet policy selects one matching
  accepted Allowance on the exact Encrypted Link; that Allowance must cover the
  entire payment without pooling capacity from other Allowances;
- the selected association is durably recorded before side effects and survives
  restart, replay, and changes to wallet priorities;
- a temporary inability to pay can be deferred for policy-controlled
  reconsideration under the same association; an explicit manual-only decision
  is never cleared by background changes;
- a Recurring Payment Request retains its association, with an explicit
  user-authorized revision required to select replacement authority for future
  unpaid Billing Periods; prior payments and reservations keep their original
  attribution and accounting;
- the same semantic payment key coordinates automatic and manual execution
  across all Allowances, including successful and unresolved payments;
- only committed automatic payments and unresolved automatic reservations
  consume Allowance capacity; admission checks and reservations are atomic;
- verified success commits a reservation, confirmed failure without settlement
  releases it, and pending, unknown, or recovery-incomplete outcomes stay
  reserved until reconciled;
- every reconsideration repeats current lifecycle, time, capacity, endpoint,
  and local safeguard checks; an accepted request never sends another
  Acceptance merely because payment was deferred; and
- explicit payment of an accepted-but-unpaid occurrence requires complete
  payment and reservation evidence and coordination with automatic admission.

The SDK implements this durable coordination through the following workflow:

1. Call `reconcile_allowance_accounting` with complete authoritative wallet
   history before first admission or after recovery. Missing history never means
   zero usage; an empty reconciliation cannot erase retained evidence.
2. Inspect `evaluate_allowance_candidates`, then persist a choice with
   `select_allowance` or atomically select and queue ordinary Acceptance through
   `accept_payment_request_automatically`. Claim the request for this app with
   `claim_payment_request_for_execution` before accepting it. Acceptance reserves
   no capacity.
3. Call `reserve_automatic_payment` for an occurrence and expected association
   revision, or `reserve_manual_payment` for an explicitly authorized manual
   payment. Both use the same scoped exclusion key.
4. Immediately before execution, call `begin_payment_execution` with fresh
   wallet checks. Only Ready with status Submitted grants one handoff. Use the
   returned attempt ID for external wallet idempotency, then report the verified
   outcome through `record_payment_outcome`.

`defer_payment_occurrence` retains temporary failures for reconsideration;
`mark_payment_manual_only` is sticky. A pending execution cannot be relabeled
as deferred. `authorize_allowance_reassociation` requires explicit user approval
for a future recurring boundary and preserves earlier attempts and usage.
`allowance_accounting_state` exposes the retained ledger and recovery status.
Wallets retain consent, selection policy, scheduling, endpoint and transfer
validation, signing, execution, and settlement decisions. Authorized apps coordinate
through the identity-wide ledger, but SDK storage cannot atomically commit an
external payment. A crash after
handoff issuance requires reconciliation, never a timeout-based release or a
second execution.

During incomplete history or Encrypted Link recovery, automatic handling must
fail closed. Recovery must retain associations, usage, unresolved reservations,
and the evaluation-time watermark. Restoring an old but valid backup does not
prove accounting freshness; reconcile it with authoritative wallet execution
records before resuming automatic handling. Optional Payment Proofs cannot
reconstruct complete spending history. See [Allowances](../specs/allowances.md)
for the normative rules.

## Profile And Contacts

The SDK uses identity-wide Paykit paths:

- `/pub/paykit/profile.json` for Paykit Profile
- `/pub/paykit/blobs/...` for Paykit Profile blobs
- `/pub/paykit/contacts/...` for optional Public Contact Markers

For display bootstrap, `resolve_profile` tries Paykit Profile first and
can fall back to the Pubky app profile at `/pub/pubky.app/profile.json` when no
Paykit Profile exists. Paykit SDK only reads Pubky app profile/follows data; it
does not write those paths.

Before publication, an outbound private send stores the exact prepared
ciphertext and advanced Encrypted Link snapshot in one transaction. A retry
publishes that same ciphertext, including after a crash or an uncertain write,
before later messages can advance the link. Outbound status is local checkpoint
state; `confirmed_at` separately records the counterparty's durable receipt of
an Event Message, not business acceptance or payment execution. The SDK retries
unconfirmed events with the same Event ID and payload, including after relinking,
and confirms duplicates without reapplying them. Apps should run both receive
and outbound processing; sending alone cannot consume confirmations.
Non-retryable link-state failures still pause the peer for recovery.
Superseded reservation cleanup failures are
reported separately and do not block delivery of current outbound messages.

Private Payment List helpers support adapter-reserved receiving details. Apps
can queue reservation-backed lists directly when they already hold reservation
metadata, including an empty list to clear a counterparty's private list.

Storage implementations must commit raw private stream items, derived indexes,
and the advanced Encrypted Link snapshot atomically. If storage cannot provide
that transaction boundary, it should fail the receive operation instead of
persisting a partial checkpoint.

The SDK serializes identity-scoped calls on one runtime instance and uses
storage-backed per-peer leases for Encrypted Link work. Independent runtimes
using `PubkySharedStateStorage` acquire a homeserver write lock before reading
state. After contention or an uncertain result, inspect durable request/payment
records and resume existing work; a multi-step operation may already have
committed intent, so do not blindly restart it.
Apps with shared keys and write access are mutually trusted; App IDs do not
provide cryptographic isolation from other authorized apps.
The homeserver must fence writes through commit when lock ownership expires
and publish complete files durably. Locks alone do not provide storage crash safety.

Pubky 0.14 checks lock ownership when a write starts, not at commit. Production
multi-app use requires homeserver commit-time fencing of expired lock holders.
For testing, shared-state writes publish a unique pending marker before their
PUT and remove it after a confirmed result. An unconfirmed write leaves its
marker; the next transaction waits five minutes under a renewed lock, then
reloads state. Cancellation leaves the marker and restarts the wait on the next
attempt. If another runtime cannot acquire the lock and pending markers exist
or cannot be checked, it receives `SharedStateBusy`, not a retryable
`ConcurrentUpdate`; back off and show
recovery as pending. This can also block reads, and there is no fixed completion
deadline. The cooldown adds no timed delay to normal successful writes,
but cannot rule out a write completing after five
minutes and does not replace the homeserver fix.
Malformed pending markers block shared state instead of being ignored or
deleted automatically. Investigate them with participating apps stopped before
removing anything that could represent an unfinished write.

`sign_out` validates the active identity, revokes this application's Pubky
grant, and clears its local session access. It does not clear the identity's
Paykit state or withdraw another application's data. If live access, remote
revocation, or provider clearing fails, shared state remains intact and the
operation returns an error.

`forget_session_access` clears local access without revoking the grant. It is
an explicit recovery operation; other persisted copies of that grant remain
valid until expiry or remote revocation.

If the provider returns no live session access during ordinary startup or
workflow calls, the SDK blocks Pubky-backed work but preserves the last
identity-scoped state. Secure `sign_out` also fails until live access is
restored and the grant can be revoked. An app that should also withdraw its
published payment capability calls `remove_paykit_app` while authenticated
before signing out.

Local storage adapters can return cached private views for the initialized
identity without live session access. `PubkySharedStateStorage` requires an
active session and the identity's Paykit secret to read private state.
These cached views are stored state, not proof that the Encrypted Link is
currently healthy; apps should surface linked-peer recovery status when using
cached private endpoints for payment resolution.

The `storage` module is the advanced adapter boundary. Its record types include
raw private payloads, Encrypted Link snapshots, and Receipt Decryption Keys so
custom adapters can persist exact SDK state. App code should usually prefer the
`PaykitSdk` runtime methods and app-facing record/view types.

Normal backup restore is accepted only into an otherwise empty SDK state backing, so
an older app backup cannot replace newer shared state. Participating apps must
publish their App Registry entries again after restore. Restore preserves
terminal invalid and recovery-required outbound private records for audit,
while pending, sending, failed, sent, and superseded outbound records are
validated before restore.
Restore retains peer authorization pins; checkpoints resume only after the
[current signed-key checks](../specs/paykit-sdk.md#establish-encrypted-link).
Missing or unsafe checkpoints pause private automation until relink.

For missing or corrupt Pubky shared state, use
`recover_shared_state_from_backup(backup, replacement_key)` with a matching
trusted backup, active `PAYKIT_AUTHORIZER_SESSION_CAPABILITIES` session, Pubky
identity secret, current Paykit key and its derived successor, and existing
protected authorization. Missing, malformed, or conflicting authorization fails
before replacement. Persist the replacement key first; recovery commits state,
updates the App Registry, then publishes replacement authorization. See
[Backup And Restore](../specs/paykit-sdk.md#backup-and-restore) for same-key retries,
key distribution, relinking/reconciliation, and supported persisted formats.

Losing the durable SDK state without a backup means losing access to private
Paykit runtime state. Public Paykit data can be rediscovered from Pubky, but
Encrypted Link snapshots, private stream history, Receipt Access keys, outbound
queues, Contact Records, and Payment Request/Allowance/Receipt history cannot be safely
reconstructed from encrypted message slots alone.

## Testing

The SDK end-to-end tests start local Pubky testnets. Run Docker so the harness
can start Postgres, or set `TEST_PUBKY_CONNECTION_STRING` to a compatible test
Postgres instance before running the suite from the workspace root:

```sh
cargo test -p paykit-sdk --test testnet_e2e
```
