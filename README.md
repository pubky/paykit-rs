⚠️ WIP - NOT FOR PRODUCTION ⚠️

# Paykit

# Description

Paykit is a method for abstracting and automating any payment settlement process behind a single static public key which refers to a location of a file containing all supported payment methods and related data and endpoints.

As a meta payment protocol, Paykit also serves as an ideal layer for handling metadata related to payments, proofs of payment, and related features like automated subscriptions.

# Paykit protocol

Peers and applications that support Paykit may share or retrieve necessary payment information by accessing a compatible **routing network**. This network facilitates the storage and retrieval of data associated with public keys. The intended solution is to utilize Pubky’s PKARR method with Mainline DHT for discovery and routing, and data storage in Pubky homeservers.

## Routing network

Paykit requires a network to lookup **Supported Payments List** in order to retrieve and share data. Therefore the minimum requirements to the network are:

* Ability to look up a node on a network based on its public key
* Data stored on the node under certain path is guaranteed to be available by URL
* Authenticity of location where data is stored is verifiable with owner’s public key
* Read access to locations can be public or restricted and granted with URL
* Write access to locations can be granted to non-owners and granted with URL
* Access levels can be changed without changing the path component of the URL.
* Optionally support sending private direct messages to users of the network?

### Pubky Core protocol

