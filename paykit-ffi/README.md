# Paykit FFI

UniFFI bindings for [paykit-lib](../paykit-lib/), exposing Paykit's payment routing functionality to iOS (Swift) and Android (Kotlin).

These bindings expose the low-level, stateless Paykit Library API. The
stateful `paykit-sdk` runtime is a separate Rust crate and does not have
platform bindings yet.

## Exported API

### Initialization

| Function | Description |
|---|---|
| `paykit_initialize()` | Create the Pubky client facade and logger. Call once at app startup. |

### Session queries

| Function | Description |
|---|---|
| `paykit_is_authenticated()` | Returns `true` if an authenticated session is active. |
| `paykit_get_current_public_key()` | Returns the public key of the current session, or `nil`/`null`. |
| `paykit_export_session()` | Exports the session secret for persistence across app restarts. |

### Read operations (unauthenticated)

| Function | Description |
|---|---|
| `paykit_get_payment_list(public_key)` | Fetch all published Payment Endpoints for a user. |
| `paykit_get_payment_endpoint(public_key, payment_endpoint_identifier)` | Fetch a specific Payment Endpoint Payload for a user. |

### Authentication

| Function | Description |
|---|---|
| `paykit_import_session(session_secret)` | Import a session from Pubky Ring auth (`<pubkey_z32>:<cookie_secret>`). Returns the user's public key. |
| `paykit_sign_up(secret_key_hex, homeserver_public_key)` | Create a new account with a raw secret key. Dev-auth only. Returns the user's public key. |
| `paykit_sign_in(secret_key_hex)` | Sign in with a raw secret key. Dev-auth only. Homeserver resolved via PKDNS. Returns the user's public key. |

### Write operations (require active session)

| Function | Description |
|---|---|
| `paykit_set_payment_endpoint(payment_endpoint_identifier, payment_endpoint_payload)` | Publish or update a Payment Endpoint. |
| `paykit_remove_payment_endpoint(payment_endpoint_identifier)` | Remove a Payment Endpoint. |
| `paykit_sign_out()` | End the current session (server + local). Restores session on failure. |
| `paykit_force_sign_out()` | Discard local session without contacting the server. |

### Private Payment Lists and Receipts

Private Paykit APIs require an active session and an established Encrypted Link handle. Persist session secrets and serialized link/handshake snapshots in app-managed secure storage if the app needs restart recovery.

| Function | Description |
|---|---|
| `paykit_default_max_send_retries()` | Return the library default for Private Application Message send retries. |
| `paykit_default_max_recovery_attempts()` | Return the library default for Encrypted Link Handshake recovery attempts. |
| `paykit_initiate_encrypted_link(secret_key_hex, receiver_public_key)` | Start an Encrypted Link Handshake as the initiator. Returns a handshake handle. |
| `paykit_accept_encrypted_link(secret_key_hex, sender_public_key)` | Start an Encrypted Link Handshake as the responder. Returns a handshake handle. |
| `paykit_advance_handshake(handshake_id)` | Advance a handshake. Returns `pending` with the same handle or `complete` with a link handle. |
| `paykit_set_encrypted_link_handshake_max_recovery_attempts(handshake_id, max)` | Override handshake recovery attempts. |
| `paykit_set_encrypted_link_max_send_retries(link_id, max)` | Override Private Application Message send retries for an Encrypted Link. |
| `paykit_set_private_payment_list(link_id, list)` | Send the complete Private Payment List. |
| `paykit_parse_private_payment_list_json(json)` | Parse a Private Payment List JSON message. |
| `paykit_receive_private_application_messages(link_id)` | Receive the current raw Private Application Message batch in send order. |
| `paykit_parse_payment_request_event_message(message)` | Parse a raw Private Application Message as a Payment Request event. |
| `paykit_serialize_payment_request_event(event)` | Serialize a Payment Request event to canonical JSON. |
| `paykit_validate_payment_proof_for_request(proof, request)` | Validate stateless proof/request correlation fields. |
| `paykit_send_payment_request(link_id, event)` | Send a Payment Request proposal event. |
| `paykit_send_payment_request_acceptance(link_id, event)` | Send a Payment Request acceptance event. |
| `paykit_send_payment_request_rejection(link_id, event)` | Send a Payment Request rejection event. |
| `paykit_send_payment_request_cancellation(link_id, event)` | Send a Payment Request cancellation event. |
| `paykit_send_payment_proof(link_id, event)` | Send a Payment Proof event. |
| `paykit_prepare_receipt(link_id, draft)` | Prepare a plaintext Receipt, Encrypted Receipt, and matching Receipt Access descriptor. |
| `paykit_store_prepared_receipt(prepared)` | Store a prepared Encrypted Receipt at its Receipt Location. |
| `paykit_send_receipt_access(link_id, access)` | Send a prepared Receipt Access descriptor over the Encrypted Link. |
| `paykit_parse_receipt_access_event_message(message)` | Parse a raw Private Application Message as a Receipt Access event. |
| `paykit_parse_receipt_access_json(json)` | Parse a Receipt Access JSON message. |
| `paykit_receipt_location(receipt_id)` | Return the canonical homeserver Receipt Location path for a Receipt ID. |
| `paykit_decrypt_receipt(encrypted_json, key, location)` | Decrypt an Encrypted Receipt fetched by the app from `location`. |
| `paykit_serialize_encrypted_link_handshake(handshake_id)` | Serialize a pending handshake snapshot as hex for durable storage. |
| `paykit_restore_encrypted_link_handshake(secret_key_hex, snapshot_hex)` | Restore a pending handshake from a snapshot. |
| `paykit_encrypted_link_handshake_snapshot_recipient(snapshot_hex)` | Inspect the counterparty in an Encrypted Link Handshake snapshot. |
| `paykit_serialize_encrypted_link(link_id)` | Serialize an established Encrypted Link snapshot as hex for durable storage. |
| `paykit_restore_encrypted_link(secret_key_hex, snapshot_hex)` | Restore an established Encrypted Link from a snapshot. |
| `paykit_encrypted_link_snapshot_recipient(snapshot_hex)` | Inspect the counterparty in an Encrypted Link snapshot. |
| `paykit_close_encrypted_link(link_id)` | Close an established Encrypted Link and remove the FFI handle. |
| `paykit_drop_encrypted_link_handshake(handshake_id)` | Drop a pending handshake handle. |

