use super::*;
use crate::{InMemoryStorage, StorageAdapter};
use chrono::{Duration, TimeZone};

fn time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap()
}
fn path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}
fn public_key() -> crate::PubkyPublicKey {
    crate::PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}
fn checks() -> PaymentExecutionChecks {
    PaymentExecutionChecks {
        trusted_time: time(),
        payment_endpoint_identifier: paykit_lib::PaymentEndpointIdentifier::new(
            "btc-lightning-bolt11",
        )
        .unwrap(),
        actual_amount: paykit_lib::PaymentAmount::new("1", "btc").unwrap(),
        endpoint_current: true,
        local_enabled: true,
        recurrence_eligible: true,
    }
}
fn message(raw_json: String) -> paykit_lib::PrivateApplicationMessage {
    let value: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
    paykit_lib::PrivateApplicationMessage {
        version: Some(1),
        kind: value["kind"].as_str().map(str::to_owned),
        raw_json,
    }
}

struct Fixture {
    storage: InMemoryStorage,
    occurrence: PaymentOccurrence,
    allowance: AllowanceId,
}

impl Fixture {
    async fn new() -> Self {
        let storage = InMemoryStorage::new();
        let peer = public_key();
        let request_id = paykit_lib::PaymentRequestId::new_v4();
        let allowance = AllowanceId::new_v4();
        let proposal_id = new_id();
        let raw_allowance = format!(
            r#"{{"version":1,"kind":"paykit.allowance_proposal","event_id":"{proposal_id}","allowance_id":"{}","proposer_role":"allower","terms":{{"asset":"btc","per_payment_amount":null,"period_limits":[],"lifetime_amount_limit":"1","active_from":null,"expires_at":null,"allowed_payment_endpoint_identifiers":null}}}}"#,
            allowance.as_str()
        );
        let raw_acceptance = format!(
            r#"{{"version":1,"kind":"paykit.allowance_acceptance","event_id":"{}","allowance_id":"{}","proposal_event_id":"{proposal_id}"}}"#,
            new_id(),
            allowance.as_str()
        );
        let raw_request = format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"{}","payment_request_id":"{}","request":{{"amount":{{"value":"1","asset":"btc"}},"payment_reference":"test","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{}}}}}}"#,
            new_id(),
            request_id.as_str()
        );
        storage
            .transaction(|tx| {
                tx.save_identity_state(crate::IdentityState {
                    local_pubky_public_key: Some(public_key()),
                    local_receiver_noise_public_key: Some(public_key()),
                    initialized_at: time(),
                    sign_out_generation: 0,
                });
                let mut linked =
                    crate::domain::linked_peers::default_linked_peer(peer.clone(), path());
                linked.state = crate::LinkedPeerState::Linked;
                tx.save_linked_peer(linked);
                reconcile(
                    tx,
                    &path(),
                    AllowanceAccountingReconciliation {
                        expected_revision: None,
                        history: Default::default(),
                        outcomes: vec![],
                        trusted_time: time(),
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        crate::domain::outbound_private::enqueue_private_message(
            &storage,
            peer.clone(),
            path(),
            raw_allowance,
            time(),
        )
        .await
        .unwrap();
        crate::domain::private_stream::persist_private_stream_batch(
            &storage,
            peer.clone(),
            path(),
            vec![message(raw_acceptance), message(raw_request)],
            None,
            time(),
        )
        .await
        .unwrap();
        let occurrence = PaymentOccurrence {
            request: PaymentRequestScope {
                counterparty: peer,
                counterparty_receiver_path: path(),
                payment_request_id: request_id,
            },
            billing_period: None,
        };
        storage
            .transaction(|tx| {
                select(
                    tx,
                    &path(),
                    occurrence.request.clone(),
                    AllowanceSelectionInput {
                        allowance_id: allowance.clone(),
                        expected_revision: None,
                        trusted_time: time(),
                    },
                    Some(checks()),
                )
            })
            .await
            .unwrap()
            .unwrap();
        Self {
            storage,
            occurrence,
            allowance,
        }
    }

    async fn reserve(&self) -> PaymentAttemptDecision {
        self.storage
            .transaction(|tx| {
                reserve(
                    tx,
                    &path(),
                    self.occurrence.clone(),
                    Some(1),
                    checks(),
                    PaymentExecutionMode::Automatic,
                )
            })
            .await
            .unwrap()
    }
    fn state(&self) -> AllowanceAccountingState {
        self.storage
            .snapshot()
            .unwrap()
            .allowance_accounting
            .unwrap()
    }
}

fn attempt(decision: PaymentAttemptDecision) -> PaymentAttemptRecord {
    match decision {
        PaymentAttemptDecision::Ready { attempt } => attempt,
        other => panic!("Expected Ready: {other:?}"),
    }
}

#[tokio::test]
async fn test_accounting_atomic_manual_and_automatic_exclusion() {
    let fixture = Fixture::new().await;
    let (automatic, manual) = tokio::join!(
        fixture.reserve(),
        fixture.storage.transaction(|tx| reserve(
            tx,
            &path(),
            fixture.occurrence.clone(),
            None,
            checks(),
            PaymentExecutionMode::Manual
        ))
    );
    let decisions = [automatic, manual.unwrap()];
    assert_eq!(
        decisions
            .iter()
            .filter(|d| matches!(d, PaymentAttemptDecision::Ready { .. }))
            .count(),
        1
    );
    assert_eq!(fixture.state().history.occurrences[0].attempts.len(), 1);
}

#[tokio::test]
async fn test_accounting_handoff_only_once_unknown_remains_reserved() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    let submitted = fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id.clone(), checks()))
        .await
        .unwrap();
    assert_eq!(attempt(submitted).status, PaymentExecutionStatus::Submitted);
    let repeated = fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id.clone(), checks()))
        .await
        .unwrap();
    assert!(matches!(
        repeated,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::PaymentAlreadyRecorded
        }
    ));
    fixture
        .storage
        .transaction(|tx| {
            report_outcome(
                tx,
                PaymentOutcomeReport {
                    attempt_id: prepared.attempt_id,
                    outcome: PaymentOutcome::Unknown,
                },
            )
        })
        .await
        .unwrap();
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::PaymentAlreadyRecorded
        }
    ));
}

