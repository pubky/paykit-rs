# Paykit Thesaurus

> Canonical ubiquitous language for Paykit v0.2. Use this file before naming protocol concepts, public APIs, docs sections, files, types, fields, endpoints, events, or components.
>
> Domain rule: the terms below win over older README/code wording when creating new names. `specs/v0.2/spec.md` (its Terms table) and `specs/v0.2/wire-format.md` are the source of these definitions; where this file and the spec disagree, the spec wins and this file is fixed.

## Bounded Contexts

### Paykit
- **Definition**: The whole Paykit system/product: Paykit Protocol, Paykit Library, Paykit SDK, Paykit Signer, and the payment flows, Allowances and Private Payment Lists they carry.
- **NOT**: Only the protocol layer or only one Rust crate.
- **Synonyms to AVOID**: Paykit SDK when referring to the whole product
- **Related terms**: Paykit Protocol, Paykit Library, Paykit SDK, Paykit Signer

### Paykit Protocol
- **Definition**: The domain rules, data model, encodings and flows of `specs/v0.2/`: identities and their apps, sealed messages through the Pubky Switch, coordination with claims and shared records, payment flows and lifecycle.
- **NOT**: A specific Rust implementation or runtime.
- **Synonyms to AVOID**: routing network protocol, Paykit SDK protocol
- **Related terms**: Paykit Library, Pubky Switch, Payment Request, Private Payment List

### Paykit Library
- **Definition**: The stateless protocol core, the `paykit-lib` crate: encodings, key derivations, signatures, envelopes and sealed values, signed documents, message checks, scopes, the pure claim and record rules, and chain verification. It does no I/O and reads no clock.
- **NOT**: An SDK, a runtime, or the protocol specification.
- **Synonyms to AVOID**: Paykit SDK, Paykit core runtime
- **Related terms**: Paykit Protocol, Paykit SDK, Paykit Signer

### Paykit SDK
- **Definition**: The stateful app SDK, the `paykit-sdk` crate, that wallets and seller apps use: the Pubky Switch client, Homeserver reads and writes, app state through a host-implemented state store, receiving and sending messages, payment flows, provider moves, recovery and push registration.
- **NOT**: Paykit Library, Paykit Protocol, Paykit Signer, or payment execution and settlement logic.
- **Synonyms to AVOID**: Paykit core, Pubky SDK
- **Related terms**: Paykit Library, Paykit Signer, Language Bindings, Rail

### Paykit Signer
- **Definition**: The signer core, the `paykit-signer` crate: the part that manages an identity on behalf of the Signer. It enrolls the identity at a Pubky Switch, signs `identity.json`, approves apps, issues certificates and key bundles, and carries out revocation, provider moves, recovery, account erasure and the hand-over.
- **NOT**: The Signer itself (the software holding the identity key), or a Paykit App's own signing key.
- **Related terms**: Signer, Identity, App certificate, Key bundle, Signer hand-over

### Language Bindings
- **Definition**: Distribution/integration surfaces for the Paykit SDK and Paykit Signer in languages or platforms such as Swift and Kotlin. Built in their own workstream, after the Rust crates.
- **NOT**: First-class Paykit architecture components.
- **Synonyms to AVOID**: Paykit SDK
- **Related terms**: Paykit SDK, Paykit Signer

## Identity and Apps

### Identity
- **Definition**: A Pubky identity, named by its Ed25519 public key (its Pubky ID).
- **NOT**: A Paykit App, an App ID, or a device.
- **Related terms**: Signer, Paykit App, `identity.json`

### Signer
- **Definition**: The software holding the identity key, such as Ring, or Bitkit acting as signer. An identity has one signer at a time and can hand it over to another (spec 13.8).
- **NOT**: A Paykit App's signing key, or the Paykit Signer crate alone.
- **Related terms**: Paykit Signer, Identity, Signer hand-over

### Paykit App
- **Definition**: One application acting for an identity in Paykit, with its own signing key, bundle key, receive keys and App certificate. An app may run on several devices; Paykit treats it as one app (spec 3.1). Short form: app.
- **NOT**: An identity, a device, or a Payment Endpoint.
- **Related terms**: App ID, App certificate, App record, Current app, Own app

