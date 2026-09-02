# Paykit Payment Request Protocol v0.2

Status: draft / discussion only
Date: 2026-05-29
Supersedes: `paykit-subscriptions-v0.1.md`

## Goal

Define the minimal Paykit protocol messages needed for a payee to ask a payer for payment over an Encrypted Link.

Payment Requests are Paykit communication objects. They coordinate request, acceptance, rejection, cancellation, and optional proof messages. Paykit does not execute payments, schedule recurring jobs, manage wallet state, or validate payment-method-specific proofs.

## Scope

This spec defines:

- Payment Request message fields
- sender and receiver role rules
- identifiers and references used to correlate messages
- request terms
- one-time and recurring request shape
- acceptance, rejection, cancellation, and proof messages
- structural validation rules
- minimum event handling expectations

This spec does not define:

- payment execution
- wallet balances
- payment-method-specific settlement
- recurring job scheduling
- retry policy
- accounting
- notifications
- local database schema
- payment-method-specific proof validation
- UI state
- recovery UX

## Roles

A Payment Request has two payment roles:

- `payee`: the party asking to receive payment
- `payer`: the party being asked to pay

In v0.2, Payment Requests are payee-initiated:

- `paykit.payment_request` MUST be sent by the payee.
- `paykit.payment_request_acceptance` MUST be sent by the payer.
- `paykit.payment_request_rejection` MUST be sent by the payer.
- `paykit.payment_proof` MUST be sent by the payer.
- `paykit.payment_request_cancellation` MAY be sent by either payer or payee.
- Existing `paykit.receipt_access` messages MAY be sent by the payee to share an optional Paykit Receipt after payment.

The Encrypted Link identifies the local party and counterparty. Payment Request messages do not need to embed both Pubky public keys. Stateless Paykit libraries cannot infer or enforce payer/payee role intent from a message alone; integrating applications, wallets, or future higher-level Paykit components MUST send each message kind only from the allowed role.

## Message transport

All Payment Request protocol messages are `pubky-noise` Private Application Messages sent over an established Encrypted Link.

An Event Message is one lifecycle message, such as a request, acceptance,
rejection, cancellation, or proof. Event Messages are FIFO within each sending
direction, not Latest-State Messages. The two directions have no protocol-level
total order.

Payment Request messages are Event Messages. Receivers MUST preserve and process
all valid recognized Payment Request events in each sender's send order. Local
record or receipt time MUST NOT be treated as proof of cross-direction causal
order.

Implementations that derive Payment Request state SHOULD consume a unified ordered stream of private messages or Payment Request protocol events. Low-level Paykit Library receive APIs should expose the ordered private message stream plus stateless parsers; per-kind convenience getters belong in higher-level SDK/runtime code that can preserve unrelated recognized Event Message kinds in a persisted event log or queue.

Paykit message kinds use logical lanes over the same Encrypted Link:

- Private Payment Lists use Latest-State Message semantics.
- Payment Request protocol messages use Event Message semantics.
- Allowance lifecycle messages use Event Message semantics.
- Receipt Access uses Event Message semantics.

Recognized Event Messages carry an `event_id` for idempotent local storage and
replay dedupe.

Private Paykit message shapes build on each other:

- Private Application Message: `version` + `kind`.
- Latest-State Message: Private Application Message where the newest valid
  message supersedes older messages of the same kind. Malformed newer messages
  do not supersede the latest valid state.
- Event Message: Private Application Message + `event_id`, where every valid message matters.
- Payment Request Event Message: Event Message + `payment_request_id`.

## Common Message Fields

Every Payment Request protocol message uses common versioned JSON fields:

```json
{
  "version": 1,
  "kind": "paykit.payment_request",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
  "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"
}
```

Rules:

- `version` MUST be `1`.
- `kind` MUST identify the message kind.
- `event_id` MUST be a UUID-v4.
- `payment_request_id` MUST be a UUID-v4.
- A retried resend of the same event MUST reuse the same `event_id` and same payload.
- Reusing the same `event_id` with different payload bytes is invalid.
- Messages MUST fit within the current `pubky-noise` message size unless a future indirection mechanism is specified.
- Message-specific fields such as `payment_reference` are added only by message kinds that need them.
- Payment Request v0.2 messages use closed-world JSON objects: unknown fields
  are invalid unless a field is explicitly defined as an open JSON object, such
  as `metadata` or `proof`.

