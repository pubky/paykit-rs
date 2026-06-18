# Paykit FFI

UniFFI bindings for [paykit-sdk](../paykit-sdk/), targeting iOS (Swift) and
Android (Kotlin).

The generated bindings expose the SDK runtime foundation: configuration, Pubky
session bootstrap helpers, opaque SDK state storage callbacks, identity
initialization/sign-out, SDK backup/restore, Paykit Profile and Contact Record
workflows, and public Pubky read helpers. Product workflows such as endpoint
sync, private links, payment requests, receipts, and contact payment resolution
belong on the SDK surface rather than on low-level `paykit-lib` protocol
bindings.

## Exported Surface

### Runtime

- `FfiPaykitSdk` — stateful SDK runtime handle.
- `FfiSdkStateBlobStore` — platform callback interface for opaque SDK state
  blob load/save/clear.
- `FfiSdkPubkySessionProvider` — platform callback interface for live Pubky
  session access and public storage availability.
- `FfiPubkySessionAccess` — opaque Pubky session access material. Use its
  explicit export methods only when persisting or loading platform-protected
  session state.
- `defaultConfig()` — return default `FfiPaykitSdkConfig`.
- `defaultPubkyClientConfig()` — return default `FfiPubkyClientConfig`.
- `requiredSessionCapabilities(config)` — return Pubky capabilities required by
  a config.
- `coreSessionCapabilities()` — return the core Paykit Pubky capability scope.

### Pubky Session Bootstrap

- `FfiPubkySessionBootstrap` — create/import Pubky sessions and auth flows.
- `FfiPubkyAuthRequest` — pending external auth-flow handle.
- `derivePubkySecretKey(seed, runtimeLabel)` — derive an app/runtime-specific
  Pubky secret key from a 64-byte wallet seed.
- `pubkyPublicKeyFromSecret(localSecretKey)` — derive a Pubky public key.
- `parsePubkyAuthUrl(authUrl)` — inspect a Pubky auth URL.
- `resolvePubkyUrl(uri)` and `parsePubkyResource(uri)` — Pubky URI helpers.

### Profiles and Contacts

- `FfiPaykitSdk.publishPaykitProfile` / `fetchPaykitProfile` — write and read
  public Paykit Profiles.
- `FfiPaykitSdk.publishPaykitBlob`, `deletePaykitBlob`, `fetchPubkyFile`, and
  `fetchPubkyText` — publish profile blobs and read public Pubky resources.
- `FfiPaykitSdk.saveContact`, `contactRecord`, `contactRecords`, and
  `removeContact` — manage local Contact Records.
- `FfiPaykitSdk.fetchPubkyProfile`, `fetchPubkyFollows`, and
  `resolveContactProfile` — read Pubky app profile/follow data and resolve
  contact display metadata.
- `FfiPaykitSdk.publishPublicContact`, `removePublicContact`, and
  `syncPublicContactMarkers` — opt-in Public Contact Marker workflows.

`FfiPaykitProfile.extraJson` is a JSON object string so apps can carry
app-specific public profile fields without exposing an FFI JSON value model.

### State and Secret Blobs

- `FfiSdkStateBlob` — internal SDK runtime state for platform durable
  storage. Store it encrypted or inside platform-protected storage.
- `FfiSdkBackupBlob` — SDK backup/export payload for app-controlled
  backup flows.
- `FfiPubkyLocalSecretKey` — local Pubky secret key bytes.

These are opaque binding objects. Use their explicit export methods only at
platform secure-storage or backup boundaries.

## Building

Always build all platforms together:

```bash
cd paykit-ffi
./build.sh all
```

Release builds use the same script with `-r`:

```bash
./build.sh -r all
./build.sh -r --rc all
```

Run focused checks:

```bash
cargo test -p paykit-ffi --all-features
cargo doc -p paykit-ffi --no-deps
```

## Android Initialization

Android apps must initialize the platform certificate verifier before Pubky
networking:

```kotlin
import com.synonym.paykit.PaykitAndroid

check(PaykitAndroid.initialize(applicationContext))
```

React Native initializes this from the native module and returns a platform
error if initialization fails.

## Project Structure

```text
paykit-ffi/
├── src/lib.rs              # UniFFI SDK exports
├── build.sh                # Unified all-platform build script
├── build_ios.sh            # Internal iOS sub-build script
├── build_android.sh        # Internal Android sub-build script
├── bindings/ios/           # Generated Swift + XCFramework
└── bindings/android/       # Generated Kotlin + Android library
```
