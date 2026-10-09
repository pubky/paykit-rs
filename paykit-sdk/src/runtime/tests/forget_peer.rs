use super::*;
use crate::domain::{
    allowances::allowance_records_in_transaction,
    outbound_private::enqueue_private_message,
    payment_requests::{
        claim_payment_request_execution, enqueue_payment_request_event,
        payment_request_records_from_transaction,
    },
    private_stream::MAX_RETAINED_PRIVATE_STREAM_ITEMS_PER_COUNTERPARTY,
};
use crate::test_utils::{allowance_application_message, receipt_access_json};
use paykit_lib::{AllowanceEvent, AllowanceProposal, AllowanceRole, EventId};

type ForgetTestSdk =
    PaykitSdk<InMemoryStorage, TestPubkySessionProvider, TestPaymentAdapter, FixedClock>;

fn forget_test_sdk(storage: &InMemoryStorage) -> ForgetTestSdk {
    PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    )
}

fn random_counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

/// Storage with a local identity, an Encrypted Link for `linked`, and every
/// listed counterparty in the `Linked` state.
async fn linked_storage(linked: &PubkyPublicKey, others: &[&PubkyPublicKey]) -> InMemoryStorage {
    let storage = registered_test_storage();
    seed_private_capable_identity_and_link(&storage, linked.clone()).await;
    storage
        .transaction(|tx| {
            for counterparty in std::iter::once(linked).chain(others.iter().copied()) {
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                tx.save_linked_peer(peer);
                save_authorized_paykit_app(
                    tx,
                    counterparty.clone(),
                    app_id(),
                    private_app_capabilities(),
                );
            }
            Ok(())
        })
        .await
        .unwrap();
    storage
}

async fn mark_linked(storage: &InMemoryStorage, counterparty: &PubkyPublicKey) {
    storage
        .transaction(|tx| {
            let mut peer = tx.linked_peer(counterparty).unwrap();
            peer.state = LinkedPeerState::Linked;
            tx.save_linked_peer(peer);
            Ok(())
        })
        .await
        .unwrap();
}

async fn receive(
    storage: &InMemoryStorage,
    counterparty: &PubkyPublicKey,
    messages: Vec<PrivateApplicationMessage>,
) -> Result<crate::PrivateStreamIntakeReport> {
    persist_private_stream_batch(
        storage,
        counterparty.clone(),
        messages,
        None,
        FixedClock.now(),
    )
    .await
}

fn future_kind_message(sequence: usize) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.future".into()),
        app_id: Some("bitkit".into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.future","app_id":"bitkit","sequence":{sequence}}}"#
        ),
    }
}

fn payment_request_message_for_amount(
    event_id: &str,
    request_id: &str,
    amount: &str,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.payment_request".into()),
        app_id: Some("bitkit".into()),
        raw_json: format!(
            r#"{{"version":1,"kind":"paykit.payment_request","app_id":"bitkit","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"{amount}","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"required_app_id":null,"metadata":{{}}}}}}"#
        ),
    }
}

fn allowance_proposal_message(
    event_id: &EventId,
    allowance_id: &AllowanceId,
    lifetime_amount_limit: &str,
) -> PrivateApplicationMessage {
    // The counterparty proposes as Allowee, so the local identity would be the paying Allower.
    allowance_application_message(&AllowanceEvent::Proposal(AllowanceProposal::new(
        event_id.clone(),
        allowance_id.clone(),
        AllowanceRole::Allowee,
        AllowanceTerms::builder("btc")
            .lifetime_amount_limit(lifetime_amount_limit)
            .build()
            .unwrap(),
    )))
}

fn receipt_access_message() -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.receipt_access".into()),
        app_id: Some("bitkit".into()),
        raw_json: receipt_access_json(EventId::new_v4().as_str()),
    }
}

