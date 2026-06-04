#![doc = "Stateful runtime layer for Paykit integrations."]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod adapters;
pub mod backup;
pub mod config;
pub mod contacts;
pub mod endpoints;
pub mod error;
pub mod identity;
pub mod linked_peers;
pub mod outbound_private;
pub mod payment_requests;
pub mod private_lists;
pub mod private_stream;
pub mod receipts;
pub mod reservations;
pub mod runtime;
pub mod scheduler;
pub mod storage;
pub mod telemetry;

#[doc(inline)]
pub use adapters::{
    ContactRecord, EndpointCompatibility, EndpointReservationAdapter, PaymentAdapter,
    PaymentAmountContext, PaymentEndpointCandidate, PaymentEndpointEvaluation,
    PaymentEndpointSelection, PaymentEndpointSelectionRequest, PaymentEndpointSource,
    PaymentExecutionResult, PaymentRequestExecution, PaymentTarget, ProfileRecord, ProfileUpdate,
    PubkySessionProvider, ReceivingDetail, ReceivingDetailScope, ReservedReceivingDetail,
    SchedulerAdapter,
};
#[doc(inline)]
pub use config::{
    EndpointManagementScope, PaykitSdkConfig, PrivateSharingPolicy, PublicFallbackPolicy,
    UnknownMessageRetentionPolicy,
};
#[doc(inline)]
pub use contacts::{
    ContactPaymentResolution, ContactPaymentResolutionRequest, ContactPaymentResolutionStatus,
};
#[doc(inline)]
pub use endpoints::{
    load_public_endpoint_records, save_public_endpoint_record, EndpointPublicationStatus,
    EndpointSyncChange, EndpointSyncReport,
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
    load_encrypted_link_state, load_linked_peer, mark_recovery_required, save_encrypted_link_state,
    save_linked_peer_state, LinkedPeerState,
};
#[doc(inline)]
pub use outbound_private::{
    enqueue_private_message, queued_outbound_private_messages, OutboundPrivateMessageStatus,
    OutboundPrivateSendFailure, OutboundPrivateSendReport,
};
#[doc(inline)]
pub use private_lists::{
    current_private_payment_list, derive_private_payment_list_view, enqueue_private_payment_list,
    PrivatePaymentListView,
};
#[doc(inline)]
pub use private_stream::{
    persist_private_stream_batch, EventIdConflict, PrivateStreamIntakeReport,
    PrivateStreamParseStatus,
};
#[doc(inline)]
pub use runtime::{Clock, InitializationReport, PaykitSdk, SystemClock};
#[doc(inline)]
pub use storage::{
    EncryptedLinkStateRecord, EventDedupRecord, InMemoryStorage, LinkedPeerRecord,
    NewOutboundPrivateMessage, NewPrivateStreamItem, OutboundPrivateMessageRecord,
    PrivateStreamItemRecord, PublicEndpointRecord, StorageAdapter, StorageState,
    StorageTransaction,
};

/// Common result alias for Paykit SDK operations.
pub type Result<T> = std::result::Result<T, PaykitSdkError>;
