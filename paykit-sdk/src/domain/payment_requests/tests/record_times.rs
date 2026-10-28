use super::*;

const REQUEST_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
const PROPOSAL_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101";
const ACCEPTANCE_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102";
const PROOF_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103";
const LATER_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104";
const REFERENCE: &str = "invoice-2026-0001";
const EXPIRES_AT: &str = "2026-06-03T12:05:00Z";
const MONTHLY: &str = r#"{"every":1,"unit":"month","starts_at":"2026-06-01T00:00:00Z","anchor":"2026-06-01T00:00:00Z","ends_at":null}"#;
const FIRST_MONTH: &str =
    r#"{"starts_at":"2026-06-01T00:00:00Z","ends_at":"2026-07-01T00:00:00Z"}"#;

fn after_expiry(minutes: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(EXPIRES_AT)
        .unwrap()
        .with_timezone(&Utc)
        + ChronoDuration::minutes(minutes)
}

async fn payee_proposal(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    recurrence: Option<&str>,
) {
    enqueue_untyped_private_message(
        storage,
        peer.clone(),
        request_raw(
            PROPOSAL_ID,
            REQUEST_ID,
            REFERENCE,
            Some(EXPIRES_AT),
            recurrence,
        ),
        timestamp(),
    )
    .await
    .unwrap();
}

async fn record_at(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> PaymentRequestRecord {
    payment_request_records(storage, peer, now)
        .await
        .unwrap()
        .remove(0)
}

/// Stores an Acceptance fetched after expiry, then `proof`, and returns the
/// Acceptance's local record time.
async fn fetch_late_acceptance_and_proof(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    proof: String,
) -> DateTime<Utc> {
    let fetched_at = after_expiry(5);
    persist_messages_at(
        storage,
        peer.clone(),
        vec![acceptance_raw(ACCEPTANCE_ID, REQUEST_ID)],
        fetched_at,
    )
    .await;
    persist_messages_at(storage, peer.clone(), vec![proof], after_expiry(6)).await;
    fetched_at
}

async fn cancel(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord> {
    enqueue_checked_payment_request_action(
        storage,
        peer.clone(),
        &app_id(),
        &parsed_event(cancellation_raw(LATER_ID, REQUEST_ID)),
        now,
    )
    .await
}

#[tokio::test]
async fn test_record_times_payee_keeps_acceptance_fetched_after_expiry() {
    for (recurrence, billing_period, expected) in [
        (None, "null", PaymentRequestLifecycleState::ProofSubmitted),
        (
            Some(MONTHLY),
            FIRST_MONTH,
            PaymentRequestLifecycleState::ActiveRecurring,
        ),
    ] {
        let storage = registered_storage();
        let peer = counterparty();
        payee_proposal(&storage, &peer, recurrence).await;
        let proof = proof_raw(PROOF_ID, REQUEST_ID, REFERENCE).replace(
            r#""billing_period":null"#,
            &format!(r#""billing_period":{billing_period}"#),
        );
        let fetched_at = fetch_late_acceptance_and_proof(&storage, &peer, proof).await;

        let record = record_at(&storage, &peer, after_expiry(7)).await;

        assert_eq!(record.state, expected, "{:?}", record.invalid_reason);
        assert!(record.invalid_reason.is_none());
        assert_eq!(record.accepted_event_id.as_deref(), Some(ACCEPTANCE_ID));
        assert_eq!(record.acceptance_recorded_at, Some(fetched_at));
        assert_eq!(record.payment_proofs.len(), 1);
        assert_eq!(record.payment_proofs[0].event_id, PROOF_ID);
        cancel(&storage, &peer, after_expiry(7)).await.unwrap();
    }
}

#[tokio::test]
async fn test_record_times_late_acceptance_crosses_expired_cancellation() {
    let storage = registered_storage();
    let peer = counterparty();
    payee_proposal(&storage, &peer, None).await;
    assert_eq!(
        record_at(&storage, &peer, after_expiry(1)).await.state,
        PaymentRequestLifecycleState::ProposalExpired
    );
    cancel(&storage, &peer, after_expiry(1)).await.unwrap();
    let fetched_at = fetch_late_acceptance_and_proof(
        &storage,
        &peer,
        proof_raw(PROOF_ID, REQUEST_ID, REFERENCE),
    )
    .await;

    let record = record_at(&storage, &peer, after_expiry(7)).await;

    assert_eq!(record.state, PaymentRequestLifecycleState::Canceled);
    assert!(record.invalid_reason.is_none());
    assert_eq!(record.canceled_event_id.as_deref(), Some(LATER_ID));
    assert_eq!(record.accepted_event_id.as_deref(), Some(ACCEPTANCE_ID));
    assert_eq!(record.acceptance_recorded_at, Some(fetched_at));
    assert!(payment_proof_allowed_states(&record).contains(&record.state));
    assert_eq!(record.payment_proofs.len(), 1);
    assert_eq!(record.payment_proofs[0].event_id, PROOF_ID);
}

#[tokio::test]
async fn test_record_times_proposal_time_stays_fixed_for_both_roles() {
    for local_role in [
        PaymentRequestLocalRole::Payee,
        PaymentRequestLocalRole::Payer,
    ] {
        let storage = registered_storage();
        let peer = counterparty();
        let proposal = request_raw(PROPOSAL_ID, REQUEST_ID, REFERENCE, None, None);
        let acceptance = acceptance_raw(ACCEPTANCE_ID, REQUEST_ID);
        let accepted_at = timestamp() + ChronoDuration::minutes(1);
        if local_role == PaymentRequestLocalRole::Payee {
            enqueue_untyped_private_message(&storage, peer.clone(), proposal, timestamp())
                .await
                .unwrap();
            persist_messages_at(&storage, peer.clone(), vec![acceptance], accepted_at).await;
        } else {
            persist_messages_at(&storage, peer.clone(), vec![proposal], timestamp()).await;
            enqueue_untyped_private_message(&storage, peer.clone(), acceptance, accepted_at)
                .await
                .unwrap();
        }

        let record = record_at(&storage, &peer, accepted_at).await;

        assert_eq!(record.local_role, Some(local_role));
        assert_eq!(record.state, PaymentRequestLifecycleState::Accepted);
        assert_eq!(record.proposal_recorded_at, Some(timestamp()));
        assert_eq!(record.acceptance_recorded_at, Some(accepted_at));
        assert_eq!(record.last_event_at, Some(accepted_at));
    }
}

#[tokio::test]
async fn test_record_times_conflicting_proposal_keeps_first_time() {
    let storage = registered_storage();
    let peer = counterparty();
    let conflicting_at = timestamp() + ChronoDuration::minutes(1);
    persist_messages_at(
        &storage,
        peer.clone(),
        vec![request_raw(PROPOSAL_ID, REQUEST_ID, REFERENCE, None, None)],
        timestamp(),
    )
    .await;
    persist_messages_at(
        &storage,
        peer.clone(),
        vec![request_raw(
            LATER_ID,
            REQUEST_ID,
            "other-reference",
            None,
            None,
        )],
        conflicting_at,
    )
    .await;

    let full = record_at(&storage, &peer, conflicting_at).await;
    let received = received_payment_request_records(&storage, &peer, conflicting_at)
        .await
        .unwrap()
        .remove(0);

    for record in [full, received] {
        assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
        assert_eq!(record.proposal_recorded_at, Some(timestamp()));
        assert_eq!(record.last_event_at, Some(conflicting_at));
    }
}
