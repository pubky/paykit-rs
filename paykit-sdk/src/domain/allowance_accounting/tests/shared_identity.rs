use super::*;

#[tokio::test]
async fn test_automatic_acceptance_rejects_missing_or_foreign_claim_atomically() {
    for foreign_claim in [false, true] {
        let fixture = Fixture::new().await;
        let other = register_other_app(&fixture).await;
        let mut state = fixture.storage.snapshot().unwrap();
        state
            .outbound_private_messages
            .retain(|message| message.kind != "paykit.payment_request_acceptance");
        if foreign_claim {
            for claim in state.payment_request_execution_claims.values_mut() {
                claim.app_id = other.clone();
            }
        } else {
            state.payment_request_execution_claims.clear();
        }
        let storage = InMemoryStorage::from_state(state.clone());
        let result = storage
            .transaction(|tx| {
                select(
                    tx,
                    &app_id(),
                    fixture.occurrence.request.clone(),
                    AllowanceSelectionInput {
                        allowance_id: fixture.allowance.clone(),
                        expected_revision: Some(1),
                        trusted_time: time(),
                    },
                    Some(checks()),
                )
            })
            .await;
        assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
        assert_eq!(storage.snapshot().unwrap(), state);
    }
}

async fn register_other_app(fixture: &Fixture) -> paykit_lib::PaykitAppId {
    let other = paykit_lib::PaykitAppId::new("server").unwrap();
    fixture
        .storage
        .transaction(|tx| {
            tx.save_paykit_app_capabilities(&other, tx.paykit_app_capabilities(&app_id()).unwrap());
            tx.activate_paykit_app(&other);
            Ok(())
        })
        .await
        .unwrap();
    other
}

#[tokio::test]
async fn test_shared_storage_codec_preserves_nonempty_accounting() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    let state = fixture.storage.snapshot().unwrap();
    let bytes = crate::storage::encode_storage_state_blob(&state).unwrap();
    assert_eq!(
        crate::storage::decode_storage_state_blob(&bytes).unwrap(),
        state
    );

    let mut foreign = state.clone();
    foreign.identity_state.as_mut().unwrap().public_key = Some(public_key());
    let bytes = crate::storage::encode_storage_state_blob(&foreign).unwrap();
    assert!(crate::storage::decode_storage_state_blob(&bytes).is_err());

    let mut corrupt = state;
    corrupt
        .allowance_accounting
        .as_mut()
        .unwrap()
        .history
        .watermarks
        .clear();
    let bytes = crate::storage::encode_storage_state_blob(&corrupt).unwrap();
    assert!(crate::storage::decode_storage_state_blob(&bytes).is_err());
}

