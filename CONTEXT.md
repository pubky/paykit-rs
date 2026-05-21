# Paykit Context

Paykit is a payment-discovery and payment-coordination context. Its language describes how a payee advertises ways to receive value, how a payer derives compatible choices, and how known peers exchange private payment information and receipts.

## Language

### Product and protocol

**Paykit**:
The whole Paykit system/product, including protocol, library, planned future daemon work, private payments, receipts, and related components.
_Avoid_: Paykit SDK

**Paykit Protocol**:
The domain rules and data model for payment discovery and exchange through Pubky Routing, including public Payment Lists, Private Payment Envelopes, Payment Endpoint Identifiers, Payment References, Receipts, and Receipt Access.
_Avoid_: Paykit SDK protocol, generic routing-network protocol

**Paykit Library**:
The developer library that implements and exposes Paykit Protocol functionality to applications.
_Avoid_: Paykit SDK, Paykit PDK

**Paykit Daemon**:
A planned future stateful Paykit service for payment execution, accounting, subscriptions, events, prioritization, and payment-provider orchestration.
_Avoid_: Paykit core, Paykit runtime core

### Payment discovery

**Pubky Routing**:
Paykit's concrete routing, discovery, and storage substrate using Pubky public-key addressing, Pkarr discovery, and homeserver storage.
_Avoid_: Routing Network, Paykit Routing Network

**Payment List**:
The list of Payment Endpoints published or shared by a payee.
_Avoid_: Payment Method List, Supported Payments List, Supported Payment List when referring to the payee-published list

**Public Payment List**:
A Payment List published by a payee in a public Pubky location for unknown or unauthenticated payers to discover.
_Avoid_: Public Payment Method List, Public Supported Payments List

**Supported Payment List**:
The payer-side intersection between the payer's supported payment capabilities/preferences and the payee's Payment List.
_Avoid_: Supported Payments List, Payment Method List

**Payment Endpoint**:
A whole payee-owned entry in a Payment List, consisting of a Payment Endpoint Identifier and a Payment Endpoint Payload.
_Avoid_: Payment Option, Payment Endpoint when referring only to payload data

**Payment Endpoint Identifier**:
The canonical machine-readable identifier for a payment endpoint type, such as btc-lightning-bolt12 or eur-sepa-iban.
_Avoid_: MethodId, method id, payment method id

**Payment Endpoint Payload**:
The data part of a Payment Endpoint containing the actual receiving handle/details, such as an address, invoice, offer, IBAN, tag, or related fields.
_Avoid_: endpoint data in domain language, Payment Endpoint when referring only to the payload

**Payment Method**:
A higher-level human/domain concept for how value can be transferred; one Payment Method can map to one or more Payment Endpoint Identifiers.
_Avoid_: Payment Option

**Asset**:
The unit of value being transferred, such as BTC, EUR, USD, or USDT.
_Avoid_: currency when the value is not specifically fiat

**Rail**:
The settlement system carrying the Asset, such as Lightning, Bitcoin, SEPA, ACH, or Revolut.
_Avoid_: network when it creates ambiguity with Pubky Routing

**Endpoint Format**:
The handle or credential format used on a Rail, such as BOLT11, BOLT12, P2TR, IBAN, address, or tag.
_Avoid_: endpoint when it is ambiguous with Payment Endpoint

### Private payments and receipts

**Private Payment Envelope**:
The canonical known-peer private payment data object exchanged over an encrypted link; it can contain Payment Endpoints plus protocol fields such as Payment Reference and freshness/versioning data.
_Avoid_: Private Payment List, Private Payment Method List, private payments payload when naming the domain concept

**Payment Reference**:
A per-payment or per-request correlation identifier.
_Avoid_: peer reference, relationship reference

**Receipt**:
A Paykit receipt for a payment; Paykit Receipts are always encrypted.
_Avoid_: plain receipt, unencrypted receipt

**Encrypted Receipt**:
A clarifying synonym for Receipt used when emphasizing storage or security; it is not a separate subtype because all Paykit Receipts are encrypted.
_Avoid_: plain receipt, unencrypted receipt

**Receipt Access**:
The descriptor/capability that lets a counterparty retrieve and decrypt a Receipt.
_Avoid_: receipt token, receipt pointer when naming the protocol concept

**Counterparty**:
The other party in a payment interaction.
_Avoid_: peer when link state matters

**Known Peer**:
A counterparty with an established encrypted link.
_Avoid_: counterparty when the encrypted-link invariant matters

**Payer**:
The party attempting to send value in a payment flow.
_Avoid_: sender when it obscures payment role

**Payee**:
The party receiving value in a payment flow and publishing or sharing Payment Endpoints.
_Avoid_: receiver when it obscures payment role

## Flagged ambiguities

**MethodId**:
Legacy implementation name for Payment Endpoint Identifier. Existing code may still contain MethodId, but new domain-facing names should use Payment Endpoint Identifier.

**EndpointData**:
Current implementation wrapper for Payment Endpoint Payload. New domain-facing names should prefer Payment Endpoint Payload.

**SupportedPayments**:
Legacy implementation name that currently represents a payee's Payment List, not a Supported Payment List. This is misleading because Supported Payment List means the payer-side intersection.

**PrivatePaymentsPayload**:
Current implementation name for Private Payment Envelope. New domain-facing names should prefer Private Payment Envelope.

## Example dialogue

Domain expert: The payee publishes a Payment List through Pubky Routing.
Developer: So get_payment_list should return the payee's Payment List, not a Supported Payment List?
Domain expert: Correct. The payer derives a Supported Payment List only after intersecting that Payment List with what the payer can use.
Developer: Each Payment Endpoint in the list has a Payment Endpoint Identifier and a Payment Endpoint Payload?
Domain expert: Yes. The identifier names the endpoint type; the payload is the actual receiving data.
Developer: For a known peer, the payee sends a Private Payment Envelope instead of publishing a private list?
Domain expert: Yes. Private Payment Envelope is the protocol term because it contains more than just endpoints.
