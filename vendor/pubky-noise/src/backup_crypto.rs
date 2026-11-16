//! Encryption for session snapshots persisted to the homeserver.
//!
//! A serialized [`PubkyNoiseSessionState`] contains the session's ephemeral
//! secret (and optionally the static secret), so it must be encrypted before
//! it leaves the device. Encryption uses `XChaCha20Poly1305` from the
//! [`chacha20poly1305`] crate — already in the dependency tree via `snow`
//! (the Noise transport itself runs `Noise_*_25519_ChaChaPoly_SHA256`) — with
//! a fresh random 192-bit nonce per write, prepended to the ciphertext.
//!
//! The nonce is not a secret — by design it only needs to be unique per
//! encryption under the same key, which a random 192-bit value provides with
//! overwhelming probability, so storing it alongside the ciphertext is
//! standard practice (it is required for decryption). The AEAD tag
//! authenticates the ciphertext against that same nonce, so tampering with
//! the prepended nonce fails decryption.
//!
//! `XChaCha20Poly1305` is used instead of the Pubky recovery-file convention
//! (`pubky_common::crypto`'s XSalsa20Poly1305 / NaCl `crypto_secretbox`)
//! because the NaCl construction accepts no associated data, which would
//! force the envelope header to be duplicated inside the authenticated
//! plaintext and re-validated after decryption. Native AAD binds the header
//! directly into the Poly1305 tag, and the envelope's closed dispatch on
//! `algorithm_id` below keeps future AEAD changes unambiguous.
//!
//! ## Envelope Format
//!
//! ```text
//! [0..4]    magic: "PNBK"
//! [4]       envelope format version (1)
//! [5]       algorithm ID (1 = XChaCha20Poly1305 with the KDF below)
//! [6..30]   nonce (192-bit, random per write)
//! [30..]    ciphertext || Poly1305 tag
//! ```
//!
//! The associated data is `header || backup_path`: the header stays in
//! cleartext so the decoder can dispatch on it, while any modification makes
//! tag verification fail. Binding the intended backup path commits each
//! record to its storage location — a malicious homeserver cannot substitute
//! a backup written for a *different* path under the same key (e.g. a
//! higher-generation backup from another session sharing the root-derived
//! key), because the AAD mismatch fails decryption before the checkpoint or
//! the session state can be poisoned. The authenticated plaintext is
//! therefore just `generation (8, big-endian) || session state`.
//! The session state has bounded variable length and includes saved handshake messages.
//! Envelope versioning is intentionally separate from the session-state
//! serialization version inside the plaintext.
//!
//! ## Key Derivation
//!
//! The encryption key is derived from the Pubky root secret with a
//! domain-separated SHA-256 KDF, mirroring the domain-separation idiom used
//! by [`crate::path_derivation`]:
//!
//! ```text
//! backup_key = SHA-256("pubky-noise/session-backup/v0" || root_secret)
//! ```
//!
//! The root secret has 256 bits of entropy, so a plain hash KDF is
//! sufficient — no password KDF (Argon2) is needed. Domain separation ensures
//! this key can never be confused with keys derived for other purposes from
//! the same root secret.
//!
//! The domain tag's `v0` versions the *key-derivation context* only. It is
//! deliberately independent of the session-state serialization version
//! ([`crate::serializer::SESSION_STATE_VERSION`]), the envelope format
//! version, and the algorithm ID, which evolve separately: the derived key
//! just encrypts bytes and does not care about their layout. The tag is
//! bumped only when the KDF itself changes, or when a fresh key space is
//! wanted — e.g. alongside a new encryption algorithm, so keys are separated
//! across algorithms. Note that a tag bump changes the derived key, so
//! backups written under the old tag become undecryptable; that is usually
//! desirable when the bump accompanies an algorithm change. The switch from
//! XSalsa20Poly1305 to XChaCha20Poly1305 predates any deployed backup, so the
//! tag stays at `v0`: the KDF itself is unchanged and no old-tag records
//! exist in the wild.
//!
//! No compression is applied: key material and handshake ciphertexts do not
//! compress usefully. Handshake packet padding is not saved.
//!
//! ## Rollback Protection
//!
//! AEAD authenticates the bytes but provides no freshness: a stale or
//! malicious homeserver can return an older, still-valid backup after the
//! session has advanced. To detect this, every backup carries a monotonic
//! `generation` in its authenticated plaintext. The caller persists each new
//! generation in *trusted local storage* and passes the latest observed value
//! as `min_generation` to [`decrypt_backup_with_key`]; older records are
//! rejected with
//! [`BackupCryptoError::Rollback`]. The `backup_path` bound into the AAD and
//! the `generation` bound into the plaintext together also defeat an
//! "in-place" replay, where the homeserver returns a record encrypted for the
//! same path but with a different generation. Without a trusted checkpoint
//! (fresh device, `min_generation = None`) rollback cannot be detected — a
//! signed or hash-chained sequence alone is not sufficient either, since the
//! homeserver can simply withhold the newest element. Compromise or rollback
//! of the local trusted checkpoint itself is outside the security model: the
//! checkpoint must be kept in trusted, integrity-protected storage.
//!
//! ### Checkpoint update order
//!
//! The trusted local checkpoint must be advanced to the new `generation`
//! **before** (or atomically with) uploading the backup to the homeserver.
//! Advancing it only *after* the upload leaves a crash window: if generation
//! `N` reaches the server but the process crashes before the checkpoint is
//! updated, the checkpoint still holds `N - 1` and a replayed generation
//! `N - 1` record is accepted — the exact rollback the checkpoint exists to
//! prevent. Crashing with the checkpoint already advanced is safe: the new
//! upload is simply lost, and loading then rejects the older record (or finds
//! no usable backup) instead of silently accepting stale state.
//!
//! One residual case remains uncovered: a crash during checkpoint generation
//! itself — before either the checkpoint or any backup of a new session is
//! durable — loses in-memory handshake progress irrecoverably, and the
//! standard Noise protocol offers no clean peer-initiated message replay.
//! Callers must treat that window as a fresh-start signal.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

