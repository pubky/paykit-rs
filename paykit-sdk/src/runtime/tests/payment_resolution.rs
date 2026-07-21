use super::*;
use crate::runtime::payment_resolution::{merge_outbound_report, merge_receive_report};

#[test]
fn test_payable_from_batch_rejects_foreign_candidates() {
    let candidate = private_endpoint_candidate("ln-private");
    let foreign = private_endpoint_candidate("ln-foreign");

    let result = private_payable_from_batch(&[foreign], &[candidate]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_payable_from_batch_rejects_duplicate_candidates() {
    let candidate = private_endpoint_candidate("ln-private");

    let result = private_payable_from_batch(&[candidate.clone(), candidate.clone()], &[candidate]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_private_payable_from_batch_preserves_adapter_order() {
    let first = private_endpoint_candidate("ln-first");
    let mut second = first.clone();
    second.payload = "ln-second".into();
    let candidates = vec![first.clone(), second.clone()];

    let result = private_payable_from_batch(&[second.clone(), first.clone()], &candidates).unwrap();

    assert_eq!(result, vec![second, first]);
}

#[test]
fn test_public_payable_from_batch_preserves_adapter_order() {
    let first = public_endpoint_candidate("ln-first");
    let mut second = first.clone();
    second.payload = "ln-second".into();
    let candidates = vec![first.clone(), second.clone()];

    let result = public_payable_from_batch(&[second.clone(), first.clone()], &candidates).unwrap();

    assert_eq!(result, vec![second, first]);
}

#[test]
fn test_merge_outbound_report_preserves_multiple_rounds() {
    let mut report = Some(OutboundPrivateSendReport {
        attempted: vec![1],
        sent: vec![1],
        failed: Vec::new(),
        reservation_cleanup_failures: Vec::new(),
        recovery_marker_failures: Vec::new(),
    });
    merge_outbound_report(
        &mut report,
        OutboundPrivateSendReport {
            attempted: vec![2],
            sent: Vec::new(),
            failed: vec![OutboundPrivateSendFailure {
                outbound_message_id: 2,
                error: "transport failed".into(),
            }],
            reservation_cleanup_failures: Vec::new(),
            recovery_marker_failures: Vec::new(),
        },
    );

    let report = report.unwrap();
    assert_eq!(report.attempted, vec![1, 2]);
    assert_eq!(report.sent, vec![1]);
    assert_eq!(report.failed[0].outbound_message_id, 2);
}

#[test]
fn test_merge_receive_report_preserves_multiple_rounds() {
    let mut report = Some(PrivateStreamIntakeReport {
        receive_batch_id: 1,
        stream_item_ids: vec![10],
        event_conflicts: Vec::new(),
    });
    merge_receive_report(
        &mut report,
        PrivateStreamIntakeReport {
            receive_batch_id: 2,
            stream_item_ids: vec![11],
            event_conflicts: vec![EventIdConflict {
                event_id: "event-1".into(),
                first_stream_item_id: 10,
                conflicting_stream_item_id: 11,
            }],
        },
    );

    let report = report.unwrap();
    assert_eq!(report.receive_batch_id, 1);
    assert_eq!(report.stream_item_ids, vec![10, 11]);
    assert_eq!(report.event_conflicts[0].conflicting_stream_item_id, 11);
}

#[tokio::test]
async fn test_resolve_private_candidate_batch_preserves_private_state() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let endpoint = private_endpoint_candidate("ln-private");
    let result = sdk
        .resolve_private_candidate_batch(
            endpoint.counterparty.clone(),
            receiver_path(),
            None,
            vec![endpoint],
            PrivatePaymentResolutionState::RecoveryPending,
            7,
        )
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.state, PrivatePaymentResolutionState::RecoveryPending);
    assert_eq!(result.private_payment_list_version, Some(7));
    assert_eq!(result.payable_endpoints.len(), 1);
}

#[tokio::test]
async fn test_resolve_public_candidate_batch_returns_ordered_payable_endpoints() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let first = public_endpoint_candidate("ln-first");
    let mut second = first.clone();
    second.payload = "ln-second".into();

    let result = sdk
        .resolve_public_candidate_batch(
            first.counterparty.clone(),
            receiver_path(),
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            vec![first.clone(), second.clone()],
        )
        .await
        .unwrap();

    assert_eq!(result.status, PublicPaymentResolutionStatus::Payable);
    assert_eq!(result.payable_endpoints.len(), 2);
    assert_eq!(result.payable_endpoints[0].endpoint, first);
    assert_eq!(result.payable_endpoints[0].target.payload, "ln-first");
    assert_eq!(result.payable_endpoints[1].endpoint, second);
    assert_eq!(result.payable_endpoints[1].target.payload, "ln-second");
}

#[tokio::test]
async fn test_resolve_private_contact_payment_hides_cached_list_without_identity() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(
            counterparty.clone(),
            receiver_path(),
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            None,
        )
        .await;

    let result = result.unwrap();
    assert_eq!(result.status, PrivatePaymentResolutionStatus::NoEndpoint);
    assert_eq!(
        result.state,
        PrivatePaymentResolutionState::NoPrivateEndpoint
    );
    assert_eq!(result.private_payment_list_version, None);
    assert!(result.payable_endpoints.is_empty());
    assert!(sdk
        .current_private_payment_list(&counterparty, &receiver_path())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_resolve_private_contact_payment_uses_cached_list_without_live_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                local_receiver_noise_public_key: Some(receiver_noise_public_key()),
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            });
            Ok(())
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(
            counterparty,
            receiver_path(),
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.state, PrivatePaymentResolutionState::Available);
    assert_eq!(result.private_payment_list_version, Some(0));
    assert_eq!(result.payable_endpoints[0].endpoint.payload, "ln-private");
}

