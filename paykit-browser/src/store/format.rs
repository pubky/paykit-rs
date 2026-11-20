//! The bytes of one slot: encoding a whole state, checking and decoding a slot, and picking the
//! current slot of the two.
//!
//! A slot holds one whole snapshot of the store:
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0..8 | [`MAGIC`] |
//! | 8..12 | format version, `u32` little-endian ([`FORMAT_VERSION`]) |
//! | 12..20 | generation, `u64` little-endian: 1 for the first commit, one more for each next |
//! | 20..28 | payload length in bytes, `u64` little-endian, at most [`MAX_STATE_BYTES`] |
//! | 28.. | payload |
//! | last 32 | tag: the BLAKE3 hash of every byte before it |
//!
//! The payload is every entry of the store, in ascending order of namespace and then key, each
//! as three parts: the namespace (UTF-8), the key and the value, each a `u32` little-endian
//! length and then its bytes. Namespaces and keys compare as bytes, the order of the in-memory
//! maps, so one state has exactly one encoding. Owners are not part of it: they live in memory
//! only, as in the Switch client's `MemoryStore`.
//!
//! The tag detects a torn or damaged write: a write that stopped part way, or bytes that
//! changed on disk. It is not a key-based authenticator, so it does not detect tampering: anyone
//! who can write the files can write a slot with a matching tag.
//!
//! A slot is valid only if its size is exactly the header, the payload length and the tag, and
//! its tag matches. Trailing bytes make it invalid, so a slot rewritten over a longer one is
//! not valid until the file is cut to the new length.
//!
//! Every format version keeps the magic and the version in the first 12 bytes, so a build can
//! tell a slot written by a newer build from a damaged one. A slot whose version is not this
//! build's, but which passes this format's size and tag checks once the version is set back to
//! this build's, is a slot of this format with a damaged version field. Any other slot with a
//! newer version is of a newer format. Version 1 is plaintext; the version leaves room for a
//! sealed format.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use super::OpenError;

/// The store's contents: namespace, then key, then value.
pub(crate) type Entries = BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>;

/// The first eight bytes of every slot.
pub(crate) const MAGIC: [u8; 8] = *b"PKBSTORE";

/// The format version this build writes, and the newest it reads.
pub(crate) const FORMAT_VERSION: u32 = 1;

/// The length of the header: magic, format version, generation and payload length.
pub(crate) const HEADER_LEN: usize = 8 + 4 + 8 + 8;

/// The length of the tag at the end of a slot.
const TAG_LEN: usize = 32;

/// The magic and the format version: the part of the header every format version keeps.
const VERSIONED_PREFIX_LEN: usize = 8 + 4;

/// The largest encoded state, the payload of a slot: 256 MiB.
///
/// The whole state lives in WebAssembly memory, which is at most 4 GiB on wasm32 and often less
/// in a browser. A commit holds the current state, the next state and the next state's encoded
/// slot at once; opening holds one slot's bytes and both slots' decoded states. A decoded state
/// takes its bytes plus about a hundred bytes per entry for its maps and allocations, so with
/// [`MAX_ENTRIES`] a state at both caps takes about 400 MiB, and those peaks stay near 1.1 GiB.
/// Paykit's states are about a hundred kilobytes, and the largest batch the Switch client
/// commits is 32 MiB, so the cap leaves a wide margin.
pub(crate) const MAX_STATE_BYTES: u64 = 256 << 20;

/// The most entries a state holds: 2^20.
///
/// Without it, 256 MiB of tiny entries would decode to several GiB of maps, past what a
/// browser gives WebAssembly: a slot whose tag matches, written by anyone who can write the
/// origin's files, could then exhaust memory on open. Paykit's states hold about a hundred
/// entries.
pub(crate) const MAX_ENTRIES: usize = 1 << 20;

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

