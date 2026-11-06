# Repository Guidelines (Paykit v0.2)

## Branch, Spec and Vocabulary
- The `v0.2` branch holds only Paykit v0.2 code. `master` holds v0.1 and is not changed from here. Work on feature branches cut from `v0.2`, with PRs into `v0.2`.
- The protocol is defined in `specs/v0.2/`:
  - `spec.md` decides behaviour: keys and apps, messages, the Pubky Switch, coordination, payment flows and lifecycle.
  - `wire-format.md` decides bytes: encodings, signatures, schemas and the HTTP API.
  - `vectors/` is normative. Every implementation reproduces all of its vector files byte for byte.
- **The spec wins.** Where the code and the spec disagree, the spec is right: fix the code, or stop and raise a spec change. Never diverge from the spec silently.
- **Spec changes come first.** A spec change goes in its own PR to `specs/v0.2/`, before any code that depends on it. Vectors are regenerated only through such a change.
- `./THESAURUS.md` is the authority for Paykit domain language. Use it before naming public APIs, docs sections, files, types, fields, endpoints, events or components.

## Project Structure & Module Organization
- The workspace root `Cargo.toml` pins resolver `2`, registers the members and holds the shared package metadata in `[workspace.package]`. `vendor/` holds the workspace's crate patch.
- **`paykit-lib`** is the stateless protocol core. It is pure: no I/O, no clock, no global state, no async. Time is always passed in. It covers encodings (`lp()`, `derive()`, labels), key derivations, signatures, envelopes and sealed values, every signed document, message checks, scopes and permissions, the pure claim and record rules, and chain verification.
- **`paykit-sdk`** is the stateful app SDK that wallets and seller apps use. It holds state, flows and network I/O: the Pubky Switch client, Homeserver reads and writes, the `StateStore` trait the host app implements, receiving and sending messages, the payment flows, provider moves, recovery and push registration.
- **`paykit-signer`** is the signer core: enrollment, `identity.json`, app approval, certificates and key bundles, revocation, provider moves, recovery, account erasure and the hand-over to another signer.
- Dependencies point one way: `paykit-sdk` and `paykit-signer` build on `paykit-lib`; `paykit-signer` may build on `paykit-sdk`'s Switch client and state store. `paykit-lib` depends on neither.
- In each crate, `src/lib.rs` is the public re-export facade and feature modules under `src/` hold the implementation. Name modules after protocol sections (`envelope`, `records`, `claims`, `chain`).
- Language bindings come later, in their own workstream. Keep the SDK and signer APIs easy to bind: plain owned types, no lifetimes in public structs, async only at the edges, and explicit structs and enums rather than loosely typed maps.

## Build, Test, and Development Commands
- `cargo fmt --all` : required before submitting changes.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` : warnings are errors; fix them, or allow one locally with a comment that justifies it.
- `cargo test --workspace --all-features` : unit, integration and doc tests; use `cargo test -p <crate> mod_name::case` for focused runs.
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` : public API docs must build without warnings.
- The end-to-end tests run a local Pubky testnet, which needs PostgreSQL.

## Coding Style & Naming Conventions
- Follow Rust 2021 defaults: four-space indentation, snake_case for functions and modules, UpperCamelCase for types and traits, SCREAMING_SNAKE_CASE for consts.
- Every public item has `///` docs; each crate sets `#![deny(missing_docs)]` and `#![forbid(unsafe_code)]`.
- Keep files ASCII. Copy Paykit vocabulary spellings from `THESAURUS.md`, and wire names (`identity.json`, `payment_request`, `receive_key.put`) exactly as the wire format spells them.
- Cite the rule a piece of code enforces by its spec or wire section, for example `(spec 3.7)` or `(wire 8.4.5)`.

## Backends
- Paykit v0.2 has two backends and no others: the **Pubky Homeserver** and the **Pubky Switch**. Do not add generic transport traits, feature-gated transport adapters or alternate-backend seams.
- The **Homeserver** holds long-term public data: `identity.json`, the records of apps with public Payment Endpoints, the Paykit Profile and optional encrypted app backups (spec 4.1). Homeserver calls take concrete Pubky SDK handles (`pubky::PubkySession` for authenticated writes, `pubky::PublicStorage` for unauthenticated reads). A missing file reads as `None`.
- The **Pubky Switch** holds an identity's inbox and feed, allowlist and deny list, claims, shared records, app records, key bundles, receive key records, push hooks and the chain (spec 6). Every request to it is signed (wire 8.1), and every response is checked against the service key the client has pinned; an answer that can't be checked is refused.
- Session creation, capability scope and key storage remain the caller's responsibility; document that on each API that takes a session or a key. Transport timeouts are configured by the caller on the HTTP or Pubky client. The protocol's own deadlines, such as claim deadlines and `server_time_in` conditions, come from the spec and use service time.

## Paths
- Paykit v0.2 publishes under `/pub/paykit/v1/`: `/pub/paykit/v1/identity.json` and `/pub/paykit/v1/apps/<app_id>.json`. Optional encrypted app backups live under `/priv/paykit/v1/apps/<app_id>/`. The Paykit Profile stays at `/pub/paykit/profile.json` (spec 4.1).
- The Switch API is versioned under `/v1/{id}/...` (wire 8.4).
- Build every path in one module from validated values; never hard-code paths at call sites. Every value interpolated into a path (a Pubky ID, an App ID) goes through its validated constructor first.

