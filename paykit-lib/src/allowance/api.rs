use tracing::instrument;

use crate::{
    error::map_error, EncryptedLink, PrivateApplicationMessage, PrivateMessageKind, Result,
};

use super::{
    types::{
        AllowanceAcceptance, AllowanceEnd, AllowanceEvent, AllowanceEventMessage,
        AllowanceProposal, AllowanceRejection,
    },
    wire::{parse_allowance_json, parse_event_header_ids, serialize_allowance_json},
};

/// Parse a raw Private Application Message as an Allowance Event Message.
///
/// Returns `None` for non-Allowance kinds. Recognized malformed messages are
/// returned with [`AllowanceEventMessage::is_valid`] set to `false`, while the
/// raw JSON remains available for durable storage and future audit.
pub fn parse_allowance_event_message(
    message: &PrivateApplicationMessage,
) -> Option<AllowanceEventMessage> {
    let kind = message.known_kind()?;
    if !is_allowance_kind(kind) {
        return None;
    }
    let event = parse_allowance_json(kind, &message.raw_json).map_err(|error| error.to_string());
    let (event_id, allowance_id) = parse_event_header_ids(&message.raw_json);
    Some(AllowanceEventMessage {
        kind,
        event_id,
        allowance_id,
        raw_json: message.raw_json.clone(),
        event,
    })
}

/// Serialize a V1 Allowance Event Message to compact JSON.
///
/// Serialization rejects a complete message larger than the single-message
/// `pubky-noise` plaintext limit.
pub fn serialize_allowance_event(event: &AllowanceEvent) -> Result<String> {
    serialize_allowance_json(event)
}

/// Send a `paykit.allowance_proposal` Event Message.
///
/// The caller is responsible for using the exact authenticated Encrypted Link
/// that binds the Allower and Allowee and for retaining the Event Message for
/// retry and lifecycle derivation.
#[instrument(skip(link, event))]
pub async fn send_allowance_proposal(
    link: &mut EncryptedLink,
    event: &AllowanceProposal,
) -> Result<()> {
    send_event(
        link,
        &AllowanceEvent::Proposal(event.clone()),
        "send_allowance_proposal",
    )
    .await
}

/// Send a `paykit.allowance_acceptance` Event Message.
///
/// Call [`AllowanceAcceptance::validate_for_proposal`] first with the
/// authenticated sender role when the proposal is available.
#[instrument(skip(link, event))]
pub async fn send_allowance_acceptance(
    link: &mut EncryptedLink,
    event: &AllowanceAcceptance,
) -> Result<()> {
    send_event(
        link,
        &AllowanceEvent::Acceptance(event.clone()),
        "send_allowance_acceptance",
    )
    .await
}

/// Send a `paykit.allowance_rejection` Event Message.
///
/// Call [`AllowanceRejection::validate_for_proposal`] first with the
/// authenticated sender role when the proposal is available.
#[instrument(skip(link, event))]
pub async fn send_allowance_rejection(
    link: &mut EncryptedLink,
    event: &AllowanceRejection,
) -> Result<()> {
    send_event(
        link,
        &AllowanceEvent::Rejection(event.clone()),
        "send_allowance_rejection",
    )
    .await
}

/// Send a `paykit.allowance_end` Event Message.
///
/// Call the appropriate withdrawal or accepted-authority validation helper on
/// [`AllowanceEnd`] when the causal events are available.
#[instrument(skip(link, event))]
pub async fn send_allowance_end(link: &mut EncryptedLink, event: &AllowanceEnd) -> Result<()> {
    send_event(
        link,
        &AllowanceEvent::End(event.clone()),
        "send_allowance_end",
    )
    .await
}

async fn send_event(
    link: &mut EncryptedLink,
    event: &AllowanceEvent,
    context: &'static str,
) -> Result<()> {
    let json = serialize_allowance_json(event).map_err(|error| map_error(context, error))?;
    link.send_allowance_message(json.as_bytes())
        .await
        .map_err(|error| map_error(context, error))
}

fn is_allowance_kind(kind: PrivateMessageKind) -> bool {
    match kind {
        PrivateMessageKind::AllowanceProposal
        | PrivateMessageKind::AllowanceAcceptance
        | PrivateMessageKind::AllowanceRejection
        | PrivateMessageKind::AllowanceEnd => true,
        PrivateMessageKind::PrivatePaymentList
        | PrivateMessageKind::ReceiptAccess
        | PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AllowanceId, AllowanceRole, AllowanceTerms, EventId, PrivateApplicationMessage};

    fn proposal() -> AllowanceEvent {
        AllowanceEvent::Proposal(AllowanceProposal::new(
            EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201").unwrap(),
            AllowanceId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44").unwrap(),
            AllowanceRole::Allower,
            AllowanceTerms::builder("btc")
                .lifetime_amount_limit("1")
                .build()
                .unwrap(),
        ))
    }

    #[test]
    fn test_parser_uses_raw_json_kind_and_redacts_debug() {
        let json = serialize_allowance_event(&proposal()).unwrap();
        let message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::ReceiptAccess.as_str().to_string()),
            raw_json: json,
        };
        let parsed = parse_allowance_event_message(&message).unwrap();
        assert!(parsed.is_valid());
        assert_eq!(parsed.kind(), PrivateMessageKind::AllowanceProposal);
        assert_eq!(parsed.parsed_event(), Some(&proposal()));
        let debug = format!("{parsed:?}");
        assert!(!debug.contains("\"asset\":\"btc\""));
        assert!(debug.contains("<redacted:"));
    }

    #[test]
    fn test_parser_preserves_malformed_recognized_message_for_audit() {
        let valid = serialize_allowance_event(&proposal()).unwrap();
        let raw_json = valid.replacen("{", "{\"private_sentinel\":true,", 1);
        let message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::AllowanceProposal.as_str().to_string()),
            raw_json: raw_json.clone(),
        };

        let parsed = parse_allowance_event_message(&message).unwrap();
        assert!(!parsed.is_valid());
        assert_eq!(parsed.event_id(), Some(proposal().event_id()));
        assert_eq!(parsed.allowance_id(), Some(proposal().allowance_id()));
        assert_eq!(parsed.raw_json(), raw_json);
        assert!(!format!("{parsed:?}").contains("private_sentinel"));
    }

    #[test]
    fn test_parser_ignores_non_allowance_kinds() {
        let message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::PaymentRequest.as_str().to_string()),
            raw_json: format!(
                "{{\"version\":1,\"kind\":\"{}\"}}",
                PrivateMessageKind::PaymentRequest.as_str()
            ),
        };
        assert!(parse_allowance_event_message(&message).is_none());
    }
}
