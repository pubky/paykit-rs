# Paykit Thesaurus

> Canonical ubiquitous language for Paykit. Use this file before naming protocol concepts, public APIs, docs sections, files, types, fields, endpoints, events, or components.
>
> Domain rule: the terms below win over older README/code wording when creating new names.

## Bounded Contexts

### Paykit
- **Definition**: The whole Paykit system/product, including Paykit Protocol, Paykit Library, future Paykit Daemon work, private payments, receipts, and related components.
- **NOT**: Only the protocol layer or only the Rust library.
- **Synonyms to AVOID**: Paykit SDK
- **Related terms**: Paykit Protocol, Paykit Library, Paykit Daemon

### Paykit Protocol
- **Definition**: The domain rules, data model, and flows for payment discovery and exchange through Pubky Routing, including Payment Lists, Private Payment Envelopes, Payment Endpoint Identifiers, Payment References, Receipts, and Receipt Access.
- **NOT**: A specific Rust implementation or daemon runtime.
- **Synonyms to AVOID**: routing network protocol, Paykit SDK protocol
- **Related terms**: Payment List, Private Payment Envelope, Payment Endpoint Identifier, Pubky Routing

### Paykit Library
- **Definition**: The canonical product/component name for the Rust library that implements and exposes Paykit Protocol functionality to applications.
- **NOT**: A daemon, SDK, or protocol specification.
- **Synonyms to AVOID**: Paykit SDK, Paykit PDK
- **Related terms**: Paykit Protocol, Paykit Daemon, Language Bindings

### Paykit Daemon
- **Definition**: A planned/future stateful Paykit service for payment execution, accounting, subscriptions, events, prioritization, and payment-provider orchestration.
- **NOT**: A current core component of Paykit architecture.
- **Synonyms to AVOID**: Paykit core, Paykit runtime core
- **Related terms**: Paykit, Paykit Library

### Paykit SDK
- **Definition**: A planned/future extension of Paykit Library with Pubky related functionality for authorization/authentication, session management, profile retreival and tag assignment.
- **NOT**: A current core component of Paykit architecture.
- **Synonyms to AVOID**: Paykit core, Paykit runtime core, Pubky 
- **Related terms**: Paykit, Paykit Library, Pubky SDK

### Language Bindings
- **Definition**: Distribution/integration surfaces under Paykit Library for languages or platforms such as Swift, Kotlin, and React Native.
- **NOT**: First-class Paykit architecture components.
- **Synonyms to AVOID**: Paykit SDK
- **Related terms**: Paykit Library

## Core Protocol Terms

### Pubky Routing
- **Definition**: Paykit's concrete routing/discovery/storage substrate in practice: Pubky public-key addressing, Pkarr discovery, and homeserver storage.
- **NOT**: A generic network abstraction in concrete Paykit docs.
- **Synonyms to AVOID**: Routing Network, Paykit Routing Network
- **Related terms**: Paykit Protocol, Payment List

### Payment List
- **Definition**: A payee-published or payee-shared collection of Payment Endpoints. A Payment List may be publicly discoverable through Pubky public storage or privately shared inside a Private Payment Envelope.
- **NOT**: The result of payer-side processing of compatible endpoints.
- **Synonyms to AVOID**: Payment Method List
- **Related terms**: Payment Endpoint, Payee

### Payment Endpoint
- **Definition**: A whole payee-owned entry in a Payment List, consisting of a Payment Endpoint Identifier and a Payment Endpoint Payload.
- **NOT**: Only the address, invoice, IBAN, offer, or credential payload.
- **Synonyms to AVOID**: Payment Option, endpoint payload when referring to the whole entry
- **Related terms**: Payment Endpoint Identifier, Payment Endpoint Payload, Payment List

### Payment Endpoint Identifier
- **Definition**: The canonical machine-readable identifier for a payment endpoint type, such as `btc-lightning-bolt12` or `eur-sepa-iban`.
- **NOT**: The full Payment Endpoint or the payload/credential itself.
- **Synonyms to AVOID**: method id, payment method id
- **Related terms**: Payment Endpoint, Payment Method, Asset, Rail, Endpoint Format

### Payment Endpoint Payload
- **Definition**: The data part of a Payment Endpoint containing the actual receiving handle/details, such as an address, invoice, offer, IBAN, tag, or related fields.
- **NOT**: The whole Payment Endpoint entry.
- **Synonyms to AVOID**: Payment Endpoint when referring only to payload data, endpoint data in domain docs
- **Related terms**: Payment Endpoint, Payment Endpoint Identifier

### Payment Method
- **Definition**: A higher-level human/domain concept for how value can be transferred, mapping to one or more Payment Endpoint Identifiers.
- **NOT**: A single machine identifier in all cases, and not a payee-specific endpoint payload.
- **Synonyms to AVOID**: Payment Option
- **Related terms**: Payment Endpoint Identifier,

