# @pubky/react-native-paykit

React Native Native Module wrapping [paykit-ffi](../paykit-ffi/) UniFFI bindings. Provides a TypeScript API for PayKit payment routing on iOS and Android.

## Installation

```sh
npm install @pubky/react-native-paykit
# or
yarn add @pubky/react-native-paykit
```

### iOS

```sh
cd ios && pod install
```

### Android

No additional steps — Gradle picks up the module automatically.

## Usage

```typescript
import {
  initialize,
  importSession,
  getPaymentList,
  getPaymentEndpoint,
  setPaymentEndpoint,
  removePaymentEndpoint,
} from '@pubky/react-native-paykit';

// Initialize the SDK (call once at app startup)
const initResult = await initialize();
if (initResult.isErr()) {
  console.error('Failed to init:', initResult.error);
}

// Restore a session from a stored secret
const sessionResult = await importSession('pubkey_z32:cookie_secret');
if (sessionResult.isOk()) {
  console.log('Authenticated as:', sessionResult.value);
}

// Fetch a user's payment methods
const listResult = await getPaymentList('user_public_key');
if (listResult.isOk()) {
  for (const entry of listResult.value) {
    console.log(`${entry.method_id}: ${entry.endpoint_data}`);
  }
}

// Get a specific payment endpoint
const endpoint = await getPaymentEndpoint('user_public_key', 'bitcoin');

// Publish your own payment endpoint
await setPaymentEndpoint('bitcoin', 'bc1q...');

// Remove a payment endpoint
await removePaymentEndpoint('bitcoin');
```

## API

### Initialization

- **`initialize()`** — Initialize the PayKit SDK. Call once at startup.

### Session

- **`isAuthenticated()`** — Check if a session is active.
- **`getCurrentPublicKey()`** — Get the authenticated user's public key.
- **`exportSession()`** — Export session secret for persistence.
- **`importSession(secret)`** — Restore session from a secret string.
- **`signUp(secretKeyHex, homeserverPublicKey)`** — Sign up (dev-auth only).
- **`signIn(secretKeyHex)`** — Sign in (dev-auth only).
- **`signOut()`** — End session on homeserver and clear local state.
- **`forceSignOut()`** — Discard local session without server contact.

### Payment List

- **`getPaymentList(publicKey)`** — Fetch all payment methods for a user.
- **`getPaymentEndpoint(publicKey, methodId)`** — Fetch a specific endpoint.
- **`setPaymentEndpoint(methodId, endpointData)`** — Publish/update an endpoint.
- **`removePaymentEndpoint(methodId)`** — Remove an endpoint.

All functions return `Promise<Result<T>>` using [`@synonymdev/result`](https://www.npmjs.com/package/@synonymdev/result).

## Updating Bindings

After rebuilding `paykit-ffi` (`cd paykit-ffi && ./build.sh all`), copy the fresh bindings:

```sh
./scripts/update-bindings.sh
```

This copies:
- **iOS**: `PaykitUniFFI.swift`, `paykitFFI.h`, `Paykit.xcframework`
- **Android**: Generated `.kt` files, `jniLibs/*.so` for all architectures

## Architecture

```
paykit-ffi/                       (Rust + UniFFI)
  ↓ uniffi-bindgen
  ├── bindings/ios/               (Swift + XCFramework)
  └── bindings/android/           (Kotlin + .so libs)
  ↓
paykit-react-native/              (this package)
  ├── ios/Paykit.swift            (hand-written RN bridge → UniFFI Swift)
  ├── ios/Paykit.mm               (ObjC++ registration)
  ├── android/.../PaykitModule.kt (hand-written RN bridge → UniFFI Kotlin)
  └── src/index.tsx               (TypeScript API)
```

## License

MIT
