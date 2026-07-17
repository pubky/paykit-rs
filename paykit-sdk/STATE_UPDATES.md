# Paykit SDK State Updates

This document maps supported `paykit-sdk` workflows across three state
boundaries:

1. **SDK local state** — durable records behind `StorageAdapter`.
2. **Live process state** — the `PubkySessionProvider`, payment adapter, temporary
   values, and operation leases.
3. **External state** — public or private files on each party's Pubky homeserver.

For payment flows, **Payer** sends value and **Payee** receives value. For private
messaging, either party can be the message sender, receiver, Receipt issuer, or
Encrypted Link initiator.

## State model

```mermaid
flowchart LR
    subgraph Local[One app-owned Paykit SDK runtime]
        App[Application]
        SDK[PaykitSdk]
        Provider[PubkySessionProvider<br/>live session and optional local secret]
        Adapter[PaymentAdapter<br/>receiving details, reservations, targets]
        Store[(StorageAdapter<br/>atomic durable SDK state)]
    end

    subgraph LocalHS[Local identity homeserver]
        Public[Public receiver, endpoints,<br/>profile, blobs, contacts]
        Private[Derived encrypted message slots]
        Receipts[Encrypted Receipts]
        Recovery[Pairwise recovery markers]
    end

    subgraph RemoteHS[Counterparty homeserver]
        RemotePublic[Counterparty public state]
        RemotePrivate[Counterparty encrypted slots]
        RemoteReceipts[Counterparty Encrypted Receipts]
        RemoteRecovery[Counterparty recovery markers]
    end

    App --> SDK
    SDK <--> Provider
    SDK <--> Adapter
    SDK <--> Store
    SDK <--> Public
    SDK <--> Private
    SDK --> Receipts
    SDK <--> Recovery
    SDK -. public reads .-> RemotePublic
    SDK -. private intake .-> RemotePrivate
    SDK -. receipt retrieval .-> RemoteReceipts
    SDK -. recovery observation .-> RemoteRecovery
```

The durable `StorageState` contains identity state, Linked Peer records, Contact
Records, public endpoint publication records, Payment Endpoint Reservations,
Encrypted Link snapshots, peer-operation leases, outbound private messages,
private stream items, Event ID dedupe records, Receipt Access records, decrypted
Receipts, and receipt issuance records. Payment Request views and the current
Private Payment List are derived from those stored streams rather than kept as
separate mutable records.

Many Pubky-backed workflows begin by refreshing `IdentityState` from the live
session provider. The remaining diagrams omit that repeated preliminary update
unless identity lifecycle is the workflow being explained.

### External path categories

| State | Default receiver-scoped path |
| --- | --- |
| Receiver Marker | `/pub/paykit/v0/<receiver_path>/receiver.json` |
| Public Payment Endpoint | `/pub/paykit/v0/<receiver_path>/endpoints/<identifier>` |
| Paykit Profile | `/pub/paykit/v0/<receiver_path>/profile.json` |
| Paykit Blob | `/pub/paykit/v0/<receiver_path>/blobs/<name>` |
| Public Contact Marker | `/pub/paykit/v0/<receiver_path>/contacts/<contact_key>/<contact_receiver_path>.json` |
| Encrypted Link stream | `/pub/paykit/v0/private/<receiver_path>/messages/<pair_hash>/<slot>` |
| Encrypted Receipt | `/pub/paykit/v0/private/<issuer_receiver_path>/receipts/<receipt_id>` |
| Encrypted Link Recovery Marker | `/pub/paykit/v0/private/<receiver_path>/encrypted-link-recovery/<pair_hash>` |

Profile, blob, and contact paths move under
`/pub/<profile_namespace>/<receiver_path>/...` when a non-default profile
namespace is configured. Private paths and public Payment Endpoint paths do not.

## Session bootstrap, initialize, sign-out, and backup

Session bootstrap helpers create or validate Pubky sessions. They do not write
SDK storage until the application installs the resulting access in its provider
and calls `initialize` or another identity-refreshing SDK method.

