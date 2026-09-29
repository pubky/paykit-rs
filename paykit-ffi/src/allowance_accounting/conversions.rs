use super::types::*;
use crate::{
    conversions_common::parse_endpoint_identifier,
    errors::validation_error,
    session::{app_public_key, parse_public_key, parse_receiver_path},
    PaykitFfiError,
};
use chrono::{DateTime, Timelike, Utc};
use paykit_sdk as sdk;
use std::sync::Arc;

fn invalid_input() -> PaykitFfiError {
    validation_error("Invalid Allowance accounting input")
}

pub(super) fn parse_time(value: String) -> Result<DateTime<Utc>, PaykitFfiError> {
    if !value.ends_with('Z') {
        return Err(invalid_input());
    }
    let time = DateTime::parse_from_rfc3339(&value)
        .map_err(|_| invalid_input())?
        .with_timezone(&Utc);
    if time.nanosecond() >= 1_000_000_000 {
        return Err(invalid_input());
    }
    Ok(time)
}
fn parse_allowance_id(value: String) -> Result<paykit_lib::AllowanceId, PaykitFfiError> {
    let parsed = paykit_lib::AllowanceId::new(value.clone()).map_err(|_| invalid_input())?;
    if parsed.as_str() != value {
        return Err(invalid_input());
    }
    Ok(parsed)
}
fn parse_request_id(value: String) -> Result<paykit_lib::PaymentRequestId, PaykitFfiError> {
    let parsed = paykit_lib::PaymentRequestId::new(value.clone()).map_err(|_| invalid_input())?;
    if parsed.as_str() != value {
        return Err(invalid_input());
    }
    Ok(parsed)
}

impl TryFrom<FfiPaymentRequestScope> for sdk::PaymentRequestScope {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentRequestScope) -> Result<Self, Self::Error> {
        Ok(Self {
            counterparty: parse_public_key(value.counterparty).map_err(|_| invalid_input())?,
            counterparty_receiver_path: parse_receiver_path(value.counterparty_receiver_path)
                .map_err(|_| invalid_input())?,
            payment_request_id: parse_request_id(value.payment_request_id)?,
        })
    }
}

impl TryFrom<FfiPaymentOccurrence> for sdk::PaymentOccurrence {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentOccurrence) -> Result<Self, Self::Error> {
        Ok(Self {
            request: value
                .request
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
            billing_period: value
                .billing_period
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .transpose()?,
        })
    }
}

impl TryFrom<FfiPaymentAccountingScope> for sdk::PaymentAccountingScope {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentAccountingScope) -> Result<Self, Self::Error> {
        Ok(Self {
            local_public_key: parse_public_key(value.local_public_key)
                .map_err(|_| invalid_input())?,
            local_receiver_path: parse_receiver_path(value.local_receiver_path)
                .map_err(|_| invalid_input())?,
            counterparty: parse_public_key(value.counterparty).map_err(|_| invalid_input())?,
            counterparty_receiver_path: parse_receiver_path(value.counterparty_receiver_path)
                .map_err(|_| invalid_input())?,
            payment_request_id: parse_request_id(value.payment_request_id)?
                .as_str()
                .to_owned(),
        })
    }
}

impl TryFrom<sdk::PaymentAccountingScope> for FfiPaymentAccountingScope {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentAccountingScope) -> Result<Self, Self::Error> {
        Ok(Self {
            local_public_key: app_public_key(&value.local_public_key),
            local_receiver_path: value.local_receiver_path.as_str().to_owned(),
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_path: value.counterparty_receiver_path.as_str().to_owned(),
            payment_request_id: value.payment_request_id,
        })
    }
}

impl TryFrom<FfiAccountingBillingPeriod> for sdk::AccountingBillingPeriod {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAccountingBillingPeriod) -> Result<Self, Self::Error> {
        let starts_at = parse_time(value.starts_at)?;
        let ends_at = parse_time(value.ends_at)?;
        if starts_at >= ends_at {
            return Err(invalid_input());
        }
        Ok(Self { starts_at, ends_at })
    }
}

impl TryFrom<sdk::AccountingBillingPeriod> for FfiAccountingBillingPeriod {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::AccountingBillingPeriod) -> Result<Self, Self::Error> {
        Ok(Self {
            starts_at: value
                .starts_at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            ends_at: value
                .ends_at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        })
    }
}