#[tokio::test]
async fn test_resolve_private_contact_payment_waits_after_current_list_version() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(counterparty, receiver_path(), None, Some(0))
        .await
        .unwrap();

    assert_eq!(
        result.status,
        PrivatePaymentResolutionStatus::WaitingForUpdatedPaymentList
    );
    assert_eq!(result.state, PrivatePaymentResolutionState::Available);
    assert_eq!(result.private_payment_list_version, Some(0));
    assert!(result.payable_endpoints.is_empty());
}

#[tokio::test]
async fn test_resolve_private_contact_payment_accepts_newer_repeated_endpoint() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![
            private_list_message("ln-reusable"),
            private_list_message("ln-reusable"),
        ],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(counterparty, receiver_path(), None, Some(0))
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.private_payment_list_version, Some(1));
    assert_eq!(result.payable_endpoints[0].endpoint.payload, "ln-reusable");
}

#[tokio::test]
async fn test_resolve_private_contact_payment_uses_private_candidates_only() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                local_receiver_noise_public_key: Some(receiver_noise_public_key()),
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            });
            Ok(())
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(counterparty, receiver_path(), None, None)
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.private_payment_list_version, Some(0));
    assert_eq!(result.payable_endpoints[0].endpoint.payload, "ln-private");
}

#[tokio::test]
async fn test_resolve_private_contact_payment_does_not_use_cached_list_while_linking() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                        &pubky::Keypair::random().public_key(),
                    )),
                    local_receiver_noise_public_key: Some(receiver_noise_public_key()),
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    counterparty_receiver_path: receiver_path(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(
            counterparty,
            receiver_path(),
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::NoEndpoint);
    assert_eq!(result.state, PrivatePaymentResolutionState::RecoveryPending);
    assert_eq!(result.private_payment_list_version, None);
    assert!(result.payable_endpoints.is_empty());
}

#[tokio::test]
async fn test_recover_private_candidates_reports_pending_for_linking_peer() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                        &pubky::Keypair::random().public_key(),
                    )),
                    local_receiver_noise_public_key: Some(receiver_noise_public_key()),
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    counterparty_receiver_path: receiver_path(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let outcome = sdk
        .recover_private_candidates_for_resolution(&counterparty, &receiver_path())
        .await
        .unwrap();

    assert!(matches!(outcome, PrivateRecoveryOutcome::Pending));
}
