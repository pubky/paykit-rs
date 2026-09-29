//! Real Encrypted Link lifecycle evidence with explicit wallet outcome fixtures.
//! No payment rail is invoked: the SDK records durable admission and handoff.

use super::*;
use async_trait::async_trait;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use paykit_lib::{
    BillingPeriod, PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms, Recurrence, RecurrenceConfig, RecurrenceUnit,
};
use paykit_sdk::{
    AllowanceAccountingBlock, AllowanceAccountingReconciliation, AllowanceReassociationInput,
    AllowanceSelectionInput, PaymentAttemptDecision, PaymentAttemptRecord, PaymentDisposition,
    PaymentExecutionChecks, PaymentExecutionStatus, PaymentOccurrence, PaymentOutcome,
    PaymentOutcomeReport, PaymentRequestLifecycleState, PaymentRequestScope,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Notify;

fn endpoint() -> PaymentEndpointIdentifier {
    PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()
}

fn checks(trusted_time: DateTime<Utc>) -> PaymentExecutionChecks {
    PaymentExecutionChecks {
        trusted_time,
        payment_endpoint_identifier: endpoint(),
        actual_amount: PaymentAmount::new("0.001", "btc").unwrap(),
        endpoint_current: true,
        local_enabled: true,
        recurrence_eligible: true,
    }
}

async fn accepted_allowance(payer: &TestUser, payee: &TestUser) -> AllowanceId {
    let record = payer
        .sdk
        .propose_allowance(
            payee.public_key.clone(),
            payee.receiver_path.clone(),
            AllowanceLocalRole::Allower,
            terms(),
        )
        .await
        .unwrap();
    let id = AllowanceId::new(record.allowance_id).unwrap();
    deliver(payer, payee).await;
    payee
        .sdk
        .accept_allowance(payer.public_key.clone(), payer.receiver_path.clone(), &id)
        .await
        .unwrap();
    deliver(payee, payer).await;
    id
}

async fn proposed_request(
    payer: &TestUser,
    payee: &TestUser,
    recurrence: Option<Recurrence>,
) -> PaymentRequestScope {
    let terms = PaymentRequestTerms::builder(
        PaymentAmount::new("0.001", "btc").unwrap(),
        PaymentReference::new("accounting-invoice").unwrap(),
        vec![endpoint()],
    )
    .recurrence(recurrence)
    .build()
    .unwrap();
    let record = payee
        .sdk
        .propose_payment_request(payer.public_key.clone(), payer.receiver_path.clone(), terms)
        .await
        .unwrap();
    deliver(payee, payer).await;
    PaymentRequestScope {
        counterparty: payee.public_key.clone(),
        counterparty_receiver_path: payee.receiver_path.clone(),
        payment_request_id: PaymentRequestId::new(record.payment_request_id).unwrap(),
    }
}

fn ready(
    decision: PaymentAttemptDecision,
    expected: PaymentExecutionStatus,
) -> PaymentAttemptRecord {
    let PaymentAttemptDecision::Ready { attempt } = decision else {
        panic!("expected durable payment admission or handoff: {decision:?}");
    };
    assert_eq!(attempt.status, expected);
    attempt
}

fn blocked(decision: PaymentAttemptDecision, expected: AllowanceAccountingBlock) {
    assert!(matches!(decision, PaymentAttemptDecision::Blocked { reason } if reason == expected));
}

fn utc_text(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn occurrence(scope: &PaymentRequestScope, start: DateTime<Utc>) -> PaymentOccurrence {
    PaymentOccurrence {
        request: scope.clone(),
        billing_period: Some(
            BillingPeriod::new(utc_text(start), utc_text(start + Duration::days(1))).unwrap(),
        ),
    }
}

async fn initialize_accounting(payer: &TestUser, trusted_time: DateTime<Utc>) {
    payer
        .sdk
        .reconcile_allowance_accounting(AllowanceAccountingReconciliation {
            expected_revision: None,
            history: Default::default(),
            outcomes: vec![],
            trusted_time,
        })
        .await
        .unwrap();
}

async fn select_and_accept(
    payer: &TestUser,
    scope: &PaymentRequestScope,
    selection: AllowanceSelectionInput,
) {
    let candidates = payer
        .sdk
        .evaluate_allowance_candidates(scope.clone(), selection.trusted_time)
        .await
        .unwrap();
    assert!(candidates.iter().any(|candidate| {
        candidate.allowance_id == selection.allowance_id.as_str() && candidate.blocked.is_none()
    }));
    let association = payer
        .sdk
        .select_allowance(scope.clone(), selection.clone())
        .await
        .unwrap();
    assert_eq!(association.revisions.len(), 1);
    let revision = association.revisions[0].revision;
    assert_eq!(revision, 1);
    let accepted = payer
        .sdk
        .accept_payment_request_automatically(
            scope.clone(),
            AllowanceSelectionInput {
                expected_revision: Some(revision),
                ..selection.clone()
            },
            checks(selection.trusted_time),
        )
        .await
        .unwrap();
    assert_eq!(accepted, association);
}

#[tokio::test]
async fn test_allowance_accounting_handoff_restart_and_stale_restore() {
    let pair = linked_two_party().await;
    let id = accepted_allowance(&pair.alice, &pair.bob).await;
    let scope = proposed_request(&pair.alice, &pair.bob, None).await;
    let time = Utc::now();
    initialize_accounting(&pair.alice, time).await;
    select_and_accept(
        &pair.alice,
        &scope,
        AllowanceSelectionInput {
            allowance_id: id.clone(),
            expected_revision: None,
            trusted_time: time,
        },
    )
    .await;
    deliver(&pair.alice, &pair.bob).await;
    let received = pair
        .bob
        .sdk
        .payment_requests_with(&pair.alice.public_key, &pair.alice.receiver_path)
        .await
        .unwrap();
    assert_eq!(received[0].state, PaymentRequestLifecycleState::Accepted);
    let occurrence = PaymentOccurrence {
        request: scope,
        billing_period: None,
    };
    let prepared = ready(
        pair.alice
            .sdk
            .reserve_automatic_payment(occurrence.clone(), 1, checks(time))
            .await
            .unwrap(),
        PaymentExecutionStatus::Prepared,
    );
    assert_eq!(prepared.allowance_id.as_deref(), Some(id.as_str()));
    let stale_backup = pair.alice.sdk.export_backup_state().await.unwrap();
    let submitted = ready(
        pair.alice
            .sdk
            .begin_payment_execution(prepared.attempt_id.clone(), checks(time))
            .await
            .unwrap(),
        PaymentExecutionStatus::Submitted,
    );
    assert_eq!(submitted.attempt_id, prepared.attempt_id);
    let unknown = pair
        .alice
        .sdk
        .record_payment_outcome(PaymentOutcomeReport {
            attempt_id: prepared.attempt_id.clone(),
            outcome: PaymentOutcome::Unknown,
        })
        .await
        .unwrap();
    assert_eq!(unknown.status, PaymentExecutionStatus::Unknown);

    let restarted = pair
        .alice
        .restart_with_storage(pair.alice.storage.clone())
        .await;
    blocked(
        restarted
            .sdk
            .reserve_automatic_payment(occurrence.clone(), 1, checks(time))
            .await
            .unwrap(),
        AllowanceAccountingBlock::PaymentAlreadyRecorded,
    );
    blocked(
        restarted
            .sdk
            .reserve_manual_payment(occurrence.clone(), checks(time))
            .await
            .unwrap(),
        AllowanceAccountingBlock::PaymentAlreadyRecorded,
    );

    // Restore the pre-handoff checkpoint on a fresh runtime. Its Prepared
    // record is stale evidence: the wallet must reconcile the external attempt.
    let restored = restarted.restart_with_storage(InMemoryStorage::new()).await;
    restored
        .sdk
        .restore_backup_state(stale_backup)
        .await
        .unwrap();
    let state = restored
        .sdk
        .allowance_accounting_state()
        .await
        .unwrap()
        .unwrap();
    assert!(state.requires_reconciliation);
    assert_ne!(state.epoch, prepared.epoch);
    blocked(
        restored
            .sdk
            .reserve_automatic_payment(occurrence.clone(), 1, checks(time))
            .await
            .unwrap(),
        AllowanceAccountingBlock::ReconciliationRequired,
    );
    blocked(
        restored
            .sdk
            .begin_payment_execution(prepared.attempt_id.clone(), checks(time))
            .await
            .unwrap(),
        AllowanceAccountingBlock::ReconciliationRequired,
    );

    let reconciled = restored
        .sdk
        .reconcile_allowance_accounting(AllowanceAccountingReconciliation {
            expected_revision: Some(state.revision),
            history: state.history,
            outcomes: vec![PaymentOutcomeReport {
                attempt_id: prepared.attempt_id.clone(),
                outcome: PaymentOutcome::Succeeded,
            }],
            trusted_time: time,
        })
        .await
        .unwrap();
    assert!(!reconciled.requires_reconciliation);
    let attempt = &reconciled.history.occurrences[0].attempts[0];
    assert_eq!(attempt.status, PaymentExecutionStatus::Succeeded);
    assert_eq!(attempt.allowance_id, prepared.allowance_id);
    assert_eq!(attempt.admitted_at, prepared.admitted_at);
    blocked(
        restored
            .sdk
            .reserve_automatic_payment(occurrence.clone(), 1, checks(time))
            .await
            .unwrap(),
        AllowanceAccountingBlock::PaymentAlreadyRecorded,
    );
    blocked(
        restored
            .sdk
            .reserve_manual_payment(occurrence, checks(time))
            .await
            .unwrap(),
        AllowanceAccountingBlock::PaymentAlreadyRecorded,
    );
    let repeated = restored
        .sdk
        .record_payment_outcome(PaymentOutcomeReport {
            attempt_id: prepared.attempt_id,
            outcome: PaymentOutcome::Succeeded,
        })
        .await
        .unwrap();
    assert_eq!(repeated.status, PaymentExecutionStatus::Succeeded);
    let final_state = restored
        .sdk
        .allowance_accounting_state()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_state.history.occurrences[0].attempts.len(), 1);
}

#[tokio::test]
async fn test_allowance_accounting_future_reassociation_preserves_prior_payment() {
    let pair = linked_two_party().await;
    let old_id = accepted_allowance(&pair.alice, &pair.bob).await;
    let replacement = accepted_allowance(&pair.alice, &pair.bob).await;
    let time: DateTime<Utc> = utc_text(Utc::now()).parse().unwrap();
    let recurrence = Recurrence::try_from(RecurrenceConfig {
        every: 1,
        unit: RecurrenceUnit::Day,
        starts_at: utc_text(time),
        anchor: utc_text(time),
        ends_at: None,
    })
    .unwrap();
    let scope = proposed_request(&pair.alice, &pair.bob, Some(recurrence)).await;
    initialize_accounting(&pair.alice, time).await;
    select_and_accept(
        &pair.alice,
        &scope,
        AllowanceSelectionInput {
            allowance_id: old_id.clone(),
            expected_revision: None,
            trusted_time: time,
        },
    )
    .await;
    deliver(&pair.alice, &pair.bob).await;
    let first = occurrence(&scope, time);
    let second = occurrence(&scope, time + Duration::days(1));
    let third = occurrence(&scope, time + Duration::days(2));
    let prepared = ready(
        pair.alice
            .sdk
            .reserve_automatic_payment(first.clone(), 1, checks(time))
            .await
            .unwrap(),
        PaymentExecutionStatus::Prepared,
    );
    ready(
        pair.alice
            .sdk
            .begin_payment_execution(prepared.attempt_id.clone(), checks(time))
            .await
            .unwrap(),
        PaymentExecutionStatus::Submitted,
    );
    pair.alice
        .sdk
        .mark_payment_manual_only(third.clone())
        .await
        .unwrap();
    let association = pair
        .alice
        .sdk
        .authorize_allowance_reassociation(
            scope,
            AllowanceReassociationInput {
                allowance_id: replacement.clone(),
                expected_revision: 1,
                effective_from: time + Duration::days(1),
                authorization_id: paykit_lib::EventId::new_v4().as_str().to_owned(),
                trusted_time: time,
            },
        )
        .await
        .unwrap();
    assert_eq!(association.revisions.len(), 2);
    assert_eq!(association.revisions[0].allowance_id, old_id.as_str());
    assert_eq!(association.revisions[1].allowance_id, replacement.as_str());

    let later = ready(
        pair.alice
            .sdk
            .reserve_automatic_payment(second, 2, checks(time + Duration::days(1)))
            .await
            .unwrap(),
        PaymentExecutionStatus::Prepared,
    );
    assert_eq!(later.allowance_id.as_deref(), Some(replacement.as_str()));
    let state = pair
        .alice
        .sdk
        .allowance_accounting_state()
        .await
        .unwrap()
        .unwrap();
    let old_attempt = state
        .history
        .occurrences
        .iter()
        .flat_map(|item| &item.attempts)
        .find(|attempt| attempt.attempt_id == prepared.attempt_id)
        .unwrap();
    assert_eq!(old_attempt.status, PaymentExecutionStatus::Submitted);
    assert_eq!(old_attempt.allowance_id.as_deref(), Some(old_id.as_str()));
    assert_eq!(old_attempt.association_revision, Some(1));
    blocked(
        pair.alice
            .sdk
            .reserve_manual_payment(first, checks(time + Duration::days(1)))
            .await
            .unwrap(),
        AllowanceAccountingBlock::PaymentAlreadyRecorded,
    );
    blocked(
        pair.alice
            .sdk
            .reserve_automatic_payment(third, 2, checks(time + Duration::days(2)))
            .await
            .unwrap(),
        AllowanceAccountingBlock::ManualOnly,
    );
    let committed = pair
        .alice
        .sdk
        .record_payment_outcome(PaymentOutcomeReport {
            attempt_id: prepared.attempt_id,
            outcome: PaymentOutcome::Succeeded,
        })
        .await
        .unwrap();
    assert_eq!(committed.allowance_id.as_deref(), Some(old_id.as_str()));
    assert_eq!(committed.admitted_at, time);
    let state = pair
        .alice
        .sdk
        .allowance_accounting_state()
        .await
        .unwrap()
        .unwrap();
    assert!(state
        .history
        .occurrences
        .iter()
        .any(|item| item.disposition == PaymentDisposition::ManualOnly));
    assert_eq!(
        state
            .history
            .occurrences
            .iter()
            .map(|item| item.attempts.len())
            .sum::<usize>(),
        2
    );
}

struct PausedSessionProvider {
    inner: crate::harness::TestnetSessionProvider,
    loads: AtomicUsize,
    pause: bool,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl paykit_sdk::PubkySessionProvider for PausedSessionProvider {
    async fn load_session_access(
        &self,
    ) -> paykit_sdk::Result<Option<paykit_sdk::PubkySessionAccess>> {
        // The second session read is after manual Acceptance's initial Proposed
        // read, or before automatic Acceptance's transaction. Suspend only the
        // session provider so the competing command can use the same storage.
        if self.pause && self.loads.fetch_add(1, Ordering::SeqCst) == 1 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.load_session_access().await
    }

    async fn load_public_storage(&self) -> paykit_sdk::Result<Option<pubky::PublicStorage>> {
        self.inner.load_public_storage().await
    }

    async fn clear_session_access(&self) -> paykit_sdk::Result<()> {
        self.inner.clear_session_access().await
    }
}

#[tokio::test]
async fn test_allowance_accounting_manual_and_automatic_acceptance_interleave() {
    let pair = linked_two_party().await;
    let id = accepted_allowance(&pair.alice, &pair.bob).await;
    initialize_accounting(&pair.alice, Utc::now()).await;
    for pause_manual in [true, false] {
        let scope = proposed_request(&pair.alice, &pair.bob, None).await;
        let time = Utc::now();
        pair.alice
            .sdk
            .select_allowance(
                scope.clone(),
                AllowanceSelectionInput {
                    allowance_id: id.clone(),
                    expected_revision: None,
                    trusted_time: time,
                },
            )
            .await
            .unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let runtime = |pause| {
            paykit_sdk::PaykitSdk::new(
                pair.alice.storage.clone(),
                PausedSessionProvider {
                    inner: crate::harness::TestnetSessionProvider::new(
                        pair.alice.access.clone(),
                        pair.alice.session_secret.clone(),
                    ),
                    loads: AtomicUsize::new(0),
                    pause,
                    entered: entered.clone(),
                    release: release.clone(),
                },
                pair.alice.adapter.clone(),
                paykit_sdk::PaykitSdkConfig::new(pair.alice.receiver_path.clone()),
            )
            .unwrap()
        };
        let manual_sdk = runtime(pause_manual);
        let automatic_sdk = runtime(!pause_manual);
        let manual = async {
            if !pause_manual {
                entered.notified().await;
            }
            let result = manual_sdk
                .accept_payment_request(
                    scope.counterparty.clone(),
                    scope.counterparty_receiver_path.clone(),
                    &scope.payment_request_id,
                )
                .await;
            if !pause_manual {
                release.notify_one();
            }
            result
        };
        let automatic = async {
            if pause_manual {
                entered.notified().await;
            }
            let result = automatic_sdk
                .accept_payment_request_automatically(
                    scope.clone(),
                    AllowanceSelectionInput {
                        allowance_id: id.clone(),
                        expected_revision: Some(1),
                        trusted_time: time,
                    },
                    checks(time),
                )
                .await;
            if pause_manual {
                release.notify_one();
            }
            result
        };
        let (manual, automatic) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            tokio::join!(manual, automatic)
        })
        .await
        .expect("Acceptance interleaving must complete");
        assert_eq!(manual.is_ok(), !pause_manual, "manual result: {manual:?}");
        assert_eq!(
            automatic.is_ok(),
            pause_manual,
            "automatic result: {automatic:?}"
        );
        let backup = pair.alice.sdk.export_backup_state().await.unwrap();
        let acceptance_count = backup
            .outbound_private_messages
            .iter()
            .filter(|message| {
                message.kind == "paykit.payment_request_acceptance"
                    && serde_json::from_str::<serde_json::Value>(&message.raw_json).unwrap()
                        ["payment_request_id"]
                        == scope.payment_request_id.as_str()
            })
            .count();
        assert_eq!(acceptance_count, 1);
        deliver(&pair.alice, &pair.bob).await;
        for (local, peer) in [(&pair.alice, &pair.bob), (&pair.bob, &pair.alice)] {
            let records = local
                .sdk
                .payment_requests_with(&peer.public_key, &peer.receiver_path)
                .await
                .unwrap();
            let record = records
                .iter()
                .find(|record| record.payment_request_id == scope.payment_request_id.as_str())
                .unwrap();
            assert_eq!(record.state, PaymentRequestLifecycleState::Accepted);
            assert!(record.invalid_reason.is_none());
        }
        let decision = pair
            .alice
            .sdk
            .reserve_automatic_payment(
                PaymentOccurrence {
                    request: scope,
                    billing_period: None,
                },
                1,
                checks(time),
            )
            .await
            .unwrap();
        if pause_manual {
            ready(decision, PaymentExecutionStatus::Prepared);
        } else {
            blocked(decision, AllowanceAccountingBlock::ManualOnly);
        }
    }
}
