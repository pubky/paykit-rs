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
        outbound_private::enqueue_private_message,
        payment_requests::{payment_request_records, PaymentRequestLifecycleState},
        private_stream::persist_private_stream_batch,
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

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

fn other_receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/server").unwrap()
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
    AllowanceEvent::Acceptance(AllowanceAcceptance::new(
        event_id(ACCEPTANCE_ID),
        allowance_id(),
        event_id(proposal_event_id),
    ))
}

fn withdrawal() -> AllowanceEvent {
    AllowanceEvent::End(AllowanceEnd::withdrawal(
        event_id(END_ID),
        allowance_id(),
        event_id(PROPOSAL_ID),
    ))
}

fn accepted_end(end_event_id: &str, acceptance_event_id: &str) -> AllowanceEvent {
    AllowanceEvent::End(AllowanceEnd::accepted(
        event_id(end_event_id),
        allowance_id(),
        event_id(PROPOSAL_ID),
        event_id(acceptance_event_id),
    ))
}

fn linked_peer(
    peer: PubkyPublicKey,
    path: PaykitReceiverPath,
    state: LinkedPeerState,
) -> LinkedPeerRecord {
    let mut record = default_linked_peer(peer, path);
    record.state = state;
    record
}

async fn derived(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    path: &PaykitReceiverPath,
) -> AllowanceRecord {
    allowance_record(storage, peer, path, &allowance_id())
        .await
        .unwrap()
        .unwrap()
}

async fn enqueue_allowance_acceptance(
    storage: &InMemoryStorage,
    peer: PubkyPublicKey,
    path: PaykitReceiverPath,
    allowance_id: AllowanceId,
    now: DateTime<Utc>,
) -> crate::Result<AllowanceRecord> {
    enqueue_allowance_response(
        storage,
        peer,
        path,
        allowance_id,
        AllowanceResponse::Acceptance,
        now,
    )
    .await
}

async fn enqueue_allowance_rejection(
    storage: &InMemoryStorage,
    peer: PubkyPublicKey,
    path: PaykitReceiverPath,
    allowance_id: AllowanceId,
    now: DateTime<Utc>,
) -> crate::Result<AllowanceRecord> {
    enqueue_allowance_response(
        storage,
        peer,
        path,
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
    path: PaykitReceiverPath,
    events: Vec<AllowanceEvent>,
    received_at: DateTime<Utc>,
) {
    persist_private_stream_batch(
        storage,
        peer,
        path,
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
    path: PaykitReceiverPath,
    event: AllowanceEvent,
    created_at: DateTime<Utc>,
) {
    enqueue_private_message(
        storage,
        peer,
        path,
        serialize_allowance_event(&event).unwrap(),
        created_at,
    )
    .await
    .unwrap();
}

async fn seed_active_link(
    storage: &InMemoryStorage,
    peer: PubkyPublicKey,
    path: PaykitReceiverPath,
) {
    storage
        .transaction(move |tx| {
            tx.save_linked_peer(linked_peer(
                peer.clone(),
                path.clone(),
                LinkedPeerState::Linked,
            ));
            tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                counterparty: peer,
                counterparty_receiver_path: path,
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
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
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
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        outbound,
        timestamp(),
    )
    .await;

    let records = allowance_records(&storage, &peer, &receiver_path())
        .await
        .unwrap();

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
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp() + ChronoDuration::hours(2),
    )
    .await;
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![acceptance(PROPOSAL_ID)],
        timestamp(),
    )
    .await;
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        withdrawal(),
        timestamp() - ChronoDuration::hours(2),
    )
    .await;

    let record = derived(&storage, &peer, &receiver_path()).await;

    assert_eq!(record.state, AllowanceLifecycleState::Ended);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
    assert_eq!(record.acceptance_event_id.as_deref(), Some(ACCEPTANCE_ID));
    assert_eq!(record.end_event_id.as_deref(), Some(END_ID));
}

#[tokio::test]
async fn test_allowance_derivation_retains_end_by_source_fifo_not_uuid() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let inbound_end_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d209";
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp() + ChronoDuration::hours(2),
    )
    .await;
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
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
        receiver_path(),
        withdrawal(),
        timestamp() - ChronoDuration::hours(2),
    )
    .await;

    let record = derived(&storage, &peer, &receiver_path()).await;

    assert_eq!(record.state, AllowanceLifecycleState::Ended);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
    assert_eq!(record.end_event_id.as_deref(), Some(inbound_end_id));
}

#[tokio::test]
async fn test_allowance_derivation_separates_unresolved_history_from_lifecycle() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;
    let missing_acceptance_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d299";
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![accepted_end(END_ID, missing_acceptance_id)],
        timestamp(),
    )
    .await;

    let record = derived(&storage, &peer, &receiver_path()).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(
        record.history_status,
        AllowanceHistoryStatus::UnresolvedReferences
    );
    assert_eq!(record.pending_causal_event_ids, [missing_acceptance_id]);
}

