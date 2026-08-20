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

fn app_id() -> paykit_lib::PaykitAppId {
    paykit_lib::PaykitAppId::new("bitkit").unwrap()
}

fn registered_storage() -> InMemoryStorage {
    InMemoryStorage::with_registered_apps([app_id()])
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
        app_id: value
            .get("app_id")
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
        r#"{{"version":1,"kind":"paykit.payment_request","app_id":"bitkit","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"{reference}","proposal_expires_at":{expiry},"recurrence":{recurrence},"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"required_app_id":null,"metadata":{{"note":"private"}}}}}}"#
    )
}

fn acceptance_raw(event_id: &str, request_id: &str) -> String {
    acceptance_raw_for_app(event_id, request_id, "bitkit")
}

fn acceptance_raw_for_app(event_id: &str, request_id: &str, app_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_acceptance","app_id":"{app_id}","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    )
}

fn rejection_raw(event_id: &str, request_id: &str) -> String {
    rejection_raw_for_app(event_id, request_id, "bitkit")
}

fn rejection_raw_for_app(event_id: &str, request_id: &str, app_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_rejection","app_id":"{app_id}","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    )
}

fn cancellation_raw(event_id: &str, request_id: &str) -> String {
    cancellation_raw_for_app(event_id, request_id, "bitkit")
}

fn cancellation_raw_for_app(event_id: &str, request_id: &str, app_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_cancellation","app_id":"{app_id}","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    )
}

fn malformed_cancellation_raw(event_id: &str, request_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_cancellation","app_id":"bitkit","event_id":"{event_id}","payment_request_id":"{request_id}","reason":null}}"#
    )
}

fn malformed_missing_request_id_raw(event_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_acceptance","app_id":"bitkit","event_id":"{event_id}"}}"#
    )
}

fn proof_raw(event_id: &str, request_id: &str, reference: &str) -> String {
    proof_raw_for_app(event_id, request_id, reference, "bitkit")
}

fn proof_raw_for_app(event_id: &str, request_id: &str, reference: &str, app_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_proof","app_id":"{app_id}","event_id":"{event_id}","payment_request_id":"{request_id}","payment_reference":"{reference}","billing_period":null,"payment_endpoint_identifier":"btc-lightning-bolt11","payment_app_id":"{app_id}","proof":{{"txid":"secret"}}}}"#
    )
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
