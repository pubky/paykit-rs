# Payment Workflows

Method names here are Rust SDK methods unless explicitly labeled FFI. Check
installed declarations for types and borrowing. Wallet execution, authorization,
settlement, external idempotency, and order/entitlement policy are caller-owned.

## Publish and Discover

Initialize the identity's live state/session/key access and explicitly publish
this app with `publish_paykit_app`. A Contact Record represents one Pubky
identity, not one app. `paykit_app_registry(owner)` discovers registered apps,
capabilities, Noise key/generation, and optional defaults. Public Payment
Endpoints are app-scoped; the private link is identity-wide. Preserve candidate
`app_id` through selection and execution; defaults do not override wallet policy
or a Payment Request's required payee App.

- Public: call `sync_public_endpoints()` or
  `sync_public_endpoints_with_receiving_details(details)`. Keep `ManagedOnly`
  unless the app intentionally owns its entire app endpoint namespace.
- Private: first call `ensure_link_with_peer(counterparty, max_advance_steps)`.
  List queueing needs an active link or persisted handshake; sync does not
  initiate one. Use
  `sync_private_payment_lists_with_reservations_and_process_outbound(updates,
  clear_unlisted_linked_peers)` for reserved wallet details. Each update contains
  the counterparty and this app's complete reservations, not receiver paths.
- An empty private list clears that app's list for the counterparty, not another
  app's list or the shared link. Use unlisted-peer cleanup only with a complete
  keep set for this app. Retain reservation IDs through retries.
- Inspect public sync failures and private `failed_to_queue`/`failed_to_deliver`
  (FFI `failedToQueue`/`failedToDeliver`). An unconfirmed shared-state write may
  have committed; earlier pending work also survives. These reports are
  not rollback guarantees. Retain submitted and previously unresolved reservations.
  For unchanged intent, retry the same complete inputs with the same reservation
  IDs and still-valid details; a retry may publish again. For a newer intended
  list, submit its complete set, preserving IDs for unchanged reservations rather
  than replaying the older update. Omitted reservations are not thereby released:
  release only after successful SDK-requested, idempotent adapter cancellation or
  adapter-confirmed safe expiry or cleanup. A superseding list cannot prove that
  previously attempted details were never shared; keep unresolved reservations.

Saved contacts are private shared records by default. Public Contact Markers
are separately opt-in, not app enrollment or proof of a live link. For display,
`resolve_profile` prefers Paykit Profile with Pubky Profile fallback. Public
profile updates/deletion use the fetched revision to reject concurrent edits;
profile/avatar/blob publication is public, not private backup storage.

## Pay a Contact

1. Choose public or private deliberately. Private preparation is
   `prepare_and_resolve_private_contact_payment(counterparty, amount,
   after_private_payment_list_version, max_advance_steps)`. Public resolution is
   `resolve_public_contact_payment(counterparty, amount)`; it never consults
   private links. `amount` is `Option<PaymentAmountContext>`.
2. Private resolution combines current lists from authorized apps with per-app
   latest-state semantics. Inspect both `resolution.status` and `.state` and
   the link/receive/outbound reports. Preparation can throw `RecoveryRequired`
   while the stored peer is `Linking`; inspect state and retry later. Never
   substitute stale endpoints or a hidden public fallback for recovery.
3. For list-backed payment, persist `private_payment_list_version` before wallet
   submission, scoped to the SDK state and counterparty. Using an endpoint
   consumes the whole list, not just that entry. Pass the opaque consumed token
   on later resolution; candidates from lists at or below it are excluded.
   Merely displaying candidates is not consumption.
4. Validate the adapter-built target and obtain wallet authorization. Preserve
   the consumed version through pending/uncertain wallet execution. Reconcile a
   lost wallet response using its durable execution journal/idempotency, not a
   fresh send because another SDK retry succeeded. Coordinate concurrent app
   execution; shared Noise storage is not itself a payment-execution lock.

Public resolution reports app-specific `failures` while retaining valid results
from other apps. `Unavailable` differs from `NoEndpoint`; `ResourceLimit` can
mean an app's complete list exceeded the bounded aggregate. Do not interpret
partial discovery as proof that no other payment route exists.

## Explicit Payment Request Destinations

Use `PaymentRequestTerms::builder(amount, payment_reference,
accepted_payment_endpoint_identifiers)` and validated builder methods. A request
may bind immutable endpoints with `.required_app_id(Some(app_id))` and
`.payment_endpoints(Some(endpoints))`. The map must be nonempty, have nonempty
payloads, and contain only accepted identifiers. `None` means ordinary discovery;
`Some` forbids fallback outside the map. The required App is the **payee's
endpoint owner**, not the payer app selected to execute. The envelope's source
App ID is a separate attribution.

