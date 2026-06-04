#![doc = "Stateful runtime layer for Paykit integrations."]
#![deny(rustdoc::broken_intra_doc_links)]

/// Adapter traits and payment endpoint selection types.
pub mod adapters;
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
pub mod private_lists;
pub mod private_stream;
pub mod receipts;
/// SDK runtime facade.
pub mod runtime;
/// Durable storage traits and in-memory test storage.
pub mod storage;

#[doc(inline)]
pub use adapters::{
    EndpointCompatibility, PaymentAdapter, PaymentAmountContext, PaymentEndpointCandidate,
    PaymentEndpointEvaluation, PaymentEndpointSelection, PaymentEndpointSelectionRequest,
    PaymentEndpointSource, PaymentTarget, PubkySessionProvider, ReceivingDetail,
    ReceivingDetailScope,
};
#[doc(inline)]
pub use config::{
    EndpointManagementScope, PaykitSdkConfig, PrivateSharingPolicy, PublicFallbackPolicy,
};
#[doc(inline)]
pub use contacts::{
    ContactPaymentResolution, ContactPaymentResolutionRequest, ContactPaymentResolutionStatus,
};
#[doc(inline)]
pub use endpoints::{
    load_public_endpoint_records, EndpointPublicationStatus, EndpointSyncChange, EndpointSyncReport,
};
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
    queued_outbound_private_messages, OutboundPrivateMessageStatus, OutboundPrivateSendFailure,
    OutboundPrivateSendReport,
};
#[doc(inline)]
pub use private_lists::{
    current_private_payment_list, derive_private_payment_list_view, PrivatePaymentListView,
};
#[doc(inline)]
pub use private_stream::{EventIdConflict, PrivateStreamIntakeReport, PrivateStreamParseStatus};
#[doc(inline)]
pub use receipts::{
    receipt_access_record_by_receipt_id, receipt_access_records, ReceiptAccessRecord,
    ReceiptBillingPeriodRecord,
};
#[doc(inline)]
pub use runtime::{Clock, InitializationReport, PaykitSdk, SystemClock};
#[doc(inline)]
pub use storage::{
    EncryptedLinkStateRecord, EventDedupRecord, InMemoryStorage, LinkedPeerRecord,
    OutboundPrivateMessageRecord, PrivateStreamItemRecord, PublicEndpointRecord, StorageAdapter,
    StorageState, StorageTransaction,
};

/// Common result alias for Paykit SDK operations.
pub type Result<T> = std::result::Result<T, PaykitSdkError>;
