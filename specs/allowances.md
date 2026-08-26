# Paykit Allowances Protocol V1

Status: normative V1 specification
Date: 2026-08-26

## Purpose and scope

An Allowance is private, scoped authority from an Allower to an Allowee. It
allows the Allowee to submit qualifying Payment Instructions without a new
Allowance consent exchange for each instruction. The Allower remains the Payer
and its wallet retains custody and the final decision to pay. A Payment
Instruction may name the Allowee or a third party as Payee.

This specification defines the V1 Allowance lifecycle, immutable Allowance
Terms, Payment Instructions, public destination references and observations,
wire compatibility, and component boundaries. It does not define private wallet
safeguards, storage or concurrency mechanisms, signing, execution, settlement,
results, proofs, acknowledgements, or user interfaces.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are normative.

## Roles and consent

- The **Allower** controls the funds and is the Payer for an executed
  instruction.
- The **Allowee** may submit Payment Instructions under an accepted Allowance.
- The **Payee** receives one payment and need not be the Allowee.

An Allowance is bound to the exact two Paykit Receiver References that own its
Encrypted Link. One is the Allower and the other is the Allowee. Moving a
message to another link does not move its authority.

Either party MAY propose exact terms. The proposal is authenticated consent by
its sender. The recipient MAY accept or reject it, but authority exists only
after explicit acceptance. Consequently, the Allower consents either by
proposing as Allower or by accepting an Allowee-authored proposal. An Allowance
ID is a correlation identifier, not a bearer credential.

## Transport and common rules

All five V1 message kinds are Private Application Messages sent over the
Allower-Allowee Encrypted Link:

- `paykit.allowance_proposal`
- `paykit.allowance_acceptance`
- `paykit.allowance_rejection`
- `paykit.allowance_end`
- `paykit.payment_instruction`

Every kind is a FIFO Event Message. Receivers MUST preserve every valid event
in send order. Event order is FIFO within each sending direction; V1 defines no
total order across the two directions. Implementations MUST use causal event
references and MUST NOT infer cross-direction order from receipt time, local
timestamps, or clock comparison.

Every message is one UTF-8 JSON object and has these rules:

- `version` MUST be the JSON integer `1`.
- `kind` MUST be exactly one kind above.
- `event_id`, `allowance_id`, `payment_instruction_id`, and all causal event
  references MUST be canonical lowercase, hyphenated UUID-v4 strings where
  present.
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
Distinct Event IDs do not imply distinct Payment Instructions; semantic replay
is specified below.

## Allowance Terms

`paykit.allowance_proposal` carries this exact closed `terms` object:

```json
{
  "asset": "btc",
  "per_instruction_amount": {
    "minimum": "0.0001",
    "maximum": "0.01"
  },
  "period_limits": [
    {
      "amount_limit": "0.03",
      "instruction_count_limit": 5,
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
  "allowed_payees": [
    {
      "pubky_public_key": "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io",
      "receiver_path": "merchant/server"
    }
  ],
  "allowed_payment_endpoint_identifiers": ["btc-lightning-bolt12"]
}
```

Every displayed field is required. Nullable and collection fields behave as
follows:

| Field | V1 rule |
| --- | --- |
| `asset` | Non-empty and contains no control characters. Matching is exact and case-sensitive. |
| `per_instruction_amount` | `null`, or an inclusive range whose `minimum` is numerically no greater than `maximum`. |
| `period_limits` | Array of zero or more limits. Each limit has at least one non-null limit. |
| `lifetime_amount_limit` | `null`, or an amount ceiling for all admitted instructions. |
| `active_from` | `null`, or the first eligible instant, inclusive. |
| `expires_at` | `null`, or the first ineligible instant, exclusive. It MUST be later than `active_from` when both exist. |
| `allowed_payees` | `null`, or a non-empty array of unique Paykit Receiver References. |
| `allowed_payment_endpoint_identifiers` | `null`, or a non-empty array of unique, valid Payment Endpoint Identifiers. Matching is exact. |

