# Allowances phased implementation plan

| Field | Value |
| --- | --- |
| Status | Proposed eight-PR implementation sequence |
| Last updated | 2026-08-24 |
| Product model | [`allowances.md`](allowances.md) |
| Vocabulary | [`THESAURUS.md`](../THESAURUS.md) |

## Purpose

This document turns the current Allowances product model into eight independently reviewable pull requests. Eight is the smallest defensible baseline for the current architecture: five to seven PRs would require combining persistence compatibility, protocol decisions, SDK authority checks, and generated platform bindings in ways that would obscure important review boundaries.

Each phase is intended to be implemented in its own session and merged as one PR. A phase may use several ordered commits to help reviewers separate mechanical work from behavior, but it must still have one primary reason to change. The plan deliberately does not freeze final Rust names, JSON field names, Private Message Kind strings, or public signatures; PR 3 makes those decisions before Allowance code is introduced.

The current direction in decisions 38 through 50 of [`allowances.md`](allowances.md) is authoritative. Decisions 1 through 37 are historical context where later decisions supersede them.

This eight-PR plan assumes the narrow V1 baseline described below. Optional protocol extensions are deferred, not hidden inside the eight phases.

## Current architecture findings

Allowances fit the existing Private Application Message architecture. The important pressure points are:

- [`PrivateMessageKind`](../paykit-lib/src/encrypted_link/private_application_message.rs) is intentionally exhaustive. A new kind currently forces explicit review of routing, intake, outbound validation, and backup validation. Two gaps weaken that guarantee: `PrivateMessageKind::parse` uses a wildcard arm, so a new variant compiles without a parse arm and silently classifies as unknown at runtime, and several SDK sites compare kind strings directly (including hard-coded literals in Payment Request derivation) instead of matching the enum.
- Generic private-body inspection is repeated across [stream intake](../paykit-sdk/src/domain/private_stream/mod.rs), [outbound validation](../paykit-sdk/src/domain/outbound_private/mod.rs), and [backup validation](../paykit-sdk/src/backup/validation.rs): three hand-rolled header parses and three per-kind dispatch matches, with backup validation also partially delegating to the intake classifier.
- No classification normalization exists today. Classification is computed once at intake and frozen; direct reads project from the stored classification, so a stale status permanently hides an item from newer code, and `initialize()` performs identity work only. Backup restore compares stored classification — including raw `parse_error` strings that currently embed serde detail — byte-for-byte against a fresh classification and rejects the whole restore on any drift, so today any classifier or serde-version change bricks existing backups.
- An unknown-kind outbound queue head is destroyed today, not parked: the flush path marks it `Invalid` (leaking the kind string into `last_error`), cancels associated Payment Endpoint Reservations, and lets later peer messages advance the Encrypted Link past it.
- [Payment Request derivation](../paykit-sdk/src/domain/payment_requests/derivation.rs) demonstrates that lifecycle views can be projected from the inbound raw stream plus outbound queue rather than materialized as a separate feature table. It is a template with known deviations from this plan's rules: the reducer takes a wall-clock `now` parameter, orders cross-stream events by local timestamps plus a hard-coded phase rank, applies the recovery overlay by overwriting the derived state in place, and reads across multiple storage transactions. PR 3 must define replacement ordering rules and PR 5 must not copy those behaviors.
- [`StorageState`](../paykit-sdk/src/storage/records.rs) already persists raw inbound messages, outbound intent, Event ID dedupe, and Encrypted Link recovery state. Records bind the remote pair (counterparty, counterparty receiver path); the local receiver path is runtime configuration and is not persisted in any record.
- [FFI storage](../paykit-ffi/src/storage.rs) uses strict Postcard envelopes with equality-checked versions in both directions and no migration path. The platform blob store already provides optimistic concurrency via an opaque revision, but every transaction — including read-only ones — takes a process-wide mutex, invokes the foreign load callback, decodes and deep-clones the full `StorageState`, and structurally compares it. Adding logical Allowance tables would require explicit state and backup migrations, while new string-valued message kinds do not by themselves change `StorageState`.
- [`PaymentEndpointIdentifier`](../paykit-lib/src/payment_endpoint.rs) is a path-safe identifier and `PaymentEndpointPayload` is opaque UTF-8. Neither supplies owner, revision, expiry, replacement, withdrawal history, or freshness, and the Pubky read path exposes no freshness primitive. The exact single-endpoint read (`get_payment_endpoint`) exists in `paykit-lib` but currently has no SDK or FFI consumer.
- The existing [SDK Payment Adapter](../paykit-sdk/src/domain/adapters/mod.rs) and [FFI adapter](../paykit-ffi/src/payment_adapter.rs) select and build payment targets. They intentionally do not run wallet policy or payment execution. The SDK trait is async; the FFI foreign callbacks are synchronous and block a runtime worker thread.

The recommended baseline architecture is:

1. Carry Allowance lifecycle events and Payment Instructions over the existing Encrypted Link as Private Event Messages.
2. Keep raw inbound messages and outbound intent as the SDK sources of truth.
3. Derive Allowance and Payment Instruction views without new feature tables.
4. Keep wallet usage, replay, reservation, capacity, execution, and settlement state in the wallet's own atomic store.
5. Resolve a destination through an exact Payee-scoped public Payment Endpoint observation and return evidence to the wallet; do not turn that observation into SDK approval.
6. Validate raw-history projection cost inside PR 6 before exposing the final platform API.

The no-new-feature-table conclusion is a design recommendation, not permission to ignore evidence. If the scale gate in PR 6 fails, stop and re-plan a versioned index/storage migration rather than hiding an unversioned cache inside the feature.

## Responsibility boundaries

| Layer | Allowance responsibility | Explicitly outside the layer |
| --- | --- | --- |
| Paykit Library | Domain types, private wire DTOs, parsing, serialization, structural validation, exact stateless correlation, and concrete Pubky helpers | Lifecycle persistence, trusted time, rule evaluation, usage, capacity, and payment execution |
| Paykit SDK | Durable intake/outbound intent, Event ID dedupe, authenticated sender and observed-lifecycle checks, deterministic projections, recovery, and app-facing coordination | Deciding that shared/private wallet rules pass, reserving capacity, or executing payment |
| Wallet/application | Shared-rule evaluation, stricter private safeguards, trusted time, semantic Instruction replay state, usage/capacity/concurrency, destination decision, signing, payment, and settlement monitoring | Treating the Allowee, Payee, or SDK as authoritative for wallet policy |
| FFI/platform bindings | Typed access to the SDK lifecycle, exact destination observation, and Instruction workflow | A second policy engine or synchronous wallet-execution callback |

## V1 baseline and deferred scope

The eight-PR limit is achievable only with an explicit baseline:

