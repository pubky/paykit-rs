# Changelog

All notable changes to this project will be documented in this file.

The format roughly follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.1.0]: https://github.com/synonymdev/paykit-rs/releases/tag/v0.1.0
