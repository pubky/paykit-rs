use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    ops::Deref,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use paykit_lib::{
    AllowanceId, AllowanceTerms, BillingPeriod, EncryptedLinkRecoveryMarker, EventId,
    PaymentEndpointIdentifier, PaymentProof, PaymentRequest, PaymentRequestAcceptance,
    PaymentRequestCancellation, PaymentRequestId, PaymentRequestRejection, PaymentRequestTerms,
    PrivateMessageKind, ReceiptDraft,
};
use pubky::{errors::RequestError, Error as PubkyError, StatusCode};
use serde_json::{Map as JsonMap, Value as JsonValue};
use tokio::sync::{OwnedRwLockReadGuard, RwLock};

#[cfg(test)]
use crate::domain::payment_requests::enqueue_payment_request_event as enqueue_payment_request_response_message;

use crate::{
    backup::{
        export_backup_state as export_sdk_backup_state,
        restore_backup_state_with_identity as restore_sdk_backup_state, RestoreReport,
        SdkBackupState,
    },
    config::{EndpointManagementScope, PaykitSdkConfig, PublicContactSharingPolicy},
    domain::allowances::{
        allowance_record as derive_allowance_record, allowance_records as derive_allowance_records,
        allowance_scopes, enqueue_allowance_end, enqueue_allowance_proposal,
        enqueue_allowance_response, sort_allowances_newest_first, AllowanceFilter,
        AllowanceLocalRole, AllowanceRecord, AllowanceResponse,
    },
    domain::contacts::{
        parse_profile_json, parse_pubky_profile_json, paykit_blob_path,
        paykit_blob_path_from_uri_or_path, paykit_blob_uri, profile_json,
        pubky_follow_keys_from_follow_entries, public_contact_json, public_contact_path,
        ContactRecord, ContactUpdate, PaykitBlobRecord, PaykitProfile, PaykitProfileRecord,
        ProfileResolution, PubkyProfileRecord, PAYKIT_PROFILE_BLOB_PATH_PREFIX,
        PAYKIT_PROFILE_PATH, PUBKY_FOLLOWS_PATH_PREFIX, PUBKY_PROFILE_PATH,
    },
    domain::endpoint_reservations::{
        expired_outbound_reservation_cancellations_in_transaction,
        queue_private_payment_list_with_reservations_with_link_lease, reservation_payload_hash,
        terminal_private_list_reservation_cancellations,
        PaymentEndpointReservationCancellationRecord, PrivatePaymentListQueuePolicy,
    },
    domain::endpoints::{
        failed_record, normalize_receiving_details, pending_publication_record,
        pending_removal_record, published_record, removed_record, EndpointSyncChange,
        EndpointSyncReport,
    },
    domain::linked_peers::{
        default_linked_peer, mark_recovery_required_for_marker_in_transaction,
        mark_recovery_required_in_transaction, mark_recovery_required_with_lease,
        requeue_recovery_required_outbound_messages, require_private_automation_ready,
        save_link_handshake_state_with_lease, save_linked_peer_state_with_lease,
        EncryptedLinkHandshakeRole, LinkedPeerHandshakeReport, LinkedPeerState,
    },
    domain::outbound_private::{
        mark_outbound_failed, mark_outbound_invalid, mark_outbound_recovery_required,
        mark_outbound_sent, queued_outbound_private_messages,
        validate_queued_outbound_private_message, OutboundPrivateCounterpartySendReport,
        OutboundPrivateMessageStatus, OutboundPrivateSendFailure, OutboundPrivateSendReport,
        RecoveryMarkerPublishFailure, ReservationCleanupFailure,
    },
    domain::payment_requests::{
        claim_payment_request_execution, enqueue_checked_payment_request_action,
        enqueue_payment_request as enqueue_payment_request_message, payment_proof_allowed_states,
        payment_request_record_blocks_app_removal,
        payment_request_records as derive_payment_request_records,
        release_payment_request_execution_claim, request_from_record, PaymentProofSubmission,
        PaymentRequestFilter, PaymentRequestLifecycleState, PaymentRequestLocalRole,
        PaymentRequestRecord, PaymentRequestTermsRecord,
    },
    domain::payment_resolution::{
        PreparedPrivateContactPayment, PrivateContactPaymentResolution,
        PrivatePaymentResolutionState, PrivatePaymentResolutionStatus,
        PublicContactPaymentResolution, PublicPaymentEndpointLoadFailure,
        PublicPaymentEndpointLoadFailureKind, PublicPaymentResolutionStatus,
        ResolvedPrivatePaymentEndpoint, ResolvedPublicPaymentEndpoint,
    },
    domain::private_lists::{
        counterparties_with_shared_private_payment_lists,
        current_private_payment_lists as load_current_private_payment_lists,
        enqueue_private_payment_list_with_link_lease as enqueue_private_payment_list_message_with_link_lease,
        PrivatePaymentListDeliveryFailure, PrivatePaymentListDeliveryReport,
        PrivatePaymentListReservationUpdate, PrivatePaymentListSyncChange,
        PrivatePaymentListSyncReport,
    },
    domain::private_stream::{
        PrivateStreamBatchWrite, PrivateStreamCounterpartyIntakeReport, PrivateStreamIntakeReport,
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
    identity::{IdentityState, IdentityStatus, PubkyIdentityCapability},
    storage::{
        outbound_private_queue_head_is_claimable,
        retry_storage_transaction as retry_storage_transaction_with_adapter,
        EncryptedLinkStateRecord, LinkedPeerRecord, OutboundPrivateMessageRecord,
        PaykitAppOperationLease, PeerLinkOperationLease, StorageAdapter, StorageTransaction,
    },
    PaykitSdkError, PaymentAdapter, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointReservation, PrivatePaymentEndpointReservationCancellation,
    PrivatePaymentEndpointSelectionRequest, PrivatePaymentListView, PrivateReceivingDetail,
    PubkyPublicKey, PubkySessionAccess, PubkySessionProvider, PublicPaymentEndpointCandidate,
    PublicPaymentEndpointSelectionRequest, PublicReceivingDetail, Result,
    PAYKIT_SESSION_CAPABILITIES,
};

const PEER_LINK_OPERATION_LEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const PAYKIT_APP_OPERATION_LEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const OUTBOUND_PRIVATE_SEND_LEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const OUTBOUND_PRIVATE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
const RESERVATION_CANCELLATION_CLAIM_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(60);

mod allowance_accounting;
mod allowances;
mod app_registry;
mod app_removal;
mod backup;
mod contacts;
mod encrypted_links;
mod key_rotation;
mod noise_key_authorization;
mod outbound_private;
mod payment_requests;
mod payment_resolution;
mod private_lists;
mod private_stream;
mod profiles;
mod public_endpoints;
mod receipts;
mod recovery;
mod reservation_cleanup;

pub use app_removal::PaykitAppRemovalBlockers;

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

/// Stateful SDK runtime for one application participating in a Paykit identity.
pub struct PaykitSdk<S, K, P, C = SystemClock> {
    storage: S,
    pubky: K,
    payment: P,
    config: PaykitSdkConfig,
    clock: C,
    identity_operation_in_progress: Arc<Mutex<bool>>,
    // Session-backed workflows hold a read guard; sign-out waits for all of
    // them before clearing access under the write guard.
    session_operation_gate: Arc<RwLock<()>>,
    // Retain only identity metadata so local cleanup and signed-out status do not
    // require access to session-protected storage.
    last_identity_state: Mutex<Option<IdentityState>>,
    // Runtime-local delivery witnesses are reusable only on the same Noise link
    // and for the same App. Restarting conservatively republishes each list.
    private_payment_list_publications:
        Mutex<HashMap<(PubkyPublicKey, paykit_lib::PaykitAppId), PrivatePaymentListPublication>>,
}

struct PrivatePaymentListPublication {
    link_id: [u8; 32],
    outbound_message_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateQueueReadiness {
    Ready,
    PendingHandshake,
}

struct RuntimeOperationGuard {
    in_progress: Arc<Mutex<bool>>,
}

struct GuardedSessionAccess {
    access: PubkySessionAccess,
    _guard: Arc<OwnedRwLockReadGuard<()>>,
}

struct SessionOperation {
    gate: Arc<RwLock<()>>,
    guard: Arc<OwnedRwLockReadGuard<()>>,
}

tokio::task_local! {
    static SESSION_OPERATION: SessionOperation;
}

impl Deref for GuardedSessionAccess {
    type Target = PubkySessionAccess;

    fn deref(&self) -> &Self::Target {
        &self.access
    }
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
            identity_operation_in_progress: Arc::new(Mutex::new(false)),
            session_operation_gate: Arc::new(RwLock::new(())),
            last_identity_state: Mutex::new(None),
            private_payment_list_publications: Mutex::new(HashMap::new()),
        }
    }

    fn claim_identity_operation(&self, context: &str) -> Result<RuntimeOperationGuard> {
        if self.active_session_guard().is_some() || crate::storage::shared_state_operation_active()
        {
            return Err(PaykitSdkError::Policy {
                context: format!("cannot {context} inside a session-backed storage operation"),
                source: None,
            });
        }
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

    fn active_session_guard(&self) -> Option<Arc<OwnedRwLockReadGuard<()>>> {
        SESSION_OPERATION
            .try_with(|operation| {
                Arc::ptr_eq(&operation.gate, &self.session_operation_gate)
                    .then(|| Arc::clone(&operation.guard))
            })
            .ok()
            .flatten()
    }

    async fn session_read_guard(&self) -> Result<Arc<OwnedRwLockReadGuard<()>>> {
        if let Some(guard) = self.active_session_guard() {
            return Ok(guard);
        }
        if crate::storage::shared_state_operation_active() {
            return Err(PaykitSdkError::Policy {
                context: "SDK session access must precede shared-state access".into(),
                source: None,
            });
        }
        Ok(Arc::new(
            Arc::clone(&self.session_operation_gate).read_owned().await,
        ))
    }

    async fn with_storage_operation<T: Send + 'static>(
        &self,
        operation: std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + '_>>,
    ) -> Result<T> {
        let guard = self.session_read_guard().await?;
        self.with_guarded_storage_operation(guard, operation).await
    }

    async fn with_guarded_storage_operation<T: Send + 'static>(
        &self,
        guard: Arc<OwnedRwLockReadGuard<()>>,
        operation: std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + '_>>,
    ) -> Result<T> {
        // Acquire the session gate before storage. Nested session reads reuse
        // this guard so a waiting sign-out cannot deadlock the operation.
        SESSION_OPERATION
            .scope(
                SessionOperation {
                    gate: Arc::clone(&self.session_operation_gate),
                    guard,
                },
                self.storage.with_operation(operation),
            )
            .await
    }

    fn cached_identity_state(&self) -> Option<IdentityState> {
        self.last_identity_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn cache_identity_state(&self, state: IdentityState) {
        *self
            .last_identity_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
    }

    async fn load_signed_out_identity_state(&self) -> Result<Option<IdentityState>> {
        let cached = self.cached_identity_state();
        if cached
            .as_ref()
            .is_some_and(|state| state.public_key.is_some())
        {
            return Ok(cached);
        }
        let state = self.storage.load_local_identity_state().await?.or(cached);
        if let Some(state) = &state {
            self.cache_identity_state(state.clone());
        }
        Ok(state)
    }

    async fn retry_storage_transaction<T, F, O>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnMut() -> O,
        O: FnOnce(&mut dyn StorageTransaction) -> Result<T> + Send,
    {
        retry_storage_transaction_with_adapter(&self.storage, operation).await
    }

    /// Initialize durable SDK identity state.
    ///
    /// Without a live session, return cached or locally available identity
    /// metadata without creating or refreshing shared state.
    pub async fn initialize(&self) -> Result<IdentityStatus> {
        let _identity_guard = self.claim_identity_operation("initialize")?;
        let (session, state, _) = self
            .load_session_access_and_refresh_identity_with(
                crate::backup::refresh_stored_message_classification,
            )
            .await?;
        let live_session_available = session.is_some();
        let required_capabilities = PAYKIT_SESSION_CAPABILITIES;
        let private_link_capable = session
            .as_ref()
            .map(|session| session.private_link_capable_for_capabilities(required_capabilities))
            .transpose()?
            .unwrap_or(false);

        Ok(IdentityStatus::from_state(
            &state,
            live_session_available,
            private_link_capable,
        ))
    }

    /// Revoke the current Pubky grant and clear this application's session access.
    ///
    /// Identity-wide Paykit state remains intact for other applications and a
    /// later session. Missing access and identity mismatches fail before remote
    /// revocation or local cleanup. Use [`Self::forget_session_access`] only
    /// when local-only cleanup is explicitly intended.
    pub async fn sign_out(&self) -> Result<IdentityStatus> {
        let _identity_guard = self.claim_identity_operation("sign out")?;
        let _session_guard = Arc::clone(&self.session_operation_gate).write_owned().await;
        let now = self.clock.now();
        let state = self
            .load_signed_out_identity_state()
            .await?
            .unwrap_or(IdentityState {
                public_key: None,
                initialized_at: now,
            });
        let session_access = self.pubky.load_session_access().await?;
        if session_access.is_none() && state.public_key.is_some() {
            return Err(PaykitSdkError::Identity {
                context: "cannot revoke Pubky grant during sign-out without live session access"
                    .into(),
                source: None,
            });
        }
        if let Some(access) = session_access {
            access.validate()?;
            if let Some(expected_public_key) = &state.public_key {
                if access.public_key()? != *expected_public_key {
                    return Err(PaykitSdkError::Identity {
                        context:
                            "cannot sign out because active Pubky session does not match initialized identity"
                                .into(),
                        source: None,
                    });
                }
            }
            self.pubky.revoke_session_access(&access).await?;
        }
        self.pubky.clear_session_access().await?;
        self.cache_identity_state(state.clone());

        Ok(IdentityStatus::from_state(&state, false, false))
    }

    /// Clear this application's session access without revoking the grant.
    ///
    /// Identity-wide Paykit state remains intact. A copied or separately
    /// persisted grant remains valid until it expires or is revoked elsewhere.
    /// No remote storage or session validation is required. The returned status
    /// includes only identity metadata already observed by this runtime.
    pub async fn forget_session_access(&self) -> Result<IdentityStatus> {
        let _identity_guard = self.claim_identity_operation("forget session access")?;
        let _session_guard = Arc::clone(&self.session_operation_gate).write_owned().await;
        self.pubky.clear_session_access().await?;
        let state = self.cached_identity_state().unwrap_or(IdentityState {
            public_key: None,
            initialized_at: self.clock.now(),
        });
        self.cache_identity_state(state.clone());

        Ok(IdentityStatus::from_state(&state, false, false))
    }

    async fn load_session_access_and_refresh_identity(
        &self,
    ) -> Result<(Option<GuardedSessionAccess>, IdentityState)> {
        let (access, identity, _) = self
            .load_session_access_and_refresh_identity_with(|_| Ok(()))
            .await?;
        Ok((access, identity))
    }

    async fn load_session_access_and_refresh_identity_with<T: Send + 'static>(
        &self,
        operation: impl Fn(&mut dyn StorageTransaction) -> Result<T> + Send + Sync,
    ) -> Result<(Option<GuardedSessionAccess>, IdentityState, Option<T>)> {
        let session_guard = self.session_read_guard().await?;
        let session = self.pubky.load_session_access().await?;
        let now = self.clock.now();

        let Some(session_access) = session else {
            let state = self
                .load_signed_out_identity_state()
                .await?
                .unwrap_or(IdentityState {
                    public_key: None,
                    initialized_at: now,
                });

            self.cache_identity_state(state.clone());
            return Ok((None, state, None));
        };

        let required_capabilities = PAYKIT_SESSION_CAPABILITIES;
        let public_key = session_access.public_key()?;
        session_access.capability_for_capabilities(required_capabilities)?;
        let noise_public_key = session_access
            .paykit_identity_secret_key()
            .as_ref()
            .map(crate::storage::paykit_noise_public_key);
        let (state, value) = self
            .retry_storage_transaction(|| {
                let public_key = public_key.clone();
                let noise_public_key = noise_public_key.clone();
                let operation = &operation;
                move |tx| {
                    let state = bind_storage_to_identity(tx, public_key, now)?;
                    if let Some(noise_public_key) = noise_public_key {
                        crate::storage::bind_paykit_noise_key(tx, noise_public_key)?;
                    }
                    Ok((state, operation(tx)?))
                }
            })
            .await?;

        self.cache_identity_state(state.clone());
        Ok((
            Some(GuardedSessionAccess {
                access: session_access,
                _guard: session_guard,
            }),
            state,
            Some(value),
        ))
    }

    async fn require_initialized_identity(&self, context: &str) -> Result<PubkyPublicKey> {
        self.storage
            .transaction(|tx| initialized_identity_in_transaction(tx, context))
            .await
    }

    async fn load_session_access_for_initialized_identity(
        &self,
        context: &str,
    ) -> Result<GuardedSessionAccess> {
        let session_guard = self.session_read_guard().await?;
        let session_access = self.pubky.load_session_access().await?;
        let session_access = self
            .storage
            .transaction(move |tx| {
                let expected_public_key = initialized_identity_in_transaction(tx, context)?;
                let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
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
                session_access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
                if let Some(key) = session_access.paykit_identity_secret_key() {
                    let public_key = crate::storage::paykit_noise_public_key(&key);
                    crate::storage::bind_paykit_noise_key(tx, public_key)?;
                }
                Ok(session_access)
            })
            .await?;
        Ok(GuardedSessionAccess {
            access: session_access,
            _guard: session_guard,
        })
    }

    /// Return the last observed identity status, if initialized.
    ///
    /// Without live session access, use only cached or locally available identity
    /// metadata. Session-protected storage is not read, and an uninitialized
    /// runtime without local metadata returns `None`.
    pub async fn identity_status(&self) -> Result<Option<IdentityStatus>> {
        let _session_guard = self.session_read_guard().await?;
        let session = self.pubky.load_session_access().await?;
        if session.is_none() {
            return Ok(self
                .load_signed_out_identity_state()
                .await?
                .map(|state| IdentityStatus::from_state(&state, false, false)));
        }
        let Some(state) = self.storage.load_identity_state().await? else {
            return Ok(None);
        };
        self.cache_identity_state(state.clone());
        if let Some(session) = &session {
            session.validate()?;
        }
        let required_capabilities = PAYKIT_SESSION_CAPABILITIES;
        let matching_session = session
            .as_ref()
            .filter(|session| session.public_key().ok().as_ref() == state.public_key.as_ref());
        let private_link_capable = matching_session
            .map(|session| session.private_link_capable_for_capabilities(required_capabilities))
            .transpose()?
            .unwrap_or(false);
        Ok(Some(IdentityStatus::from_state(
            &state,
            matching_session.is_some(),
            private_link_capable,
        )))
    }

    /// Access SDK configuration.
    pub fn config(&self) -> &PaykitSdkConfig {
        &self.config
    }

    /// List Linked Peer records for the shared identity.
    pub async fn linked_peers(&self) -> Result<Vec<LinkedPeerRecord>> {
        self.storage
            .transaction(|tx| {
                let mut records = tx
                    .export_storage_state()
                    .linked_peers
                    .into_values()
                    .collect::<Vec<_>>();
                records.sort_by(|left, right| {
                    left.counterparty.as_str().cmp(right.counterparty.as_str())
                });
                Ok(records)
            })
            .await
    }
}

