#![doc = "Stateful runtime layer for Paykit integrations."]
#![deny(rustdoc::broken_intra_doc_links)]

mod backup;
mod config;
mod domain;
mod error;
mod identity;
mod pubky_session;
mod runtime;
/// Durable storage traits and in-memory test storage.
pub mod storage;
#[cfg(test)]
mod test_utils;

#[doc(inline)]
pub use backup::{export_backup_state, RestoreReport, SdkBackupState, SDK_BACKUP_VERSION};
#[doc(inline)]
pub use config::{
    EncryptedLinkRecoveryMarkerPolicy, EndpointManagementScope, PaykitSdkConfig,
    PublicContactSharingPolicy, DEFAULT_PROFILE_NAMESPACE,
};
#[doc(inline)]
pub use domain::adapters::{
    PaymentAdapter, PaymentAmountContext, PaymentTarget, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointReservation, PrivatePaymentEndpointReservationCancellation,
    PrivatePaymentEndpointSelectionRequest, PrivateReceivingDetail, PubkySessionProvider,
    PublicPaymentEndpointCandidate, PublicPaymentEndpointSelectionRequest, PublicReceivingDetail,
};
#[doc(inline)]
pub use domain::contacts::{
    ContactProfileResolution, ContactProfileSource, ContactRecord, ContactUpdate, PaykitBlobRecord,
    PaykitProfile, PaykitProfileRecord, PubkyProfile, PubkyProfileLink, PubkyProfileRecord,
    PUBKY_FOLLOWS_PATH_PREFIX, PUBKY_PROFILE_PATH,
};
#[doc(inline)]
pub use domain::endpoints::{load_public_endpoint_records, EndpointSyncChange, EndpointSyncReport};
#[doc(inline)]
pub use domain::linked_peers::{
    load_encrypted_link_state, load_linked_peer, EncryptedLinkHandshakeRole,
    LinkedPeerHandshakeReport, LinkedPeerState,
};
#[doc(inline)]
pub use domain::outbound_private::{
    OutboundPrivateCounterpartySendReport, OutboundPrivateMessageStatus,
    OutboundPrivateSendFailure, OutboundPrivateSendReport, RecoveryMarkerPublishFailure,
    ReservationCleanupFailure,
};
#[doc(inline)]
pub use domain::payment_requests::{
    PaymentProofRecord, PaymentRequestFilter, PaymentRequestLifecycleState,
    PaymentRequestLocalRole, PaymentRequestRecord, PaymentRequestRecurrenceRecord,
    PaymentRequestTermsRecord,
};
#[doc(inline)]
pub use domain::payment_resolution::{
    PreparedPrivateContactPayment, PrivateContactPaymentResolution, PrivatePaymentResolutionState,
    PrivatePaymentResolutionStatus, PublicContactPaymentResolution, PublicPaymentResolutionStatus,
    ResolvedPrivatePaymentEndpoint, ResolvedPublicPaymentEndpoint,
};
#[doc(inline)]
pub use domain::private_lists::{
    PrivatePaymentListDeliveryFailure, PrivatePaymentListDeliveryReport,
    PrivatePaymentListReservationUpdate, PrivatePaymentListSyncChange,
    PrivatePaymentListSyncReport, PrivatePaymentListView,
};
#[doc(inline)]
pub use domain::private_stream::{
    EventIdConflict, PrivateStreamCounterpartyIntakeReport, PrivateStreamIntakeReport,
    PrivateStreamParseStatus,
};
#[doc(inline)]
pub use domain::publication::PublicationStatus;
#[doc(inline)]
pub use domain::receipts::{
    ReceiptAccessView, ReceiptDraftBuilder, ReceiptIssuanceStatus, ReceiptIssuanceView,
    ReceiptRecord, ReceiptRetrievalStatus,
};
#[doc(inline)]
pub use domain::records::{AmountRecord, BillingPeriodRecord};
#[doc(inline)]
pub use domain::recovery::EncryptedLinkRecoveryMarkerReport;
#[doc(inline)]
pub use error::PaykitSdkError;
#[doc(inline)]
pub use identity::{
    IdentityState, IdentityStatus, PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess,
    ReceiverNoiseSecretKey,
};
#[doc(inline)]
pub use paykit_lib::{PaykitReceiverCapabilities, PaykitReceiverMarker, PaykitReceiverPath};
#[doc(inline)]
pub use pubky_session::{
    parse_pubky_auth_url, parse_pubky_resource, resolve_pubky_url, PubkyAuthCompanionClaim,
    PubkyAuthCompanionClaimApprovalError, PubkyAuthDetails, PubkyAuthRequest, PubkyAuthRequestKind,
    PubkyResourceRef, PubkySessionBootstrap, PubkySessionBootstrapResult, PubkySessionSecret,
};
#[doc(inline)]
pub use runtime::{Clock, InitializationReport, PaykitSdk, SystemClock};
#[doc(inline)]
pub use storage::{InMemoryStorage, StorageAdapter, StorageTransaction};

/// Common result alias for Paykit SDK operations.
pub type Result<T> = std::result::Result<T, PaykitSdkError>;
