use super::*;
use crate::storage::{
    decode_storage_state_blob, encode_storage_state_blob, EncryptedLinkStateRecord,
};
use crate::LinkedPeerState;

fn proposal() -> PaymentRequest {
    let mut value: JsonValue = serde_json::from_str(&request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
        "invoice-2026-0001",
        Some("2026-06-04T00:00:00Z"),
        None,
    ))
    .unwrap();
    value["request"]["accepted_payment_endpoint_identifiers"] =
        serde_json::json!(["btc-lightning-bolt11", "btc-onchain-p2tr"]);
    value["request"]["metadata"]["offset"] = serde_json::json!(-0.0);
    request_from_json(value)
}

fn request_from_json(value: JsonValue) -> PaymentRequest {
    let PaymentRequestEvent::Request(request) = parsed_event(value.to_string()) else {
        panic!("expected proposal")
    };
    request
}

fn retry(request: &PaymentRequest) -> PaymentRequest {
    PaymentRequest::new(
        EventId::new_v4(),
        request.payment_request_id().clone(),
        request.request().clone(),
    )
}

fn proposal_storage(peer: &PubkyPublicKey) -> (InMemoryStorage, PubkyPublicKey) {
    use pubky_noise::snow_crypto::{HandshakePattern, NoisePhase, NoiseStep};

    let mut state = registered_storage().snapshot().unwrap();
    let identity = state
        .identity_state
        .as_ref()
        .unwrap()
        .public_key
        .clone()
        .unwrap();
    let mut linked = crate::domain::linked_peers::default_linked_peer(peer.clone());
    linked.state = LinkedPeerState::Linked;
    state.linked_peers.insert(peer.clone(), linked);
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
    let mut bytes = snapshot.serialize();
    bytes.extend_from_slice(&peer.to_public_key().unwrap().to_bytes());
    bytes.extend_from_slice(&[0; 72]);
    state.encrypted_link_states.insert(
        peer.clone(),
        EncryptedLinkStateRecord {
            counterparty: peer.clone(),
            link_snapshot: Some(bytes),
            handshake_snapshot: None,
            handshake_role: None,
            generation: 1,
            checkpointed_at: timestamp(),
        },
    );
    (InMemoryStorage::from_state(state), identity)
}

async fn propose(
    storage: &impl StorageAdapter,
    peer: &PubkyPublicKey,
    request: &PaymentRequest,
    identity: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<PaymentRequestRecord> {
    storage
        .transaction(|tx| {
            enqueue_idempotent_payment_request(tx, peer, &app_id(), request, identity, now)
        })
        .await
}

struct LostCommitAcknowledgementStorage(InMemoryStorage);

#[async_trait::async_trait]
impl StorageAdapter for LostCommitAcknowledgementStorage {
    async fn transaction_erased<'a>(
        &self,
        callback: crate::storage::StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        self.0.transaction_erased(callback).await?;
        Err(PaykitSdkError::Storage {
            context: "commit acknowledgement unavailable".into(),
            source: None,
        })
    }
}

#[tokio::test]
async fn test_idempotent_proposal_recovers_uncertain_commit_after_restart() {
    let peer = counterparty();
    let (storage, identity) = proposal_storage(&peer);
    let storage = LostCommitAcknowledgementStorage(storage);
    let request = proposal();
    assert!(matches!(
        propose(&storage, &peer, &request, &identity, timestamp()).await,
        Err(PaykitSdkError::Storage { .. })
    ));
    let committed = storage.0.snapshot().unwrap();
    let bytes = encode_storage_state_blob(&committed).unwrap();
    drop(storage);
    let restarted = InMemoryStorage::from_state(decode_storage_state_blob(&bytes).unwrap());
    let record = propose(&restarted, &peer, &retry(&request), &identity, timestamp())
        .await
        .unwrap();

    assert_eq!(
        record.proposal_event_id.as_deref(),
        Some(request.event_id().as_str())
    );
    assert_eq!(
        record.proposal_outbound_message_id,
        Some(committed.outbound_private_messages[0].outbound_message_id)
    );
    assert_eq!(restarted.snapshot().unwrap(), committed);
}

