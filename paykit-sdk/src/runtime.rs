use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    config::{EndpointManagementScope, PaykitSdkConfig, PublicFallbackPolicy},
    contacts::{
        ContactPaymentResolution, ContactPaymentResolutionRequest, ContactPaymentResolutionStatus,
    },
    endpoints::{
        failed_record, normalize_receiving_details, published_record, removed_record,
        EndpointPublicationStatus, EndpointSyncChange, EndpointSyncReport,
    },
    identity::{IdentityState, IdentityStatus, PubkyIdentityCapability},
    linked_peers::mark_recovery_required,
    outbound_private::{
        claim_next_outbound_private_message,
        enqueue_private_message as enqueue_outbound_private_message, mark_outbound_failed,
        mark_outbound_sent, queued_outbound_private_messages, OutboundPrivateSendFailure,
        OutboundPrivateSendReport,
    },
    private_lists::current_private_payment_list as load_current_private_payment_list,
    private_stream::{persist_private_stream_batch, PrivateStreamIntakeReport},
    storage::{EncryptedLinkStateRecord, OutboundPrivateMessageRecord, StorageAdapter},
    PaykitSdkError, PaymentAdapter, PaymentEndpointCandidate, PaymentEndpointEvaluation,
    PaymentEndpointSelection, PaymentEndpointSelectionRequest, PaymentEndpointSource,
    PrivatePaymentListView, PubkyPublicKey, PubkySessionProvider, ReceivingDetailScope, Result,
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

    /// Return the latest valid Private Payment List view for a counterparty.
    pub async fn current_private_payment_list(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Option<crate::PrivatePaymentListView>> {
        load_current_private_payment_list(&self.storage, counterparty).await
    }

    /// Enqueue one raw JSON Private Application Message for later delivery.
    pub async fn enqueue_private_message(
        &self,
        counterparty: PubkyPublicKey,
        raw_json: String,
    ) -> Result<OutboundPrivateMessageRecord> {
        enqueue_outbound_private_message(&self.storage, counterparty, raw_json, self.clock.now())
            .await
    }

    /// Resolve a payable endpoint for one counterparty.
    pub async fn resolve_contact_payment(
        &self,
        request: ContactPaymentResolutionRequest,
    ) -> Result<ContactPaymentResolution> {
        let mut evaluations = Vec::new();
        let private_view =
            load_current_private_payment_list(&self.storage, &request.counterparty).await?;
        let private_candidates = private_candidates(&request.counterparty, private_view.as_ref());

        if !private_candidates.is_empty() {
            let selection = self
                .payment
                .select_payment_endpoint(&PaymentEndpointSelectionRequest {
                    counterparty: request.counterparty.clone(),
                    amount: request.amount.clone(),
                    candidates: private_candidates.clone(),
                })
                .await?;
            let selected = selected_from_batch(&selection, &private_candidates)?;
            evaluations.extend(selection.evaluations);
            if let Some(selected) = selected {
                return Ok(payable_resolution(selected, evaluations, false));
            }
        }

        if self.config.public_fallback == PublicFallbackPolicy::Disabled {
            return Ok(unresolved_resolution(
                !private_candidates.is_empty(),
                evaluations,
                false,
            ));
        }

        let public_candidates = self
            .public_payment_candidates(&request.counterparty)
            .await?;
        if public_candidates.is_empty() {
            return Ok(unresolved_resolution(
                !private_candidates.is_empty(),
                evaluations,
                false,
            ));
        }

        let selection = self
            .payment
            .select_payment_endpoint(&PaymentEndpointSelectionRequest {
                counterparty: request.counterparty,
                amount: request.amount,
                candidates: public_candidates.clone(),
            })
            .await?;
        let selected = selected_from_batch(&selection, &public_candidates)?;
        evaluations.extend(selection.evaluations);
        if let Some(selected) = selected {
            return Ok(payable_resolution(selected, evaluations, true));
        }

        Ok(unresolved_resolution(true, evaluations, true))
    }

    /// Publish current public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints(&self) -> Result<EndpointSyncReport> {
        let session_access =
            self.pubky
                .load_session_access()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky session available".into(),
                    source: None,
                })?;
        let details = self
            .payment
            .current_receiving_details(ReceivingDetailScope::Public)
            .await?;
        let desired = normalize_receiving_details(details)?;
        let now = self.clock.now();
        let mut report = EndpointSyncReport::default();

        for (identifier, payload) in &desired {
            match paykit_lib::set_payment_endpoint(
                &session_access.session,
                identifier.clone(),
                payload.clone(),
            )
            .await
            {
                Ok(()) => {
                    self.storage
                        .transaction({
                            let record = published_record(identifier, payload, now);
                            move |tx| {
                                tx.save_public_endpoint_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    report.published.push(EndpointSyncChange {
                        identifier: identifier.as_str().to_owned(),
                        status: EndpointPublicationStatus::Published,
                        error: None,
                    });
                }
                Err(err) => {
                    let error = err.to_string();
                    self.storage
                        .transaction({
                            let record = failed_record(
                                identifier.as_str().to_owned(),
                                Some(payload.as_str().to_owned()),
                                error.clone(),
                                now,
                            );
                            move |tx| {
                                tx.save_public_endpoint_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    report.failed.push(EndpointSyncChange {
                        identifier: identifier.as_str().to_owned(),
                        status: EndpointPublicationStatus::Failed,
                        error: Some(error),
                    });
                }
            }
        }

        let removal_candidates = match self.config.endpoint_management_scope {
            EndpointManagementScope::ManagedOnly => self
                .storage
                .transaction(|tx| Ok(tx.public_endpoint_records()))
                .await?
                .into_iter()
                .filter(|record| {
                    record.status != EndpointPublicationStatus::Removed
                        && !desired
                            .keys()
                            .any(|identifier| identifier.as_str() == record.identifier)
                })
                .map(|record| (record.identifier, record.payload))
                .collect::<Vec<_>>(),
            EndpointManagementScope::FullPaykitNamespace => {
                let local_public_key = session_access.session.info().public_key().clone();
                let current = paykit_lib::get_payment_list(
                    &session_access.outbox_client.public_storage(),
                    &local_public_key,
                )
                .await?;
                current
                    .payment_endpoints
                    .into_iter()
                    .filter(|(identifier, _)| !desired.contains_key(identifier))
                    .map(|(identifier, payload)| {
                        (identifier.as_str().to_owned(), Some(payload.into_inner()))
                    })
                    .collect::<Vec<_>>()
            }
        };

        for (identifier_text, previous_payload) in removal_candidates {
            let identifier = paykit_lib::PaymentEndpointIdentifier::new(&identifier_text)?;
            match paykit_lib::remove_payment_endpoint(&session_access.session, identifier).await {
                Ok(()) => {
                    self.storage
                        .transaction({
                            let record = removed_record(identifier_text.clone(), now);
                            move |tx| {
                                tx.save_public_endpoint_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    report.removed.push(EndpointSyncChange {
                        identifier: identifier_text,
                        status: EndpointPublicationStatus::Removed,
                        error: None,
                    });
                }
                Err(err) => {
                    let error = err.to_string();
                    self.storage
                        .transaction({
                            let record = failed_record(
                                identifier_text.clone(),
                                previous_payload,
                                error.clone(),
                                now,
                            );
                            move |tx| {
                                tx.save_public_endpoint_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    report.failed.push(EndpointSyncChange {
                        identifier: identifier_text,
                        status: EndpointPublicationStatus::Failed,
                        error: Some(error),
                    });
                }
            }
        }

        Ok(report)
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

    /// Send queued outbound private messages for one counterparty in order.
    pub async fn process_outbound_private_messages(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateSendReport> {
        let mut report = OutboundPrivateSendReport::default();
        let queued = queued_outbound_private_messages(&self.storage, &counterparty).await?;
        if queued.is_empty() {
            return Ok(report);
        }

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
        let mut link_state = stored_link_state;

        loop {
            let now = self.clock.now();
            let lease_timeout = ChronoDuration::from_std(
                self.config.outbound_private_send_lease_timeout,
            )
            .map_err(|err| {
                PaykitSdkError::Policy(format!(
                    "invalid outbound private send lease timeout: {err}"
                ))
            })?;
            let stale_before = now - lease_timeout;
            let Some(sending) = claim_next_outbound_private_message(
                &self.storage,
                &counterparty,
                now,
                stale_before,
            )
            .await?
            else {
                break;
            };
            report.attempted.push(sending.outbound_message_id);

            match link
                .send_private_application_message_json(&sending.raw_json)
                .await
            {
                Ok(()) => {
                    let now = self.clock.now();
                    let sent = mark_outbound_sent(sending, now);
                    link_state.link_snapshot = Some(link.serialize());
                    link_state.generation = link_state.generation.saturating_add(1);
                    link_state.checkpointed_at = now;
                    self.storage
                        .transaction({
                            let sent = sent.clone();
                            let link_state = link_state.clone();
                            move |tx| {
                                tx.save_outbound_private_message(sent);
                                tx.save_encrypted_link_state(link_state);
                                Ok(())
                            }
                        })
                        .await?;
                    report.sent.push(sent.outbound_message_id);
                }
                Err(err) => {
                    let now = self.clock.now();
                    let error = err.to_string();
                    let failed = mark_outbound_failed(sending, error.clone(), now);
                    self.storage
                        .transaction({
                            let failed = failed.clone();
                            move |tx| {
                                tx.save_outbound_private_message(failed);
                                Ok(())
                            }
                        })
                        .await?;
                    report.failed.push(OutboundPrivateSendFailure {
                        outbound_message_id: failed.outbound_message_id,
                        error,
                    });
                    break;
                }
            }
        }

        Ok(report)
    }

    async fn public_payment_candidates(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PaymentEndpointCandidate>> {
        let session_access =
            self.pubky
                .load_session_access()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky session available for public Payment Endpoint lookup".into(),
                    source: None,
                })?;
        let payment_list = paykit_lib::get_payment_list(
            &session_access.outbox_client.public_storage(),
            &counterparty.to_public_key()?,
        )
        .await?;
        let mut endpoints = payment_list
            .payment_endpoints
            .into_iter()
            .map(|(identifier, payload)| PaymentEndpointCandidate {
                counterparty: counterparty.clone(),
                source: PaymentEndpointSource::PublicPaymentEndpoint,
                identifier: identifier.as_str().to_owned(),
                payload: payload.into_inner(),
            })
            .collect::<Vec<_>>();
        endpoints.sort_by(|left, right| left.identifier.cmp(&right.identifier));
        Ok(endpoints)
    }
}

fn private_candidates(
    counterparty: &PubkyPublicKey,
    view: Option<&PrivatePaymentListView>,
) -> Vec<PaymentEndpointCandidate> {
    let Some(view) = view else {
        return Vec::new();
    };
    let mut candidates = view
        .payment_endpoints
        .iter()
        .map(|(identifier, payload)| PaymentEndpointCandidate {
            counterparty: counterparty.clone(),
            source: PaymentEndpointSource::PrivatePaymentList,
            identifier: identifier.clone(),
            payload: payload.clone(),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.identifier.cmp(&right.identifier));
    candidates
}

fn payable_resolution(
    selected: PaymentEndpointCandidate,
    evaluations: Vec<PaymentEndpointEvaluation>,
    used_public_fallback: bool,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status: ContactPaymentResolutionStatus::Payable,
        selected_endpoint: Some(selected),
        evaluations,
        used_public_fallback,
    }
}

fn unresolved_resolution(
    had_candidates: bool,
    evaluations: Vec<PaymentEndpointEvaluation>,
    used_public_fallback: bool,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status: if had_candidates {
            ContactPaymentResolutionStatus::UnsupportedEndpoint
        } else {
            ContactPaymentResolutionStatus::NoEndpoint
        },
        selected_endpoint: None,
        evaluations,
        used_public_fallback,
    }
}

fn selected_from_batch(
    selection: &PaymentEndpointSelection,
    candidates: &[PaymentEndpointCandidate],
) -> Result<Option<PaymentEndpointCandidate>> {
    let Some(selected) = selection.selected.as_ref() else {
        return Ok(None);
    };
    if candidates.contains(selected) {
        Ok(Some(selected.clone()))
    } else {
        Err(PaykitSdkError::Protocol(
            "PaymentAdapter selected an endpoint that was not in the candidate batch".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{
        adapters::{
            EndpointCompatibility, PaymentEndpointCandidate, PaymentEndpointEvaluation,
            PaymentEndpointSelection, PaymentEndpointSelectionRequest, PaymentEndpointSource,
            PaymentExecutionResult, PaymentRequestExecution, PaymentTarget, ReceivingDetail,
            ReceivingDetailScope,
        },
        private_stream::persist_private_stream_batch,
        storage::InMemoryStorage,
        PubkySessionAccess,
    };
    use paykit_lib::PrivateApplicationMessage;

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

        async fn select_payment_endpoint(
            &self,
            request: &PaymentEndpointSelectionRequest,
        ) -> Result<PaymentEndpointSelection> {
            Ok(PaymentEndpointSelection {
                selected: request.candidates.first().cloned(),
                evaluations: request
                    .candidates
                    .iter()
                    .enumerate()
                    .map(|(index, candidate)| PaymentEndpointEvaluation {
                        candidate: candidate.clone(),
                        compatibility: EndpointCompatibility::Payable,
                        priority: Some(index as u32),
                    })
                    .collect(),
            })
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

    #[tokio::test]
    async fn test_sync_public_endpoints_requires_pubky_session() {
        let storage = InMemoryStorage::new();
        let pubky = TestPubkySessionProvider { session: None };
        let sdk = PaykitSdk::with_clock(
            storage,
            pubky,
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.sync_public_endpoints().await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
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

    #[tokio::test]
    async fn test_enqueue_private_message_stores_outbound_record() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let record = sdk
            .enqueue_private_message(counterparty.clone(), private_list_json())
            .await
            .unwrap();

        let queued = crate::queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();
        assert_eq!(record.outbound_message_id, 0);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].attempt_count, 0);
    }

    #[tokio::test]
    async fn test_process_outbound_private_messages_requires_pubky_session() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        crate::enqueue_private_message(
            &storage,
            counterparty.clone(),
            private_list_json(),
            FixedClock.now(),
        )
        .await
        .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk
            .process_outbound_private_messages(counterparty.clone())
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        let queued = crate::queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();
        assert_eq!(queued[0].attempt_count, 0);
    }

    #[tokio::test]
    async fn test_resolve_contact_payment_uses_private_list() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            vec![private_list_message("ln-private")],
            None,
            FixedClock.now(),
        )
        .await
        .unwrap();
        let pubky = TestPubkySessionProvider { session: None };
        let sdk = PaykitSdk::with_clock(
            storage,
            pubky,
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let resolution = sdk
            .resolve_contact_payment(ContactPaymentResolutionRequest {
                counterparty,
                amount: Some(crate::PaymentAmountContext {
                    value: "10.00".into(),
                    asset: "usd".into(),
                }),
            })
            .await
            .unwrap();

        let selected = resolution.selected_endpoint.unwrap();
        assert_eq!(resolution.status, ContactPaymentResolutionStatus::Payable);
        assert_eq!(selected.source, PaymentEndpointSource::PrivatePaymentList);
        assert_eq!(selected.identifier, "btc-lightning-bolt11");
        assert_eq!(selected.payload, "ln-private");
        assert!(!resolution.used_public_fallback);
    }
}
