# @synonymdev/react-native-paykit

React Native Native Module wrapping [paykit-ffi](../paykit-ffi/) UniFFI bindings. Provides a TypeScript API for Paykit Library functions on iOS and Android.

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
  setPrivatePaymentEnvelope,
  getPrivatePaymentEnvelope,
  issueReceipt,
  getReceiptAccess,
  receiptLocation,
  decryptReceipt,
} from '@synonymdev/react-native-paykit';

// Initialize the native binding (call once at app startup)
const initResult = await initialize();
if (initResult.isErr()) {
  console.error('Failed to init:', initResult.error);
}

// Restore a session from a stored secret
const sessionResult = await importSession('pubkey_z32:cookie_secret');
if (sessionResult.isOk()) {
  console.log('Authenticated as:', sessionResult.value);
}

// Fetch a user's Payment Endpoints
const listResult = await getPaymentList('user_public_key');
if (listResult.isOk()) {
  for (const entry of listResult.value) {
    console.log(`${entry.payment_endpoint_identifier}: ${entry.payment_endpoint_payload}`);
  }
}

// Get a specific Payment Endpoint Payload
const endpoint = await getPaymentEndpoint('user_public_key', 'bitcoin');

// Publish your own Payment Endpoint Payload
await setPaymentEndpoint('bitcoin', 'bc1q...');

// Remove a Payment Endpoint
await removePaymentEndpoint('bitcoin');

// Private Application Messages use Encrypted Link handles
const handshake = await initiateEncryptedLink(secretKeyHex, receiverPublicKey);
if (handshake.isOk()) {
  const progress = await advanceHandshake(handshake.value);
  if (progress.isOk() && progress.value.status === 'complete') {
    const reference = await generatePaymentReference();
    if (reference.isErr()) {
      throw new Error(reference.error);
    }

    await setPrivatePaymentEnvelope(progress.value.linkHandle, {
      reference: reference.value,
      entries: [
        { payment_endpoint_identifier: 'btc-lightning-bolt11', payment_endpoint_payload: '{"value":"lnbc1..."}' },
      ],
    });
    const privatePaymentEnvelope = await getPrivatePaymentEnvelope(progress.value.linkHandle);

    await issueReceipt(progress.value.linkHandle, {
      reference: reference.value,
      payment_endpoint_identifier: 'btc-lightning-bolt11',
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

- **`initialize()`** — Initialize the React Native binding. Call once at startup.

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

- **`getPaymentList(publicKey)`** — Fetch all public Payment Endpoints for a user.
- **`getPaymentEndpoint(publicKey, paymentEndpointIdentifier)`** — Fetch a specific public Payment Endpoint Payload.
- **`setPaymentEndpoint(paymentEndpointIdentifier, paymentEndpointPayload)`** — Publish/update a public Payment Endpoint.
- **`removePaymentEndpoint(paymentEndpointIdentifier)`** — Remove a public Payment Endpoint.

### Private Payment Envelopes and Receipts

- **`defaultMaxSendRetries()`** — Get the default Private Application Message send retry count.
- **`defaultMaxRecoveryAttempts()`** — Get the default handshake recovery-attempt count.
- **`generatePaymentReference()`** — Generate a UUID-v4 Payment Reference for Private Payment Envelope and Receipt correlation.
- **`initiateEncryptedLink(secretKeyHex, receiverPublicKey)`** — Start an Encrypted Link Handshake as initiator.
- **`acceptEncryptedLink(secretKeyHex, senderPublicKey)`** — Start an Encrypted Link Handshake as responder.
- **`advanceHandshake(handshakeId)`** — Advance a handshake; returns pending handshake handle or complete link handle.
- **`setEncryptedLinkHandshakeMaxRecoveryAttempts(handshakeId, max)`** — Override recovery attempts for a pending handshake.
- **`setEncryptedLinkMaxSendRetries(linkId, max)`** — Override send retries for an established Encrypted Link.
- **`setPrivatePaymentEnvelope(linkId, envelope)`** — Send the complete Private Payment Envelope over the Encrypted Link. The envelope contains `reference` and `entries`.
- **`getPrivatePaymentEnvelope(linkId)`** — Receive the newest Private Payment Envelope from the Encrypted Link, or `null` when none is available.
- **`issueReceipt(linkId, draft)`** — Store an encrypted Receipt and send Receipt Access over the Encrypted Link.
- **`getReceiptAccess(linkId)`** — Receive all currently available Receipt Access descriptors in FIFO order.
- **`receiptLocation(reference)`** — Return the canonical homeserver Receipt Location for a Payment Reference.
- **`decryptReceipt(encryptedJson, key, location)`** — Decrypt an encrypted receipt fetched from the homeserver.
- **`serializeEncryptedLinkHandshake(handshakeId)`** / **`restoreEncryptedLinkHandshake(secretKeyHex, snapshotHex)`** — Persist and restore pending handshakes.
- **`serializeEncryptedLink(linkId)`** / **`restoreEncryptedLink(secretKeyHex, snapshotHex)`** — Persist and restore established Encrypted Links.
- **`encryptedLinkSnapshotRecipient(snapshotHex)`** / **`encryptedLinkHandshakeSnapshotRecipient(snapshotHex)`** — Inspect the counterparty embedded in a snapshot.
- **`closeEncryptedLink(linkId)`** — Close an established Encrypted Link native handle.
- **`dropEncryptedLinkHandshake(handshakeId)`** — Drop a pending handshake native handle.

Private Payment Envelopes use Latest-State Message semantics: older queued envelopes are superseded by the newest queued envelope update. Receipt Access uses Event Message semantics and should be processed in order. Serialized snapshots and Receipt Decryption Keys contain sensitive key material and should be stored encrypted at rest.

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
