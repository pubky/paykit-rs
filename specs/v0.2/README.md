# Paykit v0.2

Status: review candidate, not frozen.

Paykit v0.2 is a proposed redesign of how Paykit moves messages and how an identity's apps coordinate. Paykit v0.2 is a protocol on the Pubky Switch, an inbox and coordination service for Pubky identities, specified on its own in the [pubky-switch](https://github.com/pubky/pubky-switch) repository ([spec](https://github.com/pubky/pubky-switch/blob/0c211c210a9ada2f9fc07e4c5322fae69715bfcd/spec/spec.md), [wire format](https://github.com/pubky/pubky-switch/blob/0c211c210a9ada2f9fc07e4c5322fae69715bfcd/spec/wire-format.md)). This revision builds on the Switch at commit `0c211c210a9ada2f9fc07e4c5322fae69715bfcd`, and every link to the Switch points there. Each identity uses a Pubky Switch. Senders write sealed, signed messages to its inbox. The identity's own apps coordinate through it, with claims and shared records. This replaces v0.1's shared, locked file and its Noise handshakes over Homeserver files.

This folder specifies what Paykit adds on the Switch: its scopes and the switch permissions each needs, its message kinds, its collections and their policies, the payment flows, Allowances, recurring requests, labelled selling scopes, the retention of payment evidence and the migration from v0.1.

The design is still open to review. It may change before it is frozen. The spec's Section 17 lists the open questions and the decisions left to the team; those of the switch are in the Pubky Switch spec.

## Files

| File | What it is for |
| --- | --- |
| [`spec.md`](spec.md) | The specification. It decides behaviour: Paykit's scopes and authorization, app records, message kinds, shared records, paying, payment flows and features, re-sends, threat model, guarantees and performance targets. Start here. |
| [`wire-format.md`](wire-format.md) | The wire format. It decides bytes: Paykit's encodings, derivations, signature, certificate and record checks, messages and shared record values, so that a second team can build a compatible implementation. |
| [`vectors/`](vectors/) | Test vectors for the wire format, as JSON files. Its `README.md` lists the files and how they were made. They build on the [Switch's vectors](https://github.com/pubky/pubky-switch/tree/0c211c210a9ada2f9fc07e4c5322fae69715bfcd/spec/vectors) at commit `c553428`. |

## Shared separately

The reference prototype, its results and an independent implementation aren't in this folder. They are shared separately. The documents name them but give no paths.

## Current protocol

The v0.1 specs in [`specs/`](../) stay the current protocol. This folder doesn't change them, and it adds no code.
