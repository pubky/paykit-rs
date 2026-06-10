//! Receipt Access indexing helpers.
//!
//! Indexed Receipt Access and receipt issuance records include Receipt
//! Decryption Keys or exact Receipt Access JSON. Store them as private SDK
//! state and avoid logging field values directly.

use std::fmt;

use chrono::{DateTime, Utc};
use paykit_lib::{
    serialize_receipt_access_json, PreparedReceipt, Receipt, ReceiptAccess, ReceiptDecryptionKey,
    ReceiptDraft,
};
use pubky::{errors::RequestError, Error as PubkyError, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};

use crate::{
    outbound_private::validate_outbound_private_message,
    storage::{NewOutboundPrivateMessage, StorageAdapter},
    PaykitSdkError, PubkyPublicKey, Result,
};

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

/// Local receipt issuance state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ReceiptIssuanceStatus {
    /// Receipt was prepared locally, but the Encrypted Receipt has not been
    /// stored on the issuer homeserver yet.
    #[default]
    PendingStorage,
    /// Encrypted Receipt was stored, but Receipt Access has not been queued yet.
    Stored,
    /// Receipt Access was queued for private delivery.
    AccessQueued,
    /// Last storage or queueing attempt failed.
    Failed,
}

/// Durable local state for issuing one receipt.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptIssuanceRecord {
    /// Counterparty that should receive Receipt Access.
    pub counterparty: PubkyPublicKey,
    /// Receipt ID.
    pub receipt_id: String,
    /// Receipt Access Event ID.
    pub receipt_access_event_id: String,
    /// Payment Reference copied from the Receipt.
    pub payment_reference: String,
    /// Optional Payment Request ID copied from the Receipt.
    pub payment_request_id: Option<String>,
    /// Optional Billing Period copied from the Receipt.
    pub billing_period: Option<ReceiptBillingPeriodRecord>,
    /// Optional Payment Endpoint Identifier copied from the Receipt.
    pub payment_endpoint_identifier: Option<String>,
    /// Optional Payment Amount copied from the Receipt.
    pub amount: Option<ReceiptAmountRecord>,
    /// Receipt Location path on the issuer homeserver.
    pub location: String,
    /// Encrypted Receipt JSON to store at the Receipt Location.
    pub encrypted_receipt: String,
    /// Exact Receipt Access JSON to queue for private delivery.
    pub access_json: String,
    /// Current issuance status.
    pub status: ReceiptIssuanceStatus,
    /// Outbound private message id that carries Receipt Access, once queued.
    pub outbound_message_id: Option<u64>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last status update time.
    pub updated_at: DateTime<Utc>,
    /// Time the Encrypted Receipt was stored.
    pub stored_at: Option<DateTime<Utc>>,
    /// Time Receipt Access was queued for private delivery.
    pub access_queued_at: Option<DateTime<Utc>>,
    /// Last storage or queueing error, when available.
    pub last_error: Option<String>,
}

impl ReceiptIssuanceRecord {
    pub(crate) fn from_prepared(
        counterparty: PubkyPublicKey,
        prepared: PreparedReceipt,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        let access_json = serialize_receipt_access_json(&prepared.access)?;
        Ok(Self {
            counterparty,
            receipt_id: prepared.receipt.receipt_id.as_str().to_owned(),
            receipt_access_event_id: prepared.access.event_id.as_str().to_owned(),
            payment_reference: prepared.receipt.payment_reference.as_str().to_owned(),
            payment_request_id: prepared
                .receipt
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            billing_period: prepared
                .receipt
                .billing_period
                .as_ref()
                .map(ReceiptBillingPeriodRecord::from),
            payment_endpoint_identifier: prepared
                .receipt
                .payment_endpoint_identifier
                .as_ref()
                .map(|identifier| identifier.as_str().to_owned()),
            amount: prepared
                .receipt
                .amount
                .as_ref()
                .map(|amount| ReceiptAmountRecord {
                    value: amount.value.clone(),
                    asset: amount.asset.clone(),
                }),
            location: prepared.access.location.clone(),
            encrypted_receipt: prepared.encrypted_receipt,
            access_json,
            status: ReceiptIssuanceStatus::PendingStorage,
            outbound_message_id: None,
            created_at: now,
            updated_at: now,
            stored_at: None,
            access_queued_at: None,
            last_error: None,
        })
    }