impl TryFrom<FfiPaymentOccurrenceKey> for sdk::PaymentOccurrenceKey {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentOccurrenceKey) -> Result<Self, Self::Error> {
        Ok(Self {
            request: value
                .request
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
            billing_period: value
                .billing_period
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .transpose()?,
        })
    }
}

impl TryFrom<sdk::PaymentOccurrenceKey> for FfiPaymentOccurrenceKey {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentOccurrenceKey) -> Result<Self, Self::Error> {
        Ok(Self {
            request: value.request.try_into()?,
            billing_period: value.billing_period.map(TryInto::try_into).transpose()?,
        })
    }
}

impl TryFrom<FfiPaymentAttemptRecord> for sdk::PaymentAttemptRecord {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentAttemptRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            attempt_id: value.attempt_id,
            mode: value
                .mode
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
            allowance_id: value
                .allowance_id
                .map(|value| -> Result<_, PaykitFfiError> {
                    Ok(parse_allowance_id(value)?.as_str().to_owned())
                })
                .transpose()?,
            association_revision: value.association_revision,
            amount: sdk::AmountRecord {
                value: value.amount.value(),
                asset: value.amount.asset(),
            },
            admitted_at: parse_time(value.admitted_at)?,
            status: value
                .status
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
            epoch: value.epoch,
        })
    }
}

impl TryFrom<sdk::PaymentAttemptRecord> for FfiPaymentAttemptRecord {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentAttemptRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            attempt_id: value.attempt_id,
            mode: value.mode.try_into()?,
            allowance_id: value.allowance_id,
            association_revision: value.association_revision,
            amount: Arc::new(FfiAccountingAmount {
                amount: paykit_lib::PaymentAmount::new(value.amount.value, value.amount.asset)
                    .map_err(|_| invalid_input())?,
            }),
            admitted_at: value
                .admitted_at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            status: value.status.try_into()?,
            epoch: value.epoch,
        })
    }
}

impl TryFrom<FfiPaymentOccurrenceRecord> for sdk::PaymentOccurrenceRecord {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentOccurrenceRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            key: value
                .key
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
            disposition: value
                .disposition
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
            allowance_id: value
                .allowance_id
                .map(|value| -> Result<_, PaykitFfiError> {
                    Ok(parse_allowance_id(value)?.as_str().to_owned())
                })
                .transpose()?,
            association_revision: value.association_revision,
            attempts: value
                .attempts
                .into_iter()
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<sdk::PaymentOccurrenceRecord> for FfiPaymentOccurrenceRecord {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentOccurrenceRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            key: value.key.try_into()?,
            disposition: value.disposition.try_into()?,
            allowance_id: value.allowance_id,
            association_revision: value.association_revision,
            attempts: value
                .attempts
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<FfiAllowanceAssociationRevision> for sdk::AllowanceAssociationRevision {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceAssociationRevision) -> Result<Self, Self::Error> {
        Ok(Self {
            revision: value.revision,
            allowance_id: parse_allowance_id(value.allowance_id)?.as_str().to_owned(),
            effective_from: value.effective_from.map(parse_time).transpose()?,
            authorization_id: value.authorization_id,
            authorized_at: parse_time(value.authorized_at)?,
        })
    }
}

impl TryFrom<sdk::AllowanceAssociationRevision> for FfiAllowanceAssociationRevision {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::AllowanceAssociationRevision) -> Result<Self, Self::Error> {
        Ok(Self {
            revision: value.revision,
            allowance_id: value.allowance_id,
            effective_from: value
                .effective_from
                .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
            authorization_id: value.authorization_id,
            authorized_at: value
                .authorized_at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        })
    }
}

