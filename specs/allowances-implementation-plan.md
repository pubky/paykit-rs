# Allowances implementation outline

This is a rough sequence for implementing Allowances in small, independently
reviewable pull requests. It is intentionally not a detailed implementation
specification. Each phase should receive its own focused plan in the session
where that phase is implemented.

The default sequence is seven PRs. The only likely split point is the stateless
Library work, which may become two PRs if its detailed plan is too large to
review comfortably.

## How Allowances fit the current architecture

- `paykit-lib` already owns stateless protocol types, wire parsing, serialization,
  and Encrypted Link send helpers. Allowance protocol code should follow that
  existing shape.
- `paykit-sdk` already stores the raw private-message stream, outbound intent,
  and Event ID dedupe records. Allowance lifecycle and Payment Instruction views
  can initially be derived from those records, as Payment Request views are
  today.
- The Library already supports an exact public Payment Endpoint read through
  Pubky Routing. Destination observation can build on that instead of adding a
  new transport abstraction.
- `paykit-ffi` already exposes SDK features through feature-local UniFFI modules.
  Swift and Kotlin should remain thin bindings over the settled Rust behavior.

No standalone refactor PR is planned. A phase may make a small refactor that is
directly required by its feature, but it should not redesign existing private
message handling, storage, or lifecycle APIs speculatively.

## Phase 1: protocol specification

**Purpose:** Agree on the smallest implementable V1 protocol before adding code.

Rough scope:

- Freeze the vocabulary and update `THESAURUS.md` where needed.
- Define the Allower, Allowee, and Payee roles and the consent required to create
  authority.
- Define the proposal, acceptance, rejection, and ending lifecycle, including
  how changed terms are represented.
- Define the shared terms, Allowance ID, Payment Instruction ID, message kinds,
  JSON shapes, Event ID use, ordering, correlation, and replay semantics.
- Define how a Payment Instruction names an exact Payment Amount, Payee, and
  destination.
- Set message-size limits and compatibility rules for unknown or future values.
- Confirm whether observing an exact Payee-scoped public Payment Endpoint is
  sufficient for V1 destination replacement and withdrawal behavior.
- Record the boundary between Paykit coordination and wallet-owned policy and
  execution.

This is a documentation-only PR. It should not include preparatory code changes.

## Phase 2: stateless Library protocol

**Depends on:** Phase 1.

**Purpose:** Implement the V1 wire protocol in `paykit-lib` without adding SDK
state or wallet behavior.

Rough scope:

- Add validated Allowance identifiers, terms, lifecycle events, destination
  references, and Payment Instructions.
- Add parsers, serializers, correlation checks, and Encrypted Link send helpers.
- Register the new Private Message Kinds and keep private-message routing
  exhaustive.
- Add focused positive, invalid-data, boundary, and round-trip tests.
- Make only the minimal SDK intake, outbound-validation, and backup-validation
  match additions needed for the workspace to understand and preserve the new
  raw message kinds. SDK Allowance behavior remains out of scope.

The Library stays stateless and Pubky-specific. It does not evaluate live
Allowance eligibility, keep usage, read a clock, or authorize a payment.

If this phase is too large after its detailed plan is written, split it into:

1. Allowance types and lifecycle messages.
2. Destination references and Payment Instruction messages.

That would make the overall sequence eight PRs, without introducing a separate
refactor phase.

## Phase 3: SDK lifecycle

**Depends on:** Phase 2.

**Purpose:** Expose durable Allowance lifecycle behavior in `paykit-sdk`.

Rough scope:

- Derive Allowance state from the existing inbound private stream, outbound
  message records, Event ID dedupe records, and linked-peer context.
- Add propose, accept, reject, and end operations.
- Add list and get views suitable for applications and later bindings.
- Enforce the protocol's party, consent, lifecycle, link, and recovery
  preconditions before queuing a command.
- Add only the transaction logic needed to check those preconditions and append
  outbound intent atomically. Do not create a generic lifecycle framework or
  migrate unrelated features.

