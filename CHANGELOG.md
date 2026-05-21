# Changelog

All notable changes to this project will be documented in this file.

The format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Private encrypted receipt APIs in `paykit-lib`: `ReceiptDraft`, `Receipt`,
  `ReceiptAccess`, `IssuedReceipt`, `ReceiptDecryptionKey`, `issue_receipt`,
  `get_receipt_access`, and `decrypt_receipt`.
- FFI receipt records and exports in `paykit-ffi`: `FfiReceiptDraft`,
  `FfiReceipt`, `FfiReceiptAccess`, `FfiIssuedReceipt`,
  `paykit_issue_receipt`, `paykit_get_receipt_access`,
  `paykit_receipt_location`, and `paykit_decrypt_receipt`.

### Changed
- Private encrypted messages now distinguish latest-state private-payment
  envelopes from event-like receipt-access messages. `get_private_payments`
  returns the newest valid private-payment envelope, while `get_receipt_access`
  returns all currently available receipt access descriptors as a FIFO vector.
- Unsupported syntactically valid private application message kinds are logged
  and dropped rather than buffered indefinitely.
- `MethodId::new("private")` is now rejected with `PaykitError::Validation` because
  `private` is reserved for private-payment storage paths.

### Security
- Receipt decryption keys are redacted from Rust `Debug`/`Display` formatting
  in the library and from FFI wrapper debug output. Callers must still treat raw
  key fields returned through FFI as secrets.
- Receipt access locations are validated against their `PaymentReference`, and
  decrypted receipt plaintext is rejected if its reference does not match the
  authenticated receipt location.

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

[0.1.0-rc2]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc2
[0.1.0-rc1]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0-rc1
[0.1.0]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0
