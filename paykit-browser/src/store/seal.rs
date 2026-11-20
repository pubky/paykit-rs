//! The store's keys and its snapshot cipher: [`DataKey`], [`StoreId`] and [`SnapshotSealer`].
//!
//! Every snapshot is sealed with XChaCha20-Poly1305 (the `chacha20poly1305` crate) under the
//! store's data key, with a fresh random 24-byte nonce for every write. The data key is 32
//! random bytes. It is in the clear only in WebAssembly memory, inside a [`DataKey`], which
//! wipes it when dropped, and inside the cipher, which wipes its own copy when dropped. Both
//! are kept on the heap, so moving them moves a pointer, not the key; the cipher is built on
//! the stack before it is boxed, and what the compiler leaves there is not wiped. How the key
//! is kept between sessions is the browser side's: this module never stores it.
//!
//! Randomness comes from the operating system's generator through `getrandom` 0.2
//! (`crypto.getRandomValues` in a browser). A failure of that generator is an error
//! ([`RandomnessUnavailable`]), never a panic.
//!
//! The format of a sealed slot, and what its associated data binds, is in `format.rs`.

use std::fmt;

use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{AeadInPlace, KeyInit, OsRng};
use chacha20poly1305::{Key, Tag, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

/// The length of a data key, in bytes.
pub(crate) const KEY_LEN: usize = 32;

/// The length of a store identifier, in bytes.
pub(crate) const STORE_ID_LEN: usize = 16;

/// The length of a snapshot's nonce, in bytes.
pub(crate) const NONCE_LEN: usize = 24;

/// The length of the Poly1305 tag at the end of a sealed snapshot, in bytes.
pub(crate) const TAG_LEN: usize = 16;

/// The operating system's random number generator failed, so no key, identifier or nonce was
/// made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RandomnessUnavailable;

/// Fill `bytes` from the operating system's random number generator.
fn fill_random(bytes: &mut [u8]) -> Result<(), RandomnessUnavailable> {
    #[cfg(test)]
    if fault::take() {
        return Err(RandomnessUnavailable);
    }
    OsRng
        .try_fill_bytes(bytes)
        .map_err(|_| RandomnessUnavailable)
}

/// A store's data key: the 32-byte XChaCha20-Poly1305 key its snapshots are sealed under.
///
/// The bytes live on the heap, written there in place, and are wiped when the key is dropped.
/// They are never formatted: `Debug` prints `DataKey(..)`. They leave the type only through
/// [`DataKey::as_bytes`], for the one call that wraps the key, and into the cipher of a
/// [`SnapshotSealer`].
pub(crate) struct DataKey(Box<Zeroizing<[u8; KEY_LEN]>>);

impl DataKey {
    /// A new random data key.
    pub(crate) fn generate() -> Result<DataKey, RandomnessUnavailable> {
        let mut key = Box::new(Zeroizing::new([0; KEY_LEN]));
        fill_random(key.as_mut_slice())?;
        Ok(DataKey(key))
    }

    /// The data key held in `bytes`, as an unwrap returns it, or `None` if `bytes` is not
    /// exactly [`KEY_LEN`] bytes long. The bytes are copied: the caller wipes its own copy.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<DataKey> {
        if bytes.len() != KEY_LEN {
            return None;
        }
        let mut key = Box::new(Zeroizing::new([0; KEY_LEN]));
        key.copy_from_slice(bytes);
        Some(DataKey(key))
    }

    /// The key's bytes, only to wrap the key.
    pub(crate) fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for DataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DataKey(..)")
    }
}

/// A store's identifier: 16 random bytes made with the store, in the header of each of its
/// snapshots and in the associated data that seals them, so a snapshot of one store does not
/// open as another's. It is not secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StoreId([u8; STORE_ID_LEN]);

impl StoreId {
    /// A new random store identifier.
    pub(crate) fn generate() -> Result<StoreId, RandomnessUnavailable> {
        let mut id = [0; STORE_ID_LEN];
        fill_random(&mut id)?;
        Ok(StoreId(id))
    }

    /// The store identifier `bytes`.
    pub(crate) const fn from_bytes(bytes: [u8; STORE_ID_LEN]) -> StoreId {
        StoreId(bytes)
    }

    /// The identifier's bytes.
    pub(crate) const fn to_bytes(self) -> [u8; STORE_ID_LEN] {
        self.0
    }
}

/// A fresh random nonce for one snapshot write.
pub(crate) fn random_nonce() -> Result<[u8; NONCE_LEN], RandomnessUnavailable> {
    let mut nonce = [0; NONCE_LEN];
    fill_random(&mut nonce)?;
    Ok(nonce)
}

/// Seals and opens one store's snapshots: the cipher under the store's data key, and the
/// store's identifier.
///
/// The cipher keeps its own copy of the key, on the heap, which `chacha20poly1305` wipes when
/// it is dropped. `Debug` shows the store identifier only.
pub(crate) struct SnapshotSealer {
    cipher: Box<XChaCha20Poly1305>,
    store_id: StoreId,
}

