# Repository Guidelines (Paykit v0.2)

## Branch, Specs and Vocabulary
- The `v0.2` branch holds only Paykit v0.2 code. `master` holds v0.1 and the Paykit v0.2 spec. Work on feature branches cut from `v0.2`, with PRs into `v0.2`.
- **Paykit v0.2 is one protocol on the Pubky Switch.** Two specs define it:
  - **The Pubky Switch spec**, in `pubky/pubky-switch` under `spec/`: identities and `identity.json`, app certificates and records, receive keys, Shared Data Keys and key bundles, envelopes and the signed message frame, admission, the feed and the chain, claims and shared records, the HTTP API, push, export and import, and identity management. It knows nothing of payments.
  - **The Paykit spec**, in `specs/v0.2/` (on `master`, merged into `v0.2`): what Paykit adds on the Switch. That is its scopes and the switch permissions each needs, its message kinds and bodies, its collections and their policies, the payment flows, Allowances, recurring requests and labelled selling scopes.
  - In each, the spec decides behaviour, the wire format decides bytes, and the vectors are normative: every implementation reproduces every file of its set byte for byte. Paykit's set is `specs/v0.2/vectors/`; the Switch's set is `spec/vectors/` in `pubky/pubky-switch`, and is tested there.
- **Paykit never redefines the Switch's formats.** Where Paykit relies on a Switch rule, cite it and use the Switch crates; don't restate or re-implement it here.
- **The spec wins.** Where the code and a spec disagree, the spec is right: fix the code, or stop and raise a spec change. Never diverge from either spec silently.
- **Spec changes come first.** A spec change goes in its own PR, before any code that depends on it: to `spec/` in `pubky/pubky-switch` for the Switch, or to `specs/v0.2/` on `master` for Paykit, which `v0.2` then takes by merging `master`. Vectors are regenerated only through such a change.
- `./THESAURUS.md` is the authority for Paykit domain language and says which terms are the Switch's. Use it before naming public APIs, docs sections, files, types, fields, endpoints, events or components.

## Project Structure & Module Organization
- The workspace root `Cargo.toml` pins resolver `2`, registers the members and holds the shared package metadata in `[workspace.package]`. `vendor/` holds the workspace's crate patch.
- **`paykit-lib`** holds Paykit's formats and rules, on `pubky-switch-core`. It is pure: no I/O, no clock, no global state, no async; time is always passed in. It covers:
  - Paykit's scopes, and the table of the switch permissions each needs;
  - checking a certificate's `permissions` against its Paykit scopes;
  - Paykit's labels and derivations;
  - its message kinds and their bodies;
  - its collections, their policies and their binding rules;
  - obligations, amounts and the payment rules.
- **`paykit-sdk`** holds Paykit's payment flows and their state, on `pubky-switch-client` and `pubky-switch-signer`. It covers:
  - requests, acceptance, rejection, cancellation, proofs and receipts;
  - attempts on claims, and recurring requests;
  - Allowances and the ledger;
  - Private Payment Lists, and labelled selling scopes;
  - receiving and re-sends;
  - the Paykit side of approval: the scopes an app asks for, and the permissions `paykit-lib` derives from them, which the signer signs.
- **The Switch crates** live in `pubky/pubky-switch`:
  - `pubky-switch-core`: the stateless Switch formats and checks;
  - `pubky-switch-client`: the app side of the Switch, with its `StateStore`;
  - `pubky-switch-signer`: identity management.

  Depend on them as git dependencies pinned to a commit (`rev`). Never vendor or copy them into this repository.
- **Dependencies point one way:** Paykit depends on the Switch, never the reverse. `paykit-sdk` builds on `paykit-lib`; `paykit-lib` depends on no Paykit crate. Something that a protocol other than payments would also need belongs in the Switch, through a Switch spec change, not here.
- In each crate, `src/lib.rs` is the public re-export facade and feature modules under `src/` hold the implementation. Name modules after protocol sections (`scopes`, `kinds`, `collections`, `allowances`).
- Language bindings come later, in their own workstream. Keep the SDK API easy to bind: plain owned types, no lifetimes in public structs, async only at the edges, and explicit structs and enums rather than loosely typed maps.

