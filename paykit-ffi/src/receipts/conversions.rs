use std::sync::Arc;

use paykit_lib::{
    PaymentAmount, PaymentEndpointIdentifier, PaymentRequestId, ReceiptDraft, ReceiptId,
};
use paykit_sdk::{
    AmountRecord, ReceiptAccessView, ReceiptDraftBuilder, ReceiptIssuanceStatus,
    ReceiptIssuanceView, ReceiptRecord, ReceiptRetrievalStatus,
};

use crate::{
    errors::validation_error, json::FfiPrivateJsonObject, payment_requests::FfiPaymentReference,
    PaykitFfiError,
};

use super::{
    FfiReceiptAccessView, FfiReceiptAmount, FfiReceiptDraft, FfiReceiptIssuanceStatus,
    FfiReceiptIssuanceView, FfiReceiptRecord, FfiReceiptRetrievalStatus,
};

impl From<ReceiptIssuanceStatus> for FfiReceiptIssuanceStatus {
    fn from(value: ReceiptIssuanceStatus) -> Self {
        match value {
            ReceiptIssuanceStatus::PendingStorage => Self::PendingStorage,
            ReceiptIssuanceStatus::Stored => Self::Stored,
            ReceiptIssuanceStatus::AccessQueued => Self::AccessQueued,
            ReceiptIssuanceStatus::Failed => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

impl From<ReceiptRetrievalStatus> for FfiReceiptRetrievalStatus {
    fn from(value: ReceiptRetrievalStatus) -> Self {
        match value {
            ReceiptRetrievalStatus::Pending => Self::Pending,
            ReceiptRetrievalStatus::Retrieved => Self::Retrieved,
            ReceiptRetrievalStatus::NotFound => Self::NotFound,
            ReceiptRetrievalStatus::Failed => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

impl From<AmountRecord> for FfiReceiptAmount {
    fn from(value: AmountRecord) -> Self {
        Self {
            value: value.value,
            asset: value.asset,
        }
    }
}

impl TryFrom<FfiReceiptAmount> for PaymentAmount {
    type Error = PaykitFfiError;

    fn try_from(value: FfiReceiptAmount) -> Result<Self, Self::Error> {
        Self::new(value.value, value.asset).map_err(|err| validation_error(err.to_string()))
    }
}

impl TryFrom<FfiReceiptDraft> for ReceiptDraft {
    type Error = PaykitFfiError;

    fn try_from(value: FfiReceiptDraft) -> Result<Self, Self::Error> {
        let mut builder = ReceiptDraftBuilder::new(value.payment_reference.export_text())
            .map_err(PaykitFfiError::from)?;
        if let Some(receipt_id) = value.receipt_id {
            builder = builder.with_receipt_id(parse_receipt_id(receipt_id)?);
        }
        if let Some(payment_request_id) = value.payment_request_id {
            builder =
                builder.with_payment_request_id(parse_payment_request_id(payment_request_id)?);
        }
        if let Some(billing_period) = value.billing_period {
            builder = builder.with_billing_period(billing_period.try_into()?);
        }
        if let Some(identifier) = value.payment_endpoint_identifier {
            builder =
                builder.with_payment_endpoint_identifier(parse_endpoint_identifier(identifier)?);
        }
        if let Some(amount) = value.amount {
            builder = builder.with_amount(amount.try_into()?);
        }
        builder = builder.with_metadata(value.metadata.parse_map("Receipt Draft metadata")?);
        builder.build().map_err(Into::into)
    }
}

impl From<ReceiptIssuanceView> for FfiReceiptIssuanceView {
    fn from(value: ReceiptIssuanceView) -> Self {
        Self {
            counterparty: value.counterparty.to_string(),
            receipt_id: value.receipt_id,
            receipt_access_event_id: value.receipt_access_event_id,
            payment_reference: Arc::new(FfiPaymentReference::from_validated_text(
                value.payment_reference,
            )),
            payment_request_id: value.payment_request_id,
            billing_period: value.billing_period.map(Into::into),
            payment_endpoint_identifier: value.payment_endpoint_identifier,
            amount: value.amount.map(Into::into),
            status: value.status.into(),
            outbound_message_id: value.outbound_message_id,
            created_at: value.created_at.to_rfc3339(),
            updated_at: value.updated_at.to_rfc3339(),
            stored_at: value.stored_at.map(|time| time.to_rfc3339()),
            access_queued_at: value.access_queued_at.map(|time| time.to_rfc3339()),
        }
    }
}

impl From<ReceiptAccessView> for FfiReceiptAccessView {
    fn from(value: ReceiptAccessView) -> Self {
        Self {
            counterparty: value.counterparty.to_string(),
            event_id: value.event_id,
            receipt_id: value.receipt_id,
            payment_reference: Arc::new(FfiPaymentReference::from_validated_text(
                value.payment_reference,
            )),
            payment_request_id: value.payment_request_id,
            billing_period: value.billing_period.map(Into::into),
            retrieval_status: value.retrieval_status.into(),
            retrieval_attempted_at: value.retrieval_attempted_at.map(|time| time.to_rfc3339()),
            retrieved_at: value.retrieved_at.map(|time| time.to_rfc3339()),
            received_at: value.received_at.to_rfc3339(),
        }
    }
}

impl TryFrom<ReceiptRecord> for FfiReceiptRecord {
    type Error = PaykitFfiError;

    fn try_from(value: ReceiptRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            issuer: value.issuer.to_string(),
            receipt_access_event_id: value.receipt_access_event_id,
            receipt_id: value.receipt_id,
            payment_reference: Arc::new(FfiPaymentReference::from_validated_text(
                value.payment_reference,
            )),
            payment_request_id: value.payment_request_id,
            billing_period: value.billing_period.map(Into::into),
            recipient_public_key: value.recipient_public_key.to_string(),
            payment_endpoint_identifier: value.payment_endpoint_identifier,
            amount: value.amount.map(Into::into),
            metadata: FfiPrivateJsonObject::from_json_map("Receipt metadata", &value.metadata)?,
            retrieved_at: value.retrieved_at.to_rfc3339(),
        })
    }
}

pub(super) fn receipt_issuance_views_to_ffi(
    records: Vec<ReceiptIssuanceView>,
) -> Result<Vec<FfiReceiptIssuanceView>, PaykitFfiError> {
    Ok(records.into_iter().map(Into::into).collect())
}

pub(super) fn receipt_access_views_to_ffi(
    records: Vec<ReceiptAccessView>,
) -> Result<Vec<FfiReceiptAccessView>, PaykitFfiError> {
    Ok(records.into_iter().map(Into::into).collect())
}

pub(super) fn receipt_records_to_ffi(
    records: Vec<ReceiptRecord>,
) -> Result<Vec<FfiReceiptRecord>, PaykitFfiError> {
    records.into_iter().map(TryInto::try_into).collect()
}

pub(super) fn parse_public_key(
    value: String,
) -> Result<paykit_sdk::PubkyPublicKey, PaykitFfiError> {
    paykit_sdk::PubkyPublicKey::new(value).map_err(Into::into)
}

fn parse_receipt_id(value: String) -> Result<ReceiptId, PaykitFfiError> {
    ReceiptId::new(value).map_err(|err| validation_error(err.to_string()))
}

fn parse_payment_request_id(value: String) -> Result<PaymentRequestId, PaykitFfiError> {
    PaymentRequestId::new(value).map_err(|err| validation_error(err.to_string()))
}

fn parse_endpoint_identifier(value: String) -> Result<PaymentEndpointIdentifier, PaykitFfiError> {
    PaymentEndpointIdentifier::new(value).map_err(|err| validation_error(err.to_string()))
}
