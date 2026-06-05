use std::{cmp::Reverse, collections::HashSet};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use paykit_lib::{
    BillingPeriod, EncryptedLinkRecoveryMarker, EventId, PaymentEndpointIdentifier, PaymentProof,
    PaymentRequest, PaymentRequestAcceptance, PaymentRequestCancellation, PaymentRequestEvent,
    PaymentRequestId, PaymentRequestRejection, PaymentRequestTerms,
};
use pubky::{errors::RequestError, Error as PubkyError, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    backup::{
        export_backup_state as export_sdk_backup_state,
        restore_backup_state as restore_sdk_backup_state, RestoreReport, SdkBackupState,
    },
    config::{
        EncryptedLinkRecoveryMarkerPolicy, EndpointManagementScope, PaykitSdkConfig,
        PrivateSharingPolicy, PublicContactSharingPolicy, PublicFallbackPolicy,
    },
    contacts::{
        parse_profile_json, profile_json, public_contact_json, public_contact_path,
        ContactPaymentResolution, ContactPaymentResolutionRequest, ContactPaymentResolutionStatus,
        ContactRecord, ContactUpdate, PaykitProfile, PaykitProfileRecord, PAYKIT_PROFILE_PATH,
    },
    endpoint_reservations::{
        payment_endpoint_reservations, queue_private_payment_list_with_reservations,
        reservation_payload_hash,
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
    payment_requests::{
        enqueue_payment_proof as enqueue_payment_proof_message,
        enqueue_payment_request as enqueue_payment_request_message,
        enqueue_payment_request_acceptance as enqueue_payment_request_acceptance_message,
        enqueue_payment_request_cancellation as enqueue_payment_request_cancellation_message,
        enqueue_payment_request_event as enqueue_payment_request_event_message,
        enqueue_payment_request_rejection as enqueue_payment_request_rejection_message,
        payment_request_records as derive_payment_request_records,
        received_payment_request_records as derive_received_payment_request_records,
        request_from_record, PaymentRequestLifecycleState, PaymentRequestLocalRole,
        PaymentRequestRecord,
    },
    private_lists::{
        current_private_payment_list as load_current_private_payment_list,
        enqueue_private_payment_list as enqueue_private_payment_list_message,
    },
    private_stream::{persist_private_stream_batch_with_link_lease, PrivateStreamIntakeReport},
    receipts::{
        decrypt_receipt_record_from_access, fetch_encrypted_receipt_json,
        receipt_record_matches_access, ReceiptAccessRecord, ReceiptRecord, ReceiptRetrievalStatus,
    },
    recovery::{recovery_marker_report, EncryptedLinkRecoveryMarkerReport},
    storage::{
        EncryptedLinkStateRecord, LinkedPeerRecord, OutboundPrivateMessageRecord,
        PeerLinkOperationLease, StorageAdapter,
    },
    PaykitSdkError, PaymentAdapter, PaymentEndpointCandidate, PaymentEndpointEvaluation,
    PaymentEndpointReservation, PaymentEndpointReservationRelease,
    PaymentEndpointReservationRequest, PaymentEndpointSelection, PaymentEndpointSelectionRequest,
    PaymentEndpointSource, PrivatePaymentListView, PubkyPublicKey, PubkySessionAccess,
    PubkySessionProvider, ReceivingDetail, ReceivingDetailScope, Result,
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

    async fn require_initialized_identity(&self, context: &str) -> Result<PubkyPublicKey> {
        self.storage
            .transaction(|tx| {
                tx.load_identity_state()
                    .and_then(|state| state.public_key)
                    .ok_or_else(|| PaykitSdkError::Identity {
                        context: format!("cannot {context} without an initialized Pubky identity"),
                        source: None,
                    })
            })
            .await
    }

    async fn load_session_access_for_initialized_identity(
        &self,
        context: &str,
    ) -> Result<PubkySessionAccess> {
        let expected_public_key = self.require_initialized_identity(context).await?;
        let session_access =
            self.pubky
                .load_session_access()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: format!("cannot {context} without an active Pubky session"),
                    source: None,
                })?;
        let actual_public_key = session_access.public_key()?;
        if actual_public_key != expected_public_key {
            return Err(PaykitSdkError::Identity {
                context: format!(
                    "cannot {context} because active Pubky session does not match initialized identity"
                ),
                source: None,
            });
        }
        Ok(session_access)
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

    /// Save or update a local contact record.
    pub async fn save_contact(&self, update: ContactUpdate) -> Result<ContactRecord> {
        update.validate()?;
        self.require_initialized_identity("save contact").await?;
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let existing = tx.contact_record(&update.public_key);
                let record = ContactRecord::from_update(update, existing, now);
                tx.save_contact_record(record.clone());
                Ok(record)
            })
            .await
    }

    /// Load one local contact record.
    pub async fn contact_record(
        &self,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        self.require_initialized_identity("load contact").await?;
        self.storage
            .transaction(|tx| Ok(tx.contact_record(public_key)))
            .await
    }

    /// List local contact records.
    pub async fn contact_records(&self) -> Result<Vec<ContactRecord>> {
        self.require_initialized_identity("list contacts").await?;
        self.storage
            .transaction(|tx| Ok(tx.contact_records()))
            .await
    }

    /// Remove one local contact record.
    pub async fn remove_contact(
        &self,
        public_key: &PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        self.require_initialized_identity("remove contact").await?;
        self.storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Ok(None);
                };
                if !existing.can_remove_locally() {
                    return Err(PaykitSdkError::Policy(format!(
                        "remove public contact marker before deleting contact {public_key}"
                    )));
                }
                Ok(tx.remove_contact_record(public_key))
            })
            .await
    }

    /// Publish the local public Paykit profile.
    ///
    /// This writes only `/pub/paykit/profile.json`. Profile image blob upload
    /// and deletion are caller-managed.
    pub async fn publish_profile(&self, profile: PaykitProfile) -> Result<PaykitProfileRecord> {
        let json = profile_json(&profile)?;
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available for profile publication".into(),
            source: None,
        })?;
        session_access
            .session
            .storage()
            .put(PAYKIT_PROFILE_PATH, json)
            .await
            .map_err(|err| map_pubky_transport_error("publish Paykit profile", err))?;
        Ok(PaykitProfileRecord {
            public_key: session_access.public_key()?,
            profile,
            path: PAYKIT_PROFILE_PATH.into(),
            updated_at: self.clock.now(),
        })
    }

    /// Fetch a public Paykit profile.
    pub async fn fetch_profile(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<PaykitProfileRecord>> {
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for profile lookup".into(),
                    source: None,
                })?;
        let Some(raw_json) = fetch_public_text(
            &public_storage,
            &public_key,
            PAYKIT_PROFILE_PATH,
            "fetch profile",
        )
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(PaykitProfileRecord {
            public_key,
            profile: parse_profile_json(&raw_json)?,
            path: PAYKIT_PROFILE_PATH.into(),
            updated_at: self.clock.now(),
        }))
    }

    /// Fetch a contact's public profile and cache it in the local contact record.
    pub async fn refresh_contact_profile(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        self.require_initialized_identity("refresh contact profile")
            .await?;
        let fetched = self.fetch_profile(public_key.clone()).await?;
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let Some(existing) = tx.contact_record(&public_key) else {
                    return Ok(None);
                };
                let record = existing.with_profile(fetched.map(|record| record.profile), now);
                tx.save_contact_record(record.clone());
                Ok(Some(record))
            })
            .await
    }

    /// Publish a public contact marker for a saved local contact.
    ///
    /// This can reveal part of the local contact graph. It only runs when
    /// `public_contact_sharing` is `PublicPaykitNamespace`.
    pub async fn publish_public_contact(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<ContactRecord> {
        if self.config.public_contact_sharing != PublicContactSharingPolicy::PublicPaykitNamespace {
            return Err(PaykitSdkError::Policy(
                "public contact sharing is disabled".into(),
            ));
        }
        let session_access = self
            .load_session_access_for_initialized_identity("publish public contact")
            .await?;
        let pending_at = self.clock.now();
        self.storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(&public_key) else {
                    return Err(PaykitSdkError::Protocol(format!(
                        "cannot publish unsaved contact {public_key}"
                    )));
                };
                tx.save_contact_record(
                    existing.mark_public_contact_publication_pending(pending_at),
                );
                Ok(())
            })
            .await?;
        let path = public_contact_path(&public_key);
        let write_result = session_access
            .session
            .storage()
            .put(path, public_contact_json(&public_key)?)
            .await
            .map_err(|err| map_pubky_transport_error("publish public contact", err));
        if let Err(err) = write_result {
            self.mark_public_contact_failed(&public_key, err.to_string())
                .await?;
            return Err(err);
        }
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let Some(existing) = tx.contact_record(&public_key) else {
                    return Err(PaykitSdkError::Protocol(format!(
                        "contact {public_key} disappeared before public publication was recorded"
                    )));
                };
                let record = existing.mark_public_contact_published(now);
                tx.save_contact_record(record.clone());
                Ok(record)
            })
            .await
    }

    /// Remove a public contact marker for a saved local contact.
    ///
    /// Cleanup is allowed even when public contact sharing is disabled, so an
    /// app can stop publishing markers and still remove previously published
    /// markers.
    pub async fn remove_public_contact(
        &self,
        public_key: PubkyPublicKey,
    ) -> Result<Option<ContactRecord>> {
        let existing_record = self
            .storage
            .transaction(|tx| Ok(tx.contact_record(&public_key)))
            .await?;
        let session_access = self
            .load_session_access_for_initialized_identity("remove public contact")
            .await?;
        let had_local_record = existing_record.is_some();
        if existing_record
            .as_ref()
            .is_some_and(ContactRecord::may_have_public_marker)
        {
            let pending_at = self.clock.now();
            self.storage
                .transaction(|tx| {
                    let Some(existing) = tx.contact_record(&public_key) else {
                        return Ok(());
                    };
                    tx.save_contact_record(
                        existing.mark_public_contact_removal_pending(pending_at),
                    );
                    Ok(())
                })
                .await?;
        }
        let path = public_contact_path(&public_key);
        let delete_result = session_access.session.storage().delete(path).await;
        if let Err(err) = delete_result {
            if !is_pubky_not_found(&err) {
                let err = map_pubky_transport_error("remove public contact", err);
                self.mark_public_contact_failed(&public_key, err.to_string())
                    .await?;
                return Err(err);
            }
        }
        if !had_local_record {
            return Ok(None);
        }
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                let Some(existing) = tx.contact_record(&public_key) else {
                    return Ok(None);
                };
                let record = existing.mark_public_contact_removed(now);
                tx.save_contact_record(record.clone());
                Ok(Some(record))
            })
            .await
    }

    async fn mark_public_contact_failed(
        &self,
        public_key: &PubkyPublicKey,
        error: String,
    ) -> Result<()> {
        let failed_at = self.clock.now();
        self.storage
            .transaction(|tx| {
                let Some(existing) = tx.contact_record(public_key) else {
                    return Ok(());
                };
                tx.save_contact_record(existing.mark_public_contact_failed(error, failed_at));
                Ok(())
            })
            .await
    }

    /// Access the payment adapter.
    pub fn payment_adapter(&self) -> &P {
        &self.payment
    }

    /// Access the Pubky session provider.
    pub fn pubky_session_provider(&self) -> &K {
        &self.pubky
    }

    /// Export SDK-managed backup state.
    pub async fn export_backup_state(&self) -> Result<SdkBackupState> {
        export_sdk_backup_state(&self.storage).await
    }

    /// Restore SDK-managed backup state.
    pub async fn restore_backup_state(&self, backup: SdkBackupState) -> Result<RestoreReport> {
        if backup.identity_public_key().is_some() || backup.has_identity_scoped_state() {
            let (_, identity) = self.load_session_access_and_refresh_identity().await?;
            let local_public_key =
                identity
                    .public_key
                    .as_ref()
                    .ok_or_else(|| PaykitSdkError::Identity {
                        context:
                            "cannot restore identity-scoped backup without an active Pubky identity"
                                .into(),
                        source: None,
                    })?;
            if backup.identity_public_key() != Some(local_public_key) {
                return Err(PaykitSdkError::Identity {
                    context: "backup identity does not match active Pubky identity".into(),
                    source: None,
                });
            }
            if backup.has_private_identity_scoped_state()
                && identity.capability != PubkyIdentityCapability::PrivateLinkCapable
            {
                return Err(PaykitSdkError::Identity {
                    context: "cannot restore private Paykit state without private-link capability"
                        .into(),
                    source: None,
                });
            }
        }
        restore_sdk_backup_state(&self.storage, backup).await
    }

    /// Return tracked Encrypted Link recovery marker state for a counterparty.
    pub async fn encrypted_link_recovery_marker_status(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Option<EncryptedLinkRecoveryMarkerReport>> {
        recovery_marker_report(&self.storage, counterparty).await
    }

    /// Publish a minimal local recovery marker for a counterparty.
    pub async fn publish_encrypted_link_recovery_marker(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let new_episode = self
            .mark_recovery_required_without_lease(&counterparty)
            .await?;
        self.publish_local_recovery_marker_with_session(&counterparty, &session_access, new_episode)
            .await
    }

    /// Observe a counterparty's public recovery marker.
    pub async fn observe_encrypted_link_recovery_marker(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        self.observe_remote_recovery_marker_with_session(&counterparty, &session_access)
            .await
    }

    /// Remove the local public recovery marker for a counterparty.
    pub async fn remove_encrypted_link_recovery_marker(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            return self
                .recovery_marker_report_or_default(&counterparty, false)
                .await;
        }

        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let secret_key = session_access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Identity {
                context:
                    "local Pubky secret key is unavailable for Encrypted Link recovery markers"
                        .into(),
                source: None,
            })?
            .as_bytes();
        let remote_public_key = counterparty.to_public_key()?;
        paykit_lib::remove_encrypted_link_recovery_marker(
            &session_access.session,
            secret_key,
            &remote_public_key,
        )
        .await?;

        self.storage
            .transaction(|tx| {
                if let Some(mut peer) = tx.linked_peer(&counterparty) {
                    peer.local_recovery_attempt_id = None;
                    peer.local_recovery_marker_created_at = None;
                    tx.save_linked_peer(peer);
                }
                Ok(())
            })
            .await?;
        self.recovery_marker_report_or_default(&counterparty, false)
            .await
    }

    /// Return the latest valid Private Payment List view for a counterparty.
    pub async fn current_private_payment_list(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Option<crate::PrivatePaymentListView>> {
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Ok(None);
        }
        self.ensure_peer_allows_private_automation(counterparty)
            .await?;
        self.observe_remote_recovery_marker_for_cached_private_state(
            counterparty,
            session_access.as_ref(),
        )
        .await?;
        self.ensure_peer_allows_private_automation(counterparty)
            .await?;
        load_current_private_payment_list(&self.storage, counterparty).await
    }

    /// Fetch, decrypt, and store a receipt from an indexed Receipt Access event.
    ///
    /// The decrypted Receipt is private SDK state. This returns an already
    /// stored Receipt record when available, and otherwise validates the
    /// decrypted recipient against the current local Pubky identity before
    /// saving it.
    pub async fn retrieve_receipt(
        &self,
        counterparty: PubkyPublicKey,
        receipt_id: &str,
    ) -> Result<ReceiptRecord> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        let local_public_key = identity
            .public_key
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "no local Pubky identity available for receipt retrieval".into(),
                source: None,
            })?;
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
        let (stored_receipt, access_records) = self
            .storage
            .transaction(|tx| {
                let stored_receipt = tx.receipt_record(&counterparty, receipt_id);
                let mut access_records = tx
                    .receipt_access_records(&counterparty)
                    .into_iter()
                    .filter(|record| record.receipt_id == receipt_id)
                    .collect::<Vec<_>>();
                access_records.sort_by_key(|record| Reverse(record.stream_item_id));
                Ok((stored_receipt, access_records))
            })
            .await?;
        if let Some(record) = stored_receipt {
            if record.recipient_public_key != local_public_key {
                return Err(PaykitSdkError::Protocol(
                    "stored Receipt recipient does not match local identity".into(),
                ));
            }
            self.reconcile_cached_receipt_access_records(
                &record,
                &access_records,
                self.clock.now(),
            )
            .await?;
            return Ok(record);
        }
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for receipt retrieval".into(),
                    source: None,
                })?;
        if access_records.is_empty() {
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no Receipt Access record for receipt {receipt_id} from {counterparty}"
            )));
        }
        let now = self.clock.now();
        let all_access_records = access_records.clone();
        let mut last_error = None;
        for access in access_records {
            let encrypted_json = match fetch_encrypted_receipt_json(
                &public_storage,
                &counterparty,
                &access.location,
            )
            .await
            {
                Ok(Some(encrypted_json)) => encrypted_json,
                Ok(None) => {
                    let error = format!(
                        "encrypted receipt {} was not found at {}",
                        access.receipt_id, access.location
                    );
                    self.save_receipt_retrieval_error(
                        &access,
                        ReceiptRetrievalStatus::NotFound,
                        now,
                        error.clone(),
                    )
                    .await?;
                    last_error = Some(PaykitSdkError::Transport {
                        context: error,
                        source: None,
                    });
                    continue;
                }
                Err(err) => {
                    let error = err.to_string();
                    self.save_receipt_retrieval_error(
                        &access,
                        ReceiptRetrievalStatus::Failed,
                        now,
                        error,
                    )
                    .await?;
                    last_error = Some(err);
                    continue;
                }
            };

            match decrypt_receipt_record_from_access(
                &access,
                &encrypted_json,
                now,
                &local_public_key,
            ) {
                Ok(record) => {
                    self.storage
                        .transaction({
                            let access = access.mark_retrieved(now);
                            let record = record.clone();
                            move |tx| {
                                tx.save_receipt_access_record(access);
                                tx.save_receipt_record(record);
                                Ok(())
                            }
                        })
                        .await?;
                    self.reconcile_cached_receipt_access_records(&record, &all_access_records, now)
                        .await?;
                    return Ok(record);
                }
                Err(err) => {
                    let error = err.to_string();
                    self.save_receipt_retrieval_error(
                        &access,
                        ReceiptRetrievalStatus::Failed,
                        now,
                        error,
                    )
                    .await?;
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            PaykitSdkError::RecoveryRequired(format!(
                "no usable Receipt Access record for receipt {receipt_id} from {counterparty}"
            ))
        }))
    }

    async fn reconcile_cached_receipt_access_records(
        &self,
        record: &ReceiptRecord,
        access_records: &[ReceiptAccessRecord],
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.storage
            .transaction({
                let record = record.clone();
                let access_records = access_records.to_vec();
                move |tx| {
                    for access in access_records {
                        if receipt_record_matches_access(&record, &access) {
                            if access.retrieval_status != ReceiptRetrievalStatus::Retrieved {
                                tx.save_receipt_access_record(access.mark_retrieved(now));
                            }
                        } else if access.retrieval_status == ReceiptRetrievalStatus::Pending {
                            tx.save_receipt_access_record(access.mark_retrieval_error(
                                ReceiptRetrievalStatus::Failed,
                                now,
                                "Receipt Access does not match stored Receipt".into(),
                            ));
                        }
                    }
                    Ok(())
                }
            })
            .await
    }

    async fn save_receipt_retrieval_error(
        &self,
        access: &ReceiptAccessRecord,
        status: ReceiptRetrievalStatus,
        attempted_at: DateTime<Utc>,
        error: String,
    ) -> Result<()> {
        self.storage
            .transaction({
                let access = access.mark_retrieval_error(status, attempted_at, error);
                move |tx| {
                    tx.save_receipt_access_record(access);
                    Ok(())
                }
            })
            .await
    }

    /// Return received Payment Request records for one counterparty.
    ///
    /// Records are derived from the persisted inbound private stream and
    /// returned newest-first by last applied stream item. Malformed recognized
    /// Payment Request events without a valid `payment_request_id` stay in the
    /// raw private stream log and cannot be attached to a request-scoped record.
    pub async fn received_payment_request_records(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PaymentRequestRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Ok(Vec::new());
        }
        self.ensure_peer_allows_private_automation(counterparty)
            .await?;
        derive_received_payment_request_records(&self.storage, counterparty, self.clock.now()).await
    }

    /// Return merged local Payment Request records for one counterparty.
    ///
    /// Records combine received private-stream events and local outbound
    /// Payment Request events, returned newest-first.
    pub async fn payment_request_records(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PaymentRequestRecord>> {
        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Ok(Vec::new());
        }
        self.ensure_peer_allows_private_automation(counterparty)
            .await?;
        derive_payment_request_records(&self.storage, counterparty, self.clock.now()).await
    }

    async fn ensure_private_outbound_ready(
        &self,
        counterparty: &PubkyPublicKey,
        disabled_message: &str,
    ) -> Result<()> {
        if self.config.private_sharing == PrivateSharingPolicy::Disabled {
            return Err(PaykitSdkError::Policy(disabled_message.into()));
        }

        let (_, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.capability != PubkyIdentityCapability::PrivateLinkCapable {
            return Err(PaykitSdkError::Identity {
                context: "local Pubky identity is not private-link-capable".into(),
                source: None,
            });
        }

        self.ensure_peer_allows_private_automation(counterparty)
            .await?;

        let has_active_link = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .encrypted_link_state(counterparty)
                    .and_then(|state| state.link_snapshot)
                    .is_some())
            })
            .await?;
        if !has_active_link {
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
        }

        Ok(())
    }

    /// Queue a new Payment Request proposal and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn propose_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        terms: PaymentRequestTerms,
    ) -> Result<PaymentRequestRecord> {
        let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);
        let payment_request_id = event.payment_request_id.clone();
        self.enqueue_raw_payment_request(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, &payment_request_id)
            .await
    }

    /// Queue acceptance for a received Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn accept_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_payer_role(&record, "accept Payment Request")?;
        require_state(
            &record,
            &[PaymentRequestLifecycleState::Proposed],
            "accept Payment Request",
        )?;
        let event = PaymentRequestAcceptance::new(EventId::new_v4(), payment_request_id.clone());
        self.enqueue_raw_payment_request_acceptance(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    /// Queue rejection for a received Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn reject_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        reason: Option<String>,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_payer_role(&record, "reject Payment Request")?;
        require_state(
            &record,
            &[PaymentRequestLifecycleState::Proposed],
            "reject Payment Request",
        )?;
        let event =
            PaymentRequestRejection::new(EventId::new_v4(), payment_request_id.clone(), reason);
        self.enqueue_raw_payment_request_rejection(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    /// Queue cancellation for a known non-terminal Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn cancel_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        reason: Option<String>,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_state(
            &record,
            &[
                PaymentRequestLifecycleState::Proposed,
                PaymentRequestLifecycleState::ProposalExpired,
                PaymentRequestLifecycleState::Accepted,
                PaymentRequestLifecycleState::ActiveRecurring,
                PaymentRequestLifecycleState::ProofSubmitted,
            ],
            "cancel Payment Request",
        )?;
        let event =
            PaymentRequestCancellation::new(EventId::new_v4(), payment_request_id.clone(), reason);
        self.enqueue_raw_payment_request_cancellation(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    /// Queue a Payment Proof for an accepted Payment Request and return local derived state.
    ///
    /// The returned record reflects the local outbound queue, not delivery or
    /// counterparty processing.
    pub async fn submit_payment_proof(
        &self,
        counterparty: PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
        billing_period: Option<BillingPeriod>,
        payment_endpoint_identifier: PaymentEndpointIdentifier,
        proof: JsonMap<String, JsonValue>,
    ) -> Result<PaymentRequestRecord> {
        let record = self
            .load_payment_request_record(&counterparty, payment_request_id)
            .await?;
        require_payer_role(&record, "submit Payment Proof")?;
        require_state(
            &record,
            &[
                PaymentRequestLifecycleState::Accepted,
                PaymentRequestLifecycleState::ActiveRecurring,
            ],
            "submit Payment Proof",
        )?;
        let request = request_from_record(&record).ok_or_else(|| {
            PaykitSdkError::Protocol("Payment Request terms are unavailable".into())
        })?;
        let event = PaymentProof::new(
            EventId::new_v4(),
            payment_request_id.clone(),
            request.request.payment_reference.clone(),
            billing_period,
            payment_endpoint_identifier,
            proof,
        );
        event.validate_for_request(&request)?;
        self.enqueue_raw_payment_proof(counterparty.clone(), &event)
            .await?;
        self.load_payment_request_record(&counterparty, payment_request_id)
            .await
    }

    async fn load_payment_request_record(
        &self,
        counterparty: &PubkyPublicKey,
        payment_request_id: &PaymentRequestId,
    ) -> Result<PaymentRequestRecord> {
        derive_payment_request_records(&self.storage, counterparty, self.clock.now())
            .await?
            .into_iter()
            .find(|record| record.payment_request_id == payment_request_id.as_str())
            .ok_or_else(|| {
                PaykitSdkError::Protocol(format!(
                    "Payment Request {} is not known for counterparty {}",
                    payment_request_id, counterparty
                ))
            })
    }

    /// Enqueue one raw Payment Request protocol event for outbound delivery.
    ///
    /// This validates private-send readiness and stores canonical JSON, but it
    /// does not enforce role, lifecycle, or proof/request correlation policy.
    pub async fn enqueue_raw_payment_request_event(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestEvent,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_event_message(&self.storage, counterparty, event, self.clock.now())
            .await
    }

    /// Enqueue a raw Payment Request proposal for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role or lifecycle policy.
    pub async fn enqueue_raw_payment_request(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequest,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_message(&self.storage, counterparty, event, self.clock.now()).await
    }

    /// Enqueue a raw Payment Request acceptance for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role or lifecycle policy.
    pub async fn enqueue_raw_payment_request_acceptance(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestAcceptance,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_acceptance_message(
            &self.storage,
            counterparty,
            event,
            self.clock.now(),
        )
        .await
    }

    /// Enqueue a raw Payment Request rejection for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role or lifecycle policy.
    pub async fn enqueue_raw_payment_request_rejection(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestRejection,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_rejection_message(
            &self.storage,
            counterparty,
            event,
            self.clock.now(),
        )
        .await
    }

    /// Enqueue a raw Payment Request cancellation for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role or lifecycle policy.
    pub async fn enqueue_raw_payment_request_cancellation(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentRequestCancellation,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_request_cancellation_message(
            &self.storage,
            counterparty,
            event,
            self.clock.now(),
        )
        .await
    }

    /// Enqueue a raw Payment Proof for outbound delivery.
    ///
    /// This is a queueing primitive; it does not enforce role, lifecycle, or
    /// proof/request correlation policy.
    pub async fn enqueue_raw_payment_proof(
        &self,
        counterparty: PubkyPublicKey,
        event: &PaymentProof,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment Request messaging is disabled",
        )
        .await?;
        enqueue_payment_proof_message(&self.storage, counterparty, event, self.clock.now()).await
    }

    /// Enqueue the current complete Private Payment List for one counterparty.
    pub async fn enqueue_private_payment_list(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.ensure_private_outbound_ready(
            &counterparty,
            "private Payment List sharing is disabled",
        )
        .await?;

        self.enqueue_private_payment_list_from_receiving_details(counterparty)
            .await
    }

    async fn enqueue_private_payment_list_from_receiving_details(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateMessageRecord> {
        let request = PaymentEndpointReservationRequest {
            counterparty: counterparty.clone(),
        };
        if let Some(reservations) = self.payment.reserve_receiving_details(&request).await? {
            let releases = reservations
                .iter()
                .map(|reservation| reservation_release(&counterparty, reservation))
                .collect::<Vec<_>>();
            let now = self.clock.now();
            let result = queue_private_payment_list_with_reservations(
                &self.storage,
                &request,
                reservations,
                now,
            )
            .await;
            match result {
                Ok(record) => Ok(record),
                Err(err) => {
                    if let Err(release_err) = self
                        .release_reservations_after_queue_error(&releases, &counterparty)
                        .await
                    {
                        return Err(PaykitSdkError::Policy(format!(
                            "failed to queue reserved receiving details: {err}; reservation cleanup also failed: {release_err}"
                        )));
                    }
                    Err(err)
                }
            }
        } else {
            let receiving_details = self.private_receiving_details(&counterparty).await?;
            enqueue_private_payment_list_message(
                &self.storage,
                counterparty,
                receiving_details,
                self.clock.now(),
            )
            .await
        }
    }

    async fn release_reservations_after_queue_error(
        &self,
        releases: &[PaymentEndpointReservationRelease],
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let existing = payment_endpoint_reservations(&self.storage, counterparty).await?;
        let mut release_errors = Vec::new();
        for release in releases {
            if existing.iter().any(|record| {
                record.reservation_id == release.reservation_id
                    && record.counterparty == release.counterparty
                    && record.identifier == release.identifier
                    && record.payload_hash == release.payload_hash
            }) {
                continue;
            }
            if let Err(err) = self
                .payment
                .release_receiving_detail_reservation(release)
                .await
            {
                release_errors.push(format!("{}: {err}", release.reservation_id));
            }
        }
        if release_errors.is_empty() {
            Ok(())
        } else {
            Err(PaykitSdkError::Policy(format!(
                "failed to release reserved receiving details: {}",
                release_errors.join("; ")
            )))
        }
    }

    async fn private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<ReceivingDetail>> {
        self.payment
            .current_receiving_details(ReceivingDetailScope::Private {
                counterparty: counterparty.clone(),
            })
            .await
    }

    async fn ensure_peer_allows_private_automation(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let peer_state = self
            .storage
            .transaction(|tx| Ok(tx.linked_peer(counterparty).map(|peer| peer.state)))
            .await?;
        match peer_state {
            Some(LinkedPeerState::RecoveryRequired) => Err(PaykitSdkError::RecoveryRequired(
                format!("Encrypted Link recovery is required for counterparty {counterparty}"),
            )),
            Some(LinkedPeerState::Blocked) => Err(PaykitSdkError::Policy(format!(
                "counterparty {counterparty} is blocked"
            ))),
            _ => Ok(()),
        }
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
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
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
            let mark = mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                self.clock.now(),
            )
            .await?;
            self.publish_local_recovery_marker_if_possible(&counterparty, mark.new_episode)
                .await;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "missing Encrypted Link Handshake role for counterparty {counterparty}"
            )));
        };
        let Some(snapshot_bytes) = stored_link_state.handshake_snapshot.as_ref() else {
            let mark = mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                self.clock.now(),
            )
            .await?;
            self.publish_local_recovery_marker_if_possible(&counterparty, mark.new_episode)
                .await;
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
            let mark = mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease,
                self.clock.now(),
            )
            .await?;
            self.publish_local_recovery_marker_if_possible(&counterparty, mark.new_episode)
                .await;
        }
        result
    }

    /// Resolve a payable endpoint for one counterparty.
    pub async fn resolve_contact_payment(
        &self,
        request: ContactPaymentResolutionRequest,
    ) -> Result<ContactPaymentResolution> {
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        let mut evaluations = Vec::new();
        let mut private_allowed = match self
            .ensure_peer_allows_private_automation(&request.counterparty)
            .await
        {
            Ok(()) => true,
            Err(PaykitSdkError::RecoveryRequired(_)) => false,
            Err(err) => return Err(err),
        };
        if private_allowed && identity.capability == PubkyIdentityCapability::PrivateLinkCapable {
            if let Err(err) = self
                .observe_remote_recovery_marker_for_cached_private_state(
                    &request.counterparty,
                    session_access.as_ref(),
                )
                .await
            {
                if self.config.public_fallback == PublicFallbackPolicy::Disabled {
                    return Err(err);
                }
                private_allowed = false;
            }
        }
        if private_allowed && identity.capability == PubkyIdentityCapability::PrivateLinkCapable {
            private_allowed = match self
                .ensure_peer_allows_private_automation(&request.counterparty)
                .await
            {
                Ok(()) => true,
                Err(PaykitSdkError::RecoveryRequired(_)) => false,
                Err(err) => return Err(err),
            };
        }
        let private_view = if private_allowed
            && identity.capability == PubkyIdentityCapability::PrivateLinkCapable
        {
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
            if let Err(err) = self
                .observe_remote_recovery_marker_for_cached_private_state(counterparty, None)
                .await
            {
                if self.config.public_fallback == PublicFallbackPolicy::Disabled {
                    return Err(err);
                }
                return Ok(PrivateRecoveryOutcome::NotNeeded);
            }

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
                    let recovery_update = self
                        .mark_private_recovery_pending(counterparty, link_generation)
                        .await?;
                    if let RecoveryRequiredUpdate::Marked { new_episode } = recovery_update {
                        self.publish_local_recovery_marker_if_possible(counterparty, new_episode)
                            .await;
                    }
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
    ) -> Result<RecoveryRequiredUpdate> {
        let now = self.clock.now();
        self.storage
            .transaction(|tx| {
                let current_generation = tx
                    .encrypted_link_state(counterparty)
                    .map(|state| state.generation);
                if current_generation != expected_link_generation {
                    return Ok(RecoveryRequiredUpdate::Skipped);
                }
                let mut peer = recovery_peer_or_default(tx.linked_peer(counterparty), counterparty);
                let new_episode = peer.state != LinkedPeerState::RecoveryRequired;
                peer.state = LinkedPeerState::RecoveryRequired;
                if new_episode || peer.last_sync_at.is_none() {
                    peer.last_sync_at = Some(now);
                }
                peer.failure_count = peer.failure_count.saturating_add(1);
                tx.save_linked_peer(peer);
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
                Ok(RecoveryRequiredUpdate::Marked { new_episode })
            })
            .await
    }

    async fn mark_recovery_required_without_lease(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<bool> {
        let now = self.clock.now();
        self.storage
            .transaction(|tx| {
                let existing_peer = tx.linked_peer(counterparty);
                let has_link_state = tx.encrypted_link_state(counterparty).is_some();
                if !can_publish_recovery_marker(existing_peer.as_ref(), has_link_state) {
                    return Err(PaykitSdkError::Policy(format!(
                        "cannot publish Encrypted Link recovery marker without existing private link state for counterparty {counterparty}"
                    )));
                }
                let mut peer = recovery_peer_or_default(existing_peer, counterparty);
                if peer.state == LinkedPeerState::Blocked {
                    return Err(PaykitSdkError::Policy(format!(
                        "counterparty {counterparty} is blocked"
                    )));
                }
                let new_episode = peer.state != LinkedPeerState::RecoveryRequired;
                if peer.state != LinkedPeerState::RecoveryRequired {
                    peer.failure_count = peer.failure_count.saturating_add(1);
                }
                peer.state = LinkedPeerState::RecoveryRequired;
                if new_episode || peer.last_sync_at.is_none() {
                    peer.last_sync_at = Some(now);
                }
                tx.save_linked_peer(peer);
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
                Ok(new_episode)
            })
            .await
    }

    async fn publish_local_recovery_marker_if_possible(
        &self,
        counterparty: &PubkyPublicKey,
        force_new_attempt: bool,
    ) {
        let Ok(Some(session_access)) = self.pubky.load_session_access().await else {
            return;
        };
        let _ = self
            .publish_local_recovery_marker_with_session(
                counterparty,
                &session_access,
                force_new_attempt,
            )
            .await;
    }

    async fn publish_local_recovery_marker_with_session(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        force_new_attempt: bool,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            return self
                .recovery_marker_report_or_default(counterparty, false)
                .await;
        }

        let secret_key = session_access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Identity {
                context:
                    "local Pubky secret key is unavailable for Encrypted Link recovery markers"
                        .into(),
                source: None,
            })?
            .as_bytes();
        let remote_public_key = counterparty.to_public_key()?;
        let now = self.clock.now();
        let marker = self
            .storage
            .transaction(|tx| {
                let mut peer = recovery_peer_or_default(tx.linked_peer(counterparty), counterparty);
                let has_link_state = tx.encrypted_link_state(counterparty).is_some();
                if !can_publish_recovery_marker(Some(&peer), has_link_state) {
                    return Err(PaykitSdkError::Policy(format!(
                        "cannot publish Encrypted Link recovery marker without existing private link state for counterparty {counterparty}"
                    )));
                }
                let reusable_marker = if !force_new_attempt
                    && peer.state == LinkedPeerState::RecoveryRequired
                    && local_recovery_marker_belongs_to_current_episode(&peer)
                {
                    peer.local_recovery_attempt_id
                        .as_ref()
                        .zip(peer.local_recovery_marker_created_at)
                        .map(|(attempt_id, created_at)| {
                            let created_at_text =
                                created_at.to_rfc3339_opts(SecondsFormat::Secs, true);
                            EncryptedLinkRecoveryMarker::new(attempt_id.clone(), created_at_text)
                                .map(|marker| (marker, created_at))
                        })
                        .transpose()?
                } else {
                    None
                };
                let (marker, marker_created_at) = reusable_marker
                    .map(Ok)
                    .unwrap_or_else(|| {
                        EncryptedLinkRecoveryMarker::new_v4(
                            now.to_rfc3339_opts(SecondsFormat::Secs, true),
                        )
                        .map(|marker| (marker, now))
                    })?;
                peer.local_recovery_attempt_id = Some(marker.attempt_id().to_owned());
                peer.local_recovery_marker_created_at = Some(marker_created_at);
                tx.save_linked_peer(peer);
                Ok(marker)
            })
            .await?;
        paykit_lib::publish_encrypted_link_recovery_marker(
            &session_access.session,
            secret_key,
            &remote_public_key,
            &marker,
        )
        .await?;

        self.recovery_marker_report_or_default(counterparty, false)
            .await
    }

    async fn observe_remote_recovery_marker_for_cached_private_state(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: Option<&PubkySessionAccess>,
    ) -> Result<()> {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            return Ok(());
        }

        let session_access = match session_access {
            Some(session_access) => session_access,
            None => {
                let loaded = self.pubky.load_session_access().await?;
                let Some(session_access) = loaded else {
                    return Err(PaykitSdkError::Identity {
                        context:
                            "no Pubky session available for Encrypted Link recovery marker lookup"
                                .into(),
                        source: None,
                    });
                };
                return self
                    .observe_remote_recovery_marker_with_session(counterparty, &session_access)
                    .await
                    .map(|_| ());
            }
        };

        self.observe_remote_recovery_marker_with_session(counterparty, session_access)
            .await
            .map(|_| ())
    }

    async fn observe_remote_recovery_marker_with_session(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            return self
                .recovery_marker_report_or_default(counterparty, false)
                .await;
        }

        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for recovery marker lookup".into(),
                    source: None,
                })?;
        let secret_key = session_access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Identity {
                context:
                    "local Pubky secret key is unavailable for Encrypted Link recovery markers"
                        .into(),
                source: None,
            })?
            .as_bytes();
        let remote_public_key = counterparty.to_public_key()?;
        let Some(marker) = paykit_lib::fetch_encrypted_link_recovery_marker(
            &public_storage,
            secret_key,
            &remote_public_key,
        )
        .await?
        else {
            return self
                .recovery_marker_report_or_default(counterparty, false)
                .await;
        };

        let now = self.clock.now();
        let changed = self
            .storage
            .transaction(|tx| {
                let existing_peer = tx.linked_peer(counterparty);
                let has_link_state = tx.encrypted_link_state(counterparty).is_some();
                if !can_publish_recovery_marker(existing_peer.as_ref(), has_link_state) {
                    return Ok(false);
                }
                let mut peer = recovery_peer_or_default(existing_peer, counterparty);
                if peer.remote_recovery_attempt_id.as_deref() == Some(marker.attempt_id()) {
                    tx.save_linked_peer(peer);
                    return Ok(false);
                }
                if peer.state == LinkedPeerState::Blocked {
                    return Err(PaykitSdkError::Policy(format!(
                        "counterparty {counterparty} is blocked"
                    )));
                }
                peer.remote_recovery_attempt_id = Some(marker.attempt_id().to_owned());
                peer.remote_recovery_marker_observed_at = Some(now);
                peer.state = LinkedPeerState::RecoveryRequired;
                peer.last_sync_at = Some(now);
                peer.failure_count = peer.failure_count.saturating_add(1);
                tx.save_linked_peer(peer);
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
                Ok(true)
            })
            .await?;
        self.recovery_marker_report_or_default(counterparty, changed)
            .await
    }

    async fn recovery_marker_report_or_default(
        &self,
        counterparty: &PubkyPublicKey,
        remote_marker_changed: bool,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let peer = self
            .storage
            .transaction(|tx| {
                Ok(recovery_peer_or_default(
                    tx.linked_peer(counterparty),
                    counterparty,
                ))
            })
            .await?;
        Ok(EncryptedLinkRecoveryMarkerReport::from_peer(
            &peer,
            remote_marker_changed,
        ))
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
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
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
            let mark = mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                now,
            )
            .await?;
            let _ = self
                .publish_local_recovery_marker_with_session(
                    &counterparty,
                    &session_access,
                    mark.new_episode,
                )
                .await;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
        };
        let snapshot = match paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let now = self.clock.now();
                let mark = mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    now,
                )
                .await?;
                let _ = self
                    .publish_local_recovery_marker_with_session(
                        &counterparty,
                        &session_access,
                        mark.new_episode,
                    )
                    .await;
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
        let report = OutboundPrivateSendReport::default();
        let queued = queued_outbound_private_messages(&self.storage, &counterparty).await?;
        if queued.is_empty() {
            return Ok(report);
        }
        if self.config.private_sharing == PrivateSharingPolicy::Disabled {
            return Err(PaykitSdkError::Policy(
                "private Paykit message sending is disabled".into(),
            ));
        }
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
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
            let mark = mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                now,
            )
            .await?;
            let _ = self
                .publish_local_recovery_marker_with_session(
                    &counterparty,
                    &session_access,
                    mark.new_episode,
                )
                .await;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
        };
        let snapshot = match paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let now = self.clock.now();
                let mark = mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    now,
                )
                .await?;
                let _ = self
                    .publish_local_recovery_marker_with_session(
                        &counterparty,
                        &session_access,
                        mark.new_episode,
                    )
                    .await;
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
        let peer_state = self
            .storage
            .transaction(|tx| Ok(tx.linked_peer(&counterparty).map(|peer| peer.state)))
            .await?;
        if matches!(peer_state, Some(LinkedPeerState::Blocked)) {
            return Err(PaykitSdkError::Policy(format!(
                "counterparty {counterparty} is blocked"
            )));
        }

        if !matches!(peer_state, Some(LinkedPeerState::RecoveryRequired)) {
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
                        let mark = mark_recovery_required_with_lease(
                            &self.storage,
                            counterparty.clone(),
                            lease.clone(),
                            self.clock.now(),
                        )
                        .await?;
                        self.publish_local_recovery_marker_if_possible(
                            &counterparty,
                            mark.new_episode,
                        )
                        .await;
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
                let report = save_linked_peer_link_state_if_generation_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    link.serialize(),
                    expected_generation,
                    lease,
                    self.clock.now(),
                )
                .await?;
                self.remove_local_recovery_marker_if_recorded(&counterparty)
                    .await;
                Ok(report)
            }
        }
    }

    async fn remove_local_recovery_marker_if_recorded(&self, counterparty: &PubkyPublicKey) {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            return;
        }
        let has_local_marker = self
            .storage
            .transaction(|tx| {
                Ok(tx
                    .linked_peer(counterparty)
                    .and_then(|peer| peer.local_recovery_attempt_id)
                    .is_some())
            })
            .await
            .unwrap_or(false);
        if !has_local_marker {
            return;
        }

        let Ok(Some(session_access)) = self.pubky.load_session_access().await else {
            return;
        };
        let Some(secret_key) = session_access
            .local_secret_key
            .as_ref()
            .map(|key| key.as_bytes())
        else {
            return;
        };
        let Ok(remote_public_key) = counterparty.to_public_key() else {
            return;
        };
        if paykit_lib::remove_encrypted_link_recovery_marker(
            &session_access.session,
            secret_key,
            &remote_public_key,
        )
        .await
        .is_err()
        {
            return;
        }
        let _ = self
            .storage
            .transaction(|tx| {
                if let Some(mut peer) = tx.linked_peer(counterparty) {
                    peer.local_recovery_attempt_id = None;
                    peer.local_recovery_marker_created_at = None;
                    tx.save_linked_peer(peer);
                }
                Ok(())
            })
            .await;
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

