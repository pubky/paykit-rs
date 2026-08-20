use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    ops::Deref,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use paykit_lib::{
    BillingPeriod, EncryptedLinkRecoveryMarker, EventId, PaymentEndpointIdentifier, PaymentProof,
    PaymentRequest, PaymentRequestAcceptance, PaymentRequestCancellation, PaymentRequestEvent,
    PaymentRequestId, PaymentRequestRejection, PaymentRequestTerms, PrivateMessageKind,
    ReceiptDraft,
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
    domain::contacts::{
        parse_profile_json, parse_pubky_profile_json, paykit_blob_path,
        paykit_blob_path_from_uri_or_path, paykit_blob_uri, profile_json,
        pubky_follow_keys_from_follow_entries, public_contact_json, public_contact_path,
        ContactRecord, ContactUpdate, PaykitBlobRecord, PaykitProfile, PaykitProfileRecord,
        ProfileResolution, PubkyProfileRecord, PAYKIT_PROFILE_BLOB_PATH_PREFIX,
        PAYKIT_PROFILE_PATH, PUBKY_FOLLOWS_PATH_PREFIX, PUBKY_PROFILE_PATH,
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
        requeue_recovery_required_outbound_messages,
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
        enqueue_checked_payment_request_action,
        enqueue_payment_request as enqueue_payment_request_message,
        payment_request_record_blocks_app_removal,
        payment_request_records as derive_payment_request_records,
        received_payment_request_records as derive_received_payment_request_records,
        request_from_record, PaymentRequestFilter, PaymentRequestLifecycleState,
        PaymentRequestLocalRole, PaymentRequestRecord, PaymentRequestTermsRecord,
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
    identity::{IdentityState, IdentityStatus, PubkyIdentityCapability},
    storage::{
        outbound_private_queue_head_is_claimable, EncryptedLinkStateRecord, LinkedPeerRecord,
        OutboundPrivateMessageRecord, PeerLinkOperationLease, StorageAdapter, StorageTransaction,
    },
    PaykitSdkError, PaymentAdapter, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointReservation, PrivatePaymentEndpointReservationCancellation,
    PrivatePaymentEndpointSelectionRequest, PrivatePaymentListView, PrivateReceivingDetail,
    PubkyPublicKey, PubkySessionAccess, PubkySessionProvider, PublicPaymentEndpointCandidate,
    PublicPaymentEndpointSelectionRequest, PublicReceivingDetail, Result,
    PAYKIT_SESSION_CAPABILITIES,
};

const PEER_LINK_OPERATION_LEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const OUTBOUND_PRIVATE_SEND_LEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const OUTBOUND_PRIVATE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
const RESERVATION_CANCELLATION_CLAIM_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(60);

mod app_registry;
mod app_removal;
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
    _guard: OwnedRwLockReadGuard<()>,
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
        }
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

    /// Initialize durable SDK identity state.
    pub async fn initialize(&self) -> Result<IdentityStatus> {
        let _identity_guard = self.claim_identity_operation("initialize")?;
        let (session, state) = self.load_session_access_and_refresh_identity().await?;
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

    /// Clear this application's live Pubky session access.
    ///
    /// Stored Paykit state remains intact so the same identity can resume it
    /// later and other applications are not affected.
    pub async fn sign_out(&self) -> Result<IdentityStatus> {
        let _identity_guard = self.claim_identity_operation("sign out")?;
        let _session_guard = Arc::clone(&self.session_operation_gate).write_owned().await;
        self.pubky.clear_session_access().await?;

        let now = self.clock.now();
        let state = self
            .storage
            .transaction(move |tx| {
                if let Some(state) = tx.load_identity_state() {
                    return Ok(state);
                }
                let state = IdentityState {
                    public_key: None,
                    initialized_at: now,
                };
                tx.save_identity_state(state.clone());
                Ok(state)
            })
            .await?;

        Ok(IdentityStatus::from_state(&state, false, false))
    }

    async fn load_session_access_and_refresh_identity(
        &self,
    ) -> Result<(Option<GuardedSessionAccess>, IdentityState)> {
        let session_guard = Arc::clone(&self.session_operation_gate).read_owned().await;
        let session = self.pubky.load_session_access().await?;
        let now = self.clock.now();

        let Some(session_access) = session else {
            let state = self
                .storage
                .transaction(move |tx| {
                    if let Some(previous) = tx.load_identity_state() {
                        return Ok(previous);
                    }

                    let state = IdentityState {
                        public_key: None,
                        initialized_at: now,
                    };
                    tx.save_identity_state(state.clone());
                    Ok(state)
                })
                .await?;

            return Ok((None, state));
        };

        let required_capabilities = PAYKIT_SESSION_CAPABILITIES;
        let public_key = session_access.public_key()?;
        session_access.capability_for_capabilities(required_capabilities)?;
        let state = self
            .storage
            .transaction(move |tx| bind_storage_to_identity(tx, public_key, now))
            .await?;

        Ok((
            Some(GuardedSessionAccess {
                access: session_access,
                _guard: session_guard,
            }),
            state,
        ))
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
    ) -> Result<GuardedSessionAccess> {
        let session_guard = Arc::clone(&self.session_operation_gate).read_owned().await;
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
        session_access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
        Ok(GuardedSessionAccess {
            access: session_access,
            _guard: session_guard,
        })
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
                    left.counterparty.as_str().cmp(right.counterparty.as_str())
                });
                Ok(records)
            })
            .await
    }
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