#[tokio::test]
async fn test_accounting_explicit_failure_releases_but_success_never_does() {
    let fixture = Fixture::new().await;
    let first = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| {
            report_outcome(
                tx,
                PaymentOutcomeReport {
                    attempt_id: first.attempt_id,
                    outcome: PaymentOutcome::Failed,
                },
            )
        })
        .await
        .unwrap();
    let next = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| begin(tx, &path(), next.attempt_id.clone(), checks()))
        .await
        .unwrap();
    fixture
        .storage
        .transaction(|tx| {
            report_outcome(
                tx,
                PaymentOutcomeReport {
                    attempt_id: next.attempt_id.clone(),
                    outcome: PaymentOutcome::Succeeded,
                },
            )
        })
        .await
        .unwrap();
    assert!(fixture
        .storage
        .transaction(|tx| report_outcome(
            tx,
            PaymentOutcomeReport {
                attempt_id: next.attempt_id.clone(),
                outcome: PaymentOutcome::Failed
            }
        ))
        .await
        .is_err());
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Blocked { .. }
    ));
}

#[tokio::test]
async fn test_accounting_blocked_wallet_check_advances_watermark() {
    let fixture = Fixture::new().await;
    let mut rejected = checks();
    rejected.trusted_time += Duration::hours(1);
    rejected.endpoint_current = false;
    fixture
        .storage
        .transaction(|tx| {
            reserve(
                tx,
                &path(),
                fixture.occurrence.clone(),
                Some(1),
                rejected,
                PaymentExecutionMode::Automatic,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        fixture.state().history.watermarks[0].evaluated_at,
        time() + Duration::hours(1)
    );
    assert!(
        matches!(fixture.reserve().await, PaymentAttemptDecision::Blocked { reason: AllowanceAccountingBlock::SharedRule { code } } if code == "clock_rollback")
    );
}

#[tokio::test]
async fn test_accounting_current_ledger_survives_stale_restore_and_empty_reconciliation() {
    let fixture = Fixture::new().await;
    let old = fixture.state();
    let prepared = attempt(fixture.reserve().await);
    let merged = merge_restored_accounting(Some(fixture.state()), Some(old))
        .unwrap()
        .unwrap();
    assert!(merged.requires_reconciliation);
    assert_eq!(
        merged.history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Unknown
    );
    fixture
        .storage
        .transaction(|tx| {
            tx.save_allowance_accounting_state(merged.clone());
            Ok(())
        })
        .await
        .unwrap();
    fixture
        .storage
        .transaction(|tx| {
            reconcile(
                tx,
                &path(),
                AllowanceAccountingReconciliation {
                    expected_revision: Some(merged.revision),
                    history: Default::default(),
                    outcomes: vec![],
                    trusted_time: time(),
                },
            )
        })
        .await
        .unwrap();
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::PaymentAlreadyRecorded
        }
    ));
    assert_eq!(
        fixture.state().history.occurrences[0].attempts[0].attempt_id,
        prepared.attempt_id
    );
}

