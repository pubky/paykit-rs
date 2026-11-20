//! The bytes of one slot: sealing a whole state, checking, opening and decoding a slot, and
//! picking the current slot of the two.
//!
//! A slot holds one whole snapshot of the store, sealed:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0..8 | [`MAGIC`] |
//! | 8..12 | format version, `u32` little-endian ([`FORMAT_VERSION`]) |
//! | 12..20 | generation, `u64` little-endian: 1 for the first commit, one more for each next |
//! | 20..36 | store identifier: 16 random bytes made with the store |
//! | 36..60 | nonce: 24 random bytes, fresh for every write |
//! | 60..68 | sealed length in bytes, `u64` little-endian: the ciphertext and the tag, at least 16 and at most [`MAX_STATE_BYTES`] + 16 |
//! | 68.. | the payload encrypted with XChaCha20-Poly1305, then its 16-byte Poly1305 tag |
//!
//! The payload is every entry of the store, in ascending order of namespace and then key, each
//! as three parts: the namespace (UTF-8), the key and the value, each a `u32` little-endian
//! length and then its bytes. Namespaces and keys compare as bytes, the order of the in-memory
//! maps, so one state has exactly one encoding. Owners are not part of it: they live in memory
//! only, as in the Switch client's `MemoryStore`. Namespaces and keys are inside the sealed
//! payload; only the header is in the clear.
//!
//! The payload is sealed under the store's data key (`seal.rs`) with the whole header as
//! associated data: bytes 0..68 as written. The reader rebuilds that header with its own store
//! identifier, so a slot opens only under the store's key, for the store, the generation, the
//! format version and the length its header gives: a slot cannot be moved to another store or
//! another generation, or read under another format version, without failing its tag. The tag
//! detects a torn or damaged write, and a slot written without the key. It does not detect
//! rollback: anyone who can write the files can put back an older slot of the same store,
//! which opens.
//!
//! A slot is valid only if its size is exactly the header and the sealed length, and its tag
//! opens under the store's key. Trailing bytes make it invalid, so a slot rewritten over a
//! longer one is not valid until the file is cut to the new length.
//!
//! Every format version keeps the magic and the version in the first 12 bytes, so a build can
//! tell a slot written by a newer build from a damaged one. A slot whose version is not this
//! build's, but which opens under the store's key once the version is set back to this
//! build's, is a slot of this format with a damaged version field. Any other slot with a newer
//! version is of a newer format. Version 1 is the earlier unsealed format, which held the
//! payload in plaintext: a slot that gives version 1 is refused on its header alone
//! ([`Slot::UnsealedFormat`]) and nothing more of it is read, so a slot of this format whose
//! version field alone changed to 1 is refused that way too, rather than ignored.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use zeroize::Zeroizing;

use super::seal::{SnapshotSealer, StoreId, NONCE_LEN, STORE_ID_LEN, TAG_LEN};
use super::OpenError;

/// The store's contents: namespace, then key, then value.
pub(crate) type Entries = BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>;

/// The first eight bytes of every slot.
pub(crate) const MAGIC: [u8; 8] = *b"PKBSTORE";

/// The format version this build writes, and the newest it reads: sealed snapshots.
pub(crate) const FORMAT_VERSION: u32 = 2;

/// The format version of the earlier unsealed format, which this build refuses.
pub(crate) const UNSEALED_FORMAT_VERSION: u32 = 1;

/// Where the format version starts.
pub(crate) const VERSION_AT: usize = MAGIC.len();

/// Where the generation starts.
pub(crate) const GENERATION_AT: usize = VERSION_AT + 4;

/// Where the store identifier starts.
pub(crate) const STORE_ID_AT: usize = GENERATION_AT + 8;

/// Where the nonce starts.
pub(crate) const NONCE_AT: usize = STORE_ID_AT + STORE_ID_LEN;

/// Where the sealed length starts.
pub(crate) const SEALED_LEN_AT: usize = NONCE_AT + NONCE_LEN;

/// The length of the header: magic, format version, generation, store identifier, nonce and
/// sealed length. It is the associated data of the slot's seal.
pub(crate) const HEADER_LEN: usize = SEALED_LEN_AT + 8;

/// The magic and the format version: the part of the header every format version keeps.
const VERSIONED_PREFIX_LEN: usize = GENERATION_AT;

/// The largest encoded state, the payload of a slot: 256 MiB.
///
/// The whole state lives in WebAssembly memory, which is at most 4 GiB on wasm32 and often less
/// in a browser. A commit holds the current state, the next state and the next state's sealed
/// slot at once; opening holds one slot's bytes and both slots' decoded states. A decoded state
/// takes its bytes plus about a hundred bytes per entry for its maps and allocations, so with
/// [`MAX_ENTRIES`] a state at both caps takes about 400 MiB, and those peaks stay near 1.1 GiB.
/// Paykit's states are about a hundred kilobytes, and the largest batch the Switch client
/// commits is 32 MiB, so the cap leaves a wide margin.
pub(crate) const MAX_STATE_BYTES: u64 = 256 << 20;

/// The most entries a state holds: 2^20.
///
/// Without it, 256 MiB of tiny entries would decode to several GiB of maps, past what a
/// browser gives WebAssembly: a slot that opens under the store's key, written by anyone who
/// holds the key, could then exhaust memory on open. Paykit's states hold about a hundred
/// entries.
pub(crate) const MAX_ENTRIES: usize = 1 << 20;

/// The shortest sealed length: the tag of an empty payload.
const MIN_SEALED_LEN: u64 = TAG_LEN as u64;

/// The longest sealed length: the tag of a payload of [`MAX_STATE_BYTES`].
const MAX_SEALED_LEN: u64 = MAX_STATE_BYTES + TAG_LEN as u64;

/// The shortest complete slot.
const MIN_SLOT_LEN: u64 = HEADER_LEN as u64 + MIN_SEALED_LEN;

/// The longest complete slot.
const MAX_SLOT_LEN: u64 = HEADER_LEN as u64 + MAX_SEALED_LEN;

/// One of the two slot files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotId {
    /// The first file.
    A,
    /// The second file.
    B,
}

impl SlotId {
    /// The slot's index in the store's pair of files.
    pub(crate) fn index(self) -> usize {
        match self {
            SlotId::A => 0,
            SlotId::B => 1,
        }
    }

    /// The other slot: where the next commit goes when this one is current.
    pub(crate) fn other(self) -> SlotId {
        match self {
            SlotId::A => SlotId::B,
            SlotId::B => SlotId::A,
        }
    }
}

