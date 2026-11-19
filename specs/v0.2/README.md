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

`paykit-lib` derives settlement bucket keys under the permanent policy. Its vectors cover the policy hash, blinded bucket key, sealed bucket and marks. It checks that plain and labelled selling certificates hold `paykit_settlements` and `paykit_contacts`.

Wire Section 5 describes how a payee can retain a receipt it already issued when it observes a deletion. Section 18 lists the SDK cases for receipt verification, attribution, credit restrictions and restore. The SDK on `v0.2` implements these receipt checks and preserves the installation’s verified receipt and credit restriction across an in-place restore. Wire Section 5 also defines publication of a receipt for a one-time request within one service generation. This shared replacement and its service-cursor hold still require SDK and Switch integration.

The SDK writes a settlement mark and the signed receipt in the same conditional batch and queues delivery only when requested. Two payee apps that confirm the same settlement concurrently can apply one marked credit, including when they name different occurrences. The wallet tests exercise both the first write and an existing bucket, with the scripted and PostgreSQL services. Section 18 also requires repair after provider loss. Wire Section 5 governs protective publication for other record families.

## Bounded message recovery

Sections 3.3 and 9 require an extension eviction to atomically record its recovery obligation with the payload and processed-pair changes. They do not guarantee that the payload returns. An extension-only receiver may lack permission to send a recovery ask; its mark still uses the ordinary bounded pool and may expire or be evicted before an ask is delivered. The host is notified when that undelivered obligation is retired, but the notice contains no payload. Recovery is best effort under mark retirement, sender retention and re-send limits, so an evicted payload may be lost permanently. Even a delivered ask does not prove recovery. These requirements add no unbounded waiting pool or intake block; the generic Switch support, Paykit implementation and runtime cases identified in the specification remain required.

## Allowance acceptance and initial ledgers

Pending proposals and rejections use the records and delivery checks in Section 8.3.1. The proposal binds the verified Allower generation. The SDK withholds new acceptance, executor authority and automatic spending.

## Queued message delivery

Section 8.1 requires the counterparty send rules to govern every queued delivery and retry, including kept batches, generic flush, payment resume, reload and restore. Blocked or unknown recipients do not receive held message kinds; eligible user-initiated Allowance ends, other permitted stopping and payment-outcome messages and list withdrawals still go. An automatic end for a withdrawal crossing acceptance keeps its counterparty hold while its durable end and own-app notice still apply; unknown origin grants no exception. Restore selectively replaces every carried list delivery, including an empty withdrawal, with a safely newer empty version while preserving unrelated owed messages. The generic client support for identifying and durably holding individual owed messages, its SDK integration and the Section 18 conformance cases remain required before release. Section 8.4.1 and wire 4.13 and 5.8 define a permanent counter per publishing app, recipient and generation. The publisher signs the list privately, then queues those exact bytes only when the conditional counter write applies. Repeated restores continue from the service counter. The publishing SDK, record codec, generated vectors and end-to-end cases remain outstanding.

Sections 3.1 and 3.3 require plain `paykit_request` to send Private Payment Lists and to advertise or receive list requests. The current library uses the general selling role for both checks. The plain-scope checks and their runtime conformance case remain outstanding. Section 8.4.1 requires an empty list withdrawal to omit the publisher's conversation entry on every delivery path. The pinned Switch client chooses that mutation by message kind. It cannot express the difference between an empty withdrawal and a nonempty first list. A publisher must keep an empty delivery owed until its path can honor the signed message's decision. A counter can supply a recipient or version only when the authenticated writer of its signed operation equals the publisher bound into the record key.

## Shared separately

The reference prototype, its results and an independent implementation aren't in this folder. They are shared separately. The documents name them but give no paths.

## Current protocol

The v0.1 specs in [`specs/`](../) stay the current protocol. This folder doesn't change them, and it adds no code.
