mod access;
mod crypto;
mod types;
mod wire;

pub use access::{
    prepare_receipt, prepare_receipt_for_recipient, send_receipt_access, store_prepared_receipt,
};
pub use crypto::decrypt_receipt;
pub(crate) use types::RECEIPT_ENCRYPTION_ALGORITHM;
pub use types::{
    PreparedReceipt, Receipt, ReceiptAccess, ReceiptAccessEventMessage, ReceiptDecryptionKey,
    ReceiptDraft, ReceiptId, ENCRYPTED_RECEIPT_MAX_BYTES,
};
pub use wire::{
    parse_receipt_access_event_message, parse_receipt_access_json, serialize_receipt_access_json,
};

#[cfg(test)]
use wire::{EncryptedReceiptWire, ReceiptWire};

#[cfg(test)]
mod tests;
