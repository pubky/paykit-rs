use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::*;
use super::{
    payment_resolution::{
        private_payable_from_batch, public_payable_from_batch, PrivateRecoveryOutcome,
    },
    recovery::{local_recovery_marker_belongs_to_current_episode, RecoveryRequiredUpdate},
};
use crate::{
    domain::adapters::{
        PaymentTarget, PrivatePaymentEndpointCandidate, PrivatePaymentEndpointReservation,
        PrivatePaymentEndpointReservationCancellation, PrivatePaymentEndpointSelectionRequest,
        PrivateReceivingDetail, PublicPaymentEndpointCandidate,
        PublicPaymentEndpointSelectionRequest, PublicReceivingDetail,
    },
    domain::endpoint_reservations::queue_private_payment_list_with_reservations,
    domain::private_stream::persist_private_stream_batch,
    storage::{
        EncryptedLinkStateRecord, EventDedupRecord, InMemoryStorage, LinkedPeerRecord,
        NewOutboundPrivateMessage, PublicEndpointRecord,
    },
    EventIdConflict, OutboundPrivateMessageStatus, PubkySessionAccess,
};
use paykit_lib::PrivateApplicationMessage;

#[derive(Clone)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
    }
}

#[test]
fn test_public_resource_uri_uses_pubky_scheme() {
    let public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());

    assert_eq!(
        public_resource_uri(&public_key, "/pub/staging.bitkit.to/profile.json"),
        format!("pubky://{public_key}/pub/staging.bitkit.to/profile.json")
    );
}

struct TestPubkySessionProvider {
    session: Option<PubkySessionAccess>,
}

#[async_trait]
impl PubkySessionProvider for TestPubkySessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
        Ok(self.session.clone())
    }

    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
        Ok(self
            .session
            .as_ref()
            .map(|session_access| session_access.outbox_client.public_storage()))
    }

    async fn clear_session_access(&self) -> Result<()> {
        Ok(())
    }
}

struct FailingClearSessionProvider;

#[async_trait]
impl PubkySessionProvider for FailingClearSessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
        Ok(None)
    }

    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
        Ok(None)
    }

    async fn clear_session_access(&self) -> Result<()> {
        Err(PaykitSdkError::Identity {
            context: "failed to clear Pubky session access".into(),
            source: None,
        })
    }
}

struct TestPaymentAdapter;

#[async_trait]
impl PaymentAdapter for TestPaymentAdapter {
    async fn current_public_receiving_details(&self) -> Result<Vec<PublicReceivingDetail>> {
        Ok(Vec::new())
    }

    async fn current_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<PrivateReceivingDetail>> {
        Ok(Vec::new())
    }

    async fn select_public_payment_endpoints(
        &self,
        request: &PublicPaymentEndpointSelectionRequest,
    ) -> Result<Vec<PublicPaymentEndpointCandidate>> {
        Ok(request.candidates.clone())
    }

    async fn build_public_payment_target(
        &self,
        endpoint: &PublicPaymentEndpointCandidate,
    ) -> Result<PaymentTarget> {
        Ok(PaymentTarget {
            payload: endpoint.payload.clone(),
        })
    }

    async fn select_private_payment_endpoints(
        &self,
        request: &PrivatePaymentEndpointSelectionRequest,
    ) -> Result<Vec<PrivatePaymentEndpointCandidate>> {
        Ok(request.candidates.clone())
    }

    async fn build_private_payment_target(
        &self,
        endpoint: &PrivatePaymentEndpointCandidate,
    ) -> Result<PaymentTarget> {
        Ok(PaymentTarget {
            payload: endpoint.payload.clone(),
        })
    }
}

struct PrivateListPaymentAdapter;

#[async_trait]
impl PaymentAdapter for PrivateListPaymentAdapter {
    async fn current_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<PrivateReceivingDetail>> {
        Ok(vec![PrivateReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: "ln-private".into(),
        }])
    }

    async fn cancel_private_receiving_detail_reservation(
        &self,
        _release: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        Ok(())
    }
}

struct ReservedPrivateListPaymentAdapter;

#[async_trait]
impl PaymentAdapter for ReservedPrivateListPaymentAdapter {
    async fn reserve_private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Option<Vec<PrivatePaymentEndpointReservation>>> {
        assert!(!counterparty.as_str().is_empty());
        Ok(Some(vec![PrivatePaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "ln-reserved".into(),
            },
            expires_at: None,
            attribution: HashMap::from([("contact".into(), "alice".into())]),
        }]))
    }

    async fn cancel_private_receiving_detail_reservation(
        &self,
        _release: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        Ok(())
    }
}

