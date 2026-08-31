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
identity-wide Paykit key material and initialize the registry's Noise key.

Multiple app processes using the same identity must also use the same durable
SDK state and serialize updates to the shared Encrypted Link checkpoint. The
SDK ships `PubkySharedStateStorage`, which stores that logical state as one
encrypted Pubky resource. Separate local state blobs are only suitable when
one process owns the runtime. Concurrent independent writers still require
homeserver-enforced conditional writes or locking, and crash-safe shared sends
require a pending-send journal.

## Current Scope

Implemented in this Rust SDK crate:

- SDK runtime facade and atomic storage adapter contract
- encrypted identity-wide SDK state stored in Pubky for serialized runtimes
- Pubky session bootstrap helpers, identity status tracking, and sign-out handling
- request-bound application-defined companion claims for Pubky Auth
- public Payment Endpoint sync
- Encrypted Link setup, private stream intake, outbound retries, and recovery
  marker workflows
- Private Payment List publication/cache and contact payment resolution
- Payment Endpoint Reservations for contact-scoped receiving details
- Payment Request lifecycle state, Receipt Access indexing, receipt issuance,
  and receipt retrieval
- Paykit-facing profile/contact helpers and SDK backup/restore

Not implemented in this crate yet:

- first-party durable mobile storage helpers
- payment execution, settlement confirmation, balances, fees, or route policy
- product UI/profile screens, localization, and app backup transport
- crash-safe shared Noise journaling and concurrent cross-app state updates
- multi-device checkpoint synchronization and recurring payment scheduling

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
    // Private Paykit workflows can run for linked peers.
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
  Paykit Profile metadata, including application-defined public fields in `extra`
- use `publish_paykit_blob` / `delete_paykit_blob` for files under the
  identity-wide Paykit blob prefix
- use `fetch_pubky_file` / `fetch_pubky_text` with an explicit byte limit to
  load public `pubky://` files referenced by profile metadata
- use bounded `fetch_pubky_profile` and call `fetch_pubky_follows` with an
  explicit entry limit for read-only Pubky app profile and follows data
- use `resolve_profile` when contact display should prefer Paykit
  Profile and fall back to Pubky Profile
- use `PubkySessionBootstrap` for common Pubky signup, signin, session import,
  capability-checked auth handoff, and `pubky://` normalization flows before
  exposing live access through a `PubkySessionProvider`; exported session
  secrets and auth URLs must be treated as secret material, and session export
  is an explicit call on the bootstrap result
- use `PubkySessionBootstrap::approve_auth_with_companion_claim` for a
  `pubkyauth://` request carrying an application-defined companion claim; the
  integrator supplies the query parameter, claim type, exact expected
  capabilities, and serialized unsigned payload, while the helper signs and
  encrypts that payload, delivers it to the derived relay channel, and only
  then approves normal Pubky Auth
- when deriving a Pubky key from identity seed material, use the Pubky
  Core/Ring-compatible BIP39 seed or mnemonic helpers; the Pubky secret can
  deterministically derive any generation-specific Paykit secret
- rotate identity-wide Paykit key material with
  `rotate_paykit_identity_key`; rotation preserves durable history, resets old
  Encrypted Link state, and requires the derived or externally supplied
  replacement key to be persisted by every remaining authorized app before
  private work resumes
- call `receive_private_messages` before deriving Private Payment Lists,
  Payment Requests, Receipt Access state, or resolving a private contact
  payment when the freshest private endpoints matter
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
  invoking the payment adapter
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
- call `sign_out` when the app wants to clear its live Pubky access; shared
  Paykit state remains available to other apps and to a later session
- call `remove_paykit_app` before sign-out when the app should also withdraw
  its public Payment Endpoints and App Registry entry; removal requires the app
  to cancel or finish its active Payment Requests, undelivered private events,
  and incomplete Receipt issuance first

## Profile And Contacts

The SDK uses identity-wide Paykit paths:

- `/pub/paykit/profile.json` for Paykit Profile
- `/pub/paykit/blobs/...` for Paykit Profile blobs
- `/pub/paykit/contacts/...` for optional Public Contact Markers

For display bootstrap, `resolve_profile` tries Paykit Profile first and
can fall back to the Pubky app profile at `/pub/pubky.app/profile.json` when no
Paykit Profile exists. Paykit SDK only reads Pubky app profile/follows data; it
does not write those paths.

Outbound private sends are retried from durable records while the local
Encrypted Link checkpoint is trustworthy. If a worker may have sent the queue
head but failed before storing `Sent` status and the advanced snapshot, the SDK
retries the same stale `Sending` message before later messages can advance the
link. Outbound status is local checkpoint state, not counterparty
acknowledgement. Non-retryable link-state failures still pause the peer for
recovery. Superseded reservation cleanup failures are reported separately and do
not block delivery of current outbound messages.

Private Payment List helpers support adapter-reserved receiving details. Apps
can queue reservation-backed lists directly when they already hold reservation
metadata, including an empty list to clear a counterparty's private list.

Storage implementations must commit raw private stream items, derived indexes,
and the advanced Encrypted Link snapshot atomically. If storage cannot provide
that transaction boundary, it should fail the receive operation instead of
persisting a partial checkpoint.

Apps should also serialize identity-scoped operations such as `initialize`,
`sign_out`, backup restore, App Registry updates, and public endpoint sync when
multiple runtime instances share the same storage. The SDK serializes these
calls on one runtime instance and uses storage-backed per-peer leases for
Encrypted Link work, but it does not add a process-wide identity or App
Registry lock by itself.

`sign_out` clears only this application's live Pubky session access. It does
not clear the identity's Paykit state or withdraw another application's data.
If provider clearing fails, stored state remains intact so callers can retry
safely.

If the provider returns no live session access during ordinary startup or
workflow calls, the SDK blocks Pubky-backed work but preserves the last
identity-scoped state. An app that should also withdraw its published payment
capability calls `remove_paykit_app` while authenticated before signing out.

Read-only private views such as cached Private Payment Lists can still be
returned for the initialized identity when live session access is missing.
These cached views are stored state, not proof that the Encrypted Link is
currently healthy; apps should surface linked-peer recovery status when using
cached private endpoints for payment resolution.

The `storage` module is the advanced adapter boundary. Its record types include
raw private payloads, Encrypted Link snapshots, and Receipt Decryption Keys so
custom adapters can persist exact SDK state. App code should usually prefer the
`PaykitSdk` runtime methods and app-facing record/view types.

Backup restore is accepted only into an otherwise empty SDK state backing, so
an older app backup cannot replace newer shared state. Participating apps must
publish their App Registry entries again after restore. Restore preserves
terminal invalid and recovery-required outbound private records for audit,
while pending, sending, failed, sent, and superseded outbound records are
validated before restore.
Restored Encrypted Link checkpoints resume when they are valid. Missing or
unsafe checkpoints mark affected peers recovery-required so private automation
pauses until relink.

Losing the durable SDK state without a backup means losing access to private
Paykit runtime state. Public Paykit data can be rediscovered from Pubky, but
Encrypted Link snapshots, private stream history, Receipt Access keys, outbound
queues, Contact Records, and Payment Request/Receipt history cannot be safely
reconstructed from encrypted message slots alone.