`minimum`, `maximum`, `amount_limit`, and `lifetime_amount_limit` use the exact
`PaymentAmount.value` syntax: ASCII digits, at most one `.`, and at least one
digit. Signs, exponent notation, and grouping separators are invalid. `.5`,
`10.`, and leading or trailing zeros are valid. Implementations MUST compare
values with exact decimal arithmetic, not floating point. Original spelling is
preserved and is significant for semantic replay even when two spellings are
numerically equal. Paykit defines no asset precision, normalization, registry,
FX, or cross-asset comparison.

A `period_limits` entry has required `amount_limit` and
`instruction_count_limit` fields; either MAY be `null`, but not both.
`instruction_count_limit` is an unsigned 64-bit JSON integer. All configured
period entries apply independently and MUST be unique. Period and allowlist
array order has no eligibility meaning. V1 sets no separate array cardinality
limit beyond the complete-message byte limit.

Every rule is conjunctive: an instruction qualifies only if all configured
rules pass. V1 has no OR groups, deny rules, precedence, conversion, or implied
defaults. At least one field other than `asset` MUST constrain the authority;
terms with null amount, lifetime, time, and allowlist fields and no period
limits are invalid.

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

For either anchored style, include the candidate and prior admitted
instructions whose wallet admission time `s` satisfies
`boundary(k) <= s < boundary(k+1)` for the interval containing evaluation time
`t`.

For a rolling period, `L` is the same fixed-second conversion. When evaluating
a candidate at `t`, include that candidate and prior admitted instructions with
wallet admission time `s` satisfying `t - L < s <= t`. An instruction exactly
on the lower boundary has left the window.

For every applicable period, adding the candidate amount and count MUST leave
the amount total numerically at or below `amount_limit` and the count at or
below `instruction_count_limit` where those limits are non-null.
Implementations MUST use checked duration, calendar, count, and boundary
arithmetic. An overflow or unrepresentable boundary makes the evaluation
ineligible; it MUST NOT wrap, saturate, or omit the affected rule.

## Payees and destinations

A Paykit Receiver Reference has this closed wire shape:

```json
{
  "pubky_public_key": "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io",
  "receiver_path": "merchant/server"
}
```

`pubky_public_key` MUST be canonical 52-character z-base-32 Pubky public-key
text. `receiver_path` MUST be a valid Paykit Receiver Path. References compare
by both exact validated fields.

A Destination Reference has this closed wire shape:

```json
{
  "payee": {
    "pubky_public_key": "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io",
    "receiver_path": "merchant/server"
  },
  "payment_endpoint_identifier": "btc-lightning-bolt12",
  "payment_endpoint_payload_sha256": "0228e06e9aff38b11f633089b8fab5c797ba907bde12440f1bbcd5464ce2e1ac"
}
```

The digest is 64 lowercase hexadecimal characters computed from the complete
reference and exact payload bytes as:

```text
SHA-256(
  ASCII("paykit.allowance.destination.v1") || 0x00 ||
  ASCII(payee_pubky_public_key) || 0x00 ||
  ASCII(payee_receiver_path) || 0x00 ||
  ASCII(payment_endpoint_identifier) || 0x00 ||
  UTF8(exact_payload)
)
```

The example digest is for the displayed reference and exact payload
`{"value":"lno1example"}`. No Unicode normalization, JSON parsing, whitespace
change, newline change, or other transformation occurs before hashing.

