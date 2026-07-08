# @synonymdev/react-native-paykit

React Native wrapper for the Paykit SDK bindings.

This package exposes the SDK binding foundation: SDK configuration helpers,
Pubky key/session helper functions, and Pubky URI parsing. Native Swift/Kotlin
consumers can use the generated SDK runtime objects directly; the TypeScript
facade intentionally exposes helper APIs only, not the full stateful runtime
API.

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

No additional steps. Gradle picks up the module automatically.

## Usage

```typescript
import {
  defaultConfig,
  requiredSessionCapabilities,
  pubkyPublicKeyFromBip39Mnemonic,
} from '@synonymdev/react-native-paykit';

const config = await defaultConfig('bitkit/wallet');
if (config.isErr()) {
  throw new Error(config.error);
}

const capabilities = await requiredSessionCapabilities(config.value);
if (capabilities.isErr()) {
  throw new Error(capabilities.error);
}

const publicKey = await pubkyPublicKeyFromBip39Mnemonic(mnemonicPhrase);
if (publicKey.isOk()) {
  console.log(publicKey.value);
}
```

All functions return `Promise<Result<T>>` using
[`@synonymdev/result`](https://www.npmjs.com/package/@synonymdev/result).
Errors are structured JSON strings. Use `parsePaykitError(error)` to read the
stable `category`, `code`, and redacted `context`.

## API

- `defaultConfig(receiverPath)` — return the default Paykit SDK config for a
  Paykit receiver path.
- `defaultPubkyClientConfig()` — return the default Pubky client config used by
  binding-owned Pubky clients.
- `requiredSessionCapabilities(config)` — return Pubky capabilities required by
  the supplied config.
- `pubkyPublicKeyFromBip39Seed(seedBase64)` — derive the corresponding Pubky
  public key from a 64-byte BIP39 seed without returning the local secret key
  to JavaScript.
- `pubkyPublicKeyFromBip39Mnemonic(mnemonicPhrase)` — derive the corresponding
  Pubky public key from a BIP39 English mnemonic phrase without returning the
  local secret key to JavaScript.
- `parsePubkyAuthUrl(authUrl)` — parse public details from a Pubky auth URL.
- `resolvePubkyUrl(uri)` — resolve a Pubky URI to its transport URL.
- `parsePubkyResource(uri)` — parse a `pubky://<public-key>/<path>` resource
  into owner, path, and transport URL.
- `parsePaykitError(error)` — parse a Result error string into structured
  Paykit error details.

## Updating Bindings

After rebuilding `paykit-ffi`, copy the fresh generated bindings and native
libraries into this package:

```sh
cd ../paykit-ffi
./build.sh all
cd ../paykit-react-native
./scripts/update-bindings.sh
```

This copies:

- iOS: `PaykitUniFFI.swift`, `paykitFFI.h`, `Paykit.xcframework`
- Android: generated Kotlin files, `jniLibs/*.so` for all architectures, and
  the Android verifier helper jar

`npm pack` runs the same sync step before packaging so the published package
contains the native artifacts even though they are not committed under this
directory.

## Architecture

```text
paykit-ffi/                       (Rust + UniFFI)
  ↓ uniffi-bindgen
  ├── bindings/ios/               (Swift + XCFramework)
  └── bindings/android/           (Kotlin + .so libs)
  ↓
paykit-react-native/              (this package)
  ├── ios/Paykit.swift            (hand-written RN bridge)
  ├── ios/Paykit.mm               (ObjC++ registration)
  ├── android/.../PaykitModule.kt (hand-written RN bridge)
  └── src/index.tsx               (TypeScript API)
```

## License

MIT