/// What one slot file's header shows without the store's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotHeader {
    /// The file is empty.
    Empty,
    /// The file is not empty, but does not start with the magic and a format version: a header
    /// cut short, or bytes that are not a slot.
    Unrecognized,
    /// The magic and format version 1: the earlier unsealed format.
    Unsealed,
    /// The magic and any other format version.
    Versioned {
        /// The format version: this build's, a newer one, or one no build writes.
        version: u32,
        /// The store identifier, when the version is this build's and the whole header is in
        /// the file.
        store_id: Option<StoreId>,
    },
}

impl SlotHeader {
    /// The store identifier of a complete header of this format.
    pub(crate) fn store_id(self) -> Option<StoreId> {
        match self {
            SlotHeader::Versioned { store_id, .. } => store_id,
            _ => None,
        }
    }
}

/// What the store's two files show without its key: enough to decide how to find the key
/// before opening the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inspection {
    /// Both files are empty: no commit ever wrote a byte.
    Empty,
    /// A file starts as a slot of the earlier unsealed format, which this build refuses.
    UnsealedFormat,
    /// At least one file is not empty, and neither is of the unsealed format: what each
    /// header shows.
    Present([SlotHeader; 2]),
}

impl Inspection {
    /// The inspection of two files whose headers show `headers`.
    pub(crate) fn of(headers: [SlotHeader; 2]) -> Inspection {
        if headers.contains(&SlotHeader::Unsealed) {
            Inspection::UnsealedFormat
        } else if headers == [SlotHeader::Empty; 2] {
            Inspection::Empty
        } else {
            Inspection::Present(headers)
        }
    }

    /// The newest format version newer than this build's that a header gives, if any. Without
    /// the key, such a slot cannot be told from a slot of this format whose version field
    /// changed, so it counts as a newer format.
    pub(crate) fn newer_version(&self) -> Option<u32> {
        let Inspection::Present(headers) = self else {
            return None;
        };
        headers
            .iter()
            .filter_map(|header| match header {
                SlotHeader::Versioned { version, .. } if *version > FORMAT_VERSION => {
                    Some(*version)
                }
                _ => None,
            })
            .max()
    }

    /// The error for files whose key record is missing, malformed, of the other mode or does
    /// not unwrap: [`OpenError::NewerFormat`] when a header gives a newer format, as a newer
    /// build may keep its key differently; [`OpenError::Corrupt`] when files are present but
    /// none holds a complete header of this format, as no sealed data is there to lose (bytes
    /// of no format, or a header cut short); and [`OpenError::KeyLost`] otherwise.
    pub(crate) fn key_lost_error(&self) -> OpenError {
        if let Some(version) = self.newer_version() {
            return OpenError::NewerFormat { version };
        }
        match self {
            Inspection::Present(headers)
                if headers.iter().all(|header| header.store_id().is_none()) =>
            {
                OpenError::Corrupt
            }
            _ => OpenError::KeyLost,
        }
    }
}

/// What the header in `prefix`, the first bytes of a file of `file_size` bytes, shows without
/// a key. `prefix` holds at least [`HEADER_LEN`] bytes when the file has that many.
pub(crate) fn inspect_header(prefix: &[u8], file_size: u64) -> SlotHeader {
    if file_size == 0 {
        return SlotHeader::Empty;
    }
    let (Some(magic), Some(version)) = (prefix.get(..MAGIC.len()), read_u32(prefix, VERSION_AT))
    else {
        return SlotHeader::Unrecognized;
    };
    if magic != MAGIC {
        return SlotHeader::Unrecognized;
    }
    if version == UNSEALED_FORMAT_VERSION {
        return SlotHeader::Unsealed;
    }
    let store_id = if version == FORMAT_VERSION && prefix.len() >= HEADER_LEN {
        read_store_id(prefix)
    } else {
        None
    };
    SlotHeader::Versioned { version, store_id }
}

/// Check a slot's header against its file's size, before the rest of the slot is read.
///
/// `prefix` holds the file's first bytes: at least [`HEADER_LEN`] of them when the file has
/// that many. Every length is checked against the file size and the caps here, before the
/// caller reads or allocates anything for the rest of the slot.
///
/// Returns the slot when the header alone decides it: [`Slot::Empty`], [`Slot::Torn`] or
/// [`Slot::TornFirstCommit`] (a header cut short or without the magic, a sealed length under
/// the tag's or over the cap, or a size that is not exactly the header and the sealed length),
/// [`Slot::UnsealedFormat`] or [`Slot::NewerFormat`]. Returns `None` when [`decode_slot`] must
/// check the whole file, which is then at most a header and [`MAX_STATE_BYTES`] + 16 bytes: when
/// the header is consistent with the file size in this format's layout, whatever its version;
/// or when the file starts as a first commit does but its size does not match its sealed
/// length, which a complete first commit with a changed length field also gives.
pub(crate) fn read_header(prefix: &[u8], file_size: u64) -> Option<Slot> {
    let version = match inspect_header(prefix, file_size) {
        SlotHeader::Empty => return Some(Slot::Empty),
        // A header cut short is torn; one without the magic is never a first commit's.
        SlotHeader::Unrecognized => return Some(torn(prefix)),
        // Nothing more of an unsealed slot is read: it is refused, never read or upgraded.
        SlotHeader::Unsealed => return Some(Slot::UnsealedFormat),
        SlotHeader::Versioned { version, .. } => version,
    };
    // A newer format's header may be laid out differently, so a newer version that does not fit
    // this format's layout is a newer format. One that fits is decided by `decode_slot`.
    let newer = version > FORMAT_VERSION;
    let sealed_len = read_u64(prefix, SEALED_LEN_AT);
    // The sum cannot overflow: the sealed length is at most its cap.
    let fits = sealed_len.is_some_and(|len| {
        (MIN_SEALED_LEN..=MAX_SEALED_LEN).contains(&len) && file_size == HEADER_LEN as u64 + len
    });
    if fits {
        return None;
    }
    if newer {
        return Some(Slot::NewerFormat(version));
    }
    let could_be_complete = (MIN_SLOT_LEN..=MAX_SLOT_LEN).contains(&file_size);
    if sealed_len.is_some() && could_be_complete && starts_like_first_commit(prefix) {
        return None;
    }
    Some(torn(prefix))
}

/// A torn slot: [`Slot::TornFirstCommit`] if its first bytes are those of a first commit, as
/// far as they go, and [`Slot::Torn`] otherwise.
fn torn(prefix: &[u8]) -> Slot {
    if starts_like_first_commit(prefix) {
        Slot::TornFirstCommit
    } else {
        Slot::Torn
    }
}

