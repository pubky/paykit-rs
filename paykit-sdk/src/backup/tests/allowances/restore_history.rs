use super::*;

#[tokio::test]
async fn test_restore_preserves_newer_allowance_evidence_without_accounting() {
    let proposal = allowance_event_json("paykit.allowance_proposal", SHARED_EVENT_ID);
    let end = allowance_event_json("paykit.allowance_end", ALLOWANCE_EVENT_FIXTURES[3].1);
    let withdrawal = end.replace(
        "\"acceptance_event_id\":\"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202\"",
        "\"acceptance_event_id\":null",
    );
    let unknown =
        format!(r#"{{"version":256,"kind":"paykit.future","allowance_id":"{ALLOWANCE_ID}"}}"#);
    let conflicting_receipt =
        crate::test_utils::receipt_access_json(SHARED_EVENT_ID, &other_receiver_path());
    for raw in [
        end,
        withdrawal,
        allowance_event_json("paykit.allowance_rejection", ALLOWANCE_EVENT_FIXTURES[2].1),
        crate::test_utils::malformed_allowance_event_json(),
        unknown,
        conflicting_receipt,
    ] {
        let peer = public_key();
        let backup = current_backup(&peer, vec![proposal.clone()]).await;
        let storage = InMemoryStorage::new();
        restore_backup_state(&storage, backup.clone())
            .await
            .unwrap();
        persist_messages(&storage, &peer, receiver_path(), vec![raw]).await;
        let before = storage.snapshot().unwrap();
        assert!(before.allowance_accounting.is_none());
        let error = restore_backup_state(&storage, backup).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("discard retained Allowance or Payment Request history"),
            "{error}"
        );
        assert_eq!(storage.snapshot().unwrap(), before);
    }
}

#[tokio::test]
async fn test_restore_evidence_requires_same_scope_direction_order_and_bytes() {
    let peer = public_key();
    let backup = current_backup(
        &peer,
        vec![allowance_event_json(
            "paykit.allowance_end",
            SHARED_EVENT_ID,
        )],
    )
    .await;
    let storage = InMemoryStorage::new();
    restore_backup_state(&storage, backup).await.unwrap();
    let current = storage.snapshot().unwrap();
    let check = crate::domain::allowances::ensure_payment_lifecycle_history_retained;
    check(&current, &current).unwrap();
    for mutation in 0..5 {
        let mut candidate = current.clone();
        let item = &mut candidate.private_stream_items[0];
        match mutation {
            0 => item.counterparty = public_key(),
            1 => item.counterparty_receiver_path = other_receiver_path(),
            2 => item.stream_item_id += 1,
            3 => item.raw_json.push(' '),
            _ => {
                let item = candidate.private_stream_items.pop().unwrap();
                let mut outbound =
                    private_payment_list_outbound(peer.clone(), item.stream_item_id, "unused");
                outbound.kind = "paykit.allowance_end".into();
                outbound.raw_json = item.raw_json;
                candidate.outbound_private_messages.push(outbound);
            }
        }
        assert!(check(&current, &candidate).is_err());
    }
}

