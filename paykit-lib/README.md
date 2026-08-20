# Paykit Library

Stateless Rust crate that implements Paykit Library functionality on [Pubky](https://pubky.org/). It provides helpers for **public** Payment Endpoints (stored as plaintext files on the homeserver), Private Payment Lists, Payment Requests, Encrypted Links, and Receipts, while delegating Pubky authentication and session management to callers.

Pubky is the only supported network/storage backend for this crate, and Pubky dependencies are unconditional.

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
    PaykitAppId, PaymentEndpointIdentifier, PaymentEndpointPayload,
};

// Create validated types.
let app_id = PaykitAppId::new("bitkit")?;
let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
let payload = PaymentEndpointPayload::new("lnbc1...");

// Store a Payment Endpoint using an authenticated PubkySession.
set_payment_endpoint(&session, &app_id, identifier.clone(), payload).await?;

// Read it back using pubky::PublicStorage.
let endpoint = get_payment_endpoint(&public_storage, &payee_pubkey, &app_id, &identifier).await?;

// List all Payment Endpoints published by this app for a payee.
let payment_list = get_payment_list(&public_storage, &payee_pubkey, &app_id).await?;
for (identifier, payload) in &payment_list.payment_endpoints {
    println!("{}: {}", identifier.as_str(), payload.as_str());
}
```

## Core Types

### `PaymentEndpointIdentifier`

Machine-readable identifier for a Payment Endpoint type (for example `"btc-bitcoin-p2tr"` or `"btc-lightning-bolt11"`).
`PaymentEndpointIdentifier` is validated at construction time to prevent path injection attacks.

```rust
use paykit_lib::PaymentEndpointIdentifier;

// Construction is fallible:
let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
assert_eq!(identifier.as_str(), "btc-lightning-bolt11");

// Path traversal is rejected:
assert!(PaymentEndpointIdentifier::new("../etc/passwd").is_err());
```

**Allowed values:** 1-64 ASCII characters from the set `[a-zA-Z0-9_-.]`. The value must not consist solely of dots (`.` and `..` are rejected as path traversal components), and `private` is reserved for Paykit storage. Slashes, null bytes, spaces, and other special characters are rejected.

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
use paykit_lib::{PaykitAppId, PaymentList};

let app_id = PaykitAppId::new("bitkit")?;
let payment_list: PaymentList = get_payment_list(&public_storage, &payee, &app_id).await?;

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

Private Payment Lists use a versioned `PrivatePaymentList` value. The Payment Endpoints still use the same `HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>` layout. Payment-specific correlation belongs in Payment Requests, Payment Proofs, and Receipts, not in endpoint publication:

```rust,ignore
use paykit_lib::{parse_private_payment_list_json, PrivateMessageKind};

