# Paykit

> WIP - not for production.

## Description

Paykit helps apps discover where someone can receive a payment through their
Pubky identity. As a meta payment protocol, it also provides a layer for
payment-related metadata such as Payment Requests, Payment Proofs, receipts,
and Receipt Access.

A payee can publish public payment details under their Pubky public key, or
share a Private Payment List with another person over an Encrypted Link. Paykit
handles the common Pubky storage layout, encrypted message formats, Payment
Request messages, receipts, and language bindings needed for that exchange.

Wallets, payment processors, and apps keep control of payment execution,
business rules, payment selection, local storage, key rotation, recurring
Payment Request scheduling, Payment Request lifecycle state, and timeouts.
Paykit is the discovery and exchange layer those systems can integrate.

For canonical protocol vocabulary, see [THESAURUS.md](THESAURUS.md).

## Paykit Protocol

Paykit Protocol defines the domain rules, data model, and flows for payment
discovery and exchange through Pubky Routing.

### Pubky Routing

Paykit uses Pubky as its only network and storage backend. Public data is stored
on Pubky homeservers under paths owned by a Pubky public key, and private Paykit
messages are exchanged through `pubky-noise`.

Public Payment Endpoint Payloads are stored as separate files under:

```text
/pub/paykit/v0/{payment_endpoint_identifier}
```

Public reads use `pubky::PublicStorage`; authenticated writes use
`pubky::PubkySession`. Missing files or directories are treated as absent data
rather than protocol errors.

Private Application Messages use `pubky-noise`. Paykit derives per-counterparty
private folders during the Encrypted Link Handshake, while `pubky-noise` owns
encryption, file naming, counters, and storage slots.

### Core Vocabulary

- **Payment Method**: the broad human/domain concept for how value can move,
  such as Bitcoin, Lightning, SEPA, ACH, or a card rail.
- **Payment Endpoint Identifier**: the machine-readable identifier for a
  Payment Endpoint type, such as `btc-lightning-bolt12`.
- **Payment Endpoint Payload**: the receiving handle/details for that
  identifier, such as an address, invoice, offer, IBAN, tag, or JSON descriptor.
- **Payment Endpoint**: the pairing of a Payment Endpoint Identifier and a
  Payment Endpoint Payload.
- **Payment List**: a collection of Payment Endpoints published by a payee or
  shared privately with a counterparty.
- **Private Payment List**: a versioned Private Application Message carrying
  a complete Payment List over an Encrypted Link.
- **Payment Request**: a private protocol object where a payee asks a payer for
  one-time or recurring payment. Its lifecycle messages use Event Message
  semantics.
- **Payment Amount**: decimal `value` text plus an `asset`, used by Payment
  Requests and optional Receipt details.
- **Payment Proof**: method-specific evidence for one concrete payment
  execution.
- **Receipt Access**: an Event Message that lets a counterparty retrieve and
  decrypt an Encrypted Receipt.

## Payment Lists

Paykit can describe many kinds of payment details as long as payer and payee
understand the same Payment Endpoint Identifiers. The recommended identifier
convention is documented in
[specs/payment-endpoint-identifier.md](specs/payment-endpoint-identifier.md).
The convention is recommended for interoperability, but the library only
enforces structural path-safety validation.

### Public Payment Lists

Public Payment Lists are discoverable by anyone who knows the payee's Pubky
public key.

1. The payee creates one or more Payment Endpoints.
2. The payee writes each Payment Endpoint Payload under its Payment Endpoint
   Identifier.
3. The payee shares their Pubky public key.
4. A payer calls `get_payment_list` or `get_payment_endpoint` through the
   Paykit Library or a Language Binding.

Public Payment Lists are observable by anyone with the payee public key. Apps
should avoid publishing reusable or correlation-sensitive Payment Endpoint
Payloads unless that matches the payee's privacy model.

### Private Payment Lists

Private Payment Lists are shared only with a counterparty over an
established Encrypted Link.

1. The counterparties create an Encrypted Link with `initiate_encrypted_link` /
   `accept_encrypted_link` and advance it with `advance_handshake`.