#[tokio::test]
async fn test_accounting_noise_rotation_retains_evidence_and_invalidates_preparation() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| {
            tx.clear_private_identity_scoped_state();
            Ok(())
        })
        .await
        .unwrap();
    let state = fixture.state();
    assert!(state.requires_reconciliation);
    assert_ne!(state.epoch, prepared.epoch);
    assert_eq!(
        state.history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Unknown
    );
}

#[tokio::test]
async fn test_accounting_deferred_retries_and_sticky_manual_only() {
    let fixture = Fixture::new().await;
    fixture
        .storage
        .transaction(|tx| {
            set_disposition(
                tx,
                &path(),
                fixture.occurrence.clone(),
                PaymentDisposition::Deferred {
                    reason: "endpoint unavailable".into(),
                },
            )
        })
        .await
        .unwrap();
    fixture
        .storage
        .transaction(|tx| {
            set_disposition(
                tx,
                &path(),
                fixture.occurrence.clone(),
                PaymentDisposition::ManualOnly,
            )
        })
        .await
        .unwrap();
    assert!(fixture
        .storage
        .transaction(|tx| set_disposition(
            tx,
            &path(),
            fixture.occurrence.clone(),
            PaymentDisposition::Deferred {
                reason: "retry".into()
            }
        ))
        .await
        .is_err());
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::ManualOnly
        }
    ));
    assert!(!format!(
        "{:?}",
        PaymentDisposition::Deferred {
            reason: "private sentinel".into()
        }
    )
    .contains("sentinel"));
}

#[tokio::test]
async fn test_accounting_validation_rejects_missing_watermark_and_invalid_amount() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    let mut invalid = fixture.state();
    invalid.history.watermarks.clear();
    assert!(validate_accounting(&invalid).is_err());
    let mut invalid = fixture.state();
    invalid.history.occurrences[0].attempts[0].amount.value = "NaN".into();
    assert!(validate_accounting(&invalid).is_err());
    let mut invalid = fixture.state();
    invalid.history.occurrences[0].attempts[0].allowance_id =
        Some(AllowanceId::new_v4().as_str().into());
    assert!(validate_accounting(&invalid).is_err());
}

