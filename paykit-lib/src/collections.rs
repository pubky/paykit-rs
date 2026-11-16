//! THROWAWAY (#411): `HashMap` and `HashSet` with a hash-flooding-resistant
//! seed on every target, including wasm32-unknown-unknown.
//!
//! # Why this exists
//!
//! std's `RandomState` seeds SipHash-1-3 from the OS random number generator on
//! native targets. On wasm32-unknown-unknown std has no randomness source, so
//! `std::sys::random::unsupported::hashmap_random_keys` (Rust 1.96) uses the
//! address of a stack byte and the address of a heap byte as the two keys. In
//! wasm linear memory those addresses are small and nearly the same on every
//! run of a given binary, so an attacker who knows the published `.wasm` can
//! predict the keys and precompute colliding keys for any map whose keys they
//! control (peer public keys, Event IDs, Payment Request IDs, endpoint
//! identifiers read from a homeserver, and so on). That turns those maps into a
//! CPU-exhaustion (HashDoS) vector in the browser.
//!
//! # What this does
//!
//! * Native targets: [`RandomState`] *is* `std::collections::hash_map::RandomState`,
//!   so [`HashMap`] and [`HashSet`] are exactly the std types. Native behavior
//!   and the native public API are unchanged.
//! * wasm32-unknown-unknown: [`RandomState`] seeds SipHash-1-3 (the same
//!   function std uses, from the `siphasher` crate) with 128 bits from
//!   `getrandom` (Web Crypto `crypto.getRandomValues`), once per thread, and
//!   bumps `k0` for each new map exactly like std does.
//!
//! Every Paykit map must be built with `HashMap::default()`,
//! `HashMap::from_iter(..)`, `collect()` or `with_capacity_and_hasher(..)`;
//! `HashMap::new()`, `with_capacity` and `From<[_; N]>` only exist for std's
//! `RandomState` and so do not compile on wasm32 with these aliases.
//!
//! # Limits
//!
//! * Dependencies (pubky, reqwest, http, serde_json maps, ...) still use std's
//!   `RandomState` on wasm32. This only fixes Paykit's own maps.
//! * On wasm32 the public API names these aliases, so a Rust caller that builds
//!   a std `HashMap` and passes it to Paykit must switch to these aliases there.
//!   That is a wasm-only API difference; production should decide whether to
//!   keep it, or move the attacker-keyed maps to `BTreeMap` instead.

/// Hasher builder used by every Paykit `HashMap` and `HashSet`.
///
/// On native targets this is std's `RandomState`. On wasm32-unknown-unknown it
/// is a SipHash-1-3 builder seeded from `getrandom`; see the module docs.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub type RandomState = std::collections::hash_map::RandomState;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use wasm::RandomState;

/// `std::collections::HashMap` with Paykit's [`RandomState`].
pub type HashMap<K, V> = std::collections::HashMap<K, V, RandomState>;

/// `std::collections::HashSet` with Paykit's [`RandomState`].
pub type HashSet<T> = std::collections::HashSet<T, RandomState>;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasm {
    use std::{cell::Cell, fmt, hash::BuildHasher};

    /// SipHash-1-3 hasher builder keyed from `getrandom` on wasm32.
    #[derive(Clone)]
    pub struct RandomState {
        k0: u64,
        k1: u64,
    }

    impl RandomState {
        /// Create a builder with fresh keys, mirroring std's `RandomState::new`.
        pub fn new() -> Self {
            // Same scheme as std: one random seed per thread, and `k0` is
            // bumped for every new builder so two maps never share keys.
            // wasm32-unknown-unknown has one thread (paykit-lib refuses to
            // build with `atomics`), so this is one seed per wasm instance.
            thread_local! {
                static KEYS: Cell<(u64, u64)> = Cell::new(seed_keys());
            }
            KEYS.with(|keys| {
                let (k0, k1) = keys.get();
                keys.set((k0.wrapping_add(1), k1));
                Self { k0, k1 }
            })
        }

        /// THROWAWAY (#411): test hook for paykit-wasm-tests. std's wasm32
        /// keys are 32-bit linear-memory addresses, so their high 32 bits are
        /// always zero; 128 random bits have a nonzero high half with
        /// probability 1 - 2^-64. Returns a bool so the keys never leave.
        #[doc(hidden)]
        pub fn seed_has_high_bits(&self) -> bool {
            (self.k0 >> 32) != 0 || (self.k1 >> 32) != 0
        }
    }

    impl Default for RandomState {
        fn default() -> Self {
            Self::new()
        }
    }

    impl BuildHasher for RandomState {
        type Hasher = siphasher::sip::SipHasher13;

        fn build_hasher(&self) -> Self::Hasher {
            siphasher::sip::SipHasher13::new_with_keys(self.k0, self.k1)
        }
    }

    impl fmt::Debug for RandomState {
        // Never print the keys: they are the whole defense.
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RandomState").finish_non_exhaustive()
        }
    }

    fn seed_keys() -> (u64, u64) {
        let mut seed = [0u8; 16];
        match getrandom_03::fill(&mut seed) {
            Ok(()) => {
                let mut k0 = [0u8; 8];
                let mut k1 = [0u8; 8];
                k0.copy_from_slice(&seed[..8]);
                k1.copy_from_slice(&seed[8..]);
                (u64::from_le_bytes(k0), u64::from_le_bytes(k1))
            }
            // THROWAWAY (#411): no Web Crypto (very old or locked-down JS
            // host). Fall back to std's own (address-derived, predictable)
            // keys, which is exactly the behavior before this module existed,
            // instead of panicking in library code. Every Paykit key
            // generation path also needs getrandom, so such a host cannot run
            // Paykit anyway. Production should decide between this
            // degrade-to-std behavior and failing loudly.
            Err(_) => {
                let fallback = std::collections::hash_map::RandomState::new();
                (fallback.hash_one(0u8), fallback.hash_one(1u8))
            }
        }
    }
}
