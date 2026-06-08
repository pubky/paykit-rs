use super::*;

#[tokio::test]
async fn test_restore_backup_state_rejects_malformed_link_snapshot() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: vec![EncryptedLinkStateRecord {
            counterparty,
            link_snapshot: Some(vec![1, 2, 3]),
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
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_records_without_identity() {
    let storage = InMemoryStorage::new();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: vec![PublicEndpointRecord {
            identifier: "btc-lightning-bolt11".into(),
            payload: Some("ln".into()),
            status: crate::EndpointPublicationStatus::Published,
            updated_at: timestamp(),
            last_error: None,
        }],
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_public_endpoint_record() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: vec![PublicEndpointRecord {
            identifier: "private".into(),
            payload: Some("ln".into()),
            status: crate::EndpointPublicationStatus::Published,
            updated_at: timestamp(),
            last_error: None,
        }],
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_stale_private_stream_metadata() {
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
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty,
            receive_batch_id: 0,
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                    .into(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_stale_private_stream_parse_status() {
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
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty,
            receive_batch_id: 0,
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                    .into(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.private_payment_list".into()),
            known_paykit_kind: Some("paykit.private_payment_list".into()),
            parse_status: PrivateStreamParseStatus::MalformedRecognized,
            parse_error: Some("stale".into()),
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_stale_private_stream_parse_error() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let period = ReceiptBillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, _) = receipt_access_raw_with_context(
        "650e8400-e29b-41d4-a716-446655440000",
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let raw_json = raw_json.replace(
        &location,
        "/pub/paykit/v0/private/receipts/not-the-receipt-id",
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty,
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::MalformedRecognized,
            parse_error: Some("stale".into()),
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_stale_dedupe_event_header() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
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
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: "750e8400-e29b-41d4-a716-446655440000".into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_overlapping_event_dedupe_membership() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
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
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: vec![1],
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_accepts_cross_kind_event_id_conflict() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let request_json = payment_request_json(event_id);
    let period = ReceiptBillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (receipt_access_json, _, _) = receipt_access_raw_with_context(
        event_id,
        "750e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "550e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![
            PrivateStreamItemRecord {
                stream_item_id: 1,
                counterparty: counterparty.clone(),
                receive_batch_id: 0,
                raw_json: request_json.clone(),
                parsed_version: Some(1),
                parsed_kind: Some("paykit.payment_request".into()),
                known_paykit_kind: Some("paykit.payment_request".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            },
            PrivateStreamItemRecord {
                stream_item_id: 2,
                counterparty: counterparty.clone(),
                receive_batch_id: 0,
                raw_json: receipt_access_json,
                parsed_version: Some(1),
                parsed_kind: Some("paykit.receipt_access".into()),
                known_paykit_kind: Some("paykit.receipt_access".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            },
        ],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: event_id.into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&request_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: vec![2],
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 3,
    };

    restore_backup_state(&storage, backup).await.unwrap();
}

#[tokio::test]
async fn test_restore_backup_state_rejects_missing_event_dedupe_index() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty,
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_missing_receipt_access_index() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let period = ReceiptBillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, _, _) = receipt_access_raw_with_context(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
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
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_access_context_mismatch() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_reference = "invoice-2026-0001";
    let period = ReceiptBillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        payment_reference,
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
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
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![ReceiptAccessRecord {
            counterparty,
            stream_item_id: 1,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.into(),
            payment_reference: payment_reference.into(),
            payment_request_id: Some("850e8400-e29b-41d4-a716-446655440000".into()),
            billing_period: Some(period),
            location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

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
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![invalid],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
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
async fn test_restore_backup_state_rejects_wrong_identity() {
    let storage = InMemoryStorage::new();
    let current = public_key();
    let backup_public_key = public_key();
    storage
        .save_identity_state(identity(current))
        .await
        .unwrap();

    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(backup_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_preserves_current_sign_out_generation() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let mut current_identity = identity(local_public_key.clone());
    current_identity.sign_out_generation = 7;
    storage.save_identity_state(current_identity).await.unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .identity_state
            .unwrap()
            .sign_out_generation,
        7
    );
}

#[tokio::test]
async fn test_restore_identity_less_backup_preserves_signed_out_generation() {
    let storage = InMemoryStorage::new();
    storage
        .save_identity_state(signed_out_identity(7))
        .await
        .unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    restore_backup_state(&storage, backup).await.unwrap();

    let identity = storage.snapshot().unwrap().identity_state.unwrap();
    assert_eq!(identity.capability, PubkyIdentityCapability::SignedOut);
    assert_eq!(identity.sign_out_generation, 7);
}

#[tokio::test]
async fn test_restore_backup_state_rejects_orphan_endpoint_reservation() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty,
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("ln-private"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            release_started_at: None,
            created_at: timestamp(),
        }],
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_endpoint_reservation_id() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation\n1".into(),
            counterparty: counterparty.clone(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("ln-private"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            release_started_at: None,
            created_at: timestamp(),
        }],
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![private_payment_list_outbound(
            counterparty,
            7,
            "ln-private",
        )],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_mismatched_endpoint_reservation_payload() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("different-payload"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            release_started_at: None,
            created_at: timestamp(),
        }],
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![private_payment_list_outbound(
            counterparty,
            7,
            "ln-private",
        )],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_key_hash_mismatch() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let access = ReceiptAccessRecord {
        counterparty: counterparty.clone(),
        stream_item_id: 0,
        receive_batch_id: 0,
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        location: format!("/pub/paykit/v0/private/receipts/{receipt_id}"),
        key: "receipt-secret".into(),
        retrieval_status: crate::ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: counterparty.clone(),
        receipt_access_event_id: access.event_id.clone(),
        receipt_access_key_hash: receipt_access_key_hash("wrong-secret"),
        receipt_id: receipt_id.into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: counterparty.clone(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location: access.location.clone(),
        retrieved_at: timestamp(),
    };
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![access],
        receipt_records: vec![receipt],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 1,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_recipient_mismatch() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let issuer = public_key();
    let wrong_recipient = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let payment_request_id = "750e8400-e29b-41d4-a716-446655440000";
    let period = ReceiptBillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        "invoice-2026-0001",
        payment_request_id,
        &period,
    );
    let access = ReceiptAccessRecord {
        counterparty: issuer.clone(),
        stream_item_id: 1,
        receive_batch_id: 0,
        event_id: event_id.into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: Some(payment_request_id.into()),
        billing_period: Some(period.clone()),
        location: location.clone(),
        key: key.clone(),
        retrieval_status: crate::ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: issuer.clone(),
        receipt_access_event_id: event_id.into(),
        receipt_access_key_hash: receipt_access_key_hash(&key),
        receipt_id: receipt_id.into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: access.payment_request_id.clone(),
        billing_period: access.billing_period.clone(),
        recipient_public_key: wrong_recipient,
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location,
        retrieved_at: timestamp(),
    };
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: issuer.clone(),
            receive_batch_id: 0,
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty: issuer,
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: vec![access],
        receipt_records: vec![receipt],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}