impl TryFrom<FfiAllowanceAssociationRecord> for sdk::AllowanceAssociationRecord {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceAssociationRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            request: value
                .request
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
            revisions: value
                .revisions
                .into_iter()
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<sdk::AllowanceAssociationRecord> for FfiAllowanceAssociationRecord {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::AllowanceAssociationRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            request: value.request.try_into()?,
            revisions: value
                .revisions
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<FfiAllowanceWatermarkRecord> for sdk::AllowanceWatermarkRecord {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceWatermarkRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            local_public_key: parse_public_key(value.local_public_key)
                .map_err(|_| invalid_input())?,
            local_receiver_path: parse_receiver_path(value.local_receiver_path)
                .map_err(|_| invalid_input())?,
            counterparty: parse_public_key(value.counterparty).map_err(|_| invalid_input())?,
            counterparty_receiver_path: parse_receiver_path(value.counterparty_receiver_path)
                .map_err(|_| invalid_input())?,
            allowance_id: parse_allowance_id(value.allowance_id)?.as_str().to_owned(),
            evaluated_at: parse_time(value.evaluated_at)?,
        })
    }
}

impl TryFrom<sdk::AllowanceWatermarkRecord> for FfiAllowanceWatermarkRecord {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::AllowanceWatermarkRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            local_public_key: app_public_key(&value.local_public_key),
            local_receiver_path: value.local_receiver_path.as_str().to_owned(),
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_path: value.counterparty_receiver_path.as_str().to_owned(),
            allowance_id: value.allowance_id,
            evaluated_at: value
                .evaluated_at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        })
    }
}

impl TryFrom<FfiAllowanceAccountingHistory> for sdk::AllowanceAccountingHistory {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceAccountingHistory) -> Result<Self, Self::Error> {
        Ok(Self {
            associations: value
                .associations
                .into_iter()
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .collect::<Result<_, _>>()?,
            occurrences: value
                .occurrences
                .into_iter()
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .collect::<Result<_, _>>()?,
            watermarks: value
                .watermarks
                .into_iter()
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<sdk::AllowanceAccountingHistory> for FfiAllowanceAccountingHistory {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::AllowanceAccountingHistory) -> Result<Self, Self::Error> {
        Ok(Self {
            associations: value
                .associations
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            occurrences: value
                .occurrences
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            watermarks: value
                .watermarks
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl TryFrom<FfiAllowanceAccountingState> for sdk::AllowanceAccountingState {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceAccountingState) -> Result<Self, Self::Error> {
        Ok(Self {
            revision: value.revision,
            epoch: value.epoch,
            requires_reconciliation: value.requires_reconciliation,
            history: value
                .history
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
        })
    }
}

impl TryFrom<sdk::AllowanceAccountingState> for FfiAllowanceAccountingState {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::AllowanceAccountingState) -> Result<Self, Self::Error> {
        Ok(Self {
            revision: value.revision,
            epoch: value.epoch,
            requires_reconciliation: value.requires_reconciliation,
            history: value.history.try_into()?,
        })
    }
}

impl TryFrom<FfiAllowanceSelectionInput> for sdk::AllowanceSelectionInput {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceSelectionInput) -> Result<Self, Self::Error> {
        Ok(Self {
            allowance_id: parse_allowance_id(value.allowance_id)?,
            expected_revision: value.expected_revision,
            trusted_time: parse_time(value.trusted_time)?,
        })
    }
}

impl TryFrom<FfiAllowanceReassociationInput> for sdk::AllowanceReassociationInput {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceReassociationInput) -> Result<Self, Self::Error> {
        Ok(Self {
            allowance_id: parse_allowance_id(value.allowance_id)?,
            expected_revision: value.expected_revision,
            effective_from: parse_time(value.effective_from)?,
            authorization_id: value.authorization_id,
            trusted_time: parse_time(value.trusted_time)?,
        })
    }
}

impl TryFrom<FfiPaymentExecutionChecks> for sdk::PaymentExecutionChecks {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentExecutionChecks) -> Result<Self, Self::Error> {
        Ok(Self {
            trusted_time: parse_time(value.trusted_time)?,
            payment_endpoint_identifier: parse_endpoint_identifier(
                value.payment_endpoint_identifier,
            )
            .map_err(|_| invalid_input())?,
            actual_amount: value.actual_amount.amount.clone(),
            endpoint_current: value.endpoint_current,
            local_enabled: value.local_enabled,
            recurrence_eligible: value.recurrence_eligible,
        })
    }
}

impl TryFrom<FfiAllowanceCandidate> for sdk::AllowanceCandidate {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceCandidate) -> Result<Self, Self::Error> {
        Ok(Self {
            allowance_id: parse_allowance_id(value.allowance_id)?.as_str().to_owned(),
            eligible_payment_endpoint_identifiers: value.eligible_payment_endpoint_identifiers,
            blocked: value
                .blocked
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .transpose()?,
        })
    }
}

