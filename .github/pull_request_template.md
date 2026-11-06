<!--
Thanks for contributing to Paykit! Please fill in the sections below.
Keep the title imperative and <=72 chars (e.g. "Implement private list fetch API").
-->

## Summary

<!-- What does this PR do, and why? Describe the motivation. -->

## Protocol impact

<!--
Does this change the wire format, storage paths, message kinds, or scope
strings? Cite the specs/v0.2 sections it implements. Spec changes go in their
own PR first. Reference THESAURUS.md for domain terms.
Write "None" if this PR has no protocol impact.
-->

## Downstream bindings

<!--
Does this change any public struct, enum, error code, or scope string that
language bindings will consume? If so, list it so the bindings can follow.
Write "None" if not applicable.
-->

## Checklist

- [ ] `cargo fmt` passes
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` is clean
- [ ] `cargo test` passes (unit + doc tests)
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` builds
- [ ] Added tests for new behaviour (at least one positive and one failure-path test for new protocol features)
- [ ] Public API changes include `///` docs and use THESAURUS.md vocabulary
- [ ] All vector tests in `specs/v0.2/vectors/` still pass
- [ ] No internal review IDs, process words or private reference code (see AGENTS.md)
- [ ] I reviewed this PR myself and asked an LLM to review it and check whether it can be simplified