### App ID
- **Definition**: The identifier of a Paykit App: the first 16 bytes of `BLAKE3(signing public key)`, z-base-32 encoded, 26 characters (spec 3.1, wire 1.3). It names an app; it never stands in for its keys.
- **NOT**: A Pubky ID, a Payment Endpoint Identifier, or a user-readable app name such as `bitkit`.
- **Synonyms to AVOID**: Paykit App ID when a chosen name is meant
- **Related terms**: Paykit App, App record

### Current app
- **Definition**: An app listed in the identity's newest `identity.json` that the Pubky Switch hasn't revoked or recorded as departed (spec 3.5).
- **NOT**: Any app that once held a certificate.
- **Related terms**: Own app, `identity.json`, Revocation

### Own app
- **Definition**: A current app of the identity in question.
- **NOT**: An app of a counterparty.
- **Related terms**: Current app, Counterparty

### App certificate
- **Definition**: The identity-signed certificate of a Paykit App: its signing key, bundle key, scopes, switch permissions and validity (spec 3.2, wire 3.1).
- **NOT**: The App record, which the app signs itself.
- **Related terms**: Scope, Switch permission, Bundle key, App record

### App record
- **Definition**: An app's public, self-signed description: signing key, certificate, accepted message kinds and optional public endpoints (spec 4.2, wire 3.2). Published on the Homeserver at `/pub/paykit/v1/apps/<app_id>.json` only for an app with public Payment Endpoints.
- **NOT**: The App certificate, or the App Registry of v0.1.
- **Related terms**: App ID, App certificate, Payment Endpoint

### `identity.json`
- **Definition**: The identity-signed document at `/pub/paykit/v1/identity.json` naming the Pubky Switch, the admission policy, the current and removed apps and the current data epoch (spec 4.3, wire 3.3).
- **NOT**: An App record or the Paykit Profile.
- **Related terms**: Version, Generation, Data epoch, Current app

### Version
- **Definition**: The counter of `identity.json`; it increases on every update. Shared records, Private Payment Lists and the chain have versions of their own.
- **NOT**: The generation, or the `v` member of a document.
- **Related terms**: `identity.json`, Generation, Record version

### Generation
- **Definition**: The counter of the Pubky Switch named in `identity.json`; it increases when the identity moves to another switch and after a provider loss (spec 13.6).
- **NOT**: The version of `identity.json` or the data epoch.
- **Related terms**: `identity.json`, Chain, Provider move

### Scope
- **Definition**: A permission string in an App certificate, such as `pay`, `request`, `request:<label>`, `allowance`, `allowance_execute`, `contacts`, `manage_contacts` or `ext:<prefix>` (spec 3.3).
- **NOT**: A Switch permission, which goes with scopes in the certificate's `inbox` list.
- **Related terms**: App certificate, Switch permission, Labelled selling scope, Extension kind

### Switch permission
- **Definition**: A permission at the Pubky Switch, listed in a certificate's `inbox` list and given with scopes, such as `claims.read`, `claims.write`, `lists.read`, `lists.manage` or `conversation.add` (spec 3.3).
- **NOT**: A scope.
- **Related terms**: Scope, App certificate

### Labelled selling scope
- **Definition**: A scope `request:<label>` that lets a hosted selling app act as payee for its own sales only. A certificate holds `request` or labels, never both (spec 3.3).
- **NOT**: A display name; labels should be opaque, like `shop1`.
- **Related terms**: Scope, Payment Request, Activity record

### Extension kind
- **Definition**: A message kind `<prefix>.<name>` covered by the scope `ext:<prefix>`. Extension kinds carry no Paykit payment semantics (spec 3.4, wire 5.3).
- **NOT**: A Paykit payment message kind.
- **Related terms**: Scope, Message kind

## Keys

### Signing key
- **Definition**: A Paykit App's static Ed25519 key. It signs messages, requests to the Pubky Switch, operations, its App record and its receive key records.
- **NOT**: The identity key.
- **Related terms**: App ID, Bundle key, Receive key

