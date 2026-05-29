# Test Organization

These are integration-style crate tests that need crate-private helpers, test-only
accessors, and the shared embedded Pubky testnet setup from `mod.rs`.

Keep small, pure unit tests next to the module they exercise. Add tests here
when they drive multiple Paykit modules together, need an embedded testnet, or
need shared Encrypted Link fixtures.