/// Mark the stored Receipt Access from `issuer` retrieved and save its Receipt.
async fn retrieve_receipt(
    storage: &InMemoryStorage,
    issuer: &PubkyPublicKey,
    local: &PubkyPublicKey,
) {
    storage
        .transaction(|tx| {
            let mut access = tx.receipt_access_records(issuer).pop().unwrap();
            access.app_authorized = true;
            let access = access.mark_retrieved(FixedClock.now());
            tx.save_receipt_record(ReceiptRecord {
                issuer: issuer.clone(),
                app_id: access.app_id.clone(),
                receipt_access_event_id: access.event_id.clone(),
                receipt_access_key_hash: crate::domain::receipts::receipt_access_key_hash(
                    &access.key,
                ),
                receipt_id: access.receipt_id.clone(),
                payment_reference: access.payment_reference.clone(),
                payment_request_id: None,
                billing_period: None,
                recipient_public_key: local.clone(),
                payment_endpoint_identifier: None,
                amount: None,
                metadata: JsonMap::new(),
                location: access.location.clone(),
                retrieved_at: FixedClock.now(),
            });
            tx.save_receipt_access_record(access);
            Ok(())
        })
        .await
        .unwrap();
}

fn accounting_scope(
    local_public_key: &PubkyPublicKey,
    counterparty: &PubkyPublicKey,
) -> crate::PaymentAccountingScope {
    crate::PaymentAccountingScope {
        local_public_key: local_public_key.clone(),
        counterparty: counterparty.clone(),
        payment_request_id: PaymentRequestId::new_v4().as_str().into(),
    }
}

/// A manual-only wallet decision with no payment attempt.
fn manual_only_occurrence(scope: crate::PaymentAccountingScope) -> crate::PaymentOccurrenceRecord {
    crate::PaymentOccurrenceRecord {
        key: crate::PaymentOccurrenceKey {
            request: scope,
            billing_period: None,
        },
        disposition: crate::PaymentDisposition::ManualOnly,
        allowance_id: None,
        association_revision: None,
        attempts: vec![],
    }
}

fn watermark(
    local_public_key: &PubkyPublicKey,
    counterparty: &PubkyPublicKey,
    allowance_id: &AllowanceId,
) -> crate::AllowanceWatermarkRecord {
    crate::AllowanceWatermarkRecord {
        local_public_key: local_public_key.clone(),
        counterparty: counterparty.clone(),
        allowance_id: allowance_id.as_str().into(),
        evaluated_at: FixedClock.now(),
    }
}

fn empty_accounting() -> crate::AllowanceAccountingState {
    crate::AllowanceAccountingState {
        revision: 1,
        epoch: EventId::new_v4().as_str().to_owned(),
        requires_reconciliation: false,
        history: crate::AllowanceAccountingHistory::default(),
    }
}

fn local_identity(state: &crate::storage::StorageState) -> PubkyPublicKey {
    state
        .identity_state
        .as_ref()
        .unwrap()
        .public_key
        .clone()
        .unwrap()
}

fn stream_items_from(
    state: &crate::storage::StorageState,
    counterparty: &PubkyPublicKey,
) -> Vec<crate::storage::PrivateStreamItemRecord> {
    state
        .private_stream_items
        .iter()
        .filter(|item| &item.counterparty == counterparty)
        .cloned()
        .collect()
}

fn outbound_to(
    state: &crate::storage::StorageState,
    counterparty: &PubkyPublicKey,
) -> Vec<crate::storage::OutboundPrivateMessageRecord> {
    state
        .outbound_private_messages
        .iter()
        .filter(|message| &message.counterparty == counterparty)
        .cloned()
        .collect()
}

