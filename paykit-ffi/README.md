# Paykit FFI

UniFFI bindings for [paykit-lib](../paykit-lib/), exposing Paykit's payment routing functionality to iOS (Swift) and Android (Kotlin).

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

### Private Payment Envelopes and Receipts

Private Paykit APIs require an active session and an established Encrypted Link handle. Persist session secrets and serialized link/handshake snapshots in app-managed secure storage if the app needs restart recovery.

| Function | Description |
|---|---|
| `paykit_default_max_send_retries()` | Return the library default for Private Application Message send retries. |
| `paykit_default_max_recovery_attempts()` | Return the library default for Encrypted Link Handshake recovery attempts. |
| `paykit_generate_payment_reference()` | Generate a UUID-v4 Payment Reference for Private Payment Envelope and Receipt correlation. |
| `paykit_initiate_encrypted_link(secret_key_hex, receiver_public_key)` | Start an Encrypted Link Handshake as the initiator. Returns a handshake handle. |
| `paykit_accept_encrypted_link(secret_key_hex, sender_public_key)` | Start an Encrypted Link Handshake as the responder. Returns a handshake handle. |
| `paykit_advance_handshake(handshake_id)` | Advance a handshake. Returns `pending` with the same handle or `complete` with a link handle. |
| `paykit_set_encrypted_link_handshake_max_recovery_attempts(handshake_id, max)` | Override handshake recovery attempts. |
| `paykit_set_encrypted_link_max_send_retries(link_id, max)` | Override Private Application Message send retries for an Encrypted Link. |
| `paykit_set_private_payment_envelope(link_id, envelope)` | Send the complete Private Payment Envelope. |
| `paykit_get_private_payment_envelope(link_id)` | Receive the newest queued Private Payment Envelope, or `nil`/`null`. |
| `paykit_issue_receipt(link_id, draft)` | Store an encrypted Receipt and send Receipt Access over the Encrypted Link. |
| `paykit_get_receipt_access(link_id)` | Receive all currently available Receipt Access descriptors in FIFO order. |
| `paykit_receipt_location(reference)` | Return the canonical homeserver Receipt Location for a Payment Reference. |
| `paykit_decrypt_receipt(encrypted_json, key, location)` | Decrypt an encrypted Receipt fetched by the app from `location`. |
| `paykit_serialize_encrypted_link_handshake(handshake_id)` | Serialize a pending handshake snapshot as hex for durable storage. |
| `paykit_restore_encrypted_link_handshake(secret_key_hex, snapshot_hex)` | Restore a pending handshake from a snapshot. |
| `paykit_encrypted_link_handshake_snapshot_recipient(snapshot_hex)` | Inspect the counterparty in an Encrypted Link Handshake snapshot. |
| `paykit_serialize_encrypted_link(link_id)` | Serialize an established Encrypted Link snapshot as hex for durable storage. |
| `paykit_restore_encrypted_link(secret_key_hex, snapshot_hex)` | Restore an established Encrypted Link from a snapshot. |
| `paykit_encrypted_link_snapshot_recipient(snapshot_hex)` | Inspect the counterparty in an Encrypted Link snapshot. |
| `paykit_close_encrypted_link(link_id)` | Close an established Encrypted Link and remove the FFI handle. |
| `paykit_drop_encrypted_link_handshake(handshake_id)` | Drop a pending handshake handle. |

#### Receipt records and key handling

- `FfiReceiptDraft` is caller-provided receipt data: `reference`, optional `payment_endpoint_identifier`, optional `amount`, optional `currency`, and Receipt Metadata fields.
- `FfiReceipt` is decrypted receipt plaintext: `reference`, `recipient_public_key`, optional `payment_endpoint_identifier`, and Receipt Metadata fields.
- `FfiIssuedReceipt` contains the issuer-side result after storing and sending Receipt Access: `reference`, Receipt Location, and raw Receipt Decryption Key material.
- `FfiReceiptAccess` contains the counterparty-side Receipt Access descriptor: `version`, `reference`, Receipt Location, raw Receipt Decryption Key material, and `algorithm`. Current valid Receipt Access messages use version `1` and algorithm `XChaCha20Poly1305`.
- `FfiIssuedReceipt` and `FfiReceiptAccess` contain raw Receipt Decryption Key material in their `key` field. Treat it as secret: do not log it, include it in telemetry, or store it outside platform secure storage.
- Receipt Access uses Event Message semantics: `paykit_get_receipt_access` returns every currently available Receipt Access message in send order. Private Payment Envelopes use Latest-State Message semantics: `paykit_get_private_payment_envelope` returns the newest queued envelope.
- `paykit_decrypt_receipt` authenticates the Receipt Location as AEAD associated data and rejects plaintext whose reference does not match the canonical location.
- Receipt fetching is intentionally app-managed in the current FFI surface: use the Receipt Location from `FfiReceiptAccess` to fetch the encrypted JSON, then pass it to `paykit_decrypt_receipt` with the matching `key` and `location`.
- Encrypted Link snapshots preserve Noise counters but do not make already-dispatched, not-yet-consumed Receipt Access records durable by themselves. Apps that treat Receipt Access as irreversible Event Message data should persist/reconcile their own app-level state alongside serialized Encrypted Link snapshots.

## Building the Bindings

### All Platforms
```
./build.sh all
```

### Platform-Specific Builds
```
./build.sh ios      # iOS only
./build.sh android  # Android only
```

### Release Builds (with version bump)
The `-r/--release` flag bumps versions in `Cargo.toml`, the root `Package.swift`, and `gradle.properties`, then builds.
Defaults to patch version bump; use `--major`/`-M` or `--minor`/`-m` for other increments.

```
./build.sh -r ios              # Bump patch (default) and build iOS
./build.sh -r --minor android  # Bump minor and build Android
./build.sh -r -M all           # Bump major and build all platforms
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
paykitSetPaymentEndpoint("lightning", lnurlJson)
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
├── build.sh                # Unified build script (ios|android|all + version bump)
├── build_ios.sh            # iOS build + XCFramework generation
├── build_android.sh        # Android build + Gradle publish
├── update_package.py       # Auto-update root Package.swift checksum/tag
├── bindings/
│   ├── ios/                # Generated: Swift bindings + XCFramework (after build_ios.sh)
│   └── android/            # Gradle project for Maven publishing
│       ├── build.gradle.kts
│       ├── settings.gradle.kts
│       ├── gradle.properties
│       ├── gradlew
│       └── lib/            # Android library module
│           ├── build.gradle.kts
│           └── src/main/
│               ├── AndroidManifest.xml
│               ├── jniLibs/    # Generated: .so files (after build_android.sh)
│               └── kotlin/     # Generated: Kotlin bindings (after build_android.sh)
└── README.md
```
