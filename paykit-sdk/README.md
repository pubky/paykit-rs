# Paykit SDK

Stateful Rust runtime for Paykit integrations.

`paykit-sdk` builds on `paykit-lib` and owns SDK-level local state for Pubky
identity status, public Payment Endpoint sync, Encrypted Link state, private
stream intake, Private Payment List derivation, contact payment resolution, and
outbound Private Application Message delivery. It also derives Payment Request
state, indexes Receipt Access events, retrieves/decrypts Encrypted Receipts,
tracks local receipt issuance, tracks optional Payment Endpoint Reservations,
manages Paykit-facing profile and local contact records, and exports/restores
SDK-managed backup state.

This crate exposes the Rust SDK API. The workspace also ships Swift and Kotlin
SDK bindings through `paykit-ffi`, which should be the primary mobile/app
integration surface.

Payment execution, settlement detection, balances, route policy, product UI,
and app backup transport stay outside the SDK and are provided by application
or payment-adapter code.

The SDK is designed around an app-owned Paykit runtime model. One SDK runtime
represents one app or receiver runtime with its own Paykit state: Encrypted
Links, private stream checkpoints, outbound queues, receipts, requests,
reservations, recovery state, and backup data. Multiple apps can be linked or
aggregated explicitly, but they do not share one private Paykit runtime by
default.

Each runtime is configured with one local Paykit receiver path, but that path
only describes this app/runtime's folder. Private and payment counterparty APIs take
the counterparty's exact receiver path separately. A Pubky key alone is not enough
information to route private Paykit state to a specific app/runtime folder.
Apps can call `paykit_receiver_paths` as a discovery helper, but they still choose
the receiver path explicitly. Discovery returns receiver paths that publish a
valid Receiver Marker or at least one public Payment Endpoint. A receiver with
no public Payment Endpoints can publish a lightweight Receiver Marker so it is
still discoverable from the Pubky identity. Marker publication is explicit
because it makes the receiver publicly discoverable; SDK setup, auth, and
profile helpers do not publish it automatically.

The Receiver Marker also publishes that receiver's Noise public key. Its
public visibility is intentional: it cannot decrypt messages or derive the
pairwise DH secret without the receiver Noise secret. Apps generate that
secret once per receiver, supply it when creating or importing session access,
persist it alongside that access, and reuse it after restart or
reauthentication. It is independent of the Pubky identity secret, so Ring- or
server-owned identities remain private-link-capable. Session creation and
restore require this receiver key even when the Pubky identity secret remains
in an external signer.

## Current Scope

Implemented in this Rust SDK crate:

- SDK runtime facade and atomic storage adapter contract
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
- multi-device checkpoint synchronization and recurring payment scheduling

