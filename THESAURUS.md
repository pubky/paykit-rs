# Paykit Thesaurus

> Canonical ubiquitous language for Paykit v0.2. Use this file before naming protocol concepts, public APIs, docs sections, files, types, fields, endpoints, events, or components.
>
> Paykit v0.2 is a payment protocol on the Pubky Dispatch, and its vocabulary comes in two parts. **Paykit's terms** are defined by the Paykit spec and wire format in `specs/v0.2/`; they are the payment terms this repository owns. **The Dispatch's terms** are defined by the Pubky Dispatch spec and wire format (`spec/` in `pubky/pubky-dispatch`); this file only cites them, with the Dispatch section, and adds how Paykit uses each where that matters. Where this file and a spec disagree, the spec wins and this file is fixed.
>
> Citations: "Paykit spec N" and "Paykit wire N" are Paykit's documents; "Dispatch spec N" and "Dispatch wire N" are the Dispatch's.

# Part 1: Paykit's terms

## Bounded Contexts

### Paykit
- **Definition**: The whole Paykit system/product: Paykit Protocol, Paykit Library and Paykit SDK, and the payment flows, Allowances and Private Payment Lists they carry.
- **NOT**: Only the protocol layer, only one Rust crate, or the Pubky Dispatch it runs on.
- **Synonyms to AVOID**: Paykit SDK when referring to the whole product
- **Related terms**: Paykit Protocol, Paykit Library, Paykit SDK, Pubky Dispatch

### Paykit Protocol
- **Definition**: The rules Paykit's apps follow on top of the Pubky Dispatch, defined in `specs/v0.2/`: Paykit's scopes and the Dispatch permissions each needs, its message kinds and bodies, its collections and their policies, the payment flows, Allowances, recurring requests and labelled selling scopes (Paykit spec, Dispatch spec 1).
- **NOT**: The Pubky Dispatch protocol, which Paykit uses without changing; or a specific Rust implementation.
- **Synonyms to AVOID**: routing network protocol, Paykit SDK protocol
- **Related terms**: Protocol on the Dispatch, Paykit Library, Payment Request, Private Payment List

### Paykit Library
- **Definition**: The `paykit-lib` crate: Paykit's formats and rules on `pubky-dispatch-core`. That is its scopes and permission table, the certificate permission check, its labels and derivations, message kinds and bodies, collections and policies, obligations, amounts and the payment rules. It does no I/O and reads no clock.
- **NOT**: An SDK, a runtime, the protocol specification, or `pubky-dispatch-core`.
- **Synonyms to AVOID**: Paykit SDK, Paykit core runtime
- **Related terms**: Paykit Protocol, Paykit SDK, Dispatch crates

### Paykit SDK
- **Definition**: The `paykit-sdk` crate that wallets and seller apps use: Paykit's payment flows and their state, on `pubky-dispatch-client` and `pubky-dispatch-signer`. That is requests and their answers, proofs and receipts, attempts on claims, recurring requests, Allowances and the ledger, Private Payment Lists, labelled selling scopes, receiving and re-sends, and Paykit's side of approval.
- **NOT**: Paykit Library, Paykit Protocol, the Dispatch client, or payment execution and settlement logic.
- **Synonyms to AVOID**: Paykit core, Pubky SDK
- **Related terms**: Paykit Library, Dispatch crates, Language Bindings, Rail

### Language Bindings
- **Definition**: Distribution/integration surfaces for the Paykit SDK in languages or platforms such as Swift and Kotlin. Built in their own workstream, after the Rust crates.
- **NOT**: First-class Paykit architecture components.
- **Synonyms to AVOID**: Paykit SDK
- **Related terms**: Paykit SDK

## Apps and Scopes

### Paykit App
- **Definition**: An app of the Dispatch acting for an identity in Paykit: one application with its own keys, certificate and Paykit scopes. An app may run on several devices; Paykit treats it as one app (Paykit spec Terms, Dispatch spec 3.1). Short form: app.
- **NOT**: An identity, a device, or a Payment Endpoint.
- **Related terms**: App, App ID, App certificate, Paykit scope

### Paykit scope
- **Definition**: A scope Paykit defines under the Dispatch's scope rule: `paykit_pay`, `paykit_request`, `paykit_request:<label>`, `paykit_allowance`, `paykit_allowance_execute`, `paykit_contacts`, `paykit_manage_contacts` and `paykit_ext:<prefix>` (Paykit spec 3.1). A certificate lists every scope it holds explicitly, required ones included.
- **NOT**: A Dispatch permission; the Dispatch knows no scope names. Nor a scope whose root isn't Paykit's: `pay` and `request` are another protocol's, which Paykit gives no meaning (Paykit spec 3.2).
- **Related terms**: Scope, Dispatch permission, Labelled selling scope, Extension scope