#[tokio::test]
async fn test_allowance_derivation_detects_proposal_and_cross_kind_event_conflicts() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        proposal(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d211",
            AllowanceRole::Allowee,
        ),
        timestamp(),
    )
    .await;

    let conflicted = derived(&storage, &peer, &receiver_path()).await;
    assert_eq!(conflicted.state, AllowanceLifecycleState::Conflicted);

    let second_peer = counterparty();
    let payment_event = PaymentRequestEvent::Acceptance(PaymentRequestAcceptance::new(
        event_id(PROPOSAL_ID),
        PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab55").unwrap(),
    ));
    let payment_raw = serialize_payment_request_event(&payment_event).unwrap();
    persist_private_stream_batch(
        &storage,
        second_peer.clone(),
        receiver_path(),
        vec![PrivateApplicationMessage {
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
        receiver_path(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;
    let cross_kind = derived(&storage, &second_peer, &receiver_path()).await;
    assert_eq!(cross_kind.state, AllowanceLifecycleState::Proposed);
    assert_eq!(cross_kind.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(cross_kind.conflict_event_ids, [PROPOSAL_ID]);
}

#[tokio::test]
async fn test_allowance_derivation_distinguishes_known_wrong_and_missing_causal_ids() {
    let storage = InMemoryStorage::new();
    let known_wrong_peer = counterparty();
    persist_inbound(
        &storage,
        known_wrong_peer.clone(),
        receiver_path(),
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
        receiver_path(),
        serialize_payment_request_event(&unrelated).unwrap(),
        timestamp(),
    )
    .await
    .unwrap();
    queue_outbound(
        &storage,
        known_wrong_peer.clone(),
        receiver_path(),
        acceptance(END_ID),
        timestamp(),
    )
    .await;

    let missing_peer = counterparty();
    persist_inbound(
        &storage,
        missing_peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let missing_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d299";
    queue_outbound(
        &storage,
        missing_peer.clone(),
        receiver_path(),
        acceptance(missing_id),
        timestamp(),
    )
    .await;

    let known_wrong = derived(&storage, &known_wrong_peer, &receiver_path()).await;
    let missing = derived(&storage, &missing_peer, &receiver_path()).await;

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
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![
            proposal(PROPOSAL_ID, AllowanceRole::Allower),
            proposal(PROPOSAL_ID, AllowanceRole::Allowee),
        ],
        timestamp(),
    )
    .await;

    let record = derived(&storage, &peer, &receiver_path()).await;

    assert_eq!(record.state, AllowanceLifecycleState::Conflicted);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(record.conflict_event_ids, [PROPOSAL_ID]);
}

#[tokio::test]
async fn test_allowance_derivation_scopes_ids_to_exact_link_and_marks_recovery() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    persist_inbound(
        &storage,
        peer.clone(),
        other_receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allowee)],
        timestamp(),
    )
    .await;
    storage
        .transaction({
            let peer = peer.clone();
            move |tx| {
                tx.save_linked_peer(linked_peer(
                    peer,
                    receiver_path(),
                    LinkedPeerState::RecoveryRequired,
                ));
                Ok(())
            }
        })
        .await
        .unwrap();

    let wallet = derived(&storage, &peer, &receiver_path()).await;
    let server = derived(&storage, &peer, &other_receiver_path()).await;

    assert_eq!(
        wallet.history_status,
        AllowanceHistoryStatus::RecoveryRequired
    );
    assert_eq!(server.history_status, AllowanceHistoryStatus::Consistent);
    assert_eq!(server.local_role, Some(AllowanceLocalRole::Allower));
}

#[tokio::test]
async fn test_allowance_acceptance_precondition_and_append_are_atomic() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone(), receiver_path()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;

    let first = enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        receiver_path(),
        allowance_id(),
        timestamp(),
    );
    let second = enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        receiver_path(),
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
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        1
    );
}

