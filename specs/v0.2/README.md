# Paykit v0.2

Status: review candidate, not frozen.

Paykit v0.2 is a proposed redesign of how Paykit moves messages and how an identity's apps coordinate. Paykit v0.2 is a protocol on the Pubky Switch, an inbox and coordination service for Pubky identities, specified on its own in the [pubky-switch](https://github.com/pubky/pubky-switch) repository ([spec](https://github.com/pubky/pubky-switch/blob/28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3/spec/spec.md), [wire format](https://github.com/pubky/pubky-switch/blob/28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3/spec/wire-format.md)). This revision builds on the Switch at commit `28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3`, and every link to the Switch points there. Each identity uses a Pubky Switch. Senders write sealed, signed messages to its inbox. The identity's own apps coordinate through it, with claims and shared records. This replaces v0.1's shared, locked file and its Noise handshakes over Homeserver files.

This folder specifies what Paykit adds on the Switch: its scopes and the switch permissions each needs, its message kinds, its collections and their policies, the payment flows, Allowances, recurring requests, labelled selling scopes and the retention of payment evidence. v0.2 replaces v0.1 with no migration (spec Section 16).

The design is still open to review. It may change before it is frozen. The spec's Section 17 lists the open questions and the decisions left to the team; those of the switch are in the Pubky Switch spec.

## Files

| File | What it is for |
| --- | --- |
| [`spec.md`](spec.md) | The specification. It decides behaviour: Paykit's scopes and authorization, app records, message kinds, shared records, paying, payment flows and features, re-sends, threat model, guarantees and performance targets. Start here. |
| [`wire-format.md`](wire-format.md) | The wire format. It decides bytes: Paykit's encodings, derivations, signature, certificate and record checks, messages and shared record values, so that a second team can build a compatible implementation. |
| [`vectors/`](vectors/) | Test vectors for the wire format, as JSON files. Its `README.md` lists the files and how they were made. They are self-contained: Paykit's own fixed keys, certificates and records, built with the Switch's algorithms. |

## Settlement implementation status

The settlement bucket and deletion-boundary rules in wire 5.7 are requirements for the implementation. Boundary-only buckets, monotonic boundary merging, authenticated absence at creation, exact delete-incarnation checks at put-back, provenance checks by later readers, authenticated delete application and evidence order, publication before the service acknowledges a delete, and the corresponding runtime and conformance tests are not implemented here. The pinned Switch needs generic support for the publication-time distinction and authenticated incarnation evidence, as well as the client's durable publication and acknowledgment hold. A prior read, zero-version condition, local notification or queued repair does not provide those guarantees. Publication before acknowledgment uses existing atomic batch and cursor semantics; the additional creation and provenance support is a release requirement, not a Switch wire format defined here. The specification's Section 18 lists the required scenarios, and Section 13 states the retention and availability limits.

## Bounded message recovery

Sections 3.3 and 9 require an extension eviction to atomically record its recovery obligation with the payload and processed-pair changes. They do not guarantee that the payload returns. An extension-only receiver may lack permission to send a recovery ask; its mark still uses the ordinary bounded pool and may expire or be evicted before an ask is delivered. The host is notified when that undelivered obligation is retired, but the notice contains no payload. Recovery is best effort under mark retirement, sender retention and re-send limits, so an evicted payload may be lost permanently. Even a delivered ask does not prove recovery. These requirements add no unbounded waiting pool or intake block; the generic Switch support, Paykit implementation and runtime cases identified in the specification remain required.

## Queued message delivery

Section 8.1 requires the counterparty send rules to govern every queued delivery and retry, including kept batches, generic flush, payment resume, reload and restore. Blocked or unknown recipients do not receive held message kinds; eligible user-initiated Allowance ends, other permitted stopping and payment-outcome messages and list withdrawals still go. An automatic end for a withdrawal crossing acceptance keeps its counterparty hold while its durable end and own-app notice still apply; unknown origin grants no exception. Restore selectively replaces every carried list delivery, including an empty withdrawal, with a safely newer empty version while preserving unrelated owed messages. The generic client support for identifying and durably holding individual owed messages, its SDK integration and the Section 18 conformance cases remain required before release. This specification adds no client API, wire format or generated vector.

## Shared separately

The reference prototype, its results and an independent implementation aren't in this folder. They are shared separately. The documents name them but give no paths.

## Current protocol

The v0.1 specs in [`specs/`](../) stay the current protocol. This folder doesn't change them, and it adds no code.
