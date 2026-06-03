use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    config::PaykitSdkConfig,
    identity::{IdentityState, IdentityStatus, PubkyIdentityCapability},
    linked_peers::mark_recovery_required,
    private_stream::{persist_private_stream_batch, PrivateStreamIntakeReport},
    storage::{EncryptedLinkStateRecord, StorageAdapter},
    PaykitSdkError, PaymentAdapter, PubkyPublicKey, PubkySessionProvider, Result,
};

/// Clock abstraction used by SDK workflows and tests.
pub trait Clock: Clone + Send + Sync + 'static {
    /// Return the current UTC time.
    fn now(&self) -> DateTime<Utc>;
}

/// System UTC clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Initialization report returned after SDK startup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializationReport {
    /// Current identity status.
    pub identity: IdentityStatus,
}

/// Stateful Paykit SDK runtime for one local Pubky identity.
pub struct PaykitSdk<S, K, P, C = SystemClock> {
    storage: S,
    pubky: K,
    payment: P,
    config: PaykitSdkConfig,
    clock: C,
}

impl<S, K, P> PaykitSdk<S, K, P, SystemClock>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
{
    /// Create an SDK runtime with the system clock.
    pub fn new(storage: S, pubky: K, payment: P, config: PaykitSdkConfig) -> Self {
        Self::with_clock(storage, pubky, payment, config, SystemClock)
    }
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Create an SDK runtime with an explicit clock.
    pub fn with_clock(storage: S, pubky: K, payment: P, config: PaykitSdkConfig, clock: C) -> Self {
        Self {
            storage,
            pubky,
            payment,
            config,
            clock,
        }
    }

    /// Initialize durable SDK identity state.
    pub async fn initialize(&self) -> Result<InitializationReport> {
        let session = self.pubky.load_session_access().await?;
        let (public_key, capability) = match session.as_ref() {
            Some(session) => (Some(session.public_key()?), session.capability()),
            None => (None, PubkyIdentityCapability::SignedOut),
        };
        let state = IdentityState {
            public_key,
            local_secret_available: capability == PubkyIdentityCapability::PrivateLinkCapable,
            capability,
            initialized_at: self.clock.now(),
            sign_out_generation: self
                .storage
                .load_identity_state()
                .await?
                .map(|state| state.sign_out_generation)
                .unwrap_or_default(),
        };

        self.storage.save_identity_state(state.clone()).await?;

        Ok(InitializationReport {
            identity: IdentityStatus::from(&state),
        })
    }

    /// Return the last persisted identity status, if initialized.
    pub async fn identity_status(&self) -> Result<Option<IdentityStatus>> {
        Ok(self
            .storage
            .load_identity_state()
            .await?
            .as_ref()
            .map(IdentityStatus::from))
    }

    /// Access SDK configuration.
    pub fn config(&self) -> &PaykitSdkConfig {
        &self.config
    }

    /// Access the payment adapter.
    pub fn payment_adapter(&self) -> &P {
        &self.payment
    }

    /// Access the Pubky session provider.
    pub fn pubky_session_provider(&self) -> &K {
        &self.pubky
    }

    /// Receive and durably persist currently available private messages.
    ///
    /// This requires a stored Encrypted Link snapshot for the counterparty.
    /// Handshake establishment and recovery are separate workflows.
    pub async fn receive_private_messages(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivateStreamIntakeReport> {
        let session_access =
            self.pubky
                .load_session_access()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky session available".into(),
                    source: None,
                })?;
        let secret_key = *session_access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "local Pubky secret key is unavailable for Encrypted Links".into(),
                source: None,
            })?
            .as_bytes();
        let remote_public_key = counterparty.to_public_key()?;

        let stored_link_state = self
            .storage
            .transaction(|tx| Ok(tx.encrypted_link_state(&counterparty)))
            .await?
            .ok_or_else(|| {
                PaykitSdkError::RecoveryRequired(format!(
                    "no Encrypted Link state for counterparty {counterparty}"
                ))
            })?;
        let Some(snapshot_bytes) = stored_link_state.link_snapshot.as_ref() else {
            let now = self.clock.now();
            mark_recovery_required(&self.storage, counterparty.clone(), now).await?;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
        };
        let snapshot = match paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let now = self.clock.now();
                mark_recovery_required(&self.storage, counterparty.clone(), now).await?;
                return Err(err.into());
            }
        };

        let mut link = paykit_lib::restore_encrypted_link(
            session_access.session,
            secret_key,
            &remote_public_key,
            session_access.outbox_client,
            snapshot,
        )
        .await?;
        let messages = link.receive_private_application_messages().await?;
        let now = self.clock.now();
        let next_link_state = EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: Some(link.serialize()),
            handshake_snapshot: stored_link_state.handshake_snapshot,
            generation: stored_link_state.generation.saturating_add(1),
            checkpointed_at: now,
        };

        persist_private_stream_batch(
            &self.storage,
            counterparty,
            messages,
            Some(next_link_state),
            now,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{
        adapters::{
            EndpointCompatibility, PaymentEndpointCandidate, PaymentExecutionResult,
            PaymentRequestExecution, PaymentTarget, ReceivingDetail, ReceivingDetailScope,
        },
        storage::InMemoryStorage,
        PubkySessionAccess,
    };

    #[derive(Clone)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
        }
    }

    struct TestPubkySessionProvider {
        session: Option<PubkySessionAccess>,
    }

    #[async_trait]
    impl PubkySessionProvider for TestPubkySessionProvider {
        async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
            Ok(self.session.clone())
        }

        async fn clear_session_access(&self) -> Result<()> {
            Ok(())
        }
    }

    struct TestPaymentAdapter;

    #[async_trait]
    impl PaymentAdapter for TestPaymentAdapter {
        async fn current_receiving_details(
            &self,
            _scope: ReceivingDetailScope,
        ) -> Result<Vec<ReceivingDetail>> {
            Ok(Vec::new())
        }

        async fn is_endpoint_payable(
            &self,
            _endpoint: &PaymentEndpointCandidate,
        ) -> Result<EndpointCompatibility> {
            Ok(EndpointCompatibility::Unsupported { reason: None })
        }

        async fn build_payment_target(
            &self,
            endpoint: &PaymentEndpointCandidate,
        ) -> Result<PaymentTarget> {
            Ok(PaymentTarget {
                payload: endpoint.payload.clone(),
            })
        }

        async fn execute_payment_request(
            &self,
            _request: &PaymentRequestExecution,
        ) -> Result<PaymentExecutionResult> {
            Ok(PaymentExecutionResult {
                proof: None,
                settled: false,
            })
        }
    }

    #[tokio::test]
    async fn test_initialize_persists_signed_out_identity() {
        let storage = InMemoryStorage::new();
        let pubky = TestPubkySessionProvider { session: None };
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            pubky,
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let report = sdk.initialize().await.unwrap();

        assert!(!report.identity.private_link_capable);
        let stored = storage.snapshot().unwrap().identity_state.unwrap();
        assert!(stored.public_key.is_none());
        assert_eq!(stored.capability, PubkyIdentityCapability::SignedOut);
        assert!(!stored.local_secret_available);
        assert_eq!(stored.initialized_at, FixedClock.now());
    }

    #[tokio::test]
    async fn test_receive_private_messages_requires_pubky_session() {
        let storage = InMemoryStorage::new();
        let pubky = TestPubkySessionProvider { session: None };
        let sdk = PaykitSdk::with_clock(
            storage,
            pubky,
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());

        let result = sdk.receive_private_messages(counterparty).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }
}
