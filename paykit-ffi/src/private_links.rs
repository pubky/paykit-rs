use std::{fmt, sync::Arc};

use paykit_sdk::storage::LinkedPeerRecord;
use paykit_sdk::{
    EncryptedLinkHandshakeRole, EncryptedLinkRecoveryMarkerReport, EventIdConflict,
    LinkedPeerHandshakeReport, LinkedPeerState, OutboundPrivateCounterpartySendReport,
    OutboundPrivateSendFailure, OutboundPrivateSendReport, PrivateStreamCounterpartyIntakeReport,
    PrivateStreamIntakeReport, RecoveryMarkerPublishFailure, ReservationCleanupFailure,
};

use crate::{
    sdk::FfiPaykitSdk,
    session::{app_public_key, parse_public_key},
    PaykitFfiError,
};

/// Private workflow error with redacted default context.
#[derive(uniffi::Object)]
pub struct FfiPrivateOperationError {
    category: String,
    code: String,
    redacted_context: String,
    debug_details: String,
}

impl fmt::Debug for FfiPrivateOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPrivateOperationError")
            .field("category", &self.category)
            .field("code", &self.code)
            .field("context", &self.redacted_context)
            .field(
                "debug_details",
                &format_args!("<redacted:{} bytes>", self.debug_details.len()),
            )
            .finish()
    }
}

impl FfiPrivateOperationError {
    pub(crate) fn new(
        category: &'static str,
        code: &'static str,
        context: &'static str,
        debug_details: String,
    ) -> Self {
        Self {
            category: category.into(),
            code: code.into(),
            redacted_context: format!("{context} (<redacted:{} bytes>)", debug_details.len()),
            debug_details,
        }
    }
}

#[uniffi::export]
impl FfiPrivateOperationError {
    /// Stable error category for app branching.
    pub fn category(&self) -> String {
        self.category.clone()
    }

    /// Stable error code for app branching.
    pub fn code(&self) -> String {
        self.code.clone()
    }

    /// Redacted error context safe for normal UI/log surfaces.
    pub fn redacted_context(&self) -> String {
        self.redacted_context.clone()
    }

    /// Export raw debug details for explicit diagnostic handling.
    pub fn export_debug_details(&self) -> String {
        self.debug_details.clone()
    }
}

/// Local relationship state for a counterparty.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiLinkedPeerState {
    /// The SDK tracks this counterparty, but no active Encrypted Link exists.
    NotLinked,
    /// An Encrypted Link Handshake is in progress.
    Linking,
    /// An Encrypted Link is established.
    Linked,
    /// Local state cannot safely continue without recovery.
    RecoveryRequired,
    /// Local policy blocks this peer.
    Blocked,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Local role for an in-progress Encrypted Link Handshake.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiEncryptedLinkHandshakeRole {
    /// Local peer initiated the handshake.
    Initiator,
    /// Local peer accepted a handshake initiated by the counterparty.
    Responder,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Locally tracked Linked Peer record.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiLinkedPeerRecord {
    /// Counterparty public key.
    pub counterparty: String,
    /// Current local relationship/link state.
    pub state: FfiLinkedPeerState,
    /// Last successful sync time as RFC3339 text.
    pub last_sync_at: Option<String>,
    /// Last private receive time as RFC3339 text.
    pub last_private_receive_at: Option<String>,
    /// Consecutive failure count for recovery/retry policy.
    pub failure_count: u32,
    /// Locally published Encrypted Link recovery attempt id.
    pub local_recovery_attempt_id: Option<String>,
    /// Creation time for the local recovery marker payload as RFC3339 text.
    pub local_recovery_marker_created_at: Option<String>,
    /// Last local marker publish/remove error, when available.
    pub local_recovery_marker_last_error: Option<Arc<FfiPrivateOperationError>>,
    /// Latest counterparty recovery attempt id already observed.
    pub remote_recovery_attempt_id: Option<String>,
    /// Time the counterparty recovery marker was observed as RFC3339 text.
    pub remote_recovery_marker_observed_at: Option<String>,
}

/// Result of starting or advancing an Encrypted Link Handshake.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiLinkedPeerHandshakeReport {
    /// Counterparty public key.
    pub counterparty: String,
    /// Current Linked Peer state after the operation.
    pub state: FfiLinkedPeerState,
    /// Current Encrypted Link state generation.
    pub generation: u64,
    /// In-progress handshake role, when a handshake remains pending.
    pub handshake_role: Option<FfiEncryptedLinkHandshakeRole>,
}

