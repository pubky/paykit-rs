# Paykit SDK Bindings Plan

## Goal

Define the platform binding surface for Paykit SDK.

Paykit SDK bindings should provide the app-facing mobile surface for Swift and
Kotlin apps. Platform integrators should not need to
reimplement SDK state-machine rules or combine low-level `paykit-lib` calls
themselves. The bindings should make common Paykit workflows easy, while
keeping sensitive low-level state and protocol escape hatches out of the
default app API.

## Design Principles

- Expose SDK workflows, not raw storage internals or low-level protocol
  building blocks.
- Keep the Rust SDK responsible for durable state transitions, ordering,
  dedupe, recovery, and validation.
- Make the configured Paykit App ID explicit while preserving identity-wide
  private state. Handles for apps sharing one identity must use the same
  durable SDK state and identity material.
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
```

Platform apps should depend on SDK bindings as the normal Paykit integration
surface. Protocol-level helpers should not remain as an equal parallel API.

Low-level `paykit-lib` functionality should only be exposed again when there is
a concrete SDK workflow, diagnostic export, or protocol-only use case that
needs it. Those escape hatches should be clearly separated from the default app
API.

React Native should be added later as a maintained wrapper on top of the
Swift/Kotlin binding surface once there is a tested owner for that package. It
is intentionally not part of the v0 checked-in binding surface.

## Runtime Handle

Bindings should expose one opaque SDK handle per app integration. The handle's
App ID attributes app-owned endpoints and messages; it does not create a
separate Encrypted Link or private stream.

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
Swift or Kotlin.

### StateBlobStorageAdapter

The platform storage boundary is an identity-wide durable SDK state blob plus
a revision token:

```text
load_state_blob() -> { bytes?, revision }
save_state_blob_atomically(bytes, expected_revision) -> new_revision
```

The Rust side should turn those callbacks into the real SDK storage adapter and
own transaction semantics internally. Platform code should only provide durable
loading and checked atomic replacement.

The SDK state blob is the authoritative logical runtime state shared by every
Paykit app using the Pubky identity. It is an internal, versioned serialization
and is not the public SDK backup export format. Bindings should treat it as an
opaque `SdkStateBlob`, and Rust should own schema validation and version
handling.

Each SDK storage transaction should load the current blob, mutate the full
logical state in Rust, then save the replacement with the loaded revision. If
the revision is stale, the platform store must fail with a structured storage
conflict instead of overwriting newer state. Bindings may also use a
per-identity platform lock, but the lock must cover the full load/mutate/save
transaction across app processes, extensions, services, and native modules that
share the same state blob.

The FFI storage wrapper serializes state-blob transactions inside one runtime
handle. Platform stores still need checked replacement by revision, especially
when multiple runtimes or app processes can share the same blob. Bindings may
offer helpers that encode the blob and revision into one platform record, but
apps should still treat the contents as opaque SDK state.

Storage callbacks execute while the FFI wrapper holds its per-handle storage
lock. They must not call back into that SDK handle.

Storage requirements for platform apps:

- `save_state_blob_atomically` must either fully replace the previous blob or
  leave the previous blob intact.
- Every successful changed write must return a non-empty opaque revision that
  has never represented an earlier state blob. Revisions must not be reused,
  even after intervening writes, because reuse permits ABA stale writes.
- The blob must be encrypted and protected as sensitive Paykit state.
- Apps should not log, inspect, or partially edit the blob.
- Every runtime for the same identity must resolve to this same logical blob.
- Multiple runtime instances need cross-process serialization or checked
  revision conflict handling.

This keeps platform integrators away from fragile details such as FIFO queue
ordering, monotonic IDs, Encrypted Link checkpoint coupling, lease validation,
dedupe indexes, and backup replacement invariants.

Reservation callbacks should avoid nullable meanings. Use an explicit response
shape: one value means "use current receiving details", and another means "use
exactly this reservation list". An empty reservation list is then a deliberate
empty private publication, not the same thing as no adapter response.

Bindings should also expose a direct reservation publication workflow for apps
that reserve receiving details outside the SDK callback:

- input: one counterparty plus the complete reserved receiving details for
  that counterparty
- empty reservation list: queue an empty Private Payment List for that
  counterparty
- output: per-counterparty queue and delivery failures, so apps do not have to
  merge queue reports with outbound-send reports manually

The app-facing private contact payment preparation helper must document its
sequence: refresh live session access, ensure or advance the private link when
possible, drain currently available private send/receive work for the peer,
then resolve only private endpoints. Public resolution is a separate call with
a separate result type and no implicit fallback in either direction.

Private resolution and preparation accept an optional consumed Private Payment
List version. Their private-only result returns the version from the same list
snapshot as the resolved endpoints. If the available list is not newer, the
binding returns `waitingForUpdatedPaymentList` with no payable endpoints. The
app must persist the version before handing a payment to the wallet; consuming
one endpoint consumes every endpoint returned with that version. A later
resolution includes candidates only from application lists updated after the
consumed version. An unchanged list from another application is not made fresh
by that update.

## Pubky Session Binding Shape

Bindings should make identity and live-session availability explicit.

The session binding is the boundary where an app exposes live Pubky access for
the shared Paykit identity. A binding may support Ring or another auth handoff
flow, but Paykit does not prescribe one product's identity UI.

The platform session provider should return one of:

- no live session access
- live session access, including local Pubky secret access when private Paykit
  workflows are required

`None` or `null` means no live session is currently available. Explicit
sign-out is a separate SDK call that clears this application's session access
without deleting the identity's shared Paykit state. Apps that should also
withdraw their public Payment Endpoints call `removePaykitApp` before sign-out.

The binding-level session API should not ask Swift or Kotlin to construct Rust
`pubky::PubkySession` or `pubky::Pubky` values directly. For
ordinary app use, SDK bindings should turn app-provided session material,
imported session secrets, or an auth handoff result into the Rust Pubky access
needed by the SDK. SDK bindings use `PubkySessionBootstrap` for signup, signin,
session import, capability-checked auth handoff, and `pubky://` normalization.
Binding helpers should request the capability scope returned by the active
`PaykitSdkConfig` and validate completed/imported sessions against that same
scope.
Bindings should expose a generic companion claim containing an integrator-owned
query parameter, claim type, and unsigned binary payload, plus one high-level
approval operation. Application-specific serialization stays in the
integrating app. Channel derivation, identity signatures, nonces, encryption,
and relay posting remain inside Rust. Binding errors should preserve distinct
invalid-request, invalid-claim, encryption, relay-delivery, and normal-auth
failure cases.
When bindings create the Pubky client internally, they should expose FFI-safe
client configuration for platform-owned network policy such as request
timeouts. The default configuration uses the public network; setting a local
testnet host switches to standard testnet ports so emulators and other isolated
runtimes can reach local services.
`PubkyLocalSecretKey` exposes domain-separated key derivation and
public-key-from-secret helpers. Platform bindings should wrap those helpers
where the platform has no better native primitive. Auth URLs and exported
session secrets are secret-bearing values, so bindings should avoid exposing
them through ordinary logs or debug output. If a platform binding cannot own
that construction, it must make the required Pubky binding dependency explicit
instead of implying that no Pubky integration is needed.

