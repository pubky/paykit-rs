# Payment conversion and deadlines

Payment Request amounts describe the value requested. Accepted Payment Endpoint
Identifiers describe how it may be paid. Conversion terms are optional; Paykit
transports agreed prices and validates their correlation, without obtaining
market rates, executing payments or deciding settlement.

## Conversion terms

A Payment Request can include one of:

```json
{"conversion":{"type":"fixed","rates":[{"asset":"usdt","value":"1"},{"asset":"btc","value":"0.00001234"}]}}
```

```json
{"conversion":{"type":"per_period"}}
```

`fixed` rates are immutable and apply to every installment when the request is
recurring. `per_period` is only valid for recurring requests and requires a
payee-issued Payment Conversion Quote for each cross-asset payment. Omission of
`conversion` leaves cross-asset conversion to application policy. Explicit null,
unknown fields and unknown policy types are invalid.

A rate is **units of payment asset per one unit of requested asset**. For a
request denominated in `usd`, a `btc` rate of `0.00001234` means 0.00001234 BTC per
USD. Both wallets multiply the requested amount by that rate using exact decimal
or integer arithmetic and round the result upward once to the selected payment endpoint's
smallest supported unit. Network fees are additional; they are not deducted from
the requested payment value. Rates must be positive decimal strings, without
signs, exponents or grouping separators. The grammar is `[0-9]+(\.[0-9]*)?`
or `\.[0-9]+`; leading and trailing zeroes are allowed. These are exact
decimals, not floating-point values. Asset codes must be unique within the
nonempty rates list. Rates need not sum to anything.

Conversion-enabled requests use the asset-prefixed Payment Endpoint Identifier
convention: three nonempty lowercase alphanumeric segments, separated by
hyphens. The request asset must also be lowercase alphanumeric. A rate applies to all accepted endpoints whose first segment is that
asset, including Bitcoin on-chain and Lightning endpoints. Asset spelling is
case-sensitive. Rates must name an accepted asset different from the requested
asset; same-asset payments have an implicit rate of exactly one.

| Terms | Cross-asset behavior |
| --- | --- |
| No `conversion` | Payer approves its own calculation; payee independently evaluates the received value. No shared rate is promised. Applications may decline such conversions. |
| Fixed rates or a selected quote | Both parties use its exact rates. An omitted payment asset is unavailable; there is no market-rate fallback. |
| `per_period`, no quote selected | Cross-asset payment must wait for a usable quote. Same-asset payment remains possible without a quote. |

An accepted endpoint is necessary but does not guarantee conversion-rate coverage.
Request-creation UIs should avoid offering an unquoted cross-asset endpoint as
payable under fixed terms. USD and USDT are distinct protocol assets. An
application choosing parity must explicitly offer a `1` rate.

For example, a `0.05 usd` request at `0.000012345 btc/usd` requires 62 satoshis
on Bitcoin on-chain (61.725 rounded upward), or 61,725 millisatoshis on a
Lightning endpoint supporting millisatoshis. Both peers use the selected
endpoint precision, not a common rounding unit for every BTC rail. A `0.05 usd` request at `1 usdt/usd` requires exactly
50,000 atomic units when that token has six decimals. Token identity and precision
come from the selected, validated endpoint, not its display symbol alone.

## Payment deadlines

A one-time request can specify:

```json
{"payment_deadline":{"type":"at","timestamp":"2026-10-01T12:00:00Z"}}
```

A recurring request can specify:

```json
{"payment_deadline":{"type":"period_start","seconds":86400}}
```

The latter means each installment is due 86,400 elapsed seconds after its Billing
Period begins. `seconds` is a nonnegative integer. Calendar expansion and Billing
Period eligibility remain the application's recurrence policy. This offset does
not change the schedule or the meaning of `recurrence.ends_at`. Absolute deadlines
are invalid for recurring requests; period-relative deadlines are invalid for
one-time requests. Absent `payment_deadline` means no request-level payment
deadline. Explicit null is invalid.

`proposal_expires_at` still governs acceptance, not settlement. Payment deadlines
are inclusive and are evaluated against independently verified payment time,
**not** proof arrival, parsing, or receiver login time. A quote's expiry is an
additional deadline: both it and any request-level deadline must be met.

The payment method defines acceptable time evidence. For ERC-20 transfers this is
the canonical block timestamp of successful execution, not bundler acceptance.
Bitcoin applications must document their zero-confirmation observation policy;
a claimed broadcast timestamp is not independently verifiable. This extension
does not impose a new mining-confirmation requirement on Bitcoin. Lightning
applications use their verified settlement evidence.

