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
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![OutboundPrivateMessageRecord {
            outbound_message_id: 7,
            counterparty: counterparty.clone(),
            kind: "paykit.private_payment_list".into(),
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                    .into(),
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: timestamp(),
            updated_at: timestamp(),
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
        }],
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 9,
            counterparty,
            receive_batch_id: 3,
            raw_json: "{}".into(),
            parsed_version: None,
            parsed_kind: None,
            known_paykit_kind: None,
            parse_status: PrivateStreamParseStatus::InvalidJson,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
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
