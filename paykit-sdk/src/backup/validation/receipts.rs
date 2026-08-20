use super::{
    super::*,
    private_stream::{app_id_from_raw_json, private_application_message},
};

pub(in crate::backup) fn validate_receipt_access_records(
    records: &HashMap<(PubkyPublicKey, String), ReceiptAccessRecord>,
    stream_items: &[PrivateStreamItemRecord],
) -> Result<()> {
    let stream_by_id = stream_items
        .iter()
        .map(|item| (item.stream_item_id, item))
        .collect::<HashMap<_, _>>();
    for record in records.values() {
        validate_receipt_access_retrieval_status(record)?;
        let Some(item) = stream_by_id.get(&record.stream_item_id) else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' references missing stream item {}",
                    record.event_id, record.stream_item_id
                ),
                source: None,
            });
        };
        if item.counterparty != record.counterparty
            || item.receive_batch_id != record.receive_batch_id
            || item.known_paykit_kind.as_deref() != Some(PrivateMessageKind::ReceiptAccess.as_str())
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' does not match its stream item",
                    record.event_id
                ),
                source: None,
            });
        }
        let event = paykit_lib::parse_receipt_access_event_message(&private_application_message(
            item,
            PrivateMessageKind::ReceiptAccess,
        ))
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: format!(
                "Receipt Access record '{}' stream item is not parseable",
                record.event_id
            ),
            source: None,
        })?;
        let Some(access) = event.parsed_access() else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' stream item is malformed",
                    record.event_id
                ),
                source: None,
            });
        };
        if event.app_id() != Some(&record.app_id)
            || access.event_id.as_str() != record.event_id
            || access.receipt_id.as_str() != record.receipt_id
            || access.payment_reference.as_str() != record.payment_reference
            || access
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned())
                != record.payment_request_id
            || access
                .billing_period
                .as_ref()
                .map(BillingPeriodRecord::from)
                != record.billing_period
            || access.location != record.location
            || access.key.as_str() != record.key
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' does not match parsed stream payload",
                    record.event_id
                ),
                source: None,
            });
        }
    }
    Ok(())
}

fn validate_receipt_access_retrieval_status(record: &ReceiptAccessRecord) -> Result<()> {
    match record.retrieval_status {
        ReceiptRetrievalStatus::Pending => {
            if record.retrieval_attempted_at.is_some()
                || record.retrieved_at.is_some()
                || record.last_retrieval_error.is_some()
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "pending Receipt Access record '{}' has retrieval metadata",
                        record.event_id
                    ),
                    source: None,
                });
            }
        }
        ReceiptRetrievalStatus::Retrieved => {
            if record.retrieval_attempted_at.is_none()
                || record.retrieved_at.is_none()
                || record.last_retrieval_error.is_some()
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "retrieved Receipt Access record '{}' has inconsistent retrieval metadata",
                        record.event_id
                    ),
                    source: None,
                });
            }
        }
        ReceiptRetrievalStatus::NotFound | ReceiptRetrievalStatus::Failed => {
            if record.retrieval_attempted_at.is_none()
                || record.retrieved_at.is_some()
                || record.last_retrieval_error.is_none()
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "failed Receipt Access record '{}' has inconsistent retrieval metadata",
                        record.event_id
                    ),
                    source: None,
                });
            }
        }
    }
    Ok(())
}
pub(in crate::backup) fn validate_receipt_records(
    records: &HashMap<(PubkyPublicKey, String), ReceiptRecord>,
    access_records: &HashMap<(PubkyPublicKey, String), ReceiptAccessRecord>,
    event_dedup_records: &HashMap<(PubkyPublicKey, String), EventDedupRecord>,
    expected_recipient: Option<&PubkyPublicKey>,
) -> Result<()> {
    for record in records.values() {
        ReceiptId::new(&record.receipt_id)?;
        if let Some(expected_recipient) = expected_recipient {
            if &record.recipient_public_key != expected_recipient {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Receipt record '{}' recipient does not match backup identity",
                        record.receipt_id
                    ),
                    source: None,
                });
            }
        }
        if let Some(identifier) = record.payment_endpoint_identifier.as_ref() {
            PaymentEndpointIdentifier::new(identifier)?;
        }
        let access_key = (
            record.issuer.clone(),
            record.receipt_access_event_id.clone(),
        );
        let Some(access) = access_records.get(&access_key) else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt record '{}' references missing Receipt Access event '{}'",
                    record.receipt_id, record.receipt_access_event_id
                ),
                source: None,
            });
        };
        if !access.app_authorized
            || access.retrieval_status != ReceiptRetrievalStatus::Retrieved
            || access.receipt_id != record.receipt_id
            || access.app_id != record.app_id
            || access.payment_reference != record.payment_reference
            || access.payment_request_id != record.payment_request_id
            || access.billing_period != record.billing_period
            || access.location != record.location
            || receipt_access_key_hash(&access.key) != record.receipt_access_key_hash
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt record '{}' does not match its Receipt Access record",
                    record.receipt_id
                ),
                source: None,
            });
        }
        for candidate in access_records.values().filter(|candidate| {
            candidate.counterparty == record.issuer
                && candidate.receipt_id == record.receipt_id
                && candidate.app_authorized
        }) {
            let conflicted = event_dedup_records
                .get(&(candidate.counterparty.clone(), candidate.event_id.clone()))
                .is_some_and(|dedupe| !dedupe.conflicting_stream_item_ids.is_empty());
            if conflicted || !receipt_record_matches_access(record, candidate) {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Receipt record '{}' has conflicting authorized Receipt Access",
                        record.receipt_id
                    ),
                    source: None,
                });
            }
        }
    }
    Ok(())
}