### Asset
- **Definition**: The unit of value being transferred, such as BTC, EUR, USD, or USDT; in identifiers this appears as the first segment.
- **NOT**: The settlement rail or endpoint format.
- **Synonyms to AVOID**: currency when the value is not specifically fiat
- **Related terms**: Payment Endpoint Identifier, Rail, Endpoint Format

### Rail
- **Definition**: The settlement system carrying the asset, such as Lightning, Bitcoin, SEPA, ACH, or Revolut; in identifiers this appears as the second segment.
- **NOT**: The asset itself or the payee-specific receiving payload.
- **Synonyms to AVOID**: network when it creates ambiguity with Pubky Routing
- **Related terms**: Asset, Endpoint Format, Payment Endpoint Identifier

### Endpoint Format
- **Definition**: The handle or credential format used on a rail, such as BOLT11, BOLT12, P2TR, IBAN, address, or tag; in identifiers this appears as the third segment.
- **NOT**: The actual payee-owned payload value.
- **Synonyms to AVOID**: endpoint when it is ambiguous with Payment Endpoint
- **Related terms**: Payment Endpoint Identifier, Payment Endpoint Payload

## Private Payments and Receipts

### Private Payment Envelope
- **Definition**: a versioned encrypted Paykit message carrying a specific Payment Reference and a complete Payment List of a Know Peer. Latest-wins semantics apply; a newer Private Payment Envelope supersedes older envelopes, even when they have different Payment References.
- **NOT**: A public Payment List, and not merely a private version of the list if the object includes more than endpoints.
- **Synonyms to AVOID**: Private Payment List, Private Payment Method List, private payments payload
- **Related terms**: Known Peer, Payment Endpoint, Payment Reference

### Payment Reference
- **Definition**: A per-payment or per-request correlation identifier. In private payment flows, the Private Payment Envelope carries the Payment Reference because the envelope represents the latest private payment disclosure for that payment/request.
- **NOT**: A stable relationship identifier for a known peer.
- **Synonyms to AVOID**: peer reference, relationship reference
- **Related terms**: Private Payment Envelope, Receipt

### Receipt
- **Definition**: A Paykit receipt for a payment; Paykit receipts are always encrypted.
- **NOT**: A plain or unencrypted receipt.
- **Synonyms to AVOID**: plain receipt, unencrypted receipt
- **Related terms**: Receipt Access, Payment Reference

### Receipt Access
- **Definition**: The descriptor/capability that lets a counterparty retrieve and decrypt a Receipt.
- **NOT**: The Receipt payload itself.
- **Synonyms to AVOID**: receipt token, receipt pointer when naming the protocol concept
- **Related terms**: Receipt, Payment Reference

### Counterparty
- **Definition**: The other party in a payment interaction.
- **NOT**: Necessarily a Known Peer; the counterparty may or may not have an established encrypted link.
- **Synonyms to AVOID**: peer when link state matters
- **Related terms**: Known Peer, Payer, Payee

### Known Peer
- **Definition**: A counterparty with an established encrypted link.
- **NOT**: Any arbitrary payer/payee or public-key holder.
- **Synonyms to AVOID**: counterparty when the encrypted-link invariant matters
- **Related terms**: Counterparty, Private Payment Envelope

### Payer
- **Definition**: The party attempting to send value in a payment flow.
- **NOT**: Always a Known Peer.
- **Synonyms to AVOID**: sender when it obscures payment role
- **Related terms**: Payee, Counterparty

### Payee
- **Definition**: The party receiving value in a payment flow and publishing or sharing Payment Endpoints.
- **NOT**: Always a Known Peer.
- **Synonyms to AVOID**: receiver when it obscures payment role
- **Related terms**: Payer, Payment List, Payment Endpoint

## Forbidden Lexicon

These terms must not be used for new Paykit domain/protocol/component names:

- **Payment Method List** → use **Payment List**.
- **Payment List** for the payee-published list.
- **Payment Option** → use **Payment Endpoint** or **Payment Method**, depending on meaning.
- **Routing Network** / **Paykit Routing Network** → use **Pubky Routing** in concrete Paykit docs.
- **MethodId** → use **Payment Endpoint Identifier**.
- **EndpointData** → use **Payment Endpoint Payload**.
- **SupportedPayments** → use **Payment List** for the payee-published list.
- **PrivatePaymentsPayload** → use **Private Payment Envelope**.
- **Private Payment Method List** / **Private Payment List** → use **Private Payment Envelope** when referring to known-peer private payment data.
- **Payment Endpoint** for only address/invoice/IBAN/etc. data → use **Payment Endpoint Payload**.

## Component Model

Current/core:
- Paykit
- Paykit Protocol
- Paykit Library
- Pubky Routing

Protocol concepts:
- Payment List
- Payment Endpoint
- Payment Endpoint Identifier
- Payment Endpoint Payload
- Payment Method
- Private Payment Envelope
- Payment Reference
- Receipt
- Receipt Access

Future/planned:
- Paykit Daemon

Implementation/legacy details:
- Paykit FFI
- Paykit React Native
- Paykit PDK
- MethodId
- EndpointData
- SupportedPayments
- PrivatePaymentsPayload
