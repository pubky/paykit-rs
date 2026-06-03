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
  setPrivatePaymentList,
  receivePrivateApplicationMessages,
  prepareReceipt,
  storePreparedReceipt,
  sendReceiptAccess,
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
  for (const paymentEndpoint of listResult.value) {
    console.log(`${paymentEndpoint.payment_endpoint_identifier}: ${paymentEndpoint.payment_endpoint_payload}`);
  }
}

// Get a specific Payment Endpoint Payload, or null when absent
const endpoint = await getPaymentEndpoint('user_public_key', 'btc-bitcoin-p2tr');

// Publish your own Payment Endpoint Payload
await setPaymentEndpoint('btc-bitcoin-p2tr', 'bc1p...');

// Remove a Payment Endpoint
await removePaymentEndpoint('btc-bitcoin-p2tr');

// Private Application Messages use Encrypted Link handles
const handshake = await initiateEncryptedLink(secretKeyHex, receiverPublicKey);
if (handshake.isOk()) {
  const progress = await advanceHandshake(handshake.value);
  if (progress.isOk() && progress.value.status === 'complete') {
    await setPrivatePaymentList(progress.value.linkHandle, {
      payment_endpoints: [
        { payment_endpoint_identifier: 'btc-lightning-bolt11', payment_endpoint_payload: '{"value":"lnbc1..."}' },
      ],
    });
    const messages = await receivePrivateApplicationMessages(progress.value.linkHandle);
    if (messages.isOk()) {
      for (const message of messages.value) {
        console.log(message.kind ?? '<unknown>');
      }
    }
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
- **`getPaymentEndpoint(publicKey, paymentEndpointIdentifier)`** — Fetch a specific public Payment Endpoint Payload, or `null` when absent.
- **`setPaymentEndpoint(paymentEndpointIdentifier, paymentEndpointPayload)`** — Publish/update a public Payment Endpoint.
- **`removePaymentEndpoint(paymentEndpointIdentifier)`** — Remove a public Payment Endpoint.

### Private Payment Lists and Receipts

- **`defaultMaxSendRetries()`** — Get the default Private Application Message send retry count.
- **`defaultMaxRecoveryAttempts()`** — Get the default handshake recovery-attempt count.
- **`initiateEncryptedLink(secretKeyHex, receiverPublicKey)`** — Start an Encrypted Link Handshake as initiator.
- **`acceptEncryptedLink(secretKeyHex, senderPublicKey)`** — Start an Encrypted Link Handshake as responder.
- **`advanceHandshake(handshakeId)`** — Advance a handshake; returns pending handshake handle or complete link handle.
- **`setEncryptedLinkHandshakeMaxRecoveryAttempts(handshakeId, max)`** — Override recovery attempts for a pending handshake.
- **`setEncryptedLinkMaxSendRetries(linkId, max)`** — Override send retries for an established Encrypted Link.
- **`setPrivatePaymentList(linkId, list)`** — Send the complete Private Payment List over the Encrypted Link. The list contains `payment_endpoints`.
- **`parsePrivatePaymentListJson(json)`** — Parse a Private Payment List JSON message.
- **`receivePrivateApplicationMessages(linkId)`** — Receive the current raw Private Application Message batch in send order. SDK/runtime code should persist `raw_json`, route messages with recognized `kind` values, and handle `version`/`kind` as nullable for malformed common headers.
- **`parsePaymentRequestEventMessage(message)`** — Parse a raw Private Application Message as a Payment Request event.
- **`serializePaymentRequestEvent(event)`** — Serialize a Payment Request event to canonical JSON.
- **`validatePaymentProofForRequest(proof, request)`** — Validate stateless proof/request correlation fields.
- **`sendPaymentRequest(linkId, event)`**, **`sendPaymentRequestAcceptance(linkId, event)`**, **`sendPaymentRequestRejection(linkId, event)`**, **`sendPaymentRequestCancellation(linkId, event)`**, **`sendPaymentProof(linkId, event)`** — Send Payment Request protocol events over the Encrypted Link.

Payment Request objects use protocol-shaped nullable fields. Keep `proposal_expires_at`, `recurrence`, `recurrence.ends_at`, and `billing_period` present with `null` when unset, especially when persisting app-side event objects.

- **`prepareReceipt(linkId, draft)`** — Prepare a plaintext Receipt, Encrypted Receipt, and matching Receipt Access descriptor. `draft.metadata` is an optional JSON object; `draft.payment_request_id` and `draft.billing_period` may be used for Payment Request correlation.
- **`storePreparedReceipt(prepared)`** — Store a prepared Encrypted Receipt at its Receipt Location. Receipt Location is a path on the issuer's homeserver; pair it with the Receipt Access sender/issuer context when retrieving.
- **`sendReceiptAccess(linkId, access)`** — Send a prepared Receipt Access descriptor over the Encrypted Link.
- **`parseReceiptAccessEventMessage(message)`** — Parse a raw Private Application Message as a Receipt Access event.
- **`parseReceiptAccessJson(json)`** — Parse a Receipt Access JSON message.
- **`receiptLocation(receiptId)`** — Return the canonical homeserver Receipt Location path for a Receipt ID.
- **`decryptReceipt(encryptedJson, key, location)`** — Decrypt an Encrypted Receipt fetched from the homeserver into a local plaintext Receipt. `location` is the Receipt Location path from Receipt Access.
- **`serializeEncryptedLinkHandshake(handshakeId)`** / **`restoreEncryptedLinkHandshake(secretKeyHex, snapshotHex)`** — Persist and restore pending handshakes.
- **`serializeEncryptedLink(linkId)`** / **`restoreEncryptedLink(secretKeyHex, snapshotHex)`** — Persist and restore established Encrypted Links.
- **`encryptedLinkSnapshotRecipient(snapshotHex)`** / **`encryptedLinkHandshakeSnapshotRecipient(snapshotHex)`** — Inspect the counterparty embedded in a snapshot.
- **`closeEncryptedLink(linkId)`** — Close an established Encrypted Link native handle.
- **`dropEncryptedLinkHandshake(handshakeId)`** — Drop a pending handshake native handle.

Private Payment Lists use Latest-State Message semantics at the SDK/runtime layer: older queued list messages are superseded by the newest valid queued list update, and malformed newer list messages do not supersede the latest valid state. Receipt Access and Payment Request messages use Event Message semantics and should be processed in order. Serialized snapshots and Receipt Decryption Keys contain sensitive key material and should be stored encrypted at rest.

Raw Private Application Messages may contain secret material such as Receipt Decryption Keys. Persist `raw_json` securely for durable routing, but do not log it or include it in telemetry.

Payment Request helpers are stateless. They expose typed records, send helpers, raw event parsing, canonical event serialization, and proof/request correlation validation. They do not derive lifecycle state, enforce roles, schedule recurring payments, or validate method-specific proofs.

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
