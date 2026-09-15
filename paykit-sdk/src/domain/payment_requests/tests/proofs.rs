use super::*;
use crate::domain::allowances::{
    allowance_records, AllowanceHistoryStatus, AllowanceLifecycleState,
};

const REQUEST_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
const ALLOWANCE_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";
const REFERENCE: &str = "invoice-2026-0001";
const FIRST_PROOF_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103";
const SECOND_PROOF_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104";

async fn outbound(storage: &InMemoryStorage, peer: &PubkyPublicKey, raw: String) {
    enqueue_untyped_private_message(storage, peer.clone(), receiver_path(), raw, timestamp())
        .await
        .unwrap();
}

async fn accepted_request(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    role: PaymentRequestLocalRole,
) {
    let proposal = request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        REQUEST_ID,
        REFERENCE,
        None,
        None,
    );
    let acceptance = acceptance_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102", REQUEST_ID);
    match role {
        PaymentRequestLocalRole::Payer => {
            persist_messages(storage, peer.clone(), vec![proposal]).await;
            outbound(storage, peer, acceptance).await;
        }
        PaymentRequestLocalRole::Payee => {
            outbound(storage, peer, proposal).await;
            persist_messages(storage, peer.clone(), vec![acceptance]).await;
        }
    }
}

fn attributed_proof(event_id: &str, allowance_id: &str) -> String {
    let PaymentRequestEvent::Proof(proof) =
        parsed_event(proof_raw(event_id, REQUEST_ID, REFERENCE))
    else {
        panic!("expected proof");
    };
    serialize_payment_request_event(&PaymentRequestEvent::Proof(
        proof.with_allowance_id(AllowanceId::new(allowance_id).unwrap()),
    ))
    .unwrap()
}

async fn record(storage: &InMemoryStorage, peer: &PubkyPublicKey) -> PaymentRequestRecord {
    payment_request_records(storage, peer, &receiver_path(), timestamp())
        .await
        .unwrap()
        .remove(0)
}

#[tokio::test]
async fn test_corrective_payment_proofs_preserve_inbound_and_outbound_evidence() {
    for role in [
        PaymentRequestLocalRole::Payer,
        PaymentRequestLocalRole::Payee,
    ] {
        let storage = InMemoryStorage::new();
        let peer = counterparty();
        accepted_request(&storage, &peer, role).await;
        let first = attributed_proof(FIRST_PROOF_ID, ALLOWANCE_ID);
        let mut second: serde_json::Value =
            serde_json::from_str(&attributed_proof(SECOND_PROOF_ID, ALLOWANCE_ID)).unwrap();
        second["proof"] = serde_json::json!({"txid": "corrected-private-proof"});
        for raw in [first, second.to_string()] {
            match role {
                PaymentRequestLocalRole::Payer => outbound(&storage, &peer, raw).await,
                PaymentRequestLocalRole::Payee => {
                    persist_messages(&storage, peer.clone(), vec![raw]).await
                }
            }
        }

        let result = record(&storage, &peer).await;

        assert_eq!(result.state, PaymentRequestLifecycleState::ProofSubmitted);
        assert_eq!(result.payment_proofs.len(), 2);
        assert!(result.invalid_reason.is_none());
        assert!(result
            .payment_proofs
            .iter()
            .all(|proof| proof.allowance_id.as_deref() == Some(ALLOWANCE_ID)));
        assert_eq!(
            result.payment_proofs[1].proof["txid"],
            "corrected-private-proof"
        );
        assert!(!format!("{result:?}").contains("corrected-private-proof"));
    }
}

#[tokio::test]
async fn test_replayed_payment_proof_is_not_another_proof_record() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    accepted_request(&storage, &peer, PaymentRequestLocalRole::Payee).await;
    let proof = attributed_proof(FIRST_PROOF_ID, ALLOWANCE_ID);
    persist_messages(&storage, peer.clone(), vec![proof.clone(), proof]).await;

    let result = record(&storage, &peer).await;

    assert_eq!(result.state, PaymentRequestLifecycleState::ProofSubmitted);
    assert_eq!(result.payment_proofs.len(), 1);
    assert_eq!(
        result.payment_proofs[0].allowance_id.as_deref(),
        Some(ALLOWANCE_ID)
    );
}

