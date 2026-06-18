# Paykit SDK Bindings Plan

## Goal

Define the platform binding surface for Paykit SDK.

Paykit SDK bindings should provide the app-facing mobile surface for Swift,
Kotlin, and React Native apps. Platform integrators should not need to
reimplement SDK state-machine rules or combine low-level `paykit-lib` calls
themselves. The bindings should make common Paykit workflows easy, while
keeping sensitive low-level state and protocol escape hatches out of the
default app API.

## Design Principles

- Expose SDK workflows, not raw storage internals or low-level protocol
  building blocks.
- Keep the Rust SDK responsible for durable state transitions, ordering,
  dedupe, recovery, and validation.
- Preserve the app-owned runtime model. One binding handle represents one app,
  wallet, or receiver runtime; bindings should not require Ring, another
  wallet, or a shared identity coordinator before an app can use Paykit.
- Keep platform APIs ergonomic and hard to construct incorrectly.
- Treat storage, session access, payment execution, and UI as app-provided
  integration points.
- Preserve machine-readable status and error codes across every platform.
- Redact secrets, raw private payloads, Receipt Decryption Keys, and link
  snapshots from platform debug output.

## Architecture

Bindings should be an SDK-facing integration surface above `paykit-sdk`.
Mobile binding crates should expose this surface rather than a parallel
protocol-level API.

```text
paykit-sdk/
  Rust runtime, state machine, storage contract, adapters, records

paykit-ffi/
  SDK handle, FFI-safe DTOs, callback adapters, error mapping

paykit-react-native/
  TypeScript wrapper, discriminated unions, promise/callback bridge
```

Platform apps should depend on SDK bindings as the normal Paykit integration
surface. Protocol-level helpers should not remain as an equal parallel API.

Low-level `paykit-lib` functionality should only be exposed again when there is
a concrete SDK workflow, diagnostic export, or protocol-only use case that
needs it. Those escape hatches should be clearly separated from the default app
API.

For React Native, distinguish the generated native SDK bindings from the
hand-written TypeScript facade. The TypeScript facade may expose a smaller
helper surface, but a React Native app-facing SDK runtime wrapper should expose
the default workflows below before it replaces direct native SDK use.

## Runtime Handle

Bindings should expose one opaque SDK handle per app-owned local Paykit runtime.
This handle is the primary mobile API object.

The handle should own:

- the Rust `PaykitSdk` instance
- storage callback adapter or built-in platform storage adapter
- Pubky session provider callbacks
- payment adapter callbacks
- runtime locks needed for platform calls

Bindings should expose constructor/configuration APIs that make invalid setup
fail explicitly. Platform wrappers should prefer fallible constructors over
panic-style constructors.

## Storage Binding Shape

Do not expose `StorageTransaction` or record-level storage mutation APIs to
Swift, Kotlin, or React Native.

### StateBlobStorageAdapter

The preferred platform storage shape is a durable SDK state blob plus a
revision token:

```text
load_state_blob() -> { bytes?, revision }
save_state_blob_atomically(bytes, expected_revision) -> new_revision
```

The Rust side should turn those callbacks into the real SDK storage adapter and
own transaction semantics internally. Platform code should only provide durable
loading and checked atomic replacement.

The SDK state blob is an internal, versioned serialization of SDK storage state.
It is not the public SDK backup export format. Bindings should treat it as an
opaque `SdkStateBlob`, and Rust should own schema validation and
version handling.

Each SDK storage transaction should load the current blob, mutate the full
logical state in Rust, then save the replacement with the loaded revision. If
the revision is stale, the platform store must fail with a structured storage
conflict instead of overwriting newer state. Bindings may also use a
per-identity platform lock, but the lock must cover the full load/mutate/save
transaction across app processes, extensions, services, and native modules that
share the same state blob.

Storage requirements for platform apps:

- `save_state_blob_atomically` must either fully replace the previous blob or
  leave the previous blob intact.
- The blob must be protected as sensitive local Paykit state.
- Apps should not log, inspect, or partially edit the blob.
- Multiple runtime instances sharing the same blob need app-level
  serialization, a platform lock, or revision conflict handling.

This keeps platform integrators away from fragile details such as FIFO queue
ordering, monotonic IDs, Encrypted Link checkpoint coupling, lease validation,
dedupe indexes, and backup replacement invariants.

React Native bindings should keep SDK state blob storage in the native module by
default. Passing `SdkStateBlob` bytes through the JavaScript bridge
should be an explicit advanced mode because it creates extra copies, increases
devtools/logging exposure, and can add bridge-size and performance risks.

## Pubky Session Binding Shape

Bindings should make session capability explicit.

The session binding is the boundary where the app exposes live Pubky access to
its own Paykit runtime. It should not be modeled as a global identity
coordinator shared by all Paykit apps. A binding may support Ring or other auth
handoff flows, but ordinary Paykit integration should not require users to
install or authorize through another app first.