## Identifiers and references

### Payment Request ID

`payment_request_id` identifies one Payment Request.

Rules:

- It is stable for the lifetime of the request.
- All lifecycle messages for the same request use the same `payment_request_id`.
- It is not a Payment Reference, Event ID, relationship ID, or billing period ID.
- A new request requires a new `payment_request_id`.

### Event ID

`event_id` identifies one Event Message.

Rules:

- It is used for idempotent storage and replay dedupe.
- It is stable across retries of the same event.
- Each distinct lifecycle message uses a distinct `event_id`.
- In v0.2 it is a UUID, not a hash of the event payload. Hash-based IDs would require canonical serialization rules.

### Payment Reference

`payment_reference` is a payee-provided payment correlation value. It lets the payee connect an incoming payment and Payment Proof to external state, such as an invoice, order, account, or note.

Rules:

- It MUST be a non-empty string.
- It MUST NOT exceed 256 characters.
- It MUST NOT contain control characters.
- It is not required to be a UUID.
- It is set by the payee in the Payment Request terms.
- The payer MUST copy it unchanged into Payment Proof messages for that request.
- The payer SHOULD include it in the payment-method-specific execution as a memo, reference, or remittance value when the selected payment method supports one.
- Payment-method-specific code MAY transform or omit the Payment Reference when a rail cannot safely carry it unchanged, but the Paykit Payment Proof MUST still copy the protocol `payment_reference` unchanged.
- For recurring requests, the same `payment_reference` applies to every payment for the request. The `billing_period` distinguishes the recurring period being paid.
- Per-period Payment References or templates are out of scope for v0.2.

## Payment Request terms

The `request` payload describes the terms of a one-time or recurring request.

The `payment_request_id` is carried by the top-level message and is not repeated inside `request`.

```json
{
  "amount": {
    "value": "0.001",
    "asset": "btc"
  },
  "payment_reference": "invoice-2026-0001",
  "proposal_expires_at": "2026-06-01T00:00:00Z",
  "recurrence": null,
  "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
  "metadata": {}
}
```

Rules:

- `amount` is required.
- `amount.value` MUST be a decimal string using ASCII digits with at most one
  `.` and at least one digit. Signs, exponent notation, grouping separators,
  scale limits, and zero/non-zero meaning are out of scope for v0.2.
- `amount.asset` MUST be a non-empty asset code or unit string and MUST NOT contain control characters.
- The exact asset registry and normalization rules are out of scope for v0.2.
- `amount.asset` is case-sensitive. When using the recommended Payment Endpoint
  Identifier convention, it SHOULD use the same lowercase asset string as the
  accepted endpoint identifier asset segment.
- Paykit v0.2 does not define FX, conversion, display-currency, or cross-asset
  payment semantics. When using the recommended Payment Endpoint Identifier
  convention, implementations SHOULD choose accepted endpoints whose asset
  segment matches `amount.asset`.
- `payment_reference` is required and MUST follow Payment Reference rules.
- `proposal_expires_at` is required and MUST be either `null` or an RFC3339 UTC timestamp using the `Z` suffix.
- `proposal_expires_at` is a proposal actionability deadline evaluated against
  the implementation's trusted local time when deciding whether to accept or
  display the proposal as actionable. It is not an Event Message and cannot be
  derived deterministically from event order alone.
- `recurrence` MUST be `null` for one-time requests.
- `recurrence` MUST be an object for recurring requests.
- `accepted_payment_endpoint_identifiers` MUST be a non-empty array of valid Payment Endpoint Identifiers.
- `metadata` is optional. If present, it MUST be a JSON object.
- Request terms are immutable after the initial `paykit.payment_request` event.

## Recurrence

Recurring Payment Requests use this `recurrence` shape:

```json
{
  "every": 1,
  "unit": "month",
  "starts_at": "2026-06-01T00:00:00Z",
  "anchor": "2026-06-01T00:00:00Z",
  "ends_at": null
}
```

Rules:

- `every` MUST be a positive integer.
- `unit` MUST be one of `minute`, `hour`, `day`, `week`, `month`, or `year`.
- `starts_at` MUST be an RFC3339 UTC timestamp using the `Z` suffix.
- `anchor` MUST be an RFC3339 UTC timestamp using the `Z` suffix.
- `ends_at` MUST be `null` or an RFC3339 UTC timestamp using the `Z` suffix.
- When `ends_at` is non-null, it MUST be after `starts_at`.
- Recurrence describes the requested schedule. Payment Request v0.2 does not
  define canonical calendar expansion or reusable recurrence math for SDKs.
