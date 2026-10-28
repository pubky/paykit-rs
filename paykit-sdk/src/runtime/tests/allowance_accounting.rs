use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Switch identity after the refresh transaction returned its captured identity,
/// before the accounting transaction starts.
struct IdentitySwitchStorage {
    inner: InMemoryStorage,
    transactions: AtomicUsize,
    replacement: IdentityState,
}

#[async_trait]
impl StorageAdapter for IdentitySwitchStorage {
    async fn load_local_identity_state(&self) -> Result<Option<IdentityState>> {
        self.load_identity_state().await
    }

    async fn transaction_erased<'a>(
        &self,
        callback: crate::storage::StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        let number = self.transactions.fetch_add(1, Ordering::SeqCst);
        let result = self.inner.transaction_erased(callback).await?;
        if number == 0 {
            self.inner
                .transaction(|tx| {
                    tx.save_identity_state(self.replacement.clone());
                    Ok(())
                })
                .await?;
        }
        Ok(result)
    }
}

#[tokio::test]
async fn test_accounting_reconciliation_rejects_identity_switch_after_refresh() {
    {
        let inner = InMemoryStorage::new();
        inner
            .save_identity_state(IdentityState {
                public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                initialized_at: FixedClock.now(),
            })
            .await
            .unwrap();
        let replacement = IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        };
        let sdk = PaykitSdk::with_clock(
            IdentitySwitchStorage {
                inner: inner.clone(),
                transactions: AtomicUsize::new(0),
                replacement: replacement.clone(),
            },
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::new("bitkit").unwrap(),
            FixedClock,
        );
        let result = sdk
            .reconcile_allowance_accounting(crate::AllowanceAccountingReconciliation {
                expected_revision: None,
                history: Default::default(),
                outcomes: vec![],
                trusted_time: FixedClock.now(),
            })
            .await;
        assert!(result.is_err());
        let state = inner.snapshot().unwrap();
        assert_eq!(state.identity_state, Some(replacement));
        assert!(state.allowance_accounting.is_none());
    }
}

fn prepared_accounting_storage() -> InMemoryStorage {
    let owner = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let peer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let epoch = EventId::new_v4().as_str().to_owned();
    InMemoryStorage::from_state(crate::storage::StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(owner.clone()),
            initialized_at: FixedClock.now(),
        }),
        allowance_accounting: Some(crate::AllowanceAccountingState {
            revision: 1,
            epoch: epoch.clone(),
            requires_reconciliation: false,
            history: crate::AllowanceAccountingHistory {
                associations: vec![],
                watermarks: vec![],
                occurrences: vec![crate::PaymentOccurrenceRecord {
                    key: crate::PaymentOccurrenceKey {
                        request: crate::PaymentAccountingScope {
                            local_public_key: owner,
                            counterparty: peer,
                            payment_request_id: PaymentRequestId::new_v4().as_str().into(),
                        },
                        billing_period: None,
                    },
                    disposition: crate::PaymentDisposition::Automatic,
                    allowance_id: None,
                    association_revision: None,
                    attempts: vec![crate::PaymentAttemptRecord {
                        attempt_id: EventId::new_v4().as_str().into(),
                        mode: crate::PaymentExecutionMode::Manual,
                        allowance_id: None,
                        association_revision: None,
                        amount: crate::AmountRecord {
                            value: "1".into(),
                            asset: "btc".into(),
                        },
                        admitted_at: FixedClock.now(),
                        status: crate::PaymentExecutionStatus::Prepared,
                        epoch,
                    }],
                }],
            },
        }),
        ..Default::default()
    })
}