### Permission table
- **Definition**: Paykit's table of the Dispatch permissions each Paykit scope needs (Paykit spec 3.2). The signer writes the certificate's `permissions` from it; Paykit's verifiers derive the list again and refuse a certificate whose `permissions` differ.
- **NOT**: Something the Dispatch enforces; it enforces only the signed list.
- **Related terms**: Paykit scope, Dispatch permission, App certificate

### Labelled selling scope
- **Definition**: A scope `paykit_request:<label>`, a sub-scope of `paykit_request`, that lets a hosted selling app act as payee for its own sales only. A certificate holds `paykit_request` or labels, never both, and at most 8 labels; its keys are label keys, derived from the `paykit_request` key (Paykit spec 3.1, 3.5).
- **NOT**: A display name; labels should be opaque, like `shop1`.
- **Related terms**: Paykit scope, Sub-scope, Payment Request, Activity record

### Extension scope
- **Definition**: A scope `paykit_ext:<prefix>`, such as `paykit_ext:paykit-cap`, covering every extension kind `<prefix>.<name>`. It has no scope keys; a certificate holds at most 8, and the prefix always starts with `paykit-` (Paykit spec 3.1).
- **Related terms**: Extension kind, Paykit scope

### Extension kind
- **Definition**: A message kind `<prefix>.<name>` covered by `paykit_ext:<prefix>`. Extension kinds carry no Paykit payment semantics: no payment relies on one and no Paykit collection records one (Paykit spec 3.3).
- **NOT**: A Paykit payment message kind.
- **Related terms**: Extension scope, Paykit message kind

## Messages and Shared Records

### Paykit message kind
- **Definition**: The `kind` of a Paykit message: `paykit.` followed by a name such as `payment_request`, `payment_proof`, `private_payment_list` or `allowance_proposal` (Paykit spec 5, Paykit wire 4.3). Its row in Paykit spec 3.3 fixes the scopes a sender and recipient need.
- **NOT**: The message body or the Rust type that represents it.
- **Synonyms to AVOID**: Private Message Kind
- **Related terms**: Message, Paykit scope, Extension kind

### Resend request
- **Definition**: A `paykit.resend_request` message asking a counterparty to re-send what it sent in the last 30 days, by `msg_id`, request, Billing Period or a range of acceptance times. Best effort (Paykit spec 9, Paykit wire 4.12). It is Paykit's re-send kind under Dispatch spec 11.
- **Related terms**: Message ID, Delivery

### Paykit collection
- **Definition**: One of Paykit's collections of shared records, each with a fixed policy: `payer_activity`, `payee_activity`, `payment_lists`, `allowances` and `allowance_ledgers` (Paykit spec 6, Paykit wire 5).
- **Related terms**: Collection, Policy, Activity record

### Activity record
- **Definition**: A shared record in `payer_activity` or `payee_activity` holding the messages that make up one request's, Billing Period's or payment's state, one per role (Paykit spec 6, Paykit wire 5.3).
- **Related terms**: Paykit collection, Stored message, Payment Request

### Stored message
- **Definition**: A message kept, with its exact bytes, inside a Paykit shared record value, so own apps can check it themselves (Paykit wire 5.2).
- **Related terms**: Activity record, Shared record

### Obligation ID
- **Definition**: Paykit's claim key for one obligation: of a request and Billing Period, derived under `paykit/v1/obligation` from the blinded payee and request hash and the period start; of a payment without a request, from its attempt ID alone (Paykit spec 10, Paykit wire 1.5).
- **Related terms**: Obligation, Claim, Naming key

### Paying order
- **Definition**: The steps every Paykit payment follows: write the `attempt`, pay, write `done` with the activity record, send the proof (Paykit spec 7.2).
- **Related terms**: Attempt, Claim, Payment Proof

### Handoff
- **Definition**: The moment an app gives a payment to the payment method in the paying order: before it the payment can still be stopped; after it a payment may have been made (Paykit spec 7.2). The verb is "hand off"; a payment that was never handed off was never given to the method. Wire and vector names keep their spelling for now: `never_dispatched` and `DispatchedPayment`.
- **Synonyms to AVOID**: Dispatch (the Dispatch is the service the apps use)
- **Related terms**: Paying order, Payment Method, Rail

