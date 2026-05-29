use tracing::{debug, instrument, warn};

use crate::{
    error::map_error, EncryptedLink, PaykitError, PaymentReference, PrivateMessageKind, Result,
    PAYKIT_PATH_PREFIX,
};

use super::{
    wire::{parse_receipt_access_json, serialize_receipt_access_json},
    IssuedReceipt, Receipt, ReceiptAccess, ReceiptDecryptionKey, ReceiptDraft,
};

impl ReceiptAccess {
    /// Return the canonical homeserver storage location for a Payment Reference.
    pub fn location_for(reference: &PaymentReference) -> String {
        format!(
            "{}private/receipts/{}",
            PAYKIT_PATH_PREFIX,
            reference.as_str()
        )
    }

    /// Validate that this access descriptor points at the canonical location for
    /// its Payment Reference.
    pub fn validate_location(&self) -> Result<()> {
        let expected_location = Self::location_for(&self.reference);
        if self.location != expected_location {
            return Err(PaykitError::InvalidData {
                context: "Receipt Access location does not match Payment Reference".into(),
                source: None,
            });
        }
        Ok(())
    }
}

/// Issues, stores, and shares an encrypted payment receipt with the counterparty
/// over an Encrypted Link.
///
/// The encrypted receipt is written to the caller's homeserver at a deterministic
/// Receipt Location derived from `draft.reference`. A fresh symmetric
/// [`ReceiptDecryptionKey`] is generated for each call. The corresponding
/// [`ReceiptAccess`] descriptor is then sent over the existing Noise channel so
/// the counterparty can fetch and decrypt the stored receipt with
/// [`decrypt_receipt`](crate::decrypt_receipt).
///
/// Receipt Access messages are Event Messages: every valid access descriptor matters.
/// Reissuing the same [`PaymentReference`] stores a new encrypted receipt at the
/// same location with a new key, so older access descriptors for that reference
/// may no longer decrypt after a later successful reissue.
///
/// # Identity binding
///
/// `session` is used for homeserver storage, while `link` is used to send the
/// Receipt Access message. Paykit does not currently verify that `session`
/// belongs to the same local identity that established `link`; callers must pass
/// the matching session or they may persist the receipt under the wrong identity
/// while sending access over a different Encrypted Link.
///
/// # Durability and ordering
///
/// This function stores the encrypted receipt first and sends access second. If
/// the process crashes, or the Noise send fails after storage succeeds, the
/// encrypted receipt may remain on the homeserver without the counterparty ever
/// receiving access. Callers that need stronger delivery guarantees should keep
/// their own durable issuance state and retry or reconcile at the application
/// layer.
///
/// # Secrets
///
/// The returned [`IssuedReceipt::key`] is sensitive decryption material. Paykit
/// redacts it from `Debug` and `Display`, but callers must not log or persist the
/// raw [`ReceiptDecryptionKey::as_str`] value outside secure storage.
///
/// # Errors
/// - Returns [`PaykitError::InvalidData`] if receipt serialization or encryption
///   fails.
/// - Returns [`PaykitError::Transport`] if storing the encrypted receipt fails or
///   the Receipt Access Noise message cannot be sent after configured retries.
#[instrument(skip(session, link, draft))]
pub async fn issue_receipt(
    session: &pubky::PubkySession,
    link: &mut EncryptedLink,
    draft: ReceiptDraft,
) -> Result<IssuedReceipt> {
    debug!("issuing encrypted receipt");
    let reference = draft.reference;
    let location = ReceiptAccess::location_for(&reference);
    let key = ReceiptDecryptionKey::generate();
    let receipt = Receipt {
        reference: reference.clone(),
        recipient_public_key: link.recipient().clone(),
        payment_endpoint_identifier: draft.payment_endpoint_identifier,
        amount: draft.amount,
        currency: draft.currency,
        metadata: draft.metadata,
    };
    let encrypted = receipt
        .encrypt(&key)
        .map_err(|err| map_error("issue_receipt", err))?;

    session
        .storage()
        .put(location.clone(), encrypted)
        .await
        .map_err(|err| PaykitError::Transport {
            context: format!("failed to store encrypted receipt at {location}"),
            source: err.into(),
        })?;

    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        reference: reference.clone(),
        location: location.clone(),
        key: key.clone(),
        algorithm: "XChaCha20Poly1305".to_string(),
    };
    let json =
        serialize_receipt_access_json(&access).map_err(|err| map_error("issue_receipt", err))?;
    link.send_receipt_access_message(json.as_bytes())
        .await
        .map_err(|err| map_error("issue_receipt", err))?;

    Ok(IssuedReceipt {
        reference,
        location,
        key,
    })
}

/// Receives all currently available Receipt Access descriptors from the Encrypted Link.
///
/// Unlike [`crate::get_private_payment_envelope`], Receipt Access uses Event Message
/// semantics. Every currently available Receipt Access message is returned in
/// send order in a single vector; older Receipt Access messages are not collapsed
/// when newer ones arrive.
/// Returns an empty vector when no Receipt Access messages are currently available.
///
/// Messages for other supported private app kinds remain buffered on the
/// [`EncryptedLink`] for their own typed receiver. Malformed unrelated app
/// messages are ignored by the shared dispatcher. Syntactically valid messages
/// with unsupported `kind` values are logged and dropped by the shared
/// dispatcher rather than buffered indefinitely. Malformed Receipt Access
/// messages are dropped with diagnostics while later valid Receipt Access
/// messages in the same batch are still returned.
///
/// Each selected Receipt Access location must match the canonical Paykit
/// Receipt Location for its [`PaymentReference`].
///
/// The returned [`ReceiptAccess::key`] values are sensitive. Their formatting is
/// redacted, but callers must still avoid logging raw key material from
/// [`ReceiptDecryptionKey::as_str`].
#[instrument(skip(link))]
pub async fn get_receipt_access(link: &mut EncryptedLink) -> Result<Vec<ReceiptAccess>> {
    debug!("receiving Receipt Access messages");

    let (received, raw_messages, pending) = link.receive_receipt_access_messages().await?;
    if raw_messages.is_empty() {
        debug!(received, "no Receipt Access messages available");
        return Ok(Vec::new());
    }

    let mut access = Vec::new();
    let mut malformed = 0usize;
    for raw in &raw_messages {
        match parse_receipt_access_json(raw) {
            Ok(parsed) => access.push(parsed),
            Err(err) => {
                malformed += 1;
                warn!(
                    error = ?err,
                    "dropping malformed Receipt Access message while preserving later valid messages"
                );
            }
        }
    }
    if malformed > 0 {
        warn!(
            malformed,
            selected = raw_messages.len(),
            "ignored malformed Receipt Access messages while preserving valid messages"
        );
    }
    debug!(
        count = access.len(),
        received, pending, "Receipt Access messages received"
    );
    Ok(access)
}