- An Allowance is immutable after proposal. Changed accepted terms require a new proposal with a new Allowance ID and ending the old Allowance; there is no edit or counteroffer path, and Paykit does not promise cross-message atomicity for replacement.
- Either linked party may propose, but both parties must accept the exact terms before authority exists. PR 3 freezes whether an authenticated proposal counts as the proposer's acceptance; no path may grant authority without an explicit authenticated action by the Allower.
- Either linked party may end an accepted Allowance, subject to the lifecycle rules frozen in PR 3.
- A Payment Instruction carries one exact amount, asset, Payee reference, destination reference, and semantic replay identity.
- The destination model uses an exact public Payment Endpoint reference under the named Payee's authenticated Pubky scope. Observation can report only `Match`, `Missing`, `Mismatch`, or `Unverifiable`. In this track, removing or replacing the published value under the Payee's authenticated Pubky scope is the V1 withdrawal/replacement mechanism required by decision 50; PR 3 must confirm that mapping satisfies decision 50 or trigger the descriptor-track stop condition.
- The SDK provides locally observed protocol state and an as-of marker. It does not claim that no newer remote event exists.
- The wallet independently evaluates terms, time, usage, replay, balance, capacity, fees, and private safeguards, then decides whether and how to pay.
- The wallet approves or declines the Instruction for its exact Payment Amount; it must not substitute a smaller, larger, or different-asset amount automatically.
- V1 does not add an Allowance-specific result/proof, Receipt correlation, receiver capability bit, recovery authorization epoch, SDK claim/disposition table, or automatic wallet callback.

The following are deferred extensions, not conditional work silently included in a baseline phase:

- authenticated or private destination descriptors, revision history, or durable withdrawal;
- Allowance-specific results, proofs, or settlement assertions;
- Receipt correlation when Allowee and Payee differ;
- explicit receiver capability advertisement;
- mandatory re-consent after a validated Encrypted Link recovery;
- a materialized Instruction index;
- SDK processing claims or dispositions;
- automatic wallet orchestration.

If PR 3 concludes that any deferred extension is required for V1, or PR 6 proves that a materialized index is required, this eight-PR plan is no longer accurate. Update the plan and obtain explicit scope approval before implementation continues.

## Non-negotiable implementation boundaries

- An Allowance ID alone is never authority. Every stored or derived record retains the authenticated counterparty and receiver context. PR 3 decides whether the receiver path is protocol authority or only the current storage/link namespace. Stored records currently bind only the counterparty pair — the local receiver path is runtime configuration — so binding the local side into the authority key requires an explicit persistence decision that cannot be smuggled into PR 1, which forbids `StorageState` changes.
- All shared Allowance messages use the existing Encrypted Link. Do not add typed receive getters to `paykit-lib`; the durable mixed Private Application Message stream remains the receive API.
- Do not add a generic transport trait, alternate backend seam, or hard-coded Pubky path. Any new Pubky path or storage operation belongs in `pubky_routing.rs`.
- Keep `paykit-lib` stateless. It may validate construction invariants and causal or identifier correlation, but it must not decide whether a live Instruction qualifies under Allowance rules.
- Do not reuse `Recurrence` or `BillingPeriod` for Allowance periods; those belong to Payment Requests and Subscriptions.
- Do not reuse `PaymentRequestId`, `PaymentReference`, or `EventId` as the Instruction ID.
- Do not reuse the existing `PaymentProof` without a separate protocol decision; its current shape is tied to a Payment Request.
- Do not infer an asset by splitting a Payment Endpoint Identifier. Identifier syntax is not an asset registry.
- Do not extend `PaymentAdapter` with Allowance evaluation or execution.
- Do not add SDK `executed`, `claimed`, `remaining`, usage-window, reservation, or capacity records for the baseline feature.
- Treat `RecoveryRequired` as non-actionable for lifecycle commands and Instructions.
- Internal `Debug`, logs, persisted parse summaries, and FFI error context must redact private plaintext, arbitrary metadata, destination payloads, proofs, and serde detail. Typed records intentionally returned to an app remain sensitive app data.
- No SDK operation may claim atomicity with wallet usage state, signing, broadcast, or settlement.

## PR map

| PR | Standalone review unit | Depends on | Review outcome |
| --- | --- | --- | --- |
| 1 | Make private-message evolution safe | None | Classifier changes and new kinds cannot corrupt, leak, or strand persisted state |
| 2 | Prepare the outbound command path | None | Typed sends are shared and checked enqueue is atomic |
| 3 | Freeze the V1 protocol contract | Product model | Roles, terms, lifecycle, destination, replay, and wire budgets are implementable |
| 4 | Implement the Library Allowance lifecycle protocol | 1-3 | Stateless types and lifecycle messages are complete |
| 5 | Implement the Rust SDK Allowance lifecycle | 2, 4 | Durable reads and lifecycle commands are deterministic and atomic |
| 6 | Implement the Rust destination and Instruction workflow | 1-3, 5 | Wallet handoff, semantic replay, destination observation, scale, and submission work in Rust |
| 7 | Add unified Swift and Kotlin bindings | 1, 5, 6 | The complete selected V1 API crosses the platform boundary once |
| 8 | Finish documentation and release readiness | 1-7 | Compatibility, security, integration guidance, and final verification are complete |

PRs 1 and 2 may proceed in parallel — provided PR 2 adds no `StorageAdapter`/`StorageTransaction` methods (PR 1 owns that contract change) and the two coordinate on the shared outbound-validation files they both touch. PR 3 may be drafted in parallel with them, but it must merge before PR 4. After PR 4, the remaining phases are sequential because each freezes an API consumed by the next layer.

PR 6 is the largest phase. It remains one PR only because its subparts form one security outcome: produce an exact, locally observed Instruction handoff without authorizing payment. Its implementation order and stop conditions are explicit below.

## Code-PR verification and artifact policy

PRs 1, 2, 4, 5, 6, and 7 all change Rust code consumed by the platform bindings. Each must run the repository Rust checks plus `cd paykit-ffi && ./build.sh all` (macOS-only; `all` is the only accepted target); never build only one platform.

For every non-release code PR:

- inventory generated Swift/Kotlin/header and the four tracked Android JNI library (Git LFS) changes after the build;
- commit only the generated and native changes intentionally produced by that PR, or state explicitly that there were none;
- revert the iOS build's `Package.swift` checksum rewrite and assert that its released tag, URL, and checksum have no diff. The build always recomputes the checksum from the locally built XCFramework (it never rewrites the URL, and rewrites the tag only in release mode), and the repository has historically committed those local checksums on feature commits, leaving `Package.swift` pairing the previous release's URL with a non-matching checksum. Reverting the rewrite is this plan's explicit rule, and it is a change of current practice;
- do not substitute a locally built XCFramework checksum for the released artifact;
- know that CI's stale-bindings gate covers only the six generated source files — `Package.swift` and the four `.so` files are not gated — so this policy is enforced by review, not CI;
- record the exact verification commands and results in the PR description.

Actual artifact publication changes the version, tag/URL/checksum, and matching iOS/Android artifacts together in the repository's release workflow. It is not an incidental side effect of a feature PR.

## PR 1: make private-message evolution safe

### Goal

Establish one compatibility and privacy foundation before introducing Allowance message kinds or sensitive payloads.

### Why this is one PR

Classification normalization, redacted diagnostics, shared inspection, and downgrade behavior all govern the same invariant: persisted raw private messages must remain the authority while derived classifications can evolve safely. Reviewing them together lets humans evaluate the complete upgrade and rollback story before any new feature uses it.

### Scope