Resolve received requests through
`resolve_private_payment_request(counterparty, &payment_request_id,
after_private_payment_list_version)` or
`prepare_and_resolve_private_payment_request(counterparty, &payment_request_id,
after_private_payment_list_version, max_advance_steps)`. These apply the request's
amount, accepted identifiers, and required payee App before adapter selection.
Bound endpoints replace list discovery and return no private-list version;
the consumed-list token does not apply. They still require valid private/app
state and wallet validation. `resolve_public_payment_request(counterparty,
&payment_request_id)` has no fallback for bound requests. Unsupported or expired
bound details are a blocked payment, not permission to pay a different address.

## Request, Execution, and Receipt

Complete the Encrypted Link before queueing Payment Request events or processing
Receipt Access issuance. Local receipt preparation can precede a link. Both
identities run receive and outbound workers, including Delivery Confirmations.

1. Payee: `propose_payment_request(counterparty, terms)` creates fresh request
   and Event IDs. Retain the returned Payment Request ID and order correlation.
   After an uncertain result, inspect existing records instead of proposing
   again; resume durable outbound work with original IDs/payloads.
2. Payer: query `actionable_received_payment_requests()` or
   `list_payment_requests(filter)`. Claim with
   `claim_payment_request_for_execution(counterparty, &payment_request_id)`
   before payment preparation; shared claims coordinate local apps. Check terms
   and consent, then accept or reject. A still-proposed request must be accepted
   successfully before execution. A claim or Acceptance is not payment.
3. Resolve through the request-specific API and execute only through the wallet.
   Retain one execution identity per occurrence across manual/automatic paths.
   Do not release claims or unresolved reservations to let another app retry an
   uncertain financial execution.
4. Submit method-specific Payment Proof through SDK lifecycle APIs. Preserve the
   Payment Reference, chosen payee App/endpoint, and applicable Billing Period
   and conversion quote correlation. `ProofSubmitted` is not settlement.
5. Payee: verify actual recipient, amount, method evidence, and settlement policy
   before issuing a payment-confirming Receipt or granting an entitlement.
   Persist a stable Receipt ID or retain the ID from `prepare_receipt_issuance`.
   `process_receipt_issuance` stores the Encrypted Receipt and queues Receipt
   Access; resume the same issuance after partial failure, then drain outbound.
6. Recipient: intake indexes Receipt Access; `retrieve_receipt` fetches,
   decrypts, and validates it. The app still checks issuer trust, correlation,
   and entitlement. Receipt Access, Receipt, Payment Proof, and product access
   grants are distinct objects.

Recurring requests still need app scheduling, user/local authorization, payment
execution, and one-payment-per-Billing-Period policy. Cancellation is not a refund.
For conversion/deadline features, establish peer support, preserve the selected
quote with the execution, and validate payment timing/amount externally.

For Allowance-based automation, use SDK accounting rather than a parallel local
counter: reconcile complete wallet history, persist selection/association,
reserve with `reserve_automatic_payment` or `reserve_manual_payment`, obtain a
fresh handoff through `begin_payment_execution`, then `record_payment_outcome`.
Only Ready with Submitted status grants one handoff; use its attempt ID for wallet
idempotency. Unknown outcomes stay reserved. Restore/relink requires complete
reconciliation before automation; an empty history, timeout, or Payment Proof
cannot establish unused capacity. Read the
[Allowance contract](https://github.com/pubky/paykit-rs/blob/master/specs/allowances.md)
and [SDK accounting workflow](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/README.md#allowance-integration)
when implementing automatic handling.

## Acceptance Checks

Test at app boundaries with isolated peers and no real funds:

- Two apps share one identity/state: one Contact Record/link, app-attributed
  endpoints/lists, no cross-app cleanup or duplicate request execution.
- Offline peer/restart: durable intent survives, handshakes resume, no public fallback.
- Consumed list and uncertain wallet result: no second endpoint/payment is offered
  from consumed state; request-bound endpoints bypass list freshness only.
- Partial publication/send/confirmation failure: retain IDs, reconcile progress,
  confirm event replay without duplicate application effects.
- Wrong/missing settlement evidence: no paid entitlement or payment-confirming Receipt.
- Receipt stored before access delivery fails: resume the same issuance.
- Sign-out/removal/key rotation/backup recovery: shared history and other apps are
  preserved; stale keys and unreconciled wallet history cannot resume execution.

Sources: [resolution](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/payment_resolution.rs),
[request lifecycle](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/payment_requests.rs),
[request terms](https://github.com/pubky/paykit-rs/blob/master/paykit-lib/src/payment_request/types.rs),
[receipt issuance](https://github.com/pubky/paykit-rs/tree/master/paykit-sdk/src/runtime/receipts).
