use chrono::{Duration as ChronoDuration, TimeZone};
use paykit_lib::{
    parse_allowance_event_message, serialize_allowance_event, serialize_payment_request_event,
    AllowanceAcceptance, AllowanceAmountRange, AllowanceEnd, AllowanceEvent, AllowanceId,
    AllowancePeriod, AllowancePeriodLimit, AllowancePeriodUnit, AllowanceProposal,
    AllowanceRejection, AllowanceRole, AllowanceTerms, EventId, PaymentEndpointIdentifier,
    PaymentRequestAcceptance, PaymentRequestEvent, PaymentRequestId, PrivateApplicationMessage,
};

use super::*;
use crate::{
    domain::{
        linked_peers::{default_linked_peer, LinkedPeerState},
        outbound_private::{enqueue_private_message, mark_outbound_recovery_required},
        payment_requests::{payment_request_records, PaymentRequestLifecycleState},
        private_stream::{persist_private_stream_batch, PrivateStreamParseStatus},
    },
    storage::{EncryptedLinkStateRecord, InMemoryStorage, LinkedPeerRecord, StorageAdapter},
    test_utils::allowance_application_message,
    PaykitSdkError,
};

const ALLOWANCE_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";
const PROPOSAL_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201";
const ACCEPTANCE_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202";
const END_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d203";

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn event_id(value: &str) -> EventId {
    EventId::new(value).unwrap()
}

fn allowance_id() -> AllowanceId {
    AllowanceId::new(ALLOWANCE_ID).unwrap()
}

fn terms() -> AllowanceTerms {
    AllowanceTerms::builder("private-asset-sentinel")
        .lifetime_amount_limit("10")
        .build()
        .unwrap()
}

fn full_terms() -> AllowanceTerms {
    AllowanceTerms::builder("btc")
        .per_payment_amount(AllowanceAmountRange::new("0.1", "1.00").unwrap())
        .period_limits(vec![AllowancePeriodLimit::new(
            Some("3.0".into()),
            Some(5),
            AllowancePeriod::anchored(1, AllowancePeriodUnit::Month, "2026-01-31T00:00:00Z")
                .unwrap(),
        )
        .unwrap()])
        .lifetime_amount_limit("10.0")
        .active_from("2026-06-01T00:00:00Z")
        .expires_at("2027-06-01T00:00:00Z")
        .allowed_payment_endpoint_identifiers(vec![PaymentEndpointIdentifier::new(
            "btc-lightning-bolt12",
        )
        .unwrap()])
        .build()
        .unwrap()
}

fn proposal(event_id_value: &str, role: AllowanceRole) -> AllowanceEvent {
    AllowanceEvent::Proposal(AllowanceProposal::new(
        event_id(event_id_value),
        allowance_id(),
        role,
        terms(),
    ))
}

fn acceptance(proposal_event_id: &str) -> AllowanceEvent {
    AllowanceEvent::Acceptance(
        AllowanceAcceptance::new(
            event_id(ACCEPTANCE_ID),
            allowance_id(),
            event_id(proposal_event_id),
        )
        .unwrap(),
    )
}

fn withdrawal() -> AllowanceEvent {
    AllowanceEvent::End(
        AllowanceEnd::withdrawal(event_id(END_ID), allowance_id(), event_id(PROPOSAL_ID)).unwrap(),
    )
}

fn accepted_end(end_event_id: &str, acceptance_event_id: &str) -> AllowanceEvent {
    AllowanceEvent::End(
        AllowanceEnd::accepted(
            event_id(end_event_id),
            allowance_id(),
            event_id(PROPOSAL_ID),
            event_id(acceptance_event_id),
        )
        .unwrap(),
    )
}

fn linked_peer(peer: PubkyPublicKey, state: LinkedPeerState) -> LinkedPeerRecord {
    let mut record = default_linked_peer(peer);
    record.state = state;
    record
}

async fn derived(storage: &InMemoryStorage, peer: &PubkyPublicKey) -> AllowanceRecord {
    allowance_record(storage, peer, &allowance_id())
        .await
        .unwrap()
        .unwrap()
}

async fn enqueue_allowance_acceptance(
    storage: &InMemoryStorage,
    peer: PubkyPublicKey,
    app_id: paykit_lib::PaykitAppId,
    allowance_id: AllowanceId,
    now: DateTime<Utc>,
) -> crate::Result<AllowanceRecord> {
    // Accept whichever proposal is current; a missing one still reaches the
    // command's own checks. The expected-proposal check has its own tests.
    let expected_proposal_stream_item_id = allowance_record(storage, &peer, &allowance_id)
        .await
        .ok()
        .flatten()
        .and_then(|record| record.proposal_stream_item_id)
        .unwrap_or_default();
    enqueue_allowance_response(
        storage,
        peer,
        app_id,
        allowance_id,
        AllowanceResponse::Acceptance {
            expected_proposal_stream_item_id,
        },
        now,
    )
    .await
}

async fn enqueue_allowance_rejection(
    storage: &InMemoryStorage,
    peer: PubkyPublicKey,
    app_id: paykit_lib::PaykitAppId,
    allowance_id: AllowanceId,
    now: DateTime<Utc>,
) -> crate::Result<AllowanceRecord> {
    enqueue_allowance_response(
        storage,
        peer,
        app_id,
        allowance_id,
        AllowanceResponse::Rejection,
        now,
    )
    .await
}

fn message(event: &AllowanceEvent) -> PrivateApplicationMessage {
    allowance_application_message(event)
}

async fn persist_inbound(
    storage: &InMemoryStorage,
    peer: PubkyPublicKey,
    events: Vec<AllowanceEvent>,
    received_at: DateTime<Utc>,
) {
    persist_private_stream_batch(
        storage,
        peer,
        events.iter().map(message).collect(),
        None,
        received_at,
    )
    .await
    .unwrap();
}

async fn queue_outbound(
    storage: &InMemoryStorage,
    peer: PubkyPublicKey,
    event: AllowanceEvent,
    created_at: DateTime<Utc>,
) {
    enqueue_private_message(
        storage,
        peer,
        serialize_allowance_event(&paykit_lib::PaykitAppId::new("bitkit").unwrap(), &event)
            .unwrap(),
        created_at,
    )
    .await
    .unwrap();
}

