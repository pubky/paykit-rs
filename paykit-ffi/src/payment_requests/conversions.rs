use std::sync::Arc;

use paykit_lib::{
    BillingPeriod, PaykitAppId, PaymentAmount, PaymentEndpointIdentifier, PaymentReference,
    PaymentRequestTerms, Recurrence, RecurrenceUnit,
};
use paykit_sdk::{
    AmountRecord, BillingPeriodRecord, PaymentProofRecord, PaymentRequestFilter,
    PaymentRequestLifecycleState, PaymentRequestLocalRole, PaymentRequestRecord,
    PaymentRequestRecurrenceRecord, PaymentRequestTermsRecord, PubkyPublicKey,
};

use crate::{
    errors::validation_error,
    json::FfiPrivateJsonObject,
    session::{app_public_key, parse_public_key as parse_pubky_public_key},
    PaykitFfiError,
};

use crate::conversions_common::parse_endpoint_identifier;
pub(super) use crate::conversions_common::parse_payment_request_id;

use super::{
    FfiBillingPeriod, FfiPaymentProofRecord, FfiPaymentProofSubmission, FfiPaymentReference,
    FfiPaymentRequestAmount, FfiPaymentRequestFilter, FfiPaymentRequestLifecycleState,
    FfiPaymentRequestLocalRole, FfiPaymentRequestRecord, FfiPaymentRequestRecurrence,
    FfiPaymentRequestTerms,
};

