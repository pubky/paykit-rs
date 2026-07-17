# Paykit Library State Updates

This document shows the state transitions caused by the stateful I/O APIs in
`paykit-lib`. The library itself is stateless: it returns or mutates caller-owned
Rust values, while durable network state lives on Pubky homeservers. Persisting
Encrypted Link or handshake snapshots is the caller's responsibility.

For payment flows, the diagrams use **Payer** and **Payee** rather than the
ambiguous sender/receiver terms. For Encrypted Link setup they use **Initiator**
and **Responder**. Either payment party can initiate a link, send a Private
Application Message, or issue a Receipt.

## State boundaries

```mermaid
flowchart LR
    subgraph A[Party A process]
        ACall[Caller application]
        ALib[paykit-lib]
        AMem[Caller-owned in-memory state<br/>handshake or Encrypted Link]
        ASnap[Caller-persisted snapshot<br/>optional and secret]
    end

    subgraph AH[Party A homeserver]
        APub[Public Paykit files]
        APriv[Derived private message slots]
        AReceipt[Encrypted Receipts]
        ARecovery[Recovery markers]
    end

    subgraph BH[Party B homeserver]
        BPub[Public Paykit files]
        BPriv[Derived private message slots]
        BReceipt[Encrypted Receipts]
        BRecovery[Recovery markers]
    end

    ACall --> ALib
    ALib <--> AMem
    ACall -. serialize or restore .-> ASnap
    ALib <--> APub
    ALib <--> APriv
    ALib --> AReceipt
    ALib --> ARecovery
    ALib -. public reads .-> BPub
    ALib -. private reads .-> BPriv
    ALib -. receipt reads are caller-driven .-> BReceipt
    ALib -. recovery reads .-> BRecovery
```

- `paykit-lib` has no database, event log, retry queue, or durable lifecycle
  index.
- Public files and Encrypted Receipts have stable Paykit paths. Encrypted Link
  handshake and transport slots are derived per counterparty receiver pair;
  `pubky-noise` owns their individual counter-based file names.
- An Encrypted Link or handshake object advances Noise counters in memory.
  Snapshot bytes contain secret key and counter material and must be protected by
  the caller.

### Homeserver path categories

| State | Receiver-scoped path |
| --- | --- |
| Public Payment Endpoint | `/pub/paykit/v0/<receiver_path>/endpoints/<payment_endpoint_identifier>` |
| Receiver Marker | `/pub/paykit/v0/<receiver_path>/receiver.json` |
| Encrypted Link stream | `/pub/paykit/v0/private/<receiver_path>/messages/<pair_hash>/<slot>` |
| Encrypted Receipt | `/pub/paykit/v0/private/<issuer_receiver_path>/receipts/<receipt_id>` |
| Encrypted Link Recovery Marker | `/pub/paykit/v0/private/<receiver_path>/encrypted-link-recovery/<pair_hash>` |

The pair hash and asymmetric read/write paths are derived from both parties,
both receiver paths, a shared secret, and a feature-specific domain separator.
The markers are publicly readable Pubky resources despite living below Paykit's
`private` path prefix; the pairwise-derived path limits casual enumeration but
is not an access-control boundary.

## Publish and discover public receiver state

Covers receiver marker and public Payment Endpoint APIs:

- `publish_paykit_receiver_marker` / `remove_paykit_receiver_marker`
- `set_payment_endpoint` / `remove_payment_endpoint`
- `list_paykit_receiver_paths`, `get_paykit_receiver_marker`,
  `get_payment_list`, and `get_payment_endpoint`

