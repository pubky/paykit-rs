# Paykit FFI

UniFFI bindings for [paykit-lib](../paykit-lib/), exposing Paykit's payment routing functionality to iOS (Swift) and Android (Kotlin).

## Exported API

### Initialization

| Function | Description |
|---|---|
| `paykit_initialize()` | Create the Pubky SDK facade and logger. Call once at app startup. |

### Session queries

| Function | Description |
|---|---|
| `paykit_is_authenticated()` | Returns `true` if an authenticated session is active. |
| `paykit_get_current_public_key()` | Returns the public key of the current session, or `nil`/`null`. |
| `paykit_export_session()` | Exports the session secret for persistence across app restarts. |

### Read operations (unauthenticated)

| Function | Description |
|---|---|
| `paykit_get_payment_list(public_key)` | Fetch all published payment methods for a user. |
| `paykit_get_payment_endpoint(public_key, method_id)` | Fetch a specific payment endpoint for a user. |

### Authentication

| Function | Description |
|---|---|
| `paykit_import_session(session_secret)` | Import a session from Pubky Ring auth (`<pubkey_z32>:<cookie_secret>`). Returns the user's public key. |
| `paykit_sign_up(secret_key_hex, homeserver_public_key)` | Create a new account with a raw secret key. Dev-auth only. Returns the user's public key. |
| `paykit_sign_in(secret_key_hex)` | Sign in with a raw secret key. Dev-auth only. Homeserver resolved via PKDNS. Returns the user's public key. |

### Write operations (require active session)

| Function | Description |
|---|---|
| `paykit_set_payment_endpoint(method_id, endpoint_data)` | Publish or update a payment endpoint. |
| `paykit_remove_payment_endpoint(method_id)` | Remove a payment endpoint. |
| `paykit_sign_out()` | End the current session (server + local). Restores session on failure. |
| `paykit_force_sign_out()` | Discard local session without contacting the server. |

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
```
cargo test -p paykit-lib
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

Then import and use:

```kotlin
import com.synonym.paykit.*

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
