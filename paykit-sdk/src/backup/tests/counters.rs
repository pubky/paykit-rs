use super::*;

#[tokio::test]
async fn test_restore_backup_state_advances_counters() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![OutboundPrivateMessageRecord {
            outbound_message_id: 7,
            counterparty: counterparty.clone(),
            app_id: app_id(),
            kind: "paykit.private_payment_list".into(),
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#
                    .into(),
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: timestamp(),
            updated_at: timestamp(),
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
            prepared_send: None,
        }],
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 9,
            counterparty,
            receive_batch_id: 3,
            raw_json: "{}".into(),
            parsed_version: None,
            parsed_kind: None,
            parsed_app_id: None,
            known_paykit_kind: None,
            parse_status: PrivateStreamParseStatus::InvalidJson,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 1,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 1,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(restored.next_outbound_private_message_id, 8);
    assert_eq!(restored.next_receive_batch_id, 4);
    assert_eq!(restored.next_private_stream_item_id, 10);
}

#[tokio::test]
async fn test_restore_backup_state_preserves_exhausted_counter() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let identity = identity(counterparty.clone());
    let mut backup = empty_backup(identity);
    backup.next_outbound_private_message_id = u64::MAX;

    restore_backup_state(&storage, backup).await.unwrap();
    let error = storage
        .transaction(move |tx| {
            tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty,
                app_id(),
                "paykit.private_payment_list".into(),
                r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#
                    .into(),
                timestamp(),
            ))?;
            Ok(())
        })
        .await
        .unwrap_err();

    assert!(matches!(error, PaykitSdkError::Storage { .. }));
    let state = storage.snapshot().unwrap();
    assert_eq!(state.next_outbound_private_message_id, u64::MAX);
    assert!(state.outbound_private_messages.is_empty());
}