fn recovery_peer_or_default(
    peer: Option<LinkedPeerRecord>,
    counterparty: &PubkyPublicKey,
) -> LinkedPeerRecord {
    peer.unwrap_or_else(|| LinkedPeerRecord {
        counterparty: counterparty.clone(),
        state: LinkedPeerState::Unknown,
        last_sync_at: None,
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
    })
}

fn can_publish_recovery_marker(peer: Option<&LinkedPeerRecord>, has_link_state: bool) -> bool {
    has_link_state
        || peer.is_some_and(|peer| {
            matches!(
                peer.state,
                LinkedPeerState::Linking
                    | LinkedPeerState::Linked
                    | LinkedPeerState::RecoveryRequired
            )
        })
}

fn local_recovery_marker_belongs_to_current_episode(peer: &LinkedPeerRecord) -> bool {
    let Some(created_at) = peer.local_recovery_marker_created_at else {
        return false;
    };
    peer.last_sync_at
        .map(|recovery_started_at| created_at >= recovery_started_at)
        .unwrap_or(true)
}

enum PrivateRecoveryOutcome {
    NotNeeded,
    Pending,
    PublicOnly,
    Refreshed(Vec<PaymentEndpointCandidate>),
}

enum RecoveryRequiredUpdate {
    Skipped,
    Marked { new_episode: bool },
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

fn require_payer_role(record: &PaymentRequestRecord, action: &str) -> Result<()> {
    if record.local_role == Some(PaymentRequestLocalRole::Payer) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy(format!(
            "cannot {action}: local identity is not the payer"
        )))
    }
}