/// Reused Event ID with a different payload.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiEventIdConflict {
    /// Conflicting Event ID.
    pub event_id: String,
    /// First stream item that used this Event ID.
    pub first_stream_item_id: u64,
    /// Stream item that reused this Event ID with a different payload.
    pub conflicting_stream_item_id: u64,
}

/// Summary of a persisted private stream batch.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPrivateStreamIntakeReport {
    /// Receive batch id assigned by storage.
    pub receive_batch_id: u64,
    /// Stored stream item ids in input order.
    pub stream_item_ids: Vec<u64>,
    /// Event ID conflicts found while updating dedupe records.
    pub event_conflicts: Vec<FfiEventIdConflict>,
}

/// Summary for receiving private messages from one counterparty.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivateStreamCounterpartyIntakeReport {
    /// Counterparty whose private stream was received.
    pub counterparty: String,
    /// Successful intake report, when receive completed.
    pub report: Option<FfiPrivateStreamIntakeReport>,
    /// Error text, when receive failed for this counterparty.
    pub error: Option<Arc<FfiPrivateOperationError>>,
}

/// Failed outbound private send attempt.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiOutboundPrivateSendFailure {
    /// Outbound message id.
    pub outbound_message_id: u64,
    /// Error from the send attempt.
    pub error: Arc<FfiPrivateOperationError>,
}

/// Failed cleanup of a superseded Payment Endpoint Reservation.
#[derive(uniffi::Record, Clone)]
pub struct FfiReservationCleanupFailure {
    /// Reservation id, when the failure is tied to a specific reservation.
    pub reservation_id: Option<String>,
    /// Cleanup error.
    pub error: Arc<FfiPrivateOperationError>,
}

impl fmt::Debug for FfiReservationCleanupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiReservationCleanupFailure")
            .field(
                "reservation_id",
                &self.reservation_id.as_ref().map(|_| "<redacted>"),
            )
            .field("error", &"<redacted>")
            .finish()
    }
}

/// Failed recovery marker publication during outbound private send recovery.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiRecoveryMarkerPublishFailure {
    /// Outbound message id that triggered recovery, when available.
    pub outbound_message_id: Option<u64>,
    /// Recovery marker publication error.
    pub error: Arc<FfiPrivateOperationError>,
}

/// Summary returned after processing outbound private messages.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiOutboundPrivateSendReport {
    /// Messages attempted in this run.
    pub attempted: Vec<u64>,
    /// Messages marked sent in this run.
    pub sent: Vec<u64>,
    /// Messages that failed in this run.
    pub failed: Vec<FfiOutboundPrivateSendFailure>,
    /// Superseded reservation cleanup failures observed in this run.
    pub reservation_cleanup_failures: Vec<FfiReservationCleanupFailure>,
    /// Recovery marker publication failures observed after fail-closed recovery.
    pub recovery_marker_failures: Vec<FfiRecoveryMarkerPublishFailure>,
}

/// Summary for processing outbound private messages for one counterparty.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiOutboundPrivateCounterpartySendReport {
    /// Counterparty whose queue was processed.
    pub counterparty: String,
    /// Successful send report, when processing completed.
    pub report: Option<FfiOutboundPrivateSendReport>,
    /// Error text, when processing failed for this counterparty.
    pub error: Option<Arc<FfiPrivateOperationError>>,
}

