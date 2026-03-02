# Changelog

All notable changes to this project will be documented in this file.

The format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-03-02

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
- Removed `paykit_switch_network` — simplified global state.

### Added
- **UniFFI mobile bindings** (`paykit-ffi` crate) for iOS and Android, with build
  infrastructure (`build.sh`, `build_ios.sh`, `build_android.sh`, `Package.swift`,
  Android Gradle project) and CI workflow for Android publishing.
- `MethodId::new()` — validated constructor enforcing safe path-segment invariants.
- `MethodId::as_str()`, `Display`, and `AsRef<str>` for read access.
- `EndpointData::new()`, `EndpointData::as_str()`, `EndpointData::into_inner()`,
  `Display`, and `AsRef<str>`.
- 23 unit tests covering `MethodId` validation (positive and negative cases) and
  `EndpointData` accessors.
- Dev-only functions gated behind a feature flag.

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
- Remove any calls to `paykit_switch_network`.

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

[0.2.0]: https://github.com/pubky/paykit-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/pubky/paykit-rs/releases/tag/v0.1.0