- Declare raw JSON plus immutable per-item source context (counterparty, receiver path, stream and batch IDs, and receive time) the durable source of truth.
- Treat the current Encrypted Link state and checkpoint as a mutable peer-level recovery overlay. Do not claim historical per-message checkpoint provenance that `PrivateStreamItemRecord` does not store.
- Treat parsed headers, recognized-kind text, parse status, redacted parse summary, and Event ID dedupe membership as rebuildable derived data.
- Add narrowly scoped `StorageTransaction` operations to update an existing stream item's derived classification and reconcile affected dedupe indexes. Do not expose full-state replacement as a general mutation escape hatch.
- Reconcile Receipt Access indexing when first, duplicate, or conflicting Event ID membership changes. Preserve local retrieval state only when the same source event remains authoritative; fail closed otherwise. This is the hardest item in the PR: the Receipt Access index is written only for the first Event ID observation, and its retrieval state (status, timestamps, last error) is local and cannot be rebuilt from raw data.
- Introduce classification normalization for the first time — none exists today, and `initialize()` performs identity work only. Make `initialize()` run normalization, and make direct lifecycle, private-stream, outbound, and backup operations call an internal idempotent guard, so no classification-dependent public workflow projects from stale derived data.
- During restore, validate immutable/contextual fields, normalize derived caches from raw data, and then validate the normalized state. Normalization must run before outbound payload re-validation as well as before stream-item comparison, because outbound validation executes first in the current restore order.
- Replace private-message parse errors that can contain serde detail or offending values with stable redacted categories such as invalid JSON, unsupported version, wrong kind, and invalid structure. Treat this as a compatibility prerequisite, not a nicety: restore currently compares stored `parse_error` strings byte-for-byte against fresh classifier output, so serde-derived error text makes every existing backup unrestorable after any serde version bump.
- Close the classification escape hatches: make `PrivateMessageKind::parse` exhaustive by construction (its wildcard arm currently lets a new variant compile while classifying as unknown at runtime), and migrate string-literal kind comparisons — including Payment Request derivation's hard-coded strings — onto the enum.
- Add one `paykit-lib` inspection entry point that reports recognized kind, Latest-State versus Event semantics, structural validity, recoverable Event ID, and stable redacted error category. It should accept raw JSON text rather than a pre-built message struct so the SDK's synthetic message constructions can be retired; it must preserve the pinned rule that the body's kind is authoritative over the envelope header; it must keep special-casing the invalid-UTF-8 sentinel marker (persisted raw JSON is not always the literal wire payload); and it must state whether Receipt Access receiver-scope enforcement is part of inspection or a separate policy pass — today intake and backup apply it and outbound validation does not.
- Refactor SDK intake, outbound validation, and backup validation to use the common inspection result while retaining explicit exhaustive branches at those security boundaries.
- Introduce the next compatibility generation for SDK backups and FFI state/backup envelopes without adding Allowance fields to `StorageState`. Move the three version constants in lockstep: `SDK_STATE_BLOB_VERSION` and `SDK_BACKUP_BLOB_VERSION` in `paykit-ffi` plus the publicly re-exported `SDK_BACKUP_VERSION` in `paykit-sdk`.
- Make new code read the previous generation and write the safeguarded generation. Ensure a pre-safeguard binary rejects the new generation before it can mutate unknown outbound intent. Postcard is non-self-describing and both directions currently reject on strict version inequality, so reading the previous generation requires a genuinely new decode path, not `#[serde(default)]`.
- Explicitly version the public `StorageAdapter`/`StorageTransaction` contract change introduced by normalization. Document the source-breaking adapter upgrade and provide a conformance fixture for third-party implementations.
- For custom durable adapters outside the built-in envelopes, require an app-owned persisted generation fence that prevents a pre-safeguard binary from opening newer state. State plainly that rollback is unsupported when an adapter cannot provide that fence. The FFI blob-store contract exchanges opaque bytes plus an opaque revision, so a platform store cannot refuse a newer generation without decoding; either add the minimal FFI surface for the fence (a binding compatibility change to inventory explicitly) or document that the fence is Rust-adapter-only.
- Make a safeguarded reader leave a well-formed but unknown pending, retryable, or stale/in-progress queue head byte-for-byte and status-for-status unchanged. The unknown head blocks later FIFO messages for that peer. This is a behavior change: today the flush path marks an unknown head `Invalid`, cancels its Payment Endpoint Reservations, and continues past it.
- Return a stable redacted SDK and platform-visible unsupported-kind block when an unknown queue head is parked; do not expose the raw body or silently report successful processing.
- Keep malformed messages for recognized kinds on their existing invalid-message path.

### Review order inside the PR

1. Establish normalization and frozen old-state fixtures against the current classifier.
2. Add redacted categories and prove old malformed records normalize safely.
3. Introduce shared inspection and prove all existing decisions remain equivalent.
4. Add envelope compatibility and unknown-queue parking fixtures.

These should be separate commits where practical, but they remain one PR because none is independently useful for Allowances without the complete evolution guarantee.

### Verification

- Normalize stale header, status, summary, and Event ID index fixtures without changing raw JSON or immutable source context.
- Rebuild first, duplicate, and conflicting Event ID membership deterministically in stream order.
- Cover direct reads without prior `initialize()`, repeated initialization, FFI first-transaction loading, backup restore, and rollback on storage failure.
- Use recognizable sentinel secrets and assert they do not appear in Rust `Display` or `Debug`, tracing output, persisted summaries, SDK errors, or FFI error mappings. The concrete leak channels today are persisted `parse_error` values, outbound `last_error` values, and the FFI `export_debug_details()` accessor, which is intentionally unredacted — so redaction must happen at error construction, and tests must cover `export_debug_details()` separately from `redacted_context()`. `paykit-sdk` currently emits no tracing, so tracing assertions alone prove nothing.
- Matrix-test every existing Private Message kind through inspection, intake, outbound validation, and backup validation.
- Freeze previous-generation state, snapshot, backup, and queue fixtures, including the last safe reader and an intermediate reader that knows some but not all future kinds.
- Exercise the custom-adapter conformance fixture, generation rejection, and documented unsupported rollback path.
- Prove unknown queue heads are parked, later peer messages do not leapfrog them, and recognized malformed messages are still rejected.
- Prove the parked-head signal is stable and redacted in both Rust and the generated platform boundary.
- Prove a state produced by the current classifier normalizes without logical changes.
- Run `cd paykit-ffi && ./build.sh all` because FFI state and backup envelopes are touched, then apply the non-release artifact policy above.

### Not in this PR

- Allowance types, kinds, or public API.
- A generic message plugin registry.
- A new feature table or transport abstraction.
- Changes to current valid message bytes or lifecycle behavior.

### Exit criteria

A later PR can add a Private Message kind, change a derived parse category, restore an older backup, or encounter a newer queued kind without leaking plaintext or corrupting durable intent when using the built-in envelopes or a conforming generation-fenced custom adapter.

## PR 2: prepare the outbound command path

### Goal

Create one behavior-preserving path for typed private sends and atomic lifecycle command enqueueing.

### Why this is one PR

The Library send helper and SDK checked-enqueue seam are the two halves of outbound intent. Combining them provides a complete, reusable path while keeping the PR free of Allowance semantics. Migrating existing Payment Request transitions proves the seam before Allowances depend on it.

### Scope