### Reconciliation
- **Definition**: How an app settles the state of its payments against its claims, kept proofs and receipts after a gap, a restore or a move (Paykit spec 7.3).
- **Related terms**: Claim, Paying order

## Payments

### Payment List
- **Definition**: A payee-published or payee-shared collection of Payment Endpoints: public in an app record's `public_endpoints`, or private inside a Private Payment List (Paykit spec 4).
- **NOT**: The result of payer-side processing of compatible endpoints.
- **Synonyms to AVOID**: Payment Method List
- **Related terms**: Payment Endpoint, Private Payment List, Payee

### Payment Endpoint
- **Definition**: A whole payee-owned item in a Payment List, consisting of a Payment Endpoint Identifier and a Payment Endpoint Payload.
- **NOT**: Only the address, invoice, IBAN, offer, or credential payload.
- **Synonyms to AVOID**: Payment Option
- **Related terms**: Payment Endpoint Identifier, Payment Endpoint Payload, Payment List

### Payment Endpoint Identifier
- **Definition**: The canonical machine-readable identifier for a payment endpoint type, such as `btc-lightning-bolt12` or `eur-sepa-iban`: ASCII letters and digits, `-`, `_` and `.`, at most 64 characters, never `.`, `..` or `private` (Paykit wire 1.4).
- **NOT**: The full Payment Endpoint or its payload.
- **Synonyms to AVOID**: method id, payment method id
- **Related terms**: Payment Endpoint, Payment Method, Asset, Rail

### Payment Endpoint Payload
- **Definition**: The opaque, non-empty UTF-8 data part of a Payment Endpoint containing the actual receiving handle or details. Paykit never interprets it (Paykit wire 1.4).
- **NOT**: The whole Payment Endpoint.
- **Synonyms to AVOID**: endpoint data in domain docs
- **Related terms**: Payment Endpoint, Payment Endpoint Identifier

### Payment Method
- **Definition**: A higher-level human/domain concept for how value can be transferred, mapping to one or more Payment Endpoint Identifiers.
- **Synonyms to AVOID**: Payment Option
- **Related terms**: Payment Endpoint Identifier, Rail

### Asset
- **Definition**: The unit of value being transferred, such as `btc`, `eur`, `usd`, or `usdt`; compared exactly and case-sensitively (Paykit wire 1.2).
- **NOT**: The settlement rail or endpoint format.
- **Related terms**: Payment Amount, Payment Endpoint Identifier

### Payment Amount
- **Definition**: An amount of value, `{ "value", "asset" }`, with `value` a decimal string in whole units of its asset, compared and added exactly as an integer scaled by 10^18 (Paykit wire 1.2).
- **NOT**: A float, or an amount in the asset's smallest unit.
- **Related terms**: Asset, Payment Request, Allowance

### Rail
- **Definition**: The settlement system carrying the asset, such as Lightning, Bitcoin or SEPA. Rails are outside Paykit: the SDK pays through a narrow rail interface (pay an endpoint with an idempotency key; read the verified payment time).
- **NOT**: Part of the Paykit Protocol.
- **Related terms**: Payment Method, Attempt, Payment Proof, Settlement ID

### Settlement ID
- **Definition**: The bytes the payment method defines to name one settlement, the same in every proof of it. The payee's payment method works it out from the proof's `proof` object and the Payment Endpoint Identifier; Paykit defines no settlement ID and no proof format, and whether a method names a settlement by a whole transaction or by one part of it is the method's. It keys the payee's record of a payment without a request (Paykit spec 8.4, Paykit wire 1.5).
- **Related terms**: Payment Proof, Payment without a request

### Payer
- **Definition**: The party sending value in a payment flow; its apps hold `paykit_pay`.
- **Related terms**: Payee, Counterparty

### Payee
- **Definition**: The party receiving value in a payment flow and publishing or sharing Payment Endpoints; its apps hold `paykit_request` or a labelled selling scope.
- **Related terms**: Payer, Payment List

### Counterparty
- **Definition**: The other identity in a payment interaction or conversation.
- **Related terms**: Payer, Payee, Conversation entry

### Payment Request
- **Definition**: A `paykit.payment_request` message where a payee asks a payer for payment, one-time or recurring, answered by acceptance, rejection or cancellation (Paykit spec 8.1, Paykit wire 4.5).
- **NOT**: An Allowance, a Payment List, or payment execution.
- **Related terms**: Recurring Payment Request, Payment Proof, Payment Receipt, Activity record

### Payment without a request
- **Definition**: A payment a payer makes to a payee's endpoint with no Payment Request, reported by a proof; its obligation is derived from the attempt ID alone (Paykit spec 8.4).
- **Related terms**: Settlement ID, Single-use endpoint, Payment Proof