- Implementations that materialize monthly or yearly recurrence periods SHOULD
  clamp to the last day of the target month when the anchor day does not exist,
  then return to the original anchor day when possible.

Paykit defines recurrence terms for communication. Paykit does not run the scheduler.

## Recurring Payment Requests And Subscriptions

A Subscription is product shorthand for an accepted Recurring Payment Request.
It is not a separate protocol object, message family, or SDK subsystem. A
Recurring Payment Request remains active across multiple payments until it is
cancelled or its recurrence ends.

A recurring flow works as follows:

1. The payee sends a Payment Request with non-null `recurrence` terms.
2. The payer accepts or rejects the proposal.
3. Acceptance makes the request active, but does not execute a payment or give
   the payee authority to pull funds.
4. For each Billing Period, the payer application determines that payment is
   due, obtains any authorization required by its product, executes the
   payment, and sends a Payment Proof for that period.
5. The payee validates the payment using payment-method-specific logic and may
   issue a Receipt and Receipt Access for that Billing Period.
6. A Payment Proof covers one Billing Period. It does not complete the
   Recurring Payment Request or move it out of its active recurring state.
7. Either party may cancel the request to stop future payments.

Paykit communicates the request terms and lifecycle events. Integrating
applications remain responsible for:

- calculating due Billing Periods and running any scheduler
- deciding whether each payment requires explicit user authorization
- retry policy and preventing duplicate payment for one Billing Period
- executing payments and validating method-specific settlement
- deciding grace periods and missed-payment policy
- deciding when service remains active, is restricted, or is terminated
- deciding whether and how a late payment restores service

These service and entitlement decisions are not Paykit protocol state. For
example, a subscription-gated service may create the Recurring Payment Request,
while the payer's wallet schedules each payment and submits its proof. The
service validates each period's payment and applies its own access policy.

Payment Requests and Subscriptions do not require an
[Allowance](allowances.md), and their wire messages and ordinary manual flow do
not change when Allowances are implemented. A wallet may use one matching,
accepted Allowance as prior permission to send the ordinary Acceptance and pay
automatically. This is optional wallet behavior, not a Payment Request
requirement or a guarantee of payment.

Automatic Acceptance does not imply that payment succeeded or remains in
flight. If an Allowance-aware wallet cannot complete payment after recording
Acceptance, it uses durable wallet-local execution state to decide whether the
accepted request or Billing Period is safe to present for explicit payment. It
does not send another Acceptance. This recovery path does not change the
Payment Request wire lifecycle or make every accepted request actionable.

The same matching applies to one-time and Recurring Payment Requests. Recurring
Acceptance consumes no Allowance capacity; the wallet rechecks and meters the
pinned Allowance for each Billing Period. Ending or expiring that Allowance
stops future automatic payments. Insufficient capacity blocks the current
payment and is re-evaluated for later Billing Periods. None of these conditions
rejects or cancels the Payment Request.

## paykit.payment_request

Creates a proposed Payment Request.

Sent by the payee to the payer.

```json
{
  "version": 1,
  "kind": "paykit.payment_request",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
  "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
  "request": {
    "amount": {
      "value": "0.001",
      "asset": "btc"
    },
    "payment_reference": "invoice-2026-0001",
    "proposal_expires_at": "2026-06-01T00:00:00Z",
    "recurrence": null,
    "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
    "metadata": {}
  }
}
```

Validation rules:

- Sender MUST be the payee.
- Receiver MUST be the payer.
- `request` MUST be present and valid.
- The first valid `paykit.payment_request` for a `payment_request_id` defines immutable terms.
- A later `paykit.payment_request` with the same `payment_request_id` and a
  different `event_id` is invalid, even if the terms are byte-identical.
  Retries of the original proposal MUST reuse the same `event_id` and payload.
- If `proposal_expires_at` is non-null and in the past according to the payer's
  trusted local time at the acceptance decision, the proposal is expired and
  MUST NOT be accepted.

## paykit.payment_request_acceptance

Accepts a proposed Payment Request.

