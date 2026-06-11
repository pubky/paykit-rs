#![doc = "Stateful runtime layer for Paykit integrations."]
#![deny(rustdoc::broken_intra_doc_links)]

/// Adapter traits and payment endpoint selection types.
pub mod adapters;
/// SDK backup and restore payloads.
pub mod backup;
/// SDK runtime policy configuration.
pub mod config;
pub mod contacts;
pub mod endpoints;
/// SDK error type.
pub mod error;
/// Pubky identity and capability types.
pub mod identity;
pub mod linked_peers;
pub mod outbound_private;
pub mod payment_requests;
pub mod private_lists;
pub mod private_stream;
pub mod publication;
pub mod receipts;
pub mod records;
pub mod recovery;
/// SDK runtime facade.
pub mod runtime;
/// Durable storage traits and in-memory test storage.
pub mod storage;

mod endpoint_reservations;

#[doc(inline)]
pub use adapters::{
    PaymentAdapter, PaymentAmountContext, PaymentEndpointCandidate, PaymentEndpointReservation,
    PaymentEndpointReservationCancellation, PaymentEndpointSelectionRequest, PaymentEndpointSource,
    PaymentTarget, PubkySessionProvider, ReceivingDetail, ReceivingDetailScope,
};
#[doc(inline)]
pub use backup::{export_backup_state, RestoreReport, SdkBackupState, SDK_BACKUP_VERSION};
#[doc(inline)]
pub use config::{
    EncryptedLinkRecoveryMarkerPolicy, EndpointManagementScope, PaykitSdkConfig,
    PrivateSharingPolicy, PublicContactSharingPolicy, DEFAULT_PROFILE_NAMESPACE,
};
#[doc(inline)]
pub use contacts::{
    ContactPaymentResolution, ContactPaymentResolutionRequest, ContactPaymentResolutionStatus,
    ContactProfileResolution, ContactProfileSource, ContactRecord, ContactUpdate, PaykitBlobRecord,
    PaykitProfile, PaykitProfileRecord, PubkyProfile, PubkyProfileLink, PubkyProfileRecord,
    ResolvedPaymentEndpoint, PAYKIT_PROFILE_BLOB_PATH_PREFIX, PAYKIT_PROFILE_PATH,
    PAYKIT_PUBLIC_CONTACT_PATH_PREFIX, PUBKY_FOLLOWS_PATH_PREFIX, PUBKY_PROFILE_PATH,
};
#[doc(inline)]
pub use endpoints::{load_public_endpoint_records, EndpointSyncChange, EndpointSyncReport};
#[doc(inline)]
pub use error::PaykitSdkError;
#[doc(inline)]
pub use identity::{
    IdentityState, IdentityStatus, PubkyIdentityCapability, PubkyLocalSecretKey, PubkyPublicKey,
    PubkySessionAccess,
};
#[doc(inline)]
pub use linked_peers::{
    load_encrypted_link_state, load_linked_peer, EncryptedLinkHandshakeRole,
    LinkedPeerHandshakeReport, LinkedPeerState,
};
#[doc(inline)]
pub use outbound_private::{
    OutboundPrivateCounterpartySendReport, OutboundPrivateMessageStatus,
    OutboundPrivateSendFailure, OutboundPrivateSendReport, RecoveryMarkerPublishFailure,
    ReservationCleanupFailure,
};
#[doc(inline)]
pub use payment_requests::{
    PaymentProofRecord, PaymentRequestFilter, PaymentRequestLifecycleState,
    PaymentRequestLocalRole, PaymentRequestRecord, PaymentRequestRecurrenceRecord,
    PaymentRequestTermsRecord,
};
#[doc(inline)]
pub use private_lists::PrivatePaymentListView;
#[doc(inline)]
pub use private_stream::{
    EventIdConflict, PrivateStreamCounterpartyIntakeReport, PrivateStreamIntakeReport,
    PrivateStreamParseStatus,
};
#[doc(inline)]
pub use publication::PublicationStatus;
#[doc(inline)]
pub use receipts::{
    ReceiptAccessView, ReceiptDraftBuilder, ReceiptIssuanceStatus, ReceiptIssuanceView,
    ReceiptRecord, ReceiptRetrievalStatus,
};
#[doc(inline)]
pub use records::{AmountRecord, BillingPeriodRecord};
#[doc(inline)]
pub use recovery::EncryptedLinkRecoveryMarkerReport;
#[doc(inline)]
pub use runtime::{Clock, InitializationReport, PaykitSdk, SystemClock};
#[doc(inline)]
pub use storage::{InMemoryStorage, StorageAdapter, StorageTransaction};

/// Common result alias for Paykit SDK operations.
pub type Result<T> = std::result::Result<T, PaykitSdkError>;