### Recurring Payment Request
- **Definition**: A Payment Request with a `recurrence`, paid once per Billing Period after acceptance (Paykit spec 8.2).
- **NOT**: A payee-controlled pull authorization.
- **Synonyms to AVOID**: pull subscription
- **Related terms**: Payment Request, Billing Period, Recurrence, Period details

### Subscription
- **Definition**: Product shorthand for an accepted Recurring Payment Request.
- **NOT**: An Allowance or a separate protocol family.
- **Related terms**: Recurring Payment Request

### Recurrence
- **Definition**: The schedule object of a Recurring Payment Request: `every`, `unit`, `starts_at`, `anchor` and optional `ends_at` (Paykit wire 4.4).
- **NOT**: A cron expression or a local scheduler.
- **Related terms**: Billing Period

### Billing Period
- **Definition**: One period of a recurring request, `[boundary(k), boundary(k+1))`, computed from its Recurrence (Paykit spec 8.2, Paykit wire 4.4). Messages name it by `{starts_at, ends_at}`.
- **NOT**: A Payment Request or a recurrence rule.
- **Synonyms to AVOID**: billing cycle
- **Related terms**: Recurrence, Obligation ID, Period details

### Period details
- **Definition**: A `paykit.payment_period_details` message from the payee with one Billing Period's conversion rates, its destination, or both, valid until `expires_at` (Paykit spec 8.2, Paykit wire 4.9).
- **Synonyms to AVOID**: Payment Conversion Quote
- **Related terms**: Billing Period, Conversion Rate

### Conversion Rate
- **Definition**: An exact decimal multiplier expressing payment Asset units per one requested Asset unit (`specs/payment-conversion.md`, Paykit wire 1.2).
- **NOT**: A market-rate feed or a payment authorization.
- **Related terms**: Payment Amount, Period details

### Payment deadline
- **Definition**: An inclusive deadline for actual payment, absolute for a one-time request or relative to a recurring Billing Period's start (Paykit wire 4.4, 4.5).
- **NOT**: The proposal expiry or the recurrence end.
- **Related terms**: Payment Request, Claim

### Payment Proof
- **Definition**: A `paykit.payment_proof` message: method-specific evidence for one concrete payment, referencing the request it pays, if any, and the `attempt_id` it reports (Paykit wire 4.10).
- **NOT**: A Payment Receipt, or proof that Paykit itself validates.
- **Synonyms to AVOID**: payment receipt when the method-specific proof is meant
- **Related terms**: Paying order, Payment Receipt, Rail, Settlement ID

### Payment Receipt
- **Definition**: A `paykit.payment_receipt` message from the payee confirming a payment it received, referencing its proof (Paykit wire 4.11).
- **NOT**: A v0.1 Encrypted Receipt stored on a Homeserver, or Receipt Access.
- **Related terms**: Payment Proof, Claim

### Private Payment List
- **Definition**: A `paykit.private_payment_list` message carrying one Paykit App's Payment List for one counterparty, with a version (its publishing time in milliseconds), an expiry and optional single-use endpoints. The latest valid list from an app supersedes its older ones (Paykit spec 5, Paykit wire 4.13).
- **NOT**: A Payment Request or a Payment Proof.
- **Synonyms to AVOID**: Private Payment Method List, Private Payment Envelope
- **Related terms**: Payment List, Single-use endpoint

### Single-use endpoint
- **Definition**: A Payment Endpoint in a Private Payment List marked `single_use`, which may be paid once; the payer reserves it in the `payment_lists` record in the attempt's batch (Paykit spec 8.4).
- **Related terms**: Private Payment List, Attempt

## Allowances

### Allowance
- **Definition**: Prior authority, built entirely in apps on claims and shared records, for one executor app to pay a counterparty's qualifying requests automatically within accepted terms (Paykit spec 8.3).
- **NOT**: A balance, a transfer of custody, a Payment Request, a Subscription, or a bearer credential. The Pubky Dispatch has no Allowance objects.
- **Synonyms to AVOID**: subscription, standing order, spending account
- **Related terms**: Allowance terms, Executor, Executor grant, Allowance ledger

### Allowance terms
- **Definition**: The immutable maximum authority of one Allowance: its asset, limits, time bounds and allowed Payment Endpoint Identifiers (Paykit wire 4.15).
- **Related terms**: Allowance, Executor grant, Terms hash

### Terms hash
- **Definition**: `hash(lp(proposal message JSON bytes, acceptance message JSON bytes))`, naming one Allowance's accepted terms (Paykit wire 1.5).
- **Related terms**: Allowance terms, Executor grant