### Bundle key
- **Definition**: An app's static X25519 key, named in its certificate, to which the signer seals its key bundles (spec 3.6).
- **NOT**: A receive key; only key bundles are sealed to it.
- **Related terms**: Key bundle, Receive key

### Receive key
- **Definition**: An app's current X25519 encryption key for messages, with a number `n` and an 8-byte key ID (kid). A fresh random key replaces it after the app reads its feed, about once a day by default (spec 3.7).
- **NOT**: The bundle key or the signing key.
- **Related terms**: Receive key record, kid, Envelope

### Receive key record
- **Definition**: The app-signed record of an app's current receive key, held by the Pubky Switch and returned by its directory (spec 4.4, wire 3.4).
- **NOT**: An App record.
- **Related terms**: Receive key, Directory

### kid
- **Definition**: The 8-byte key ID derived from a receive key, written `b64(8)`. It names one key in the whole account and routes an envelope's key wrap.
- **NOT**: An App ID.
- **Related terms**: Receive key, Envelope

### Shared Data Key
- **Definition**: Identity-wide keys, one per scope and data epoch, that encrypt data shared between own apps (spec 3.6).
- **NOT**: A receive key, or the single shared-state key of v0.1.
- **Related terms**: Data epoch, Key bundle, Sealed value

### Data epoch
- **Definition**: The counter of the Shared Data Key; it increases on each rotation.
- **NOT**: The generation or the version of `identity.json`.
- **Related terms**: Shared Data Key, `identity.json`

### Key bundle
- **Definition**: An app's scope keys for the data epochs it lacks, and the naming key, sealed to its bundle key by the signer (spec 3.6, wire 3.5).
- **Related terms**: Bundle key, Shared Data Key, Naming key

### Naming key
- **Definition**: An identity-wide key that blinds natural keys and obligation inputs, so the keys of records and claims don't name their counterparty (spec 3.6).
- **Related terms**: Natural key, Obligation, Shared record

### Sealed value
- **Definition**: A field of shared state encrypted under a Shared Data Key for its scope and data epoch (spec 14.4).
- **NOT**: An envelope.
- **Related terms**: Shared Data Key, Shared record

## Pubky Switch

### Pubky Switch
- **Definition**: The service that receives an identity's messages and hosts its coordination primitives: inbox and feed, allowlist and deny list, claims, shared records, app records, key bundles, receive key records, push hooks, directory, revocation, freeze, export and recovery. One service hosts many identities; each identity uses one at a time (spec 6). Short form: switch.
- **NOT**: A Homeserver, a federation, or an ordering server.
- **Synonyms to AVOID**: Inbox Service, relay, mailbox server
- **Related terms**: Inbox, Feed, Claim, Shared record, Chain

### Inbox
- **Definition**: The mailbox part of the Pubky Switch, where senders' messages land and the identity's apps read them.
- **NOT**: Homeserver files.
- **Related terms**: Pubky Switch, Inbox entry, Feed

### Inbox entry
- **Definition**: One encrypted message stored for one or more target apps.
- **Related terms**: Inbox, Envelope, `seq`

### `seq`
- **Definition**: The position of an item at the switch within one generation. Inbox entries and chain items share one sequence.
- **Related terms**: Feed, Cursor, Chain

### Feed
- **Definition**: The ordered stream an own app reads: the items after its cursor that it may read, and chain stubs for the other chain items.
- **Related terms**: Cursor, Chain stub, Inbox entry

### Cursor
- **Definition**: An app's position in the feed, written `<generation>:<seq>`.
- **Related terms**: Feed, `seq`

### Directory
- **Definition**: The Pubky Switch's answer naming an identity's current apps for senders: their app records and receive key records (spec 8.1).
- **Related terms**: Receive key record, App record

### Operation
- **Definition**: A change sent in a batch: a claim entry, a shared record write, an allowlist or deny list change, a cursor move, an app record, a receive key registration or a key bundle. All are signed by their writer except cursor moves, which the request signature covers (spec 14.5, wire 7.1).
- **Related terms**: Batch, Condition, Transition proof

### Condition
- **Definition**: A requirement attached to an operation, checked atomically with it, such as a record version, a claim step or `server_time_in` (wire 7.3).
- **Related terms**: Operation, Batch

