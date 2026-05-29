# Paykit Payment Requests Initial Spec v0.2

Status: draft / discussion only
Date: 2026-05-29
Supersedes: `paykit-subscriptions-v0.1.md`

## v0.2 decisions

This version reframes the previous subscription draft around Payment Requests.

1. A **Payment Request** is the base protocol object: a payee requests a payment from a payer.
2. A subscription is an accepted **recurring Payment Request**, not a separate protocol family.
3. `payment_request_id` identifies the long-lived request or recurring agreement.
4. `PaymentReference` identifies one concrete payment execution.
5. Payment Requests are private-first: public Payment Requests may be explored later, but implementation scope is private-only for now.
6. The old URL-secret pull design is replaced by `pubky-noise` Private Application Messages.
7. Payment Request messages are payee-initiated in v0.2.
8. Payer-initiated standing orders are outside Paykit Payment Requests in v0.2; they are wallet/runtime scheduling concerns unless future counterparty coordination is needed.
9. One-time requests use `recurrence: null`; recurring requests use an explicit recurrence object.
10. `expires_at` is required on proposals, but may be `null` for requests with no protocol-level expiry.
11. Event messages carry stable `event_id` values for dedupe, indexing, and recovery.
12. Failed payment executions are local state in v0.2 unless a future event becomes actionable.
13. Method-specific payment evidence is modeled as Payment Proof, not as Paykit Receipt.
14. The Paykit library exposes primitives; SDKs/runtimes provide automation.

## Goal

Define the minimal Paykit Payment Request model for one-time and recurring payments between Linked Peers using:

- `pubky-noise` Private Application Messages
- existing Private Payment Envelopes for payment endpoint discovery
- stable `payment_request_id` values
- stable `event_id` values for event messages
- per-payment-execution `PaymentReference` values
- method-specific `PaymentProof` messages

## Non-goals for v0.2

- No public Payment Requests in implementation scope.
- No payment-method-specific execution logic.
- No local database schema beyond conceptual indexing and recovery requirements.
- No UI/UX spec.
- No attempt to replace native recurring-payment mechanisms such as SEPA standing orders, BOLT12 recurrence, app-store subscriptions, etc.
- No payer-initiated standing order protocol in v0.2.
- No full allowance/authorization protocol. The model should be compatible with future allowance-like recurring requests, but detailed authorization limits are deferred.
- No protocol-level resync message for recovering from lost local state.

## Core principle

Paykit coordinates payment request state and payment endpoint discovery. It does not execute payments itself.

Paykit is responsible for:

- identifying the counterparty through the Encrypted Link
- exchanging Payment Request lifecycle messages
- discovering current compatible Payment Endpoints
- correlating payment executions and proofs
- giving applications/SDKs/runtimes a common state model

Payment execution remains method-specific and application/SDK/runtime-specific.

## Roles

A Payment Request has two economic roles:

- `payer`: the party expected to pay
- `payee`: the party expected to receive payment

Payment Requests are payee-initiated in v0.2:

- the sender of `paykit.payment_request` is the payee
- the receiver of `paykit.payment_request` is the payer

Common cases:

- Invoice/request for payment: payee proposes, payer accepts or pays.
- Subscription: payee proposes a recurring Payment Request, payer accepts.

Because the Encrypted Link already identifies the local party and counterparty, the request does not need to embed both Pubky public keys. Each side derives payer/payee from the message direction.

Payer-initiated recurring payments, such as standing orders, are not Payment Requests in v0.2. They can be represented as payer-side wallet, processor, SDK, or runtime scheduling state. Paykit may still be used to discover current Payment Endpoints when each scheduled payment is executed.

## Identifier model

### payment_request_id

`payment_request_id` identifies the long-lived Payment Request.

Rules:

- UUID-v4.
- Stable for the life of the request.
- Shared by all lifecycle events for the same request.
- Referenced by payment executions and payment proofs.
- For a one-time request, it identifies the single requested payment lifecycle.
- For a recurring request, it identifies the recurring payment relationship.

### event_id

`event_id` identifies one lifecycle event message.

Rules:

- UUID-v4.
- Required on every event-like Payment Request message.
- Stable across retries/resends of the same event.
- Reusing the same `event_id` with a different payload is invalid.
- Used for idempotent storage, replay dedupe, local indexing, and recovery.

`payment_request_id` identifies the request; `event_id` identifies one event within that request. Both are needed.

### PaymentReference

`PaymentReference` identifies one concrete payment execution.

Rules:

