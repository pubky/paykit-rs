# Paykit Thesaurus

> Canonical ubiquitous language for Paykit. Use this file before naming protocol concepts, public APIs, docs sections, files, types, fields, endpoints, events, or components.
>
> Domain rule: the terms below win over older README/code wording when creating new names.

## Bounded Contexts

### Paykit
- **Definition**: The whole Paykit system/product, including Paykit Protocol, Paykit Library, Paykit SDK/runtime work, private payments, receipts, Payment Requests, and related components.
- **NOT**: Only the protocol layer or only the Rust library.
- **Synonyms to AVOID**: Paykit SDK when referring to the whole product or current stateless library
- **Related terms**: Paykit Protocol, Paykit Library, Paykit SDK, Payment Request

### Paykit Protocol
- **Definition**: The domain rules, data model, and flows for payment discovery and exchange through Pubky Routing, including Payment Lists, Private Payment Lists, Payment Endpoint Identifiers, Payment References, Payment Requests, Payment Proofs, Receipts, Receipt IDs, and Receipt Access.
- **NOT**: A specific Rust implementation or runtime.
- **Synonyms to AVOID**: routing network protocol, Paykit SDK protocol
- **Related terms**: Payment List, Private Payment List, Payment Endpoint Identifier, Payment Request, Pubky Routing

### Paykit Library
- **Definition**: The canonical product/component name for the Rust library that implements and exposes Paykit Protocol functionality to applications.
- **NOT**: An SDK, runtime, or protocol specification.
- **Synonyms to AVOID**: Paykit SDK, Paykit PDK
- **Related terms**: Paykit Protocol, Paykit SDK, Language Bindings

### Paykit SDK
- **Definition**: The stateful integration layer above Paykit Library for durable Encrypted Link state, private stream routing, event logs, recovery behavior, Payment Request lifecycle state, receipt indexing, and ergonomic wallet/payment-processor workflows.
- **NOT**: Paykit Library, Paykit Protocol, or payment execution/settlement logic.
- **Synonyms to AVOID**: Paykit core, Paykit runtime core, Pubky SDK
- **Related terms**: Paykit, Paykit Library, Language Bindings

### Language Bindings
- **Definition**: Distribution/integration surfaces under Paykit Library for languages or platforms such as Swift, Kotlin, and React Native.
- **NOT**: First-class Paykit architecture components.
- **Synonyms to AVOID**: Paykit SDK
- **Related terms**: Paykit Library

## SDK Terms

### Paykit Profile
- **Definition**: Public Paykit-facing display metadata published by a Pubky identity, such as display name and image pointer, under the SDK default profile path.
- **NOT**: A product-specific profile page, app account record, or Payment Endpoint.
- **Synonyms to AVOID**: Payment Profile when referring to SDK display metadata
- **Related terms**: Paykit SDK, Pubky Routing, Contact Record

### Contact Record
- **Definition**: A local SDK record for a saved Pubky public key, optional local label, cached Paykit Profile, and contact-related SDK state.
- **NOT**: A public social graph requirement or a Payment List.
- **Synonyms to AVOID**: contact payment option
- **Related terms**: Paykit SDK, Paykit Profile, Public Contact Marker

### Public Contact Marker
- **Definition**: An optional public Pubky marker published by explicit SDK policy to indicate a saved contact in the shared Paykit namespace.
- **NOT**: The default Contact Record storage model or proof of an active Encrypted Link.
- **Synonyms to AVOID**: public contact record when referring to the marker only
- **Related terms**: Contact Record, Paykit Profile, Paykit SDK

## Core Protocol Terms

### Pubky Routing
- **Definition**: Paykit's concrete routing/discovery/storage substrate in practice: Pubky public-key addressing, Pkarr discovery, and homeserver storage.
- **NOT**: A generic network abstraction in concrete Paykit docs.
- **Synonyms to AVOID**: Routing Network, Paykit Routing Network
- **Related terms**: Paykit Protocol, Payment List

### Payment List
- **Definition**: A payee-published or payee-shared collection of Payment Endpoints. A Payment List may be publicly discoverable through Pubky public storage or privately shared inside a Private Payment List.
- **NOT**: The result of payer-side processing of compatible endpoints.
- **Synonyms to AVOID**: Payment Method List
- **Related terms**: Payment Endpoint, Payee

