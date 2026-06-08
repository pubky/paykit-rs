//! Receipt Access indexing helpers.
//!
//! Indexed Receipt Access records include Receipt Decryption Keys. Store them
//! as private SDK state and avoid logging field values directly.

use std::fmt;

use chrono::{DateTime, Utc};
use paykit_lib::{Receipt, ReceiptAccess, ReceiptDecryptionKey};
use pubky::{errors::RequestError, Error as PubkyError, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};

use crate::{PaykitSdkError, PubkyPublicKey, Result};

#[cfg(test)]
use crate::storage::StorageAdapter;

/// Durable Billing Period fields copied from a Receipt Access event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptBillingPeriodRecord {
    /// RFC3339 UTC timestamp using `Z`.
    pub starts_at: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub ends_at: String,
}

impl From<&paykit_lib::BillingPeriod> for ReceiptBillingPeriodRecord {
    fn from(period: &paykit_lib::BillingPeriod) -> Self {
        Self {
            starts_at: period.starts_at.clone(),
            ends_at: period.ends_at.clone(),
        }
    }
}

/// Receipt retrieval state for an indexed Receipt Access event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ReceiptRetrievalStatus {
    /// Receipt Access has been indexed, but retrieval has not succeeded yet.
    #[default]
    Pending,
    /// Encrypted Receipt was fetched and decrypted.
    Retrieved,
    /// Receipt Location was missing on the issuer homeserver.
    NotFound,
    /// Retrieval or decryption failed.
    Failed,
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
    /// Current retrieval state for the referenced receipt.
    #[serde(default)]
    pub retrieval_status: ReceiptRetrievalStatus,
    /// Last retrieval attempt time.
    #[serde(default)]
    pub retrieval_attempted_at: Option<DateTime<Utc>>,
    /// Successful retrieval/decryption time.
    #[serde(default)]
    pub retrieved_at: Option<DateTime<Utc>>,
    /// Last retrieval/decryption error, when available.
    #[serde(default)]
    pub last_retrieval_error: Option<String>,
    /// Receive time of the indexed stream item.
    pub received_at: DateTime<Utc>,
}

/// App-facing view of an indexed Receipt Access event.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptAccessView {
    /// Counterparty that sent the Receipt Access event.
    pub counterparty: PubkyPublicKey,
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
    /// Current retrieval state for the referenced receipt.
    pub retrieval_status: ReceiptRetrievalStatus,
    /// Last retrieval attempt time.
    pub retrieval_attempted_at: Option<DateTime<Utc>>,
    /// Successful retrieval/decryption time.
    pub retrieved_at: Option<DateTime<Utc>>,
    /// Receive time of the indexed stream item.
    pub received_at: DateTime<Utc>,
}

impl fmt::Debug for ReceiptAccessView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptAccessView")
            .field("counterparty", &self.counterparty)
            .field("event_id", &self.event_id)
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field("retrieval_status", &self.retrieval_status)
            .field("retrieval_attempted_at", &self.retrieval_attempted_at)
            .field("retrieved_at", &self.retrieved_at)
            .field("received_at", &self.received_at)
            .finish()
    }
}

impl From<&ReceiptAccessRecord> for ReceiptAccessView {
    fn from(record: &ReceiptAccessRecord) -> Self {
        Self {
            counterparty: record.counterparty.clone(),
            event_id: record.event_id.clone(),
            receipt_id: record.receipt_id.clone(),
            payment_reference: record.payment_reference.clone(),
            payment_request_id: record.payment_request_id.clone(),
            billing_period: record.billing_period.clone(),
            retrieval_status: record.retrieval_status,
            retrieval_attempted_at: record.retrieval_attempted_at,
            retrieved_at: record.retrieved_at,
            received_at: record.received_at,
        }
    }
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
            billing_period: access
                .billing_period
                .as_ref()
                .map(ReceiptBillingPeriodRecord::from),
            location: access.location.clone(),
            key: access.key.as_str().to_owned(),
            retrieval_status: ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at,
        }
    }

    pub(crate) fn mark_retrieved(&self, retrieved_at: DateTime<Utc>) -> Self {
        let mut record = self.clone();
        record.retrieval_status = ReceiptRetrievalStatus::Retrieved;
        record.retrieval_attempted_at = Some(retrieved_at);
        record.retrieved_at = Some(retrieved_at);
        record.last_retrieval_error = None;
        record
    }

    pub(crate) fn mark_retrieval_error(
        &self,
        status: ReceiptRetrievalStatus,
        attempted_at: DateTime<Utc>,
        error: String,
    ) -> Self {
        let mut record = self.clone();
        record.retrieval_status = status;
        record.retrieval_attempted_at = Some(attempted_at);
        record.retrieved_at = None;
        record.last_retrieval_error = Some(error);
        record
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
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field("location", &"<redacted>")
            .field("key", &"<redacted>")
            .field("retrieval_status", &self.retrieval_status)
            .field("retrieval_attempted_at", &self.retrieval_attempted_at)
            .field("retrieved_at", &self.retrieved_at)
            .field(
                "last_retrieval_error",
                &self.last_retrieval_error.as_ref().map(|_| "<redacted>"),
            )
            .field("received_at", &self.received_at)
            .finish()
    }
}