#[tokio::test]
async fn test_key_rotation_preserves_accounting_and_requires_reconciliation() {
    let storage = prepared_accounting_storage();
    let before = storage.snapshot().unwrap();
    let owner = before.identity_state.unwrap().public_key.unwrap();
    let previous = before.allowance_accounting.unwrap();
    storage
        .transaction(|tx| {
            crate::runtime::key_rotation::rotate_private_state(tx, &owner, FixedClock.now())
        })
        .await
        .unwrap();
    let state = storage.snapshot().unwrap();
    crate::validate_storage_state(&state).unwrap();
    let accounting = state.allowance_accounting.unwrap();
    assert!(accounting.requires_reconciliation);
    assert!(accounting.revision > previous.revision);
    assert_ne!(accounting.epoch, previous.epoch);
    let mut expected = previous.history;
    expected.occurrences[0].attempts[0].status = crate::PaymentExecutionStatus::Unknown;
    assert_eq!(accounting.history, expected);

    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    sdk.record_payment_outcome(crate::PaymentOutcomeReport {
        attempt_id: accounting.history.occurrences[0].attempts[0]
            .attempt_id
            .clone(),
        outcome: crate::PaymentOutcome::Failed,
    })
    .await
    .unwrap();
    assert!(
        storage
            .snapshot()
            .unwrap()
            .allowance_accounting
            .unwrap()
            .requires_reconciliation
    );
}

#[tokio::test]
async fn test_sign_out_without_live_grant_and_forget_preserve_shared_accounting() {
    let storage = prepared_accounting_storage();
    let before = storage.snapshot().unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    assert!(sdk.sign_out().await.is_err());
    assert_eq!(storage.snapshot().unwrap(), before);
    sdk.forget_session_access().await.unwrap();
    assert_eq!(storage.snapshot().unwrap(), before);
}

const MISMATCHED_TRUSTED_TIME_CONTEXT: &str =
    "Automatic Acceptance requires one trusted time for selection and execution checks";

async fn accept_automatically_at(
    storage: &InMemoryStorage,
    selection_time: DateTime<Utc>,
    checks_time: DateTime<Utc>,
) -> Result<crate::AllowanceAssociationRecord> {
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    sdk.accept_payment_request_automatically(
        crate::PaymentRequestScope {
            counterparty: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
            payment_request_id: PaymentRequestId::new_v4(),
        },
        crate::AllowanceSelectionInput {
            allowance_id: AllowanceId::new_v4(),
            expected_revision: None,
            trusted_time: selection_time,
        },
        crate::PaymentExecutionChecks {
            trusted_time: checks_time,
            payment_endpoint_identifier: paykit_lib::PaymentEndpointIdentifier::new(
                "btc-lightning-bolt11",
            )
            .unwrap(),
            actual_amount: paykit_lib::PaymentAmount::new("1", "btc").unwrap(),
            endpoint_current: true,
            local_enabled: true,
            recurrence_eligible: true,
        },
    )
    .await
}

fn is_mismatched_trusted_time_error(error: &PaykitSdkError) -> bool {
    matches!(
        error,
        PaykitSdkError::Protocol { context, source: None }
            if context == MISMATCHED_TRUSTED_TIME_CONTEXT
    )
}

#[tokio::test]
async fn test_automatic_acceptance_rejects_mismatched_trusted_times() {
    let storage = prepared_accounting_storage();
    let before = storage.snapshot().unwrap();
    let time = FixedClock.now();
    let error = accept_automatically_at(&storage, time, time + chrono::Duration::milliseconds(1))
        .await
        .unwrap_err();
    assert!(is_mismatched_trusted_time_error(&error), "{error:?}");
    let after = storage.snapshot().unwrap();
    assert!(after.outbound_private_messages.is_empty());
    assert_eq!(after, before);
}

#[tokio::test]
async fn test_automatic_acceptance_equal_trusted_times_pass_time_check() {
    let storage = prepared_accounting_storage();
    let time = FixedClock.now();
    // The harness has no Payment Request, so a later step fails; only the
    // trusted-time check is under test here.
    let error = accept_automatically_at(&storage, time, time)
        .await
        .unwrap_err();
    assert!(!is_mismatched_trusted_time_error(&error), "{error:?}");
}
