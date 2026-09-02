use super::*;
use crate::domain::payment_requests::payment_request_record_blocks_app_removal;
use crate::runtime::app_removal::{
    app_removal_blockers, detach_shared_app_reservations, retire_app_outbound_private_messages,
};
use crate::storage::PaymentEndpointReservationRecord;

struct CountingPublicPaymentAdapter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl PaymentAdapter for CountingPublicPaymentAdapter {
    async fn current_public_receiving_details(&self) -> Result<Vec<PublicReceivingDetail>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }
}

#[test]
fn test_app_removal_blocks_owned_payer_and_payee_subscriptions() {
    let app_id = app_id();
    let mut payee = payment_request_removal_record(
        PaymentRequestLocalRole::Payee,
        PaymentRequestLifecycleState::ActiveRecurring,
    );
    payee.proposal_app_id = Some(app_id.clone());
    let mut payer = payment_request_removal_record(
        PaymentRequestLocalRole::Payer,
        PaymentRequestLifecycleState::ActiveRecurring,
    );
    payer.payer_app_id = Some(app_id.clone());
    payer.execution_claim_app_id = Some(app_id.clone());

    assert!(payment_request_record_blocks_app_removal(&payee, &app_id));
    assert!(payment_request_record_blocks_app_removal(&payer, &app_id));

    payer.state = PaymentRequestLifecycleState::ProofSubmitted;
    assert!(!payment_request_record_blocks_app_removal(&payer, &app_id));

    payer.state = PaymentRequestLifecycleState::Canceled;
    assert!(!payment_request_record_blocks_app_removal(&payer, &app_id));
}

#[test]
fn test_app_removal_does_not_claim_unanswered_identity_request() {
    let app_id = app_id();
    let mut request = payment_request_removal_record(
        PaymentRequestLocalRole::Payer,
        PaymentRequestLifecycleState::Proposed,
    );
    request.terms = Some(PaymentRequestTermsRecord {
        amount: crate::AmountRecord {
            value: "0.001".into(),
            asset: "btc".into(),
        },
        payment_reference: "invoice-2026-0001".into(),
        proposal_expires_at: None,
        recurrence: None,
        accepted_payment_endpoint_identifiers: vec!["btc-lightning-bolt11".into()],
        required_app_id: Some(app_id.clone()),
        metadata: serde_json::Map::new(),
    });

    assert!(!payment_request_record_blocks_app_removal(
        &request, &app_id
    ));
}

#[tokio::test]
async fn test_app_removal_blocks_undelivered_events_and_receipts() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let app_id = app_id();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let app_id = app_id.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    app_id.clone(),
                    PrivateMessageKind::PrivatePaymentList.as_str().into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    app_id.clone(),
                    PrivateMessageKind::PaymentProof.as_str().into(),
                    r#"{"version":1,"kind":"paykit.payment_proof","app_id":"bitkit"}"#.into(),
                    FixedClock.now(),
                ))?;
                tx.save_receipt_issuance_record(ReceiptIssuanceRecord {
                    counterparty,
                    app_id,
                    receipt_id: "receipt-1".into(),
                    receipt_access_event_id: "event-1".into(),
                    payment_reference: "reference-1".into(),
                    payment_request_id: None,
                    billing_period: None,
                    payment_endpoint_identifier: None,
                    amount: None,
                    location: "/pub/paykit/v0/private/receipts/receipt-1".into(),
                    encrypted_receipt: "encrypted".into(),
                    access_json: "{}".into(),
                    status: ReceiptIssuanceStatus::PendingStorage,
                    outbound_message_id: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    stored_at: None,
                    access_queued_at: None,
                    last_error: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();

    let blockers = app_removal_blockers(&storage, &app_id, FixedClock.now())
        .await
        .unwrap();

    assert_eq!(blockers.active_payment_requests, 0);
    assert_eq!(blockers.undelivered_private_events, 1);
    assert_eq!(blockers.incomplete_receipt_issuances, 1);
    assert_eq!(blockers.shared_private_payment_lists, 0);
}

#[tokio::test]
async fn test_app_removal_requires_shared_private_list_to_be_cleared() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let app_id = app_id();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let app_id = app_id.clone();
            move |tx| {
                let mut shared = tx.insert_outbound_private_message(
                    NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        app_id.clone(),
                        PrivateMessageKind::PrivatePaymentList.as_str().into(),
                        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{"btc-lightning-bolt11":"ln-private"}}"#.into(),
                        FixedClock.now(),
                    ),
                )?;
                shared.status = OutboundPrivateMessageStatus::Sent;
                shared.attempt_count = 1;
                shared.last_attempt_at = Some(FixedClock.now());
                shared.sent_at = Some(FixedClock.now());
                tx.save_outbound_private_message(shared)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let blockers = app_removal_blockers(&storage, &app_id, FixedClock.now())
        .await
        .unwrap();
    assert_eq!(blockers.shared_private_payment_lists, 1);

    storage
        .transaction({
            let counterparty = counterparty.clone();
            let app_id = app_id.clone();
            move |tx| {
                let mut cleared =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty,
                        app_id,
                        PrivateMessageKind::PrivatePaymentList.as_str().into(),
                        private_list_json(),
                        FixedClock.now(),
                    ))?;
                cleared.status = OutboundPrivateMessageStatus::Sent;
                cleared.attempt_count = 1;
                cleared.last_attempt_at = Some(FixedClock.now());
                cleared.sent_at = Some(FixedClock.now());
                tx.save_outbound_private_message(cleared)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let blockers = app_removal_blockers(&storage, &app_id, FixedClock.now())
        .await
        .unwrap();
    assert_eq!(blockers.shared_private_payment_lists, 0);
}