- Make the existing generic internal JSON sender available to typed protocol modules with a static, redacted operation label. That sender already exists as a private labelled method with seven per-kind wrappers, so this is visibility and call-site plumbing, not new machinery. Route typed sends through the internal labelled method, not the public raw-JSON sender — the public path re-parses the payload, collapses the per-kind operation labels, and has an error path that embeds raw serde detail. Preserve the Receipt Access send's extra preconditions (access validation and receiver-location check).
- Refactor Payment Request, Receipt Access, and Private Payment List typed sends to use it without changing public raw-send behavior or serialized bytes. The SDK's production sends flush raw JSON from the durable queue and never call the typed helpers, so this consolidation is a `paykit-lib` public-surface change with no SDK behavior impact.
- Add a transaction-scoped SDK primitive plus a concrete snapshot-input derivation seam so feature code can load raw/outbound history, derive current state, and append one exact outbound record atomically. Follow the existing in-repo template — the Private Payment List reservation enqueue already validates outside the transaction, then re-checks the lease, reads dependent records, and appends inside one transaction.
- Do not add methods to `StorageAdapter`/`StorageTransaction`; every operation the seam needs already exists on the trait. That constraint is what keeps PR 2 parallel-safe with PR 1's versioned adapter-contract change.
- Construct and serialize the candidate protocol event before entering the storage transaction. Inside the transaction, revalidate the exact body, lifecycle precondition, link readiness, monotonic outbound ID allocation, and FIFO append.
- Re-evaluate lifecycle preconditions and link/outbound readiness inside the transaction. Only storage-backed checks can move inside the synchronous transaction closure; session and identity checks are async (a foreign callback on FFI builds) and stay outside. "Link readiness" is currently two different policies — Payment Requests require a linked peer with a snapshot, while Private Payment Lists also accept a restorable handshake — so the seam takes the readiness rule as an input per message family.
- Do not perform Pubky, network, or foreign callback work while holding the transaction.
- Migrate Payment Request propose, accept, reject, cancel, and proof transitions to the seam as the production proof. Propose shares the identical check-then-act race and must not be left on the old pattern.
- Keep transport delivery processing separate from intent creation.
- Preserve current return values, error mapping, payload bytes, queue ordering, retry semantics, and recovery behavior. Where moving checks inside the transaction necessarily changes error precedence (existing tests pin, for example, which of proposal expiry and outbound readiness wins), change the pinned tests deliberately and list every precedence change in the PR description.

### Review order inside the PR

1. Mechanical typed-send consolidation with byte-equivalence tests.
2. Storage transaction API and in-memory implementation.
3. FFI storage conflict classification. The blob store's compare-and-swap (`expected_revision`) already exists; what is missing is that a stale revision surfaces as an opaque storage error with no typed classification or retry decision.
4. Payment Request command migration and concurrency tests.

### Verification

- Assert byte-for-byte JSON input to pubky-noise is unchanged for existing message families.
- Cover fixed-size rejection, retryable and non-retryable send mapping, and redacted operation context.
- Prove concurrent valid transitions can append at most one legal event. Race tests must interleave at the call level (two futures racing the same command): both storage adapters serialize transactions on one mutex, so transaction-level tests cannot expose the check-then-act race, and no such concurrency test exists today. Note the enqueue path does not participate in Event ID dedupe, so uniqueness comes from the in-transaction lifecycle re-check, not the dedupe table.
- Prove a failed precondition, storage error, or revision conflict appends nothing.
- Exercise in-memory and FFI-backed storage, process restart, and recovery-required peers.
- Prove no network call or platform callback occurs inside a transaction, excepting the FFI blob-store load/save pair, which is the transaction mechanism itself. SDK-side the guarantee is already true by construction because the transaction closure is synchronous; the test guards against regression to an async closure design.
- Run `cd paykit-ffi && ./build.sh all` because the FFI error/status mapping for storage conflicts changes, then apply the non-release artifact policy above.

### Not in this PR

- Allowance logic or public Allowance methods.
- A generic lifecycle engine or transport trait.
- Changes to Payment Request wire formats.

### Exit criteria

Allowance modules can serialize through one internal send primitive, and SDK commands can make state-dependent outbound intent atomic without inventing a second transaction pattern.

## PR 3: freeze the V1 protocol contract

### Goal

Resolve the product and wire decisions needed by every later implementation PR.

### Why this is one PR

This is a specification-only review. Keeping it separate prevents Rust types from prematurely encoding unresolved authority, destination, replay, or accounting assumptions.

### Scope

- Update `THESAURUS.md` first for any new public domain terms. Allowance, Allower, Allowee, and Allowance ID already exist there (marked future/planned); Payment Instruction, Instruction ID, destination reference, the observation result vocabulary, semantic replay key, as-of marker, and proposer do not and must be added. Reconcile the existing THESAURUS note that an asset "appears as the first segment" of a Payment Endpoint Identifier with this plan's rule against inferring an asset from identifier syntax.
- Freeze the roles of Allower, Allowee, Payee, proposer, receiver, and local wallet for both proposal directions.
- Define exactly what is bound into an Allowance's authority key: Allowance ID, authenticated counterparty pair, local/remote roles, receiver path or namespace, and any protocol version or epoch. The local receiver path is runtime configuration today and is not persisted in any record; if it is part of the authority key, specify where it gets bound and persisted.
- Define immutable terms and how changed accepted terms require a new proposal/new Allowance ID plus an end of the old Allowance. Freeze replacement/end ordering and whether events carry an explicit replacement link, without claiming cross-message atomicity.
- Define proposal, acceptance by both parties, rejection, unilateral end, duplicate, conflict, and terminal ordering semantics, including whether proposing counts as proposer consent and how same-batch or in-flight races resolve.
- Define causal references and deterministic conflict rules for the two directional streams, which have no shared global order. Do not use local receive timestamps as consent or cross-peer ordering authority. The existing Payment Request derivation orders cross-stream events by local wall-clock plus a hard-coded phase rank; the rules frozen here must replace that approach, not inherit it.
- Decide whether accepted authority survives validated Encrypted Link recovery. The baseline recommendation is to retain the derived lifecycle but expose `RecoveryRequired` until relink is validated; requiring fresh consent is deferred.
- Define exact decimal amount syntax, canonicalization, comparison, overflow bounds, and asset identification. The workspace has no decimal library and no amount comparison semantics today — `PaymentAmount` is derived string equality with unvalidated production construction paths, and accepts forms like `.5` and `10.` — so if the exact-decimal value adds a dependency it must satisfy the CI-pinned MSRV; otherwise specify the hand-rolled invariants.
- Define the all-rules-must-pass V1 model: inclusive per-Instruction amount range; period amount and count limits; lifetime amount limit; activation and expiry; allowed Payees; and allowed Payment Endpoint Identifiers.
- Define canonical rule ordering, duplicate handling, maximum cardinalities, and whether an empty or unbounded rule set is legal. V1 excludes OR groups, ordered allow/deny rules, FX, and cross-asset evaluation.
- Define one exact Allowance asset, exact-match semantics, anchored UTC calendar periods, fixed rolling windows, end-of-month behavior, and the accounting rule for failed or unknown payment outcomes.
- Clarify that activation and expiry are wallet-evaluated eligibility terms, not SDK clock-driven lifecycle events. If proposal expiry exists, specify and name it separately.
- State that fees do not consume Allowance capacity and refunds do not automatically restore it unless the contract explicitly changes that rule.
- State that the wallet approves or declines the exact instructed Payment Amount and must not automatically substitute another amount or asset.
- Freeze the Payment Instruction ID, Event ID, causal references, semantic replay key, and canonical fingerprint fields.
- Freeze Payee identity and the destination reference. The baseline should include the exact Payee Pubky identity/scope, receiver path, validated Payment Endpoint Identifier, and a domain-separated digest of the expected payload.
- Define destination observation timing, cache/freshness guarantees, expiry, replacement, withdrawal, and the wallet's last safe recheck point. A missing or changed public value is fail-closed evidence for one observation, not proof of historical withdrawal or global currentness.
- Define the exact observation result vocabulary: `Match`, `Missing`, `Mismatch`, and `Unverifiable`. Observation is evidence, not payment authorization. Specify the mapping for degenerate reads against actual Pubky helper behavior: an empty published file currently reads as absent, only 404/GONE mean absence, and every other failure (including authorization errors) must map to `Unverifiable`, never `Missing`.
- Decide whether V1 has any result, proof, Receipt, or processing acknowledgement. The baseline recommendation is no.
- Allocate worst-case private JSON budgets for proposal, lifecycle controls, and Instruction messages below `PUBKY_NOISE_MSG_LEN`, including escaping and envelope overhead.
- Freeze versioning, unknown-field rejection, unknown-kind behavior, downgrade guarantees, and compatibility fixtures.
- Document which decisions are enforced structurally by the Library, derived by the SDK, and evaluated privately by the wallet.
- Freeze the app-visible error contract in terms of what SDK and FFI callers actually observe: the SDK remaps Library `InvalidData` to its `Protocol` variant and drops the source, so a contract promising `InvalidData` at the app boundary would be wrong.
- Freeze the platform exposure boundary: which values may be generated records, which sensitive or likely-to-evolve aggregates must be opaque getter-based objects, and which fields may appear in generated Swift/Kotlin constructors and stringification. Treat adding a record field later as a source-compatibility change.
- Explicitly list every deferred extension that is not part of V1.