fn initialized_identity_in_transaction(
    tx: &dyn StorageTransaction,
    context: &str,
) -> Result<PubkyPublicKey> {
    tx.load_identity_state()
        .and_then(|state| state.public_key)
        .ok_or_else(|| PaykitSdkError::Identity {
            context: format!("cannot {context} without an initialized Pubky identity"),
            source: None,
        })
}

fn bind_storage_to_identity(
    tx: &mut dyn StorageTransaction,
    public_key: PubkyPublicKey,
    initialized_at: DateTime<Utc>,
) -> Result<IdentityState> {
    if let Some(state) = tx.load_identity_state() {
        if state
            .public_key
            .as_ref()
            .is_some_and(|stored| stored != &public_key)
        {
            return Err(PaykitSdkError::Identity {
                context: "active Pubky session does not match this SDK state backing".into(),
                source: None,
            });
        }
        if state.public_key.is_some() {
            return Ok(state);
        }
    }

    let state = IdentityState {
        public_key: Some(public_key),
        initialized_at,
    };
    tx.save_identity_state(state.clone());
    Ok(state)
}

async fn fetch_public_text(
    storage: &pubky::PublicStorage,
    public_key: &PubkyPublicKey,
    path: &str,
    context: &'static str,
    max_bytes: usize,
) -> Result<Option<String>> {
    let addr = public_resource_uri(public_key, path);
    match storage.get(addr).await {
        Ok(resp) => {
            let Some(bytes) = read_public_response(resp, max_bytes, context).await? else {
                return Ok(None);
            };
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| PaykitSdkError::Protocol {
                    context: format!("{context}: response is not valid UTF-8"),
                    source: None,
                })
        }
        Err(err) if is_pubky_not_found(&err) => Ok(None),
        Err(err) => Err(map_pubky_transport_error(context, err)),
    }
}

