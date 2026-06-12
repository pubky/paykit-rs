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
    PaymentEndpointCandidate, PaymentExecutionResult, PaymentRequestExecution, PaymentTarget,
    ProfileRecord, ProfileUpdate, PubkySessionProvider, ReceivingDetail, ReceivingDetailScope,
    ReservedReceivingDetail, SchedulerAdapter,
};
#[doc(inline)]
pub use config::{
    EndpointManagementScope, PaykitSdkConfig, PrivateSharingPolicy, PublicFallbackPolicy,
    UnknownMessageRetentionPolicy,
};
#[doc(inline)]
pub use error::PaykitSdkError;
#[doc(inline)]
pub use identity::{
    IdentityState, IdentityStatus, PubkyIdentityCapability, PubkyLocalSecretKey, PubkyPublicKey,
    PubkySessionAccess,
};
#[doc(inline)]
pub use runtime::{Clock, InitializationReport, PaykitSdk, SystemClock};
#[doc(inline)]
pub use storage::{
    EncryptedLinkStateRecord, EventDedupRecord, InMemoryStorage, LinkedPeerRecord,
    NewPrivateStreamItem, PrivateStreamItemRecord, StorageAdapter, StorageState,
    StorageTransaction,
};

/// Common result alias for Paykit SDK operations.
pub type Result<T> = std::result::Result<T, PaykitSdkError>;