use crate::serializer::{PubkyNoiseSessionState, MAX_SESSION_STATE_LEN, MIN_SESSION_STATE_LEN};

/// Domain separation tag for backup key derivation.
const BACKUP_KEY_DOMAIN: &[u8] = b"pubky-noise/session-backup/v0";

/// Magic bytes identifying a pubky-noise session backup envelope.
const ENVELOPE_MAGIC: &[u8; 4] = b"PNBK";
/// Current backup envelope format version.
const ENVELOPE_VERSION: u8 = 1;
/// Algorithm ID: XChaCha20Poly1305 with the domain-separated SHA-256 KDF.
const ALG_XCHACHA20POLY1305: u8 = 1;

/// Header length: magic (4) || version (1) || algorithm (1).
const HEADER_LEN: usize = 6;
/// XChaCha20Poly1305 nonce length.
const NONCE_LEN: usize = 24;
/// Poly1305 authentication tag length.
const TAG_LEN: usize = 16;
/// Monotonic generation counter length (big-endian u64).
const GENERATION_LEN: usize = 8;

/// Minimum encrypted backup length, including the header, nonce, generation, and tag.
pub const MIN_BACKUP_RECORD_LEN: usize =
    HEADER_LEN + NONCE_LEN + GENERATION_LEN + MIN_SESSION_STATE_LEN + TAG_LEN;
/// Maximum encrypted backup length, including the complete handshake transcript.
pub const MAX_BACKUP_RECORD_LEN: usize =
    HEADER_LEN + NONCE_LEN + GENERATION_LEN + MAX_SESSION_STATE_LEN + TAG_LEN;

/// Size cap on successful (2xx) homeserver response bodies when fetching a backup.
///
/// Bounds the accumulated backup body while accommodating the largest supported
/// record. Non-2xx error-body capture is bounded separately by the Pubky SDK's
/// configured limit. Neither byte cap bounds elapsed time or total memory use.
pub const MAX_BACKUP_RESPONSE_BYTES: usize = 4096;

// The size cap must always fit the largest supported record.
const _: () = assert!(MAX_BACKUP_RECORD_LEN <= MAX_BACKUP_RESPONSE_BYTES);