```mermaid
sequenceDiagram
    participant Payee as Payee application
    participant PL as paykit-lib
    participant PHS as Payee homeserver
    participant Payer as Payer application

    opt Make an empty receiver discoverable
        Payee->>PL: publish_paykit_receiver_marker
        PL->>PHS: PUT /pub/paykit/v0/<payee_receiver>/receiver.json
    end

    loop For each public Payment Endpoint
        Payee->>PL: set_payment_endpoint
        PL->>PHS: PUT /pub/paykit/v0/<payee_receiver>/endpoints/<identifier>
    end

    Payer->>PL: list_paykit_receiver_paths(payee)
    PL->>PHS: LIST receiver tree
    PL-->>Payer: paths with a valid marker or at least one endpoint

    Payer->>PL: get_payment_list or get_payment_endpoint
    PL->>PHS: LIST endpoint directory, then GET endpoint files
    PL-->>Payer: best-effort Payment List or optional payload

    opt Remove public state
        Payee->>PL: remove_payment_endpoint
        PL->>PHS: DELETE endpoint file
        Payee->>PL: remove_paykit_receiver_marker
        PL->>PHS: DELETE receiver.json
    end
```

State effects:

- Publication and removal do not create local library records.
- Missing endpoint files/directories are represented as `None` or an empty
  Payment List. Removing an already missing endpoint or marker succeeds.
- `get_payment_list` is not an atomic snapshot: the directory listing and file
  reads can observe different remote revisions.
- A receiver remains discoverable while it has a valid Receiver Marker or at
  least one public Payment Endpoint.

## Establish and resume an Encrypted Link

Covers `initiate_encrypted_link`, `accept_encrypted_link`, `advance_handshake`,
handshake snapshots/restores, and established link snapshots/restores.

```mermaid
sequenceDiagram
    participant IA as Initiator application
    participant IL as Initiator paykit-lib
    participant IHS as Initiator homeserver
    participant RHS as Responder homeserver
    participant RL as Responder paykit-lib
    participant RA as Responder application

    IA->>IL: initiate_encrypted_link
    IL-->>IA: in-memory handshake
    RA->>RL: accept_encrypted_link
    RL-->>RA: in-memory handshake

    loop Poll advance_handshake until complete
        IA->>IL: advance_handshake
        IL->>IHS: write next local derived handshake slot when required
        IL->>RHS: read responder derived handshake slot when required
        IL-->>IA: Pending handshake or complete Encrypted Link

        RA->>RL: advance_handshake
        RL->>RHS: write next local derived handshake slot when required
        RL->>IHS: read initiator derived handshake slot when required
        RL-->>RA: Pending handshake or complete Encrypted Link
    end

    par Caller checkpoints initiator state
        IA->>IL: serialize handshake or link
        IL-->>IA: secret snapshot bytes
    and Caller checkpoints responder state
        RA->>RL: serialize handshake or link
        RL-->>RA: secret snapshot bytes
    end

    opt Process restart
        IA->>IL: restore_encrypted_link_handshake or restore_encrypted_link
        IL-->>IA: restored in-memory state and counters
    end
```

State effects and failure rules:

- Each advance mutates the caller-owned handshake and may read or write derived
  homeserver handshake data.
- A completed handshake replaces the in-progress object with an in-memory
  `EncryptedLink`; the library does not persist that transition.
- Handshake write failures are recovered from a pre-mutation snapshot and return
  `Pending` up to the configured recovery-attempt limit.
- Restores require the same local and remote receiver paths. A stale established
  link snapshot can replay messages or desynchronize Noise counters.

## Exchange Private Application Messages

This common transport flow covers:

- Private Payment Lists through `set_private_payment_list`
- Payment Request proposal, acceptance, rejection, cancellation, and Payment
  Proof send helpers
- Receipt Access through `send_receipt_access`
- raw intake through `EncryptedLink::receive_private_application_messages`