#[tokio::test]
async fn test_receipt_access_delivery_clears_app_removal_blocker() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let app_id = app_id();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let app_id = app_id.clone();
            move |tx| {
                tx.save_receipt_issuance_record(ReceiptIssuanceRecord {
                    counterparty,
                    app_id,
                    receipt_id: "receipt-1".into(),
                    receipt_access_event_id: "event-1".into(),
                    payment_reference: "reference-1".into(),
                    payment_request_id: None,
                    billing_period: None,
                    payment_endpoint_identifier: None,
                    amount: None,
                    location: "/pub/paykit/v0/private/receipts/receipt-1".into(),
                    encrypted_receipt: "encrypted".into(),
                    access_json: "{}".into(),
                    status: ReceiptIssuanceStatus::PendingStorage,
                    outbound_message_id: None,
                    created_at: FixedClock.now(),
                    updated_at: FixedClock.now(),
                    stored_at: None,
                    access_queued_at: None,
                    last_error: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    assert_eq!(
        app_removal_blockers(&storage, &app_id, FixedClock.now())
            .await
            .unwrap()
            .incomplete_receipt_issuances,
        1
    );

    let outbound_id = storage
        .transaction({
            let counterparty = counterparty.clone();
            let app_id = app_id.clone();
            move |tx| {
                let outbound =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        app_id,
                        PrivateMessageKind::ReceiptAccess.as_str().into(),
                        r#"{"version":1,"kind":"paykit.receipt_access","app_id":"bitkit"}"#.into(),
                        FixedClock.now(),
                    ))?;
                let mut issuance = tx
                    .receipt_issuance_record(&counterparty, "receipt-1")
                    .unwrap();
                issuance.status = ReceiptIssuanceStatus::AccessQueued;
                issuance.outbound_message_id = Some(outbound.outbound_message_id);
                issuance.access_queued_at = Some(FixedClock.now());
                tx.save_receipt_issuance_record(issuance);
                Ok(outbound.outbound_message_id)
            }
        })
        .await
        .unwrap();
    assert_eq!(
        app_removal_blockers(&storage, &app_id, FixedClock.now())
            .await
            .unwrap()
            .incomplete_receipt_issuances,
        1
    );

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut outbound = tx
                    .outbound_private_messages(&counterparty)
                    .into_iter()
                    .find(|message| message.outbound_message_id == outbound_id)
                    .expect("queued Receipt Access");
                outbound.status = OutboundPrivateMessageStatus::Sent;
                outbound.attempt_count = 1;
                outbound.last_attempt_at = Some(FixedClock.now());
                outbound.sent_at = Some(FixedClock.now());
                tx.save_outbound_private_message(outbound)
            }
        })
        .await
        .unwrap();
    assert_eq!(
        app_removal_blockers(&storage, &app_id, FixedClock.now())
            .await
            .unwrap()
            .incomplete_receipt_issuances,
        0
    );
}

