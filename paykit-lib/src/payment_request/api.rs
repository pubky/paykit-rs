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

use crate::PrivateMessageKind;

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
    let event = parse_event(kind, &message.raw_json)?.map_err(|err| err.to_string());
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
}