### Executor
- **Definition**: The one app that an Allowance record names, with the app's own signed grant, to pay automatically under that Allowance. It must hold `paykit_allowance_execute` (Paykit spec Terms, 8.3).
- **Related terms**: Executor grant, Consent list, Allowance

### Executor grant
- **Definition**: The executor's signed statement in the Allowance record, under the label `paykit/v1/executor-grant`, over the identity, the counterparty, the `allowance_id`, the terms hash and the record version it builds on (Paykit spec 10, Paykit wire 3.3).
- **Related terms**: Executor, Allowance terms

### Consent list
- **Definition**: The Allowances an executor keeps durably in its own state as consented to; it pays under an Allowance only while it is on the list (Paykit spec 8.3).
- **Related terms**: Executor, `executor_notice`

### Allowance ledger
- **Definition**: One shared record per Allowance in `allowance_ledgers`: an entry per attempt made under it, and running totals (Paykit spec 8.3, Paykit wire 5.5).
- **Related terms**: Allowance, Attempt

# Part 2: The Dispatch's terms

Paykit uses these as the Pubky Dispatch defines them. Each entry cites its Dispatch section; the Dispatch spec's Terms table is the full list.

## The Dispatch and its crates

### Pubky Dispatch
- **Definition**: An inbox and coordination service for Pubky identities. It receives an identity's messages and hosts its coordination primitives; it reads no message and no shared data, and knows nothing of payments. One service hosts many identities; each identity uses one at a time. Short form: Dispatch (Dispatch spec Terms, 2).
- **NOT**: A Homeserver, a federation, an ordering server, or anything Paykit-specific.
- **Synonyms to AVOID**: Inbox Service, relay, mailbox server, Paykit Dispatch
- **Related terms**: Inbox, Feed, Claim, Shared record, Chain

### Protocol on the Dispatch
- **Definition**: The rules a protocol's apps follow on top of the Dispatch's formats and primitives: its scopes, the permissions they need, its message kinds, its collections and its own signed formats (Dispatch spec 1). Paykit is the first.
- **Related terms**: Paykit Protocol, Scope, Collection

### Dispatch crates
- **Definition**: The Rust crates in `pubky/pubky-dispatch`: `pubky-dispatch-core` (the stateless Dispatch formats and checks), `pubky-dispatch-client` (the app side, with its `StateStore`), `pubky-dispatch-signer` (identity management) and `pubky-dispatch` (the service). Paykit depends on the first three; none depends on Paykit.
- **NOT**: Part of this repository.
- **Related terms**: Paykit Library, Paykit SDK

## Identities, Apps and Keys