Bindings should also provide pure platform public-key formatting helpers for
ordinary app code and unit tests. Native UniFFI helpers can perform canonical
SDK validation, but app-wide display/normalization helpers should not require
native library loading in plain JVM or Swift unit tests.

The session provider should expose only the platform state the SDK needs:

- current local Pubky public key
- session material or opaque handle needed to build authenticated Pubky writes
- public storage configuration needed to build unauthenticated reads
- optional local Pubky identity secret-key access
- session clear operation for sign-out

Platform identity status should contain an optional public key and one
capability value: signed out, public-only, or private-link-capable. The public
key is the last initialized identity when known.

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

Callback-supplied native objects are scoped to that callback. Android
implementations should export needed values and close generated `AutoCloseable`
wrappers before returning; Swift relies on ARC for the equivalent lifetime.

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
`SdkStateBlob` and `SdkBackupBlob`. Shared state must be encrypted before it is
stored remotely. Exported backups likewise require caller-managed encryption
before cloud transport. A device-local implementation must use platform-protected
storage, but separate device-local blobs are not a valid cross-app shared-state
implementation.

Platform docs should also make the durability consequence explicit: if the
sensitive SDK state blob or exported SDK backup is lost, private Paykit runtime
state cannot be safely reconstructed from homeserver data alone. Apps may
recover by relinking peers and receiving fresh private data, but they should not
promise restoration of old private links, Receipt Access keys, private stream
history, Contact Records, or Payment Request/Receipt history without SDK
backup data.

## Default App Workflows

Bindings should expose high-level workflows before low-level records:

- initialize runtime
- sign out
- read the SDK state revision before/after mutating workflows so apps can mark
  backups dirty without remembering every state-changing method name
- sync public Payment Endpoints, including an explicit receiving-details helper
  for apps that want to avoid adapter-side mutable setup
- publish/fetch/delete Paykit Profile
- upload profile avatar blobs and fetch public Pubky files/text
- save/list/remove Contact Records
- sync public contact markers when enabled
- initiate or advance linked peer state
- receive private messages
- process outbound private messages
- publish Private Payment Lists
- sync Private Payment Lists for saved contacts, including a helper that also
  processes outbound delivery
- prepare and resolve private contact payment: ensure private state when
  possible, drain currently available private send/receive work for the peer,
  then return a private-only resolution with atomic list-version provenance
- resolve private contact payment without preparation when cached/private-link
  state should be used directly, optionally requiring a version newer than the
  last consumed Private Payment List
- resolve public contact payment independently, with public-only candidates and
  a public-only result
- queue and list Payment Requests
- submit Payment Proofs with caller-supplied proof data
- retrieve Receipts
- export and restore SDK-managed backup state, including text-form wrappers

Bindings should avoid typed private receive helpers that bypass durable ordered
stream handling.

Private-derived app records should carry enough freshness context for mobile UI
and payment logic. Where applicable, expose fields such as `received_at`,
`verified_with_private_link`, and `recovery_required`. Apps should not have to
infer whether a cached Private Payment List is current from unrelated identity
status fields, and public records must not be represented in private result
types.

## Testing Expectations

Binding tests should cover:

- SDK state blob load/save behavior
- atomic save failure preserving the previous blob
- stale state blob revision conflicts
- identity and live-session availability transitions
- missing live session access preserving cached private state
- identity-wide Noise key derivation from local Pubky secret material
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
- Mobile bindings use app-provided state-blob callbacks until the Pubky-hosted
  shared-state implementation is available. The callbacks must still represent
  one logical state backing per identity; first-party local file/keychain
  helpers are not a substitute for cross-app shared state.
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