### Payment Endpoint
- **Definition**: A whole payee-owned item in a Payment List, consisting of a Payment Endpoint Identifier and a Payment Endpoint Payload.
- **NOT**: Only the address, invoice, IBAN, offer, or credential payload.
- **Synonyms to AVOID**: Payment Option, endpoint payload when referring to the whole Payment Endpoint
- **Related terms**: Payment Endpoint Identifier, Payment Endpoint Payload, Payment List

### Payment Endpoint Identifier
- **Definition**: The canonical machine-readable identifier for a payment endpoint type, such as `btc-lightning-bolt12` or `eur-sepa-iban`.
- **NOT**: The full Payment Endpoint, the payload/credential itself, or reserved Paykit storage path segments such as `private` or `encrypted-link-recovery`.
- **Synonyms to AVOID**: method id, payment method id
- **Related terms**: Payment Endpoint, Payment Method, Asset, Rail, Endpoint Format

### Payment Endpoint Payload
- **Definition**: The opaque UTF-8 data part of a Payment Endpoint containing the actual receiving handle/details, such as an address, invoice, offer, IBAN, tag, JSON descriptor, or related fields. Paykit stores and transports it without interpreting its internal structure.
- **NOT**: The whole Payment Endpoint.
- **Synonyms to AVOID**: Payment Endpoint when referring only to payload data, endpoint data in domain docs
- **Related terms**: Payment Endpoint, Payment Endpoint Identifier

### Payment Method
- **Definition**: A higher-level human/domain concept for how value can be transferred, mapping to one or more Payment Endpoint Identifiers.
- **NOT**: A single machine identifier in all cases, and not a payee-specific endpoint payload.
- **Synonyms to AVOID**: Payment Option
- **Related terms**: Payment Endpoint Identifier

### Asset
- **Definition**: The unit of value being transferred, such as `btc`, `eur`, `usd`, or `usdt`; in identifiers this appears as the first segment.
- **NOT**: The settlement rail or endpoint format.
- **Synonyms to AVOID**: currency when the value is not specifically fiat
- **Related terms**: Payment Endpoint Identifier, Rail, Endpoint Format

### Payment Amount
- **Definition**: A concrete amount of value in a payment flow, represented as decimal `value` text plus an `asset` string. Paykit does not define a global asset registry; implementations should use consistent asset spelling when matching Payment Amounts to Payment Endpoint Identifiers.
- **NOT**: A Payment Reference, Payment Endpoint Payload, or payment execution state.
- **Synonyms to AVOID**: currency when naming the asset field, amount string when both value and asset are meant
- **Related terms**: Asset, Payment Request, Receipt, Payment Proof

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
- **Definition**: A versioned JSON message sent over an Encrypted Link and identified by a Private Message Kind. This is the base private message shape: `version` plus `kind`.
- **NOT**: Public Paykit data published through Pubky public storage or the Encrypted Link itself.
- **Synonyms to AVOID**: private payload when naming the protocol concept
- **Related terms**: Encrypted Link, Private Message Kind, Private Payment List, Receipt Access

### Private Message Kind
- **Definition**: The kind discriminator for private Paykit messages, e.g. `paykit.private_payment_list`, `paykit.receipt_access`, or `paykit.payment_request`.
- **NOT**: The Private Application Message body or the Rust enum/type that may represent it in an implementation.
- **Synonyms to AVOID**: private message type when naming the protocol concept
- **Related terms**: Private Application Message, Private Payment List, Receipt Access

### Latest-State Message
- **Definition**: A private message semantic where only the newest valid message of that kind matters. Malformed newer messages do not supersede the latest valid state.
- **NOT**: An Event Message where every valid message must be preserved and processed in send order.
- **Synonyms to AVOID**: latest-wins message when naming the protocol concept
- **Related terms**: Private Application Message, Private Message Kind, Event Message, Private Payment List

### Event Message
- **Definition**: One FIFO private Paykit message where every valid message matters and receivers must process messages in send order, such as Receipt Access or Payment Request lifecycle messages like request, acceptance, rejection, cancellation, and proof. An Event Message uses the Private Application Message base shape and adds an Event ID.
- **NOT**: A Latest-State Message where newer messages supersede older messages of the same kind.
- **Synonyms to AVOID**: event-like message when naming the protocol concept
- **Related terms**: Private Application Message, Private Message Kind, Latest-State Message, Receipt Access, Event ID