#[tokio::test]
async fn test_idempotent_proposal_retains_binding_after_expiry_and_cancellation() {
    for canceled in [false, true] {
        let peer = counterparty();
        let (storage, identity) = proposal_storage(&peer);
        let request = proposal();
        let first = propose(&storage, &peer, &request, &identity, timestamp())
            .await
            .unwrap();
        storage
            .transaction(|tx| {
                let mut message = tx.outbound_private_messages(&peer).remove(0);
                message.status = OutboundPrivateMessageStatus::Sent;
                message.sent_at = Some(timestamp());
                message.confirmed_at = Some(timestamp());
                message.attempt_count = 1;
                tx.save_outbound_private_message(message)
            })
            .await
            .unwrap();
        if canceled {
            enqueue_payment_request_event(
                &storage,
                peer.clone(),
                &app_id(),
                &PaymentRequestEvent::Cancellation(PaymentRequestCancellation::new(
                    EventId::new_v4(),
                    request.payment_request_id().clone(),
                    None,
                )),
                timestamp(),
            )
            .await
            .unwrap();
        }
        let before = storage.snapshot().unwrap();
        let now = timestamp() + ChronoDuration::days(2);
        let record = propose(&storage, &peer, &retry(&request), &identity, now)
            .await
            .unwrap();
        assert_eq!(
            record.state,
            if canceled {
                PaymentRequestLifecycleState::Canceled
            } else {
                PaymentRequestLifecycleState::ProposalExpired
            }
        );
        assert_eq!(record.proposal_event_id, first.proposal_event_id);
        assert_eq!(
            record.proposal_outbound_message_id,
            first.proposal_outbound_message_id
        );
        assert_eq!(
            record.proposal_outbound_status,
            Some(OutboundPrivateMessageStatus::Sent)
        );

        let mut changed: JsonValue =
            serde_json::from_str(&before.outbound_private_messages[0].raw_json).unwrap();
        changed["request"]["metadata"] = serde_json::json!({"note":"changed"});
        assert!(matches!(
            propose(&storage, &peer, &request_from_json(changed), &identity, now).await,
            Err(PaykitSdkError::Policy { .. })
        ));
        assert_eq!(storage.snapshot().unwrap(), before);
    }
}

#[tokio::test]
async fn test_idempotent_proposal_rejects_different_original_input() {
    let peer = counterparty();
    let (storage, identity) = proposal_storage(&peer);
    let request = proposal();
    propose(&storage, &peer, &request, &identity, timestamp())
        .await
        .unwrap();
    let before = storage.snapshot().unwrap();
    let original: JsonValue =
        serde_json::from_str(&before.outbound_private_messages[0].raw_json).unwrap();
    for (field, value) in [
        (
            "amount",
            serde_json::json!({"value":"0.0010","asset":"btc"}),
        ),
        ("payment_reference", serde_json::json!("another-reference")),
        (
            "proposal_expires_at",
            serde_json::json!("2026-06-04T00:00:00.000Z"),
        ),
        ("metadata", serde_json::json!({"note":"other"})),
        (
            "metadata",
            serde_json::json!({"note":"private","offset":0.0}),
        ),
        ("required_app_id", serde_json::json!("paykit-server")),
        (
            "accepted_payment_endpoint_identifiers",
            serde_json::json!(["btc-onchain-p2tr", "btc-lightning-bolt11"]),
        ),
        (
            "recurrence",
            serde_json::json!({"every":1,"unit":"month","starts_at":"2026-06-01T00:00:00Z","anchor":"2026-06-01T00:00:00Z","ends_at":null}),
        ),
    ] {
        let mut changed = original.clone();
        changed["request"][field] = value;
        assert!(
            matches!(
                propose(
                    &storage,
                    &peer,
                    &request_from_json(changed),
                    &identity,
                    timestamp()
                )
                .await,
                Err(PaykitSdkError::Policy { .. })
            ),
            "{field}"
        );
        assert_eq!(storage.snapshot().unwrap(), before);
    }
}