async fn fetch_public_text_with_revision(
    storage: &pubky::PublicStorage,
    public_key: &PubkyPublicKey,
    path: &str,
    context: &'static str,
    max_bytes: usize,
) -> Result<Option<(String, String)>> {
    let addr = public_resource_uri(public_key, path);
    match storage.get(addr).await {
        Ok(resp) => {
            let Some(bytes) = read_public_response(resp, max_bytes, context).await? else {
                return Ok(None);
            };
            let revision = paykit_lib::content_revision(&bytes);
            let text = String::from_utf8(bytes).map_err(|_| PaykitSdkError::Protocol {
                context: format!("{context}: response is not valid UTF-8"),
                source: None,
            })?;
            Ok(Some((text, revision)))
        }
        Err(err) if is_pubky_not_found(&err) => Ok(None),
        Err(err) => Err(map_pubky_transport_error(context, err)),
    }
}

async fn fetch_public_file_uri(
    storage: &pubky::PublicStorage,
    uri: &str,
    context: &'static str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let resource = uri
        .parse::<pubky::PubkyResource>()
        .map_err(|err| PaykitSdkError::Protocol {
            context: format!("{context}: invalid Pubky URI: {err}"),
            source: None,
        })?;
    match storage.get(resource).await {
        Ok(resp) => read_public_response(resp, max_bytes, context).await,
        Err(err) if is_pubky_not_found(&err) => Ok(None),
        Err(err) => Err(map_pubky_transport_error(context, err)),
    }
}

