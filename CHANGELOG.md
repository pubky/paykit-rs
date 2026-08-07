# Changelog

All notable changes to this project will be documented in this file.

The format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-rc43] - 2026-08-05

### Fixed
- Rejected invalid local Pubky testnet hosts before constructing the native
  client, avoiding a panic for malformed configuration.

## [0.1.0-rc42] - 2026-08-05

### Changed
- Simplified binding-layer Pubky network configuration so production remains
  the default and setting a local testnet host opts into local testnet.

## [0.1.0-rc41] - 2026-08-04

### Added
- Exposed explicit production and local-testnet Pubky client configuration to
  Swift and Kotlin consumers, including custom DNS hostname and IPv4 support
  for emulator-hosted test environments.

## [0.1.0-rc40] - 2026-07-27

### Fixed
- Published Android libraries with stable GNU build IDs.

## [0.1.0-rc39] - 2026-07-21

### Fixed
- Republished the iOS and Android release artifacts under a fresh release
  candidate after the Android `0.1.0-rc38` package upload ended in an
  immutable GitHub Packages artifact conflict.

## [0.1.0-rc38] - 2026-07-20

### Added
- Private payment resolution now returns an opaque Private Payment List version
  and accepts the last consumed version, returning a distinct waiting status
  until a newer complete list is available.

### Changed
- **Breaking (Rust API):** `PaykitSdkError::NotFound`, `Protocol`, `Policy`,
  and `RecoveryRequired` changed from tuple variants (`NotFound(String)`) to
  struct variants (`NotFound { context: String, source: Option<anyhow::Error> }`),
  matching the other four variants. Update constructions to
  `NotFound { context: msg, source: None }` and patterns to
  `NotFound { context, .. }`. The generated Swift/Kotlin error shape is
  unchanged.
- A missing Encrypted Receipt during `retrieve_receipt` now surfaces as the
  `not_found` FFI error code instead of `transport_error`.
- FFI exception messages are redacted:
  - anyhow cause chains carried in the SDK error's `source` (which can carry
    request URLs and response bodies) are no longer rendered into exception
    text; the only `source` that survives the FFI conversion is a
    `PaykitFfiError` stashed by paykit-ffi's own callback plumbing,
    recovered by downcast (see Fixed).
  - Receipt Locations no longer appear in storage/retrieval error messages.
  - These Receipt and Receipt Access error paths now use static labels: the
    shared version/kind check (which also backs the Payment Request event
    wire parsers), the encrypted-envelope serde, base64, and nonce-length
    checks, the decrypted-plaintext and Receipt Access JSON parses, the
    fetched-body UTF-8 check, Receipt Decryption Key validation, and
    backup-restore encrypted-receipt validation. Values from those paths no
    longer reach generated Swift/Kotlin exception text. Private Payment
    List parsing and the SDK's outbound private-message validation still
    embed offending values in error contexts; redacting those is a known
    follow-up.
- Rust SDK errors converted from `paykit_lib::PaykitError::InvalidData` no
  longer retain the structured source chain: the lib-to-SDK conversion drops
  the `source` before the FFI boundary, so `Error::source()` returns `None`
  and `Debug` output no longer renders the dropped cause chain. Detail that
  a lib call site folds into the `context` string itself is unaffected by
  this conversion and still appears in `Display`/`Debug`.
- Split public and private payment resolution across the SDK and platform
  bindings. Each mode now has distinct receiving details, endpoint candidates,
  adapter callbacks, statuses, and result types.

### Fixed
- Platform callback errors now survive the FFI -> SDK -> FFI round trip
  losslessly for all eight error variants: the original variant, custom
  machine-readable code, and reason are recovered by downcast instead of
  degrading to the variant's generic code.

### Removed
- Removed mixed contact-payment resolution and implicit private-to-public
  fallback. Applications now choose public or private payment resolution
  explicitly.

## [0.1.0-rc37] - 2026-07-17

### Added
- Receiver Markers now publish a receiver-scoped Noise public key. The SDK
  discovers that key before establishing an Encrypted Link, so private path
  derivation no longer requires access to the Pubky identity secret key.
- SDK and FFI session access now carry a separately persisted receiver Noise
  secret key, including secure-storage import/export support in the bindings.

### Changed
- Encrypted Link, handshake, and recovery path derivation now use receiver
  Noise keys while retaining Pubky identity keys solely for homeserver routing
  and receiver-pair domain separation.
- Encrypted Link snapshots now include the counterparty receiver Noise public
  key so existing links can be restored without repeating public discovery.
- Session signup, signin, auth completion, and import require the receiver Noise
  key explicitly so reauthentication can reuse it. Ring- or server-owned Pubky
  identities remain private-link-capable without exposing the identity secret.