#[tokio::test]
async fn test_idempotent_proposal_binds_app_and_counterparty() {
    let peer = counterparty();
    let other_peer = counterparty();
    let (storage, identity) = proposal_storage(&peer);
    let request = proposal();
    propose(&storage, &peer, &request, &identity, timestamp())
        .await
        .unwrap();
    let mut state = storage.snapshot().unwrap();
    let (other, _) = proposal_storage(&other_peer);
    state
        .linked_peers
        .extend(other.snapshot().unwrap().linked_peers);
    state
        .encrypted_link_states
        .extend(other.snapshot().unwrap().encrypted_link_states);
    let other_app = paykit_lib::PaykitAppId::new("paykit-server").unwrap();
    state.registered_paykit_apps.insert(other_app.clone());
    state
        .registered_paykit_app_capabilities
        .insert(other_app.clone(), payment_request_capabilities());
    let storage = InMemoryStorage::from_state(state.clone());
    for (peer, app) in [(&peer, &other_app), (&other_peer, &app_id())] {
        let result = storage
            .transaction(|tx| {
                enqueue_idempotent_payment_request(tx, peer, app, &request, &identity, timestamp())
            })
            .await;
        assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
        assert_eq!(storage.snapshot().unwrap(), state);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_idempotent_proposal_concurrent_creation_commits_once() {
    for conflicting in [false, true] {
        let peer = counterparty();
        let (storage, identity) = proposal_storage(&peer);
        let first = proposal();
        let mut second: JsonValue = serde_json::from_str(
            &serialize_payment_request_event(
                &app_id(),
                &PaymentRequestEvent::Request(retry(&first)),
            )
            .unwrap(),
        )
        .unwrap();
        if conflicting {
            second["request"]["metadata"] = serde_json::json!({"note":"competing"});
        }
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let tasks = [first, request_from_json(second)].map(|request| {
            let storage = storage.clone();
            let peer = peer.clone();
            let identity = identity.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                propose(&storage, &peer, &request, &identity, timestamp()).await
            })
        });
        let [first, second] = tasks;
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        if conflicting {
            let error = match (first, second) {
                (Ok(_), Err(error)) | (Err(error), Ok(_)) => error,
                other => panic!("expected exactly one commit: {other:?}"),
            };
            assert!(matches!(error, PaykitSdkError::Policy { .. }));
        } else {
            assert_eq!(first.unwrap(), second.unwrap());
        }
        assert_eq!(
            storage.snapshot().unwrap().outbound_private_messages.len(),
            1
        );
    }
}

#[tokio::test]
async fn test_idempotent_proposal_retry_rechecks_identity_capability_and_link() {
    let peer = counterparty();
    let (storage, identity) = proposal_storage(&peer);
    let request = proposal();
    propose(&storage, &peer, &request, &identity, timestamp())
        .await
        .unwrap();
    let original = storage.snapshot().unwrap();
    for change in ["identity", "capability", "retired", "blocked", "link"] {
        let mut state = original.clone();
        match change {
            "identity" => state.identity_state.as_mut().unwrap().public_key = Some(counterparty()),
            "capability" => {
                state
                    .registered_paykit_app_capabilities
                    .get_mut(&app_id())
                    .unwrap()
                    .payment_requests = false
            }
            "retired" => {
                state.retired_paykit_apps.insert(app_id());
            }
            "blocked" => {
                state.linked_peers.get_mut(&peer).unwrap().state = LinkedPeerState::Blocked
            }
            "link" => {
                state
                    .encrypted_link_states
                    .get_mut(&peer)
                    .unwrap()
                    .link_snapshot = None
            }
            _ => unreachable!(),
        }
        let storage = InMemoryStorage::from_state(state.clone());
        assert!(
            propose(&storage, &peer, &retry(&request), &identity, timestamp())
                .await
                .is_err(),
            "{change}"
        );
        assert_eq!(storage.snapshot().unwrap(), state);
    }
}