```mermaid
sequenceDiagram
    participant App
    participant Bootstrap as PubkySessionBootstrap
    participant Pubky as Pubky auth, relay, and homeserver
    participant Provider as PubkySessionProvider
    participant SDK as PaykitSdk
    participant Store as SDK local storage
    participant Backup as Caller-owned backup storage

    alt Sign up, sign in, import, or external auth approval
        App->>Bootstrap: bootstrap session with exact required capabilities
        Bootstrap->>Pubky: create, resume, approve, or import Pubky session
        Bootstrap-->>App: validated session access and identity capability
        App->>Provider: install live session access
    end

    opt Approve auth with an application-defined companion claim
        App->>Bootstrap: approve_auth_with_companion_claim
        Bootstrap->>Pubky: sign and encrypt claim, deliver through relay, then approve Pubky Auth
        Note over Bootstrap,Pubky: This changes auth and relay state, not Paykit homeserver files or SDK storage.
    end

    App->>SDK: initialize
    SDK->>Provider: load session access
    SDK->>Store: atomically save IdentityState
    Note over SDK,Store: A new or changed public key clears prior identity-scoped SDK state first.

    opt Export before destructive sign-out
        App->>SDK: export_backup_state
        SDK->>Store: atomically snapshot logical SDK state
        SDK-->>App: sensitive SdkBackupState
        App->>Backup: encrypt and persist outside the SDK
    end

    App->>SDK: sign_out
    SDK->>Provider: clear live session access
    SDK->>Store: atomically clear identity-scoped state and save SignedOut identity state
    Note over SDK,Pubky: Sign-out does not delete Paykit homeserver files.

    opt Restore for the same identity and receiver path
        App->>Provider: install matching capable session
        App->>Backup: load and decrypt backup
        App->>SDK: restore_backup_state
        SDK->>Provider: validate identity, capabilities, and local secret availability
        SDK->>Store: atomically replace all logical SDK state after validation
    end
```

Failure boundaries:

- If provider clearing fails, `sign_out` leaves local SDK state intact. If
  provider clearing succeeds and local clearing fails, the SDK has no live
  session but local state may remain; retry is required.
- Losing the external caller-managed backup loses private stream history,
  Encrypted Link counters, Receipt Decryption Keys, queues, contacts, and local
  payment history. Homeserver data alone cannot reconstruct them safely.
- Restore performs no homeserver writes. Unsafe or incomplete restored link
  checkpoints mark affected peers recovery-required.
- Backup export excludes active peer-operation leases and their transient lease
  state. Restore clears leases, preserves the live lease counter, and advances
  message/stream counters as needed during reconciliation.
- Exported session secrets, auth URLs, companion claims, and SDK backups are
  secret material; the SDK does not provide backup transport or at-rest
  encryption.

## Profiles, blobs, public Pubky reads, and contacts

```mermaid
sequenceDiagram
    participant Local as Local application
    participant SDK
    participant Store as SDK local storage
    participant LHS as Local homeserver
    participant RHS as Contact homeserver

    opt Publish local Paykit Profile or blob
        Local->>SDK: publish_paykit_profile or publish_paykit_blob
        SDK->>LHS: PUT configured profile.json or blobs/<name>
        SDK-->>Local: transient publication record
        Note over SDK,Store: Profile and blob publication records are returned, not persisted by SDK storage.
    end

    opt Read public profile, blob, Pubky Profile, or follows
        Local->>SDK: fetch or resolve public data
        SDK->>RHS: GET or LIST public resources
        SDK-->>Local: transient parsed result
    end

    Local->>SDK: save_contact
    SDK->>Store: upsert local Contact Record

    opt Refresh cached Paykit Profile
        Local->>SDK: refresh_contact_paykit_profile
        SDK->>RHS: GET contact profile.json
        SDK->>Store: update cached profile and refresh time
    end

    opt Explicitly publish contact graph marker
        Local->>SDK: publish_public_contact
        SDK->>Store: mark PendingPublication
        SDK->>LHS: PUT contacts/<contact_key>/<receiver_path>.json
        SDK->>Store: mark Published or Failed
    end

    opt Remove public contact marker
        Local->>SDK: remove_public_contact
        SDK->>Store: mark PendingRemoval when tracked
        SDK->>LHS: DELETE contact marker
        SDK->>Store: mark Removed or Failed
    end

    opt Delete local profile or blob
        Local->>SDK: delete helper
        SDK->>LHS: DELETE configured file
    end
```