/// Whether `prefix` starts as every first commit of this build does: the magic, this format
/// version and generation 1, as far as `prefix` reaches. A first commit is written into an
/// empty file, or over an earlier first commit that stopped part way, so a first commit cut
/// short at any point passes this.
fn starts_like_first_commit(prefix: &[u8]) -> bool {
    let mut first = [0u8; VERSIONED_PREFIX_LEN + 8];
    first[..MAGIC.len()].copy_from_slice(&MAGIC);
    first[VERSION_AT..GENERATION_AT].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    first[GENERATION_AT..].copy_from_slice(&1u64.to_le_bytes());
    let len = prefix.len().min(first.len());
    prefix[..len] == first[..len]
}

/// One slot as read at open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// The file is empty.
    Empty,
    /// A slot a write that stopped part way can leave: a header cut short, without the magic or
    /// with a sealed length out of bounds, or a size other than the one the header gives (a
    /// write over a longer slot leaves the old slot's last bytes until the file is cut). The
    /// next commit overwrites it.
    Torn,
    /// A torn slot whose first bytes are those of a first commit: the magic, this format
    /// version and generation 1, as far as the file reaches. Next to an empty file, it is the
    /// only torn slot a first commit that stopped part way can leave.
    TornFirstCommit,
    /// A complete slot of this format that does not open under the store's key: exactly as long
    /// as its header says, with a tag that does not match, or that matches only with the format
    /// version set back to this build's; or a slot that starts as a first commit and whose tag
    /// matches only with its sealed length set to the file's. Most likely bytes that changed
    /// after a complete write, or a slot not sealed for this store.
    Damaged,
    /// A slot of the earlier unsealed format. The store refuses to open, and never reads,
    /// upgrades or overwrites it.
    UnsealedFormat,
    /// A slot of a newer format version than this build reads.
    NewerFormat(u32),
    /// A slot that opens under the store's key but whose payload this build cannot decode. A
    /// torn write cannot produce one, and no writer of this format does: the store refuses to
    /// open.
    Malformed,
    /// A valid slot.
    Valid {
        /// The slot's generation.
        generation: u64,
        /// The state it holds.
        entries: Entries,
    },
}

/// Check, open and decode a whole slot with the store's sealer.
///
/// At most one decryption is tried, in place. The buffer is wiped when this returns, so the
/// plaintext of a slot that opens is gone once it is decoded.
pub(crate) fn decode_slot(sealer: &SnapshotSealer, bytes: Vec<u8>) -> Slot {
    let mut bytes = Zeroizing::new(bytes);
    let file_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if let Some(slot) = read_header(&bytes, file_size) {
        return slot;
    }
    // `read_header` checked that the size is at least a header and a tag.
    let Some((header, sealed)) = bytes.split_at_mut_checked(HEADER_LEN) else {
        return Slot::Torn;
    };
    let (Some(version), Some(generation), Some(nonce), Some(sealed_len)) = (
        read_u32(header, VERSION_AT),
        read_u64(header, GENERATION_AT),
        header
            .get(NONCE_AT..SEALED_LEN_AT)
            .and_then(|nonce| <[u8; NONCE_LEN]>::try_from(nonce).ok()),
        read_u64(header, SEALED_LEN_AT),
    ) else {
        return Slot::Torn;
    };
    let file_sealed_len = u64::try_from(sealed.len()).unwrap_or(u64::MAX);
    let Some((ciphertext, tag)) = sealed.split_last_chunk_mut::<TAG_LEN>() else {
        return Slot::Torn;
    };
    let tag = *tag;
    // A slot that names another store is not this store's, whatever its tag; a newer format
    // may keep its store identifier elsewhere, so a newer version stays a newer format. (Store
    // identifiers are not secret, so a plain comparison is fine.) With that, the associated
    // data rebuilt below with the reader's store identifier is the header as written, but for
    // the version or length field the two rules below set back.
    if read_store_id(header) != Some(sealer.store_id()) {
        return if version > FORMAT_VERSION {
            Slot::NewerFormat(version)
        } else {
            Slot::Damaged
        };
    }
    let open = |version: u32, sealed_len: u64, ciphertext: &mut [u8]| {
        let header = header_bytes(version, generation, sealer.store_id(), &nonce, sealed_len);
        sealer.open_in_place(&nonce, &header, ciphertext, &tag)
    };
    if sealed_len != file_sealed_len {
        // `read_header` lets this through only for a slot that starts as a first commit. With
        // nothing else in the store, a changed length field must not pass for a torn first
        // commit, which would open a new store over the committed state.
        return if open(FORMAT_VERSION, file_sealed_len, ciphertext) {
            Slot::Damaged
        } else {
            Slot::TornFirstCommit
        };
    }
    if version != FORMAT_VERSION {
        // A later format must keep its version under its tag, so that none of its slots passes
        // this check by chance.
        return if open(FORMAT_VERSION, sealed_len, ciphertext) {
            Slot::Damaged
        } else if version > FORMAT_VERSION {
            Slot::NewerFormat(version)
        } else {
            Slot::Torn
        };
    }
    if !open(FORMAT_VERSION, sealed_len, ciphertext) {
        return Slot::Damaged;
    }
    match decode_entries(ciphertext) {
        Some(entries) => Slot::Valid {
            generation,
            entries,
        },
        None => Slot::Malformed,
    }
}

/// The header of a slot with these fields: the bytes written before the ciphertext, and the
/// associated data of the seal.
fn header_bytes(
    version: u32,
    generation: u64,
    store_id: StoreId,
    nonce: &[u8; NONCE_LEN],
    sealed_len: u64,
) -> [u8; HEADER_LEN] {
    let mut header = [0; HEADER_LEN];
    header[..VERSION_AT].copy_from_slice(&MAGIC);
    header[VERSION_AT..GENERATION_AT].copy_from_slice(&version.to_le_bytes());
    header[GENERATION_AT..STORE_ID_AT].copy_from_slice(&generation.to_le_bytes());
    header[STORE_ID_AT..NONCE_AT].copy_from_slice(&store_id.to_bytes());
    header[NONCE_AT..SEALED_LEN_AT].copy_from_slice(nonce);
    header[SEALED_LEN_AT..].copy_from_slice(&sealed_len.to_le_bytes());
    header
}

/// The length of `entries`' encoded payload, saturating at `u64::MAX`.
fn payload_len(entries: &Entries) -> u64 {
    let len = |bytes: usize| u64::try_from(bytes).unwrap_or(u64::MAX);
    entries
        .iter()
        .flat_map(|(ns, keys)| keys.iter().map(move |(key, value)| (ns, key, value)))
        .fold(0u64, |total, (ns, key, value)| {
            total
                .saturating_add(3 * 4)
                .saturating_add(len(ns.len()))
                .saturating_add(len(key.len()))
                .saturating_add(len(value.len()))
        })
}

