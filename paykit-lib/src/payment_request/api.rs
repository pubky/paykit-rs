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

fn parse_event(kind: PrivateMessageKind, raw: &str) -> Result<PaymentRequestEvent> {
    match kind {
        PrivateMessageKind::PaymentRequest => {
            parse_payment_request_json(raw).map(PaymentRequestEvent::Request)
        }
        PrivateMessageKind::PaymentRequestAcceptance => {
            parse_acceptance_json(raw).map(PaymentRequestEvent::Acceptance)
        }
        PrivateMessageKind::PaymentRequestRejection => {
            parse_rejection_json(raw).map(PaymentRequestEvent::Rejection)
        }
        PrivateMessageKind::PaymentRequestCancellation => {
            parse_cancellation_json(raw).map(PaymentRequestEvent::Cancellation)
        }
        PrivateMessageKind::PaymentProof => {
            parse_payment_proof_json(raw).map(PaymentRequestEvent::Proof)
        }
        _ => unreachable!("only Payment Request event kinds are selected"),
    }
}

fn build_event_message(kind: PrivateMessageKind, raw: String) -> PaymentRequestEventMessage {
    let (event_id, payment_request_id) = parse_event_header_ids(&raw);
    let event = parse_event(kind, &raw).map_err(|err| err.to_string());
    PaymentRequestEventMessage {
        kind,
        event_id,
        payment_request_id,
        raw_json: raw,
        event,
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
    kind.is_payment_request_event()
        .then(|| build_event_message(kind, message.raw_json.clone()))
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