impl SnapshotSealer {
    /// The sealer of the store `store_id` under `key`.
    pub(crate) fn new(key: &DataKey, store_id: StoreId) -> SnapshotSealer {
        SnapshotSealer {
            cipher: Box::new(XChaCha20Poly1305::new(<&Key>::from(key.as_bytes()))),
            store_id,
        }
    }

    /// The identifier of the store this sealer is for.
    pub(crate) fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Encrypt `buffer` in place under `nonce` with `associated_data`, and return the tag; `None`
    /// only for a buffer of 256 GiB or more, which the cipher refuses.
    pub(crate) fn seal_in_place(
        &self,
        nonce: &[u8; NONCE_LEN],
        associated_data: &[u8],
        buffer: &mut [u8],
    ) -> Option<[u8; TAG_LEN]> {
        self.cipher
            .encrypt_in_place_detached(<&XNonce>::from(nonce), associated_data, buffer)
            .ok()
            .map(Into::into)
    }

    /// Check `tag` over `associated_data` and the ciphertext in `buffer`, and only if it
    /// matches decrypt `buffer` in place. Returns whether it matched. `chacha20poly1305` checks
    /// the tag, in constant time, before it decrypts anything, so a buffer that fails is left
    /// as it was: ciphertext.
    pub(crate) fn open_in_place(
        &self,
        nonce: &[u8; NONCE_LEN],
        associated_data: &[u8],
        buffer: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> bool {
        self.cipher
            .decrypt_in_place_detached(
                <&XNonce>::from(nonce),
                associated_data,
                buffer,
                <&Tag>::from(tag),
            )
            .is_ok()
    }
}

impl fmt::Debug for SnapshotSealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotSealer")
            .field("store_id", &self.store_id)
            .finish_non_exhaustive()
    }
}

/// A one-shot failure of the random number generator, for the unit tests. Each test runs on
/// its own thread, so the fault is per thread.
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::Cell;

    thread_local! {
        static FAIL_NEXT: Cell<bool> = const { Cell::new(false) };
    }

    /// Make the next request for random bytes on this thread fail.
    pub(crate) fn fail_next_random() {
        FAIL_NEXT.with(|fail| fail.set(true));
    }

    /// Whether the request being made fails, clearing the fault.
    pub(super) fn take() -> bool {
        FAIL_NEXT.with(|fail| fail.replace(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    store_test! {
        fn test_seal_round_trip_in_place() {
            let sealer = SnapshotSealer::new(&DataKey::generate().unwrap(), StoreId::generate().unwrap());
            let nonce = random_nonce().unwrap();
            let mut buffer = b"a snapshot".to_vec();
            let tag = sealer.seal_in_place(&nonce, b"header", &mut buffer).unwrap();
            assert_ne!(buffer, b"a snapshot");
            // A failed open leaves the ciphertext as it was.
            let sealed = buffer.clone();
            assert!(!sealer.open_in_place(&nonce, b"other header", &mut buffer, &tag));
            assert_eq!(buffer, sealed);
            assert!(sealer.open_in_place(&nonce, b"header", &mut buffer, &tag));
            assert_eq!(buffer, b"a snapshot");
        }
    }

    store_test! {
        fn test_seal_keys_and_identifiers_are_random() {
            let [a, b] = [(), ()].map(|()| DataKey::generate().unwrap());
            assert_ne!(a.as_bytes(), b.as_bytes());
            assert_ne!(StoreId::generate().unwrap(), StoreId::generate().unwrap());
            assert_ne!(random_nonce().unwrap(), random_nonce().unwrap());
            // An unwrapped key round-trips through its bytes, and only 32 bytes make a key.
            assert_eq!(DataKey::from_bytes(a.as_bytes()).unwrap().as_bytes(), a.as_bytes());
            assert!(DataKey::from_bytes(&[0; KEY_LEN - 1]).is_none());
            assert!(DataKey::from_bytes(&[0; KEY_LEN + 1]).is_none());
            let id = StoreId::from_bytes([9; STORE_ID_LEN]);
            assert_eq!(id.to_bytes(), [9; STORE_ID_LEN]);
        }
    }

    store_test! {
        fn test_seal_randomness_failure_is_an_error() {
            fault::fail_next_random();
            assert_eq!(DataKey::generate().err().map(|_| ()), Some(()));
            fault::fail_next_random();
            assert_eq!(StoreId::generate(), Err(RandomnessUnavailable));
            fault::fail_next_random();
            assert_eq!(random_nonce(), Err(RandomnessUnavailable));
            // The fault is one-shot.
            assert!(random_nonce().is_ok());
        }
    }

    store_test! {
        fn test_seal_debug_shows_no_key_bytes() {
            let key = DataKey::from_bytes(&[0xab; KEY_LEN]).unwrap();
            assert_eq!(format!("{key:?}"), "DataKey(..)");
            let sealer = SnapshotSealer::new(&key, StoreId::from_bytes([1; STORE_ID_LEN]));
            let shown = format!("{sealer:?}");
            assert_eq!(
                shown,
                "SnapshotSealer { store_id: StoreId([1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, \
                 1, 1]), .. }"
            );
            assert!(!shown.contains("171"));
        }
    }
}