async fn seed_active_link(storage: &InMemoryStorage, peer: PubkyPublicKey) {
    storage
        .transaction(move |tx| {
            tx.save_linked_peer(linked_peer(peer.clone(), LinkedPeerState::Linked));
            tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                counterparty: peer,
                link_snapshot: Some(vec![1, 2, 3]),
                handshake_snapshot: None,
                handshake_role: None,
                generation: 1,
                checkpointed_at: timestamp(),
            });
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_allowance_records_derive_local_roles_from_authenticated_source() {
    let storage = registered_storage();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let second_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab45";
    let outbound = AllowanceEvent::Proposal(AllowanceProposal::new(
        event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d211"),
        AllowanceId::new(second_id).unwrap(),
        AllowanceRole::Allowee,
        terms(),
    ));
    queue_outbound(&storage, peer.clone(), outbound, timestamp()).await;

    let records = allowance_records(&storage, &peer).await.unwrap();

    assert_eq!(records.len(), 2);
    assert_eq!(
        records
            .iter()
            .find(|record| record.allowance_id == ALLOWANCE_ID)
            .unwrap()
            .local_role,
        Some(AllowanceLocalRole::Allowee)
    );
    assert_eq!(
        records
            .iter()
            .find(|record| record.allowance_id == second_id)
            .unwrap()
            .local_role,
        Some(AllowanceLocalRole::Allowee)
    );
}

#[tokio::test]
async fn test_allowance_derivation_end_wins_over_crossing_acceptance_without_clock_order() {
    let storage = registered_storage();
    let peer = counterparty();
    queue_outbound(
        &storage,
        peer.clone(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp() + ChronoDuration::hours(2),
    )
    .await;
    persist_inbound(
        &storage,
        peer.clone(),
        vec![acceptance(PROPOSAL_ID)],
        timestamp(),
    )
    .await;
    queue_outbound(
        &storage,
        peer.clone(),
        withdrawal(),
        timestamp() - ChronoDuration::hours(2),
    )
    .await;

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Ended);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
    assert_eq!(record.acceptance_event_id.as_deref(), Some(ACCEPTANCE_ID));
    assert_eq!(record.end_event_id.as_deref(), Some(END_ID));
}

#[tokio::test]
async fn test_allowance_derivation_retains_end_by_source_fifo_not_uuid() {
    let storage = registered_storage();
    let peer = counterparty();
    let inbound_end_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d209";
    queue_outbound(
        &storage,
        peer.clone(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp() + ChronoDuration::hours(2),
    )
    .await;
    persist_inbound(
        &storage,
        peer.clone(),
        vec![
            acceptance(PROPOSAL_ID),
            accepted_end(inbound_end_id, ACCEPTANCE_ID),
        ],
        timestamp(),
    )
    .await;
    queue_outbound(
        &storage,
        peer.clone(),
        withdrawal(),
        timestamp() - ChronoDuration::hours(2),
    )
    .await;

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Ended);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
    assert_eq!(record.end_event_id.as_deref(), Some(inbound_end_id));
}

#[tokio::test]
async fn test_allowance_derivation_separates_unresolved_history_from_lifecycle() {
    let storage = registered_storage();
    let peer = counterparty();
    queue_outbound(
        &storage,
        peer.clone(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;
    let missing_acceptance_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d299";
    persist_inbound(
        &storage,
        peer.clone(),
        vec![accepted_end(END_ID, missing_acceptance_id)],
        timestamp(),
    )
    .await;

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(
        record.history_status,
        AllowanceHistoryStatus::UnresolvedReferences
    );
    assert_eq!(record.pending_causal_event_ids, [missing_acceptance_id]);
}

#[tokio::test]
async fn test_allowance_derivation_detects_proposal_and_cross_kind_event_conflicts() {
    let storage = registered_storage();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    queue_outbound(
        &storage,
        peer.clone(),
        proposal(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d211",
            AllowanceRole::Allowee,
        ),
        timestamp(),
    )
    .await;

    let conflicted = derived(&storage, &peer).await;
    assert_eq!(conflicted.state, AllowanceLifecycleState::Conflicted);

    let second_peer = counterparty();
    let payment_event = PaymentRequestEvent::Acceptance(PaymentRequestAcceptance::new(
        event_id(PROPOSAL_ID),
        PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab55").unwrap(),
    ));
    let payment_raw = serialize_payment_request_event(
        &paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        &payment_event,
    )
    .unwrap();
    persist_private_stream_batch(
        &storage,
        second_peer.clone(),
        vec![PrivateApplicationMessage {
            app_id: Some("bitkit".into()),

            version: Some(1),
            kind: Some(payment_event.kind().as_str().to_owned()),
            raw_json: payment_raw,
        }],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    queue_outbound(
        &storage,
        second_peer.clone(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;
    let cross_kind = derived(&storage, &second_peer).await;
    assert_eq!(cross_kind.state, AllowanceLifecycleState::Proposed);
    assert_eq!(cross_kind.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(cross_kind.conflict_event_ids, [PROPOSAL_ID]);
}

#[tokio::test]
async fn test_allowance_derivation_distinguishes_known_wrong_and_missing_causal_ids() {
    let storage = registered_storage();
    let known_wrong_peer = counterparty();
    persist_inbound(
        &storage,
        known_wrong_peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let unrelated = PaymentRequestEvent::Acceptance(PaymentRequestAcceptance::new(
        event_id(END_ID),
        PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab55").unwrap(),
    ));
    enqueue_private_message(
        &storage,
        known_wrong_peer.clone(),
        serialize_payment_request_event(
            &paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            &unrelated,
        )
        .unwrap(),
        timestamp(),
    )
    .await
    .unwrap();
    queue_outbound(
        &storage,
        known_wrong_peer.clone(),
        acceptance(END_ID),
        timestamp(),
    )
    .await;

    let missing_peer = counterparty();
    persist_inbound(
        &storage,
        missing_peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let missing_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d299";
    queue_outbound(
        &storage,
        missing_peer.clone(),
        acceptance(missing_id),
        timestamp(),
    )
    .await;

    let known_wrong = derived(&storage, &known_wrong_peer).await;
    let missing = derived(&storage, &missing_peer).await;

    assert_eq!(known_wrong.history_status, AllowanceHistoryStatus::Invalid);
    assert!(known_wrong.pending_causal_event_ids.is_empty());
    assert_eq!(
        missing.history_status,
        AllowanceHistoryStatus::UnresolvedReferences
    );
    assert_eq!(missing.pending_causal_event_ids, [missing_id]);
}

#[tokio::test]
async fn test_allowance_derivation_conflicts_proposals_reusing_one_event_id() {
    let storage = registered_storage();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        vec![
            proposal(PROPOSAL_ID, AllowanceRole::Allower),
            proposal(PROPOSAL_ID, AllowanceRole::Allowee),
        ],
        timestamp(),
    )
    .await;

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Conflicted);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(record.conflict_event_ids, [PROPOSAL_ID]);
}

#[tokio::test]
async fn test_allowance_derivation_scopes_ids_to_exact_link_and_marks_recovery() {
    let storage = registered_storage();
    let peer = counterparty();
    let other_peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    persist_inbound(
        &storage,
        other_peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allowee)],
        timestamp(),
    )
    .await;
    storage
        .transaction({
            let peer = peer.clone();
            move |tx| {
                tx.save_linked_peer(linked_peer(peer, LinkedPeerState::RecoveryRequired));
                Ok(())
            }
        })
        .await
        .unwrap();

    let wallet = derived(&storage, &peer).await;
    let server = derived(&storage, &other_peer).await;

    assert_eq!(
        wallet.history_status,
        AllowanceHistoryStatus::RecoveryRequired
    );
    assert_eq!(server.history_status, AllowanceHistoryStatus::Consistent);
    assert_eq!(server.local_role, Some(AllowanceLocalRole::Allower));
}

#[tokio::test]
async fn test_allowance_acceptance_precondition_and_append_are_atomic() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;

    let before = storage.snapshot().unwrap().outbound_private_messages;

    let first = enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    );
    let second = enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    );
    let (first, second) = tokio::join!(first, second);

    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(first
        .as_ref()
        .err()
        .or_else(|| second.as_ref().err())
        .is_some_and(|error| matches!(error, PaykitSdkError::Policy { .. })));
    let after = storage.snapshot().unwrap().outbound_private_messages;
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(&after[..before.len()], before.as_slice());
    assert_eq!(after.last().unwrap().kind, "paykit.allowance_acceptance");
}

#[tokio::test]
async fn test_allowance_acceptance_requires_expected_proposal_without_queue_mutation() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let proposal_stream_item_id = derived(&storage, &peer)
        .await
        .proposal_stream_item_id
        .unwrap();
    let accept = |expected_proposal_stream_item_id: u64| {
        enqueue_allowance_response(
            &storage,
            peer.clone(),
            paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            allowance_id(),
            AllowanceResponse::Acceptance {
                expected_proposal_stream_item_id,
            },
            timestamp(),
        )
    };
    let before = storage.snapshot().unwrap().outbound_private_messages;

    let refused = accept(proposal_stream_item_id + 1).await;

    assert!(matches!(refused, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        before
    );
    assert_eq!(
        derived(&storage, &peer).await.state,
        AllowanceLifecycleState::Proposed
    );

    let accepted = accept(proposal_stream_item_id).await.unwrap();

    assert_eq!(accepted.state, AllowanceLifecycleState::Accepted);
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        before.len() + 1
    );
}

#[tokio::test]
async fn test_allowance_proposal_requires_complete_link_without_queue_mutation() {
    let storage = registered_storage();
    let peer = counterparty();

    let result = enqueue_allowance_proposal(
        &storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        AllowanceLocalRole::Allower,
        terms(),
        timestamp(),
    )
    .await;

    assert!(matches!(
        result,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());

    seed_active_link(&storage, peer.clone()).await;
    let proposed = enqueue_allowance_proposal(
        &storage,
        peer,
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        AllowanceLocalRole::Allower,
        terms(),
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(proposed.state, AllowanceLifecycleState::Proposed);
    assert_eq!(proposed.history_status, AllowanceHistoryStatus::Consistent);
    assert_eq!(proposed.local_role, Some(AllowanceLocalRole::Allower));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        1
    );
}

#[tokio::test]
async fn test_allowance_rejection_blocks_later_response_without_queue_mutation() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;

    let rejected = enqueue_allowance_rejection(
        &storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();
    let before = storage.snapshot().unwrap().outbound_private_messages;

    let later = enqueue_allowance_acceptance(
        &storage,
        peer,
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await;

    assert_eq!(rejected.state, AllowanceLifecycleState::Rejected);
    assert!(matches!(later, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        before
    );
}

#[tokio::test]
async fn test_allowance_end_after_rejection_allows_proposer_withdrawal() {
    for role in [AllowanceRole::Allower, AllowanceRole::Allowee] {
        let storage = registered_storage();
        let peer = counterparty();
        seed_active_link(&storage, peer.clone()).await;
        queue_outbound(
            &storage,
            peer.clone(),
            proposal(PROPOSAL_ID, role),
            timestamp(),
        )
        .await;
        persist_inbound(
            &storage,
            peer.clone(),
            vec![AllowanceEvent::Rejection(
                AllowanceRejection::new(
                    event_id(ACCEPTANCE_ID),
                    allowance_id(),
                    event_id(PROPOSAL_ID),
                )
                .unwrap(),
            )],
            timestamp(),
        )
        .await;
        assert_eq!(
            derived(&storage, &peer).await.state,
            AllowanceLifecycleState::Rejected
        );

        let before = storage.snapshot().unwrap().outbound_private_messages;

        let ended = enqueue_allowance_end(
            &storage,
            peer.clone(),
            paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            allowance_id(),
            timestamp(),
        )
        .await
        .unwrap();

        assert_eq!(ended.state, AllowanceLifecycleState::Ended);
        assert_eq!(ended.history_status, AllowanceHistoryStatus::Consistent);
        let state = storage.snapshot().unwrap();
        assert_eq!(state.outbound_private_messages.len(), before.len() + 1);
        assert_eq!(
            &state.outbound_private_messages[..before.len()],
            before.as_slice()
        );
        let end_message = state.outbound_private_messages.last().unwrap();
        let parsed = parse_allowance_event_message(&PrivateApplicationMessage {
            app_id: Some("bitkit".into()),

            version: Some(1),
            kind: Some(end_message.kind.clone()),
            raw_json: end_message.raw_json.clone(),
        })
        .unwrap();
        let AllowanceEvent::End(end_event) = parsed.parsed_event().unwrap() else {
            panic!("last outbound message must be an Allowance End")
        };
        assert_eq!(end_event.proposal_event_id().as_str(), PROPOSAL_ID);
        assert!(end_event.acceptance_event_id().is_none());
        assert_eq!(
            derived(&storage, &peer).await.state,
            AllowanceLifecycleState::Ended
        );
    }
}

#[tokio::test]
async fn test_allowance_end_after_rejection_rejects_recipient_without_queue_mutation() {
    for role in [AllowanceRole::Allower, AllowanceRole::Allowee] {
        let storage = registered_storage();
        let peer = counterparty();
        seed_active_link(&storage, peer.clone()).await;
        persist_inbound(
            &storage,
            peer.clone(),
            vec![proposal(PROPOSAL_ID, role)],
            timestamp(),
        )
        .await;
        let rejected = enqueue_allowance_rejection(
            &storage,
            peer.clone(),
            paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            allowance_id(),
            timestamp(),
        )
        .await
        .unwrap();
        assert_eq!(rejected.state, AllowanceLifecycleState::Rejected);

        let before = storage.snapshot().unwrap().outbound_private_messages;

        let result = enqueue_allowance_end(
            &storage,
            peer.clone(),
            paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            allowance_id(),
            timestamp(),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
        assert_eq!(
            storage.snapshot().unwrap().outbound_private_messages,
            before
        );
        let record = derived(&storage, &peer).await;
        assert_eq!(record.state, AllowanceLifecycleState::Rejected);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
        assert!(record.end_event_id.is_none());
    }
}

#[tokio::test]
async fn test_allowance_end_for_accepted_authority_references_exact_acceptance() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let accepted = enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();

    let ended = enqueue_allowance_end(
        &storage,
        peer,
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();
    let state = storage.snapshot().unwrap();
    let end_message = state.outbound_private_messages.last().unwrap();
    let parsed = parse_allowance_event_message(&PrivateApplicationMessage {
        app_id: Some("bitkit".into()),

        version: Some(1),
        kind: Some(end_message.kind.clone()),
        raw_json: end_message.raw_json.clone(),
    })
    .unwrap();
    let AllowanceEvent::End(end_event) = parsed.parsed_event().unwrap() else {
        panic!("last outbound message must be an Allowance End")
    };

    assert_eq!(ended.state, AllowanceLifecycleState::Ended);
    assert_eq!(
        end_event.acceptance_event_id().map(EventId::as_str),
        accepted.acceptance_event_id.as_deref()
    );
}

#[tokio::test]
async fn test_allowance_first_response_fifo_controls_and_later_response_invalidates_history() {
    let storage = registered_storage();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    queue_outbound(&storage, peer.clone(), acceptance(PROPOSAL_ID), timestamp()).await;
    queue_outbound(
        &storage,
        peer.clone(),
        AllowanceEvent::Rejection(
            AllowanceRejection::new(
                event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204"),
                allowance_id(),
                event_id(PROPOSAL_ID),
            )
            .unwrap(),
        ),
        timestamp() - ChronoDuration::hours(1),
    )
    .await;

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Accepted);
    assert_eq!(record.acceptance_event_id.as_deref(), Some(ACCEPTANCE_ID));
    assert!(record.rejection_event_id.is_none());
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
}

#[tokio::test]
async fn test_allowance_end_command_succeeds_on_invalid_history() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    queue_outbound(&storage, peer.clone(), acceptance(PROPOSAL_ID), timestamp()).await;
    queue_outbound(
        &storage,
        peer.clone(),
        AllowanceEvent::Rejection(
            AllowanceRejection::new(
                event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204"),
                allowance_id(),
                event_id(PROPOSAL_ID),
            )
            .unwrap(),
        ),
        timestamp(),
    )
    .await;
    assert_eq!(
        derived(&storage, &peer).await.history_status,
        AllowanceHistoryStatus::Invalid
    );

    let before = storage.snapshot().unwrap().outbound_private_messages;

    let ended = enqueue_allowance_end(
        &storage,
        peer,
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();
    let state = storage.snapshot().unwrap();
    let end_message = state.outbound_private_messages.last().unwrap();
    let parsed = parse_allowance_event_message(&PrivateApplicationMessage {
        app_id: Some("bitkit".into()),

        version: Some(1),
        kind: Some(end_message.kind.clone()),
        raw_json: end_message.raw_json.clone(),
    })
    .unwrap();
    let AllowanceEvent::End(end_event) = parsed.parsed_event().unwrap() else {
        panic!("last outbound message must be an Allowance End")
    };

    assert_eq!(ended.state, AllowanceLifecycleState::Ended);
    assert_eq!(ended.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(
        end_event.acceptance_event_id().map(EventId::as_str),
        Some(ACCEPTANCE_ID)
    );
    assert_eq!(state.outbound_private_messages.len(), before.len() + 1);
    assert_eq!(
        &state.outbound_private_messages[..before.len()],
        before.as_slice()
    );
}

#[tokio::test]
async fn test_allowance_end_command_blocked_when_history_needs_recovery() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    queue_outbound(
        &storage,
        peer.clone(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;
    storage
        .transaction({
            let peer = peer.clone();
            move |tx| {
                for message in tx.outbound_private_messages(&peer) {
                    tx.save_outbound_private_message(mark_outbound_recovery_required(
                        message,
                        "link reset".into(),
                        timestamp(),
                    ))?;
                }
                Ok(())
            }
        })
        .await
        .unwrap();

    let before = storage.snapshot().unwrap().outbound_private_messages;

    let result = enqueue_allowance_end(
        &storage,
        peer,
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await;

    assert!(matches!(
        result,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        before
    );
}

#[tokio::test]
async fn test_accepted_allowance_view_coexists_with_ordinary_payment_request() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    let proposal = AllowanceEvent::Proposal(AllowanceProposal::new(
        event_id(PROPOSAL_ID),
        allowance_id(),
        AllowanceRole::Allowee,
        full_terms(),
    ));
    let payment_request_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_request = PrivateApplicationMessage {
        app_id: Some("bitkit".into()),

        version: Some(1),
        kind: Some("paykit.payment_request".into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.payment_request","app_id":"bitkit","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"{payment_request_id}","request":{{"amount":{{"value":"0.5","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt12"],"required_app_id":null,"metadata":{{}}}}}}"#
        ),
    };
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        vec![message(&proposal), payment_request],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();

    let allowance = derived(&storage, &peer).await;
    let filter = AllowanceFilter {
        counterparty: Some(peer.clone()),
        local_role: Some(AllowanceLocalRole::Allower),
        states: vec![AllowanceLifecycleState::Accepted],
    };
    let requests = payment_request_records(&storage, &peer, timestamp())
        .await
        .unwrap();

    assert!(filter.matches(&allowance));
    assert_eq!(allowance.state, AllowanceLifecycleState::Accepted);
    assert_eq!(allowance.history_status, AllowanceHistoryStatus::Consistent);
    let projected_terms = allowance.terms.as_ref().unwrap();
    assert_eq!(projected_terms.asset, "btc");
    assert_eq!(
        projected_terms
            .per_payment_amount
            .as_ref()
            .map(|range| (range.minimum.as_str(), range.maximum.as_str())),
        Some(("0.1", "1.00"))
    );
    assert_eq!(projected_terms.period_limits.len(), 1);
    assert_eq!(
        projected_terms.period_limits[0].amount_limit.as_deref(),
        Some("3.0")
    );
    assert_eq!(
        projected_terms.period_limits[0].payment_count_limit,
        Some(5)
    );
    assert_eq!(
        projected_terms.lifetime_amount_limit.as_deref(),
        Some("10.0")
    );
    assert_eq!(
        projected_terms.allowed_payment_endpoint_identifiers,
        Some(vec!["btc-lightning-bolt12".into()])
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].payment_request_id, payment_request_id);
    assert_eq!(requests[0].state, PaymentRequestLifecycleState::Proposed);
    assert!(serde_json::to_value(&requests[0])
        .unwrap()
        .get("allowance_id")
        .is_none());
}

#[tokio::test]
async fn test_allowance_commands_reject_wrong_party_without_queue_mutation() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    queue_outbound(
        &storage,
        peer.clone(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;

    let before = storage.snapshot().unwrap().outbound_private_messages;

    let result = enqueue_allowance_acceptance(
        &storage,
        peer,
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        before
    );
}

#[tokio::test]
async fn test_allowance_derivation_deduplicates_exact_same_sender_retry() {
    let storage = registered_storage();
    let peer = counterparty();
    let proposal = proposal(PROPOSAL_ID, AllowanceRole::Allower);
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal.clone(), proposal],
        timestamp(),
    )
    .await;

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
    assert!(record.conflict_event_ids.is_empty());
}

#[tokio::test]
async fn test_allowance_derivation_preserves_state_when_recognized_event_is_malformed() {
    let storage = registered_storage();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let acceptance = acceptance(PROPOSAL_ID);
    let malformed = serialize_allowance_event(
        &paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        &acceptance,
    )
    .unwrap()
    .replacen('}', r#", "private_sentinel": true}"#, 1);
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        vec![PrivateApplicationMessage {
            app_id: Some("bitkit".into()),

            version: Some(1),
            kind: Some(acceptance.kind().as_str().to_owned()),
            raw_json: malformed,
        }],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
    assert!(!format!("{record:?}").contains("private_sentinel"));
}

#[tokio::test]
async fn test_allowance_response_rejects_invalid_history_without_queue_mutation() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let acceptance = acceptance(PROPOSAL_ID);
    let malformed = serialize_allowance_event(
        &paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        &acceptance,
    )
    .unwrap()
    .replacen('}', r#", "private_sentinel": true}"#, 1);
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        vec![PrivateApplicationMessage {
            app_id: Some("bitkit".into()),

            version: Some(1),
            kind: Some(acceptance.kind().as_str().to_owned()),
            raw_json: malformed,
        }],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let before = storage.snapshot().unwrap().outbound_private_messages;

    let accepted = enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await;
    let rejected = enqueue_allowance_rejection(
        &storage,
        peer,
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await;

    assert!(matches!(accepted, Err(PaykitSdkError::Protocol { .. })));
    assert!(matches!(rejected, Err(PaykitSdkError::Protocol { .. })));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        before
    );
}

#[tokio::test]
async fn test_allowance_derivation_blocks_on_malformed_recognized_request_with_allowance_id() {
    let storage = registered_storage();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    // A Payment Request rejects unknown top-level fields, so this carrier is
    // stored as a malformed recognized kind that still names the Allowance.
    let malformed_request = PrivateApplicationMessage {
        app_id: Some("bitkit".into()),

        version: Some(1),
        kind: Some("paykit.payment_request".into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"550e8400-e29b-41d4-a716-446655440000","allowance_id":"{ALLOWANCE_ID}","private_sentinel":true,"request":{{"amount":{{"value":"0.5","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt12"],"metadata":{{}}}}}}"#
        ),
    };
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        vec![malformed_request],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(
        record.invalid_reason.as_deref(),
        Some("unsupported Allowance-correlated private message")
    );
    assert!(!format!("{record:?}").contains("private_sentinel"));
}

#[tokio::test]
async fn test_allowance_derivation_blocks_on_allowance_correlated_unknown_kind() {
    for (version, raw_version, parse_status) in [
        (Some(1), "1", PrivateStreamParseStatus::UnknownKind),
        (None, "256", PrivateStreamParseStatus::InvalidJson),
        (None, "-1", PrivateStreamParseStatus::InvalidJson),
        (None, "\"1\"", PrivateStreamParseStatus::InvalidJson),
        (None, "null", PrivateStreamParseStatus::InvalidJson),
    ] {
        let storage = registered_storage();
        let peer = counterparty();
        queue_outbound(
            &storage,
            peer.clone(),
            proposal(PROPOSAL_ID, AllowanceRole::Allower),
            timestamp(),
        )
        .await;
        persist_inbound(
            &storage,
            peer.clone(),
            vec![acceptance(PROPOSAL_ID)],
            timestamp(),
        )
        .await;
        let accepted = derived(&storage, &peer).await;
        assert_eq!(accepted.state, AllowanceLifecycleState::Accepted);
        assert_eq!(accepted.history_status, AllowanceHistoryStatus::Consistent);

        persist_private_stream_batch(
            &storage,
            peer.clone(),
            vec![PrivateApplicationMessage {
                app_id: Some("bitkit".into()),

                version,
                kind: Some("paykit.allowance_future".into()),
                raw_json: serde_json::json!({
                    "version": serde_json::from_str::<serde_json::Value>(raw_version).unwrap(),
                    "kind": "paykit.allowance_future",
                    "allowance_id": ALLOWANCE_ID,
                    "private_sentinel": true,
                })
                .to_string(),
            }],
            None,
            timestamp(),
        )
        .await
        .unwrap();

        let snapshot = storage.snapshot().unwrap();
        let item = snapshot.private_stream_items.last().unwrap();
        assert_eq!(item.parse_status, parse_status);
        assert!(item.raw_json.contains("private_sentinel"));

        let record = derived(&storage, &peer).await;

        assert_eq!(record.state, AllowanceLifecycleState::Accepted);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
        assert_eq!(
            record.invalid_reason.as_deref(),
            Some("unsupported Allowance-correlated private message")
        );
        assert!(!format!("{record:?}").contains("private_sentinel"));
    }
}

#[tokio::test]
async fn test_allowance_derivation_ignores_unknown_kind_without_top_level_allowance_id() {
    for (version, raw_version) in [(Some(1), "1"), (None, "256")] {
        let storage = registered_storage();
        let peer = counterparty();
        persist_inbound(
            &storage,
            peer.clone(),
            vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
            timestamp(),
        )
        .await;
        let unknown = |fields: serde_json::Value| {
            let mut value = serde_json::json!({
                "version": serde_json::from_str::<serde_json::Value>(raw_version).unwrap(),
                "kind": "paykit.allowance_future",
            });
            value
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            PrivateApplicationMessage {
                app_id: Some("bitkit".into()),

                version,
                kind: Some("paykit.allowance_future".into()),
                raw_json: value.to_string(),
            }
        };
        persist_private_stream_batch(
            &storage,
            peer.clone(),
            vec![
                unknown(serde_json::json!({})),
                unknown(serde_json::json!({"payload": {"allowance_id": ALLOWANCE_ID}})),
                unknown(serde_json::json!({"allowance_id": "not-a-uuid"})),
                PrivateApplicationMessage {
                    app_id: Some("bitkit".into()),

                    version: None,
                    kind: None,
                    raw_json: "not JSON".into(),
                },
            ],
            None,
            timestamp(),
        )
        .await
        .unwrap();

        let record = derived(&storage, &peer).await;

        assert_eq!(record.state, AllowanceLifecycleState::Proposed);
        assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
        assert!(record.invalid_reason.is_none());
    }
}

#[tokio::test]
async fn test_allowance_derivation_ignores_response_from_proposal_sender() {
    let storage = registered_storage();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        vec![
            proposal(PROPOSAL_ID, AllowanceRole::Allower),
            acceptance(PROPOSAL_ID),
        ],
        timestamp(),
    )
    .await;

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert!(record.acceptance_event_id.is_none());
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(
        record.invalid_reason.as_deref(),
        Some("Allowance response came from the proposal sender")
    );
}

#[tokio::test]
async fn test_allowance_derivation_rejects_withdrawal_from_proposal_recipient() {
    let storage = registered_storage();
    let peer = counterparty();
    queue_outbound(
        &storage,
        peer.clone(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;
    persist_inbound(&storage, peer.clone(), vec![withdrawal()], timestamp()).await;

    let record = derived(&storage, &peer).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert!(record.end_event_id.is_none());
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(
        record.invalid_reason.as_deref(),
        Some("Allowance proposal withdrawal came from the proposal recipient")
    );
}

#[tokio::test]
async fn test_allowance_end_command_rejects_later_end_without_queue_mutation() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    queue_outbound(
        &storage,
        peer.clone(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;

    let ended = enqueue_allowance_end(
        &storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();
    let before = storage.snapshot().unwrap().outbound_private_messages;

    let duplicate = enqueue_allowance_end(
        &storage,
        peer,
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        allowance_id(),
        timestamp(),
    )
    .await;

    assert_eq!(ended.state, AllowanceLifecycleState::Ended);
    assert!(matches!(duplicate, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        before
    );
}

#[test]
fn test_allowance_record_debug_redacts_terms() {
    let mut record = AllowanceRecord::new(counterparty(), ALLOWANCE_ID.into());
    record.terms = Some(AllowanceTermsRecord {
        asset: "private-asset-sentinel".into(),
        per_payment_amount: None,
        period_limits: Vec::new(),
        lifetime_amount_limit: Some("private-limit-sentinel".into()),
        active_from: None,
        expires_at: None,
        allowed_payment_endpoint_identifiers: None,
    });

    let debug = format!("{record:?}");

    assert!(!debug.contains("private-asset-sentinel"));
    assert!(!debug.contains("private-limit-sentinel"));
}

#[tokio::test]
async fn test_allowance_invalid_location_receipt_reuse_invalidates_history() {
    // Cover both same-direction acceptance reuse and cross-direction proposal reuse,
    // with the rejected receipt arriving before or after the Allowance evidence.
    for reused_id in [PROPOSAL_ID, ACCEPTANCE_ID] {
        for receipt_first in [false, true] {
            let storage = registered_storage();
            let peer = counterparty();
            let receipt = PrivateApplicationMessage {
                app_id: Some("bitkit".into()),

                version: Some(1),
                kind: Some("paykit.receipt_access".into()),
                raw_json: {
                    let mut raw: serde_json::Value =
                        serde_json::from_str(&crate::test_utils::receipt_access_json(reused_id))
                            .unwrap();
                    raw["location"] = serde_json::json!("/invalid-location");
                    raw.to_string()
                },
            };
            if receipt_first {
                persist_private_stream_batch(
                    &storage,
                    peer.clone(),
                    vec![receipt.clone()],
                    None,
                    timestamp(),
                )
                .await
                .unwrap();
            }
            queue_outbound(
                &storage,
                peer.clone(),
                proposal(PROPOSAL_ID, AllowanceRole::Allower),
                timestamp(),
            )
            .await;
            persist_inbound(
                &storage,
                peer.clone(),
                vec![acceptance(PROPOSAL_ID)],
                timestamp(),
            )
            .await;
            if !receipt_first {
                let accepted = derived(&storage, &peer).await;
                assert_eq!(accepted.state, AllowanceLifecycleState::Accepted);
                assert_eq!(accepted.history_status, AllowanceHistoryStatus::Consistent);
                persist_private_stream_batch(
                    &storage,
                    peer.clone(),
                    vec![receipt],
                    None,
                    timestamp(),
                )
                .await
                .unwrap();
            }
            let record = derived(&storage, &peer).await;
            assert_eq!(
                record.history_status,
                AllowanceHistoryStatus::Invalid,
                "reused {reused_id}, receipt_first={receipt_first}"
            );
            assert!(record.conflict_event_ids.iter().any(|id| id == reused_id));
            assert!(storage
                .snapshot()
                .unwrap()
                .receipt_access_records
                .is_empty());
        }
    }
}

fn registered_storage() -> InMemoryStorage {
    InMemoryStorage::with_registered_apps([paykit_lib::PaykitAppId::new("bitkit").unwrap()])
}

fn received_allowee_proposal(role: AllowanceRole) -> AllowanceEvent {
    AllowanceEvent::Proposal(AllowanceProposal::new(
        EventId::new_v4(),
        AllowanceId::new_v4(),
        role,
        terms(),
    ))
}

/// Persist `count` proposals that make the local identity the Allowee, in
/// one receive batch, and return their stream item ids oldest first.
async fn persist_allowee_proposals(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    count: usize,
) -> Vec<u64> {
    persist_private_stream_batch(
        storage,
        peer.clone(),
        (0..count)
            .map(|_| message(&received_allowee_proposal(AllowanceRole::Allower)))
            .collect(),
        None,
        timestamp(),
    )
    .await
    .unwrap()
    .stream_item_ids
}

/// Seed an identity and a ready link whose state passes the validation
/// shared storage runs before every write.
async fn seed_validating_link(storage: &InMemoryStorage, peer: &PubkyPublicKey) {
    use pubky_noise::snow_crypto::{HandshakePattern, NoisePhase, NoiseStep};

    let snapshot = pubky_noise::serializer::PubkyNoiseSessionState {
        version: pubky_noise::serializer::SESSION_STATE_VERSION,
        phase: NoisePhase::Transport,
        pattern: HandshakePattern::PatternXX,
        initiator: true,
        ephemeral_secret: [1; 32],
        static_secret: Some([2; 32]),
        counter: 3,
        noise_step: NoiseStep::Final,
        sub_step_index: 0,
        handshake_hash: Some([3; 32]),
        link_id: Some([4; 32]),
        sending_nonce: 0,
        receiving_nonce: 0,
        write_counter: 3,
        read_counter: 3,
        endpoint_pubkey: peer.to_public_key().unwrap().to_bytes(),
        handshake_messages: vec![vec![5; 96]],
    };
    let mut link_snapshot = snapshot.serialize();
    link_snapshot.extend_from_slice(&peer.to_public_key().unwrap().to_bytes());
    // The Paykit snapshot appends the routing key and empty recovery context.
    link_snapshot.extend_from_slice(&[0; 72]);
    let peer = peer.clone();
    storage
        .transaction(move |tx| {
            tx.save_identity_state(crate::IdentityState {
                public_key: Some(counterparty()),
                initialized_at: timestamp(),
            });
            tx.save_linked_peer(linked_peer(peer.clone(), LinkedPeerState::Linked));
            tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                counterparty: peer,
                link_snapshot: Some(link_snapshot),
                handshake_snapshot: None,
                handshake_role: None,
                generation: 1,
                checkpointed_at: timestamp(),
            });
            Ok(())
        })
        .await
        .unwrap();
}

/// Proposals that, with their Delivery Confirmations and the message from
/// [`choose_counterparty`], leave a link one record under the cap.
const AT_CAP_PROPOSALS: usize = (MAX_PENDING_ALLOWANCE_LINK_RECORDS - 2) / 2;

/// Lift the retained-item limit, as an earlier local Allowance message does.
async fn choose_counterparty(storage: &InMemoryStorage, peer: &PubkyPublicKey) {
    queue_outbound(
        storage,
        peer.clone(),
        received_allowee_proposal(AllowanceRole::Allowee),
        timestamp(),
    )
    .await;
}

fn link_records(storage: &InMemoryStorage, peer: &PubkyPublicKey) -> usize {
    let state = storage.snapshot().unwrap();
    state
        .private_stream_items
        .iter()
        .filter(|item| &item.counterparty == peer)
        .count()
        + state
            .outbound_private_messages
            .iter()
            .filter(|message| &message.counterparty == peer)
            .count()
}

async fn accept_pending(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    limit: usize,
) -> crate::Result<PendingAllowanceAcceptance> {
    accept_pending_allowance_proposals(
        storage,
        peer.clone(),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        limit,
        timestamp(),
    )
    .await
}

fn reset_history_counters() {
    LINK_HISTORY_LOADS.with(|loads| loads.set(0));
    LINK_HISTORY_RECORDS_READ.with(|read| read.set(0));
}

fn history_counters() -> (usize, usize) {
    (
        LINK_HISTORY_LOADS.with(std::cell::Cell::get),
        LINK_HISTORY_RECORDS_READ.with(std::cell::Cell::get),
    )
}

/// Comparable view of a link's derived Allowances and queued messages, free
/// of the random Event IDs each Acceptance gets.
async fn link_outcome(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
) -> (Vec<impl PartialEq + std::fmt::Debug>, Vec<String>) {
    let mut records = allowance_records(storage, peer)
        .await
        .unwrap()
        .into_iter()
        .map(|record| {
            (
                record.allowance_id,
                record.state,
                record.history_status,
                record.local_role,
                record.proposal_stream_item_id,
                record.acceptance_outbound_status,
                record.last_outbound_message_id,
            )
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| left.0.cmp(&right.0));
    let kinds = storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .into_iter()
        .filter(|message| &message.counterparty == peer)
        .map(|message| message.kind)
        .collect();
    (records, kinds)
}

fn accepted(outcome: PendingAllowanceAcceptance) -> (Vec<AllowanceRecord>, bool) {
    match outcome {
        PendingAllowanceAcceptance::Accepted {
            records,
            more_pending,
        } => (records, more_pending),
        other => panic!("expected Acceptances, got {other:?}"),
    }
}

#[tokio::test]
async fn test_pending_allowance_acceptance_matches_accept_allowance_for_oldest_proposals() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_validating_link(&storage, &peer).await;
    let proposal_items = persist_allowee_proposals(&storage, &peer, 6).await;
    // Proposals the local identity may not accept as Allowee stay untouched.
    persist_inbound(
        &storage,
        peer.clone(),
        vec![received_allowee_proposal(AllowanceRole::Allowee)],
        timestamp(),
    )
    .await;
    queue_outbound(
        &storage,
        peer.clone(),
        received_allowee_proposal(AllowanceRole::Allower),
        timestamp(),
    )
    .await;
    let expected = InMemoryStorage::from_state(storage.snapshot().unwrap());
    let records = allowance_records(&expected, &peer).await.unwrap();
    for stream_item_id in &proposal_items[..4] {
        let record = records
            .iter()
            .find(|record| record.proposal_stream_item_id == Some(*stream_item_id))
            .unwrap();
        enqueue_allowance_response(
            &expected,
            peer.clone(),
            paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            AllowanceId::new(&record.allowance_id).unwrap(),
            AllowanceResponse::Acceptance {
                expected_proposal_stream_item_id: *stream_item_id,
            },
            timestamp(),
        )
        .await
        .unwrap();
    }

    let (first, more_pending) = accepted(accept_pending(&storage, &peer, 4).await.unwrap());

    assert!(more_pending);
    assert_eq!(
        first
            .iter()
            .map(|record| record.proposal_stream_item_id.unwrap())
            .collect::<Vec<_>>(),
        proposal_items[..4]
    );
    assert!(first
        .iter()
        .all(|record| record.state == AllowanceLifecycleState::Accepted));
    assert_eq!(
        link_outcome(&storage, &peer).await,
        link_outcome(&expected, &peer).await
    );
    // The state the call commits passes the validation shared storage runs
    // before every write.
    let state = storage.snapshot().unwrap();
    assert_eq!(
        crate::storage::decode_storage_state_blob(
            &crate::storage::encode_storage_state_blob(&state).unwrap()
        )
        .unwrap()
        .outbound_private_messages,
        state.outbound_private_messages
    );

    let (rest, more_pending) = accepted(accept_pending(&storage, &peer, 4).await.unwrap());
    assert!(!more_pending);
    assert_eq!(
        rest.iter()
            .map(|record| record.proposal_stream_item_id.unwrap())
            .collect::<Vec<_>>(),
        proposal_items[4..]
    );
    assert_eq!(
        accept_pending(&storage, &peer, 4).await.unwrap(),
        PendingAllowanceAcceptance::Accepted {
            records: Vec::new(),
            more_pending: false,
        }
    );
}

#[tokio::test]
async fn test_pending_allowance_acceptance_derives_at_cap_history_once_per_call() {
    let storage = registered_storage();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone()).await;
    choose_counterparty(&storage, &peer).await;
    let proposal_items = persist_allowee_proposals(&storage, &peer, AT_CAP_PROPOSALS).await;
    let records_before = link_records(&storage, &peer);
    assert_eq!(records_before, MAX_PENDING_ALLOWANCE_LINK_RECORDS - 1);

    reset_history_counters();
    let (records, more_pending) = accepted(accept_pending(&storage, &peer, 4).await.unwrap());

    assert!(more_pending);
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].proposal_stream_item_id, Some(proposal_items[0]));
    // One derivation to select and one to report, whatever `limit` is; the
    // per-proposal path would derive the link again for every Acceptance.
    let (loads, records_read) = history_counters();
    assert_eq!(loads, 2);
    assert_eq!(records_read, 2 * records_before + 4);
}

#[tokio::test]
async fn test_pending_allowance_acceptance_reads_no_history_over_the_link_cap() {
    let storage = registered_storage();
    let heavy = counterparty();
    let other = counterparty();
    seed_active_link(&storage, heavy.clone()).await;
    seed_active_link(&storage, other.clone()).await;
    choose_counterparty(&storage, &heavy).await;
    persist_allowee_proposals(&storage, &heavy, MAX_PENDING_ALLOWANCE_LINK_RECORDS / 2 + 1).await;
    let other_proposal = persist_allowee_proposals(&storage, &other, 1).await[0];
    let heavy_records = link_records(&storage, &heavy);
    assert!(heavy_records > MAX_PENDING_ALLOWANCE_LINK_RECORDS);
    let before = storage.snapshot().unwrap().outbound_private_messages;

    reset_history_counters();
    let outcome = accept_pending(&storage, &heavy, 4).await.unwrap();

    assert_eq!(
        outcome,
        PendingAllowanceAcceptance::LinkHistoryTooLarge {
            link_records: heavy_records,
        }
    );
    assert_eq!(history_counters(), (0, 0));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages,
        before
    );

    // Another counterparty's intake on the same identity reads only its own link.
    let other_records = link_records(&storage, &other);
    let (records, more_pending) = accepted(accept_pending(&storage, &other, 4).await.unwrap());
    assert!(!more_pending);
    assert_eq!(records[0].proposal_stream_item_id, Some(other_proposal));
    assert_eq!(history_counters(), (2, 2 * other_records + 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pending_allowance_acceptance_at_the_cap_leaves_identity_work_and_shutdown_bounded() {
    let storage = std::sync::Arc::new(registered_storage());
    let heavy = counterparty();
    let other = counterparty();
    seed_active_link(&storage, heavy.clone()).await;
    seed_active_link(&storage, other.clone()).await;
    choose_counterparty(&storage, &heavy).await;
    persist_allowee_proposals(&storage, &heavy, AT_CAP_PROPOSALS).await;
    persist_allowee_proposals(&storage, &other, 1).await;

    // A caller's shutdown deadline: the largest history the call derives is
    // synchronous work, so both calls must end within it while sharing the
    // identity's storage.
    let deadline = std::time::Duration::from_secs(30);
    let heavy_call = tokio::spawn({
        let storage = storage.clone();
        async move { accept_pending(&storage, &heavy, 4).await }
    });
    let other_call = tokio::spawn({
        let storage = storage.clone();
        async move { accept_pending(&storage, &other, 4).await }
    });
    let (heavy_outcome, other_outcome) = tokio::time::timeout(deadline, async {
        (heavy_call.await.unwrap(), other_call.await.unwrap())
    })
    .await
    .expect("calls ended within the shutdown deadline");

    let (heavy_records, more_pending) = accepted(heavy_outcome.unwrap());
    assert_eq!(heavy_records.len(), 4);
    assert!(more_pending);
    assert_eq!(accepted(other_outcome.unwrap()).0.len(), 1);
}

#[tokio::test]
async fn test_pending_allowance_acceptance_requires_ready_link_without_queue_mutation() {
    for state in [LinkedPeerState::Blocked, LinkedPeerState::RecoveryRequired] {
        let storage = registered_storage();
        let peer = counterparty();
        seed_active_link(&storage, peer.clone()).await;
        persist_allowee_proposals(&storage, &peer, 2).await;
        storage
            .transaction({
                let peer = peer.clone();
                move |tx| {
                    tx.save_linked_peer(linked_peer(peer, state));
                    Ok(())
                }
            })
            .await
            .unwrap();
        let before = storage.snapshot().unwrap().outbound_private_messages;
        let single = enqueue_allowance_acceptance(
            &storage,
            peer.clone(),
            paykit_lib::PaykitAppId::new("bitkit").unwrap(),
            AllowanceId::new(&allowance_records(&storage, &peer).await.unwrap()[0].allowance_id)
                .unwrap(),
            timestamp(),
        )
        .await
        .unwrap_err();

        let pending = accept_pending(&storage, &peer, 4).await.unwrap_err();

        assert_eq!(
            std::mem::discriminant(&pending),
            std::mem::discriminant(&single)
        );
        assert_eq!(
            storage.snapshot().unwrap().outbound_private_messages,
            before
        );
    }
}