- Contact Records are local by default. Public contact markers are opt-in and
  expose part of the contact graph.
- `sync_public_contact_markers` retries locally pending publication/removal
  records. A contact cannot be deleted while its marker may still exist.
- `resolve_contact_profile` prefers the configured Paykit Profile and can fall
  back to `/pub/pubky.app/profile.json`. These ordinary reads do not mutate local
  state unless the explicit contact refresh helper is used.
- Profile/blob/contact paths use `/pub/paykit/v0/<receiver_path>/...` by default,
  or `/pub/<profile_namespace>/<receiver_path>/...` when configured.

## Receiver discovery and public Payment Endpoint synchronization

```mermaid
sequenceDiagram
    participant PayeeApp as Payee application
    participant PayeeSDK as Payee SDK
    participant Adapter as Payee PaymentAdapter
    participant Store as Payee local storage
    participant PHS as Payee homeserver
    participant PayerSDK as Payer SDK

    opt Publish discoverability without endpoints
        PayeeApp->>PayeeSDK: publish_paykit_receiver_marker
        PayeeSDK->>PHS: PUT receiver.json
    end

    PayeeApp->>PayeeSDK: sync_public_endpoints
    PayeeSDK->>Adapter: current_receiving_details Public

    loop Each desired endpoint
        PayeeSDK->>Store: save PendingPublication record
        PayeeSDK->>PHS: PUT endpoints/<identifier>
        PayeeSDK->>Store: save Published or Failed record
    end

    loop Each stale endpoint in configured management scope
        PayeeSDK->>Store: save PendingRemoval record
        PayeeSDK->>PHS: DELETE endpoints/<identifier>
        PayeeSDK->>Store: save Removed or Failed record
    end

    PayerSDK->>PHS: LIST receiver paths or endpoint directory, then GET files
    PayerSDK-->>PayeeApp: no payee state change and transient payer discovery data

    opt Stop marker-only discovery
        PayeeApp->>PayeeSDK: remove_paykit_receiver_marker
        PayeeSDK->>PHS: DELETE receiver.json
    end
```

- `ManagedOnly` removes only endpoints tracked by this SDK. `FullPaykitNamespace`
  first reads the local identity's remote Payment List and reconciles the whole
  receiver namespace.
- Local publication intent is committed before each remote write/delete so a
  failure is visible and retryable. Endpoint operations are per-file, not one
  atomic batch.
- Receiver marker publication/removal has no corresponding durable SDK
  publication record.

## Encrypted Link setup and local peer policy

```mermaid
sequenceDiagram
    participant AApp as Party A application
    participant ASDK as Party A SDK
    participant AStore as Party A local storage
    participant AHS as Party A homeserver
    participant BHS as Party B homeserver
    participant BSDK as Party B SDK
    participant BStore as Party B local storage

    AApp->>ASDK: ensure_link_with_peer or initiate_link_with_peer
    ASDK->>AStore: claim per-peer operation lease
    ASDK->>AStore: save Linked Peer = Linking and handshake snapshot
    ASDK->>AHS: write derived handshake slots when required
    ASDK->>BHS: read responder slots when required

    BSDK->>BStore: save responder Linking state and handshake snapshot
    BSDK->>BHS: write derived handshake slots when required
    BSDK->>AHS: read initiator slots when required

    loop advance_link_handshake
        ASDK->>AStore: restore current snapshot under lease
        ASDK->>AHS: read or write next handshake data
        ASDK->>AStore: atomically save next generation snapshot and peer state
    end

    ASDK->>AStore: save active link snapshot and Linked state
    BSDK->>BStore: save active link snapshot and Linked state

    opt Local block
        AApp->>ASDK: block_peer
        ASDK->>AStore: set Blocked and clear saved link or handshake snapshot
        Note over ASDK,AHS: Blocking is local policy and does not modify either homeserver.
    end

    opt Local unblock
        AApp->>ASDK: unblock_peer
        ASDK->>AStore: set NotLinked and keep snapshots cleared
    end
```