impl From<PaymentRequestLocalRole> for FfiPaymentRequestLocalRole {
    fn from(value: PaymentRequestLocalRole) -> Self {
        match value {
            PaymentRequestLocalRole::Payer => Self::Payer,
            PaymentRequestLocalRole::Payee => Self::Payee,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiPaymentRequestLocalRole> for PaymentRequestLocalRole {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaymentRequestLocalRole) -> Result<Self, Self::Error> {
        match value {
            FfiPaymentRequestLocalRole::Payer => Ok(Self::Payer),
            FfiPaymentRequestLocalRole::Payee => Ok(Self::Payee),
            FfiPaymentRequestLocalRole::Unknown => Err(validation_error(
                "Payment Request local role must not be Unknown in filters",
            )),
        }
    }
}

impl From<PaymentRequestLifecycleState> for FfiPaymentRequestLifecycleState {
    fn from(value: PaymentRequestLifecycleState) -> Self {
        match value {
            PaymentRequestLifecycleState::Proposed => Self::Proposed,
            PaymentRequestLifecycleState::ProposalExpired => Self::ProposalExpired,
            PaymentRequestLifecycleState::Accepted => Self::Accepted,
            PaymentRequestLifecycleState::Rejected => Self::Rejected,
            PaymentRequestLifecycleState::Canceled => Self::Canceled,
            PaymentRequestLifecycleState::ProofSubmitted => Self::ProofSubmitted,
            PaymentRequestLifecycleState::ActiveRecurring => Self::ActiveRecurring,
            PaymentRequestLifecycleState::RecoveryRequired => Self::RecoveryRequired,
            PaymentRequestLifecycleState::InvalidConflict => Self::InvalidConflict,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiPaymentRequestLifecycleState> for PaymentRequestLifecycleState {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaymentRequestLifecycleState) -> Result<Self, Self::Error> {
        match value {
            FfiPaymentRequestLifecycleState::Proposed => Ok(Self::Proposed),
            FfiPaymentRequestLifecycleState::ProposalExpired => Ok(Self::ProposalExpired),
            FfiPaymentRequestLifecycleState::Accepted => Ok(Self::Accepted),
            FfiPaymentRequestLifecycleState::Rejected => Ok(Self::Rejected),
            FfiPaymentRequestLifecycleState::Canceled => Ok(Self::Canceled),
            FfiPaymentRequestLifecycleState::ProofSubmitted => Ok(Self::ProofSubmitted),
            FfiPaymentRequestLifecycleState::ActiveRecurring => Ok(Self::ActiveRecurring),
            FfiPaymentRequestLifecycleState::RecoveryRequired => Ok(Self::RecoveryRequired),
            FfiPaymentRequestLifecycleState::InvalidConflict => Ok(Self::InvalidConflict),
            FfiPaymentRequestLifecycleState::Unknown => Err(validation_error(
                "Payment Request lifecycle state must not be Unknown in filters",
            )),
        }
    }
}

impl TryFrom<FfiPaymentRequestFilter> for PaymentRequestFilter {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaymentRequestFilter) -> Result<Self, Self::Error> {
        Ok(Self {
            counterparty: value.counterparty.map(parse_public_key).transpose()?,
            local_role: value.local_role.map(TryInto::try_into).transpose()?,
            states: value
                .states
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            recurring: value.recurring,
            received_only: value.received_only,
        })
    }
}

impl From<AmountRecord> for FfiPaymentRequestAmount {
    fn from(value: AmountRecord) -> Self {
        Self {
            value: value.value,
            asset: value.asset,
        }
    }
}

impl From<BillingPeriodRecord> for FfiBillingPeriod {
    fn from(value: BillingPeriodRecord) -> Self {
        Self {
            starts_at: value.starts_at,
            ends_at: value.ends_at,
        }
    }
}

impl TryFrom<FfiBillingPeriod> for BillingPeriod {
    type Error = PaykitFfiError;

    fn try_from(value: FfiBillingPeriod) -> Result<Self, Self::Error> {
        Ok(Self {
            starts_at: value.starts_at,
            ends_at: value.ends_at,
        })
    }
}

impl From<PaymentRequestRecurrenceRecord> for FfiPaymentRequestRecurrence {
    fn from(value: PaymentRequestRecurrenceRecord) -> Self {
        Self {
            every: value.every,
            unit: value.unit,
            starts_at: value.starts_at,
            anchor: value.anchor,
            ends_at: value.ends_at,
        }
    }
}

impl TryFrom<FfiPaymentRequestRecurrence> for Recurrence {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaymentRequestRecurrence) -> Result<Self, Self::Error> {
        Ok(Self {
            every: value.every,
            unit: parse_recurrence_unit(&value.unit)?,
            starts_at: value.starts_at,
            anchor: value.anchor,
            ends_at: value.ends_at,
        })
    }
}

impl TryFrom<FfiPaymentRequestTerms> for PaymentRequestTerms {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaymentRequestTerms) -> Result<Self, Self::Error> {
        Ok(Self {
            amount: PaymentAmount::new(value.amount.value, value.amount.asset)
                .map_err(|err| validation_error(err.to_string()))?,
            payment_reference: PaymentReference::new(value.payment_reference.export_text())
                .map_err(|err| validation_error(err.to_string()))?,
            proposal_expires_at: value.proposal_expires_at,
            recurrence: value.recurrence.map(TryInto::try_into).transpose()?,
            accepted_payment_endpoint_identifiers: value
                .accepted_payment_endpoint_identifiers
                .into_iter()
                .map(parse_endpoint_identifier)
                .collect::<Result<Vec<_>, _>>()?,
            required_app_id: value
                .required_app_id
                .map(PaykitAppId::new)
                .transpose()
                .map_err(|err| validation_error(err.to_string()))?,
            metadata: value.metadata.parse_map("Payment Request metadata")?,
        })
    }
}

impl TryFrom<PaymentRequestTermsRecord> for FfiPaymentRequestTerms {
    type Error = PaykitFfiError;

    fn try_from(value: PaymentRequestTermsRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            amount: value.amount.into(),
            payment_reference: Arc::new(FfiPaymentReference::from_validated_text(
                value.payment_reference,
            )),
            proposal_expires_at: value.proposal_expires_at,
            recurrence: value.recurrence.map(Into::into),
            accepted_payment_endpoint_identifiers: value.accepted_payment_endpoint_identifiers,
            required_app_id: value.required_app_id.map(|app_id| app_id.to_string()),
            metadata: FfiPrivateJsonObject::from_json_map(
                "Payment Request metadata",
                &value.metadata,
            )?,
        })
    }
}

impl TryFrom<PaymentProofRecord> for FfiPaymentProofRecord {
    type Error = PaykitFfiError;