let messages = link.receive_private_application_messages().await?;
for message in messages {
    if message.known_kind() == Some(PrivateMessageKind::PrivatePaymentList) {
        let list = parse_private_payment_list_json(&message.raw_json)?;
        for (identifier, payload) in &list.payment_endpoints {
            println!(
                "identifier={} payload={}",
                identifier.as_str(),
                payload.as_str()
            );
        }
    }
}
```

Private Payment Lists use Latest-State Message semantics per Paykit App at the
SDK/runtime layer: if one app queues several list messages, its latest valid
list supersedes its older list messages without replacing lists from other
apps. Malformed newer list messages do not supersede the latest valid state.
Receipt Access and Payment Request messages use Event Message
semantics, so SDK/runtime code must preserve every valid message in send order.

The `payment_endpoints` field is a `HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>`.

### Paykit Apps And App Registry

`PaykitAppId` identifies the app that owns a public Payment Endpoint or sent a
private message. `PaykitAppRegistry` is one public identity-wide record at
`/pub/paykit/v0/app-registry.json`; it lists participating apps, their coarse
capabilities, the identity-wide Noise public key, and optional default-app
preferences. It is not private SDK state.

Registries are bounded to 64 KiB, 64 applications, and 256 endpoint-specific
defaults so remote data cannot trigger unbounded parsing or endpoint lookups.

Public Payment Endpoints are stored under
`/pub/paykit/v0/apps/{app_id}/endpoints/`. Private Paykit messages share one
identity-wide Encrypted Link per counterparty and include `app_id` in their
versioned JSON.

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
- **Private Payment Lists** use `pubky-noise`'s `PubkyNoiseEncryptor` for Noise-encrypted messaging, which handles both encryption and homeserver I/O through Pubky. Private Payment List functions accept an `EncryptedLink` established via an Encrypted Link Handshake.
- Paykit stays stateless. Session creation, capability scoping, key rotation, account recovery, and client timeout configuration remain caller responsibilities.
- Public payment paths are centralized by `PAYKIT_PATH_PREFIX` (`/pub/paykit/v0`). Private Paykit message paths use `PAYKIT_PRIVATE_PATH_PREFIX` (`/pub/paykit/v0/private`) as the base for pubky-noise path derivation.

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
use paykit_lib::{set_payment_endpoint, PaykitAppId, PaymentEndpointIdentifier, PaymentEndpointPayload};

async fn demo(session: &pubky::PubkySession) -> paykit_lib::Result<()> {
    // NOTE: parties need to agree on Payment Endpoint Identifiers to interoperate.

    let app_id = PaykitAppId::new("bitkit")?;
    let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
    let payload = PaymentEndpointPayload::new("ln...");
    set_payment_endpoint(session, &app_id, identifier, payload).await?;

    let identifier = PaymentEndpointIdentifier::new("btc-bitcoin-p2wpkh")?;
    let payload = PaymentEndpointPayload::new("bc1...");
    set_payment_endpoint(session, &app_id, identifier, payload).await?;
    // or
    let identifier = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr")?;
    let payload = PaymentEndpointPayload::new("bc1...");
    set_payment_endpoint(session, &app_id, identifier, payload).await?;

    Ok(())
}
```

#### `remove_payment_endpoint`

Remove a previously published Payment Endpoint Payload for a given Payment Endpoint Identifier.

```rust,ignore
use paykit_lib::{remove_payment_endpoint, PaykitAppId, PaymentEndpointIdentifier};

async fn demo(session: &pubky::PubkySession) -> paykit_lib::Result<()> {
    let app_id = PaykitAppId::new("bitkit")?;
    let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
    remove_payment_endpoint(session, &app_id, identifier).await?;
    Ok(())
}
```

**Note:** Removing a non-existent Payment Endpoint succeeds, so cleanup can be retried safely.

#### `get_payment_list`

Fetch the public Payment List for a public key. The result is empty when no Payment Endpoints are published.