- UUID-v4.
- Created per payment execution, not per Payment Request and not per billing period.
- Referenced by the corresponding method-specific `PaymentProof`.
- Multiple payment executions may exist for the same Payment Request and billing period.
- Retries must use new `PaymentReference` values unless explicitly modeled as the same execution by a payment-method-specific layer.

Rationale:

A billing period can have multiple payment executions: failed Lightning payment, fallback onchain payment, retry with refreshed endpoint, etc. Execution-level references make proofs precise and avoid overloading one ID across ambiguous retry flows.

## Privacy scope

Payment Requests are private-only for current implementation.

All Payment Request lifecycle messages use the same private encrypted communication direction as Paykit private payments: `pubky-noise` Private Application Messages over an established Encrypted Link.

Public Payment Requests may be explored later, but they are out of implementation scope for v0.2.

## Transport model

The old URL-secret pull model is not used in v0.2.

Instead:

- Payment Request proposals
- acceptances
- rejections
- updates
- pauses
- resumes
- cancellations
- payment proofs

are exchanged as typed `pubky-noise` Private Application Messages.

This aligns with current Paykit private payment infrastructure and avoids bearer-secret URL semantics.

## State model

Payment Request state is exchanged as private event messages and indexed locally by each participant's application, SDK, or runtime.

The Paykit protocol messages form the synchronization/audit stream. Local durable storage provides efficient querying, scheduling, retries, recovery, and accounting.

The canonical state of a Payment Request is derived from ordered events, not from a latest-state object.

## Event semantics

Payment Request lifecycle messages are Event Messages.

They must not use Latest-State Message semantics.

Receivers must preserve and process all valid recognized Payment Request events in order. Typed getters for one recognized Event Message kind must not discard unrelated recognized Event Message kinds.

Unsupported Private Application Message kinds may follow the library's normal unsupported-message policy until Paykit explicitly recognizes them. Once a Payment Request message kind is recognized, implementations must preserve all valid messages of that kind in send order.

## Recovery and durability expectations

Homeservers are transport, not the durable source of local application state.

Applications, SDKs, or runtimes that process Payment Request events must persist enough local state before treating an event as handled:

- `event_id`
- `payment_request_id`
- message `kind`
- raw message or canonical parsed payload
- validation result
- derived Payment Request state
- local payment execution indexes and payment proof indexes
- latest Encrypted Link snapshot and read progress

Recommended receive flow:

1. Receive and decrypt events.
2. Validate each event.
3. Persist valid events durably and idempotently by `event_id`.
4. Update derived request state.
5. Persist the latest Encrypted Link snapshot/read progress.
6. Only then treat the events as handled for scheduling or other irreversible side effects.

Risks if this is not done:

- missed cancellation or pause events
- duplicate automatic payments
- payments made under stale terms
- lost payment proof indexing
- local state diverging between payer and payee
- inability to safely resume after restart

If local derived state is lost but durable events and link progress remain, the runtime should rebuild state from stored events and resume from the saved link snapshot.

If local event history and link progress are missing or inconsistent, the runtime must not guess. It should mark affected requests as `recovery_required` (or equivalent), stop automatic execution, and require explicit user/counterparty resync. A protocol-level resync message may be added later, but is out of scope for v0.2.

## Message envelope conventions

All private Paykit Payment Request messages are versioned JSON envelopes.

Common fields:

