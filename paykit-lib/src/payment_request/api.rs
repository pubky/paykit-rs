use tracing::instrument;

use crate::{error::map_error, EncryptedLink, PrivateApplicationMessage, Result};

use super::{
    types::{
        PaymentProof, PaymentRequest, PaymentRequestAcceptance, PaymentRequestCancellation,
        PaymentRequestEvent, PaymentRequestEventMessage, PaymentRequestRejection,
    },
    wire::{
        parse_acceptance_json, parse_cancellation_json, parse_event_header_ids,
        parse_payment_proof_json, parse_payment_request_json, parse_rejection_json,
        serialize_acceptance_json, serialize_cancellation_json, serialize_payment_proof_json,
        serialize_payment_request_json, serialize_rejection_json,
    },
};

use crate::{PrivateMessageKind, PrivateMessageParseCategory};

/// Parse `raw` as the Payment Request protocol event selected by `kind`, or
/// return `None` when `kind` is not a Payment Request protocol event kind.
///
/// Routing and dispatch live in this single `match`, which deliberately has no
/// wildcard arm: adding a `PrivateMessageKind` variant fails to compile until
/// it is explicitly routed to a parser or rejected here, so the kind selector
/// and the per-kind dispatch cannot drift apart.
fn parse_event(kind: PrivateMessageKind, raw: &str) -> Option<Result<PaymentRequestEvent>> {
    match kind {
        PrivateMessageKind::PaymentRequest => {
            Some(parse_payment_request_json(raw).map(PaymentRequestEvent::Request))
        }
        PrivateMessageKind::PaymentRequestAcceptance => {
            Some(parse_acceptance_json(raw).map(PaymentRequestEvent::Acceptance))
        }
        PrivateMessageKind::PaymentRequestRejection => {
            Some(parse_rejection_json(raw).map(PaymentRequestEvent::Rejection))
        }
        PrivateMessageKind::PaymentRequestCancellation => {
            Some(parse_cancellation_json(raw).map(PaymentRequestEvent::Cancellation))
        }
        PrivateMessageKind::PaymentProof => {
            Some(parse_payment_proof_json(raw).map(PaymentRequestEvent::Proof))
        }
        // Non-request kinds are ignored, producing nothing derived from `raw`
        // (decrypted private payload), so there is no error context to leak.
        PrivateMessageKind::PrivatePaymentList | PrivateMessageKind::ReceiptAccess => None,
    }
}

/// Parse a raw Private Application Message as a Payment Request protocol event.
///
/// Returns `None` when the message kind is not one of the Payment Request
/// protocol event kinds. Recognized but malformed Payment Request events return
/// `Some` with [`PaymentRequestEventMessage::is_valid`] set to `false`.
pub fn parse_payment_request_event_message(
    message: &PrivateApplicationMessage,
) -> Option<PaymentRequestEventMessage> {
    let kind = message.known_kind()?;
    // SECURITY / REDACTION: the stored validation error is exactly a stable
    // redacted category string (persisted by SDK callers and byte-compared on
    // backup restore), never free-form error text.
    let event = parse_event(kind, &message.raw_json)?.map_err(|err| {
        err.private_message_parse_category()
            .unwrap_or(PrivateMessageParseCategory::InvalidStructure)
            .as_str()
            .to_owned()
    });
    let (event_id, payment_request_id) = parse_event_header_ids(&message.raw_json);
    Some(PaymentRequestEventMessage {
        kind,
        event_id,
        payment_request_id,
        raw_json: message.raw_json.clone(),
        event,
    })
}

/// Serialize a Payment Request protocol Event Message to canonical JSON.
pub fn serialize_payment_request_event(event: &PaymentRequestEvent) -> Result<String> {
    match event {
        PaymentRequestEvent::Request(event) => serialize_payment_request_json(event),
        PaymentRequestEvent::Acceptance(event) => serialize_acceptance_json(event),
        PaymentRequestEvent::Rejection(event) => serialize_rejection_json(event),
        PaymentRequestEvent::Cancellation(event) => serialize_cancellation_json(event),
        PaymentRequestEvent::Proof(event) => serialize_payment_proof_json(event),
    }
}

