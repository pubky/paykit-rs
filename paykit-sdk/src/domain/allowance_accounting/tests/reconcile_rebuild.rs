//! Reconciliation that rebuilds or corrects payments against the retained ledger.
use super::*;

fn assert_error<T: std::fmt::Debug>(result: Result<T>, expected: &str) {
    match result {
        Err(PaykitSdkError::Policy { context, .. } | PaykitSdkError::Protocol { context, .. }) => {
            assert_eq!(context, expected)
        }
        other => panic!("Expected {expected:?}: {other:?}"),
    }
}

async fn settle(
    fixture: &Fixture,
    attempt_id: String,
    outcome: PaymentOutcome,
) -> Result<PaymentAttemptRecord> {
    fixture
        .storage
        .transaction(|tx| {
            report_outcome(
                tx,
                PaymentOutcomeReport {
                    attempt_id,
                    outcome,
                },
            )
        })
        .await
}

/// Reserves, hands off, and settles one automatic payment, returning its attempt ID.
async fn pay(fixture: &Fixture, outcome: PaymentOutcome) -> String {
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| begin(tx, &app_id(), prepared.attempt_id.clone(), checks()))
        .await
        .map(attempt)
        .unwrap();
    settle(fixture, prepared.attempt_id.clone(), outcome)
        .await
        .unwrap();
    prepared.attempt_id
}

/// Pays once, then restores the ledger snapshot taken before the payment.
async fn stale_restore() -> Fixture {
    let fixture = Fixture::new().await;
    let stale = fixture.state();
    pay(&fixture, PaymentOutcome::Succeeded).await;
    let restored = merge_restored_accounting(None, Some(stale))
        .unwrap()
        .unwrap();
    fixture
        .storage
        .transaction(|tx| {
            tx.save_allowance_accounting_state(restored);
            Ok(())
        })
        .await
        .unwrap();
    fixture
}

async fn reconcile_history(
    fixture: &Fixture,
    history: AllowanceAccountingHistory,
    trusted_time: DateTime<Utc>,
) -> Result<AllowanceAccountingState> {
    let expected_revision = Some(fixture.state().revision);
    fixture
        .storage
        .transaction(|tx| {
            reconcile(
                tx,
                AllowanceAccountingReconciliation {
                    expected_revision,
                    history,
                    outcomes: vec![],
                    trusted_time,
                },
            )
        })
        .await
}

fn succeeded(fixture: &Fixture, revision: u64, admitted_at: DateTime<Utc>) -> PaymentAttemptRecord {
    PaymentAttemptRecord {
        attempt_id: new_id(),
        mode: PaymentExecutionMode::Automatic,
        allowance_id: Some(fixture.allowance.as_str().to_owned()),
        association_revision: Some(revision),
        amount: crate::AmountRecord {
            value: "1".into(),
            asset: "btc".into(),
        },
        admitted_at,
        status: PaymentExecutionStatus::Succeeded,
        epoch: new_id(),
    }
}

/// History holding only the fixture occurrence with `attempt`, attributed to
/// retained association revision 1.
fn rebuilt(fixture: &Fixture, attempt: PaymentAttemptRecord) -> AllowanceAccountingHistory {
    let state = fixture.state();
    AllowanceAccountingHistory {
        occurrences: vec![PaymentOccurrenceRecord {
            key: PaymentOccurrenceKey {
                request: state.history.associations[0].request.clone(),
                billing_period: None,
            },
            disposition: PaymentDisposition::Automatic,
            allowance_id: Some(fixture.allowance.as_str().to_owned()),
            association_revision: Some(1),
            attempts: vec![attempt],
        }],
        ..Default::default()
    }
}

fn already_recorded(decision: PaymentAttemptDecision) -> bool {
    matches!(
        decision,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::PaymentAlreadyRecorded
        }
    )
}

fn committed_usage(fixture: &Fixture) -> usize {
    let state = fixture.state();
    let scope = &state.history.associations[0].request;
    execution::allowance_usage(&state, scope, fixture.allowance.as_str())
        .unwrap()
        .len()
}

/// Pins the #220 hazard: an empty reconciliation after a stale restore forgets the payment.
#[tokio::test]
async fn test_accounting_reconcile_empty_after_stale_restore_forgets_payment() {
    let fixture = stale_restore().await;
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::ReconciliationRequired
        }
    ));
    reconcile_history(&fixture, Default::default(), time())
        .await
        .unwrap();
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Ready { .. }
    ));
}