Expired evidence is not malformed merely because it arrives late. Wallets still
credit received funds, and may label them underpaid, after expiry, or both. Paykit
does not add refunds, disputes, top-up aggregation or resolution workflows.

## Payment Conversion Quote

The payee can issue quotes after an opted-in recurring request has been accepted:

```json
{
  "version": 1,
  "kind": "paykit.payment_conversion_quote",
  "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
  "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
  "billing_period": {"starts_at":"2026-10-01T00:00:00Z","ends_at":"2026-11-01T00:00:00Z"},
  "rates": [{"asset":"usdt","value":"1"},{"asset":"btc","value":"0.00001234"}],
  "valid_from": "2026-10-01T00:00:00Z",
  "expires_at": "2026-10-02T00:00:00Z"
}
```

The Event ID is the quote identifier; no second identity or revision counter is
needed. A quote is an authenticated payee Event Message, scoped to the exact
Encrypted Link and request. It has its own nonempty rates list and required
validity interval (`valid_from` through `expires_at`, inclusive). The SDK sets
`valid_from` to its issuance clock rounded down to whole seconds, matching
common chain timestamp precision. The payee must not backdate quotes to reprice
existing payments. Wallets compare independently verified payment time against
both bounds; selecting a later quote cannot satisfy an earlier underpayment.
The payer cannot issue it. A quote cannot change the requested amount,
asset, endpoints, recurrence or other immutable request terms.

A newer quote never overwrites an older one. **Each issued quote remains usable
from its validity start until its own expiry**, subject to the request deadline and cancellation policy.
The payee commits to each price for that interval; applications should choose
quote lifetimes accordingly. A wallet may choose a newer quote before approval,
but cannot silently switch the approved amount or selected quote afterward.
Quote publication or receipt never grants spending authorization.

The payer sets the Payment Proof's optional `conversion_quote_id` to the selected
quote's Event ID and supplies the same `billing_period`. The quote must belong to
the same request, period and payee; its rates must cover the payment asset. A
quote identifier is invalid on requests without `per_period` conversion. The
identifier is required for cross-asset proofs under `per_period`, and optional
for same-asset payments. Method-specific signatures must bind this identifier and
period, not just the long-lived Payment Request ID.

The SDK preserves all quotes and validates quote/proof correlation without
rejecting evidence based on the current clock. Applications check actual amount,
payment-time validity, Billing Period eligibility and whether a transfer has already
been used. A single payment must not satisfy multiple requests or periods. Each
installment has its own local payment status; proof submission does not settle the
whole recurring request. A quote crossing a cancellation may remain historical
evidence for a payment already underway; it does not reopen the request. New quote
issuance through the SDK requires an active recurring request.

## Integration

The Rust SDK exposes `quote_payment_request`, immutable `conversion_quotes` on
request records, and `conversion_quote_id` on proof submissions/records. Swift
and Kotlin expose the same fields and method. Wallets persist the selected quote
and period alongside their payment identity before execution, and retry evidence
delivery independently of execution. Signing a post-execution ERC-20 proof must
also survive interruption without causing a second payment.

Conversion timestamps use four-digit years, uppercase `T` and `Z`, and optional
fractional seconds; leap seconds are not supported. Equivalent Billing Period
instants compare equal even when fractional-second spellings differ. Signed
proof context keeps the timestamp strings carried by that proof verbatim.

Existing messages with no conversion/deadline fields retain their meaning. Peers
must support these fields and the new event before using them. This is a
coordinated protocol revision; the caller must establish support through its
application deployment or an explicitly agreed peer capability mechanism. The
existing `payment_requests` receiver flag does not advertise this extension.
The SDK does not discover extension support or negotiate a downgrade; implementations
must not strip unknown terms to make a request appear payable. Closed-world
parsing rejects unsupported terms. No on-chain execution, allowance accounting,
market-rate provider, new database table or automatic-payment authority is added.

All Event Messages must fit the existing encrypted message limit. Keep metadata
and rate lists compact; oversized messages are rejected by the outbound queue.
Before accepting a request or sending funds, wallets must also preflight the
complete maximum-size Payment Proof for the chosen method, including the actual
reference, period, optional IDs and JSON escaping. A request fitting in one
message does not imply its proof will fit. See the ERC-20 profile's size rules.