async fn read_public_response(
    mut response: reqwest::Response,
    max_bytes: usize,
    context: &'static str,
) -> Result<Option<Vec<u8>>> {
    let status = response.status();
    if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return Ok(None);
    }
    if !status.is_success() {
        // Do not read or retain an attacker-controlled HTTP error body.
        return Err(PaykitSdkError::Transport {
            context: format!("{context}: HTTP {status}"),
            source: None,
        });
    }
    require_response_size_within_limit(response.content_length(), max_bytes, context)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| PaykitSdkError::Transport {
            context: context.into(),
            source: Some(err.into()),
        })?
    {
        append_response_chunk(&mut bytes, &chunk, max_bytes, context)?;
    }
    Ok(Some(bytes))
}

fn require_positive_response_limit(max_bytes: usize, context: &'static str) -> Result<()> {
    if max_bytes == 0 {
        return Err(PaykitSdkError::Protocol {
            context: format!("{context}: byte limit must be greater than zero"),
            source: None,
        });
    }
    Ok(())
}

fn require_response_size_within_limit(
    content_length: Option<u64>,
    max_bytes: usize,
    context: &'static str,
) -> Result<()> {
    if content_length.is_some_and(|length| length > max_bytes as u64) {
        return Err(PaykitSdkError::Protocol {
            context: format!("{context}: response exceeds the {max_bytes}-byte limit"),
            source: None,
        });
    }
    Ok(())
}

