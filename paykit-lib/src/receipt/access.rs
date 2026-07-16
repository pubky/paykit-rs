use tracing::{debug, instrument};

use crate::{
    error::map_error,
    pubky_routing::{receipt_path_prefix, PAYKIT_PRIVATE_PATH_PREFIX},
    EncryptedLink, PaykitError, PaykitReceiverPath, PrivateMessageKind, PublicKey, Result,
};

use super::{
    wire::serialize_receipt_access_json, PreparedReceipt, Receipt, ReceiptAccess,
    ReceiptDecryptionKey, ReceiptDraft, ReceiptId,
};

impl ReceiptAccess {
    /// Return the canonical Receipt Location path for a receiver and Receipt ID.
    pub fn location(receiver_path: &PaykitReceiverPath, receipt_id: &ReceiptId) -> String {
        format!("{}/{receipt_id}", receipt_path_prefix(receiver_path))
    }

    /// Return true when a Receipt Location points at the expected receiver
    /// folder and Receipt ID.
    pub fn location_matches_receiver_path(
        location: &str,
        receiver_path: &PaykitReceiverPath,
        receipt_id: &ReceiptId,
    ) -> bool {
        location == Self::location(receiver_path, receipt_id)
    }

    /// Return true when this descriptor points at the expected receiver path.
    pub fn has_location_for_receiver(&self, receiver_path: &PaykitReceiverPath) -> bool {
        Self::location_matches_receiver_path(&self.location, receiver_path, &self.receipt_id)
    }

    /// Validate that this access descriptor points at a canonical
    /// receiver-scoped location for its Receipt ID.
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
        Self::location_matches_receipt_id(&self.location, &self.receipt_id)
    }

    pub(crate) fn location_matches_receipt_id(location: &str, receipt_id: &ReceiptId) -> bool {
        receiver_scoped_location_matches(location, receipt_id)
    }
}

fn receiver_scoped_location_matches(location: &str, receipt_id: &ReceiptId) -> bool {
    let private_prefix = format!("{PAYKIT_PRIVATE_PATH_PREFIX}/");
    let Some(rest) = location.strip_prefix(&private_prefix) else {
        return false;
    };
    let Some((receiver_path, receipt_segment)) = rest.split_once("/receipts/") else {
        return false;
    };
    if receipt_segment != receipt_id.as_str() {
        return false;
    }
    PaykitReceiverPath::new(receiver_path).is_ok()
}

/// Prepare a plaintext Receipt, Encrypted Receipt, and Receipt Access
/// descriptor without touching the network.
///
/// The returned [`PreparedReceipt`] contains the Receipt Decryption Key and
/// must be handled as sensitive data.
#[instrument(skip(link, draft))]
pub fn prepare_receipt(
    link: &EncryptedLink,
    receiver_path: &PaykitReceiverPath,
    draft: ReceiptDraft,
) -> Result<PreparedReceipt> {
    if receiver_path != link.local_receiver_path() {
        return Err(PaykitError::Validation(format!(
            "Receipt receiver path {receiver_path} does not match Encrypted Link local receiver {}",
            link.local_receiver_path()
        )));
    }
    prepare_receipt_for_recipient(link.recipient().clone(), receiver_path, draft)
}

/// Prepare a plaintext Receipt, Encrypted Receipt, and Receipt Access
/// descriptor for an explicit recipient public key.
///
/// This is useful for stateful runtimes that queue Receipt Access delivery
/// separately from receipt preparation.
#[instrument(skip(recipient_public_key, draft))]
pub fn prepare_receipt_for_recipient(
    recipient_public_key: PublicKey,
    receiver_path: &PaykitReceiverPath,
    draft: ReceiptDraft,
) -> Result<PreparedReceipt> {
    prepare_receipt_for_recipient_at_location(recipient_public_key, draft, |receipt_id| {
        ReceiptAccess::location(receiver_path, receipt_id)
    })
}

fn prepare_receipt_for_recipient_at_location(
    recipient_public_key: PublicKey,
    draft: ReceiptDraft,
    location_for: impl FnOnce(&ReceiptId) -> String,
) -> Result<PreparedReceipt> {
    debug!("preparing encrypted receipt");
    draft.validate_request_context()?;
    let receipt_id = draft.receipt_id.unwrap_or_else(ReceiptId::new_v4);
    if let Some(amount) = &draft.amount {
        amount.validate_with_label("Receipt amount")?;
    }
    let payment_reference = draft.payment_reference;
    let payment_request_id = draft.payment_request_id;
    let billing_period = draft.billing_period;
    let location = location_for(&receipt_id);
    let key = ReceiptDecryptionKey::generate();
    let receipt = Receipt {
        receipt_id: receipt_id.clone(),
        payment_reference: payment_reference.clone(),
        payment_request_id: payment_request_id.clone(),
        billing_period: billing_period.clone(),
        recipient_public_key,
        payment_endpoint_identifier: draft.payment_endpoint_identifier,
        amount: draft.amount,
        metadata: draft.metadata,
    };
    let encrypted_receipt = receipt
        .encrypt_for_location(&key, &location)
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
        .map_err(|err| store_prepared_receipt_error(&prepared.access.location, err.into()))?;

    Ok(())
}

/// Build the transport error returned when storing a Prepared Receipt fails.
///
/// SECURITY / REDACTION: `location` is the Receipt Location
/// (`/pub/paykit/v0/private/.../receipts/{id}`), a PRIVATE storage path whose
/// folder prefix is DH-derived per counterparty. This `context` can be rendered
/// verbatim into a caller-facing error message (and, once wired, the FFI
/// Kotlin/Swift exception), so the Receipt Location MUST NOT be embedded in it.
/// The location is accepted here only to make the redaction explicit and
/// testable; it is deliberately dropped, leaving a static, non-sensitive label.
/// The concrete cause stays in `source`, which is not rendered across the FFI.
pub(crate) fn store_prepared_receipt_error(_location: &str, source: anyhow::Error) -> PaykitError {
    PaykitError::Transport {
        context: "failed to store encrypted receipt".into(),
        source,
    }
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
    if !access.has_location_for_receiver(link.local_receiver_path()) {
        return Err(PaykitError::Validation(format!(
            "Receipt Access location does not match Encrypted Link local receiver {}",
            link.local_receiver_path()
        )));
    }
    let json = serialize_receipt_access_json(access)
        .map_err(|err| map_error("send_receipt_access", err))?;
    link.send_receipt_access_message(json.as_bytes())
        .await
        .map_err(|err| map_error("send_receipt_access", err))
}
