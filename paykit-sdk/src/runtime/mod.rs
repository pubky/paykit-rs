use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use paykit_lib::{
    BillingPeriod, EncryptedLinkRecoveryMarker, EventId, PaykitReceiverCapabilities,
    PaykitReceiverMarker, PaymentEndpointIdentifier, PaymentProof, PaymentRequest,
    PaymentRequestAcceptance, PaymentRequestCancellation, PaymentRequestId,
    PaymentRequestRejection, PaymentRequestTerms, PrivateMessageKind, ReceiptDraft,
};
use pubky::{errors::RequestError, Error as PubkyError, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

#[cfg(test)]
use paykit_lib::PaymentRequestEvent;

use crate::{
    backup::{
        export_backup_state as export_sdk_backup_state,
        restore_backup_state_with_identity as restore_sdk_backup_state, RestoreReport,
        SdkBackupState,
    },
    config::{
        EncryptedLinkRecoveryMarkerPolicy, EndpointManagementScope, PaykitSdkConfig,
        PublicContactSharingPolicy,
    },
    domain::contacts::{
        parse_profile_json, parse_pubky_profile_json, paykit_blob_path,
        paykit_blob_path_from_uri_or_path, paykit_blob_uri, profile_json,
        pubky_follow_keys_from_follow_entries, public_contact_json, ContactProfileResolution,
        ContactRecord, ContactUpdate, PaykitBlobRecord, PaykitProfile, PaykitProfileRecord,
        PubkyProfileRecord, PUBKY_FOLLOWS_PATH_PREFIX, PUBKY_PROFILE_PATH,
    },
    domain::endpoint_reservations::{
        expired_outbound_reservation_cancellations, invalid_private_list_reservation_cancellations,
        queue_private_payment_list_with_reservations_with_link_lease, reservation_payload_hash,
        unattempted_superseded_reservation_cancellations,
        PaymentEndpointReservationCancellationRecord,
    },
    domain::endpoints::{
        failed_record, normalize_receiving_details, pending_publication_record,
        pending_removal_record, published_record, removed_record, EndpointSyncChange,
        EndpointSyncReport,
    },
    domain::linked_peers::{
        default_linked_peer, mark_recovery_required_for_marker_in_transaction,
        mark_recovery_required_in_transaction, mark_recovery_required_with_lease,
        save_link_handshake_state_if_generation_with_lease, save_link_handshake_state_with_lease,
        save_linked_peer_link_state_if_generation_with_lease, save_linked_peer_state_with_lease,
        EncryptedLinkHandshakeRole, LinkedPeerHandshakeReport, LinkedPeerState,
    },
    domain::outbound_private::{
        claim_next_outbound_private_message_with_peer_lease, mark_outbound_failed,
        mark_outbound_invalid, mark_outbound_recovery_required, mark_outbound_sent,
        queued_outbound_private_messages, validate_queued_outbound_private_message,
        OutboundPrivateCounterpartySendReport, OutboundPrivateMessageStatus,
        OutboundPrivateSendFailure, OutboundPrivateSendReport, RecoveryMarkerPublishFailure,
        ReservationCleanupFailure,
    },
    domain::payment_requests::{
        enqueue_payment_proof as enqueue_payment_proof_message,
        enqueue_payment_request as enqueue_payment_request_message,
        enqueue_payment_request_acceptance as enqueue_payment_request_acceptance_message,
        enqueue_payment_request_cancellation as enqueue_payment_request_cancellation_message,
        enqueue_payment_request_rejection as enqueue_payment_request_rejection_message,
        payment_request_records as derive_payment_request_records,
        received_payment_request_records as derive_received_payment_request_records,
        request_from_record, PaymentRequestFilter, PaymentRequestLifecycleState,
        PaymentRequestLocalRole, PaymentRequestRecord,
    },
    domain::payment_resolution::{
        PreparedPrivateContactPayment, PrivateContactPaymentResolution,
        PrivatePaymentResolutionState, PrivatePaymentResolutionStatus,
        PublicContactPaymentResolution, PublicPaymentResolutionStatus,
        ResolvedPrivatePaymentEndpoint, ResolvedPublicPaymentEndpoint,
    },
    domain::private_lists::{
        current_private_payment_list as load_current_private_payment_list,
        enqueue_private_payment_list_with_link_lease as enqueue_private_payment_list_message_with_link_lease,
        PrivatePaymentListDeliveryFailure, PrivatePaymentListDeliveryReport,
        PrivatePaymentListReservationUpdate, PrivatePaymentListSyncChange,
        PrivatePaymentListSyncReport,
    },
    domain::private_stream::{
        normalize::normalize_private_stream_classifications,
        persist_private_stream_batch_with_link_lease, PrivateStreamCounterpartyIntakeReport,
        PrivateStreamIntakeReport,
    },
    domain::publication::PublicationStatus,
    domain::receipts::{
        decrypt_receipt_record_from_access, enqueue_receipt_access_for_issuance,
        fetch_encrypted_receipt_json, merge_retrieval_error, missing_encrypted_receipt_error,
        receipt_issuance_record as load_receipt_issuance_record,
        receipt_issuance_record_by_receipt_id as load_receipt_issuance_record_by_receipt_id,
        receipt_issuance_record_matches_draft,
        receipt_issuance_records as load_receipt_issuance_records, receipt_record_matches_access,
        store_encrypted_receipt_json, ReceiptAccessRecord, ReceiptAccessView,
        ReceiptIssuanceRecord, ReceiptIssuanceStatus, ReceiptIssuanceView, ReceiptRecord,
        ReceiptRetrievalStatus,
    },
    domain::recovery::{recovery_marker_report, EncryptedLinkRecoveryMarkerReport},
    identity::{IdentityState, IdentityStatus},
    storage::{
        outbound_private_queue_head_is_claimable, EncryptedLinkStateRecord, LinkedPeerRecord,
        OutboundPrivateMessageRecord, PeerLinkOperationLease, StorageAdapter, StorageTransaction,
    },
    PaykitReceiverPath, PaykitSdkError, PaymentAdapter, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointReservation, PrivatePaymentEndpointReservationCancellation,
    PrivatePaymentEndpointSelectionRequest, PrivatePaymentListView, PrivateReceivingDetail,
    PubkyPublicKey, PubkySessionAccess, PubkySessionProvider, PublicPaymentEndpointCandidate,
    PublicPaymentEndpointSelectionRequest, PublicReceivingDetail, Result,
};

mod backup;
mod contacts;
mod encrypted_links;
mod outbound_private;
mod payment_requests;
mod payment_resolution;
mod private_lists;
mod private_stream;
mod profiles;
mod public_endpoints;
mod receipts;
mod recovery;

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
    /// Last persisted identity status.
    pub identity: IdentityStatus,
}