/// The slot that holds `entries` at `generation`, sealed by `sealer` under `nonce`, or `None`
/// if the encoded state would be over [`MAX_STATE_BYTES`] or hold more than [`MAX_ENTRIES`]
/// entries. The size is checked before anything is allocated. The payload is encoded straight
/// into the one buffer that holds the slot, whose capacity is exact so it never moves, and is
/// encrypted there in place: no copy of the plaintext is left behind in the buffer.
///
/// The caller passes a fresh random nonce for every call (see `SlotStore`'s commit).
pub(crate) fn encode_slot(
    sealer: &SnapshotSealer,
    generation: u64,
    nonce: &[u8; NONCE_LEN],
    entries: &Entries,
) -> Option<Vec<u8>> {
    let len = payload_len(entries);
    if len > MAX_STATE_BYTES || entries.values().map(BTreeMap::len).sum::<usize>() > MAX_ENTRIES {
        return None;
    }
    // At most the cap, which fits in wasm32's `usize`.
    let payload_size = usize::try_from(len).ok()?;
    // At most the cap and a tag: no overflow.
    let header = header_bytes(
        FORMAT_VERSION,
        generation,
        sealer.store_id(),
        nonce,
        len + MIN_SEALED_LEN,
    );
    // Wiped if this returns early, while it still holds plaintext.
    let mut slot = Zeroizing::new(Vec::with_capacity(HEADER_LEN + payload_size + TAG_LEN));
    slot.extend_from_slice(&header);
    // Every part is shorter than the payload, so its length fits in a `u32`.
    for (ns, keys) in entries {
        for (key, value) in keys {
            for part in [ns.as_bytes(), key, value] {
                slot.extend_from_slice(&u32::try_from(part.len()).ok()?.to_le_bytes());
                slot.extend_from_slice(part);
            }
        }
    }
    debug_assert_eq!(slot.len(), HEADER_LEN + payload_size);
    // The cipher refuses only a payload of 256 GiB or more, far over the cap.
    let tag = sealer.seal_in_place(nonce, &header, slot.get_mut(HEADER_LEN..)?)?;
    slot.extend_from_slice(&tag);
    // Sealed: the buffer leaves the wrapper whole, without a copy.
    Some(std::mem::take(&mut *slot))
}

/// A slot of format `version` holding any `payload` at `generation`, sealed by `sealer` with
/// its header as written, for tests that need a slot that opens but whose payload is not a
/// state's encoding, or a newer format's slot in this layout with its version under its tag.
#[cfg(test)]
pub(crate) fn seal_raw(
    sealer: &SnapshotSealer,
    version: u32,
    generation: u64,
    payload: &[u8],
) -> Vec<u8> {
    let nonce = super::seal::random_nonce().unwrap();
    let sealed_len = u64::try_from(payload.len() + TAG_LEN).unwrap();
    let header = header_bytes(version, generation, sealer.store_id(), &nonce, sealed_len);
    let mut slot = header.to_vec();
    slot.extend_from_slice(payload);
    let tag = sealer
        .seal_in_place(&nonce, &header, &mut slot[HEADER_LEN..])
        .unwrap();
    slot.extend_from_slice(&tag);
    slot
}

/// Decode a payload, or `None` if it is not the one encoding of a state: a part that runs past
/// the end, a namespace that is not UTF-8, entries out of order or repeated, bytes left over
/// after the last entry, or more than [`MAX_ENTRIES`] entries, which are counted before any
/// entry is decoded.
fn decode_entries(mut rest: &[u8]) -> Option<Entries> {
    let mut entries = Entries::new();
    let mut previous: Option<(&str, &[u8])> = None;
    // The keys of the namespace being read. Entries arrive sorted, so a namespace's keys are
    // contiguous: they are moved into `entries` once, when the next namespace starts.
    let mut keys = BTreeMap::new();
    if entry_count(rest)? > MAX_ENTRIES {
        return None;
    }
    while !rest.is_empty() {
        let ns = std::str::from_utf8(take_part(&mut rest)?).ok()?;
        let key = take_part(&mut rest)?;
        let value = take_part(&mut rest)?;
        // Strictly ascending: this refuses a repeated entry as well as one out of order.
        if previous.is_some_and(|previous| (ns, key) <= previous) {
            return None;
        }
        if let Some((done, _)) = previous.filter(|(previous_ns, _)| *previous_ns != ns) {
            entries.insert(done.to_owned(), std::mem::take(&mut keys));
        }
        previous = Some((ns, key));
        keys.insert(key.to_vec(), value.to_vec());
    }
    if let Some((last, _)) = previous {
        entries.insert(last.to_owned(), keys);
    }
    Some(entries)
}

/// The number of entries in a payload, counted without allocating, up to one past
/// [`MAX_ENTRIES`]; `None` if a part runs past the end.
fn entry_count(mut rest: &[u8]) -> Option<usize> {
    let mut count = 0;
    while !rest.is_empty() && count <= MAX_ENTRIES {
        for _ in 0..3 {
            take_part(&mut rest)?;
        }
        count += 1;
    }
    Some(count)
}

/// Take one length-prefixed part off the front of `rest`, checking its length against what is
/// left first.
fn take_part<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = usize::try_from(read_u32(rest, 0)?).ok()?;
    let part = rest.get(4..)?.get(..len)?;
    *rest = rest.get(4 + len..)?;
    Some(part)
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(field.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let field = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(field.try_into().ok()?))
}

fn read_store_id(bytes: &[u8]) -> Option<StoreId> {
    let field = bytes.get(STORE_ID_AT..NONCE_AT)?;
    Some(StoreId::from_bytes(field.try_into().ok()?))
}

/// The state the store opens with.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Current {
    /// The slot that holds it, or `None` for a new store.
    pub(crate) slot: Option<SlotId>,
    /// Its generation; 0 for a new store.
    pub(crate) generation: u64,
    /// The state.
    pub(crate) entries: Entries,
}

