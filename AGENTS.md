# Repository Guidelines

## Project Structure & Module Organization
- Workspace root hosts `Cargo.toml` that pins resolver `2` and registers members.
- Core library lives in `paykit-lib/` with its own `Cargo.toml` and `src/lib.rs`; treat this crate as the canonical abstraction over the routing network.
- Transport abstractions live in `paykit-lib/src/transport/`: `traits.rs` defines the public interfaces, while feature-gated adapters (e.g., `transport/pubky/*`) provide concrete implementations.
- `transport/pubky/mod.rs` exports `PAYKIT_PATH_PREFIX` (`/pub/paykit.app/v0/`) and `PUBKY_FOLLOWS_PATH` (`/pub/pubky.app/follows/`) to keep all Pubky paths consistent—reuse them instead of hard-coding strings.

## Build, Test, and Development Commands
- `cargo fmt` — run rustfmt on every crate; required before submitting changes.
- `cargo clippy --all-targets --all-features` — lint with the default warning set; fix or allow with justification.
- `cargo test` — executes unit tests + doc tests; use `cargo test mod_name::case` for focused runs.
- `cargo doc --no-deps` — verify public API docs compile; treat warnings as blockers because Paykit is SDK-facing.

## Coding Style & Naming Conventions
- Follow Rust 2021 defaults: four-space indentation, snake_case for functions/modules, UpperCamelCase for types/traits, SCREAMING_SNAKE_CASE for consts.
- Public APIs must include `///` docs and favor explicit structs/enums over loosely typed maps.
- Keep files ASCII; when referencing Paykit vocabulary copy spellings from `README.md`.
- Prefer descriptive module names such as `routing`, `payments`, `endpoints` to mirror protocol sections.

## Transport Abstraction & Dependency Injection
- Keep the library stateless. Functions that touch remote state must accept `AuthenticatedTransport` or `UnauthenticatedTransportRead` implementors instead of concrete SDK types.
- Pubky support lives behind the default `pubky` feature; adapters such as `PubkyAuthenticatedTransport` and `PubkyUnauthenticatedTransport` simply wrap `PubkySession` and `pubky::PublicStorage`. Disable the feature if you need to compile without the SDK.
- When adding or updating adapters, follow the convention: `fetch_payment_endpoint` returns `Option`, list operations treat 404s as empty, and contact discovery relies on directory listings rather than file contents.
- Document in each API that session creation, capability scope, and key rotation remain the caller’s responsibility; Paykit only consumes the trait methods it needs.

## Testing Guidelines
- Rely on the standard Rust test harness; embed minimal reproducible examples in doc comments so `cargo test` exercises them automatically.
- Name tests using the pattern `test_<feature>_<case>()` (e.g., `test_supported_list_parsing`).
- New protocol features require at least one positive and one failure-path test.
- Aim for full coverage on serialization/deserialization paths that map to on-chain or network data.

## Commit & Pull Request Guidelines
- Use imperative, present-tense commit titles ≤72 chars (e.g., `Implement private list fetch API`).
- Each PR should describe motivation, list protocol impacts, and link relevant spec/issue references; include `cargo fmt`, `cargo clippy`, and `cargo test` outputs or mention if skipped.
- Highlight any changes to exposed structs or capability strings so downstream bindings (Swift/RN/Kotlin) can be updated in sync.

## Security & Configuration Tips
- Never commit real routing keys or secrets; stub them via env vars or fixture files ignored by git.
- Treat private URL handling code as sensitive: add comments describing assumptions about encryption and access control to aid auditing.
