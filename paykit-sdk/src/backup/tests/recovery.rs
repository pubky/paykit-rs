use super::*;

#[tokio::test]
async fn test_restore_backup_state_marks_link_state_without_peer_recovery_required() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: vec![EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: 0,
            checkpointed_at: timestamp(),
        }],
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(
        restored
            .linked_peers
            .get(&peer_key(&counterparty))
            .unwrap()
            .state,
        LinkedPeerState::RecoveryRequired
    );
}

#[tokio::test]
async fn test_restore_backup_state_preserves_private_stream_without_forcing_recovery() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            receive_batch_id: 0,
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                    .into(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.private_payment_list".into()),
            known_paykit_kind: Some("paykit.private_payment_list".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert!(!restored.linked_peers.contains_key(&peer_key(&counterparty)));
}