```mermaid
sequenceDiagram
    participant Sender as Sending party application
    participant SL as Sender paykit-lib
    participant SHS as Sender homeserver
    participant RHS as Receiving party homeserver
    participant RL as Receiver paykit-lib
    participant Receiver as Receiving party application

    Sender->>SL: send typed event, list, access, or raw JSON
    SL->>SL: serialize and encrypt, then advance write counter
    SL->>SHS: write encrypted counter slot under local derived message folder
    SL-->>Sender: updated in-memory Encrypted Link

    Receiver->>RL: receive_private_application_messages
    RL->>SHS: read available sender counter slots
    RL->>RL: decrypt in order, then advance read counter
    RL-->>Receiver: ordered raw PrivateApplicationMessage values

    Receiver->>RL: parse by known kind
    RL-->>Receiver: typed value, typed validation error, or unknown raw message

    Note over Receiver,RHS: The receiving party must persist raw items and dedupe state before persisting an advanced link snapshot.
```

The apparent sender-to-receiver transfer is implemented as a write to the
sender's homeserver followed by the receiver reading that derived folder. The
receiver's own homeserver is not updated by intake.

Message semantics:

- Private Payment List is a **Latest-State Message**. The library sends every
  call; a stateful caller decides which valid list supersedes older lists.
- Payment Request lifecycle messages and Receipt Access are **Event Messages**.
  Every valid event matters and must be retained in stream order.
- The library neither persists raw events nor enforces durable Event ID dedupe,
  conflicting-ID detection, role authorization, or full Payment Request
  lifecycle policy. Those are SDK/application responsibilities.
- A successful send advances the in-memory write state. The caller should
  checkpoint the new link state together with its own durable send status.
- A successful receive advances the in-memory read state. If raw events are
  stored but the new snapshot is not, replay is expected and must be deduped.

## Private Payment List update and clear

A clear is not a special delete. It is a newer encrypted Private Payment List
whose `payment_endpoints` map is empty.

```mermaid
flowchart LR
    Build[Payee builds complete Private Payment List] --> Serialize[Serialize versioned JSON]
    Serialize --> Size{Fits one pubky-noise message?}
    Size -- no --> Reject[Return validation error<br/>no link or homeserver update]
    Size -- yes --> Encrypt[Encrypt and advance link write state]
    Encrypt --> Slot[Write new encrypted slot<br/>on payee homeserver]
    Slot --> Intake[Payer reads and decrypts ordered stream]
    Intake --> Parse{Valid Private Payment List?}
    Parse -- no --> Preserve[Caller preserves malformed raw item<br/>latest valid list remains current]
    Parse -- yes --> Replace[Stateful caller replaces cached list]
```

The complete map must be supplied on every update. The library does not merge
endpoints or retain the previous list.

## Payment Request lifecycle and Payment Proofs

```mermaid
sequenceDiagram
    participant Payee
    participant PayeeLink as Payee Encrypted Link
    participant PayerLink as Payer Encrypted Link
    participant Payer

    Payee->>PayeeLink: send_payment_request
    Note over Payee,PayeeLink: Local link write counter advances and no durable lifecycle record is created.
    Payer->>PayerLink: receive and parse proposal

    alt Payer accepts
        Payer->>PayerLink: send_payment_request_acceptance
        Payee->>PayeeLink: receive and parse acceptance
        opt After payer-controlled payment execution outside Paykit
            Payer->>PayerLink: send_payment_proof
            Payee->>PayeeLink: receive and parse proof
        end
    else Payer rejects
        Payer->>PayerLink: send_payment_request_rejection
        Payee->>PayeeLink: receive and parse rejection
    end

    opt Either party cancels a known non-terminal request
        Payee->>PayeeLink: send_payment_request_cancellation
        Payer->>PayerLink: receive and parse cancellation
    end
```

All arrows use the private transport flow above and therefore create encrypted
slots on the sending party's homeserver. `PaymentProof::validate_for_request`
checks structural correlation only. Payment execution, settlement, recurring
scheduling, sender-role authorization, and durable lifecycle derivation remain
outside `paykit-lib`.

## Issue and retrieve a Receipt

Covers `prepare_receipt` / `prepare_receipt_for_recipient`,
`store_prepared_receipt`, `send_receipt_access`, Receipt Access parsing, and
`decrypt_receipt`.