/// Pick the current state from the two slots.
///
/// - A slot of the earlier unsealed format makes the store refuse to open, whatever the other
///   slot holds: it is never read, upgraded or overwritten.
/// - A slot of a newer format makes the store refuse to open, even when the other slot is
///   valid: a downgrade would overwrite state it cannot read.
/// - A malformed slot, or two valid slots of the same generation, which no writer produces,
///   make the store corrupt.
/// - Otherwise the valid slot with the higher generation is current, and a torn or damaged slot
///   next to it is ignored: it is an interrupted write, or damage the next commit overwrites.
/// - Two torn or damaged slots are corrupt: nothing is repaired by discarding data.
/// - Two empty slots, or one empty slot and one torn first commit
///   ([`Slot::TornFirstCommit`]), are a new store: no commit ever completed, since a completed
///   first commit leaves a valid slot, and a first commit that stopped part way leaves a slot
///   that starts as a first commit does.
/// - One empty slot and one damaged slot, or a torn slot that does not start as a first commit,
///   are corrupt: a complete first commit whose bytes changed later. A first commit cut off by
///   a power loss in its flush, with its full size on disk but not all of its bytes, reads this
///   way too, and is refused rather than discarded.
///
/// A damaged slot next to a valid one is ignored like a torn one, so damage to the current slot
/// after the fact opens the store at the other slot's older state.
pub(crate) fn pick_current(a: Slot, b: Slot) -> Result<Current, OpenError> {
    if matches!(a, Slot::UnsealedFormat) || matches!(b, Slot::UnsealedFormat) {
        return Err(OpenError::UnsealedFormat);
    }
    let newer = [&a, &b]
        .into_iter()
        .filter_map(|slot| match slot {
            Slot::NewerFormat(version) => Some(*version),
            _ => None,
        })
        .max();
    if let Some(version) = newer {
        return Err(OpenError::NewerFormat { version });
    }
    let malformed = matches!(a, Slot::Malformed) || matches!(b, Slot::Malformed);
    let unreadable = matches!(
        (&a, &b),
        (
            Slot::Torn | Slot::TornFirstCommit | Slot::Damaged,
            Slot::Torn | Slot::TornFirstCommit | Slot::Damaged
        ) | (Slot::Empty, Slot::Torn | Slot::Damaged)
            | (Slot::Torn | Slot::Damaged, Slot::Empty)
    );
    if malformed || unreadable {
        return Err(OpenError::Corrupt);
    }
    let (slot, (generation, entries)) = match (valid(a), valid(b)) {
        (Some(a), Some(b)) => match a.0.cmp(&b.0) {
            Ordering::Greater => (SlotId::A, a),
            Ordering::Less => (SlotId::B, b),
            Ordering::Equal => return Err(OpenError::Corrupt),
        },
        (Some(a), None) => (SlotId::A, a),
        (None, Some(b)) => (SlotId::B, b),
        // Two empty slots, or one empty slot and one torn first commit.
        (None, None) => {
            return Ok(Current {
                slot: None,
                generation: 0,
                entries: Entries::new(),
            })
        }
    };
    Ok(Current {
        slot: Some(slot),
        generation,
        entries,
    })
}

