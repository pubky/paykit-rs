use std::collections::HashSet;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    config::{
        EndpointManagementScope, PaykitSdkConfig, PrivateSharingPolicy, PublicFallbackPolicy,
    },
    contacts::{
        ContactPaymentResolution, ContactPaymentResolutionRequest, ContactPaymentResolutionStatus,
    },
    endpoints::{
        desired_record, failed_record, normalize_receiving_details, pending_removal_record,
        published_record, removed_record, EndpointPublicationStatus, EndpointSyncChange,
        EndpointSyncReport,
    },
    identity::{IdentityState, IdentityStatus, PubkyIdentityCapability},
    linked_peers::{
        mark_recovery_required_with_lease, save_link_handshake_state_if_generation_with_lease,
        save_link_handshake_state_with_lease, save_linked_peer_link_state_if_generation_with_lease,
        save_linked_peer_state_with_lease, EncryptedLinkHandshakeRole, LinkedPeerHandshakeReport,
        LinkedPeerState,
    },
    outbound_private::{
        claim_next_outbound_private_message_with_peer_lease, mark_outbound_failed,
        mark_outbound_invalid, mark_outbound_sent, queued_outbound_private_messages,
        validate_queued_outbound_private_message, OutboundPrivateSendFailure,
        OutboundPrivateSendReport,
    },
    private_lists::{
        current_private_payment_list as load_current_private_payment_list,
        enqueue_private_payment_list as enqueue_private_payment_list_message,
    },
    private_stream::{persist_private_stream_batch_with_link_lease, PrivateStreamIntakeReport},
    storage::{
        EncryptedLinkStateRecord, OutboundPrivateMessageRecord, PeerLinkOperationLease,
        StorageAdapter,
    },
    PaykitSdkError, PaymentAdapter, PaymentEndpointCandidate, PaymentEndpointEvaluation,
    PaymentEndpointSelection, PaymentEndpointSelectionRequest, PaymentEndpointSource,
    PrivatePaymentListView, PubkyPublicKey, PubkySessionAccess, PubkySessionProvider,
    ReceivingDetailScope, Result,
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
        let (_, state) = self.load_session_access_and_refresh_identity().await?;

        Ok(InitializationReport {
            identity: IdentityStatus::from(&state),
        })
    }

    async fn load_session_access_and_refresh_identity(
        &self,
    ) -> Result<(Option<PubkySessionAccess>, IdentityState)> {
        let session = self.pubky.load_session_access().await?;
        let (public_key, capability) = match session.as_ref() {
            Some(session) => (Some(session.public_key()?), session.capability()),
            None => (None, PubkyIdentityCapability::SignedOut),
        };
        let now = self.clock.now();
        let state = self
            .storage
            .transaction(move |tx| {
                let previous = tx.load_identity_state();
                let identity_missing = previous.is_none();
                let previous_generation = previous
                    .as_ref()
                    .map(|state| state.sign_out_generation)
                    .unwrap_or_default();
                let identity_changed = previous
                    .as_ref()
                    .is_some_and(|state| state.public_key != public_key);
                let private_capability_downgraded = previous.as_ref().is_some_and(|state| {
                    state.public_key == public_key
                        && state.capability == PubkyIdentityCapability::PrivateLinkCapable
                        && capability != PubkyIdentityCapability::PrivateLinkCapable
                });
                let signing_out = public_key.is_none()
                    && previous.as_ref().is_some_and(|state| {
                        state.public_key.is_some()
                            || state.capability != PubkyIdentityCapability::SignedOut
                    });
                let generation = if identity_changed || signing_out || private_capability_downgraded
                {
                    previous_generation.saturating_add(1)
                } else {
                    previous_generation
                };

                if identity_missing || identity_changed || signing_out {
                    tx.clear_identity_scoped_state();
                } else if private_capability_downgraded {
                    tx.clear_private_identity_scoped_state();
                }

                let state = IdentityState {
                    public_key,
                    local_secret_available: capability
                        == PubkyIdentityCapability::PrivateLinkCapable,
                    capability,
                    initialized_at: now,
                    sign_out_generation: generation,
                };
                tx.save_identity_state(state.clone());
                Ok(state)
            })
            .await?;

        Ok((session, state))
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
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Ok(None);
        }
        load_current_private_payment_list(&self.storage, counterparty).await
    }

    /// Enqueue the current complete Private Payment List for one counterparty.
    pub async fn enqueue_private_payment_list(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateMessageRecord> {
        if self.config.private_sharing == PrivateSharingPolicy::Disabled {
            return Err(PaykitSdkError::Policy(
                "private Payment List sharing is disabled".into(),
            ));
        }

        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Err(PaykitSdkError::Identity {
                context: "local Pubky identity is not private-link-capable".into(),
                source: None,
            });
        }

        let has_active_link = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .encrypted_link_state(&counterparty)
                    .and_then(|state| state.link_snapshot)
                    .is_some())
            })
            .await?;
        if !has_active_link {
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
        }

        let receiving_details = self
            .payment
            .current_receiving_details(ReceivingDetailScope::Private {
                counterparty: counterparty.clone(),
            })
            .await?;
        enqueue_private_payment_list_message(
            &self.storage,
            counterparty,
            receiving_details,
            self.clock.now(),
        )
        .await
    }

    /// Start an Encrypted Link Handshake as the initiator.
    pub async fn initiate_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.start_link_handshake(counterparty, EncryptedLinkHandshakeRole::Initiator)
            .await
    }

    /// Start an Encrypted Link Handshake as the responder.
    pub async fn accept_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.start_link_handshake(counterparty, EncryptedLinkHandshakeRole::Responder)
            .await
    }

    /// Advance the stored Encrypted Link Handshake for one counterparty.
    pub async fn advance_link_handshake(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.load_session_access_and_refresh_identity().await?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .advance_link_handshake_with_claim(counterparty, lease.clone())
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    async fn advance_link_handshake_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.ensure_peer_not_blocked(&counterparty).await?;
        let Some(stored_link_state) = self
            .storage
            .transaction(|tx| Ok(tx.encrypted_link_state(&counterparty)))
            .await?
        else {
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no Encrypted Link state for counterparty {counterparty}"
            )));
        };
        if stored_link_state.link_snapshot.is_some() {
            save_linked_peer_state_with_lease(
                &self.storage,
                counterparty.clone(),
                LinkedPeerState::Linked,
                lease.clone(),
                self.clock.now(),
            )
            .await?;
            return Ok(LinkedPeerHandshakeReport {
                counterparty: counterparty.clone(),
                state: LinkedPeerState::Linked,
                generation: stored_link_state.generation,
                handshake_role: None,
            });
        }

        let Some(handshake_role) = stored_link_state.handshake_role else {
            mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                self.clock.now(),
            )
            .await?;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "missing Encrypted Link Handshake role for counterparty {counterparty}"
            )));
        };
        let Some(snapshot_bytes) = stored_link_state.handshake_snapshot.as_ref() else {
            mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                self.clock.now(),
            )
            .await?;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no in-progress Encrypted Link Handshake snapshot for counterparty {counterparty}"
            )));
        };

        let result = self
            .advance_link_handshake_from_snapshot(
                counterparty.clone(),
                snapshot_bytes,
                handshake_role,
                stored_link_state.generation,
                lease.clone(),
            )
            .await;
        if result
            .as_ref()
            .is_err_and(should_mark_link_recovery_required)
        {
            mark_recovery_required_with_lease(&self.storage, counterparty, lease, self.clock.now())
                .await?;
        }
        result
    }

    /// Resolve a payable endpoint for one counterparty.
    pub async fn resolve_contact_payment(
        &self,
        request: ContactPaymentResolutionRequest,
    ) -> Result<ContactPaymentResolution> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let mut evaluations = Vec::new();
        let private_view = if identity.capability == PubkyIdentityCapability::PrivateLinkCapable {
            load_current_private_payment_list(&self.storage, &request.counterparty).await?
        } else {
            None
        };
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

        let mut public_only_session = false;
        if self.config.public_fallback != PublicFallbackPolicy::WhenPrivateUnavailable {
            match self
                .recover_private_candidates_for_resolution(&request.counterparty)
                .await?
            {
                PrivateRecoveryOutcome::Refreshed(refreshed_candidates)
                    if !refreshed_candidates.is_empty() =>
                {
                    let selection = self
                        .payment
                        .select_payment_endpoint(&PaymentEndpointSelectionRequest {
                            counterparty: request.counterparty.clone(),
                            amount: request.amount.clone(),
                            candidates: refreshed_candidates.clone(),
                        })
                        .await?;
                    let selected = selected_from_batch(&selection, &refreshed_candidates)?;
                    evaluations.extend(selection.evaluations);
                    if let Some(selected) = selected {
                        return Ok(payable_resolution(selected, evaluations, false));
                    }
                }
                PrivateRecoveryOutcome::Pending => {
                    return Ok(status_resolution(
                        ContactPaymentResolutionStatus::PrivateRecoveryPending,
                        evaluations,
                        false,
                    ));
                }
                PrivateRecoveryOutcome::PublicOnly => {
                    public_only_session = true;
                }
                PrivateRecoveryOutcome::NotNeeded | PrivateRecoveryOutcome::Refreshed(_) => {}
            }
        }

        if self.config.public_fallback == PublicFallbackPolicy::Disabled {
            if public_only_session {
                return Ok(status_resolution(
                    ContactPaymentResolutionStatus::PublicOnlySession,
                    evaluations,
                    false,
                ));
            }
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
            if public_only_session {
                return Ok(status_resolution(
                    ContactPaymentResolutionStatus::PublicOnlySession,
                    evaluations,
                    false,
                ));
            }
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

    async fn recover_private_candidates_for_resolution(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PrivateRecoveryOutcome> {
        if self.config.private_sharing == PrivateSharingPolicy::Disabled {
            return Ok(PrivateRecoveryOutcome::NotNeeded);
        }

        let Some(identity) = self.storage.load_identity_state().await? else {
            return Ok(PrivateRecoveryOutcome::PublicOnly);
        };
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Ok(PrivateRecoveryOutcome::PublicOnly);
        }

        let (peer_state, peer_last_sync_at, has_active_link, link_generation) = self
            .storage
            .transaction(|tx| {
                let peer = tx.linked_peer(counterparty);
                let link_state = tx.encrypted_link_state(counterparty);
                let has_active_link = link_state
                    .as_ref()
                    .and_then(|state| state.link_snapshot.as_ref())
                    .is_some();
                let link_generation = link_state.as_ref().map(|state| state.generation);
                Ok((
                    peer.as_ref().map(|peer| peer.state.clone()),
                    peer.and_then(|peer| peer.last_sync_at),
                    has_active_link,
                    link_generation,
                ))
            })
            .await?;

        if matches!(
            peer_state,
            Some(LinkedPeerState::Linking | LinkedPeerState::RecoveryRequired)
        ) {
            if peer_last_sync_at
                .map(|last_sync_at| self.private_recovery_window_open(last_sync_at))
                .transpose()?
                .unwrap_or(false)
            {
                return Ok(PrivateRecoveryOutcome::Pending);
            }

            if matches!(peer_state, Some(LinkedPeerState::RecoveryRequired)) {
                return Ok(PrivateRecoveryOutcome::NotNeeded);
            }
        }

        if has_active_link {
            match self.receive_private_messages(counterparty.clone()).await {
                Ok(_) => {
                    let private_view =
                        load_current_private_payment_list(&self.storage, counterparty).await?;
                    return Ok(PrivateRecoveryOutcome::Refreshed(private_candidates(
                        counterparty,
                        private_view.as_ref(),
                    )));
                }
                Err(PaykitSdkError::Policy(_)) => return Ok(PrivateRecoveryOutcome::Pending),
                Err(PaykitSdkError::Identity { .. }) => {
                    return Ok(PrivateRecoveryOutcome::PublicOnly)
                }
                Err(PaykitSdkError::RecoveryRequired(_))
                | Err(PaykitSdkError::Transport { .. })
                | Err(PaykitSdkError::Protocol(_)) => {
                    self.mark_private_recovery_pending(counterparty, link_generation)
                        .await?;
                    return Ok(PrivateRecoveryOutcome::Pending);
                }
                Err(err) => return Err(err),
            }
        }

        Ok(PrivateRecoveryOutcome::NotNeeded)
    }

    async fn mark_private_recovery_pending(
        &self,
        counterparty: &PubkyPublicKey,
        expected_link_generation: Option<u64>,
    ) -> Result<()> {
        let now = self.clock.now();
        self.storage
            .transaction(|tx| {
                let current_generation = tx
                    .encrypted_link_state(counterparty)
                    .map(|state| state.generation);
                if current_generation != expected_link_generation {
                    return Ok(());
                }
                let previous = tx.linked_peer(counterparty);
                tx.save_linked_peer(crate::LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: Some(now),
                    last_private_receive_at: previous
                        .as_ref()
                        .and_then(|peer| peer.last_private_receive_at),
                    failure_count: previous
                        .as_ref()
                        .map(|peer| peer.failure_count.saturating_add(1))
                        .unwrap_or(1),
                });
                if let Some(link_state) = tx.encrypted_link_state(counterparty) {
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty: counterparty.clone(),
                        link_snapshot: None,
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: link_state.generation.saturating_add(1),
                        checkpointed_at: now,
                    });
                }
                Ok(())
            })
            .await
    }

    fn private_recovery_window_open(&self, started_at: DateTime<Utc>) -> Result<bool> {
        let timeout =
            ChronoDuration::from_std(self.config.private_recovery_timeout).map_err(|err| {
                PaykitSdkError::Policy(format!("invalid private recovery timeout: {err}"))
            })?;
        Ok(self.clock.now() < started_at + timeout)
    }

    /// Publish current public receiving details and remove stale SDK-managed endpoints.
    pub async fn sync_public_endpoints(&self) -> Result<EndpointSyncReport> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
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
            self.storage
                .transaction({
                    let record = desired_record(identifier, payload, now);
                    move |tx| {
                        tx.save_public_endpoint_record(record);
                        Ok(())
                    }
                })
                .await?;
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
                let remote_identifiers = current
                    .payment_endpoints
                    .keys()
                    .map(|identifier| identifier.as_str().to_owned())
                    .collect::<HashSet<_>>();
                let already_removed = self
                    .storage
                    .transaction(|tx| Ok(tx.public_endpoint_records()))
                    .await?
                    .into_iter()
                    .filter(|record| {
                        matches!(
                            record.status,
                            EndpointPublicationStatus::PendingRemoval
                                | EndpointPublicationStatus::Failed
                        ) && !remote_identifiers.contains(&record.identifier)
                            && !desired
                                .keys()
                                .any(|identifier| identifier.as_str() == record.identifier)
                    })
                    .collect::<Vec<_>>();
                for record in already_removed {
                    self.storage
                        .transaction({
                            let removed = removed_record(record.identifier.clone(), now);
                            move |tx| {
                                tx.save_public_endpoint_record(removed);
                                Ok(())
                            }
                        })
                        .await?;
                    report.removed.push(EndpointSyncChange {
                        identifier: record.identifier,
                        status: EndpointPublicationStatus::Removed,
                        error: None,
                    });
                }
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
            self.storage
                .transaction({
                    let record = pending_removal_record(
                        identifier_text.clone(),
                        previous_payload.clone(),
                        now,
                    );
                    move |tx| {
                        tx.save_public_endpoint_record(record);
                        Ok(())
                    }
                })
                .await?;
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
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .receive_private_messages_with_claim(counterparty, lease.clone(), session_access)
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    async fn receive_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
        session_access: PubkySessionAccess,
    ) -> Result<PrivateStreamIntakeReport> {
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
            mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                now,
            )
            .await?;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
        };
        let snapshot = match paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let now = self.clock.now();
                mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    now,
                )
                .await?;
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
            handshake_snapshot: None,
            handshake_role: None,
            generation: stored_link_state.generation.saturating_add(1),
            checkpointed_at: now,
        };

        persist_private_stream_batch_with_link_lease(
            &self.storage,
            counterparty,
            messages,
            Some(next_link_state),
            Some(lease),
            now,
        )
        .await
    }

    /// Send queued outbound private messages for one counterparty in order.
    pub async fn process_outbound_private_messages(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateSendReport> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let report = OutboundPrivateSendReport::default();
        let queued = queued_outbound_private_messages(&self.storage, &counterparty).await?;
        if queued.is_empty() {
            return Ok(report);
        }
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;

        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .process_outbound_private_messages_with_claim(
                counterparty,
                report,
                lease.clone(),
                session_access,
            )
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    async fn process_outbound_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        mut report: OutboundPrivateSendReport,
        lease: PeerLinkOperationLease,
        session_access: PubkySessionAccess,
    ) -> Result<OutboundPrivateSendReport> {
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
            mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                now,
            )
            .await?;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
        };
        let snapshot = match paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let now = self.clock.now();
                mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    now,
                )
                .await?;
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
            let Some(sending) = claim_next_outbound_private_message_with_peer_lease(
                &self.storage,
                &counterparty,
                now,
                stale_before,
                lease.clone(),
            )
            .await?
            else {
                break;
            };
            report.attempted.push(sending.outbound_message_id);

            if let Err(err) = validate_queued_outbound_private_message(&sending) {
                let now = self.clock.now();
                let error = err.to_string();
                let failed = mark_outbound_invalid(sending, error.clone(), now);
                self.storage
                    .transaction({
                        let failed = failed.clone();
                        let lease = lease.clone();
                        move |tx| {
                            crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                            tx.save_outbound_private_message(failed);
                            Ok(())
                        }
                    })
                    .await?;
                report.failed.push(OutboundPrivateSendFailure {
                    outbound_message_id: failed.outbound_message_id,
                    error,
                });
                continue;
            }

            match link
                .send_private_application_message_json(&sending.raw_json)
                .await
            {
                Ok(()) => {
                    let now = self.clock.now();
                    let sent = mark_outbound_sent(sending, now);
                    link_state.link_snapshot = Some(link.serialize());
                    link_state.handshake_snapshot = None;
                    link_state.handshake_role = None;
                    link_state.generation = link_state.generation.saturating_add(1);
                    link_state.checkpointed_at = now;
                    self.storage
                        .transaction({
                            let sent = sent.clone();
                            let link_state = link_state.clone();
                            let lease = lease.clone();
                            move |tx| {
                                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
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
                            let lease = lease.clone();
                            move |tx| {
                                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
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
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for public Payment Endpoint lookup"
                        .into(),
                    source: None,
                })?;
        let payment_list =
            paykit_lib::get_payment_list(&public_storage, &counterparty.to_public_key()?).await?;
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

    async fn start_link_handshake(
        &self,
        counterparty: PubkyPublicKey,
        role: EncryptedLinkHandshakeRole,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.load_session_access_and_refresh_identity().await?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .start_link_handshake_with_claim(counterparty, role, lease.clone())
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    async fn start_link_handshake_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        role: EncryptedLinkHandshakeRole,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.ensure_peer_not_blocked(&counterparty).await?;
        if let Some(existing) = self
            .storage
            .transaction(|tx| Ok(tx.encrypted_link_state(&counterparty)))
            .await?
        {
            if existing.link_snapshot.is_some() {
                save_linked_peer_state_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    LinkedPeerState::Linked,
                    lease.clone(),
                    self.clock.now(),
                )
                .await?;
                return Ok(LinkedPeerHandshakeReport {
                    counterparty,
                    state: LinkedPeerState::Linked,
                    generation: existing.generation,
                    handshake_role: None,
                });
            }
            if existing.handshake_snapshot.is_some() {
                if existing.handshake_role.is_none() {
                    mark_recovery_required_with_lease(
                        &self.storage,
                        counterparty.clone(),
                        lease.clone(),
                        self.clock.now(),
                    )
                    .await?;
                    return Err(PaykitSdkError::RecoveryRequired(format!(
                        "missing Encrypted Link Handshake role for counterparty {counterparty}"
                    )));
                }
                save_linked_peer_state_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    LinkedPeerState::Linking,
                    lease.clone(),
                    self.clock.now(),
                )
                .await?;
                return Ok(LinkedPeerHandshakeReport {
                    counterparty,
                    state: LinkedPeerState::Linking,
                    generation: existing.generation,
                    handshake_role: existing.handshake_role,
                });
            }
        }

        let (session_access, secret_key) = self.private_link_session_access().await?;
        let remote_public_key = counterparty.to_public_key()?;
        let handshake = match role {
            EncryptedLinkHandshakeRole::Initiator => paykit_lib::initiate_encrypted_link(
                session_access.session,
                secret_key,
                &remote_public_key,
                session_access.outbox_client,
            )?,
            EncryptedLinkHandshakeRole::Responder => paykit_lib::accept_encrypted_link(
                session_access.session,
                secret_key,
                &remote_public_key,
                session_access.outbox_client,
            )?,
        };

        save_link_handshake_state_with_lease(
            &self.storage,
            counterparty,
            role,
            handshake.serialize(),
            lease,
            self.clock.now(),
        )
        .await
    }

    async fn claim_peer_link_operation(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PeerLinkOperationLease> {
        let now = self.clock.now();
        let lease_timeout = ChronoDuration::from_std(self.config.peer_link_operation_lease_timeout)
            .map_err(|err| {
                PaykitSdkError::Policy(format!("invalid peer link lease timeout: {err}"))
            })?;
        let expires_at = now + lease_timeout;
        self.storage
            .transaction(|tx| Ok(tx.claim_peer_link_operation(counterparty, now, expires_at)))
            .await?
            .ok_or_else(|| {
                PaykitSdkError::Policy(format!(
                    "peer link operation already in progress for counterparty {counterparty}"
                ))
            })
    }

    async fn release_peer_link_operation(&self, lease: &PeerLinkOperationLease) -> Result<()> {
        self.storage
            .transaction(|tx| {
                tx.release_peer_link_operation(&lease.counterparty, lease.lease_id);
                Ok(())
            })
            .await
    }

    async fn finish_peer_link_operation<T>(
        &self,
        lease: PeerLinkOperationLease,
        result: Result<T>,
    ) -> Result<T> {
        let release_result = self.release_peer_link_operation(&lease).await;
        match (result, release_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    async fn ensure_peer_not_blocked(&self, counterparty: &PubkyPublicKey) -> Result<()> {
        let peer = self
            .storage
            .transaction(|tx| Ok(tx.linked_peer(counterparty)))
            .await?;
        if peer
            .as_ref()
            .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
        {
            return Err(PaykitSdkError::Policy(format!(
                "Linked Peer {counterparty} is blocked"
            )));
        }
        Ok(())
    }

    async fn advance_link_handshake_from_snapshot(
        &self,
        counterparty: PubkyPublicKey,
        snapshot_bytes: &[u8],
        handshake_role: EncryptedLinkHandshakeRole,
        expected_generation: u64,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerHandshakeReport> {
        let (session_access, secret_key) = self.private_link_session_access().await?;
        let remote_public_key = counterparty.to_public_key()?;
        let snapshot = paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(snapshot_bytes)?;
        let handshake = paykit_lib::restore_encrypted_link_handshake(
            session_access.session,
            secret_key,
            &remote_public_key,
            session_access.outbox_client,
            snapshot,
        )
        .await?;

        match paykit_lib::advance_handshake(handshake).await? {
            paykit_lib::HandshakeProgress::Pending(handshake) => {
                save_link_handshake_state_if_generation_with_lease(
                    &self.storage,
                    counterparty,
                    handshake_role,
                    handshake.serialize(),
                    expected_generation,
                    lease,
                    self.clock.now(),
                )
                .await
            }
            paykit_lib::HandshakeProgress::Complete(link) => {
                save_linked_peer_link_state_if_generation_with_lease(
                    &self.storage,
                    counterparty,
                    link.serialize(),
                    expected_generation,
                    lease,
                    self.clock.now(),
                )
                .await
            }
        }
    }

    async fn private_link_session_access(&self) -> Result<(PubkySessionAccess, [u8; 32])> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
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
        Ok((session_access, secret_key))
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

fn should_mark_link_recovery_required(err: &PaykitSdkError) -> bool {
    matches!(
        err,
        PaykitSdkError::Transport { .. }
            | PaykitSdkError::Protocol(_)
            | PaykitSdkError::RecoveryRequired(_)
    )
}

enum PrivateRecoveryOutcome {
    NotNeeded,
    Pending,
    PublicOnly,
    Refreshed(Vec<PaymentEndpointCandidate>),
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

fn status_resolution(
    status: ContactPaymentResolutionStatus,
    evaluations: Vec<PaymentEndpointEvaluation>,
    used_public_fallback: bool,
) -> ContactPaymentResolution {
    ContactPaymentResolution {
        status,
        selected_endpoint: None,
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
            PaymentEndpointSelection, PaymentEndpointSelectionRequest, PaymentTarget,
            ReceivingDetail, ReceivingDetailScope,
        },
        private_stream::persist_private_stream_batch,
        storage::{EncryptedLinkStateRecord, InMemoryStorage},
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
    }

    struct PrivateListPaymentAdapter;

    #[async_trait]
    impl PaymentAdapter for PrivateListPaymentAdapter {
        async fn current_receiving_details(
            &self,
            scope: ReceivingDetailScope,
        ) -> Result<Vec<ReceivingDetail>> {
            assert!(matches!(scope, ReceivingDetailScope::Private { .. }));
            Ok(vec![ReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "ln-private".into(),
            }])
        }

        async fn select_payment_endpoint(
            &self,
            _request: &PaymentEndpointSelectionRequest,
        ) -> Result<PaymentEndpointSelection> {
            Ok(PaymentEndpointSelection {
                selected: None,
                evaluations: Vec::new(),
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
    async fn test_initialize_signed_out_clears_identity_scoped_state() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_identity_state(IdentityState {
                        public_key: Some(counterparty.clone()),
                        capability: PubkyIdentityCapability::PrivateLinkCapable,
                        local_secret_available: true,
                        initialized_at: FixedClock.now(),
                        sign_out_generation: 3,
                    });
                    tx.save_linked_peer(crate::LinkedPeerRecord {
                        counterparty: counterparty.clone(),
                        state: LinkedPeerState::Linked,
                        last_sync_at: Some(FixedClock.now()),
                        last_private_receive_at: None,
                        failure_count: 0,
                    });
                    tx.save_public_endpoint_record(crate::PublicEndpointRecord {
                        identifier: "btc-lightning-bolt11".into(),
                        payload: Some("payload".into()),
                        status: EndpointPublicationStatus::Published,
                        updated_at: FixedClock.now(),
                        last_error: None,
                    });
                    tx.insert_outbound_private_message(
                        crate::storage::NewOutboundPrivateMessage::new(
                            counterparty,
                            "paykit.private_payment_list".into(),
                            private_list_json(),
                            FixedClock.now(),
                        ),
                    );
                    Ok(())
                }
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        sdk.initialize().await.unwrap();

        let snapshot = storage.snapshot().unwrap();
        let identity = snapshot.identity_state.unwrap();
        assert_eq!(identity.sign_out_generation, 4);
        assert!(snapshot.linked_peers.is_empty());
        assert!(snapshot.public_endpoint_records.is_empty());
        assert!(snapshot.outbound_private_messages.is_empty());
    }

    #[tokio::test]
    async fn test_receive_private_messages_requires_pubky_session() {
        let storage = InMemoryStorage::new();
        let pubky = TestPubkySessionProvider { session: None };
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
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
            storage.clone(),
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

    async fn seed_private_capable_identity_and_link(
        storage: &InMemoryStorage,
        counterparty: PubkyPublicKey,
    ) {
        storage
            .save_identity_state(IdentityState {
                public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                capability: PubkyIdentityCapability::PrivateLinkCapable,
                local_secret_available: true,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            })
            .await
            .unwrap();
        storage
            .transaction(move |tx| {
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
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

    #[tokio::test]
    async fn test_enqueue_private_payment_list_requires_live_session_for_stored_link() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            PrivateListPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.enqueue_private_payment_list(counterparty).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        let snapshot = storage.snapshot().unwrap();
        assert!(snapshot.encrypted_link_states.is_empty());
    }

    #[tokio::test]
    async fn test_enqueue_private_payment_list_requires_private_capable_identity() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let sdk = PaykitSdk::with_clock(
            storage,
            TestPubkySessionProvider { session: None },
            PrivateListPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.enqueue_private_payment_list(counterparty).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }

    #[tokio::test]
    async fn test_initiate_link_with_peer_requires_pubky_session() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let sdk = PaykitSdk::with_clock(
            storage,
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.initiate_link_with_peer(counterparty).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }

    #[tokio::test]
    async fn test_initiate_link_with_peer_requires_session_before_using_stored_link() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.initiate_link_with_peer(counterparty).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        let snapshot = storage.snapshot().unwrap();
        assert!(snapshot.encrypted_link_states.is_empty());
    }

    #[tokio::test]
    async fn test_initiate_link_with_peer_clears_untrusted_linking_state() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        crate::linked_peers::save_link_handshake_state(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Initiator,
            vec![1, 2, 3],
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

        let result = sdk.initiate_link_with_peer(counterparty.clone()).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        assert!(crate::load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_initiate_link_with_peer_requires_identity_before_block_policy() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        crate::linked_peers::save_linked_peer_state(
            &storage,
            counterparty.clone(),
            LinkedPeerState::Blocked,
            FixedClock.now(),
        )
        .await
        .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage,
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.initiate_link_with_peer(counterparty).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }

    #[tokio::test]
    async fn test_initiate_link_with_peer_requires_identity_before_peer_lease() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + chrono::Duration::seconds(60),
                    );
                    Ok(())
                }
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage,
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.initiate_link_with_peer(counterparty).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }

    #[tokio::test]
    async fn test_advance_link_handshake_clears_untrusted_missing_snapshot_state() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty,
                        link_snapshot: None,
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: 0,
                        checkpointed_at: FixedClock.now(),
                    });
                    Ok(())
                }
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.advance_link_handshake(counterparty.clone()).await;

        assert!(matches!(result, Err(PaykitSdkError::RecoveryRequired(_))));
        assert!(crate::load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_advance_link_handshake_clears_untrusted_state_without_session() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty,
                        link_snapshot: None,
                        handshake_snapshot: Some(vec![1, 2, 3]),
                        handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
                        generation: 0,
                        checkpointed_at: FixedClock.now(),
                    });
                    Ok(())
                }
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.advance_link_handshake(counterparty.clone()).await;

        assert!(matches!(result, Err(PaykitSdkError::RecoveryRequired(_))));
        assert!(crate::load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_advance_link_handshake_clears_untrusted_missing_role_state() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty,
                        link_snapshot: None,
                        handshake_snapshot: Some(vec![1, 2, 3]),
                        handshake_role: None,
                        generation: 0,
                        checkpointed_at: FixedClock.now(),
                    });
                    Ok(())
                }
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.advance_link_handshake(counterparty.clone()).await;

        assert!(matches!(result, Err(PaykitSdkError::RecoveryRequired(_))));
        assert!(crate::load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_process_outbound_private_messages_clears_untrusted_queue() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        crate::outbound_private::enqueue_private_message(
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

        let report = result.unwrap();
        assert!(report.attempted.is_empty());
        assert!(report.sent.is_empty());
        assert!(report.failed.is_empty());
        let queued = crate::queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();
        assert!(queued.is_empty());
    }

    #[tokio::test]
    async fn test_resolve_contact_payment_requires_private_capability_for_private_list() {
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
            storage.clone(),
            pubky,
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk
            .resolve_contact_payment(ContactPaymentResolutionRequest {
                counterparty: counterparty.clone(),
                amount: Some(crate::PaymentAmountContext {
                    value: "10.00".into(),
                    asset: "usd".into(),
                }),
            })
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        assert!(sdk
            .current_private_payment_list(&counterparty)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_recover_private_candidates_reports_pending_for_linking_peer() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_identity_state(IdentityState {
                        public_key: Some(PubkyPublicKey::from_public_key(
                            &pubky::Keypair::random().public_key(),
                        )),
                        capability: PubkyIdentityCapability::PrivateLinkCapable,
                        local_secret_available: true,
                        initialized_at: FixedClock.now(),
                        sign_out_generation: 0,
                    });
                    tx.save_linked_peer(crate::LinkedPeerRecord {
                        counterparty,
                        state: LinkedPeerState::Linking,
                        last_sync_at: Some(FixedClock.now()),
                        last_private_receive_at: None,
                        failure_count: 0,
                    });
                    Ok(())
                }
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage,
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let outcome = sdk
            .recover_private_candidates_for_resolution(&counterparty)
            .await
            .unwrap();

        assert!(matches!(outcome, PrivateRecoveryOutcome::Pending));
    }

    #[tokio::test]
    async fn test_mark_private_recovery_pending_skips_newer_link_generation() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_linked_peer(crate::LinkedPeerRecord {
                        counterparty: counterparty.clone(),
                        state: LinkedPeerState::Linked,
                        last_sync_at: Some(FixedClock.now()),
                        last_private_receive_at: None,
                        failure_count: 0,
                    });
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty,
                        link_snapshot: Some(vec![4, 5, 6]),
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: 2,
                        checkpointed_at: FixedClock.now(),
                    });
                    Ok(())
                }
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        sdk.mark_private_recovery_pending(&counterparty, Some(1))
            .await
            .unwrap();

        let peer = crate::load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::Linked);

        sdk.mark_private_recovery_pending(&counterparty, Some(2))
            .await
            .unwrap();

        let peer = crate::load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert!(link_state.link_snapshot.is_none());
        assert_eq!(link_state.generation, 3);
    }
}