#[tokio::test]
async fn test_accounting_reconcile_rebuilds_lost_payment_from_occurrence_alone() {
    let fixture = stale_restore().await;
    let trusted_time = time() + Duration::minutes(2);
    let history = rebuilt(
        &fixture,
        succeeded(&fixture, 1, time() + Duration::minutes(1)),
    );
    let state = reconcile_history(&fixture, history, trusted_time)
        .await
        .unwrap();
    assert!(!state.requires_reconciliation);
    assert!(state
        .history
        .watermarks
        .iter()
        .all(|w| w.evaluated_at == trusted_time));
    assert!(already_recorded(fixture.reserve().await));
    assert_eq!(committed_usage(&fixture), 1);
}

#[tokio::test]
async fn test_accounting_reconcile_rejects_unknown_association_revision() {
    let fixture = stale_restore().await;
    let before = fixture.state();
    let history = rebuilt(&fixture, succeeded(&fixture, 2, time()));
    assert_error(
        reconcile_history(&fixture, history, time()).await,
        "Accounting attribution has no matching association revision",
    );
    assert_eq!(fixture.state(), before);
}

#[tokio::test]
async fn test_accounting_reconcile_rejects_admission_after_trusted_time() {
    let fixture = stale_restore().await;
    let before = fixture.state();
    let history = rebuilt(
        &fixture,
        succeeded(&fixture, 1, time() + Duration::minutes(1)),
    );
    assert_error(
        reconcile_history(&fixture, history, time()).await,
        "Reconciliation time precedes retained evidence",
    );
    assert_eq!(fixture.state(), before);
}

#[tokio::test]
async fn test_accounting_reconcile_rejects_conflicting_association_history() {
    let fixture = stale_restore().await;
    let before = fixture.state();
    let mut history = rebuilt(&fixture, succeeded(&fixture, 1, time()));
    let mut association = before.history.associations[0].clone();
    association.revisions[0].allowance_id = AllowanceId::new_v4().as_str().to_owned();
    history.associations.push(association);
    assert_error(
        reconcile_history(&fixture, history, time()).await,
        "Conflicting association history",
    );
    assert_eq!(fixture.state(), before);
}

#[tokio::test]
async fn test_accounting_reconcile_rejects_duplicate_supplied_rows() {
    let fixture = stale_restore().await;
    let before = fixture.state();
    let mut duplicate_occurrence = rebuilt(&fixture, succeeded(&fixture, 1, time()));
    duplicate_occurrence
        .occurrences
        .push(duplicate_occurrence.occurrences[0].clone());
    let duplicate_watermark = AllowanceAccountingHistory {
        watermarks: vec![before.history.watermarks[0].clone(); 2],
        ..Default::default()
    };
    for (history, expected) in [
        (duplicate_occurrence, "Duplicate payment occurrence"),
        (duplicate_watermark, "Duplicate accounting watermark"),
    ] {
        assert_error(reconcile_history(&fixture, history, time()).await, expected);
        assert_eq!(fixture.state(), before);
    }
}

/// #219: reconciliation appends a success after a wrongly reported failure.
#[tokio::test]
async fn test_accounting_reconcile_appends_success_after_reported_failure() {
    let fixture = Fixture::new().await;
    let failed = pay(&fixture, PaymentOutcome::Failed).await;
    assert_eq!(committed_usage(&fixture), 0);
    let history = rebuilt(&fixture, succeeded(&fixture, 1, time()));
    let state = reconcile_history(&fixture, history, time()).await.unwrap();
    let attempts = &state.history.occurrences[0].attempts;
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].attempt_id, failed);
    assert_eq!(attempts[0].status, PaymentExecutionStatus::Failed);
    assert!(already_recorded(fixture.reserve().await));
    assert_eq!(committed_usage(&fixture), 1);
}

#[tokio::test]
async fn test_accounting_reconcile_append_rejects_second_live_attempt() {
    let fixture = Fixture::new().await;
    pay(&fixture, PaymentOutcome::Failed).await;
    attempt(fixture.reserve().await);
    let before = fixture.state();
    let history = rebuilt(&fixture, succeeded(&fixture, 1, time()));
    assert_error(
        reconcile_history(&fixture, history, time()).await,
        "Multiple live attempts for one payment occurrence",
    );
    assert_eq!(fixture.state(), before);
}

#[tokio::test]
async fn test_accounting_failed_outcome_cannot_become_succeeded() {
    let fixture = Fixture::new().await;
    let failed = pay(&fixture, PaymentOutcome::Failed).await;
    assert_error(
        settle(&fixture, failed, PaymentOutcome::Succeeded).await,
        "Terminal payment outcome cannot be rewritten",
    );
    let failed_side = fixture.state();
    let mut succeeded_side = failed_side.clone();
    succeeded_side.history.occurrences[0].attempts[0].status = PaymentExecutionStatus::Succeeded;
    assert_error(
        merge_restored_accounting(Some(failed_side), Some(succeeded_side)),
        "Conflicting terminal payment outcomes",
    );
}
