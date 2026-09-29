use super::*;

async fn defer(fixture: &Fixture) -> AllowanceAccountingState {
    fixture
        .storage
        .transaction(|tx| {
            set_disposition(
                tx,
                fixture.occurrence.clone(),
                PaymentDisposition::Deferred {
                    reason: "endpoint temporarily unavailable".into(),
                },
            )
        })
        .await
        .unwrap();
    fixture.state()
}

async fn execute_after_deferral(
    mode: PaymentExecutionMode,
    status: PaymentExecutionStatus,
) -> (Fixture, AllowanceAccountingState, AllowanceAccountingState) {
    let fixture = Fixture::new().await;
    let deferred = defer(&fixture).await;
    let prepared = attempt(
        fixture
            .storage
            .transaction(|tx| {
                reserve(
                    tx,
                    &app_id(),
                    fixture.occurrence.clone(),
                    Some(1),
                    checks(),
                    mode,
                )
            })
            .await
            .unwrap(),
    );
    if status != PaymentExecutionStatus::Prepared {
        fixture
            .storage
            .transaction(|tx| begin(tx, &app_id(), prepared.attempt_id.clone(), checks()))
            .await
            .map(attempt)
            .unwrap();
    }
    let outcome = match status {
        PaymentExecutionStatus::Unknown => Some(PaymentOutcome::Unknown),
        PaymentExecutionStatus::Succeeded => Some(PaymentOutcome::Succeeded),
        PaymentExecutionStatus::Failed => Some(PaymentOutcome::Failed),
        _ => None,
    };
    if let Some(outcome) = outcome {
        fixture
            .storage
            .transaction(|tx| {
                report_outcome(
                    tx,
                    PaymentOutcomeReport {
                        attempt_id: prepared.attempt_id,
                        outcome,
                    },
                )
            })
            .await
            .unwrap();
    }
    let executed = fixture.state();
    (fixture, deferred, executed)
}

async fn restore_and_reconcile(
    fixture: &Fixture,
    destination: AllowanceAccountingState,
    recovered: AllowanceAccountingState,
) -> [AllowanceAccountingState; 2] {
    let restored = merge_restored_accounting(Some(destination.clone()), Some(recovered.clone()))
        .unwrap()
        .unwrap();
    assert!(restored.requires_reconciliation);
    assert_ne!(restored.epoch, destination.epoch);
    let reconciled = fixture
        .storage
        .transaction(|tx| {
            tx.save_allowance_accounting_state(destination.clone());
            reconcile(
                tx,
                AllowanceAccountingReconciliation {
                    expected_revision: Some(destination.revision),
                    history: recovered.history,
                    outcomes: vec![],
                    trusted_time: time(),
                },
            )
        })
        .await
        .unwrap();
    assert!(!reconciled.requires_reconciliation);
    assert_eq!(fixture.state(), reconciled);
    [restored, reconciled]
}

#[tokio::test]
async fn test_accounting_recovery_execution_supersedes_deferred_in_both_directions() {
    for status in [
        PaymentExecutionStatus::Prepared,
        PaymentExecutionStatus::Submitted,
        PaymentExecutionStatus::Unknown,
        PaymentExecutionStatus::Succeeded,
    ] {
        let (fixture, deferred, executed) =
            execute_after_deferral(PaymentExecutionMode::Automatic, status.clone()).await;
        let mut expected_attempt = executed.history.occurrences[0].attempts[0].clone();
        if status == PaymentExecutionStatus::Prepared {
            expected_attempt.status = PaymentExecutionStatus::Unknown;
        }
        for (destination, recovered) in [
            (deferred.clone(), executed.clone()),
            (executed.clone(), deferred.clone()),
        ] {
            for state in restore_and_reconcile(&fixture, destination, recovered).await {
                let occurrence = &state.history.occurrences[0];
                assert_eq!(occurrence.disposition, PaymentDisposition::Automatic);
                assert_eq!(occurrence.attempts, vec![expected_attempt.clone()]);
                validate_accounting(&state).unwrap();
            }
            assert!(matches!(
                fixture.reserve().await,
                PaymentAttemptDecision::Blocked {
                    reason: AllowanceAccountingBlock::PaymentAlreadyRecorded
                }
            ));
        }
    }
}

