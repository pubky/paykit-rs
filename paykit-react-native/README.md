# @synonymdev/react-native-paykit

React Native Native Module wrapping [paykit-ffi](../paykit-ffi/) UniFFI bindings. Provides a TypeScript API for PayKit payment routing on iOS and Android.

## Installation

```sh
npm install @synonymdev/react-native-paykit
# or
yarn add @synonymdev/react-native-paykit
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
  initiateEncryptedLink,
  advanceHandshake,
  generatePaymentReference,
  setPrivatePayments,
  getPrivatePayments,
  issueReceipt,
  getReceiptAccess,
  receiptLocation,
  decryptReceipt,
} from '@synonymdev/react-native-paykit';

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

// Private encrypted payments use encrypted-link handles
const handshake = await initiateEncryptedLink(secretKeyHex, receiverPublicKey);
if (handshake.isOk()) {
  const progress = await advanceHandshake(handshake.value);
  if (progress.isOk() && progress.value.status === 'complete') {
    const reference = await generatePaymentReference();
    if (reference.isErr()) {
      throw new Error(reference.error);
    }

    await setPrivatePayments(progress.value.linkHandle, {
      reference: reference.value,
      entries: [
        { method_id: 'btc-lightning-bolt11', endpoint_data: '{"value":"lnbc1..."}' },
      ],
    });
    const privatePayments = await getPrivatePayments(progress.value.linkHandle);

    await issueReceipt(progress.value.linkHandle, {
      reference: reference.value,
      payment_method: 'btc-lightning-bolt11',
      amount: '1000',
      currency: 'sats',
      metadata: [{ key: 'note', value: 'Paid' }],
    });
    const access = await getReceiptAccess(progress.value.linkHandle);
  }
}
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

- **`getPaymentList(publicKey)`** — Fetch all public payment methods for a user.
- **`getPaymentEndpoint(publicKey, methodId)`** — Fetch a specific public endpoint.
- **`setPaymentEndpoint(methodId, endpointData)`** — Publish/update a public endpoint.
- **`removePaymentEndpoint(methodId)`** — Remove a public endpoint.

### Private encrypted payments

- **`defaultMaxSendRetries()`** — Get the default private-message send retry count.
- **`defaultMaxRecoveryAttempts()`** — Get the default handshake recovery-attempt count.
- **`generatePaymentReference()`** — Generate a UUID-v4 payment reference for private payment and receipt correlation.
- **`initiateEncryptedLink(secretKeyHex, receiverPublicKey)`** — Start a private encrypted-link handshake as initiator.
- **`acceptEncryptedLink(secretKeyHex, senderPublicKey)`** — Start a private encrypted-link handshake as responder.
- **`advanceHandshake(handshakeId)`** — Advance a handshake; returns pending handshake handle or complete link handle.
- **`setEncryptedLinkHandshakeMaxRecoveryAttempts(handshakeId, max)`** — Override recovery attempts for a pending handshake.
- **`setEncryptedLinkMaxSendRetries(linkId, max)`** — Override send retries for an established encrypted link.
- **`setPrivatePayments(linkId, payload)`** — Send the complete latest-state private payment payload over the link. The payload contains `reference` and `entries`.
- **`getPrivatePayments(linkId)`** — Receive the newest private payment payload from the link, or `null` when none is available.
- **`issueReceipt(linkId, draft)`** — Store an encrypted receipt and send receipt access over the link.
- **`getReceiptAccess(linkId)`** — Receive all currently available receipt access descriptors in FIFO order.
- **`receiptLocation(reference)`** — Return the canonical homeserver receipt location for a payment reference.
- **`decryptReceipt(encryptedJson, key, location)`** — Decrypt an encrypted receipt fetched from the homeserver.
- **`serializeEncryptedLinkHandshake(handshakeId)`** / **`restoreEncryptedLinkHandshake(secretKeyHex, snapshotHex)`** — Persist and restore pending handshakes.
- **`serializeEncryptedLink(linkId)`** / **`restoreEncryptedLink(secretKeyHex, snapshotHex)`** — Persist and restore established encrypted links.
- **`encryptedLinkSnapshotRecipient(snapshotHex)`** / **`encryptedLinkHandshakeSnapshotRecipient(snapshotHex)`** — Inspect the counterparty embedded in a snapshot.
- **`closeEncryptedLink(linkId)`** — Close an established encrypted-link native handle.
- **`dropEncryptedLinkHandshake(handshakeId)`** — Drop a pending handshake native handle.

Private payments are latest-state data: older queued private payment updates are superseded by the latest/newest queued envelope update. Receipt access is event-like and should be processed in order. Serialized snapshots and receipt keys contain sensitive key material and should be stored encrypted at rest.

All functions return `Promise<Result<T>>` using [`@synonymdev/result`](https://www.npmjs.com/package/@synonymdev/result).

## Updating Bindings

After rebuilding `paykit-ffi` (`cd paykit-ffi && ./build.sh all`), copy the fresh bindings:

```sh
./scripts/update-bindings.sh
```

This copies:
- **iOS**: `PaykitUniFFI.swift`, `paykitFFI.h`, `Paykit.xcframework`
- **Android**: Generated `.kt` files, `jniLibs/*.so` for all architectures

## Publishing to npm

Requires npm publish access to the `@synonymdev` org.

```sh
# 1. Build FFI bindings for all platforms
cd paykit-ffi && ./build.sh all

# 2. Copy bindings into the RN package
cd ../paykit-react-native
./scripts/update-bindings.sh

# 3. Install dependencies and build TypeScript
npm install
npm run prepare

# 4. Bump version
npm version patch  # or minor/major

# 5. Publish
npm publish
```

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
