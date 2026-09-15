//! Real Encrypted Link evidence exchange; these tests do not execute payments.

use super::*;
use paykit_lib::{
    BillingPeriod, PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms, Recurrence, RecurrenceUnit,
};
use paykit_sdk::{PaymentProofSubmission, PaymentRequestLifecycleState, PaymentRequestRecord};

async fn accepted_allowance(allower: &TestUser, allowee: &TestUser) -> AllowanceId {
    let proposed = allower
        .sdk
        .propose_allowance(
            allowee.public_key.clone(),
            allowee.receiver_path.clone(),
            AllowanceLocalRole::Allower,
            terms(),
        )
        .await
        .unwrap();
    let id = AllowanceId::new(proposed.allowance_id).unwrap();
    deliver(allower, allowee).await;
    allowee
        .sdk
        .accept_allowance(
            allower.public_key.clone(),
            allower.receiver_path.clone(),
            &id,
        )
        .await
        .unwrap();
    deliver(allowee, allower).await;
    id
}

async fn accepted_request(
    payer: &TestUser,
    payee: &TestUser,
    recurrence: Option<Recurrence>,
) -> PaymentRequestId {
    let request = payee
        .sdk
        .propose_payment_request(
            payer.public_key.clone(),
            payer.receiver_path.clone(),
            PaymentRequestTerms {
                amount: PaymentAmount::new("0.001", "btc").unwrap(),
                payment_reference: PaymentReference::new("allowance-proof-invoice").unwrap(),
                proposal_expires_at: None,
                recurrence,
                accepted_payment_endpoint_identifiers: vec![PaymentEndpointIdentifier::new(
                    "btc-lightning-bolt11",
                )
                .unwrap()],
                metadata: serde_json::Map::new(),
            },
        )
        .await
        .unwrap();
    let id = PaymentRequestId::new(request.payment_request_id).unwrap();
    deliver(payee, payer).await;
    payer
        .sdk
        .accept_payment_request(payee.public_key.clone(), payee.receiver_path.clone(), &id)
        .await
        .unwrap();
    deliver(payer, payee).await;
    id
}

fn submission(
    allowance_id: &AllowanceId,
    period: Option<BillingPeriod>,
    evidence: &str,
) -> PaymentProofSubmission {
    PaymentProofSubmission {
        allowance_id: Some(allowance_id.clone()),
        billing_period: period,
        payment_endpoint_identifier: PaymentEndpointIdentifier::new("btc-lightning-bolt11")
            .unwrap(),
        proof: serde_json::json!({"test_evidence": evidence})
            .as_object()
            .unwrap()
            .clone(),
    }
}

async fn request_record(
    local: &TestUser,
    peer: &TestUser,
    request_id: &PaymentRequestId,
) -> PaymentRequestRecord {
    local
        .sdk
        .payment_requests_with(&peer.public_key, &peer.receiver_path)
        .await
        .unwrap()
        .into_iter()
        .find(|record| record.payment_request_id == request_id.as_str())
        .unwrap()
}

async fn restored_user(user: &TestUser) -> TestUser {
    let backup = user.sdk.export_backup_state().await.unwrap();
    let restored = user.restart_with_storage(InMemoryStorage::new()).await;
    let report = restored.sdk.restore_backup_state(backup).await.unwrap();
    assert!(report.recovery_required_peers.is_empty());
    restored
}

