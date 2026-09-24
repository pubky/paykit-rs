# Payment Workflows

These sequences describe SDK integration, not a hosted checkout API. Method
names are Rust; mobile equivalents use camelCase. Check installed declarations
for concrete types and argument labels. Keep caller-owned steps, such as wallet
execution and order authorization, outside SDK calls.

## Wallet Receive and Pay a Contact

1. Restore secure session/receiver-key material and durable SDK state, then
   initialize the runtime. Use its configured capabilities for auth. Publish a
   Receiver Marker explicitly when enabling private communication.
2. Discover the contact's receiver paths. Keep one Contact Record for the Pubky
   identity with its selected receiver paths. Use an exact remote path for the
   payment; the SDK does not select a preferred wallet or aggregate receivers.
   A new scan can refresh discovery without creating a duplicate identity contact.
3. The receiving wallet creates wallet-valid receiving details. Public sync
   publishes the current public set; private sync queues a complete private set
   for each target receiver after link setup. Use stable reservation IDs if the
   wallet reserves addresses/invoices. An empty private set clears it. Do not
   enable unlisted-peer cleanup on a partial set of updates.
4. The payer invokes `prepare_and_resolve_private_contact_payment` and examines
   `resolution.status`, `resolution.state`, and all preparation reports. No
   endpoints while linking/recovering is not permission to construct a payment
   from stale cached data. For a public flow, call `resolve_public_contact_payment`
   separately under the product's explicit fallback/selection policy.
5. The wallet validates and authorizes execution of an adapter-built target.
   If consuming a whole private list, persist its returned version before
   submitting payment, scoped to SDK state plus counterparty key/path. Pass that
   version back on the next resolution; merely displaying a list does not consume it.
6. Track actual payment outcome in the wallet. A lost response from the wallet
   payment operation requires wallet-specific reconciliation, not another payment
   because a Paykit message was retried. Continue SDK receive/outbound work and
   backup dirty tracking independently.

Payment References correlate a payment with an order/invoice; they are not Event
IDs, Receipt IDs, or receiver identifiers. Use endpoint identifiers and payload
constructors/helpers rather than inventing path or invoice parsing rules.

## Payment Request to Receipt

Prerequisites: initialized peers with session access and published markers.
Complete the handshake before queueing Payment Request events or processing
Receipt Access issuance; unlike Private Payment Lists, these operations require
an active link. Preparing a receipt locally does not require an active link.
Both sides run their private synchronization cycle. The following is a sequence,
not a single cross-party atomic transaction.

1. The payee builds `PaymentRequestTerms`, including its Payment Reference,
   amount, allowed endpoint identifiers, expiry, and optional recurrence. Call
   `propose_payment_request` once for that logical proposal and retain its
   Payment Request ID in the application's order correlation.
2. Process outbound work. The payer receives through `receive_private_messages`
   and queries `actionable_received_payment_requests` or `list_payment_requests`.
   The payee's returned proposal record reflects local queueing, not peer delivery.
3. The payer checks terms and user authorization, then calls
   `accept_payment_request` or `reject_payment_request`. Exchange the queued
   event through normal workers; acceptance does not execute payment.
4. The payer's wallet executes the selected payment and calls
   `submit_payment_proof` with method-specific evidence and the relevant billing
   period, if recurring. The SDK correlates the request/reference; it does not
   establish settlement from arbitrary proof JSON.
5. The payee validates the evidence using the payment method, expected recipient,
   amount, and its settlement policy. Only then should its application issue a
   payment-confirming Receipt or authorize the corresponding order entitlement.
6. Build a Receipt draft with the same Payment Reference and applicable request/
   billing-period correlation. Persist a stable Receipt ID before repeatable
   issuance, or retain the ID returned by `prepare_receipt_issuance`. Continue
   with `process_receipt_issuance`; it stores the Encrypted Receipt and queues
   Receipt Access. Process outbound work to send that access event.
7. The recipient indexes Receipt Access through normal intake and uses
   `retrieve_receipt` to fetch/decrypt/validate it. Successful retrieval is not a
   generic blockchain check or permission to unlock any product: the consuming
   application still checks issuer trust, recipient, correlation, and entitlement.

An uncertain proposal call can already have queued a request. Inspect existing
SDK records before proposing again: `propose_payment_request` generates fresh
IDs per call. Retry durable outbound work instead of recreating events. Apply
the same distinction to receipt issuance: repeat with the retained Receipt ID,
not a newly generated receipt for every network attempt.

For recurring requests, the app owns scheduling, spending authorization,
settlement validation, and one-payment-per-period policy. `ActiveRecurring` is
not an automatic payment worker. Cancellation changes the coordination state;
it does not reverse an already executed payment or refund an order.

## Checkout, Processors, and Locks

A shop may call its transaction service, which calls a payment processor using
that processor's HTTP contract. Keep SDK payment coordination separate from
service invoice state and shop order/entitlement state. Do not treat an HTTP
`activate` or `resolve` operation as a Paykit protocol event unless the actual
service defines that translation.

Preserve the order-to-request/invoice correlation and the issuing receiver or
processor identity/endpoint across retries. An idempotent HTTP request is not
necessarily an idempotent wallet payment. A new signature, Event ID, or request
ID must not turn an uncertain old operation into a second payment.

Payment Proof, Receipt, and Locks access grant are different objects. Follow the
Locks/service contract for content authorization and secret handling; do not
invent a conversion from a Receipt to an access grant. A watch-only processor
can observe supported payments but cannot spend funds merely because it can
issue receipts. Its always-on availability also does not make a mobile wallet
always available to generate new receiving details.

## Integration Acceptance Checks

Test the selected flow at the application's boundaries; mocks alone cannot prove
the two peers' lifecycle, generated binding, and storage contracts agree.

| Case | What to check |
| --- | --- |
| Two receivers under one contact | Requests and payments reach the intended receiver; no duplicate identity contact or unintended cleanup |
| No private link or offline peer | The UI reports pending/unavailable; queued work survives restart; no hidden public fallback |
| One recipient list consumed | Resolution waits for a newer list rather than offering another entry from the consumed list |
| Public/private publication partially fails | Per-target failures are handled even when the outer call succeeds |
| Retry after an uncertain send | Logical IDs/correlation remain stable; no duplicate payment or request is created |
| Proof submitted but settlement absent/mismatched | No payment-confirming Receipt or paid entitlement is granted solely on the submitted proof |
| Receipt stored before access delivery fails | The same issuance is resumed and recipient access eventually works without a second Receipt |
| Restart or backup restore | Matching session/key material and SDK state restore access; the seed alone is not treated as a private-history backup |

For corrupted links and simultaneous recovery, use the cases in
[Link Recovery](recovery.md). A fresh successful payment is not evidence that a
preserved broken-link scenario is repaired.

Sources: [payment resolution](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/payment_resolution.rs),
[request lifecycle](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/payment_requests.rs),
[receipt issuance/retrieval](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/runtime/receipts.rs),
[Payment Requests spec](https://github.com/pubky/paykit-rs/blob/master/specs/payment-requests.md).