/// A valid slot's generation and state; `None` for any other slot.
fn valid(slot: Slot) -> Option<(u64, Entries)> {
    match slot {
        Slot::Valid {
            generation,
            entries,
        } => Some((generation, entries)),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::seal::{random_nonce, DataKey, KEY_LEN};
    use super::*;

    /// The store identifier of the tests' store.
    pub(crate) const TEST_STORE: StoreId = StoreId::from_bytes([0x5a; STORE_ID_LEN]);

    /// The data key of the tests' store.
    pub(crate) fn test_key() -> DataKey {
        DataKey::from_bytes(&[0x42; KEY_LEN]).unwrap()
    }

    /// The sealer of the tests' store.
    pub(crate) fn sealer() -> SnapshotSealer {
        SnapshotSealer::new(&test_key(), TEST_STORE)
    }

    /// A slot of the tests' store holding `entries` at `generation`, under a fresh nonce.
    pub(crate) fn encode(generation: u64, entries: &Entries) -> Option<Vec<u8>> {
        encode_slot(&sealer(), generation, &random_nonce().unwrap(), entries)
    }

    /// `bytes` decoded as a slot of the tests' store.
    pub(crate) fn decode(bytes: &[u8]) -> Slot {
        decode_slot(&sealer(), bytes.to_vec())
    }

    /// The first commit of an earlier build in the unsealed format, built by hand: the magic,
    /// version 1, generation, payload length, the payload and a 32-byte tag.
    pub(crate) fn unsealed_slot(generation: u64, payload: &[u8]) -> Vec<u8> {
        let mut slot = MAGIC.to_vec();
        slot.extend_from_slice(&UNSEALED_FORMAT_VERSION.to_le_bytes());
        slot.extend_from_slice(&generation.to_le_bytes());
        slot.extend_from_slice(&u64::try_from(payload.len()).unwrap().to_le_bytes());
        slot.extend_from_slice(payload);
        slot.extend_from_slice(&[0x77; 32]);
        slot
    }

    fn entries(items: &[(&str, &[u8], &[u8])]) -> Entries {
        let mut entries = Entries::new();
        for (ns, key, value) in items {
            entries
                .entry((*ns).to_owned())
                .or_default()
                .insert(key.to_vec(), value.to_vec());
        }
        entries
    }

    fn part(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_le_bytes());
        out.extend_from_slice(bytes);
    }

    fn payload(items: &[(&[u8], &[u8], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (ns, key, value) in items {
            part(&mut out, ns);
            part(&mut out, key);
            part(&mut out, value);
        }
        out
    }

    store_test! {
        fn test_format_round_trip() {
            let state = entries(&[
                ("client/a", b"", b"empty key"),
                ("client/a", b"k", b""),
                ("client/a", b"k/1", &[0, 255, 7]),
                ("signer", b"k", b"v"),
                ("", b"k", b"empty namespace"),
            ]);
            let slot = encode(9, &state).unwrap();
            assert_eq!(&slot[..8], &MAGIC);
            assert_eq!(
                decode(&slot),
                Slot::Valid {
                    generation: 9,
                    entries: state.clone()
                }
            );
            // One state has one encoding, sealed under each write's own nonce.
            let nonce = random_nonce().unwrap();
            let once = encode_slot(&sealer(), 9, &nonce, &state).unwrap();
            assert_eq!(encode_slot(&sealer(), 9, &nonce, &state.clone()).unwrap(), once);
            assert_eq!(once.len(), slot.len());
            assert_ne!(once, slot);
            // An empty state is a valid slot of only a header and a tag.
            let empty = encode(1, &Entries::new()).unwrap();
            assert_eq!(empty.len(), HEADER_LEN + TAG_LEN);
            assert_eq!(
                decode(&empty),
                Slot::Valid {
                    generation: 1,
                    entries: Entries::new()
                }
            );
        }
    }

    store_test! {
        fn test_format_header_layout() {
            let state = entries(&[("ns", b"k", b"value")]);
            let payload_len = payload(&[(b"ns", b"k", b"value")]).len();
            let nonce = random_nonce().unwrap();
            let slot = encode_slot(&sealer(), 7, &nonce, &state).unwrap();
            assert_eq!(HEADER_LEN, 68);
            assert_eq!(slot.len(), HEADER_LEN + payload_len + TAG_LEN);
            assert_eq!(&slot[..8], b"PKBSTORE");
            assert_eq!(slot[8..12], 2u32.to_le_bytes());
            assert_eq!(slot[12..20], 7u64.to_le_bytes());
            assert_eq!(slot[20..36], TEST_STORE.to_bytes());
            assert_eq!(slot[36..60], nonce);
            assert_eq!(slot[60..68], u64::try_from(payload_len + TAG_LEN).unwrap().to_le_bytes());
            // The payload is not in the clear.
            assert_ne!(slot[HEADER_LEN..HEADER_LEN + payload_len], payload(&[(b"ns", b"k", b"value")]));
            assert_eq!(
                inspect_header(&slot[..HEADER_LEN], u64::try_from(slot.len()).unwrap()),
                SlotHeader::Versioned { version: 2, store_id: Some(TEST_STORE) }
            );
        }
    }

    store_test! {
        fn test_format_decode_refuses_a_payload_that_is_not_canonical() {
            // Each payload is sealed with a matching tag, so only the payload is at fault.
            let cases: [(&str, Vec<u8>); 7] = [
                ("valid", payload(&[(b"a", b"1", b"x"), (b"a", b"2", b"y"), (b"b", b"1", b"z")])),
                ("non-UTF-8 namespace", payload(&[(&[0xff, 0xfe], b"k", b"v")])),
                ("keys out of order", payload(&[(b"a", b"2", b"x"), (b"a", b"1", b"y")])),
                ("repeated key", payload(&[(b"a", b"1", b"x"), (b"a", b"1", b"y")])),
                ("namespaces out of order", payload(&[(b"b", b"1", b"x"), (b"a", b"1", b"y")])),
                ("trailing bytes", {
                    let mut bytes = payload(&[(b"a", b"1", b"x")]);
                    bytes.extend_from_slice(&[0, 0]);
                    bytes
                }),
                ("part past the end", {
                    let mut bytes = payload(&[(b"a", b"1", b"x")]);
                    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
                    bytes
                }),
            ];
            for (case, bytes) in cases {
                let decoded = decode(&seal_raw(&sealer(), FORMAT_VERSION, 3, &bytes));
                if case == "valid" {
                    assert!(matches!(decoded, Slot::Valid { generation: 3, .. }), "{case}");
                } else {
                    assert_eq!(decoded, Slot::Malformed, "{case}");
                }
            }
        }
    }

    store_test! {
        fn test_format_decode_refuses_too_many_entries_before_decoding() {
            // One more entry than the cap, each with an empty namespace and value and a distinct
            // three-byte key in ascending order: a canonical payload but for its count.
            let mut payload = Vec::with_capacity((MAX_ENTRIES + 1) * 15);
            for index in 0..=MAX_ENTRIES {
                let key = u32::try_from(index).unwrap().to_be_bytes();
                part(&mut payload, b"");
                part(&mut payload, &key[1..]);
                part(&mut payload, b"");
            }
            assert_eq!(entry_count(&payload), Some(MAX_ENTRIES + 1));
            assert_eq!(decode_entries(&payload), None);
            assert_eq!(decode(&seal_raw(&sealer(), FORMAT_VERSION, 1, &payload)), Slot::Malformed);
            // The same entries, one fewer, are counted in full.
            assert_eq!(entry_count(&payload[..MAX_ENTRIES * 15]), Some(MAX_ENTRIES));
        }
    }

    // Natively only: a state at the cap takes over a hundred megabytes of maps.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn test_format_encode_refuses_too_many_entries() {
        let mut keys = BTreeMap::new();
        for index in 0..=MAX_ENTRIES {
            keys.insert(
                u32::try_from(index).unwrap().to_be_bytes().to_vec(),
                Vec::new(),
            );
        }
        let mut state = Entries::new();
        state.insert("ns".to_owned(), keys);
        assert_eq!(encode(1, &state), None);
        state.get_mut("ns").unwrap().pop_first();
        assert!(encode(1, &state).is_some());
    }

    store_test! {
        fn test_format_header_refuses_lengths_that_do_not_fit() {
            let slot = encode(2, &entries(&[("ns", b"k", b"v")])).unwrap();
            let size = u64::try_from(slot.len()).unwrap();
            assert_eq!(read_header(&slot, size), None);
            assert_eq!(read_header(&slot, size + 1), Some(Slot::Torn));
            assert_eq!(read_header(&slot, size - 1), Some(Slot::Torn));
            assert_eq!(read_header(&slot[..4], 4), Some(Slot::TornFirstCommit));
            assert_eq!(read_header(b"PKBX", 4), Some(Slot::Torn));
            assert_eq!(read_header(&[], 0), Some(Slot::Empty));
            // A sealed length of zero, shorter than a tag, larger than the file, larger than the
            // cap, and the largest.
            for sealed_len in [0, MIN_SEALED_LEN - 1, size, MAX_SEALED_LEN + 1, u64::MAX] {
                let mut header = slot[..HEADER_LEN].to_vec();
                header[SEALED_LEN_AT..].copy_from_slice(&sealed_len.to_le_bytes());
                assert_eq!(read_header(&header, size), Some(Slot::Torn), "{sealed_len}");
            }
            // Out of bounds, the header is torn even for a file of the size it asks for.
            for sealed_len in [0, MIN_SEALED_LEN - 1, MAX_SEALED_LEN + 1, u64::MAX - 100] {
                let mut header = slot[..HEADER_LEN].to_vec();
                header[SEALED_LEN_AT..].copy_from_slice(&sealed_len.to_le_bytes());
                let claimed = sealed_len + HEADER_LEN as u64;
                assert_eq!(read_header(&header, claimed), Some(Slot::Torn), "{sealed_len}");
            }
            // A newer version with a header shorter than this format's is a newer format.
            let mut newer = slot[..VERSIONED_PREFIX_LEN].to_vec();
            newer[8..12].copy_from_slice(&3u32.to_le_bytes());
            assert_eq!(read_header(&newer, 12), Some(Slot::NewerFormat(3)));
        }
    }

    store_test! {
        fn test_format_complete_slot_with_a_wrong_tag_is_damaged() {
            let slot = encode(1, &entries(&[("ns", b"k", b"v")])).unwrap();
            let len = slot.len();
            // A changed byte in the magic is torn: nothing says how long the slot should be. It
            // is not a torn first commit either, which always starts with the magic.
            let mut magic = slot.clone();
            magic[0] ^= 0x01;
            assert_eq!(decode(&magic), Slot::Torn);
            // A changed byte in the generation, the store identifier, the nonce, the ciphertext
            // or the tag, at the full length.
            for at in [14, STORE_ID_AT, NONCE_AT + 5, HEADER_LEN, HEADER_LEN + 2, len - TAG_LEN - 1, len - TAG_LEN, len - 1] {
                let mut damaged = slot.clone();
                damaged[at] ^= 0x01;
                assert_eq!(decode(&damaged), Slot::Damaged, "byte {at}");
            }
            // A changed byte in the sealed length of a first commit, to a length smaller or
            // larger than the file, or over the cap: its tag matches with the length set back
            // to the file's.
            for at in [SEALED_LEN_AT, SEALED_LEN_AT + 1, HEADER_LEN - 1] {
                let mut damaged = slot.clone();
                damaged[at] ^= 0x01;
                assert_eq!(decode(&damaged), Slot::Damaged, "byte {at}");
            }
            // Cut short at any point, the slot is a torn first commit.
            for cut in [1, 11, HEADER_LEN - 1, HEADER_LEN, len - TAG_LEN, len - 1] {
                assert_eq!(decode(&slot[..cut]), Slot::TornFirstCommit, "cut {cut}");
            }
            // So is a first commit written whole over a longer one that stopped part way, before
            // the file was cut.
            let longer = encode(1, &entries(&[("ns", b"k", b"a longer value")])).unwrap();
            let mut over = longer[..longer.len() - 1].to_vec();
            over[..len].copy_from_slice(&slot);
            assert_eq!(decode(&over), Slot::TornFirstCommit);
            // A later slot cut short is torn, and so is one whose first bytes are not a first
            // commit's.
            let later = encode(2, &entries(&[("ns", b"k", b"v")])).unwrap();
            assert_eq!(decode(&later[..len - 1]), Slot::Torn);
            assert_eq!(decode(&later[..HEADER_LEN - 1]), Slot::Torn);
        }
    }

    store_test! {
        fn test_format_every_changed_byte_is_refused() {
            // Every bit of every byte of a first commit and of a later slot, flipped: the magic,
            // version, generation, store identifier, nonce, sealed length, ciphertext and tag.
            // The slot is torn or damaged, never valid, and decoding never panics. (No single
            // bit turns version 2 into version 1, the unsealed format.)
            for generation in [1, 2] {
                let slot = encode(generation, &entries(&[("ns", b"k", b"value")])).unwrap();
                for at in 0..slot.len() {
                    for bit in 0..8 {
                        let mut changed = slot.clone();
                        changed[at] ^= 1 << bit;
                        let decoded = decode(&changed);
                        assert!(
                            matches!(decoded, Slot::Torn | Slot::Damaged),
                            "generation {generation}, byte {at}, bit {bit}: {decoded:?}"
                        );
                    }
                }
            }
        }
    }

    store_test! {
        fn test_format_sealed_blob_is_bound_to_its_header_and_store() {
            let state = entries(&[("ns", b"k", b"v")]);
            let second = encode(2, &state).unwrap();
            let third = encode(3, &state).unwrap();
            // The nonce, ciphertext and tag of generation 2 under generation 3's header.
            let mut moved = third[..NONCE_AT].to_vec();
            moved.extend_from_slice(&second[NONCE_AT..]);
            assert_eq!(decode(&moved), Slot::Damaged);
            // A slot of another store, under the same key and under another key.
            let same_key = SnapshotSealer::new(&test_key(), StoreId::from_bytes([1; STORE_ID_LEN]));
            let other_key = SnapshotSealer::new(&DataKey::generate().unwrap(), StoreId::from_bytes([1; STORE_ID_LEN]));
            for other in [&same_key, &other_key] {
                let slot = encode_slot(other, 2, &random_nonce().unwrap(), &state).unwrap();
                assert!(matches!(decode_slot(other, slot.clone()), Slot::Valid { generation: 2, .. }));
                assert_eq!(decode(&slot), Slot::Damaged);
                // With its store identifier rewritten to this store's, it still does not open:
                // it was sealed with the other identifier in its associated data.
                let mut renamed = slot;
                renamed[STORE_ID_AT..NONCE_AT].copy_from_slice(&TEST_STORE.to_bytes());
                assert_eq!(decode(&renamed), Slot::Damaged);
            }
            // A slot of this store under another key.
            let wrong_key = SnapshotSealer::new(&DataKey::generate().unwrap(), TEST_STORE);
            let slot = encode_slot(&wrong_key, 2, &random_nonce().unwrap(), &state).unwrap();
            assert_eq!(decode(&slot), Slot::Damaged);
        }
    }

    store_test! {
        fn test_format_unsealed_slot_is_refused_by_its_header() {
            let unsealed = unsealed_slot(1, &payload(&[(b"ns", b"k", b"v")]));
            let size = u64::try_from(unsealed.len()).unwrap();
            assert_eq!(read_header(&unsealed[..HEADER_LEN.min(unsealed.len())], size), Some(Slot::UnsealedFormat));
            assert_eq!(decode(&unsealed), Slot::UnsealedFormat);
            // Only the magic and the version are needed.
            assert_eq!(read_header(&unsealed[..12], 12), Some(Slot::UnsealedFormat));
            assert_eq!(inspect_header(&unsealed[..12], size), SlotHeader::Unsealed);
            // A slot of this format whose version field changed to 1 is refused the same way.
            let mut changed = encode(4, &entries(&[("ns", b"k", b"v")])).unwrap();
            changed[8] ^= 0x03;
            assert_eq!(decode(&changed), Slot::UnsealedFormat);
        }
    }

    store_test! {
        fn test_format_changed_version_field_is_damaged_not_newer() {
            let slot = encode(4, &entries(&[("ns", b"k", b"v")])).unwrap();
            // The version field changed after the write, to an older, a newer or the largest
            // version: the slot opens with the version set back.
            for version in [0, FORMAT_VERSION + 1, 0x82, u32::MAX] {
                let mut changed = slot.clone();
                changed[8..12].copy_from_slice(&version.to_le_bytes());
                assert_eq!(decode(&changed), Slot::Damaged, "version {version}");
            }
            // A newer slot in this layout, its version under its tag, is a newer format.
            let newer = seal_raw(&sealer(), FORMAT_VERSION + 1, 4, b"anything");
            assert_eq!(decode(&newer), Slot::NewerFormat(FORMAT_VERSION + 1));
            // An unknown older version under its own tag is not a slot this format wrote.
            assert_eq!(decode(&seal_raw(&sealer(), 0, 4, b"anything")), Slot::Torn);
            // A newer version whose size does not fit this layout is decided by the header
            // alone, before the rest of the file is read.
            let size = u64::try_from(newer.len()).unwrap();
            assert_eq!(read_header(&newer, size + 1), Some(Slot::NewerFormat(FORMAT_VERSION + 1)));
            assert_eq!(read_header(&newer, size), None);
        }
    }

    store_test! {
        fn test_format_inspection_without_a_key() {
            let slot = encode(1, &entries(&[("ns", b"k", b"v")])).unwrap();
            let size = u64::try_from(slot.len()).unwrap();
            let header = inspect_header(&slot[..HEADER_LEN], size);
            assert_eq!(header.store_id(), Some(TEST_STORE));
            assert_eq!(Inspection::of([SlotHeader::Empty; 2]), Inspection::Empty);
            assert_eq!(Inspection::of([SlotHeader::Empty, header]), Inspection::Present([SlotHeader::Empty, header]));
            assert_eq!(Inspection::of([header, SlotHeader::Unsealed]), Inspection::UnsealedFormat);
            assert_eq!(Inspection::of([SlotHeader::Empty, header]).newer_version(), None);
            // A header cut short has no store identifier yet.
            let short = inspect_header(&slot[..HEADER_LEN - 1], u64::try_from(HEADER_LEN - 1).unwrap());
            assert_eq!(short, SlotHeader::Versioned { version: FORMAT_VERSION, store_id: None });
            assert_eq!(inspect_header(b"PKBS", 4), SlotHeader::Unrecognized);
            assert_eq!(inspect_header(b"not a slot at all", 17), SlotHeader::Unrecognized);
            assert_eq!(inspect_header(&[], 0), SlotHeader::Empty);
            // A newer version: its layout is unknown, so it has no store identifier here.
            let mut newer = slot.clone();
            newer[8..12].copy_from_slice(&9u32.to_le_bytes());
            let newer = inspect_header(&newer[..HEADER_LEN], size);
            assert_eq!(newer, SlotHeader::Versioned { version: 9, store_id: None });
            assert_eq!(Inspection::of([header, newer]).newer_version(), Some(9));
            // Without the key, a newer format wins over a lost key.
            assert_eq!(Inspection::of([header, newer]).key_lost_error(), OpenError::NewerFormat { version: 9 });
            assert_eq!(Inspection::of([SlotHeader::Empty, header]).key_lost_error(), OpenError::KeyLost);
            assert_eq!(Inspection::Empty.key_lost_error(), OpenError::KeyLost);
            // No complete header of this format: nothing sealed is there, so it is not a lost
            // key but files with nothing to trust.
            let garbage = Inspection::of([SlotHeader::Unrecognized, SlotHeader::Empty]);
            assert_eq!(garbage.key_lost_error(), OpenError::Corrupt);
            assert_eq!(Inspection::of([short, SlotHeader::Empty]).key_lost_error(), OpenError::Corrupt);
            assert_eq!(Inspection::of([short, header]).key_lost_error(), OpenError::KeyLost);
        }
    }

    store_test! {
        fn test_format_pick_current_takes_the_higher_generation() {
            let valid = |generation, value: &[u8]| Slot::Valid {
                generation,
                entries: entries(&[("ns", b"k", value)]),
            };
            let picked = pick_current(valid(5, b"old"), valid(7, b"new")).unwrap();
            assert_eq!((picked.slot, picked.generation), (Some(SlotId::B), 7));
            let picked = pick_current(valid(7, b"new"), valid(5, b"old")).unwrap();
            assert_eq!((picked.slot, picked.generation), (Some(SlotId::A), 7));
            assert_eq!(picked.entries, entries(&[("ns", b"k", b"new")]));
            assert_eq!(
                pick_current(valid(7, b"a"), valid(7, b"b")),
                Err(OpenError::Corrupt)
            );
            let others = || [Slot::Empty, Slot::Torn, Slot::TornFirstCommit, Slot::Damaged];
            for other in others() {
                let picked = pick_current(other, valid(1, b"v")).unwrap();
                assert_eq!(picked.slot, Some(SlotId::B));
            }
            for other in others() {
                let picked = pick_current(valid(1, b"v"), other).unwrap();
                assert_eq!(picked.slot, Some(SlotId::A));
            }
            assert_eq!(
                pick_current(valid(9, b"v"), Slot::Malformed),
                Err(OpenError::Corrupt)
            );
            assert_eq!(
                pick_current(valid(9, b"v"), Slot::NewerFormat(4)),
                Err(OpenError::NewerFormat { version: 4 })
            );
            // The unsealed format is refused whatever the other slot holds.
            for other in [valid(9, b"v"), Slot::Empty, Slot::NewerFormat(4)] {
                assert_eq!(pick_current(Slot::UnsealedFormat, other), Err(OpenError::UnsealedFormat));
            }
            assert_eq!(pick_current(Slot::Empty, Slot::UnsealedFormat), Err(OpenError::UnsealedFormat));
            let invalid = || [Slot::Torn, Slot::TornFirstCommit, Slot::Damaged];
            for (i, j) in (0..3).flat_map(|i| (0..3).map(move |j| (i, j))) {
                let [a, b] = [i, j].map(|at| invalid().into_iter().nth(at).unwrap());
                assert_eq!(pick_current(a, b), Err(OpenError::Corrupt));
            }
            for (a, b) in [
                // A complete slot whose bytes changed, with no other slot to fall back to: a
                // first commit cut short never fails its tag at full length, and always starts
                // as a first commit.
                (Slot::Empty, Slot::Damaged),
                (Slot::Damaged, Slot::Empty),
                (Slot::Empty, Slot::Torn),
                (Slot::Torn, Slot::Empty),
            ] {
                assert_eq!(pick_current(a, b), Err(OpenError::Corrupt));
            }
            for (a, b) in [
                (Slot::Empty, Slot::Empty),
                (Slot::Empty, Slot::TornFirstCommit),
                (Slot::TornFirstCommit, Slot::Empty),
            ] {
                let picked = pick_current(a, b).unwrap();
                assert_eq!(picked.slot, None);
                assert_eq!(picked.generation, 0);
                assert!(picked.entries.is_empty());
            }
        }
    }
}
