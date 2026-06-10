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

This crate is Rust-only. Platform bindings expose low-level `paykit-lib` APIs;
SDK bindings are a separate integration surface.

Payment execution, settlement detection, balances, route policy, product UI,
and app backup transport stay outside the SDK and are provided by application
or payment-adapter code.

## Integration Shape

Apps construct `PaykitSdk` with three pieces:

- a `StorageAdapter` that provides atomic local transactions
- a `PubkySessionProvider` that returns live Pubky session access and clears it
  during sign-out
- a `PaymentAdapter` that supplies receiving details, endpoint selection, and
  payment-target construction

Typical startup:

```rust,no_run
use paykit_sdk::{PaykitSdk, PaykitSdkConfig};

# async fn example<S, K, P>(storage: S, pubky: K, payment: P) -> paykit_sdk::Result<()>
# where
#     S: paykit_sdk::StorageAdapter,
#     K: paykit_sdk::PubkySessionProvider,
#     P: paykit_sdk::PaymentAdapter,
# {
let sdk = PaykitSdk::try_new(storage, pubky, payment, PaykitSdkConfig::default())?;
let report = sdk.initialize().await?;

if report.identity.private_link_capable {
    // Private Paykit workflows can run for linked peers.
}
# Ok(())
# }
```

Common workflows:

- call `initialize` on startup to refresh identity capability from the Pubky
  provider when live session access is available
- call `sync_public_endpoints` after local receiving details change
- call `receive_private_messages` before deriving private Payment Lists,
  Payment Requests, Receipt Access state, or resolving a contact payment when
  the freshest private endpoints matter
- call `resolve_contact_payment` to get both the selected endpoint and
  adapter-built `PaymentTarget`
- call `prepare_receipt_issuance` before receipt network side effects, then
  `process_receipt_issuance`; use `issue_receipt` only when the draft already
  has a caller-provided Receipt ID
- call `linked_peers`, `pending_outbound_private_counterparties`, and
  `receipt_access_records` to drive app-visible work queues
- call `process_outbound_private_messages` for one counterparty, or
  `process_pending_private_messages` from a broader retry worker
- call `sync_public_contact_markers` on startup if the app uses public contact
  markers
- call `sign_out` when the app wants to clear live Pubky access and
  SDK-managed identity-scoped state
- export and persist an SDK backup before `sign_out` if the app wants that
  sign-out to be reversible for the same user

Outbound private sends are retried from durable records while the local
Encrypted Link checkpoint is trustworthy. If a worker may have sent the queue
head but failed before storing `Sent` status and the advanced snapshot, the SDK
retries the same stale `Sending` message before later messages can advance the
link. Outbound status is local checkpoint state, not counterparty
acknowledgement. Non-retryable link-state failures still pause the peer for
recovery. Superseded reservation cleanup failures are reported separately and do
not block delivery of current outbound messages.

Storage implementations must commit raw private stream items, derived indexes,
and the advanced Encrypted Link snapshot atomically. If storage cannot provide
that transaction boundary, it should fail the receive operation instead of
persisting a partial checkpoint.

Apps should also serialize identity-scoped operations such as `initialize`,
`sign_out`, backup restore, and public endpoint sync when multiple runtime
instances share the same storage. The SDK serializes `initialize`, `sign_out`,
and public endpoint sync calls on one runtime instance and uses storage-backed
per-peer leases for Encrypted Link work, but it does not add a process-wide
identity or public endpoint lock by itself.

`sign_out` clears live Pubky session access before clearing SDK-managed
identity-scoped storage. If provider clearing fails, the SDK leaves local state
intact so callers can retry without losing contacts, links, queues, or receipts.
If provider clearing succeeds but local storage clearing fails, Pubky-backed
workflows remain blocked because no live session is available; retry `sign_out`
or clear SDK storage through the adapter.

If the provider returns no live session access during ordinary startup or
workflow calls, the SDK blocks Pubky-backed work but preserves the last
identity-scoped state. Call `sign_out` when the app intentionally wants to clear
that state. If the app wants to restore private Paykit state after sign-out, it
must keep a separate SDK backup and not delete it as part of sign-out.

Read-only private views such as cached Private Payment Lists can still be
returned for the initialized identity when live private-link capability is
missing. These cached views are local state, not proof that the Encrypted Link is
currently healthy; apps should surface linked-peer recovery status when using
cached private endpoints for payment resolution.

The `storage` module is the advanced adapter boundary. Its record types include
raw private payloads, Encrypted Link snapshots, and Receipt Decryption Keys so
custom adapters can persist exact SDK state. App code should usually prefer the
`PaykitSdk` runtime methods and app-facing record/view types.

Backup restore preserves terminal invalid and recovery-required outbound
private records for audit, while pending, sending, failed, sent, and superseded
outbound records are validated before restore.
Restored Encrypted Link snapshots are not resumed automatically today; affected
peers are marked recovery-required so private automation pauses until relink.

Losing SDK-managed backup or state blob data means losing access to local
private Paykit runtime state. Public Paykit data can be rediscovered from Pubky,
but Encrypted Link snapshots, private stream history, Receipt Access keys,
outbound queues, local Contact Records, and Payment Request/Receipt history
cannot be safely reconstructed from homeserver data alone.
