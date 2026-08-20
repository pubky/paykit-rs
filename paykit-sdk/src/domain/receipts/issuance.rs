use chrono::{DateTime, Utc};
use paykit_lib::{PrivateMessageKind, ReceiptDraft};

use super::records::ReceiptIssuanceRecord;
use crate::{
    domain::outbound_private::validate_outbound_private_message,
    storage::{require_paykit_app_capability, NewOutboundPrivateMessage, StorageAdapter},
    PaykitSdkError, PubkyPublicKey, Result,
};

pub(crate) async fn store_encrypted_receipt_json(
    session: &pubky::PubkySession,
    record: &ReceiptIssuanceRecord,
) -> Result<()> {
    session
        .storage()
        .put(record.location.clone(), record.encrypted_receipt.clone())
        .await
        .map(|_| ())
        .map_err(|err| store_encrypted_receipt_error(&record.location, err.into()))
}

/// Build a transport error without exposing the private Receipt Location.
///
/// The static context is safe to cross the FFI boundary; the detailed source
/// remains available to Rust callers.
pub(super) fn store_encrypted_receipt_error(
    _location: &str,
    source: anyhow::Error,
) -> PaykitSdkError {
    PaykitSdkError::Transport {
        context: "failed to store encrypted receipt".into(),
        source: Some(source),
    }
}

pub(crate) fn receipt_issuance_record_matches_draft(
    record: &ReceiptIssuanceRecord,
    draft: &ReceiptDraft,
) -> Result<bool> {
    let access = paykit_lib::parse_receipt_access_json(&record.access_json)?;
    let receipt =
        paykit_lib::decrypt_receipt(&record.encrypted_receipt, &access.key, &access.location)?;
    let expected_receipt_id = draft.receipt_id.as_ref().map(|id| id.as_str());
    let expected_recipient = record.counterparty.to_public_key()?;
    Ok(Some(receipt.receipt_id.as_str()) == expected_receipt_id
        && receipt.payment_reference == draft.payment_reference
        && receipt.payment_request_id == draft.payment_request_id
        && receipt.billing_period == draft.billing_period
        && receipt.recipient_public_key == expected_recipient
        && receipt.payment_endpoint_identifier == draft.payment_endpoint_identifier
        && receipt.amount == draft.amount
        && receipt.metadata == draft.metadata)
}

pub(crate) async fn receipt_issuance_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<ReceiptIssuanceRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_issuance_records(counterparty)))
        .await
}

pub(crate) async fn receipt_issuance_record<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    receipt_id: &str,
) -> Result<Option<ReceiptIssuanceRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_issuance_record(counterparty, receipt_id)))
        .await
}

pub(crate) async fn receipt_issuance_record_by_receipt_id<S>(
    storage: &S,
    receipt_id: &str,
) -> Result<Option<ReceiptIssuanceRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_issuance_record_by_receipt_id(receipt_id)))
        .await
}

pub(crate) async fn enqueue_receipt_access_for_issuance<S>(
    storage: &S,
    record: ReceiptIssuanceRecord,
    now: DateTime<Utc>,
) -> Result<ReceiptIssuanceRecord>
where
    S: StorageAdapter,
{
    if record.outbound_message_id.is_some() {
        return Ok(record);
    }
    let (app_id, kind) = validate_outbound_private_message(&record.access_json)?;
    if kind != paykit_lib::PrivateMessageKind::ReceiptAccess.as_str() {
        return Err(PaykitSdkError::Protocol {
            context: "receipt issuance access JSON is not Receipt Access".into(),
            source: None,
        });
    }
    storage
        .transaction(move |tx| {
            require_paykit_app_capability(tx, &app_id, PrivateMessageKind::ReceiptAccess)?;
            let outbound = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                record.counterparty.clone(),
                app_id,
                kind,
                record.access_json.clone(),
                now,
            ));
            let queued = record.mark_access_queued(outbound.outbound_message_id, now);
            tx.save_receipt_issuance_record(queued.clone());
            Ok(queued)
        })
        .await
}
