use chrono::{Duration as ChronoDuration, TimeZone};

use super::*;
use crate::{
    domain::outbound_private::{
        enqueue_private_message as enqueue_untyped_private_message,
        queued_outbound_private_messages,
    },
    domain::private_stream::persist_private_stream_batch,
    storage::InMemoryStorage,
};

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

fn private_message(raw_json: String) -> PrivateApplicationMessage {
    let value: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
    PrivateApplicationMessage {
        version: value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok()),
        kind: value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        raw_json,
    }
}

fn parsed_event(raw_json: String) -> PaymentRequestEvent {
    parse_payment_request_event_message(&private_message(raw_json))
        .unwrap()
        .parsed_event()
        .unwrap()
        .clone()
}

fn request_raw(
    event_id: &str,
    request_id: &str,
    reference: &str,
    expires_at: Option<&str>,
    recurrence: Option<&str>,
) -> String {
    let expiry = expires_at
        .map(|value| format!(r#""{value}""#))
        .unwrap_or_else(|| "null".into());
    let recurrence = recurrence.unwrap_or("null");
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"{reference}","proposal_expires_at":{expiry},"recurrence":{recurrence},"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{"note":"private"}}}}}}"#
    )
}

fn acceptance_raw(event_id: &str, request_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    )
}

fn rejection_raw(event_id: &str, request_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_rejection","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    )
}

fn cancellation_raw(event_id: &str, request_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    )
}

fn malformed_cancellation_raw(event_id: &str, request_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"{event_id}","payment_request_id":"{request_id}","reason":null}}"#
    )
}

fn malformed_missing_request_id_raw(event_id: &str) -> String {
    format!(r#"{{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"{event_id}"}}"#)
}

fn proof_raw(event_id: &str, request_id: &str, reference: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_proof","event_id":"{event_id}","payment_request_id":"{request_id}","payment_reference":"{reference}","billing_period":null,"payment_endpoint_identifier":"btc-lightning-bolt11","proof":{{"txid":"secret"}}}}"#
    )
}

fn allowance_proposal_raw(event_id: &str) -> String {
    let event = paykit_lib::AllowanceEvent::Proposal(paykit_lib::AllowanceProposal::new(
        paykit_lib::EventId::new(event_id).unwrap(),
        paykit_lib::AllowanceId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44").unwrap(),
        paykit_lib::AllowanceRole::Allower,
        paykit_lib::AllowanceTerms::builder("private-asset-sentinel")
            .lifetime_amount_limit("10")
            .build()
            .unwrap(),
    ));
    paykit_lib::serialize_allowance_event(&event).unwrap()
}

async fn persist_messages(
    storage: &InMemoryStorage,
    counterparty: PubkyPublicKey,
    messages: Vec<String>,
) {
    persist_messages_at(storage, counterparty, messages, timestamp()).await
}

async fn persist_messages_at(
    storage: &InMemoryStorage,
    counterparty: PubkyPublicKey,
    messages: Vec<String>,
    received_at: DateTime<Utc>,
) {
    persist_private_stream_batch(
        storage,
        counterparty,
        receiver_path(),
        messages.into_iter().map(private_message).collect(),
        None,
        received_at,
    )
    .await
    .unwrap();
}

mod enqueue;
mod outbound_records;
mod received_records;