fn require_state(
    record: &PaymentRequestRecord,
    allowed: &[PaymentRequestLifecycleState],
    action: &str,
) -> Result<()> {
    if allowed.contains(&record.state) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy(format!(
            "cannot {action}: Payment Request {} is in state {:?}",
            record.payment_request_id, record.state
        )))
    }
}

fn reservation_release(
    counterparty: &PubkyPublicKey,
    reservation: &PaymentEndpointReservation,
) -> PaymentEndpointReservationRelease {
    PaymentEndpointReservationRelease {
        reservation_id: reservation.reservation_id.clone(),
        counterparty: counterparty.clone(),
        identifier: reservation.receiving_detail.identifier.clone(),
        payload_hash: reservation_payload_hash(&reservation.receiving_detail.payload),
        attribution: reservation.attribution.clone(),
    }
}

async fn fetch_public_text(
    storage: &pubky::PublicStorage,
    public_key: &PubkyPublicKey,
    path: &str,
    context: &'static str,
) -> Result<Option<String>> {
    let addr = format!("{public_key}{path}");
    match storage.get(addr).await {
        Ok(resp) => {
            let bytes = resp
                .bytes()
                .await
                .map_err(|err| PaykitSdkError::Transport {
                    context: context.into(),
                    source: Some(err.into()),
                })?;
            if bytes.is_empty() {
                return Ok(None);
            }
            String::from_utf8(bytes.to_vec())
                .map(Some)
                .map_err(|err| PaykitSdkError::Protocol(format!("{context}: invalid UTF-8: {err}")))
        }
        Err(err) if is_pubky_not_found(&err) => Ok(None),
        Err(err) => Err(map_pubky_transport_error(context, err)),
    }
}