/// Check a slot's header against its file's size, before the rest of the slot is read.
///
/// `prefix` holds the file's first bytes: at least [`HEADER_LEN`] of them when the file has
/// that many. Every length is checked against the file size and the state cap here, before the
/// caller reads or allocates anything for the rest of the slot.
///
/// Returns the slot when the header alone decides it: [`Slot::Empty`], [`Slot::Torn`] or
/// [`Slot::TornFirstCommit`] (a header cut short or without the magic, a payload length over
/// the cap, or a size that is not exactly the header, the payload length and the tag) or
/// [`Slot::NewerFormat`]. Returns `None` when [`decode_slot`] must check the whole file, which
/// is then at most a header, a payload of [`MAX_STATE_BYTES`] and a tag: when the header is
/// consistent with the file size in this format's layout, whatever its version; or when the
/// file starts as a first commit does but its size does not match its payload length, which a
/// complete first commit with a changed length field also gives.
pub(crate) fn read_header(prefix: &[u8], file_size: u64) -> Option<Slot> {
    if file_size == 0 {
        return Some(Slot::Empty);
    }
    let (Some(magic), Some(version)) = (prefix.get(..MAGIC.len()), read_u32(prefix, MAGIC.len()))
    else {
        return Some(torn(prefix));
    };
    if magic != MAGIC {
        return Some(Slot::Torn);
    }
    // A newer format's header may be laid out differently, so a newer version that does not fit
    // this format's layout is a newer format. One that fits is decided by `decode_slot`.
    let newer = version > FORMAT_VERSION;
    let payload_len = read_u64(prefix, VERSIONED_PREFIX_LEN + 8);
    // The sum cannot overflow: the payload length is at most the cap.
    let fits = payload_len.is_some_and(|len| {
        len <= MAX_STATE_BYTES && file_size == HEADER_LEN as u64 + len + TAG_LEN as u64
    });
    if fits {
        return None;
    }
    if newer {
        return Some(Slot::NewerFormat(version));
    }
    let could_be_complete = (SLOT_OVERHEAD..=SLOT_OVERHEAD + MAX_STATE_BYTES).contains(&file_size);
    if payload_len.is_some() && could_be_complete && starts_like_first_commit(prefix) {
        return None;
    }
    Some(torn(prefix))
}

/// The bytes a slot has besides its payload: the header and the tag.
const SLOT_OVERHEAD: u64 = (HEADER_LEN + TAG_LEN) as u64;

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
    first[MAGIC.len()..VERSIONED_PREFIX_LEN].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    first[VERSIONED_PREFIX_LEN..].copy_from_slice(&1u64.to_le_bytes());
    let len = prefix.len().min(first.len());
    prefix[..len] == first[..len]
}

/// One slot as read at open.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// The file is empty.
    Empty,
    /// A slot a write that stopped part way can leave: a header cut short, without the magic or
    /// over the cap, or a size other than the one the header gives (a write over a longer slot
    /// leaves the old slot's last bytes until the file is cut). The next commit overwrites it.
    Torn,
    /// A torn slot whose first bytes are those of a first commit: the magic, this format
    /// version and generation 1, as far as the file reaches. Next to an empty file, it is the
    /// only torn slot a first commit that stopped part way can leave.
    TornFirstCommit,
    /// A complete slot of this format that fails its tag: exactly as long as its header says,
    /// with a tag that does not match, or that matches only with the format version set back
    /// to this build's; or a slot that starts as a first commit and whose tag matches only with
    /// its payload length set to the file's. Most likely bytes that changed after a complete
    /// write.
    Damaged,
    /// A slot of a newer format version than this build reads.
    NewerFormat(u32),
    /// A slot whose tag matches but whose payload this build cannot decode. A torn write cannot
    /// produce one, and no writer of this format does: the store refuses to open.
    Malformed,
    /// A valid slot.
    Valid {
        /// The slot's generation.
        generation: u64,
        /// The state it holds.
        entries: Entries,
    },
}