#[tokio::test]
async fn test_forget_peer_is_atomic_and_requires_a_blocked_idle_peer() {
    use std::{any::Any, sync::atomic::AtomicUsize};

    struct CountingStorage {
        inner: InMemoryStorage,
        calls: Arc<AtomicUsize>,
        reject: bool,
    }

    #[async_trait]
    impl StorageAdapter for CountingStorage {
        async fn transaction_erased<'a>(
            &self,
            callback: crate::storage::StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn Any + Send>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner
                .transaction_erased(Box::new(|tx| {
                    let result = callback(tx)?;
                    if self.reject {
                        return Err(PaykitSdkError::Storage {
                            context: "forget commit rejected".into(),
                            source: None,
                        });
                    }
                    Ok(result)
                }))
                .await
        }
    }

    for case in [
        "ready", "busy", "expired", "identity", "self", "rollback", "linked", "unknown",
    ] {
        let storage = registered_test_storage();
        let counterparty = random_counterparty();
        seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
        receive(&storage, &counterparty, vec![future_kind_message(0)])
            .await
            .unwrap();
        let mut before = storage.snapshot().unwrap();
        let local = local_identity(&before);
        let mut peer = default_linked_peer(counterparty.clone());
        peer.state = if case == "linked" {
            LinkedPeerState::Linked
        } else {
            LinkedPeerState::Blocked
        };
        if case != "unknown" {
            before.linked_peers.insert(counterparty.clone(), peer);
        }
        // Blocking already cleared the Encrypted Link snapshot.
        before
            .encrypted_link_states
            .get_mut(&counterparty)
            .unwrap()
            .link_snapshot = None;
        if matches!(case, "busy" | "expired") {
            before.peer_link_operation_leases.insert(
                counterparty.clone(),
                PeerLinkOperationLease {
                    counterparty: counterparty.clone(),
                    lease_id: 0,
                    claimed_at: FixedClock.now() - ChronoDuration::minutes(1),
                    expires_at: FixedClock.now()
                        + ChronoDuration::seconds(if case == "busy" { 60 } else { 0 }),
                },
            );
            before.next_peer_link_operation_lease_id = 1;
        }
        let target = if case == "self" {
            local
        } else {
            counterparty.clone()
        };
        if case == "identity" {
            before.identity_state = None;
        }
        let storage = InMemoryStorage::from_state(before.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let sdk = PaykitSdk::with_clock(
            CountingStorage {
                inner: storage.clone(),
                calls: calls.clone(),
                reject: case == "rollback",
            },
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::new("bitkit").unwrap(),
            FixedClock,
        );

        let result = sdk.forget_peer(target).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1, "{case}");
        let after = storage.snapshot().unwrap();
        if !matches!(case, "ready" | "expired") {
            let error = result.unwrap_err();
            match case {
                "busy" => assert!(matches!(error, PaykitSdkError::ConcurrentUpdate { .. })),
                "identity" => assert!(matches!(error, PaykitSdkError::Identity { .. })),
                "rollback" => assert!(matches!(error, PaykitSdkError::Storage { .. })),
                _ => {
                    assert!(matches!(error, PaykitSdkError::Policy { .. }), "{case}");
                    assert!(!error.is_retention_limit_reached(), "{case}");
                }
            }
            assert_eq!(after, before, "{case}");
            continue;
        }
        assert_eq!(
            result.unwrap(),
            before.linked_peers[&counterparty],
            "{case}"
        );
        crate::validate_storage_state(&after).unwrap();
        // Only the removed history and the lease bookkeeping differ.
        let mut expected = before.clone();
        expected.private_stream_items.clear();
        expected.next_peer_link_operation_lease_id += 1;
        expected.peer_link_operation_leases.clear();
        assert_eq!(after, expected, "{case}");
    }
}