### Recommended baseline decisions

- Use exact decimal strings backed by an invariant-preserving Rust value, not floating point.
- Keep terms immutable and require a new Allowance ID for changes.
- Require the old accepted Allowance to be ended as part of the documented replacement workflow; do not add an edit or counteroffer operation.
- Require affirmative consent from both parties and an explicit authenticated Allower action; decide whether the proposal itself counts as the proposer's consent.
- Use the public-reference destination track for V1 and make the wallet compare one exact Payee-scoped observation.
- Use that track only if the Pubky read/cache guarantees meet the required observation semantics. If V1 requires authenticated history, tombstones, private destinations, or stronger freshness, stop and replace the baseline with the descriptor track.
- Treat the semantic Instruction replay key as distinct from Event ID dedupe.
- Keep payment execution and settlement outside Paykit.

### Verification

- Provide truth tables for proposal orientation, acceptance authority, rejection/end ordering, duplicate/conflict handling, recovery, and semantic replay.
- Include maximum-size worked examples for every private message.
- Walk at least one Allower-as-proposer, Allowee-as-proposer, Allowee-as-Payee, and third-party-Payee scenario.
- Validate the public Payment Endpoint observation assumptions against the concrete Pubky APIs and record what they do and do not prove, including the explicit mapping that authenticated removal or replacement of the published value is V1's decision-50 withdrawal mechanism.
- Confirm every public term matches `THESAURUS.md`.
- Confirm the selected baseline still fits the eight-PR plan; otherwise update this plan before code begins.

### Exit criteria

Reviewers can answer who authorized what, for whom, under which exact terms and destination reference, how replay is recognized, what the wallet must decide, and what V1 deliberately excludes.

## PR 4: implement the Library Allowance lifecycle protocol

### Goal

Implement the complete stateless Allowance lifecycle in `paykit-lib` without SDK persistence or policy.

### Why this is one PR

Primitives, immutable terms, and lifecycle events form one low-level protocol contract. Splitting them would land public types that cannot yet round-trip a complete lifecycle. Keeping SDK work out preserves a clear Library boundary and a reviewable wire-format diff.

### Scope

- Add encapsulated, validated identifiers for Allowances and any supporting protocol concepts chosen in PR 3.
- Add an Allowance-specific validated amount/value representation or exact-decimal helper with the comparison and canonicalization semantics frozen in PR 3.
- Reuse `PaymentAmount` input where appropriate, but copy and revalidate it into invariant-preserving Allowance values rather than relying on its publicly mutable fields.
- Add explicit party/role, anchored-period, rolling-window, and limit types rather than loosely typed maps.
- Add a validated immutable Allowance terms aggregate.
- Keep wire DTOs private, versioned, closed-world, and distinct from public domain types.
- Add proposal, acceptance, rejection, and end Event Message types with Event IDs and the causal references selected in PR 3.
- Add canonical JSON serialization and parsing with structural validation.
- Preserve recoverable Event ID and Allowance ID on malformed recognized payloads when safely possible.
- Add raw parsed-message wrappers following existing Payment Request patterns.
- Add typed send helpers using PR 2's internal sender.
- Register every lifecycle kind with the PR 1 inspector and with explicit SDK intake, outbound, and backup audit branches. These SDK branches are mechanical exhaustive-match classification arms forced by the kind enum, not lifecycle logic; that boundary is what keeps this a Library-scoped PR.
- Add fixtures showing a safeguarded older reader parks each new outbound kind unchanged.
- Enforce the one-message size budget before an event can enter the durable outbound queue.

### Design guardrails

- Construction validation covers syntax, cardinality, exact decimals, internal identifier correlation, and wire-size bounds.
- The Library does not evaluate trusted time, remaining capacity, period usage, endpoint currentness, wallet balance, or whether an Instruction should be paid.
- Do not reuse Payment Request recurrence, billing period, IDs, references, or proofs merely because they look similar.
- Public fields remain private behind validating constructors and accessors.
- Public APIs include rustdoc describing session/link responsibility and the 1000-byte private-message constraint.

### Verification

- Positive canonical fixtures for both proposal orientations and every lifecycle event.
- Round-trip, canonicalization, and field-by-field failure tests.
- Wrong version/kind, missing/extra/null fields, invalid IDs, invalid role combinations, malformed decimal/asset/period data, causal mismatch, and maximum-size boundaries.
- Sentinel-redaction tests for every parse failure surface.
- Mixed-stream classification and Event ID extraction tests.
- Frozen JSON fixtures that later bindings and compatibility tests can consume.
- `cargo fmt`, `cargo clippy --all-targets --all-features`, `cargo test`, and `cargo doc --no-deps`.
- `cd paykit-ffi && ./build.sh all` plus the non-release artifact policy above.

### Not in this PR

- Lifecycle persistence, projection, or commands.
- Destination observation or Payment Instructions.
- FFI types or generated bindings.
- Wallet policy or payment execution.

### Exit criteria

The Library can construct, validate, serialize, parse, inspect, correlate, and send every V1 Allowance lifecycle event while remaining stateless and policy-neutral.

## PR 5: implement the Rust SDK Allowance lifecycle

### Goal

Derive durable Allowance lifecycle views and expose atomic lifecycle commands in `paykit-sdk`.

### Why this is one PR

The pure reducer, storage-backed reads, and checked commands are one SDK use case. They share the same lifecycle truth table and source snapshot. Reviewers can read the reducer first, then verify that I/O orchestration preserves it, without generated FFI churn.

### Scope

