# Paykit Library

Stateless Rust crate that implements the Paykit transport layer. It provides helpers for both **public** payment endpoints (stored as plaintext files on the homeserver) and **private** payment endpoints (end-to-end encrypted via `pubky-noise`'s Noise protocol), while delegating authentication and session management to callers.

Paykit relies on a **transport** protocol for network *communication* between peers.

The default transport protocol in this implementation is [pubky](https://pubky.org/), enabled via the default feature flag `pubky`.

## Auth & Dependency Injection

- **Public endpoints** use the generic transport traits (`AuthenticatedTransport` / `UnauthenticatedTransportRead`). Instead of hard-coding `PubkySession`, public APIs accept any type implementing these traits. The crate provides adapters so callers can wrap [`pubky::PubkySession`](https://docs.rs/pubky/0.6.0/pubky/struct.PubkySession.html) or provide mocks for tests.
- **Private endpoints** bypass the transport traits entirely. They use `pubky-noise`'s `PubkyNoiseEncryptor` for Noise-encrypted messaging, which handles both encryption and homeserver I/O. Private payment functions accept an `EncryptedLink` (established via a Noise handshake) and are gated behind the `pubky` feature.
- Public reads only require the `UnauthenticatedTransportRead` trait, keeping unauthenticated flows lightweight. Session lifecycle, capability scoping, and key rotation stay outside this crate.
- The `pubky` feature flag (enabled by default) wires in Pubky adapters under `transport::pubky` and enables the private payment helpers. Disable it if you want to use custom transports for public endpoints only.

## Timeout Handling

The transport traits intentionally do **not** enforce timeouts. Each transport implementation is responsible for configuring appropriate timeout behaviour at its own layer. A slow or unresponsive backend will block the caller indefinitely unless the underlying transport applies a timeout.

For the Pubky adapter the underlying SDK handles this via [`PubkyHttpClientBuilder::request_timeout`](https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout):

```rust
use std::time::Duration;
let client = PubkyHttpClient::builder()
    .request_timeout(Duration::from_secs(10))
    .build()?;
```

Custom transport implementations should apply equivalent safeguards (e.g. per-request deadlines, connect timeouts) before passing the transport to Paykit APIs.

## Core Types

### `MethodId`

Identifier for a payment method specification (e.g. `"lightning"`, `"onchain"`, `"bolt11"`).
`MethodId` is validated at construction time to prevent path injection attacks.

```rust
// Construction is fallible:
let method = MethodId::new("lightning")?;

// Read access via as_str(), Display, or AsRef<str>:
println!("{}", method.as_str());
```

**Allowed values:** 1-64 ASCII characters from the set `[a-zA-Z0-9_-.]`. The value must not consist solely of dots (`.` and `..` are rejected as path traversal components). Slashes, null bytes, spaces, and other special characters are rejected.

`MethodId::new()` returns `Err(PaykitError::Validation(...))` for invalid input.

### `EndpointData`

Serialized payload served by a payment endpoint (UTF-8 text such as JSON, lnurl, etc.).

```rust
let data = EndpointData::new("{\"bolt11\":\"ln...\"}");
let payload: &str = data.as_str();
let owned: String = data.into_inner();
```

### `PaykitError`

Domain error enum with the following variants:

| Variant | Meaning |
|---------|---------|
| `Transport { context, source }` | Network or SDK failure |
| `NotFound(String)` | Requested resource does not exist (404/GONE) |
| `InvalidData { context, source }` | Fetched data is corrupt or structurally invalid |
| `Validation(String)` | Caller-supplied input failed validation (e.g. invalid `MethodId`) |

## API Surface

### Public Payment Endpoints

These functions use the transport traits (`AuthenticatedTransport` / `UnauthenticatedTransportRead`) and work with any backend.

- `set_payment_endpoint(client: impl AuthenticatedTransport, method: MethodId, data: EndpointData) -> Result<()>`  
  Store or update a payee-owned endpoint using the caller's authenticated client.
- `remove_payment_endpoint(client: impl AuthenticatedTransport, method: MethodId) -> Result<()>`  
  Remove previously published endpoint data for a given method.
- `get_payment_list(reader: impl UnauthenticatedTransportRead, payee: PublicKey) -> Result<SupportedPayments>`  
  Resolve the supported methods document for a public key. The result is empty when no endpoints are published.
- `get_payment_endpoint(reader: impl UnauthenticatedTransportRead, payee: PublicKey, method: &MethodId) -> Result<Option<EndpointData>>`  
  Convenience resolver for a single method. Returns `Ok(None)` when the endpoint is missing or empty.

### Private Payment Endpoints (`pubky` feature)

Private payments are end-to-end encrypted via a Noise protocol handshake managed by `pubky-noise`. They bypass the transport traits entirely — `PubkyNoiseEncryptor` handles encryption, file naming, and homeserver storage via `send_message`/`receive_message`.

- `initiate_encrypted_link(session, sender_secret_key, receiver_pubkey, outbox_client) -> Result<EncryptedLinkHandshake>`  
  Initializes a Noise XX handshake as the **initiator**. Returns a handshake handle to be driven forward with `advance_handshake`.
- `accept_encrypted_link(session, receiver_secret_key, sender_pubkey, outbox_client) -> Result<EncryptedLinkHandshake>`  
  Initializes a Noise XX handshake as the **responder**. Returns a handshake handle to be driven forward with `advance_handshake`.
- `advance_handshake(handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress>`  
  Advances the handshake by one step. Returns `HandshakeProgress::Pending(handle)` when waiting for the peer, or `HandshakeProgress::Complete(EncryptedLink)` when finished. Polling-safe — the caller controls retry timing and timeouts. If a homeserver write fails during the handshake (`HomeserverWriteError`), the function automatically recovers from a pre-mutation snapshot and returns `Pending` so the caller's polling loop retries transparently. The maximum number of consecutive recovery attempts is configurable via `EncryptedLinkHandshake::set_max_recovery_attempts` (default: `DEFAULT_MAX_RECOVERY_ATTEMPTS`, 3). The counter resets to zero after every successful step.
- `close_encrypted_link(link: EncryptedLink) -> Result<()>`  
  Closes the Noise session and releases resources.
- `set_private_payments(link: &mut EncryptedLink, entries: &HashMap<MethodId, EndpointData>) -> Result<()>`  
  Serializes the complete payments map to JSON, encrypts it, and sends it via the encrypted link. The caller is responsible for managing the map (adding/removing entries) and passing the full map each time. The serialized JSON must fit within `PUBKY_NOISE_MSG_LEN` (1000 bytes). Transient `send_message` failures are retried automatically up to `EncryptedLink::set_max_send_retries` times (default: `DEFAULT_MAX_SEND_RETRIES`, 3). Transport-phase send failures do not corrupt the Noise state, so retries are safe without snapshot-based recovery.
- `get_private_payments(link: &mut EncryptedLink) -> Result<SupportedPayments>`  
  Receives and decrypts the private payments map from the remote peer. Returns an empty map when no messages are available.

Storage paths for private data are derived per-peer-pair using `pubky_noise::path_derivation::derive_asymmetric_paths`. Each party writes to a different path than they read from (`write_path` vs `read_path`), preventing third parties from enumerating communication relationships. The base prefix is `/pub/paykit/v0/private`; the derived hex component is appended as a child segment. Within each derived folder, `pubky-noise` manages individual file slots using a counter-based scheme — Paykit does not control file names or locations for private data.

### Session Resumption (`pubky` feature)

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

**When to snapshot:**

Take a snapshot after the link is established and periodically after exchanging messages. The snapshot includes nonce counters that must stay in sync with the remote peer — restoring from a stale snapshot may cause nonce desynchronization. Persist the serialized bytes to durable storage so the session can be resumed after an app restart.

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

### Contacts & Profiles

- `get_known_contacts(reader: impl UnauthenticatedTransportRead, owner: &PublicKey) -> Result<Vec<PublicKey>>`  
  Retrieve all known contacts by listing `/pub/pubky.app/follows/`. Returns an empty vector when none are stored.
- `get_profile(reader: &PubkyUnauthenticatedTransport, user: &PublicKey) -> Result<Profile>` *(pubky feature)*  
  Fetch and parse a user's profile from `/pub/pubky.app/profile.json`.

Method/endpoint naming follows the PMIP consensus described in the repository root `README.md`. Each API returns well-typed structures (enums/structs) that mirror the protocol specification so downstream clients can share the same serialization layer.  
When the `pubky` feature is enabled the crate exports:

- `transport::pubky::PAYKIT_PATH_PREFIX` (`/pub/paykit/v0/`) and `PUBKY_FOLLOWS_PATH` (`/pub/pubky.app/follows/`) to standardize path construction.  
- `PubkyAuthenticatedTransport` (wraps `PubkySession`) and `PubkyUnauthenticatedTransport` (wraps `pubky::PublicStorage`) as ready-to-use adapters that satisfy the public payment traits above.
- `EncryptedLink`, `EncryptedLinkHandshake`, `HandshakeProgress`, `EncryptedLinkSnapshot` for private encrypted payment types.
- `initiate_encrypted_link`, `accept_encrypted_link`, `advance_handshake`, `close_encrypted_link`, `set_private_payments`, `get_private_payments` for private encrypted payment operations.
- `restore_encrypted_link`, `restore_encrypted_link_from_config` for session resumption after app restart or in-process recovery.
- `DEFAULT_MAX_RECOVERY_ATTEMPTS`, `DEFAULT_MAX_SEND_RETRIES` for configurable retry/recovery limits.
- `pubky_noise` re-export for advanced callers that need direct access to the encryption layer.
