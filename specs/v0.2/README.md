# Paykit v0.2

Status: draft, not frozen.

Paykit v0.2 is a proposed redesign of how Paykit moves messages and how an identity's apps coordinate. Paykit v0.2 is a protocol on the Pubky Dispatch, an inbox and coordination service for Pubky identities, specified on its own in the [pubky-dispatch](https://github.com/pubky/pubky-dispatch) repository ([spec](https://github.com/pubky/pubky-dispatch/blob/c2b19c95f5f60937df3995efda13e477e9b1ff47/spec/spec.md), [wire format](https://github.com/pubky/pubky-dispatch/blob/c2b19c95f5f60937df3995efda13e477e9b1ff47/spec/wire-format.md)). This revision builds on the Dispatch at commit `c2b19c95f5f60937df3995efda13e477e9b1ff47`, which is the normative baseline for Dispatch citations. Its adoption does not establish complete implementation or vector coverage; the specification distinguishes the available client contracts from the protected-record paths that remain withheld. Each identity uses a Pubky Dispatch. Senders write sealed, signed messages to its inbox. The identity's own apps coordinate through it, with claims and shared records. This replaces v0.1's shared, locked file and its Noise handshakes over Homeserver files.

Section 4 of the spec separately adopts the Dispatch's host-local app display names for the outer app-record body. That scoped change leaves Paykit's certificate scopes and signed seller labels authoritative. The signed app-record examples and their expected Paykit results are described in the [vector guide](vectors/README.md#app-record-format-coverage).

This folder specifies what Paykit adds on the Dispatch: its scopes and the Dispatch permissions each needs, its message kinds, its collections and their policies, the payment flows, Allowances, recurring requests, labelled selling scopes and the retention of payment evidence. v0.2 replaces v0.1 with no migration (spec Section 16).

The design may still change before it is frozen. The spec's Section 17 lists the open questions and the decisions left to the team; those of the Dispatch are in the Pubky Dispatch spec.

## Files

| File | What it is for |
| --- | --- |
| [`spec.md`](spec.md) | The specification. It decides behaviour: Paykit's scopes and authorization, app records, message kinds, shared records, paying, payment flows and features, re-sends, threat model, guarantees and performance targets. Start here. |
| [`wire-format.md`](wire-format.md) | The wire format. It decides bytes: Paykit's encodings, derivations, signature, certificate and record checks, messages and shared record values, so that a second team can build a compatible implementation. |
| [`vectors/`](vectors/) | Test vectors for the wire format, as JSON files. Its `README.md` lists the files and how they were made. They are self-contained: Paykit's own fixed keys, certificates and records, built with the Dispatch's algorithms. |
| [`conformance.md`](conformance.md) | Observable wallet and service outcomes, including ordering, retry, restore and refusal. |
| [`implementation.md`](implementation.md) | Library, vector and wallet evidence for the published implementation and its remaining limits. |

## Current protocol

The [v0.1 specifications](../) remain available for the current protocol. The documents in this directory describe v0.2.