#[tokio::test]
async fn test_accounting_recovery_preserves_manual_only_in_both_directions() {
    for mode in [
        PaymentExecutionMode::Automatic,
        PaymentExecutionMode::Manual,
    ] {
        for status in [
            PaymentExecutionStatus::Prepared,
            PaymentExecutionStatus::Failed,
        ] {
            let (fixture, mut deferred, executed) =
                execute_after_deferral(mode.clone(), status.clone()).await;
            if mode == PaymentExecutionMode::Automatic {
                deferred.history.occurrences[0].disposition = PaymentDisposition::ManualOnly;
            }
            for (destination, recovered) in [
                (deferred.clone(), executed.clone()),
                (executed.clone(), deferred.clone()),
            ] {
                for state in restore_and_reconcile(&fixture, destination, recovered).await {
                    assert_eq!(
                        state.history.occurrences[0].disposition,
                        PaymentDisposition::ManualOnly
                    );
                    assert_eq!(state.history.occurrences[0].attempts.len(), 1);
                    validate_accounting(&state).unwrap();
                }
                if status == PaymentExecutionStatus::Failed {
                    assert!(matches!(
                        fixture.reserve().await,
                        PaymentAttemptDecision::Blocked {
                            reason: AllowanceAccountingBlock::ManualOnly
                        }
                    ));
                }
            }
        }
    }
}

#[tokio::test]
async fn test_accounting_recovery_preserves_deferral_with_failed_only_evidence() {
    let (fixture, before_execution, failed) = execute_after_deferral(
        PaymentExecutionMode::Automatic,
        PaymentExecutionStatus::Failed,
    )
    .await;
    let after_failure = defer(&fixture).await;
    for destination in [before_execution, after_failure] {
        for state in restore_and_reconcile(&fixture, destination.clone(), failed.clone()).await {
            assert_eq!(
                state.history.occurrences[0].disposition,
                destination.history.occurrences[0].disposition
            );
            assert_eq!(
                state.history.occurrences[0].attempts,
                failed.history.occurrences[0].attempts
            );
            validate_accounting(&state).unwrap();
        }
    }
}

#[tokio::test]
async fn test_accounting_backup_restore_rejects_populated_destination() {
    let (fixture, deferred, executed) = execute_after_deferral(
        PaymentExecutionMode::Automatic,
        PaymentExecutionStatus::Prepared,
    )
    .await;
    let backup = crate::export_backup_state(&fixture.storage).await.unwrap();
    fixture
        .storage
        .transaction(|tx| {
            tx.save_allowance_accounting_state(deferred);
            Ok(())
        })
        .await
        .unwrap();
    let before = fixture.storage.snapshot().unwrap();
    assert!(crate::backup::restore_backup_state_with_identity(
        &fixture.storage,
        backup.clone(),
        None,
        time()
    )
    .await
    .is_err());
    assert_eq!(fixture.storage.snapshot().unwrap(), before);
    let restored_storage = InMemoryStorage::new();
    crate::backup::restore_backup_state_with_identity(&restored_storage, backup, None, time())
        .await
        .unwrap();
    let restored = restored_storage
        .snapshot()
        .unwrap()
        .allowance_accounting
        .unwrap();
    assert!(restored.requires_reconciliation);
    assert_eq!(
        restored.history.occurrences[0].disposition,
        PaymentDisposition::Automatic
    );
    let mut expected_attempt = executed.history.occurrences[0].attempts[0].clone();
    expected_attempt.status = PaymentExecutionStatus::Unknown;
    assert_eq!(
        restored.history.occurrences[0].attempts,
        vec![expected_attempt]
    );
    assert!(matches!(
        restored_storage
            .transaction(|tx| reserve(
                tx,
                &app_id(),
                fixture.occurrence.clone(),
                Some(1),
                checks(),
                PaymentExecutionMode::Automatic
            ))
            .await
            .unwrap(),
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::ReconciliationRequired
        }
    ));
}

#[tokio::test]
async fn test_accounting_recovery_rejects_already_malformed_deferred_evidence() {
    let (fixture, deferred, mut malformed) = execute_after_deferral(
        PaymentExecutionMode::Automatic,
        PaymentExecutionStatus::Submitted,
    )
    .await;
    malformed.history.occurrences[0].disposition =
        deferred.history.occurrences[0].disposition.clone();
    assert!(merge_restored_accounting(Some(deferred.clone()), Some(malformed.clone())).is_err());
    assert!(merge_restored_accounting(Some(malformed.clone()), Some(deferred)).is_err());
    let before = fixture.state();
    assert!(fixture
        .storage
        .transaction(|tx| {
            reconcile(
                tx,
                AllowanceAccountingReconciliation {
                    expected_revision: Some(before.revision),
                    history: malformed.history,
                    outcomes: vec![],
                    trusted_time: time(),
                },
            )
        })
        .await
        .is_err());
    assert_eq!(fixture.state(), before);
}