#[tokio::test]
async fn test_accounting_actual_decimal_equivalence_uses_shared_math() {
    let fixture = Fixture::new().await;
    let mut equivalent = checks();
    equivalent.actual_amount = paykit_lib::PaymentAmount::new("001.00", "btc").unwrap();
    let decision = fixture
        .storage
        .transaction(|tx| {
            reserve(
                tx,
                &path(),
                fixture.occurrence.clone(),
                Some(1),
                equivalent,
                PaymentExecutionMode::Automatic,
            )
        })
        .await
        .unwrap();
    assert!(matches!(decision, PaymentAttemptDecision::Ready { .. }));
    let record = fixture
        .storage
        .transaction(|tx| {
            request(
                tx,
                &scope(tx, &path(), &fixture.occurrence.request)?,
                time(),
            )
        })
        .await
        .unwrap();
    let terms = request_terms(&record).unwrap();
    let mut unequal = checks();
    unequal.actual_amount = paykit_lib::PaymentAmount::new("1.01", "btc").unwrap();
    assert!(!execution::wallet_checks(&terms, &unequal));
}

#[tokio::test]
async fn test_accounting_reservation_consumes_capacity_across_distinct_requests() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    let mut second = fixture.occurrence.clone();
    second.request.payment_request_id = paykit_lib::PaymentRequestId::new_v4();
    let snapshot = fixture.storage.snapshot().unwrap();
    let original = snapshot
        .private_stream_items
        .iter()
        .find(|i| i.raw_json.contains("paykit.payment_request\""))
        .unwrap();
    let mut raw: serde_json::Value = serde_json::from_str(&original.raw_json).unwrap();
    raw["event_id"] = new_id().into();
    raw["payment_request_id"] = second.request.payment_request_id.as_str().into();
    crate::domain::private_stream::persist_private_stream_batch(
        &fixture.storage,
        second.request.counterparty.clone(),
        path(),
        vec![message(raw.to_string())],
        None,
        time(),
    )
    .await
    .unwrap();
    fixture
        .storage
        .transaction(|tx| {
            select(
                tx,
                &path(),
                second.request.clone(),
                AllowanceSelectionInput {
                    allowance_id: fixture.allowance.clone(),
                    expected_revision: None,
                    trusted_time: time(),
                },
                Some(checks()),
            )
        })
        .await
        .unwrap()
        .unwrap();
    let decision = fixture
        .storage
        .transaction(|tx| {
            reserve(
                tx,
                &path(),
                second.clone(),
                Some(1),
                checks(),
                PaymentExecutionMode::Automatic,
            )
        })
        .await
        .unwrap();
    assert!(
        matches!(decision, PaymentAttemptDecision::Blocked { reason: AllowanceAccountingBlock::SharedRule { code } } if code == "lifetime_amount_limit")
    );
}

#[tokio::test]
async fn test_accounting_pending_execution_cannot_be_labeled_deferred() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id, checks()))
        .await
        .unwrap();
    for disposition in [
        PaymentDisposition::Deferred {
            reason: "timeout".into(),
        },
        PaymentDisposition::ManualOnly,
    ] {
        assert!(fixture
            .storage
            .transaction(|tx| set_disposition(tx, &path(), fixture.occurrence.clone(), disposition))
            .await
            .is_err());
    }
    assert_eq!(
        fixture.state().history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Submitted
    );
}

#[tokio::test]
async fn test_accounting_restore_failure_is_atomic_and_missing_ledger_stays_blocked() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    let before = fixture.storage.snapshot().unwrap();
    let mut backup = crate::export_backup_state(&fixture.storage, path())
        .await
        .unwrap();
    backup
        .allowance_accounting
        .as_mut()
        .unwrap()
        .history
        .watermarks
        .clear();
    assert!(crate::backup::restore_backup_state_with_identity(
        &fixture.storage,
        backup,
        path(),
        None
    )
    .await
    .is_err());
    assert_eq!(fixture.storage.snapshot().unwrap(), before);
    let empty = InMemoryStorage::new();
    let decision = empty
        .transaction(|tx| {
            tx.save_identity_state(before.identity_state.clone().unwrap());
            reserve(
                tx,
                &path(),
                fixture.occurrence.clone(),
                Some(1),
                checks(),
                PaymentExecutionMode::Automatic,
            )
        })
        .await
        .unwrap();
    assert!(matches!(
        decision,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::ReconciliationRequired
        }
    ));
}