## Build, Test, and Development Commands
- `cargo fmt --all` : required before submitting changes.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` : warnings are errors; fix them, or allow one locally with a comment that justifies it.
- `cargo test --workspace --all-features` : unit, integration and doc tests; use `cargo test -p <crate> mod_name::case` for focused runs.
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` : public API docs must build without warnings.
- The end-to-end tests run a local Pubky testnet, which needs PostgreSQL.

## Coding Style & Naming Conventions
- Follow Rust 2021 defaults: four-space indentation, snake_case for functions and modules, UpperCamelCase for types and traits, SCREAMING_SNAKE_CASE for consts.
- Every public item has `///` docs; each crate sets `#![deny(missing_docs)]` and `#![forbid(unsafe_code)]`.
- Keep files ASCII. Copy Paykit vocabulary spellings from `THESAURUS.md`, and wire names (`paykit.payment_request`, `payer_activity`, `allowance_execute`) exactly as the wire formats spell them.
- Cite the rule a piece of code enforces by its section, naming the spec: `(Paykit spec 3.1)`, `(Paykit wire 4.5)`, `(Switch spec 3.5)` or `(Switch wire 7.4)`.

## Backends
- Paykit v0.2 has two backends and no others: the **Pubky Homeserver** and the **Pubky Switch**. Do not add generic transport traits, feature-gated transport adapters or alternate-backend seams.
- **The Pubky Switch** is reached only through `pubky-switch-client`: its request signing, response checks, operation batches, feed, `StateStore` and admission. Paykit adds no HTTP client of its own for the Switch.
- **The Homeserver** holds long-term public data. Homeserver calls take concrete Pubky SDK handles (`pubky::PubkySession` for authenticated writes, `pubky::PublicStorage` for unauthenticated reads), and a missing file reads as `None`. Payers read public Payment Endpoints from app records on the Homeserver (Paykit spec 4).
- Session creation, capability scope and key storage remain the caller's responsibility; document that on each API that takes a session or a key. Transport timeouts are configured by the caller on the HTTP or Pubky client. The protocol's own deadlines, such as claim deadlines and `server_time_in` conditions, come from the specs and use service time.

## Paths and Labels
- The Switch's paths are the Switch's (Switch spec 4.1): `identity.json`, app records and app backups live under `/pub/pubky-switch/v1/` and `/priv/pubky-switch/v1/`. Take them from the Switch crates; never build them here.
- Paykit's own Homeserver file is the Paykit Profile, at `/pub/paykit/profile.json` (Paykit spec 4). Build it in one place, never at call sites.
- Paykit's own signed formats and derivations use labels under `paykit/v1/`, such as `paykit/v1/executor-grant` and `paykit/v1/obligation` (Paykit spec 10, Paykit wire 1.5). Never sign or derive a Paykit format under a `pubky-switch/v1/` label (Switch spec 1).

## Messages and Shared State
- Sealing, opening, the message frame and the common message checks are the Switch's (Switch wire 4 and 5.1), and come from `pubky-switch-core`. A receiving app runs those checks, then Paykit's, before acting on a message; a message that fails a check is dropped as that check says, and is never partly applied.
- Paykit's kinds are `paykit.<name>`, plus extension kinds `<prefix>.<name>` under an `ext:<prefix>` scope, which carry no Paykit payment semantics (Paykit spec 3.3 and 5). Which scope sends and which receives each kind is the table of Paykit spec 3.3; a payer's replies are narrowed by label (Paykit spec 3.4). Bodies are Paykit wire 4.
- A kind stored in an activity record is at most 16 KiB (Paykit spec 5). Envelope sizes are the Switch's.
- Paykit's collections use the fixed policies of Paykit spec 6; apps ignore a record whose policy or label doesn't match. Destinations come only from the payee: no own app can redirect a payment by writing shared state (Paykit spec 6).
- The record write a received message causes and the cursor move past it go in one batch (Switch spec 8.2). Every payment follows the paying order of Paykit spec 7.2.