#[tokio::test]
async fn test_allowance_proposal_requires_complete_link_without_queue_mutation() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let path = receiver_path();

    let result = enqueue_allowance_proposal(
        &storage,
        peer.clone(),
        path.clone(),
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

    seed_active_link(&storage, peer.clone(), path.clone()).await;
    let proposed = enqueue_allowance_proposal(
        &storage,
        peer,
        path,
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
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone(), receiver_path()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;

    let rejected = enqueue_allowance_rejection(
        &storage,
        peer.clone(),
        receiver_path(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();
    let later =
        enqueue_allowance_acceptance(&storage, peer, receiver_path(), allowance_id(), timestamp())
            .await;

    assert_eq!(rejected.state, AllowanceLifecycleState::Rejected);
    assert!(matches!(later, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        1
    );
}

#[tokio::test]
async fn test_allowance_end_for_accepted_authority_references_exact_acceptance() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone(), receiver_path()).await;
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let accepted = enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        receiver_path(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();

    let ended = enqueue_allowance_end(&storage, peer, receiver_path(), allowance_id(), timestamp())
        .await
        .unwrap();
    let state = storage.snapshot().unwrap();
    let end_message = state.outbound_private_messages.last().unwrap();
    let parsed = parse_allowance_event_message(&PrivateApplicationMessage {
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
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        acceptance(PROPOSAL_ID),
        timestamp(),
    )
    .await;
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        AllowanceEvent::Rejection(AllowanceRejection::new(
            event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204"),
            allowance_id(),
            event_id(PROPOSAL_ID),
        )),
        timestamp() - ChronoDuration::hours(1),
    )
    .await;

    let record = derived(&storage, &peer, &receiver_path()).await;

    assert_eq!(record.state, AllowanceLifecycleState::Accepted);
    assert_eq!(record.acceptance_event_id.as_deref(), Some(ACCEPTANCE_ID));
    assert!(record.rejection_event_id.is_none());
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
}

#[tokio::test]
async fn test_accepted_allowance_view_coexists_with_ordinary_payment_request() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone(), receiver_path()).await;
    let proposal = AllowanceEvent::Proposal(AllowanceProposal::new(
        event_id(PROPOSAL_ID),
        allowance_id(),
        AllowanceRole::Allowee,
        full_terms(),
    ));
    let payment_request_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_request = PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.payment_request".into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"{payment_request_id}","request":{{"amount":{{"value":"0.5","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt12"],"metadata":{{}}}}}}"#
        ),
    };
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![message(&proposal), payment_request],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_allowance_acceptance(
        &storage,
        peer.clone(),
        receiver_path(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();

    let allowance = derived(&storage, &peer, &receiver_path()).await;
    let filter = AllowanceFilter {
        counterparty: Some(peer.clone()),
        counterparty_receiver_path: Some(receiver_path()),
        local_role: Some(AllowanceLocalRole::Allower),
        states: vec![AllowanceLifecycleState::Accepted],
    };
    let requests = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone(), receiver_path()).await;
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;

    let result =
        enqueue_allowance_acceptance(&storage, peer, receiver_path(), allowance_id(), timestamp())
            .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        1
    );
}

#[tokio::test]
async fn test_allowance_derivation_deduplicates_exact_same_sender_retry() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let proposal = proposal(PROPOSAL_ID, AllowanceRole::Allower);
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal.clone(), proposal],
        timestamp(),
    )
    .await;

    let record = derived(&storage, &peer, &receiver_path()).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Consistent);
    assert!(record.conflict_event_ids.is_empty());
}

#[tokio::test]
async fn test_allowance_derivation_preserves_state_when_recognized_event_is_malformed() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    let acceptance = acceptance(PROPOSAL_ID);
    let malformed = serialize_allowance_event(&acceptance).unwrap().replacen(
        '}',
        r#", "private_sentinel": true}"#,
        1,
    );
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![PrivateApplicationMessage {
            version: Some(1),
            kind: Some(acceptance.kind().as_str().to_owned()),
            raw_json: malformed,
        }],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let record = derived(&storage, &peer, &receiver_path()).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
    assert!(!format!("{record:?}").contains("private_sentinel"));
}

#[tokio::test]
async fn test_allowance_derivation_blocks_on_malformed_recognized_request_with_allowance_id() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    persist_inbound(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![proposal(PROPOSAL_ID, AllowanceRole::Allower)],
        timestamp(),
    )
    .await;
    // A Payment Request rejects unknown top-level fields, so this carrier is
    // stored as a malformed recognized kind that still names the Allowance.
    let malformed_request = PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.payment_request".into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"550e8400-e29b-41d4-a716-446655440000","allowance_id":"{ALLOWANCE_ID}","private_sentinel":true,"request":{{"amount":{{"value":"0.5","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt12"],"metadata":{{}}}}}}"#
        ),
    };
    persist_private_stream_batch(
        &storage,
        peer.clone(),
        receiver_path(),
        vec![malformed_request],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let record = derived(&storage, &peer, &receiver_path()).await;

    assert_eq!(record.state, AllowanceLifecycleState::Proposed);
    assert_eq!(record.history_status, AllowanceHistoryStatus::Invalid);
    assert_eq!(
        record.invalid_reason.as_deref(),
        Some("unsupported Allowance-correlated private message")
    );
    assert!(!format!("{record:?}").contains("private_sentinel"));
}

#[tokio::test]
async fn test_allowance_end_command_rejects_later_end_without_queue_mutation() {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    seed_active_link(&storage, peer.clone(), receiver_path()).await;
    queue_outbound(
        &storage,
        peer.clone(),
        receiver_path(),
        proposal(PROPOSAL_ID, AllowanceRole::Allower),
        timestamp(),
    )
    .await;

    let ended = enqueue_allowance_end(
        &storage,
        peer.clone(),
        receiver_path(),
        allowance_id(),
        timestamp(),
    )
    .await
    .unwrap();
    let duplicate =
        enqueue_allowance_end(&storage, peer, receiver_path(), allowance_id(), timestamp()).await;

    assert_eq!(ended.state, AllowanceLifecycleState::Ended);
    assert!(matches!(duplicate, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        2
    );
}

#[test]
fn test_allowance_record_debug_redacts_terms() {
    let mut record = AllowanceRecord::new(counterparty(), receiver_path(), ALLOWANCE_ID.into());
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