```rust,ignore
use paykit_lib::{get_payment_list, PaykitAppId, PublicKey};

async fn demo(public_storage: &pubky::PublicStorage, pk: &PublicKey) -> paykit_lib::Result<()> {
    let app_id = PaykitAppId::new("bitkit")?;
    let payment_list = get_payment_list(public_storage, pk, &app_id).await?;
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
use paykit_lib::{get_payment_endpoint, PaykitAppId, PaymentEndpointIdentifier, PublicKey};

async fn inspect(public_storage: &pubky::PublicStorage, pk: &PublicKey) -> paykit_lib::Result<()> {
    let app_id = PaykitAppId::new("bitkit")?;
    let bolt11 = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
    if let Some(endpoint) = get_payment_endpoint(public_storage, pk, &app_id, &bolt11).await? {
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

1. Re-fetch the specific app-owned endpoint via `get_payment_endpoint`.
2. Compare the newly retrieved `PaymentEndpointPayload` with the value used in the failed attempt.
3. If the Payment Endpoint Payload differs, retry the payment with the updated value.

### Private Payment Lists

Private Payment Lists are end-to-end encrypted via a Noise protocol handshake managed by `pubky-noise`. `PubkyNoiseEncryptor` handles encryption, file naming, and homeserver storage via `send_message`/`receive_message`.

Storage paths for private Paykit data are derived per-counterparty pair using `pubky_noise::path_derivation::derive_asymmetric_paths`. Each party writes to a different path than they read from (`write_path` vs `read_path`), preventing third parties from enumerating communication relationships. The base prefix is `/pub/paykit/v0/private`; the derived hex component is appended as a child segment. Within each derived folder, `pubky-noise` manages individual file slots using a counter-based scheme — Paykit does not control file names or locations for private Paykit data.

#### Handshake Initiation
- `initiate_encrypted_link(session, sender_noise_secret_key, receiver_identity_public_key, receiver_noise_public_key, outbox_client) -> Result<EncryptedLinkHandshake>`
  Initializes a Noise XX handshake as the **initiator**. Returns a handshake handle to be driven forward with `advance_handshake`.
- `accept_encrypted_link(session, receiver_noise_secret_key, sender_identity_public_key, sender_noise_public_key, outbox_client) -> Result<EncryptedLinkHandshake>`
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
- `EncryptedLinkHandshakeSnapshot::serialize() -> Vec<u8>` / `EncryptedLinkHandshakeSnapshot::deserialize(bytes: &[u8]) -> Result<EncryptedLinkHandshakeSnapshot>` / `EncryptedLinkHandshakeSnapshot::recipient() -> &PublicKey` / `EncryptedLinkHandshakeSnapshot::remote_noise_public_key() -> &PublicKey`
  Snapshot wire format helpers. Serialized snapshots contain the `pubky-noise` session state followed by the counterparty's 32-byte identity-wide Noise public key.
- `restore_encrypted_link_handshake(session, secret_key, remote_identity_public_key, outbox_client, snapshot) -> Result<EncryptedLinkHandshake>`
  Cross-restart restore for an in-progress handshake.
- `restore_encrypted_link_handshake_from_config(config, remote_identity_public_key, snapshot) -> Result<EncryptedLinkHandshake>`
  In-process restore for an in-progress handshake.

After handshake restore, recovery tuning resets to defaults: `recovery_attempts = 0` and `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`.

Snapshot bytes include sensitive key material and must be treated as secrets (store encrypted at rest; never log or expose them).

#### Private Payment List exchange
- `set_private_payment_list(link: &mut EncryptedLink, list: &PrivatePaymentList) -> Result<()>`
  Serializes the complete Private Payment List to JSON, encrypts it, and sends it via the Encrypted Link. The caller is responsible for managing the Payment Endpoints in the Payment List and passing the full map each time in `list.payment_endpoints`. The serialized JSON must fit within `PUBKY_NOISE_MSG_LEN` (1000 bytes). Transient homeserver write failures are retried automatically up to `EncryptedLink::set_max_send_retries` times (default: `DEFAULT_MAX_SEND_RETRIES`, 3). Transport-phase homeserver write failures do not corrupt the Noise state, so retries are safe without snapshot-based recovery. Deterministic state, counter, nonce, or encryption errors fail immediately.
- `parse_private_payment_list_json(json: &str) -> Result<PrivatePaymentList>`
  Stateless parser for callers or SDK layers that route messages from `EncryptedLink::receive_private_application_messages`.

All Private Application Messages share one ordered encrypted stream and carry a
source `app_id`. Use `EncryptedLink::receive_private_application_messages` when
a caller or SDK must route private message kinds durably. The raw JSON is
preserved even when parsed `version`/`kind`/`app_id` header fields are missing
or malformed. Callers that perform irreversible side effects after receiving
Event Messages should persist their own app-level handled/unhandled event state
before persisting a snapshot whose read counter has advanced past those
messages.

Private Paykit wire messages are closed-world JSON objects: unknown fields are rejected unless a field is explicitly defined as an open JSON object, such as Payment Request `metadata`, Payment Proof `proof`, or Receipt Metadata.

#### Payment Request exchange

Payment Requests are payee-initiated private protocol objects exchanged over an Encrypted Link. Their lifecycle messages (`paykit.payment_request`, acceptance, rejection, cancellation, and proof) use Event Message semantics. Paykit serializes, encrypts, sends, receives, and structurally validates these messages, but does not execute payments, schedule recurring jobs, manage wallet state, validate sender-role intent, or validate payment-method-specific proofs.

- `send_payment_request(link, app_id, event: &PaymentRequest) -> Result<()>`
  Sends a `paykit.payment_request` proposal with immutable terms. Terms include `PaymentAmount`, a payee-provided `PaymentReference`, required nullable `proposal_expires_at` (`None`/`null` means no protocol-level proposal expiry before acceptance), optional `Recurrence`, accepted Payment Endpoint Identifiers, and optional metadata.
- `serialize_payment_request_event(app_id, event: &PaymentRequestEvent) -> Result<String>`
  Serializes a Payment Request event to the exact JSON payload callers can persist before sending for crash-safe retry/idempotency.
- `parse_payment_request_event_message(message: &PrivateApplicationMessage) -> Option<PaymentRequestEventMessage>`
  Stateless parser for Payment Request events from the raw Private Application Message stream. Recognized but malformed events are returned with a validation error so callers can persist the raw payload before advancing their durable checkpoint.
- `send_payment_request_acceptance(link, app_id, event: &PaymentRequestAcceptance) -> Result<()>`
  Sends an explicit payer acceptance for a proposed Payment Request.
- `send_payment_request_rejection(link, app_id, event: &PaymentRequestRejection) -> Result<()>`
  Sends a payer rejection with an optional reason.
- `send_payment_request_cancellation(link, app_id, event: &PaymentRequestCancellation) -> Result<()>`
  Sends a unilateral cancellation with an optional reason.
- `send_payment_proof(link, app_id, event: &PaymentProof) -> Result<()>`
  Sends method-specific proof data for one concrete payment execution. The requested amount is inherited from immutable Payment Request terms and is not repeated in the generic Payment Proof. The opaque `proof` object is method-specific and has no required generic discriminator. The `payment_reference` must be copied from the accepted Payment Request; recurring proofs include a `BillingPeriod`.
- `PaymentProof::validate_for_request(&request) -> Result<()>`
  Validates stateless proof/request correlation fields: matching Payment Request ID and Payment Reference, one-time vs recurring Billing Period presence, Billing Period shape when present, and accepted Payment Endpoint Identifier. Caller-managed application or wallet state must still decide whether the request is known, accepted, proposal-expired before acceptance, rejected, cancelled, already processed or settled, whether the sender role is allowed for the message kind, or whether a recurring Billing Period is eligible under the request recurrence.

Payment Request and Receipt Access messages use Event Message semantics. Use `EncryptedLink::receive_private_application_messages`, persist the returned raw stream messages, then parse/reroute them with stateless parsers such as `parse_payment_request_event_message`, `parse_receipt_access_event_message`, and `parse_private_payment_list_json`.

Paykit Library is stateless and does not persist Event Message history. Callers that derive state or trigger side effects must persist the raw JSON payload, parsed `kind` when available, parse result, and parsed IDs when available from event wrappers such as `PaymentRequestEventMessage` or `ReceiptAccessEventMessage`, detect conflicting reused Event IDs or conflicting repeated `payment_request_id` terms, and apply lifecycle rules such as proof-after-rejection/cancellation before acting.

Callers that persist Encrypted Link snapshots should treat the snapshot as the local read checkpoint. Persist received Event Messages and dedupe state before replacing the stored snapshot with a snapshot whose read counter has advanced past those messages. If events are persisted but the snapshot is not, replay is expected; Event Messages should be deduped by `event_id`, while Receipt Access can also be reconciled by Receipt ID and caller receipt state.

#### Payment receipts

Receipts involve three related objects:

1. The plaintext `Receipt` is created and decrypted locally. It is not stored directly on the homeserver.
2. The Encrypted Receipt is stored on the issuer's homeserver under `/pub/paykit/v0/private/receipts/{ReceiptId}`.
3. A `ReceiptAccess` descriptor is sent to the counterparty over the existing Encrypted Link. It contains the Event ID, `ReceiptId`, `payment_reference`, optional `payment_request_id` and `billing_period`, Receipt Location, and symmetric Receipt Decryption Key.

Receipt Location is a path on the issuer's homeserver, not a complete Pubky resource by itself. SDK/runtime code pairs it with the Receipt Access sender/issuer context when retrieving the Encrypted Receipt. The Receipt is encrypted with `XChaCha20Poly1305`; the storage location path is used as authenticated associated data, so fetching the right ciphertext but decrypting it against a different location fails.

- `prepare_receipt(link, draft: ReceiptDraft) -> Result<PreparedReceipt>`
  Builds a canonical local `Receipt` from the caller's `ReceiptDraft`, fills in the recipient public key from `link`, generates a fresh `ReceiptId` when the draft does not provide one, copies optional Payment Request ID and Billing Period correlation into the Receipt and Receipt Access descriptor, generates a fresh `ReceiptDecryptionKey`, encrypts the Receipt into an Encrypted Receipt, and returns the matching `ReceiptAccess` descriptor without storing or sending anything. Receipt Metadata is a caller-defined JSON object.
- `prepare_receipt_for_recipient(recipient_public_key, draft: ReceiptDraft) -> Result<PreparedReceipt>`
  Same as `prepare_receipt`, but takes the recipient public key directly so stateful runtimes can prepare receipt issuance before restoring or sending over an Encrypted Link.
- `store_prepared_receipt(session, prepared: &PreparedReceipt) -> Result<()>`
  Stores the Encrypted Receipt from a prepared issuance at its Receipt Location.
- `send_receipt_access(link, app_id, access: &ReceiptAccess) -> Result<()>`
  Sends a prepared Receipt Access descriptor over Noise. This can be retried with the same descriptor if storage already succeeded.
- `serialize_receipt_access_json(app_id, access: &ReceiptAccess) -> Result<String>`
  Returns the canonical Receipt Access Event Message JSON for durable outbound queues.
- `parse_receipt_access_event_message(message: &PrivateApplicationMessage) -> Option<ReceiptAccessEventMessage>`
  Stateless parser for Receipt Access events from the raw Private Application Message stream. Recognized but malformed Receipt Access events are returned with a validation error so callers can persist the raw payload before advancing their durable checkpoint.
- `parse_receipt_access_json(json: &str) -> Result<ReceiptAccess>`
  Stateless parser for a known Receipt Access JSON payload.
- `ReceiptAccess::location_for(receipt_id) -> String`
  Returns Paykit's canonical Receipt Location for an Encrypted Receipt.
- `Receipt::encrypt(&self, key) -> Result<String>` / `Receipt::decrypt(encrypted_json, key, location) -> Result<Receipt>`
  Encrypts local plaintext Receipts into Encrypted Receipts, or decrypts Encrypted Receipts back into local plaintext Receipts, using `XChaCha20Poly1305`. Encryption derives the canonical Receipt Location path from the Receipt's `ReceiptId` and authenticates that path as AAD. Pass the exact location from the access descriptor when decrypting; it is authenticated as AAD. Decryption also rejects plaintext whose internal Receipt ID does not match the authenticated location.
- `decrypt_receipt(encrypted_json, key, location) -> Result<Receipt>`
  Convenience wrapper around `Receipt::decrypt`. This decrypts with the supplied location as authenticated data and rejects plaintext whose `ReceiptId` does not map to that location. Receipt Access descriptor validation happens when parsing or sending Receipt Access.

Receipt Decryption Keys are sensitive. `ReceiptDecryptionKey` and `ReceiptAccess` redact key material from formatted output through the key's custom `Debug`/`Display`, but callers must still avoid logging or persisting the raw `ReceiptDecryptionKey::as_str()` value outside secure storage.

Apps should call `prepare_receipt`, persist the returned `PreparedReceipt` or equivalent issuance state, then call `store_prepared_receipt` and `send_receipt_access` in retryable steps. If storage succeeds but sending Receipt Access fails, callers can retry sending the same `PreparedReceipt::access`.

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
  Captures the current Encrypted Link state (transport keys, nonce counters, and counterparty identity) as a serializable snapshot.
- `EncryptedLink::serialize() -> Vec<u8>`
  Convenience method equivalent to `self.snapshot().serialize()`.
- `EncryptedLink::config() -> &Arc<PubkyNoiseConfig>`
  Access the shared Noise configuration for in-process restore via `restore_encrypted_link_from_config`.

**Snapshot type:**

- `EncryptedLinkSnapshot::serialize() -> Vec<u8>`
  Serializes the `pubky-noise` session state followed by the counterparty's 32-byte identity-wide Noise public key. The counterparty Pubky identity key remains embedded in the Noise state.
- `EncryptedLinkSnapshot::deserialize(bytes: &[u8]) -> Result<EncryptedLinkSnapshot>`
  Reconstructs a snapshot from bytes, including the embedded recipient public key.
- `EncryptedLinkSnapshot::recipient() -> &PublicKey`
  Access the counterparty's public key embedded in the snapshot.
- `EncryptedLinkSnapshot::remote_noise_public_key() -> &PublicKey`
  Access the counterparty's identity-wide Noise public key embedded in the snapshot.

**Restore:**

- `restore_encrypted_link(session, secret_key, remote_identity_public_key, outbox_client, snapshot) -> Result<EncryptedLink>`
  Cross-restart restore. Accepts a fresh `PubkySession` and the same secret key used in the original `initiate_encrypted_link` or `accept_encrypted_link` call. Internally builds a new `PubkyNoiseConfig` and restores the transport-phase Noise state and counters directly from the serialized snapshot.
- `restore_encrypted_link_from_config(config, remote_identity_public_key, snapshot) -> Result<EncryptedLink>`
  In-process restore. Reuses an existing `Arc<PubkyNoiseConfig>` (obtainable via `EncryptedLink::config()`) when the link needs rebuilding without an app restart.

After link restore, `max_send_retries` resets to `DEFAULT_MAX_SEND_RETRIES`. Call `EncryptedLink::set_max_send_retries` after restore if you need a non-default value.

**Recovery markers:**

`EncryptedLinkRecoveryMarker` is a minimal public Pubky marker for relink
coordination when an Encrypted Link can no longer be trusted. `paykit-lib`
provides stateless marker parsing, serialization, path derivation, and
publish/fetch/remove helpers. SDKs decide when to publish or act on markers.

**When to snapshot:**

Take a snapshot after the Encrypted Link is established and periodically after exchanging messages. The snapshot includes nonce counters that must stay in sync with the counterparty. Restoring from a stale snapshot may cause nonce desynchronization or replay newer messages. Persist any returned Event Messages and caller dedupe state before replacing the stored snapshot with one whose read counter has advanced past those messages.

Snapshot bytes include sensitive key material, so they must be treated as secrets (store encrypted at rest; never log or expose them).

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
// Continue using set_private_payment_list and raw stream receive/parsing
```

