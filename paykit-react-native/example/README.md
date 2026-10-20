# Paykit React Native example

A bare React Native 0.78.2 app (New Architecture, TypeScript) that runs a
smoke test of `@synonymdev/react-native-paykit` against a real Pubky
homeserver. It is the package's test harness and a reference for
integrators.

The app installs the package from an `npm pack` tarball, not a symlink, so it
tests what npm users get: the published files, the prebuilt native
libraries, the podspec and Gradle autolinking.

Everything else is fake or test-only:

- **Wallet.** `src/mocks/mockWallet.ts` returns fixed fake Lightning and
  on-chain strings that no wallet accepts. No funds move.
- **State store.** `src/mocks/stateBlobStore.ts` keeps the SDK state blob in
  memory. A real app must use synchronous encrypted storage.
- **Identities.** A and B come from public BIP39 test vectors in
  `src/identities.ts`. Anyone can derive their keys. Never use them for
  anything real.

## Build and install

From `paykit-react-native/`:

```sh
npm ci
npm run build:native      # or build:ios / build:android
npm run example:refresh   # npm pack, then install the tarball into example/
cd example/ios && pod install
```

Run `npm run example:refresh` again after every native rebuild or package
change. The tarball (`example/tarballs/`) is gitignored. The refresh keeps
the tarball's hash out of `example/package-lock.json`: with a hash, npm
installs its cached copy of an older tarball. It then checks that the
installed package matches the tarball and fails if it does not.

Start the app with `npm run example:ios` or `npm run example:android`. Both
start in `staging` mode.

The Android app uses NDK r28c (`28.2.13676358`), like the library. To build
with another installed NDK, pass `-PndkVersion=<version>` to Gradle.

For the emulator only, `npm run build:android -- --targets x86_64` builds
just that ABI; then build the app with `-PreactNativeArchitectures=x86_64`. Pick the mode on screen, or pass launch options
(below).

## Staging mode

Uses the staging homeserver behind `staging.pubky.app`. The first run for
each identity needs a single-use invite code from the Pubky staging invite
flow (for Synonym staff: the `pubky-staging-invite` skill). Enter the codes
on screen, or pass them at launch. Never commit them.

Without a code, the identity signs in to its existing staging account. Each
run writes a small amount of test data under the `paykit-rn-example` Paykit
App ID of the two test identities.

## Local mode

Uses a local Pubky stack on your machine: a DHT bootstrap node, a pkarr
relay, an HTTP relay and a homeserver that accepts signups without a code.

```sh
cd paykit-react-native/example
DOCKER_HOST=unix://$HOME/.colima/default/docker.sock npm run local-stack
```

The script runs `pubky-testnet` from the pubky-testnet crate, on the version
pinned in the workspace `Cargo.lock`. The first run builds it with
`cargo install` into `target/local-stack/`, which takes about two minutes.
Postgres comes from a throwaway Docker container, or from
`TEST_PUBKY_CONNECTION_STRING` when that is set. The stack is ready when the
script prints `PAYKIT_LOCAL_STACK_READY startup_ms=<n>`. Stop it with Ctrl+C;
that also removes the Postgres container.

`scripts/start-local-stack.sh start <dir>` runs the same script in the
background, with its PID and log in `<dir>`, and
`scripts/start-local-stack.sh wait <dir>` waits until it is ready. CI uses
them.

The stack uses fixed ports: 6881/udp, 6286, 6287, 6288, 15411 and 15412.
They must be free, so stop any other local Pubky testnet first.

- **iOS simulator:** reaches the stack on `127.0.0.1`.
- **Android emulator:** reaches the host as `10.0.2.2`. The homeserver
  publishes a `127.0.0.1` address, so also forward its ports:

  ```sh
  adb reverse tcp:6286 tcp:6286
  adb reverse tcp:6287 tcp:6287
  ```

## Launch options

| Option | Values |
| --- | --- |
| `paykitMode` | `staging` (default) or `local` |
| `paykitAutorun` | `true` runs every step at launch, without taps |
| `paykitLocalHost` | host of the local stack (default `127.0.0.1`, Android `10.0.2.2`) |
| `paykitHomeserver` | homeserver public key (defaults to the mode's homeserver) |
| `paykitSignupTokenA`, `paykitSignupTokenB` | staging invite codes |

iOS takes them as launch arguments:

```sh
xcrun simctl launch booted com.synonym.paykit.rnexample \
  -paykitMode local -paykitAutorun true
```

Android takes them as string intent extras:

```sh
adb shell am start -n com.synonym.paykit.rnexample/.MainActivity \
  --es paykitMode local --es paykitAutorun true
```

Launch arguments are visible to other tools on the machine, such as the
shell history and the process list. Pass invite codes this way only on a
development machine.

## Smoke steps

Each step logs `SMOKE_STEP <name> pass|fail|skip <reason>`, and the run ends
with `SMOKE_RESULT pass|fail`. A run passes when no step fails. The log never
contains secrets, mnemonics, invite codes, payloads or state blob bytes.

| Step | What it proves |
| --- | --- |
| `offline` | The native library loads and offline calls work: `defaultConfig`, `requiredSessionCapabilities`, and public keys derived from the test mnemonics. |
| `sign_up` | `FfiPubkySessionBootstrap` signs A and B up (or in), and an `FfiPaykitSdk` runs on the JS state store, session provider and payment adapter: `initialize`, `publishPaykitNoiseKeyAuthorization`, `publishPaykitApp`. |
| `public_endpoint` | A publishes its public Payment Endpoints (`syncPublicEndpoints`), B resolves them through its mock wallet (`resolvePublicContactPayment`), A removes them, and B sees them gone. |
| `private_payment_list` | A and B complete an Encrypted Link Handshake, A sends a Private Payment List, and B receives it (`currentPrivatePaymentLists`). |
| `payment_request` | A proposes a Payment Request, B accepts it (`claimAndAcceptPaymentRequest`), resolves it from A's Private Payment List, and submits a Payment Proof that A receives. |
| `restart` | A new SDK handle for A, on the same store, restarts from the stored blob bytes and still sees the Encrypted Link and the Payment Request. Its async calls load the blob through the synchronous store callback from a Rust worker thread. A timeout fails the step instead of hanging on a deadlock. |
| `typed_error` | An invalid Payment Endpoint Identifier (`../private`) throws a typed `PaykitFfiError.Protocol` with code `validation`. |
| `android_https` | Android only, staging only: a fresh SDK handle resolves A through the public pkarr relays over HTTPS, which needs the rustls platform verifier that the package initializes. The local stack has no HTTPS endpoint, so the step skips in local mode. |

## CI

The `smoke` and `smoke-ios` jobs in `.github/workflows/react-native.yml` run
on master pushes, nightly and on demand, not on pull requests. They start the
local stack on the runner in the background, build a Release app (the JS is
bundled, so no Metro server is needed; Android builds only `x86_64`), wait
for the stack, and run `scripts/android-smoke.sh` or
`scripts/ios-smoke.sh`, which launch the app in `local` mode with autorun
and wait for `SMOKE_RESULT pass`. macOS runners have no Docker, so the iOS
job uses Postgres from Homebrew through `TEST_PUBKY_CONNECTION_STRING`.