- Per-peer storage-backed leases serialize link, send, receive, recovery, block,
  and unblock operations across workers.
- Snapshot generation checks prevent stale workers from replacing newer state.
- Missing, corrupt, mismatched, or unsafe snapshots transition the peer to
  `RecoveryRequired`; private automation then pauses.
- Unblocking does not restore the old link. A fresh handshake is required.

## Durable private outbound and intake pipeline

Every Private Payment List, Payment Request lifecycle event, Payment Proof, and
Receipt Access message uses this pipeline.

```mermaid
sequenceDiagram
    participant SenderApp as Sending application
    participant SenderSDK as Sender SDK
    participant SenderStore as Sender local storage
    participant SHS as Sender homeserver
    participant ReceiverSDK as Receiver SDK
    participant ReceiverStore as Receiver local storage

    SenderApp->>SenderSDK: enqueue typed Private Application Message
    SenderSDK->>SenderStore: insert exact JSON as Pending outbound record
    Note over SenderSDK,SenderStore: Event Messages remain FIFO and older unsent Private Payment Lists may become Superseded.

    SenderApp->>SenderSDK: process_outbound_private_messages
    SenderSDK->>SenderStore: claim queue head as Sending under peer lease
    SenderSDK->>SHS: write encrypted counter slot
    SenderSDK->>SenderStore: atomically mark Sent and save advanced link snapshot

    ReceiverSDK->>SHS: read and decrypt available private slots
    ReceiverSDK->>ReceiverStore: one atomic transaction
    Note over ReceiverSDK,ReceiverStore: Insert raw stream items, classify payloads, index Receipt Access, dedupe Event IDs, and save the advanced link snapshot.

    alt Retryable send failure
        SenderSDK->>SenderStore: mark Failed and retain old link checkpoint for retry
    else Non-retryable link failure
        SenderSDK->>SenderStore: mark outbound RecoveryRequired and peer RecoveryRequired
    end
```

Important guarantees:

- `Sent` means the encrypted write completed and the sender's new checkpoint was
  committed. It is not counterparty acknowledgement.
- If a process may have written a message but crashed before the final local
  transaction, the stale `Sending` queue head is retried before later messages.
- Private receive commits raw payloads, derived indexes, dedupe records, and the
  advanced read checkpoint atomically. A storage adapter that cannot provide
  that transaction must reject the operation.
- Duplicate Event IDs with identical payloads are tracked as duplicates;
  conflicting payload reuse is retained and excluded from trusted derived views.
- Unknown or malformed private messages remain in the raw stream and do not
  replace the latest valid Private Payment List.

## Private Payment Lists, reservations, and payment resolution

```mermaid
sequenceDiagram
    participant PayeeApp as Payee application
    participant PayeeAdapter as Payee PaymentAdapter
    participant PayeeSDK as Payee SDK
    participant PayeeStore as Payee local storage
    participant PHS as Payee homeserver
    participant PayerSDK as Payer SDK
    participant PayerStore as Payer local storage
    participant PayerAdapter as Payer PaymentAdapter

    alt Current receiving details
        PayeeSDK->>PayeeAdapter: current_receiving_details Private
    else Reservation-backed details
        PayeeApp->>PayeeAdapter: reserve receiving details outside or through integration
        PayeeApp->>PayeeSDK: enqueue list with reservations
        PayeeSDK->>PayeeStore: atomically insert outbound list and reservation records
    end

    PayeeSDK->>PayeeStore: queue complete Private Payment List
    PayeeSDK->>PHS: send through durable private outbound pipeline
    PayerSDK->>PHS: receive through durable private intake pipeline
    PayerSDK->>PayerStore: latest valid list becomes derived current view

    PayerApp->>PayerSDK: resolve_contact_payment
    PayerSDK->>PayerStore: read cached private list first
    opt Fresh private state needed
        PayerSDK->>PHS: receive current private stream
        PayerSDK->>PayerStore: atomically update stream, list view, and checkpoint
    end
    opt Public fallback requested
        PayerSDK->>PHS: LIST and GET public endpoints
    end
    PayerSDK->>PayerAdapter: select endpoints and build PaymentTarget values
    PayerSDK-->>PayerApp: ordered payable endpoints and private-state status

    opt List superseded, invalid, expired, or terminal
        PayeeSDK->>PayeeAdapter: cancel eligible Payment Endpoint Reservations
        PayeeSDK->>PayeeStore: remove successfully cancelled reservation records
    end
```