#[tokio::test]
async fn test_restore_preserves_payment_request_lifecycle_evidence() {
    let peer = public_key();
    let proposal = payment_request_json(SHARED_EVENT_ID);
    let request_id = serde_json::from_str::<serde_json::Value>(&proposal).unwrap()
        ["payment_request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let cancellation = format!(
        r#"{{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d299","payment_request_id":"{request_id}"}}"#
    );
    let malformed = cancellation.replace("\"version\":1", "\"version\":256");
    let unknown =
        format!(r#"{{"version":256,"kind":"paykit.future","payment_request_id":"{request_id}"}}"#);
    let conflict = crate::test_utils::receipt_access_json(SHARED_EVENT_ID, &other_receiver_path());
    let mut payloads = vec![cancellation, malformed, unknown, conflict];
    for kind in [
        "paykit.payment_request",
        "paykit.payment_request_acceptance",
        "paykit.payment_request_rejection",
        "paykit.payment_conversion_quote",
        "paykit.payment_proof",
    ] {
        // A recognized malformed event without a Request ID is still retained.
        payloads.push(format!(r#"{{"version":1,"kind":"{kind}"}}"#));
    }
    for raw in payloads {
        let backup = current_backup(&peer, vec![proposal.clone()]).await;
        let storage = InMemoryStorage::new();
        restore_backup_state(&storage, backup.clone())
            .await
            .unwrap();
        persist_messages(&storage, &peer, receiver_path(), vec![raw]).await;
        let before = storage.snapshot().unwrap();
        assert!(before.allowance_accounting.is_none());
        let error = restore_backup_state(&storage, backup).await.unwrap_err();
        assert!(error.to_string().contains("discard retained"), "{error}");
        assert_eq!(storage.snapshot().unwrap(), before);
        let current = export_backup_state(&storage, receiver_path())
            .await
            .unwrap();
        restore_backup_state(&storage, current).await.unwrap();
        assert_eq!(storage.snapshot().unwrap(), before);
    }
}

#[tokio::test]
async fn test_restore_payment_request_evidence_requires_same_scope_direction_order_and_bytes() {
    let peer = public_key();
    let backup = current_backup(&peer, vec![payment_request_json(SHARED_EVENT_ID)]).await;
    let storage = InMemoryStorage::new();
    restore_backup_state(&storage, backup).await.unwrap();
    let current = storage.snapshot().unwrap();
    let check = crate::domain::allowances::ensure_payment_lifecycle_history_retained;
    for mutation in 0..5 {
        let mut candidate = current.clone();
        let item = &mut candidate.private_stream_items[0];
        match mutation {
            0 => item.counterparty = public_key(),
            1 => item.counterparty_receiver_path = other_receiver_path(),
            2 => item.stream_item_id += 1,
            3 => item.raw_json.push(' '),
            _ => {
                let item = candidate.private_stream_items.pop().unwrap();
                let mut outbound =
                    private_payment_list_outbound(peer.clone(), item.stream_item_id, "unused");
                outbound.kind = "paykit.payment_request".into();
                outbound.raw_json = item.raw_json;
                candidate.outbound_private_messages.push(outbound);
            }
        }
        assert!(check(&current, &candidate).is_err(), "mutation {mutation}");
    }
}

#[tokio::test]
async fn test_restore_payment_request_conflicts_are_receiver_scoped() {
    let peer = public_key();
    let backup = current_backup(&peer, vec![payment_request_json(SHARED_EVENT_ID)]).await;
    let storage = InMemoryStorage::new();
    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();
    let raw = crate::test_utils::receipt_access_json(SHARED_EVENT_ID, &other_receiver_path());
    persist_messages(&storage, &peer, other_receiver_path(), vec![raw]).await;
    let current = storage.snapshot().unwrap();
    let check = crate::domain::allowances::ensure_payment_lifecycle_history_retained;
    check(&current, &restored).unwrap();
    let mut conflicting = current;
    conflicting
        .private_stream_items
        .last_mut()
        .unwrap()
        .counterparty_receiver_path = receiver_path();
    assert!(check(&conflicting, &restored).is_err());
}

#[tokio::test]
async fn test_restore_retains_outbound_payment_request_cancellation_intent() {
    let peer = public_key();
    let backup = current_backup(&peer, vec![payment_request_json(SHARED_EVENT_ID)]).await;
    let storage = InMemoryStorage::new();
    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();
    let check = crate::domain::allowances::ensure_payment_lifecycle_history_retained;
    for status in [
        OutboundPrivateMessageStatus::Pending,
        OutboundPrivateMessageStatus::Sent,
        OutboundPrivateMessageStatus::RecoveryRequired,
    ] {
        let mut current = restored.clone();
        let mut outbound = private_payment_list_outbound(peer.clone(), 0, "unused");
        outbound.kind = "paykit.payment_request_cancellation".into();
        outbound.raw_json = format!(
            r#"{{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"{SHARED_EVENT_ID}","payment_request_id":"550e8400-e29b-41d4-a716-446655440000"}}"#
        );
        outbound.status = status;
        current.outbound_private_messages.push(outbound);
        assert!(check(&current, &restored).is_err());
        check(&current, &current).unwrap();
    }
}