#[tokio::test]
async fn test_cross_app_claim_handoff_cannot_repeat_successful_payment() {
    let fixture = Fixture::new().await;
    let other = register_other_app(&fixture).await;
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| {
            begin(tx, &app_id(), prepared.attempt_id.clone(), checks())?;
            report_outcome(
                tx,
                PaymentOutcomeReport {
                    attempt_id: prepared.attempt_id,
                    outcome: PaymentOutcome::Succeeded,
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    crate::domain::payment_requests::release_payment_request_execution_claim(
        &fixture.storage,
        fixture.occurrence.request.counterparty.clone(),
        &app_id(),
        &fixture.occurrence.request.payment_request_id,
        time(),
    )
    .await
    .unwrap();
    crate::domain::payment_requests::claim_payment_request_execution(
        &fixture.storage,
        fixture.occurrence.request.counterparty.clone(),
        &other,
        &fixture.occurrence.request.payment_request_id,
        time(),
    )
    .await
    .unwrap();
    for mode in [
        PaymentExecutionMode::Manual,
        PaymentExecutionMode::Automatic,
    ] {
        let decision = fixture
            .storage
            .transaction(|tx| {
                reserve(
                    tx,
                    &other,
                    fixture.occurrence.clone(),
                    Some(1),
                    checks(),
                    mode,
                )
            })
            .await
            .unwrap();
        assert!(matches!(
            decision,
            PaymentAttemptDecision::Blocked {
                reason: AllowanceAccountingBlock::PaymentAlreadyRecorded
            }
        ));
    }
    assert_eq!(fixture.state().history.occurrences.len(), 1);
    assert_eq!(fixture.state().history.occurrences[0].attempts.len(), 1);
}

#[tokio::test]
async fn test_reservation_and_handoff_require_current_app_authority() {
    let fixture = Fixture::new().await;
    let other = register_other_app(&fixture).await;
    let decision = fixture
        .storage
        .transaction(|tx| {
            reserve(
                tx,
                &other,
                fixture.occurrence.clone(),
                Some(1),
                checks(),
                PaymentExecutionMode::Automatic,
            )
        })
        .await
        .unwrap();
    assert!(matches!(
        decision,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::InvalidLifecycle
        }
    ));
    let prepared = attempt(fixture.reserve().await);
    let decision = fixture
        .storage
        .transaction(|tx| begin(tx, &other, prepared.attempt_id.clone(), checks()))
        .await
        .unwrap();
    assert!(matches!(
        decision,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::InvalidLifecycle
        }
    ));
    fixture
        .storage
        .transaction(|tx| {
            let mut capabilities = tx.paykit_app_capabilities(&app_id()).unwrap();
            capabilities.outgoing_payments = false;
            tx.save_paykit_app_capabilities(&app_id(), capabilities);
            Ok(())
        })
        .await
        .unwrap();
    let decision = fixture
        .storage
        .transaction(|tx| begin(tx, &app_id(), prepared.attempt_id.clone(), checks()))
        .await
        .unwrap();
    assert!(matches!(
        decision,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::InvalidLifecycle
        }
    ));
    fixture
        .storage
        .transaction(|tx| {
            report_outcome(
                tx,
                PaymentOutcomeReport {
                    attempt_id: prepared.attempt_id,
                    outcome: PaymentOutcome::Failed,
                },
            )
        })
        .await
        .unwrap();
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::InvalidLifecycle
        }
    ));
}

