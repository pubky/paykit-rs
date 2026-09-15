# Paykit Allowances Protocol V1

Status: normative V1 specification
Date: 2026-08-26

## Purpose and scope

An Allowance is private, scoped authority from an Allower to an Allowee. It
allows the Allower's wallet to handle qualifying Payment Requests from the
Allowee automatically, without fresh user approval for each payment. The
Allower remains the Payer, retains custody, and controls whether automatic
handling is enabled.

Allowances do not replace the Payment Request protocol or its lifecycle. A
Payment Request remains valid and usable without an Allowance, and a wallet may
always require its ordinary manual flow. A Subscription remains an accepted
Recurring Payment Request; an Allowance neither schedules payments nor creates
a separate subscription object.

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
sender or with different payload bytes is a conflict and MUST fail closed, as
defined in [Payment Requests](payment-requests.md#event-id).

A message whose Event ID conflicts is invalid evidence. A conflicted
acceptance, rejection, or End never controls the Allowance, and any Allowance
whose history contains conflicted evidence has invalid history and MUST NOT
authorize automatic handling until explicit review. Conflicted Event IDs do not
by themselves change the derived lifecycle state; `conflicted` remains reserved
for multiple distinct proposals sharing one Allowance ID.

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
entries apply independently and MUST be unique. Two period entries are
duplicates when every field is equal as spelled on the wire: `amount_limit` and
`anchor` compare as strings (numerically equal amounts or instant-equal anchors
with different spellings are distinct entries), `payment_count_limit` and
`every` compare as integers, and `kind` and `unit` compare exactly. Period and
allowlist array order has no eligibility meaning. V1 sets no separate array
cardinality limit beyond the complete-message byte limit.

Every rule is conjunctive: a payment qualifies only if all configured rules
pass. V1 has no OR groups, deny rules, precedence, conversion, or implied
defaults. Terms MUST configure at least one monetary ceiling or expiry: a
non-null `per_payment_amount`, a `period_limits` entry with non-null
`amount_limit`, a non-null `lifetime_amount_limit`, or non-null `expires_at`.
An endpoint allowlist, `active_from`, or payment-count limits alone do not
satisfy this requirement, including a zero payment-count limit. Zero amount
ceilings are valid; zero is not absence.

This minimum rule does not guarantee a lifetime spending budget. A per-payment
maximum limits each payment, period amount limits renew, and expiry alone
limits time without limiting the amount spent before expiry. Wallet policy MAY
require stronger bounds. Structural validation MUST accept past expiry times
for historical replay; current eligibility is evaluated separately.

Allowance Terms are immutable. Changed accepted terms require a proposal with
a new Allowance ID and a separate End for the old Allowance. V1 defines no
update, counteroffer, wire replacement-link, or cross-message atomicity. Until
the old End is observed, the old and new Allowances remain independent. This
does not prohibit explicitly authorized local reassociation of future Billing
Periods as defined below.

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

Wallets MUST durably retain a nondecreasing evaluation-time watermark for each
Allowance alongside its usage and reservations. Before evaluating automatic
handling at trusted time `t`, the wallet MUST reject that evaluation if `t` is
earlier than the watermark. Advancing the watermark MUST be atomic with any
automatic admission or removal of usage that has left a period. A failed or
restarted operation MUST NOT leave admitted usage or discarded history with an
older watermark. This rule applies to both rolling and anchored periods and
MUST survive restart and backup recovery. Missing or uncertain watermark state
requires the same fail-closed recovery as missing usage history.

Clock rollback MUST NOT restore capacity, move evaluation into an earlier
anchored period, or discard future-dated durable usage. Automatic handling may
resume when trusted time reaches the retained watermark and the ordinary
eligibility checks pass. For example, usage admitted at 10:00 remains protected
when the clock moves to 09:00, including after restart; crossing midnight
backwards MUST NOT reopen the previous day's anchored capacity. Wallet tests
SHOULD cover those cases and normal forward-time period expiry.

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

Every lifecycle message has its own Event ID. An Acceptance or Rejection Event
ID MUST differ from its Proposal Event ID. An End Event ID, Proposal Event ID,
and non-null Acceptance Event ID MUST be pairwise distinct. Reusing a causal
Event ID for the current message is invalid rather than an exact replay.

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
`acceptance_event_id: null` (a proposal withdrawal). A withdrawal is valid only
from the proposal sender and is valid whether or not that sender has already
loaded an acceptance or rejection; it always derives `ended`. A proposer that
has already loaded a valid Acceptance SHOULD end accepted authority by naming
that Acceptance Event ID instead; a recipient still derives `ended` from any
valid null-acceptance End sent by the proposal sender, whether or not it has
loaded the Acceptance. A proposal recipient MUST NOT send a null-acceptance
End; such an End is invalid at any point. Either party MAY end accepted
authority by sending End naming the exact bound Acceptance Event ID. An End is
unilateral and terminal. An End with a wrong causal reference is invalid.

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
| Proposal + proposal withdrawal | `ended` | No |
| Proposal + Rejection + proposal withdrawal | `ended` | No |
| Proposal + Acceptance + End | `ended` | No |
| Proposal + crossing Acceptance + proposal withdrawal | `ended` | No |
| Multiple distinct Proposals for one Allowance ID | `conflicted` | No |

The table describes valid events only; invalid history, including Event ID
conflicts, blocks automatic handling under the eligibility rules below.
Rejection and End are terminal: no later event restores authority, and a valid
End after Rejection only moves the derived state to `ended`. Expiry is a
trusted-time eligibility result, not an Event Message or another lifecycle
state.

## Payment Request integration

An ordinary `paykit.payment_request` is the only V1 request that may exercise
an Allowance. V1 defines no Allowance-specific payment message. Payment Request
proposals do not carry an Allowance ID or use an alternate lifecycle. They follow
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

Because the request carries no Allowance ID, the wallet selects the authority.
It MAY choose one of multiple candidates using local priority rules or explicit
user choice. Every candidate considered MUST satisfy the shared matching and
lifecycle rules. Automatic handling requires exactly one selected Allowance;
the wallet MUST NOT combine authority or capacity from multiple Allowances for
one payment. If local policy cannot choose, the ordinary manual flow remains
available.

The selected Allowance and decision MUST be durably persisted before queuing
automatic Acceptance or any other automatic side effect. Selection MUST be
serialized with payment admission and manual handling for the same semantic
payment key. A retry uses the persisted
selection rather than choosing again because capacity, priority, or candidate
availability changed. A manual-only decision MUST NOT be reversed by automatic
matching. Explicit recurring reassociation follows the rules below.

The wallet resolves current Payment Endpoint details using the normal Payment
Request rules. Private/current details SHOULD be preferred, and a wallet MAY
permit reusable public details by local policy. A stale, expired, consumed,
missing, invalid, or uncertain endpoint MUST NOT be used for automatic payment.
Allowances add no destination digest, public-endpoint requirement, or fallback
selection rule.

Before an automatic payment, the wallet MUST establish that the amount and
asset the selected payment method will actually transfer equal the requested
Payment Amount; amount-less endpoint details (for example an amount-less
invoice or offer) MUST be paid at exactly the requested Payment Amount.
Endpoint details that fix a different amount or asset, or whose transferred
amount the wallet cannot determine before execution, MUST be treated as
unusable for automatic payment, with the same manual-flow outcome as a stale or
invalid endpoint.

Before automatic Acceptance, no selected candidate, disabled local
automatic handling, failed endpoint resolution, unsupported payment methods,
or any stricter wallet check preserve the proposed request's ordinary manual
response flow; they do not cause an automatic rejection or cancellation. A
wallet SHOULD complete every current eligibility, capacity, endpoint, and
private-safeguard preflight check that does not require prior Acceptance before
queuing Acceptance. Durable local processing order, not a claimed sender
timestamp, determines this boundary.

The wallet MUST distinguish a deferred automatic decision from an explicit
manual-only disposition. A temporary endpoint-resolution or availability
failure MAY defer automatic handling while preserving the ordinary manual
flow. Deferral MUST retain its reason, any selected Allowance, and the relevant
request or Billing Period identity durably. It grants no authority to pay and
does not queue Acceptance by itself. An explicit manual-only decision remains
sticky and MUST NOT be cleared merely because an endpoint or other condition
later changes.

Before Acceptance, a deferred request remains proposed. On reconsideration the
wallet MUST recheck proposal expiry, request and Allowance lifecycle, shared
terms, current endpoint details, capacity where applicable, local enablement,
and payment/reservation history. It MUST use any persisted selection; deferral
does not authorize silent reselection. A concurrent manual response or payment
MUST exclude conflicting automatic work through the same durable concurrency
control. Proposal expiry or a terminal lifecycle event takes precedence and
MUST NOT be presented as payable.

If automatic handling stops after Acceptance is recorded and the request is not
cancelled, the Payment Request remains accepted. The wallet MUST first establish
that no automatic attempt or Allowance reservation is unresolved and that no
successful or unresolved payment, including an in-flight, pending, unknown, or
recovery-incomplete payment through any automatic or manual path, exists for the
semantic payment key. It may then durably defer that occurrence after a
temporary failure, or mark it manual-only, and make explicit payment available
without sending another Acceptance. Reconsidering a deferred occurrence MUST
repeat the full current eligibility and exclusion checks. Once marked
manual-only, that occurrence MUST NOT be retried automatically. Cancellation or
a successful or unresolved payment makes the occurrence unavailable for manual
execution. A pending, unknown, or recovery-incomplete attempt is unresolved
execution, not a deferred occurrence eligible for a new attempt.

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
automatically rebinds the request to another Allowance, rejects or cancels the
Payment Request, or ends an accepted Subscription.

### Authorized recurring reassociation

The Allower MAY explicitly authorize another accepted Allowance for future,
unpaid Billing Periods of an accepted, non-cancelled Recurring Payment Request.
This is a local selection change, not a mutation of either Allowance, a new
Payment Request Acceptance, or an automatic consequence of accepting or ending
an Allowance. The replacement MUST have the exact same authenticated party
scope and statically cover the request. Current eligibility and capacity MUST
still be checked separately when each payment becomes due.

The wallet MUST durably record the authorization, old and replacement Allowance
IDs, an effective Billing Period boundary, and an association revision before
the change can affect execution. Only periods starting at or after that
explicit boundary are covered; overdue or already-started periods MUST NOT be
silently included. The change MUST compare the expected previous revision and
be serialized with automatic admission and manual handling. A worker using an
obsolete revision MUST NOT start an irreversible payment after the change has
taken effect.

Reassociation MUST preserve existing occurrence identities and selection
history. A successful or unresolved payment through any path blocks another
payment for the same semantic key, independent of the selected Allowance ID.
Existing reservations MUST remain attached to their original Allowance until
reconciled; committed usage MUST remain charged to that Allowance. Neither
Allowance's counters or evaluation-time watermark may be reset. Future
admissions use the replacement's own existing usage and limits. An earlier
period's unresolved attempt does not by itself reassign its reservation or
prevent a different future period from being evaluated.

Explicit manual-only dispositions MUST survive reassociation. A blanket change
of future association does not restore automatic permission for such an
occurrence. Missing authorization, revision, payment, or reservation history
MUST block automatic handling after restart or restore; lifecycle messages alone
cannot reconstruct a local reassociation decision.

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

Usage is durable SDK/runtime state driven by wallet payment outcomes. Payment
Request Acceptance consumes no capacity. A manual payment consumes no Allowance
capacity, but its successful or unresolved occurrence blocks automatic
execution for the same semantic
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
allows optional historical attribution in the existing Payment Proof as defined
below; it does not communicate an authoritative usage ledger.

### Payment Proof attribution

A Payment Proof MAY carry the optional top-level `allowance_id` defined in
[Payment Requests](payment-requests.md#paykitpayment_proof). It identifies the
Allowance used for that execution, not the request's current association. It
MUST be omitted when attribution is absent; explicit `null` is invalid. When
present it MUST be a canonical lowercase, hyphenated UUID-v4. Manual payments
MUST omit it. Absence is not evidence that a payment was manual: attribution
is optional even for automatic payments.

The payer MUST derive attribution from retained execution/reservation history.
An Allowance that has since ended or expired may still be named for a valid
earlier execution. Reassociation MUST NOT rewrite attribution to the new
Allowance. The field does not grant authority, establish settlement, create a
reservation, or commit, release, or restore usage. The Allower's durable ledger
remains authoritative; the Allowee may use attribution only as informational
evidence with method-specific payment verification.

Receiving, replaying, or correcting a proof MUST NOT change Allowance capacity.
Different proof Event IDs for the same semantic payment key MUST NOT be counted
as different executions. A later proof that omits, adds, or changes attribution
MUST NOT silently replace a prior attribution; retain the evidence and flag
disagreement for reconciliation without moving usage. A well-formed proof does
not require the receiver to have the named Allowance's complete history to be
structurally valid. No proof field substitutes for missing local execution
history or enables automatic handling.

## Durability and recovery

Receivers SHOULD durably persist received stream items, authenticated link
scope, raw bytes, and validation results before wallet side effects. Replayed
events after a checkpoint loss are expected and use Event ID and Payment
Request dedupe rules.

The wallet/runtime MUST durably retain Request-to-Allowance associations,
their revisions and reassociation authorizations, deferred and manual-only
decisions, semantic payment keys, and usage reservations before automatic
execution.
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
implementations MUST retain their raw bytes for audit and future upgrade.

The optional Payment Proof `allowance_id` is a coordinated pre-release extension
of Payment Request wire version 1. Implementations predating this extension
reject it as an unknown field. Absence preserves the prior proof shape; this
change is not transparent compatibility with those older readers. Both peers
must support the extension before a payer sends attributed proofs.

A message is Allowance-correlated when it is a JSON object whose top-level
`allowance_id` member is a canonical UUID string as defined above; this probe
applies to every unknown kind and to every recognized kind that fails V1
validation, including an unsupported `version`. Implementations MUST NOT
search nested members. Any Allowance-correlated such message MUST block
automatic handling for that Allowance until a compatible implementation or
explicit review resolves it. A message that cannot be probed (not a JSON
object, or no canonical top-level `allowance_id`) is unrelated: it does not
change V1 Allowance state and probing failure is not an error. Extensions
require a new version or message kind.

Every complete message must fit the 1000-byte limit. Large arrays or long
decimal spellings may exceed it and MUST be rejected as a whole; V1 has no
fragmentation or indirection.

## Component responsibilities

| Component | V1 responsibility |
| --- | --- |
| Paykit Protocol / Paykit Library | Closed lifecycle wire types, parsing, serialization, structural validation, stateless lifecycle correlation, exact decimal and period math, and shared eligibility/limit evaluation. |
| Paykit SDK/runtime | Durable ordered events, Event ID dedupe, lifecycle derivation, candidate evaluation, persisted selections and dispositions, association revisions, semantic payment exclusion, atomic reservations and outcome accounting, watermarks, backup/recovery, and wallet-facing views. |
| Wallet | Local enablement and candidate priority, explicit consent, trusted time, private safeguards, endpoint and payment-method validation, scheduling, signing, irreversible execution, and settlement reconciliation. |

The Library's evaluation helpers MUST take explicit terms, time, scope, and
usage inputs and return an eligibility result; they MUST NOT store usage or
perform payment side effects. They MUST use the exact arithmetic and boundary
rules above. A match result is evidence for a wallet decision, not authority
to execute without the remaining current checks.

The SDK/runtime MUST provide one durable admission path that validates the
selected association and its revision, checks lifecycle/history and the
semantic payment key, evaluates current capacity, and atomically records the
reservation and evaluation-time watermark before execution. All automatic and
manual paths MUST participate in the same occurrence exclusion. Candidate
lists are advisory snapshots; admission MUST recheck rather than trust a prior
query. Outcome updates MUST be idempotent: confirmed success commits once,
confirmed terminal failure releases once, and uncertain outcomes remain
reserved. A callback or replay MUST NOT turn committed usage back into free
capacity. Payment Proof receipt is not a settlement callback.

The wallet MUST report manual in-flight, successful, and unresolved payments
to the shared occurrence ledger before allowing competing automatic work. The
SDK/runtime cannot prevent a duplicate payment made outside this coordination.
Independent devices MUST NOT each assume exclusive admission authority merely
because they restored the same backup. One coordinated durable writer or an
equivalent exclusion mechanism is required for the affected payment scope.

Backups MUST retain association/decision history, semantic payment keys,
reservations, outcome history, and evaluation-time watermarks together with
lifecycle evidence. Restored or recovery-incomplete execution state MUST remain
ineligible until wallet reconciliation establishes that no later successful or
unresolved payment is missing. Missing accounting MUST NOT be reconstructed as
zero usage from lifecycle messages or Payment Proofs.

The Library MUST remain stateless and does not authorize payment. The
SDK/runtime coordinates evidence and MUST NOT turn eligibility into a payment
decision. Session creation, Pubky capabilities, key rotation, request timeout,
and payment execution remain caller or wallet responsibilities.