#[tokio::test]
async fn test_accounting_reconciliation_requires_explicit_outcome_to_release_unknown() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| {
            report_outcome(
                tx,
                PaymentOutcomeReport {
                    attempt_id: prepared.attempt_id.clone(),
                    outcome: PaymentOutcome::Unknown,
                },
            )
        })
        .await
        .unwrap();
    let current = fixture.state();
    let mut recovered = current.history.clone();
    recovered.occurrences[0].attempts[0].status = PaymentExecutionStatus::Failed;
    fixture
        .storage
        .transaction(|tx| {
            reconcile(
                tx,
                &path(),
                AllowanceAccountingReconciliation {
                    expected_revision: Some(current.revision),
                    history: recovered,
                    outcomes: vec![],
                    trusted_time: time(),
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(
        fixture.state().history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Unknown
    );
    let revision = fixture.state().revision;
    fixture
        .storage
        .transaction(|tx| {
            reconcile(
                tx,
                &path(),
                AllowanceAccountingReconciliation {
                    expected_revision: Some(revision),
                    history: Default::default(),
                    outcomes: vec![PaymentOutcomeReport {
                        attempt_id: prepared.attempt_id,
                        outcome: PaymentOutcome::Failed,
                    }],
                    trusted_time: time(),
                },
            )
        })
        .await
        .unwrap();
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Ready { .. }
    ));
}

#[tokio::test]
async fn test_accounting_noncanonical_restored_ids_cannot_bypass_occurrence_exclusion() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    for field in 0..6 {
        let mut invalid = fixture.state();
        match field {
            0 => invalid.history.occurrences[0]
                .key
                .request
                .payment_request_id
                .make_ascii_uppercase(),
            1 => invalid.history.occurrences[0].attempts[0]
                .attempt_id
                .make_ascii_uppercase(),
            2 => invalid.history.occurrences[0].attempts[0]
                .epoch
                .make_ascii_uppercase(),
            3 => invalid.history.associations[0].revisions[0]
                .allowance_id
                .make_ascii_uppercase(),
            4 => invalid.history.watermarks[0]
                .allowance_id
                .make_ascii_uppercase(),
            _ => invalid.epoch.make_ascii_uppercase(),
        }
        assert!(validate_accounting(&invalid).is_err(), "field {field}");
    }
}

#[tokio::test]
async fn test_accounting_restore_identity_switch_does_not_merge_previous_payer() {
    let fixture = Fixture::new().await;
    attempt(fixture.reserve().await);
    let other = InMemoryStorage::new();
    let identity = crate::IdentityState {
        local_pubky_public_key: Some(public_key()),
        local_receiver_noise_public_key: Some(public_key()),
        initialized_at: time(),
        sign_out_generation: 1,
    };
    other.save_identity_state(identity.clone()).await.unwrap();
    let backup = crate::export_backup_state(&other, path()).await.unwrap();
    crate::backup::restore_backup_state_with_identity(
        &fixture.storage,
        backup,
        path(),
        Some(identity.clone()),
    )
    .await
    .unwrap();
    let state = fixture.storage.snapshot().unwrap();
    assert_eq!(
        state.identity_state.unwrap().local_pubky_public_key,
        identity.local_pubky_public_key
    );
    assert!(state.allowance_accounting.is_none());
}

#[tokio::test]
async fn test_accounting_reconciliation_rejects_foreign_retained_ledger() {
    let fixture = Fixture::new().await;
    let current = fixture.state();
    fixture
        .storage
        .transaction(|tx| {
            let mut identity = tx.load_identity_state().unwrap();
            identity.local_pubky_public_key = Some(public_key());
            tx.save_identity_state(identity);
            Ok(())
        })
        .await
        .unwrap();
    let before = fixture.storage.snapshot().unwrap();
    assert!(fixture
        .storage
        .transaction(|tx| reconcile(
            tx,
            &path(),
            AllowanceAccountingReconciliation {
                expected_revision: Some(current.revision),
                history: Default::default(),
                outcomes: vec![],
                trusted_time: time()
            }
        ))
        .await
        .is_err());
    assert_eq!(fixture.storage.snapshot().unwrap(), before);
}

