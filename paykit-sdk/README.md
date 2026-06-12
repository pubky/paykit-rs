# Paykit SDK

Stateful Rust runtime for Paykit integrations.

`paykit-sdk` builds on `paykit-lib` and owns SDK-level local state for Pubky
identity status, public Payment Endpoint sync, Encrypted Link state, private
stream intake, Private Payment List derivation, contact payment resolution, and
outbound Private Application Message delivery.

The crate is currently Rust-only. Existing Swift, Kotlin, and React Native
bindings continue to expose low-level `paykit-lib` APIs until SDK bindings are
added.

Payment execution, settlement detection, balances, route policy, product UI,
and app backup transport stay outside the SDK and are provided by application
or payment-adapter code.
