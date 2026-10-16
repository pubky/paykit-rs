# Security Policy

## Reporting a Vulnerability

Please report security vulnerabilities **privately — do not open a public issue.**

Use GitHub's private vulnerability reporting:
**https://github.com/pubky/paykit-rs/security/advisories/new**
(Repository → **Security** → **Report a vulnerability**)

Include as much as you can:

- affected crate(s) and version or commit — `paykit-lib`, `paykit-sdk`,
  `paykit-ffi`, or a Swift / Kotlin binding
- the impact (e.g. key exposure, ciphertext malleability, path traversal,
  timing leak, broken access control on private messages/Receipts)
- steps to reproduce, or a minimal proof of concept
- any suggested remediation

**Do not include real routing keys, secrets, or private keys** in a report —
redact them or use fixtures.

We aim to acknowledge reports within a few business days and will keep you
updated as we investigate.

## Supported Versions

Paykit remains pre-1.0. The final `0.1.0` release and `master` receive security
fixes. Release candidates (`0.1.0-rc*`) are no longer back-patched.

| Version            | Supported |
| ------------------ | --------- |
| `0.1.0` / `master`  | Yes       |
| `0.1.0-rc*`         | No        |

## Scope

**In scope** — the crates in this repository (`paykit-lib`, `paykit-sdk`,
`paykit-ffi`) and their generated bindings, specifically Paykit's own:

- cryptographic and private-message / Receipt handling
- storage-path construction (e.g. traversal via a Payment Endpoint identifier)
- access-control and durability assumptions around private Paykit storage

**Out of scope — report upstream instead:**

- the Pubky SDK, `pubky-noise`, homeservers, or other dependencies
- applications that consume Paykit

## Coordinated Disclosure

Please give us reasonable time to ship a fix before public disclosure. We're
happy to credit reporters who would like acknowledgement.