### Batch
- **Definition**: Up to 8 operations and their conditions, applied all or nothing (wire 7.4).
- **Related terms**: Operation, Condition

### Transition proof
- **Definition**: The service-signed result of one operation, with its place in the chain (wire 7.5).
- **Related terms**: Chain, Signed head, Operation

### Chain
- **Definition**: The hash chain over operations, `identity.json` deliveries and service transitions, one per generation (spec 8.4). Inbox entries and cursor moves are not in it.
- **Related terms**: Chain stub, Signed head, Transition proof, Generation

### Chain stub
- **Definition**: A chain item's `seq` and hashes, without its content.
- **Related terms**: Chain, Feed

### Signed head
- **Definition**: The service-signed chain hash at a given `seq` (wire 7.7).
- **Related terms**: Chain, Transition proof

### Service key
- **Definition**: The key a Pubky Switch signs its responses, transition proofs and signed heads with. Clients pin it and refuse answers they can't check (spec 6.1).
- **Related terms**: Transition proof, Signed head, `identity.json`

## Admission

### Allowlist
- **Definition**: The contacts and conversation entries at an identity's Pubky Switch that admit senders. v0.2 has no unsolicited first contact by Pubky ID alone (spec 1, 7).
- **NOT**: A social graph.
- **Related terms**: Contact, Conversation entry, Deny list

### Deny list
- **Definition**: The identities an identity has blocked at its Pubky Switch.
- **Related terms**: Allowlist

### Contact
- **Definition**: An allowlist entry for a counterparty the user keeps.
- **NOT**: A v0.1 Public Contact Marker.
- **Related terms**: Allowlist, Conversation entry

### Conversation entry
- **Definition**: An allowlist entry an app adds for a counterparty it writes to or invites, so that the two can keep talking until the user blocks the counterparty (spec 7).
- **Related terms**: Invite, Allowlist

### Invite
- **Definition**: A conversation entry added before any message, so that the counterparty can write first, for example a shop at checkout (spec 7).
- **Related terms**: Conversation entry, Invite code

### Invite code
- **Definition**: A random code an app registers at its own switch and shows, for example as a QR code, so that anyone who has it can write once and start a conversation (spec 7). Shown as `pubky://<id>?paykit=invite&code=<code>`.
- **Related terms**: Invite, Connect request

### Connect request
- **Definition**: A content-free request from a stranger to be invited, which an identity may allow (spec 7).
- **Related terms**: Invite, Allowlist

## Messages

### Envelope
- **Definition**: The binary sealed form of a message (`PKT1`, suite 1): one HPKE key wrap per recipient app, to its receive key, and the content under XChaCha20-Poly1305 with a key commitment. Padded to 1, 4, 16 or 64 KiB; at most 64 KiB (spec 5.1, wire 4).
- **NOT**: An Encrypted Link, a Noise message, or a Homeserver file.
- **Synonyms to AVOID**: Private Payment Envelope, noise message
- **Related terms**: Receive key, kid, Inbox entry, Message kind

### Message kind
- **Definition**: The `kind` of a Paykit message, such as `payment_request`, `payment_proof`, `private_payment_list` or `allowance_proposal`, which fixes the scopes a sender and recipient need (spec 3.4, wire 5.3).
- **NOT**: The message body or the Rust type that represents it.
- **Synonyms to AVOID**: Private Message Kind, `paykit.`-prefixed kind names
- **Related terms**: Scope, Extension kind, Envelope

### Message ID
- **Definition**: The `msg_id` of a message, a lower-case UUID v4 (wire 1.8). Receivers keep processed message IDs to drop replays.
- **NOT**: A Payment Request's hash, an Event ID of v0.1, or a Payment Reference.
- **Synonyms to AVOID**: Event ID
- **Related terms**: Message kind, Resend

### Resend
- **Definition**: The re-sending of messages sent in the last 30 days, asked for with `resend_request` (spec 12).
- **Related terms**: Message ID, Delivery

### Stored message
- **Definition**: A message kept, with its exact bytes, inside a shared record value, so own apps can check it themselves (wire 6.2).
- **Related terms**: Activity record, Shared record