| Term | Meaning, and Paykit's use | Source |
|---|---|---|
| **Identity** | A Pubky identity, named by its Ed25519 public key (its Pubky ID). | Dispatch spec Terms |
| **Signer** | The software holding the identity key, such as Ring, or Bitkit acting as signer. One per identity at a time. In code, identity management is `pubky-dispatch-signer`. | Dispatch spec 2 |
| **App** | One application acting for an identity, with its own keys and certificate. A Paykit App is an app holding Paykit scopes. | Dispatch spec 3.1 |
| **App ID** | The first 16 bytes of `BLAKE3(signing public key)`, z-base-32, 26 characters. It names an app, never stands in for its keys. Not a chosen name such as `bitkit`. | Dispatch spec 3.1, Dispatch wire 1.3 |
| **Current app** | An app listed in the newest `identity.json` that the Dispatch hasn't revoked or recorded as departed. Payers pay only to current apps' destinations (Paykit spec 4). | Dispatch spec 3.4 |
| **Own app** | A current app of the identity in question. | Dispatch spec Terms |
| **App certificate** | The identity-signed certificate of an app: its signing key, bundle key, `scopes`, `permissions` and validity. | Dispatch spec 3.2, Dispatch wire 3.1 |
| **App record** | An app's public, self-signed description: signing key, certificate, accepted kinds, optional public endpoints. Paykit fills `accepts`, `capabilities` and `public_endpoints` (Paykit spec 4). | Dispatch spec 4.2, Dispatch wire 3.2 |
| **`identity.json`** | The identity-signed document naming the Dispatch, the admission policy, the current and removed apps, the data epoch and the signing install. | Dispatch spec 4.3, Dispatch wire 3.3 |
| **Version** | The counter of `identity.json`. Shared records and the chain have versions of their own, and so do Private Payment Lists (Paykit spec Terms). | Dispatch spec Terms |
| **Generation** | The counter of the Dispatch named in `identity.json`; it increases on a move and after a provider loss. | Dispatch spec 12.6 |
| **Scope** | A permission string in a certificate: a name, or a name and a sub-scope `name:<sub>`. The Dispatch knows no scope names; Paykit's are its Paykit scopes. | Dispatch spec 3.3 |
| **Sub-scope** | The `<sub>` part of `name:<sub>`, with keys derived from its parent's. A labelled selling scope is one. | Dispatch spec 3.3, 3.5 |
| **Dispatch permission** | One of `claims.read`, `claims.write`, `lists.read`, `lists.manage`, `reply.add`, listed in a certificate's `permissions`. Paykit's permission table says which each Paykit scope needs. | Dispatch spec 3.2 |
| **Signing key** | An app's static Ed25519 key: it signs messages, requests, operations, its record and its receive key records. Not the identity key. | Dispatch spec 3.1 |
| **Bundle key** | An app's static X25519 key, named in its certificate, to which the signer seals its key bundles. | Dispatch spec 3.5 |
| **Receive key** | An app's current X25519 key for messages, with a number `n` and a kid; replaced by a fresh key about once a day. | Dispatch spec 3.6 |
| **Receive key record** | The app-signed record of an app's current receive key, held by the dispatch. | Dispatch spec 4.4, Dispatch wire 3.4 |
| **kid** | The 8-byte key ID of a receive key, `b64(8)`; it names one key in the whole account. | Dispatch wire 1.7 |
| **Shared Data Key** | Identity-wide keys, one per scope and data epoch, that encrypt data shared between own apps. Which scopes have keys is the protocol's choice (Paykit spec 3.5). | Dispatch spec 3.5 |
| **Data epoch** | The counter of the Shared Data Key; it increases on each rotation. | Dispatch spec 3.5 |
| **Key bundle** | An app's scope keys for the data epochs it lacks, and the naming key, sealed to its bundle key by the signer. | Dispatch spec 3.5, Dispatch wire 3.5 |
| **Naming key** | An identity-wide key that blinds natural keys and obligation inputs, so record and claim keys don't name their counterparty. | Dispatch spec 3.5 |
| **Service key** | The key a Dispatch generates for one account at enrollment; it signs that account's responses, transition proofs and signed heads. | Dispatch spec 4.3 |

## Messages, Admission and the Feed

| Term | Meaning, and Paykit's use | Source |
|---|---|---|
| **Envelope** | The binary sealed form of a message: HPKE key wraps to each recipient app's receive key, and the content under a key commitment. Paykit messages travel only as envelopes. | Dispatch spec 5.1, Dispatch wire 4 |
| **Message** | The signed plaintext inside an envelope, with the common fields every protocol's kinds share. | Dispatch spec 5.2, 5.3, Dispatch wire 5.1 |
| **Message ID** | The `msg_id` of a message, a lower-case UUID v4. Paykit's `refs` and replay checks use it. Not a v0.1 Event ID. | Dispatch wire 1.7 |
| **Sealed value** | A field of shared state sealed under a Shared Data Key for its scope and data epoch. Not an envelope. | Dispatch spec 13.4 |
| **Inbox** | The mailbox part of the Dispatch, where senders' messages land and the identity's apps read them. Never Homeserver files. | Dispatch spec Terms |
| **Inbox entry** | One encrypted message stored for one or more target apps. | Dispatch spec Terms |
| **`seq`** | The position of an item at the Dispatch within one generation; inbox entries and chain items share one sequence. | Dispatch spec Terms |
| **Feed** | The ordered stream an own app reads: the items after its cursor that it may read, and chain stubs for the rest. | Dispatch spec 8.3 |
| **Cursor** | An app's position in the feed, `<generation>:<seq>`. | Dispatch wire 1.7 |
| **Directory** | The Dispatch's answer naming an identity's current apps for senders: their app records and receive key records. | Dispatch spec 8.1 |
| **Delivery** | How a message reaches its recipient's inbox, with retries and re-sends; Paykit's re-send kind is its Resend request. | Dispatch spec 11 |
| **Allowlist** | The kept peers and reply grants that admit senders, one row per peer with each reason held on its own. No unsolicited first contact by Pubky ID alone. | Dispatch spec 7 |
| **Deny list** | The identities an identity has blocked at its dispatch. | Dispatch spec 7 |
| **Kept peer** | A peer a managing app explicitly kept on the allowlist. Not a v0.1 Public Contact Marker. | Dispatch spec 7 |
| **Reply grant** | The allowlist reason that lets a peer reply after an own app sent to it, invited it or accepted its connect request. | Dispatch spec 7 |
| **Invite** | A reply grant added before any message, so the counterparty can write first, for example a shop at checkout. | Dispatch spec 7 |
| **Invite code** | A random code an app registers and shows, as `pubky://<id>?dispatch=invite&code=<code>`, so that anyone who has it can write once. | Dispatch spec 7, Dispatch wire 1.7 |
| **Connect request** | A content-free request from a stranger to be invited. | Dispatch spec 7 |