```json
{
  "version": 1,
  "kind": "paykit.payment_request",
  "event_id": "650e8400-e29b-41d4-a716-446655440000",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

Rules:

- `version` is currently `1`.
- `kind` identifies the message type.
- `event_id` is required on every event-like message.
- `payment_request_id` is required on every Payment Request lifecycle message.
- Message payloads must fit within the current `pubky-noise` message size unless a future chunking or indirection mechanism is explicitly specified.

## Core request payload

The `request` payload describes the terms for one requested payment or recurring payment relationship.

The stable `payment_request_id` is carried by the message envelope, not repeated inside the `request` payload.

Initial one-time shape:

```json
{
  "amount": {
    "value": "10.00",
    "currency": "USD"
  },
  "expires_at": "2026-06-01T00:00:00Z",
  "recurrence": null,
  "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
  "metadata": {}
}
```

Initial recurring shape:

```json
{
  "amount": {
    "value": "10.00",
    "currency": "USD"
  },
  "expires_at": "2026-06-01T00:00:00Z",
  "recurrence": {
    "every": 1,
    "unit": "month",
    "starts_at": "2026-06-01T00:00:00Z",
    "anchor": "2026-06-01T00:00:00Z",
    "ends_at": null
  },
  "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11", "btc-lightning-bolt12"],
  "metadata": {}
}
```

### amount

Amount uses string decimal + currency code.

Rules:

- `value` is a decimal string.
- `currency` is a currency or asset code.
- Exact currency/asset registry is out of scope for v0.2.
- Payment-method-specific execution code is responsible for converting this into method-specific payment details.

Open detail for later:

- Whether currency codes should follow ISO 4217, asset identifiers, method-specific IDs, or Paykit-defined conventions.

### expires_at

`expires_at` defines when the proposed request stops being actionable, or `null` when the request has no protocol-level expiry.

Rules:

- The `expires_at` field is required on every Payment Request proposal.
- The value must be either `null` or an RFC3339 UTC timestamp using the `Z` suffix, such as `2026-06-01T00:00:00Z`.
- `null` means the protocol does not define an expiry for the proposal.
- Implementations may reject `expires_at: null` or apply a local maximum expiry by policy.
- Before acceptance, a proposal with a non-null `expires_at` in the past must be rejected.
- For one-time Payment Requests with non-null `expires_at`, automatic payment execution must not start after `expires_at` unless an implementation has explicit user approval to override expiry.
- For recurring Payment Requests, `expires_at` applies to proposal acceptance. After acceptance, recurrence timing is controlled by `recurrence.starts_at`, `recurrence.anchor`, and `recurrence.ends_at`.

### recurrence

`recurrence` is `null` for a one-time Payment Request.

For recurring Payment Requests, recurrence uses interval count + unit + explicit anchors.

Shape:

```json
{
  "every": 1,
  "unit": "month",
  "starts_at": "2026-06-01T00:00:00Z",
  "anchor": "2026-06-01T00:00:00Z",
  "ends_at": null
}
```

Allowed schema units:

- `minute`
- `hour`
- `day`
- `week`
- `month`
- `year`

Rules:

- `every` is a positive integer.
- `unit` identifies the recurrence unit.
- `starts_at` defines when the recurring request becomes eligible for payment.
- `anchor` defines the recurring schedule anchor.
- `ends_at` is optional and may be `null`.
- Time values must be RFC3339 UTC timestamps using the `Z` suffix.
- For monthly and yearly recurrence, if the anchor day does not exist in a target month, the due date is clamped to the last day of that month and returns to the original anchor day when that day exists again.
- Example: a monthly request anchored on January 31 is due on February 28 or 29, then March 31, then April 30.
- Cron-like schedules are out of scope.
- ISO8601 durations are out of scope for v0.2.
- Implementations may support only a subset of allowed schema units. Unsupported units must be rejected before acceptance and should be documented by the implementation.

Open detail for later:

- Grace periods.
- Retry windows.
- Timezone/user-local billing semantics.

### accepted_payment_endpoint_identifiers

`accepted_payment_endpoint_identifiers` is the non-empty list of Payment Endpoint Identifiers allowed for the request.

Rules:

- Each entry must be a valid `PaymentEndpointIdentifier`.
- The payer may still apply local Payment Selection Policy within this allowed set.
- Empty lists are invalid in v0.2. A future version may add explicit "any mutually supported endpoint" semantics if needed.

## Message kinds

### paykit.payment_request

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
      "currency": "USD"
    },
    "expires_at": "2026-06-01T00:00:00Z",
    "recurrence": null,
    "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
    "metadata": {}
  }
}
```

Validation rules:

- `event_id` must be UUID-v4.
- `payment_request_id` must be UUID-v4.
- The sender must be the payee and the receiver must be the payer.
- `amount.value` must be a decimal string.
- `expires_at` must be `null` or a valid RFC3339 UTC timestamp using the `Z` suffix. A non-null `expires_at` must not already be expired when accepted.
- `recurrence` must be `null` or valid recurrence.
- `accepted_payment_endpoint_identifiers` must be non-empty.
- Each accepted identifier must be a valid `PaymentEndpointIdentifier`.

### paykit.payment_request_acceptance

Accepts a Payment Request proposal or update proposal.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_acceptance",
  "event_id": "650e8400-e29b-41d4-a716-446655440001",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "accepted_event_id": "650e8400-e29b-41d4-a716-446655440000"
}
```

Rules:

- Must refer to a known proposal or update proposal.
- Must be sent by the payer.
- The accepted event is identified by `accepted_event_id`. Reusing the same `event_id` with different payload bytes is invalid.

### paykit.payment_request_rejection

Rejects a Payment Request proposal or update proposal.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_rejection",
  "event_id": "650e8400-e29b-41d4-a716-446655440002",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "rejected_event_id": "650e8400-e29b-41d4-a716-446655440000",
  "reason": "user_rejected"
}
```

