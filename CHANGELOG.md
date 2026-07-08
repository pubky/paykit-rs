# Changelog

All notable changes to this project will be documented in this file.

The format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/pubky/paykit-rs/compare/v0.1.0-rc32...HEAD
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