/// Check and decode a whole slot.
pub(crate) fn decode_slot(bytes: &[u8]) -> Slot {
    let file_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if let Some(slot) = read_header(bytes, file_size) {
        return slot;
    }
    // `read_header` checked that the size is at least a header and a tag.
    let (Some(version), Some(payload_len), Some(body_len)) = (
        read_u32(bytes, MAGIC.len()),
        read_u64(bytes, VERSIONED_PREFIX_LEN + 8),
        bytes.len().checked_sub(TAG_LEN),
    ) else {
        return Slot::Torn;
    };
    let (body, tag) = bytes.split_at(body_len);
    let file_payload_len = u64::try_from(body_len.saturating_sub(HEADER_LEN)).unwrap_or(u64::MAX);
    if payload_len != file_payload_len {
        // `read_header` lets this through only for a slot that starts as a first commit. With
        // nothing else in the store, a changed length field must not pass for a torn first
        // commit, which would open a new store over the committed state.
        return if tag_matches_with_header(body, tag, FORMAT_VERSION, file_payload_len) {
            Slot::Damaged
        } else {
            Slot::TornFirstCommit
        };
    }
    if version != FORMAT_VERSION {
        // A later format must keep its version under its tag, so that none of its slots passes
        // this check by chance.
        return if tag_matches_with_header(body, tag, FORMAT_VERSION, payload_len) {
            Slot::Damaged
        } else if version > FORMAT_VERSION {
            Slot::NewerFormat(version)
        } else {
            Slot::Torn
        };
    }
    if blake3::hash(body).as_bytes() != tag {
        return Slot::Damaged;
    }
    let (Some(generation), Some(payload)) =
        (read_u64(body, VERSIONED_PREFIX_LEN), body.get(HEADER_LEN..))
    else {
        return Slot::Torn;
    };
    match decode_entries(payload) {
        Some(entries) => Slot::Valid {
            generation,
            entries,
        },
        None => Slot::Malformed,
    }
}

