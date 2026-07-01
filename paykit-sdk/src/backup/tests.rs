use chrono::{TimeZone, Utc};

use super::*;
use crate::{
    domain::outbound_private::OutboundPrivateMessageStatus,
    domain::private_stream::PrivateStreamParseStatus, identity::PubkyIdentityCapability,
    storage::InMemoryStorage,
};

fn timestamp() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn public_key() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn receiver_id() -> paykit_lib::PaykitReceiverId {
    paykit_lib::PaykitReceiverId::new("bitkit").unwrap()
}

fn identity(public_key: PubkyPublicKey) -> IdentityState {
    IdentityState {
        public_key: Some(public_key),
        capability: PubkyIdentityCapability::PrivateLinkCapable,
        local_secret_available: true,
        initialized_at: timestamp(),
        sign_out_generation: 0,
    }
}

fn signed_out_identity(sign_out_generation: u64) -> IdentityState {
    IdentityState {
        public_key: None,
        capability: PubkyIdentityCapability::SignedOut,
        local_secret_available: false,
        initialized_at: timestamp(),
        sign_out_generation,
    }
}

fn contact_record(public_key: PubkyPublicKey) -> ContactRecord {
    ContactRecord {
        public_key,
        label: Some("Alice".into()),
        profile: None,
        profile_fetched_at: None,
        created_at: timestamp(),
        updated_at: timestamp(),
        public_contact_marker_status: crate::PublicationStatus::NotPublished,
        public_contact_published_at: None,
        public_contact_removed_at: None,
        public_contact_last_error: None,
    }
}

fn payment_request_json(event_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request","event_id":"{event_id}","payment_request_id":"550e8400-e29b-41d4-a716-446655440000","request":{{"amount":{{"value":"1","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{}}}}}}"#
    )
}

fn private_payment_list_outbound(
    counterparty: PubkyPublicKey,
    outbound_message_id: u64,
    payload: &str,
) -> OutboundPrivateMessageRecord {
    OutboundPrivateMessageRecord {
        outbound_message_id,
        counterparty,
        kind: PrivateMessageKind::PrivatePaymentList.as_str().into(),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{{"btc-lightning-bolt11":"{payload}"}}}}"#
        ),
        status: OutboundPrivateMessageStatus::Pending,
        attempt_count: 0,
        created_at: timestamp(),
        updated_at: timestamp(),
        last_attempt_at: None,
        sent_at: None,
        last_error: None,
    }
}

async fn assert_restore_rejects_outbound_record(record: OutboundPrivateMessageRecord) {
    let storage = InMemoryStorage::new();
    let counterparty = record.counterparty.clone();
    let next_id = record.outbound_message_id.saturating_add(1);
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![record],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: next_id,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

fn receipt_access_raw_with_context(
    event_id: &str,
    receipt_id: &str,
    payment_reference: &str,
    payment_request_id: &str,
    billing_period: &BillingPeriodRecord,
) -> (String, String, String) {
    let receipt_id = ReceiptId::new(receipt_id).unwrap();
    let location = paykit_lib::ReceiptAccess::location(&receiver_id(), &receipt_id);
    let key = paykit_lib::ReceiptDecryptionKey::generate()
        .as_str()
        .to_owned();
    let raw_json = format!(
        r#"{{"version":1,"kind":"paykit.receipt_access","event_id":"{event_id}","receipt_id":"{}","payment_reference":"{payment_reference}","payment_request_id":"{payment_request_id}","billing_period":{{"starts_at":"{}","ends_at":"{}"}},"location":"{location}","key":"{key}"}}"#,
        receipt_id.as_str(),
        billing_period.starts_at,
        billing_period.ends_at
    );
    (raw_json, location, key)
}

mod basic;
mod counters;
mod recovery;
mod validation;
