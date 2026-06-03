use tracing::{debug, instrument};

use crate::{
    error::map_error, EncryptedLink, PaykitError, PrivateMessageKind, Result,
    PAYKIT_PRIVATE_PATH_PREFIX,
};

use super::{
    wire::serialize_receipt_access_json, PreparedReceipt, Receipt, ReceiptAccess,
    ReceiptDecryptionKey, ReceiptDraft, ReceiptId,
};

impl ReceiptAccess {
    /// Return the canonical Receipt Location path for a Receipt ID.
    pub fn location_for(receipt_id: &ReceiptId) -> String {
        format!("{PAYKIT_PRIVATE_PATH_PREFIX}/receipts/{receipt_id}")
    }

    /// Validate that this access descriptor points at the canonical location for
    /// its Receipt ID.
    ///
    /// This public validator is for caller-supplied values and returns
    /// [`PaykitError::Validation`] on mismatch. Wire parsing maps the same
    /// mismatch to [`PaykitError::InvalidData`] because incoming private message
    /// payloads are external data.
    pub fn validate_location(&self) -> Result<()> {
        if !self.has_canonical_location() {
            return Err(PaykitError::Validation(
                "Receipt Access location does not match Receipt ID".into(),
            ));
        }
        Ok(())
    }

    /// Validate caller-supplied Receipt Access before sending or storing.
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(PaykitError::Validation(
                "Receipt Access version must be 1".into(),
            ));
        }
        if self.kind != PrivateMessageKind::ReceiptAccess {
            return Err(PaykitError::Validation(
                "Receipt Access kind must be paykit.receipt_access".into(),
            ));
        }
        self.validate_request_context()?;
        self.validate_location()
    }

    pub(crate) fn validate_wire_location(&self) -> Result<()> {
        if !self.has_canonical_location() {
            return Err(PaykitError::InvalidData {
                context: "Receipt Access location does not match Receipt ID".into(),
                source: None,
            });
        }
        Ok(())
    }

    fn has_canonical_location(&self) -> bool {
        let expected_location = Self::location_for(&self.receipt_id);
        self.location == expected_location
    }
}

/// Prepare a plaintext Receipt, Encrypted Receipt, and Receipt Access
/// descriptor without touching the network.
///
/// The returned [`PreparedReceipt`] contains the Receipt Decryption Key and
/// must be handled as sensitive data.
#[instrument(skip(link, draft))]
pub fn prepare_receipt(link: &EncryptedLink, draft: ReceiptDraft) -> Result<PreparedReceipt> {
    debug!("preparing encrypted receipt");
    draft.validate_request_context()?;
    let receipt_id = draft.receipt_id.unwrap_or_else(ReceiptId::new_v4);
    if let Some(amount) = &draft.amount {
        amount.validate_with_label("Receipt amount")?;
    }
    let payment_reference = draft.payment_reference;
    let payment_request_id = draft.payment_request_id;
    let billing_period = draft.billing_period;
    let location = ReceiptAccess::location_for(&receipt_id);
    let key = ReceiptDecryptionKey::generate();
    let receipt = Receipt {
        receipt_id: receipt_id.clone(),
        payment_reference: payment_reference.clone(),
        payment_request_id: payment_request_id.clone(),
        billing_period: billing_period.clone(),
        recipient_public_key: link.recipient().clone(),
        payment_endpoint_identifier: draft.payment_endpoint_identifier,
        amount: draft.amount,
        metadata: draft.metadata,
    };
    let encrypted_receipt = receipt
        .encrypt(&key)
        .map_err(|err| map_error("prepare_receipt", err))?;
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: crate::EventId::new_v4(),
        receipt_id,
        payment_reference,
        payment_request_id,
        billing_period,
        location,
        key,
    };

    Ok(PreparedReceipt {
        receipt,
        encrypted_receipt,
        access,
    })
}

/// Store a prepared Encrypted Receipt at its Receipt Location.
///
/// This only performs the homeserver write; send the access descriptor with
/// [`send_receipt_access`].
#[instrument(skip(session, prepared))]
pub async fn store_prepared_receipt(
    session: &pubky::PubkySession,
    prepared: &PreparedReceipt,
) -> Result<()> {
    debug!("storing prepared encrypted receipt");
    validate_prepared_receipt(prepared)?;

    session
        .storage()
        .put(
            prepared.access.location.clone(),
            prepared.encrypted_receipt.clone(),
        )
        .await
        .map_err(|err| PaykitError::Transport {
            context: format!(
                "failed to store encrypted receipt at {}",
                prepared.access.location
            ),
            source: err.into(),
        })?;

    Ok(())
}

pub(super) fn validate_prepared_receipt(prepared: &PreparedReceipt) -> Result<()> {
    prepared.access.validate()?;
    if prepared.receipt.receipt_id != prepared.access.receipt_id {
        return Err(PaykitError::Validation(
            "Prepared Receipt plaintext Receipt ID does not match Receipt Access".into(),
        ));
    }
    if prepared.receipt.payment_reference != prepared.access.payment_reference {
        return Err(PaykitError::Validation(
            "Prepared Receipt plaintext Payment Reference does not match Receipt Access".into(),
        ));
    }
    if prepared.receipt.payment_request_id != prepared.access.payment_request_id {
        return Err(PaykitError::Validation(
            "Prepared Receipt plaintext Payment Request ID does not match Receipt Access".into(),
        ));
    }
    if prepared.receipt.billing_period != prepared.access.billing_period {
        return Err(PaykitError::Validation(
            "Prepared Receipt plaintext Billing Period does not match Receipt Access".into(),
        ));
    }
    let decrypted = Receipt::decrypt(
        &prepared.encrypted_receipt,
        &prepared.access.key,
        &prepared.access.location,
    )
    .map_err(|err| {
        PaykitError::Validation(format!(
            "Prepared Receipt encrypted payload does not decrypt with Receipt Access: {err}"
        ))
    })?;

    if decrypted != prepared.receipt {
        return Err(PaykitError::Validation(
            "Prepared Receipt encrypted payload does not match plaintext Receipt".into(),
        ));
    }

    Ok(())
}

/// Send a prepared Receipt Access descriptor over an Encrypted Link.
#[instrument(skip(link, access))]
pub async fn send_receipt_access(link: &mut EncryptedLink, access: &ReceiptAccess) -> Result<()> {
    debug!("sending Receipt Access message");
    access.validate()?;
    let json = serialize_receipt_access_json(access)
        .map_err(|err| map_error("send_receipt_access", err))?;
    link.send_receipt_access_message(json.as_bytes())
        .await
        .map_err(|err| map_error("send_receipt_access", err))
}
