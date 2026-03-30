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
  Advances the handshake by one step. Returns `HandshakeProgress::Pending(handle)` when waiting for the peer, or `HandshakeProgress::Complete(EncryptedLink)` when finished. Polling-safe — the caller controls retry timing and timeouts.
- `close_encrypted_link(link: EncryptedLink) -> Result<()>`  
  Closes the Noise session and releases resources.
- `set_private_payments(link: &mut EncryptedLink, entries: &HashMap<MethodId, EndpointData>) -> Result<()>`  
  Serializes the complete payments map to JSON, encrypts it, and sends it via the encrypted link. The caller is responsible for managing the map (adding/removing entries) and passing the full map each time. The serialized JSON must fit within `PUBKY_NOISE_MSG_LEN` (1000 bytes).
- `get_private_payments(link: &mut EncryptedLink) -> Result<SupportedPayments>`  
  Receives and decrypts the private payments map from the remote peer. Returns an empty map when no messages are available.

Storage paths for private data are derived per-peer-pair using `pubky_noise::path_derivation::derive_asymmetric_paths`. Each party writes to a different path than they read from (`write_path` vs `read_path`), preventing third parties from enumerating communication relationships. The base prefix is `/pub/paykit/v0/private`; the derived hex component is appended as a child segment. Within each derived folder, `pubky-noise` manages individual file slots using a counter-based scheme — Paykit does not control file names or locations for private data.

### Contacts & Profiles

- `get_known_contacts(reader: impl UnauthenticatedTransportRead, owner: &PublicKey) -> Result<Vec<PublicKey>>`  
  Retrieve all known contacts by listing `/pub/pubky.app/follows/`. Returns an empty vector when none are stored.
- `get_profile(reader: &PubkyUnauthenticatedTransport, user: &PublicKey) -> Result<Profile>` *(pubky feature)*  
  Fetch and parse a user's profile from `/pub/pubky.app/profile.json`.

Method/endpoint naming follows the PMIP consensus described in the repository root `README.md`. Each API returns well-typed structures (enums/structs) that mirror the protocol specification so downstream clients can share the same serialization layer.  
When the `pubky` feature is enabled the crate exports:

- `transport::pubky::PAYKIT_PATH_PREFIX` (`/pub/paykit/v0/`) and `PUBKY_FOLLOWS_PATH` (`/pub/pubky.app/follows/`) to standardize path construction.  
- `PubkyAuthenticatedTransport` (wraps `PubkySession`) and `PubkyUnauthenticatedTransport` (wraps `pubky::PublicStorage`) as ready-to-use adapters that satisfy the public payment traits above.
- `EncryptedLink`, `EncryptedLinkHandshake`, `HandshakeProgress`, `initiate_encrypted_link`, `accept_encrypted_link`, `advance_handshake`, `close_encrypted_link`, `set_private_payments`, `get_private_payments` for private encrypted payment operations.
- `pubky_noise` re-export for advanced callers that need direct access to the encryption layer.