The platform session provider should return one of:

- no live session access
- public-only session access
- private-link-capable session access

`None` or `null` means no live session is currently available. It does not mean
explicit sign-out. Explicit sign-out should be a separate SDK call that clears
session access first and then clears SDK-managed identity-scoped state.
Bindings should document that apps must export and persist an SDK backup before
explicit sign-out if they want to restore the same user's private Paykit state
later.

The binding-level session API should not ask Swift, Kotlin, or React Native to
construct Rust `pubky::PubkySession` or `pubky::Pubky` values directly. For
ordinary app use, SDK bindings should turn app-provided session material,
imported session secrets, or an auth handoff result into the Rust Pubky access
needed by the SDK. SDK bindings use `PubkySessionBootstrap` for signup, signin,
session import, capability-checked auth handoff, and `pubky://` normalization.
Binding helpers should request the capability scope returned by the active
`PaykitSdkConfig` and validate completed/imported sessions against that same
scope.
When bindings create the Pubky client internally, they should expose FFI-safe
client configuration for platform-owned network policy such as request
timeouts.
`PubkyLocalSecretKey` exposes app/runtime-domain-separated key derivation and
public-key-from-secret helpers. Platform bindings should wrap those helpers
where the platform has no better native primitive. Auth URLs and exported
session secrets are secret-bearing values, so bindings should avoid exposing
them through ordinary logs or debug output. If a platform binding cannot own
that construction, it must make the required Pubky binding dependency explicit
instead of implying that no Pubky integration is needed.

The session provider should expose only the platform state the SDK needs:

- current local Pubky public key
- session material or opaque handle needed to build authenticated Pubky writes
- public storage configuration needed to build unauthenticated reads
- local secret-key access when Encrypted Links are available
- session clear operation for sign-out

Platform APIs should surface capability status in app-facing records so product
code can distinguish public-only mode from private-link-capable mode.

## Payment Adapter Binding Shape

Payment execution stays outside Paykit SDK, but bindings need a clear payment
adapter surface.

The payment adapter should work in batches. Given candidate Payment Endpoints
and optional Payment Amount context, it should return the payable candidates in
the order payment execution should try them.

Candidate batches should use stable opaque candidate IDs. Adapter callbacks
should return payable candidate IDs and errors by candidate ID instead of
copying full endpoint payload records back and forth. Raw Payment Endpoint
Payloads and payment targets should only be exposed through explicitly
sensitive fields or helper types.

Adapter callbacks should cover:

- local Receiving Detail generation for public and private Payment Lists
- batch payable-endpoint ordering
- payment target construction for payable endpoints
- optional Payment Endpoint Reservation creation
- Payment Endpoint Reservation release

The adapter should not be Paykit-specific to one payment method. Bitcoin,
Lightning, bank rails, and future payment methods should all fit through the
same endpoint/candidate model.

Payment Proof submission is a separate workflow. Proof data is caller-supplied
after external payment execution or settlement evidence is available; the
endpoint-selection adapter does not automatically create Payment Proofs unless
a future adapter callback is added for that purpose.

## Platform Data Shapes

Bindings should prefer platform-native discriminated shapes over records with a
string type plus many nullable variant fields.

Recommended shapes:

- Swift: enums with associated values or wrapper structs where generated FFI
  enums are not ergonomic.
- Kotlin: sealed classes or tagged data wrappers.
- React Native: TypeScript discriminated unions.

Platform enum/union wrappers that mirror SDK statuses, lifecycle states,
payment resolution results, recovery states, or error codes should include an
unknown case. When the underlying value is a string/tagged payload, preserve
the raw code/value. When a generated FFI enum only exposes a future Rust variant
as an unknown fallback, surface `unknown` rather than silently mapping it into a
misleading known platform value.

This applies especially to:

- Payment Request lifecycle actions and events
- payment resolution results
- payable endpoint ordering results
- Receipt retrieval status
- recovery marker status
- structured errors

Fields that are protocol-required but nullable should be represented clearly in
platform docs and wrappers. Apps should not mistake "required and null" for
"field may be omitted from protocol JSON".

## Errors

Bindings should preserve SDK error categories and stable machine-readable error
codes.

At minimum, platform errors should expose:

- category
- code
- redacted context
- optional underlying platform error

Apps should be able to branch on identity failures, policy failures, recovery
required, storage failures, transport failures, protocol failures, and payment
adapter failures without parsing display strings.

Underlying platform errors exposed through app-facing objects must also be
redacted or sanitized. Raw provider/storage/adapter error strings should be
available only through an explicit debug channel, because they may contain file
paths, Payment Endpoint Payload snippets, provider metadata, reservation IDs,
Payment References, or other sensitive context.

## Async And Concurrency

Bindings should document callback and runtime concurrency rules.

