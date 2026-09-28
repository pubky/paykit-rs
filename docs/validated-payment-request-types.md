# Validated Payment Request types: Rust migration

Payment Request domain types now validate before construction and expose
read-only accessors. This is a Rust source API change; it preserves accepted
wire values, protocol rules, and SDK backup record shapes. Swift and Kotlin
input records remain editable inputs and validate when converted into Rust
domain values.

Use `PaymentAmount::new(value, asset)` and
`BillingPeriod::new(starts_at, ends_at)` instead of struct literals. Both return
`Result`. Decimal spellings remain unchanged, including `.5`, `10.`, leading
zeros, and zero amounts. A Billing Period constructor checks UTC timestamp
syntax and increasing endpoints; it does not prove recurrence membership.

Construct a Recurrence with `Recurrence::try_from(RecurrenceConfig { ... })`.
The config is explicitly unvalidated input. Its fields remain `every`, `unit`,
`starts_at`, `anchor`, and `ends_at`; validation requires a positive interval
and valid timestamps, with an optional end later than the start. Anchor ordering
remains unconstrained.

Build complete Payment Request terms before creating a proposal:

```rust
use paykit_lib::{
    EventId, PaymentAmount, PaymentEndpointIdentifier, PaymentReference,
    PaymentRequest, PaymentRequestId, PaymentRequestTerms,
};

let terms = PaymentRequestTerms::builder(
    PaymentAmount::new(".5", "btc")?,
    PaymentReference::new("invoice-1")?,
    vec![PaymentEndpointIdentifier::new("btc-lightning-bolt11")?],
)
.proposal_expires_at(None)
.recurrence(None)
.metadata(serde_json::Map::new())
.build()?;
let request = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);
```

The builder requires amount, reference, and endpoint inputs. The optional
builder methods accept an optional expiry, an optional validated recurrence,
and a metadata map. `build()` rejects empty endpoint lists and invalid expiry
syntax; it does not reject historical expiry times or duplicate endpoint
identifiers that were already accepted by the protocol.

Replace field reads with methods: `request.request().amount().value()`,
`request.event_id()`, `period.starts_at()`, and so on. String accessors borrow
`str`; endpoint collections borrow slices; optional fields are borrowed
`Option` values, so existing `.as_ref()` and `.as_deref()` access patterns can
continue. Use `.to_owned()`, `.to_vec()`, or `.clone()` explicitly when ownership
is needed. To change terms, build a new value rather than mutate an existing
validated value.

All five Payment Request event constructors set their own version and kind.
Read headers with `version()` and `kind()`; callers cannot override them.
`PaymentProof::new` keeps its existing six validated-component arguments, and
`with_allowance_id` adds optional attribution. `validate_for_request` is still
required for stateless correlation with a particular Request. Valid component
types do not establish consent, lifecycle eligibility, recurrence membership,
settlement, or permission to pay.

SDK records and FFI input records are data records, not validated authority.
Conversions from these records and from raw wire objects now use the validating
constructors and terms builder. Malformed incoming data remains `InvalidData`,
caller input errors remain `Validation`, and private parse errors remain
redacted. Receipt parsing and preparation use the same validated Payment Amount
and Billing Period types; Receipt-specific context checks remain in place.