## Private Messages
- Private messages are **sealed envelopes** written to the recipient identity's Switch inbox (spec 5, wire 4): suite 1 is HPKE (X25519, HKDF-SHA256, ChaCha20-Poly1305) for the per-app key wraps and XChaCha20-Poly1305 for the content, with a key commitment. There are no Encrypted Links, no Noise handshakes and no `pubky-noise` in v0.2, and messages are never stored as Homeserver files.
- Envelopes are padded up to 1, 4, 16 or 64 KiB. **The maximum message size is 64 KiB**; the inbox answers `too_large` above it (wire 4.4).
- Sealing takes its randomness (content key, nonce, ephemeral keys) from the operating system's secure generator. No production interface accepts randomness from its caller; fixed randomness for the shared vectors exists only in test code.
- A receiving app runs the full message checks of wire 5.1, in order, before acting on a message. A message that fails a check is dropped as that check says, and is never partly applied. Who may send and receive each kind is fixed by spec 3.3 and 3.4 and wire 5.3.
- The record writes a received message causes and the cursor move past it go in one batch (wire 7.4).

## Error Handling
- **No panics on network input.** Data from the network, from decrypted plaintext or from the state store is never `unwrap`ped, `expect`ed or indexed unchecked; arithmetic on it is checked. Amounts are exact scaled integers (wire 1.7) and fail closed on overflow.
- **`paykit-lib`** has one error type for encodings, crypto and document checks. It separates malformed input or a broken rule (`Invalid`), a signature that doesn't verify (`Signature`) and a sealing, opening or key failure (`Crypto`). Its messages name the wire section of the rule that failed. Any exhaustive `match` covers every variant.
- **`paykit-sdk`** has its own error type. It separates:
  - a Switch error response, carrying its HTTP status, its spec 6.4 `code` as a typed enum covering every code in the table, its `message` and its `current` value;
  - a transport failure, and a response whose service signature doesn't verify or can't be checked;
  - Homeserver failures and state store failures;
  - local refusals: a detected rollback or fork, reconcile mode after a restore, and an action a local rule refuses;
  - a conflict the caller resolves by re-reading and recomputing.

  Whether a Switch error may be retried follows the Retry column of spec 6.4; expose that from the error rather than leaving each caller to decode it.
- Distinguish caller-supplied input that fails structural checks from network data that turns out to be malformed, so callers can tell a bug in their own input from a misbehaving counterparty or service.
- Error text never quotes decrypted plaintext, secret keys or the body of a document that failed to parse; name the member and the rule instead.

## Testing Guidelines
- Rely on the standard Rust test harness; embed minimal examples in doc comments so `cargo test` exercises them.
- Name tests `test_<feature>_<case>()`, for example `test_envelope_rejects_wrong_commitment`.
- **Every rule gets a positive and a failure-path test.**
- **Vectors:** `paykit-lib` has one test per file in `specs/v0.2/vectors/`, reading the file from that folder rather than copying it. Every deterministic value is rebuilt byte for byte, and every rejection case is refused for its stated rule. Responses that only a running service produces are checked here as signed bytes; replaying them against a service is the Switch's job.
- **Test-only code stays test-only:** fault injection, fixed sealing randomness and any signing helper that exists only to reproduce vectors go behind `cfg(test)` or a test-only feature that no production build enables.
- Tests that need a running Switch which CI can't start yet are `#[ignore]`d with a reason, so CI skips them; they are turned on once the Switch is available as a dev-dependency. The end-to-end tests use a local Pubky testnet.
- Aim for full coverage of serialization, parsing and signature paths: they handle network data.

## Comments, Docs and Public Material
These rules apply to everything that goes into this repository: code, comments, docs, test names and commit messages.
- **The reference prototype stays private.** It is shared separately. Port code from it with production error handling and docs; never commit, vendor, push or publish it, or any part of it, including its service or test harness.
- **No internal identifiers or process words.** Do not carry over review identifiers (short letter-and-number tags such as `F54` or `G107`), review or process words such as "PLAN", "phase", "finding" or "prototype only", or reviewers' names. Cite spec and wire sections instead.
- Keep earlier material out: no v0.1 security details and no references to older internal PRs.

## Commit & Pull Request Guidelines
- Use imperative, present-tense commit titles of at most 72 characters (e.g., `Port the sealed envelope to paykit-lib`).
- Each PR describes its motivation, lists its protocol impact, and cites the spec and wire sections it implements; include the `cargo fmt`, `cargo clippy` and `cargo test` results, or say what was skipped.
- Highlight changes to public structs, enums, error codes or scope strings, so the language bindings can follow when they are built.

## Security & Configuration Tips
- Never commit real keys or secrets; stub them with env vars or fixture files ignored by git. Test keys come from `specs/v0.2/vectors/`.
- Treat key handling, sealing and opening, message checks, claims and chain verification as sensitive: add comments describing assumptions about encryption, access control and durability to aid auditing.
- Secret keys never appear in `Debug` output, logs or error text, and are zeroized when dropped. Where they are stored is the host app's choice, behind an SDK trait, under the rules of spec 3.1.
- The signer's key interface signs only over the signer's own inputs, never caller-supplied bytes (wire 2.1), and exports the identity secret only for a hand-over or a backup.
- Payment rails are outside Paykit. The SDK uses a narrow rail interface (pay an endpoint with an idempotency key; read the verified payment time) and never interprets Payment Endpoint Payloads.
- Identifiers are validated at construction and keep their inner field private, with no escape hatch that bypasses validation. A Payment Endpoint Identifier is ASCII letters and digits, `-`, `_` and `.`, at most 64 characters, and never `.`, `..` or `private` (wire 1.9).
