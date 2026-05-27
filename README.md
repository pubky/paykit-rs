# Paykit

> WIP - not for production.

## Description

Paykit helps apps discover where someone can receive a payment through their
Pubky identity.

A payee can publish public payment details under their Pubky public key, or
share a Private Payment Envelope with another person over an Encrypted Link. Paykit
handles the common Pubky storage layout, encrypted message formats, receipts,
and language bindings needed for that exchange.

Wallets, payment processors, and apps keep control of payment execution,
business rules, payment selection, local storage, key rotation, subscription
logic, and timeouts. Paykit is the discovery and exchange layer those systems
can integrate.

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
- **Private Payment Envelope**: a versioned Private Application Message carrying
  a Payment Reference and a complete Payment List over an Encrypted Link.
- **Receipt Access**: an Event Message that lets a counterparty retrieve and
  decrypt an encrypted Receipt.

## Payment Lists

Paykit can describe many kinds of payment details as long as payer and payee
understand the same Payment Endpoint Identifiers. The recommended identifier
convention is documented in
[specs/payment-endpoint-identifier.md](specs/payment-endpoint-identifier.md).
The convention is recommended for interoperability, but the current library only
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

### Private Payment Envelopes

Private Payment Envelopes are shared only with a Linked Peer over an Encrypted
Link.

1. The counterparties create an Encrypted Link with `initiate_encrypted_link` /
   `accept_encrypted_link` and advance it with `advance_handshake`.
2. The payee builds a complete Private Payment Envelope containing a UUID-v4
   Payment Reference and the counterparty-specific Payment List.
3. The payee sends it with `set_private_payment_envelope`.
4. The payer calls `get_private_payment_envelope` and receives the newest queued
   envelope, if one is available.

Private Payment Envelopes use Latest-State Message semantics: newer envelopes
supersede older queued envelopes of the same kind. The caller is responsible for
maintaining the complete `payment_endpoints` map and sending the full desired Payment List on
each update.

## Payment Selection

Paykit helps wallets and processors discover candidate Payment Endpoints. It
does not execute payments or choose the final endpoint. The caller decides which
Payment Endpoint to use according to its own Payment Selection Policy.

For a Linked Peer, callers can prefer the latest Private Payment Envelope. If no
Encrypted Link or Private Payment Envelope is available, callers can fall back to the
payee's public Payment List when that is acceptable for the payment's privacy
model.

If a payment attempt fails because an endpoint was consumed, expired, or changed,
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

Paykit receipts are encrypted before storage. The payee stores the encrypted
Receipt at the canonical homeserver path derived from the Payment Reference,
then sends Receipt Access to the counterparty over the Encrypted Link.

Receipt Access uses Event Message semantics: every valid Receipt Access message
matters and typed getters must preserve send order. Apps that perform
irreversible side effects after receiving Receipt Access should persist their own
handled/unhandled state alongside Encrypted Link snapshots.

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
details for contacts or counterparties, exchange Private Payment Envelopes over
an Encrypted Link, and receive encrypted receipts.

#### Payment Processors

Payment processors can use Paykit to expose the Payment Endpoints they support,
retrieve Payment Lists for payees, and apply their own Payment Selection Policy
before executing a payment through their existing infrastructure.

### Current Boundaries

Paykit currently does not:

- execute payments
- choose the final Payment Endpoint for a payer
- manage subscriptions
- maintain a stateful daemon/runtime
- fetch profiles or contacts
- manage Pubky session creation, authorization scope, key rotation, or account
  recovery

These responsibilities remain with the integrating application, the Pubky SDK,
or future higher-level Paykit components.

## Library Crates

- [`paykit-lib`](paykit-lib/) is the canonical Rust Paykit Library. It consumes
  concrete Pubky SDK handles and keeps no global application state.
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

#### Send a Private Payment Envelope

`set_private_payment_envelope` sends a complete Private Payment Envelope over an
Encrypted Link. The serialized envelope must fit within one `pubky-noise`
message.

#### Receive a Private Payment Envelope

`get_private_payment_envelope` returns the newest queued Private Payment
Envelope, if one is available. Older queued envelopes of the same kind are
superseded.

#### Issue and Receive Encrypted Receipts

`issue_receipt` stores an encrypted Receipt and sends Receipt Access to the
counterparty. `get_receipt_access` returns every currently available Receipt
Access message in send order. `decrypt_receipt` decrypts a Receipt fetched by
the app from its Receipt Location.

Private Application Messages share one ordered encrypted stream. Supported
message kinds that are received for a different typed helper are buffered in
memory, but that buffer is not crash-durable unless the application persists and
reconciles its own state alongside Encrypted Link snapshots. Unsupported
syntactically valid message kinds are logged and dropped rather than buffered
indefinitely.

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
- `serialize_encrypted_link(...)` / `restore_encrypted_link(...)`: snapshot and
  restore an established Encrypted Link.
- `serialize_encrypted_link_handshake(...)` /
  `restore_encrypted_link_handshake(...)`: snapshot and restore an in-progress
  handshake.

### Private Payment Envelopes

- `set_private_payment_envelope(link, envelope)`: send a complete Private
  Payment Envelope over the Encrypted Link.
- `get_private_payment_envelope(link)`: receive the newest queued Private
  Payment Envelope, if one is available.

### Receipts

- `issue_receipt(session, link, draft)`: store an encrypted Receipt and send
  Receipt Access over the Encrypted Link.
- `get_receipt_access(link)`: receive all currently available Receipt Access
  messages in send order.
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
- First draft implementation of paykit daemon:
  <https://github.com/pubky/paykit>
- Product overview:
  <https://docs.google.com/document/d/1Z1HHdxpkOtelOXJRgPldso4_-lchzs3NL_JqDxCdiu8/edit?pli=1&tab=t.0>
