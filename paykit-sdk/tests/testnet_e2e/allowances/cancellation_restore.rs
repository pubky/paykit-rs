//! Restore must preserve received Cancellation even when wallet history is unchanged.
use super::*;
use chrono::Utc;
use paykit_lib::{
    PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms,
};
use paykit_sdk::{
    AllowanceAccountingBlock, AllowanceAccountingReconciliation, AllowanceSelectionInput,
    PaymentAttemptDecision, PaymentExecutionChecks, PaymentOccurrence,
    PaymentRequestLifecycleState, PaymentRequestScope,
};

fn checks() -> PaymentExecutionChecks {
    PaymentExecutionChecks {
        trusted_time: Utc::now(),
        payment_endpoint_identifier: PaymentEndpointIdentifier::new("btc-lightning-bolt11")
            .unwrap(),
        actual_amount: PaymentAmount::new("0.001", "btc").unwrap(),
        endpoint_current: true,
        local_enabled: true,
        recurrence_eligible: true,
    }
}

async fn accept_automatic_request(payer: &TestUser, payee: &TestUser) -> PaymentOccurrence {
    let proposal = payer
        .sdk
        .propose_allowance(
            payee.public_key.clone(),
            AllowanceLocalRole::Allower,
            terms(),
        )
        .await
        .unwrap();
    let allowance_id = AllowanceId::new(proposal.allowance_id).unwrap();
    deliver(payer, payee).await;
    payee
        .sdk
        .accept_allowance(payer.public_key.clone(), &allowance_id)
        .await
        .unwrap();
    deliver(payee, payer).await;
    let request = payee
        .sdk
        .propose_payment_request(
            payer.public_key.clone(),
            PaymentRequestTerms::builder(
                checks().actual_amount,
                PaymentReference::new("restore-cancellation").unwrap(),
                vec![checks().payment_endpoint_identifier],
            )
            .build()
            .unwrap(),
        )
        .await
        .unwrap();
    deliver(payee, payer).await;
    payer
        .sdk
        .reconcile_allowance_accounting(AllowanceAccountingReconciliation {
            expected_revision: None,
            history: Default::default(),
            outcomes: vec![],
            trusted_time: Utc::now(),
        })
        .await
        .unwrap();
    let scope = PaymentRequestScope {
        counterparty: payee.public_key.clone(),
        payment_request_id: PaymentRequestId::new(request.payment_request_id).unwrap(),
    };
    payer
        .sdk
        .claim_payment_request_for_execution(scope.counterparty.clone(), &scope.payment_request_id)
        .await
        .unwrap();
    let checks = checks();
    payer
        .sdk
        .accept_payment_request_automatically(
            scope.clone(),
            AllowanceSelectionInput {
                allowance_id,
                expected_revision: None,
                trusted_time: checks.trusted_time,
            },
            checks,
        )
        .await
        .unwrap();
    deliver(payer, payee).await;
    PaymentOccurrence {
        request: scope,
        billing_period: None,
    }
}

async fn cancel_request(payer: &TestUser, payee: &TestUser, occurrence: &PaymentOccurrence) {
    payee
        .sdk
        .cancel_payment_request(
            payer.public_key.clone(),
            &occurrence.request.payment_request_id,
            None,
        )
        .await
        .unwrap();
    deliver(payee, payer).await;
    let records = payer
        .sdk
        .payment_requests_with(&payee.public_key)
        .await
        .unwrap();
    assert_eq!(records[0].state, PaymentRequestLifecycleState::Canceled);
}

async fn reconcile_unchanged_history(payer: &TestUser) {
    let state = payer
        .sdk
        .allowance_accounting_state()
        .await
        .unwrap()
        .unwrap();
    payer
        .sdk
        .reconcile_allowance_accounting(AllowanceAccountingReconciliation {
            expected_revision: Some(state.revision),
            history: state.history,
            outcomes: vec![],
            trusted_time: Utc::now(),
        })
        .await
        .unwrap();
}

fn assert_cancellation_blocks(decision: PaymentAttemptDecision) {
    assert!(
        matches!(
            decision,
            PaymentAttemptDecision::Blocked {
                reason: AllowanceAccountingBlock::InvalidLifecycle
            }
        ),
        "{decision:?}"
    );
}

#[tokio::test]
async fn test_stale_restore_cannot_erase_cancellation_and_readmit_payment() {
    let pair = linked_two_party().await;
    let occurrence = accept_automatic_request(&pair.alice, &pair.bob).await;
    // No attempt exists yet, so retained payment history cannot mask a lost Cancellation.
    let backup = pair.alice.sdk.export_backup_state().await.unwrap();
    cancel_request(&pair.alice, &pair.bob, &occurrence).await;
    let before = pair.alice.storage.snapshot().unwrap();
    assert!(pair.alice.sdk.restore_backup_state(backup).await.is_err());
    assert_eq!(pair.alice.storage.snapshot().unwrap(), before);
    reconcile_unchanged_history(&pair.alice).await;
    assert_cancellation_blocks(
        pair.alice
            .sdk
            .reserve_automatic_payment(occurrence, 1, checks())
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn test_stale_restore_cannot_erase_cancellation_before_handoff() {
    let pair = linked_two_party().await;
    let occurrence = accept_automatic_request(&pair.alice, &pair.bob).await;
    let PaymentAttemptDecision::Ready { attempt } = pair
        .alice
        .sdk
        .reserve_automatic_payment(occurrence.clone(), 1, checks())
        .await
        .unwrap()
    else {
        panic!("expected prepared attempt")
    };
    let backup = pair.alice.sdk.export_backup_state().await.unwrap();
    cancel_request(&pair.alice, &pair.bob, &occurrence).await;
    let before = pair.alice.storage.snapshot().unwrap();
    assert!(pair.alice.sdk.restore_backup_state(backup).await.is_err());
    assert_eq!(pair.alice.storage.snapshot().unwrap(), before);
    assert_cancellation_blocks(
        pair.alice
            .sdk
            .begin_payment_execution(attempt.attempt_id, checks())
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn test_current_restore_retains_cancellation_after_reconciliation() {
    let pair = linked_two_party().await;
    let occurrence = accept_automatic_request(&pair.alice, &pair.bob).await;
    cancel_request(&pair.alice, &pair.bob, &occurrence).await;
    let backup = pair.alice.sdk.export_backup_state().await.unwrap();
    let restored = pair
        .alice
        .restart_with_storage(InMemoryStorage::new())
        .await;
    restored.sdk.restore_backup_state(backup).await.unwrap();
    reconcile_unchanged_history(&restored).await;
    assert_cancellation_blocks(
        restored
            .sdk
            .reserve_automatic_payment(occurrence, 1, checks())
            .await
            .unwrap(),
    );
}