### Event ID
- **Definition**: A stable UUID-v4 identifier carried by an Event Message, used for idempotent storage, replay dedupe, local indexing, and recovery.
- **NOT**: A Payment Request ID, Payment Reference, relationship identifier, or a hash of the Event Message payload.
- **Synonyms to AVOID**: event reference, message reference when naming the protocol identifier
- **Related terms**: Event Message, Payment Request, Payment Request ID

### Encrypted Link Recovery Marker
- **Definition**: A minimal public Pubky marker that one peer publishes to signal that a counterparty should relink an Encrypted Link. Marker paths are pairwise-derived; marker payloads carry only version, kind, recovery attempt ID, and creation time.
- **NOT**: A Private Application Message, payment message, recovery transcript, or proof that the counterparty received the marker.
- **Synonyms to AVOID**: private recovery marker, recovery message when naming the public marker concept
- **Related terms**: Encrypted Link, Linked Peer, Paykit SDK

### Private Payment List
- **Definition**: A versioned encrypted Paykit message carrying a complete Payment List shared with a Linked Peer. Latest-State Message semantics apply; a newer Private Payment List supersedes older queued Private Payment List messages.
- **NOT**: A Payment Request, Payment Proof, or the source of a Payment Request's Payment Reference.
- **Synonyms to AVOID**: Private Payment Method List, Private Payment Envelope, private payments payload
- **Related terms**: Linked Peer, Private Application Message, Latest-State Message, Payment List, Payment Endpoint

### Payment Reference
- **Definition**: An opaque payee-provided text value visible to the payee, used to connect an incoming payment, Payment Proof, or Receipt to external state such as an invoice, order, account, or note. In Payment Request flows, the payee sets the Payment Reference in the request and the payer copies it into Payment Proof messages.
- **NOT**: A stable relationship identifier, Payment Request ID, Event ID, Receipt ID, billing period identifier, endpoint-publication identifier, or necessarily a UUID.
- **Synonyms to AVOID**: peer reference, relationship reference, request id when referring to a Payment Request, memo when naming the cross-rail Paykit concept
- **Related terms**: Payment Request, Payment Request ID, Payment Proof, Receipt, Receipt ID

### Receipt
- **Definition**: The plaintext Paykit receipt object for a payment, created locally by the issuer and decrypted locally by the receiver. It may carry a Payment Amount, optional Payment Request ID and Billing Period correlation, and Receipt Metadata.
- **NOT**: The encrypted stored artifact on a homeserver, Receipt Access, Receipt ID, or Payment Reference.
- **Synonyms to AVOID**: receipt container when the plaintext Receipt is meant
- **Related terms**: Encrypted Receipt, Receipt ID, Receipt Access, Receipt Decryption Key, Payment Reference, Payment Amount

### Encrypted Receipt
- **Definition**: The encrypted stored artifact on the issuer's homeserver that contains the plaintext Receipt ciphertext. It carries encryption metadata such as algorithm, nonce, and ciphertext.
- **NOT**: The plaintext Receipt after local decryption, Receipt Access, or a Private Application Message.
- **Synonyms to AVOID**: Receipt when the stored encrypted artifact is specifically meant
- **Related terms**: Receipt, Receipt ID, Receipt Location, Receipt Access, Receipt Decryption Key

### Receipt ID
- **Definition**: A UUID-v4 identifier for one Encrypted Receipt artifact. It is used to derive the Receipt Location.
- **NOT**: A Payment Reference, Payment Request ID, Event ID, or human invoice/order reference.
- **Synonyms to AVOID**: receipt reference when naming the storage identifier
- **Related terms**: Receipt, Receipt Location, Receipt Access, Payment Reference

### Receipt Decryption Key
- **Definition**: The symmetric key needed to decrypt a Receipt; sensitive material carried in Receipt Access.
- **NOT**: A Receipt, Receipt Access, or a general-purpose peer/link key.
- **Synonyms to AVOID**: receipt key when the decryption purpose matters
- **Related terms**: Receipt, Receipt Access

### Receipt Location
- **Definition**: The canonical path on the issuer's homeserver where an Encrypted Receipt is stored. It is interpreted together with the Receipt Access sender/issuer context.
- **NOT**: The Receipt payload itself, Payment Reference, Receipt Decryption Key, or a complete Pubky resource without issuer identity.
- **Synonyms to AVOID**: receipt resource when the issuer identity is not included
- **Related terms**: Receipt, Receipt ID, Receipt Access

