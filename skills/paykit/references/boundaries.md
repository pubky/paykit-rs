# Integration Boundaries

## Verify the Installed Contract

Check source commit, Cargo locks, mobile package pins, generated declarations,
and custom-binding provenance. A fork's name/version does not establish API,
wire, or recovery compatibility. A successful handshake does not prove restart,
concurrency, or recovery safety.

| Layer | Ownership | Not supplied |
| --- | --- | --- |
| `paykit-lib` | Stateless protocol types, Pubky operations, Encrypted Link primitives | SDK queues, checkpoints, or application lifecycle |
| `paykit-sdk` | Shared identity state, private delivery/recovery, payment coordination and accounting | Wallet execution, chain observation, general chat, or product authorization |
| `paykit-ffi` | Swift/Kotlin SDK access and platform callbacks | Equivalence with an unrelated custom binding |
| Paykit Server / processor | Its published service contract and application-specific payment logic | A universal `paykit-rs` HTTP checkout API |
| Wallet / shop / Locks app | Financial execution, settlement/entitlement policy, UI, external idempotency | Automatic authority from a proof, signature, or readable Receipt |

Read the actual service contract for invoice prepare/activate/void routes,
signed HTTP auth, and content access. Those are distinct from Pubky grants,
Noise, and Paykit events. Do not invent a Receipt-to-Locks-access conversion.
A watch-only processor may observe settlement but cannot spend merely because
it issues receipts; its uptime does not make a mobile wallet always available.

## Browser and WASM

The workspace contains Rust and Swift/Kotlin surfaces, not a first-party browser
SDK or `paykit-wasm` member. Do not claim browser SDK availability unless the
actual package, build, networking/auth, storage, and lifecycle are verified.

For an existing browser binding, inspect whether it calls the stateful SDK or
only raw library functions. Check normal and target-specific dependencies at
its pin; the shared-state SDK uses Tokio in normal dependencies. Where the
target is installed, inspect `cargo tree -p paykit-sdk --edges normal --target
wasm32-unknown-unknown` and run `cargo check -p paykit-sdk --lib --target
wasm32-unknown-unknown`. Record real errors, not inferred platform blockers.
Compilation alone does not verify browser secret protection, storage atomicity,
async constraints, Pubky auth/networking, WebDAV locking, or runtime behavior.
Prefer exposing reusable Rust lifecycle logic when feasible; identify and test
any actual port/extension instead of silently implementing a second protocol.

## Custom Application Messages

Low-level Private Application Messages can carry app-defined kinds. The SDK's
typed payment workflows and automatic confirmations do not constitute a general
chat API. Unknown inbound kinds are retained in raw storage, but custom send,
application delivery, history, attachments, and acknowledgements still need an
explicit integration contract. Do not promise a drop-in replacement for custom
chat merely because it uses Noise.

There must be one coordinated owner of the shared private state machine. Never
run direct Noise send/receive alongside SDK workers on the same identity links.
Keep application history and logical IDs separate from replaceable snapshots.
An extension must participate in SDK durability and recovery, not fork its state.

Serialized private messages, including their JSON envelope, must fit
`PUBKY_NOISE_MSG_LEN` (currently 1000 bytes). Do not truncate or invent attachment
chunking. A `private` segment under `/pub/` is not an access-control boundary;
use encrypted protocol helpers, never plaintext secret-bearing state there.

Sources: [workspace](https://github.com/pubky/paykit-rs/blob/master/Cargo.toml),
[SDK manifest](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/Cargo.toml),
[stream classification](https://github.com/pubky/paykit-rs/blob/master/paykit-sdk/src/domain/private_stream/mod.rs).
