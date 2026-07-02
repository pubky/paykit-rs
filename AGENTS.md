# Repository Guidelines

## Project Structure & Module Organization
- Workspace root hosts `Cargo.toml` that pins resolver `2` and registers members.
- Core library lives in `paykit-lib/` with its own `Cargo.toml`; `src/lib.rs` is the public re-export facade, and feature modules under `src/` hold the implementation. Treat this crate as the canonical Pubky-backed Paykit implementation.
- `./THESAURUS.md` is the authority for Paykit domain language. Use it before naming public APIs, docs sections, files, types, fields, endpoints, events, or components.
- Pubky routing helpers live in `paykit-lib/src/pubky_routing.rs`; it owns receiver-scoped Payment Endpoint storage operations and path helpers. Reuse the helper functions instead of hard-coding `/pub/paykit/v0/receivers/{receiver_id}/...` or `/pub/paykit/v0/private/{receiver_id}/...` paths.

## Build, Test, and Development Commands
- `cargo fmt` — run rustfmt on every crate; required before submitting changes.
- `cargo clippy --all-targets --all-features` — lint with the default warning set; fix or allow with justification.
- `cargo test` — executes unit tests + doc tests; use `cargo test mod_name::case` for focused runs.
- `cargo doc --no-deps` — verify public API docs compile; treat warnings as blockers because Paykit is public API-facing.
- **Platform bindings**: Always build **all** platform bindings (`cd paykit-ffi && ./build.sh all`), never just one target. This ensures iOS and Android bindings stay in sync.

## Coding Style & Naming Conventions
- Follow Rust 2021 defaults: four-space indentation, snake_case for functions/modules, UpperCamelCase for types/traits, SCREAMING_SNAKE_CASE for consts.
- Public APIs must include `///` docs and favor explicit structs/enums over loosely typed maps.
- Keep files ASCII; when referencing Paykit vocabulary copy spellings from `THESAURUS.md`.
- Prefer descriptive module names such as `routing`, `payments`, `endpoints` to mirror protocol sections.

## Pubky Routing & Dependency Handling
- Paykit supports Pubky as its only network/storage backend. Do not add generic transport traits, feature-gated transport adapters, or alternate-backend seams unless product direction changes explicitly.
- Keep the library stateless. Functions that touch remote public state should accept concrete Pubky SDK handles (`pubky::PubkySession` for authenticated writes, `pubky::PublicStorage` for unauthenticated reads).
- Public Payment Endpoint reads treat missing files/directories (404/GONE) as `None` or an empty list. Contact/payment discovery relies on directory listings rather than file contents.
- Document in each API that session creation, capability scope, and key rotation remain the caller's responsibility; Paykit only consumes the Pubky methods it needs.
- Timeout handling is the caller/Pubky-client responsibility, not Paykit's. Paykit does not enforce any deadline. The Pubky SDK exposes [`PubkyHttpClientBuilder::request_timeout`](https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout) for this purpose.

### Public vs. Private Payload Types
- **Public** Payment Endpoints use `PaymentEndpointPayload` (a UTF-8 `String` wrapper). Each endpoint is stored as a separate file under a receiver-scoped Pubky path. `set_payment_endpoint` / `remove_payment_endpoint` operate on `PubkySession`; `get_payment_list` / `get_payment_endpoint` operate on `PublicStorage`.
- Counterparty-specific Payment Lists are shared as Private Payment List messages over `pubky-noise`'s `PubkyNoiseEncryptor`, which manages encryption, file naming, and storage via `send_message`/`receive_message`. The Private Payment List plaintext is versioned JSON (`version`, `kind`, and `payment_endpoints`). Payment References belong in Payment Requests, Payment Proofs, and Receipts, not in endpoint publication. The `write_path` and `read_path` (asymmetric folder prefixes derived per-counterparty pair via `pubky_noise::path_derivation::derive_asymmetric_paths`) are set during `initiate_encrypted_link` / `accept_encrypted_link`; pubky-noise manages individual file slots within those folders using a counter-based scheme.
- The helper function `set_private_payment_list` in `paykit-lib/src/private_payment_list.rs` composes JSON serialization with Encrypted Link send helpers. The caller is responsible for managing the Payment Endpoints in the Payment List and passing the complete map inside `PrivatePaymentList` to `set_private_payment_list`.
- Private Payment Lists use Latest-State Message semantics at the SDK/runtime layer: when multiple list messages are queued, the latest valid list supersedes older list messages. Receipt Access and Payment Request protocol messages (`paykit.payment_request`, acceptance, rejection, cancellation, and proof) use Event Message semantics. `EncryptedLink::receive_private_application_messages` is the durable low-level receive API for mixed private message kinds; SDK/runtime code routes that ordered stream and then calls stateless parsers such as `parse_payment_request_event_message`, `parse_receipt_access_event_message`, and `parse_private_payment_list_json`. Do not add typed receive getters to `paykit-lib`; typed private message consumption belongs in the SDK/runtime layer.
- The serialized Private Payment List JSON must fit within a single pubky-noise message (`PUBKY_NOISE_MSG_LEN`, currently 1000 bytes).

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
- Use `PaykitError::Validation` for caller-supplied input that fails structural checks (e.g. invalid `PaymentEndpointIdentifier`). Use `PaykitError::InvalidData` for data fetched from the network that turns out to be corrupt.

## Security & Configuration Tips
- Never commit real routing keys or secrets; stub them via env vars or fixture files ignored by git.
- Treat private Paykit message, Receipt, and storage-path handling code as sensitive: add comments describing assumptions about encryption, access control, and durability to aid auditing.
- `PaymentEndpointIdentifier` is validated at construction time (`PaymentEndpointIdentifier::new()`). The inner field is private. Do not add escape hatches that bypass validation — all values interpolated into storage paths must go through the validated constructor. Allowed characters: ASCII alphanumeric, hyphens, underscores, and dots; max 64 chars; no path traversal (`.`, `..`). The value `"private"` is reserved for private Paykit storage and is rejected.
- `PaymentEndpointPayload` has a private inner field accessed via `.as_str()` / `.into_inner()`. Construct via `PaymentEndpointPayload::new()`.