#### Private Application Message and receipt handling

- `FfiPrivateApplicationMessage` is one received Private Application Message with optional parsed `version`, optional parsed `kind`, and raw payload text. Platform SDK/runtime code should persist the raw payload and route messages with recognized `kind` values.
- Payment Request bindings expose the same stateless protocol tools as `paykit-lib`: typed records, send helpers, raw event parsing, canonical event serialization, and proof/request correlation validation. They do not derive lifecycle state, enforce roles, schedule recurring payments, or validate method-specific proofs.
- `FfiPaymentRequestEventMessage` preserves `kind`, optional parsed IDs, `raw_json`, parsed event data when valid, and `validation_error` when a recognized event is malformed.
- `FfiReceiptDraft` is caller input for `paykit_prepare_receipt`: optional `receipt_id`, `payment_reference`, optional `payment_request_id` and `billing_period` for Payment Request correlation, optional `payment_endpoint_identifier`, optional `amount` (`value` plus `asset`), and `metadata_json`, which must serialize to a JSON object.
- `FfiReceipt` is decrypted receipt plaintext: `receipt_id`, `payment_reference`, optional `payment_request_id` and `billing_period`, `recipient_public_key`, optional `payment_endpoint_identifier`, optional `amount` (`value` plus `asset`), and `metadata_json`, which serializes the Receipt Metadata JSON object.
- `FfiReceiptAccess` is the private descriptor sent to the counterparty: `event_id`, `receipt_id`, `payment_reference`, optional `payment_request_id` and `billing_period`, `location`, and secret `key`.
- `FfiReceiptAccessEventMessage` preserves `kind`, optional parsed IDs, `raw_json`, parsed Receipt Access data when valid, and `validation_error` when a recognized event is malformed.
- `FfiPreparedReceipt` contains `receipt`, `encrypted_receipt`, and `access`; persist it, or equivalent issuance state, before storing/sending if crash-safe receipt issuance matters.
- Receipt Access messages contain raw Receipt Decryption Key material in their JSON `key` field. Treat it as secret: do not log it, include it in telemetry, or store it outside platform secure storage.
- Receipt Access and Payment Request messages use Event Message semantics and must be preserved in send order by the SDK/runtime. Private Payment Lists use Latest-State Message semantics at the SDK/runtime layer.
- Receipt Location is a path on the issuer's homeserver, not a complete Pubky resource by itself. Pair it with the Receipt Access sender/issuer context when retrieving.
- `paykit_decrypt_receipt` authenticates the Receipt Location path as AEAD associated data and rejects plaintext whose Receipt ID does not match the canonical location.
- Receipt fetching is intentionally app-managed in the current FFI surface: use the Receipt Location path and key from a Receipt Access message to fetch the encrypted JSON, then pass it to `paykit_decrypt_receipt`.
- Encrypted Link snapshots preserve Noise counters and key material. Snapshot bytes must be treated as secret. Apps that treat Event Message data as irreversible should persist/reconcile their own app-level state before storing an advanced Encrypted Link snapshot.
- Generated Swift/Kotlin bindings use platform casing, such as `metadataJson`, `encryptedReceipt`, and `paymentReference`; `metadataJson` is still a JSON object string, not a native metadata map.

