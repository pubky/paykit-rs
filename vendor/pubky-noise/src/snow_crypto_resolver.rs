//! Custom Snow CryptoResolver for deterministic ephemeral key generation.
//!
//! Snow's `Builder` always generates ephemeral keys internally via its RNG.
//! To support session restore (replaying handshake messages to re-derive the
//! same transport keys), we need to inject the same ephemeral key material
//! that was used in the original session.
//!
//! This module provides:
//! - [`DeterministicRng`]: A `snow::types::Random` implementation that returns
//!   a pre-set 32-byte seed on the first fill, then delegates to the real OS RNG.
//! - [`ReplayResolver`]: A `snow::resolvers::CryptoResolver` that wraps Snow's
//!   `DefaultResolver` but overrides `resolve_rng()` to return a `DeterministicRng`.

use snow::resolvers::{CryptoResolver, DefaultResolver};
use snow::types::{Cipher, Dh, Hash, Random};

/// A deterministic RNG that returns a pre-set seed on the first 32-byte fill.
///
/// Snow calls `resolve_rng()` to get an RNG, then uses it exactly once during
/// `build_initiator()` / `build_responder()` to generate the local ephemeral
/// keypair (via `Dh::generate()`). By returning our pre-set seed bytes, we
/// force Snow to derive the same ephemeral keypair every time.
///
/// After the first fill, subsequent calls delegate to the real OS RNG (via
/// `getrandom`). In practice, Snow only calls the RNG once for ephemeral key
/// generation during handshake construction.
pub struct DeterministicRng {
    seed: [u8; 32],
    used: bool,
}

impl DeterministicRng {
    pub fn new(seed: [u8; 32]) -> Self {
        DeterministicRng { seed, used: false }
    }
}

impl Random for DeterministicRng {
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), snow::error::Error> {
        if !self.used && dest.len() == 32 {
            dest.copy_from_slice(&self.seed);
            self.used = true;
            Ok(())
        } else {
            // Fallback to real randomness for any other calls.
            // This should not happen during normal handshake construction,
            // but we handle it gracefully.
            getrandom::fill(dest).map_err(|_| snow::error::Error::Rng)
        }
    }
}

/// A CryptoResolver that injects a deterministic RNG for ephemeral key replay.
///
/// All other crypto primitives (DH, Hash, Cipher) are delegated to Snow's
/// `DefaultResolver`. Only the RNG is overridden.
pub struct ReplayResolver {
    default: DefaultResolver,
    seed: [u8; 32],
}

impl ReplayResolver {
    pub fn new(seed: [u8; 32]) -> Box<Self> {
        Box::new(ReplayResolver {
            default: DefaultResolver,
            seed,
        })
    }
}

impl CryptoResolver for ReplayResolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        Some(Box::new(DeterministicRng::new(self.seed)))
    }

    fn resolve_dh(&self, choice: &snow::params::DHChoice) -> Option<Box<dyn Dh>> {
        self.default.resolve_dh(choice)
    }

    fn resolve_hash(&self, choice: &snow::params::HashChoice) -> Option<Box<dyn Hash>> {
        self.default.resolve_hash(choice)
    }

    fn resolve_cipher(&self, choice: &snow::params::CipherChoice) -> Option<Box<dyn Cipher>> {
        self.default.resolve_cipher(choice)
    }
}