#[tokio::test]
async fn test_sync_public_endpoints_requires_pubky_session() {
    let storage = InMemoryStorage::new();
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.sync_public_endpoints().await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_sync_public_endpoints_rejects_reentrant_call() {
    let storage = InMemoryStorage::new();
    let pubky = TestPubkySessionProvider { session: None };
    let adapter_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sdk = PaykitSdk::with_clock(
        storage,
        pubky,
        CountingPublicPaymentAdapter(std::sync::Arc::clone(&adapter_calls)),
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let _guard = sdk.claim_identity_operation("test operation").unwrap();

    let result = sdk.sync_public_endpoints().await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        adapter_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a rejected sync must not read an adapter snapshot"
    );
}

#[tokio::test]
async fn test_retire_app_outbound_private_messages_stops_app_queue() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let identity = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let app_id = app_id();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let identity = identity.clone();
            let app_id = app_id.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(identity),
                    initialized_at: FixedClock.now(),
                });
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    app_id.clone(),
                    PrivateMessageKind::PrivatePaymentList.as_str().into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id.clone(),
                    "paykit.payment_request".into(),
                    r#"{"version":1,"kind":"paykit.payment_request","app_id":"bitkit"}"#.into(),
                    FixedClock.now(),
                ))?;
                retire_app_outbound_private_messages(
                    tx,
                    &app_id,
                    FixedClock.now(),
                    FixedClock.now() + chrono::Duration::seconds(60),
                )
            }
        })
        .await
        .unwrap();

    let records = storage
        .transaction(|tx| Ok(tx.outbound_private_messages(&counterparty)))
        .await
        .unwrap();
    assert_eq!(records[0].status, OutboundPrivateMessageStatus::Superseded);
    assert_eq!(records[1].status, OutboundPrivateMessageStatus::Invalid);
    assert!(records[1].last_error.is_some());
    assert!(storage
        .snapshot()
        .unwrap()
        .retired_paykit_apps
        .contains(&app_id));

    let backup = crate::backup::export_backup_state(&storage).await.unwrap();
    let restored = InMemoryStorage::new();
    crate::backup::restore_backup_state(&restored, backup)
        .await
        .expect("retired queue metadata should survive backup validation");
}

#[tokio::test]
async fn test_retire_app_outbound_private_messages_rejects_active_peer_work() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let app_id = app_id();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let app_id = app_id.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    app_id.clone(),
                    PrivateMessageKind::PrivatePaymentList.as_str().into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                tx.claim_peer_link_operation(
                    &counterparty,
                    FixedClock.now(),
                    FixedClock.now() + chrono::Duration::seconds(60),
                )?
                .unwrap();
                Ok(())
            }
        })
        .await
        .unwrap();
    let err = storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                retire_app_outbound_private_messages(
                    tx,
                    &app_id,
                    FixedClock.now(),
                    FixedClock.now() + chrono::Duration::seconds(60),
                )
            }
        })
        .await
        .unwrap_err();

    assert!(matches!(err, PaykitSdkError::Policy { .. }));
    let records = storage
        .transaction(|tx| Ok(tx.outbound_private_messages(&counterparty)))
        .await
        .unwrap();
    assert_eq!(records[0].status, OutboundPrivateMessageStatus::Pending);

    storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                retire_app_outbound_private_messages(
                    tx,
                    &app_id,
                    FixedClock.now() + chrono::Duration::seconds(61),
                    FixedClock.now() + chrono::Duration::seconds(121),
                )
            }
        })
        .await
        .unwrap();
    let records = storage
        .transaction(|tx| Ok(tx.outbound_private_messages(&counterparty)))
        .await
        .unwrap();
    assert_eq!(records[0].status, OutboundPrivateMessageStatus::Superseded);
}

