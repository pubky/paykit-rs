# Paykit Allowances Protocol V1

Status: normative V1 specification
Date: 2026-08-26

## Purpose and scope

An Allowance is private, scoped authority from an Allower to an Allowee. It
allows the Allower's wallet to handle qualifying Payment Requests from the
Allowee automatically, without fresh user approval for each payment. The
Allower remains the Payer, retains custody, and controls whether automatic
handling is enabled.

Allowances do not replace or modify the Payment Request protocol. A Payment
Request remains valid and usable without an Allowance, and a wallet may always
require its ordinary manual flow. A Subscription remains an accepted Recurring
Payment Request; an Allowance neither schedules payments nor creates a separate
subscription object.

This specification defines the V1 Allowance lifecycle, immutable Allowance
Terms, compatibility with Payment Requests, usage boundaries, wire
compatibility, and component responsibilities. It does not define wallet-local
enablement, storage or concurrency mechanisms, recurring scheduling, payment
execution, settlement, payment-method-specific validation, or user interfaces.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are normative.

## Roles and consent

- The **Allower** controls the funds and is the Payer for a payment authorized
  through an Allowance.
- The **Allowee** is the authenticated Payment Request sender whose requests the
  Allower's wallet may consider for automatic handling.
- For Allowance matching, the authenticated Allowee is also the Payment Request
  protocol's **Payee**.

The Payment Endpoint details selected for a request may contain an invoice,
address, or other destination that economically benefits another party. Paykit
authenticates who sent the request and shared the endpoint details; it does not
determine the ultimate economic beneficiary of those details.

An Allowance is bound to the exact two Paykit Receiver References that own its
Encrypted Link. One is the Allower and the other is the Allowee. Moving a
message to another link does not move its authority.

Either party MAY propose exact terms. The proposal is authenticated consent by
its sender. The recipient MAY accept or reject it, but authority exists only
after explicit acceptance. Consequently, the Allower consents either by
proposing as Allower or by accepting an Allowee-authored proposal. An Allowance
ID is a correlation identifier, not a bearer credential, and is not added to a
Payment Request.

## Transport and common rules

All four V1 message kinds are Private Application Messages sent over the
Allower-Allowee Encrypted Link:

- `paykit.allowance_proposal`
- `paykit.allowance_acceptance`
- `paykit.allowance_rejection`
- `paykit.allowance_end`

Every kind is a FIFO Event Message. Receivers MUST preserve every valid event
in send order. Event order is FIFO within each sending direction; V1 defines no
total order across the two directions. Implementations MUST use causal event
references and MUST NOT infer cross-direction order from receipt time, local
timestamps, or clock comparison.

Every message is one UTF-8 JSON object and has these rules:

- `version` MUST be the JSON integer `1`.
- `kind` MUST be exactly one kind above.
- `event_id`, `allowance_id`, and all causal event references MUST be canonical
  lowercase, hyphenated UUID-v4 strings where present.
- Every JSON object is closed: unknown fields are invalid.
- Duplicate object member names are invalid.
- Missing required fields and explicit `null` for non-nullable fields are
  invalid.
- The complete compact or non-compact UTF-8 JSON encoding MUST be at most 1000
  bytes, the V1 `pubky-noise` whole-message limit.

A transport retry from the same authenticated sender MUST reuse the same Event
ID and exact payload bytes. Within one authenticated Encrypted Link scope,
Event ID dedupe applies across all Event Message kinds: reuse by the other
sender or with different payload bytes is a conflict and MUST fail closed.

## Allowance Terms

`paykit.allowance_proposal` carries this exact closed `terms` object:

```json
{
  "asset": "btc",
  "per_payment_amount": {
    "minimum": "0.0001",
    "maximum": "0.01"
  },
  "period_limits": [
    {
      "amount_limit": "0.03",
      "payment_count_limit": 5,
      "period": {
        "kind": "anchored",
        "every": 1,
        "unit": "month",
        "anchor": "2026-01-31T00:00:00Z"
      }
    }
  ],
  "lifetime_amount_limit": "0.10",
  "active_from": "2026-06-01T00:00:00Z",
  "expires_at": null,
  "allowed_payment_endpoint_identifiers": ["btc-lightning-bolt12"]
}
```

Every displayed field is required. Nullable and collection fields behave as
follows:

| Field | V1 rule |
| --- | --- |
| `asset` | Non-empty and contains no control characters. Matching is exact and case-sensitive. |
| `per_payment_amount` | `null`, or an inclusive range whose `minimum` is numerically no greater than `maximum`. |
| `period_limits` | Array of zero or more limits. Each limit has at least one non-null limit. |
| `lifetime_amount_limit` | `null`, or an amount ceiling across committed automatic payments and unresolved automatic reservations. |
| `active_from` | `null`, or the first eligible instant, inclusive. |
| `expires_at` | `null`, or the first ineligible instant, exclusive. It MUST be later than `active_from` when both exist. |
| `allowed_payment_endpoint_identifiers` | `null`, or a non-empty array of unique, valid Payment Endpoint Identifiers. Matching is exact. |

`minimum`, `maximum`, `amount_limit`, and `lifetime_amount_limit` use the exact
`PaymentAmount.value` syntax: ASCII digits, at most one `.`, and at least one
digit. Signs, exponent notation, and grouping separators are invalid. `.5`,
`10.`, and leading or trailing zeros are valid. Implementations MUST compare
values with exact decimal arithmetic, not floating point. Original spelling is
preserved and remains significant when comparing retried proposal bytes, even
when two spellings are numerically equal. Paykit defines no asset precision,
normalization, registry, FX, or cross-asset comparison.

A `period_limits` entry has required `amount_limit` and
`payment_count_limit` fields; either MAY be `null`, but not both.
`payment_count_limit` is an unsigned 64-bit JSON integer. All configured period
entries apply independently and MUST be unique. Period and allowlist array
order has no eligibility meaning. V1 sets no separate array cardinality limit
beyond the complete-message byte limit.

Every rule is conjunctive: a payment qualifies only if all configured rules
pass. V1 has no OR groups, deny rules, precedence, conversion, or implied
defaults. At least one field other than `asset` MUST constrain the authority;
terms with null amount, lifetime, time, and endpoint-identifier fields and no
period limits are invalid.

Allowance Terms are immutable. Changed accepted terms require a proposal with
a new Allowance ID and a separate End for the old Allowance. V1 defines no
update, counteroffer, replacement-link, or cross-message atomicity. Until the
old End is observed, the old and new Allowances remain independent.

## Time and period math

All wire times MUST be RFC3339 UTC timestamps with the `Z` suffix. Eligibility
uses the Allower wallet's trusted time `t`; message receipt time is not a
protocol timestamp.

A period is one of these closed objects:

```json
{"kind":"anchored","every":1,"unit":"month","anchor":"2026-01-31T00:00:00Z"}
```

```json
{"kind":"rolling","every":7,"unit":"day"}
```

`every` MUST be a positive unsigned 64-bit JSON integer. Anchored units are
`minute`, `hour`, `day`, `week`, `month`, or `year`. Rolling units are `minute`,
`hour`, `day`, or `week`; rolling months and years are invalid.

For anchored minutes, hours, days, and weeks, let `L` be `every` multiplied by
60, 3600, 86400, or 604800 seconds. The period containing `t` is the unique
half-open interval `[anchor + kL, anchor + (k+1)L)` for integer `k`.

For anchored months or years, boundary `k` is the UTC calendar result of adding
`k * every` units to the original anchor. Preserve the anchor time and original
day when it exists; otherwise clamp to the target month's final day. Calculate
each boundary from the original anchor, not the preceding boundary. The period
is `[boundary(k), boundary(k+1))`. Thus a January 31 monthly anchor clamps in
February and returns to day 31 when possible; a February 29 yearly anchor does
the same across non-leap years.

For either anchored style, include the candidate, committed automatic
payments, and unresolved automatic reservations whose original wallet
admission time `s` satisfies
`boundary(k) <= s < boundary(k+1)` for the interval containing evaluation time
`t`.

For a rolling period, `L` is the same fixed-second conversion. When evaluating
a candidate at `t`, include that candidate, committed automatic payments, and
unresolved automatic reservations with original wallet admission time `s`
satisfying `t - L < s <= t`. A payment exactly on the lower boundary has left
the window.

For every applicable period, adding the candidate amount and count MUST leave
the amount total numerically at or below `amount_limit` and the count at or
below `payment_count_limit` where those limits are non-null. Implementations
MUST use checked duration, calendar, count, and boundary arithmetic. An overflow
or unrepresentable boundary makes the evaluation ineligible; it MUST NOT wrap,
saturate, or omit the affected rule.

When `lifetime_amount_limit` is non-null, the candidate amount plus all
committed automatic payments and unresolved automatic reservations MUST be
numerically at or below that limit.

## Lifecycle messages

### Proposal