#[tokio::test]
async fn test_payment_proof_reused_event_id_with_changed_attribution_fails_closed() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    accepted_request(&storage, &peer, PaymentRequestLocalRole::Payee).await;
    persist_messages(
        &storage,
        peer.clone(),
        vec![
            attributed_proof(FIRST_PROOF_ID, ALLOWANCE_ID),
            attributed_proof(FIRST_PROOF_ID, REQUEST_ID),
        ],
    )
    .await;

    let result = record(&storage, &peer).await;

    assert_eq!(result.state, PaymentRequestLifecycleState::InvalidConflict);
    assert!(result.invalid_reason.is_some());
}

#[tokio::test]
async fn test_different_payment_proof_attributions_remain_informational_claims() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    accepted_request(&storage, &peer, PaymentRequestLocalRole::Payee).await;
    persist_messages(
        &storage,
        peer.clone(),
        vec![
            attributed_proof(FIRST_PROOF_ID, ALLOWANCE_ID),
            attributed_proof(SECOND_PROOF_ID, REQUEST_ID),
        ],
    )
    .await;

    let result = record(&storage, &peer).await;

    assert_eq!(result.state, PaymentRequestLifecycleState::ProofSubmitted);
    assert_eq!(result.payment_proofs.len(), 2);
    assert_eq!(
        result.payment_proofs[0].allowance_id.as_deref(),
        Some(ALLOWANCE_ID)
    );
    assert_eq!(
        result.payment_proofs[1].allowance_id.as_deref(),
        Some(REQUEST_ID)
    );
    assert!(allowance_records(&storage, &peer, &receiver_path())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_payment_proof_can_report_ended_allowance_without_restoring_authority() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    accepted_request(&storage, &peer, PaymentRequestLocalRole::Payee).await;
    persist_messages(
        &storage,
        peer.clone(),
        vec![allowance_proposal_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
        )],
    )
    .await;
    outbound(
        &storage,
        &peer,
        crate::test_utils::allowance_event_json(
            "paykit.allowance_acceptance",
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202",
        ),
    )
    .await;
    persist_messages(
        &storage,
        peer.clone(),
        vec![crate::test_utils::allowance_event_json(
            "paykit.allowance_end",
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204",
        )],
    )
    .await;
    let before = allowance_records(&storage, &peer, &receiver_path())
        .await
        .unwrap();
    assert_eq!(before[0].state, AllowanceLifecycleState::Ended);
    assert_eq!(before[0].history_status, AllowanceHistoryStatus::Consistent);

    persist_messages(
        &storage,
        peer.clone(),
        vec![attributed_proof(FIRST_PROOF_ID, ALLOWANCE_ID)],
    )
    .await;

    assert_eq!(
        record(&storage, &peer).await.payment_proofs[0]
            .allowance_id
            .as_deref(),
        Some(ALLOWANCE_ID)
    );
    assert_eq!(
        allowance_records(&storage, &peer, &receiver_path())
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn test_proof_submitted_requires_retained_acceptance_for_another_proof() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    accepted_request(&storage, &peer, PaymentRequestLocalRole::Payee).await;
    persist_messages(
        &storage,
        peer.clone(),
        vec![proof_raw(FIRST_PROOF_ID, REQUEST_ID, REFERENCE)],
    )
    .await;
    let mut result = record(&storage, &peer).await;
    assert!(payment_proof_allowed_states(&result)
        .contains(&PaymentRequestLifecycleState::ProofSubmitted));

    result.accepted_event_id = None;

    assert!(!payment_proof_allowed_states(&result)
        .contains(&PaymentRequestLifecycleState::ProofSubmitted));
}

#[tokio::test]
async fn test_payment_proof_record_omits_absent_attribution_and_redacts_submission() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    accepted_request(&storage, &peer, PaymentRequestLocalRole::Payee).await;
    persist_messages(
        &storage,
        peer.clone(),
        vec![proof_raw(FIRST_PROOF_ID, REQUEST_ID, REFERENCE)],
    )
    .await;
    let proof = record(&storage, &peer).await.payment_proofs.remove(0);
    let serialized = serde_json::to_value(&proof).unwrap();

    assert!(serialized.get("allowance_id").is_none());
    assert_eq!(
        serde_json::from_value::<PaymentProofRecord>(serialized).unwrap(),
        proof
    );
    let submission = PaymentProofSubmission {
        billing_period: None,
        payment_endpoint_identifier: PaymentEndpointIdentifier::new("btc-lightning-bolt11")
            .unwrap(),
        allowance_id: Some(AllowanceId::new(ALLOWANCE_ID).unwrap()),
        proof: serde_json::json!({"preimage":"private-submission-secret"})
            .as_object()
            .unwrap()
            .clone(),
    };
    assert!(!format!("{submission:?}").contains("private-submission-secret"));
}
