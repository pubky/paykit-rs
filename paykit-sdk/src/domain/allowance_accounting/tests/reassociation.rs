use super::*;

fn boundary() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
}

async fn recurring_fixture() -> Fixture {
    let mut fixture = Fixture::new().await;
    let snapshot = fixture.storage.snapshot().unwrap();
    let original = snapshot
        .private_stream_items
        .iter()
        .find(|item| item.raw_json.contains("paykit.payment_request\""))
        .unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&original.raw_json).unwrap();
    fixture.occurrence.request.payment_request_id = paykit_lib::PaymentRequestId::new_v4();
    raw["event_id"] = new_id().into();
    raw["payment_request_id"] = fixture
        .occurrence
        .request
        .payment_request_id
        .as_str()
        .into();
    raw["request"]["recurrence"] = serde_json::json!({
        "every": 1, "unit": "month", "starts_at": time().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "anchor": time().to_rfc3339_opts(chrono::SecondsFormat::Secs, true), "ends_at": null
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
    fixture
        .storage
        .transaction(|tx| {
            select(
                tx,
                &path(),
                fixture.occurrence.request.clone(),
                AllowanceSelectionInput {
                    allowance_id: fixture.allowance.clone(),
                    expected_revision: None,
                    trusted_time: time(),
                },
                Some(checks()),
            )
        })
        .await
        .unwrap()
        .unwrap();
    fixture
}

async fn replacement(fixture: &Fixture, changes: serde_json::Value, accepted: bool) -> AllowanceId {
    let snapshot = fixture.storage.snapshot().unwrap();
    let original = snapshot
        .outbound_private_messages
        .iter()
        .find(|item| item.raw_json.contains("paykit.allowance_proposal\""))
        .unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&original.raw_json).unwrap();
    let id = AllowanceId::new_v4();
    let proposal_id = new_id();
    raw["event_id"] = proposal_id.clone().into();
    raw["allowance_id"] = id.as_str().into();
    raw["terms"]["active_from"] = boundary()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        .into();
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
    if accepted {
        let acceptance = serde_json::json!({
            "version": 1, "kind": "paykit.allowance_acceptance", "event_id": new_id(),
            "allowance_id": id.as_str(), "proposal_event_id": proposal_id
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
    }
    id
}

fn authorization(id: &AllowanceId) -> AllowanceReassociationInput {
    AllowanceReassociationInput {
        allowance_id: id.clone(),
        expected_revision: 1,
        effective_from: boundary(),
        authorization_id: new_id(),
        trusted_time: time(),
    }
}

async fn authorize(
    fixture: &Fixture,
    input: AllowanceReassociationInput,
) -> std::result::Result<AllowanceAssociationRecord, AllowanceAccountingBlock> {
    fixture
        .storage
        .transaction(|tx| reassociate(tx, &path(), fixture.occurrence.request.clone(), input))
        .await
        .unwrap()
}

fn occurrence(fixture: &Fixture, start: DateTime<Utc>) -> PaymentOccurrence {
    PaymentOccurrence {
        request: fixture.occurrence.request.clone(),
        billing_period: Some(
            paykit_lib::BillingPeriod::new(
                start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                (start + Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            )
            .unwrap(),
        ),
    }
}

async fn reserve_at(
    fixture: &Fixture,
    start: DateTime<Utc>,
    revision: u64,
) -> PaymentAttemptDecision {
    let mut current_checks = checks();
    current_checks.trusted_time = start;
    fixture
        .storage
        .transaction(|tx| {
            reserve(
                tx,
                &path(),
                occurrence(fixture, start),
                Some(revision),
                current_checks,
                PaymentExecutionMode::Automatic,
            )
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn test_reassociation_future_activation_preserves_boundary_and_authorization_watermark() {
    let fixture = recurring_fixture().await;
    let replacement = replacement(&fixture, serde_json::json!({}), true).await;
    let input = authorization(&replacement);
    let authorization_id = input.authorization_id.clone();
    let result = authorize(&fixture, input).await.unwrap();
    let revision = result.revisions.last().unwrap();
    assert_eq!(revision.revision, 2);
    assert_eq!(revision.effective_from, Some(boundary()));
    assert_eq!(revision.authorized_at, time());
    assert_eq!(
        revision.authorization_id.as_deref(),
        Some(authorization_id.as_str())
    );
    let state = fixture.state();
    let watermark = state
        .history
        .watermarks
        .iter()
        .find(|record| record.allowance_id == replacement.as_str())
        .unwrap();
    assert_eq!(watermark.evaluated_at, time());

    let previous = attempt(reserve_at(&fixture, boundary() - Duration::days(1), 1).await);
    assert_eq!(
        previous.allowance_id.as_deref(),
        Some(fixture.allowance.as_str())
    );
    let future = attempt(reserve_at(&fixture, boundary(), 2).await);
    assert_eq!(future.allowance_id.as_deref(), Some(replacement.as_str()));
    assert_eq!(fixture.state().history.occurrences.len(), 2);
}

#[tokio::test]
async fn test_reassociation_does_not_authorize_payment_before_activation() {
    let fixture = recurring_fixture().await;
    let replacement = replacement(
        &fixture,
        serde_json::json!({
            "active_from": (boundary() + Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }),
        true,
    )
    .await;
    authorize(&fixture, authorization(&replacement))
        .await
        .unwrap();
    assert!(matches!(reserve_at(&fixture, boundary(), 2).await,
        PaymentAttemptDecision::Blocked { reason: AllowanceAccountingBlock::SharedRule { code } }
        if code == "not_active"));
    assert!(fixture.state().history.occurrences.is_empty());
    attempt(reserve_at(&fixture, boundary() + Duration::days(1), 2).await);
}

#[tokio::test]
async fn test_reassociation_handoff_rechecks_replacement_expiry() {
    let fixture = recurring_fixture().await;
    let replacement = replacement(
        &fixture,
        serde_json::json!({
            "expires_at": (boundary() + Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }),
        true,
    )
    .await;
    authorize(&fixture, authorization(&replacement))
        .await
        .unwrap();
    let prepared = attempt(reserve_at(&fixture, boundary(), 2).await);
    let mut current_checks = checks();
    current_checks.trusted_time = boundary() + Duration::hours(1);
    let decision = fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id, current_checks))
        .await
        .unwrap();
    assert!(matches!(decision,
        PaymentAttemptDecision::Blocked { reason: AllowanceAccountingBlock::SharedRule { code } }
        if code == "expired"));
    assert_eq!(
        fixture.state().history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Prepared
    );
}

#[tokio::test]
async fn test_reassociation_rejects_rollback_without_advancing_to_future_boundary() {
    let fixture = recurring_fixture().await;
    let replacement = replacement(&fixture, serde_json::json!({}), true).await;
    let later = time() + Duration::hours(1);
    fixture
        .storage
        .transaction(|tx| candidates(tx, &path(), fixture.occurrence.request.clone(), later))
        .await
        .unwrap();
    assert!(
        matches!(authorize(&fixture, authorization(&replacement)).await,
        Err(AllowanceAccountingBlock::SharedRule { code }) if code == "clock_rollback")
    );
    let state = fixture.state();
    assert!(state
        .history
        .associations
        .iter()
        .all(|record| record.revisions.len() == 1));
    assert_eq!(
        state
            .history
            .watermarks
            .iter()
            .find(|record| { record.allowance_id == replacement.as_str() })
            .unwrap()
            .evaluated_at,
        later
    );
}

#[tokio::test]
async fn test_reassociation_requires_accepted_matching_authority() {
    for (changes, accepted, expected) in [
        (
            serde_json::json!({}),
            false,
            AllowanceAccountingBlock::InvalidLifecycle,
        ),
        (
            serde_json::json!({"asset": "usd"}),
            true,
            shared(paykit_lib::AllowanceEvaluationBlock::AssetMismatch),
        ),
        (
            serde_json::json!({"allowed_payment_endpoint_identifiers": ["btc-onchain-p2tr"]}),
            true,
            shared(paykit_lib::AllowanceEvaluationBlock::NoEligibleEndpoint),
        ),
    ] {
        let fixture = recurring_fixture().await;
        let replacement = replacement(&fixture, changes, accepted).await;
        assert!(
            matches!(authorize(&fixture, authorization(&replacement)).await,
            Err(reason) if reason == expected)
        );
        assert!(fixture
            .state()
            .history
            .associations
            .iter()
            .all(|record| record.revisions.len() == 1));
    }
}

#[tokio::test]
async fn test_reassociation_requires_current_revision() {
    let fixture = recurring_fixture().await;
    let replacement = replacement(&fixture, serde_json::json!({}), true).await;
    let mut input = authorization(&replacement);
    input.expected_revision = 2;
    assert!(matches!(
        authorize(&fixture, input).await,
        Err(AllowanceAccountingBlock::StaleRevision)
    ));
}

#[tokio::test]
async fn test_initial_selection_still_requires_current_activation() {
    let fixture = recurring_fixture().await;
    let replacement = replacement(&fixture, serde_json::json!({}), true).await;
    let result = fixture
        .storage
        .transaction(|tx| {
            select(
                tx,
                &path(),
                fixture.occurrence.request.clone(),
                AllowanceSelectionInput {
                    allowance_id: replacement,
                    expected_revision: Some(1),
                    trusted_time: time(),
                },
                None,
            )
        })
        .await
        .unwrap();
    assert!(matches!(result,
        Err(AllowanceAccountingBlock::SharedRule { code }) if code == "not_active"));
}