pub(in crate::backup) fn validate_receipt_issuance_records(
    records: &HashMap<(PubkyPublicKey, String), ReceiptIssuanceRecord>,
    outbound_private_messages: &[OutboundPrivateMessageRecord],
) -> Result<()> {
    let outbound_by_id = outbound_private_messages
        .iter()
        .map(|record| (record.outbound_message_id, record))
        .collect::<HashMap<_, _>>();
    let mut receipt_ids = HashSet::new();

    for record in records.values() {
        if !receipt_ids.insert(record.receipt_id.clone()) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' is duplicated across counterparties",
                    record.receipt_id
                ),
                source: None,
            });
        }
        validate_receipt_issuance_status(record)?;
        ReceiptId::new(&record.receipt_id)?;
        if let Some(identifier) = record.payment_endpoint_identifier.as_ref() {
            PaymentEndpointIdentifier::new(identifier)?;
        }

        let access = paykit_lib::parse_receipt_access_json(&record.access_json).map_err(|err| {
            PaykitSdkError::Protocol {
                context: err.to_string(),
                source: None,
            }
        })?;
        let access_app_id = app_id_from_raw_json(&record.access_json)
            .map(paykit_lib::PaykitAppId::new)
            .transpose()?
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' has no App ID",
                    record.receipt_id
                ),
                source: None,
            })?;
        if access_app_id != record.app_id
            || access.event_id.as_str() != record.receipt_access_event_id
            || access.receipt_id.as_str() != record.receipt_id
            || access.payment_reference.as_str() != record.payment_reference
            || access
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned())
                != record.payment_request_id
            || access
                .billing_period
                .as_ref()
                .map(BillingPeriodRecord::from)
                != record.billing_period
            || access.location != record.location
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' does not match Receipt Access payload",
                    record.receipt_id
                ),
                source: None,
            });
        }

        let receipt =
            paykit_lib::decrypt_receipt(&record.encrypted_receipt, &access.key, &access.location)
                .map_err(|_| PaykitSdkError::Protocol {
                // The lib error can describe the stored encrypted receipt
                // (and its `source` can carry envelope parse detail), and
                // this context crosses the FFI from backup restore; keep it
                // static and drop the cause.
                context: "stored encrypted receipt is invalid".into(),
                source: None,
            })?;
        let recipient = PubkyPublicKey::from_public_key(&receipt.recipient_public_key);
        if recipient != record.counterparty
            || receipt.receipt_id.as_str() != record.receipt_id
            || receipt.payment_reference.as_str() != record.payment_reference
            || receipt
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned())
                != record.payment_request_id
            || receipt
                .billing_period
                .as_ref()
                .map(BillingPeriodRecord::from)
                != record.billing_period
            || receipt
                .payment_endpoint_identifier
                .as_ref()
                .map(|identifier| identifier.as_str().to_owned())
                != record.payment_endpoint_identifier
            || receipt.amount.as_ref().map(AmountRecord::from) != record.amount
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' does not match encrypted Receipt",
                    record.receipt_id
                ),
                source: None,
            });
        }

        if let Some(outbound_message_id) = record.outbound_message_id {
            let Some(outbound) = outbound_by_id.get(&outbound_message_id) else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Receipt issuance record '{}' references missing outbound message {}",
                        record.receipt_id, outbound_message_id
                    ),
                    source: None,
                });
            };
            if outbound.counterparty != record.counterparty
                || outbound.kind != PrivateMessageKind::ReceiptAccess.as_str()
                || outbound.raw_json != record.access_json
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Receipt issuance record '{}' does not match outbound message {}",
                        record.receipt_id, outbound_message_id
                    ),
                    source: None,
                });
            }
        }
    }

    Ok(())
}

fn validate_receipt_issuance_status(record: &ReceiptIssuanceRecord) -> Result<()> {
    if record.updated_at < record.created_at
        || record
            .stored_at
            .is_some_and(|stored_at| stored_at < record.created_at)
        || record
            .access_queued_at
            .is_some_and(|queued_at| queued_at < record.created_at)
    {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Receipt issuance record '{}' has inconsistent timestamps",
                record.receipt_id
            ),
            source: None,
        });
    }

    let invalid = match record.status {
        ReceiptIssuanceStatus::PendingStorage => {
            record.stored_at.is_some()
                || record.access_queued_at.is_some()
                || record.outbound_message_id.is_some()
                || record.last_error.is_some()
        }
        ReceiptIssuanceStatus::Stored => {
            record.stored_at.is_none()
                || record.access_queued_at.is_some()
                || record.outbound_message_id.is_some()
                || record.last_error.is_some()
        }
        ReceiptIssuanceStatus::AccessQueued => {
            record.stored_at.is_none()
                || record.access_queued_at.is_none()
                || record.outbound_message_id.is_none()
                || record.last_error.is_some()
        }
        ReceiptIssuanceStatus::Failed => {
            record.access_queued_at.is_some()
                || record.outbound_message_id.is_some()
                || record.last_error.is_none()
        }
    };
    if invalid {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Receipt issuance record '{}' has inconsistent {:?} status metadata",
                record.receipt_id, record.status
            ),
            source: None,
        });
    }
    Ok(())
}