```json
{
  "version": 1,
  "kind": "paykit.allowance_proposal",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
  "allowance_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44",
  "proposer_role": "allower",
  "terms": {
    "asset": "btc",
    "per_payment_amount": {
      "minimum": "0.0001",
      "maximum": "0.01"
    },
    "period_limits": [
      {
        "amount_limit": "0.03",
        "payment_count_limit": 5,
        "period": {
          "kind": "anchored",
          "every": 1,
          "unit": "month",
          "anchor": "2026-01-31T00:00:00Z"
        }
      }
    ],
    "lifetime_amount_limit": "0.10",
    "active_from": "2026-06-01T00:00:00Z",
    "expires_at": null,
    "allowed_payment_endpoint_identifiers": ["btc-lightning-bolt12"]
  }
}
```

`proposer_role` MUST be `allower` or `allowee` and assigns the sender that role
and the recipient the other role. One Allowance ID may have exactly one
proposal source and Event ID across combined authenticated inbound and outbound
history. Multiple distinct proposals with the same Allowance ID are an
order-independent collision: the Allowance is `conflicted`, neither proposal
binds authority, and no later event may make it usable. An exact retry from the
same sender is the original proposal, not another proposal.

### Acceptance and rejection

```json
{
  "version": 1,
  "kind": "paykit.allowance_acceptance",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202",
  "allowance_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44",
  "proposal_event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201"
}
```

Rejection has the same fields and uses kind `paykit.allowance_rejection` with a
distinct Event ID. Only the proposal recipient may send either response. It
MUST reference the bound proposal Event ID. The first valid acceptance or
rejection in that sender's FIFO direction controls; another response is
invalid. A local view containing the proposal and valid acceptance has accepted
authority, but no global acceptance instant exists. Acceptance does not create
or execute a payment.

### End

```json
{
  "version": 1,
  "kind": "paykit.allowance_end",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d203",
  "allowance_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44",
  "proposal_event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
  "acceptance_event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202"
}
```

The proposal sender MAY withdraw a proposal by sending End with
`acceptance_event_id: null`. Either party MAY end accepted authority by naming
its exact Acceptance Event ID. An end is unilateral and terminal. An End with
a wrong causal reference or sent by the proposal recipient before acceptance
is invalid.

Because the directions have no total order, a valid proposal withdrawal and a
crossing acceptance or rejection may both exist. End wins safely: no authority
remains. Events whose causal references have not yet been loaded MUST NOT
affect state; an SDK may retain them pending resolution against durable inbound
and outbound history.

Lifecycle derivation is closed by this table:

| Valid causally linked events | Derived state | May authorize automatic handling? |
| --- | --- | --- |
| Proposal only | `proposed` | No |
| Proposal + Acceptance | `accepted` | Yes, while terms and wallet checks pass |
| Proposal + Rejection | `rejected` | No |
| Proposal + pending End | `ended` | No |
| Proposal + Rejection + pending End | `ended` | No |
| Proposal + Acceptance + End | `ended` | No |
| Proposal + crossing Acceptance + pending End | `ended` | No |
| Multiple distinct Proposals for one Allowance ID | `conflicted` | No |

Rejection and End are terminal. Expiry is a trusted-time eligibility result,
not an Event Message or another lifecycle state.

## Payment Request integration

An ordinary `paykit.payment_request` is the only V1 request that may exercise
an Allowance. V1 defines no Allowance-specific payment message. Payment Request
messages do not carry an Allowance ID or use an alternate lifecycle. They follow
the Payment Request validation, endpoint lookup, cancellation, and proof rules.

The request sender MUST be the Allowee on the Allowance's exact Encrypted Link.
The immutable request terms statically match an Allowance only when the request
asset exactly equals the Allowance asset, the request amount is within
`per_payment_amount` when configured, and the eligible endpoint-identifier set
is non-empty.

The eligible endpoint-identifier set is the request's complete
`accepted_payment_endpoint_identifiers` set when the Allowance allowlist is
null. Otherwise it is the exact intersection of the request set and the
Allowance allowlist. An automatic payment authorized by that Allowance MUST use
only an identifier in that eligible set.

At the request's first automatic-handling decision, an Allowance is a candidate
only when its immutable terms statically match, its lifecycle is accepted and
not ended, and its active time window includes the wallet's trusted time. The
request MUST be a known, valid proposal that remains proposed, unexpired, and
neither rejected nor cancelled.