- Add a pure reducer that consumes normalized inbound events, outbound intent, Event ID dedupe, authenticated counterparty/receiver context, and stream ordering.
- Bind every record to the exact party pair and receiver scope selected in PR 3.
- Derive proposal, accepted, rejected, ended, conflicted, malformed, and other frozen states without a clock, storage handle, or network client. Unlike the Payment Request reducer, do not take a `now` parameter and do not use local receive/create timestamps as cross-peer ordering authority; use the PR 3 ordering rules.
- Enforce authenticated direction, affirmative consent from both parties, and an explicit authenticated Allower action.
- Define deterministic duplicate, causal-race, same-batch, and end-dominance behavior from the PR 3 truth tables.
- Attach malformed lifecycle events only when the Allowance ID and source context are safely recoverable; otherwise leave them in the raw stream audit view.
- Overlay `RecoveryRequired` without rewriting the underlying derived lifecycle. This requires a separate overlay field: the Payment Request precedent overwrites the derived state in place, from two independent code paths that PR 5 must not multiply.
- Add storage-backed list/get APIs that load raw inbound, outbound, and dedupe inputs from one snapshot. Single-snapshot reads are new behavior — the existing Payment Request list path reads across multiple transactions.
- Keep raw events and outbound intent as the source of truth; do not add Allowance feature tables to `StorageState`.
- Add propose, accept, reject, and end commands using PR 2's checked-enqueue seam.
- Re-evaluate lifecycle state, local role, link readiness, and recovery state inside the transaction before appending exact serialized intent.
- Keep delivery status distinct from counterparty observation or authority state.

### Review order inside the PR

1. Pure lifecycle reducer and golden truth-table tests.
2. Snapshot-backed list/get queries and recovery overlay.
3. Atomic commands and concurrency tests.
4. Two-runtime integration cases for the complete lifecycle.

### Verification

- Both proposer orientations, mutual consent, and explicit Allower authorization.
- Duplicates, conflicting reuse, wrong sender/role, cross-peer ID reuse, causal mismatches, same-batch ordering, end races, and late events.
- Restart and backup/restore reproduce the same underlying record, expecting the recovery overlay: restore forces every peer into `RecoveryRequired` until relink is validated, so the comparison targets the underlying lifecycle, not the overlaid view.
- Direct reads normalize old classifications before projection.
- Concurrent commands append at most one legal transition; failures append nothing.
- Recovery-required, blocked, and unlinked peers cannot enqueue new lifecycle commands.
- Verify that changed terms require a new Allowance ID through the end-old/new-proposal replacement workflow, including both possible event orders, retries, and failure between the two non-atomic messages; expose no edit or counteroffer command.
- No amount, asset, time, Payee, endpoint, usage, capacity, or wallet-policy evaluation occurs.
- No network call runs inside a storage transaction.
- `cd paykit-ffi && ./build.sh all` plus the non-release artifact policy above.

### Not in this PR

- Payment Instructions or destination lookup.
- FFI/public platform methods.
- SDK claims, usage records, payment results, or callbacks.

### Exit criteria

Rust callers can propose, inspect, accept, reject, and end an Allowance through deterministic durable views and atomic outbound intent, with no SDK claim that a payment is eligible.

## PR 6: implement the Rust destination and Payment Instruction workflow

### Goal

Complete the Rust-side workflow that turns an accepted Allowance into an exact, locally observed wallet handoff while leaving authorization and execution with the wallet.

### Why this is one PR

Destination evidence, Instruction identity, semantic replay, lifecycle correlation, and wallet handoff are one security boundary. An isolated subpart is not useful to a wallet and risks implying more trust than the complete result provides. This is intentionally the largest PR; its ordered commits and hard scale gate are mandatory for reviewability.

### Required implementation order

1. Library destination-reference and observation primitives.
2. SDK exact-destination observation workflow.
3. Library Payment Instruction event and semantic fingerprint.
4. Pure SDK Instruction reducer.
5. SDK audit reads and wallet-qualified handoff.
6. Raw-projection scale gate.
7. Atomic Instruction submission command.

Do not begin submission or platform API work until the scale gate passes.

### Destination reference and observation

- Add a value containing the named Payee's Pubky identity/scope, receiver path, validated Payment Endpoint Identifier, and domain-separated expected-payload digest selected in PR 3.
- Bind the digest to protocol/domain/version, Payee, receiver scope, identifier, and exact payload bytes so values cannot be replayed across owners or namespaces.
- Add typed observation outcomes for `Match`, `Missing`, `Mismatch`, and `Unverifiable`.
- Reuse `pubky_routing.rs` and existing public endpoint reads; do not hard-code a path or enumerate alternative endpoints. The exact single-endpoint read already exists in `paykit-lib` with no SDK or FFI consumer, so the SDK workflow and platform export are new plumbing over an existing helper.
- Add one SDK workflow that reads only the exact referenced value from the named Payee's public storage scope near the wallet's execution decision.
- Map observation outcomes against real read behavior per the PR 3 rules: the existing text-fetch helper returns absence for an empty published file, treats only 404/GONE as absence, and surfaces every other failure as a transport error, which must map to `Unverifiable`, never `Missing`.
- Keep `PublicStorage` and network I/O out of the pure lifecycle and Instruction reducers.
- Return observed evidence plus local observation time/source metadata explicitly marked as non-authoritative for wallet trusted time.
- Report `Unverifiable` cleanly when Pubky access is unavailable or fails; timeout configuration remains the caller/Pubky client's responsibility.
- Do not choose an alternative, invoke `PaymentAdapter`, apply trusted time, or claim approval.
- Describe absence or changed bytes only as `Missing` or `Mismatch` for that observation. Do not claim authenticated withdrawal history or global freshness unless the Pubky read/cache contract selected in PR 3 actually proves it.

### Payment Instruction protocol

- Add an encapsulated `InstructionId` using the PR 3 format.
- Add the Payment Instruction type containing exact Allowance ID, Instruction ID, amount, asset, Payee reference, destination reference, Event ID, and causal fields.
- Add a canonical semantic fingerprint over the fields selected in PR 3, excluding Event ID and transport-attempt fields.
- Add private versioned DTOs, canonical serialization, structural validation, raw parsed-message wrapper, and typed send helper.
- Preserve recoverable Event ID, Allowance ID, and Instruction ID on malformed recognized payloads when possible.
- Register the new kind with inspection and all explicit intake, outbound, and backup audit points.
- Add old-reader queue-parking fixtures and enforce the one-message plaintext size limit before enqueue.
- Limit Library correlation to identifiers, causal references, and internal Payee/destination ownership consistency. Do not match the Instruction against Allowance policy.

### SDK projection and handoff