#[tokio::test]
async fn test_accounting_corrupt_loaded_history_fails_without_mutation_or_handoff() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| {
            let mut state = tx.allowance_accounting_state().unwrap();
            state.history.associations[0].revisions.clear();
            tx.save_allowance_accounting_state(state);
            Ok(())
        })
        .await
        .unwrap();
    let before = fixture.storage.snapshot().unwrap();
    assert!(fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id, checks()))
        .await
        .is_err());
    assert!(fixture
        .storage
        .transaction(|tx| select(
            tx,
            &path(),
            fixture.occurrence.request.clone(),
            AllowanceSelectionInput {
                allowance_id: fixture.allowance.clone(),
                expected_revision: Some(1),
                trusted_time: time()
            },
            None
        ))
        .await
        .is_err());
    assert_eq!(fixture.storage.snapshot().unwrap(), before);
}

#[tokio::test]
async fn test_accounting_manual_response_revokes_prepared_but_retains_submitted_usage() {
    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| manual_response(tx, &path(), fixture.occurrence.request.clone()))
        .await
        .unwrap();
    assert_eq!(
        fixture.state().history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Failed
    );
    assert!(matches!(
        fixture
            .storage
            .transaction(|tx| begin(tx, &path(), prepared.attempt_id, checks()))
            .await
            .unwrap(),
        PaymentAttemptDecision::Blocked { .. }
    ));
    assert!(matches!(
        fixture.reserve().await,
        PaymentAttemptDecision::Blocked {
            reason: AllowanceAccountingBlock::ManualOnly
        }
    ));

    let fixture = Fixture::new().await;
    let prepared = attempt(fixture.reserve().await);
    fixture
        .storage
        .transaction(|tx| begin(tx, &path(), prepared.attempt_id, checks()))
        .await
        .unwrap();
    fixture
        .storage
        .transaction(|tx| manual_response(tx, &path(), fixture.occurrence.request.clone()))
        .await
        .unwrap();
    assert_eq!(
        fixture.state().history.occurrences[0].attempts[0].status,
        PaymentExecutionStatus::Submitted
    );
}

#[tokio::test]
async fn test_accounting_successful_reconsideration_replaces_deferred_disposition() {
    for manual in [false, true] {
        let fixture = Fixture::new().await;
        fixture
            .storage
            .transaction(|tx| {
                set_disposition(
                    tx,
                    &path(),
                    fixture.occurrence.clone(),
                    PaymentDisposition::Deferred {
                        reason: "temporary endpoint failure".into(),
                    },
                )
            })
            .await
            .unwrap();
        let decision = fixture
            .storage
            .transaction(|tx| {
                reserve(
                    tx,
                    &path(),
                    fixture.occurrence.clone(),
                    Some(1),
                    checks(),
                    if manual {
                        PaymentExecutionMode::Manual
                    } else {
                        PaymentExecutionMode::Automatic
                    },
                )
            })
            .await
            .unwrap();
        let prepared = attempt(decision);
        let expected = if manual {
            PaymentDisposition::ManualOnly
        } else {
            PaymentDisposition::Automatic
        };
        assert_eq!(fixture.state().history.occurrences[0].disposition, expected);
        fixture
            .storage
            .transaction(|tx| {
                report_outcome(
                    tx,
                    PaymentOutcomeReport {
                        attempt_id: prepared.attempt_id,
                        outcome: PaymentOutcome::Failed,
                    },
                )
            })
            .await
            .unwrap();
        assert_eq!(fixture.state().history.occurrences[0].disposition, expected);
    }
}