## Coordination

| Term | Meaning, and Paykit's use | Source |
|---|---|---|
| **Obligation** | Something that must happen at most once, such as paying one request in one Billing Period. Paykit names it by its Obligation ID. | Dispatch spec Terms, 10.2 |
| **Claim** | The server-checked state of one obligation, written with claim entries such as `attempt` and `done`. Not a lock or a lease. | Dispatch spec 10.2, Dispatch wire 6.8 |
| **Attempt** | One try at carrying out an obligation, with its own outcome and an `attempt_id`, which Paykit passes to the rail as its idempotency key. | Dispatch spec Terms, 10.2 |
| **Step** | The position of an entry in one obligation's log. | Dispatch spec Terms |
| **Shared record** | A small piece of encrypted shared state, updated by compare-and-swap on its record version. Not the v0.1 shared-state blob. | Dispatch spec 10.3 |
| **Collection** | A set of shared records that can be listed together. Paykit's are its Paykit collections. | Dispatch spec 10.3 |
| **Policy** | The scopes a shared record requires, `read_scope` and `write_scopes`, fixed at creation. The admission settings in `identity.json` are also called `policy`. | Dispatch spec 10.3 |
| **Record version** | `{ "generation", "version" }`, the generation and `seq` of the write that produced a record. | Dispatch wire 1.7 |
| **Natural key** | The input that names what a record is about, blinded with the naming key into the record key. Paykit's natural keys are in Paykit wire 1.5. | Dispatch spec 13.3 |
| **Operation** | A change sent in a batch, signed by its writer except cursor moves. | Dispatch spec 13.5, Dispatch wire 6.1 |
| **Condition** | A requirement attached to an operation, checked atomically with it, such as a record version or `server_time_in`. | Dispatch wire 6.3 |
| **Batch** | Up to 8 operations and their conditions, applied all or nothing. | Dispatch wire 6.4 |
| **Transition proof** | The service-signed result of one operation, with its place in the chain. | Dispatch wire 6.5 |
| **Chain** | The hash chain over operations, `identity.json` deliveries and service transitions, one per generation. | Dispatch spec 8.3, Dispatch wire 6.6 |
| **Chain stub** | A chain item's `seq` and hashes, without its content. | Dispatch spec 8.3 |
| **Signed head** | The service-signed chain hash at a given `seq`. | Dispatch wire 6.7 |

## Lifecycle

| Term | Meaning, and Paykit's use | Source |
|---|---|---|
| **Approval** | The signer's answer to an app request: certificate, `identity.json` and a full key bundle. Paykit supplies the scopes asked for and their permissions. | Dispatch spec 3.2, Dispatch wire 3.9, 3.10 |
| **Enrollment** | The signer's setup of an identity at a dispatch. | Dispatch wire 3.7 |
| **Revocation** | The signer's removal of an app, with `distrust_from` and a data epoch rotation; its App ID is never approved again. | Dispatch spec 12.2 |
| **Provider move** | A cooperative move to another dispatch: freeze, export, import, activation and publish. | Dispatch spec 12.5, Dispatch wire 10 |
| **Freeze** | The signed stop of an account during a move; the Dispatch then answers `410 moved`. | Dispatch wire 3.6 |
| **Provider loss** | Recovery when the Dispatch is unreachable or hostile: a new generation at another dispatch. | Dispatch spec 12.6 |
| **Account erasure** | Erasing an identity's account at its dispatch. | Dispatch spec 12.7 |
| **Signer hand-over** | The transfer of an identity from one signer to another. | Dispatch spec 12.8, Dispatch wire 3.12 |
| **Push relay** | A vendor-run service that turns the Dispatch's signed, content-free webhooks into APNs or FCM pushes. | Dispatch spec 9 |
| **Handle** | An opaque ID the push relay maps to one device. | Dispatch spec 9 |
| **Push key** | The key the Dispatch mints for one app's push registration; it signs that app's webhooks. | Dispatch spec 9 |

# Forbidden Lexicon

These terms must not be used for new Paykit v0.2 names:

- **Encrypted Link**, **Encrypted Link Handshake**, **Linked Peer**, **Noise link**, **relinking** -> v0.2 has none; use **Envelope**, **Inbox** and **Counterparty**.
- **pubky-noise**, **Noise key** -> use **Receive key** or **Bundle key**.
- **Paykit App Registry**, **Noise Key Authorization**, **Public Contact Marker** -> removed in v0.2; use **`identity.json`**, **App record** and **Kept peer**.
- **Paykit Signer**, **`paykit-signer`**, **`pubky-dispatch-paykit`** -> these crates don't exist; identity management is **`pubky-dispatch-signer`**.
- **Paykit Dispatch**, **Paykit identity**, **Paykit certificate**, **Paykit envelope** -> these are the Dispatch's: use **Pubky Dispatch**, **Identity**, **App certificate** and **Envelope**.
- **Paykit Profile**, **Paykit Blob** -> removed in v0.2. Each app chooses how to display identities.
- **shared-state blob**, **lease**, **lock** -> use **Shared record** and **Claim**.
- **Inbox Service** -> use **Pubky Dispatch**.
- **Delivery Confirmation** -> not mandatory in v0.2; use **Payment Receipt** for a payee's confirmation of payment.
- **Receipt Access**, **Receipt Location**, **Encrypted Receipt**, **Receipt Decryption Key** -> removed in v0.2; use **Payment Receipt**.
- **Event ID**, **Event Message**, **Latest-State Message**, **Private Application Message** -> use **Message ID**, **Paykit message kind**, and the semantics each kind's section gives.
- **Payment Conversion Quote** -> use **Period details**.
- **Payment Method List** -> use **Payment List**.
- **Payment Option** -> use **Payment Endpoint** or **Payment Method**, depending on meaning.
- **MethodId** -> use **Payment Endpoint Identifier**.
- **EndpointData** -> use **Payment Endpoint Payload**.
- **Private Payment Envelope** / **PrivatePaymentsPayload** -> use **Private Payment List**.
- **SubscriptionAgreement** / **subscription_id** -> use **Recurring Payment Request** and its request hash or message ID.
- **pull subscription** -> use **Recurring Payment Request**, and **Allowance** for prior authority to pay it automatically.
- **Paykit App ID** for a chosen app name such as `bitkit` -> an **App ID** is derived from the signing key.

# Component Model

Paykit crates (this repository):
- Paykit Library (`paykit-lib`), on `pubky-dispatch-core`
- Paykit SDK (`paykit-sdk`), on `pubky-dispatch-client` and `pubky-dispatch-signer`

Dispatch crates (`pubky/pubky-dispatch`):
- `pubky-dispatch-core`, `pubky-dispatch-client`, `pubky-dispatch-signer`, `pubky-dispatch`

Services:
- Pubky Homeserver
- Pubky Dispatch
- Push relay

Paykit's terms:
- Paykit App, Paykit scope, Permission table, Labelled selling scope, Extension scope, Extension kind
- Paykit message kind, Resend request, Paykit collection, Activity record, Stored message, Obligation ID, Paying order, Reconciliation
- Payment List, Payment Endpoint, Payment Endpoint Identifier, Payment Endpoint Payload, Payment Method, Asset, Payment Amount, Rail, Settlement ID
- Payer, Payee, Counterparty
- Payment Request, Payment without a request, Recurring Payment Request, Subscription, Recurrence, Billing Period, Period details, Conversion Rate, Payment deadline
- Payment Proof, Payment Receipt, Private Payment List, Single-use endpoint
- Allowance, Allowance terms, Terms hash, Executor, Executor grant, Consent list, Allowance ledger

The Dispatch's terms, as Paykit uses them:
- Identity, Signer, App, App ID, Current app, Own app, App certificate, App record, `identity.json`, Version, Generation
- Scope, Sub-scope, Dispatch permission
- Signing key, Bundle key, Receive key, Receive key record, kid, Shared Data Key, Data epoch, Key bundle, Naming key, Service key
- Envelope, Message, Message ID, Sealed value, Inbox, Inbox entry, `seq`, Feed, Cursor, Directory, Delivery
- Allowlist, Deny list, Kept peer, Reply grant, Invite, Invite code, Connect request
- Obligation, Claim, Attempt, Step, Shared record, Collection, Policy, Record version, Natural key
- Operation, Condition, Batch, Transition proof, Chain, Chain stub, Signed head
- Approval, Enrollment, Revocation, Provider move, Freeze, Provider loss, Account erasure, Signer hand-over, Push relay, Handle, Push key
