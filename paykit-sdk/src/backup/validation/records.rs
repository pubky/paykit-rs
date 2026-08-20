use super::{super::*, private_stream::private_message_header};
use chrono::{DateTime, Utc};

pub(in crate::backup) fn validate_public_endpoint_records(
    records: &HashMap<(paykit_lib::PaykitAppId, String), PublicEndpointRecord>,
) -> Result<()> {
    for record in records.values() {
        PaymentEndpointIdentifier::new(&record.identifier)?;
        match record.status {
            PublicationStatus::NotPublished => {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "public endpoint record '{}' cannot be not-published",
                        record.identifier
                    ),
                    source: None,
                });
            }
            PublicationStatus::PendingPublication | PublicationStatus::Published => {
                if record.payload.is_none() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "public endpoint record '{}' has no payload for status {:?}",
                            record.identifier, record.status
                        ),
                        source: None,
                    });
                }
                if record.last_error.is_some() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "public endpoint record '{}' has an error for status {:?}",
                            record.identifier, record.status
                        ),
                        source: None,
                    });
                }
            }
            PublicationStatus::PendingRemoval | PublicationStatus::Removed => {
                if record.last_error.is_some() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "public endpoint record '{}' has an error for status {:?}",
                            record.identifier, record.status
                        ),
                        source: None,
                    });
                }
                if record.status == PublicationStatus::Removed && record.payload.is_some() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "removed public endpoint record '{}' still has a payload",
                            record.identifier
                        ),
                        source: None,
                    });
                }
            }
            PublicationStatus::Failed => {
                if record.last_error.is_none() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "failed public endpoint record '{}' has no error",
                            record.identifier
                        ),
                        source: None,
                    });
                }
            }
        }
    }
    Ok(())
}

pub(in crate::backup) fn validate_contact_records(
    records: &HashMap<PubkyPublicKey, ContactRecord>,
) -> Result<()> {
    for record in records.values() {
        if let Some(profile) = record.profile.as_ref() {
            profile.validate()?;
        }
        if let Some(label) = record.label.as_deref() {
            crate::ContactUpdate {
                public_key: record.public_key.clone(),
                label: Some(label.to_owned()),
            }
            .validate()?;
        }
        validate_contact_marker_state(record)?;
    }
    Ok(())
}

fn validate_contact_marker_state(record: &ContactRecord) -> Result<()> {
    use crate::PublicationStatus::{
        Failed, NotPublished, PendingPublication, PendingRemoval, Published, Removed,
    };

    if record.public_contact_published_at.is_some() && record.public_contact_removed_at.is_some() {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Contact Record {} has inconsistent public contact marker timestamps",
                record.public_key
            ),
            source: None,
        });
    }

    let invalid = match record.public_contact_marker_status {
        NotPublished => {
            record.public_contact_published_at.is_some()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
        }
        PendingPublication => record.public_contact_last_error.is_some(),
        Published => {
            record.public_contact_published_at.is_none()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
        }
        PendingRemoval => {
            record.public_contact_published_at.is_none()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
        }
        Removed => {
            record.public_contact_published_at.is_some()
                || record.public_contact_removed_at.is_none()
                || record.public_contact_last_error.is_some()
        }
        Failed => record.public_contact_last_error.is_none(),
    };
    if invalid {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Contact Record {} has inconsistent public contact marker state",
                record.public_key
            ),
            source: None,
        });
    }
    Ok(())
}