The SDK derives and persists Recurring Payment Request lifecycle state, but the
integrating application owns scheduling, payment authorization, execution,
settlement validation, and service-access policy. See
[Recurring Payment Requests And Subscriptions](../specs/payment-requests.md#recurring-payment-requests-and-subscriptions).

## Integration Shape

Apps construct `PaykitSdk` with three pieces:

- a `StorageAdapter` that provides atomic local transactions
- a `PubkySessionProvider` that returns live Pubky session access and clears it
  during sign-out
- a `PaymentAdapter` that supplies receiving details, endpoint selection, and
  payment-target construction

The session provider is only the boundary for live Pubky access. It is not a
requirement to use Ring or another wallet as an identity coordinator before an
app can use Paykit. The app or binding layer decides how session material is
created, stored, unlocked, or imported, then exposes the current access to the
SDK.

Typical startup:

```rust,no_run
use paykit_sdk::{PaykitReceiverPath, PaykitSdk, PaykitSdkConfig};

# async fn example<S, K, P>(storage: S, pubky: K, payment: P) -> paykit_sdk::Result<()>
# where
#     S: paykit_sdk::StorageAdapter,
#     K: paykit_sdk::PubkySessionProvider,
#     P: paykit_sdk::PaymentAdapter,
# {
let config = PaykitSdkConfig::new(PaykitReceiverPath::new("bitkit/wallet")?);
let sdk = PaykitSdk::try_new(storage, pubky, payment, config)?;
let report = sdk.initialize().await?;

if report.identity.live_session_available {
    // Private Paykit workflows can run for linked peers.
}
# Ok(())
# }
```

Common workflows:

- call `initialize` on startup to refresh identity status from the Pubky
  provider
- call `sync_public_endpoints` after local receiving details change
- request and validate `config.required_session_capabilities()` for full SDK
  auth/session handoff
- set `profile_namespace` to a namespace segment such as `bitkit.to` when
  profile/contact helpers should publish under
  `/pub/bitkit.to/{receiver_path}/...`
- use `publish_paykit_profile` / `fetch_paykit_profile` for configured
  Paykit Profile metadata, including app-specific public fields in `extra`
- use `publish_paykit_blob` / `delete_paykit_blob` for files under the
  configured Paykit blob prefix
- use `fetch_pubky_file` / `fetch_pubky_text` to load public `pubky://`
  files referenced by profile metadata
- use `fetch_pubky_profile` / `fetch_pubky_follows` for read-only Pubky app
  profile and follows data
- use `resolve_contact_profile` when contact display should prefer Paykit
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
- use `PubkySessionBootstrap::approve_auth_with_companion_claim` for a
  `pubkyauth://` request carrying an application-defined companion claim; the
  integrator supplies the query parameter, claim type, exact expected
  capabilities, and serialized unsigned payload, while the helper signs and
  encrypts that payload, delivers it to the derived relay channel, and only
  then approves the Pubky grant
- when deriving a Pubky key from identity seed material, use the Pubky
  Core/Ring-compatible BIP39 seed or mnemonic helpers; app/runtime separation
  should come from receiver folders, Noise keys, and SDK state, not a different
  Pubky identity derivation label
- call `receive_private_messages` before deriving Private Payment Lists,
  Payment Requests, Receipt Access state, or resolving a private contact
  payment when the freshest private endpoints matter
- call `resolve_private_contact_payment` for Private Payment List endpoints or
  `resolve_public_contact_payment` for public Payment Endpoints; each returns a
  source-specific result with ordered adapter-built `PaymentTarget` values;
  private results also include an opaque `private_payment_list_version`, and
  passing the last consumed version back prevents every endpoint from that
  Private Payment List from being reused
- call `prepare_and_resolve_private_contact_payment` when private payment setup
  should also advance the Encrypted Link and drain pending private work; it
  never reads or falls back to public Payment Endpoints, and accepts the same
  optional consumed Private Payment List version
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
- call `sign_out` when the app wants to revoke its Pubky grant and clear
  SDK-managed identity-scoped state
- export and persist an SDK backup before `sign_out` if the app wants that
  sign-out to be reversible for the same user

## Profile And Contact Namespace

The SDK profile/contact namespace is SDK-level Paykit app data. By default the
SDK uses receiver-scoped Paykit paths:

- `/pub/paykit/v0/{receiver_path}/profile.json` for Paykit Profile
- `/pub/paykit/v0/{receiver_path}/blobs/...` for Paykit Profile blobs
- `/pub/paykit/v0/{receiver_path}/contacts/...` for optional Public Contact Markers
- `/pub/paykit/v0/{receiver_path}/receiver.json` for optional public receiver discovery

Apps can set `profile_namespace` to use their own public app namespace, such as
`bitkit.to`. Those helper paths still stay receiver-scoped, for example
`/pub/bitkit.to/{receiver_path}/profile.json`,
`/pub/bitkit.to/{receiver_path}/blobs/...`, and
`/pub/bitkit.to/{receiver_path}/contacts/...`. Public Payment
Endpoints and Encrypted Link/private runtime state stay under the configured
receiver path. `profile_namespace` is not a shared-runtime selector or
app-specific core Paykit path prefix.

Remote profile fetches use the same configured profile namespace. Cross-app
profile discovery therefore needs an agreed namespace or explicit metadata that
describes which profile namespace a receiver path uses.

The SDK rejects `pubky.app` as a configured profile namespace only as a local
guardrail against accidental writes through Paykit helpers. It is not a
permission boundary. Pubky session scopes are still the authority for what a
caller can write.

For display bootstrap, `resolve_contact_profile` tries Paykit Profile first and
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
`sign_out`, `forget_session_access`, backup restore, and public endpoint sync
when multiple runtime instances share the same storage. The SDK serializes
these calls on one runtime instance and uses storage-backed per-peer leases for
Encrypted Link work, but it does not add a process-wide identity or public
endpoint lock by itself.

`sign_out` revokes the current Pubky grant before clearing live session access
and SDK-managed identity-scoped storage. If revocation or provider clearing
fails, the SDK leaves local state intact so callers can retry without losing
contacts, links, queues, or receipts. If local storage clearing fails after the
grant and provider access are cleared, retry `sign_out` or clear SDK storage
through the adapter.

`forget_session_access` performs the same destructive local cleanup without
remote revocation. It is an offline recovery escape hatch: other persisted
copies of the grant remain usable until the grant expires or is revoked
elsewhere.

If the provider returns no live session access during ordinary startup or
workflow calls, the SDK blocks Pubky-backed work but preserves the last
identity-scoped state. Call `sign_out` when the app intentionally wants to clear
that state. If the app wants to restore private Paykit state after sign-out, it
must keep a separate SDK backup and not delete it as part of sign-out.

Read-only private views such as cached Private Payment Lists can still be
returned for the initialized identity when live session access is missing.
These cached views are local state, not proof that the Encrypted Link is
currently healthy; apps should surface linked-peer recovery status when using
cached private endpoints for payment resolution.

The `storage` module is the advanced adapter boundary. Its record types include
raw private payloads, Encrypted Link snapshots, and Receipt Decryption Keys so
custom adapters can persist exact SDK state. App code should usually prefer the
`PaykitSdk` runtime methods and app-facing record/view types.

Backup restore preserves terminal invalid and recovery-required outbound
private records for audit, while pending, sending, failed, sent, and superseded
outbound records are validated before restore.
Restored Encrypted Link checkpoints resume when they are valid. Missing or
unsafe checkpoints mark affected peers recovery-required so private automation
pauses until relink.

Losing SDK-managed backup or state blob data means losing access to local
private Paykit runtime state. Public Paykit data can be rediscovered from Pubky,
but Encrypted Link snapshots, private stream history, Receipt Access keys,
outbound queues, local Contact Records, and Payment Request/Receipt history
cannot be safely reconstructed from homeserver data alone.