- Identity status now reports the optional persisted public key and live-session
  availability directly, removing redundant capability enums and flags.

## [0.1.0-rc36] - 2026-07-14

### Fixed
- Corrected Android certificate verification so unavailable CRL status does
  not masquerade as explicit certificate revocation during Pubky Auth relay
  delivery. Explicit revocation and unrelated validation failures remain
  fail-closed.

## [0.1.0-rc35] - 2026-07-14

### Added
- Added high-level application-defined companion claim approval for
  `pubkyauth://` requests, including request-bound identity signatures,
  claim-specific relay delivery, and fail-closed AuthToken ordering.
- Exposed generic companion claim inputs and distinct approval errors in the
  generated Swift and Kotlin bindings, leaving application payload schemas and
  capability choices to SDK integrators.

## [0.1.0-rc33] - 2026-07-08

### Changed
- Public Payment Endpoints, private message paths, receipt locations, recovery
  markers, Paykit Profile paths, Contact Record paths, and Paykit blob paths
  are now scoped under explicit receiver paths.
- Encrypted Link and handshake snapshots now persist receiver scope with the
  underlying Noise state and reject restore into mismatched receiver paths.
- SDK and FFI session bootstrap now require caller-provided
  `requiredSessionCapabilities(config)` scopes instead of exposing a broad core
  Paykit capability helper.

### Removed
- Removed default Paykit profile/contact path constants and broad core session
  capability helpers. Apps must pass an explicit receiver path/config.
- Removed the unmaintained `paykit-react-native` package. Supported platform
  bindings are Swift and Kotlin.

## [0.1.0-rc32] - 2026-07-07

### Fixed
- Serialized recovery-marker outbox cleanup under the peer link operation lease,
  preventing concurrent relink handshakes from losing their first message.
- Allowed newer recovery markers to recover stale pending handshakes after the
  peer link operation freshness window.

## [0.1.0-rc31] - 2026-07-06

### Fixed
- Rebuilt and republished SDK/FFI release artifacts so the Swift Package
  manifest checksum matches the uploaded `Paykit.xcframework.zip` asset.

## [0.1.0-rc30] - 2026-07-06

### Fixed
- Rebuilt SDK and FFI release artifacts for the Encrypted Link recovery outbox
  cleanup release.
- Stabilized recovery marker testnet coverage around second-precision marker
  freshness.

## [0.1.0-rc29] - 2026-07-05

### Fixed
- Published release artifacts that include Encrypted Link recovery outbox
  cleanup for delete/recreate identity flows.

## [0.1.0-rc28] - 2026-07-05

### Fixed
- Remote Encrypted Link recovery markers no longer reset an already in-progress
  recovery handshake, preventing peers with mutual recovery markers from
  repeatedly restarting relink attempts.
- Fresh recovery handshakes now clear the local encrypted outbox so peers do not
  read stale ciphertext left at deterministic private stream paths after an
  identity is deleted and recreated.

## [0.1.0-rc27] - 2026-07-04

### Fixed
- Stored Encrypted Link handshakes that restore but fail to advance now mark
  the peer recovery-required so `ensure_link_with_peer` can start a fresh
  handshake instead of retrying the same stale snapshot.

## [0.1.0-rc26] - 2026-07-03

### Changed
- SDK and FFI Pubky auth start/resume helpers are now async so Ring auth flows
  start inside the UniFFI Tokio runtime on mobile.

### Fixed
- `ensure_link_with_peer` now starts a fresh deterministic Encrypted Link
  handshake in the same call after a stale stored handshake fails to restore and
  marks the peer recovery-required.

## [0.1.0-rc25] - 2026-07-02

### Changed
- Pubky secret key derivation now matches Pubky Core/Ring BIP39 behavior:
  SDK and FFI helpers derive from a BIP39 seed or mnemonic by using the first
  32 bytes of the BIP39 seed with an empty passphrase, instead of Paykit
  runtime-label HMAC derivation.

## [0.1.0-rc23] - 2026-07-02

### Fixed
- `ensure_link_with_peer` now starts a fresh deterministic Encrypted Link
  handshake when a peer is already marked recovery-required, instead of
  restoring stale local link or handshake snapshots.

## [0.1.0-rc21] - 2026-06-23

### Added
- Added the `paykit-sdk` crate, a stateful Pubky-backed runtime for Paykit
  identities, public endpoint publication, Encrypted Links, private streams,
  Private Payment Lists, Payment Requests, Receipts, contacts, profiles, and
  SDK-managed backup/restore.
- Added SDK FFI bindings for iOS and Android, including session bootstrap,
  state blob storage, payment adapter callbacks, contact payment resolution,
  private list publication, Payment Request flows, Receipt flows, profile/blob
  helpers, and backup export/restore.