## Building the Bindings

### All Platforms
```
./build.sh all
```

### Release Builds (with version bump)
Always build all platform bindings together so Swift and Kotlin stay in sync.
The `-r/--release` flag bumps versions in the crate `Cargo.toml` files, the
SDK's `paykit-lib` dependency, the root `Package.swift`, and
`gradle.properties`, then builds.
When the current version is an RC, `-r` without `--rc` finalizes that RC
version. When the current version is not an RC, `-r` defaults to a patch bump.
Use `--rc` to create or increment an RC version.

```
./build.sh -r all           # Finalize current RC, or bump patch if not on an RC
./build.sh -r --rc all      # Create/increment an RC and build all platforms
./build.sh -r --minor all   # Bump minor and build all platforms
./build.sh -r -M all        # Bump major and build all platforms
```

### Run Tests
```bash
cargo test -p paykit-ffi --all-features
```

## Integration

### iOS (Swift Package Manager)

The built XCFramework is distributed as an SPM package. After uploading the zip to a GitHub release:

1. The root `Package.swift` is updated automatically by the build scripts with the release tag and checksum.
2. Add the package dependency pointing to this repo.
3. Import and use:

```swift
import Paykit

try await paykitInitialize()
let payments = try await paykitGetPaymentList(publicKey: pk)
```

### Android (Gradle / GitHub Packages)

Published to GitHub Packages as `com.synonym:paykit-android`.

In `settings.gradle.kts`, add the repository:

```kotlin
dependencyResolutionManagement {
    repositories {
        maven {
            url = uri("https://maven.pkg.github.com/pubky/paykit-rs")
            credentials {
                username = System.getenv("GITHUB_ACTOR") ?: extra["gpr.user"] as? String
                password = System.getenv("GITHUB_TOKEN") ?: extra["gpr.key"] as? String
            }
        }
    }
}
```

In `build.gradle.kts`, add the dependency:

```kotlin
dependencies {
    implementation("com.synonym:paykit-android:<version>")
}
```

Then initialize the Android platform verifier once before Paykit networking, then use Paykit:

```kotlin
import com.synonym.paykit.*

check(PaykitAndroid.initialize(applicationContext))
paykitInitialize()
val payments = paykitGetPaymentList(publicKey = pk)
```

## Typical app startup

### Production (Pubky Ring auth)

```
paykitInitialize()    // defaults to production network

// Cold start — try restoring a persisted session first
if let saved = loadFromKeychain("paykit_session") {
    paykitImportSession(saved)
}

// If no saved session, authenticate via Pubky Ring
paykitImportSession(sessionSecretFromRing)
    → Returns own public key

// Persist session for next cold start
let secret = paykitExportSession()
saveToKeychain("paykit_session", secret)

// Normal operations
paykitSetPaymentEndpoint("btc-lightning-bolt11", bolt11Payload)
paykitGetPaymentList(contactPk)
```

### Development (direct key)

```
paykitInitialize()
paykitSignIn(secretKeyHex)
    → Returns own public key
    → Same flow as above for reads/writes
```

## Project structure

```
paykit-ffi/
├── Cargo.toml              # Crate config (lib name = "paykit")
├── src/
│   ├── lib.rs              # UniFFI scaffolding, types, exported functions
│   └── bin/
│       └── uniffi-bindgen.rs
├── uniffi.toml             # Swift binding config
├── uniffi-android.toml     # Kotlin binding config
├── build.sh                # Unified all-platform build script + version bump
├── build_ios.sh            # Internal iOS sub-build script
├── build_android.sh        # Internal Android sub-build script
├── update_package.py       # Auto-update root Package.swift checksum/tag
├── bindings/
│   ├── ios/                # Generated: Swift bindings + XCFramework
│   └── android/            # Gradle project for Maven publishing
│       ├── build.gradle.kts
│       ├── settings.gradle.kts
│       ├── gradle.properties
│       ├── gradlew
│       └── lib/            # Android library module
│           ├── build.gradle.kts
│           └── src/main/
│               ├── AndroidManifest.xml
│               ├── jniLibs/    # Generated: .so files
│               └── kotlin/     # Generated: Kotlin bindings
└── README.md
```
