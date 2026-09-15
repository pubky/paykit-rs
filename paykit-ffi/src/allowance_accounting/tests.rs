use super::*;
use crate::storage::{
    decode_backup_state, decode_storage_state, encode_backup_state, encode_storage_state,
    FfiSdkStorage,
};
use crate::{FfiBillingPeriod, FfiSdkStateBlob, FfiSdkStateBlobSnapshot, FfiSdkStateBlobStore};
use paykit_sdk as sdk;
use sdk::storage::{StorageAdapter, StorageState};
use std::{
    any::Any,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

const ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";
const TIME: &str = "2026-09-15T12:00:00.123456789Z";
const KEY: &str = "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io";

fn amount() -> Arc<FfiAccountingAmount> {
    Arc::new(FfiAccountingAmount::new("001.230000000000000000000001".into(), "usd".into()).unwrap())
}
fn scope() -> FfiPaymentRequestScope {
    FfiPaymentRequestScope {
        counterparty: KEY.into(),
        counterparty_receiver_path: "bitkit/wallet".into(),
        payment_request_id: ID.into(),
    }
}
fn checks() -> FfiPaymentExecutionChecks {
    FfiPaymentExecutionChecks {
        trusted_time: TIME.into(),
        payment_endpoint_identifier: "btc-lightning-bolt11".into(),
        actual_amount: amount(),
        endpoint_current: true,
        local_enabled: true,
        recurrence_eligible: true,
    }
}
fn ledger() -> sdk::AllowanceAccountingState {
    let scope: sdk::PaymentRequestScope = scope().try_into().unwrap();
    let request = sdk::PaymentAccountingScope {
        local_public_key: scope.counterparty.clone(),
        local_receiver_path: scope.counterparty_receiver_path.clone(),
        counterparty: scope.counterparty,
        counterparty_receiver_path: scope.counterparty_receiver_path,
        payment_request_id: ID.into(),
    };
    let now = conversions::parse_time(TIME.into()).unwrap();
    let mut attempts = Vec::new();
    for status in [
        sdk::PaymentExecutionStatus::Prepared,
        sdk::PaymentExecutionStatus::Submitted,
        sdk::PaymentExecutionStatus::Unknown,
        sdk::PaymentExecutionStatus::Succeeded,
        sdk::PaymentExecutionStatus::Failed,
    ] {
        attempts.push(sdk::PaymentAttemptRecord {
            attempt_id: paykit_lib::EventId::new_v4().as_str().into(),
            mode: sdk::PaymentExecutionMode::Automatic,
            allowance_id: Some(ID.into()),
            association_revision: Some(1),
            amount: sdk::AmountRecord {
                value: amount().value(),
                asset: amount().asset(),
            },
            admitted_at: now,
            status,
            epoch: ID.into(),
        });
    }
    attempts.push(sdk::PaymentAttemptRecord {
        mode: sdk::PaymentExecutionMode::Manual,
        allowance_id: None,
        association_revision: None,
        ..attempts[0].clone()
    });
    sdk::AllowanceAccountingState {
        revision: u64::MAX,
        epoch: ID.into(),
        requires_reconciliation: true,
        history: sdk::AllowanceAccountingHistory {
            associations: vec![sdk::AllowanceAssociationRecord {
                request: request.clone(),
                revisions: vec![
                    sdk::AllowanceAssociationRevision {
                        revision: 1,
                        allowance_id: ID.into(),
                        effective_from: None,
                        authorization_id: None,
                        authorized_at: now,
                    },
                    sdk::AllowanceAssociationRevision {
                        revision: 2,
                        allowance_id: ID.into(),
                        effective_from: Some(now),
                        authorization_id: Some(ID.into()),
                        authorized_at: now,
                    },
                ],
            }],
            occurrences: vec![sdk::PaymentOccurrenceRecord {
                key: sdk::PaymentOccurrenceKey {
                    request: request.clone(),
                    billing_period: Some(sdk::AccountingBillingPeriod {
                        starts_at: now,
                        ends_at: now + chrono::Duration::hours(1),
                    }),
                },
                disposition: sdk::PaymentDisposition::Deferred {
                    reason: "private wallet reason".into(),
                },
                allowance_id: Some(ID.into()),
                association_revision: Some(1),
                attempts,
            }],
            watermarks: vec![sdk::AllowanceWatermarkRecord {
                local_public_key: request.local_public_key,
                local_receiver_path: request.local_receiver_path,
                counterparty: request.counterparty,
                counterparty_receiver_path: request.counterparty_receiver_path,
                allowance_id: ID.into(),
                evaluated_at: now,
            }],
        },
    }
}

#[test]
fn test_accounting_complete_history_round_trips_without_decimal_or_time_loss() {
    let original = ledger();
    let ffi = FfiAllowanceAccountingState::try_from(original.clone()).unwrap();
    assert_eq!(
        ffi.history.occurrences[0].attempts[0].amount.value(),
        "001.230000000000000000000001"
    );
    assert_eq!(ffi.history.watermarks[0].evaluated_at, TIME);
    let restored: sdk::AllowanceAccountingState = ffi.try_into().unwrap();
    assert_eq!(restored, original);
}

#[test]
fn test_accounting_inputs_validate_scope_and_preserve_wallet_checks() {
    let converted: sdk::PaymentExecutionChecks = checks().try_into().unwrap();
    assert!(converted.endpoint_current && converted.local_enabled && converted.recurrence_eligible);
    assert_eq!(converted.actual_amount.value(), amount().value());
    let occurrence: sdk::PaymentOccurrence = FfiPaymentOccurrence {
        request: scope(),
        billing_period: Some(FfiBillingPeriod {
            starts_at: TIME.into(),
            ends_at: "2026-09-15T13:00:00Z".into(),
        }),
    }
    .try_into()
    .unwrap();
    assert!(occurrence.billing_period.is_some());
    let once: sdk::PaymentOccurrence = FfiPaymentOccurrence {
        request: scope(),
        billing_period: None,
    }
    .try_into()
    .unwrap();
    assert!(once.billing_period.is_none());
    for invalid in [ID.to_uppercase(), "private-invalid-id".into()] {
        let selection = FfiAllowanceSelectionInput {
            allowance_id: invalid.clone(),
            expected_revision: None,
            trusted_time: TIME.into(),
        };
        let error = sdk::AllowanceSelectionInput::try_from(selection).unwrap_err();
        assert!(!format!("{error:?}").contains(&invalid));
        assert!(sdk::PaymentRequestScope::try_from(FfiPaymentRequestScope {
            payment_request_id: invalid,
            ..scope()
        })
        .is_err());
    }
    assert!(sdk::PaymentRequestScope::try_from(FfiPaymentRequestScope {
        counterparty_receiver_path: "../private".into(),
        ..scope()
    })
    .is_err());
    assert!(sdk::PaymentRequestScope::try_from(FfiPaymentRequestScope {
        counterparty: "private-key-input".into(),
        ..scope()
    })
    .is_err());
    assert!(
        sdk::PaymentExecutionChecks::try_from(FfiPaymentExecutionChecks {
            payment_endpoint_identifier: "../private".into(),
            ..checks()
        })
        .is_err()
    );
}

#[test]
fn test_accounting_timestamp_rejections_and_redacted_formatting() {
    for time in [
        "private-invalid-time",
        "2026-09-15T12:00:00+00:00",
        "2016-12-31T23:59:60Z",
    ] {
        let error = conversions::parse_time(time.into()).unwrap_err();
        assert!(!format!("{error:?}").contains(time));
    }
    assert!(FfiAccountingAmount::new("private-invalid-amount".into(), "usd".into()).is_err());
    assert!(!format!("{:?}", checks()).contains("001.23"));
    assert_eq!(format!("{}", amount()), "AccountingAmount(<redacted>)");
    let ffi = FfiAllowanceAccountingState::try_from(ledger()).unwrap();
    assert!(!format!("{ffi:?}").contains(ID));
    assert!(
        !format!("{:?}", ffi.history.occurrences[0].disposition).contains("private wallet reason")
    );
}

#[test]
fn test_accounting_decisions_and_every_block_reason_preserve_structure() {
    for reason in [
        sdk::AllowanceAccountingBlock::ReconciliationRequired,
        sdk::AllowanceAccountingBlock::InvalidLifecycle,
        sdk::AllowanceAccountingBlock::StaleRevision,
        sdk::AllowanceAccountingBlock::NoSelection,
        sdk::AllowanceAccountingBlock::ManualOnly,
        sdk::AllowanceAccountingBlock::PaymentAlreadyRecorded,
        sdk::AllowanceAccountingBlock::WalletChecksFailed,
        sdk::AllowanceAccountingBlock::SharedRule {
            code: "period_amount_limit".into(),
        },
    ] {
        let original = sdk::PaymentAttemptDecision::Blocked { reason };
        let ffi: FfiPaymentAttemptDecision = original.clone().try_into().unwrap();
        assert_eq!(
            sdk::PaymentAttemptDecision::try_from(ffi).unwrap(),
            original
        );
    }
    let original = sdk::PaymentAttemptDecision::Ready {
        attempt: ledger().history.occurrences.remove(0).attempts.remove(0),
    };
    assert_eq!(
        sdk::PaymentAttemptDecision::try_from(
            FfiPaymentAttemptDecision::try_from(original.clone()).unwrap()
        )
        .unwrap(),
        original
    );
    for disposition in [
        sdk::PaymentDisposition::Automatic,
        sdk::PaymentDisposition::ManualOnly,
    ] {
        assert_eq!(
            sdk::PaymentDisposition::try_from(
                FfiPaymentDisposition::try_from(disposition.clone()).unwrap()
            )
            .unwrap(),
            disposition
        );
    }
}

#[test]
fn test_accounting_reconciliation_converts_complete_typed_history_and_outcomes() {
    let original = ledger();
    let history = FfiAllowanceAccountingHistory::try_from(original.history.clone()).unwrap();
    let reconciliation = FfiAllowanceAccountingReconciliation {
        expected_revision: Some(original.revision),
        history,
        outcomes: vec![FfiPaymentOutcomeReport {
            attempt_id: ID.into(),
            outcome: FfiPaymentOutcome::Unknown,
        }],
        trusted_time: TIME.into(),
    };
    let converted: sdk::AllowanceAccountingReconciliation = reconciliation.try_into().unwrap();
    assert_eq!(converted.history, original.history);
    assert_eq!(converted.expected_revision, Some(u64::MAX));
    assert_eq!(converted.outcomes[0].outcome, sdk::PaymentOutcome::Unknown);
}

#[test]
fn test_accounting_state_blob_round_trip_and_version_truncation_rejection() {
    let state = StorageState {
        allowance_accounting: Some(ledger()),
        ..StorageState::default()
    };
    let bytes = encode_storage_state(&state).unwrap();
    assert_eq!(decode_storage_state(&bytes).unwrap(), state);
    assert_eq!(bytes[0], 2);
    let mut old = bytes.clone();
    old[0] = 1;
    assert!(decode_storage_state(&old).is_err());
    for end in [0, 1, bytes.len() / 2, bytes.len() - 1] {
        assert!(decode_storage_state(&bytes[..end]).is_err());
    }
}

#[test]
fn test_accounting_backup_blob_retains_all_history_and_rejects_old_or_truncated() {
    let backup = sdk::SdkBackupState {
        version: sdk::SDK_BACKUP_VERSION,
        local_receiver_path: sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap(),
        identity_state: None,
        linked_peers: vec![],
        contact_records: vec![],
        public_endpoint_records: vec![],
        payment_endpoint_reservations: vec![],
        encrypted_link_states: vec![],
        outbound_private_messages: vec![],
        private_stream_items: vec![],
        event_dedup_records: vec![],
        receipt_access_records: vec![],
        receipt_records: vec![],
        receipt_issuance_records: vec![],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
        allowance_accounting: Some(ledger()),
    };
    let bytes = encode_backup_state(&backup).unwrap();
    assert_eq!(decode_backup_state(&bytes).unwrap(), backup);
    assert_eq!(bytes[0], 2);
    let mut old = bytes.clone();
    old[0] = 1;
    assert!(decode_backup_state(&old).is_err());
    for end in [0, 1, bytes.len() / 2, bytes.len() - 1] {
        assert!(decode_backup_state(&bytes[..end]).is_err());
    }
}

#[tokio::test]
async fn test_accounting_invalid_loaded_blob_never_runs_transaction_or_writes() {
    struct Store {
        bytes: Vec<u8>,
        writes: AtomicUsize,
    }
    impl FfiSdkStateBlobStore for Store {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Ok(Some(FfiSdkStateBlobSnapshot {
                blob: Arc::new(FfiSdkStateBlob::new(self.bytes.clone())),
                revision: "opaque-revision".into(),
            }))
        }
        fn save_state_blob_atomically(
            &self,
            _: Arc<FfiSdkStateBlob>,
            _: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok("unexpected".into())
        }
    }
    let mut bytes = encode_storage_state(&StorageState {
        allowance_accounting: Some(ledger()),
        ..StorageState::default()
    })
    .unwrap();
    bytes[0] = 1;
    for bytes in [bytes, vec![2, 255]] {
        let store = Arc::new(Store {
            bytes,
            writes: AtomicUsize::new(0),
        });
        let storage = FfiSdkStorage {
            store: store.clone(),
            transaction_lock: Arc::new(Mutex::new(())),
        };
        let result = storage
            .transaction_erased(Box::new(|_| {
                panic!("invalid state must fail before transaction");
                #[allow(unreachable_code)]
                Ok(Box::new(()) as Box<dyn Any + Send>)
            }))
            .await;
        assert!(result.is_err());
        assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    }
}
