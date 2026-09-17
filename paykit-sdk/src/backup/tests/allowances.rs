use super::*;
use crate::domain::{
    allowances::{allowance_record, AllowanceHistoryStatus, AllowanceLifecycleState},
    private_stream::persist_private_stream_batch,
};
use paykit_lib::AllowanceId;

const SHARED_EVENT_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201";
const ALLOWANCE_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";

async fn persist_messages(
    storage: &InMemoryStorage,
    counterparty: &PubkyPublicKey,
    receiver: PaykitReceiverPath,
    payloads: Vec<String>,
) {
    let messages = payloads
        .into_iter()
        .map(|raw| {
            let (version, kind, _) = private_message_header(&raw).unwrap();
            private_application_message_from_raw(raw, version, kind)
        })
        .collect();
    persist_private_stream_batch(
        storage,
        counterparty.clone(),
        receiver,
        messages,
        None,
        timestamp(),
    )
    .await
    .unwrap();
}

async fn current_backup(counterparty: &PubkyPublicKey, payloads: Vec<String>) -> SdkBackupState {
    let storage = InMemoryStorage::new();
    storage
        .transaction(|tx| {
            tx.save_identity_state(identity(public_key()));
            Ok(())
        })
        .await
        .unwrap();
    persist_messages(&storage, counterparty, receiver_path(), payloads).await;
    export_backup_state(&storage, receiver_path())
        .await
        .unwrap()
}