Platform callbacks must not call back into the same SDK handle while a binding
call is waiting on that callback unless the binding explicitly supports
reentrancy. Bindings should serialize identity-scoped operations on one handle,
and should expose clear guidance for apps that create multiple handles sharing
the same storage blob.

Cancellation should be best-effort. If a platform cancels a call after the SDK
has started a durable transaction, the SDK should still finish or fail the
transaction consistently.

## Sensitive Data

Bindings should not expose sensitive state through default debug, string, or log
surfaces.

Sensitive fields include:

- session secrets and local secret keys
- Encrypted Link snapshots and handshake snapshots
- raw private message payloads
- Receipt Decryption Keys
- encrypted receipt payloads
- Payment References when they may contain invoice or order identifiers
- Receiving Details
- Payment Endpoint Payloads
- Payment Targets
- Payment Endpoint Reservation IDs and attribution
- payable endpoint ordering or provider metadata

If a platform wrapper must expose sensitive data for backup/export or storage
callbacks, that type should be documented as sensitive and must redact default
debug/string output.

State blobs and exported backups should use explicit object names such as
`SdkStateBlob` and `SdkBackupBlob`. Bindings should require
caller-managed encryption before cloud transport or cross-device backup. If a
blob is not app-encrypted, platform docs should require protected local storage:
iOS Keychain or protected files with backup exclusion as appropriate, and
Android encrypted storage or `noBackup` placement unless the app encrypts before
backup.

Platform docs should also make the durability consequence explicit: if the
sensitive SDK state blob or exported SDK backup is lost, private Paykit runtime
state cannot be safely reconstructed from homeserver data alone. Apps may
recover by relinking peers and receiving fresh private data, but they should not
promise restoration of old private links, Receipt Access keys, private stream
history, local Contact Records, or Payment Request/Receipt history without SDK
backup data.

## Default App Workflows

Bindings should expose high-level workflows before low-level records:

- initialize runtime
- sign out
- sync public Payment Endpoints
- publish/fetch Paykit Profile
- save/list/remove Contact Records
- sync public contact markers when enabled
- initiate or advance linked peer state
- receive private messages
- process outbound private messages
- publish Private Payment Lists
- resolve contact payment
- queue and list Payment Requests
- submit Payment Proofs with caller-supplied proof data
- retrieve Receipts
- export and restore SDK-managed backup state

Bindings should avoid typed private receive helpers that bypass durable ordered
stream handling.

Private-derived app records should carry enough source and freshness context for
mobile UI and payment logic. Where applicable, expose fields such as source
(`fresh_private`, `cached_private`, or `public`), `received_at`,
`verified_with_private_link`, `recovery_required`, and `public_only_session`.
Apps should not have to infer whether a cached Private Payment List is current
from unrelated identity status fields.

## Testing Expectations

Binding tests should cover:

- SDK state blob load/save/clear behavior
- atomic save failure preserving the previous blob
- stale state blob revision conflicts
- session capability transitions
- public-only behavior preserving cached private state
- payment adapter batch selection and reservation release
- candidate ID mapping across payment adapter callbacks
- structured error mapping
- unknown enum/union case preservation
- discriminated-union validation
- redacted debug output
- sensitive backup/state blob handling
- backup export/restore round trips
- cancellation or callback failure during SDK operations

Rust SDK tests remain responsible for state-machine correctness. Binding tests
should focus on platform shape, callback mapping, error mapping, and preventing
misconstruction.

## Non-Goals

- Do not maintain a low-level mobile protocol FFI as a second app-facing
  Paykit API.
- Do not expose the full Rust `StorageTransaction` trait to platform apps.
- Do not make platform apps parse raw Private Application Messages for normal
  workflows.
- Do not bake one payment method or wallet implementation into the binding API.
- Do not require a separate Pubky SDK integration for ordinary Paykit SDK use
  when bindings own Pubky session construction. If a binding cannot own that
  construction, document the Pubky binding dependency explicitly.
- Do not expose UI copy or product-specific screens from Paykit bindings.

## Implementation Decisions

- Start SDK bindings from a clean SDK-first FFI surface. Protocol-only exports
  should be added intentionally, not preserved by default.
- The first mobile storage implementation should use app-provided state blob
  callbacks. First-party file/keychain helpers can be added later only if one
  generic helper clearly fits multiple apps and does not hide platform security
  decisions.
- Paykit SDK bindings should give integrators one Paykit SDK package/import for
  normal app integration. That package should expose SDK workflows first; any
  lower-level protocol helpers should be intentionally added, documented, and
  kept separate from default app APIs.
- Default app APIs should expose redacted typed audit records only. Examples
  include event kind, status, timestamps, peer, error code, payload size,
  payload hash, and redacted summaries. Raw Private Application Message JSON,
  malformed event payloads, receipt-access descriptors, outbound queue records,
  and link recovery diagnostics should be available only through an explicitly
  named sensitive diagnostics export, because they can contain sensitive data.