## Coordination

### Obligation
- **Definition**: Something that must happen at most once, such as paying one request in one Billing Period. Its obligation ID is the claim key (spec 10.2, 14.3).
- **Related terms**: Claim, Attempt, Billing Period

### Claim
- **Definition**: The server-checked state of one obligation at the Pubky Switch, written with claim entries such as `attempt` and `done` (spec 10.2, wire 7.8).
- **NOT**: A lock or a lease.
- **Synonyms to AVOID**: lease, lock
- **Related terms**: Obligation, Attempt, Step

### Attempt
- **Definition**: One try at carrying out an obligation, such as one payment, with its own outcome and an `attempt_id`, which the payment method uses as its idempotency key.
- **Related terms**: Claim, Obligation, Paying order

### Step
- **Definition**: The position of an entry in one obligation's log.
- **Related terms**: Claim

### Shared record
- **Definition**: A small piece of encrypted shared state at the Pubky Switch, updated by compare-and-swap on its record version (spec 10.3).
- **NOT**: The v0.1 shared-state blob.
- **Synonyms to AVOID**: blob, shared state file
- **Related terms**: Collection, Policy, Record version, Sealed value

### Collection
- **Definition**: A set of shared records that can be listed together, such as `payer_activity`, `payee_activity`, `payment_lists`, `allowances` and `allowance_ledgers` (spec 8.3).
- **Related terms**: Shared record, Policy

### Policy
- **Definition**: The scopes a shared record requires, `read_scope` and `write_scopes`, fixed at creation. The admission settings in `identity.json` are also called `policy`.
- **Related terms**: Shared record, Scope

### Record version
- **Definition**: `{ "generation", "version" }`, the generation and `seq` of the write that produced a shared record (wire 1.8).
- **Related terms**: Shared record, Condition

### Natural key
- **Definition**: The input that names what a shared record is about, such as a counterparty and request, before it is blinded with the naming key into the record key (spec 14.3).
- **Related terms**: Naming key, Shared record

### Activity record
- **Definition**: A shared record in `payer_activity` or `payee_activity` holding the messages that make up one request's, Billing Period's or payment's state, one per role (spec 8.3, wire 6.3).
- **Related terms**: Collection, Stored message, Payment Request

### Paying order
- **Definition**: The steps every payment follows: write the `attempt`, pay, write `done`, update the record, send the proof (spec 10.4).
- **Related terms**: Attempt, Claim, Payment Proof

## Payments

### Payment List
- **Definition**: A payee-published or payee-shared collection of Payment Endpoints: public in an App record, or private inside a Private Payment List.
- **NOT**: The result of payer-side processing of compatible endpoints.
- **Synonyms to AVOID**: Payment Method List
- **Related terms**: Payment Endpoint, Private Payment List, Payee

### Payment Endpoint
- **Definition**: A whole payee-owned item in a Payment List, consisting of a Payment Endpoint Identifier and a Payment Endpoint Payload.
- **NOT**: Only the address, invoice, IBAN, offer, or credential payload.
- **Synonyms to AVOID**: Payment Option
- **Related terms**: Payment Endpoint Identifier, Payment Endpoint Payload, Payment List

### Payment Endpoint Identifier
- **Definition**: The canonical machine-readable identifier for a payment endpoint type, such as `btc-lightning-bolt12` or `eur-sepa-iban`: ASCII letters and digits, `-`, `_` and `.`, at most 64 characters, never `.`, `..` or `private` (wire 1.9).
- **NOT**: The full Payment Endpoint or its payload.
- **Synonyms to AVOID**: method id, payment method id
- **Related terms**: Payment Endpoint, Payment Method, Asset, Rail

### Payment Endpoint Payload
- **Definition**: The opaque, non-empty UTF-8 data part of a Payment Endpoint containing the actual receiving handle or details. Paykit never interprets it.
- **NOT**: The whole Payment Endpoint.
- **Synonyms to AVOID**: endpoint data in domain docs
- **Related terms**: Payment Endpoint, Payment Endpoint Identifier