/// Stateful Paykit SDK runtime for one app-owned local Paykit runtime.
pub struct PaykitSdk<S, K, P, C = SystemClock> {
    storage: S,
    pubky: K,
    payment: P,
    config: PaykitSdkConfig,
    clock: C,
    identity_operation_in_progress: Arc<Mutex<bool>>,
    private_stream_normalized: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateQueueReadiness {
    Ready,
    PendingHandshake,
}

struct RuntimeOperationGuard {
    in_progress: Arc<Mutex<bool>>,
}

impl Drop for RuntimeOperationGuard {
    fn drop(&mut self) {
        if let Ok(mut in_progress) = self.in_progress.lock() {
            *in_progress = false;
        }
    }
}

impl<S, K, P> PaykitSdk<S, K, P, SystemClock>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
{
    /// Create an SDK runtime with the system clock.
    pub fn new(storage: S, pubky: K, payment: P, config: PaykitSdkConfig) -> Result<Self> {
        Self::try_with_clock(storage, pubky, payment, config, SystemClock)
    }

    /// Fallible alias for [`Self::new`].
    pub fn try_new(storage: S, pubky: K, payment: P, config: PaykitSdkConfig) -> Result<Self> {
        Self::new(storage, pubky, payment, config)
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
    pub fn try_with_clock(
        storage: S,
        pubky: K,
        payment: P,
        config: PaykitSdkConfig,
        clock: C,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            storage,
            pubky,
            payment,
            config,
            clock,
            identity_operation_in_progress: Arc::new(Mutex::new(false)),
            private_stream_normalized: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Normalize stored derived private stream classification state once per
    /// process before it is projected or re-validated.
    ///
    /// The memo flag is rescan-avoidance only: normalization is idempotent, a
    /// fresh process re-runs it once, and a concurrent double-run is harmless.
    /// The flag is set only after the normalization transaction commits, so a
    /// failed run is retried by the next guarded operation.
    pub(crate) async fn ensure_private_stream_classifications_normalized(&self) -> Result<()> {
        if self.private_stream_normalized.load(Ordering::Acquire) {
            return Ok(());
        }
        self.storage
            .transaction(|tx| normalize_private_stream_classifications(tx))
            .await?;
        self.private_stream_normalized
            .store(true, Ordering::Release);
        Ok(())
    }

    fn claim_identity_operation(&self, context: &str) -> Result<RuntimeOperationGuard> {
        let mut in_progress =
            self.identity_operation_in_progress
                .lock()
                .map_err(|_| PaykitSdkError::Policy {
                    context: "identity operation lock poisoned".into(),
                    source: None,
                })?;
        if *in_progress {
            return Err(PaykitSdkError::Policy {
                context: format!(
                    "cannot {context} while another identity-scoped operation is in progress"
                ),
                source: None,
            });
        }
        *in_progress = true;
        Ok(RuntimeOperationGuard {
            in_progress: Arc::clone(&self.identity_operation_in_progress),
        })
    }

    #[cfg(test)]
    fn with_clock(storage: S, pubky: K, payment: P, config: PaykitSdkConfig, clock: C) -> Self {
        Self::try_with_clock(storage, pubky, payment, config, clock)
            .expect("test PaykitSdkConfig must be valid")
    }

    /// Initialize durable SDK identity state.
    pub async fn initialize(&self) -> Result<InitializationReport> {
        let _identity_guard = self.claim_identity_operation("initialize")?;
        let (session, state) = self.load_session_access_and_refresh_identity().await?;
        let live_session_available = session.is_some();
        self.ensure_private_stream_classifications_normalized()
            .await?;

        Ok(InitializationReport {
            identity: IdentityStatus::from_state(&state, live_session_available),
        })
    }

    /// Clear live Pubky session access and SDK-managed identity-scoped state.
    ///
    /// This is an explicit destructive sign-out. Apps that want to restore the
    /// same user's private Paykit state later should export and persist an SDK
    /// backup before calling this method.
    ///
    /// Missing live session access should be represented by
    /// [`PubkySessionProvider::load_session_access`] returning `None`; that
    /// preserves stored SDK state and only blocks Pubky-backed workflows.
    pub async fn sign_out(&self) -> Result<IdentityStatus> {
        let _identity_guard = self.claim_identity_operation("sign out")?;
        self.pubky.clear_session_access().await?;

        let now = self.clock.now();
        let state = self
            .storage
            .transaction(move |tx| {
                let previous = tx.load_identity_state();
                let previous_generation = previous
                    .as_ref()
                    .map(|state| state.sign_out_generation)
                    .unwrap_or_default();
                let was_signed_in = previous
                    .as_ref()
                    .is_some_and(|state| state.local_pubky_public_key.is_some());
                let generation = if was_signed_in {
                    previous_generation.saturating_add(1)
                } else {
                    previous_generation
                };

                tx.clear_identity_scoped_state();
                let state = IdentityState {
                    local_pubky_public_key: None,
                    local_receiver_noise_public_key: None,
                    initialized_at: now,
                    sign_out_generation: generation,
                };
                tx.save_identity_state(state.clone());
                Ok(state)
            })
            .await?;

        Ok(IdentityStatus::from_state(&state, false))
    }

    async fn load_session_access_and_refresh_identity(
        &self,
    ) -> Result<(Option<PubkySessionAccess>, IdentityState)> {
        let session = self.pubky.load_session_access().await?;
        let now = self.clock.now();

        let Some(session_access) = session.as_ref() else {
            let state = self
                .storage
                .transaction(move |tx| {
                    if let Some(previous) = tx.load_identity_state() {
                        return Ok(previous);
                    }

                    let state = IdentityState {
                        local_pubky_public_key: None,
                        local_receiver_noise_public_key: None,
                        initialized_at: now,
                        sign_out_generation: 0,
                    };
                    tx.save_identity_state(state.clone());
                    Ok(state)
                })
                .await?;

            return Ok((session, state));
        };

        let required_capabilities = self.config.required_session_capabilities();
        let active_identity = ActiveReceiverIdentity {
            local_pubky_public_key: session_access.public_key()?,
            local_receiver_noise_public_key: session_access.receiver_noise_public_key(),
        };
        session_access.validate_for_capabilities(&required_capabilities)?;
        let state = self
            .storage
            .transaction(move |tx| Ok(refresh_active_identity(tx, active_identity, now)))
            .await?;

        Ok((session, state))
    }

    async fn require_initialized_identity(&self, context: &str) -> Result<PubkyPublicKey> {
        self.storage
            .transaction(|tx| {
                tx.load_identity_state()
                    .and_then(|state| state.local_pubky_public_key)
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
        session_access.validate_for_capabilities(&self.config.required_session_capabilities())?;
        Ok(session_access)
    }

    /// Return the last persisted identity status, if initialized.
    pub async fn identity_status(&self) -> Result<Option<IdentityStatus>> {
        let session = self.pubky.load_session_access().await?;
        let Some(state) = self.storage.load_identity_state().await? else {
            return Ok(None);
        };
        if let Some(session) = &session {
            session.validate()?;
        }
        let required_capabilities = self.config.required_session_capabilities();
        let matching_session = session.as_ref().filter(|session| {
            session.public_key().ok().as_ref() == state.local_pubky_public_key.as_ref()
        });
        if let Some(session) = matching_session {
            session.validate_for_capabilities(&required_capabilities)?;
        }
        Ok(Some(IdentityStatus::from_state(
            &state,
            matching_session.is_some(),
        )))
    }

    /// Access SDK configuration.
    pub fn config(&self) -> &PaykitSdkConfig {
        &self.config
    }

    /// List locally tracked Linked Peer records.
    pub async fn linked_peers(&self) -> Result<Vec<LinkedPeerRecord>> {
        self.storage
            .transaction(|tx| {
                let mut records = tx
                    .export_storage_state()
                    .linked_peers
                    .into_values()
                    .collect::<Vec<_>>();
                records.sort_by(|left, right| {
                    left.counterparty
                        .as_str()
                        .cmp(right.counterparty.as_str())
                        .then_with(|| {
                            left.counterparty_receiver_path
                                .as_str()
                                .cmp(right.counterparty_receiver_path.as_str())
                        })
                });
                Ok(records)
            })
            .await
    }

    fn ensure_recovery_marker_publishing_enabled(&self) -> Result<()> {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            Err(PaykitSdkError::Policy {
                context: "Encrypted Link recovery marker publishing is disabled".into(),
                source: None,
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveReceiverIdentity {
    local_pubky_public_key: PubkyPublicKey,
    local_receiver_noise_public_key: PubkyPublicKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdentityTransition {
    Initial,
    PubkyIdentityChanged,
    ReceiverNoiseKeyChanged,
    Unchanged,
}

fn refresh_active_identity(
    tx: &mut dyn StorageTransaction,
    active: ActiveReceiverIdentity,
    initialized_at: DateTime<Utc>,
) -> IdentityState {
    let previous = tx.load_identity_state();
    let transition = identity_transition(previous.as_ref(), &active);
    let previous_generation = previous
        .as_ref()
        .map(|state| state.sign_out_generation)
        .unwrap_or_default();

    match transition {
        IdentityTransition::Initial | IdentityTransition::PubkyIdentityChanged => {
            tx.clear_identity_scoped_state();
        }
        IdentityTransition::ReceiverNoiseKeyChanged => tx.clear_private_identity_scoped_state(),
        IdentityTransition::Unchanged => {}
    }

    let sign_out_generation = match transition {
        IdentityTransition::PubkyIdentityChanged => previous_generation.saturating_add(1),
        _ => previous_generation,
    };
    let state = IdentityState {
        local_pubky_public_key: Some(active.local_pubky_public_key),
        local_receiver_noise_public_key: Some(active.local_receiver_noise_public_key),
        initialized_at,
        sign_out_generation,
    };
    tx.save_identity_state(state.clone());
    state
}

fn identity_transition(
    previous: Option<&IdentityState>,
    active: &ActiveReceiverIdentity,
) -> IdentityTransition {
    let Some(previous) = previous else {
        return IdentityTransition::Initial;
    };
    if previous.local_pubky_public_key.as_ref() != Some(&active.local_pubky_public_key) {
        return IdentityTransition::PubkyIdentityChanged;
    }
    if previous.local_receiver_noise_public_key.as_ref()
        != Some(&active.local_receiver_noise_public_key)
    {
        return IdentityTransition::ReceiverNoiseKeyChanged;
    }
    IdentityTransition::Unchanged
}

async fn fetch_public_text(
    storage: &pubky::PublicStorage,
    public_key: &PubkyPublicKey,
    path: &str,
    context: &'static str,
) -> Result<Option<String>> {
    let addr = public_resource_uri(public_key, path);
    match storage.get(addr).await {
        Ok(resp) => {
            let bytes = resp
                .bytes()
                .await
                .map_err(|err| PaykitSdkError::Transport {
                    context: context.into(),
                    source: Some(err.into()),
                })?;
            String::from_utf8(bytes.to_vec())
                .map(Some)
                .map_err(|err| PaykitSdkError::Protocol {
                    context: format!("{context}: invalid UTF-8: {err}"),
                    source: None,
                })
        }
        Err(err) if is_pubky_not_found(&err) => Ok(None),
        Err(err) => Err(map_pubky_transport_error(context, err)),
    }
}

async fn fetch_public_file_uri(
    storage: &pubky::PublicStorage,
    uri: &str,
    context: &'static str,
) -> Result<Option<Vec<u8>>> {
    let resource = uri
        .parse::<pubky::PubkyResource>()
        .map_err(|err| PaykitSdkError::Protocol {
            context: format!("{context}: invalid Pubky URI: {err}"),
            source: None,
        })?;
    match storage.get(resource).await {
        Ok(resp) => resp
            .bytes()
            .await
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|err| PaykitSdkError::Transport {
                context: context.into(),
                source: Some(err.into()),
            }),
        Err(err) if is_pubky_not_found(&err) => Ok(None),
        Err(err) => Err(map_pubky_transport_error(context, err)),
    }
}

async fn list_public_resources(
    storage: &pubky::PublicStorage,
    public_key: &PubkyPublicKey,
    path: &str,
    context: &'static str,
) -> Result<Vec<pubky::PubkyResource>> {
    const LIST_PAGE_LIMIT: u16 = 100;

    let addr = public_resource_uri(public_key, path);
    let mut entries = Vec::new();
    let mut cursor = None::<String>;
    loop {
        let mut builder = storage
            .list(&addr)
            .map_err(|err| map_pubky_transport_error(context, err))?
            .shallow(true)
            .limit(LIST_PAGE_LIMIT);
        if let Some(cursor) = cursor.as_deref() {
            builder = builder.cursor(cursor);
        }
        let page = match builder.send().await {
            Ok(page) => page,
            Err(err) if is_pubky_not_found(&err) => return Ok(entries),
            Err(err) => return Err(map_pubky_transport_error(context, err)),
        };
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        cursor = page
            .last()
            .map(|entry| format!("{}{}", entry.owner.z32(), entry.path.as_str()));
        entries.extend(page);
        if page_len < LIST_PAGE_LIMIT as usize {
            break;
        }
    }
    Ok(entries)
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

fn public_resource_uri(public_key: &PubkyPublicKey, path: &str) -> String {
    format!("pubky://{public_key}{path}")
}

#[cfg(test)]
mod tests;
