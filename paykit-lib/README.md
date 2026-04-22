# Paykit Library

Stateless Rust crate that implements the Paykit transport layer. It provides helpers for both **public** payment endpoints (stored as plaintext files on the homeserver) and **private** payment endpoints (end-to-end encrypted via `pubky-noise`'s Noise protocol), while delegating authentication and session management to callers.

Paykit relies on a **transport** protocol for network *communication* between peers.

The default transport protocol in this implementation is [pubky](https://pubky.org/), enabled via the default feature flag `pubky`.

## Quick Start

Add `paykit-lib` to your `Cargo.toml`.

To use only the generic transport traits without the Pubky adapters:

```toml
[dependencies]
paykit-lib = { version = "x.x.x", default-features = false }
```

Minimal example — store and retrieve a public payment endpoint:

```rust,ignore
use paykit_lib::{
    set_payment_endpoint, get_payment_endpoint, get_payment_list,
    MethodId, EndpointData,
};

// Create validated types.
let method = MethodId::new("bitcoin-bolt11")?;
let data = EndpointData::new("lnbc1...");

// Store an endpoint (requires an AuthenticatedTransport).
set_payment_endpoint(&client, method.clone(), data).await?;

// Read it back (requires an UnauthenticatedTransportRead).
let endpoint = get_payment_endpoint(&reader, &payee_pubkey, &method).await?;

// List all published endpoints for a payee.
let payments = get_payment_list(&reader, &payee_pubkey).await?;
for (method, data) in &payments.entries {
    println!("{}: {}", method.as_str(), data.as_str());
}
```

## Core Types

### `MethodId`

Identifier for a payment method specification (e.g. `"bitcoin-p2sh"`, `"bitcoin-bolt11"` basically anything parties agree on).
`MethodId` is validated at construction time to prevent path injection attacks.

```rust
use paykit_lib::MethodId;

// Construction is fallible:
let method = MethodId::new("bitcoin-bolt11").unwrap();
assert_eq!(method.as_str(), "bitcoin-bolt11");

// Path traversal is rejected:
assert!(MethodId::new("../etc/passwd").is_err());
```

**Allowed values:** 1-64 ASCII characters from the set `[a-zA-Z0-9_-.]`. The value must not consist solely of dots (`.` and `..` are rejected as path traversal components), and must not be the reserved identifier `private` (used by private payment storage). Slashes, null bytes, spaces, and other special characters are rejected.

`MethodId::new()` returns `Err(PaykitError::Validation(...))` for invalid input.

Read access is available via `as_str()`, `Display`, and `AsRef<str>`.

**Naming convention:** `MethodId` values are opaque to the library, but peers need to agree on them to interoperate. A recommended (non-obligatory) convention for the shape of these identifiers is described in [../specs/payment-endpoint-identifier.md](../specs/payment-endpoint-identifier.md).

### `EndpointData`

Serialized payload served by a payment endpoint (UTF-8 text such as JSON, lnurl, etc.).

```rust
use paykit_lib::EndpointData;

let data = EndpointData::new("ln...");
let payload: &str = data.as_str();
let owned: String = data.into_inner();
```

### `SupportedPayments`

Collection of payment entries keyed by method identifiers. Returned by `get_payment_list` and `get_private_payments`.

```rust,ignore
use paykit_lib::SupportedPayments;

let payments: SupportedPayments = get_payment_list(&reader, &payee).await?;

// Access the underlying map:
for (method, data) in &payments.entries {
    println!("method={} payload={}", method.as_str(), data.as_str());
}

// Check if empty:
if payments.entries.is_empty() {
    println!("no endpoints published");
}
```

Private payments follow the same `SupportedPayments` layout:

```rust,ignore
use paykit_lib::{get_private_payments, SupportedPayments};

let payments: SupportedPayments = get_private_payments(&mut link).await?;
for (method, data) in &payments.entries {
    println!("method={} payload={}", method.as_str(), data.as_str());
}

// Check if empty:
if payments.entries.is_empty() {
    println!("no endpoints published");
}
```

The `entries` field is a `HashMap<MethodId, EndpointData>`.

### `PaykitError`

Domain error enum with the following variants:

| Variant | Meaning |
|---------|---------|
| `Transport { context, source }` | Network or SDK failure |
| `NotFound(String)` | Requested resource does not exist (404/GONE) |
| `InvalidData { context, source }` | Fetched data is corrupt or structurally invalid |
| `Validation(String)` | Caller-supplied input failed validation (e.g. invalid `MethodId`) |

The `source` field on `Transport` and `InvalidData` is backed by [`anyhow::Error`] so callers can downcast via `anyhow::Error::downcast_ref` when they need the original typed error.

### `Result<T>`

All public APIs return `paykit_lib::Result<T>`, which is an alias for `std::result::Result<T, PaykitError>`.

## Auth & Dependency Injection

- **Public endpoints** use the generic transport traits (`AuthenticatedTransport` / `UnauthenticatedTransportRead`). Instead of hard-coding `PubkySession`, public APIs accept any type implementing these traits. The crate provides adapters so callers can wrap [`pubky::PubkySession`](https://docs.rs/pubky/latest/pubky/struct.PubkySession.html) or provide mocks for tests.
- **Private endpoints** bypass the transport traits entirely. They use `pubky-noise`'s `PubkyNoiseEncryptor` for Noise-encrypted messaging, which handles both encryption and homeserver I/O. Private payment functions accept an `EncryptedLink` (established via a Noise handshake) and are gated behind the `pubky` feature.
- Public reads only require the `UnauthenticatedTransportRead` trait, keeping unauthenticated flows lightweight. Session lifecycle, capability scoping, and key rotation stay outside this crate.
- The `pubky` feature flag (enabled by default) wires in Pubky adapters under `transport::pubky` and enables the private payment helpers. Disable it if you want to use custom transports for public endpoints only.

## Timeout Handling

The transport traits intentionally do **not** enforce timeouts. Each transport implementation is responsible for configuring appropriate timeout behaviour at its own layer. A slow or unresponsive backend will block the caller indefinitely unless the underlying transport applies a timeout.

For the Pubky adapter the underlying SDK handles this via [`PubkyHttpClientBuilder::request_timeout`](https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout):

```rust,ignore
use std::time::Duration;
let client = PubkyHttpClient::builder()
    .request_timeout(Duration::from_secs(10))
    .build()?;
```

Custom transport implementations should apply equivalent safeguards (e.g. per-request deadlines, connect timeouts) before passing the transport to Paykit APIs.

## API Surface

### Public Payment Endpoints

These functions use the transport traits (`AuthenticatedTransport` / `UnauthenticatedTransportRead`) and work with any backend.

#### `set_payment_endpoint`

Store or update a payee-owned endpoint using the caller's authenticated client.

```rust,ignore
use paykit_lib::{set_payment_endpoint, MethodId, EndpointData, AuthenticatedTransport};

async fn demo(client: &impl AuthenticatedTransport) -> paykit_lib::Result<()> {
    // NOTE: parties need to agree on method ids in order to understand each other

    let method = MethodId::new("bitcoin-bolt11")?;
    let data = EndpointData::new("ln...");
    set_payment_endpoint(client, method, data).await?;

    let method = MethodId::new("bitcoin-p2wpkh")?;
    let data = EndpointData::new("bc1...");
    set_payment_endpoint(client, method, data).await?;
    // or 
    let method = MethodId::new("bitcoin-p2tr")?;
    let data = EndpointData::new("bc1...");
    set_payment_endpoint(client, method, data).await?;

    Ok(())
}
```

#### `remove_payment_endpoint`

Remove previously published endpoint data for a given method.

```rust,ignore
use paykit_lib::{remove_payment_endpoint, MethodId, AuthenticatedTransport};

async fn demo(client: &impl AuthenticatedTransport) -> paykit_lib::Result<()> {
    let method = MethodId::new("bitcoin-bolt11")?;
    remove_payment_endpoint(client, method).await?;
    Ok(())
}
```

**Note:** Removing a non-existent endpoint returns an error (`PaykitError::NotFound` or `PaykitError::Transport` depending on the transport implementation).

#### `get_payment_list`

Resolve the supported methods document for a public key. The result is empty when no endpoints are published.

```rust,ignore
use paykit_lib::{get_payment_list, UnauthenticatedTransportRead, PublicKey};

async fn demo(reader: &impl UnauthenticatedTransportRead, pk: &PublicKey) -> paykit_lib::Result<()> {
    let payments = get_payment_list(reader, pk).await?;
    if payments.entries.is_empty() {
        println!("payee published no endpoints yet");
    } else {
        for (method, data) in &payments.entries {
            println!("method={} payload={}", method.as_str(), data.as_str());
        }
    }
    Ok(())
}
```

#### `get_payment_endpoint`

Convenience resolver for a single method. Returns `Ok(None)` when the endpoint is missing or empty.

```rust,ignore
use paykit_lib::{get_payment_endpoint, MethodId, PublicKey, UnauthenticatedTransportRead};

async fn inspect(reader: &impl UnauthenticatedTransportRead, pk: &PublicKey) -> paykit_lib::Result<()> {
    let bolt11 = MethodId::new("bitcoin-bolt11")?;
    if let Some(endpoint) = get_payment_endpoint(reader, pk, &bolt11).await? {
        println!("bolt11 endpoint: {}", endpoint.as_str());
    } else {
        println!("no bolt11 endpoint published");
    }
    Ok(())
}
```

#### Consistency Note

`get_payment_list` first lists available payment method entries and then fetches each one individually. Because the underlying transport does not support atomic reads, a **race condition** exists: between the directory listing and the individual fetches, endpoints may be added, removed, or modified by the payee. The returned `SupportedPayments` is therefore a **best-effort snapshot**.

If a payment execution fails with an error suggesting the endpoint has been consumed or is no longer valid, callers should:

1. Re-fetch the specific endpoint via `get_payment_endpoint`.
2. Compare the newly retrieved `EndpointData` with the value used in the failed attempt.
3. If the endpoint data differs, retry the payment with the updated value.

### Private Payment Endpoints (`pubky` feature)

Private payments are end-to-end encrypted via a Noise protocol handshake managed by `pubky-noise`. They bypass the transport traits entirely — `PubkyNoiseEncryptor` handles encryption, file naming, and homeserver storage via `send_message`/`receive_message`.

Storage paths for private data are derived per-peer-pair using `pubky_noise::path_derivation::derive_asymmetric_paths`. Each party writes to a different path than they read from (`write_path` vs `read_path`), preventing third parties from enumerating communication relationships. The base prefix is `/pub/paykit/v0/private`; the derived hex component is appended as a child segment. Within each derived folder, `pubky-noise` manages individual file slots using a counter-based scheme — Paykit does not control file names or locations for private data.

#### Handshake Initiation
- `initiate_encrypted_link(session, sender_secret_key, receiver_pubkey, outbox_client) -> Result<EncryptedLinkHandshake>`  
  Initializes a Noise XX handshake as the **initiator**. Returns a handshake handle to be driven forward with `advance_handshake`.
- `accept_encrypted_link(session, receiver_secret_key, sender_pubkey, outbox_client) -> Result<EncryptedLinkHandshake>`  
  Initializes a Noise XX handshake as the **responder**. Returns a handshake handle to be driven forward with `advance_handshake`.

**NOTE**: Due to nature of noise it is important that one of the peers is "initiator" and another is "responder". Sometimes it is impossible to determine who is who based on user flow. One option is to compare counterparty key to own key and let the initiator be the one with lexicographically bigger public key.

#### Handshake advancing
- `advance_handshake(handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress>`  
  Advances the handshake by one step. Returns `HandshakeProgress::Pending(handle)` when waiting for the peer, or `HandshakeProgress::Complete(EncryptedLink)` when finished. Polling-safe — the caller controls retry timing and timeouts. If a homeserver write fails during the handshake (`HomeserverWriteError`), the function automatically recovers from a pre-mutation snapshot and returns `Pending` so the caller's polling loop retries transparently. The maximum number of consecutive recovery attempts is configurable via `EncryptedLinkHandshake::set_max_recovery_attempts` (default: `DEFAULT_MAX_RECOVERY_ATTEMPTS`, 3). The counter resets to zero after every successful step.

#### Handshake checkpointing / resumption
- `EncryptedLinkHandshake::snapshot() -> EncryptedLinkHandshakeSnapshot`  
  Captures the current in-progress handshake state.
- `EncryptedLinkHandshake::serialize() -> Vec<u8>`  
  Convenience method equivalent to `self.snapshot().serialize()`.
- `EncryptedLinkHandshake::config() -> &Arc<PubkyNoiseConfig>`  
  Access the shared Noise configuration for in-process handshake restore.
- `EncryptedLinkHandshakeSnapshot::serialize() -> Vec<u8>` / `EncryptedLinkHandshakeSnapshot::deserialize(bytes: &[u8]) -> Result<EncryptedLinkHandshakeSnapshot>` / `EncryptedLinkHandshakeSnapshot::recipient() -> &PublicKey`  
  Snapshot wire format helpers (same compact 189-byte `PubkyNoiseSessionState` format as link snapshots).
- `restore_encrypted_link_handshake(session, secret_key, remote_pubkey, outbox_client, snapshot) -> Result<EncryptedLinkHandshake>`  
  Cross-restart restore for an in-progress handshake.
- `restore_encrypted_link_handshake_from_config(config, remote_pubkey, snapshot) -> Result<EncryptedLinkHandshake>`  
  In-process restore for an in-progress handshake.

After handshake restore, recovery tuning resets to defaults: `recovery_attempts = 0` and `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`.

Snapshot bytes include sensitive key material and must be treated as secrets (store encrypted at rest; never log or expose them).

#### Payment endpoint exchange
- `set_private_payments(link: &mut EncryptedLink, entries: &HashMap<MethodId, EndpointData>) -> Result<()>`  
  Serializes the complete payments map to JSON, encrypts it, and sends it via the encrypted link. The caller is responsible for managing the map (adding/removing entries) and passing the full map each time. The serialized JSON must fit within `PUBKY_NOISE_MSG_LEN` (1000 bytes). Transient `send_message` failures are retried automatically up to `EncryptedLink::set_max_send_retries` times (default: `DEFAULT_MAX_SEND_RETRIES`, 3). Transport-phase send failures do not corrupt the Noise state, so retries are safe without snapshot-based recovery.
- `get_private_payments(link: &mut EncryptedLink) -> Result<SupportedPayments>`  
  Receives and decrypts private payments updates from the remote peer, drains currently unread queued updates, and returns the latest map. Returns an empty map when no messages are available.

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

### Session Resumption (`pubky` feature)

The handshake checkpointing API above covers **in-progress** handshakes.

An established `EncryptedLink` can be snapshotted, serialized to bytes, persisted to durable storage, and later restored without re-doing the Noise handshake. This enables session resumption after app restarts or in-process recovery.

**Snapshot and serialize:**

- `EncryptedLink::snapshot() -> EncryptedLinkSnapshot`  
  Captures the current link state (transport keys, nonce counters, peer identity) as a serializable snapshot.
- `EncryptedLink::serialize() -> Vec<u8>`  
  Convenience method equivalent to `self.snapshot().serialize()`.
- `EncryptedLink::config() -> &Arc<PubkyNoiseConfig>`  
  Access the shared Noise configuration for in-process restore via `restore_encrypted_link_from_config`.

**Snapshot type:**

- `EncryptedLinkSnapshot::serialize() -> Vec<u8>`  
  Serializes to a compact 189-byte binary format (the `pubky-noise` `PubkyNoiseSessionState` wire format). The remote peer's public key is embedded at bytes 157-188.
- `EncryptedLinkSnapshot::deserialize(bytes: &[u8]) -> Result<EncryptedLinkSnapshot>`  
  Reconstructs a snapshot from bytes, including the embedded recipient public key.
- `EncryptedLinkSnapshot::recipient() -> &PublicKey`  
  Access the counterparty's public key embedded in the snapshot.

**Restore:**

- `restore_encrypted_link(session, secret_key, remote_pubkey, outbox_client, snapshot) -> Result<EncryptedLink>`  
  Cross-restart restore. Accepts a fresh `PubkySession` and the same secret key used in the original `initiate_encrypted_link` or `accept_encrypted_link` call. Internally builds a new `PubkyNoiseConfig`, replays all handshake messages from the homeservers through a fresh Noise state with the same ephemeral key material, transitions to transport mode, and sets nonces/counter from the saved state.
- `restore_encrypted_link_from_config(config, remote_pubkey, snapshot) -> Result<EncryptedLink>`  
  In-process restore. Reuses an existing `Arc<PubkyNoiseConfig>` (obtainable via `EncryptedLink::config()`) when the link needs rebuilding without an app restart.

After link restore, `max_send_retries` resets to `DEFAULT_MAX_SEND_RETRIES`. Call `EncryptedLink::set_max_send_retries` after restore if you need a non-default value.

**When to snapshot:**

Take a snapshot after the link is established and periodically after exchanging messages. The snapshot includes nonce counters that must stay in sync with the remote peer — restoring from a stale snapshot may cause nonce desynchronization. Persist the serialized bytes to durable storage so the session can be resumed after an app restart.

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
// Continue using set_private_payments / get_private_payments
```

## Exports (`pubky` feature)

When the `pubky` feature is enabled the crate exports:

- `transport::pubky::PAYKIT_PATH_PREFIX` (`/pub/paykit/v0/`) to standardize path construction.
- `PubkyAuthenticatedTransport` (wraps `PubkySession`) and `PubkyUnauthenticatedTransport` (wraps `pubky::PublicStorage`) as ready-to-use adapters that satisfy the public payment traits above.
- `EncryptedLink`, `EncryptedLinkHandshake`, `HandshakeProgress`, `EncryptedLinkSnapshot`, `EncryptedLinkHandshakeSnapshot` for private encrypted payment types.
- `initiate_encrypted_link`, `accept_encrypted_link`, `advance_handshake`, `close_encrypted_link`, `set_private_payments`, `get_private_payments` for private encrypted payment operations.
- `restore_encrypted_link`, `restore_encrypted_link_from_config`, `restore_encrypted_link_handshake`, `restore_encrypted_link_handshake_from_config` for session resumption after app restart or in-process recovery.
- `DEFAULT_MAX_RECOVERY_ATTEMPTS`, `DEFAULT_MAX_SEND_RETRIES` for configurable retry/recovery limits.
- `pubky_noise` re-export for advanced callers that need direct access to the encryption layer.
