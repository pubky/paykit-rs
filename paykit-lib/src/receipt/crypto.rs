use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    XChaCha20Poly1305,
};

use crate::{PaykitError, Result};

use super::{
    wire::{EncryptedReceiptWire, ReceiptWire},
    Receipt, ReceiptAccess, ReceiptDecryptionKey,
};

impl Receipt {
    pub(crate) fn aad_for_location(location: &str) -> String {
        format!("paykit.receipt.v1:{location}")
    }

    /// Encrypt this receipt for storage at its canonical location using `key`.
    ///
    /// The location is derived from the Payment Reference and authenticated as
    /// AEAD associated data; callers must use that same canonical location when
    /// decrypting.
    pub fn encrypt(&self, key: &ReceiptDecryptionKey) -> Result<String> {
        let location = ReceiptAccess::location_for(&self.reference);
        let key_bytes = key.bytes()?;
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
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
                    aad: Self::aad_for_location(&location).as_bytes(),
                },
            )
            .map_err(|err| PaykitError::InvalidData {
                context: format!("failed to encrypt receipt: {err}"),
                source: None,
            })?;
        let wire = EncryptedReceiptWire {
            version: 1,
            kind: "paykit.receipt.encrypted".to_string(),
            algorithm: "XChaCha20Poly1305".to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        serde_json::to_string(&wire).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to serialize encrypted receipt JSON: {err}"),
            source: Some(err.into()),
        })
    }

    /// Decrypt an encrypted Receipt fetched from a homeserver.
    ///
    /// `key` and `location` normally come from a [`ReceiptAccess`] message. The
    /// location is authenticated as AEAD associated data and the decrypted
    /// Payment Reference must match the canonical location.
    pub fn decrypt(
        encrypted_json: &str,
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> Result<Self> {
        let wire: EncryptedReceiptWire =
            serde_json::from_str(encrypted_json).map_err(|err| PaykitError::InvalidData {
                context: format!("failed to parse encrypted receipt JSON: {err}"),
                source: Some(err.into()),
            })?;
        if wire.version != 1
            || wire.kind != "paykit.receipt.encrypted"
            || wire.algorithm != "XChaCha20Poly1305"
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
        let key_bytes = key.bytes()?;
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
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
        let receipt_wire: ReceiptWire =
            serde_json::from_slice(&plaintext).map_err(|err| PaykitError::InvalidData {
                context: format!("failed to parse receipt plaintext JSON: {err}"),
                source: Some(err.into()),
            })?;
        let receipt = Self::try_from(receipt_wire)?;
        if ReceiptAccess::location_for(&receipt.reference) != location {
            return Err(PaykitError::InvalidData {
                context: "Receipt Payment Reference does not match Receipt Location".into(),
                source: None,
            });
        }
        Ok(receipt)
    }
}

/// Decrypts an encrypted Receipt fetched from a homeserver.
///
/// `encrypted_json` is the public receipt object stored by
/// [`issue_receipt`](crate::issue_receipt).
/// `key` and `location` normally come from a [`ReceiptAccess`] message received
/// with [`get_receipt_access`](crate::get_receipt_access). The `location` is
/// authenticated as additional data, so decrypting with a different location
/// fails even when the key and ciphertext are otherwise correct.
///
/// Receipt Decryption Keys are sensitive. [`ReceiptDecryptionKey`] redacts its
/// `Debug` and `Display` output, but callers must still avoid logging values
/// returned by [`ReceiptDecryptionKey::as_str`].
///
/// # Errors
/// - Returns [`PaykitError::InvalidData`] if the encrypted envelope is malformed,
///   uses an unsupported version/kind/algorithm, has invalid base64url fields,
///   fails authenticated decryption, decrypts to malformed receipt JSON, or
///   decrypts to a receipt whose reference does not match the authenticated
///   Receipt Location.
pub fn decrypt_receipt(
    encrypted_json: &str,
    key: &ReceiptDecryptionKey,
    location: &str,
) -> Result<Receipt> {
    Receipt::decrypt(encrypted_json, key, location)
}