/// Whether `tag` is the tag of `body` with its format version and payload length fields set
/// to `version` and `payload_len`: a slot of this format whose version or length field alone
/// changed. The body is hashed in place, not copied. `body` holds at least a header.
fn tag_matches_with_header(body: &[u8], tag: &[u8], version: u32, payload_len: u64) -> bool {
    let (Some(magic), Some(generation), Some(payload)) = (
        body.get(..MAGIC.len()),
        body.get(VERSIONED_PREFIX_LEN..VERSIONED_PREFIX_LEN + 8),
        body.get(HEADER_LEN..),
    ) else {
        return false;
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(magic);
    hasher.update(&version.to_le_bytes());
    hasher.update(generation);
    hasher.update(&payload_len.to_le_bytes());
    hasher.update(payload);
    hasher.finalize().as_bytes() == tag
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

/// The slot that holds `entries` at `generation`, or `None` if the encoded state would be over
/// [`MAX_STATE_BYTES`] or hold more than [`MAX_ENTRIES`] entries. The size is checked before
/// anything is allocated, and the payload is encoded straight into the one buffer that holds
/// the slot.
pub(crate) fn encode_slot(generation: u64, entries: &Entries) -> Option<Vec<u8>> {
    let len = payload_len(entries);
    if len > MAX_STATE_BYTES || entries.values().map(BTreeMap::len).sum::<usize>() > MAX_ENTRIES {
        return None;
    }
    // At most the cap, which fits in wasm32's `usize`.
    let payload_size = usize::try_from(len).ok()?;
    let mut slot = Vec::with_capacity(HEADER_LEN + payload_size + TAG_LEN);
    push_header(&mut slot, generation, len);
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
    push_tag(&mut slot);
    Some(slot)
}

/// Start a slot of the current format version: the header for a payload of `payload_len` bytes
/// at `generation`.
fn push_header(slot: &mut Vec<u8>, generation: u64, payload_len: u64) {
    slot.extend_from_slice(&MAGIC);
    slot.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    slot.extend_from_slice(&generation.to_le_bytes());
    slot.extend_from_slice(&payload_len.to_le_bytes());
}

/// End a slot: append the tag of every byte before it.
fn push_tag(slot: &mut Vec<u8>) {
    let tag = blake3::hash(slot);
    slot.extend_from_slice(tag.as_bytes());
}

/// A slot of the current format version holding any `payload` at `generation`, for tests that
/// need a slot whose tag matches but whose payload is not a state's encoding.
#[cfg(test)]
pub(crate) fn seal(generation: u64, payload: &[u8]) -> Vec<u8> {
    let mut slot = Vec::with_capacity(HEADER_LEN + payload.len() + TAG_LEN);
    push_header(&mut slot, generation, u64::try_from(payload.len()).unwrap());
    slot.extend_from_slice(payload);
    push_tag(&mut slot);
    slot
}

/// `slot` with its tag replaced by the tag of its bytes as they are now, for tests that need a
/// slot of a newer format: a slot whose changed header is under its tag.
#[cfg(test)]
pub(crate) fn retag(mut slot: Vec<u8>) -> Vec<u8> {
    slot.truncate(slot.len() - TAG_LEN);
    push_tag(&mut slot);
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
mod tests {
    use super::*;

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
            let slot = encode_slot(9, &state).unwrap();
            assert_eq!(&slot[..8], &MAGIC);
            assert_eq!(
                decode_slot(&slot),
                Slot::Valid {
                    generation: 9,
                    entries: state.clone()
                }
            );
            // One state has one encoding.
            assert_eq!(encode_slot(9, &state.clone()).unwrap(), slot);
            // An empty state is a valid slot of only a header and a tag.
            let empty = encode_slot(1, &Entries::new()).unwrap();
            assert_eq!(empty.len(), HEADER_LEN + TAG_LEN);
            assert_eq!(
                decode_slot(&empty),
                Slot::Valid {
                    generation: 1,
                    entries: Entries::new()
                }
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
                let decoded = decode_slot(&seal(3, &bytes));
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
            assert_eq!(decode_slot(&seal(1, &payload)), Slot::Malformed);
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
        assert_eq!(encode_slot(1, &state), None);
        state.get_mut("ns").unwrap().pop_first();
        assert!(encode_slot(1, &state).is_some());
    }

    store_test! {
        fn test_format_header_refuses_lengths_that_do_not_fit() {
            let slot = encode_slot(2, &entries(&[("ns", b"k", b"v")])).unwrap();
            let size = u64::try_from(slot.len()).unwrap();
            assert_eq!(read_header(&slot, size), None);
            assert_eq!(read_header(&slot, size + 1), Some(Slot::Torn));
            assert_eq!(read_header(&slot, size - 1), Some(Slot::Torn));
            assert_eq!(read_header(&slot[..4], 4), Some(Slot::TornFirstCommit));
            assert_eq!(read_header(b"PKBX", 4), Some(Slot::Torn));
            assert_eq!(read_header(&[], 0), Some(Slot::Empty));
            // A payload length larger than the file, larger than the cap, and the largest.
            for payload_len in [size, MAX_STATE_BYTES + 1, u64::MAX] {
                let mut header = slot[..HEADER_LEN].to_vec();
                header[20..28].copy_from_slice(&payload_len.to_le_bytes());
                assert_eq!(read_header(&header, size), Some(Slot::Torn), "{payload_len}");
            }
            // Over the cap, the header is torn even for a file of the size it asks for.
            for payload_len in [MAX_STATE_BYTES + 1, u64::MAX - 100] {
                let mut header = slot[..HEADER_LEN].to_vec();
                header[20..28].copy_from_slice(&payload_len.to_le_bytes());
                let claimed = payload_len + (HEADER_LEN + TAG_LEN) as u64;
                assert_eq!(read_header(&header, claimed), Some(Slot::Torn), "{payload_len}");
            }
            // A newer version with a header shorter than this format's is a newer format.
            let mut newer = slot[..VERSIONED_PREFIX_LEN].to_vec();
            newer[8..12].copy_from_slice(&2u32.to_le_bytes());
            assert_eq!(read_header(&newer, 12), Some(Slot::NewerFormat(2)));
        }
    }

    store_test! {
        fn test_format_complete_slot_with_a_wrong_tag_is_damaged() {
            let slot = encode_slot(1, &entries(&[("ns", b"k", b"v")])).unwrap();
            let len = slot.len();
            // A changed byte in the magic is torn: nothing says how long the slot should be. It
            // is not a torn first commit either, which always starts with the magic.
            let mut magic = slot.clone();
            magic[0] ^= 0x01;
            assert_eq!(decode_slot(&magic), Slot::Torn);
            // A changed byte in the generation, the payload or the tag, at the full length.
            for at in [14, HEADER_LEN + 2, len - TAG_LEN, len - 1] {
                let mut damaged = slot.clone();
                damaged[at] ^= 0x01;
                assert_eq!(decode_slot(&damaged), Slot::Damaged, "byte {at}");
            }
            // A changed byte in the payload length of a first commit, to a length smaller or
            // larger than the file, or over the cap: its tag matches with the length set back
            // to the file's.
            for at in [20, 21, 27] {
                let mut damaged = slot.clone();
                damaged[at] ^= 0x01;
                assert_eq!(decode_slot(&damaged), Slot::Damaged, "byte {at}");
            }
            // Cut short at any point, the slot is a torn first commit.
            for cut in [1, 11, HEADER_LEN - 1, HEADER_LEN, len - TAG_LEN, len - 1] {
                assert_eq!(decode_slot(&slot[..cut]), Slot::TornFirstCommit, "cut {cut}");
            }
            // So is a first commit written whole over a longer one that stopped part way, before
            // the file was cut.
            let longer = encode_slot(1, &entries(&[("ns", b"k", b"a longer value")])).unwrap();
            let mut over = longer[..longer.len() - 1].to_vec();
            over[..len].copy_from_slice(&slot);
            assert_eq!(decode_slot(&over), Slot::TornFirstCommit);
            // A later slot cut short is torn, and so is one whose first bytes are not a first
            // commit's.
            let later = encode_slot(2, &entries(&[("ns", b"k", b"v")])).unwrap();
            assert_eq!(decode_slot(&later[..len - 1]), Slot::Torn);
            assert_eq!(decode_slot(&later[..HEADER_LEN - 1]), Slot::Torn);
        }
    }

    store_test! {
        fn test_format_changed_version_field_is_damaged_not_newer() {
            let slot = encode_slot(4, &entries(&[("ns", b"k", b"v")])).unwrap();
            // The version field changed after the write, to an older, a newer or the largest
            // version: the slot passes this format's checks with the version set back.
            for version in [0, FORMAT_VERSION + 1, 0x81, u32::MAX] {
                let mut changed = slot.clone();
                changed[8..12].copy_from_slice(&version.to_le_bytes());
                assert_eq!(decode_slot(&changed), Slot::Damaged, "version {version}");
            }
            // A newer slot in this layout, its version under its tag, is a newer format.
            let mut newer = slot.clone();
            newer[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
            let newer = retag(newer);
            assert_eq!(decode_slot(&newer), Slot::NewerFormat(FORMAT_VERSION + 1));
            // An unknown older version under its own tag is not a slot this format wrote.
            let mut old = slot.clone();
            old[8..12].copy_from_slice(&0u32.to_le_bytes());
            assert_eq!(decode_slot(&retag(old)), Slot::Torn);
            // A newer version whose size does not fit this layout is decided by the header
            // alone, before the rest of the file is read.
            let size = u64::try_from(newer.len()).unwrap();
            assert_eq!(read_header(&newer, size + 1), Some(Slot::NewerFormat(FORMAT_VERSION + 1)));
            assert_eq!(read_header(&newer, size), None);
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