/// Durable Payment Amount fields copied from a decrypted Receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptAmountRecord {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

/// Decrypted Receipt record stored by the SDK.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRecord {
    /// Counterparty that issued the Receipt Access event.
    pub issuer: PubkyPublicKey,
    /// Receipt Access Event ID used for retrieval.
    pub receipt_access_event_id: String,
    /// Hash of the Receipt Access key that decrypted the Receipt.
    #[serde(default)]
    pub receipt_access_key_hash: String,
    /// Receipt ID.
    pub receipt_id: String,
    /// Payment Reference copied from the decrypted Receipt.
    pub payment_reference: String,
    /// Optional Payment Request ID copied from the decrypted Receipt.
    pub payment_request_id: Option<String>,
    /// Optional Billing Period copied from the decrypted Receipt.
    pub billing_period: Option<ReceiptBillingPeriodRecord>,
    /// Recipient public key from the decrypted Receipt.
    pub recipient_public_key: PubkyPublicKey,
    /// Optional Payment Endpoint Identifier copied from the decrypted Receipt.
    pub payment_endpoint_identifier: Option<String>,
    /// Optional Payment Amount copied from the decrypted Receipt.
    pub amount: Option<ReceiptAmountRecord>,
    /// Caller-defined Receipt Metadata.
    pub metadata: JsonMap<String, JsonValue>,
    /// Receipt Location path used for retrieval.
    pub location: String,
    /// Successful retrieval/decryption time.
    pub retrieved_at: DateTime<Utc>,
}

impl ReceiptRecord {
    fn from_receipt(
        issuer: PubkyPublicKey,
        access: &ReceiptAccessRecord,
        receipt: Receipt,
        retrieved_at: DateTime<Utc>,
    ) -> Self {
        Self {
            issuer,
            receipt_access_event_id: access.event_id.clone(),
            receipt_access_key_hash: receipt_access_key_hash(&access.key),
            receipt_id: receipt.receipt_id.as_str().to_owned(),
            payment_reference: receipt.payment_reference.as_str().to_owned(),
            payment_request_id: receipt
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            billing_period: receipt
                .billing_period
                .as_ref()
                .map(ReceiptBillingPeriodRecord::from),
            recipient_public_key: PubkyPublicKey::from_public_key(&receipt.recipient_public_key),
            payment_endpoint_identifier: receipt
                .payment_endpoint_identifier
                .as_ref()
                .map(|identifier| identifier.as_str().to_owned()),
            amount: receipt.amount.as_ref().map(|amount| ReceiptAmountRecord {
                value: amount.value.clone(),
                asset: amount.asset.clone(),
            }),
            metadata: receipt.metadata,
            location: access.location.clone(),
            retrieved_at,
        }
    }
}

impl fmt::Debug for ReceiptRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptRecord")
            .field("issuer", &self.issuer)
            .field("receipt_access_event_id", &self.receipt_access_event_id)
            .field("receipt_access_key_hash", &self.receipt_access_key_hash)
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field("recipient_public_key", &self.recipient_public_key)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("amount", &self.amount.as_ref().map(|_| "<redacted>"))
            .field(
                "metadata",
                &format_args!("<redacted:{} fields>", self.metadata.len()),
            )
            .field("location", &"<redacted>")
            .field("retrieved_at", &self.retrieved_at)
            .finish()
    }
}