Because the request carries no Allowance ID, initial automatic handling
requires exactly one candidate. On this first decision, the wallet MUST durably
persist either the selected Allowance or a manual-only disposition; the
disposition MUST be stored before any automatic side effect. The wallet MUST
NOT automatically select among multiple candidates or later rematch a
manual-only request because Allowances, local enablement, capacity, or endpoint
availability changed.

The wallet resolves current Payment Endpoint details using the normal Payment
Request rules. Private/current details SHOULD be preferred, and a wallet MAY
permit reusable public details by local policy. A stale, expired, consumed,
missing, invalid, or uncertain endpoint MUST NOT be used for automatic payment.
Allowances add no destination digest, public-endpoint requirement, or fallback
selection rule.

Before automatic Acceptance, zero or multiple candidates, disabled local
automatic handling, failed endpoint resolution, unsupported payment methods,
or any stricter wallet check preserve the proposed request's ordinary manual
response flow; they do not cause an automatic rejection or cancellation. A
wallet SHOULD complete every current eligibility, capacity, endpoint, and
private-safeguard preflight check that does not require prior Acceptance before
queuing Acceptance. Durable local processing order, not a claimed sender
timestamp, determines this boundary.

If automatic handling stops before Acceptance is recorded while the request
remains a valid, actionable proposal, the wallet records the whole request as
manual-only and leaves it proposed. Proposal expiry or a terminal lifecycle
event takes precedence and MUST NOT be presented as payable.

If automatic handling stops after Acceptance is recorded and the request is not
cancelled, the Payment Request remains accepted. The wallet MUST first establish
that no automatic attempt or Allowance reservation is unresolved and that no
successful or unresolved payment, including an in-flight, pending, unknown, or
recovery-incomplete payment through any automatic or manual path, exists for the
semantic payment key. It then durably marks that one-time request or affected
Billing Period manual-only and makes it available for explicit payment without
sending another Acceptance. Once marked manual-only, that occurrence MUST NOT be
retried automatically. Cancellation or a successful or unresolved payment makes
the occurrence unavailable for manual execution.

This accepted-but-unpaid action state is wallet-local and cannot be inferred
from the Payment Request's `accepted` state alone. It is separate from an
existing query or queue whose purpose is to find proposals needing a payer
response; implementations MUST NOT make every accepted request payable without
also consulting durable execution and reservation state. For a Recurring
Payment Request, this rule applies to the affected Billing Period and does not
remove the pinned Allowance from later Billing Periods. Once an attempt crosses
the irreversible boundary, its outcome follows the reservation and wallet-owned
retry rules below.

An accepted Allowance permits automatic handling but never requires it. A
wallet-local enablement setting, consent presentation, risk controls, and the
final decision to pay are outside Paykit. A wallet MAY decline or require fresh
approval even when all shared terms pass, but it MUST NOT expand the shared
authority.

### One-time and recurring requests

The same Allowance Terms apply to one-time and Recurring Payment Requests. V1
has no separate recurrence permission.

For a one-time request with a selected Allowance, a wallet MAY send the ordinary
Payment Request Acceptance automatically after the applicable preflight checks.
Any Payment Proof remains optional and follows the Payment Request rules. The
semantic payment key is the exact Allower and Allowee Receiver References plus
the Payment Request ID. That key may consume Allowance usage at most once for a
successful or unresolved payment.

A wallet MAY automatically accept a Recurring Payment Request with a selected
Allowance. At acceptance it pins that Allowance but does not reserve or consume
usage. Static terms and current lifecycle eligibility are checked at
acceptance; current period and lifetime capacity are checked when each payment
becomes due.

For every Billing Period, the wallet scheduler supplies the eligible period and
the wallet rechecks the pinned Allowance before automatic payment. The semantic
payment key is the exact Allower and Allowee Receiver References, Payment
Request ID, and the validated `starts_at` and `ends_at` Billing Period instants.
Equivalent timestamp spellings for the same instants MUST NOT create distinct
keys. The Allowance does not calculate the schedule or prove that a Billing
Period belongs to the request.

Ending or expiring the pinned Allowance prevents new automatic payments.
Unavailable period or lifetime capacity blocks the affected Billing Period;
the wallet makes that period available for explicit payment when the
accepted-but-unpaid safety conditions above pass. Later Billing Periods are
evaluated independently against then-current capacity. None of these conditions
rebinds the request to another Allowance, rejects or cancels the Payment Request,
or ends an accepted Subscription.

## Eligibility, usage, and execution boundary

Immediately before each automatic payment, the wallet evaluates at trusted
time `t`. Shared eligibility requires all of:

- the persisted Request-to-Allowance association and exact authenticated party
  scope;