### Receipt Access
- **Definition**: The Event Message descriptor that lets a counterparty retrieve an Encrypted Receipt and decrypt it locally into a Receipt.
- **NOT**: The Receipt payload itself, the Encrypted Receipt, only the Receipt Location, or only the Receipt Decryption Key.
- **Synonyms to AVOID**: receipt token, receipt pointer when naming the protocol concept
- **Related terms**: Receipt, Receipt ID, Receipt Location, Receipt Decryption Key, Payment Reference, Event Message

### Counterparty
- **Definition**: The other party in a payment interaction.
- **NOT**: Necessarily a Linked Peer; the counterparty may or may not have an established Encrypted Link.
- **Synonyms to AVOID**: peer when link state matters
- **Related terms**: Linked Peer, Payer, Payee

### Encrypted Link
- **Definition**: An established pubky-noise channel used to exchange private Paykit application messages.
- **NOT**: The counterparty itself or arbitrary public Pubky storage.
- **Synonyms to AVOID**: linked peer when referring to the channel itself
- **Related terms**: Linked Peer, Encrypted Link Handshake, Private Application Message, Private Payment List, Receipt Access

### Encrypted Link Handshake
- **Definition**: The setup flow that establishes an Encrypted Link between two peers.
- **NOT**: The established Encrypted Link itself or subsequent private Paykit application messages sent over it.
- **Synonyms to AVOID**: link setup when naming the protocol concept
- **Related terms**: Encrypted Link, Linked Peer

### Linked Peer
- **Definition**: A counterparty with an established Encrypted Link.
- **NOT**: Any arbitrary payer/payee or public-key holder.
- **Synonyms to AVOID**: counterparty when the encrypted-link invariant matters
- **Related terms**: Counterparty, Encrypted Link, Private Payment List

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

## Payment Requests

### Payment Request
- **Definition**: A private Paykit protocol object where a payee asks a payer for payment. A Payment Request may be one-time or recurring.
- **NOT**: A Payment Endpoint, Payment List, payment execution, public invoice URL, or payer-initiated standing order.
- **Synonyms to AVOID**: SubscriptionAgreement, subscription proposal when naming the base protocol object
- **Related terms**: Recurring Payment Request, Payment Request ID, Payment Reference, Payment Amount, Payment Proof, Linked Peer

### Recurring Payment Request
- **Definition**: A payee-initiated Payment Request with non-null Recurrence that can lead to repeated payer-controlled payments after acceptance.
- **NOT**: A separate protocol family from Payment Request, or a payee-controlled pull authorization.
- **Synonyms to AVOID**: subscription when naming the protocol object, pull subscription
- **Related terms**: Payment Request, Subscription, Recurrence, Billing Period

### Subscription
- **Definition**: Product shorthand for an accepted Recurring Payment Request.
- **NOT**: The base protocol family or a separate message namespace.
- **Synonyms to AVOID**: subscription agreement, subscription protocol when Payment Request is the intended protocol concept
- **Related terms**: Recurring Payment Request, Payment Request

### Payment Request ID
- **Definition**: A stable UUID-v4 identifier for the lifetime of one Payment Request. All lifecycle Event Messages for the same request share the same Payment Request ID.
- **NOT**: A Payment Reference, Event ID, Billing Period, or peer relationship identifier.
- **Synonyms to AVOID**: subscription_id, agreement id, request reference
- **Related terms**: Payment Request, Event ID, Payment Reference

### Recurrence
- **Definition**: The schedule object on a Payment Request that describes intended repeated payment eligibility using interval count, unit, starts_at, anchor, and optional ends_at.
- **NOT**: A cron expression, local scheduler implementation, payment retry policy, or canonical calendar expansion algorithm.
- **Synonyms to AVOID**: schedule when naming the protocol object
- **Related terms**: Recurring Payment Request, Billing Period, Proposal Expiry

### Billing Period
- **Definition**: The concrete time interval a recurring payment execution or Payment Proof claims to apply to.
- **NOT**: A Payment Request ID, Payment Reference, Recurrence rule, protocol message kind, or proof by itself that the interval is eligible under local recurrence policy.
- **Synonyms to AVOID**: billing cycle when naming the protocol field
- **Related terms**: Recurring Payment Request, Recurrence, Payment Proof, Payment Reference

