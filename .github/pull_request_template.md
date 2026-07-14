<!--
Thanks for contributing to Paykit! Please fill in the sections below.
Keep the title imperative and <=72 chars (e.g. "Implement private list fetch API").
-->

## Summary

<!-- What does this PR do, and why? Describe the motivation. -->

## Protocol impact

<!--
Does this change the wire format, storage paths, message kinds, or capability
strings? Link the relevant spec/issue. Reference THESAURUS.md for domain terms.
Write "None" if this PR has no protocol impact.
-->

## Downstream bindings

<!--
Does this change any exposed struct, enum, or capability string that the
Swift / Kotlin bindings consume? If so, note what needs to be
regenerated or updated in sync. Write "None" if not applicable.
-->

## Checklist

- [ ] `cargo fmt` passes
- [ ] `cargo clippy --all-targets --all-features` is clean (or allows are justified)
- [ ] `cargo test` passes (unit + doc tests)
- [ ] `cargo doc --no-deps` builds without warnings
- [ ] Added tests for new behaviour (at least one positive and one failure-path test for new protocol features)
- [ ] Public API changes include `///` docs and use THESAURUS.md vocabulary
- [ ] Platform bindings rebuilt for **all** targets if FFI changed (`cd paykit-ffi && ./build.sh all`)
- [ ] I reviewed this PR myself and asked an LLM to review it and check whether it can be simplified
