use super::*;
use PaymentRequestLocalRole::{Payee, Payer};

const REQUEST_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
const PROPOSAL_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101";
const REJECTION_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102";
const CANCELLATION_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103";
const LATER_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104";
const REFERENCE: &str = "invoice-2026-0001";

async fn derive_history(
    local_role: PaymentRequestLocalRole,
    actions: Vec<(PaymentRequestLocalRole, String)>,
) -> PaymentRequestRecord {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let proposal = (
        Payee,
        request_raw(PROPOSAL_ID, REQUEST_ID, REFERENCE, None, None),
    );
    for (index, (sender, raw)) in std::iter::once(proposal).chain(actions).enumerate() {
        let recorded_at = timestamp() + ChronoDuration::seconds(index as i64);
        if sender == local_role {
            enqueue_untyped_private_message(
                &storage,
                peer.clone(),
                receiver_path(),
                raw,
                recorded_at,
            )
            .await
            .unwrap();
        } else {
            persist_messages_at(&storage, peer.clone(), vec![raw], recorded_at).await;
        }
    }
    let mut records = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    records.pop().unwrap()
}

fn crossing_actions() -> Vec<(PaymentRequestLocalRole, String)> {
    vec![
        (Payer, rejection_raw(REJECTION_ID, REQUEST_ID)),
        (Payee, cancellation_raw(CANCELLATION_ID, REQUEST_ID)),
    ]
}

#[tokio::test]
async fn test_crossing_rejection_cancellation_converges_for_both_peers_and_orders() {
    for local_role in [Payer, Payee] {
        for cancellation_first in [false, true] {
            let mut actions = crossing_actions();
            if cancellation_first {
                actions.reverse();
            }
            let record = derive_history(local_role, actions).await;

            assert_eq!(record.local_role, Some(local_role));
            assert_eq!(record.state, PaymentRequestLifecycleState::Canceled);
            assert_eq!(record.rejected_event_id.as_deref(), Some(REJECTION_ID));
            assert_eq!(record.canceled_event_id.as_deref(), Some(CANCELLATION_ID));
            assert_eq!(
                record.rejected_outbound_status.is_some(),
                local_role == Payer
            );
            assert_eq!(
                record.canceled_outbound_status.is_some(),
                local_role == Payee
            );
            assert!(record.accepted_event_id.is_none());
            assert!(record.payment_proofs.is_empty());
            assert!(record.invalid_reason.is_none());
        }
    }
}

#[tokio::test]
async fn test_crossing_rejection_cancellation_rejects_same_payer_direction() {
    for local_role in [Payer, Payee] {
        for cancellation_first in [false, true] {
            let mut actions = vec![
                (Payer, rejection_raw(REJECTION_ID, REQUEST_ID)),
                (Payer, cancellation_raw(CANCELLATION_ID, REQUEST_ID)),
            ];
            if cancellation_first {
                actions.reverse();
            }
            let record = derive_history(local_role, actions).await;

            assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
            assert_eq!(
                record.rejected_event_id.as_deref(),
                (!cancellation_first).then_some(REJECTION_ID)
            );
            assert_eq!(
                record.canceled_event_id.as_deref(),
                cancellation_first.then_some(CANCELLATION_ID)
            );
        }
    }
}

#[tokio::test]
async fn test_crossing_rejection_cancellation_rejects_later_payer_actions() {
    for local_role in [Payer, Payee] {
        for cancellation_first in [false, true] {
            for later_action in [
                acceptance_raw(LATER_ID, REQUEST_ID),
                proof_raw(LATER_ID, REQUEST_ID, REFERENCE),
                rejection_raw(LATER_ID, REQUEST_ID),
            ] {
                let mut actions = crossing_actions();
                if cancellation_first {
                    actions.reverse();
                }
                actions.push((Payer, later_action));
                let record = derive_history(local_role, actions).await;

                assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
                assert_eq!(record.rejected_event_id.as_deref(), Some(REJECTION_ID));
                assert_eq!(record.canceled_event_id.as_deref(), Some(CANCELLATION_ID));
                assert!(record.accepted_event_id.is_none());
                assert!(record.payment_proofs.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn test_crossing_rejection_cancellation_rejects_second_payee_cancellation() {
    for local_role in [Payer, Payee] {
        let mut actions = crossing_actions();
        actions.push((Payee, cancellation_raw(LATER_ID, REQUEST_ID)));
        let record = derive_history(local_role, actions).await;

        assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
        assert_eq!(record.rejected_event_id.as_deref(), Some(REJECTION_ID));
        assert_eq!(record.canceled_event_id.as_deref(), Some(CANCELLATION_ID));
    }
}

#[tokio::test]
async fn test_crossing_rejection_cancellation_replays_keep_original_evidence() {
    for local_role in [Payer, Payee] {
        let mut actions = crossing_actions();
        actions.extend(crossing_actions());
        let record = derive_history(local_role, actions).await;

        assert_eq!(record.state, PaymentRequestLifecycleState::Canceled);
        assert_eq!(record.rejected_event_id.as_deref(), Some(REJECTION_ID));
        assert_eq!(record.canceled_event_id.as_deref(), Some(CANCELLATION_ID));
        assert!(record.invalid_reason.is_none());
    }
}

#[tokio::test]
async fn test_crossing_rejection_cancellation_conflicting_event_id_cannot_replace_evidence() {
    for local_role in [Payer, Payee] {
        let mut actions = crossing_actions();
        actions.push((Payee, cancellation_raw(REJECTION_ID, REQUEST_ID)));
        let record = derive_history(local_role, actions).await;

        assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
        assert_ne!(record.canceled_event_id.as_deref(), Some(REJECTION_ID));
        assert!(record
            .invalid_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Event ID")));
    }
}

#[tokio::test]
async fn test_crossing_rejection_cancellation_rejects_rejection_after_acceptance() {
    for local_role in [Payer, Payee] {
        for cancellation_first in [false, true] {
            let acceptance = (Payer, acceptance_raw(LATER_ID, REQUEST_ID));
            let cancellation = (Payee, cancellation_raw(CANCELLATION_ID, REQUEST_ID));
            let mut actions = if cancellation_first {
                vec![cancellation, acceptance]
            } else {
                vec![acceptance, cancellation]
            };
            actions.push((Payer, rejection_raw(REJECTION_ID, REQUEST_ID)));
            let record = derive_history(local_role, actions).await;

            assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
            assert_eq!(record.accepted_event_id.as_deref(), Some(LATER_ID));
            assert_eq!(record.canceled_event_id.as_deref(), Some(CANCELLATION_ID));
            assert!(record.rejected_event_id.is_none());
        }
    }
}