- Added Encrypted Link Recovery Marker helpers and paginated Pubky public
  directory reads in `paykit-lib`.
- Added `specs/paykit-sdk.md` and `specs/paykit-sdk-bindings.md` to document
  the SDK architecture and mobile binding direction.

### Changed
- Public and private payment coordination now keeps durable runtime concerns in
  `paykit-sdk`, while `paykit-lib` remains the stateless protocol/Pubky helper
  crate.
- Mobile bindings now expose SDK-level workflows for the main Bitkit integration
  paths instead of requiring apps to compose low-level protocol operations.
- Pubky profile fallback now tolerates invalid optional profile fields by
  dropping those fields while preserving usable profile data.

## [0.1.0-rc19] - 2026-06-17

### Fixed
- Published Android native symbols from a reproducible release workflow with
  normalized release tags and pinned bindgen tooling.

## [0.1.0-rc18] - 2026-06-15

### Fixed
- Published stripped Android bindings with separate native debug symbols and
  16 KB LOAD alignment for all packaged ABIs.

## [0.1.0-rc12] - 2026-06-03

### Added
- Encrypted Receipt APIs in `paykit-lib`: `ReceiptDraft`, `Receipt`,
  `PreparedReceipt`, `ReceiptAccess`, `ReceiptDecryptionKey`,
  `prepare_receipt`, `store_prepared_receipt`, `send_receipt_access`,
  `parse_receipt_access_json`, and `decrypt_receipt`.
- Payment Request APIs in `paykit-lib`: request/proof/event types, send helpers,
  `parse_payment_request_event_message`, and stateless proof/request validation.
- FFI private stream and receipt exports in `paykit-ffi`:
  `FfiPrivateApplicationMessage`, `FfiReceiptDraft`, `FfiReceipt`,
  `FfiReceiptAccess`, `FfiPreparedReceipt`,
  `paykit_receive_private_application_messages`, `paykit_prepare_receipt`,
  `paykit_store_prepared_receipt`, `paykit_send_receipt_access`,
  `paykit_receipt_location`, and `paykit_decrypt_receipt`.
- FFI and React Native Payment Request exports for typed event records, send
  helpers, raw event parsing, canonical event serialization, and stateless
  proof/request correlation validation.

### Changed
- **BREAKING:** `paykit-lib` now treats Pubky as the only supported transport.
  Public Payment Endpoint APIs accept concrete Pubky SDK handles (`PubkySession` for
  writes and `PublicStorage` for reads) instead of generic transport traits.
- Private Application Messages now use
  `EncryptedLink::receive_private_application_messages` as the low-level
  ordered receive API. Typed receive/routing helpers belong in a future
  SDK/runtime layer.
- The private endpoint-sharing protocol and APIs now use
  `PrivatePaymentList`, `set_private_payment_list`, and
  `paykit.private_payment_list`.
- Receipt structs and wire JSON now use `payment_reference` and optional
  `PaymentAmount { value, asset }` instead of generic `reference`,
  string-only `amount`, and `currency`.
- Receipt Metadata is now a JSON object, matching Payment Request metadata.
  FFI exposes it as `metadata_json`; React Native exposes it as `metadata`.
- Event Messages now carry `EventId`; `ReceiptAccess` includes `event_id` and
  no longer carries an encryption `algorithm` field.
- Unsupported syntactically valid Private Application Message kinds are returned
  by the raw stream for future SDK/runtime routing instead of being hidden or
  dropped by typed getters.
- `PaymentEndpointIdentifier::new("private")` is now rejected with `PaykitError::Validation` because
  `private` is reserved for private Paykit storage paths.

### Removed
- **BREAKING:** Removed the `pubky` feature flag, `AuthenticatedTransport` /
  `UnauthenticatedTransportRead` traits, and Pubky transport adapter wrappers.
  Pubky dependencies are now unconditional.
- **BREAKING:** Removed library and FFI typed private receive helpers for
  individual private message kinds. Callers now use the ordered private message
  stream plus stateless parsers.
- **BREAKING:** Removed the one-shot receipt issuance convenience API in favor
  of explicit `prepare_receipt`, `store_prepared_receipt`, and
  `send_receipt_access` steps.
- **BREAKING:** Removed `reference` from `PrivatePaymentList`; Payment
  References now belong to Payment Requests, Payment Proofs, and Receipts.
- **BREAKING:** Removed Payment Reference generator helpers from the library,
  FFI, and React Native surfaces. Callers now provide their own free-form
  Payment Reference values.

### Security
- Receipt Decryption Keys are redacted from Rust `Debug`/`Display` formatting
  in the library and from Rust FFI wrapper debug output. Generated platform
  bindings still expose raw key fields, so callers must treat them as secrets.
- Receipt Locations are validated against their `ReceiptId`, and decrypted
  receipt plaintext is rejected if its Receipt ID does not match the
  authenticated Receipt Location.

