# Allowances implementation outline

This is a rough sequence for implementing Allowances in small, independently
reviewable pull requests. It is intentionally not a detailed implementation
specification. Each phase should receive its own focused plan in the session
where that phase is implemented.

The default sequence is five PRs. Allowances reuse ordinary Payment Requests,
so there is no separate Payment Instruction, destination-observation, or
Allowance-specific payment execution phase.

## How Allowances fit the current architecture

- `paykit-lib` already owns stateless protocol types, wire parsing,
  serialization, and Encrypted Link send helpers. Allowance lifecycle code
  should follow that existing shape.
- `paykit-sdk` already stores the raw private-message stream, outbound intent,
  and Event ID dedupe records. Allowance lifecycle views can initially be
  derived from those records, as Payment Request views are today.
- Ordinary Payment Requests are the only way to exercise an Allowance. Their
  messages, lifecycle, endpoint resolution, and Payment Proof behavior remain
  unchanged.
- `paykit-ffi` already exposes SDK features through feature-local UniFFI
  modules. Swift and Kotlin should remain thin bindings over settled Rust
  behavior.

No standalone refactor PR is planned. A phase may make a small refactor that is
directly required by its feature, but it should not redesign existing private
message handling, storage, or lifecycle APIs speculatively.

## Phase 1: protocol specification

**Purpose:** Agree on the smallest implementable V1 protocol before adding
code.

Rough scope:

- Freeze the vocabulary and update `THESAURUS.md` where needed.
- Define the exact Allower-Allowee pair, with the authenticated Payment Request
  sender serving as both Allowee and protocol Payee. Treat the ultimate
  economic beneficiary of an opaque Payment Endpoint Payload as out of scope.
- Define the proposal, acceptance, rejection, and ending lifecycle, including
  how changed terms are represented.
- Define the shared terms, Allowance ID, lifecycle message kinds, JSON shapes,
  Event ID use, ordering, correlation, replay semantics, message-size limits,
  and compatibility rules.
- Record that Payment Requests carry no Allowance ID. The wallet matches only a
  newly handled request to exactly one active compatible Allowance and otherwise
  retains ordinary manual handling.
- Define the one-time and recurring integration boundary: automatic acceptance
  may use an Allowance, acceptance consumes no capacity, and the wallet pins and
  rechecks the selected Allowance for each Billing Period payment.
- Record the boundary between Paykit coordination and wallet-owned enablement,
  policy, usage, reservation, and execution.

This is a documentation-only PR. It should not include preparatory code
changes.

## Phase 2: stateless Library lifecycle protocol

**Depends on:** Phase 1.

**Purpose:** Implement the V1 Allowance lifecycle wire protocol in
`paykit-lib` without adding SDK state or wallet behavior.

Rough scope:

- Add validated Allowance identifiers, shared terms, and proposal, acceptance,
  rejection, and ending Event Messages.
- Add parsers, serializers, party and correlation checks, and Encrypted Link
  send helpers.
- Register the new Private Message Kinds and keep private-message routing
  exhaustive.
- Add focused positive, invalid-data, boundary, message-size, and round-trip
  tests.
- Make only the minimal SDK intake, outbound-validation, and backup-validation
  match additions needed for the workspace to understand and preserve the new
  raw message kinds. SDK Allowance behavior remains out of scope.

The Library stays stateless and Pubky-specific. It does not match Payment
Requests, evaluate live Allowance eligibility, keep usage, read a clock,
reserve capacity, or authorize a payment.

## Phase 3: SDK lifecycle and wallet-facing views

**Depends on:** Phase 2.

**Purpose:** Expose durable Allowance lifecycle behavior and enough
authenticated context for a wallet to perform its own Payment Request matching.

Rough scope:

- Derive Allowance state from the existing inbound private stream, outbound
  message records, Event ID dedupe records, and Linked Peer context.
- Add propose, accept, reject, and end operations.
- Add list and get views that retain the exact peer and link scope and are
  suitable for applications and later bindings.
- Enforce the protocol's party, consent, lifecycle, link, and recovery
  preconditions before queuing a command.
- Add only the transaction logic needed to check those preconditions and append
  outbound intent atomically. Do not create a generic lifecycle framework or
  migrate unrelated features.
- Verify that an application can combine an ordinary Payment Request record
  with active Allowance views without changing or annotating the Request.

Start with derived views rather than a new Allowance storage table. Storage or
index changes should be introduced only if the implementation demonstrates a
real need for them.

The SDK does not choose an Allowance, turn on automatic payment, persist wallet
usage or reservations, auto-reject a Request, select a Payment Endpoint, or
decide that a payment is safe.

## Phase 4: Swift and Kotlin bindings

**Depends on:** Phase 3.

**Purpose:** Expose the settled Rust Allowance lifecycle API to both supported
platforms in one coordinated PR.

Rough scope:

- Add FFI records, enums, conversions, and SDK methods for Allowance lifecycle
  operations and authenticated views.
- Keep FFI code as conversion and delegation; do not recreate lifecycle,
  Payment Request matching, or wallet policy in Swift/Kotlin-facing code.
- Regenerate Swift and Kotlin together and add small consumer-facing tests or
  compile checks for the new surface.
- Document any source changes required for application mocks or protocol
  conformers.

## Phase 5: integration and release readiness

**Depends on:** Phases 1 through 4.

**Purpose:** Verify the complete Paykit feature across real boundaries and
prepare it for downstream wallet adoption.

Rough scope:

- Add end-to-end two-party Allowance lifecycle flows, including restart,
  backup/restore, Event ID replay, and Encrypted Link recovery.
- Verify integration guidance against ordinary one-time and Recurring Payment
  Requests without adding an Allowance field or alternate Request lifecycle.
- Document wallet requirements for the local auto-pay setting, exact-one match,
  durable first-processing disposition, non-retroactive handling, pinned
  recurring association, per-Billing-Period rechecks, duplicate prevention
  across manual and automatic paths, automatic-payment-only capacity,
  terminal-failure release, and unknown-outcome reservation.
- Document that Payment Endpoint selection, Acceptance, cancellation, payment,
  and Payment Proof follow the existing Payment Request behavior. Uncertain
  automatic handling falls back to the manual flow.
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
  Phase 5.
- For every Rust code PR, run the repository Rust checks and
  `cd paykit-ffi && ./build.sh all` so Swift and Kotlin remain synchronized.
- Write the detailed implementation plan for one phase at a time. Later phases
  may be adjusted based on what earlier reviews decide while preserving the
  five-PR target.

## V1 boundary

Completing these phases completes Paykit's part of Allowances V1. Paykit does
not implement the wallet-local auto-pay setting, Payment Request matching,
Allowance selection or pinning, policy evaluation, usage accounting, trusted
time, capacity reservation, signing, payment execution, or settlement. Those
remain wallet responsibilities.