- Load normalized inbound messages, outbound intent, peer/receiver-scoped `EventDedupRecord` state, and Allowance lifecycle inputs from one storage transaction snapshot. Derive the wallet handoff and its as-of marker from that same snapshot.
- Add a pure Instruction reducer using the exact semantic replay key and fingerprint.
- Consume first, duplicate, and conflicting Event ID membership before semantic replay reduction. Fail closed on conflicting Event IDs even when the other body belongs to a different Private Message kind.
- Treat the same semantic key and fingerprint as one logical Instruction across Event ID retries.
- Treat the same semantic key with a different fingerprint as a fail-closed conflict.
- Verify the sender is the bound Allowee and the locally observed lifecycle is accepted and not ended according to the frozen ordering rules.
- Distinguish inbound Instructions observed by the local Allower from outbound audit records created by the local Allowee. Only the former can enter the wallet-qualified view.
- Attach malformed Instructions only when their identifiers and source context are safely recoverable; otherwise retain them only in the raw stream.
- Add audit list/get filters and a separate wallet-qualified read.
- Include complete accepted terms or an exact terms object plus digest, semantic replay key, source Event IDs, Payee/destination reference, lifecycle/correlation status, recovery status, and a local as-of stream marker. The as-of marker is new surface; no read API exposes a stream position today.
- Require and document the private-stream sync step needed for the freshest locally observed view.
- State that the as-of marker cannot prove that no remote end event is in flight and cannot make the handoff atomic with wallet state.
- Exclude `RecoveryRequired` and conflicted records from the wallet-qualified view while retaining them in the audit view.
- State that repeated query results are normal and that the wallet atomically owns replay, usage, capacity, concurrency, trusted time, and execution.
- Restate the contracted accounting context: only the exact instructed amount can consume capacity, fees do not, refunds do not restore it, and failed/unknown outcomes follow PR 3's rule.
- State in the wallet handoff that the Instruction must be approved or declined for that exact Payment Amount and cannot be automatically resized or asset-substituted.
- Add the Allowee submission command through the checked-enqueue seam. Lifecycle validation and exact append must occur in one transaction.
- Require the local sender to be the bound Allowee, and keep outbound delivery status distinct from counterparty observation or payment outcome.
- Preserve exact Event/Instruction identity and serialized bytes across transport retries.

### Scale gate

- Establish representative V1 workload bounds before exposing the final SDK surface. No latency, memory, or blob budget exists anywhere in the specs today; this gate creates them, so record the workload model alongside the numbers.
- Measure lifecycle plus high-volume Instruction projection, semantic dedupe/conflicts, restart, backup restore, and FFI storage transactions that clone and Postcard-encode `StorageState`.
- Measure against the real FFI cost model: every storage transaction — including read-only projections — serializes behind one process-wide mutex and pays a full blob decode, deep clone, and structural comparison, so the gate must bound the length of that serialized critical section, not only blob size.
- Record accepted latency, memory, and persisted-blob budgets in the relevant SDK specification.
- Verify common filters do not repeatedly perform avoidable work outside the accepted raw-projection design.
- If the budgets fail, do not merge an ad hoc cache. Stop, design a versioned index and rebuild/migration path, and update the PR plan. That outcome cannot honestly fit the current eight-PR baseline without changing scope.

### Verification

- Destination match, missing, mismatch, and unverifiable behavior against the exact requested Payee and identifier.
- No fallback to another receiver, identifier, cached Private Payment List, or Allowee-supplied payload.
- Domain-separation and exact-byte digest fixtures, Pubky access absence, transport failure, and caller-configured timeout behavior.
- Instruction round-trip, maximum-size, wrong version/kind, invalid identity, malformed amount, Payee/destination mismatch, and sentinel redaction.
- Fingerprint stability across different Event IDs and change detection for every semantic field.
- Same/new Event ID retries, semantic conflicts, cross-peer/cross-Allowance reuse, wrong role, before-accept, after-end, same-batch, and recovery cases.
- Cross-kind Event ID conflicts and a race where an end event commits between two attempted reads; no wallet handoff may combine inputs from different snapshots.
- Allower wallet-qualified versus Allowee outbound-audit filtering.
- Complete handoff context and deterministic repeated results across restart/restore.
- Concurrent submit, stale/end race, unlinked/recovery-required peer, storage rollback, and exact retry.
- Exact-amount handoff tests proving no SDK helper returns or recommends a substituted amount or asset.
- Three-party Allowee/Payee scenarios.
- Performance budgets on both in-memory and FFI-backed storage representations.
- Proof that pure reducers have no clock, storage, `PublicStorage`, or network dependency.
- `cd paykit-ffi && ./build.sh all` plus the non-release artifact policy above.

### Not in this PR

- A private/authenticated destination descriptor protocol.
- Wallet rule evaluation, usage accounting, reservation, signing, payment, or settlement.
- Result/proof/Receipt events or SDK processing dispositions.
- Platform bindings or foreign callbacks.

### Exit criteria

Rust applications can submit, observe, correlate, and hand off exact Payment Instructions with authenticated lifecycle and destination evidence, while the SDK remains explicit that the wallet must independently decide and execute.

## PR 7: add unified Swift and Kotlin bindings

### Goal

Expose the complete selected V1 Allowance workflow through one coordinated platform-facade expansion after the Rust APIs are stable.

### Why this is one PR

One binding PR avoids two separate root Swift/Kotlin protocol breaks and repeated generated/native churn. The semantic work is already reviewed in PRs 4 through 6; this PR is limited to adapter types, conversions, delegation, documentation, and generated outputs.

### Scope

- Add a feature-local Allowances FFI facade with focused DTO and conversion modules.
- Export async/suspend lifecycle list/get and propose/accept/reject/end methods.
- Export Instruction submission, audit list/get, wallet-qualified handoff, and exact destination-observation methods.
- Represent exact decimals as validated strings across the boundary. The FFI already carries three separate value/asset record shapes; adding the Allowance amount as a fourth must be a deliberate, documented choice rather than an accident.
- Use typed public reference metadata for Pubky identity, receiver path, endpoint identifier, and digest.
- Follow PR 3's frozen exposure boundary: use opaque getter-based objects for sensitive or likely-to-evolve aggregates, and freeze the complete field set of every generated record.
- Include an explicit `Unknown` case in every new Allowances output FFI enum and map unsupported/non-exhaustive SDK values to it in exhaustive Rust conversions. Reject `Unknown` as caller input. Both halves follow existing convention: sixteen FFI output enums already carry a terminal `Unknown` with a standard doc line, and there is precedent for rejecting `Unknown` on input. Adding a new FFI wire discriminant later remains a binding compatibility change; old generated clients cannot decode an unknown tag automatically.
- Provide stable status codes or typed data for correlation conflict, recovery block, unsupported kind, and observation uncertainty without casually expanding the top-level FFI error enum.
- Document repeated delivery, semantic replay keys, app-owned atomic state, local as-of semantics, and generated Swift/Kotlin stringification risks.
- Call out both root-interface expansions for downstream fakes, mocks, and conformers. The generated `PaykitSdkProtocol`/`PaykitSdkInterface` is already roughly one hundred methods assembled from `#[uniffi::export]` impl blocks across many files, so any new exported method widens it. The foreign `SdkPaymentAdapter` trait has no default methods, so any addition is a hard compile break for every app conformer — V1 must not add methods to it.
- Account for the binding toolchain: generated names pass through a post-processor that strips the `Ffi` prefix, Kotlin is generated by a pinned fork of the bindgen (verify Swift and Kotlin agree on every new construct), and `./build.sh all` requires macOS.
- Keep wallet execution app-driven; do not add synchronous foreign callbacks.

### Review strategy

- Review handwritten UniFFI interfaces, DTOs, conversions, error/status mappings, and API docs first.
- Treat generated Swift/Kotlin/header changes as reproducible artifacts and verify that they correspond exactly to the handwritten interface.
- Focus FFI tests on validation delegation and boundary conversion; do not duplicate the core/SDK state-machine matrix.
- Use separate commits for: handwritten facade/DTO/conversion code; boundary tests and API docs; generated Swift/Kotlin/header sources; and tracked native binaries.
- Include a PR-description inventory of every added method, type, enum, record field, and downstream root-interface conformance change.

### Verification