```mermaid
sequenceDiagram
    participant Issuer as Receipt issuer application
    participant IL as Issuer paykit-lib
    participant IHS as Issuer homeserver
    participant RL as Recipient paykit-lib
    participant Recipient as Receipt recipient application

    Issuer->>IL: prepare_receipt or prepare_receipt_for_recipient
    IL-->>Issuer: plaintext Receipt + Encrypted Receipt + Receipt Access
    Note over Issuer,IL: Preparation is local only and generates the Receipt ID and decryption key.

    Issuer->>IL: store_prepared_receipt
    IL->>IHS: PUT /pub/paykit/v0/private/<issuer_receiver>/receipts/<receipt_id>

    Issuer->>IL: send_receipt_access
    IL->>IHS: write encrypted Receipt Access event slot

    Recipient->>RL: receive private stream and parse Receipt Access
    RL-->>Recipient: location + decryption key + correlation fields
    Recipient->>IHS: GET Encrypted Receipt at issuer and location
    Recipient->>RL: decrypt_receipt(ciphertext, key, location)
    RL-->>Recipient: authenticated plaintext Receipt
```

- Preparation does not touch a homeserver or Encrypted Link.
- The encrypted artifact must be stored before Receipt Access is sent. If access
  delivery fails, the same prepared access descriptor can be retried.
- `paykit-lib` does not fetch Encrypted Receipts; the caller combines the Receipt
  Access sender identity with its path, fetches the bytes, and passes them to
  `decrypt_receipt`.
- The location is authenticated as AEAD associated data, and the decrypted
  Receipt ID must match the location.

## Recover, block at a higher layer, or close a link

The library provides recovery marker primitives but does not decide when a link
is unsafe or maintain a local peer status.

```mermaid
sequenceDiagram
    participant A as Party A application
    participant AL as Party A paykit-lib
    participant AHS as Party A homeserver
    participant BHS as Party B homeserver
    participant BL as Party B paykit-lib
    participant B as Party B application

    A->>AL: publish_encrypted_link_recovery_marker
    AL->>AHS: PUT pairwise-derived public recovery marker

    B->>BL: fetch_encrypted_link_recovery_marker
    BL->>AHS: GET Party A marker
    BL-->>B: marker or None
    Note over B: Caller decides to discard snapshots, clear queues, or relink.

    opt Clear stale local encrypted outbox before relinking
        B->>BL: clear_encrypted_link_outbox
        BL->>BHS: delete Party B derived message folder contents
    end

    opt Remove marker after recovery policy allows
        A->>AL: remove_encrypted_link_recovery_marker
        AL->>AHS: DELETE marker
    end

    opt Close only the in-memory transport
        A->>AL: close_encrypted_link
        AL-->>A: link consumed and closed
    end
```

`close_encrypted_link` does not remove public endpoints, encrypted messages,
Receipts, recovery markers, or caller-persisted snapshots.

## Pure APIs with no durable state effect

Constructors, validation, canonical serialization/parsing, path derivation,
Payment Proof correlation validation, and Receipt encryption/decryption are pure
with respect to durable state. They may create or transform caller-owned values,
but they do not write local storage or a homeserver until an explicit I/O helper
is called.

## Implementation anchors

- Public storage: `src/payment_endpoint.rs`, `src/receiver_marker.rs`, and
  `src/pubky_routing.rs`
- Encrypted Link state: `src/encrypted_link/handshake.rs`,
  `src/encrypted_link/link.rs`, and `src/encrypted_link/snapshot.rs`
- Private stream and lists: `src/encrypted_link/private_application_message.rs`
  and `src/private_payment_list.rs`
- Payment Requests: `src/payment_request/api.rs`
- Receipts: `src/receipt/access.rs` and `src/receipt/crypto.rs`
- Recovery markers: `src/encrypted_link_recovery.rs`
