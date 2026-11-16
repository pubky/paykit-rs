# Pubky Noise

A fully-integrated [Noise protocol](https://noiseprotocol.org/) framework for encrypted peer-to-peer messaging over [Pubky](https://pubky.org) homeservers.

Peers use their homeservers as outboxes: each party writes encrypted Noise messages to their own homeserver, and reads from the remote peer's homeserver. The library wraps the [Snow](https://github.com/mcginty/snow) Noise implementation in a clean async interface with built-in session backup and restore.

## Install

```toml
# Cargo.toml
[dependencies]
pubky-noise = "0.1.0"
```

### Actual dependencies (for reference)

| Crate | Version | Purpose |
|---|---|---|
| `pubky` | 0.16.2 | Pubky SDK (homeserver client, sessions, keys) |
| `snow` | 0.10.0 | Noise protocol implementation |
| `ed25519-dalek` | 3.0.0 | Ed25519 signatures and key conversions |
| `curve25519-dalek` | 5.0.0 | X25519 Diffie-Hellman for path derivation |
| `sha2` | 0.11.0 | SHA-256 hashing (path derivation, Noise suite) |
| `getrandom` | 0.3 | Cryptographic RNG |
| `hex` | 0.4 | Hex encoding for derived paths |
| `rand` | 0.9.0 | Random key generation |

## Quick Start

```rust,no_run
use std::sync::Arc;
use pubky::prelude::*;
use pubky_noise::{PubkyNoiseConfig, PubkyNoiseEncryptor, HandshakeResult};

// 1. Create shared configuration
//    (requires an authenticated PubkySession and a Pubky HTTP client)
let config = PubkyNoiseConfig::new(
    root_secret_key,          // [u8; 32] - root Ed25519 secret key
    0,                        // protocol version
    "XX",                     // Noise handshake pattern
    homeserver_session,       // authenticated PubkySession
    "/pub/data".to_string(),  // storage path prefix
    pubky_client,             // Pubky HTTP client
).unwrap();

// 2. Create encryptors for each side
let mut initiator = PubkyNoiseEncryptor::new(
    config.clone(),
    ephemeral_secret_key,     // [u8; 32] - per-session key
    true,                     // initiator = true
    responder_public_key,     // remote peer's PublicKey
).unwrap();

// 3. Run the handshake (polling-safe, call repeatedly)
loop {
    match initiator.handle_handshake().await? {
        HandshakeResult::Pending => { /* poll again later */ },
        HandshakeResult::Terminal => break,
    }
}

// 4. Transition to transport phase
let link_id = initiator.transition_transport().unwrap();

// 5. Prepare, persist, and publish an encrypted message
let prepared = initiator.prepare_send(b"Hello, peer!")?;
persistent_store.commit_send(
    prepared.destination_path(),
    prepared.ciphertext(),
    prepared.resulting_session_state(),
)?;
initiator.acknowledge_persisted_send(prepared)?;
ordered_publisher.flush_in_order().await?;

// Receive convenience API; use prepare_receive for durable processing.
let messages = initiator.receive_message().await?;

// 6. Clean up
initiator.close();
```

When polling a handshake packet, only HTTP 404/410 is treated as absent (`Pending`).
Transport reads treat only 404 as absent; snapshot reads map 404/410 to
`RestoreBackupNotFoundError`. Other non-success HTTP statuses return
`HomeserverHttpError { status }`. Read failures without an HTTP status and
successful (2xx) response-body failures return `HomeserverResponseError`. Pubky
preserves a known non-success status even if reading its error body fails or times out.

Neither error implies that retrying unchanged will help. Use the status to choose
application-specific handling: authorization or request errors may require
correction, while 408, 429 and some 5xx responses may warrant bounded retries.
The failed read does not advance its cursor; earlier successful handshake actions
remain applied. The quick start propagates errors to the caller; write failures
still require the [snapshot recovery](#code-example) described below.

### Request Deadlines

Packet and snapshot byte limits bound accumulated successful-response bytes.
Pubky separately caps captured error-body bytes. These byte limits do not bound
elapsed time or total/peak memory. Configure a total
`PubkyHttpClientBuilder::request_timeout` on native clients before using them for
peer reads or creating the authenticated session used for snapshot reads. Pubky
0.16.2 disables native HTTP timeouts by default. Its total request deadline runs
from connection setup through completion of the response body, including error
bodies. A separate `read_timeout` resets on each successful read, so it is not a
substitute for a total deadline when a server continually sends data or empty
frames. See the compiled example in the `PubkyNoiseConfig` API documentation.

Noise does not impose a timeout or retry automatically. Choose deadlines and
backoff for the application. Browser/WASM callers must provide operation-level
deadlines and cancellation; Pubky's native HTTP timeout settings do not apply
there. Cancellation of a write does not establish that it was rejected: preserve
the existing snapshot and exact-ciphertext recovery requirements.

An HTTP/1.1 zero-sized chunk terminates a chunked body; an HTTP/2 empty DATA frame
does not necessarily end the stream. The reader accepts empty frames without
treating them as EOF or malformed ciphertext. A configured total deadline bounds
both stalled responses and streams of empty frames.

## Architecture

### Outbox Model

Each peer writes to their **own** homeserver and reads from the **remote** peer's homeserver:

```text
Alice's Homeserver                 Bob's Homeserver
  alice.write_path/{n}               bob.write_path/{m}
    ^ Alice writes                     ^ Bob writes
    | Bob reads via bob.read_path      | Alice reads via alice.read_path
```

Messages are stored at incrementing slot indices under each direction's path.
During the handshake, reads and writes follow the Noise pattern's ordered action
sequence and share one slot counter. After `transition_transport()`, that counter
becomes the transport base slot, and each direction advances its own homeserver
slot counter independently. Snow's transport nonces are tracked separately from
homeserver slot selection.

### Wire Format

Handshake messages use a length-prefixed, fixed-size storage packet:

```text
[len_hi, len_lo, ciphertext..., zero padding...]
```

- `len`: big-endian u16 indicating ciphertext length
- `ciphertext`: the Noise handshake message
- Total stored packet size: 1018 bytes

Transport messages encrypt and authenticate one fixed-size plaintext frame:

```text
NoiseAEAD([body_len_hi, body_len_lo, body..., zero padding...])
```

- `body_len`: encrypted big-endian u16 indicating the application message length
- `body`: up to 1000 bytes (`PUBKY_NOISE_MSG_LEN`)
- Total stored packet size: 1018 bytes, independent of the body length

### Crypto Primitives

For peer authentication, compare `remote_static_public_key()` with an independently
authenticated X25519 key before using a completed or restored session. The getter
returns `None` until the handshake completes, or for patterns without a remote
static key. Noise XX proves possession of the presented key, not its association
with a Pubky identity.
`derive_static_public_key()` derives the public key from the static secret passed
to `PubkyNoiseEncryptor::new`; it is distinct from the Ed25519 keys used for routing.

The Noise protocol name is:

```text
Noise_{pattern}_25519_ChaChaPoly_SHA256
```

| Primitive | Algorithm | Purpose |
|---|---|---|
| Key exchange | X25519 | Diffie-Hellman |
| Stream cipher | ChaCha20-Poly1305 | Authenticated encryption |
| Hash | SHA-256 | Handshake transcript hashing |
| Transport mode | Stateless | Explicit nonce per message |

## Mental Model

### Core Types

- **`PubkyNoiseConfig`** -- Shared configuration and resources for multiple sessions. Holds the HTTP client, authenticated homeserver session, read/write paths, root keypair, and default Noise pattern. Wrap in `Arc` and share across encryptors.

- **`PubkyNoiseEncryptor`** -- A single-session Noise encryptor. Each instance manages exactly one Noise session (handshake + transport) with a single remote peer. Create multiple instances sharing the same `Arc<PubkyNoiseConfig>` for concurrent sessions.

- **`LinkId`** -- A 32-byte identifier derived from the Noise handshake transcript hash. Changes after every handshake when ephemeral keys are used. Available after calling `transition_transport()`.

- **`PubkyNoiseSessionState`** -- Serializable snapshot of a session, including incoming handshake messages for local replay through a fresh Noise state. Because it includes the session's secret keys, it is encrypted before homeserver storage (see [Session Backup & Restore](#session-backup--restore)).

- **`PreparedSend` / `PreparedReceive`** -- Staged transport results containing the exact message data and resulting session state. Use these when message publication or processing must be committed atomically with session state.

- **`DataLinkContext`** -- Internal Noise state machine managing the handshake and transport phases. Not used directly by consumers.

### Resource Limits

The library does not impose a total session limit. For the supported NN/XX
sessions, the current packet and version-1 snapshot formats have these bounds:

| Data | Size in bytes | Source |
|---|---|---|
| Application body in one transport message | 0 to 1000 | `snow_crypto::PUBKY_NOISE_MSG_LEN` |
| Stored handshake or transport packet | 1018 when emitted | `PUBKY_NOISE_CIPHERTEXT_LEN + 2` / `PUBKY_NOISE_TRANSPORT_PACKET_LEN` |
| Serialized valid session state | 198 to 2234 | `serializer::MIN_SESSION_STATE_LEN` / `MAX_SESSION_STATE_LEN` |
| Encrypted backup record | 252 to 2288 | `backup_crypto::MIN_BACKUP_RECORD_LEN` / `MAX_BACKUP_RECORD_LEN` |
| Accumulated successful backup response body | At most 4096; the accepted backup must still satisfy the format bounds | `backup_crypto::MAX_BACKUP_RESPONSE_BYTES` |

The maximum serialized state is 198 fixed bytes plus two incoming handshake
messages of at most 1016 bytes each, with a two-byte length for each message.
The encrypted record adds 54 bytes for its header, nonce, generation and tag.
These are data-length limits, not a per-session RAM ceiling. Live encryptors also
hold Snow state and may retain a pre-mutation snapshot or pending ciphertext.
Prepared operations, snapshot clones, serialization/encryption buffers, vector
capacity, and HTTP client/chunk buffers add memory beyond the serialized sizes;
`size_of::<PubkyNoiseEncryptor>()` alone excludes heap allocations.

For a host budget, measure peak memory on the deployed target while varying live
handshakes, transport sessions, concurrent reads/restores and queued prepared
operations. Track session counts, retained snapshot counts/bytes, pending-output
queue bytes, and handshake age; account separately for shared configuration/HTTP
resources and per-operation peak allocations. Use those measurements with
headroom to choose admission, concurrency, rate and retention limits. The library
does not cap aggregate caller-owned queues or saved snapshots. Cleanup of
abandoned handshakes must preserve the pending-output persistence and checkpoint
requirements described below. A bound on one snapshot alone does not prevent
resource exhaustion across many sessions.

### Lifecycle

```text
new() --> handle_handshake() [loop] --> transition_transport() --> send/receive --> close()
        |                                     |
    last_good_snapshot                     snapshot() --> persist_snapshot() [encrypted]
        |                                     |
    restore() [on crash recovery]     load_snapshot() --> restore() [on crash recovery]
```

### Staged Transport Operations

Transport sends use staged state transitions so ambiguous homeserver responses
cannot cause a fresh plaintext to reuse a nonce:

These APIs coordinate an application's durable state with transport processing.
They do not acknowledge that the remote peer received a message.

1. Call `prepare_send()` to obtain the destination path, exact ciphertext, and resulting session state.
2. Atomically persist the exact ciphertext and resulting session state as one outbound record.
3. Call `acknowledge_persisted_send()`. The handle is consumed and the encryptor may prepare the next message.
4. Write persisted outbound records to their homeserver destination paths in order. If a write is uncertain, retry the exact stored ciphertext.
5. For inbound data, fetch `next_receive_path()`, call `prepare_receive()`, atomically persist the resulting state with the application's durable processing result, and call `acknowledge_persisted_receive()`.

If atomic persistence fails, drop the advanced encryptor and restore the previous
persisted state. There is no in-place discard operation. Callers sharing state
across processes must serialize preparation and use conditional state updates.
Do not call `persist_snapshot()` to commit a staged operation: it is rejected
while an operation is awaiting acknowledgement. In the caller's atomic storage
transaction, replace the previous session snapshot with `resulting_session_state`
and persist the matching outbound record or inbound processing result.

`send_message()` is deprecated because it cannot make the exact ciphertext and
resulting state durable atomically. After an ambiguous in-process write failure,
`retry_pending_send()` republishes the exact retained ciphertext. This recovery
does not survive a process crash. `receive_message()` remains available for
callers that do not need atomic application-state processing.

Decrypted plaintext is sensitive application data. Avoid logging it, minimize copies, and protect it at rest whenever the application protocol requires persistence.

## Noise Handshake Patterns

| Pattern | Status | Auth | Description |
|---|---|---|---|
| `NN` | Implemented | None | No authentication, anonymous ephemeral keys |
| `XX` | Implemented | Mutual | Mutual authentication, both sides reveal static keys |
| `N` | Declared | One-way | Sender authenticates to known recipient |
| `IK` | Declared | Mutual | Initiator knows responder's static key upfront |
| `NK` | Declared | One-way | Initiator authenticates to known responder |

Patterns marked "Declared" are recognized but are not supported. Resolving their
handshake actions returns `UnsupportedNoisePattern`. An unrecognized pattern name
passed to a configuration constructor returns `UnknownNoisePattern`.

### Handshake Flow (XX Pattern)

```text
Initiator                          Responder
    |                                  |
    |-- Step 1: -> e ----------------->|
    |                                  |
    |<-- Step 2: <- e, ee, s, es ------|
    |                                  |
    |-- Step 3: -> s, se ------------>|
    |                                  |
    [transition_transport()]    [transition_transport()]
    |                                  |
    |<======= encrypted transport ====>|
```

### Polling-Safe Handshake

`handle_handshake()` is designed for polling: it can be called repeatedly by either side in any order. If the peer's message is not yet available, it returns `HandshakeResult::Pending` without advancing state. This makes it safe for use in event loops and async contexts.

### Durable Handshake Preparation

`prepare_handshake_step()` consumes an encryptor and prepares one NN or XX step
without network I/O. For a handshake-phase snapshot, inspect
`next_handshake_read_slot()` first:

- `Some(slot)`: fetch the complete peer packet and pass `Some(packet)`. If that
  slot returns HTTP 404/410, wait and poll again without preparing the step.
- `None`: pass `None` for the remaining write or control actions, including an
  already-completed handshake step. This is not a way to skip a required read.

Persist the returned snapshot and exact outgoing packet/path atomically before
publication. Keep uncertain writes pending and retry the same bytes. Do not
advance or enter transport until publication is durably acknowledged, including
the final XX packet. Preparation consumes the encryptor even on failure, so
recover from the previous durable checkpoint after an error.

The caller must recheck its session lease, source checkpoint, and application
policy permitting use of the peer before applying a prepared result. Noise does
not grant application access. For XX, compare the learned static key against the
independently authenticated expected peer key before exchanging application
messages. NN does not authenticate a peer identity.

## Asymmetric Path Derivation

For per-peer-pair path privacy, use `derive_asymmetric_paths()` to compute distinct write/read paths from a DH shared secret:

```rust,ignore
use pubky_noise::path_derivation::derive_asymmetric_paths;

let (write_path, read_path) = derive_asymmetric_paths(
    &my_secret_key,
    &their_pubkey,
    b"paykit-path-v0",                // domain separation
    "/pub/paykit.app/v0/private",     // base path
);
// write_path = "/pub/paykit.app/v0/private/a1b2c3d4...64 hex chars"
// read_path  = "/pub/paykit.app/v0/private/e5f6a7b8...64 hex chars"
```

**Correctness guarantee**: For parties Alice and Bob:
- `derive(alice_sk, bob_pk, ...).write_path == derive(bob_sk, alice_pk, ...).read_path`
- `derive(alice_sk, bob_pk, ...).read_path == derive(bob_sk, alice_pk, ...).write_path`

This holds because `X25519(a, B) == X25519(b, A)` (DH commutativity).

**Derivation formula**:
```text
dh_secret   = X25519(to_scalar_bytes(ed25519_seed), to_montgomery(remote_ed25519_pk))
write_path  = "{base_path}/{hex(SHA-256(domain || dh_secret || local_ed25519_pk))}"
read_path   = "{base_path}/{hex(SHA-256(domain || dh_secret || remote_ed25519_pk))}"
```

Use `PubkyNoiseConfig::new_with_paths()` to supply separate write/read paths.

## Session Backup & Restore

Sessions can be snapshotted, serialized, and restored to recover from crashes or write failures.

Snapshots restore without network I/O: they retain the incoming handshake
messages and regenerate outgoing handshake bytes in memory from the saved
ephemeral seed. Replay verifies message authentication and the saved transcript
hash or LinkId. Snapshots must remain encrypted, authenticated, and current;
this is not a cache of live transport counters. Peer authorization and
cross-process coordination remain the caller's responsibility. Remote deletion
or replacement of old handshake files
does not invalidate a saved transcript or revoke a session.

`snapshot.next_handshake_read_slot()` inspects the saved cursor without restoring
the Noise state or performing network I/O. `Some(slot)` lets a caller probe
`{peer_public_key}/{read_path}/{slot}` before taking a lease. `None` means normal
advancement is required, not that the session is idle: the next action may be a
write, control action, step completion, or transport operation. The probe checks
version and cursor consistency, not snapshot authenticity or transcript validity.
Existence is advisory only; all authorization, restore, recovery, and advancement
checks must still run under the existing leases.

Invalid snapshot structure returns `RestoreBackupDeserializeError`, cryptographic
replay failures return `RestoreBackupReplayError`, and transcript hash mismatches
return `RestoreBackupHashMismatch`. Restore neither downloads nor writes remote state.

### Snapshot Format

`PubkyNoiseSessionState` uses the following binary format:

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | version |
| 1 | 1 | phase (0=Handshake, 1=Transport) |
| 2 | 1 | pattern |
| 3 | 1 | initiator flag |
| 4-35 | 32 | ephemeral secret key |
| 36 | 1 | has static secret flag |
| 37-68 | 32 | static secret key |
| 69-72 | 4 | handshake/base counter (u32 big-endian) |
| 73 | 1 | noise step |
| 74 | 1 | sub-step index |
| 75 | 1 | has handshake hash flag |
| 76-107 | 32 | handshake hash |
| 108 | 1 | has link ID flag |
| 109-140 | 32 | link ID |
| 141-148 | 8 | sending nonce (u64 big-endian) |
| 149-156 | 8 | receiving nonce (u64 big-endian) |
| 157-160 | 4 | write counter (u32 big-endian) |
| 161-164 | 4 | read counter (u32 big-endian) |
| 165-196 | 32 | endpoint public key |
| 197 | 1 | incoming handshake message count |
| 198+ | variable | each incoming message: u16 big-endian length, then unpadded bytes |

The format version is 1. Snapshots store at most two incoming messages, each
bounded by `PUBKY_NOISE_CIPHERTEXT_LEN`; the size ranges from `MIN_SESSION_STATE_LEN`
(198 bytes) to `MAX_SESSION_STATE_LEN` (2234 bytes).
With the library's empty handshake payloads, completed NN snapshots
are 248/232 bytes and XX snapshots are 296/298 bytes (initiator/responder).

### Encrypted Homeserver Backup

The serialized snapshot contains the session's ephemeral and static secrets, so it must never
be stored in plaintext. `persist_snapshot()` encrypts it before uploading to
`{write_path}/backup` on the local homeserver, and `load_snapshot()` fetches and decrypts it:

```rust,ignore
use pubky_noise::backup_crypto;

// `save_checkpoint`/`load_checkpoint` are caller-side trusted local storage:
// they must durably persist and return the generation (e.g. on disk).

// Obtain the 32-byte backup key. Apps with access to the Pubky root secret
// can derive it via `derive_backup_key()`; delegated apps that do not hold
// the root secret may supply their own key instead (e.g. derived from a
// shared Noise/state key).
let backup_key = backup_crypto::derive_backup_key(&root_secret);

// Encrypt and upload the snapshot to the homeserver. `generation` is a
// caller-managed counter that must be *strictly* higher for each new
// snapshot (equality with the checkpoint is accepted on load, so a reused
// generation would not be flagged as a rollback).
let generation = local_checkpoint.map_or(1, |checkpoint| checkpoint + 1);

// IMPORTANT: advance your trusted local checkpoint to `generation` *before*
// (or atomically with) this call -- see "Rollback protection" below.
save_checkpoint(generation)?;
encryptor.persist_snapshot(&backup_key, generation).await?;

// Later (e.g. after a crash or on another device): fetch, decrypt and restore.
// Reload the checkpoint (it may have advanced since this process cached it):
// passing a stale or missing checkpoint would weaken rollback detection.
let local_checkpoint = load_checkpoint();
let loaded = PubkyNoiseEncryptor::load_snapshot(&config, &backup_key, local_checkpoint).await?;

// The accepted generation may be higher than your checkpoint; record it as
// the new checkpoint *before* the restored session resumes activity.
// Otherwise a crash could leave the old checkpoint in place, letting a
// stale homeserver replay an older backup and reuse counters or nonces.
save_checkpoint(loaded.generation)?;

let mut restored = PubkyNoiseEncryptor::restore(config, loaded.state, peer_pubkey).await?;
// (`peer_pubkey` is the remote peer you were talking to; it is also stored
//  in `loaded.state.endpoint_pubkey` and can be reconstructed from it via pkarr.)
```

Encryption uses XChaCha20Poly1305 (the same AEAD the Noise transport runs on, via `snow`)
with a random 192-bit nonce per write, prepended to the ciphertext (the nonce is not secret --
it only needs to be unique per write, and decryption requires it). `persist_snapshot()` and
`load_snapshot()` take a caller-provided 32-byte `backup_key`. The optional `backup_crypto::derive_backup_key()`
helper derives it from the Pubky root secret with a domain-separated KDF --
`SHA-256("pubky-noise/session-backup/v0" || root_secret)` -- so the raw root secret is never
used directly; callers that do not hold the root secret supply their own key instead.
The snapshot is not compressed: key material and handshake ciphertexts do not
compress usefully. Network packet padding is omitted.

Backup encryption does not hide the serialized snapshot's length. A homeserver
can observe the record size and upload timing, which can reveal handshake
metadata such as the pattern, role, or progress. Snapshots retain incoming
handshake messages, not application transport-message history. With the
library's empty outgoing handshake payloads, these sizes do not track application
message lengths or directly disclose their contents. Fixed-size transport
packets do not make backups fixed-size, and this backup format does not provide
length-hiding padding.

The stored record is a closed, versioned envelope:

```text
magic ("PNBK") || envelope_version || algorithm_id || nonce || ciphertext
```

The 6-byte header is authenticated as AEAD associated data (AAD): it stays in cleartext so the
decoder can dispatch on it, and any modification fails decryption. The AAD also commits the
intended backup path (`{write_path}/backup`), so a malicious homeserver cannot substitute a
backup written for a different path under the same key -- the tag mismatch fails decryption
before the rollback checkpoint or session state can be poisoned. Only explicitly supported
envelope versions are accepted, the record must fit the size bounds of its version, and the
(2xx) response body is read in chunks under a size cap -- malformed, truncated, trailing, and
oversized records are all rejected. Pubky 0.16.2 separately bounds non-2xx error bodies
using its configured error-body cap. Neither byte cap bounds elapsed time; callers
must also configure a request deadline as described above.

**Rollback protection.** AEAD authenticates the bytes but provides no freshness: a stale or
malicious homeserver can return an older, still-valid backup after the session has advanced,
which would reinstall old transport nonces and slot counters (nonce reuse, slot overwrites,
peer desynchronization). To detect this, every backup carries a monotonic `generation` in its
authenticated plaintext. Pass your trusted local checkpoint as `min_generation` to
`load_snapshot()`; older backups are rejected with `RestoreBackupRollbackError`. Without a
trusted checkpoint (`None`, e.g. a fresh device) rollback cannot be detected -- a signed or
hash-chained sequence alone is not sufficient either, since the homeserver can simply withhold
the newest element.

**Losing the checkpoint.** The checkpoint is the only rollback anchor, so it must be stored
with at least as much care as the backup key, and independently of the replayable backup itself.
If the checkpoint is stored only beside the backup, a malicious or compromised homeserver can
roll back both together. It should therefore be kept in trusted, integrity-protected,
rollback-resistant storage (e.g. a local secure element, a separately authenticated cloud
account, or tamper-resistant local hardware), not fetched from the same homeserver path as the
backup.

If the device holding the checkpoint fails hard and the client is migrated to new hardware
without a trusted checkpoint, `load_snapshot()` must be called with `min_generation = None`.
A homeserver that detects the migration (e.g. via a changed client or OS fingerprint) can then
serve an older, still-valid backup and the rollback is accepted silently. Restoring stale state
reuses the same Noise key material and nonces that the peer has already seen in the advanced
session: this breaks confidentiality and authentication for those packets and can lead to
forgery or session desynchronization. When checkpoint freshness is unknown, the only safe
choice is to discard the restored session state and start a fresh Noise session with the peer
rather than resuming the old one.

**Checkpoint update order matters.** Advance the trusted local checkpoint to the new
`generation` *before* (or atomically with) calling `persist_snapshot()`. If the checkpoint is
advanced only after the upload and the process crashes in between, the checkpoint still holds
`generation - 1`, so a homeserver replaying the previous backup would be accepted. Crashing
with the checkpoint already advanced is safe: the new upload is simply lost and loading then
rejects the older record instead of silently accepting stale state. Two rules follow: (1) each
new snapshot must use a *strictly* higher generation, because a backup whose generation equals
the checkpoint is accepted; (2) after a successful `load_snapshot()`, persist
`loaded.generation` as the new checkpoint before the restored session resumes activity, since
the accepted generation can be higher than the checkpoint you supplied.

If you persist snapshots through your own storage instead of `persist_snapshot()`, you must
encrypt the serialized bytes yourself.

### Snapshot Security

Session snapshots contain static and ephemeral secret key material. Encrypt and
authenticate them at rest (as `persist_snapshot()` does), restrict access to apps
or processes authorized for the same identity, and ensure superseded snapshots are
no longer recoverable. Retaining restorable ephemeral material extends its lifetime
and can expose messages from that Noise session if the snapshot is compromised.
Starting a fresh session does not protect old traffic while older snapshots remain
recoverable.

At-rest encryption protects the stored bytes but does not remove this tradeoff
while a snapshot remains recoverable. The staged transport APIs also do not
provide cross-process authentication, authorization, credential management, or
locking; callers must enforce those requirements.

### Recovery Flow

```rust,ignore
// Persist the encrypted snapshot to the homeserver (the snapshot contains
// session secrets -- never store the serialized bytes in plaintext).
// Advance your trusted local checkpoint to `generation` first.
save_checkpoint(generation)?;
encryptor.persist_snapshot(&backup_key, generation).await?;

// On crash/failure: fetch, decrypt and restore. Reload the checkpoint first
// so rollback detection uses the latest value.
let local_checkpoint = load_checkpoint();
let loaded = PubkyNoiseEncryptor::load_snapshot(&config, &backup_key, local_checkpoint).await?;
// Record the accepted generation as the new checkpoint *before* restoring.
save_checkpoint(loaded.generation)?;
let mut restored = PubkyNoiseEncryptor::restore(config, loaded.state, endpoint_pubkey).await.unwrap();
// Continue from where you left off
```

### Write Failure Recovery

During handshake, if a homeserver write fails:

1. `handle_handshake()` returns `Err(HomeserverWriteError)`.
2. Snow's internal state has already advanced irreversibly.
3. Retrieve the pre-mutation snapshot via `last_good_snapshot()`.
4. Persist it and pass to `restore()` to rebuild the session from the correct position.

The restore mechanism replays saved incoming messages and regenerates outgoing
handshake bytes in memory through a fresh Noise state built with the same
ephemeral key material. It does not issue homeserver writes or republish packets.

### Handshake Recovery with `last_good_snapshot`

Every call to `handle_handshake()` automatically captures a pre-mutation snapshot before doing any work. If the call fails (or if a written message is subsequently lost), this snapshot is the recovery point.

#### Why recovery is needed

Snow's `HandshakeState` is a one-way ratchet: once `write_message()` is called, the internal state advances irreversibly. If the homeserver `put()` then fails, the encryptor's Noise state no longer matches what is actually stored on the homeservers. The encryptor cannot simply retry -- it must be rebuilt from scratch.

#### Recovery sequence diagram

```text
                    Initiator                    Homeserver                  Responder
                        |                            |                          |
  [snapshot captured]   |                            |                          |
                        |--- handle_handshake() ---->|                          |
                        |   Snow advances state      |                          |
                        |   put() FAILS              |                          |
                        |<-- Err(HomeserverWrite) ---|                          |
                        |                            |                          |
  [encryptor is now     |                            |                          |
   corrupted -- Snow    |                            |                          |
   advanced but message |                            |                          |
   never reached the    |                            |                          |
   homeserver]          |                            |                          |
                        |                            |                          |
  [get last_good_       |                            |                          |
   snapshot, serialize, |                            |                          |
   persist to storage]  |                            |                          |
                        |                            |                          |
  [discard corrupted    |                            |                          |
   encryptor]           |                            |                          |
                        |                            |                          |
  [restore() from       |                            |                          |
   persisted snapshot:  |                            |                          |
   - builds fresh Snow  |                            |                          |
     with same          |                            |                          |
     ephemeral key      |                            |                          |
   - replays saved      |                            |                          |
     handshake messages |                            |                          |
   - state matches the  |                            |                          |
     saved checkpoint]  |                            |                          |
                        |                            |                          |
  [restored encryptor]  |--- handle_handshake() ---->| (write succeeds)         |
                        |                            |--- message available --->|
                        |                            |                          |
                        |          ... handshake continues normally ...         |
```

#### Code example

```rust,ignore
use pubky_noise::{PubkyNoiseEncryptor, PubkyNoiseConfig, PubkyNoiseError, HandshakeResult};
use pubky_noise::backup_crypto;

// Recover from homeserver write failures *in-process*: the fresh
// `last_good_snapshot()` captured at the start of the failed call is the
// recovery point, so no disk is involved.
async fn handshake_with_recovery(
    encryptor: &mut PubkyNoiseEncryptor,
    config: Arc<PubkyNoiseConfig>,
    endpoint_pubkey: PublicKey,
) -> Result<HandshakeResult, PubkyNoiseError> {
    match encryptor.handle_handshake().await {
        Ok(result) => Ok(result),
        Err(PubkyNoiseError::HomeserverWriteError) => {
            // The encryptor is corrupted, but the snapshot captured at the
            // start of the failed call is still in memory -- restore directly
            // from it. This also works when no prior call ever succeeded.
            let snapshot = encryptor
                .last_good_snapshot()
                .expect("always Some after handle_handshake")
                .clone();

            *encryptor = PubkyNoiseEncryptor::restore(
                config,
                snapshot,
                endpoint_pubkey,
            )
            .await?;

            // The restored encryptor is back to the pre-failure position.
            // The caller can retry handle_handshake() on the next poll.
            Ok(HandshakeResult::Pending)
        }
        Err(e) => Err(e),
    }
}

// Crash coverage additionally requires the recovery point to be durable.
// This helper scopes that to explicit `HomeserverWriteError` failures only:
// it persists the current state before each call, so if the `put()` inside
// that call explicitly fails (returns an error), restoring the persisted
// checkpoint replays correctly and the write can be retried. It does NOT
// cover the lost-message case (Case a2, described below), where `put()`
// succeeded and the server later lost the write: restoring the post-write
// state would skip the lost write rather than republish it. Case a2 recovery
// needs the durable checkpoint to remain at the *pre-write* state until peer
// progress confirms the write actually persisted — call-side persistence
// alone cannot establish that.
async fn handshake_recovery_explicit_write_errors(
    encryptor: &mut PubkyNoiseEncryptor,
    config: Arc<PubkyNoiseConfig>,
    endpoint_pubkey: PublicKey,
    backup_key: &[u8; 32],
    backup_path: &str,
    generation: u64,
) -> Result<HandshakeResult, PubkyNoiseError> {
    let snapshot = encryptor.snapshot().unwrap();
    let encrypted =
        backup_crypto::encrypt_backup_with_key(backup_key, backup_path, generation, &snapshot);
    save_to_disk(&encrypted); // your persistence logic

    handshake_with_recovery(encryptor, config, endpoint_pubkey).await
}
```

#### Lost-message recovery (Case a2)

If `put()` succeeds but the data is subsequently lost (e.g., homeserver crash after acknowledgment), `handle_handshake()` returns `Ok(Pending)` -- the loss is undetectable at the protocol level. The handshake gets stuck: the responder keeps polling but finds nothing to read, and the initiator waits for a reply that will never come.

Recovery requires a persisted snapshot from **before** the lost write: restore it
and re-run the handshake to reproduce the write. Restoring a post-write snapshot
does not detect or republish lost data.

#### Key invariants

- `last_good_snapshot()` returns `None` before the first `handle_handshake()` call.
- Each `handle_handshake()` call overwrites the previous snapshot with the state from the start of *that* call.
- The snapshot contains the ephemeral secret key, which is the critical piece that allows `restore()` to re-derive the same transport keys via replay. Any persisted snapshot must be encrypted (as `persist_snapshot()` does).
- `restore()` checks the replayed transcript against the saved handshake hash or LinkId for transport-phase restores. Missing or mismatched hashes return `RestoreBackupHashMismatch`.

## Error Handling

| Error | Cause | Recovery |
|---|---|---|
| `UnknownNoisePattern` | Unrecognized configuration pattern name | Correct the pattern name |
| `UnsupportedNoisePattern` | Recognized pattern has no supported handshake actions | Use a supported pattern: "NN", "XX" |
| `SnowNoiseBuildError` | Noise stack failed to initialize | Check key material and pattern compatibility |
| `BadLengthCiphertext` | Received packet or authenticated transport frame is malformed | Discard message, check sender |
| `MissingHandshakePacket` | Preparation requires a read, but no packet was supplied | Recover the previous checkpoint and fetch the required packet before preparing |
| `UnexpectedHandshakePacket` | Preparation received a packet when no read was required | Recover the previous checkpoint and follow its read-slot probe |
| `HomeserverHttpError { status }` | A handshake, transport or snapshot read returned a non-success HTTP status not handled as absence | Inspect the status and correct the cause or apply caller-owned retry policy; no automatic retry |
| `HomeserverResponseError` | A read failed without an HTTP status, a successful (2xx) body read failed, or a snapshot exceeded its size cap | Check client configuration, connectivity and response validity; retry only after addressing the cause |
| `HomeserverWriteError` | Homeserver write failed | Restore from `last_good_snapshot()` |
| `IsHandshake` | Called a transport operation before transport phase | Wait for `is_handshake_complete()` and `transition_transport()` |
| `EncryptionError` | Noise encryption failed during `send_message()` | Check transport state; session may be corrupted |
| `DecryptionError` | Handshake or transport message authentication/decryption failed | Message may be tampered or nonces desynchronized |
| `CounterOverflow` | Message slot counter space is exhausted | Start a new Noise session |
| `NonceOverflow` | Transport nonce space is exhausted | Start a new Noise session |
| `UnacknowledgedPreparedTransport` | A prepared operation has not been durably acknowledged | Persist and acknowledge its handle, or restore the previous durable state if persistence failed |
| `NoPreparedTransport` | An acknowledgement was attempted with no pending operation | Check the caller's operation lifecycle |
| `PreparedTransportMismatch` | A prepared handle belongs to another encryptor or operation | Use the handle returned by the current encryptor |
| `RestoreBackupReplayError` | Handshake replay failed during restore | Check snapshot integrity |
| `RestoreBackupHashMismatch` | Replayed handshake produced different hash | Snapshot may be from a different session |
| `RestoreBackupDeserializeError` | Backup envelope or snapshot is invalid, including an unknown or unsupported snapshot pattern | Check data integrity and snapshot compatibility |
| `RestoreBackupDecryptError` | Persisted snapshot decryption failed | Wrong backup key, or tampered/corrupted backup |
| `RestoreBackupRollbackError` | Backup generation is older than the trusted local checkpoint | Restart from a fresh handshake; investigate homeserver |
| `RestoreBackupNotFoundError` | No backup exists at the backup path (distinct from connectivity/server failures) | Persist a snapshot first, or start a new session |

Callers matching `PubkyNoiseError` must handle `HomeserverHttpError` explicitly
when classifying read failures. In particular, a catch-all that treats errors as
invalid ciphertext or permanently non-retryable must not silently classify HTTP
408/429/5xx that way. Status codes are retained without server-supplied error
bodies or request URLs. Write errors keep their existing recovery contract.

## Features

| Feature | Description |
|---|---|
| `test-utils` | Enables test-only APIs: ciphertext tampering simulation (`test_enable_tampering`), homeserver write failure simulation (`test_enable_write_failure`), and last ciphertext inspection (`test_last_ciphertext`). |

## Examples

See the [e2e tests](../e2e/src/tests/pubky_noise.rs) for complete working examples including:

- NN and XX pattern handshakes
- Bidirectional message exchange
- Ciphertext tampering detection
- Out-of-order polling
- Incomplete handshake handling
- Session backup and restore (transport and handshake phases)
- Write failure recovery (both immediate error and lost-message scenarios)
- Dual homeserver setups