impl TryFrom<sdk::AllowanceCandidate> for FfiAllowanceCandidate {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::AllowanceCandidate) -> Result<Self, Self::Error> {
        Ok(Self {
            allowance_id: value.allowance_id,
            eligible_payment_endpoint_identifiers: value.eligible_payment_endpoint_identifiers,
            blocked: value.blocked.map(TryInto::try_into).transpose()?,
        })
    }
}

impl TryFrom<FfiPaymentOutcomeReport> for sdk::PaymentOutcomeReport {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentOutcomeReport) -> Result<Self, Self::Error> {
        Ok(Self {
            attempt_id: value.attempt_id,
            outcome: value
                .outcome
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
        })
    }
}

impl TryFrom<FfiAllowanceAccountingReconciliation> for sdk::AllowanceAccountingReconciliation {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceAccountingReconciliation) -> Result<Self, Self::Error> {
        Ok(Self {
            expected_revision: value.expected_revision,
            history: value
                .history
                .try_into()
                .map_err(|_: PaykitFfiError| invalid_input())?,
            outcomes: value
                .outcomes
                .into_iter()
                .map(|value| {
                    value
                        .try_into()
                        .map_err(|_: PaykitFfiError| invalid_input())
                })
                .collect::<Result<_, _>>()?,
            trusted_time: parse_time(value.trusted_time)?,
        })
    }
}

impl TryFrom<FfiPaymentDisposition> for sdk::PaymentDisposition {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentDisposition) -> Result<Self, Self::Error> {
        Ok(match value {
            FfiPaymentDisposition::Automatic => Self::Automatic,
            FfiPaymentDisposition::Deferred { reason } => Self::Deferred { reason },
            FfiPaymentDisposition::ManualOnly => Self::ManualOnly,
        })
    }
}

impl TryFrom<sdk::PaymentDisposition> for FfiPaymentDisposition {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentDisposition) -> Result<Self, Self::Error> {
        Ok(match value {
            sdk::PaymentDisposition::Automatic => Self::Automatic,
            sdk::PaymentDisposition::Deferred { reason } => Self::Deferred { reason },
            sdk::PaymentDisposition::ManualOnly => Self::ManualOnly,
        })
    }
}

impl TryFrom<FfiPaymentExecutionMode> for sdk::PaymentExecutionMode {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentExecutionMode) -> Result<Self, Self::Error> {
        Ok(match value {
            FfiPaymentExecutionMode::Automatic => Self::Automatic,
            FfiPaymentExecutionMode::Manual => Self::Manual,
        })
    }
}

impl TryFrom<sdk::PaymentExecutionMode> for FfiPaymentExecutionMode {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentExecutionMode) -> Result<Self, Self::Error> {
        Ok(match value {
            sdk::PaymentExecutionMode::Automatic => Self::Automatic,
            sdk::PaymentExecutionMode::Manual => Self::Manual,
        })
    }
}

impl TryFrom<FfiPaymentExecutionStatus> for sdk::PaymentExecutionStatus {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentExecutionStatus) -> Result<Self, Self::Error> {
        Ok(match value {
            FfiPaymentExecutionStatus::Prepared => Self::Prepared,
            FfiPaymentExecutionStatus::Submitted => Self::Submitted,
            FfiPaymentExecutionStatus::Unknown => Self::Unknown,
            FfiPaymentExecutionStatus::Succeeded => Self::Succeeded,
            FfiPaymentExecutionStatus::Failed => Self::Failed,
        })
    }
}

impl TryFrom<sdk::PaymentExecutionStatus> for FfiPaymentExecutionStatus {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentExecutionStatus) -> Result<Self, Self::Error> {
        Ok(match value {
            sdk::PaymentExecutionStatus::Prepared => Self::Prepared,
            sdk::PaymentExecutionStatus::Submitted => Self::Submitted,
            sdk::PaymentExecutionStatus::Unknown => Self::Unknown,
            sdk::PaymentExecutionStatus::Succeeded => Self::Succeeded,
            sdk::PaymentExecutionStatus::Failed => Self::Failed,
        })
    }
}