- accepted, active, unexpired, and not-ended Allowance authority;
- an accepted and not-cancelled Payment Request;
- exact asset, inclusive amount range, all period limits, lifetime limit, and
  any configured endpoint-identifier allowlist;
- no conflicting or unresolved lifecycle history;
- no successful or unresolved payment, including an in-flight, pending, or
  unknown payment, for the semantic payment key through any automatic or manual
  path, and no unresolved Allowance reservation for it; and
- a current, usable Payment Endpoint allowed by the request and Allowance.

Usage is wallet-owned durable state. Payment Request Acceptance consumes no
capacity. A manual payment consumes no Allowance capacity, but its successful
or unresolved occurrence blocks automatic execution for the same semantic
payment key. Counted usage consists only of committed automatic payments and
unresolved automatic reservations; released reservations are excluded. An
automatic payment reserves one count and the exact requested Payment Amount
before an irreversible payment side effect, using concurrency control. Fees do
not add usage and refunds do not restore committed usage.

A verified successful payment commits the reservation. A confirmed terminal
failure before settlement releases it. A pending, unknown, or recovery-incomplete
outcome remains reserved until reconciled. Retry policy is wallet-owned, but a
retry MUST NOT create two successful payments or two committed usage entries
for one semantic payment key. When the wallet stops automatic retries after a
confirmed terminal failure, an accepted occurrence follows the manual-only
action rules above.

The wallet MUST recheck both the Allowance and Payment Request lifecycle near
the irreversible execution step. If an End or cancellation is observed before
any irreversible payment side effect, the wallet MUST abort and release the
reservation. Once the payment is irreversible, later lifecycle events affect
only future payments and the in-flight outcome remains reserved until
reconciled. Proof for an execution that was already past its irreversible
boundary when cancellation was observed, and Acceptances that cross a payee
Cancellation, follow the Payment Request rules in
[payment-requests.md](payment-requests.md); the request remains cancelled. V1
does not communicate Allowance usage or selection to the Allowee; the existing
Payment Request messages communicate acceptance and proof.

## Durability and recovery

Receivers SHOULD durably persist received stream items, authenticated link
scope, raw bytes, and validation results before wallet side effects. Replayed
events after a checkpoint loss are expected and use Event ID and Payment
Request dedupe rules.

The wallet/runtime MUST durably retain Request-to-Allowance associations,
semantic payment keys, and usage reservations before automatic execution.
Validated Encrypted Link recovery for the same Receiver References does not
require fresh Allowance consent when the complete durable event history and
wallet-owned state are retained.

While link recovery is required, or when lifecycle, request, association,
dedupe, or usage history is incomplete, the wallet MUST NOT perform automatic
payment under an Allowance. A proposed Payment Request remains available to its
ordinary manual response flow. An accepted request or Billing Period becomes
available for explicit payment only after the wallet can establish the
accepted-but-unpaid safety conditions above. A new link does not reconstruct
missing history.

## Compatibility and size

V1 is closed-world. Unknown fields, enum values, or period shapes inside a
recognized V1 kind are invalid. Unsupported versions and unknown kinds MUST
NOT be interpreted as V1 or cause side effects. Durable private-stream
implementations MUST retain their raw bytes for audit and future upgrade. Any
recoverably Allowance-correlated unknown kind or unsupported version MUST block
automatic handling for that Allowance until a compatible implementation or
explicit review resolves it. Unrelated unknown kinds do not change V1
Allowance state. Extensions require a new version or message kind.

Every complete message must fit the 1000-byte limit. Large arrays or long
decimal spellings may exceed it and MUST be rejected as a whole; V1 has no
fragmentation or indirection.

## Component responsibilities

| Component | V1 responsibility |
| --- | --- |
| Paykit Protocol / Paykit Library | Closed Allowance lifecycle wire types, parsing, serialization, structural validation, and stateless lifecycle correlation helpers. |
| Paykit SDK/runtime | Durable ordered events, Event ID dedupe, Allowance and Payment Request lifecycle derivation, recovery, and wallet-facing views. |
| Wallet | Local auto-payment enablement, Allowance matching and pinning, trusted time, private safeguards, usage and concurrency, endpoint and payment-method validation, scheduling, capacity, signing, execution, and settlement. |

The Library MUST remain stateless and does not authorize payment. The
SDK/runtime coordinates evidence and MUST NOT turn eligibility into a payment
decision. Session creation, Pubky capabilities, key rotation, request timeout,
and payment execution remain caller or wallet responsibilities.
