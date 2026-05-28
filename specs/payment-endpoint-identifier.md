# Paykit Payment Endpoint Identifier Specification

| Field       | Value                                      |
| ----------- | ------------------------------------------ |
| Status      | Draft                                      |
| Version     | 0.1                                        |
| Last updated| 2026-04-21                                 |
| Repository  | <https://github.com/pubky/paykit-rs>       |

## Abstract

This document specifies a naming convention for *Payment Endpoint Identifiers*
in Paykit: short, structured strings that name an unambiguous way for a payee
to receive value. It also defines the shape of the JSON payload that
accompanies each identifier.

The intent is to give implementers and downstream bindings (Swift, Kotlin,
React Native) a single, consistent vocabulary to exchange, so that a payer
and payee who independently support "`btc-lightning-bolt12`" can recognise
each other without side-channel coordination.

## 1. Status of this document

This is a *recommended* convention, not a mandatory one. Paykit's routing
layer does not enforce identifier shape beyond the structural checks performed
by [`PaymentEndpointIdentifier`](../paykit-lib/src/lib.rs) (ASCII alphanumeric, `-`, `_`, `.`;
max 64 characters; no path traversal; the value `private` is reserved).

Identifiers that follow this specification are interoperable with Paykit
clients that follow the same conventions. Identifiers that do not follow it
may still be valid `PaymentEndpointIdentifier` values, but carry no cross-implementation
guarantees.

A future revision may introduce a formal registry, additional conformance
checks, or normative validators. Until then, the grammar and conventions
below are the working agreement.

## 2. Conformance

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY**
in this document are to be interpreted as described in [RFC 2119] when, and
only when, they appear in all capitals.

## 3. Terminology

- **Payment Endpoint Identifier** (or *identifier*): the three-segment string
  that names a Payment Endpoint type, e.g. `btc-bitcoin-p2tr`. In the
  Paykit library this is represented by the `PaymentEndpointIdentifier` type.
- **Payment Endpoint Payload**: the JSON object stored alongside the
  identifier, describing the specific payee handle for a Payment Endpoint of
  that type.
- **Asset**, **Rail**, **Endpoint Format**: the three positional segments of a
  Payment Endpoint Identifier, defined in Section 5.
- **Segment**: one positional component of an identifier, delimited by `-`.

Paykit's higher-level terms (*Payment Method*, *Payment Endpoint*,
*Payment List*) are defined in the top-level [README](../README.md);
this specification does not redefine them.

## 4. Grammar

An identifier is a three-segment string of the form
`{asset}-{rail}-{endpoint_format}`.

```abnf
identifier      = asset "-" rail "-" endpoint-format
asset           = 1*segment-char
rail            = 1*segment-char
endpoint-format = 1*segment-char
segment-char    = %x61-7A / DIGIT         ; lowercase a-z or 0-9
```

The following rules apply:

1. All three segments MUST be present and non-empty.
2. Each segment MUST consist of one or more characters from the lowercase
   ASCII alphabet (`a`–`z`) and decimal digits (`0`–`9`).
3. The only separator between segments is the hyphen `-`. A segment MUST NOT
   itself contain a hyphen, underscore, dot, or any other punctuation.
4. Identifiers are compared byte-for-byte. Implementations MUST NOT perform
   case folding, Unicode normalisation, or alias resolution when matching.

## 5. Segments

### 5.1 Asset

The *asset* segment names the unit of value being transferred.

- For cryptoassets, use the lowercased ticker (`btc`, `usdt`, `eth`).
- For fiat, use the lowercased [ISO 4217] code (`usd`, `eur`, `gbp`).
- The asset segment MUST NOT encode the chain or token standard; the rail
  segment disambiguates those.

### 5.2 Rail

The *rail* segment names the settlement system carrying the asset. Examples:

- Base chain: `bitcoin`, `ethereum`, `solana`, `tron`.
- Layer-2 or off-chain: `lightning`, `liquid`, `arbitrum`.
- Fiat network: `sepa`, `ach`, `fps`.
- Custodial provider: `revolut`, `cashapp`, `wise`.

When a rail is commonly written with an internal space or hyphen (e.g.
"Faster Payments"), collapse it to a single lowercase token (`fps`) rather
than introducing intra-segment punctuation.

### 5.3 Endpoint Format

The *endpoint format* segment names the handle format on that rail
(`p2tr`, `bolt11`, `bolt12`, `address`, `iban`, `tag`).