#[tokio::test]
async fn test_forget_peer_refuses_counterparty_with_payment_history() {
    for case in [
        "request response",
        "allowance message",
        "execution claim",
        "retrieved receipt",
        "accounting occurrence",
        "accounting association",
    ] {
        let peer = random_counterparty();
        let storage = linked_storage(&peer, &[]).await;
        let local = local_identity(&storage.snapshot().unwrap());
        let request_id = PaymentRequestId::new_v4();
        receive(
            &storage,
            &peer,
            vec![
                payment_request_message(EventId::new_v4().as_str(), request_id.as_str(), None),
                receipt_access_message(),
            ],
        )
        .await
        .unwrap();
        match case {
            "request response" => {
                enqueue_payment_request_event(
                    &storage,
                    peer.clone(),
                    &app_id(),
                    &PaymentRequestEvent::Rejection(PaymentRequestRejection::new(
                        EventId::new_v4(),
                        request_id.clone(),
                        None,
                    )),
                    FixedClock.now(),
                )
                .await
                .unwrap();
            }
            "allowance message" => {
                enqueue_private_message(
                    &storage,
                    peer.clone(),
                    allowance_proposal_message(&EventId::new_v4(), &AllowanceId::new_v4(), "1")
                        .raw_json,
                    FixedClock.now(),
                )
                .await
                .unwrap();
            }
            "execution claim" => {
                claim_payment_request_execution(
                    &storage,
                    peer.clone(),
                    &app_id(),
                    &request_id,
                    FixedClock.now(),
                )
                .await
                .unwrap();
            }
            "retrieved receipt" => retrieve_receipt(&storage, &peer, &local).await,
            _ => {
                let mut accounting = empty_accounting();
                let scope = accounting_scope(&local, &peer);
                if case == "accounting occurrence" {
                    // Even a manual-only decision with no attempt is local history.
                    accounting
                        .history
                        .occurrences
                        .push(manual_only_occurrence(scope));
                } else {
                    let allowance_id = AllowanceId::new_v4();
                    accounting
                        .history
                        .watermarks
                        .push(watermark(&local, &peer, &allowance_id));
                    accounting
                        .history
                        .associations
                        .push(crate::AllowanceAssociationRecord {
                            request: scope,
                            revisions: vec![crate::AllowanceAssociationRevision {
                                revision: 1,
                                allowance_id: allowance_id.as_str().into(),
                                effective_from: None,
                                authorization_id: None,
                                authorized_at: FixedClock.now(),
                            }],
                        });
                }
                storage
                    .transaction(|tx| {
                        tx.save_allowance_accounting_state(accounting);
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
        }
        let sdk = forget_test_sdk(&storage);
        sdk.block_peer(peer.clone()).await.unwrap();
        let before = storage.snapshot().unwrap();
        crate::validate_storage_state(&before).unwrap_or_else(|error| panic!("{case}: {error}"));

        let error = sdk.forget_peer(peer.clone()).await.unwrap_err();

        assert!(matches!(error, PaykitSdkError::Policy { .. }), "{case}");
        assert!(!error.is_retention_limit_reached(), "{case}");
        assert_eq!(storage.snapshot().unwrap(), before, "{case}");
    }
}

#[tokio::test]
async fn test_forget_peer_releases_counterparty_at_retention_limit() {
    let flooder = random_counterparty();
    let other = random_counterparty();
    let storage = linked_storage(&flooder, &[&other]).await;
    receive(
        &storage,
        &flooder,
        (0..MAX_RETAINED_PRIVATE_STREAM_ITEMS_PER_COUNTERPARTY)
            .map(future_kind_message)
            .collect(),
    )
    .await
    .unwrap();
    receive(&storage, &other, (0..3).map(future_kind_message).collect())
        .await
        .unwrap();
    let refused = receive(&storage, &flooder, vec![future_kind_message(0)])
        .await
        .unwrap_err();
    assert!(matches!(refused, PaykitSdkError::Policy { .. }));
    assert!(refused.is_retention_limit_reached());
    let sdk = forget_test_sdk(&storage);

    // A counterparty that is still linked keeps its history.
    let linked = storage.snapshot().unwrap();
    let error = sdk.forget_peer(flooder.clone()).await.unwrap_err();
    assert!(matches!(error, PaykitSdkError::Policy { .. }));
    assert_eq!(storage.snapshot().unwrap(), linked);

    sdk.block_peer(flooder.clone()).await.unwrap();
    let blocked = storage.snapshot().unwrap();
    let record = sdk.forget_peer(flooder.clone()).await.unwrap();

    assert_eq!(record.state, LinkedPeerState::Blocked);
    let forgotten = storage.snapshot().unwrap();
    crate::validate_storage_state(&forgotten).unwrap();
    assert!(stream_items_from(&forgotten, &flooder).is_empty());
    assert_eq!(
        stream_items_from(&forgotten, &other),
        stream_items_from(&blocked, &other)
    );
    // Removed ids are never handed out again.
    assert_eq!(
        forgotten.next_private_stream_item_id,
        blocked.next_private_stream_item_id
    );
    assert_eq!(
        forgotten.next_receive_batch_id,
        blocked.next_receive_batch_id
    );

    // Forgetting again has nothing left to remove.
    sdk.forget_peer(flooder.clone()).await.unwrap();
    assert!(stream_items_from(&storage.snapshot().unwrap(), &flooder).is_empty());

    sdk.unblock_peer(flooder.clone()).await.unwrap();
    let report = receive(&storage, &flooder, vec![future_kind_message(0)])
        .await
        .unwrap();
    assert_eq!(
        report.stream_item_ids,
        vec![blocked.next_private_stream_item_id]
    );
    assert_eq!(
        stream_items_from(&storage.snapshot().unwrap(), &flooder).len(),
        1
    );
}

#[tokio::test]
async fn test_forget_peer_removes_only_what_the_counterparty_sent() {
    let peer = random_counterparty();
    let other = random_counterparty();
    let storage = linked_storage(&peer, &[&other]).await;
    let local = local_identity(&storage.snapshot().unwrap());
    let other_request_id = PaymentRequestId::new_v4();
    let peer_allowance_id = AllowanceId::new_v4();
    for counterparty in [&peer, &other] {
        let request_id = if counterparty == &other {
            other_request_id.clone()
        } else {
            PaymentRequestId::new_v4()
        };
        receive(
            &storage,
            counterparty,
            vec![
                payment_request_message(EventId::new_v4().as_str(), request_id.as_str(), None),
                allowance_proposal_message(&EventId::new_v4(), &peer_allowance_id, "1"),
                receipt_access_message(),
                private_list_message("lnbc1received"),
                future_kind_message(0),
            ],
        )
        .await
        .unwrap();
        // Local publications that answer nothing the counterparty sent.
        for raw_json in [
            private_list_json(),
            receipt_access_json(EventId::new_v4().as_str()),
        ] {
            enqueue_private_message(&storage, counterparty.clone(), raw_json, FixedClock.now())
                .await
                .unwrap();
        }
    }
    // Payment history with another counterparty does not hold this one back.
    enqueue_private_message(
        &storage,
        other.clone(),
        allowance_proposal_message(&EventId::new_v4(), &AllowanceId::new_v4(), "1").raw_json,
        FixedClock.now(),
    )
    .await
    .unwrap();
    claim_payment_request_execution(
        &storage,
        other.clone(),
        &app_id(),
        &other_request_id,
        FixedClock.now(),
    )
    .await
    .unwrap();
    retrieve_receipt(&storage, &other, &local).await;
    let mut accounting = empty_accounting();
    accounting
        .history
        .occurrences
        .push(manual_only_occurrence(accounting_scope(&local, &other)));
    // Evaluating the proposal left a watermark, which is not payment history.
    accounting
        .history
        .watermarks
        .push(watermark(&local, &peer, &peer_allowance_id));
    storage
        .transaction(|tx| {
            tx.save_allowance_accounting_state(accounting);
            Ok(())
        })
        .await
        .unwrap();
    let sdk = forget_test_sdk(&storage);
    sdk.block_peer(peer.clone()).await.unwrap();
    let before = storage.snapshot().unwrap();
    crate::validate_storage_state(&before).unwrap();
    assert_eq!(stream_items_from(&before, &peer).len(), 5);
    assert_eq!(before.event_dedup_records.len(), 6);
    assert_eq!(before.receipt_access_records.len(), 2);
    let confirmations = outbound_to(&before, &peer)
        .into_iter()
        .filter(|message| message.is_delivery_confirmation())
        .count();
    assert_eq!(confirmations, 3);

    sdk.forget_peer(peer.clone()).await.unwrap();

    let after = storage.snapshot().unwrap();
    crate::validate_storage_state(&after).unwrap();
    // The other counterparty keeps exactly what it had.
    let mut expected = before.clone();
    expected.next_peer_link_operation_lease_id += 1;
    expected
        .private_stream_items
        .retain(|item| item.counterparty != peer);
    expected.event_dedup_records.retain(|key, _| key.0 != peer);
    expected
        .receipt_access_records
        .retain(|key, _| key.0 != peer);
    // A confirmation must not outlive the Event Message it confirms.
    expected
        .outbound_private_messages
        .retain(|message| message.counterparty != peer || !message.is_delivery_confirmation());
    assert_eq!(after, expected);
    assert_eq!(
        outbound_to(&after, &peer)
            .iter()
            .map(|message| message.kind.as_str())
            .collect::<Vec<_>>(),
        vec![
            PrivateMessageKind::PrivatePaymentList.as_str(),
            PrivateMessageKind::ReceiptAccess.as_str()
        ]
    );
    assert_eq!(stream_items_from(&after, &other).len(), 5);
    assert_eq!(after.receipt_access_records.len(), 1);
    assert_eq!(after.event_dedup_records.len(), 3);
    let (requests, allowances) = storage
        .transaction(|tx| {
            Ok((
                payment_request_records_from_transaction(tx, &peer, FixedClock.now())?,
                allowance_records_in_transaction(tx, &peer),
            ))
        })
        .await
        .unwrap();
    assert!(requests.is_empty());
    assert!(allowances.is_empty());
}

#[tokio::test]
async fn test_forget_peer_resent_proposals_are_new_input() {
    let peer = random_counterparty();
    let storage = linked_storage(&peer, &[]).await;
    let allowance_id = AllowanceId::new_v4();
    let allowance_event_id = EventId::new_v4();
    let request_id = PaymentRequestId::new_v4();
    let request_event_id = EventId::new_v4();
    let proposals = |limit: &str, amount: &str| {
        vec![
            allowance_proposal_message(&allowance_event_id, &allowance_id, limit),
            payment_request_message_for_amount(
                request_event_id.as_str(),
                request_id.as_str(),
                amount,
            ),
        ]
    };
    receive(&storage, &peer, proposals("1", "0.001"))
        .await
        .unwrap();
    // Without forgetting, reusing the ids with other terms is a recorded conflict.
    let conflicting = InMemoryStorage::from_state(storage.snapshot().unwrap());
    let report = receive(&conflicting, &peer, proposals("1000", "5"))
        .await
        .unwrap();
    assert_eq!(report.event_conflicts.len(), 2);

    let sdk = forget_test_sdk(&storage);
    sdk.block_peer(peer.clone()).await.unwrap();
    sdk.forget_peer(peer.clone()).await.unwrap();
    sdk.unblock_peer(peer.clone()).await.unwrap();
    mark_linked(&storage, &peer).await;
    let report = receive(&storage, &peer, proposals("1000", "5"))
        .await
        .unwrap();

    // Nothing local answered the old proposals, so the re-sent ones are plain
    // proposals that still wait for a local decision.
    assert!(report.event_conflicts.is_empty());
    let (allowances, requests) = storage
        .transaction(|tx| {
            Ok((
                allowance_records_in_transaction(tx, &peer),
                payment_request_records_from_transaction(tx, &peer, FixedClock.now())?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(allowances.len(), 1);
    assert_eq!(allowances[0].local_role, Some(AllowanceLocalRole::Allower));
    assert_eq!(
        allowances[0].state,
        crate::AllowanceLifecycleState::Proposed
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].state, PaymentRequestLifecycleState::Proposed);
    assert_eq!(requests[0].accepted_event_id, None);
    assert_eq!(requests[0].execution_claim_app_id, None);
    assert_eq!(requests[0].terms.as_ref().unwrap().amount.value, "5");
}
