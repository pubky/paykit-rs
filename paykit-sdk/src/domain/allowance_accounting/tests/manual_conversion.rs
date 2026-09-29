use super::*;

async fn converted_fixture() -> Fixture {
    let mut fixture = Fixture::new().await;
    fixture.occurrence.request.payment_request_id = paykit_lib::PaymentRequestId::new_v4();
    let request_id = fixture.occurrence.request.payment_request_id.clone();
    let raw = serde_json::json!({
        "version": 1, "kind": "paykit.payment_request", "event_id": new_id(),
        "payment_request_id": request_id.as_str(),
        "request": {
            "amount": { "value": "1", "asset": "usd" },
            "payment_reference": "converted", "proposal_expires_at": null,
            "recurrence": null, "metadata": {},
            "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
            "conversion": { "type": "fixed", "rates": [{"asset": "btc", "value": "0.00001"}] }
        }
    });
    crate::domain::private_stream::persist_private_stream_batch(
        &fixture.storage,
        fixture.occurrence.request.counterparty.clone(),
        path(),
        vec![message(raw.to_string())],
        None,
        time(),
    )
    .await
    .unwrap();
    crate::domain::payment_requests::enqueue_payment_request_acceptance(
        &fixture.storage,
        fixture.occurrence.request.counterparty.clone(),
        path(),
        &paykit_lib::PaymentRequestAcceptance::new(paykit_lib::EventId::new_v4(), request_id),
        time(),
    )
    .await
    .unwrap();
    fixture
}

fn converted_checks() -> PaymentExecutionChecks {
    let mut checks = checks();
    // One USD at the agreed 0.00001 BTC/USD rate, independently checked by the wallet.
    checks.actual_amount = paykit_lib::PaymentAmount::new("0.00001", "btc").unwrap();
    checks
}

async fn reserve_manual(
    fixture: &Fixture,
    checks: PaymentExecutionChecks,
) -> PaymentAttemptDecision {
    fixture
        .storage
        .transaction(|tx| {
            reserve(
                tx,
                &path(),
                fixture.occurrence.clone(),
                None,
                checks,
                PaymentExecutionMode::Manual,
            )
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn test_accounting_manual_conversion_retains_actual_amount_and_hands_off() {
    let fixture = converted_fixture().await;
    let prepared = attempt(reserve_manual(&fixture, converted_checks()).await);
    assert_eq!(prepared.amount.value, "0.00001");
    assert_eq!(prepared.amount.asset, "btc");
    assert_eq!(prepared.allowance_id, None);
    let submitted = fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id.clone(), converted_checks()))
        .await
        .unwrap();
    assert_eq!(attempt(submitted).status, PaymentExecutionStatus::Submitted);
    let scope = &fixture.state().history.occurrences[0].key.request;
    assert!(
        execution::allowance_usage(&fixture.state(), scope, fixture.allowance.as_str())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn test_accounting_manual_conversion_handoff_cannot_change_amount_or_asset() {
    let fixture = converted_fixture().await;
    let prepared = attempt(reserve_manual(&fixture, converted_checks()).await);
    for (value, asset) in [("0.00002", "btc"), ("1", "usd")] {
        let mut changed = converted_checks();
        changed.actual_amount = paykit_lib::PaymentAmount::new(value, asset).unwrap();
        let decision = fixture
            .storage
            .transaction(|tx| begin(tx, &path(), prepared.attempt_id.clone(), changed))
            .await
            .unwrap();
        assert!(matches!(
            decision,
            PaymentAttemptDecision::Blocked {
                reason: AllowanceAccountingBlock::WalletChecksFailed
            }
        ));
    }
    assert_eq!(
        fixture.state().history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Prepared
    );
}

#[tokio::test]
async fn test_accounting_conversion_requires_manual_authority_and_wallet_attestation() {
    let fixture = converted_fixture().await;
    let automatic = fixture
        .storage
        .transaction(|tx| {
            reserve(
                tx,
                &path(),
                fixture.occurrence.clone(),
                None,
                converted_checks(),
                PaymentExecutionMode::Automatic,
            )
        })
        .await
        .unwrap();
    assert!(matches!(
        automatic,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::WalletChecksFailed
        }
    ));
    let mut unverified = converted_checks();
    unverified.local_enabled = false;
    assert!(matches!(
        reserve_manual(&fixture, unverified).await,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::WalletChecksFailed
        }
    ));
    assert!(fixture.state().history.occurrences.is_empty());
}

#[tokio::test]
async fn test_accounting_manual_same_asset_requires_requested_amount() {
    let fixture = Fixture::new().await;
    let mut underpaid = checks();
    underpaid.actual_amount = paykit_lib::PaymentAmount::new("0.5", "btc").unwrap();
    assert!(matches!(
        reserve_manual(&fixture, underpaid).await,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::WalletChecksFailed
        }
    ));
    let prepared = attempt(reserve_manual(&fixture, checks()).await);
    assert_eq!(prepared.amount.value, "1");
    let submitted = fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id.clone(), checks()))
        .await
        .unwrap();
    assert_eq!(attempt(submitted).status, PaymentExecutionStatus::Submitted);
}

#[tokio::test]
async fn test_accounting_recovery_preserves_manual_conversion_amount() {
    let fixture = converted_fixture().await;
    let _prepared = attempt(reserve_manual(&fixture, converted_checks()).await);
    let state = fixture.state();
    validate_accounting(&state).unwrap();
    let restored = merge_restored_accounting(None, Some(state.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(
        restored.history.occurrences[0].attempts[0].amount,
        state.history.occurrences[0].attempts[0].amount
    );
    assert_eq!(
        restored.history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Unknown
    );
    let mut conflicting = state.clone();
    conflicting.history.occurrences[0].attempts[0].amount.value = "0.00002".into();
    assert!(merge_restored_accounting(Some(state), Some(conflicting)).is_err());
}