2. The payee builds a complete Private Payment List containing the
   counterparty-specific Payment List.
3. The payee sends it with `set_private_payment_list`.
4. The payer receives the raw Private Application Message stream with
   `EncryptedLink::receive_private_application_messages` and parses
   Private Payment List messages with `parse_private_payment_list_json`.

Private Payment Lists use Latest-State Message semantics: newer list messages
supersede older queued list messages of the same kind. Only valid list messages
participate in latest-state selection; malformed newer messages do not supersede
the latest valid state. The caller is responsible for maintaining the complete
`payment_endpoints` map and sending the full desired Payment List on each
update.

## Payment Selection

Paykit helps wallets and processors discover candidate Payment Endpoints. It
does not execute payments or choose the final endpoint. The caller decides which
Payment Endpoint to use according to its own Payment Selection Policy.

When an Encrypted Link exists, callers can prefer the latest Private Payment
List. If no Encrypted Link or Private Payment List is available, callers
can fall back to the payee's public Payment List when that is acceptable for the
payment's privacy model.

If payment execution fails because an endpoint was consumed, expired, or changed,
callers should re-fetch the relevant Payment Endpoint or Payment List and apply
their own retry policy.

## Payment Interactivity

Payment Endpoint Payloads can represent static or interactive payment flows.
Paykit transports the Payment Endpoint Payload; the payment-specific protocol is still
implemented by the wallet or processor.

### Interactive Payments

A Payment Endpoint Payload may point to a server, offer, API, or protocol flow
that requires the payer to interact before a payment can be executed.

### Non-Interactive Payments

A Payment Endpoint Payload may also contain a static receiving detail such as an
on-chain address, reusable offer, bank account detail, payment tag, or similar
handle.

## Receipts

Paykit receipts are encrypted before storage. The plaintext Receipt is created
and read locally; the payee stores only the Encrypted Receipt at the canonical
homeserver path derived from the Receipt ID, then sends Receipt Access to the
counterparty over the Encrypted Link.

Receipt Access uses Event Message semantics: every valid Receipt Access message
matters. Apps that process multiple Private Message Kinds should consume the
full Encrypted Link Private Application Message stream, persist
handled/unhandled state, and only then persist the advanced Encrypted Link
snapshot; the snapshot is the local read checkpoint.

Receipt Decryption Keys are sensitive. Callers must not log raw key material and
should store it only in platform secure storage.

## Paykit Library

The Paykit Library is a stateless Rust library for interacting with Paykit
Protocol data on Pubky. It is intended to be used inside wallets, payment
processors, and apps that already own their payment execution logic.

For release history and upgrade notes, see [CHANGELOG.md](CHANGELOG.md).

### Intended Users

#### Wallets

Wallets can use Paykit to publish their own receiving details, discover payment
details for contacts or counterparties, exchange Private Payment Lists over
an Encrypted Link, and receive Encrypted Receipts.

#### Payment Processors

Payment processors can use Paykit to expose the Payment Endpoints they support,
retrieve Payment Lists for payees, and apply their own Payment Selection Policy
before executing a payment through their existing infrastructure.

### Boundaries

`paykit-lib` and platform bindings do not:

- execute payments
- choose the final Payment Endpoint for a payer
- manage recurring execution, recurring Payment Request scheduling, or Payment
  Request lifecycle state
- maintain a stateful background service/runtime
- fetch profiles or contacts
- manage Pubky session creation, authorization scope, key rotation, or account
  recovery

`paykit-sdk` is the Rust runtime layer for SDK-managed local
state such as endpoint sync, Encrypted Link snapshots, private stream intake,
Private Payment Lists, Paykit Profiles, Paykit Blob helpers, read-only Pubky
app profile/follows helpers, local Contact Records, and contact payment resolution. Payment
execution, settlement detection, product UI, and platform session storage
remain with the integrating application and its adapters.

## Library Crates

- [`paykit-lib`](paykit-lib/) is the canonical Rust Paykit Library. It consumes
  concrete Pubky SDK handles and keeps no global application state.
