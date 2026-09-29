use super::*;

async fn propose(fixture: &Fixture, recurring: bool) -> PaymentOccurrence {
    let snapshot = fixture.storage.snapshot().unwrap();
    let original = snapshot
        .private_stream_items
        .iter()
        .find(|item| item.raw_json.contains("paykit.payment_request\""))
        .unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&original.raw_json).unwrap();
    let mut occurrence = fixture.occurrence.clone();
    occurrence.request.payment_request_id = paykit_lib::PaymentRequestId::new_v4();
    raw["event_id"] = new_id().into();
    raw["payment_request_id"] = occurrence.request.payment_request_id.as_str().into();
    if recurring {
        raw["request"]["recurrence"] = serde_json::json!({
            "every": 1, "unit": "month", "starts_at": "2026-09-15T12:00:00Z",
            "anchor": "2026-09-15T12:00:00Z", "ends_at": null
        });
    }
    crate::domain::private_stream::persist_private_stream_batch(
        &fixture.storage,
        occurrence.request.counterparty.clone(),
        path(),
        vec![message(raw.to_string())],
        None,
        time(),
    )
    .await
    .unwrap();
    occurrence
}

async fn accept(
    fixture: &Fixture,
    occurrence: &PaymentOccurrence,
    expected_revision: Option<u64>,
    trusted_time: DateTime<Utc>,
) -> std::result::Result<AllowanceAssociationRecord, AllowanceAccountingBlock> {
    let mut wallet_checks = checks();
    wallet_checks.trusted_time = trusted_time;
    fixture
        .storage
        .transaction(|tx| {
            select(
                tx,
                &path(),
                occurrence.request.clone(),
                AllowanceSelectionInput {
                    allowance_id: fixture.allowance.clone(),
                    expected_revision,
                    trusted_time,
                },
                Some(wallet_checks),
            )
        })
        .await
        .unwrap()
}

async fn with_limits(changes: serde_json::Value) -> Fixture {
    let mut fixture = Fixture::new().await;
    let snapshot = fixture.storage.snapshot().unwrap();
    let original = snapshot
        .outbound_private_messages
        .iter()
        .find(|item| item.raw_json.contains("paykit.allowance_proposal\""))
        .unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&original.raw_json).unwrap();
    fixture.allowance = AllowanceId::new_v4();
    let proposal_id = new_id();
    raw["event_id"] = proposal_id.clone().into();
    raw["allowance_id"] = fixture.allowance.as_str().into();
    for (key, value) in changes.as_object().unwrap() {
        raw["terms"][key] = value.clone();
    }
    crate::domain::outbound_private::enqueue_private_message(
        &fixture.storage,
        fixture.occurrence.request.counterparty.clone(),
        path(),
        raw.to_string(),
        time(),
    )
    .await
    .unwrap();
    let acceptance = serde_json::json!({
        "version": 1, "kind": "paykit.allowance_acceptance", "event_id": new_id(),
        "allowance_id": fixture.allowance.as_str(), "proposal_event_id": proposal_id
    });
    crate::domain::private_stream::persist_private_stream_batch(
        &fixture.storage,
        fixture.occurrence.request.counterparty.clone(),
        path(),
        vec![message(acceptance.to_string())],
        None,
        time(),
    )
    .await
    .unwrap();
    fixture.occurrence = propose(&fixture, false).await;
    accept(&fixture, &fixture.occurrence, None, time())
        .await
        .unwrap();
    fixture
}

fn period_limit(amount: Option<&str>, count: Option<u64>) -> serde_json::Value {
    serde_json::json!({
        "lifetime_amount_limit": null,
        "period_limits": [{
            "amount_limit": amount, "payment_count_limit": count,
            "period": {"kind": "rolling", "every": 1, "unit": "day"}
        }]
    })
}

async fn request_state(
    fixture: &Fixture,
    occurrence: &PaymentOccurrence,
) -> crate::PaymentRequestLifecycleState {
    fixture
        .storage
        .transaction(|tx| Ok(request(tx, &scope(tx, &path(), &occurrence.request)?, time())?.state))
        .await
        .unwrap()
}

#[tokio::test]
async fn test_one_time_acceptance_blocks_exhausted_limits_without_side_effects() {
    for (limits, code) in [
        (
            serde_json::json!({"lifetime_amount_limit": "1"}),
            "lifetime_amount_limit",
        ),
        (period_limit(Some("1"), None), "period_amount_limit"),
        (period_limit(Some("10"), Some(1)), "period_count_limit"),
    ] {
        let fixture = with_limits(limits).await;
        attempt(fixture.reserve().await);
        let second = propose(&fixture, false).await;
        let before = fixture.storage.snapshot().unwrap();
        assert!(matches!(accept(&fixture, &second, None, time()).await,
            Err(AllowanceAccountingBlock::SharedRule { code: actual }) if actual == code));
        let after = fixture.storage.snapshot().unwrap();
        assert_eq!(
            after.outbound_private_messages,
            before.outbound_private_messages
        );
        assert_eq!(
            after.allowance_accounting.as_ref().unwrap().history,
            before.allowance_accounting.as_ref().unwrap().history
        );
        assert_eq!(
            request_state(&fixture, &second).await,
            crate::PaymentRequestLifecycleState::Proposed
        );
    }
}

#[tokio::test]
async fn test_one_time_acceptance_at_exact_remaining_capacity_consumes_nothing() {
    for limits in [
        serde_json::json!({"lifetime_amount_limit": "2.00"}),
        period_limit(Some("2.00"), Some(2)),
    ] {
        let fixture = with_limits(limits).await;
        attempt(fixture.reserve().await);
        let second = propose(&fixture, false).await;
        let before = fixture.state().history.occurrences;
        accept(&fixture, &second, None, time()).await.unwrap();
        assert_eq!(fixture.state().history.occurrences, before);
        assert_eq!(
            request_state(&fixture, &second).await,
            crate::PaymentRequestLifecycleState::Accepted
        );
    }
}

