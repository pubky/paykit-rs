//! Receipt Access and Receipt indexing records.

use serde::{Deserialize, Serialize};

/// Retrieval status for an Encrypted Receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptRetrievalState {
    /// Receipt Access was received, but the receipt has not been fetched.
    Pending,
    /// Receipt was fetched and decrypted.
    Retrieved,
    /// Retrieval or decryption failed.
    Failed,
}