## [0.1.0-rc2] - 2026-03-10

### Removed (BREAKING)
- **`get_profile` and `get_known_contacts` helpers removed.** Profile fetching and
  contact discovery are outside Paykit's scope — callers should use the Pubky SDK
  directly for these operations.
- **`pub use pubky` re-export removed from `paykit-lib`.** Downstream crates that
  relied on `paykit_lib::pubky` must add `pubky` as a direct dependency.
- **`PaykitError::Profile` variant removed.** Exhaustive `match` on `PaykitError`
  must drop this arm. The enum now has four variants.
- **`PUBKY_FOLLOWS_PATH` constant removed** from `transport::pubky`.
- **`pubky-app-specs` dependency removed** from `paykit-lib`.
- **FFI:** `paykit_get_profile`, `paykit_get_contacts`, `FfiProfile`,
  `FfiProfileLink`, and `PaykitFfiError::ProfileError` removed from bindings.

## [0.1.0-rc1] - 2026-03-04

### Changed (BREAKING)
- **`MethodId` is now validated at construction time.** The inner field is private;
  use `MethodId::new("lightning")?` instead of `MethodId("lightning".into())`.
  Accepted characters: ASCII alphanumeric, hyphens, underscores, and dots (max 64
  chars). Path traversal components (`.`, `..`) are rejected.
- **`EndpointData` inner field is now private.** Use `EndpointData::new("...")` to
  construct and `.as_str()` / `.into_inner()` to read.
- **New `PaykitError::Validation` variant.** Exhaustive `match` on `PaykitError`
  must now handle this variant, returned when `MethodId::new()` rejects invalid
  input.

### Added
- `MethodId::new()` — validated constructor enforcing safe path-segment invariants.
- `MethodId::as_str()`, `Display`, and `AsRef<str>` for read access.
- `EndpointData::new()`, `EndpointData::as_str()`, `EndpointData::into_inner()`,
  `Display`, and `AsRef<str>`.
- 23 unit tests covering `MethodId` validation (positive and negative cases) and
  `EndpointData` accessors.

### Security
- Mitigated path injection vulnerability in `MethodId`. Previously, a caller could
  inject path traversal sequences (`../`), null bytes, or special characters into
  storage paths via unvalidated `MethodId` values. Depending on how the storage
  backend handles paths, this could lead to writing to unintended locations, reading
  other users' data, or storage corruption.

### Migration guide
- Replace `MethodId("name".into())` with `MethodId::new("name")?` (or `.unwrap()`
  for known-good literals in tests).
- Replace `EndpointData("payload".into())` with `EndpointData::new("payload")`.
- Replace `.0` field access with `.as_str()` on both types.
- Add a `PaykitError::Validation(_)` arm to any exhaustive `match` on `PaykitError`.
- Downstream bindings (Swift/RN/Kotlin) that construct `MethodId` must be updated to
  handle the `Result` returned by `new()`.

## [0.1.0] - 2025-11-21

### Added
- Initial public release of `paykit-lib`, exposing a stateless transport layer for the
  Paykit protocol.
- Trait-based abstraction (`AuthenticatedTransport`, `UnauthenticatedTransportRead`)
  so integrators can inject their own SDKs or mocks.
- Feature-gated `pubky` adapters providing ready-made transport implementations plus
  exported constants for path prefixes.
- High-level helpers to set/remove endpoints, list supported payments, and list known
  contacts, including comprehensive async tests that run against the `pubky-testnet`
  harness.
- Crate metadata, README documentation, and MIT licensing to prepare the crate for
  publication on crates.io and docs.rs.

[Unreleased]: https://github.com/pubky/paykit-rs/compare/v0.1.0-rc43...HEAD
[0.1.0-rc43]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc43
[0.1.0-rc42]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc42
[0.1.0-rc41]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc41
[0.1.0-rc40]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc40
[0.1.0-rc39]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc39
[0.1.0-rc38]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc38
[0.1.0-rc37]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc37
[0.1.0-rc36]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc36
[0.1.0-rc35]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc35
[0.1.0-rc33]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc33
[0.1.0-rc32]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc32
[0.1.0-rc31]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc31
[0.1.0-rc30]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc30
[0.1.0-rc29]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc29
[0.1.0-rc28]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc28
[0.1.0-rc27]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc27
[0.1.0-rc26]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc26
[0.1.0-rc25]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc25
[0.1.0-rc24]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc24
[0.1.0-rc23]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc23
[0.1.0-rc21]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc21
[0.1.0-rc19]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc19
[0.1.0-rc18]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc18
[0.1.0-rc12]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc12
[0.1.0-rc2]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc2
[0.1.0-rc1]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc1
[0.1.0]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0
