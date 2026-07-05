#![doc = include_str!("../README.md")]
#![deny(rustdoc::broken_intra_doc_links)]

mod encrypted_link;
mod encrypted_link_recovery;
mod error;
mod event;
mod payment_amount;
mod payment_endpoint;
mod payment_reference;
mod payment_request;
mod private_payment_list;
mod pubky_routing;
mod receipt;
mod shared_wire;
mod validation;

#[doc(inline)]
pub use encrypted_link::{
    accept_encrypted_link, advance_handshake, clear_encrypted_link_outbox, close_encrypted_link,
    initiate_encrypted_link, restore_encrypted_link, restore_encrypted_link_from_config,
    restore_encrypted_link_handshake, restore_encrypted_link_handshake_from_config, EncryptedLink,
    EncryptedLinkHandshake, EncryptedLinkHandshakeSnapshot, EncryptedLinkSnapshot,
    HandshakeProgress, PrivateApplicationMessage, PrivateMessageKind,
    DEFAULT_MAX_RECOVERY_ATTEMPTS, DEFAULT_MAX_SEND_RETRIES,
};
#[doc(inline)]
pub use encrypted_link_recovery::{
    encrypted_link_recovery_marker_paths, fetch_encrypted_link_recovery_marker,
    parse_encrypted_link_recovery_marker_json, publish_encrypted_link_recovery_marker,
    remove_encrypted_link_recovery_marker, serialize_encrypted_link_recovery_marker,
    EncryptedLinkRecoveryMarker,
};
#[doc(inline)]
pub use error::PaykitError;
#[doc(inline)]
pub use event::EventId;
#[doc(inline)]
pub use payment_amount::PaymentAmount;
#[doc(inline)]
pub use payment_endpoint::{
    get_payment_endpoint, get_payment_list, remove_payment_endpoint, set_payment_endpoint,
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentList,
};
#[doc(inline)]
pub use payment_reference::{PaymentReference, PAYMENT_REFERENCE_MAX_LEN};
#[doc(inline)]
pub use payment_request::{
    parse_payment_request_event_message, send_payment_proof, send_payment_request,
    send_payment_request_acceptance, send_payment_request_cancellation,
    send_payment_request_rejection, serialize_payment_request_event, BillingPeriod, PaymentProof,
    PaymentRequest, PaymentRequestAcceptance, PaymentRequestCancellation, PaymentRequestEvent,
    PaymentRequestEventMessage, PaymentRequestId, PaymentRequestRejection, PaymentRequestTerms,
    Recurrence, RecurrenceUnit,
};
#[doc(inline)]
pub use private_payment_list::{
    parse_private_payment_list_json, serialize_private_payment_list_json, set_private_payment_list,
    PrivatePaymentList,
};
#[doc(inline)]
pub use pubky::PublicKey;
pub use pubky_noise;
#[doc(inline)]
pub use pubky_routing::{
    PAYKIT_ENCRYPTED_LINK_RECOVERY_PATH_PREFIX, PAYKIT_PATH_PREFIX, PAYKIT_PRIVATE_PATH_PREFIX,
};
#[doc(inline)]
pub use receipt::{
    decrypt_receipt, parse_receipt_access_event_message, parse_receipt_access_json,
    prepare_receipt, prepare_receipt_for_recipient, send_receipt_access,
    serialize_receipt_access_json, store_prepared_receipt, PreparedReceipt, Receipt, ReceiptAccess,
    ReceiptAccessEventMessage, ReceiptDecryptionKey, ReceiptDraft, ReceiptId,
};

/// Common result alias for Paykit operations.
pub type Result<T> = std::result::Result<T, PaykitError>;

#[cfg(test)]
mod tests;