impl TryFrom<FfiAllowanceAccountingBlock> for sdk::AllowanceAccountingBlock {
    type Error = PaykitFfiError;
    fn try_from(value: FfiAllowanceAccountingBlock) -> Result<Self, Self::Error> {
        Ok(match value {
            FfiAllowanceAccountingBlock::ReconciliationRequired => Self::ReconciliationRequired,
            FfiAllowanceAccountingBlock::InvalidLifecycle => Self::InvalidLifecycle,
            FfiAllowanceAccountingBlock::StaleRevision => Self::StaleRevision,
            FfiAllowanceAccountingBlock::NoSelection => Self::NoSelection,
            FfiAllowanceAccountingBlock::ManualOnly => Self::ManualOnly,
            FfiAllowanceAccountingBlock::PaymentAlreadyRecorded => Self::PaymentAlreadyRecorded,
            FfiAllowanceAccountingBlock::WalletChecksFailed => Self::WalletChecksFailed,
            FfiAllowanceAccountingBlock::SharedRule { code } => Self::SharedRule { code },
        })
    }
}

impl TryFrom<sdk::AllowanceAccountingBlock> for FfiAllowanceAccountingBlock {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::AllowanceAccountingBlock) -> Result<Self, Self::Error> {
        Ok(match value {
            sdk::AllowanceAccountingBlock::ReconciliationRequired => Self::ReconciliationRequired,
            sdk::AllowanceAccountingBlock::InvalidLifecycle => Self::InvalidLifecycle,
            sdk::AllowanceAccountingBlock::StaleRevision => Self::StaleRevision,
            sdk::AllowanceAccountingBlock::NoSelection => Self::NoSelection,
            sdk::AllowanceAccountingBlock::ManualOnly => Self::ManualOnly,
            sdk::AllowanceAccountingBlock::PaymentAlreadyRecorded => Self::PaymentAlreadyRecorded,
            sdk::AllowanceAccountingBlock::WalletChecksFailed => Self::WalletChecksFailed,
            sdk::AllowanceAccountingBlock::SharedRule { code } => Self::SharedRule { code },
        })
    }
}

impl TryFrom<FfiPaymentAttemptDecision> for sdk::PaymentAttemptDecision {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentAttemptDecision) -> Result<Self, Self::Error> {
        Ok(match value {
            FfiPaymentAttemptDecision::Ready { attempt } => Self::Ready {
                attempt: attempt.try_into()?,
            },
            FfiPaymentAttemptDecision::Blocked { reason } => Self::Blocked {
                reason: reason.try_into()?,
            },
        })
    }
}

impl TryFrom<sdk::PaymentAttemptDecision> for FfiPaymentAttemptDecision {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentAttemptDecision) -> Result<Self, Self::Error> {
        Ok(match value {
            sdk::PaymentAttemptDecision::Ready { attempt } => Self::Ready {
                attempt: attempt.try_into()?,
            },
            sdk::PaymentAttemptDecision::Blocked { reason } => Self::Blocked {
                reason: reason.try_into()?,
            },
        })
    }
}

impl TryFrom<FfiPaymentOutcome> for sdk::PaymentOutcome {
    type Error = PaykitFfiError;
    fn try_from(value: FfiPaymentOutcome) -> Result<Self, Self::Error> {
        Ok(match value {
            FfiPaymentOutcome::Succeeded => Self::Succeeded,
            FfiPaymentOutcome::Failed => Self::Failed,
            FfiPaymentOutcome::Unknown => Self::Unknown,
        })
    }
}

impl TryFrom<sdk::PaymentOutcome> for FfiPaymentOutcome {
    type Error = PaykitFfiError;
    fn try_from(value: sdk::PaymentOutcome) -> Result<Self, Self::Error> {
        Ok(match value {
            sdk::PaymentOutcome::Succeeded => Self::Succeeded,
            sdk::PaymentOutcome::Failed => Self::Failed,
            sdk::PaymentOutcome::Unknown => Self::Unknown,
        })
    }
}