    pub(crate) fn mark_stored(&self, stored_at: DateTime<Utc>) -> Self {
        let mut record = self.clone();
        record.status = ReceiptIssuanceStatus::Stored;
        record.updated_at = stored_at;
        record.stored_at = Some(stored_at);
        record.last_error = None;
        record
    }

    pub(crate) fn mark_access_queued(
        &self,
        outbound_message_id: u64,
        queued_at: DateTime<Utc>,
    ) -> Self {
        let mut record = self.clone();
        record.status = ReceiptIssuanceStatus::AccessQueued;
        record.updated_at = queued_at;
        record.outbound_message_id = Some(outbound_message_id);
        record.access_queued_at = Some(queued_at);
        record.last_error = None;
        record
    }

    pub(crate) fn mark_failed(&self, failed_at: DateTime<Utc>, error: String) -> Self {
        let mut record = self.clone();
        record.status = ReceiptIssuanceStatus::Failed;
        record.updated_at = failed_at;
        record.last_error = Some(error);
        record
    }
}

impl fmt::Debug for ReceiptIssuanceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptIssuanceRecord")
            .field("counterparty", &self.counterparty)
            .field("receipt_id", &self.receipt_id)
            .field("receipt_access_event_id", &self.receipt_access_event_id)
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("amount", &self.amount.as_ref().map(|_| "<redacted>"))
            .field("location", &"<redacted>")
            .field(
                "encrypted_receipt",
                &format_args!("<redacted:{} bytes>", self.encrypted_receipt.len()),
            )
            .field(
                "access_json",
                &format_args!("<redacted:{} bytes>", self.access_json.len()),
            )
            .field("status", &self.status)
            .field("outbound_message_id", &self.outbound_message_id)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("stored_at", &self.stored_at)
            .field("access_queued_at", &self.access_queued_at)
            .field(
                "last_error",
                &self.last_error.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// App-facing view of local receipt issuance progress.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptIssuanceView {
    /// Counterparty that should receive Receipt Access.
    pub counterparty: PubkyPublicKey,
    /// Receipt ID.
    pub receipt_id: String,
    /// Receipt Access Event ID.
    pub receipt_access_event_id: String,
    /// Payment Reference copied from the Receipt.
    pub payment_reference: String,
    /// Optional Payment Request ID copied from the Receipt.
    pub payment_request_id: Option<String>,
    /// Optional Billing Period copied from the Receipt.
    pub billing_period: Option<ReceiptBillingPeriodRecord>,
    /// Optional Payment Endpoint Identifier copied from the Receipt.
    pub payment_endpoint_identifier: Option<String>,
    /// Optional Payment Amount copied from the Receipt.
    pub amount: Option<ReceiptAmountRecord>,
    /// Current issuance status.
    pub status: ReceiptIssuanceStatus,
    /// Outbound private message id that carries Receipt Access, once queued.
    pub outbound_message_id: Option<u64>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last status update time.
    pub updated_at: DateTime<Utc>,
    /// Time the Encrypted Receipt was stored.
    pub stored_at: Option<DateTime<Utc>>,
    /// Time Receipt Access was queued for private delivery.
    pub access_queued_at: Option<DateTime<Utc>>,
}

impl fmt::Debug for ReceiptIssuanceView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptIssuanceView")
            .field("counterparty", &self.counterparty)
            .field("receipt_id", &self.receipt_id)
            .field("receipt_access_event_id", &self.receipt_access_event_id)
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("amount", &self.amount.as_ref().map(|_| "<redacted>"))
            .field("status", &self.status)
            .field("outbound_message_id", &self.outbound_message_id)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("stored_at", &self.stored_at)
            .field("access_queued_at", &self.access_queued_at)
            .finish()
    }
}

