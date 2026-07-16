use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    XChaCha20Poly1305,
};

use zeroize::Zeroize;

use crate::validation::invalid_plaintext_json;
use crate::{PaykitError, PaykitReceiverPath, Result};

use super::{
    wire::{EncryptedReceiptWire, ReceiptWire},
    Receipt, ReceiptAccess, ReceiptDecryptionKey, RECEIPT_ENCRYPTION_ALGORITHM,
};

impl Receipt {
    pub(super) fn aad_for_location(location: &str) -> String {
        format!("paykit.receipt.v1:{location}")
    }

    /// Encrypt this receipt for storage at its canonical Receipt Location path
    /// using `key`.
    ///
    /// The location path is derived from the issuer receiver path and Receipt ID
    /// and authenticated as AEAD associated data; callers must use that same
    /// canonical path when decrypting.
    pub fn encrypt(
        &self,
        receiver_path: &PaykitReceiverPath,
        key: &ReceiptDecryptionKey,
    ) -> Result<String> {
        let location = ReceiptAccess::location(receiver_path, &self.receipt_id);
        self.encrypt_for_location(key, &location)
    }

    pub(crate) fn encrypt_for_location(
        &self,
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> Result<String> {
        self.validate_request_context()?;
        if let Some(amount) = &self.amount {
            amount.validate_with_label("Receipt amount")?;
        }
        if !ReceiptAccess::location_matches_receipt_id(location, &self.receipt_id) {
            return Err(PaykitError::Validation(
                "Receipt Location does not match Receipt ID".into(),
            ));
        }
        let mut key_bytes = key.bytes()?;
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        // Scrub the raw key from the stack; the cipher holds its own key schedule.
        key_bytes.zeroize();
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let plaintext = serde_json::to_vec(&ReceiptWire::from(self)).map_err(|err| {
            PaykitError::InvalidData {
                context: format!("failed to serialize receipt JSON: {err}"),
                source: Some(err.into()),
            }
        })?;
        let ciphertext = cipher
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: &plaintext,
                    aad: Self::aad_for_location(location).as_bytes(),
                },
            )
            .map_err(|err| PaykitError::InvalidData {
                context: format!("failed to encrypt receipt: {err}"),
                source: None,
            })?;
        let wire = EncryptedReceiptWire {
            version: 1,
            kind: "paykit.receipt.encrypted".to_string(),
            algorithm: RECEIPT_ENCRYPTION_ALGORITHM.to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        serde_json::to_string(&wire).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to serialize encrypted receipt JSON: {err}"),
            source: Some(err.into()),
        })
    }

    /// Decrypt an Encrypted Receipt fetched from a homeserver.
    ///
    /// `key` and `location` normally come from a [`ReceiptAccess`] message. The
    /// location path is authenticated as AEAD associated data and the decrypted
    /// Receipt ID must match the canonical path.
    pub fn decrypt(
        encrypted_json: &str,
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> Result<Self> {
        // The serde error can embed fragments of the fetched document; keep the
        // context static and leave the detail in `source`, which stays local.
        let wire: EncryptedReceiptWire =
            serde_json::from_str(encrypted_json).map_err(|err| PaykitError::InvalidData {
                context: "failed to parse encrypted receipt JSON".into(),
                source: Some(err.into()),
            })?;
        if wire.version != 1
            || wire.kind != "paykit.receipt.encrypted"
            || wire.algorithm != RECEIPT_ENCRYPTION_ALGORITHM
        {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "unsupported encrypted receipt envelope version/kind/algorithm: {}/{}/{}",
                    wire.version, wire.kind, wire.algorithm
                ),
                source: None,
            });
        }
        let nonce = URL_SAFE_NO_PAD
            .decode(wire.nonce)
            .map_err(|err| PaykitError::InvalidData {
                context: format!("encrypted receipt nonce is not valid base64url: {err}"),
                source: Some(err.into()),
            })?;
        let ciphertext =
            URL_SAFE_NO_PAD
                .decode(wire.ciphertext)
                .map_err(|err| PaykitError::InvalidData {
                    context: format!("encrypted receipt ciphertext is not valid base64url: {err}"),
                    source: Some(err.into()),
                })?;
        if nonce.len() != 24 {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "encrypted receipt nonce must be 24 bytes, got {}",
                    nonce.len()
                ),
                source: None,
            });
        }
        let mut key_bytes = key.bytes()?;
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        // Scrub the raw key from the stack; the cipher holds its own key schedule.
        key_bytes.zeroize();
        let plaintext = cipher
            .decrypt(
                nonce.as_slice().into(),
                chacha20poly1305::aead::Payload {
                    msg: &ciphertext,
                    aad: Self::aad_for_location(location).as_bytes(),
                },
            )
            .map_err(|err| PaykitError::InvalidData {
                context: format!("failed to decrypt receipt: {err}"),
                source: None,
            })?;
        // SECURITY / REDACTION: the serde error is derived from DECRYPTED
        // receipt plaintext (its Display embeds field values on type
        // mismatches), so it must not be folded into the context or kept as
        // `source` -- this error crosses the FFI boundary as exception text.
        let receipt_wire: ReceiptWire = serde_json::from_slice(&plaintext)
            .map_err(|_| invalid_plaintext_json("failed to parse receipt plaintext JSON"))?;
        let receipt = Self::try_from(receipt_wire)?;
        if !ReceiptAccess::location_matches_receipt_id(location, &receipt.receipt_id) {
            return Err(PaykitError::InvalidData {
                context: "Receipt ID does not match Receipt Location".into(),
                source: None,
            });
        }
        Ok(receipt)
    }
}

/// Decrypts an Encrypted Receipt fetched from a homeserver.
///
/// `encrypted_json` is the stored Encrypted Receipt JSON. `key` and `location`
/// normally come from a [`ReceiptAccess`] message. The `location` path is
/// authenticated as additional data, and the decrypted Receipt ID must match
/// that path.
///
/// Receipt Decryption Keys are sensitive. [`ReceiptDecryptionKey`] redacts its
/// `Debug` and `Display` output, but callers must still avoid logging raw values
/// returned by [`ReceiptDecryptionKey::as_str`].
///
/// # Errors
/// Returns [`PaykitError::InvalidData`] if the encrypted receipt is malformed,
/// fails authenticated decryption, or decrypts to receipt data that does not
/// match the Receipt Location.
pub fn decrypt_receipt(
    encrypted_json: &str,
    key: &ReceiptDecryptionKey,
    location: &str,
) -> Result<Receipt> {
    Receipt::decrypt(encrypted_json, key, location)
}
