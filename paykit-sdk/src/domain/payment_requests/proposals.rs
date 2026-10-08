use super::*;

pub(crate) fn enqueue_idempotent_payment_request(
    tx: &mut dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    request: &PaymentRequest,
    identity: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<PaymentRequestRecord> {
    let raw_json =
        serialize_payment_request_event(app_id, &PaymentRequestEvent::Request(request.clone()))?;
    crate::domain::outbound_private::validate_outbound_private_message(&raw_json)?;
    if tx
        .load_identity_state()
        .and_then(|state| state.public_key)
        .as_ref()
        != Some(identity)
    {
        return Err(PaykitSdkError::Identity {
            context: "Payment Request proposal identity changed during operation".into(),
            source: None,
        });
    }
    require_paykit_app_capability(tx, app_id, PrivateMessageKind::PaymentRequest)?;
    crate::domain::linked_peers::require_private_automation_ready(
        tx.linked_peer(counterparty).map(|peer| peer.state),
        tx.encrypted_link_state(counterparty)
            .is_some_and(|state| state.link_snapshot.is_some()),
        counterparty,
    )?;

    // Event Message history is retained across delivery and terminal states.
    // Compare canonical bytes, not typed equality or a normalized lifecycle view.
    let state = tx.export_storage_state();
    let mut proposal_ids = None;
    for message in state.outbound_private_messages {
        if message.kind != PrivateMessageKind::PaymentRequest.as_str() {
            continue;
        }
        let parsed = parse_payment_request_event_message(&PrivateApplicationMessage {
            version: Some(1),
            kind: Some(message.kind),
            app_id: Some(message.app_id.to_string()),
            raw_json: message.raw_json,
        })
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: "stored Payment Request proposal is unrecognized".into(),
            source: None,
        })?;
        if parsed.payment_request_id() != Some(request.payment_request_id()) {
            continue;
        }
        let Some(PaymentRequestEvent::Request(original)) = parsed.parsed_event() else {
            return Err(PaykitSdkError::Protocol {
                context: "stored Payment Request proposal is invalid".into(),
                source: None,
            });
        };
        if message.counterparty != *counterparty
            || message.app_id != *app_id
            || serialize_payment_request_event(
                app_id,
                &PaymentRequestEvent::Request(PaymentRequest::new(
                    original.event_id().clone(),
                    request.payment_request_id().clone(),
                    request.request().clone(),
                )),
            )? != parsed.raw_json
        {
            return Err(PaykitSdkError::Policy {
                context: "Payment Request ID is already bound to a different proposal".into(),
                source: None,
            });
        }
        proposal_ids = Some((
            original.event_id().as_str().to_owned(),
            message.outbound_message_id,
        ));
    }
    let (event_id, outbound_message_id) = if let Some(ids) = proposal_ids {
        ids
    } else {
        for item in state.private_stream_items {
            if item.known_paykit_kind.as_deref()
                != Some(PrivateMessageKind::PaymentRequest.as_str())
            {
                continue;
            }
            let message = PrivateApplicationMessage {
                version: item
                    .parsed_version
                    .and_then(|version| u8::try_from(version).ok()),
                kind: item.parsed_kind,
                app_id: item.parsed_app_id,
                raw_json: item.raw_json,
            };
            if parse_payment_request_event_message(&message).is_some_and(|parsed| {
                matches!(parsed.parsed_event(), Some(PaymentRequestEvent::Request(original))
                    if original.payment_request_id() == request.payment_request_id())
            }) {
                return Err(PaykitSdkError::Policy {
                    context: "Payment Request ID is already bound to a received proposal".into(),
                    source: None,
                });
            }
        }
        let message = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
            counterparty.clone(),
            app_id.clone(),
            PrivateMessageKind::PaymentRequest.as_str().to_owned(),
            raw_json,
            now,
        ))?;
        (
            request.event_id().as_str().to_owned(),
            message.outbound_message_id,
        )
    };
    let mut record = payment_request_records_from_transaction(tx, counterparty, now)?
        .into_iter()
        .find(|record| record.payment_request_id == request.payment_request_id().as_str())
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: "queued Payment Request has no derived record".into(),
            source: None,
        })?;
    // Invalid lifecycle events can suppress proposal fields during derivation.
    // Recover queue identities without changing lifecycle or payment authority.
    record.proposal_event_id = Some(event_id);
    record.proposal_outbound_message_id = Some(outbound_message_id);
    Ok(record)
}
