# Paykit v0.2

Status: draft, not frozen.

Paykit v0.2 is a proposed redesign of how Paykit moves messages and how an identity's apps coordinate. Paykit v0.2 is a protocol on the Pubky Switch, an inbox and coordination service for Pubky identities, specified on its own in the [pubky-switch](https://github.com/pubky/pubky-switch) repository ([spec](https://github.com/pubky/pubky-switch/blob/5f3209aae28d1b538a31c8ce43506b39df828b61/spec/spec.md), [wire format](https://github.com/pubky/pubky-switch/blob/5f3209aae28d1b538a31c8ce43506b39df828b61/spec/wire-format.md)). This revision builds on the Switch at commit `5f3209aae28d1b538a31c8ce43506b39df828b61`, which is the normative baseline for Switch citations. Its adoption does not establish complete implementation or vector coverage; the specification distinguishes the available client contracts from the protected-record paths that remain withheld. Each identity uses a Pubky Switch. Senders write sealed, signed messages to its inbox. The identity's own apps coordinate through it, with claims and shared records. This replaces v0.1's shared, locked file and its Noise handshakes over Homeserver files.

This folder specifies what Paykit adds on the Switch: its scopes and the switch permissions each needs, its message kinds, its collections and their policies, the payment flows, Allowances, recurring requests, labelled selling scopes and the retention of payment evidence. v0.2 replaces v0.1 with no migration (spec Section 16).

The design may still change before it is frozen. The spec's Section 17 lists the open questions and the decisions left to the team; those of the switch are in the Pubky Switch spec.

## Files

| File | What it is for |
| --- | --- |
| [`spec.md`](spec.md) | The specification. It decides behaviour: Paykit's scopes and authorization, app records, message kinds, shared records, paying, payment flows and features, re-sends, threat model, guarantees and performance targets. Start here. |
| [`wire-format.md`](wire-format.md) | The wire format. It decides bytes: Paykit's encodings, derivations, signature, certificate and record checks, messages and shared record values, so that a second team can build a compatible implementation. |
| [`vectors/`](vectors/) | Test vectors for the wire format, as JSON files. Its `README.md` lists the files and how they were made. They are self-contained: Paykit's own fixed keys, certificates and records, built with the Switch's algorithms. |

## Settlement implementation status

The spec's Status paragraph is the full statement of implementation status; this section summarizes its settlement part.

`paykit-lib` on `v0.2` implements settlement marks and the earlier bucket format with deletion boundaries. Its vectors use the earlier policy without `permanent`. It checks that plain and labelled selling certificates hold `paykit_settlements` and `paykit_contacts`.

Wire Section 5 describes how a payee can retain a receipt it already issued when it observes a deletion. Section 18 lists the SDK cases for receipt verification, attribution, credit restrictions and restore. The SDK on `v0.2` implements these receipt checks and preserves the installation’s verified receipt and credit restriction across an in-place restore. Shared replacement of a deleted record remains outstanding.

Wire 5.7 uses a permanent bucket for each group of settlements. The Switch supports the policy. Paykit still needs to use it to derive the bucket key, publish its vectors and write the mark with the signed receipt in the same conditional batch. Sections 8.1 and 18 specify concurrent first credit, retry and provider loss. Wire Section 5 continues to govern protective publication for other record families.

## Bounded message recovery

Sections 3.3 and 9 require an extension eviction to atomically record its recovery obligation with the payload and processed-pair changes. They do not guarantee that the payload returns. An extension-only receiver may lack permission to send a recovery ask; its mark still uses the ordinary bounded pool and may expire or be evicted before an ask is delivered. The host is notified when that undelivered obligation is retired, but the notice contains no payload. Recovery is best effort under mark retirement, sender retention and re-send limits, so an evicted payload may be lost permanently. Even a delivered ask does not prove recovery. These requirements add no unbounded waiting pool or intake block; the generic Switch support, Paykit implementation and runtime cases identified in the specification remain required.

## Allowance acceptance and initial ledgers

Pending proposals and rejections use the records and delivery checks in Section 8.3.1. The proposal binds the verified Allower generation. The SDK withholds new acceptance, executor authority and automatic spending.

## Queued message delivery

Section 8.1 requires the counterparty send rules to govern every queued delivery and retry, including kept batches, generic flush, payment resume, reload and restore. Blocked or unknown recipients do not receive held message kinds; eligible user-initiated Allowance ends, other permitted stopping and payment-outcome messages and list withdrawals still go. An automatic end for a withdrawal crossing acceptance keeps its counterparty hold while its durable end and own-app notice still apply; unknown origin grants no exception. Restore selectively replaces every carried list delivery, including an empty withdrawal, with a safely newer empty version while preserving unrelated owed messages. The generic client support for identifying and durably holding individual owed messages, its SDK integration and the Section 18 conformance cases remain required before release. Section 8.4.1 and wire 4.13 and 5.8 define a permanent counter per publishing app, recipient and generation. The publisher signs the list privately, then queues those exact bytes only when the conditional counter write applies. Repeated restores continue from the service counter. The SDK publishes reusable endpoints to chosen recipients, updates them and withdraws their lists after restore. A new sharing choice is required before a restored publisher sends nonempty endpoints. For a selected recipient, the SDK answers requests for reusable lists within the hourly and rolling 24-hour limits. A repeated request for an answered version resends the current signed list. Single-use renewal, the withdrawal retry schedule and generated publication-counter vectors remain outstanding.

The library and SDK require plain `paykit_request` to send Private Payment Lists and to advertise or receive list requests (Sections 3.1 and 3.3). An app with a labelled selling scope and `paykit_pay` can ask for and pay a plain publisher’s list. If a restored app lacks the plain selling scope, former-recipient discovery and withdrawal wait until its certificate permits them. The publisher binds each counter to its authenticated writer and compares the signed message ID and hash at a given version (wire 5.8). The Switch retains the conversation decision of each signed message through delivery and retry. An empty withdrawal leaves the publisher’s conversation unchanged (Section 8.4.1).

## Shared separately

The reference prototype, its results and an independent implementation aren't in this folder. They are shared separately. The documents name them but give no paths.

## Current protocol

The v0.1 specs in [`specs/`](../) stay the current protocol. This folder doesn't change them, and it adds no code.