#[tokio::test]
async fn test_retire_app_outbound_private_messages_includes_cleanup_only_reservations() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let app_id = app_id();
    let cleanup_counterparties = storage
        .transaction({
            let counterparty = counterparty.clone();
            let app_id = app_id.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(PaymentEndpointReservationRecord {
                    reservation_id: "reservation-1".into(),
                    counterparty,
                    app_id: app_id.clone(),
                    identifier: "btc-lightning-bolt11".into(),
                    payload_hash: "payload-hash".into(),
                    outbound_message_id: 42,
                    attribution: HashMap::new(),
                    expires_at: None,
                    cancellation_started_at: None,
                    created_at: FixedClock.now(),
                });
                retire_app_outbound_private_messages(
                    tx,
                    &app_id,
                    FixedClock.now(),
                    FixedClock.now() + chrono::Duration::seconds(60),
                )
            }
        })
        .await
        .unwrap();

    assert_eq!(cleanup_counterparties.len(), 1);
    assert_eq!(cleanup_counterparties[0].counterparty, counterparty);
    assert!(storage
        .snapshot()
        .unwrap()
        .retired_paykit_apps
        .contains(&app_id));
}

#[tokio::test]
async fn test_detach_shared_app_reservations_keeps_unshared_cleanup_work() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let app_id = app_id();
    let remaining = storage
        .transaction({
            let counterparty = counterparty.clone();
            let app_id = app_id.clone();
            move |tx| {
                let mut sent =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        app_id.clone(),
                        PrivateMessageKind::PrivatePaymentList.as_str().into(),
                        private_list_json(),
                        FixedClock.now(),
                    ))?;
                sent.status = OutboundPrivateMessageStatus::Sent;
                sent.last_attempt_at = Some(FixedClock.now());
                sent.sent_at = Some(FixedClock.now());
                tx.save_outbound_private_message(sent.clone())?;
                let pending =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        app_id.clone(),
                        PrivateMessageKind::PrivatePaymentList.as_str().into(),
                        private_list_json(),
                        FixedClock.now(),
                    ))?;
                for (reservation_id, outbound_message_id) in [
                    ("shared", sent.outbound_message_id),
                    ("unshared", pending.outbound_message_id),
                ] {
                    tx.save_payment_endpoint_reservation(PaymentEndpointReservationRecord {
                        reservation_id: reservation_id.into(),
                        counterparty: counterparty.clone(),
                        app_id: app_id.clone(),
                        identifier: "btc-lightning-bolt11".into(),
                        payload_hash: "payload-hash".into(),
                        outbound_message_id,
                        attribution: HashMap::new(),
                        expires_at: None,
                        cancellation_started_at: None,
                        created_at: FixedClock.now(),
                    });
                }
                retire_app_outbound_private_messages(
                    tx,
                    &app_id,
                    FixedClock.now(),
                    FixedClock.now() + chrono::Duration::seconds(60),
                )?;
                detach_shared_app_reservations(tx, &app_id)
            }
        })
        .await
        .unwrap();

    assert_eq!(remaining, 1);
    let records = storage
        .transaction(|tx| Ok(tx.payment_endpoint_reservations(&counterparty)))
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reservation_id, "unshared");
}

fn payment_request_removal_record(
    local_role: PaymentRequestLocalRole,
    state: PaymentRequestLifecycleState,
) -> PaymentRequestRecord {
    PaymentRequestRecord {
        counterparty: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
        payment_request_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        local_role: Some(local_role),
        state,
        proposal_stream_item_id: None,
        proposal_outbound_message_id: None,
        proposal_outbound_status: None,
        proposal_event_id: None,
        proposal_app_id: None,
        payer_app_id: None,
        execution_claim_app_id: None,
        terms: None,
        accepted_event_id: None,
        accepted_outbound_status: None,
        rejected_event_id: None,
        rejected_outbound_status: None,
        canceled_event_id: None,
        canceled_outbound_status: None,
        payment_proofs: Vec::new(),
        last_stream_item_id: None,
        last_outbound_message_id: None,
        last_outbound_status: None,
        last_event_at: None,
        invalid_reason: None,
    }
}
