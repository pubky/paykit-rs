# Repository Guidelines

## Project Structure & Module Organization
- Workspace root hosts `Cargo.toml` that pins resolver `2` and registers members.
- Core library lives in `paykit-lib/` with its own `Cargo.toml` and `src/lib.rs`; treat this crate as the canonical Pubky-backed Paykit implementation.
- `./THESAURUS.md` is the authority for Paykit domain language. Use it before naming public APIs, docs sections, files, types, fields, endpoints, events, or components.
- Pubky routing helpers live in `paykit-lib/src/pubky_routing.rs`; it owns public endpoint storage operations and exports `PAYKIT_PATH_PREFIX` (`/pub/paykit/v0/`) plus `PAYKIT_PRIVATE_PATH_PREFIX` (`/pub/paykit/v0/private`). Reuse those constants instead of hard-coding strings.

## Build, Test, and Development Commands
- `cargo fmt` — run rustfmt on every crate; required before submitting changes.
- `cargo clippy --all-targets --all-features` — lint with the default warning set; fix or allow with justification.
- `cargo test` — executes unit tests + doc tests; use `cargo test mod_name::case` for focused runs.
- `cargo doc --no-deps` — verify public API docs compile; treat warnings as blockers because Paykit is SDK-facing.
- **Platform bindings**: Always build **all** platform bindings (`cd paykit-ffi && ./build.sh all`), never just one target. This ensures iOS and Android bindings stay in sync.

## Coding Style & Naming Conventions
- Follow Rust 2021 defaults: four-space indentation, snake_case for functions/modules, UpperCamelCase for types/traits, SCREAMING_SNAKE_CASE for consts.
- Public APIs must include `///` docs and favor explicit structs/enums over loosely typed maps.
- Keep files ASCII; when referencing Paykit vocabulary copy spellings from `README.md`.
- Prefer descriptive module names such as `routing`, `payments`, `endpoints` to mirror protocol sections.

## Pubky Routing & Dependency Handling
- Paykit supports Pubky as its only network/storage backend. Do not add generic transport traits, feature-gated transport adapters, or alternate-backend seams unless product direction changes explicitly.
- Keep the library stateless. Functions that touch remote public state should accept concrete Pubky SDK handles (`pubky::PubkySession` for authenticated writes, `pubky::PublicStorage` for unauthenticated reads).
- Public endpoint reads treat missing files/directories (404/GONE) as `None` or an empty list. Contact/payment discovery relies on directory listings rather than file contents.
- Document in each API that session creation, capability scope, and key rotation remain the caller's responsibility; Paykit only consumes the Pubky methods it needs.
- Timeout handling is the caller/Pubky-client responsibility, not Paykit's. Paykit does not enforce any deadline. The Pubky SDK exposes [`PubkyHttpClientBuilder::request_timeout`](https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout) for this purpose.

### Public vs. Private Payload Types
- **Public** payment methods use `EndpointData` (a UTF-8 `String` wrapper). Each method is stored as a separate file at a well-known Pubky path under `PAYKIT_PATH_PREFIX`. `set_payment_endpoint` / `remove_payment_endpoint` operate on `PubkySession`; `get_payment_list` / `get_payment_endpoint` operate on `PublicStorage`.
- **Private** payment methods are handled by `pubky-noise`'s `PubkyNoiseEncryptor`, which manages encryption, file naming, and storage via `send_message`/`receive_message`. Private payment plaintext is a versioned JSON envelope (`version`, `kind`, UUID-v4 `reference`, and `entries`). The `write_path` and `read_path` (asymmetric folder prefixes derived per-peer-pair via `pubky_noise::path_derivation::derive_asymmetric_paths`) are set during `initiate_encrypted_link` / `accept_encrypted_link`; pubky-noise manages individual file slots within those folders using a counter-based scheme.
- The helper functions `set_private_payments` and `get_private_payments` in `lib.rs` compose JSON serialization with `PubkyNoiseEncryptor::send_message`/`receive_message`. The caller is responsible for managing the payments map (adding/removing entries) and passing the complete map inside `PrivatePaymentsPayload` to `set_private_payments`.
- Private payments are latest-state data: when multiple private payment envelopes are queued, `get_private_payments` returns the latest and supersedes older private-payment envelopes. Receipt access is event-like: `get_receipt_access` must return every currently available `paykit.receipt_access` message as a FIFO vector instead of using latest-wins or one-at-a-time semantics. Unsupported syntactically valid private application message kinds must be logged and dropped rather than buffered indefinitely. Future event-like kinds must preserve all messages in send order once they are explicitly recognized by Paykit.
- The serialized private payments JSON must fit within a single pubky-noise message (`PUBKY_NOISE_MSG_LEN`, currently 1000 bytes).

## Testing Guidelines
- Rely on the standard Rust test harness; embed minimal reproducible examples in doc comments so `cargo test` exercises them automatically.
- Name tests using the pattern `test_<feature>_<case>()` (e.g. `test_supported_list_parsing`).
- New protocol features require at least one positive and one failure-path test.
- Aim for full coverage on serialization/deserialization paths that map to on-chain or network data.

## Commit & Pull Request Guidelines
- Use imperative, present-tense commit titles ≤72 chars (e.g., `Implement private list fetch API`).
- Each PR should describe motivation, list protocol impacts, and link relevant spec/issue references; include `cargo fmt`, `cargo clippy`, and `cargo test` outputs or mention if skipped.
- Highlight any changes to exposed structs or capability strings so downstream bindings (Swift/RN/Kotlin) can be updated in sync.

## Error Handling
- `PaykitError` has four variants: `Transport`, `NotFound`, `InvalidData`, and `Validation`. Any exhaustive `match` must cover all four.
- Use `PaykitError::Validation` for caller-supplied input that fails structural checks (e.g. invalid `MethodId`). Use `PaykitError::InvalidData` for data fetched from the network that turns out to be corrupt.

## Security & Configuration Tips
- Never commit real routing keys or secrets; stub them via env vars or fixture files ignored by git.
- Treat private encrypted payment, receipt, and storage-path handling code as sensitive: add comments describing assumptions about encryption, access control, and durability to aid auditing.
- `MethodId` is validated at construction time (`MethodId::new()`). The inner field is private. Do not add escape hatches that bypass validation — all values interpolated into storage paths must go through the validated constructor. Allowed characters: ASCII alphanumeric, hyphens, underscores, and dots; max 64 chars; no path traversal (`.`, `..`). The value `"private"` is reserved for private payment storage and is rejected.
- `EndpointData` has a private inner field accessed via `.as_str()` / `.into_inner()`. Construct via `EndpointData::new()`.