/// Send a `paykit.payment_request` Event Message.
///
/// Payment Requests are payee-initiated; caller code must enforce the sender
/// role.
#[instrument(skip(link, event))]
pub async fn send_payment_request(link: &mut EncryptedLink, event: &PaymentRequest) -> Result<()> {
    let json = serialize_payment_request_json(event)
        .map_err(|err| map_error("send_payment_request", err))?;
    link.send_payment_request_message(json.as_bytes())
        .await
        .map_err(|err| map_error("send_payment_request", err))
}

/// Send a `paykit.payment_request_acceptance` Event Message.
///
/// Acceptances are payer-initiated; caller code must enforce the sender role.
#[instrument(skip(link, event))]
pub async fn send_payment_request_acceptance(
    link: &mut EncryptedLink,
    event: &PaymentRequestAcceptance,
) -> Result<()> {
    let json = serialize_acceptance_json(event)
        .map_err(|err| map_error("send_payment_request_acceptance", err))?;
    link.send_payment_request_acceptance_message(json.as_bytes())
        .await
        .map_err(|err| map_error("send_payment_request_acceptance", err))
}

/// Send a `paykit.payment_request_rejection` Event Message.
///
/// Rejections are payer-initiated; caller code must enforce the sender role.
#[instrument(skip(link, event))]
pub async fn send_payment_request_rejection(
    link: &mut EncryptedLink,
    event: &PaymentRequestRejection,
) -> Result<()> {
    let json = serialize_rejection_json(event)
        .map_err(|err| map_error("send_payment_request_rejection", err))?;
    link.send_payment_request_rejection_message(json.as_bytes())
        .await
        .map_err(|err| map_error("send_payment_request_rejection", err))
}

/// Send a `paykit.payment_request_cancellation` Event Message.
///
/// Cancellations may be sent by either payer or payee; caller code must enforce
/// the sender role.
#[instrument(skip(link, event))]
pub async fn send_payment_request_cancellation(
    link: &mut EncryptedLink,
    event: &PaymentRequestCancellation,
) -> Result<()> {
    let json = serialize_cancellation_json(event)
        .map_err(|err| map_error("send_payment_request_cancellation", err))?;
    link.send_payment_request_cancellation_message(json.as_bytes())
        .await
        .map_err(|err| map_error("send_payment_request_cancellation", err))
}

/// Send a `paykit.payment_proof` Event Message.
///
/// Payment Proofs are payer-initiated; caller code must enforce the sender role.
#[instrument(skip(link, event))]
pub async fn send_payment_proof(link: &mut EncryptedLink, event: &PaymentProof) -> Result<()> {
    let json =
        serialize_payment_proof_json(event).map_err(|err| map_error("send_payment_proof", err))?;
    link.send_payment_proof_message(json.as_bytes())
        .await
        .map_err(|err| map_error("send_payment_proof", err))
}

#[cfg(test)]
mod tests {
    use super::*;

    // parse_event owns both routing and dispatch: non-request kinds must be
    // ignored (`None`), not parsed and not turned into an error. Returning
    // `None` means nothing derived from `raw` (decrypted private payload) is
    // ever produced for these kinds, so there is no error context that could
    // leak the plaintext. The public parser covers this path via `known_kind`;
    // this exercises parse_event directly.
    #[test]
    fn test_parse_event_non_request_kind_ignored() {
        // Sentinel plaintext standing in for a decrypted private payload.
        let raw = "{\"secret\":\"SENTINEL_DECRYPTED_PLAINTEXT\"}";
        for kind in [
            PrivateMessageKind::PrivatePaymentList,
            PrivateMessageKind::ReceiptAccess,
        ] {
            assert!(
                parse_event(kind, raw).is_none(),
                "expected non-request kind {kind} to be ignored",
            );
        }
    }

    use crate::{EventId, PaymentRequestId};

    fn event_id() -> EventId {
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201").expect("hard-coded UUID v4 is valid")
    }

    fn request_id() -> PaymentRequestId {
        PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33")
            .expect("hard-coded UUID v4 is valid")
    }