- Every private update is a complete list. Clearing is an empty latest-state list,
  not deletion of old encrypted slots.
- Reservation records tie adapter-owned receiving details to the outbound list
  that shared them. Cancellation cleanup failures are reported separately from
  message delivery failures.
- `prepare_and_resolve_contact_payment` can ensure/advance the link, drain
  outbound and inbound work for bounded rounds, then resolve private-first with
  optional public fallback.
- Resolution does not execute a payment or persist the returned `PaymentTarget`.

## Payment Request lifecycle

```mermaid
sequenceDiagram
    participant PayeeApp
    participant PayeeSDK
    participant PayeeStore as Payee local storage
    participant PHS as Payee homeserver
    participant PayerSDK
    participant PayerStore as Payer local storage
    participant PayerApp

    PayeeApp->>PayeeSDK: propose_payment_request
    PayeeSDK->>PayeeStore: enqueue proposal Event Message
    PayeeSDK->>PHS: later send through outbound worker
    PayerSDK->>PHS: receive proposal
    PayerSDK->>PayerStore: append raw event, Event ID dedupe, and checkpoint atomically
    PayerSDK-->>PayerApp: derive Proposed request view

    alt Accept
        PayerApp->>PayerSDK: accept_payment_request
        PayerSDK->>PayerStore: enqueue acceptance event
        Note over PayerApp: Payment execution remains outside Paykit.
        PayerApp->>PayerSDK: submit_payment_proof after execution
        PayerSDK->>PayerStore: validate correlation and enqueue proof event
    else Reject
        PayerApp->>PayerSDK: reject_payment_request
        PayerSDK->>PayerStore: enqueue rejection event
    end

    opt Either side cancels an allowed non-terminal request
        PayeeApp->>PayeeSDK: cancel_payment_request
        PayeeSDK->>PayeeStore: enqueue cancellation event
    end

    Note over PayeeStore,PayerStore: Lifecycle views are re-derived from inbound stream items plus local outbound queue records.
```

API calls that enqueue events return local derived state, not delivery or remote
processing confirmation. The SDK enforces local role and lifecycle preconditions
for its ergonomic APIs, but payment execution, settlement confirmation,
method-specific proof validation, and recurring scheduling remain application
responsibilities.

## Receipt issuance and retrieval

```mermaid
sequenceDiagram
    participant IssuerApp as Receipt issuer application
    participant IssuerSDK
    participant IssuerStore as Issuer local storage
    participant IHS as Issuer homeserver
    participant RecipientSDK
    participant RecipientStore as Recipient local storage
    participant RecipientApp as Recipient application

    IssuerApp->>IssuerSDK: prepare_receipt_issuance
    IssuerSDK->>IssuerStore: persist Prepared issuance with plaintext, ciphertext, access, and key
    Note over IssuerSDK,IHS: No network side effect occurs before the issuance record is durable.

    IssuerApp->>IssuerSDK: process_receipt_issuance
    IssuerSDK->>IHS: PUT Encrypted Receipt at canonical Receipt Location
    IssuerSDK->>IssuerStore: mark Stored or Failed
    IssuerSDK->>IssuerStore: atomically enqueue Receipt Access and mark AccessQueued
    IssuerSDK->>IHS: later send Receipt Access through outbound worker

    RecipientSDK->>IHS: receive and index Receipt Access event
    RecipientSDK->>RecipientStore: store raw event, dedupe record, Receipt Access index, and checkpoint atomically

    RecipientApp->>RecipientSDK: retrieve_receipt
    RecipientSDK->>IHS: GET Encrypted Receipt using issuer identity and Receipt Location
    RecipientSDK->>RecipientSDK: authenticate location, decrypt, verify Receipt ID and recipient
    RecipientSDK->>RecipientStore: atomically mark access Retrieved and save decrypted Receipt
```