### Payment Method
- **Definition**: A higher-level human/domain concept for how value can be transferred, mapping to one or more Payment Endpoint Identifiers.
- **Synonyms to AVOID**: Payment Option
- **Related terms**: Payment Endpoint Identifier, Rail

### Asset
- **Definition**: The unit of value being transferred, such as `btc`, `eur`, `usd`, or `usdt`; compared exactly and case-sensitively (wire 1.7).
- **NOT**: The settlement rail or endpoint format.
- **Related terms**: Payment Amount, Payment Endpoint Identifier

### Payment Amount
- **Definition**: An amount of value, `{ "value", "asset" }`, with `value` a decimal string in whole units of its asset, compared and added exactly (wire 1.7).
- **NOT**: A float, or an amount in the asset's smallest unit.
- **Related terms**: Asset, Payment Request, Allowance

### Rail
- **Definition**: The settlement system carrying the asset, such as Lightning, Bitcoin or SEPA. Rails are outside Paykit: the SDK pays through a narrow rail interface (pay an endpoint with an idempotency key; read the verified payment time).
- **NOT**: Part of the Paykit Protocol.
- **Related terms**: Payment Method, Attempt, Payment Proof

### Payer
- **Definition**: The party sending value in a payment flow; its apps hold `pay`.
- **Related terms**: Payee, Counterparty

### Payee
- **Definition**: The party receiving value in a payment flow and publishing or sharing Payment Endpoints; its apps hold `request` or a labelled selling scope.
- **Related terms**: Payer, Payment List

### Counterparty
- **Definition**: The other identity in a payment interaction or conversation.
- **Related terms**: Payer, Payee, Conversation entry

### Payment Request
- **Definition**: A `payment_request` message where a payee asks a payer for payment, one-time or recurring, answered by acceptance, rejection or cancellation (spec 11.1, wire 5.5).
- **NOT**: An Allowance, a Payment List, or payment execution.
- **Related terms**: Recurring Payment Request, Payment Proof, Payment Receipt, Activity record

### Recurring Payment Request
- **Definition**: A Payment Request with a `recurrence`, paid once per Billing Period after acceptance (spec 11.2).
- **NOT**: A payee-controlled pull authorization.
- **Synonyms to AVOID**: pull subscription
- **Related terms**: Payment Request, Billing Period, Recurrence, Period details

### Subscription
- **Definition**: Product shorthand for an accepted Recurring Payment Request.
- **NOT**: An Allowance or a separate protocol family.
- **Related terms**: Recurring Payment Request

### Recurrence
- **Definition**: The schedule object of a Recurring Payment Request: `every`, `unit`, `starts_at`, `anchor` and optional `ends_at` (wire 5.4).
- **NOT**: A cron expression or a local scheduler.
- **Related terms**: Billing Period

### Billing Period
- **Definition**: One period of a recurring request, `[boundary(k), boundary(k+1))`, computed from its Recurrence as spec 11.2 and wire 5.4 define. Messages name it by `{starts_at, ends_at}`.
- **NOT**: A Payment Request or a recurrence rule.
- **Synonyms to AVOID**: billing cycle
- **Related terms**: Recurrence, Obligation, Period details

### Period details
- **Definition**: A `payment_period_details` message from the payee with one Billing Period's conversion rates, its destination, or both, valid until `expires_at` (spec 11.2, wire 5.9).
- **Synonyms to AVOID**: Payment Conversion Quote
- **Related terms**: Billing Period, Conversion Rate

### Conversion Rate
- **Definition**: An exact decimal multiplier expressing payment Asset units per one requested Asset unit (`specs/payment-conversion.md`).
- **NOT**: A market-rate feed or a payment authorization.
- **Related terms**: Payment Amount, Period details

### Payment deadline
- **Definition**: An inclusive deadline for actual payment, absolute for a one-time request or relative to a recurring Billing Period's start (wire 5.4).
- **NOT**: The proposal expiry or the recurrence end.
- **Related terms**: Payment Request, Claim

### Payment Proof
- **Definition**: A `payment_proof` message: method-specific evidence for one concrete payment, referencing the request it pays, if any (wire 5.10).
- **NOT**: A Payment Receipt, or proof that Paykit itself validates.
- **Synonyms to AVOID**: payment receipt when the method-specific proof is meant
- **Related terms**: Paying order, Payment Receipt, Rail