pub(in crate::backup) fn validate_payment_endpoint_reservations(
    records: &HashMap<
        (PubkyPublicKey, paykit_lib::PaykitAppId, String),
        PaymentEndpointReservationRecord,
    >,
    outbound_private_messages: &[OutboundPrivateMessageRecord],
) -> Result<()> {
    let outbound_by_id = outbound_private_messages
        .iter()
        .map(|record| (record.outbound_message_id, record))
        .collect::<HashMap<_, _>>();

    for record in records.values() {
        validate_reservation_id(&record.reservation_id)?;
        let identifier = PaymentEndpointIdentifier::new(&record.identifier)?;
        let outbound = outbound_by_id
            .get(&record.outbound_message_id)
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "Payment Endpoint Reservation '{}' references missing outbound message {}",
                    record.reservation_id, record.outbound_message_id
                ),
                source: None,
            })?;
        if outbound.counterparty != record.counterparty {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Payment Endpoint Reservation '{}' counterparty does not match outbound message {}",
                    record.reservation_id, record.outbound_message_id
                ),
                source: None,
            });
        }
        if outbound.app_id != record.app_id {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Payment Endpoint Reservation '{}' app does not match outbound message {}",
                    record.reservation_id, record.outbound_message_id
                ),
                source: None,
            });
        }
        if outbound.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Payment Endpoint Reservation '{}' references non-list outbound message {}",
                    record.reservation_id, record.outbound_message_id
                ),
                source: None,
            });
        }
        let private_list = parse_private_payment_list_json(&outbound.raw_json).map_err(|err| {
            PaykitSdkError::Protocol {
                context: err.to_string(),
                source: None,
            }
        })?;
        let payload = private_list.get(&identifier).ok_or_else(|| {
            PaykitSdkError::Protocol { context: format!(
                "Payment Endpoint Reservation '{}' identifier is missing from outbound Private Payment List {}",
                record.reservation_id, record.outbound_message_id
            ), source: None }
        })?;
        let payload_hash = reservation_payload_hash(payload.as_str());
        if record.payload_hash != payload_hash {
            return Err(PaykitSdkError::Protocol { context: format!(
                "Payment Endpoint Reservation '{}' payload hash does not match outbound Private Payment List {}",
                record.reservation_id, record.outbound_message_id
            ), source: None });
        }
    }
    Ok(())
}

pub(in crate::backup) fn validate_outbound_private_messages(
    records: &[OutboundPrivateMessageRecord],
) -> Result<()> {
    for record in records {
        validate_outbound_private_status(record)?;
        if matches!(
            record.status,
            OutboundPrivateMessageStatus::Invalid | OutboundPrivateMessageStatus::RecoveryRequired
        ) {
            continue;
        }
        let (_, parsed_kind, parsed_app_id, _) = private_message_header(&record.raw_json);
        if parsed_kind.as_deref() != Some(record.kind.as_str())
            || parsed_app_id.as_deref() != Some(record.app_id.as_str())
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "outbound Private Application Message {} has stale header metadata",
                    record.outbound_message_id
                ),
                source: None,
            });
        }
        validate_queued_outbound_private_message(record)?;
    }
    Ok(())
}

pub(in crate::backup) fn validate_retired_app_outbound_messages(
    retired_apps: &HashSet<paykit_lib::PaykitAppId>,
    records: &[OutboundPrivateMessageRecord],
) -> Result<()> {
    for record in records {
        if retired_apps.contains(&record.app_id)
            && matches!(
                record.status,
                OutboundPrivateMessageStatus::Pending
                    | OutboundPrivateMessageStatus::Sending
                    | OutboundPrivateMessageStatus::Failed
                    | OutboundPrivateMessageStatus::RecoveryRequired
            )
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "retired Paykit App '{}' has deliverable outbound Private Application Message {}",
                    record.app_id, record.outbound_message_id
                ),
                source: None,
            });
        }
    }
    Ok(())
}

