# Integration Boundaries

## Establish What Is Actually Installed

Identify the repository, commit/tag, package version, and generated artifact used
by each client and service. Check Cargo locks, mobile package pins, or vendored
WASM provenance. A fork's package name or version is not proof it has upstream's
API, wire format, or recovery behavior. Compare the relevant declarations and
implementation at those pins before suggesting a replacement.

Separate confirmed behavior from an integration report's assumptions. A successful
handshake test does not establish restart/recovery safety, and an unavailable
binding does not prove the underlying state machine cannot support that platform.

## Ownership

| Layer | What it owns | What not to assume |
| --- | --- | --- |
| `paykit-lib` | Stateless protocol operations, wire types, public storage helpers, Encrypted Link primitives | Calling raw link functions gives no SDK-managed durable queue or lifecycle |
| `paykit-sdk` | Local receiver state, atomic checkpoints, link recovery, typed payment workflows | It is not a wallet executor, chain observer, general chat SDK, or shared multi-device runtime |
| `paykit-ffi` | Swift/Kotlin access to the SDK with platform callbacks | A similarly named custom binding exposes the same stateful operations |
| Paykit Server or another processor | The service's published API and application-specific invoice/settlement logic | HTTP invoice prepare/activate/void routes or a hosted setup page are `paykit-rs` APIs |
| Wallet/shop/Locks integration | Wallet execution or commerce/entitlement policy, application persistence and UI | A submitted proof, transport success, or readable Receipt automatically authorizes goods/content |

Read the actual service contract when integrating a server. It can legitimately
own chain observation and coordinate orders while using Paykit underneath; do
not move those concerns into every wallet or invent an equivalent SDK method.
Signed HTTP authentication is also distinct from Pubky grant auth and Noise.
Signing an operation does not by itself establish its idempotency or settlement.

## Browser and WASM Work

Upstream master ships Rust and Swift/Kotlin surfaces, not a `paykit-wasm`
workspace member. Treat an existing browser package as its own binding until
its source and capabilities are verified.

Before recommending a TypeScript lifecycle reimplementation:

1. Check which layer the binding calls: raw `paykit-lib` handles or the
   stateful SDK. List the missing operations needed by the application.
2. Inspect normal versus development/target-specific dependencies. The SDK
   manifest on master does not depend on SQLx; Tokio is listed under development
   dependencies. That alone proves neither browser support nor a browser blocker.
3. In the dependency checkout, inspect
   `cargo tree -p paykit-sdk --edges normal --target wasm32-unknown-unknown` and,
   where the target is available, run
   `cargo check -p paykit-sdk --lib --target wasm32-unknown-unknown`.
   Record the actual compilation errors rather than inferring them from native
   tests, the processor's database, or an unrelated dependency.
4. Separately assess browser storage transactions, async/`Send` constraints,
   Pubky networking/auth, secret protection, bindings, and runtime tests. A
   compiling crate is not a working browser integration.
5. Prefer exposing reusable Rust lifecycle logic when practical. If a genuine
   gap requires a port or extension, describe it explicitly and test its wire,
   recovery, and persistence behavior against the other supported clients.
   Do not silently substitute a second recovery protocol.

## Custom Chat or Application Messages

The low-level Encrypted Link can carry Private Application Messages, including
app-defined kinds. The SDK's public outbound workflows handle Private Payment
Lists, Payment Requests/Proofs, and Receipt Access. Unknown inbound kinds are
preserved in raw SDK storage, but that is not a complete public custom-message
send/read API in the SDK or its mobile bindings.

Therefore, do not promise that replacing a raw chat handle with `PaykitSdk`
is a drop-in migration. Identify how custom sending, receiving, durable
application delivery, and shared link ownership will be exposed. Do not let a
raw handle and the SDK independently advance the same Noise checkpoint.

Prefer one owner for each link's state machine. App-specific envelopes, message
IDs, history, attachments, and any delivery acknowledgements remain distinct
from link recovery. Using SDK link logic does not supply those product contracts.

For low-level payloads, the serialized Private Application Message, including
its JSON envelope, must fit `PUBKY_NOISE_MSG_LEN` (currently 1000 bytes). Do not
silently truncate it or assume an attachment-transfer/chunking protocol exists.
Snapshots contain secret material; serialized does not mean encrypted at rest.
The `private` segment in Paykit's `/pub/...` storage paths does not make those
files access-controlled. Use the protocol's encrypted payload helpers; never
publish plaintext SDK state, snapshot secrets, or attachment keys there.

## Sources

- [Workspace](https://github.com/pubky/paykit-rs/blob/master/Cargo.toml) and
  [SDK dependencies](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/Cargo.toml).
- [SDK scope](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/README.md#current-scope).
- [Private message kinds](https://github.com/pubky/paykit-rs/blob/master/paykit-lib/src/encrypted_link/private_application_message.rs)
  and [SDK stream classification](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/domain/private_stream/mod.rs).
- [Domain vocabulary](https://github.com/pubky/paykit-rs/blob/master/THESAURUS.md).
