use super::*;
use crate::storage::PreparedOutboundPrivateSend;

#[tokio::test]
async fn test_restore_backup_state_preserves_invalid_outbound_audit_record() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut invalid = private_payment_list_outbound(counterparty.clone(), 7, "ln-private");
    invalid.raw_json = "{malformed".into();
    invalid.status = OutboundPrivateMessageStatus::Invalid;
    invalid.last_error = Some("invalid private message JSON".into());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![invalid],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(restored.outbound_private_messages.len(), 1);
    assert_eq!(
        restored.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::Invalid
    );
    assert_eq!(restored.outbound_private_messages[0].raw_json, "{malformed");
}

#[tokio::test]
async fn test_restore_backup_state_preserves_recovery_required_outbound_audit_record() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut recovery_required =
        private_payment_list_outbound(counterparty.clone(), 7, "ln-private");
    recovery_required.raw_json = "{malformed".into();
    recovery_required.status = OutboundPrivateMessageStatus::RecoveryRequired;
    recovery_required.last_error = Some("Encrypted Link recovery is required".into());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![recovery_required],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(restored.outbound_private_messages.len(), 1);
    assert_eq!(
        restored.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert_eq!(restored.outbound_private_messages[0].raw_json, "{malformed");
}

#[tokio::test]
async fn test_restore_rejects_retired_app_recovery_required_message() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut message = private_payment_list_outbound(counterparty.clone(), 7, "ln-private");
    message.status = OutboundPrivateMessageStatus::RecoveryRequired;
    message.last_error = Some("Encrypted Link recovery is required".into());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: vec![app_id()],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![message],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_rejects_retired_app_shared_private_payment_list() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut message = private_payment_list_outbound(counterparty.clone(), 7, "ln-private");
    message.status = OutboundPrivateMessageStatus::Sent;
    message.attempt_count = 1;
    message.last_attempt_at = Some(timestamp());
    message.sent_at = Some(timestamp());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: vec![app_id()],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![message],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_rejects_retired_app_active_payment_request() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut message = private_payment_list_outbound(counterparty.clone(), 7, "unused");
    message.kind = PrivateMessageKind::PaymentRequest.as_str().into();
    message.raw_json = payment_request_json("550e8400-e29b-41d4-a716-446655440001");
    message.status = OutboundPrivateMessageStatus::Sent;
    message.attempt_count = 1;
    message.last_attempt_at = Some(timestamp());
    message.sent_at = Some(timestamp());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: vec![app_id()],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![message],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_rejects_retired_app_incomplete_receipt_issuance() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let counterparty = public_key();
    let prepared = paykit_lib::prepare_receipt_for_recipient(
        counterparty.to_public_key().unwrap(),
        paykit_lib::ReceiptDraft {
            receipt_id: Some(
                paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            ),
            payment_reference: paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
            payment_request_id: None,
            billing_period: None,
            payment_endpoint_identifier: None,
            amount: None,
            metadata: serde_json::Map::new(),
        },
    )
    .unwrap();
    let issuance =
        ReceiptIssuanceRecord::from_prepared(counterparty, app_id(), prepared, timestamp())
            .unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: vec![app_id()],
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: vec![issuance],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_marks_sending_outbound_recovery_required() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let mut sending = private_payment_list_outbound(counterparty.clone(), 7, "ln-private");
    sending.status = OutboundPrivateMessageStatus::Sending;
    sending.attempt_count = 1;
    sending.last_attempt_at = Some(timestamp());
    sending.prepared_send = Some(PreparedOutboundPrivateSend {
        destination_path: format!(
            "{}/{}/0",
            paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX,
            "0".repeat(64)
        ),
        ciphertext: vec![0; pubky_noise::snow_crypto::PUBKY_NOISE_CIPHERTEXT_LEN + 2],
    });
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![sending],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 8,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(
        restored.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert!(restored.outbound_private_messages[0]
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("recovery")));
    assert!(restored.outbound_private_messages[0]
        .prepared_send
        .is_none());
}

#[tokio::test]
async fn test_restore_backup_state_rejects_prepared_send_on_pending_message() {
    let counterparty = public_key();
    let mut pending = private_payment_list_outbound(counterparty, 7, "ln-private");
    pending.prepared_send = Some(PreparedOutboundPrivateSend {
        destination_path: format!(
            "{}/{}/0",
            paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX,
            "0".repeat(64)
        ),
        ciphertext: vec![0; pubky_noise::snow_crypto::PUBKY_NOISE_CIPHERTEXT_LEN + 2],
    });

    assert_restore_rejects_outbound_record(pending).await;
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_prepared_send() {
    let counterparty = public_key();
    let mut sending = private_payment_list_outbound(counterparty, 7, "ln-private");
    sending.status = OutboundPrivateMessageStatus::Sending;
    sending.attempt_count = 1;
    sending.last_attempt_at = Some(timestamp());
    sending.prepared_send = Some(PreparedOutboundPrivateSend {
        destination_path: "/pub/paykit/v0/private/../0".into(),
        ciphertext: vec![0; 8],
    });

    assert_restore_rejects_outbound_record(sending).await;
}

#[tokio::test]
async fn test_restore_backup_state_rejects_sent_outbound_without_sent_time() {
    let counterparty = public_key();
    let mut sent = private_payment_list_outbound(counterparty, 7, "ln-private");
    sent.status = OutboundPrivateMessageStatus::Sent;
    sent.attempt_count = 1;
    sent.last_attempt_at = Some(timestamp());

    assert_restore_rejects_outbound_record(sent).await;
}

#[tokio::test]
async fn test_restore_backup_state_rejects_stale_outbound_app_id() {
    let counterparty = public_key();
    let mut pending = private_payment_list_outbound(counterparty, 7, "ln-private");
    pending.app_id = paykit_lib::PaykitAppId::new("tether").unwrap();

    assert_restore_rejects_outbound_record(pending).await;
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_outbound_without_error() {
    let counterparty = public_key();
    let mut invalid = private_payment_list_outbound(counterparty, 7, "ln-private");
    invalid.status = OutboundPrivateMessageStatus::Invalid;

    assert_restore_rejects_outbound_record(invalid).await;
}

#[tokio::test]
async fn test_restore_backup_state_rejects_recovery_required_outbound_with_sent_time() {
    let counterparty = public_key();
    let mut recovery_required = private_payment_list_outbound(counterparty, 7, "ln-private");
    recovery_required.status = OutboundPrivateMessageStatus::RecoveryRequired;
    recovery_required.last_error = Some("Encrypted Link recovery is required".into());
    recovery_required.sent_at = Some(timestamp());

    assert_restore_rejects_outbound_record(recovery_required).await;
}