struct InvalidReservedPrivateListPaymentAdapter {
    canceled: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl PaymentAdapter for InvalidReservedPrivateListPaymentAdapter {
    async fn reserve_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Option<Vec<PrivatePaymentEndpointReservation>>> {
        Ok(Some(vec![
            PrivatePaymentEndpointReservation {
                reservation_id: "reservation-1".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "one".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            },
            PrivatePaymentEndpointReservation {
                reservation_id: "reservation-2".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "two".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            },
        ]))
    }

    async fn cancel_private_receiving_detail_reservation(
        &self,
        cancellation: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        self.canceled
            .lock()
            .unwrap()
            .push(cancellation.reservation_id.clone());
        Ok(())
    }
}

struct FailingCancellationPaymentAdapter;

#[async_trait]
impl PaymentAdapter for FailingCancellationPaymentAdapter {
    async fn cancel_private_receiving_detail_reservation(
        &self,
        _release: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        Err(PaykitSdkError::PaymentAdapter {
            context: "cancellation failed".into(),
            source: None,
        })
    }
}

struct LeaseChangingCancellationPaymentAdapter {
    storage: InMemoryStorage,
    counterparty: PubkyPublicKey,
    canceled: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl PaymentAdapter for LeaseChangingCancellationPaymentAdapter {
    async fn cancel_private_receiving_detail_reservation(
        &self,
        cancellation: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        self.storage
            .transaction({
                let counterparty = self.counterparty.clone();
                move |tx| {
                    let _ = tx.claim_peer_link_operation(
                        &counterparty,
                        &receiver_path(),
                        FixedClock.now() + ChronoDuration::seconds(11),
                        FixedClock.now() + ChronoDuration::seconds(71),
                    );
                    Ok(())
                }
            })
            .await?;
        self.canceled
            .lock()
            .unwrap()
            .push(cancellation.reservation_id.clone());
        Ok(())
    }
}

struct LeaseChangingInvalidReservedPrivateListPaymentAdapter {
    storage: InMemoryStorage,
    counterparty: PubkyPublicKey,
    canceled: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl PaymentAdapter for LeaseChangingInvalidReservedPrivateListPaymentAdapter {
    async fn reserve_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Option<Vec<PrivatePaymentEndpointReservation>>> {
        self.storage
            .transaction({
                let counterparty = self.counterparty.clone();
                move |tx| {
                    let _ = tx.claim_peer_link_operation(
                        &counterparty,
                        &receiver_path(),
                        FixedClock.now() + ChronoDuration::seconds(61),
                        FixedClock.now() + ChronoDuration::seconds(121),
                    );
                    Ok(())
                }
            })
            .await?;
        Ok(Some(vec![
            PrivatePaymentEndpointReservation {
                reservation_id: "reservation-1".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "one".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            },
            PrivatePaymentEndpointReservation {
                reservation_id: "reservation-2".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-onchain-address".into(),
                    payload: "two".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            },
        ]))
    }

    async fn cancel_private_receiving_detail_reservation(
        &self,
        cancellation: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        self.canceled
            .lock()
            .unwrap()
            .push(cancellation.reservation_id.clone());
        Ok(())
    }
}

struct MixedExistingReservedPrivateListPaymentAdapter {
    canceled: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl PaymentAdapter for MixedExistingReservedPrivateListPaymentAdapter {
    async fn reserve_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Option<Vec<PrivatePaymentEndpointReservation>>> {
        Ok(Some(vec![
            PrivatePaymentEndpointReservation {
                reservation_id: "existing-reservation".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "existing".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            },
            PrivatePaymentEndpointReservation {
                reservation_id: "conflicting-reservation".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "conflict".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            },
        ]))
    }

    async fn cancel_private_receiving_detail_reservation(
        &self,
        cancellation: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        self.canceled
            .lock()
            .unwrap()
            .push(cancellation.reservation_id.clone());
        Ok(())
    }
}

fn private_list_message(payload: &str) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.private_payment_list".into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{{"btc-lightning-bolt11":"{payload}"}}}}"#
        ),
    }
}

fn private_list_json() -> String {
    r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into()
}

fn receipt_access_record(counterparty: PubkyPublicKey, receipt_id: &str) -> ReceiptAccessRecord {
    ReceiptAccessRecord {
        counterparty,
        counterparty_receiver_path: receiver_path(),
        stream_item_id: 1,
        receive_batch_id: 1,
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        location: format!("/pub/paykit/v0/private/bitkit/wallet/receipts/{receipt_id}"),
        key: "receipt-secret-key".into(),
        retrieval_status: ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: FixedClock.now(),
    }
}

fn receipt_record(
    issuer: PubkyPublicKey,
    receipt_id: &str,
    recipient_public_key: PubkyPublicKey,
) -> ReceiptRecord {
    ReceiptRecord {
        issuer,
        issuer_receiver_path: receiver_path(),
        receipt_access_event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_access_key_hash: "receipt-access-key-hash".into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key,
        payment_endpoint_identifier: None,
        amount: None,
        metadata: JsonMap::new(),
        location: format!("/pub/paykit/v0/private/bitkit/wallet/receipts/{receipt_id}"),
        retrieved_at: FixedClock.now(),
    }
}

fn conflicted_event_dedup_record(access: &ReceiptAccessRecord) -> EventDedupRecord {
    EventDedupRecord {
        counterparty: access.counterparty.clone(),
        counterparty_receiver_path: access.counterparty_receiver_path.clone(),
        event_id: access.event_id.clone(),
        event_kind: "paykit.receipt_access".into(),
        payload_hash: "sha256:first".into(),
        first_stream_item_id: access.stream_item_id,
        duplicate_stream_item_ids: Vec::new(),
        conflicting_stream_item_ids: vec![access.stream_item_id + 1],
    }
}

fn private_endpoint_candidate(payload: &str) -> PrivatePaymentEndpointCandidate {
    PrivatePaymentEndpointCandidate {
        counterparty: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
        counterparty_receiver_path: receiver_path(),
        identifier: "btc-lightning-bolt11".into(),
        payload: payload.into(),
    }
}

fn public_endpoint_candidate(payload: &str) -> PublicPaymentEndpointCandidate {
    PublicPaymentEndpointCandidate {
        counterparty: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
        counterparty_receiver_path: receiver_path(),
        identifier: "btc-lightning-bolt11".into(),
        payload: payload.into(),
    }
}

fn payment_request_message(
    event_id: &str,
    request_id: &str,
    expires_at: Option<&str>,
) -> PrivateApplicationMessage {
    let expiry = expires_at
        .map(|value| format!(r#""{value}""#))
        .unwrap_or_else(|| "null".into());
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.payment_request".into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":{expiry},"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{}}}}}}"#
        ),
    }
}

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

fn other_receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("tether/wallet").unwrap()
}