Sent by the payer to the payee.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_acceptance",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
  "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"
}
```

Validation rules:

- Sender MUST be the payer.
- At the payer's acceptance decision, the request MUST be known and in the
  proposed state.
- The proposal MUST NOT be expired according to the payer's trusted local time
  at the acceptance decision. Payees that receive an acceptance after local
  expiry MAY reject or flag it according to local policy; deterministic replay
  requires the SDK/runtime to persist its local decision or processing time.
- A payee that has already recorded its own Cancellation MUST record an
  otherwise-valid later-received Acceptance from the payer as crossing, without
  reopening the request, only when no earlier Acceptance has been recorded; the
  lifecycle remains `cancelled`. A second Acceptance is invalid and MUST NOT
  replace the first recorded Acceptance. This exception is safe because the
  events occupy opposite FIFO directions and Cancellation still prevents new
  execution. An Acceptance after the payer's own earlier Cancellation is
  invalid because those events occupy the same FIFO direction.
- Acceptance is explicit. Paying without this message is not protocol-level acceptance in v0.2.

## paykit.payment_request_rejection

Rejects a proposed Payment Request.

Sent by the payer to the payee.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_rejection",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
  "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
  "reason": "user_rejected"
}
```

Validation rules:

- Sender MUST be the payer.
- The request MUST be known.
- The request MUST be in the proposed state.
- `reason` is optional and SHOULD be omitted when absent. If present, it MUST be a string; `null` is invalid.
- Rejection is terminal for that `payment_request_id`. Later acceptance or proof messages for the same request are invalid.

## paykit.payment_request_cancellation

Cancels a known non-terminal Payment Request.