    fn try_from(value: PaymentProofRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            event_id: value.event_id,
            outbound_message_id: value.outbound_message_id,
            outbound_status: value.outbound_status.map(Into::into),
            stream_item_id: value.stream_item_id,
            payment_reference: Arc::new(FfiPaymentReference::from_validated_text(
                value.payment_reference,
            )),
            billing_period: value.billing_period.map(Into::into),
            payment_app_id: value.payment_app_id.to_string(),
            payment_endpoint_identifier: value.payment_endpoint_identifier,
            proof: FfiPrivateJsonObject::from_json_map("Payment Proof proof", &value.proof)?,
            recorded_at: value.recorded_at.to_rfc3339(),
        })
    }
}

impl TryFrom<PaymentRequestRecord> for FfiPaymentRequestRecord {
    type Error = PaykitFfiError;

    fn try_from(value: PaymentRequestRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            counterparty: app_public_key(&value.counterparty),
            payment_request_id: value.payment_request_id,
            local_role: value.local_role.map(Into::into),
            state: value.state.into(),
            proposal_stream_item_id: value.proposal_stream_item_id,
            proposal_outbound_message_id: value.proposal_outbound_message_id,
            proposal_outbound_status: value.proposal_outbound_status.map(Into::into),
            proposal_event_id: value.proposal_event_id,
            proposal_app_id: value.proposal_app_id.map(|app_id| app_id.to_string()),
            payer_app_id: value.payer_app_id.map(|app_id| app_id.to_string()),
            terms: value.terms.map(TryInto::try_into).transpose()?,
            accepted_event_id: value.accepted_event_id,
            accepted_outbound_status: value.accepted_outbound_status.map(Into::into),
            rejected_event_id: value.rejected_event_id,
            rejected_outbound_status: value.rejected_outbound_status.map(Into::into),
            canceled_event_id: value.canceled_event_id,
            canceled_outbound_status: value.canceled_outbound_status.map(Into::into),
            payment_proofs: value
                .payment_proofs
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
            last_stream_item_id: value.last_stream_item_id,
            last_outbound_message_id: value.last_outbound_message_id,
            last_outbound_status: value.last_outbound_status.map(Into::into),
            last_event_at: value.last_event_at.map(|time| time.to_rfc3339()),
            invalid_reason: value.invalid_reason,
        })
    }
}

pub(super) struct ParsedPaymentProofSubmission {
    pub(super) billing_period: Option<BillingPeriod>,
    pub(super) payment_app_id: PaykitAppId,
    pub(super) payment_endpoint_identifier: PaymentEndpointIdentifier,
    pub(super) proof: serde_json::Map<String, serde_json::Value>,
}

impl TryFrom<FfiPaymentProofSubmission> for ParsedPaymentProofSubmission {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaymentProofSubmission) -> Result<Self, Self::Error> {
        Ok(Self {
            billing_period: value.billing_period.map(TryInto::try_into).transpose()?,
            payment_app_id: PaykitAppId::new(value.payment_app_id)
                .map_err(|err| validation_error(err.to_string()))?,
            payment_endpoint_identifier: parse_endpoint_identifier(
                value.payment_endpoint_identifier,
            )?,
            proof: value.proof.parse_map("Payment Proof proof")?,
        })
    }
}

pub(super) fn payment_request_records_to_ffi(
    records: Vec<PaymentRequestRecord>,
) -> Result<Vec<FfiPaymentRequestRecord>, PaykitFfiError> {
    records.into_iter().map(TryInto::try_into).collect()
}

pub(super) fn parse_public_key(value: String) -> Result<PubkyPublicKey, PaykitFfiError> {
    parse_pubky_public_key(value)
}

fn parse_recurrence_unit(value: &str) -> Result<RecurrenceUnit, PaykitFfiError> {
    match value {
        "minute" => Ok(RecurrenceUnit::Minute),
        "hour" => Ok(RecurrenceUnit::Hour),
        "day" => Ok(RecurrenceUnit::Day),
        "week" => Ok(RecurrenceUnit::Week),
        "month" => Ok(RecurrenceUnit::Month),
        "year" => Ok(RecurrenceUnit::Year),
        _ => Err(validation_error(format!(
            "unsupported Recurrence unit '{value}'"
        ))),
    }
}