impl From<&ReceiptIssuanceRecord> for ReceiptIssuanceView {
    fn from(record: &ReceiptIssuanceRecord) -> Self {
        Self {
            counterparty: record.counterparty.clone(),
            receipt_id: record.receipt_id.clone(),
            receipt_access_event_id: record.receipt_access_event_id.clone(),
            payment_reference: record.payment_reference.clone(),
            payment_request_id: record.payment_request_id.clone(),
            billing_period: record.billing_period.clone(),
            payment_endpoint_identifier: record.payment_endpoint_identifier.clone(),
            amount: record.amount.clone(),
            status: record.status,
            outbound_message_id: record.outbound_message_id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            stored_at: record.stored_at,
            access_queued_at: record.access_queued_at,
        }
    }
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

pub(crate) async fn store_encrypted_receipt_json(
    session: &pubky::PubkySession,
    record: &ReceiptIssuanceRecord,
) -> Result<()> {
    session
        .storage()
        .put(record.location.clone(), record.encrypted_receipt.clone())
        .await
        .map(|_| ())
        .map_err(|err| PaykitSdkError::Transport {
            context: format!("failed to store encrypted receipt at {}", record.location),
            source: Some(err.into()),
        })
}

pub(crate) fn receipt_issuance_record_matches_draft(
    record: &ReceiptIssuanceRecord,
    draft: &ReceiptDraft,
) -> Result<bool> {
    let access = paykit_lib::parse_receipt_access_json(&record.access_json)?;
    let receipt =
        paykit_lib::decrypt_receipt(&record.encrypted_receipt, &access.key, &access.location)?;
    let expected_receipt_id = draft.receipt_id.as_ref().map(|id| id.as_str());
    let expected_recipient = record.counterparty.to_public_key()?;
    Ok(Some(receipt.receipt_id.as_str()) == expected_receipt_id
        && receipt.payment_reference == draft.payment_reference
        && receipt.payment_request_id == draft.payment_request_id
        && receipt.billing_period == draft.billing_period
        && receipt.recipient_public_key == expected_recipient
        && receipt.payment_endpoint_identifier == draft.payment_endpoint_identifier
        && receipt.amount == draft.amount
        && receipt.metadata == draft.metadata)
}

pub(crate) async fn receipt_issuance_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<ReceiptIssuanceRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_issuance_records(counterparty)))
        .await
}

pub(crate) async fn receipt_issuance_record<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    receipt_id: &str,
) -> Result<Option<ReceiptIssuanceRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_issuance_record(counterparty, receipt_id)))
        .await
}

pub(crate) async fn receipt_issuance_record_by_receipt_id<S>(
    storage: &S,
    receipt_id: &str,
) -> Result<Option<ReceiptIssuanceRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.receipt_issuance_record_by_receipt_id(receipt_id)))
        .await
}

pub(crate) async fn enqueue_receipt_access_for_issuance<S>(
    storage: &S,
    record: ReceiptIssuanceRecord,
    now: DateTime<Utc>,
) -> Result<ReceiptIssuanceRecord>
where
    S: StorageAdapter,
{
    if record.outbound_message_id.is_some() {
        return Ok(record);
    }
    let kind = validate_outbound_private_message(&record.access_json)?;
    if kind != paykit_lib::PrivateMessageKind::ReceiptAccess.as_str() {
        return Err(PaykitSdkError::Protocol(
            "receipt issuance access JSON is not Receipt Access".into(),
        ));
    }
    storage
        .transaction(move |tx| {
            let outbound = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                record.counterparty.clone(),
                kind,
                record.access_json.clone(),
                now,
            ));
            let queued = record.mark_access_queued(outbound.outbound_message_id, now);
            tx.save_receipt_issuance_record(queued.clone());
            Ok(queued)
        })
        .await
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
