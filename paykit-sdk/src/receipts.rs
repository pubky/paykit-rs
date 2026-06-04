//! Receipt Access indexing helpers.
//!
//! Indexed Receipt Access records include Receipt Decryption Keys. Store them
//! as private SDK state and avoid logging field values directly.

use std::fmt;

use chrono::{DateTime, Utc};
use paykit_lib::ReceiptAccess;
use serde::{Deserialize, Serialize};

use crate::{storage::StorageAdapter, PubkyPublicKey, Result};

/// Durable Billing Period fields copied from a Receipt Access event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptBillingPeriodRecord {
    /// RFC3339 UTC timestamp using `Z`.
    pub starts_at: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub ends_at: String,
}

/// Indexed Receipt Access event.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptAccessRecord {
    /// Counterparty that sent the Receipt Access event.
    pub counterparty: PubkyPublicKey,
    /// Stream item id that first carried this Event ID.
    pub stream_item_id: u64,
    /// Receive batch id that contained the stream item.
    pub receive_batch_id: u64,
    /// Receipt Access Event ID.
    pub event_id: String,
    /// Receipt ID.
    pub receipt_id: String,
    /// Payment Reference copied from Receipt Access.
    pub payment_reference: String,
    /// Optional Payment Request ID copied from Receipt Access.
    pub payment_request_id: Option<String>,
    /// Optional Billing Period copied from Receipt Access.
    pub billing_period: Option<ReceiptBillingPeriodRecord>,
    /// Receipt Location path on the issuer homeserver.
    pub location: String,
    /// Receipt Decryption Key material. Treat as secret storage data.
    pub key: String,
    /// Receive time of the indexed stream item.
    pub received_at: DateTime<Utc>,
}

impl ReceiptAccessRecord {
    pub(crate) fn from_access(
        counterparty: PubkyPublicKey,
        stream_item_id: u64,
        receive_batch_id: u64,
        received_at: DateTime<Utc>,
        access: &ReceiptAccess,
    ) -> Self {
        Self {
            counterparty,
            stream_item_id,
            receive_batch_id,
            event_id: access.event_id.as_str().to_owned(),
            receipt_id: access.receipt_id.as_str().to_owned(),
            payment_reference: access.payment_reference.as_str().to_owned(),
            payment_request_id: access
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            billing_period: access.billing_period.as_ref().map(|period| {
                ReceiptBillingPeriodRecord {
                    starts_at: period.starts_at.clone(),
                    ends_at: period.ends_at.clone(),
                }
            }),
            location: access.location.clone(),
            key: access.key.as_str().to_owned(),
            received_at,
        }
    }
}

impl fmt::Debug for ReceiptAccessRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptAccessRecord")
            .field("counterparty", &self.counterparty)
            .field("stream_item_id", &self.stream_item_id)
            .field("receive_batch_id", &self.receive_batch_id)
            .field("event_id", &self.event_id)
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &self.payment_reference)
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field("location", &self.location)
            .field("key", &"<redacted>")
            .field("received_at", &self.received_at)
            .finish()
    }
}

/// List indexed Receipt Access records for one counterparty.
pub async fn receipt_access_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<ReceiptAccessRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_access_records(counterparty)))
        .await
}

/// Load the latest indexed Receipt Access record for a receipt.
pub async fn receipt_access_record_by_receipt_id<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    receipt_id: &str,
) -> Result<Option<ReceiptAccessRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_access_record_by_receipt_id(counterparty, receipt_id)))
        .await
}
