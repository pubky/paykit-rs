# Paykit v0.2

[![CI](https://github.com/pubky/paykit-rs/actions/workflows/ci.yml/badge.svg?branch=v0.2)](https://github.com/pubky/paykit-rs/actions/workflows/ci.yml?query=branch%3Av0.2)

This branch builds Paykit v0.2. The current protocol and its crates, v0.1, live on
[`master`](https://github.com/pubky/paykit-rs/tree/master).

Paykit v0.2 gives each Pubky identity a Pubky Switch, a small service of its own.
Senders write sealed, signed messages to its inbox, and the identity's own apps
coordinate through it with claims and shared records. The Pubky Homeserver keeps
the identity's long-term public data. The design is a review candidate, not yet
frozen.

## Specification

- [`specs/v0.2/spec.md`](specs/v0.2/spec.md) decides behaviour.
- [`specs/v0.2/wire-format.md`](specs/v0.2/wire-format.md) decides bytes.
- [`specs/v0.2/vectors/`](specs/v0.2/vectors/) holds the shared test vectors,
  which every implementation reproduces byte for byte.

For canonical protocol vocabulary, see [THESAURUS.md](THESAURUS.md).

## Crates

| Crate | What it is |
| --- | --- |
| `paykit-lib` | The stateless protocol core: encodings, keys, signatures, envelopes, signed documents, message checks and chain verification. No I/O, no clock. |
| `paykit-sdk` | The app SDK for wallets and seller apps: the Pubky Switch client, app state, payment flows and network I/O. |
| `paykit-signer` | The signer core: enrollment, app approval, `identity.json`, revocation, provider moves, recovery and the hand-over. |

The crates are being built; none is published yet. Language bindings come later.

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