#[tokio::test]
async fn test_unresolved_attempt_prevents_claim_release() {
    for status in [
        PaymentExecutionStatus::Prepared,
        PaymentExecutionStatus::Submitted,
        PaymentExecutionStatus::Unknown,
    ] {
        let fixture = Fixture::new().await;
        let prepared = attempt(fixture.reserve().await);
        fixture
            .storage
            .transaction(|tx| {
                if status != PaymentExecutionStatus::Prepared {
                    begin(tx, &app_id(), prepared.attempt_id.clone(), checks())?;
                }
                if status == PaymentExecutionStatus::Unknown {
                    report_outcome(
                        tx,
                        PaymentOutcomeReport {
                            attempt_id: prepared.attempt_id.clone(),
                            outcome: PaymentOutcome::Unknown,
                        },
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let before = fixture.storage.snapshot().unwrap();
        assert!(
            crate::domain::payment_requests::release_payment_request_execution_claim(
                &fixture.storage,
                fixture.occurrence.request.counterparty.clone(),
                &app_id(),
                &fixture.occurrence.request.payment_request_id,
                time(),
            )
            .await
            .is_err()
        );
        assert_eq!(fixture.storage.snapshot().unwrap(), before);
    }
}

#[tokio::test]
async fn test_inbound_cancellation_retains_claim_until_wallet_reconciles() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| begin(tx, &app_id(), prepared.attempt_id.clone(), checks()))
        .await
        .unwrap();
    let event =
        paykit_lib::PaymentRequestEvent::Cancellation(paykit_lib::PaymentRequestCancellation::new(
            paykit_lib::EventId::new_v4(),
            fixture.occurrence.request.payment_request_id.clone(),
            None,
        ));
    let raw = paykit_lib::serialize_payment_request_event(&app_id(), &event).unwrap();
    crate::domain::private_stream::persist_private_stream_batch(
        &fixture.storage,
        fixture.occurrence.request.counterparty.clone(),
        vec![message(raw)],
        None,
        time(),
    )
    .await
    .unwrap();
    let state = fixture.storage.snapshot().unwrap();
    assert_eq!(state.payment_request_execution_claims.len(), 1);
    crate::validate_storage_state(&state).unwrap();
    fixture
        .storage
        .transaction(|tx| {
            report_outcome(
                tx,
                PaymentOutcomeReport {
                    attempt_id: prepared.attempt_id,
                    outcome: PaymentOutcome::Failed,
                },
            )
        })
        .await
        .unwrap();
    assert!(fixture
        .storage
        .snapshot()
        .unwrap()
        .payment_request_execution_claims
        .is_empty());
}

#[tokio::test]
async fn test_conflicting_proposal_preserves_unresolved_claim_through_restore() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    let before = fixture.storage.snapshot().unwrap();
    let proposal = before
        .private_stream_items
        .iter()
        .find(|item| item.raw_json.contains("\"paykit.payment_request\""))
        .unwrap();
    let mut conflict: serde_json::Value = serde_json::from_str(&proposal.raw_json).unwrap();
    conflict["request"]["amount"]["value"] = "2".into();
    crate::domain::private_stream::persist_private_stream_batch(
        &fixture.storage,
        fixture.occurrence.request.counterparty.clone(),
        vec![message(conflict.to_string())],
        None,
        time(),
    )
    .await
    .unwrap();

    let state = fixture.storage.snapshot().unwrap();
    assert_eq!(
        state.payment_request_execution_claims,
        before.payment_request_execution_claims
    );
    assert_eq!(state.allowance_accounting, before.allowance_accounting);
    crate::validate_storage_state(&state).unwrap();
    let bytes = crate::storage::encode_storage_state_blob(&state).unwrap();
    assert_eq!(
        crate::storage::decode_storage_state_blob(&bytes).unwrap(),
        state
    );

    let restored = InMemoryStorage::new();
    crate::backup::restore_backup_state(
        &restored,
        crate::export_backup_state(&fixture.storage).await.unwrap(),
    )
    .await
    .unwrap();
    let restored_state = restored.snapshot().unwrap();
    crate::validate_storage_state(&restored_state).unwrap();
    assert_eq!(restored_state.identity_state, state.identity_state);
    assert_eq!(
        restored_state.payment_request_execution_claims,
        state.payment_request_execution_claims
    );
    let accounting = restored_state.allowance_accounting.as_ref().unwrap();
    assert!(accounting.requires_reconciliation);
    let mut expected_history = state.allowance_accounting.as_ref().unwrap().history.clone();
    expected_history.occurrences[0].attempts[0].status = PaymentExecutionStatus::Unknown;
    assert_eq!(accounting.history, expected_history);

    for (storage, reason) in [
        (&fixture.storage, AllowanceAccountingBlock::InvalidLifecycle),
        (&restored, AllowanceAccountingBlock::ReconciliationRequired),
    ] {
        let record = storage
            .transaction(|tx| request(tx, &scope(tx, &fixture.occurrence.request)?, time()))
            .await
            .unwrap();
        assert_eq!(
            record.state,
            crate::PaymentRequestLifecycleState::InvalidConflict
        );
        assert_eq!(record.local_role, None);
        assert_eq!(
            storage
                .transaction(|tx| begin(tx, &app_id(), prepared.attempt_id.clone(), checks()))
                .await
                .unwrap(),
            PaymentAttemptDecision::Blocked { reason }
        );
    }

    let mut missing_accounting = state.clone();
    missing_accounting.allowance_accounting = None;
    assert!(crate::validate_storage_state(&missing_accounting).is_err());

    let mut resolved = state.clone();
    resolved
        .allowance_accounting
        .as_mut()
        .unwrap()
        .history
        .occurrences[0]
        .attempts[0]
        .status = PaymentExecutionStatus::Failed;
    validate_accounting(resolved.allowance_accounting.as_ref().unwrap()).unwrap();
    assert!(crate::validate_storage_state(&resolved).is_err());

    let mut foreign_payer = state;
    foreign_payer.identity_state.as_mut().unwrap().public_key = Some(public_key());
    assert!(crate::validate_storage_state(&foreign_payer).is_err());
}

#[tokio::test]
async fn test_requeue_canceled_request_releases_only_resolved_accounting_claims() {
    use crate::OutboundPrivateMessageStatus::{Failed, Pending};

    for (delivery, restored) in [(Pending, false), (Failed, false), (Pending, true)] {
        assert_requeue_canceled_request_claims(delivery, restored).await;
    }
}

async fn assert_requeue_canceled_request_claims(
    delivery: crate::OutboundPrivateMessageStatus,
    restored: bool,
) {
    use crate::OutboundPrivateMessageStatus::{Pending, Sent};

    for outcome in [PaymentOutcome::Failed, PaymentOutcome::Unknown] {
        let resolved = outcome == PaymentOutcome::Failed;
        let mut fixture = Fixture::new().await;
        let prepared = attempt(fixture.reserve().await);
        let submitted = fixture
            .storage
            .transaction(|tx| begin(tx, &app_id(), prepared.attempt_id.clone(), checks()))
            .await
            .unwrap();
        assert_eq!(attempt(submitted).status, PaymentExecutionStatus::Submitted);
        let peer = fixture.occurrence.request.counterparty.clone();
        let cancellation = paykit_lib::PaymentRequestEvent::Cancellation(
            paykit_lib::PaymentRequestCancellation::new(
                paykit_lib::EventId::new_v4(),
                fixture.occurrence.request.payment_request_id.clone(),
                None,
            ),
        );
        crate::domain::payment_requests::enqueue_checked_payment_request_action(
            &fixture.storage,
            peer.clone(),
            &app_id(),
            &cancellation,
            time(),
        )
        .await
        .unwrap();
        fixture
            .storage
            .transaction(|tx| {
                for mut message in tx.outbound_private_messages(&peer) {
                    if delivery != Pending {
                        message.attempt_count = 1;
                        message.last_attempt_at = Some(time());
                        message = if delivery == Sent {
                            crate::domain::outbound_private::mark_outbound_sent(message, time())
                        } else {
                            crate::domain::outbound_private::mark_outbound_failed(
                                message,
                                "publication failed".into(),
                                time(),
                            )
                        };
                    }
                    tx.save_outbound_private_message(message)?;
                }
                assert_eq!(
                    request(tx, &scope(tx, &fixture.occurrence.request)?, time())?.state,
                    crate::PaymentRequestLifecycleState::Canceled
                );
                crate::domain::linked_peers::mark_recovery_required_in_transaction(
                    tx,
                    &peer,
                    time(),
                )?;
                crate::validate_storage_state(&tx.export_storage_state())
            })
            .await
            .unwrap();

        if restored {
            let capabilities = fixture
                .storage
                .transaction(|tx| Ok(tx.paykit_app_capabilities(&app_id()).unwrap()))
                .await
                .unwrap();
            let backup = crate::export_backup_state(&fixture.storage).await.unwrap();
            fixture.storage = InMemoryStorage::new();
            crate::backup::restore_backup_state(&fixture.storage, backup)
                .await
                .unwrap();
            assert!(fixture.state().requires_reconciliation);
            fixture
                .storage
                .transaction(|tx| {
                    tx.save_paykit_app_capabilities(&app_id(), capabilities);
                    tx.activate_paykit_app(&app_id());
                    Ok(())
                })
                .await
                .unwrap();
        }
        let report = PaymentOutcomeReport {
            attempt_id: prepared.attempt_id,
            outcome,
        };
        fixture
            .storage
            .transaction(|tx| {
                if restored {
                    let revision = load(tx)?.revision;
                    reconcile(
                        tx,
                        AllowanceAccountingReconciliation {
                            expected_revision: Some(revision),
                            history: Default::default(),
                            outcomes: vec![report],
                            trusted_time: time(),
                        },
                    )?;
                } else {
                    report_outcome(tx, report)?;
                }
                assert_eq!(
                    request(tx, &scope(tx, &fixture.occurrence.request)?, time())?.state,
                    crate::PaymentRequestLifecycleState::RecoveryRequired
                );
                crate::validate_storage_state(&tx.export_storage_state())
            })
            .await
            .unwrap();
        let before = fixture.storage.snapshot().unwrap();
        assert_eq!(before.payment_request_execution_claims.len(), 1);

        fixture
            .storage
            .transaction(|tx| {
                crate::domain::linked_peers::requeue_recovery_required_outbound_messages(
                    tx,
                    &peer,
                    time(),
                )?;
                assert_eq!(
                    request(tx, &scope(tx, &fixture.occurrence.request)?, time())?.state,
                    crate::PaymentRequestLifecycleState::Canceled
                );
                crate::validate_storage_state(&tx.export_storage_state())
            })
            .await
            .unwrap();
        let after = fixture.storage.snapshot().unwrap();
        assert_eq!(after.allowance_accounting, before.allowance_accounting);
        assert_eq!(after.private_stream_items, before.private_stream_items);
        if resolved {
            assert!(after.payment_request_execution_claims.is_empty());
        } else {
            assert_eq!(
                after.payment_request_execution_claims,
                before.payment_request_execution_claims
            );
        }
    }
}