- [`paykit-sdk`](paykit-sdk/) is the Rust SDK runtime for stateful Paykit
  workflows.
- [`paykit-ffi`](paykit-ffi/) exposes UniFFI bindings for Swift and Kotlin.
- [`paykit-react-native`](paykit-react-native/) wraps the generated bindings for
  React Native.

## Functional Requirements

### Public Payment Data

#### Retrieve a public Payment List

`get_payment_list` fetches all public Payment Endpoints published by a payee.
The result is empty when the payee has not published any endpoints.

#### Retrieve one public Payment Endpoint Payload

`get_payment_endpoint` fetches one Payment Endpoint Payload for a payee and a
Payment Endpoint Identifier. Missing files are returned as `None`.

#### Store a public Payment Endpoint

`set_payment_endpoint` publishes or updates one Payment Endpoint Payload under
the caller's authenticated Pubky session.

#### Remove a public Payment Endpoint

`remove_payment_endpoint` removes a previously published Payment Endpoint.

### Private Payment Data

#### Establish an Encrypted Link

`initiate_encrypted_link`, `accept_encrypted_link`, and `advance_handshake`
perform the Encrypted Link Handshake. Handshakes and established Encrypted Links
can be serialized and restored by callers that need restart recovery.

Serialized Encrypted Link snapshots include sensitive key material. Store them
encrypted at rest and never log them or expose them in telemetry.

#### Send a Private Payment List

`set_private_payment_list` sends a complete Private Payment List over an
Encrypted Link. The serialized message must fit within one `pubky-noise`
message.

#### Receive Private Application Messages

`EncryptedLink::receive_private_application_messages` returns the available
Private Application Message batch in send order. SDK/runtime code should persist
and route that raw stream, then use stateless parsers such as
`parse_private_payment_list_json`, `parse_payment_request_event_message`,
and `parse_receipt_access_event_message`. The raw payload is preserved even
when parsed `version`/`kind` header fields are missing or malformed.

#### Exchange Payment Requests

Payment Request protocol messages are Event Messages. Use
`EncryptedLink::receive_private_application_messages` when deriving durable
state across multiple Private Message Kinds. Payment Request events can
then be parsed with `parse_payment_request_event_message`, which keeps the
canonical kind, raw payload, and parse result so malformed recognized messages
can be persisted before the app persists an advanced Encrypted Link snapshot or
treats them as handled.

For outbound idempotency, SDK/runtime code can call
`serialize_payment_request_event` and persist the exact JSON payload before
sending. A retried send should reuse the same `event_id` and payload.

#### Issue and Receive Encrypted Receipts

Apps issue receipts in retryable steps: `prepare_receipt` creates the plaintext
Receipt locally, encrypts it into an Encrypted Receipt, and returns the matching
Receipt Access descriptor. `store_prepared_receipt` stores the Encrypted
Receipt, and `send_receipt_access` sends access to the counterparty. Receivers
get Receipt Access messages from the raw Private Application Message stream and parse them with
`parse_receipt_access_event_message`, or `parse_receipt_access_json` when they
already have a known Receipt Access JSON payload. `decrypt_receipt` decrypts an
Encrypted Receipt fetched by the app from its Receipt Location into the local
plaintext Receipt.
Receipt Location is a path on the issuer's homeserver; SDK/runtime code pairs
it with the Receipt Access sender/issuer context when retrieving the Encrypted
Receipt.

Private Application Messages share one ordered encrypted stream. The raw stream
API returns every received Private Application Message plaintext payload in
send order, including malformed JSON payloads. Callers that trigger side
effects from Event Messages must persist and reconcile their
own handled/unhandled event state before persisting a snapshot whose read
counter has advanced past those messages. If event state is persisted but the
snapshot is not, replay is expected; Event Messages should be deduped by
`event_id`, while Receipt Access can also be reconciled by Receipt ID and caller
receipt state.