- A caller-provided Receipt ID makes `issue_receipt` retry-safe. Otherwise call
  `prepare_receipt_issuance` once, retain its generated ID, then process it.
- Receipt Access delivery is separate from Encrypted Receipt storage. Repeated
  processing resumes from the durable issuance status.
- Retrieval failures update the local Receipt Access retrieval status without
  changing issuer homeserver state.

## Encrypted Link recovery markers

```mermaid
sequenceDiagram
    participant AApp as Party A application or worker
    participant ASDK as Party A SDK
    participant AStore as Party A local storage
    participant AHS as Party A homeserver
    participant BSDK as Party B SDK
    participant BStore as Party B local storage
    participant BHS as Party B homeserver

    ASDK->>AStore: mark peer RecoveryRequired and clear unsafe active link state
    ASDK->>AStore: create or reuse local recovery attempt ID
    ASDK->>AHS: PUT pairwise-derived public recovery marker
    ASDK->>AStore: clear or save marker publication error

    BSDK->>AHS: GET Party A recovery marker
    alt New and non-stale marker
        BSDK->>BStore: under peer lease, record remote attempt and mark RecoveryRequired
        BSDK->>BHS: clear Party B derived encrypted outbox
    else Missing, stale, or already observed
        BSDK-->>BStore: no state transition
    end

    opt Relink after recovery is required
        BSDK->>BHS: clear any remaining old local outbox state
        BSDK->>BStore: save fresh handshake snapshots and generations
        BSDK->>BHS: write normal local Encrypted Link Handshake slots
        BSDK->>AHS: read normal remote Encrypted Link Handshake slots
    end

    opt Fresh link established and policy permits cleanup
        AApp->>ASDK: remove_encrypted_link_recovery_marker
        ASDK->>AHS: DELETE marker
        ASDK->>AStore: clear local marker fields
    end
```

Recovery markers are public but pairwise-derived and minimal. They coordinate a
fresh handshake; they are not acknowledgements, transcripts, or private message
replacements. When marker policy is disabled, automatic marker I/O is skipped,
but local recovery-required state still protects unsafe private automation.

## Read-only and process-local operations

These supported operations do not themselves update SDK local storage or
homeserver data unless noted elsewhere:

- record/list queries such as `linked_peers`, `contact_records`, Payment Request
  views, receipt views, and pending queue inspection;
- path, Pubky URI, auth URL, and capability parsing/normalization;
- `identity_status` when an identity record already exists;
- public profile, blob, follows, receiver marker, receiver path, and endpoint
  reads, except `refresh_contact_paykit_profile`, which explicitly updates the
  cached Contact Record;
- payment endpoint selection and `PaymentTarget` construction by the adapter.

Some query-like private helpers may observe a remote recovery marker or attempt a
private stream refresh to prevent stale private state from being treated as
healthy; those side effects are shown in the private resolution and recovery
flows above.

## Implementation anchors

- Storage contract and records: `src/storage/mod.rs` and
  `src/storage/records.rs`
- Identity lifecycle: `src/runtime/mod.rs`, `src/pubky_session.rs`, and
  `src/runtime/backup.rs`
- Profiles and contacts: `src/runtime/profiles.rs` and
  `src/runtime/contacts.rs`
- Public endpoints: `src/runtime/public_endpoints.rs`
- Links, outbound, intake, and recovery: `src/runtime/encrypted_links.rs`,
  `src/runtime/outbound_private.rs`, `src/runtime/private_stream.rs`, and
  `src/runtime/recovery.rs`
- Private lists and resolution: `src/runtime/private_lists.rs` and
  `src/runtime/payment_resolution.rs`
- Payment Requests and receipts: `src/runtime/payment_requests.rs` and
  `src/runtime/receipts.rs`
