use std::sync::Arc;

use paykit_lib::ReceiptId;

use crate::{
    payment_requests::{FfiBillingPeriod, FfiPaymentReference},
    sdk::FfiPaykitSdk,
    PaykitFfiError,
};

mod conversions;
#[cfg(test)]
mod tests;

use conversions::{
    parse_public_key, receipt_access_views_to_ffi, receipt_issuance_views_to_ffi,
    receipt_records_to_ffi,
};

/// Payment Amount fields copied into receipts.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiReceiptAmount {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

/// Caller-provided receipt fields.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiReceiptDraft {
    /// Optional caller-stable Receipt ID.
    pub receipt_id: Option<String>,
    /// Payment Reference being receipted.
    pub payment_reference: Arc<FfiPaymentReference>,
    /// Optional Payment Request ID this receipt corresponds to.
    pub payment_request_id: Option<String>,
    /// Optional Billing Period for recurring Payment Request receipts.
    pub billing_period: Option<FfiBillingPeriod>,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<String>,
    /// Optional Payment Amount being receipted.
    pub amount: Option<FfiReceiptAmount>,
    /// Caller-defined Receipt Metadata encoded as a JSON object.
    pub metadata_json: String,
}

/// Local receipt issuance state.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiReceiptIssuanceStatus {
    /// Encrypted Receipt has not been stored yet.
    PendingStorage,
    /// Encrypted Receipt was stored, but Receipt Access has not been queued yet.
    Stored,
    /// Receipt Access was queued for private delivery.
    AccessQueued,
    /// Last storage or queueing attempt failed.
    Failed,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Receipt retrieval state for an indexed Receipt Access event.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiReceiptRetrievalStatus {
    /// Receipt Access has been indexed, but retrieval has not succeeded yet.
    Pending,
    /// Encrypted Receipt was fetched and decrypted.
    Retrieved,
    /// Receipt Location was missing on the issuer homeserver.
    NotFound,
    /// Retrieval or decryption failed.
    Failed,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// App-facing view of local receipt issuance progress.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiReceiptIssuanceView {
    /// Counterparty that should receive Receipt Access.
    pub counterparty: String,
    /// Receipt ID.
    pub receipt_id: String,
    /// Receipt Access Event ID.
    pub receipt_access_event_id: String,
    /// Payment Reference copied from the Receipt.
    pub payment_reference: Arc<FfiPaymentReference>,
    /// Optional Payment Request ID copied from the Receipt.
    pub payment_request_id: Option<String>,
    /// Optional Billing Period copied from the Receipt.
    pub billing_period: Option<FfiBillingPeriod>,
    /// Optional Payment Endpoint Identifier copied from the Receipt.
    pub payment_endpoint_identifier: Option<String>,
    /// Optional Payment Amount copied from the Receipt.
    pub amount: Option<FfiReceiptAmount>,
    /// Current issuance status.
    pub status: FfiReceiptIssuanceStatus,
    /// Outbound private message id that carries Receipt Access, once queued.
    pub outbound_message_id: Option<u64>,
    /// Creation time as RFC3339 text.
    pub created_at: String,
    /// Last status update time as RFC3339 text.
    pub updated_at: String,
    /// Time the Encrypted Receipt was stored as RFC3339 text.
    pub stored_at: Option<String>,
    /// Time Receipt Access was queued for private delivery as RFC3339 text.
    pub access_queued_at: Option<String>,
}

/// App-facing view of an indexed Receipt Access event.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiReceiptAccessView {
    /// Counterparty that sent the Receipt Access event.
    pub counterparty: String,
    /// Receipt Access Event ID.
    pub event_id: String,
    /// Receipt ID.
    pub receipt_id: String,
    /// Payment Reference copied from Receipt Access.
    pub payment_reference: Arc<FfiPaymentReference>,
    /// Optional Payment Request ID copied from Receipt Access.
    pub payment_request_id: Option<String>,
    /// Optional Billing Period copied from Receipt Access.
    pub billing_period: Option<FfiBillingPeriod>,
    /// Current retrieval state for the referenced receipt.
    pub retrieval_status: FfiReceiptRetrievalStatus,
    /// Last retrieval attempt time as RFC3339 text.
    pub retrieval_attempted_at: Option<String>,
    /// Successful retrieval/decryption time as RFC3339 text.
    pub retrieved_at: Option<String>,
    /// Receive time of the indexed stream item as RFC3339 text.
    pub received_at: String,
}

/// Decrypted Receipt record stored by the SDK.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiReceiptRecord {
    /// Counterparty that issued the Receipt Access event.
    pub issuer: String,
    /// Receipt Access Event ID used for retrieval.
    pub receipt_access_event_id: String,
    /// Receipt ID.
    pub receipt_id: String,
    /// Payment Reference copied from the decrypted Receipt.
    pub payment_reference: Arc<FfiPaymentReference>,
    /// Optional Payment Request ID copied from the decrypted Receipt.
    pub payment_request_id: Option<String>,
    /// Optional Billing Period copied from the decrypted Receipt.
    pub billing_period: Option<FfiBillingPeriod>,
    /// Recipient public key from the decrypted Receipt.
    pub recipient_public_key: String,
    /// Optional Payment Endpoint Identifier copied from the decrypted Receipt.
    pub payment_endpoint_identifier: Option<String>,
    /// Optional Payment Amount copied from the decrypted Receipt.
    pub amount: Option<FfiReceiptAmount>,
    /// Caller-defined Receipt Metadata encoded as a JSON object.
    pub metadata_json: String,
    /// Successful retrieval/decryption time as RFC3339 text.
    pub retrieved_at: String,
}