/// Errors produced when parsing or decrypting a backup envelope.
#[derive(Debug, PartialEq, Eq)]
pub enum BackupCryptoError {
    /// Record does not start with the backup magic bytes (e.g. a
    /// plaintext snapshot or unrelated data). Carries the first four bytes
    /// of the record (zero-padded if shorter).
    InvalidMagic([u8; 4]),
    /// Unsupported envelope format version.
    UnsupportedEnvelopeVersion(u8),
    /// Unsupported algorithm identifier.
    UnsupportedAlgorithm(u8),
    /// Record length is outside the bounds of its envelope version.
    InvalidLength {
        /// Expected length in bytes.
        expected: usize,
        /// Actual length in bytes.
        actual: usize,
    },
    /// AEAD decryption failed (wrong key or tampered record).
    DecryptError,
    /// The backup generation is older than the trusted local checkpoint.
    Rollback {
        /// Generation found in the backup.
        generation: u64,
        /// Trusted local checkpoint the backup was checked against.
        min_generation: u64,
    },
}

/// Derives the snapshot backup encryption key from the Pubky root secret.
///
/// `SHA-256(BACKUP_KEY_DOMAIN || root_secret)`.
pub fn derive_backup_key(root_secret: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(BACKUP_KEY_DOMAIN);
    hasher.update(root_secret);
    hasher.finalize().into()
}

/// Encrypts a session snapshot into a versioned backup envelope using a
/// caller-provided 32-byte encryption key directly.
///
/// `generation` is a caller-managed, monotonically increasing counter used
/// for rollback detection (see the module docs). The caller must persist the
/// latest generation in trusted local storage and supply it as
/// `min_generation` when loading.
///
/// `backup_path` is the homeserver path the record will be stored at; it is
/// bound into the Poly1305 tag as associated data (together with the
/// header), so the record cannot be replayed at — or accepted from — a
/// different path. Together with the monotonic `generation`, this also
/// prevents an in-place replay of a same-path record with a different
/// generation; compromise of the caller's trusted local checkpoint is
/// outside the security model (see module-level "Rollback Protection").
///
/// Returns `magic || version || algorithm || nonce || ciphertext`, with the
/// header and backup path bound into the Poly1305 tag as associated data.
pub fn encrypt_backup_with_key(
    key: &[u8; 32],
    backup_path: &str,
    generation: u64,
    state: &PubkyNoiseSessionState,
) -> Vec<u8> {
    let header = envelope_header();
    let aad = build_aad(&header, backup_path);

    let serialized = state.serialize();
    let mut plaintext = Vec::with_capacity(GENERATION_LEN + serialized.len());
    plaintext.extend_from_slice(&generation.to_be_bytes());
    plaintext.extend_from_slice(&serialized);

    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = random_nonce();
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .expect("encryption with a fresh random nonce cannot fail");

    let mut out = Vec::with_capacity(HEADER_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    out
}

/// Encrypts a session snapshot into a versioned backup envelope, deriving the
/// encryption key from the Pubky root secret via [`derive_backup_key()`].
///
/// Convenience wrapper around [`encrypt_backup_with_key()`] for callers that
/// hold the root identity secret directly.
pub fn encrypt_backup(
    root_secret: &[u8; 32],
    backup_path: &str,
    generation: u64,
    state: &PubkyNoiseSessionState,
) -> Vec<u8> {
    let key = derive_backup_key(root_secret);
    encrypt_backup_with_key(&key, backup_path, generation, state)
}

/// Decrypts and parses a backup envelope produced by
/// [`encrypt_backup_with_key()`] or [`encrypt_backup()`], using a
/// caller-provided 32-byte decryption key directly.
///
/// `backup_path` is the homeserver path the record was fetched from; it is
/// bound into the Poly1305 tag as associated data (together with the
/// header). A record encrypted for a different path fails decryption, which
/// defeats cross-path substitution by a malicious homeserver. Together with
/// `min_generation`, this also rejects an in-place replay of a same-path
/// record carrying an older generation; a compromised local checkpoint is
/// outside the security model (see module-level "Rollback Protection").
///
/// Only explicitly supported envelope versions and algorithms are accepted,
/// and the record length must be within that version's bounds.
///
/// `min_generation` is the caller's trusted local checkpoint: the highest
/// generation previously observed for this backup path. Records older than
/// the checkpoint are rejected with [`BackupCryptoError::Rollback`].
///
/// Returns the backup generation and the serialized session state.
pub fn decrypt_backup_with_key(
    key: &[u8; 32],
    backup_path: &str,
    record: &[u8],
    min_generation: Option<u64>,
) -> Result<(u64, Vec<u8>), BackupCryptoError> {
    if record.len() < HEADER_LEN {
        return Err(BackupCryptoError::InvalidLength {
            expected: HEADER_LEN,
            actual: record.len(),
        });
    }
    if &record[..4] != ENVELOPE_MAGIC {
        let mut magic = [0u8; 4];
        let len = record.len().min(4);
        magic[..len].copy_from_slice(&record[..len]);
        return Err(BackupCryptoError::InvalidMagic(magic));
    }
    let version = record[4];
    if version != ENVELOPE_VERSION {
        return Err(BackupCryptoError::UnsupportedEnvelopeVersion(version));
    }
    let algorithm = record[5];
    if algorithm != ALG_XCHACHA20POLY1305 {
        return Err(BackupCryptoError::UnsupportedAlgorithm(algorithm));
    }
    if !(MIN_BACKUP_RECORD_LEN..=MAX_BACKUP_RECORD_LEN).contains(&record.len()) {
        return Err(BackupCryptoError::InvalidLength {
            expected: record
                .len()
                .clamp(MIN_BACKUP_RECORD_LEN, MAX_BACKUP_RECORD_LEN),
            actual: record.len(),
        });
    }

    let (header, body) = record.split_at(HEADER_LEN);
    let (nonce, ciphertext) = body.split_at(NONCE_LEN);

    let aad = build_aad(
        header.try_into().expect("header length is validated above"),
        backup_path,
    );
    let cipher = XChaCha20Poly1305::new(key.into());
    let plaintext = cipher
        .decrypt(
            nonce.into(),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| BackupCryptoError::DecryptError)?;
    let generation = u64::from_be_bytes(
        plaintext[..GENERATION_LEN]
            .try_into()
            .expect("plaintext length is validated above"),
    );
    if let Some(min_generation) = min_generation {
        if generation < min_generation {
            return Err(BackupCryptoError::Rollback {
                generation,
                min_generation,
            });
        }
    }

    Ok((generation, plaintext[GENERATION_LEN..].to_vec()))
}

/// Decrypts and parses a backup envelope, deriving the decryption key from
/// the Pubky root secret via [`derive_backup_key()`].
///
/// Convenience wrapper around [`decrypt_backup_with_key()`] for callers that
/// hold the root identity secret directly.
pub fn decrypt_backup(
    root_secret: &[u8; 32],
    backup_path: &str,
    record: &[u8],
    min_generation: Option<u64>,
) -> Result<(u64, Vec<u8>), BackupCryptoError> {
    let key = derive_backup_key(root_secret);
    decrypt_backup_with_key(&key, backup_path, record, min_generation)
}

/// Draws a fresh random 192-bit nonce from the OS CSPRNG.
fn random_nonce() -> XNonce {
    let mut nonce = XNonce::default();
    getrandom::fill(&mut nonce).expect("OS entropy source must be available");
    nonce
}

/// Builds the 6-byte envelope header, authenticated as AEAD associated data.
fn envelope_header() -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(ENVELOPE_MAGIC);
    header[4] = ENVELOPE_VERSION;
    header[5] = ALG_XCHACHA20POLY1305;
    header
}

