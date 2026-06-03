#![doc = include_str!("../README.md")]
#![deny(rustdoc::broken_intra_doc_links)]

mod encrypted_link;
mod error;
mod payment_endpoint;
mod payment_reference;
mod private_payment_list;
mod pubky_routing;
mod receipt;

#[doc(inline)]
pub use encrypted_link::{
    accept_encrypted_link, advance_handshake, close_encrypted_link, initiate_encrypted_link,
    restore_encrypted_link, restore_encrypted_link_from_config, restore_encrypted_link_handshake,
    restore_encrypted_link_handshake_from_config, EncryptedLink, EncryptedLinkHandshake,
    EncryptedLinkHandshakeSnapshot, EncryptedLinkSnapshot, HandshakeProgress,
    PrivateApplicationMessage, PrivateMessageKind, DEFAULT_MAX_RECOVERY_ATTEMPTS,
    DEFAULT_MAX_SEND_RETRIES,
};
#[doc(inline)]
pub use error::PaykitError;
#[doc(inline)]
pub use payment_endpoint::{
    get_payment_endpoint, get_payment_list, remove_payment_endpoint, set_payment_endpoint,
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentList,
};
#[doc(inline)]
pub use payment_reference::PaymentReference;
#[doc(inline)]
pub use private_payment_list::{
    parse_private_payment_list_json, set_private_payment_list, PrivatePaymentList,
};
#[doc(inline)]
pub use pubky::PublicKey;
pub use pubky_noise;
#[doc(inline)]
pub use pubky_routing::{PAYKIT_PATH_PREFIX, PAYKIT_PRIVATE_PATH_PREFIX};
#[doc(inline)]
pub use receipt::{
    decrypt_receipt, get_receipt_access, issue_receipt, IssuedReceipt, Receipt, ReceiptAccess,
    ReceiptDecryptionKey, ReceiptDraft,
};

/// Common result alias for Paykit operations.
pub type Result<T> = std::result::Result<T, PaykitError>;

#[cfg(test)]
mod tests;