fn map_pubky_transport_error(context: &'static str, err: PubkyError) -> PaykitSdkError {
    PaykitSdkError::Transport {
        context: context.into(),
        source: Some(err.into()),
    }
}

fn is_pubky_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::{
        adapters::{
            EndpointCompatibility, PaymentEndpointCandidate, PaymentEndpointEvaluation,
            PaymentEndpointReservation, PaymentEndpointReservationRelease,
            PaymentEndpointReservationRequest, PaymentEndpointSelection,
            PaymentEndpointSelectionRequest, PaymentTarget, ReceivingDetail, ReceivingDetailScope,
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

    struct ReservedPrivateListPaymentAdapter;

    #[async_trait]
    impl PaymentAdapter for ReservedPrivateListPaymentAdapter {
        async fn current_receiving_details(
            &self,
            _scope: ReceivingDetailScope,
        ) -> Result<Vec<ReceivingDetail>> {
            panic!("reservation-capable adapter should not use fallback details");
        }

        async fn reserve_receiving_details(
            &self,
            request: &PaymentEndpointReservationRequest,
        ) -> Result<Option<Vec<PaymentEndpointReservation>>> {
            assert!(!request.counterparty.as_str().is_empty());
            Ok(Some(vec![PaymentEndpointReservation {
                reservation_id: "reservation-1".into(),
                receiving_detail: ReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "ln-reserved".into(),
                },
                expires_at: None,
                attribution: HashMap::from([("contact".into(), "alice".into())]),
            }]))
        }

        async fn select_payment_endpoint(
            &self,
            request: &PaymentEndpointSelectionRequest,
        ) -> Result<PaymentEndpointSelection> {
            Ok(PaymentEndpointSelection {
                selected: request.candidates.first().cloned(),
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

    struct InvalidReservedPrivateListPaymentAdapter {
        released: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl PaymentAdapter for InvalidReservedPrivateListPaymentAdapter {
        async fn current_receiving_details(
            &self,
            _scope: ReceivingDetailScope,
        ) -> Result<Vec<ReceivingDetail>> {
            panic!("reservation-capable adapter should not use fallback details");
        }

        async fn reserve_receiving_details(
            &self,
            _request: &PaymentEndpointReservationRequest,
        ) -> Result<Option<Vec<PaymentEndpointReservation>>> {
            Ok(Some(vec![
                PaymentEndpointReservation {
                    reservation_id: "reservation-1".into(),
                    receiving_detail: ReceivingDetail {
                        identifier: "btc-lightning-bolt11".into(),
                        payload: "one".into(),
                    },
                    expires_at: None,
                    attribution: HashMap::new(),
                },
                PaymentEndpointReservation {
                    reservation_id: "reservation-2".into(),
                    receiving_detail: ReceivingDetail {
                        identifier: "btc-lightning-bolt11".into(),
                        payload: "two".into(),
                    },
                    expires_at: None,
                    attribution: HashMap::new(),
                },
            ]))
        }

        async fn release_receiving_detail_reservation(
            &self,
            release: &PaymentEndpointReservationRelease,
        ) -> Result<()> {
            self.released
                .lock()
                .unwrap()
                .push(release.reservation_id.clone());
            Ok(())
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

    struct MixedExistingReservedPrivateListPaymentAdapter {
        released: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl PaymentAdapter for MixedExistingReservedPrivateListPaymentAdapter {
        async fn current_receiving_details(
            &self,
            _scope: ReceivingDetailScope,
        ) -> Result<Vec<ReceivingDetail>> {
            panic!("reservation-capable adapter should not use fallback details");
        }

        async fn reserve_receiving_details(
            &self,
            _request: &PaymentEndpointReservationRequest,
        ) -> Result<Option<Vec<PaymentEndpointReservation>>> {
            Ok(Some(vec![
                PaymentEndpointReservation {
                    reservation_id: "existing-reservation".into(),
                    receiving_detail: ReceivingDetail {
                        identifier: "btc-lightning-bolt11".into(),
                        payload: "existing".into(),
                    },
                    expires_at: None,
                    attribution: HashMap::new(),
                },
                PaymentEndpointReservation {
                    reservation_id: "conflicting-reservation".into(),
                    receiving_detail: ReceivingDetail {
                        identifier: "btc-lightning-bolt11".into(),
                        payload: "conflict".into(),
                    },
                    expires_at: None,
                    attribution: HashMap::new(),
                },
            ]))
        }

        async fn release_receiving_detail_reservation(
            &self,
            release: &PaymentEndpointReservationRelease,
        ) -> Result<()> {
            self.released
                .lock()
                .unwrap()
                .push(release.reservation_id.clone());
            Ok(())
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
                        local_recovery_attempt_id: None,
                        local_recovery_marker_created_at: None,
                        remote_recovery_attempt_id: None,
                        remote_recovery_marker_observed_at: None,
                    });
                    tx.save_public_endpoint_record(crate::PublicEndpointRecord {
                        identifier: "btc-lightning-bolt11".into(),
                        payload: Some("payload".into()),
                        status: EndpointPublicationStatus::Published,
                        updated_at: FixedClock.now(),
                        last_error: None,
                    });
                    tx.save_contact_record(ContactRecord {
                        public_key: counterparty.clone(),
                        label: Some("Alice".into()),
                        profile: None,
                        profile_fetched_at: None,
                        created_at: FixedClock.now(),
                        updated_at: FixedClock.now(),
                        public_contact_marker_status:
                            crate::PublicContactMarkerStatus::NotPublished,
                        public_contact_published_at: None,
                        public_contact_removed_at: None,
                        public_contact_last_error: None,
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
        assert!(snapshot.contact_records.is_empty());
        assert!(snapshot.public_endpoint_records.is_empty());
        assert!(snapshot.outbound_private_messages.is_empty());
    }

    #[tokio::test]
    async fn test_contact_records_save_list_and_remove_locally() {
        let storage = InMemoryStorage::new();
        let local_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let contact_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .save_identity_state(IdentityState {
                public_key: Some(local_public_key),
                capability: PubkyIdentityCapability::PublicOnly,
                local_secret_available: false,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
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

        let saved = sdk
            .save_contact(ContactUpdate {
                public_key: contact_public_key.clone(),
                label: Some("Alice".into()),
            })
            .await
            .unwrap();

        assert_eq!(saved.label.as_deref(), Some("Alice"));
        assert_eq!(sdk.contact_records().await.unwrap(), vec![saved.clone()]);
        assert_eq!(
            sdk.contact_record(&contact_public_key).await.unwrap(),
            Some(saved.clone())
        );
        assert_eq!(
            sdk.remove_contact(&contact_public_key).await.unwrap(),
            Some(saved)
        );
        assert!(sdk
            .contact_record(&contact_public_key)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_save_contact_empty_label_clears_existing_label() {
        let storage = InMemoryStorage::new();
        let local_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let contact_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .save_identity_state(IdentityState {
                public_key: Some(local_public_key),
                capability: PubkyIdentityCapability::PublicOnly,
                local_secret_available: false,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
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

        sdk.save_contact(ContactUpdate {
            public_key: contact_public_key.clone(),
            label: Some("Alice".into()),
        })
        .await
        .unwrap();
        let updated = sdk
            .save_contact(ContactUpdate {
                public_key: contact_public_key,
                label: Some(String::new()),
            })
            .await
            .unwrap();

        assert!(updated.label.is_none());
    }

    #[tokio::test]
    async fn test_remove_contact_blocks_when_public_marker_may_exist() {
        let storage = InMemoryStorage::new();
        let local_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let contact_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .save_identity_state(IdentityState {
                public_key: Some(local_public_key),
                capability: PubkyIdentityCapability::PublicOnly,
                local_secret_available: false,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            })
            .await
            .unwrap();
        storage
            .transaction({
                let contact_public_key = contact_public_key.clone();
                move |tx| {
                    tx.save_contact_record(ContactRecord {
                        public_key: contact_public_key,
                        label: None,
                        profile: None,
                        profile_fetched_at: None,
                        created_at: FixedClock.now(),
                        updated_at: FixedClock.now(),
                        public_contact_marker_status: crate::PublicContactMarkerStatus::Published,
                        public_contact_published_at: Some(FixedClock.now()),
                        public_contact_removed_at: None,
                        public_contact_last_error: None,
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

        let result = sdk.remove_contact(&contact_public_key).await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    }

    #[tokio::test]
    async fn test_publish_public_contact_does_not_mark_pending_without_session() {
        let storage = InMemoryStorage::new();
        let local_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let contact_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .save_identity_state(IdentityState {
                public_key: Some(local_public_key),
                capability: PubkyIdentityCapability::PublicOnly,
                local_secret_available: false,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            })
            .await
            .unwrap();
        storage
            .transaction({
                let contact_public_key = contact_public_key.clone();
                move |tx| {
                    tx.save_contact_record(ContactRecord {
                        public_key: contact_public_key,
                        label: None,
                        profile: None,
                        profile_fetched_at: None,
                        created_at: FixedClock.now(),
                        updated_at: FixedClock.now(),
                        public_contact_marker_status:
                            crate::PublicContactMarkerStatus::NotPublished,
                        public_contact_published_at: None,
                        public_contact_removed_at: None,
                        public_contact_last_error: None,
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
            PaykitSdkConfig {
                public_contact_sharing: PublicContactSharingPolicy::PublicPaykitNamespace,
                ..PaykitSdkConfig::default()
            },
            FixedClock,
        );

        let result = sdk.publish_public_contact(contact_public_key.clone()).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        let record = storage
            .snapshot()
            .unwrap()
            .contact_records
            .get(&contact_public_key)
            .unwrap()
            .clone();
        assert_eq!(
            record.public_contact_marker_status,
            crate::PublicContactMarkerStatus::NotPublished
        );
    }

    #[tokio::test]
    async fn test_remove_public_contact_cleanup_is_allowed_when_sharing_disabled() {
        let storage = InMemoryStorage::new();
        let local_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let contact_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .save_identity_state(IdentityState {
                public_key: Some(local_public_key),
                capability: PubkyIdentityCapability::PublicOnly,
                local_secret_available: false,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            })
            .await
            .unwrap();
        storage
            .transaction({
                let contact_public_key = contact_public_key.clone();
                move |tx| {
                    tx.save_contact_record(ContactRecord {
                        public_key: contact_public_key,
                        label: None,
                        profile: None,
                        profile_fetched_at: None,
                        created_at: FixedClock.now(),
                        updated_at: FixedClock.now(),
                        public_contact_marker_status: crate::PublicContactMarkerStatus::Published,
                        public_contact_published_at: Some(FixedClock.now()),
                        public_contact_removed_at: None,
                        public_contact_last_error: None,
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

        let result = sdk.remove_public_contact(contact_public_key.clone()).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        let record = storage
            .snapshot()
            .unwrap()
            .contact_records
            .get(&contact_public_key)
            .unwrap()
            .clone();
        assert_eq!(
            record.public_contact_marker_status,
            crate::PublicContactMarkerStatus::Published
        );
    }

    #[tokio::test]
    async fn test_remove_public_contact_without_local_record_still_requires_session() {
        let storage = InMemoryStorage::new();
        let local_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let contact_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .save_identity_state(IdentityState {
                public_key: Some(local_public_key),
                capability: PubkyIdentityCapability::PublicOnly,
                local_secret_available: false,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
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

        let result = sdk.remove_public_contact(contact_public_key).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }

    #[tokio::test]
    async fn test_save_contact_requires_initialized_identity() {
        let storage = InMemoryStorage::new();
        let sdk = PaykitSdk::with_clock(
            storage,
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );
        let contact_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());

        let result = sdk
            .save_contact(ContactUpdate {
                public_key: contact_public_key,
                label: None,
            })
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
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
    async fn test_enqueue_private_payment_list_uses_fallback_details() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            PrivateListPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let outbound = sdk
            .enqueue_private_payment_list_from_receiving_details(counterparty)
            .await
            .unwrap();

        let list = paykit_lib::parse_private_payment_list_json(&outbound.raw_json).unwrap();
        assert_eq!(
            list.get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap())
                .unwrap()
                .as_str(),
            "ln-private"
        );
        assert!(storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .is_empty());
    }

    #[tokio::test]
    async fn test_enqueue_private_payment_list_uses_reserved_details() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            ReservedPrivateListPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let outbound = sdk
            .enqueue_private_payment_list_from_receiving_details(counterparty.clone())
            .await
            .unwrap();

        let list = paykit_lib::parse_private_payment_list_json(&outbound.raw_json).unwrap();
        assert_eq!(
            list.get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap())
                .unwrap()
                .as_str(),
            "ln-reserved"
        );
        let reservations = storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .into_values()
            .collect::<Vec<_>>();
        assert_eq!(reservations.len(), 1);
        assert_eq!(
            reservations[0].outbound_message_id,
            outbound.outbound_message_id
        );
        assert_ne!(reservations[0].payload_hash, "ln-reserved");
        assert!(!format!("{:?}", reservations[0]).contains("ln-reserved"));
    }

    #[tokio::test]
    async fn test_enqueue_private_payment_list_releases_invalid_reservations() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let released = Arc::new(Mutex::new(Vec::new()));
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            InvalidReservedPrivateListPaymentAdapter {
                released: released.clone(),
            },
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk
            .enqueue_private_payment_list_from_receiving_details(counterparty)
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
        assert_eq!(
            *released.lock().unwrap(),
            vec!["reservation-1".to_string(), "reservation-2".to_string()]
        );
        let snapshot = storage.snapshot().unwrap();
        assert!(snapshot.payment_endpoint_reservations.is_empty());
        assert!(snapshot.outbound_private_messages.is_empty());
    }

    #[tokio::test]
    async fn test_enqueue_private_payment_list_keeps_existing_reservation_on_error() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        queue_private_payment_list_with_reservations(
            &storage,
            &PaymentEndpointReservationRequest {
                counterparty: counterparty.clone(),
            },
            vec![PaymentEndpointReservation {
                reservation_id: "existing-reservation".into(),
                receiving_detail: ReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "existing".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            }],
            FixedClock.now(),
        )
        .await
        .unwrap();
        let released = Arc::new(Mutex::new(Vec::new()));
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            MixedExistingReservedPrivateListPaymentAdapter {
                released: released.clone(),
            },
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk
            .enqueue_private_payment_list_from_receiving_details(counterparty)
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
        assert_eq!(
            *released.lock().unwrap(),
            vec!["conflicting-reservation".to_string()]
        );
        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .payment_endpoint_reservations
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_enqueue_payment_request_event_requires_private_capable_identity() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let sdk = PaykitSdk::with_clock(
            storage,
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );
        let event = PaymentRequestAcceptance::new(
            paykit_lib::EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102").unwrap(),
            paykit_lib::PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap(),
        );

        let result = sdk
            .enqueue_raw_payment_request_acceptance(counterparty, &event)
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }

    #[tokio::test]
    async fn test_enqueue_payment_request_event_respects_private_sharing_policy() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig {
                private_sharing: PrivateSharingPolicy::Disabled,
                ..PaykitSdkConfig::default()
            },
            FixedClock,
        );
        let event = PaymentRequestAcceptance::new(
            paykit_lib::EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102").unwrap(),
            paykit_lib::PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap(),
        );

        let result = sdk
            .enqueue_raw_payment_request_acceptance(counterparty, &event)
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
        assert!(storage
            .snapshot()
            .unwrap()
            .outbound_private_messages
            .is_empty());
    }

    #[tokio::test]
    async fn test_process_outbound_private_messages_respects_private_sharing_policy() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let event = PaymentRequestAcceptance::new(
            paykit_lib::EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102").unwrap(),
            paykit_lib::PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap(),
        );
        let event = PaymentRequestEvent::Acceptance(event);
        crate::payment_requests::enqueue_payment_request_event(
            &storage,
            counterparty.clone(),
            &event,
            FixedClock.now(),
        )
        .await
        .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig {
                private_sharing: PrivateSharingPolicy::Disabled,
                ..PaykitSdkConfig::default()
            },
            FixedClock,
        );

        let result = sdk.process_outbound_private_messages(counterparty).await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .outbound_private_messages
                .first()
                .unwrap()
                .status,
            crate::OutboundPrivateMessageStatus::Pending
        );
    }

    #[tokio::test]
    async fn test_accept_payment_request_rejects_expired_proposal_before_enqueue() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let request_id = PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            vec![payment_request_message(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id.as_str(),
                Some("2026-06-03T11:59:59Z"),
            )],
            None,
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

        let result = sdk.accept_payment_request(counterparty, &request_id).await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
        assert!(storage
            .snapshot()
            .unwrap()
            .outbound_private_messages
            .is_empty());
    }

    #[tokio::test]
    async fn test_reject_payment_request_rejects_expired_proposal_before_enqueue() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let request_id = PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            vec![payment_request_message(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id.as_str(),
                Some("2026-06-03T11:59:59Z"),
            )],
            None,
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
            .reject_payment_request(counterparty, &request_id, None)
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
        assert!(storage
            .snapshot()
            .unwrap()
            .outbound_private_messages
            .is_empty());
    }

    #[tokio::test]
    async fn test_accept_payment_request_does_not_queue_without_private_send_readiness() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let request_id = PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            vec![payment_request_message(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id.as_str(),
                None,
            )],
            None,
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

        let result = sdk.accept_payment_request(counterparty, &request_id).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
        assert!(storage
            .snapshot()
            .unwrap()
            .outbound_private_messages
            .is_empty());
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
    async fn test_recovery_required_peer_allows_relink_attempt() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        crate::linked_peers::save_linked_peer_state(
            &storage,
            counterparty.clone(),
            LinkedPeerState::RecoveryRequired,
            FixedClock.now(),
        )
        .await
        .unwrap();
        let lease = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx
                        .claim_peer_link_operation(
                            &counterparty,
                            FixedClock.now(),
                            FixedClock.now() + chrono::Duration::seconds(60),
                        )
                        .unwrap())
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

        let result = sdk
            .start_link_handshake_with_claim(
                counterparty,
                EncryptedLinkHandshakeRole::Initiator,
                lease,
            )
            .await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }

    #[tokio::test]
    async fn test_advance_link_handshake_clears_unusable_link_state() {
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
    async fn test_advance_link_handshake_clears_unusable_handshake_snapshot() {
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
    async fn test_advance_link_handshake_clears_unusable_handshake_metadata() {
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
        let queued =
            crate::outbound_private::queued_outbound_private_messages(&storage, &counterparty)
                .await
                .unwrap();
        assert!(queued.is_empty());
    }

    #[tokio::test]
    async fn test_process_outbound_private_messages_blocks_recovery_required_peer() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        crate::linked_peers::save_linked_peer_state(
            &storage,
            counterparty.clone(),
            LinkedPeerState::RecoveryRequired,
            FixedClock.now(),
        )
        .await
        .unwrap();
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

        assert!(matches!(result, Err(PaykitSdkError::RecoveryRequired(_))));
        let queued =
            crate::outbound_private::queued_outbound_private_messages(&storage, &counterparty)
                .await
                .unwrap();
        assert_eq!(queued.len(), 1);
    }

    #[tokio::test]
    async fn test_restore_backup_state_requires_active_identity() {
        let storage = InMemoryStorage::new();
        let backup_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let backup = SdkBackupState {
            version: crate::SDK_BACKUP_VERSION,
            identity_state: Some(IdentityState {
                public_key: Some(backup_public_key),
                capability: PubkyIdentityCapability::PrivateLinkCapable,
                local_secret_available: true,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            }),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };
        let sdk = PaykitSdk::with_clock(
            storage,
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::default(),
            FixedClock,
        );

        let result = sdk.restore_backup_state(backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
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
                        local_recovery_attempt_id: None,
                        local_recovery_marker_created_at: None,
                        remote_recovery_attempt_id: None,
                        remote_recovery_marker_observed_at: None,
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
                        local_recovery_attempt_id: None,
                        local_recovery_marker_created_at: None,
                        remote_recovery_attempt_id: None,
                        remote_recovery_marker_observed_at: None,
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

        let recovery_update = sdk
            .mark_private_recovery_pending(&counterparty, Some(1))
            .await
            .unwrap();
        assert!(matches!(recovery_update, RecoveryRequiredUpdate::Skipped));

        let peer = crate::load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::Linked);

        let recovery_update = sdk
            .mark_private_recovery_pending(&counterparty, Some(2))
            .await
            .unwrap();
        assert!(matches!(
            recovery_update,
            RecoveryRequiredUpdate::Marked { new_episode: true }
        ));

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

    #[tokio::test]
    async fn test_encrypted_link_recovery_marker_status_reports_peer_fields() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_linked_peer(crate::LinkedPeerRecord {
                        counterparty,
                        state: LinkedPeerState::RecoveryRequired,
                        last_sync_at: Some(FixedClock.now()),
                        last_private_receive_at: None,
                        failure_count: 1,
                        local_recovery_attempt_id: Some(
                            "650e8400-e29b-41d4-a716-446655440000".into(),
                        ),
                        local_recovery_marker_created_at: Some(FixedClock.now()),
                        remote_recovery_attempt_id: Some(
                            "550e8400-e29b-41d4-a716-446655440000".into(),
                        ),
                        remote_recovery_marker_observed_at: Some(FixedClock.now()),
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

        let status = sdk
            .encrypted_link_recovery_marker_status(&counterparty)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(status.state, LinkedPeerState::RecoveryRequired);
        assert_eq!(
            status.local_attempt_id.as_deref(),
            Some("650e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            status.remote_attempt_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert!(!status.remote_marker_changed);
    }

    #[tokio::test]
    async fn test_mark_private_recovery_pending_preserves_marker_until_publish() {
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
                        local_recovery_attempt_id: Some(
                            "650e8400-e29b-41d4-a716-446655440000".into(),
                        ),
                        local_recovery_marker_created_at: Some(FixedClock.now()),
                        remote_recovery_attempt_id: Some(
                            "550e8400-e29b-41d4-a716-446655440000".into(),
                        ),
                        remote_recovery_marker_observed_at: Some(FixedClock.now()),
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

        let recovery_update = sdk
            .mark_private_recovery_pending(&counterparty, Some(2))
            .await
            .unwrap();
        assert!(matches!(
            recovery_update,
            RecoveryRequiredUpdate::Marked { new_episode: true }
        ));

        let peer = crate::load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        assert_eq!(
            peer.local_recovery_attempt_id.as_deref(),
            Some("650e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            peer.remote_recovery_attempt_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn test_local_recovery_marker_must_belong_to_current_episode() {
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let recovery_started_at = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        let previous_episode_marker_at = Utc.with_ymd_and_hms(2026, 6, 3, 11, 59, 0).unwrap();
        let current_episode_marker_at = Utc.with_ymd_and_hms(2026, 6, 3, 12, 1, 0).unwrap();
        let mut peer = crate::LinkedPeerRecord {
            counterparty,
            state: LinkedPeerState::RecoveryRequired,
            last_sync_at: Some(recovery_started_at),
            last_private_receive_at: None,
            failure_count: 1,
            local_recovery_attempt_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
            local_recovery_marker_created_at: Some(previous_episode_marker_at),
            remote_recovery_attempt_id: None,
            remote_recovery_marker_observed_at: None,
        };

        assert!(!local_recovery_marker_belongs_to_current_episode(&peer));

        peer.local_recovery_marker_created_at = Some(current_episode_marker_at);

        assert!(local_recovery_marker_belongs_to_current_episode(&peer));
    }

    #[tokio::test]
    async fn test_mark_private_recovery_pending_preserves_ongoing_local_marker() {
        let storage = InMemoryStorage::new();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_linked_peer(crate::LinkedPeerRecord {
                        counterparty: counterparty.clone(),
                        state: LinkedPeerState::RecoveryRequired,
                        last_sync_at: Some(FixedClock.now()),
                        last_private_receive_at: None,
                        failure_count: 1,
                        local_recovery_attempt_id: Some(
                            "650e8400-e29b-41d4-a716-446655440000".into(),
                        ),
                        local_recovery_marker_created_at: Some(FixedClock.now()),
                        remote_recovery_attempt_id: None,
                        remote_recovery_marker_observed_at: None,
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

        let recovery_update = sdk
            .mark_private_recovery_pending(&counterparty, Some(2))
            .await
            .unwrap();
        assert!(matches!(
            recovery_update,
            RecoveryRequiredUpdate::Marked { new_episode: false }
        ));

        let peer = crate::load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            peer.local_recovery_attempt_id.as_deref(),
            Some("650e8400-e29b-41d4-a716-446655440000")
        );
    }
}