fn receiver_noise_public_key() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&[7; 32]).public_key())
}

async fn seed_initialized_identity_and_link(
    storage: &InMemoryStorage,
    counterparty: PubkyPublicKey,
) {
    storage
        .save_identity_state(IdentityState {
            local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    storage
        .transaction(move |tx| {
            tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                counterparty,
                counterparty_receiver_path: receiver_path(),
                link_snapshot: Some(vec![1, 2, 3]),
                handshake_snapshot: None,
                handshake_role: None,
                generation: 0,
                checkpointed_at: FixedClock.now(),
            });
            Ok(())
        })
        .await
        .unwrap();
}

async fn seed_initialized_identity_and_handshake(
    storage: &InMemoryStorage,
    counterparty: PubkyPublicKey,
) {
    storage
        .save_identity_state(IdentityState {
            local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    storage
        .transaction(move |tx| {
            tx.save_linked_peer(LinkedPeerRecord {
                counterparty: counterparty.clone(),
                counterparty_receiver_path: receiver_path(),
                state: LinkedPeerState::Linking,
                last_sync_at: Some(FixedClock.now()),
                last_private_receive_at: None,
                failure_count: 0,
                local_recovery_attempt_id: None,
                local_recovery_marker_created_at: None,
                local_recovery_marker_last_error: None,
                remote_recovery_attempt_id: None,
                remote_recovery_marker_observed_at: None,
            });
            tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                counterparty,
                counterparty_receiver_path: receiver_path(),
                link_snapshot: None,
                handshake_snapshot: Some(vec![1, 2, 3]),
                handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
                generation: 0,
                checkpointed_at: FixedClock.now(),
            });
            Ok(())
        })
        .await
        .unwrap();
}

mod backup;
mod contacts;
mod encrypted_links;
mod identity;
mod linked_peers;
mod not_found_classifier;
mod outbound_private;
mod payment_requests;
mod payment_resolution;
mod private_lists;
mod private_stream;
mod public_endpoints;
mod receipts;
mod recovery;
