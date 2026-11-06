# Paykit v0.2

[![CI](https://github.com/pubky/paykit-rs/actions/workflows/ci.yml/badge.svg?branch=v0.2)](https://github.com/pubky/paykit-rs/actions/workflows/ci.yml?query=branch%3Av0.2)

This branch builds Paykit v0.2. The current protocol and its crates, v0.1, live on
[`master`](https://github.com/pubky/paykit-rs/tree/master).

Paykit v0.2 is a payment protocol on the Pubky Switch, an inbox and coordination
service for Pubky identities. Senders write sealed, signed messages to an
identity's switch, and the identity's own apps coordinate through it with claims
and shared records. The Pubky Homeserver keeps the identity's long-term public
data. Paykit adds its scopes, message kinds, collections and payment flows on
top. The design is a review candidate, not yet frozen.

## Specification

- **The Paykit spec**, in [`specs/v0.2/`](specs/v0.2/): what Paykit adds on the
  Switch. Its spec decides behaviour, its wire format decides bytes, and its
  vectors in [`specs/v0.2/vectors/`](specs/v0.2/vectors/) are normative.
- **The Pubky Switch spec**, in
  [`pubky/pubky-switch`](https://github.com/pubky/pubky-switch) under `spec/`:
  identities, apps and certificates, envelopes, admission, claims, shared
  records and the HTTP API. Paykit builds on it and never redefines it.

For canonical protocol vocabulary, see [THESAURUS.md](THESAURUS.md).

## Crates

| Crate | What it is |
| --- | --- |
| `paykit-lib` | Paykit's formats and rules, on `pubky-switch-core`: scopes and their switch permissions, message kinds and bodies, collections and policies, obligations, amounts and the payment rules. No I/O, no clock. |
| `paykit-sdk` | The app SDK for wallets and seller apps, on `pubky-switch-client` and `pubky-switch-signer`: payment requests, proofs and receipts, recurring requests, Allowances, Private Payment Lists and receiving. |

The Switch's own crates live in `pubky/pubky-switch`. The crates here are being
built; none is published yet. Language bindings come later.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Contributor rules are in [AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