When a rail currently has a single canonical address format, use `address`
rather than inventing a more specific name. Reserve distinct Endpoint Format
values for rails that genuinely expose multiple incompatible formats (for
example, Bitcoin's `p2wpkh`, `p2tr`, and so on).

Whichever name is used, authors SHOULD still name it explicitly, so that
future formats on the same rail can be added as new identifiers without
ambiguity or breaking changes.

## 6. Examples

### 6.1 Valid

```
btc-bitcoin-p2tr
btc-lightning-bolt11
btc-lightning-bolt12
usdt-ethereum-address
usdt-tron-address
usdc-solana-address
usd-revolut-tag
eur-sepa-iban
gbp-fps-sortcode
```

### 6.2 Invalid

| Identifier                     | Reason                                  |
| ------------------------------ | --------------------------------------- |
| `Btc-bitcoin-p2tr`             | Uppercase characters are not permitted. |
| `usdt-ethereum`                | Endpoint Format segment is missing.     |
| `gbp-faster-payments-sortcode` | Rail segment contains an internal `-`.  |
| `usd-revolut-pay_link`         | Underscores are not permitted.          |
| `-btc-bitcoin-p2tr`            | Empty leading segment.                  |
| `btc--p2tr`                    | Empty middle segment.                   |

## 7. Payload

Each identifier is paired with a *payload* object that carries the actual
receiving details. A payload MUST be a JSON object; bare strings, numbers, and
arrays are not valid payloads.

### 7.1 Required fields

- `value` *(string, required)*: the primary handle on the rail. This is the
  address, invoice, offer, IBAN, tag, or equivalent identifier that a payer
  needs in order to initiate a transfer.

### 7.2 Recommended field names

Beyond `value`, a payload MAY contain any fields the Endpoint Format
requires. Additional fields MAY be of any JSON type (string, number,
boolean, array, or object), chosen to fit the data being represented.
Unknown fields MUST be ignored by receivers that do not recognise them.

The following field names are not required, but SHOULD be used where they
apply, to keep naming consistent across Payment Endpoint Payloads:

- `min` *(string)*: minimum amount the Payment Endpoint will accept, in the
  asset's major unit, represented as a decimal string to avoid
  floating-point rounding.
- `max` *(string)*: maximum amount the Payment Endpoint will accept, in the same
  form as `min`.
- Endpoint Format-specific identifying fields (for example `bic`,
  `beneficiary_name`, `memo`): named descriptively, in lowercase with
  underscores separating words.

Authors defining a new Endpoint Format MAY introduce further fields as needed.
Consistency with existing patterns is encouraged but not enforced.

### 7.3 What does not belong in the payload

Per-payment details (the specific amount being sent, a memo for a single
transfer, a label attached to one payment) belong in the *payment request*,
not in the Payment Endpoint Payload. The payload describes what the payee
accepts; the request describes a specific transfer.

### 7.4 Examples

```json
// btc-bitcoin-p2tr
{ "value": "bc1p..." }
```

```json
// btc-lightning-bolt12
{
  "value": "lno1...",
  "min": "0.0001",
  "max": "0.01"
}
```

```json
// eur-sepa-iban
{
  "value": "DE89370400440532013000",
  "bic": "COBADEFFXXX",
  "beneficiary_name": "Jane Doe"
}
```

## 8. Extending the format

New identifiers are introduced simply by using them, following the
conventions in Section 5. Before minting a new identifier, authors SHOULD
check whether an existing identifier already covers the case. Duplicate
names for the same semantic Endpoint Format harm interoperability.

Changes to an identifier's *meaning* after it is in use are a breaking
change for any payer that has already implemented it. Where a new variant
of a format emerges on the same rail, prefer a new Endpoint Format segment
(`bolt12` alongside `bolt11`) to redefining an existing one.

## 9. Relation to `paykit-lib`

In the reference implementation, a Payment Endpoint Identifier is stored
as a [`PaymentEndpointIdentifier`](../paykit-lib/src/lib.rs). The `PaymentEndpointIdentifier` constructor
performs structural validation (character set, length, reserved values,
path-traversal rejection) but does not enforce the three-segment shape
defined here. Callers that want to verify conformance to this
specification SHOULD apply an additional check on top of `PaymentEndpointIdentifier::new`.

The reserved identifier `private` is used internally by Paykit for private
Paykit storage paths; it is rejected at `PaymentEndpointIdentifier`
construction and therefore cannot appear as a conforming identifier under this
specification either.

## 10. Security considerations

- Identifiers are untrusted input when received from a counterparty. Implementations
  MUST treat them as opaque tokens and MUST NOT interpret any segment as a
  filesystem path, URL component, or shell argument without prior
  validation. `PaymentEndpointIdentifier::new` already rejects path-traversal sequences;
  callers interpolating identifiers into storage paths MUST continue to
  route them through that validated constructor rather than using raw
  strings.
- Payload values may contain sensitive data (bank account numbers, real
  names, recovery-style invoices). Public Payment Lists are visible to any
  party that knows the payee's public key; authors MUST consider this when
  deciding which Payment Endpoints to publish publicly versus exchange privately. See
  the Private Payment Envelope sections of the [README](../README.md) and
  [paykit-lib README](../paykit-lib/README.md) for the current Encrypted Link
  flow.
- Byte-for-byte comparison (Section 4, rule 4) is a security property as
  well as a correctness one: case-insensitive or normalising matchers can
  cause a payer to treat two distinct identifiers as equivalent and select
  the wrong Payment Endpoint.

## 11. References

- [RFC 2119]: Key words for use in RFCs to indicate requirement levels.
- [RFC 5234]: Augmented BNF for Syntax Specifications: ABNF.
- [ISO 4217]: Codes for the representation of currencies.
- Paykit [README](../README.md): protocol overview and vocabulary.
- Paykit contributor guide [AGENTS.md](../AGENTS.md): repository-level
  conventions referenced by this document.

[RFC 2119]: https://www.rfc-editor.org/rfc/rfc2119
[RFC 5234]: https://www.rfc-editor.org/rfc/rfc5234
[ISO 4217]: https://www.iso.org/iso-4217-currency-codes.html