Rules:

- Must be sent by the payer.

### paykit.payment_request_update

Proposes changed terms for an existing accepted Payment Request.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_update",
  "event_id": "650e8400-e29b-41d4-a716-446655440003",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "request": {
    "amount": {
      "value": "12.00",
      "currency": "USD"
    },
    "expires_at": "2026-07-01T00:00:00Z",
    "recurrence": {
      "every": 1,
      "unit": "month",
      "starts_at": "2026-07-01T00:00:00Z",
      "anchor": "2026-07-01T00:00:00Z",
      "ends_at": null
    },
    "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
    "metadata": {}
  }
}
```

Rules:

- Updates are Event Messages.
- Updates are sent by the payee to the payer.
- `request` is the complete proposed replacement terms, not a partial patch.
- Updated terms must satisfy the same validation rules as an initial Payment Request.
- Updates require counterparty acceptance before becoming active.
- While an update is pending, automatic payments for the current accepted request are paused/stopped until the update is accepted, rejected, cancelled, or expired. This avoids needing to settle differences for payments due during the update proposal window.

### paykit.payment_request_pause

Pauses future automatic execution for a recurring Payment Request.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_pause",
  "event_id": "650e8400-e29b-41d4-a716-446655440004",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "reason": "user_requested"
}
```

### paykit.payment_request_resume

Resumes automatic execution for a paused recurring Payment Request.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_resume",
  "event_id": "650e8400-e29b-41d4-a716-446655440005",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

### paykit.payment_request_cancellation

Cancels a Payment Request.

```json
{
  "version": 1,
  "kind": "paykit.payment_request_cancellation",
  "event_id": "650e8400-e29b-41d4-a716-446655440006",
  "payment_request_id": "550e8400-e29b-41d4-a716-446655440000",
  "reason": "user_requested"
}
```

Rules:

- Either payer or payee may cancel.
- Cancellation is an Event Message.
- After cancellation, payer runtimes must not execute new payments for the request.

Open detail for later:

- Whether cancellation can be unilateral or must be acknowledged.

### paykit.payment_proof

Carries method-specific proof for one payment execution.