#[tokio::test]
async fn test_one_time_acceptance_counts_committed_and_unresolved_usage() {
    for status in [
        PaymentExecutionStatus::Prepared,
        PaymentExecutionStatus::Submitted,
        PaymentExecutionStatus::Unknown,
        PaymentExecutionStatus::Succeeded,
    ] {
        let fixture = Fixture::new().await;
        let prepared = attempt(fixture.reserve().await);
        if status != PaymentExecutionStatus::Prepared {
            fixture
                .storage
                .transaction(|tx| begin(tx, &path(), prepared.attempt_id.clone(), checks()))
                .await
                .unwrap();
        }
        if matches!(
            status,
            PaymentExecutionStatus::Unknown | PaymentExecutionStatus::Succeeded
        ) {
            fixture
                .storage
                .transaction(|tx| {
                    report_outcome(
                        tx,
                        PaymentOutcomeReport {
                            attempt_id: prepared.attempt_id.clone(),
                            outcome: if status == PaymentExecutionStatus::Unknown {
                                PaymentOutcome::Unknown
                            } else {
                                PaymentOutcome::Succeeded
                            },
                        },
                    )
                })
                .await
                .unwrap();
        }
        let second = propose(&fixture, false).await;
        assert!(matches!(accept(&fixture, &second, None, time()).await,
            Err(AllowanceAccountingBlock::SharedRule { code }) if code == "lifetime_amount_limit"));
    }
}

#[tokio::test]
async fn test_one_time_acceptance_excludes_failed_and_manual_attempts() {
    for manual in [false, true] {
        let fixture = Fixture::new().await;
        if manual {
            attempt(
                fixture
                    .storage
                    .transaction(|tx| {
                        reserve(
                            tx,
                            &path(),
                            fixture.occurrence.clone(),
                            None,
                            checks(),
                            PaymentExecutionMode::Manual,
                        )
                    })
                    .await
                    .unwrap(),
            );
        } else {
            let prepared = attempt(fixture.reserve().await);
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
        }
        let second = propose(&fixture, false).await;
        let before = fixture.state().history.occurrences;
        accept(&fixture, &second, None, time()).await.unwrap();
        assert_eq!(fixture.state().history.occurrences, before);
    }
}

#[tokio::test]
async fn test_recurring_acceptance_does_not_require_current_capacity() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    let recurring = propose(&fixture, true).await;
    let before = fixture.state().history.occurrences;
    accept(&fixture, &recurring, None, time()).await.unwrap();
    assert_eq!(fixture.state().history.occurrences, before);
    assert_eq!(
        request_state(&fixture, &recurring).await,
        crate::PaymentRequestLifecycleState::ActiveRecurring
    );
}

#[tokio::test]
async fn test_blocked_acceptance_preserves_selection_and_advances_watermark() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    let second = propose(&fixture, false).await;
    fixture
        .storage
        .transaction(|tx| {
            select(
                tx,
                &path(),
                second.request.clone(),
                AllowanceSelectionInput {
                    allowance_id: fixture.allowance.clone(),
                    expected_revision: None,
                    trusted_time: time(),
                },
                None,
            )
        })
        .await
        .unwrap()
        .unwrap();
    let before = fixture.state();
    let later = time() + Duration::hours(1);
    assert!(matches!(accept(&fixture, &second, Some(1), later).await,
        Err(AllowanceAccountingBlock::SharedRule { code }) if code == "lifetime_amount_limit"));
    let after = fixture.state();
    assert_eq!(after.history.associations, before.history.associations);
    assert_eq!(after.history.occurrences, before.history.occurrences);
    assert_eq!(after.history.watermarks[0].evaluated_at, later);
    assert_eq!(
        request_state(&fixture, &second).await,
        crate::PaymentRequestLifecycleState::Proposed
    );
    // The failed preflight leaves the ordinary manual response path available.
    fixture
        .storage
        .transaction(|tx| manual_response(tx, &path(), second.request.clone()))
        .await
        .unwrap();
    assert_eq!(
        fixture
            .state()
            .history
            .occurrences
            .last()
            .unwrap()
            .disposition,
        PaymentDisposition::ManualOnly
    );
}

#[tokio::test]
async fn test_capacity_usage_is_scoped_to_both_receiver_references_and_allowance() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    let state = fixture.state();
    let scope = &state.history.occurrences[0].key.request;
    let usage = execution::allowance_usage(&state, scope, fixture.allowance.as_str()).unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].admitted_at(), time());
    assert_eq!(usage[0].amount().value(), "1");
    // Identical Allowance IDs in another authenticated receiver scope cannot
    // spend this scope's capacity. Request IDs, however, share that capacity.
    for field in 0..5 {
        let mut other = scope.clone();
        match field {
            0 => other.local_public_key = public_key(),
            1 => other.local_receiver_path = PaykitReceiverPath::new("bitkit/server").unwrap(),
            2 => other.counterparty = public_key(),
            3 => {
                other.counterparty_receiver_path = PaykitReceiverPath::new("bitkit/server").unwrap()
            }
            _ => other.payment_request_id = new_id(),
        }
        let usage = execution::allowance_usage(&state, &other, fixture.allowance.as_str()).unwrap();
        assert_eq!(usage.len(), usize::from(field == 4));
    }
    assert!(
        execution::allowance_usage(&state, scope, AllowanceId::new_v4().as_str())
            .unwrap()
            .is_empty()
    );
}
