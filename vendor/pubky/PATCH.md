# Pubky public response access

This directory contains the published `pubky` 0.11.0 crate, with one addition:
`PublicStorage::get_unchecked` returns the response without consuming HTTP error
bodies. Paykit needs this to apply its download limit before any body buffering.
Existing GET behavior, resource validation, client configuration, routing and
ICANN fallback remain unchanged.

The crate archive SHA-256 is
`15a7b191157d57d5095f1577c8064aeda85f96606957fcf4c0a6f4b675adb96d`.
The published archive records upstream commit
`6a14bdb8fa2e30ef4e4b241fcdd3992c453d2378` with a dirty source tree. The archive,
not that commit alone, is the reproducible source for this copy. The normalized
Cargo.toml, README.md and src tree are preserved. Cache metadata and the crate's
own lockfile are omitted. LICENSE is from that upstream commit, with trailing whitespace removed.

`public-get-unchecked.patch` records the sole source change. Apply it with `patch -p1 < public-get-unchecked.patch` from the
published crate root to reproduce this source tree. The workspace patch selects this
copy for every Pubky consumer, including pubky-noise. Do not update only one
consumer to a different Pubky source.

## Distribution

Build mobile artifacts from the complete workspace so the Cargo patch is active.
Swift and Kotlin binary packages contain the compiled implementation and do not
need a runtime path to this directory.

Cargo ignores a dependency's patch table. Rust consumers using Paykit as a Git
or path dependency must apply the equivalent Pubky patch at their workspace
root. Registry packaging does not carry the workspace override. The SDK requires
this added method and must not be released to crates.io against unpatched Pubky
0.11.0. A separately published compatible dependency is required before such a
release. No release is performed by the binding build checks.
