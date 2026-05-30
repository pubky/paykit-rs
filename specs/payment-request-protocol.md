# Paykit Payment Request Protocol v0.2

Status: draft / discussion only
Date: 2026-05-29
Supersedes: `paykit-subscriptions-v0.1.md`

## Goal

Define the minimal Paykit protocol messages needed for a payee to ask a payer for payment over an Encrypted Link.

Payment Requests are Paykit communication objects. They coordinate request, acceptance, rejection, cancellation, and optional proof messages. Paykit does not execute payments, schedule recurring jobs, manage wallet state, or validate payment-method-specific proofs.

## Scope

This spec defines:

- Payment Request message envelopes
- sender and receiver role rules
- identifiers used to correlate messages
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

The Encrypted Link identifies the local party and counterparty. Payment Request messages do not need to embed both Pubky public keys.

## Message transport

All Payment Request protocol messages are `pubky-noise` Private Application Messages sent over an established Encrypted Link.

Payment Request messages are Event Messages. Receivers MUST preserve and process all valid recognized Payment Request events in send order.

Typed getters for one recognized Event Message kind MUST NOT discard unrelated recognized Event Message kinds.

## Common envelope

Every Payment Request protocol message uses a versioned JSON envelope:

```json
{
  "version": 1,
  "kind": "paykit.payment_request",
  "event_id": "650e8400-e29b-41d4-a716-446655440000",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000"
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

## Identifiers

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

### Payment Reference

`payment_reference` identifies one concrete payment execution for correlation with Payment Proofs and related artifacts.

Rules:

- It MUST be a UUID-v4.
- It is created per payment execution, not per Payment Request and not per billing period.
- Multiple payment executions MAY exist for the same Payment Request and billing period.
- Retries SHOULD use new Payment References unless a payment-method-specific layer explicitly treats them as the same execution.

## Payment Request terms

The `request` payload describes the terms of a one-time or recurring request.

The `payment_request_id` is carried by the message envelope and is not repeated inside `request`.

```json
{
  "amount": {
    "value": "10.00",
    "asset": "USD"
  },
  "expires_at": "2026-06-01T00:00:00Z",
  "recurrence": null,
  "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
  "metadata": {}
}
```

Rules:

- `amount` is required.
- `amount.value` MUST be a decimal string.
- `amount.asset` MUST be a currency or asset code.
- The exact asset registry is out of scope for v0.2.
- `expires_at` is required and MUST be either `null` or an RFC3339 UTC timestamp using the `Z` suffix.
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
- Monthly and yearly recurrence MUST clamp to the last day of the target month when the anchor day does not exist, then return to the original anchor day when possible.

Paykit defines recurrence terms for communication. Paykit does not run the scheduler.

## paykit.payment_request

Creates a proposed Payment Request.

Sent by the payee to the payer.

```json
{
  "version": 1,
  "kind": "paykit.payment_request",
  "event_id": "650e8400-e29b-41d4-a716-446655440000",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "request": {
    "amount": {
      "value": "10.00",
      "asset": "USD"
    },
    "expires_at": "2026-06-01T00:00:00Z",
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
- A conflicting later `paykit.payment_request` with the same `payment_request_id` is invalid.
- If `expires_at` is non-null and in the past, the proposal is expired and MUST NOT be accepted.

## paykit.payment_request_acceptance

Accepts a proposed Payment Request.

Sent by the payer to the payee.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_acceptance",
  "event_id": "650e8400-e29b-41d4-a716-446655440001",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

Validation rules:

- Sender MUST be the payer.
- The request MUST be known.
- The request MUST be in the proposed state.
- The request MUST NOT be expired.
- Acceptance is explicit. Paying without this message is not protocol-level acceptance in v0.2.

## paykit.payment_request_rejection

Rejects a proposed Payment Request.

Sent by the payer to the payee.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_rejection",
  "event_id": "650e8400-e29b-41d4-a716-446655440002",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "reason": "user_rejected"
}
```

Validation rules:

- Sender MUST be the payer.
- The request MUST be known.
- The request MUST be in the proposed state.
- `reason` is optional. If present, it MUST be a string.
- Rejection is terminal for that `payment_request_id`. Later acceptance or proof messages for the same request are invalid.

## paykit.payment_request_cancellation

Cancels a known non-terminal Payment Request.

Sent by either payer or payee.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_cancellation",
  "event_id": "650e8400-e29b-41d4-a716-446655440003",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "reason": "user_requested"
}
```

Validation rules:

- Sender MUST be either the payer or payee.
- The request MUST be known.
- The request MUST be non-terminal.
- Cancellation is unilateral. No counterparty confirmation is required.
- After cancellation, payer runtimes MUST NOT start new payment execution for the request.
- `reason` is optional. If present, it MUST be a string.

## paykit.payment_proof

Carries method-specific evidence for one payment execution.

Sent by the payer to the payee.

```json
{
  "version": 1,
  "kind": "paykit.payment_proof",
  "event_id": "650e8400-e29b-41d4-a716-446655440004",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "payment_reference": "550e8400-e29b-41d4-a716-446655440001",
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
- The request MUST be known and accepted.
- The request MUST NOT be cancelled or expired.
- `payment_reference` MUST be a UUID-v4.
- `billing_period` MUST be `null` for one-time requests.
- `billing_period` MUST be present for recurring requests.
- For recurring requests, `billing_period` MUST identify an interval derived from the accepted recurrence.
- `payment_endpoint_identifier` MUST be one of the request's `accepted_payment_endpoint_identifiers`.
- `proof` MUST be a JSON object.

Paykit validates the envelope and correlation fields. Payment-method-specific code validates whether the proof actually proves payment.

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

Wallets, SDKs, or runtimes derive local state from ordered Event Messages.

The protocol defines these minimal shared lifecycle states:

- `proposed`
- `accepted`
- `rejected`
- `cancelled`
- `completed`
- `expired`

State transitions:

```text
payment_request -> proposed
proposed + acceptance -> accepted
proposed + rejection -> rejected
proposed|accepted + cancellation -> cancelled
accepted one-time request + payment_proof -> completed
proposal with non-null expires_at past expiry -> expired
```

Recurring request scheduling state is local runtime state, not Paykit protocol state.

## Changing terms

Payment Request terms are immutable in v0.2.

To change terms:

1. cancel the old Payment Request
2. create a new Payment Request with a new Payment Request ID

V0.2 does not define update messages or replacement links.

## Payment Endpoint lookup

When executing a payment, the payer chooses one of the accepted Payment Endpoint Identifiers and fetches current Payment Endpoint details.

For public endpoints, the payer may use Pubky Routing public storage.

For private endpoints, the payee may have shared a Private Payment Envelope.

The Payment Reference carried by an existing Private Payment Envelope is independent from the `payment_reference` used in a Payment Proof unless a future protocol version defines a stronger relationship. Payment Request payment execution SHOULD create its own Payment Reference for the proof.

## Event durability

Receivers SHOULD persist valid Event Messages before triggering irreversible side effects.

Runtimes that trigger payment execution MUST persist valid events idempotently before side effects.

At minimum, runtimes SHOULD persist:

- `event_id`
- `payment_request_id`
- `kind`
- raw or canonical message payload
- validation result

If local derived state is lost but durable events remain, the runtime can rebuild local state from events.

If local event history is missing or inconsistent, the runtime should fail closed for automatic payment execution and require user or counterparty resync. This is local safety behavior, not Paykit protocol state.

If Encrypted Link state is lost, a new handshake may be needed to restore private communication. A new handshake does not recover missing local Payment Request history.

## Open questions

1. Should Payment Proof payloads be opaque JSON objects or method-specific typed envelopes?
2. Should Payment Proofs be allowed after cancellation if the payment execution happened before cancellation was received?
3. Should future versions model update or replacement links between Payment Requests?
4. Should future versions define payer-requested term changes?
5. Should future versions define protocol-level resync messages?
6. Should future versions define grace periods or retry windows as protocol fields?