When `allowed_payees` is non-null, the Payee MUST appear in it. When
`allowed_payment_endpoint_identifiers` is non-null, the identifier MUST appear
in it. The selected public endpoint is the exact file owned by that Payee's
Pubky key under that receiver path and identifier. The terms asset and endpoint
identifier are independent exact values; V1 MUST NOT infer an asset from
identifier segments. There is no fallback endpoint selection in V1.

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
    "per_instruction_amount": {
      "minimum": "0.0001",
      "maximum": "0.01"
    },
    "period_limits": [
      {
        "amount_limit": "0.03",
        "instruction_count_limit": 5,
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
    "allowed_payees": [
      {
        "pubky_public_key": "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io",
        "receiver_path": "merchant/server"
      }
    ],
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

| Valid causally linked events | Derived state | May qualify instructions? |
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

## Payment Instruction

Only the Allowee may send a Payment Instruction, and only after the referenced
acceptance is known to it.

```json
{
  "version": 1,
  "kind": "paykit.payment_instruction",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204",
  "allowance_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44",
  "proposal_event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
  "acceptance_event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202",
  "payment_instruction_id": "ca30e14b-4d41-4c79-a5a0-9681a7bf3051",
  "amount": {
    "value": "0.0050",
    "asset": "btc"
  },
  "destination": {
    "payee": {
      "pubky_public_key": "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io",
      "receiver_path": "merchant/server"
    },
    "payment_endpoint_identifier": "btc-lightning-bolt12",
    "payment_endpoint_payload_sha256": "0228e06e9aff38b11f633089b8fab5c797ba907bde12440f1bbcd5464ce2e1ac"
  }
}
```

The causal IDs MUST name the Allowance's bound proposal and acceptance. The
Payment Amount MUST follow the syntax above, and its asset MUST exactly equal
the terms asset. The amount and Destination Reference are exact; Paykit and the
wallet MUST NOT substitute another amount, asset, Payee, receiver path,
identifier, or payload. This does not constrain wallet-internal funding or
routing through another asset; such conversion is outside Paykit and cannot
change the instructed Payment Amount.

The semantic replay key is the exact Allower Receiver Reference, Allowee
Receiver Reference, Allowance ID, and Payment Instruction ID. The first valid
event for that key binds its semantic content: causal IDs, exact amount strings,
and exact destination fields. A later event with the same key is a semantic
replay if all those fields are exactly equal, even when its Event ID differs. A
difference in any field, including `1.0` versus `1.00`, is a semantic conflict
and MUST fail closed. Event ID dedupe occurs independently before this
classification.

Replay handling is deterministic:

| Observation | Classification and effect |
| --- | --- |
| Same Event ID, sender, and bytes | Transport duplicate; no new logical instruction. |
| Same semantic key and fields, never admitted | Semantic replay; the wallet MAY reevaluate the same logical instruction. Its first admission consumes usage once. |
| Same semantic key and fields, already admitted | Semantic replay; no new execution or usage. |
| Same semantic key with any changed field | Conflict; fail closed. |
| Different Payment Instruction ID | Distinct protocol instruction, still subject to wallet replay and policy checks. |

A local decline does not change protocol lifecycle or burn the Payment
Instruction ID. The wallet MAY retain a stricter terminal local decision, but
V1 does not communicate it to the Allowee.

V1 defines no Payment Instruction result, acknowledgement, acceptance,
rejection, proof, or cancellation message. Receipts remain separate objects;
the existing Payment Proof is specific to Payment Requests and is not reused.

## Destination Observation

The Allower side compares the exact current public Payment Endpoint with the
instruction's Destination Reference:

| Observation | Exact result |
| --- | --- |
| `Match` | Fetch succeeded with a valid UTF-8 payload whose domain-separated digest equals the reference. |
| `Missing` | The exact public endpoint read returned `404`, `GONE`, or an empty file. |
| `Mismatch` | Fetch succeeded with valid UTF-8, but its digest differs. |
| `Unverifiable` | Transport, server, parsing, non-UTF-8, or other failure prevents an exact conclusion. |

An observation is point-in-time evidence, not authorization, guaranteed
freshness, or proof of later availability. Replacement yields Mismatch and
deletion yields Missing. A wallet MUST require Match and re-fetch the exact
endpoint near its irreversible execution step; a cached or earlier Match alone
is insufficient. If End, expiry, Mismatch, Missing, or Unverifiable is observed
before that step, the wallet MUST NOT start a new payment for the instruction.

Authenticated overwrite or deletion in the Payee's Pubky storage is V1's
replacement or withdrawal mechanism. The public read provides no revision
history, tombstone, endpoint expiry, or global freshness proof. Private Payment
Lists and private or embedded destination descriptors are outside V1.

## Eligibility, usage, and execution boundary

The wallet evaluates at trusted time `t`. Shared eligibility requires all of:

- authenticated Allowee sender and matching causal lifecycle;
- accepted and not ended authority;
- active and unexpired terms;
- exact asset, inclusive amount range, all period limits, lifetime limit, and
  any configured Payee or endpoint-identifier allowlist;
- no semantic conflict, and no prior admission for the semantic replay key; and
- a current `Match` Destination Observation.

These are maximum shared terms, not a promise. The wallet MAY apply stricter
private safeguards or decline any instruction, but MUST NOT expand the shared
authority.

Usage is wallet-owned durable state. An instruction consumes one count and its
exact Payment Amount once the wallet admits it for execution. The wallet is
responsible for recording admission with concurrency control before an
irreversible side effect. Declined instructions consume nothing. Once admitted,
failed or unknown attempts remain counted; fees do not add usage and refunds do
not restore it. Replays never create more than one usage entry for the same
semantic key. V1 does not communicate usage or payment outcome to the Allowee.

## Durability and recovery

Receivers SHOULD durably persist received stream items, their authenticated
link scope, raw bytes, and validation result before wallet side effects.
Replayed events after a checkpoint loss are expected and use the dedupe rules
above.

Validated Encrypted Link recovery for the same Receiver References does not
require fresh Allowance consent when the complete durable event history is
retained. While link recovery is required, or when lifecycle or dedupe history
is incomplete, the SDK MUST NOT qualify Payment Instructions for automatic
wallet handling. A new link does not reconstruct missing Allowance history.

## Compatibility and size

V1 is closed-world. Unknown fields, enum values, digest encodings, or period
shapes inside a recognized V1 kind are invalid. Unsupported versions and
unknown kinds MUST NOT be interpreted as V1 or cause side effects. Durable
private-stream implementations MUST retain their raw bytes for audit and future
upgrade. Any recoverably Allowance-correlated unknown kind or unsupported
version, including a lifecycle or Payment Instruction message, MUST block
automatic handling for that Allowance until a compatible implementation or
explicit review resolves it. Unrelated unknown kinds do not change V1
Allowance state. Extensions require a new version or message kind.

Representative compact encodings fit the 1000-byte cap:

| Message | Representative content | Compact UTF-8 bytes |
| --- | --- | ---: |
| Proposal | Complete terms example above | 700 |
| Acceptance / rejection | Common IDs and proposal reference | 213 / 212 |
| End | Common IDs and both causal references | 267 |
| Payment Instruction | Complete example above | 667 |

The values are planning examples, not alternate limits. Implementations MUST
measure the actual complete serialized message. Large arrays or long decimal
spellings may exceed 1000 bytes and MUST be rejected as a whole; V1 has no
fragmentation or indirection.

## Component responsibilities

| Component | V1 responsibility |
| --- | --- |
| Paykit Protocol / Paykit Library | Closed wire types, parsing, serialization, structural validation, causal correlation helpers, digest calculation, and stateless destination observation helpers. |
| Paykit SDK | Durable ordered inbound and outbound events, Event ID dedupe, semantic replay classification, lifecycle derivation, correlation, recovery, and wallet-facing evidence. |
| Wallet | Consent UI, trusted time, private safeguards, usage and concurrency, freshness recheck, capacity, payment-method validation, signing, execution, and settlement. |

The Library MUST remain stateless and does not authorize payment. The SDK
coordinates evidence and MUST NOT turn eligibility into a payment decision.
Session creation, Pubky capabilities, key rotation, request timeout, and
payment execution remain caller or wallet responsibilities.
