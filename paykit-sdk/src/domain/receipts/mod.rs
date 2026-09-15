//! Receipt Access indexing helpers.
//!
//! Indexed Receipt Access and receipt issuance records include Receipt
//! Decryption Keys or exact Receipt Access JSON. Store them as private SDK
//! state and avoid logging field values directly.

mod access;
mod builder;
mod issuance;
mod records;

pub use builder::ReceiptDraftBuilder;
pub use records::{
    ReceiptAccessRecord, ReceiptAccessView, ReceiptIssuanceRecord, ReceiptIssuanceStatus,
    ReceiptIssuanceView, ReceiptRecord, ReceiptRetrievalStatus,
};

pub(crate) use access::{
    decrypt_receipt_record_from_access, fetch_encrypted_receipt_json, merge_retrieval_error,
    missing_encrypted_receipt_error, receipt_access_key_hash, receipt_record_matches_access,
};
pub(crate) use issuance::{
    enqueue_receipt_access_for_issuance, receipt_issuance_record,
    receipt_issuance_record_by_receipt_id, receipt_issuance_record_matches_draft,
    receipt_issuance_records, store_encrypted_receipt_json,
};

#[cfg(test)]
use crate::{PaykitSdkError, PubkyPublicKey};
#[cfg(test)]
use access::{encrypted_receipt_json_from_bytes, is_not_found};
#[cfg(test)]
pub(crate) use access::{receipt_access_record_by_receipt_id, receipt_access_records};
#[cfg(test)]
use chrono::DateTime;
#[cfg(test)]
use issuance::store_encrypted_receipt_error;
#[cfg(test)]
use paykit_lib::{Receipt, ReceiptDecryptionKey};
#[cfg(test)]
use pubky::{errors::RequestError, Error as PubkyError, StatusCode};

#[cfg(test)]
mod tests;
