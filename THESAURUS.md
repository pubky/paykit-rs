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

### Private Application Message
- **Definition**: A versioned JSON message sent over an Encrypted Link and identified by a Private Message Kind.
- **NOT**: Public Paykit data published through Pubky public storage or the Encrypted Link itself.
- **Synonyms to AVOID**: private payload when naming the protocol concept
- **Related terms**: Encrypted Link, Private Message Kind, Private Payment Envelope, Receipt Access

### Private Message Kind
- **Definition**: The kind discriminator for private Paykit messages, e.g. `paykit.private_payment_envelope` or `paykit.receipt_access`.
- **NOT**: The Private Application Message body or the Rust enum/type that may represent it in an implementation.
- **Synonyms to AVOID**: private message type when naming the protocol concept
- **Related terms**: Private Application Message, Private Payment Envelope, Receipt Access

### Latest-State Message
- **Definition**: A private message semantic where only the newest valid message of that kind matters.
- **NOT**: An Event Message where every valid message must be preserved and processed in send order.
- **Synonyms to AVOID**: latest-wins message when naming the protocol concept
- **Related terms**: Private Application Message, Private Message Kind, Event Message, Private Payment Envelope

### Event Message
- **Definition**: A private Paykit message where every valid message matters and receivers must process messages in send order.
- **NOT**: A Latest-State Message where newer messages supersede older messages of the same kind.
- **Synonyms to AVOID**: event-like message when naming the protocol concept
- **Related terms**: Private Application Message, Private Message Kind, Latest-State Message, Receipt Access

### Private Payment Envelope
- **Definition**: a versioned encrypted Paykit message carrying a specific Payment Reference and a complete Payment List of a Linked Peer. Latest-State Message semantics apply; a newer Private Payment Envelope supersedes older envelopes, even when they have different Payment References.
- **NOT**: A Payment List with addition fields.
- **Synonyms to AVOID**: Private Payment List, Private Payment Method List, private payments payload
- **Related terms**: Linked Peer, Private Application Message, Latest-State Message, Payment Endpoint, Payment Reference

### Payment Reference
- **Definition**: A per-payment or per-request correlation identifier. In private payment flows, the Private Payment Envelope carries the Payment Reference because the envelope represents the latest private payment disclosure for that payment/request.
- **NOT**: A stable relationship identifier for a known peer.
- **Synonyms to AVOID**: peer reference, relationship reference
- **Related terms**: Private Payment Envelope, Receipt

### Receipt
- **Definition**: A Paykit receipt for a payment; Paykit receipts are always encrypted.
- **NOT**: A plain or unencrypted receipt.
- **Synonyms to AVOID**: plain receipt, unencrypted receipt
- **Related terms**: Receipt Access, Receipt Decryption Key, Payment Reference

### Receipt Decryption Key
- **Definition**: The symmetric key needed to decrypt a Receipt; sensitive material carried in Receipt Access.
- **NOT**: A Receipt, Receipt Access, or a general-purpose peer/link key.
- **Synonyms to AVOID**: receipt key when the decryption purpose matters
- **Related terms**: Receipt, Receipt Access

### Receipt Location
- **Definition**: The canonical homeserver public key and path (pubky resource) where an encrypted Receipt payload is stored.
- **NOT**: The Receipt payload itself or the Receipt Decryption Key.
- **Synonyms to AVOID**: receipt path when naming the protocol concept
- **Related terms**: Receipt, Receipt Access

### Receipt Access
- **Definition**: The descriptor object that lets a counterparty retrieve and decrypt a Receipt. It is sent over the Encrypted Link and carries the Receipt Location and Receipt Decryption Key. Its retrieval and processing follows Event Message semantics, unlike private payments which are latest-state. 
- **NOT**: The Receipt payload itself, only the Receipt Location, or only the Receipt Decryption Key.
- **Synonyms to AVOID**: receipt token, receipt pointer when naming the protocol concept
- **Related terms**: Receipt, Receipt Location, Receipt Decryption Key, Payment Reference, Event Message

### Counterparty
- **Definition**: The other party in a payment interaction.
- **NOT**: Necessarily a Linked Peer; the counterparty may or may not have an established Encrypted Link.
- **Synonyms to AVOID**: peer when link state matters
- **Related terms**: Linked Peer, Payer, Payee

### Encrypted Link
- **Definition**: An established pubky-noise channel used to exchange private Paykit application messages.
- **NOT**: The counterparty itself or arbitrary public Pubky storage.
- **Synonyms to AVOID**: linked peer when referring to the channel itself
- **Related terms**: Linked Peer, Encrypted Link Handshake, Private Application Message, Private Payment Envelope, Receipt Access

### Encrypted Link Handshake
- **Definition**: The setup flow that establishes an Encrypted Link between two peers.
- **NOT**: The established Encrypted Link itself or subsequent private Paykit application messages sent over it.
- **Synonyms to AVOID**: link setup when naming the protocol concept
- **Related terms**: Encrypted Link, Linked Peer

### Linked Peer
- **Definition**: A counterparty with an established Encrypted Link.
- **NOT**: Any arbitrary payer/payee or public-key holder.
- **Synonyms to AVOID**: counterparty when the encrypted-link invariant matters
- **Related terms**: Counterparty, Encrypted Link, Private Payment Envelope

### Payer
- **Definition**: The party attempting to send value in a payment flow.
- **NOT**: Always a Linked Peer.
- **Synonyms to AVOID**: sender when it obscures payment role
- **Related terms**: Payee, Counterparty

### Payee
- **Definition**: The party receiving value in a payment flow and publishing or sharing Payment Endpoints.
- **NOT**: Always a Linked Peer.
- **Synonyms to AVOID**: receiver when it obscures payment role
- **Related terms**: Payer, Payment List, Payment Endpoint

## Forbidden Lexicon

These terms must not be used for new Paykit domain/protocol/component names:

- **Payment Method List** → use **Payment List**.
- **Payment Option** → use **Payment Endpoint** or **Payment Method**, depending on meaning.
- **Routing Network** / **Paykit Routing Network** → use **Pubky Routing** in concrete Paykit docs.
- **MethodId** → use **Payment Endpoint Identifier**.
- **EndpointData** → use **Payment Endpoint Payload**.
- **SupportedPayments** → use **Payment List** for the payee-published list.
- **PrivatePaymentsPayload** → use **Private Payment Envelope**.
- **Private Payment List** for the whole private message → use **Private Payment Envelope**. Use **Payment List** when referring only to the endpoint collection carried inside the envelope.
- **Private Payment Method List** → use **Payment List** or **Private Payment Envelope**, depending on whether you mean the endpoint collection or the whole private message.
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
- Private Application Message
- Private Message Kind
- Latest-State Message
- Event Message
- Private Payment Envelope
- Payment Reference
- Receipt
- Receipt Decryption Key
- Receipt Location
- Receipt Access
- Encrypted Link
- Encrypted Link Handshake

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