Start with derived views rather than a new Allowance storage table. Storage or
index changes should be introduced only if the implementation demonstrates a
real need for them.

## Phase 4: destination observation

**Depends on:** Phases 1 and 2.

**Purpose:** Let a wallet compare the destination named by a Payment Instruction
with the Payee's currently observable destination.

Rough scope:

- Resolve the exact Payee and Payment Endpoint reference chosen in Phase 1.
- Return `Match`, `Missing`, `Mismatch`, or `Unverifiable` evidence.
- Use the existing Pubky public-storage and routing helpers when V1 uses public
  Payment Endpoints.
- Cover replacement, deletion, malformed data, and transport-failure cases.

This result is an observation, not authorization. The SDK should not select a
fallback endpoint, cache an authoritative answer, invoke `PaymentAdapter`, or
decide that a payment is safe.

## Phase 5: Payment Instruction workflow

**Depends on:** Phases 2 through 4.

**Purpose:** Add durable Instruction coordination and produce input that an
Allower's wallet can evaluate.

Rough scope:

- Add Allowee submission of a Payment Instruction through the existing durable
  outbound path.
- Derive Instruction list, get, and audit views from the raw private-message and
  outbound records.
- Detect semantic duplicates and conflicts separately from Event ID transport
  dedupe.
- Correlate each Instruction with the authenticated parties and the relevant
  Allowance lifecycle defined by the protocol.
- Combine the exact Instruction, lifecycle evidence, replay classification, and
  destination observation into a wallet-qualified handoff.
- Preserve the Instruction's exact Payment Amount; Paykit must not substitute an
  amount, asset, Payee, or destination.

The handoff means that Paykit has validated and correlated protocol evidence. It
does not mean the Instruction is approved or payable. The wallet remains the
authority for policy, trusted time, usage, capacity, and execution.

## Phase 6: Swift and Kotlin bindings

**Depends on:** Phases 3 through 5.

**Purpose:** Expose the settled Rust API to both supported platforms in one
coordinated PR.

Rough scope:

- Add FFI records, enums, conversions, and SDK methods for Allowance lifecycle,
  destination observations, Instruction submission and audit, and the wallet
  handoff.
- Keep FFI code as conversion and delegation; do not recreate lifecycle or
  wallet policy in Swift/Kotlin-facing code.
- Regenerate Swift and Kotlin together and add small consumer-facing tests or
  compile checks for the new surface.
- Document any source changes required for application mocks or protocol
  conformers.

## Phase 7: integration and release readiness

**Depends on:** Phases 1 through 6.

**Purpose:** Verify the complete feature across real boundaries and prepare it
for downstream adoption.

Rough scope:

- Add end-to-end two-party lifecycle and Payment Instruction flows.
- Cover a third-party Payee, destination change or disappearance, replay and
  conflict cases, restart, backup/restore, and Encrypted Link recovery.
- Check that derived views remain reasonable for representative message
  histories; add an index only through a separately reviewed re-plan if the
  measured result requires one.
- Finish public API docs, examples, integration guidance, and release notes.
- Run the full Rust, documentation, and all-platform binding verification.

Feature behavior and its focused tests belong in the earlier owning PRs. This
final PR should integrate and document settled behavior, not become a catch-all
for unfinished implementation.

## PR discipline

- Each phase is one independently buildable and reviewable PR.
- Keep refactors local to the phase that needs them and explain why they are
  required.
- Land feature-specific tests and public API docs with the feature, not only in
  Phase 7.
- For every Rust code PR, run the repository Rust checks and
  `cd paykit-ffi && ./build.sh all` so Swift and Kotlin remain synchronized.
- Write the detailed implementation plan for one phase at a time. Later phases
  may be adjusted based on what earlier reviews decide, while preserving the
  seven-PR target or the explicit eight-PR Library split.

## V1 boundary

Completing these phases completes Paykit's part of Allowances V1. Paykit does
not implement wallet policy evaluation, usage accounting, trusted time,
capacity reservation, signing, payment execution, or settlement. Those remain
wallet responsibilities.
