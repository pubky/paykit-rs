use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Switch identity after the refresh transaction returned its captured identity,
/// before the accounting transaction starts. This models the sign-out barrier.
struct IdentitySwitchStorage {
    inner: InMemoryStorage,
    transactions: AtomicUsize,
    replacement: IdentityState,
}

#[async_trait]
impl StorageAdapter for IdentitySwitchStorage {
    async fn transaction_erased<'a>(
        &self,
        callback: crate::storage::StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        let number = self.transactions.fetch_add(1, Ordering::SeqCst);
        let result = self.inner.transaction_erased(callback).await?;
        if number == 1 {
            self.inner
                .transaction(|tx| {
                    tx.clear_identity_scoped_state();
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
    for generation in [0, 1] {
        let inner = InMemoryStorage::new();
        inner
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
        let replacement = IdentityState {
            local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: FixedClock.now(),
            sign_out_generation: generation,
        };
        let sdk = PaykitSdk::with_clock(
            IdentitySwitchStorage {
                inner: inner.clone(),
                transactions: AtomicUsize::new(0),
                replacement: replacement.clone(),
            },
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
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