## Exports

The crate exports:

- `PAYKIT_PATH_PREFIX` (`/pub/paykit/v0/`) and `PAYKIT_PRIVATE_PATH_PREFIX` (`/pub/paykit/v0/private`) to standardize Pubky path construction.
- `set_payment_endpoint`, `remove_payment_endpoint`, `get_payment_list`, and `get_payment_endpoint` for Public Payment Endpoint operations over `pubky::PubkySession` and `pubky::PublicStorage`.
- `EncryptedLink`, `EncryptedLinkHandshake`, `HandshakeProgress`, `EncryptedLinkSnapshot`, `EncryptedLinkHandshakeSnapshot`, `PrivateApplicationMessage`, and `PrivateMessageKind` for Encrypted Link types.
- `initiate_encrypted_link`, `accept_encrypted_link`, `advance_handshake`, `close_encrypted_link`, `EncryptedLink::receive_private_application_messages`, `set_private_payment_list`, and `parse_private_payment_list_json` for Encrypted Link and Private Payment List operations.
- `PaymentAmount`, `PaymentRequest`, `PaymentRequestEvent`, `PaymentRequestEventMessage`, `PaymentRequestAcceptance`, `PaymentRequestRejection`, `PaymentRequestCancellation`, `PaymentProof`, `send_payment_request`, `serialize_payment_request_event`, `parse_payment_request_event_message`, and proof validation helpers for Payment Request exchange.
- `ReceiptId`, `ReceiptDraft`, `Receipt`, `PreparedReceipt`, `ReceiptAccess`, `ReceiptAccessEventMessage`, `ReceiptDecryptionKey`, `prepare_receipt`, `store_prepared_receipt`, `send_receipt_access`, `parse_receipt_access_event_message`, `parse_receipt_access_json`, and `decrypt_receipt` for Encrypted Receipt issuance, access delivery, and decryption.
- `restore_encrypted_link`, `restore_encrypted_link_from_config`, `restore_encrypted_link_handshake`, `restore_encrypted_link_handshake_from_config` for session resumption after app restart or in-process recovery.
- `DEFAULT_MAX_RECOVERY_ATTEMPTS`, `DEFAULT_MAX_SEND_RETRIES` for configurable retry/recovery limits.
- `pubky_noise` re-export for advanced callers that need direct access to the encryption layer.