### Proposal Expiry
- **Definition**: The `proposal_expires_at` value on a Payment Request proposal that defines when the proposal stops being actionable before acceptance. A null value means no protocol-level proposal expiry.
- **NOT**: The recurrence end date, billing period end, receipt expiry, or an expiry for an already accepted Payment Request.
- **Synonyms to AVOID**: request expiry when it is ambiguous with recurrence end
- **Related terms**: Payment Request, Recurrence

### Payment Proof
- **Definition**: Method-specific evidence for one concrete payment execution, correlated by Payment Request ID, Payment Reference, Payment Endpoint Identifier, and Billing Period when recurring.
- **NOT**: A Paykit Receipt, Receipt Access, or proof that Paykit itself validates generically.
- **Synonyms to AVOID**: PaymentReceipt, payment receipt when referring to method-specific proof
- **Related terms**: Payment Request, Payment Reference, Payment Endpoint Identifier, Billing Period, Receipt, Receipt Access

### Proof Submitted
- **Definition**: A derived lifecycle state meaning a valid `paykit.payment_proof` Event Message was received for a one-time Payment Request.
- **NOT**: Settlement confirmation, method-specific proof validation, or proof that Paykit itself knows the payment completed.
- **Synonyms to AVOID**: completed when only Paykit-level proof receipt is known
- **Related terms**: Payment Proof, Payment Request

## Forbidden Lexicon

These terms must not be used for new Paykit domain/protocol/component names:

- **Payment Method List** → use **Payment List**.
- **Payment Option** → use **Payment Endpoint** or **Payment Method**, depending on meaning.
- **Routing Network** / **Paykit Routing Network** → use **Pubky Routing** in concrete Paykit docs.
- **MethodId** → use **Payment Endpoint Identifier**.
- **EndpointData** → use **Payment Endpoint Payload**.
- **SupportedPayments** → use **Payment List** for the payee-published list.
- **PrivatePaymentsPayload** → use **Private Payment List**.
- **Private Payment Envelope** → use **Private Payment List**.
- **Payment List** when referring to the encrypted private message → use **Private Payment List**. Use **Payment List** only for the endpoint collection itself.
- **Private Payment Method List** → use **Payment List** or **Private Payment List**, depending on whether you mean the endpoint collection or the whole private message.
- **Payment Endpoint** for only address/invoice/IBAN/etc. data → use **Payment Endpoint Payload**.
- **SubscriptionAgreement** → use **Payment Request** or **Recurring Payment Request**, depending on whether recurrence is present.
- **subscription_id** → use **Payment Request ID** for the long-lived request identifier.
- **push subscription** → avoid for Paykit protocol concepts. Payer-initiated recurring payments are wallet/runtime scheduling outside Payment Request v0.2.
- **pull subscription** → use **Recurring Payment Request** for payee-initiated recurring requests. Future payee-pull authorization should be named separately.
- **payment_receipt** / **PaymentReceipt** for method-specific proof → use **Payment Proof**.
- **accepted_methods** → use **accepted Payment Endpoint Identifiers** or the concrete field `accepted_payment_endpoint_identifiers`.
- **payment_attempt** as a protocol message → use local payment execution state; do not model it as a Paykit Event Message in Payment Request v0.2.
- **payment_request_update** / **paykit.payment_request_update** → cancel the old Payment Request and create a separate new Payment Request in v0.2.

## Component Model

Current/core:
- Paykit
- Paykit Protocol
- Paykit Library
- Paykit SDK
- Pubky Routing

Protocol concepts:
- Payment List
- Payment Endpoint
- Payment Endpoint Identifier
- Payment Endpoint Payload
- Payment Method
- Payment Amount
- Private Application Message
- Private Message Kind
- Latest-State Message
- Event Message
- Event ID
- Private Payment List
- Payment Reference
- Payment Request
- Recurring Payment Request
- Payment Request ID
- Recurrence
- Billing Period
- Proposal Expiry
- Payment Proof
- Proof Submitted
- Receipt
- Encrypted Receipt
- Receipt ID
- Receipt Decryption Key
- Receipt Location
- Receipt Access
- Encrypted Link
- Encrypted Link Handshake

Future/planned:
- Paykit SDK platform bindings

Implementation/legacy details:
- Paykit FFI
- Paykit React Native
- Paykit PDK
- MethodId
- EndpointData
- SupportedPayments
- PrivatePaymentsPayload