- Round-trip every public type and every enum status, including unknown-output behavior.
- Confirm invalid decimals, identifiers, role combinations, and references are rejected through canonical Rust constructors.
- Confirm diagnostics never include private payloads, metadata, proofs, or serde detail.
- Exercise a small lifecycle and Instruction delegation smoke with in-memory and FFI-backed storage.
- Compile small Swift and Kotlin consumer examples that construct representative public values, call lifecycle and Instruction async APIs, and exhaustively handle every status. A generator-only typecheck is not sufficient to verify postprocessed names and signatures.
- Re-run previous-generation state/backup and unknown-kind compatibility fixtures.
- Run `cd paykit-ffi && ./build.sh all`.
- Commit required generated Swift/Kotlin/header sources and tracked Android JNI libraries.
- Leave `Package.swift`'s released URL and checksum unchanged unless this PR publishes that exact XCFramework release.

### Not in this PR

- New protocol or SDK policy decisions.
- A second lifecycle reducer in FFI.
- Platform-side wallet execution or storage claims.
- Deferred V1 extensions.

### Exit criteria

Swift and Kotlin apps can use the same lifecycle, destination observation, Instruction audit, wallet handoff, and submission capabilities as Rust through one coherent interface expansion.

## PR 8: finish documentation and release readiness

### Goal

Make the completed eight-PR baseline auditable, integrable, and ready for the repository's release process without introducing late architectural code.

### Why this is one PR

Security, recovery, and integration tests belong beside the code that establishes each invariant. The final PR is therefore documentation and verification, not a catch-all hardening implementation. Humans can review the public contract and rollout story without unrelated code churn.

### Scope

- Update root and crate READMEs, rustdoc examples, binding usage guides, changelog, release notes, and compatibility policy.
- Document the complete lifecycle for both proposer directions, mutual consent, and explicit Allower approval.
- Document initialization/sync requirements, local as-of semantics, recovery blocking, and the fact that remote events may still be in flight.
- Document the exact semantic replay key and the wallet's responsibility for atomic replay, time, usage, capacity, concurrency, balance, fees, private safeguards, signing, payment, and settlement.
- Document that execution is approve-or-decline for the exact instructed Payment Amount; automatic amount or asset substitution is forbidden.
- Document destination observation limits and why `Match` is evidence rather than authorization.
- Document repeated Instruction delivery and restart/restore behavior.
- Document unsupported downgrade behavior and the previous-generation reader path.
- Document sensitive app-visible records and generated platform stringification risks.
- Document the single generated root-interface expansion and downstream migration considerations.
- List every deferred extension and make clear that it is not part of V1.
- Record the scale-gate operating envelope and final cross-layer verification results.
- Keep actual artifact publication, tag creation, and checksum replacement in the repository's normal release workflow unless this PR is explicitly designated as the release PR.

### Final verification

- Re-run two-runtime lifecycle scenarios for both proposal directions.
- Re-run the third-party Payee, exact destination, replay/conflict, end-race, restart, restore, normalization, downgrade, and recovery scenarios owned by earlier PRs.
- Re-run diagnostic/FFI redaction checks with sentinel values.
- Re-run the accepted scale budget.
- Run:

```text
cargo fmt
cargo clippy --all-targets --all-features
cargo test
cargo doc --no-deps
cd paykit-ffi && ./build.sh all
```

- If the final build produces no intended FFI/native changes, do not commit generated or native churn and do not change `Package.swift`'s released checksum.

### Baseline definition of done

- Either linked party can propose exact immutable terms, both parties affirm them, and an explicit authenticated Allower action approves the authority.
- Replacing accepted terms uses a new proposal/new Allowance ID and ends the old Allowance without an edit/counteroffer path or a cross-message atomicity promise.
- Both parties derive the same fail-closed lifecycle from authenticated private events, including rejection and unilateral end.
- The Allowee can submit one exact uniquely identified Payment Instruction for an accepted Allowance.
- The SDK durably retains, correlates, and semantically deduplicates Instructions and exposes only inbound, locally observed protocol-qualified Instructions to the Allower wallet.
- The wallet receives complete accepted terms, exact Instruction data, semantic replay key, destination reference/evidence, recovery state, and local as-of context.
- The wallet independently owns shared/private rule evaluation, trusted time, usage, replay, capacity, concurrency, fees, balance, signing, payment, and settlement.
- The wallet approves or declines the exact instructed Payment Amount and never automatically substitutes another amount or asset.
- Restart, classifier evolution, state/backup compatibility, and Encrypted Link recovery preserve fail-closed behavior.
- Rust, Swift, and Kotlin expose the same selected V1 scope.
- No Paykit layer promises payment, restores wallet capacity after a refund, or performs wallet side effects.

### Exit criteria

The selected V1 is documented and verified across Library, SDK, storage, Pubky routing, Swift, and Kotlin, with deferred capabilities and release mechanics clearly separated.

## Review checklist for every PR

- The PR has one primary architectural reason to change and can be reverted independently.
- The PR description maps its scope to this plan and names any deliberately deferred work.
- Public names match `THESAURUS.md`; vocabulary changes land in PR 3 before code depends on them.
- Public APIs have rustdoc and use invariant-preserving structs/enums with private fields.
- Wire DTOs remain private, versioned, closed-world, and separately validated.
- Library caller-input failures use `PaykitError::Validation`; corrupt network data uses `PaykitError::InvalidData`; exhaustive matches cover `Transport`, `NotFound`, `InvalidData`, and `Validation`. The SDK remaps Library `InvalidData` to its `Protocol` variant and drops the source, so contracts for SDK/FFI callers must be written in those terms.
- Private plaintext, arbitrary metadata, payloads, proofs, serde details, and secret material cannot escape through internal diagnostics or FFI errors.
- App-visible typed records are documented as sensitive; Rust `Debug` redaction is not claimed to control generated platform stringification.
- No Pubky path is hard-coded outside `pubky_routing.rs`.
- Missing public endpoint files and directories retain the repository's `None`/empty semantics.
- Every maximum-size private message fits one `PUBKY_NOISE_MSG_LEN` buffer before durable queueing.
- Positive and failure tests accompany every protocol type; replay, conflict, recovery, normalization, restore, and concurrency tests accompany the SDK event families that need them.
- A storage, backup, compatibility-envelope, or index change is explicit and never hidden inside a query or binding PR.
- Binding changes run `cd paykit-ffi && ./build.sh all` and update both platforms together.
- `Package.swift`'s release checksum changes only for the exact artifact release it names; feature PRs revert the build's checksum rewrite, and CI does not gate this file.
- FFI tests focus on boundary mapping rather than duplicating core/SDK policy matrices.
- No network request or foreign callback occurs while an SDK storage transaction is held.
- No PR implies atomicity between SDK state and the wallet's payment store.

## Planning rule for implementation sessions

At the start of each PR session:

1. Reread [`allowances.md`](allowances.md), [`THESAURUS.md`](../THESAURUS.md), this plan, and the contracts governing that phase.
2. Inspect changes merged since this plan was written.
3. Confirm the phase's dependencies and baseline assumptions still hold.
4. Write a focused implementation plan for that PR only.
5. Keep tests beside the invariant they establish rather than deferring them to PR 8.
6. Stop if a protocol decision, scale result, or compatibility discovery invalidates the eight-PR shape.

Do not compensate for an unresolved protocol question by embedding an undocumented policy or authenticity assumption in Rust code. If a phase can no longer be reviewed as one coherent PR, update this document and obtain scope direction before implementation.
