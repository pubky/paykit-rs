# Paykit v0.2

Status: review candidate, not frozen.

Paykit v0.2 is a proposed redesign of how Paykit moves messages and how an identity's apps coordinate. Each identity gets a Pubky Switch, a small service of its own. Senders write sealed, signed messages to its inbox. The identity's own apps coordinate through it, with claims and shared records. This replaces v0.1's shared, locked file and its Noise handshakes over Homeserver files.

The design is still open to review. It may change before it is frozen. The spec's Section 22 lists the open questions and the decisions left to the team.

## Files

| File | What it is for |
| --- | --- |
| [`spec.md`](spec.md) | The specification. It decides behaviour: keys and apps, messages, the Pubky Switch, coordination, payment flows, lifecycle, threat model, guarantees and performance targets. Start here. |
| [`wire-format.md`](wire-format.md) | The wire format. It decides bytes: encodings, signatures, schemas and the HTTP API, so that a second team can build a compatible implementation. |
| [`vectors/`](vectors/) | Shared test vectors for the wire format, as JSON files. Its `README.md` lists the files and how they were made. |

## Shared separately

The reference prototype, its results, an independent implementation and the review records aren't in this folder. They are shared separately. The documents name them but give no paths.

## Current protocol

The v0.1 specs in [`specs/`](../) stay the current protocol. This folder doesn't change them, and it adds no code.