/// Public recovery marker state tracked for one Linked Peer.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiEncryptedLinkRecoveryMarkerReport {
    /// Counterparty public key.
    pub counterparty: String,
    /// Current Linked Peer state.
    pub state: FfiLinkedPeerState,
    /// Locally published recovery attempt id.
    pub local_attempt_id: Option<String>,
    /// Creation time for the local marker payload as RFC3339 text.
    pub local_marker_created_at: Option<String>,
    /// Last local marker publish/remove error, when available.
    pub local_marker_last_error: Option<Arc<FfiPrivateOperationError>>,
    /// Latest observed counterparty recovery attempt id.
    pub remote_attempt_id: Option<String>,
    /// Time the counterparty marker was observed as RFC3339 text.
    pub remote_marker_observed_at: Option<String>,
    /// Whether this operation observed a new counterparty marker.
    pub remote_marker_changed: bool,
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// List locally tracked Linked Peer records.
    pub async fn linked_peers(&self) -> Result<Vec<FfiLinkedPeerRecord>, PaykitFfiError> {
        self.runtime
            .linked_peers()
            .await
            .map(|records| records.into_iter().map(Into::into).collect())
            .map_err(Into::into)
    }

    /// Block a counterparty for local Paykit private workflows.
    pub async fn block_peer(
        &self,
        counterparty: String,
    ) -> Result<FfiLinkedPeerRecord, PaykitFfiError> {
        self.runtime
            .block_peer(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Remove a local peer block and return the peer to NotLinked.
    pub async fn unblock_peer(
        &self,
        counterparty: String,
    ) -> Result<FfiLinkedPeerRecord, PaykitFfiError> {
        self.runtime
            .unblock_peer(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Start an Encrypted Link Handshake as the initiator.
    pub async fn initiate_link_with_peer(
        &self,
        counterparty: String,
    ) -> Result<FfiLinkedPeerHandshakeReport, PaykitFfiError> {
        self.runtime
            .initiate_link_with_peer(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Start an Encrypted Link Handshake as the responder.
    pub async fn accept_link_with_peer(
        &self,
        counterparty: String,
    ) -> Result<FfiLinkedPeerHandshakeReport, PaykitFfiError> {
        self.runtime
            .accept_link_with_peer(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Advance the stored Encrypted Link Handshake for one counterparty.
    pub async fn advance_link_handshake(
        &self,
        counterparty: String,
    ) -> Result<FfiLinkedPeerHandshakeReport, PaykitFfiError> {
        self.runtime
            .advance_link_handshake(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Start or advance an Encrypted Link Handshake for one counterparty.
    pub async fn ensure_link_with_peer(
        &self,
        counterparty: String,
        max_advance_steps: u32,
    ) -> Result<FfiLinkedPeerHandshakeReport, PaykitFfiError> {
        self.runtime
            .ensure_link_with_peer(parse_public_key(counterparty)?, max_advance_steps)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Receive and durably persist available private messages.
    pub async fn receive_private_messages(
        &self,
        counterparty: String,
    ) -> Result<FfiPrivateStreamIntakeReport, PaykitFfiError> {
        self.runtime
            .receive_private_messages(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Receive private messages from every locally linked counterparty.
    pub async fn receive_private_messages_from_linked_peers(
        &self,
    ) -> Result<Vec<FfiPrivateStreamCounterpartyIntakeReport>, PaykitFfiError> {
        self.runtime
            .receive_private_messages_from_linked_peers()
            .await
            .map(|reports| reports.into_iter().map(Into::into).collect())
            .map_err(Into::into)
    }

    /// Send queued outbound private messages for one counterparty in order.
    pub async fn process_outbound_private_messages(
        &self,
        counterparty: String,
    ) -> Result<FfiOutboundPrivateSendReport, PaykitFfiError> {
        self.runtime
            .process_outbound_private_messages(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// List counterparties with queued private messages ready for retry.
    pub async fn pending_outbound_private_counterparties(
        &self,
    ) -> Result<Vec<String>, PaykitFfiError> {
        self.runtime
            .pending_outbound_private_counterparties()
            .await
            .map(|keys| keys.into_iter().map(|key| app_public_key(&key)).collect())
            .map_err(Into::into)
    }

    /// Process queued outbound private messages for every pending counterparty.
    pub async fn process_pending_private_messages(
        &self,
    ) -> Result<Vec<FfiOutboundPrivateCounterpartySendReport>, PaykitFfiError> {
        self.runtime
            .process_pending_private_messages()
            .await
            .map(|reports| reports.into_iter().map(Into::into).collect())
            .map_err(Into::into)
    }

    /// Return tracked Encrypted Link recovery marker state for a counterparty.
    pub async fn encrypted_link_recovery_marker_status(
        &self,
        counterparty: String,
    ) -> Result<Option<FfiEncryptedLinkRecoveryMarkerReport>, PaykitFfiError> {
        self.runtime
            .encrypted_link_recovery_marker_status(&parse_public_key(counterparty)?)
            .await
            .map(|report| report.map(Into::into))
            .map_err(Into::into)
    }

    /// Publish a minimal local recovery marker for a counterparty.
    pub async fn publish_encrypted_link_recovery_marker(
        &self,
        counterparty: String,
    ) -> Result<FfiEncryptedLinkRecoveryMarkerReport, PaykitFfiError> {
        self.runtime
            .publish_encrypted_link_recovery_marker(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Observe a counterparty's public recovery marker.
    pub async fn observe_encrypted_link_recovery_marker(
        &self,
        counterparty: String,
    ) -> Result<FfiEncryptedLinkRecoveryMarkerReport, PaykitFfiError> {
        self.runtime
            .observe_encrypted_link_recovery_marker(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Remove the local public recovery marker for a counterparty.
    pub async fn remove_encrypted_link_recovery_marker(
        &self,
        counterparty: String,
    ) -> Result<FfiEncryptedLinkRecoveryMarkerReport, PaykitFfiError> {
        self.runtime
            .remove_encrypted_link_recovery_marker(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<LinkedPeerState> for FfiLinkedPeerState {
    fn from(value: LinkedPeerState) -> Self {
        match value {
            LinkedPeerState::NotLinked => Self::NotLinked,
            LinkedPeerState::Linking => Self::Linking,
            LinkedPeerState::Linked => Self::Linked,
            LinkedPeerState::RecoveryRequired => Self::RecoveryRequired,
            LinkedPeerState::Blocked => Self::Blocked,
            _ => Self::Unknown,
        }
    }
}

impl From<EncryptedLinkHandshakeRole> for FfiEncryptedLinkHandshakeRole {
    fn from(value: EncryptedLinkHandshakeRole) -> Self {
        match value {
            EncryptedLinkHandshakeRole::Initiator => Self::Initiator,
            EncryptedLinkHandshakeRole::Responder => Self::Responder,
            _ => Self::Unknown,
        }
    }
}

impl From<LinkedPeerRecord> for FfiLinkedPeerRecord {
    fn from(value: LinkedPeerRecord) -> Self {
        Self {
            counterparty: app_public_key(&value.counterparty),
            state: value.state.into(),
            last_sync_at: value.last_sync_at.map(|time| time.to_rfc3339()),
            last_private_receive_at: value.last_private_receive_at.map(|time| time.to_rfc3339()),
            failure_count: value.failure_count,
            local_recovery_attempt_id: value.local_recovery_attempt_id,
            local_recovery_marker_created_at: value
                .local_recovery_marker_created_at
                .map(|time| time.to_rfc3339()),
            local_recovery_marker_last_error: recovery_marker_error_opt(
                value.local_recovery_marker_last_error,
            ),
            remote_recovery_attempt_id: value.remote_recovery_attempt_id,
            remote_recovery_marker_observed_at: value
                .remote_recovery_marker_observed_at
                .map(|time| time.to_rfc3339()),
        }
    }
}

impl From<LinkedPeerHandshakeReport> for FfiLinkedPeerHandshakeReport {
    fn from(value: LinkedPeerHandshakeReport) -> Self {
        Self {
            counterparty: app_public_key(&value.counterparty),
            state: value.state.into(),
            generation: value.generation,
            handshake_role: value.handshake_role.map(Into::into),
        }
    }
}

impl From<EventIdConflict> for FfiEventIdConflict {
    fn from(value: EventIdConflict) -> Self {
        Self {
            event_id: value.event_id,
            first_stream_item_id: value.first_stream_item_id,
            conflicting_stream_item_id: value.conflicting_stream_item_id,
        }
    }
}

impl From<PrivateStreamIntakeReport> for FfiPrivateStreamIntakeReport {
    fn from(value: PrivateStreamIntakeReport) -> Self {
        Self {
            receive_batch_id: value.receive_batch_id,
            stream_item_ids: value.stream_item_ids,
            event_conflicts: value.event_conflicts.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<PrivateStreamCounterpartyIntakeReport> for FfiPrivateStreamCounterpartyIntakeReport {
    fn from(value: PrivateStreamCounterpartyIntakeReport) -> Self {
        Self {
            counterparty: app_public_key(&value.counterparty),
            report: value.report.map(Into::into),
            error: private_receive_error_opt(value.error),
        }
    }
}

impl From<OutboundPrivateSendFailure> for FfiOutboundPrivateSendFailure {
    fn from(value: OutboundPrivateSendFailure) -> Self {
        Self {
            outbound_message_id: value.outbound_message_id,
            error: private_error(
                "outbound_private_send",
                "send_failed",
                "outbound private send failed",
                value.error,
            ),
        }
    }
}

impl From<ReservationCleanupFailure> for FfiReservationCleanupFailure {
    fn from(value: ReservationCleanupFailure) -> Self {
        Self {
            reservation_id: value.reservation_id,
            error: private_error(
                "reservation_cleanup",
                "cleanup_failed",
                "reservation cleanup failed",
                value.error,
            ),
        }
    }
}

impl From<RecoveryMarkerPublishFailure> for FfiRecoveryMarkerPublishFailure {
    fn from(value: RecoveryMarkerPublishFailure) -> Self {
        Self {
            outbound_message_id: value.outbound_message_id,
            error: private_error(
                "recovery_marker",
                "publish_failed",
                "recovery marker publish failed",
                value.error,
            ),
        }
    }
}

impl From<OutboundPrivateSendReport> for FfiOutboundPrivateSendReport {
    fn from(value: OutboundPrivateSendReport) -> Self {
        Self {
            attempted: value.attempted,
            sent: value.sent,
            failed: value.failed.into_iter().map(Into::into).collect(),
            reservation_cleanup_failures: value
                .reservation_cleanup_failures
                .into_iter()
                .map(Into::into)
                .collect(),
            recovery_marker_failures: value
                .recovery_marker_failures
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl From<OutboundPrivateCounterpartySendReport> for FfiOutboundPrivateCounterpartySendReport {
    fn from(value: OutboundPrivateCounterpartySendReport) -> Self {
        Self {
            counterparty: app_public_key(&value.counterparty),
            report: value.report.map(Into::into),
            error: outbound_queue_error_opt(value.error),
        }
    }
}

impl From<EncryptedLinkRecoveryMarkerReport> for FfiEncryptedLinkRecoveryMarkerReport {
    fn from(value: EncryptedLinkRecoveryMarkerReport) -> Self {
        Self {
            counterparty: app_public_key(&value.counterparty),
            state: value.state.into(),
            local_attempt_id: value.local_attempt_id,
            local_marker_created_at: value.local_marker_created_at.map(|time| time.to_rfc3339()),
            local_marker_last_error: recovery_marker_error_opt(value.local_marker_last_error),
            remote_attempt_id: value.remote_attempt_id,
            remote_marker_observed_at: value
                .remote_marker_observed_at
                .map(|time| time.to_rfc3339()),
            remote_marker_changed: value.remote_marker_changed,
        }
    }
}

fn private_error(
    category: &'static str,
    code: &'static str,
    context: &'static str,
    value: String,
) -> Arc<FfiPrivateOperationError> {
    Arc::new(FfiPrivateOperationError::new(
        category, code, context, value,
    ))
}

fn private_receive_error_opt(value: Option<String>) -> Option<Arc<FfiPrivateOperationError>> {
    value.map(|value| {
        private_error(
            "private_receive",
            "receive_failed",
            "private receive failed",
            value,
        )
    })
}

fn outbound_queue_error_opt(value: Option<String>) -> Option<Arc<FfiPrivateOperationError>> {
    value.map(|value| {
        private_error(
            "outbound_private_queue",
            "queue_processing_failed",
            "outbound private queue processing failed",
            value,
        )
    })
}

fn recovery_marker_error_opt(value: Option<String>) -> Option<Arc<FfiPrivateOperationError>> {
    value.map(|value| {
        private_error(
            "recovery_marker",
            "marker_operation_failed",
            "recovery marker operation failed",
            value,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_private_operation_error_debug_redacts_text() {
        let error = FfiPrivateOperationError::new(
            "private_receive",
            "receive_failed",
            "private receive failed",
            "private transport failure".into(),
        );

        let debug = format!("{error:?}");
        assert!(debug.contains("private_receive"));
        assert!(debug.contains("receive_failed"));
        assert!(debug.contains("<redacted:25 bytes>"));
        assert!(!debug.contains("private transport failure"));
        assert_eq!(error.category(), "private_receive");
        assert_eq!(error.code(), "receive_failed");
        assert_eq!(
            error.redacted_context(),
            "private receive failed (<redacted:25 bytes>)"
        );
        assert_eq!(error.export_debug_details(), "private transport failure");
    }

    #[test]
    fn test_reservation_cleanup_failure_debug_redacts_id_and_error() {
        let failure = FfiReservationCleanupFailure {
            reservation_id: Some("reservation-id-secret".into()),
            error: Arc::new(FfiPrivateOperationError::new(
                "reservation_cleanup",
                "cleanup_failed",
                "reservation cleanup failed",
                "cleanup-error-secret".into(),
            )),
        };

        let debug = format!("{failure:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("reservation-id-secret"));
        assert!(!debug.contains("cleanup-error-secret"));
        assert!(!debug.contains("cleanup_failed"));
    }

    #[test]
    fn test_linked_peer_record_maps_timestamps_and_errors() {
        let record = LinkedPeerRecord {
            counterparty: parse_public_key(
                "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into(),
            )
            .unwrap(),
            state: LinkedPeerState::RecoveryRequired,
            last_sync_at: Some("2026-06-18T11:00:00Z".parse().unwrap()),
            last_private_receive_at: None,
            failure_count: 2,
            local_recovery_attempt_id: Some("attempt-1".into()),
            local_recovery_marker_created_at: None,
            local_recovery_marker_last_error: Some("publish failed".into()),
            remote_recovery_attempt_id: None,
            remote_recovery_marker_observed_at: None,
        };

        let ffi = FfiLinkedPeerRecord::from(record);

        assert_eq!(ffi.state, FfiLinkedPeerState::RecoveryRequired);
        assert_eq!(
            ffi.last_sync_at.as_deref(),
            Some("2026-06-18T11:00:00+00:00")
        );
        assert_eq!(
            ffi.local_recovery_marker_last_error
                .as_ref()
                .unwrap()
                .export_debug_details(),
            "publish failed"
        );
        assert_eq!(
            ffi.local_recovery_marker_last_error
                .as_ref()
                .unwrap()
                .category(),
            "recovery_marker"
        );
    }

    #[test]
    fn test_private_stream_report_maps_conflicts() {
        let report = PrivateStreamIntakeReport {
            receive_batch_id: 7,
            stream_item_ids: vec![10, 11],
            event_conflicts: vec![EventIdConflict {
                event_id: "event-1".into(),
                first_stream_item_id: 10,
                conflicting_stream_item_id: 11,
            }],
        };

        let ffi = FfiPrivateStreamIntakeReport::from(report);

        assert_eq!(ffi.receive_batch_id, 7);
        assert_eq!(ffi.stream_item_ids, vec![10, 11]);
        assert_eq!(ffi.event_conflicts[0].event_id, "event-1");
    }
}