DHT’s are optimal routing mechanisms for key based methods like Paykit. Paykit currently utilizes the Mainline DHT via use of [**Pubky Core**](https://github.com/pubky/pubky-core) protocol but theoretically any network could be used that satisfies the requirements above.

## Supported Payments List, Payment Method and Payment Endpoint

The examples of following concepts are provided in the Appendix at the end of this document. For the reasons explained below they are for illustrative purposes only.

### Supported Payments List

Read request to **Paykit Routing Network** with public key returns a **Supported Payments List** stored at **default public path**. This is an array of objects with one key being “**method**” whose value is URL to **Payment Method** and another key is **“endpoint”** with value being **Payment Endpoint** URL.

### Payment Method

The term "**Payment Method**" refers to the general concept of the medium through which a payment can be executed.

### Payment Endpoint

**Payment Endpoint** corresponds to the specific payee owned credentials/reference on which they can receive corresponding payments.

### Paykit Method Implementation Proposals

To prevent miscommunication between payer and payee which will result in inability to execute payments. The terms used on both ends of the payment are to be decided by the developers community.
Given that both the payer and payee may have preferences regarding payment media based on a virtually infinite number of factors—such as market conditions, address types, and interbank settlement times—it is up to social consensus to determine the naming conventions for payment methods and the structure of payment endpoints. Therefore, the examples provided in this document are for illustrative purposes only.

An initial draft convention is available in [specs/payment-endpoint-identifier.md](specs/payment-endpoint-identifier.md). It is recommended but not obligatory, and is intended as the first such proposal rather than a final format.

## Payment Method Lists

Paykit can support virtually any payment method as long as payer and payee can mutually describe and identify it. Paykit users create the **Supported Payments Lists** \- minimum necessary data related to their supported payment methods and publish them as records on **Paykit routing** network.

### Public Payment Method Lists

Paykit allows you to receive payments from anyone who is aware of the payee's public key.

#### Flow

1. The payee creates **Supported Payments List, Methods and Endpoints**
2. The payee stores created data under public location on Paykit Routing Network associated with their key pair
3. The payee publicly shares public key

#### NOTE:

It is important to understand that this data could be logged and monitored by all peers that know of this pubkey, and thus some methods, like bitcoin addresses, could expose correlations and payment information of peers in a suboptimal or undesirable way.

### Private Payment Method Lists

Paykit can exchange personalized, dedicated payment methods with known peers over an end-to-end encrypted Pubky Noise link. Private payments are:

* Encrypted before they are written to homeserver storage
* Exchanged only with the counterparty that completed the encrypted-link handshake
* Stored as latest-state envelopes: the newest private-payment envelope supersedes older queued private-payment envelopes

#### Flow

1. The peers establish an encrypted link with `initiate_encrypted_link` / `accept_encrypted_link` and drive it with `advance_handshake`.
2. The payee creates a complete private-payments envelope containing a UUID-v4 `PaymentReference` and the currently supported private payment endpoints.
3. The payee sends the envelope with `set_private_payments`. Pubky Noise handles encryption, derived private storage paths, file slots, and counterparty retrieval.
4. The payer calls `get_private_payments` on the same encrypted link and receives the latest/newest queued envelope, if one is available.

## Payment Method Selection

Paykit attempts to match the supported payment methods of two peers by comparing payers supported payments against payees **Payment Method Lists** to find a match. If multiple matches are detected, paykit uses the payer’s **Payment Selection Logic** settings to prioritize the order of execution. If a user has not customized their **Payment Selection Logic**, paykit will use the **Default Payment Selection Logic**

### Default Payment Selection Logic

The “known peer” relationship means that there was previous out-of-band communication between payer and payee, during which public keys were exchanged. The payee can establish a private encrypted link with that peer and send a personalized private-payments envelope over the link. If no encrypted link or private update is available, payment should be treated as a public-list flow with the payee's threat model adjusted accordingly.

#### Payee is a known peer

1. The payer establishes or restores the encrypted link with the payee
2. The payer receives the payee’s latest private-payments envelope
3. The payer filters out supported payment methods
4. The payer selects the first payment method according to the payer’s own personal preferences
5. The payer attempts to execute a payment
6. In case of failure \- repeats from step 4 until the list from step 3 is empty.
7. In case all payments failed, the payer can notify the payee over an available secure communication channel

#### Payee is not a known peer

1. The payer resolves payee’s **Public Payment Method List** using their public key
2. The filters out supported payment methods
3. The payer selects the first payment method according to payers own personal preference
4. The payer retrieves data from the corresponding payment endpoint
5. The payer attempts to execute a payment
6. In case of failure \- repeats from step 3 until the list from step 2 is empty.

## Payment Method Interactivity

Both Private & Public **Payment Method Lists** can contain virtually any payment data, regardless of the interactivity requirements to either payer or payee on any level. In other words, paykit peers implement hooks for uni- and bi- directional communication.

### Interactive Payments

For example, peers may include URLs that direct the payer to an appropriate server or API in order to interact using other specific payment protocols that are mutually supported.

### Non-interactive Payments

For example, any static blockchain address, lightning network invoice, address or offer, email address, cashtag etc.

# Paykit library

A stateless toolkit featuring developer-friendly APIs and language bindings to engage with Paykit’s Payment Method Lists. This kit is intended to serve as a new dependency in the existing logic of applications and services responsible for processing of payments.

For release history and upgrade notes see [CHANGELOG.md](CHANGELOG.md).

## Usecases

The Paykit Library is intended for users who have already implemented payment receiving and execution functions including both push and pull subscription functionality implemented with these methods.

### Intended users

#### Light user wallets

These wallets already incorporate specific payment functionalities and aim to integrate Paykit features, such as enabling payment to contacts based on their public key.

#### Payment processors

These entities have already implemented various payment methods and seek to enhance the Paykit user experience by offering a single payment endpoint. This endpoint allows Paykit payees to execute payments without the need for manual selection of payment methods.

## Technical Requirements

The Paykit Library is an abstraction for payment specific CRUD methods of Paykit Protocol. It is to be used as a transport layer dependency for payment processing business logic. This will allow it to be used in implementations of payment logic of any complexity \- from single to recurring payments in implementations of any architecture from micro services to monolith.

### Implementation language

* **Rust**: Edition 2021, Version \+1.91.1

### Dependencies

* **The Pubky SDK** [https://github.com/pubky/pubky-core/tree/main/pubky](https://github.com/pubky/pubky-core/tree/main/pubky)
* **tracing** — structured, context-aware logging

### Language bindings

Mobile bindings are generated via [UniFFI](https://mozilla.github.io/uniffi-rs/) in the [`paykit-ffi/`](paykit-ffi/) crate. The FFI crate exposes public payment endpoints, encrypted-link/private-payment helpers, and encrypted receipt issuance/access/decryption APIs. See its [README](paykit-ffi/README.md) for build and integration instructions.

* **Swift:** Version \+5
* **Kotlin:** Version \+2.0

### Test coverage

* Documentation tests for all public methods

### Documentation

* Rust doc documentation for all public methods

#### Examples

* Send and receive test payment with mocked payment logic for two payment methods available for the receiving key.

## Functional Requirements

**Notes**: *The implementation of ID and its usage is subject to your specific application design and requirements*

### Public Payment Data

The APIs facilitate seamless interaction with public payment data using Paykit’s Routing Network which ensures efficient communication between payees and payers.

#### Retrieve public Supported Payments List for a given payee's public key

Allow users to fetch the list of payment methods that are publicly available for a specific payee, identified by their public key.

#### Retrieve Payment Endpoint for a payee's public key and payment method

Enable users to access detailed payment information associated with a particular payment method for a given payee's public key.

#### Store Payment Endpoint for a specific Payment Method and make it publicly accessible

Allow users to store payment data for a specific payment method, making it publicly accessible.

### Private Payment Data

These APIs facilitate secure interaction with private payment data between known peers over an established encrypted link. Private payment data is encrypted by `pubky-noise` and exchanged as complete latest-state envelopes: callers manage the full map of private payment entries and pass the complete map each time they call `set_private_payments`.

#### Establish an encrypted link with a counterparty

Allows users to initiate or accept a Pubky Noise handshake, advance it until complete, serialize handshake/link state for durable storage, and restore it after an app restart.

#### Send a private payment envelope

Allows users to send a complete private-payments envelope containing a UUID-v4 `PaymentReference` and a map of `MethodId` to UTF-8 endpoint data. The encrypted plaintext must fit within one pubky-noise message.

#### Receive private payment updates

Allows users to receive the latest/newest queued private-payments envelope from the encrypted link. Private payments are latest-state data: older queued private-payment envelopes are superseded by the latest/newest queued envelope.

#### Issue and receive encrypted receipts

Allows the receiver of a private payment to store an encrypted receipt on their homeserver and send a `ReceiptAccess` descriptor to the counterparty over the encrypted link. Receipt access is event-like/FIFO: every currently available access descriptor is returned in send order by `get_receipt_access` and is not collapsed like private-payment state.

Receipt decryption keys are sensitive. Callers must not log raw receipt keys and should store them only in platform secure storage. Receipt payloads are stored at deterministic homeserver locations derived from `PaymentReference`; reissuing the same reference overwrites the same receipt payload and older access descriptors may stop decrypting.

##### Note:

Private encrypted messages share one ordered Noise stream. Supported message kinds that are received for a different typed helper are buffered in memory, but that buffer is not crash-durable unless the application persists and reconciles its own state alongside link snapshots. Unsupported syntactically valid message kinds are logged and dropped rather than buffered indefinitely.

## Deliverable Example

The first integration will be into bitkit. Thus [https://github.com/synonymdev/bitkit-core/](https://github.com/synonymdev/bitkit-core/) will be a wrapper around deliverables while also provided as a deliverables example.

# Paykit Daemon

The **Paykit Daemon** is a stateful component that keeps track of sent and received payments, provides a unified API for various payment operations, and includes advanced logic for payment prioritization and subscription management.

## Functional Requirements

The **Paykit Daemon** offers the following features:

### Payments

#### To public key

Allows sending payments to a public key with automatic fallback to alternative payment methods based on the default payment selection logic assuming the location of Supported Payment List under conventional path.

#### To URL

Allows sending payments to a URL with automatic fallback to alternative payment methods based on the default payment selection logic. If write permission are granted for a location associated with the provided URL then it should be possible to add optional memo to the executed payment.

### Request

For a known peer who has provided write access to the secure location under their URL, it should be possible to send payment requests. See Appendix for payment request example.

### Receive

Both public and private receiving have an option of automatically recycling payment receiving data upon use, expiration or change in conditions.

#### Receive on public key

Enables publicly receiving payments using multiple selected payment endpoints via one public key. From anyone, with or without specifying amount and / or ID shared with a payer. Using conventional path for Supported Payments List.

#### Receive on URL

Enables receiving payments for optionally specified amount using given multiple selected payment endpoints via one URL using private path and encryption key for the stored content returned as a part of the URL pointing to the Supported Payment List.

### Accounting

Provides an API to retrieve payments based on various filters, such as date range, payment status, payment method, ID, receiver etc

### Events

Provides an API to receive notifications about change of both incoming and outgoing payments as well as new write events in owned locations shared with other network participants.
This feature allows for additional custom data to be provided for transitions after intermediate steps, such as handling payments from multisig accounts or providing second-factor authentication with OTPs for payments.

### Subscriptions

Subscription management in **Paykit Daemon** is designed to be flexible and efficient. Ideally, subscription-related logic should be resolved at the payment protocol level (SEPA standing order / direct debit, BOLT12 subscriptions, etc).
However, if not possible, the Paykit Daemon can handle subscription management by implementing the following subscription functionality while relying on the **Paykit Library** for individual payments in the subscription process.

#### Push subscriptions (Payments)

Allows the payer to create a push subscription to a payee's public key with custom subscription parameters which will satisfy for execution and termination conditions allowing the daemon periodically executes payments based on these conditions.

#### Pull subscriptions (Payment Requests)

Allows the payee to give the payer a subscription URL with secret component and subscription parameters which satisfy for execution and termination conditions. So that the daemon based periodically sends a unique secret derived from the shared key/secret to trigger a payment on the client side to daemon’s public key upon successful validation for the received secret.

## Additional functionality for customization

The Paykit Daemon allows for the following customizable logic for advanced payment management.

### Customization of payment prioritization logic for payee

Allows configuring payment prioritization based on various factors, such as spending fiat currency when the price of Bitcoin is low.

### Customization of payment receiving prioritization

Allows configuring payment receiving prioritization, such as prioritizing payment channels based on available funds or reliability.

## Future development directions

* In the future, Paykit will be able to specify all of the payment types within the Bitcoin world, including all of the competing methods like Offers & LNURL, etc.
* This payment negotiation process is so abstracted that it could allow for new ways to coordinate Bitcoin transactions, including multisigs, DLCs, and mixes.
* It could even support non-Bitcoin payments like credit cards and other payment processors. You only need to locally and mutually support the method across payer and payee.

## Technical Requirements

The daemon is expected to be run as a standalone background process with CLI for administration or to be added as a dependency into a web server. The design and implementation should account for extensibility of adding new payment methods and infrastructure, thus a plugin system is suggested with the instance of the Paykit Daemon being passed as parameter using dependency injection pattern.

### Implementation language

* **Rust:** Edition 2021\. Version \+1.82.0

### Dependencies

* **The Paykit library**

### Database connectivity

* **SQLite:** Version3 (using pluggable connector)

### Payment Infrastructure Connectivity

* **Lightning Network Daemon** \+v0.18.3
* **LNDK** v0.2.0

### Payment Methods Requirements

* Onchain payments for all supported address types
* BOLT11 Invoices. 0 amount BOLT11 Invoices
* BOLT12 Offers

# Resources:

* First draft implementation of paykit library [https://github.com/pubky/paykit-pdk](https://github.com/pubky/paykit-pdk)
* First draft implementation of paykit daemon [https://github.com/pubky/paykit](https://github.com/pubky/paykit)
* Original Slashpay POC Presentation: [https://docs.google.com/presentation/d/1TqbQUbWANzMdze5\_OSdqy7RjajOwHUXRCEC73LQsjyY/edit\#slide=id.g100ef2f468b\_0\_139](https://docs.google.com/presentation/d/1TqbQUbWANzMdze5_OSdqy7RjajOwHUXRCEC73LQsjyY/edit#slide=id.g100ef2f468b_0_139)
* Initial roadmap [https://docs.google.com/document/d/16mpEuyX3yRYLsQRD92T6J1VdUdp2nPhW7Jm6fHgYpHY/edit?pli=1\&tab=t.0](https://docs.google.com/document/d/16mpEuyX3yRYLsQRD92T6J1VdUdp2nPhW7Jm6fHgYpHY/edit?pli=1&tab=t.0)
* Product overview [https://docs.google.com/document/d/1Z1HHdxpkOtelOXJRgPldso4\_-lchzs3NL\_JqDxCdiu8/edit?pli=1\&tab=t.0](https://docs.google.com/document/d/1Z1HHdxpkOtelOXJRgPldso4_-lchzs3NL_JqDxCdiu8/edit?pli=1&tab=t.0)

# Appendix

## Examples

### Possible Examples (Require PMIP)

Examples of **Payment Methods** can be “bitcoin” \- referring to bitcoin onchain payments, “lighting” \- referring to bitcoin lightning network payment, “SEPA” referring to SEPA network bank transfer. Correspondingly **Payment Endpoints** will be bitcoin onchain address, bolt11 invoice or bolt12 offer, IBAN with optional BIC code.

#### Supported Payments List

```
[
  {
    “method”: "paykit.standards.com/p2pkh"
    “endpoint": “payee-paykit-server.com/bitcoin/p2pkh"
  },
  {
    “method”: "paykit.standards.com/lightning",
    “endpoint”: "payee-paykit-server.com/bitcoin/bolt11",
  },
  {
    “method”: "paykit.standards.com/sepa",
    “endpoint”: "payee-paykit-server.com/fiat/euro"
  }
]
```

#### Payment Method Specification (p2pkh)

`Payment Endpoint should return UTF-8 encoded string containing p2pkh address`

#### Payment Endpoint (p2pkh)

`n2HyESbFJAz6PAFuRL5wEqv21yrKt9UTCP`

#### Payment Request

```
{
  "supported payment list": "payee-paykit-server.com/private/random-id-path/payment.json",
  "freequency": "1d",
  "startsAt": "1736415571",
  "endsAt": "1736445571",
  "amount": 0.001,
  "currency": "BTC"
}
```