/// Builds the AEAD associated data: the 6-byte envelope header followed by
/// the intended backup path. The fixed-length header prefix makes the
/// concatenation unambiguous.
fn build_aad(header: &[u8; HEADER_LEN], backup_path: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(HEADER_LEN + backup_path.len());
    aad.extend_from_slice(header);
    aad.extend_from_slice(backup_path.as_bytes());
    aad
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snow_crypto::{HandshakePattern, NoisePhase, NoiseStep};

    fn test_state() -> PubkyNoiseSessionState {
        PubkyNoiseSessionState {
            version: crate::serializer::SESSION_STATE_VERSION,
            phase: NoisePhase::Transport,
            pattern: HandshakePattern::PatternNN,
            initiator: true,
            ephemeral_secret: [1; 32],
            static_secret: None,
            counter: 2,
            noise_step: NoiseStep::Final,
            sub_step_index: 0,
            handshake_hash: Some([2; 32]),
            link_id: Some([3; 32]),
            sending_nonce: 2,
            receiving_nonce: 1,
            write_counter: 9,
            read_counter: 7,
            endpoint_pubkey: [4; 32],
            handshake_messages: vec![vec![5; 48]],
        }
    }

    /// Path used as AAD throughout the tests.
    const TEST_PATH: &str = "/pub/test/backup";

    #[test]
    fn transcript_backup_roundtrip_and_authentication() {
        let mut state = test_state();
        state.pattern = HandshakePattern::PatternXX;
        state.initiator = false;
        state.counter = 3;
        for length in [32, crate::snow_crypto::PUBKY_NOISE_CIPHERTEXT_LEN] {
            state.handshake_messages = vec![vec![5; length], vec![6; length]];
            let record = encrypt_backup_with_key(&[42; 32], TEST_PATH, 7, &state);
            assert!(record.len() <= MAX_BACKUP_RECORD_LEN);
            assert_eq!(record[4], ENVELOPE_VERSION);
            let (generation, bytes) =
                decrypt_backup_with_key(&[42; 32], TEST_PATH, &record, Some(7)).unwrap();
            assert_eq!(generation, 7);
            assert_eq!(
                PubkyNoiseSessionState::deserialize(&bytes)
                    .unwrap()
                    .serialize(),
                state.serialize()
            );
            assert!(matches!(
                decrypt_backup_with_key(&[42; 32], TEST_PATH, &record, Some(8)),
                Err(BackupCryptoError::Rollback { .. })
            ));
            assert_eq!(
                decrypt_backup_with_key(&[42; 32], "/pub/another/backup", &record, None),
                Err(BackupCryptoError::DecryptError)
            );
            let mut tampered = record;
            *tampered.last_mut().unwrap() ^= 1;
            assert_eq!(
                decrypt_backup_with_key(&[42; 32], TEST_PATH, &tampered, None),
                Err(BackupCryptoError::DecryptError)
            );
        }
        let record = encrypt_backup_with_key(&[42; 32], TEST_PATH, 7, &state);
        assert_eq!(record.len(), MAX_BACKUP_RECORD_LEN);
        for length in [MIN_BACKUP_RECORD_LEN - 1, MAX_BACKUP_RECORD_LEN + 1] {
            let mut invalid = record.clone();
            invalid.resize(length, 0);
            assert!(matches!(
                decrypt_backup_with_key(&[42; 32], TEST_PATH, &invalid, None),
                Err(BackupCryptoError::InvalidLength { .. })
            ));
        }
    }

    /// Manually build a record encrypting an arbitrary `plaintext` with the
    /// standard header and `TEST_PATH` as AAD. Used to construct records the
    /// public API would never produce (non-standard plaintext lengths).
    fn craft_record(root_secret: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
        craft_record_for_path(root_secret, TEST_PATH, plaintext)
    }

    /// Like [`craft_record`], but binds the record to an arbitrary path.
    fn craft_record_for_path(root_secret: &[u8; 32], path: &str, plaintext: &[u8]) -> Vec<u8> {
        let key = derive_backup_key(root_secret);
        let header = envelope_header();
        let aad = build_aad(&header, path);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let nonce = random_nonce();
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .unwrap();

        let mut record = header.to_vec();
        record.extend_from_slice(&nonce);
        record.extend_from_slice(&ciphertext);
        record
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let root_secret = [42u8; 32];
        let state = test_state();

        let record = encrypt_backup(&root_secret, TEST_PATH, 7, &state);
        let (generation, plaintext) =
            decrypt_backup(&root_secret, TEST_PATH, &record, Some(7)).unwrap();

        assert_eq!(generation, 7);
        assert_eq!(plaintext, state.serialize());
    }

    #[test]
    fn record_length_includes_snapshot_and_envelope() {
        let root_secret = [42u8; 32];
        let record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());

        assert_eq!(
            record.len(),
            HEADER_LEN + NONCE_LEN + GENERATION_LEN + test_state().serialize().len() + TAG_LEN
        );
        assert_eq!(&record[..4], ENVELOPE_MAGIC);
        assert_eq!(record[4], ENVELOPE_VERSION);
        assert_eq!(record[5], ALG_XCHACHA20POLY1305);

        let initial = PubkyNoiseSessionState {
            phase: NoisePhase::HandShake,
            counter: 0,
            noise_step: NoiseStep::StepOne,
            sending_nonce: 0,
            receiving_nonce: 0,
            write_counter: 0,
            read_counter: 0,
            handshake_hash: None,
            link_id: None,
            handshake_messages: Vec::new(),
            ..test_state()
        };
        let record = encrypt_backup(&root_secret, TEST_PATH, 1, &initial);
        assert_eq!(record.len(), MIN_BACKUP_RECORD_LEN);
        let (_, bytes) = decrypt_backup(&root_secret, TEST_PATH, &record, None).unwrap();
        assert_eq!(bytes.len(), MIN_SESSION_STATE_LEN);
        assert!(PubkyNoiseSessionState::deserialize(&bytes).is_ok());
    }

    #[test]
    fn ciphertext_is_not_plaintext() {
        let root_secret = [42u8; 32];
        let state = test_state();

        let record = encrypt_backup(&root_secret, TEST_PATH, 1, &state);

        assert!(!record
            .windows(state.serialize().len())
            .any(|w| w == state.serialize().as_slice()));
    }

    #[test]
    fn random_nonce_produces_unique_ciphertexts() {
        let root_secret = [42u8; 32];
        let state = test_state();

        let a = encrypt_backup(&root_secret, TEST_PATH, 1, &state);
        let b = encrypt_backup(&root_secret, TEST_PATH, 1, &state);

        assert_ne!(a, b, "each encryption must use a fresh random nonce");
    }

    #[test]
    fn wrong_root_secret_fails() {
        let record = encrypt_backup(&[1u8; 32], TEST_PATH, 1, &test_state());

        assert_eq!(
            decrypt_backup(&[2u8; 32], TEST_PATH, &record, None),
            Err(BackupCryptoError::DecryptError)
        );
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let root_secret = [42u8; 32];
        let mut record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        let last = record.len() - 1;
        record[last] ^= 1;

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &record, None),
            Err(BackupCryptoError::DecryptError)
        );
    }

    #[test]
    fn tampered_nonce_fails() {
        let root_secret = [42u8; 32];
        let mut record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        record[HEADER_LEN] ^= 1;

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &record, None),
            Err(BackupCryptoError::DecryptError)
        );
    }

    #[test]
    fn header_is_authenticated_as_aad() {
        // The header is bound into the Poly1305 tag as associated data: a
        // ciphertext produced under one header cannot be re-framed under a
        // different one, even though the header travels in cleartext.
        let root_secret = [42u8; 32];
        let record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        let key = derive_backup_key(&root_secret);

        // Same nonce and ciphertext, but decrypt against a modified header.
        let mut other_header = envelope_header();
        other_header[5] = 2;
        let other_aad = build_aad(&other_header, TEST_PATH);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let result = cipher.decrypt(
            record[HEADER_LEN..HEADER_LEN + NONCE_LEN].into(),
            Payload {
                msg: &record[HEADER_LEN + NONCE_LEN..],
                aad: &other_aad,
            },
        );

        assert!(
            result.is_err(),
            "tag verification must fail on AAD mismatch"
        );
    }

    #[test]
    fn cross_path_substitution_fails() {
        // A record encrypted for one backup path must not decrypt against a
        // different path, even under the same key: a malicious homeserver
        // cannot substitute a backup from another session sharing the key.
        let root_secret = [42u8; 32];
        let state = test_state();
        let record = encrypt_backup(&root_secret, TEST_PATH, 1, &state);

        assert_eq!(
            decrypt_backup(&root_secret, "/pub/other/backup", &record, None),
            Err(BackupCryptoError::DecryptError)
        );

        // The intended path still decrypts, and a well-formed record
        // crafted for a foreign path is rejected at the intended path with
        // a tag mismatch (not a length or classification error).
        assert!(decrypt_backup(&root_secret, TEST_PATH, &record, None).is_ok());
        let mut plaintext = 9u64.to_be_bytes().to_vec();
        plaintext.extend_from_slice(&test_state().serialize());
        let foreign = craft_record_for_path(&root_secret, "/pub/other/backup", &plaintext);
        assert_eq!(foreign.len(), record.len());
        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &foreign, None),
            Err(BackupCryptoError::DecryptError)
        );
    }

    #[test]
    fn plaintext_snapshot_is_rejected() {
        let root_secret = [42u8; 32];
        let plaintext = test_state().serialize();

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &plaintext, None),
            Err(BackupCryptoError::InvalidMagic([1, 1, 1, 1]))
        );
    }

    #[test]
    fn bad_magic_is_classified() {
        let root_secret = [42u8; 32];
        let mut record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        record[0] ^= 1;

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &record, None),
            Err(BackupCryptoError::InvalidMagic([
                b'P' ^ 1,
                b'N',
                b'B',
                b'K'
            ]))
        );
    }

    #[test]
    fn unsupported_envelope_version_is_classified() {
        let root_secret = [42u8; 32];
        let mut record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        record[4] = 2;

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &record, None),
            Err(BackupCryptoError::UnsupportedEnvelopeVersion(2))
        );
    }

    #[test]
    fn unsupported_algorithm_is_classified() {
        let root_secret = [42u8; 32];
        let mut record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        record[5] = 9;

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &record, None),
            Err(BackupCryptoError::UnsupportedAlgorithm(9))
        );
    }

    #[test]
    fn truncated_record_fails_authentication() {
        let root_secret = [42u8; 32];
        let record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        let truncated = &record[..record.len() - 1];

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, truncated, None),
            Err(BackupCryptoError::DecryptError)
        );
    }

    #[test]
    fn appended_record_fails_authentication() {
        let root_secret = [42u8; 32];
        let mut record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        record.push(0);

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &record, None),
            Err(BackupCryptoError::DecryptError)
        );
    }

    #[test]
    fn oversized_record_is_rejected() {
        let root_secret = [42u8; 32];
        let mut record = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());
        record.resize(MAX_BACKUP_RESPONSE_BYTES + 1, 0);
        let actual = record.len();

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &record, None),
            Err(BackupCryptoError::InvalidLength {
                expected: MAX_BACKUP_RECORD_LEN,
                actual,
            })
        );
    }

    #[test]
    fn trailing_plaintext_is_rejected() {
        // The envelope authenticates bytes; the state parser rejects trailing data.
        let root_secret = [42u8; 32];
        let state = test_state();
        let mut plaintext = 1u64.to_be_bytes().to_vec();
        plaintext.extend_from_slice(&state.serialize());
        plaintext.push(0);
        let record = craft_record(&root_secret, &plaintext);
        let (_, bytes) = decrypt_backup(&root_secret, TEST_PATH, &record, None).unwrap();

        assert_eq!(
            PubkyNoiseSessionState::deserialize(&bytes).unwrap_err(),
            crate::serializer::SerializerError::TrailingBytes
        );
    }

    #[test]
    fn rolled_back_generation_is_rejected() {
        let root_secret = [42u8; 32];
        let record = encrypt_backup(&root_secret, TEST_PATH, 5, &test_state());

        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &record, Some(6)),
            Err(BackupCryptoError::Rollback {
                generation: 5,
                min_generation: 6,
            })
        );
    }

    #[test]
    fn current_and_newer_generations_are_accepted() {
        let root_secret = [42u8; 32];
        let record = encrypt_backup(&root_secret, TEST_PATH, 5, &test_state());

        assert!(decrypt_backup(&root_secret, TEST_PATH, &record, Some(5)).is_ok());
        assert!(decrypt_backup(&root_secret, TEST_PATH, &record, Some(4)).is_ok());
        assert!(decrypt_backup(&root_secret, TEST_PATH, &record, None).is_ok());
    }

    #[test]
    fn checkpoint_first_ordering_closes_the_upload_crash_window() {
        // Scenario: the caller persisted generation 2 to the homeserver and
        // crashed before (or after) advancing its trusted local checkpoint.
        // A stale/malicious homeserver then replays the generation-1 record.
        let root_secret = [42u8; 32];
        let replayed_gen_1 = encrypt_backup(&root_secret, TEST_PATH, 1, &test_state());

        // Required order (checkpoint advanced to 2 before/with the upload):
        // the replayed generation-1 record is rejected.
        assert_eq!(
            decrypt_backup(&root_secret, TEST_PATH, &replayed_gen_1, Some(2)),
            Err(BackupCryptoError::Rollback {
                generation: 1,
                min_generation: 2,
            })
        );

        // Wrong order (checkpoint still 1 because the process crashed after
        // the upload but before updating it): the replayed record is
        // silently accepted. This is why the checkpoint must be advanced
        // before (or atomically with) the upload.
        assert!(decrypt_backup(&root_secret, TEST_PATH, &replayed_gen_1, Some(1)).is_ok());
    }

    #[test]
    fn key_derivation_is_deterministic_and_domain_separated() {
        let root_secret = [42u8; 32];

        assert_eq!(
            derive_backup_key(&root_secret),
            derive_backup_key(&root_secret)
        );
        assert_ne!(
            derive_backup_key(&root_secret),
            Sha256::digest(root_secret).as_slice(),
            "derived key must differ from a bare hash of the secret"
        );
    }
}