```json
{
  "version": 1,
  "kind": "paykit.payment_proof",
  "event_id": "650e8400-e29b-41d4-a716-446655440008",
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

Rules:

- Payment Proof is a separate concept from Paykit Receipt.
- `payment_reference` must match the payment execution being proven.
- `billing_period` is `null` for one-time requests.
- For recurring requests, `billing_period` identifies the period being paid.
- `payment_endpoint_identifier` identifies which method-specific proof rules apply.
- Paykit stores/transports the proof; method-specific code validates it.
- Paykit Receipt and Receipt Access remain separate optional artifacts.
- Failed payment executions are local state in v0.2 unless a future protocol event becomes actionable for the counterparty.

For recurring requests, `billing_period` has this shape:

```json
{
  "starts_at": "2026-06-01T00:00:00Z",
  "ends_at": "2026-07-01T00:00:00Z"
}
```

Open detail for later:

- Whether proof payloads should be opaque strings, structured JSON, or method-specific typed envelopes.
- Whether a Payment Proof may include a Paykit Receipt Location or Receipt Access reference.

## Payment endpoint refresh requests

Refreshing private payment details is a separate concept from recording local payment execution state.

If a payer needs fresher payment details before paying, a future protocol version may add an explicit refresh request message, such as `paykit.payment_endpoint_refresh_request`. That message should not be modeled as `paykit.payment_attempt`, because payment attempts are local runtime state about trying to pay, while a refresh request only asks the payee to publish or send updated receiving details.

## One-time Payment Request flow v0.2

1. Payee creates `paykit.payment_request` with `recurrence: null`.
2. Payer validates the terms.
3. Payer sends `paykit.payment_request_acceptance`, or pays directly if the implementation treats payment as implicit acceptance.
4. Payer selects an allowed Payment Endpoint.
5. Payer fetches current private payment details for the payee.
6. Payer generates a new `PaymentReference`.
7. Payer executes the payment with method-specific code.
8. Payer sends `paykit.payment_proof` with method-specific proof.
9. Both sides index the proof locally.

## Recurring Payment Request flow v0.2

A subscription is an accepted recurring Payment Request.

1. Payee creates `paykit.payment_request` with a non-null `recurrence`.
2. Payer validates the terms.
3. Payer sends `paykit.payment_request_acceptance`.
4. Both sides index the accepted recurring request locally.

On each due interval:

1. Payer runtime derives the billing period from the accepted recurrence.
2. Payer runtime selects an allowed Payment Endpoint.
3. Payer runtime fetches current private payment details for the payee.
4. Payer runtime generates a new `PaymentReference` for this execution.
5. Payer runtime executes the payment with method-specific code.
6. Payer runtime sends `paykit.payment_proof` with method-specific proof and billing period.
7. Both sides index the proof locally.

## Important property

Payment Requests are payee-initiated and payer-controlled at execution time.

The payee cannot directly pull funds in v0.2. The payee can only provide receiving details, receive lifecycle messages, and verify payment proofs.

Future allowance or pull-style flows need additional authorization messages and are out of scope for v0.2.

Payer-initiated recurring payments are standing-order-like scheduling behavior. They are outside Payment Request v0.2 because they do not require Paykit lifecycle coordination with the payee beyond normal Payment Endpoint discovery.

## Library vs SDK/runtime responsibilities

### Paykit library

The library should expose primitives for apps, SDKs, and runtimes:

- typed Payment Request payload structs
- typed Payment Proof payload structs
- JSON serialization/deserialization
- validation for IDs, references, Payment Endpoint Identifiers, recurrence, amount shape, and expiry
- private message send/receive helpers for Payment Request and Payment Proof message kinds
- ordered Event Message retrieval semantics for lifecycle events

The library should not:

- schedule recurring jobs
- execute payments
- persist app/runtime state
- decide payment-method-specific policy

### SDK/runtime automation

Automatic one-time or recurring Payment Requests require runtime behavior somewhere in the integrating app, wallet, processor backend, SDK, or service.

The SDK/runtime provides automation:

- persistent Payment Request event index
- derived state index
- recurrence evaluation
- payer-side tracking of accepted recurring Payment Requests and outstanding payments
- retry policy
- payment-method execution integration
- payment proof generation
- payment proof validation/indexing
- accounting and query API
- recovery handling

A standalone service is one possible packaging for this runtime, but not a protocol requirement.

## State derivation

A local implementation derives Payment Request state from ordered events.

Minimal states:

- `proposed`
- `accepted`
- `active`
- `pending_update`
- `paused`
- `cancelled`
- `completed`
- `expired`
- `recovery_required`

State transitions:

```text
payment_request -> proposed
proposed + acceptance -> accepted
accepted one-time request + payment_proof -> completed
accepted recurring request + recurrence starts -> active
active + pause -> paused
paused + resume -> active
active|paused + cancellation -> cancelled
active + update -> pending_update
pending_update + acceptance -> active under new terms
pending_update + rejection|expiry|cancellation -> active under previous terms, paused, or cancelled as indicated by the event
active + recurrence end reached -> completed
proposal/update with non-null expires_at past expiry -> expired
lost or inconsistent local event/link state -> recovery_required
```

## Validation invariants

- `payment_request_id` must be UUID-v4.
- `event_id` must be UUID-v4 and unique per event.
- `PaymentReference` must be UUID-v4 and is per payment execution.
- Every lifecycle event must include `payment_request_id`.
- Every event-like message must include `event_id`.
- Payment Request proposals and updates must be sent by the payee to the payer in v0.2.
- Every payment proof must include `payment_request_id`, `payment_reference`, `billing_period`, and `payment_endpoint_identifier`.
- All non-null timestamps must be RFC3339 UTC timestamps using the `Z` suffix.
- Private Payment Request messages must be versioned JSON envelopes.
- Private Payment Request messages must preserve event order.
- Private Payment Request messages must fit within `pubky-noise` message size unless a future indirection mechanism is specified.
- Payment Request implementation is private-only for now.
- Public Payment Requests are future work.
- URL-secret pull requests are not part of v0.2.

## Open questions for v0.3

1. Are grace periods part of the Payment Request object or runtime policy?
2. Are retry windows part of the Payment Request object or runtime policy?
3. What is the proof envelope shape for `paykit.payment_proof`?
4. Should proof validation be part of Paykit Payment Endpoint specs?
5. Is an explicit `paykit.payment_endpoint_refresh_request` message needed?
6. Can either party unilaterally pause a recurring Payment Request, or only the payer?
7. Should payer-requested term changes be modeled as a future separate event, or remain out of protocol scope?
8. How should implementations handle conflicting simultaneous events?
9. What local runtime DB indexes are minimally required?
10. What protocol-level resync message is needed if local event history is lost?
11. How should future allowance/pull-style authorization build on recurring Payment Requests?