## Error Handling
- **No panics on network input.** Data from the network, from decrypted plaintext or from the state store is never `unwrap`ped, `expect`ed or indexed unchecked; arithmetic on it is checked. Amounts are exact scaled integers (Paykit wire 1.2) and fail closed on overflow.
- **`paykit-lib`** has one error type for Paykit's rules. Its messages name the Paykit section of the rule that failed. A failure from `pubky-switch-core` is carried as the Switch's error, not re-described. Any exhaustive `match` covers every variant.
- **`paykit-sdk`** has its own error type. It separates:
  - Switch failures from `pubky-switch-client`, carried as that crate reports them, error codes and retry rules included;
  - Homeserver failures, state store failures and rail failures;
  - local refusals under Paykit's rules, such as a destination from an app that isn't current, an expired list or an Allowance limit;
  - a conflict the caller resolves by re-reading and recomputing.
- Distinguish caller-supplied input that fails structural checks from network data that turns out to be malformed, so callers can tell a bug in their own input from a misbehaving counterparty or service.
- Error text never quotes decrypted plaintext, secret keys or the body of a document that failed to parse; name the member and the rule instead.

## Testing Guidelines
- Rely on the standard Rust test harness; embed minimal examples in doc comments so `cargo test` exercises them.
- Name tests `test_<feature>_<case>()`, for example `test_scopes_reject_request_with_label`.
- **Every rule gets a positive and a failure-path test.**
- **Vectors:** `paykit-lib` has one test per file of Paykit's set in `specs/v0.2/vectors/`, reading the file from that folder rather than copying it. Every deterministic value is rebuilt byte for byte, and every rejection case is refused for its stated rule. The Switch's set is tested in `pubky/pubky-switch`.
- **Test-only code stays test-only:** fault injection, the fake rail and any fixed randomness the vectors need go behind `cfg(test)` or a test-only feature that no production build enables.
- **End-to-end tests** run the Paykit flows against the Switch service and a local Pubky testnet. Tests that need a Switch service CI can't start yet are `#[ignore]`d with a reason, so CI skips them, and are turned on once the service is available as a dependency. No service code is committed here for them.
- Aim for full coverage of serialization, parsing and signature paths: they handle network data.

## Comments, Docs and Public Material
These rules apply to everything that goes into this repository: code, comments, docs, test names and commit messages.
- **The reference prototype stays private.** It is shared separately. Port code from it with production error handling and docs; never commit, vendor, push or publish it, or any part of it, including its service or test harness.
- **No internal identifiers or process words.** Do not carry over review or work identifiers (short letter-and-number tags such as `F54` or `G107`), process words such as "PLAN", "phase", "finding" or "prototype only", or reviewers' names. Cite spec and wire sections instead.
- Keep earlier material out: no v0.1 security details and no references to older internal PRs.

## Commit & Pull Request Guidelines
- Use imperative, present-tense commit titles of at most 72 characters (e.g., `Add the Paykit scope and permission table`).
- Each PR describes its motivation, lists its protocol impact, and cites the Paykit and Switch sections it implements; include the `cargo fmt`, `cargo clippy` and `cargo test` results, or say what was skipped.
- Highlight changes to public structs, enums, error codes or scope strings, so the language bindings can follow when they are built.
- A PR that moves the pinned `pubky-switch` commit says why, and what changed in the Switch crates it uses.

## Security & Configuration Tips
- Never commit real keys or secrets; stub them with env vars or fixture files ignored by git. Test keys come from the vector sets.
- Treat certificate and scope checks, message checks, claims, Allowance evaluation and destination checks as sensitive: add comments describing assumptions about encryption, access control and durability to aid auditing.
- Secret keys never appear in `Debug` output, logs or error text, and are zeroized when dropped. Key storage is the Switch's rule (Switch spec 3.1), behind the host app's `StateStore`.
- Identity management is the Switch's, in `pubky-switch-signer`. Paykit gives the signer only its side of approval: the scopes an app asks for and the permissions they need (Paykit spec 3.2). Paykit's verifiers derive that list again and refuse a certificate whose `permissions` differ.
- Payment rails are outside Paykit. The SDK uses a narrow rail interface (pay an endpoint with an idempotency key; read the verified payment time) and never interprets Payment Endpoint Payloads.
- Identifiers are validated at construction and keep their inner field private, with no escape hatch that bypasses validation. A Payment Endpoint Identifier is ASCII letters and digits, `-`, `_` and `.`, at most 64 characters, and never `.`, `..` or `private` (Paykit wire 1.4).
