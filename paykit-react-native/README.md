# @synonymdev/react-native-paykit

React Native Language Bindings for the Paykit SDK.

The package is generated from the same UniFFI metadata as the Swift and Kotlin
bindings in [`paykit-ffi`](../paykit-ffi/), using
[uniffi-bindgen-react-native](https://github.com/jhugman/uniffi-bindgen-react-native)
(ubrn). There is no hand-written bridge code: every exported object, record,
enum, error, async method and callback interface comes straight from Rust as
typed TypeScript and JSI C++.

## Requirements

- React Native's New Architecture (TurboModules and JSI). The legacy bridge is
  not supported.
- Hermes.
- iOS 15.1 or later; Android API 24 or later.

### Android TLS initialization

On Android, Paykit checks server certificates with the platform trust store
through `rustls-platform-verifier`, which needs an Android `Context`. The
package does this for you: when React Native creates the TurboModule, it calls
`com.synonym.paykit.PaykitAndroid.initializeOrThrow` with the application
context. If that fails, module creation throws `IllegalStateException`
instead of letting every HTTPS request fail later.

Do not add the Kotlin package `com.synonym:paykit-android` to the same app.
Both ship `com.synonym.paykit.PaykitAndroid` and `libpaykit.so`, so the build
fails with duplicate classes and native libraries.

The package's consumer R8 rules keep the verifier classes that Rust calls
through JNI.

### Expo

Expo Go cannot load this package because it contains native code. Use a
[development build](https://docs.expo.dev/develop/development-builds/introduction/)
(`npx expo prebuild`, or EAS Build) with the New Architecture enabled.

## Installation

```sh
npm install @synonymdev/react-native-paykit
cd ios && pod install
```

### React Native 0.78 with Xcode 26

React Native 0.78 pins the `fmt` pod at 11.0.2, which does not compile as
C++20 with the Xcode 26 clang. Build it as C++17 in your `Podfile`, after
`react_native_post_install`:

```ruby
post_install do |installer|
  react_native_post_install(installer, config[:reactNativePath])
  installer.pods_project.targets.each do |target|
    next unless target.name == 'fmt'
    target.build_configurations.each do |build_config|
      build_config.build_settings['CLANG_CXX_LANGUAGE_STANDARD'] = 'c++17'
    end
  end
end
```

The example app's [`Podfile`](example/ios/Podfile) does this.

### iOS simulator: arm64 only

The XCFramework has an `ios-arm64` device slice and an
`ios-arm64-simulator` slice, with no x86_64 simulator slice. Apps run on
Apple silicon simulators only. A simulator build that also builds x86_64
fails with `ld: library 'paykit' not found`. Release builds do, because
`ONLY_ACTIVE_ARCH` is `NO`.

Exclude x86_64 from simulator builds in your `Podfile`, after
`react_native_post_install`:

```ruby
post_install do |installer|
  react_native_post_install(installer, config[:reactNativePath])
  excluded_archs = 'EXCLUDED_ARCHS[sdk=iphonesimulator*]'
  installer.pods_project.build_configurations.each do |build_config|
    build_config.build_settings[excluded_archs] = 'x86_64'
  end
  installer.aggregate_targets.map(&:user_project).uniq.each do |project|
    project.native_targets.each do |target|
      target.build_configurations.each do |build_config|
        build_config.build_settings[excluded_archs] = 'x86_64'
      end
    end
    project.save
  end
end
```

Or pass `ARCHS=arm64` to `xcodebuild`, as the CI `smoke-ios` job does.

## Usage

The TypeScript API mirrors `paykit-ffi`. Async Rust methods return Promises,
and `PaykitFfiError` is thrown as typed error classes.

```ts
import { defaultConfig } from '@synonymdev/react-native-paykit';

const config = defaultConfig('mywallet'); // your Paykit App ID
```

### Type names

Swift and Kotlin strip the `Ffi` prefix from exported names (for example
`FfiPaykitSdk` becomes `PaykitSdk`). The TypeScript bindings keep the Rust
names, so use `FfiPaykitSdk`, `FfiSdkStateBlobStore`, and so on. ubrn for
UniFFI 0.29 has no rename option.

### Callbacks and threads

`FfiSdkStateBlobStore`, `FfiSdkPubkySessionProvider` and `FfiSdkPaymentAdapter`
are synchronous interfaces. When the SDK calls them from a Rust worker thread,
the call runs on the JS thread and the worker waits for the return value. So:

- Implement every callback synchronously. Do not return Promises.
- Do not call back into the same `FfiPaykitSdk` handle from a callback.
- `FfiPaykitSdk.stateRevision()` does not wait for other calls on the same
  handle. While one of them is in a state transaction, it throws
  `PaykitFfiError.SharedStateBusy` with code `state_transaction_in_progress`.
  Retry after that call finishes.

## Security

### SDK state blob bytes enter the JS heap

`FfiSdkStateBlob` is zeroized when Rust drops it. A JavaScript
`FfiSdkStateBlobStore` must call `exportBytes()` to persist it, and the
resulting `ArrayBuffer` cannot be zeroized. It stays in the JS heap until
garbage collection and is visible to JS debuggers.

`FfiSdkStateBlobStore` is synchronous, so the store must be **synchronous and
encrypted**:

- Use synchronous encrypted storage, such as `expo-secure-store`'s sync API or
  MMKV with an encryption key held in the Keychain or Android Keystore.
- Asynchronous storage, such as AsyncStorage, cannot implement this interface.
- Never log, inspect or partially edit the blob.
- Return a new, never-reused revision on every successful changed write, as
  described in [the bindings spec](../specs/paykit-sdk-bindings.md).

Apps that do not want SDK state in JavaScript can use the
`FfiPaykitSdk.withPubkySharedState*` constructors, which keep the state in
encrypted Pubky storage instead of platform callbacks.

### JS records are not redacted

Swift and Kotlin override `description` and `toString` for sensitive records
so they print redacted. TypeScript records are plain objects, so
`console.log`, error reporters and JS debuggers show every field. Do not log
or report these types, or any object that contains them:

`FfiContactRecord`, `FfiContactUpdate`, `FfiConversionRate`,
`FfiPaymentAmountContext`, `FfiPaymentRequestTerms`,
`FfiPaymentProofSubmission`, `FfiPaymentTarget`,
`FfiPreparedPrivateContactPayment`, `FfiPrivateContactPaymentResolution`,
`FfiPrivatePaymentEndpointCandidate`, `FfiPrivatePaymentEndpointReservation`,
`FfiPrivatePaymentEndpointReservationCancellation`,
`FfiPrivatePaymentEndpointReservationInput`,
`FfiPrivatePaymentEndpointSelectionRequest`,
`FfiPrivatePaymentListDeliveryFailure`, `FfiPrivatePaymentListSyncChange`,
`FfiPrivateReceivingDetail`, `FfiPrivateReceivingDetailReservationResponse`,
`FfiProfileResolution`, `FfiPubkyAuthCompanionClaim`,
`FfiPublicContactPaymentResolution`, `FfiPublicPaymentEndpointCandidate`,
`FfiPublicPaymentEndpointLoadFailure`,
`FfiPublicPaymentEndpointSelectionRequest`, `FfiPublicReceivingDetail`,
`FfiReceiptDraft`, `FfiReceiptIssuanceView`, `FfiReceiptRecord`,
`FfiReservationCleanupFailure`, `FfiResolvedPrivatePaymentEndpoint`,
`FfiResolvedPublicPaymentEndpoint`, `FfiPaymentRequestScope`,
`FfiPaymentOccurrence`, `FfiPaymentAccountingScope`,
`FfiAccountingBillingPeriod`, `FfiPaymentOccurrenceKey`,
`FfiPaymentAttemptRecord`, `FfiPaymentOccurrenceRecord`,
`FfiAllowanceAssociationRevision`, `FfiAllowanceAssociationRecord`,
`FfiAllowanceWatermarkRecord`, `FfiAllowanceAccountingHistory`,
`FfiAllowanceAccountingState`, `FfiAllowanceSelectionInput`,
`FfiAllowanceReassociationInput`, `FfiPaymentExecutionChecks`,
`FfiAllowanceCandidate`, `FfiPaymentOutcomeReport`,
`FfiAllowanceAccountingReconciliation`, `FfiPaymentDisposition`,
`FfiPaymentRequestRecord`, `FfiPaymentProofRecord`,
`FfiPaymentConversionQuoteRecord`, `FfiPaymentRequestAmount`,
`FfiPaymentRequestRecurrence`, `FfiAllowanceRecord`, `FfiLinkedPeerRecord`,
`FfiPrivatePaymentListEndpoint`, `FfiPrivatePaymentListView`,
`FfiQueuedPrivateMessage`, `FfiReceiptAccessView`, `FfiPaykitProfileRecord`,
`FfiPubkyProfileRecord`, `FfiPaykitBlobRecord`, `FfiSdkStateBlobSnapshot`,
`FfiPaykitStateObservation`

This list comes from
[`paykit-ffi/redacted_binding_types.txt`](../paykit-ffi/redacted_binding_types.txt).
A test fails when the two lists differ.

Secret keys never come back as raw values. `FfiPaykitIdentitySecretKey` and
the other key types stay opaque object handles.

## Development

ubrn writes two kinds of files:

- The TypeScript and C++ bindings in `src/generated/` and `cpp/generated/`.
  They are not committed. API changes are reviewed through the committed Swift
  bindings in `paykit-ffi/bindings/ios/`.
- The TurboModule glue (`src/index.tsx`, `src/NativeReactNativePaykit.ts`,
  `cpp/`, `ios/`, `android/` and the podspec), rendered from
  `ubrn.config.yaml` and `package.json`. It is committed, and CI fails when it
  is out of date. Do not edit it by hand, except for the files listed
  below.

Regenerate both after any change to `paykit-ffi`:

```sh
cargo build --release -p paykit-ffi   # from the repository root
cd paykit-react-native
npm ci
npm run ubrn:generate                 # TypeScript and C++ from the host library
npm run typecheck && npm run lint && npm test
```

`npm run build:native` regenerates the same files from the native libraries.

Two files are hand-edited after ubrn rendered them and are listed under
`noOverwrite` in `ubrn.config.yaml`: `android/build.gradle` and the Kotlin
TurboModule. ubrn leaves them alone, so review them by hand when you upgrade
ubrn. `android/src/main/java/com/synonym/paykit/PaykitAndroid.kt` is not
rendered by ubrn at all.

`./build.sh all` in `paykit-ffi` builds Swift and Kotlin only. The
`React Native` CI workflow regenerates and type-checks this package on every
FFI change, and builds the native libraries for both platforms.

## Build from source

The npm package ships prebuilt native libraries. To build them from a
checkout of this repository:

### Prerequisites

- Node.js from `.nvmrc` and npm.
- Rust stable from [rustup](https://rustup.rs).
- iOS (macOS only): Xcode with its command line tools, and CocoaPods for
  `npm run ios:check`. Add the Rust targets:

  ```sh
  rustup target add aarch64-apple-ios aarch64-apple-ios-sim
  ```

- Android: JDK 17, the Android SDK with platform 35, NDK r28c
  (`28.2.13676358`) and cargo-ndk 3.5.4. Add the Rust targets:

  ```sh
  rustup target add aarch64-linux-android armv7-linux-androideabi \
    i686-linux-android x86_64-linux-android
  cargo install cargo-ndk --version 3.5.4 --locked
  export ANDROID_HOME=<Android SDK>
  export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/28.2.13676358
  ```

  To build with another installed NDK, point `ANDROID_NDK_HOME` at it and
  pass `-PReactNativePaykit_ndkVersion=<version>` to Gradle. An app's own
  `ndkVersion` always takes precedence.

### Build

```sh
cd paykit-react-native
npm ci
npm run build:native   # or build:ios / build:android
```

`npm run build:native` runs `ubrn build ios` and `ubrn build android` with
`--and-generate --release`, so it also generates the bindings and glue. The
targets come from `ubrn.config.yaml`. It builds:

- `SynonymdevReactNativePaykitFramework.xcframework` from `aarch64-apple-ios`
  and `aarch64-apple-ios-sim`, with `IPHONEOS_DEPLOYMENT_TARGET` 13.4 as in
  `paykit-ffi/build_ios.sh`;
- `android/src/main/jniLibs/<abi>/libpaykit.so` for `arm64-v8a`,
  `armeabi-v7a`, `x86_64` and `x86`, with the 16 KB page-size and linker
  flags from the workspace `.cargo/config.toml`;
- `android/libs/rustls-platform-verifier.jar`, the verifier's Kotlin classes,
  taken from `vendor/rustls-platform-verifier-android`.

After `ubrn build android`, `scripts/android-elf.mjs` writes the jar, strips
the copied libraries and fails if one is not 16 KB aligned or does not
export the TLS init symbol. It needs `ANDROID_NDK_HOME` for `llvm-strip` and
`llvm-readelf`.

To build some Android ABIs only, for example for an emulator, pass ubrn's
`--targets`:

```sh
npm run build:android -- --targets x86_64
```

Builds share the `../target` cache with `paykit-ffi/build.sh`.

Check the native builds without an app:

```sh
npm run ios:check          # podspec, XCFramework slices, C++ glue compile
npm run android:assemble   # assembles the library AAR and checks its .so files
npm run pack:check         # lists what npm would publish, with sizes
```

`npm run android:assemble` uses the standalone Gradle build in
`android-check/` and the Gradle wrapper from React Native's Gradle plugin.

## Example app and smoke test

[`example/`](example/) is a React Native app that installs this package from
an `npm pack` tarball and runs a smoke test against a real Pubky homeserver,
on staging or on a local Pubky stack:

```sh
npm run example:refresh   # pack this package and install it into example/
npm run example:ios       # or example:android
```

See [`example/README.md`](example/README.md) for the network modes, launch
options and what each smoke step proves.

## License

MIT