### Payment Receipt
- **Definition**: A `payment_receipt` message from the payee confirming a payment it received, referencing its proof (wire 5.11). It settles the payer's claim by evidence.
- **NOT**: A v0.1 Encrypted Receipt stored on a Homeserver, or Receipt Access.
- **Related terms**: Payment Proof, Claim

### Private Payment List
- **Definition**: A `private_payment_list` message carrying one Paykit App's Payment List for one counterparty, with a version, an expiry and optional single-use endpoints. The latest valid list from an app supersedes its older ones (spec 11.4, wire 5.13).
- **NOT**: A Payment Request or a Payment Proof.
- **Synonyms to AVOID**: Private Payment Method List, Private Payment Envelope
- **Related terms**: Payment List, Single-use endpoint

### Single-use endpoint
- **Definition**: A Payment Endpoint in a Private Payment List that may be paid once; the payer reserves it in the `payment_lists` record in the attempt's batch (spec 11.4).
- **Related terms**: Private Payment List, Attempt

## Allowances

### Allowance
- **Definition**: Prior authority, built entirely in apps on claims and shared records, for one executor app to pay a counterparty's qualifying requests automatically within accepted terms (spec 11.3).
- **NOT**: A balance, a transfer of custody, a Payment Request, a Subscription, or a bearer credential. The Pubky Switch has no Allowance objects.
- **Synonyms to AVOID**: subscription, standing order, spending account
- **Related terms**: Allowance terms, Executor, Executor grant, Allowance ledger

### Allowance terms
- **Definition**: The immutable maximum authority of one Allowance: its asset, limits, time bounds and allowed Payment Endpoint Identifiers (wire 5.15).
- **Related terms**: Allowance, Executor grant

### Executor
- **Definition**: The one app that an Allowance record names, with the app's own signed grant, to pay automatically under that Allowance. It must hold `allowance_execute`.
- **Related terms**: Executor grant, Consent list, Allowance

### Executor grant
- **Definition**: The executor's signed statement in the Allowance record, over the identity, the counterparty, the `allowance_id`, the terms hash and its generation (spec 11.3, wire 3.6).
- **Related terms**: Executor, Allowance terms

### Consent list
- **Definition**: The Allowances an executor keeps durably in its own state as consented to; it pays under an Allowance only while it is on the list.
- **Related terms**: Executor, `executor_notice`

### Allowance ledger
- **Definition**: One shared record per Allowance in `allowance_ledgers`: an entry per attempt made under it, and running totals (spec 11.3, wire 6.5).
- **Related terms**: Allowance, Attempt

## Lifecycle

### Enrollment
- **Definition**: The signer's setup of an identity at a Pubky Switch (wire 3.8).
- **Related terms**: Paykit Signer, Pubky Switch, `identity.json`

### Revocation
- **Definition**: The signer's removal of an app, with `distrust_from`, a data epoch rotation, and its App ID kept for good (spec 13.2).
- **Related terms**: Current app, Data epoch

### Provider move
- **Definition**: An identity's cooperative move to another Pubky Switch: freeze, export transfer, import, activation and publish (spec 13.5, wire 3.7, 3.12, 11).
- **Related terms**: Freeze, Generation, Provider loss

### Freeze
- **Definition**: The signed stop of an account at its Pubky Switch during a provider move; the switch then answers `410 moved` (wire 3.7).
- **Related terms**: Provider move

### Provider loss
- **Definition**: Recovery when the identity's Pubky Switch is unreachable or hostile: a new generation at another switch (spec 13.6).
- **Related terms**: Provider move, Generation

### Signer hand-over
- **Definition**: The transfer of an identity from one signer to another (spec 13.8, wire 3.13).
- **Related terms**: Signer, Paykit Signer

### Push relay
- **Definition**: A vendor-run service that turns the Pubky Switch's signed, content-free webhooks into APNs or FCM pushes (spec 9).
- **Related terms**: Handle, Push key

### Handle
- **Definition**: An opaque ID the push relay maps to one device.
- **Related terms**: Push relay