Paykit v0.2 private wire messages are closed-world JSON objects:
unknown fields are rejected unless a field is explicitly defined as an open JSON
object, such as Payment Request `metadata`, Payment Proof `proof`, or Receipt
Metadata.

## Main Rust APIs

### Public Payment Endpoints

- `set_payment_endpoint(session, identifier, payload)`: publish or update one
  public Payment Endpoint.
- `remove_payment_endpoint(session, identifier)`: remove one public Payment
  Endpoint.
- `get_payment_list(storage, payee)`: fetch the payee's public Payment List.
- `get_payment_endpoint(storage, payee, identifier)`: fetch one public Payment
  Endpoint Payload.

### Encrypted Links

- `initiate_encrypted_link(...)` / `accept_encrypted_link(...)`: start the
  Encrypted Link Handshake.
- `advance_handshake(...)`: progress the handshake until it returns a completed
  `EncryptedLink`.
- `EncryptedLink::serialize()` / `restore_encrypted_link(...)`: snapshot and
  restore an established Encrypted Link.
- `EncryptedLink::receive_private_application_messages()`: receive the full
  Private Application Message stream batch in send order for SDK/runtime
  routing.
- `EncryptedLinkHandshake::serialize()` /
  `restore_encrypted_link_handshake(...)`: snapshot and restore an in-progress
  handshake.

### Private Payment Lists

- `set_private_payment_list(link, list)`: send a complete Private
  Payment List over the Encrypted Link.
- `parse_private_payment_list_json(json)`: parse a Private Payment List
  from a raw Private Application Message.

### Payment Requests

- `send_payment_request(link, request)`: send a payee-initiated Payment Request.
- `send_payment_request_acceptance(link, acceptance)`: send payer acceptance
  for a Payment Request.
- `send_payment_request_rejection(link, rejection)`: send payer rejection for
  a Payment Request.
- `send_payment_request_cancellation(link, cancellation)`: send payer or payee
  cancellation for a Payment Request.
- `send_payment_proof(link, proof)`: send payer-submitted Payment Proof after
  payment.
- `parse_payment_request_event_message(message)`: parse a raw Private
  Application Message as a Payment Request event when applicable.
- `serialize_payment_request_event(event)`: serialize a Payment Request event
  so SDK/runtime code can persist the outbound payload before sending.
- `PaymentProof::validate_for_request(request)`: validate stateless proof and
  request correlation fields.

### Receipts

- `prepare_receipt(link, draft)`: build the plaintext Receipt, Encrypted Receipt, and Receipt
  Access descriptor without storing or sending. Receipt drafts may include
  optional `payment_request_id` and `billing_period` fields for Payment Request
  correlation.
- `store_prepared_receipt(session, prepared)`: store a prepared Encrypted
  Receipt at its Receipt Location.
- `send_receipt_access(link, access)`: send a prepared Receipt Access descriptor
  over the Encrypted Link.
- `parse_receipt_access_event_message(message)`: parse a raw Private
  Application Message as a Receipt Access event when applicable.
- `parse_receipt_access_json(json)`: parse Receipt Access from a raw private
  JSON payload.
- `decrypt_receipt(encrypted_json, key, location)`: decrypt a Receipt fetched by
  the app from its Receipt Location.

## Payment Endpoint Identifier Convention

Example identifiers:

```text
btc-bitcoin-p2tr
btc-lightning-bolt11
btc-lightning-bolt12
eur-sepa-iban
```

Example Payment Endpoint Payload:

```json
{
  "value": "lnurl1...",
  "label": "primary lightning endpoint"
}
```

## Development

```sh
cargo fmt
cargo clippy --all-targets --all-features
cargo test
cargo doc --no-deps
```

Platform bindings must be built for every target:

```sh
cd paykit-ffi
./build.sh all
```

## Resources

- First draft implementation of paykit library:
  <https://github.com/pubky/paykit-pdk>
- Product overview:
  <https://docs.google.com/document/d/1Z1HHdxpkOtelOXJRgPldso4_-lchzs3NL_JqDxCdiu8/edit?pli=1&tab=t.0>
