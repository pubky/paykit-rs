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

### Expo

Expo Go cannot load this package because it contains native code. Use a
[development build](https://docs.expo.dev/develop/development-builds/introduction/)
(`npx expo prebuild`, or EAS Build) with the New Architecture enabled.

## Installation

```sh
npm install @synonymdev/react-native-paykit
cd ios && pod install
```

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
- Do not call `FfiPaykitSdk.stateRevision()` while another call on the same
  handle is still pending. Both wait on the same storage lock, so the JS thread
  and the Rust worker can block each other forever.

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
  is out of date. Do not edit it by hand.

Regenerate both after any change to `paykit-ffi`:

```sh
cargo build --release -p paykit-ffi   # from the repository root
cd paykit-react-native
npm ci
npm run ubrn:generate                 # TypeScript and C++ from the host library
npm run typecheck && npm run lint && npm test
```

`npm run build:native` builds the iOS XCFramework and the Android libraries
with ubrn. It reuses the Rust build cache in `../target`.

`./build.sh all` in `paykit-ffi` builds Swift and Kotlin only. The
`React Native` CI workflow regenerates and type-checks this package on every
FFI change.

## License

MIT