### Push key
- **Definition**: The signing key the switch mints for one app's push registration; it signs that app's webhooks and appears in no public document (spec 9).
- **Related terms**: Push relay

### Paykit Profile
- **Definition**: Optional public display metadata of an identity at `/pub/paykit/profile.json`, with v0.1 semantics.
- **NOT**: A Pubky app profile or a Payment Endpoint.
- **Related terms**: Identity

## Forbidden Lexicon

These terms must not be used for new Paykit v0.2 names:

- **Encrypted Link**, **Encrypted Link Handshake**, **Linked Peer**, **Noise link**, **relinking** -> v0.2 has none; use **Envelope**, **Inbox** and **Counterparty**.
- **pubky-noise**, **Noise key** -> use **Receive key** or **Bundle key**.
- **Paykit App Registry**, **Noise Key Authorization**, **Public Contact Marker** -> removed in v0.2; use **`identity.json`**, **App record** and **Contact**.
- **Paykit Blob**, **shared-state blob**, **lease**, **lock** -> use **Shared record** and **Claim**.
- **Inbox Service** -> use **Pubky Switch**.
- **Delivery Confirmation** -> not mandatory in v0.2; use **Payment Receipt** for a payee's confirmation of payment.
- **Receipt Access**, **Receipt Location**, **Encrypted Receipt**, **Receipt Decryption Key** -> removed in v0.2; use **Payment Receipt**.
- **Event ID**, **Event Message**, **Latest-State Message**, **Private Application Message** -> use **Message ID**, **Message kind**, and the semantics each kind's spec section gives.
- **Payment Conversion Quote** -> use **Period details**.
- **Payment Method List** -> use **Payment List**.
- **Payment Option** -> use **Payment Endpoint** or **Payment Method**, depending on meaning.
- **MethodId** -> use **Payment Endpoint Identifier**.
- **EndpointData** -> use **Payment Endpoint Payload**.
- **Private Payment Envelope** / **PrivatePaymentsPayload** -> use **Private Payment List**.
- **SubscriptionAgreement** / **subscription_id** -> use **Recurring Payment Request** and its request hash or message ID.
- **pull subscription** -> use **Recurring Payment Request**, and **Allowance** for prior authority to pay it automatically.
- **Paykit App ID** for a chosen app name such as `bitkit` -> an **App ID** is derived from the signing key.

## Component Model

Crates:
- Paykit Library (`paykit-lib`)
- Paykit SDK (`paykit-sdk`)
- Paykit Signer (`paykit-signer`)

Services:
- Pubky Homeserver
- Pubky Switch
- Push relay

Identity and apps:
- Identity, Signer, Paykit App, App ID, Current app, Own app
- App certificate, App record, `identity.json`, Version, Generation
- Scope, Switch permission, Labelled selling scope, Extension kind

Keys:
- Signing key, Bundle key, Receive key, Receive key record, kid
- Shared Data Key, Data epoch, Key bundle, Naming key, Sealed value

Switch and coordination:
- Inbox, Inbox entry, `seq`, Feed, Cursor, Directory
- Operation, Condition, Batch, Transition proof, Chain, Chain stub, Signed head, Service key
- Allowlist, Deny list, Contact, Conversation entry, Invite, Invite code, Connect request
- Obligation, Claim, Attempt, Step, Shared record, Collection, Policy, Record version, Natural key, Activity record, Paying order

Messages and payments:
- Envelope, Message kind, Message ID, Resend, Stored message
- Payment List, Payment Endpoint, Payment Endpoint Identifier, Payment Endpoint Payload, Payment Method, Asset, Payment Amount, Rail
- Payer, Payee, Counterparty
- Payment Request, Recurring Payment Request, Subscription, Recurrence, Billing Period, Period details, Conversion Rate, Payment deadline
- Payment Proof, Payment Receipt, Private Payment List, Single-use endpoint
- Allowance, Allowance terms, Executor, Executor grant, Consent list, Allowance ledger

Lifecycle:
- Enrollment, Revocation, Provider move, Freeze, Provider loss, Signer hand-over
- Push relay, Handle, Push key, Paykit Profile