#[tokio::test]
async fn test_allowance_one_time_proof_attribution_survives_end_restore_and_correction() {
    let pair = linked_two_party().await;
    let allowance_id = accepted_allowance(&pair.alice, &pair.bob).await;
    let request_id = accepted_request(&pair.alice, &pair.bob, None).await;
    let first = pair
        .alice
        .sdk
        .submit_payment_proof_submission(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
            &request_id,
            submission(&allowance_id, None, "first"),
        )
        .await
        .unwrap();
    assert_eq!(first.state, PaymentRequestLifecycleState::ProofSubmitted);
    assert_eq!(
        first.payment_proofs[0].allowance_id.as_deref(),
        Some(allowance_id.as_str())
    );
    deliver(&pair.alice, &pair.bob).await;
    let received = request_record(&pair.bob, &pair.alice, &request_id).await;
    assert_eq!(
        received.payment_proofs[0].event_id,
        first.payment_proofs[0].event_id
    );
    assert_eq!(
        received.payment_proofs[0].allowance_id,
        first.payment_proofs[0].allowance_id
    );

    pair.alice
        .sdk
        .end_allowance(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
            &allowance_id,
        )
        .await
        .unwrap();
    deliver(&pair.alice, &pair.bob).await;
    let restored_bob = restored_user(&pair.bob).await;
    assert_eq!(
        request_record(&restored_bob, &pair.alice, &request_id)
            .await
            .payment_proofs,
        received.payment_proofs
    );

    // Historical evidence remains reportable after End. It does not reopen
    // authority, execute another payment, or consume additional Allowance usage.
    pair.alice
        .sdk
        .submit_payment_proof_submission(
            restored_bob.public_key.clone(),
            restored_bob.receiver_path.clone(),
            &request_id,
            submission(&allowance_id, None, "corrected"),
        )
        .await
        .unwrap();
    deliver(&pair.alice, &restored_bob).await;
    for (local, peer) in [(&pair.alice, &restored_bob), (&restored_bob, &pair.alice)] {
        let record = request_record(local, peer, &request_id).await;
        assert_eq!(record.state, PaymentRequestLifecycleState::ProofSubmitted);
        assert_eq!(record.payment_proofs.len(), 2);
        assert_ne!(
            record.payment_proofs[0].event_id,
            record.payment_proofs[1].event_id
        );
        assert!(record
            .payment_proofs
            .iter()
            .all(
                |proof| proof.allowance_id.as_deref() == Some(allowance_id.as_str())
                    && proof.billing_period.is_none()
            ));
        assert_eq!(record.payment_proofs[1].proof["test_evidence"], "corrected");
        let authority = allowance(local, peer, &allowance_id).await;
        assert_eq!(authority.state, AllowanceLifecycleState::Ended);
        assert_eq!(authority.history_status, AllowanceHistoryStatus::Consistent);
    }
}

#[tokio::test]
async fn test_allowance_recurring_proofs_retain_billing_periods_after_restore() {
    let pair = linked_two_party().await;
    let allowance_id = accepted_allowance(&pair.alice, &pair.bob).await;
    let first_period = BillingPeriod {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let second_period = BillingPeriod {
        starts_at: first_period.ends_at.clone(),
        ends_at: "2026-08-01T00:00:00Z".into(),
    };
    let request_id = accepted_request(
        &pair.alice,
        &pair.bob,
        Some(Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: first_period.starts_at.clone(),
            anchor: first_period.starts_at.clone(),
            ends_at: None,
        }),
    )
    .await;

    let invalid = pair
        .alice
        .sdk
        .submit_payment_proof_submission(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
            &request_id,
            submission(&allowance_id, None, "missing-period"),
        )
        .await;
    assert!(invalid.is_err());
    assert!(request_record(&pair.alice, &pair.bob, &request_id)
        .await
        .payment_proofs
        .is_empty());

    for (period, evidence) in [
        (&first_period, "first"),
        (&first_period, "corrected"),
        (&second_period, "second"),
    ] {
        pair.alice
            .sdk
            .submit_payment_proof_submission(
                pair.bob.public_key.clone(),
                pair.bob.receiver_path.clone(),
                &request_id,
                submission(&allowance_id, Some(period.clone()), evidence),
            )
            .await
            .unwrap();
        deliver(&pair.alice, &pair.bob).await;
    }
    let before = request_record(&pair.bob, &pair.alice, &request_id).await;
    let restored_bob = restored_user(&pair.bob).await;
    let after = request_record(&restored_bob, &pair.alice, &request_id).await;
    assert_eq!(after.state, PaymentRequestLifecycleState::ActiveRecurring);
    assert_eq!(after.payment_proofs, before.payment_proofs);
    assert_eq!(after.payment_proofs.len(), 3);
    assert!(after
        .payment_proofs
        .iter()
        .all(|proof| proof.allowance_id.as_deref() == Some(allowance_id.as_str())));
    assert_eq!(
        after.payment_proofs[0].billing_period,
        after.payment_proofs[1].billing_period
    );
    assert_ne!(
        after.payment_proofs[1].billing_period,
        after.payment_proofs[2].billing_period
    );
    assert_eq!(
        after.payment_proofs[2]
            .billing_period
            .as_ref()
            .unwrap()
            .starts_at,
        second_period.starts_at
    );
    assert_eq!(
        allowance(&restored_bob, &pair.alice, &allowance_id)
            .await
            .state,
        AllowanceLifecycleState::Accepted
    );
}