/// List indexed Receipt Access records for one counterparty.
#[cfg(test)]
pub(crate) async fn receipt_access_records<S>(
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
#[cfg(test)]
pub(crate) async fn receipt_access_record_by_receipt_id<S>(
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

pub(crate) async fn fetch_encrypted_receipt_json(
    public_storage: &pubky::PublicStorage,
    issuer: &PubkyPublicKey,
    location: &str,
) -> Result<Option<String>> {
    let addr = format!("{}{}", issuer.to_public_key()?, location);
    match public_storage.get(addr).await {
        Ok(response) => {
            let bytes = response
                .bytes()
                .await
                .map_err(|err| PaykitSdkError::Transport {
                    context: "read encrypted receipt bytes".into(),
                    source: Some(err.into()),
                })?;
            let json = String::from_utf8(bytes.to_vec()).map_err(|err| {
                PaykitSdkError::Protocol(format!("encrypted receipt is not UTF-8: {err}"))
            })?;
            Ok(Some(json))
        }
        Err(err) if is_not_found(&err) => Ok(None),
        Err(err) => Err(PaykitSdkError::Transport {
            context: "fetch encrypted receipt".into(),
            source: Some(err.into()),
        }),
    }
}

pub(crate) fn decrypt_receipt_record_from_access(
    access: &ReceiptAccessRecord,
    encrypted_json: &str,
    retrieved_at: DateTime<Utc>,
    expected_recipient: &PubkyPublicKey,
) -> Result<ReceiptRecord> {
    let key = ReceiptDecryptionKey::new(access.key.clone())?;
    let receipt = paykit_lib::decrypt_receipt(encrypted_json, &key, &access.location)?;
    validate_receipt_matches_access(access, &receipt, expected_recipient)?;
    Ok(ReceiptRecord::from_receipt(
        access.counterparty.clone(),
        access,
        receipt,
        retrieved_at,
    ))
}

pub(crate) fn receipt_record_matches_access(
    record: &ReceiptRecord,
    access: &ReceiptAccessRecord,
) -> bool {
    record.issuer == access.counterparty
        && record.receipt_access_key_hash == receipt_access_key_hash(&access.key)
        && record.receipt_id == access.receipt_id
        && record.payment_reference == access.payment_reference
        && record.payment_request_id == access.payment_request_id
        && record.billing_period == access.billing_period
        && record.location == access.location
}

pub(crate) fn receipt_access_key_hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("sha256:{digest:x}")
}

fn validate_receipt_matches_access(
    access: &ReceiptAccessRecord,
    receipt: &Receipt,
    expected_recipient: &PubkyPublicKey,
) -> Result<()> {
    if receipt.receipt_id.as_str() != access.receipt_id {
        return Err(PaykitSdkError::Protocol(
            "decrypted Receipt ID does not match Receipt Access".into(),
        ));
    }
    let recipient = PubkyPublicKey::from_public_key(&receipt.recipient_public_key);
    if &recipient != expected_recipient {
        return Err(PaykitSdkError::Protocol(
            "decrypted Receipt recipient does not match local identity".into(),
        ));
    }
    if receipt.payment_reference.as_str() != access.payment_reference {
        return Err(PaykitSdkError::Protocol(
            "decrypted Receipt Payment Reference does not match Receipt Access".into(),
        ));
    }
    let receipt_payment_request_id = receipt
        .payment_request_id
        .as_ref()
        .map(|id| id.as_str().to_owned());
    if receipt_payment_request_id != access.payment_request_id {
        return Err(PaykitSdkError::Protocol(
            "decrypted Receipt Payment Request ID does not match Receipt Access".into(),
        ));
    }
    let receipt_billing_period = receipt
        .billing_period
        .as_ref()
        .map(ReceiptBillingPeriodRecord::from);
    if receipt_billing_period != access.billing_period {
        return Err(PaykitSdkError::Protocol(
            "decrypted Receipt Billing Period does not match Receipt Access".into(),
        ));
    }
    Ok(())
}

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}

#[cfg(test)]
mod tests;
