# Paykit Subscriptions Initial Spec v0.2

Status: draft / discussion only  
Date: 2026-05-19  
Supersedes: `paykit-subscriptions-v0.1.md`

## v0.2 decisions

This version incorporates the first design decisions after v0.1.

1. `PaymentReference` is **per payment attempt**.
2. `subscription_id` exists separately from `PaymentReference`.
3. Subscriptions are private-first: public subscriptions may be mentioned as future work, but implementation scope is private-only for now.
4. v0.2 defines a common model, but specifies push subscriptions first.
5. The old URL-secret pull design is replaced by `pubky-noise` private messages.
6. Subscription proposals may be created by either payer or payee.
7. Schedule format is simple interval + explicit anchors.
8. Amount format is string decimal + currency code.
9. Receipts are method-specific proof containers.
10. Subscription state is exchanged as private messages and indexed by local daemon DBs.
11. Pause, resume, cancel, and update are event messages.
12. The Paykit library exposes primitives; the Paykit daemon provides automation.

## Goal

Define the minimal Paykit subscription model for recurring payments between known peers using:

- `pubky-noise` private messages
- existing Paykit private payment lists
- stable `subscription_id` values
- per-attempt `PaymentReference` values
- future method-specific `PaymentReceipt` proof messages

## Non-goals for v0.2

- No public subscriptions in implementation scope.
- No public payment receipts.
- No URL-secret pull subscription model.
- No payment-method-specific execution logic.
- No daemon database schema beyond conceptual indexing requirements.
- No UI/UX spec.
- No attempt to replace native recurring-payment mechanisms such as SEPA standing orders, BOLT12 recurrence, app-store subscriptions, etc.

## Core principle

Paykit coordinates recurring payment state and payment-method discovery. It does not execute payments itself.

Paykit is responsible for:

- identifying the counterparty
- exchanging subscription agreement messages
- discovering current compatible payment methods/endpoints
- correlating attempts and receipts
- giving applications/daemons a common state model

Payment execution remains method-specific and application/daemon-specific.

## Roles

A subscription has two economic roles:

- `payer`: the party expected to pay periodically
- `payee`: the party expected to receive payments

Either party may create the initial proposal.

Common cases:

- Merchant subscription: payee proposes, payer accepts.
- Recurring donation: payer proposes, payee accepts.

## Identifier model

### subscription_id

`subscription_id` identifies the long-lived subscription agreement.

Rules:

- UUID-v4.
- Stable for the life of the subscription.
- Shared by all lifecycle events for the same subscription.
- Referenced by payment attempts and receipts.

### PaymentReference

`PaymentReference` identifies one concrete payment attempt.

Rules:

- UUID-v4.
- Created per payment attempt, not per subscription and not per billing period.
- Referenced by the corresponding method-specific `PaymentReceipt`.
- Multiple payment attempts may exist for the same subscription and billing period.
- Retries must use new `PaymentReference` values unless explicitly modeled as the same attempt by a payment-method-specific layer.

Rationale:

A billing period can have multiple attempts: failed Lightning payment, fallback onchain payment, retry with refreshed endpoint, etc. Attempt-level references make receipts precise and avoid overloading one ID across ambiguous retry flows.

## Privacy scope

Subscriptions are private-only for current implementation.

All subscription lifecycle messages use the same private encrypted communication direction as Paykit private payments: `pubky-noise` private messages over an established encrypted link.

Public subscriptions may be explored later, but they are out of implementation scope for v0.2.

## Transport model

The old URL-secret pull model is not used in v0.2.

Instead:

- subscription proposals
- acceptances
- updates
- cancellations
- payment requests
- receipts

are exchanged as typed `pubky-noise` private messages.

This aligns with current Paykit private payment infrastructure and avoids bearer-secret URL semantics.

## State model

Subscription state is exchanged as private event messages and indexed locally by each participant's daemon or application.

The Paykit protocol messages form the synchronization/audit stream. Local daemon storage provides efficient querying, scheduling, retries, and accounting.

The canonical state of a subscription is derived from ordered events, not from a latest-state object.

## Event semantics

Subscription lifecycle messages are event-like.

They must not use latest-state semantics.

Receivers must preserve and process all valid messages in order. Typed getters for one message kind must not discard unrelated message kinds.

This matches the existing `EncryptedLink` buffering direction in `paykit-rs`, where unknown private message kinds are preserved for future typed receivers.

## Message envelope conventions

All private Paykit subscription messages are versioned JSON envelopes.

Common fields:

```json
{
  "version": 1,
  "kind": "paykit.subscription_proposal",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

Rules:

- `version` is currently `1`.
- `kind` identifies the message type.
- `subscription_id` is required on every subscription lifecycle message.
- Message payloads must fit within the current `pubky-noise` message size unless a future chunking or indirection mechanism is explicitly specified.

## Core object: SubscriptionAgreement

A `SubscriptionAgreement` describes the recurring payment relationship.

Initial shape:

```json
{
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
  "payer": "<payer-pubky>",
  "payee": "<payee-pubky>",
  "amount": {
    "value": "10.00",
    "currency": "USD"
  },
  "schedule": {
    "interval": "monthly",
    "starts_at": "2026-06-01T00:00:00Z",
    "anchor": "2026-06-01T00:00:00Z",
    "ends_at": null
  },
  "accepted_methods": ["bitcoin-bolt11", "bitcoin-bolt12"],
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

- Whether currency codes should follow ISO 4217, asset identifiers, method-specific IDs, or Paykit-defined method conventions.

### schedule

Schedule uses simple interval + explicit anchors.

Initial shape:

```json
{
  "interval": "monthly",
  "starts_at": "2026-06-01T00:00:00Z",
  "anchor": "2026-06-01T00:00:00Z",
  "ends_at": null
}
```

Allowed initial intervals:

- `daily`
- `weekly`
- `monthly`
- `yearly`

Rules:

- `starts_at` defines when the subscription becomes eligible for payment.
- `anchor` defines the recurring schedule anchor.
- `ends_at` is optional and may be `null`.
- Time values are UTC timestamps.
- Cron-like schedules are out of scope.
- ISO8601 durations are out of scope for v0.2.

Open detail for later:

- Month-end behavior: e.g. subscription anchored on January 31.
- Grace periods.
- Retry windows.
- Timezone/user-local billing semantics.

### accepted_methods

`accepted_methods` is a list of Paykit method IDs allowed for the subscription.

Rules:

- Each method must be a valid Paykit `MethodId`.
- The payer daemon may still apply local payment-selection policy within this allowed set.
- If empty, the payer daemon may treat any mutually supported method as allowed, unless later forbidden by policy.

Open detail for later:

- Whether empty `accepted_methods` should be valid or rejected.

## Message kinds

### paykit.subscription_proposal

Creates a proposed subscription agreement.

May be sent by payer or payee.

```json
{
  "version": 1,
  "kind": "paykit.subscription_proposal",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
  "agreement": {
    "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
    "payer": "<payer-pubky>",
    "payee": "<payee-pubky>",
    "amount": {
      "value": "10.00",
      "currency": "USD"
    },
    "schedule": {
      "interval": "monthly",
      "starts_at": "2026-06-01T00:00:00Z",
      "anchor": "2026-06-01T00:00:00Z",
      "ends_at": null
    },
    "accepted_methods": ["bitcoin-bolt11"],
    "metadata": {}
  }
}
```

Validation rules:

- Envelope `subscription_id` must equal `agreement.subscription_id`.
- `payer` and `payee` must be valid Pubky public keys.
- Sender must be either payer or payee.
- `amount.value` must be a decimal string.
- `schedule.interval` must be one of the allowed interval values.
- `accepted_methods` entries must be valid `MethodId` values.

### paykit.subscription_acceptance

Accepts a proposal.

```json
{
  "version": 1,
  "kind": "paykit.subscription_acceptance",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
  "accepted_proposal_hash": "<hash-of-proposal-message>"
}
```

Rules:

- Must refer to a known proposal.
- Must be sent by the counterparty who did not create the proposal.
- The accepted proposal hash binds acceptance to exact terms.

Open detail for later:

- Hash algorithm and canonical JSON rules.

### paykit.subscription_update

Proposes changed terms for an existing subscription.

```json
{
  "version": 1,
  "kind": "paykit.subscription_update",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
  "agreement": { }
}
```

Rules:

- Updates are event messages.
- Updates should require counterparty acceptance before becoming active.
- Exact update/acceptance flow is not finalized in v0.2.

### paykit.subscription_pause

Pauses future automatic execution.

```json
{
  "version": 1,
  "kind": "paykit.subscription_pause",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
  "reason": "user_requested"
}
```

### paykit.subscription_resume

Resumes automatic execution.

```json
{
  "version": 1,
  "kind": "paykit.subscription_resume",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

### paykit.subscription_cancellation

Cancels a subscription.

```json
{
  "version": 1,
  "kind": "paykit.subscription_cancellation",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
  "reason": "user_requested"
}
```

Rules:

- Either payer or payee may cancel.
- Cancellation is an event.
- After cancellation, payer daemon must not execute new payment attempts for the subscription.

Open detail for later:

- Whether cancellation can be unilateral or must be acknowledged.

### paykit.payment_attempt

Records that a payer daemon is attempting a payment for a subscription.

```json
{
  "version": 1,
  "kind": "paykit.payment_attempt",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
  "payment_reference": "550e8400-e29b-41d4-a716-446655440001",
  "billing_period": {
    "starts_at": "2026-06-01T00:00:00Z",
    "ends_at": "2026-07-01T00:00:00Z"
  },
  "method_id": "bitcoin-bolt11"
}
```

Rules:

- `payment_reference` is generated per attempt.
- Multiple attempts may exist for the same billing period.
- Each attempt should result in either a method-specific receipt or a local failure record.

Open detail for later:

- Whether failed attempts are sent to the counterparty or kept local.

### paykit.payment_receipt

Carries method-specific proof for one payment attempt.

```json
{
  "version": 1,
  "kind": "paykit.payment_receipt",
  "subscription_id": "550e8400-e29b-41d4-a716-446655440000",
  "payment_reference": "550e8400-e29b-41d4-a716-446655440001",
  "method_id": "bitcoin-bolt11",
  "proof": {
    "type": "bitcoin-bolt11-preimage",
    "data": "<method-specific-proof>"
  }
}
```

Rules:

- Receipts are method-specific proof containers.
- `payment_reference` must match the payment attempt being proven.
- `method_id` identifies which method-specific proof rules apply.
- Paykit stores/transports the proof; method-specific code validates it.
- Generic claimed-payment receipts are out of scope for current Paykit implementation.

Open detail for later:

- Whether proof payloads should be opaque strings, structured JSON, or method-specific typed envelopes.

## Push subscription flow v0.2

Push is the first implementation target.

### Proposal and acceptance

1. Either payer or payee creates `paykit.subscription_proposal`.
2. Counterparty validates the terms.
3. Counterparty sends `paykit.subscription_acceptance`.
4. Both sides index the accepted subscription locally.

### Recurring execution

On each due interval:

1. Payer daemon derives the billing period from the accepted schedule.
2. Payer daemon selects an allowed payment method.
3. Payer daemon fetches current private payment details for the payee.
4. Payer daemon generates a new `PaymentReference` for this attempt.
5. Payer daemon optionally sends `paykit.payment_attempt`.
6. Payer daemon executes the payment with method-specific code.
7. Payer daemon sends `paykit.payment_receipt` with method-specific proof.
8. Both sides index the receipt locally.

### Important property

Push subscriptions are payer-controlled.

The payee cannot directly pull funds. The payee can only provide receiving details, receive lifecycle messages, and verify receipts.

## Pull subscription direction

Pull is not implemented in v0.2.

The model should be compatible with future pull subscriptions, but pull-specific messages are deferred.

Future pull design must use `pubky-noise` private messages, not URL-secret bearer capability flows.

Future pull will need at least:

- `paykit.payment_request`
- request authentication
- replay protection
- billing-period uniqueness
- authorization limits
- cancellation semantics
- offline-payer behavior

Pull is not push in reverse. It adds a request/authorization protocol.

## Library vs daemon responsibilities

### Paykit library

The library should expose primitives for apps and daemons:

- typed subscription payload structs
- typed receipt payload structs
- JSON serialization/deserialization
- validation for IDs, references, method IDs, schedule, amount shape
- private message send/receive helpers for subscription and receipt message kinds
- ordered/event-like retrieval semantics for receipts and lifecycle events

The library should not:

- schedule recurring jobs
- execute payments
- persist daemon state
- decide payment-method-specific policy

### Paykit daemon

The daemon provides automation:

- persistent subscription index
- schedule evaluation
- retry policy
- payment-method execution integration
- receipt generation
- receipt validation/indexing
- accounting and query API

Apps may use library primitives without running the full daemon, but automatic subscriptions require daemon-like behavior somewhere.

## State derivation

A local implementation derives subscription state from ordered events.

Minimal states:

- `proposed`
- `active`
- `paused`
- `cancelled`
- `completed`

State transitions:

```text
proposal -> proposed
proposal + acceptance -> active
active + pause -> paused
paused + resume -> active
active|paused + cancellation -> cancelled
active + schedule end reached -> completed
```

Open detail for later:

- Whether updates create a `proposed_update` state.
- Whether cancellation requires acknowledgment.
- How to handle conflicting simultaneous events.

## Validation invariants

- `subscription_id` must be UUID-v4.
- `PaymentReference` must be UUID-v4 and is per attempt.
- Every lifecycle event must include `subscription_id`.
- Every receipt must include `subscription_id`, `payment_reference`, and `method_id`.
- Private subscription messages must be versioned JSON envelopes.
- Private subscription messages must preserve event order.
- Private subscription messages must fit within `pubky-noise` message size unless a future indirection mechanism is specified.
- Subscription implementation is private-only for now.
- Public subscriptions and public receipts are future work.
- URL-secret pull subscriptions are not part of v0.2.

## Open questions for v0.3

1. Should empty `accepted_methods` mean “any mutually supported method” or be rejected?
2. What canonical JSON and hash algorithm should be used for `accepted_proposal_hash`?
3. What exact timestamp format should be required? RFC3339 UTC only?
4. How should month-end schedules behave?
5. Are grace periods part of the subscription object or daemon policy?
6. Are retry windows part of the subscription object or daemon policy?
7. Should failed payment attempts be sent as protocol events or kept local?
8. Should `paykit.payment_attempt` be required, optional, or removed?
9. What is the proof envelope shape for `paykit.payment_receipt`?
10. Should receipt proof validation be part of Paykit method specs?
11. Can either party unilaterally pause, or only the payer?
12. Can either party unilaterally update terms, or must all updates be proposal + acceptance?
13. How should implementations handle conflicting simultaneous events?
14. What local daemon DB indexes are minimally required?
15. How should session recovery interact with long-lived subscriptions?