async fn assert_rejected_without_destination_changes(backup: SdkBackupState, reason: &str) {
    let storage = InMemoryStorage::new();
    let existing_identity = backup.identity_state.clone().unwrap();
    storage
        .transaction(move |tx| {
            tx.save_identity_state(existing_identity);
            tx.save_contact_record(contact_record(public_key()));
            Ok(())
        })
        .await
        .unwrap();
    persist_messages(
        &storage,
        &public_key(),
        receiver_path(),
        vec![payment_request_json(SHARED_EVENT_ID)],
    )
    .await;
    let before = export_backup_state(&storage, receiver_path())
        .await
        .unwrap();

    let error = restore_backup_state(&storage, backup).await.unwrap_err();

    assert!(matches!(error, PaykitSdkError::Protocol { .. }));
    assert!(error.to_string().contains(reason), "{error}");
    assert_eq!(
        export_backup_state(&storage, receiver_path())
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn test_restore_allowance_current_format_roundtrips_without_reclassification() {
    let counterparty = public_key();
    let backup = current_backup(
        &counterparty,
        allowance_event_jsons()
            .into_iter()
            .map(|(_, raw)| raw)
            .collect(),
    )
    .await;
    assert_eq!(backup.event_dedup_records.len(), 4);
    let restored = InMemoryStorage::new();

    restore_backup_state(&restored, backup.clone())
        .await
        .unwrap();

    let roundtrip = export_backup_state(&restored, receiver_path())
        .await
        .unwrap();
    assert_eq!(roundtrip, backup);
    let second = InMemoryStorage::new();
    restore_backup_state(&second, roundtrip).await.unwrap();
    assert_eq!(
        export_backup_state(&second, receiver_path()).await.unwrap(),
        backup
    );
}

#[tokio::test]
async fn test_restore_allowance_rejects_stale_unknown_kind_metadata_atomically() {
    let mut backup = current_backup(
        &public_key(),
        vec![allowance_event_json(
            "paykit.allowance_proposal",
            SHARED_EVENT_ID,
        )],
    )
    .await;
    let item = &mut backup.private_stream_items[0];
    item.known_paykit_kind = None;
    item.parse_status = PrivateStreamParseStatus::UnknownKind;
    item.parse_error = None;

    assert_rejected_without_destination_changes(backup, "stale known kind metadata").await;
}

#[tokio::test]
async fn test_restore_allowance_rejects_missing_dedupe_atomically() {
    let mut backup = current_backup(
        &public_key(),
        vec![allowance_event_json(
            "paykit.allowance_proposal",
            SHARED_EVENT_ID,
        )],
    )
    .await;
    backup.event_dedup_records.clear();

    assert_rejected_without_destination_changes(backup, "missing required Event dedupe").await;
}

#[tokio::test]
async fn test_restore_allowance_preserves_current_unsupported_correlated_evidence() {
    let counterparty = public_key();
    let unsupported = format!(
        r#"{{"version":256,"kind":"paykit.allowance_future","allowance_id":"{ALLOWANCE_ID}","private_sentinel":true}}"#
    );
    let malformed = allowance_event_json(
        "paykit.allowance_proposal",
        "650e8400-e29b-41d4-a716-446655440000",
    )
    .replacen('{', r#"{"private_sentinel":true,"#, 1);
    for (evidence, status) in [
        (unsupported, PrivateStreamParseStatus::InvalidJson),
        (malformed, PrivateStreamParseStatus::MalformedRecognized),
    ] {
        let backup = current_backup(
            &counterparty,
            vec![
                allowance_event_json("paykit.allowance_proposal", SHARED_EVENT_ID),
                evidence,
            ],
        )
        .await;
        assert_eq!(backup.private_stream_items[1].parse_status, status);
        let storage = InMemoryStorage::new();

        restore_backup_state(&storage, backup.clone())
            .await
            .unwrap();

        assert_eq!(
            export_backup_state(&storage, receiver_path())
                .await
                .unwrap(),
            backup
        );
        let record = allowance_record(
            &storage,
            &counterparty,
            &receiver_path(),
            &AllowanceId::new(ALLOWANCE_ID).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(record.state, AllowanceLifecycleState::Proposed);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
        assert!(!format!("{record:?}").contains("private_sentinel"));
    }
}

#[tokio::test]
async fn test_restore_allowance_preserves_cross_kind_dedupe_and_exact_link_scope() {
    let counterparty = public_key();
    let allowance = allowance_event_json("paykit.allowance_proposal", SHARED_EVENT_ID);
    let mut backup = current_backup(
        &counterparty,
        vec![
            allowance.clone(),
            allowance,
            payment_request_json(SHARED_EVENT_ID),
        ],
    )
    .await;
    let storage = InMemoryStorage::new();
    restore_backup_state(&storage, backup).await.unwrap();
    persist_messages(
        &storage,
        &counterparty,
        other_receiver_path(),
        vec![payment_request_json(SHARED_EVENT_ID)],
    )
    .await;
    backup = export_backup_state(&storage, receiver_path())
        .await
        .unwrap();
    let restored = InMemoryStorage::new();

    restore_backup_state(&restored, backup.clone())
        .await
        .unwrap();

    assert_eq!(
        export_backup_state(&restored, receiver_path())
            .await
            .unwrap(),
        backup
    );
    let state = restored.snapshot().unwrap();
    let dedupe = &state.event_dedup_records[&(
        counterparty.clone(),
        receiver_path(),
        SHARED_EVENT_ID.into(),
    )];
    assert_eq!(dedupe.event_kind, "paykit.allowance_proposal");
    assert_eq!(dedupe.duplicate_stream_item_ids.len(), 1);
    assert_eq!(dedupe.conflicting_stream_item_ids.len(), 1);
    let other =
        &state.event_dedup_records[&(counterparty, other_receiver_path(), SHARED_EVENT_ID.into())];
    assert_eq!(other.event_kind, "paykit.payment_request");
    assert!(other.duplicate_stream_item_ids.is_empty());
    assert!(other.conflicting_stream_item_ids.is_empty());
}

#[tokio::test]
async fn test_restore_allowance_rejects_receipt_index_for_conflicting_event() {
    let counterparty = public_key();
    let (receipt, _, _) = receipt_access_raw_with_context(
        SHARED_EVENT_ID,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &BillingPeriodRecord {
            starts_at: "2026-06-01T00:00:00Z".into(),
            ends_at: "2026-07-01T00:00:00Z".into(),
        },
    );
    let mut backup = current_backup(
        &counterparty,
        vec![
            allowance_event_json("paykit.allowance_proposal", SHARED_EVENT_ID),
            receipt.clone(),
        ],
    )
    .await;
    assert!(backup.receipt_access_records.is_empty());
    assert!(backup.receipt_records.is_empty());
    let clean = InMemoryStorage::new();
    restore_backup_state(&clean, backup.clone()).await.unwrap();
    assert_eq!(
        export_backup_state(&clean, receiver_path()).await.unwrap(),
        backup
    );

    // A later conflicting Receipt Access must never acquire an authoritative
    // index during restore, even when its own payload and receipt scope are valid.
    let receipt_backup = current_backup(&counterparty, vec![receipt]).await;
    let mut access = receipt_backup.receipt_access_records[0].clone();
    access.stream_item_id = backup.private_stream_items[1].stream_item_id;
    access.receive_batch_id = backup.private_stream_items[1].receive_batch_id;
    backup.receipt_access_records.push(access);

    assert_rejected_without_destination_changes(backup, "authoritative").await;
}
