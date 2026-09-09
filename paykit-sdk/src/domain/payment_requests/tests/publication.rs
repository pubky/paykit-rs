use super::*;
use crate::storage::{InMemoryStorage, StorageTransactionCallback};
use crate::PaykitSdkError;
use std::sync::atomic::{AtomicBool, Ordering};

struct AmbiguousStorage {
    inner: InMemoryStorage,
    fail_after_commit: AtomicBool,
}

#[async_trait::async_trait]
impl StorageAdapter for AmbiguousStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        let result = self.inner.transaction_erased(f).await?;
        if self.fail_after_commit.swap(false, Ordering::SeqCst) {
            return Err(storage_error());
        }
        Ok(result)
    }
}

fn storage_error() -> PaykitSdkError {
    PaykitSdkError::Storage {
        context: "injected storage failure".into(),
        source: None,
    }
}

fn proposal() -> PaymentRequest {
    let PaymentRequestEvent::Request(mut event) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
        "invoice",
        None,
        None,
    )) else {
        panic!("expected proposal")
    };
    event.request.metadata.insert(
        "icon_uri".into(),
        JsonValue::String("pubky://shared/avatar.png".into()),
    );
    event
}

#[tokio::test]
async fn test_publication_reconciles_after_commit_failure_without_duplicate() {
    let storage = AmbiguousStorage {
        inner: InMemoryStorage::new(),
        fail_after_commit: AtomicBool::new(true),
    };
    let target = counterparty();
    let event = proposal();
    let first = publish_payment_request(
        &storage,
        target.clone(),
        receiver_path(),
        &event,
        Ok(()),
        timestamp(),
    )
    .await;
    assert!(matches!(first, PaymentRequestPublication::Uncertain { .. }));
    let queued = storage.inner.snapshot().unwrap().outbound_private_messages;
    assert_eq!(queued.len(), 1);
    // Reconciliation does not require a live link for an already queued event.
    let retry = publish_payment_request(
        &storage,
        target,
        receiver_path(),
        &event,
        Err(storage_error()),
        timestamp(),
    )
    .await;
    assert!(matches!(retry, PaymentRequestPublication::Queued { .. }));
    assert_eq!(
        storage.inner.snapshot().unwrap().outbound_private_messages,
        queued
    );
}

#[tokio::test]
async fn test_publication_not_queued_and_retry_after_readiness_recovers() {
    let storage = InMemoryStorage::new();
    let target = counterparty();
    let event = proposal();
    let first = publish_payment_request(
        &storage,
        target.clone(),
        receiver_path(),
        &event,
        Err(storage_error()),
        timestamp(),
    )
    .await;
    assert!(matches!(first, PaymentRequestPublication::NotQueued { .. }));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
    let retry = publish_payment_request(
        &storage,
        target,
        receiver_path(),
        &event,
        Ok(()),
        timestamp(),
    )
    .await;
    assert!(matches!(retry, PaymentRequestPublication::Queued { .. }));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        1
    );
}

#[tokio::test]
async fn test_publication_concurrent_retries_and_sent_reconciliation_preserve_content() {
    let storage = InMemoryStorage::new();
    let target = counterparty();
    let event = proposal();
    let (a, b) = tokio::join!(
        publish_payment_request(
            &storage,
            target.clone(),
            receiver_path(),
            &event,
            Ok(()),
            timestamp()
        ),
        publish_payment_request(
            &storage,
            target.clone(),
            receiver_path(),
            &event,
            Ok(()),
            timestamp()
        ),
    );
    let PaymentRequestPublication::Queued {
        outbound_message_id: a,
    } = a
    else {
        panic!("expected queued")
    };
    let PaymentRequestPublication::Queued {
        outbound_message_id: b,
    } = b
    else {
        panic!("expected queued")
    };
    assert_eq!(a, b);
    storage
        .transaction(|tx| {
            let mut record = tx
                .outbound_private_messages(&target, &receiver_path())
                .remove(0);
            record.status = OutboundPrivateMessageStatus::Sent;
            tx.save_outbound_private_message(record)
        })
        .await
        .unwrap();
    let before = storage.snapshot().unwrap();
    let retry = publish_payment_request(
        &storage,
        target.clone(),
        receiver_path(),
        &event,
        Ok(()),
        timestamp(),
    )
    .await;
    assert!(
        matches!(retry, PaymentRequestPublication::Queued { outbound_message_id } if outbound_message_id == a)
    );
    let mut conflict = event.clone();
    conflict.request.metadata.clear();
    assert!(matches!(
        publish_payment_request(
            &storage,
            target,
            receiver_path(),
            &conflict,
            Ok(()),
            timestamp()
        )
        .await,
        PaymentRequestPublication::Uncertain { .. }
    ));
    assert_eq!(storage.snapshot().unwrap(), before);
    // The original proposal, including its shared icon reference, is retained.
    assert!(before
        .outbound_private_messages
        .first()
        .unwrap()
        .raw_json
        .contains("pubky://shared/avatar.png"));
}

#[tokio::test]
async fn test_publication_rejects_oversized_terms_without_queueing() {
    let storage = InMemoryStorage::new();
    let mut event = proposal();
    event
        .request
        .metadata
        .insert("large".into(), JsonValue::String("x".repeat(1000)));
    assert!(matches!(
        publish_payment_request(
            &storage,
            counterparty(),
            receiver_path(),
            &event,
            Ok(()),
            timestamp()
        )
        .await,
        PaymentRequestPublication::NotQueued { .. }
    ));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_publication_survives_derived_record_failure() {
    let storage = AmbiguousStorage {
        inner: InMemoryStorage::new(),
        fail_after_commit: AtomicBool::new(false),
    };
    let target = counterparty();
    let event = proposal();
    let published = publish_payment_request(
        &storage,
        target.clone(),
        receiver_path(),
        &event,
        Ok(()),
        timestamp(),
    )
    .await;
    let PaymentRequestPublication::Queued {
        outbound_message_id,
    } = published
    else {
        panic!("expected queued")
    };
    storage.fail_after_commit.store(true, Ordering::SeqCst);
    assert!(
        payment_request_records(&storage, &target, &receiver_path(), timestamp())
            .await
            .is_err()
    );
    let saved = storage.inner.snapshot().unwrap();
    let retried = publish_payment_request(
        &storage,
        target.clone(),
        receiver_path(),
        &event,
        Ok(()),
        timestamp(),
    )
    .await;
    assert!(
        matches!(retried, PaymentRequestPublication::Queued { outbound_message_id: id } if id == outbound_message_id)
    );
    assert_eq!(storage.inner.snapshot().unwrap(), saved);
    let records = payment_request_records(&storage, &target, &receiver_path(), timestamp())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].proposal_outbound_message_id,
        Some(outbound_message_id)
    );
}