Sent by either payer or payee.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_cancellation",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
  "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
  "reason": "user_requested"
}
```

Validation rules:

- Sender MUST be either the payer or payee.
- The request MUST be known.
- The request MUST be non-terminal.
- Cancellation is unilateral. No counterparty confirmation is required.
- After cancellation, payer implementations MUST NOT start new payment execution for the request.
- Cancellation does not invalidate a payment execution that the payer durably
  recorded as past its irreversible boundary before observing the cancellation.
  The payer MAY later send the ordinary Payment Proof for that execution, but
  MUST NOT use this exception for an execution started after observing
  cancellation.
- `reason` is optional and SHOULD be omitted when absent. If present, it MUST be a string; `null` is invalid.

## paykit.payment_proof

Carries method-specific evidence for one payment execution.

Sent by the payer to the payee.

The requested Payment Amount is inherited from the immutable Payment Request
terms. `paykit.payment_proof` does not repeat a generic paid amount. The `proof`
field is an opaque method-specific JSON object; Paykit v0.2 does not require
generic fields such as `type`. Rail- or processor-specific settlement details
may be included inside `proof` when needed.

```json
{
  "version": 1,
  "kind": "paykit.payment_proof",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
  "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
  "payment_reference": "invoice-2026-0001",
  "billing_period": null,
  "payment_endpoint_identifier": "btc-lightning-bolt11",
  "proof": {
    "type": "bitcoin-bolt11-preimage",
    "data": "<method-specific-proof>"
  }
}
```

Validation rules:

- Sender MUST be the payer.
- The request MUST be known and have a valid Acceptance that precedes the proof
  in the payer's FIFO event direction.
- The request MUST NOT be rejected.
- Implementations MUST reconcile repeated or corrective proofs for the same
  one-time request or Billing Period using local payment state. A later proof
  MUST NOT be treated as evidence of another payment merely because it has a
  different Event ID.
- If the request is cancelled, the payer MUST send a proof only for an execution
  it durably recorded as past its irreversible boundary before observing the
  cancellation. A receiver MUST NOT reject an otherwise-valid proof solely
  because cancellation precedes it in the receiver's event history.
- A Payment Proof after cancellation records the earlier execution but does not
  reopen the request, authorize another payment, or change its cancelled state.
- `payment_reference` MUST equal the accepted request's `payment_reference`.
- `billing_period` MUST be `null` for one-time requests.
- `billing_period` MUST be present for recurring requests.
- For recurring requests, `billing_period` identifies the claimed interval being
  paid. Paykit v0.2 validates its shape but does not define canonical recurrence
  math for proving it was derived from the accepted recurrence. Integrating
  applications, wallets, or future higher-level Paykit components that execute
  or index recurring payments SHOULD enforce recurrence eligibility according
  to their local scheduling policy.
- `payment_endpoint_identifier` MUST be one of the request's `accepted_payment_endpoint_identifiers`.
- `proof` MUST be a JSON object. Its internal fields are method-specific and are
  not interpreted by Paykit v0.2.

Paykit validates the message shape and can validate stateless request/proof
correlation fields against a known Payment Request. Integrating applications,
wallets, or future higher-level Paykit components validate whether the request
is known, has an earlier valid Acceptance, was rejected, is already processed,
or whether a recurring Billing Period is eligible under the accepted
recurrence. Whether execution crossed its irreversible boundary before the
payer observed cancellation is durable payer-wallet state and cannot be proven
from Event Message order alone. Payment-method-specific code validates whether
the proof actually proves payment.

## Billing period

For recurring requests, `billing_period` uses this shape:

```json
{
  "starts_at": "2026-06-01T00:00:00Z",
  "ends_at": "2026-07-01T00:00:00Z"
}
```

Rules:

- `starts_at` MUST be an RFC3339 UTC timestamp using the `Z` suffix.
- `ends_at` MUST be an RFC3339 UTC timestamp using the `Z` suffix.
- `ends_at` MUST be after `starts_at`.

## State derivation

Integrating applications, wallets, or future higher-level Paykit components derive local state from ordered Event Messages.

The protocol defines these minimal event-derived lifecycle states:

- `proposed`
- `accepted`
- `rejected`
- `cancelled`
- `proof_submitted`

State transitions:

```text
payment_request -> proposed
proposed + acceptance -> accepted
proposed + rejection -> rejected
accepted one-time request + payment_proof -> proof_submitted
proposed|accepted|proof_submitted + cancellation -> cancelled
proposed + crossing acceptance and cancellation -> cancelled (acceptance recorded)
cancelled request with recorded acceptance + qualifying payment_proof -> cancelled (proof recorded)
```

Terminal authorization states:

- `rejected`
- `cancelled`

Neither state can be reopened by a later Acceptance. The first crossing
Acceptance may be recorded on a cancelled request only as described above;
another Acceptance is invalid. Rejection admits no later Payment Proof.
Cancellation prevents new execution, but a qualifying
Payment Proof may still report an execution that crossed its irreversible
boundary first; recording that proof does not transition the request out of
`cancelled`.

For one-time requests, `proof_submitted` means Paykit has received a Payment
Proof event, not that payment settlement was independently verified. It is not a
Paykit protocol-final state. Wallets, processors, SDKs, or apps decide whether a
submitted proof settles the request, whether additional or corrective proofs are
accepted, and when the local request can be treated as closed. For recurring
requests, a Payment Proof reports one billing period, not completion of the
whole request.

Implementations MAY expose `proposal_expired` as a local view state for a
proposed request whose trusted local time is past `proposal_expires_at`.
`proposal_expired` is not an Event Message and is not event-log-derived.
SDKs that need deterministic audit or replay of expiry decisions should persist
the local decision or processing timestamp used for the expiry check.

Recurring request scheduling state and Billing Period recurrence eligibility are
local application or wallet state, not Paykit protocol state in v0.2.

## Changing terms

Payment Request terms are immutable in v0.2.

To change terms:

1. cancel the old Payment Request
2. create a new Payment Request with a new Payment Request ID

V0.2 does not define update messages or replacement links.

## Payment Endpoint lookup

When executing a payment, the payer chooses one of the accepted Payment Endpoint Identifiers and fetches current Payment Endpoint details.

Payment Request v0.2 does not require public Payment Endpoint lookup for execution. Implementations SHOULD prefer private/current endpoint details shared over the Encrypted Link.

Implementations MAY allow public reusable endpoint details by local policy, but Paykit does not treat public endpoint publication as the normal private Payment Request flow.

The `payment_reference` comes from the accepted Payment Request, not from the selected Payment Endpoint publication or Private Payment List.

Paykit v0.2 does not define FX, conversion, or cross-asset payment semantics. Implementations SHOULD choose endpoints whose Payment Endpoint Identifier asset segment matches `amount.asset` when using the recommended identifier convention. If an implementation accepts a different asset through local wallet or payment-processor policy, conversion and settlement semantics are outside Paykit.

Payment-method-specific code is responsible for deciding whether selected endpoint details are reusable or payment-specific. If the selected endpoint details are single-use, expired, already consumed, or otherwise stale, the payer MUST NOT execute until fresh usable details are available.

## Receipts for Payment Requests

Paykit Receipts may be issued for payments made through Payment Requests.

When a Receipt corresponds to a Payment Request, the Receipt and Receipt Access
descriptor SHOULD carry `payment_request_id` so receivers can index it without
relying on metadata conventions.

When a Receipt corresponds to a recurring Payment Request payment, the Receipt
and Receipt Access descriptor SHOULD also carry the same `billing_period` used
by the related Payment Proof. This distinguishes individual recurring payments
when the same Payment Reference is reused across periods.

Receipts that are not tied to a Payment Request omit `payment_request_id` and
`billing_period`. A `billing_period` without `payment_request_id` is invalid.

## Receipt Access Event Messages

`paykit.receipt_access` is an Event Message sent over an Encrypted Link by the
issuer of an Encrypted Receipt. It lets the receiver locate the Encrypted
Receipt on the issuer's homeserver and decrypt it locally.

Receipt Access carries:

- `event_id`: an Event ID for idempotent processing
- `receipt_id`: the Receipt ID of the Encrypted Receipt
- `payment_reference`: the Payment Reference for the receipted payment
- `payment_request_id`: optional Payment Request correlation
- `billing_period`: optional recurring Payment Request correlation
- `location`: the Receipt Location path on the issuer's homeserver
- `key`: the Receipt Decryption Key

The receipt encryption algorithm is carried by the Encrypted Receipt stored at
the Receipt Location, not by Receipt Access.

Rules:

- Receipt Access uses FIFO Event Message semantics. Receivers SHOULD persist
  every valid Receipt Access event in send order before triggering receipt
  retrieval, decryption, indexing, or other side effects.
- A retried resend of the same Receipt Access event MUST reuse the same
  `event_id` and same payload.
- Reusing the same `event_id` with different payload bytes is invalid.
- A repeated `receipt_id` with a different `event_id` is not automatically
  invalid. Receivers SHOULD reconcile it by Receipt ID and local receipt state.
  If the repeated descriptor conflicts with an already fetched/decrypted
  receipt, the SDK/runtime should fail closed or require local review.
- `location` is a path, not a complete Pubky resource. Receivers MUST interpret
  it together with the Receipt Access sender/issuer context when fetching the
  Encrypted Receipt.
- When `payment_request_id` is absent, `billing_period` MUST be absent.
- When `billing_period` is present, it SHOULD match the Billing Period used by
  the related Payment Proof.

## Event durability

Receivers SHOULD persist valid Event Messages before triggering irreversible side effects.

Implementations that trigger payment execution MUST persist valid events idempotently before side effects.

At minimum, implementations SHOULD persist:

- `event_id`
- `payment_request_id`, when applicable
- `kind`
- raw or canonical message payload
- validation result

If local derived state is lost but durable events remain, the implementation can rebuild local state from events.

If local event history is missing or inconsistent, the implementation should fail closed for automatic payment execution and require user or counterparty resync. This is local safety behavior, not Paykit protocol state.

If Encrypted Link state is lost, a new handshake may be needed to restore private communication. A new handshake does not recover missing local Payment Request history.

If an implementation persists Encrypted Link snapshots, it MUST treat the snapshot as the local read checkpoint. It MUST persist received Event Messages and dedupe state before replacing the stored snapshot with a snapshot whose read counter has advanced past those messages. A crash after event persistence but before snapshot persistence may replay messages; receivers MUST dedupe replayed Event Messages by `event_id`.

Paykit libraries may parse, order, and structurally validate messages, and should expose either raw or canonical payloads from ordered receive APIs. Durable idempotency is the caller's responsibility. Implementations MUST use persisted history to dedupe repeated `event_id`s, reject conflicting reused `event_id`s, and reject later `paykit.payment_request` events that reuse an existing `payment_request_id` with a different `event_id`.

## Open questions

1. Should Payment Proof payloads be opaque JSON objects or method-specific typed payloads?
2. Should future versions model update or replacement links between Payment Requests?
3. Should future versions define payer-requested term changes?
4. Should future versions define protocol-level resync messages?
5. Should future versions define grace periods or retry windows as protocol fields?
6. Should recurring requests support per-period Payment Reference templates or overrides?
