# Paykit Library

Stateless Rust crate that implements Paykit Library functionality on [Pubky](https://pubky.org/). It provides helpers for **public** Payment Endpoints (stored as plaintext files on the homeserver), Private Payment Envelopes, Encrypted Links, and Receipts, while delegating Pubky authentication and session management to callers.

Pubky is the only supported network/storage backend for this crate. The former generic transport traits and `pubky` feature flag have been removed; Pubky dependencies are unconditional.

## Quick Start

Add `paykit-lib` to your `Cargo.toml`.

```toml
[dependencies]
paykit-lib = { version = "x.x.x" }
pubky = "0.8"
```

Minimal example — store and retrieve a public Payment Endpoint:

```rust,ignore
use paykit_lib::{
    set_payment_endpoint, get_payment_endpoint, get_payment_list,
    PaymentEndpointIdentifier, PaymentEndpointPayload,
};

// Create validated types.
let identifier = PaymentEndpointIdentifier::new("bitcoin-bolt11")?;
let payload = PaymentEndpointPayload::new("lnbc1...");

// Store a Payment Endpoint using an authenticated PubkySession.
set_payment_endpoint(&session, identifier.clone(), payload).await?;

// Read it back using pubky::PublicStorage.
let endpoint = get_payment_endpoint(&public_storage, &payee_pubkey, &identifier).await?;

// List all published Payment Endpoints for a payee.
let payment_list = get_payment_list(&public_storage, &payee_pubkey).await?;
for (identifier, payload) in &payment_list.payment_endpoints {
    println!("{}: {}", identifier.as_str(), payload.as_str());
}
```

## Core Types

### `PaymentEndpointIdentifier`

Machine-readable identifier for a Payment Endpoint type (for example `"bitcoin-p2sh"` or `"bitcoin-bolt11"`).
`PaymentEndpointIdentifier` is validated at construction time to prevent path injection attacks.

```rust
use paykit_lib::PaymentEndpointIdentifier;

// Construction is fallible:
let identifier = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
assert_eq!(identifier.as_str(), "bitcoin-bolt11");

// Path traversal is rejected:
assert!(PaymentEndpointIdentifier::new("../etc/passwd").is_err());
```

**Allowed values:** 1-64 ASCII characters from the set `[a-zA-Z0-9_-.]`. The value must not consist solely of dots (`.` and `..` are rejected as path traversal components), and must not be the reserved identifier `private` (used by private Paykit storage). Slashes, null bytes, spaces, and other special characters are rejected.

`PaymentEndpointIdentifier::new()` returns `Err(PaykitError::Validation(...))` for invalid input.

Read access is available via `as_str()`, `Display`, and `AsRef<str>`.

**Naming convention:** `PaymentEndpointIdentifier` values are opaque to the library, but counterparties need to agree on them to interoperate. A recommended (non-obligatory) convention for the shape of these identifiers is described in [../specs/payment-endpoint-identifier.md](../specs/payment-endpoint-identifier.md).

### `PaymentEndpointPayload`

Serialized Payment Endpoint Payload (UTF-8 text such as JSON, lnurl, etc.).

```rust
use paykit_lib::PaymentEndpointPayload;

let payload = PaymentEndpointPayload::new("ln...");
let payload_str: &str = payload.as_str();
let owned: String = payload.into_inner();
```

### `PaymentList`

Collection of Payment Endpoints keyed by Payment Endpoint Identifiers. Returned by `get_payment_list`.

```rust,ignore
use paykit_lib::PaymentList;

let payment_list: PaymentList = get_payment_list(&public_storage, &payee).await?;

// Access the underlying map:
for (identifier, payload) in &payment_list.payment_endpoints {
    println!(
        "identifier={} payload={}",
        identifier.as_str(),
        payload.as_str()
    );
}

// Check if empty:
if payment_list.payment_endpoints.is_empty() {
    println!("no Payment Endpoints published");
}
```

Private Payment Envelopes use a versioned `PrivatePaymentEnvelope` value. The Payment Endpoints still use the same `HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>` layout, but the envelope also carries a UUID-v4 `PaymentReference` for correlation with later protocol artifacts such as receipts:

```rust,ignore
use paykit_lib::{get_private_payment_envelope, PrivatePaymentEnvelope};

if let Some(envelope) = get_private_payment_envelope(&mut link).await? {
    println!("reference={}", envelope.reference.as_str());
    for (identifier, payload) in &envelope.payment_endpoints {
        println!(
            "identifier={} payload={}",
            identifier.as_str(),
            payload.as_str()
        );
    }
} else {
    println!("no Private Payment Envelope available yet");
}
```

Private Payment Envelopes use Latest-State Message semantics: if several envelopes are queued, `get_private_payment_envelope` returns the newest one and supersedes older envelopes. Receipt Access uses Event Message semantics: `get_receipt_access` returns all currently available Receipt Access descriptors in FIFO order.

The `payment_endpoints` field is a `HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>`.

### `PaykitError`

Domain error enum with the following variants:

| Variant | Meaning |
|---------|---------|
| `Transport { context, source }` | Network or Pubky SDK failure |
| `NotFound(String)` | Requested resource does not exist (404/GONE) |
| `InvalidData { context, source }` | Fetched data is corrupt or structurally invalid |
| `Validation(String)` | Caller-supplied input failed validation (e.g. invalid `PaymentEndpointIdentifier`) |

The `source` field on `Transport` and `InvalidData` is backed by [`anyhow::Error`] so callers can downcast via `anyhow::Error::downcast_ref` when they need the original typed error.

### `Result<T>`

All public APIs return `paykit_lib::Result<T>`, which is an alias for `std::result::Result<T, PaykitError>`.

## Pubky Sessions and I/O

- **Public Payment Endpoints** use concrete Pubky SDK handles. Writes take `&pubky::PubkySession`; reads take `&pubky::PublicStorage`.
- **Private Payment Envelopes** use `pubky-noise`'s `PubkyNoiseEncryptor` for Noise-encrypted messaging, which handles both encryption and homeserver I/O through Pubky. Private Payment Envelope functions accept an `EncryptedLink` established via an Encrypted Link Handshake.
- Paykit stays stateless. Session creation, capability scoping, key rotation, account recovery, and client timeout configuration remain caller responsibilities.
- Public payment paths are centralized by `PAYKIT_PATH_PREFIX` (`/pub/paykit/v0/`). Private Paykit message paths use `PAYKIT_PRIVATE_PATH_PREFIX` (`/pub/paykit/v0/private`) as the base for pubky-noise path derivation.

## Timeout Handling

Paykit does not wrap Pubky SDK calls with its own deadline. A slow or unresponsive homeserver can block the caller unless the Pubky client is configured with appropriate timeouts. The Pubky SDK exposes [`PubkyHttpClientBuilder::request_timeout`](https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout):

```rust,ignore
use std::time::Duration;
let client = PubkyHttpClient::builder()
    .request_timeout(Duration::from_secs(10))
    .build()?;
```

## API Surface

### Public Payment Endpoints

These functions operate on concrete Pubky SDK handles.

#### `set_payment_endpoint`

Store or update a payee-owned Payment Endpoint using the caller's authenticated `pubky::PubkySession`.

```rust,ignore
use paykit_lib::{set_payment_endpoint, PaymentEndpointIdentifier, PaymentEndpointPayload};

async fn demo(session: &pubky::PubkySession) -> paykit_lib::Result<()> {
    // NOTE: parties need to agree on Payment Endpoint Identifiers to interoperate.

    let identifier = PaymentEndpointIdentifier::new("bitcoin-bolt11")?;
    let payload = PaymentEndpointPayload::new("ln...");
    set_payment_endpoint(session, identifier, payload).await?;

    let identifier = PaymentEndpointIdentifier::new("bitcoin-p2wpkh")?;
    let payload = PaymentEndpointPayload::new("bc1...");
    set_payment_endpoint(session, identifier, payload).await?;
    // or
    let identifier = PaymentEndpointIdentifier::new("bitcoin-p2tr")?;
    let payload = PaymentEndpointPayload::new("bc1...");
    set_payment_endpoint(session, identifier, payload).await?;

    Ok(())
}
```

#### `remove_payment_endpoint`

Remove a previously published Payment Endpoint Payload for a given Payment Endpoint Identifier.

```rust,ignore
use paykit_lib::{remove_payment_endpoint, PaymentEndpointIdentifier};

async fn demo(session: &pubky::PubkySession) -> paykit_lib::Result<()> {
    let identifier = PaymentEndpointIdentifier::new("bitcoin-bolt11")?;
    remove_payment_endpoint(session, identifier).await?;
    Ok(())
}
```

**Note:** Removing a non-existent Payment Endpoint returns the error surfaced by the Pubky SDK as `PaykitError::Transport`.

#### `get_payment_list`

Fetch the public Payment List for a public key. The result is empty when no Payment Endpoints are published.

```rust,ignore
use paykit_lib::{get_payment_list, PublicKey};

async fn demo(public_storage: &pubky::PublicStorage, pk: &PublicKey) -> paykit_lib::Result<()> {
    let payment_list = get_payment_list(public_storage, pk).await?;
    if payment_list.payment_endpoints.is_empty() {
        println!("payee published no Payment Endpoints yet");
    } else {
        for (identifier, payload) in &payment_list.payment_endpoints {
            println!(
                "identifier={} payload={}",
                identifier.as_str(),
                payload.as_str()
            );
        }
    }
    Ok(())
}
```

#### `get_payment_endpoint`

Convenience resolver for a single Payment Endpoint Identifier. Returns `Ok(None)` when the Payment Endpoint Payload is missing or empty.

```rust,ignore
use paykit_lib::{get_payment_endpoint, PaymentEndpointIdentifier, PublicKey};

async fn inspect(public_storage: &pubky::PublicStorage, pk: &PublicKey) -> paykit_lib::Result<()> {
    let bolt11 = PaymentEndpointIdentifier::new("bitcoin-bolt11")?;
    if let Some(endpoint) = get_payment_endpoint(public_storage, pk, &bolt11).await? {
        println!("bolt11 payload: {}", endpoint.as_str());
    } else {
        println!("no bolt11 Payment Endpoint published");
    }
    Ok(())
}
```

#### Consistency Note

`get_payment_list` first lists available Payment Endpoints and then fetches each one individually. Because Pubky public storage does not support atomic reads, a **race condition** exists: between the directory listing and the individual fetches, endpoints may be added, removed, or modified by the payee. The returned `PaymentList` is therefore a **best-effort snapshot**.

If a payment execution fails with an error suggesting the endpoint has been consumed or is no longer valid, callers should:

1. Re-fetch the specific endpoint via `get_payment_endpoint`.
2. Compare the newly retrieved `PaymentEndpointPayload` with the value used in the failed attempt.
3. If the Payment Endpoint Payload differs, retry the payment with the updated value.

### Private Payment Envelopes

Private Payment Envelopes are end-to-end encrypted via a Noise protocol handshake managed by `pubky-noise`. `PubkyNoiseEncryptor` handles encryption, file naming, and homeserver storage via `send_message`/`receive_message`.

Storage paths for private Paykit data are derived per-counterparty pair using `pubky_noise::path_derivation::derive_asymmetric_paths`. Each party writes to a different path than they read from (`write_path` vs `read_path`), preventing third parties from enumerating communication relationships. The base prefix is `/pub/paykit/v0/private`; the derived hex component is appended as a child segment. Within each derived folder, `pubky-noise` manages individual file slots using a counter-based scheme — Paykit does not control file names or locations for private Paykit data.

#### Handshake Initiation
- `initiate_encrypted_link(session, sender_secret_key, receiver_pubkey, outbox_client) -> Result<EncryptedLinkHandshake>`  
  Initializes a Noise XX handshake as the **initiator**. Returns a handshake handle to be driven forward with `advance_handshake`.
- `accept_encrypted_link(session, receiver_secret_key, sender_pubkey, outbox_client) -> Result<EncryptedLinkHandshake>`  
  Initializes a Noise XX handshake as the **responder**. Returns a handshake handle to be driven forward with `advance_handshake`.

**NOTE**: Due to the nature of Noise it is important that one party is the "initiator" and the other is the "responder". Sometimes it is impossible to determine the roles from user flow alone. One option is to compare the counterparty key to the local key and let the initiator be the one with the lexicographically bigger public key.

#### Handshake advancing
- `advance_handshake(handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress>`  
  Advances the handshake by one step. Returns `HandshakeProgress::Pending(handle)` when waiting for the counterparty, or `HandshakeProgress::Complete(EncryptedLink)` when finished. Polling-safe — the caller controls retry timing and timeouts. If a homeserver write fails during the handshake (`HomeserverWriteError`), the function automatically recovers from a pre-mutation snapshot and returns `Pending` so the caller's polling loop retries transparently. The maximum number of consecutive recovery attempts is configurable via `EncryptedLinkHandshake::set_max_recovery_attempts` (default: `DEFAULT_MAX_RECOVERY_ATTEMPTS`, 3). The recovery-attempt counter resets to zero after every successful step.

#### Handshake checkpointing / resumption
- `EncryptedLinkHandshake::snapshot() -> EncryptedLinkHandshakeSnapshot`  
  Captures the current in-progress handshake state.
- `EncryptedLinkHandshake::serialize() -> Vec<u8>`  
  Convenience method equivalent to `self.snapshot().serialize()`.
- `EncryptedLinkHandshake::config() -> &Arc<PubkyNoiseConfig>`  
  Access the shared Noise configuration for in-process handshake restore.
- `EncryptedLinkHandshakeSnapshot::serialize() -> Vec<u8>` / `EncryptedLinkHandshakeSnapshot::deserialize(bytes: &[u8]) -> Result<EncryptedLinkHandshakeSnapshot>` / `EncryptedLinkHandshakeSnapshot::recipient() -> &PublicKey`  
  Snapshot wire format helpers (same compact 197-byte `PubkyNoiseSessionState` format as link snapshots).
- `restore_encrypted_link_handshake(session, secret_key, remote_pubkey, outbox_client, snapshot) -> Result<EncryptedLinkHandshake>`  
  Cross-restart restore for an in-progress handshake.
- `restore_encrypted_link_handshake_from_config(config, remote_pubkey, snapshot) -> Result<EncryptedLinkHandshake>`  
  In-process restore for an in-progress handshake.

After handshake restore, recovery tuning resets to defaults: `recovery_attempts = 0` and `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`.

Snapshot bytes include sensitive key material and must be treated as secrets (store encrypted at rest; never log or expose them).

#### Private Payment Envelope exchange
- `set_private_payment_envelope(link: &mut EncryptedLink, envelope: &PrivatePaymentEnvelope) -> Result<()>`
  Serializes the complete Private Payment Envelope to JSON, encrypts it, and sends it via the Encrypted Link. The caller is responsible for managing the Payment Endpoints in the Payment List and passing the full map each time in `envelope.payment_endpoints`. The envelope includes a UUID-v4 `PaymentReference`; `PaymentReference::new_v4()` generates a fresh canonical reference. The serialized JSON must fit within `PUBKY_NOISE_MSG_LEN` (1000 bytes). Transient homeserver write failures are retried automatically up to `EncryptedLink::set_max_send_retries` times (default: `DEFAULT_MAX_SEND_RETRIES`, 3). Transport-phase homeserver write failures do not corrupt the Noise state, so retries are safe without snapshot-based recovery. Deterministic state, counter, nonce, or encryption errors fail immediately.
- `get_private_payment_envelope(link: &mut EncryptedLink) -> Result<Option<PrivatePaymentEnvelope>>`
  Receives and decrypts currently available Private Application Messages from the counterparty and returns the latest Private Payment Envelope, if one is available. `Ok(None)` means no Private Payment Envelope is currently available; it is distinct from an envelope with an empty `payment_endpoints` map. Private Payment Envelopes use Latest-State Message semantics: queued older envelopes are superseded by the newest one. Other supported message kinds remain buffered for their own typed receivers. Syntactically valid messages with unsupported `kind` values are logged and dropped rather than buffered indefinitely. Malformed Private Application Messages are ignored with diagnostics so they do not prevent later valid messages from being processed.

All Private Application Messages share one ordered encrypted stream. Private Payment Envelopes use Latest-State Message semantics and intentionally collapse older queued envelopes. Receipt Access uses Event Message semantics; `get_receipt_access` drains and returns all currently available Receipt Access messages in FIFO/send order. Unsupported syntactically valid Private Application Message kinds are logged and dropped by the shared dispatcher. The in-memory buffer for supported messages dispatched but not yet consumed by a typed helper is not crash-durable; callers that perform irreversible side effects after receiving Event Messages should persist their own app-level state alongside Encrypted Link snapshots/read counters.

#### Payment receipts

Receipts are split into two persisted artifacts:

1. The encrypted Receipt is stored on the issuer's homeserver under `/pub/paykit/v0/private/receipts/{PaymentReference}`.
2. A `ReceiptAccess` descriptor is sent to the counterparty over the existing Encrypted Link. It contains the `PaymentReference`, the Receipt Location, the encryption `algorithm`, and the symmetric Receipt Decryption Key.

The Receipt is encrypted with `XChaCha20Poly1305`; the storage location is used as authenticated associated data, so fetching the right ciphertext but decrypting it against a different location fails.

- `issue_receipt(session, link, draft: ReceiptDraft) -> Result<IssuedReceipt>`
  Builds a canonical `Receipt` from the caller's `ReceiptDraft`, fills in the recipient public key from `link`, generates a fresh `ReceiptDecryptionKey`, stores the encrypted receipt on the issuer's homeserver, then sends a `ReceiptAccess` message over Noise. Reissuing the same `PaymentReference` overwrites the same Receipt Location with new ciphertext and a new key; older Receipt Access descriptors for that reference may stop decrypting after a later successful reissue.
- `get_receipt_access(link: &mut EncryptedLink) -> Result<Vec<ReceiptAccess>>`
  Receives all currently available queued Receipt Access messages. Receipt Access uses Event Message semantics: every Receipt Access message matters, and older Receipt Access descriptors are not collapsed when newer ones arrive. An empty vector means no Receipt Access messages are currently available. Calling `get_private_payment_envelope` will not discard Receipt Access messages; they remain buffered for `get_receipt_access`.
- `ReceiptAccess::location_for(reference) -> String`
  Returns Paykit's canonical Receipt Location for an encrypted Receipt.
- `Receipt::encrypt(&self, key) -> Result<String>` / `Receipt::decrypt(encrypted_json, key, location) -> Result<Receipt>`
  Encrypts or decrypts Receipts using `XChaCha20Poly1305`. Encryption derives the canonical Receipt Location from the Receipt's `PaymentReference` and authenticates that location as AAD. Pass the exact location from the access descriptor when decrypting; it is authenticated as AAD. Decryption also rejects plaintext whose internal reference does not match the authenticated location.
- `decrypt_receipt(encrypted_json, key, location) -> Result<Receipt>`
  Convenience wrapper around `Receipt::decrypt`. Incoming Receipt Access descriptors are accepted only when `location` equals Paykit's canonical Receipt Location for their `PaymentReference`.

Receipt Decryption Keys are sensitive. `ReceiptDecryptionKey`, `ReceiptAccess`, and `IssuedReceipt` redact key material from formatted output through the key's custom `Debug`/`Display`, but callers must still avoid logging or persisting the raw `ReceiptDecryptionKey::as_str()` value outside secure storage.

`issue_receipt` stores first and sends access second. If the process crashes or the Noise send ultimately fails after storage succeeds, an encrypted receipt can remain on the issuer's homeserver without the counterparty receiving access. Apps that need stronger delivery guarantees should track receipt issuance in durable app state and retry/reconcile at the application layer.

#### Termination
- `close_encrypted_link(link: EncryptedLink) -> Result<()>`  
  Closes the Noise session and releases resources.

### Handshake Polling Patterns

The caller controls the polling strategy for `advance_handshake`. Two common patterns:

**Fixed interval:**

```rust,ignore
use std::time::Duration;
use paykit_lib::{advance_handshake, HandshakeProgress, EncryptedLinkHandshake};

async fn poll_fixed(mut handshake: EncryptedLinkHandshake) -> paykit_lib::Result<paykit_lib::EncryptedLink> {
    loop {
        match advance_handshake(handshake).await? {
            HandshakeProgress::Pending(h) => {
                handshake = h;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            HandshakeProgress::Complete(link) => return Ok(link),
        }
    }
}
```

**With timeout:**

```rust,ignore
use std::time::{Duration, Instant};
use paykit_lib::{advance_handshake, HandshakeProgress, EncryptedLinkHandshake};

async fn poll_with_timeout(mut handshake: EncryptedLinkHandshake) -> paykit_lib::Result<paykit_lib::EncryptedLink> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() > deadline {
            return Err(paykit_lib::PaykitError::Transport {
                context: "handshake timed out".into(),
                source: anyhow::anyhow!("deadline exceeded"),
            });
        }
        match advance_handshake(handshake).await? {
            HandshakeProgress::Pending(h) => {
                handshake = h;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            HandshakeProgress::Complete(link) => return Ok(link),
        }
    }
}
```

### Session Resumption

The handshake checkpointing API above covers **in-progress** handshakes.

An established `EncryptedLink` can be snapshotted, serialized to bytes, persisted to durable storage, and later restored without re-doing the Noise handshake. This enables session resumption after app restarts or in-process recovery.

**Snapshot and serialize:**

- `EncryptedLink::snapshot() -> EncryptedLinkSnapshot`  
  Captures the current Encrypted Link state (transport keys, nonce counters, counterparty identity) as a serializable snapshot.
- `EncryptedLink::serialize() -> Vec<u8>`  
  Convenience method equivalent to `self.snapshot().serialize()`.
- `EncryptedLink::config() -> &Arc<PubkyNoiseConfig>`  
  Access the shared Noise configuration for in-process restore via `restore_encrypted_link_from_config`.

**Snapshot type:**

- `EncryptedLinkSnapshot::serialize() -> Vec<u8>`  
  Serializes to a compact 197-byte binary format (the `pubky-noise` 0.1.0-rc5 `PubkyNoiseSessionState` wire format). The counterparty public key is embedded at bytes 165-196.
- `EncryptedLinkSnapshot::deserialize(bytes: &[u8]) -> Result<EncryptedLinkSnapshot>`  
  Reconstructs a snapshot from bytes, including the embedded recipient public key.
- `EncryptedLinkSnapshot::recipient() -> &PublicKey`  
  Access the counterparty's public key embedded in the snapshot.

Snapshots produced by `pubky-noise` `0.1.0-rc3` used the older 189-byte format and are not accepted by the current 197-byte deserializer. Re-establish the Encrypted Link before restoring if an app has persisted an older snapshot.

**Restore:**

- `restore_encrypted_link(session, secret_key, remote_pubkey, outbox_client, snapshot) -> Result<EncryptedLink>`  
  Cross-restart restore. Accepts a fresh `PubkySession` and the same secret key used in the original `initiate_encrypted_link` or `accept_encrypted_link` call. Internally builds a new `PubkyNoiseConfig`, replays all handshake messages from the homeservers through a fresh Noise state with the same ephemeral key material, transitions to transport mode, and sets nonces and transport slot counters from the saved state.
- `restore_encrypted_link_from_config(config, remote_pubkey, snapshot) -> Result<EncryptedLink>`  
  In-process restore. Reuses an existing `Arc<PubkyNoiseConfig>` (obtainable via `EncryptedLink::config()`) when the link needs rebuilding without an app restart.

After link restore, `max_send_retries` resets to `DEFAULT_MAX_SEND_RETRIES`. Call `EncryptedLink::set_max_send_retries` after restore if you need a non-default value.

**When to snapshot:**

Take a snapshot after the Encrypted Link is established and periodically after exchanging messages. The snapshot includes nonce counters that must stay in sync with the counterparty — restoring from a stale snapshot may cause nonce desynchronization. Persist the serialized bytes to durable storage so the session can be resumed after an app restart.

Snapshot bytes include sensitive key material and must be treated as secrets (store encrypted at rest; never log or expose them).

```rust,ignore
// After establishing the link:
let link: EncryptedLink = /* ... handshake complete ... */;
let bytes = link.serialize();
save_to_disk(&bytes);

// After app restart:
let bytes = load_from_disk();
let snapshot = EncryptedLinkSnapshot::deserialize(&bytes)?;
let remote_pubkey = snapshot.recipient().clone();
let mut link = restore_encrypted_link(
    fresh_session, secret_key, &remote_pubkey, outbox_client, snapshot,
).await?;
// Continue using set_private_payment_envelope / get_private_payment_envelope
```

## Exports

The crate exports:

- `PAYKIT_PATH_PREFIX` (`/pub/paykit/v0/`) and `PAYKIT_PRIVATE_PATH_PREFIX` (`/pub/paykit/v0/private`) to standardize Pubky path construction.
- `set_payment_endpoint`, `remove_payment_endpoint`, `get_payment_list`, and `get_payment_endpoint` for Public Payment Endpoint operations over `pubky::PubkySession` and `pubky::PublicStorage`.
- `EncryptedLink`, `EncryptedLinkHandshake`, `HandshakeProgress`, `EncryptedLinkSnapshot`, `EncryptedLinkHandshakeSnapshot` for Encrypted Link types.
- `initiate_encrypted_link`, `accept_encrypted_link`, `advance_handshake`, `close_encrypted_link`, `set_private_payment_envelope`, `get_private_payment_envelope` for Encrypted Link and Private Payment Envelope operations.
- `ReceiptDraft`, `Receipt`, `ReceiptAccess`, `IssuedReceipt`, `ReceiptDecryptionKey`, `issue_receipt`, `get_receipt_access`, and `decrypt_receipt` for encrypted receipt issuance, access delivery, and decryption.
- `restore_encrypted_link`, `restore_encrypted_link_from_config`, `restore_encrypted_link_handshake`, `restore_encrypted_link_handshake_from_config` for session resumption after app restart or in-process recovery.
- `DEFAULT_MAX_RECOVERY_ATTEMPTS`, `DEFAULT_MAX_SEND_RETRIES` for configurable retry/recovery limits.
- `pubky_noise` re-export for advanced callers that need direct access to the encryption layer.