    // Build a received message whose header `kind` field claims
    // `paykit.payment_request` regardless of the raw JSON payload, so every
    // caller below also pins that routing never falls back to the header.
    fn message_with_request_header(raw_json: &str) -> PrivateApplicationMessage {
        PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::PaymentRequest.as_str().to_string()),
            raw_json: raw_json.to_string(),
        }
    }

    // Serialize `event`, wrap it as a received Private Application Message,
    // and parse it back through the public entry point, asserting structural
    // equality and the parsed header ids.
    fn assert_serialize_round_trip(event: PaymentRequestEvent) {
        let serialized = serialize_payment_request_event(&event)
            .expect("construction-valid event must serialize");
        let message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some(event.kind().as_str().to_string()),
            raw_json: serialized,
        };
        let parsed = parse_payment_request_event_message(&message)
            .expect("serialized Payment Request event kind must be routed");
        assert!(
            parsed.is_valid(),
            "round-tripped event must stay valid: {:?}",
            parsed.validation_error()
        );
        assert_eq!(parsed.kind(), event.kind());
        assert_eq!(parsed.event_id(), Some(event.event_id()));
        assert_eq!(
            parsed.payment_request_id(),
            Some(event.payment_request_id())
        );
        assert_eq!(parsed.parsed_event(), Some(&event));
    }

    // Mirror of `payment_request_event_parser_uses_raw_json_kind` in
    // `crate::tests::payment_request`, which pins a stale ReceiptAccess header
    // routing a payment-request raw payload. Here the header claims
    // `paykit.payment_request` while the raw JSON kind is
    // `paykit.receipt_access`: the raw JSON kind wins, the message is not a
    // Payment Request protocol event, and the parser must return `None`.
    #[test]
    fn test_parse_payment_request_event_message_raw_json_kind_wins_over_header() {
        let message =
            message_with_request_header(r#"{"version":1,"kind":"paykit.receipt_access"}"#);
        assert!(parse_payment_request_event_message(&message).is_none());
    }

    // Pins current behavior for unroutable raw payloads: a missing,
    // non-string, or unrecognized raw JSON `kind` yields `None` (not an
    // error), even when the message header claims a request kind.
    #[test]
    fn test_parse_payment_request_event_message_unusable_raw_json_kind_is_none() {
        for raw_json in [
            r#"{"version":1}"#,
            r#"{"version":1,"kind":42}"#,
            r#"{"version":1,"kind":null}"#,
            r#"{"version":1,"kind":true}"#,
            r#"{"version":1,"kind":["paykit.payment_request"]}"#,
            r#"{"version":1,"kind":{"kind":"paykit.payment_request"}}"#,
            r#"{"version":1,"kind":"paykit.unknown_kind"}"#,
        ] {
            let message = message_with_request_header(raw_json);
            assert!(
                parse_payment_request_event_message(&message).is_none(),
                "expected None for raw JSON {raw_json}"
            );
        }
    }

    #[test]
    fn test_payment_request_acceptance_serialize_round_trip() {
        assert_serialize_round_trip(PaymentRequestEvent::Acceptance(
            PaymentRequestAcceptance::new(event_id(), request_id()),
        ));
    }

    #[test]
    fn test_payment_request_rejection_serialize_round_trip_covers_reason_presence() {
        assert_serialize_round_trip(PaymentRequestEvent::Rejection(
            PaymentRequestRejection::new(
                event_id(),
                request_id(),
                Some("amount no longer payable".to_string()),
            ),
        ));

        let without_reason = PaymentRequestEvent::Rejection(PaymentRequestRejection::new(
            event_id(),
            request_id(),
            None,
        ));
        let serialized = serialize_payment_request_event(&without_reason)
            .expect("construction-valid event must serialize");
        assert!(
            !serialized.contains("\"reason\""),
            "a None reason must be omitted from the wire JSON"
        );
        assert_serialize_round_trip(without_reason);
    }

    #[test]
    fn test_payment_request_cancellation_serialize_round_trip_covers_reason_presence() {
        assert_serialize_round_trip(PaymentRequestEvent::Cancellation(
            PaymentRequestCancellation::new(
                event_id(),
                request_id(),
                Some("request superseded".to_string()),
            ),
        ));

        let without_reason = PaymentRequestEvent::Cancellation(PaymentRequestCancellation::new(
            event_id(),
            request_id(),
            None,
        ));
        let serialized = serialize_payment_request_event(&without_reason)
            .expect("construction-valid event must serialize");
        assert!(
            !serialized.contains("\"reason\""),
            "a None reason must be omitted from the wire JSON"
        );
        assert_serialize_round_trip(without_reason);
    }
}