/// Generate a fresh Receipt ID.
#[uniffi::export]
pub fn generate_receipt_id() -> String {
    ReceiptId::new_v4().to_string()
}

#[uniffi::export]
impl FfiPaykitSdk {
    /// Prepare a receipt issuance and persist it before network side effects.
    pub async fn prepare_receipt_issuance(
        &self,
        counterparty: String,
        draft: FfiReceiptDraft,
    ) -> Result<FfiReceiptIssuanceView, PaykitFfiError> {
        self.runtime
            .prepare_receipt_issuance(parse_public_key(counterparty)?, draft.try_into()?)
            .await
            .map_err(Into::into)
            .map(Into::into)
    }

    /// Prepare, store, and queue Receipt Access for private delivery.
    pub async fn issue_receipt(
        &self,
        counterparty: String,
        draft: FfiReceiptDraft,
    ) -> Result<FfiReceiptIssuanceView, PaykitFfiError> {
        self.runtime
            .issue_receipt(parse_public_key(counterparty)?, draft.try_into()?)
            .await
            .map_err(Into::into)
            .map(Into::into)
    }

    /// Continue storage and Receipt Access queueing for a prepared issuance.
    pub async fn process_receipt_issuance(
        &self,
        counterparty: String,
        receipt_id: String,
    ) -> Result<FfiReceiptIssuanceView, PaykitFfiError> {
        self.runtime
            .process_receipt_issuance(parse_public_key(counterparty)?, &receipt_id)
            .await
            .map_err(Into::into)
            .map(Into::into)
    }

    /// List local receipt issuance records for one counterparty.
    pub async fn receipt_issuance_records(
        &self,
        counterparty: String,
    ) -> Result<Vec<FfiReceiptIssuanceView>, PaykitFfiError> {
        let records = self
            .runtime
            .receipt_issuance_records(&parse_public_key(counterparty)?)
            .await?;
        receipt_issuance_views_to_ffi(records)
    }

    /// List issued receipts for one counterparty, newest first.
    pub async fn issued_receipts_to(
        &self,
        counterparty: String,
    ) -> Result<Vec<FfiReceiptIssuanceView>, PaykitFfiError> {
        let records = self
            .runtime
            .issued_receipts_to(&parse_public_key(counterparty)?)
            .await?;
        receipt_issuance_views_to_ffi(records)
    }

    /// List issued receipts across non-blocked counterparties, newest first.
    pub async fn issued_receipts(&self) -> Result<Vec<FfiReceiptIssuanceView>, PaykitFfiError> {
        let records = self.runtime.issued_receipts().await?;
        receipt_issuance_views_to_ffi(records)
    }

    /// Fetch, decrypt, and store a receipt from an indexed Receipt Access event.
    pub async fn retrieve_receipt(
        &self,
        counterparty: String,
        receipt_id: String,
    ) -> Result<FfiReceiptRecord, PaykitFfiError> {
        self.runtime
            .retrieve_receipt(parse_public_key(counterparty)?, &receipt_id)
            .await
            .map_err(Into::into)
            .and_then(FfiReceiptRecord::try_from)
    }

    /// List indexed Receipt Access records for one counterparty.
    pub async fn receipt_access_records(
        &self,
        counterparty: String,
    ) -> Result<Vec<FfiReceiptAccessView>, PaykitFfiError> {
        let records = self
            .runtime
            .receipt_access_records(&parse_public_key(counterparty)?)
            .await?;
        receipt_access_views_to_ffi(records)
    }

    /// List Receipt Access received from one counterparty.
    pub async fn receipt_access_from(
        &self,
        counterparty: String,
    ) -> Result<Vec<FfiReceiptAccessView>, PaykitFfiError> {
        let records = self
            .runtime
            .receipt_access_from(&parse_public_key(counterparty)?)
            .await?;
        receipt_access_views_to_ffi(records)
    }

    /// List Receipt Access across non-blocked counterparties, newest first.
    pub async fn receipt_access(&self) -> Result<Vec<FfiReceiptAccessView>, PaykitFfiError> {
        let records = self.runtime.receipt_access().await?;
        receipt_access_views_to_ffi(records)
    }

    /// List decrypted Receipt records for one issuer, newest first.
    pub async fn receipt_records(
        &self,
        issuer: String,
    ) -> Result<Vec<FfiReceiptRecord>, PaykitFfiError> {
        let records = self
            .runtime
            .receipt_records(&parse_public_key(issuer)?)
            .await?;
        receipt_records_to_ffi(records)
    }

    /// List decrypted receipts from one issuer, newest first.
    pub async fn receipts_from(
        &self,
        issuer: String,
    ) -> Result<Vec<FfiReceiptRecord>, PaykitFfiError> {
        let records = self
            .runtime
            .receipts_from(&parse_public_key(issuer)?)
            .await?;
        receipt_records_to_ffi(records)
    }

    /// List decrypted receipts across non-blocked issuers, newest first.
    pub async fn receipts(&self) -> Result<Vec<FfiReceiptRecord>, PaykitFfiError> {
        let records = self.runtime.receipts().await?;
        receipt_records_to_ffi(records)
    }
}