pub(in crate::backup) fn validate_retired_app_payment_requests(
    retired_apps: &HashSet<paykit_lib::PaykitAppId>,
    private_stream_items: &[PrivateStreamItemRecord],
    outbound_messages: &[OutboundPrivateMessageRecord],
    dedupe_records: &HashMap<(PubkyPublicKey, String), EventDedupRecord>,
) -> Result<()> {
    let counterparties = private_stream_items
        .iter()
        .map(|item| item.counterparty.clone())
        .chain(
            outbound_messages
                .iter()
                .map(|message| message.counterparty.clone()),
        )
        .collect::<HashSet<_>>();
    for counterparty in counterparties {
        let items = private_stream_items
            .iter()
            .filter(|item| item.counterparty == counterparty)
            .cloned()
            .collect();
        let outbound = outbound_messages
            .iter()
            .filter(|message| message.counterparty == counterparty)
            .cloned()
            .collect();
        let dedupe = dedupe_records
            .iter()
            .filter(|((record_counterparty, _), _)| record_counterparty == &counterparty)
            .map(|((_, event_id), record)| (event_id.clone(), record.clone()))
            .collect();
        let records = derive_payment_request_records_from_parts(
            counterparty,
            items,
            outbound,
            dedupe,
            DateTime::<Utc>::MAX_UTC,
        )?;
        for app_id in retired_apps {
            if records
                .iter()
                .any(|record| payment_request_record_blocks_app_removal(record, app_id))
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "retired Paykit App '{app_id}' owns an active Payment Request"
                    ),
                    source: None,
                });
            }
        }
    }
    Ok(())
}

pub(in crate::backup) fn validate_retired_app_private_payment_lists(
    retired_apps: &HashSet<paykit_lib::PaykitAppId>,
    outbound_messages: &[OutboundPrivateMessageRecord],
) -> Result<()> {
    for app_id in retired_apps {
        if !counterparties_with_shared_private_payment_lists(outbound_messages, app_id)?.is_empty()
        {
            return Err(PaykitSdkError::Protocol {
                context: format!("retired Paykit App '{app_id}' has a shared Private Payment List"),
                source: None,
            });
        }
    }
    Ok(())
}

pub(in crate::backup) fn validate_retired_app_receipt_issuance(
    retired_apps: &HashSet<paykit_lib::PaykitAppId>,
    issuance_records: &HashMap<(PubkyPublicKey, String), ReceiptIssuanceRecord>,
    outbound_messages: &[OutboundPrivateMessageRecord],
) -> Result<()> {
    let outbound_statuses = outbound_messages
        .iter()
        .map(|message| (message.outbound_message_id, &message.status))
        .collect::<HashMap<_, _>>();
    for record in issuance_records.values() {
        if !retired_apps.contains(&record.app_id) {
            continue;
        }
        let complete = record.status == ReceiptIssuanceStatus::AccessQueued
            && record
                .outbound_message_id
                .is_some_and(|outbound_message_id| {
                    outbound_statuses.get(&outbound_message_id)
                        == Some(&&OutboundPrivateMessageStatus::Sent)
                });
        if !complete {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "retired Paykit App '{}' owns incomplete Receipt issuance '{}'",
                    record.app_id, record.receipt_id
                ),
                source: None,
            });
        }
    }
    Ok(())
}

fn validate_outbound_private_status(record: &OutboundPrivateMessageRecord) -> Result<()> {
    let invalid = match record.status {
        OutboundPrivateMessageStatus::Pending => {
            record.attempt_count != 0
                || record.last_attempt_at.is_some()
                || record.sent_at.is_some()
                || record.last_error.is_some()
        }
        OutboundPrivateMessageStatus::Sending => {
            record.attempt_count == 0
                || record.last_attempt_at.is_none()
                || record.sent_at.is_some()
                || record.last_error.is_some()
        }
        OutboundPrivateMessageStatus::Sent => {
            record.attempt_count == 0
                || record.last_attempt_at.is_none()
                || record.sent_at.is_none()
                || record.last_error.is_some()
        }
        OutboundPrivateMessageStatus::Failed => {
            record.attempt_count == 0
                || record.last_attempt_at.is_none()
                || record.sent_at.is_some()
                || record.last_error.is_none()
        }
        OutboundPrivateMessageStatus::Invalid | OutboundPrivateMessageStatus::RecoveryRequired => {
            record.sent_at.is_some() || record.last_error.is_none()
        }
        OutboundPrivateMessageStatus::Superseded => {
            record.sent_at.is_some() || record.last_error.is_some()
        }
    };
    if invalid {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "outbound Private Application Message {} has inconsistent {:?} status metadata",
                record.outbound_message_id, record.status
            ),
            source: None,
        });
    }
    Ok(())
}