fn append_response_chunk(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: usize,
    context: &'static str,
) -> Result<()> {
    if bytes.len().saturating_add(chunk.len()) > max_bytes {
        return Err(PaykitSdkError::Protocol {
            context: format!("{context}: response exceeds the {max_bytes}-byte limit"),
            source: None,
        });
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

async fn list_public_resources(
    storage: &pubky::PublicStorage,
    public_key: &PubkyPublicKey,
    path: &str,
    context: &'static str,
    max_entries: usize,
) -> Result<Vec<pubky::PubkyResource>> {
    const LIST_PAGE_LIMIT: u16 = 100;
    const LIST_MAX_PAGES: usize = 100;

    require_public_resource_entry_limit(max_entries, context)?;
    let addr = public_resource_uri(public_key, path);
    let mut entries = Vec::new();
    let mut cursor = None::<String>;
    let mut pages = 0usize;
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
        pages += 1;
        if pages > LIST_MAX_PAGES {
            return Err(PaykitSdkError::Protocol {
                context: format!("{context}: listing exceeded {LIST_MAX_PAGES} pages"),
                source: None,
            });
        }
        let page_len = page.len();
        cursor = Some(next_public_resource_cursor(
            entries.len(),
            &page,
            cursor.as_deref(),
            max_entries,
            context,
        )?);
        entries.extend(page);
        if page_len < LIST_PAGE_LIMIT as usize {
            break;
        }
    }
    Ok(entries)
}

fn next_public_resource_cursor(
    existing_entries: usize,
    page: &[pubky::PubkyResource],
    previous_cursor: Option<&str>,
    max_entries: usize,
    context: &'static str,
) -> Result<String> {
    require_public_resource_entry_limit(max_entries, context)?;
    if existing_entries.saturating_add(page.len()) > max_entries {
        return Err(PaykitSdkError::Protocol {
            context: format!("{context}: directory exceeds the {max_entries}-entry limit"),
            source: None,
        });
    }
    let next_cursor = page
        .last()
        .map(|entry| format!("{}{}", entry.owner.z32(), entry.path.as_str()))
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: format!("{context}: non-empty page has no cursor resource"),
            source: None,
        })?;
    if previous_cursor.is_some_and(|previous| next_cursor.as_str() <= previous) {
        return Err(PaykitSdkError::Protocol {
            context: format!("{context}: directory cursor did not advance"),
            source: None,
        });
    }
    Ok(next_cursor)
}

fn require_public_resource_entry_limit(max_entries: usize, context: &'static str) -> Result<()> {
    if max_entries == 0 {
        return Err(PaykitSdkError::Protocol {
            context: format!("{context}: entry limit must be greater than zero"),
            source: None,
        });
    }
    Ok(())
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
