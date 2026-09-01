use std::sync::Arc;

use paykit_lib::{
    AllowanceAmountRange, AllowanceId, AllowancePeriod, AllowancePeriodLimit, AllowancePeriodUnit,
    AllowanceTerms,
};
use paykit_sdk::{
    AllowanceFilter, AllowanceHistoryStatus, AllowanceLifecycleState, AllowanceLocalRole,
    AllowanceRecord, AllowanceTermsRecord,
};

use crate::{
    conversions_common::parse_endpoint_identifier,
    errors::validation_error,
    session::{app_public_key, parse_public_key, parse_receiver_path},
    PaykitFfiError,
};

use super::{
    FfiAllowanceAmountRange, FfiAllowanceFilter, FfiAllowanceHistoryStatus,
    FfiAllowanceLifecycleState, FfiAllowanceLocalRole, FfiAllowancePeriod, FfiAllowancePeriodLimit,
    FfiAllowanceRecord, FfiAllowanceTerms,
};

impl From<AllowanceLocalRole> for FfiAllowanceLocalRole {
    fn from(value: AllowanceLocalRole) -> Self {
        match value {
            AllowanceLocalRole::Allower => Self::Allower,
            AllowanceLocalRole::Allowee => Self::Allowee,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiAllowanceLocalRole> for AllowanceLocalRole {
    type Error = PaykitFfiError;

    fn try_from(value: FfiAllowanceLocalRole) -> Result<Self, Self::Error> {
        match value {
            FfiAllowanceLocalRole::Allower => Ok(Self::Allower),
            FfiAllowanceLocalRole::Allowee => Ok(Self::Allowee),
            FfiAllowanceLocalRole::Unknown => {
                Err(validation_error("Allowance local role must not be Unknown"))
            }
        }
    }
}

impl From<AllowanceLifecycleState> for FfiAllowanceLifecycleState {
    fn from(value: AllowanceLifecycleState) -> Self {
        match value {
            AllowanceLifecycleState::Proposed => Self::Proposed,
            AllowanceLifecycleState::Accepted => Self::Accepted,
            AllowanceLifecycleState::Rejected => Self::Rejected,
            AllowanceLifecycleState::Ended => Self::Ended,
            AllowanceLifecycleState::Conflicted => Self::Conflicted,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiAllowanceLifecycleState> for AllowanceLifecycleState {
    type Error = PaykitFfiError;

    fn try_from(value: FfiAllowanceLifecycleState) -> Result<Self, Self::Error> {
        match value {
            FfiAllowanceLifecycleState::Proposed => Ok(Self::Proposed),
            FfiAllowanceLifecycleState::Accepted => Ok(Self::Accepted),
            FfiAllowanceLifecycleState::Rejected => Ok(Self::Rejected),
            FfiAllowanceLifecycleState::Ended => Ok(Self::Ended),
            FfiAllowanceLifecycleState::Conflicted => Ok(Self::Conflicted),
            FfiAllowanceLifecycleState::Unknown => Err(validation_error(
                "Allowance lifecycle state must not be Unknown in filters",
            )),
        }
    }
}

impl From<AllowanceHistoryStatus> for FfiAllowanceHistoryStatus {
    fn from(value: AllowanceHistoryStatus) -> Self {
        match value {
            AllowanceHistoryStatus::Consistent => Self::Consistent,
            AllowanceHistoryStatus::UnresolvedReferences => Self::UnresolvedReferences,
            AllowanceHistoryStatus::Invalid => Self::Invalid,
            AllowanceHistoryStatus::RecoveryRequired => Self::RecoveryRequired,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiAllowanceFilter> for AllowanceFilter {
    type Error = PaykitFfiError;

    fn try_from(value: FfiAllowanceFilter) -> Result<Self, Self::Error> {
        Ok(Self {
            counterparty: value.counterparty.map(parse_public_key).transpose()?,
            counterparty_receiver_path: value
                .counterparty_receiver_path
                .map(parse_receiver_path)
                .transpose()?,
            local_role: value.local_role.map(TryInto::try_into).transpose()?,
            states: value
                .states
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

impl From<&AllowanceAmountRange> for FfiAllowanceAmountRange {
    fn from(value: &AllowanceAmountRange) -> Self {
        Self::from_validated_range(value.clone())
    }
}

impl From<&AllowancePeriod> for FfiAllowancePeriod {
    fn from(value: &AllowancePeriod) -> Self {
        Self::from_validated_period(value.clone())
    }
}

impl From<&AllowancePeriodLimit> for FfiAllowancePeriodLimit {
    fn from(value: &AllowancePeriodLimit) -> Self {
        Self::from_validated_limit(value.clone())
    }
}

impl TryFrom<AllowanceTermsRecord> for FfiAllowanceTerms {
    type Error = PaykitFfiError;

    fn try_from(value: AllowanceTermsRecord) -> Result<Self, Self::Error> {
        parse_allowance_terms_record(
            value.asset,
            value.per_payment_amount,
            value.period_limits,
            value.lifetime_amount_limit,
            value.active_from,
            value.expires_at,
            value.allowed_payment_endpoint_identifiers,
        )
        .map(Self::from_validated_terms)
    }
}

impl TryFrom<AllowanceRecord> for FfiAllowanceRecord {
    type Error = PaykitFfiError;

    fn try_from(value: AllowanceRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_path: value.counterparty_receiver_path.to_string(),
            allowance_id: value.allowance_id,
            local_role: value.local_role.map(Into::into),
            state: value.state.into(),
            history_status: value.history_status.into(),
            proposal_event_id: value.proposal_event_id,
            terms: value
                .terms
                .map(TryInto::try_into)
                .transpose()?
                .map(Arc::new),
            proposal_stream_item_id: value.proposal_stream_item_id,
            proposal_outbound_message_id: value.proposal_outbound_message_id,
            proposal_outbound_status: value.proposal_outbound_status.map(Into::into),
            acceptance_event_id: value.acceptance_event_id,
            acceptance_outbound_status: value.acceptance_outbound_status.map(Into::into),
            rejection_event_id: value.rejection_event_id,
            rejection_outbound_status: value.rejection_outbound_status.map(Into::into),
            end_event_id: value.end_event_id,
            end_outbound_status: value.end_outbound_status.map(Into::into),
            pending_causal_event_ids: value.pending_causal_event_ids,
            conflict_event_ids: value.conflict_event_ids,
            last_stream_item_id: value.last_stream_item_id,
            last_outbound_message_id: value.last_outbound_message_id,
            last_outbound_status: value.last_outbound_status.map(Into::into),
            last_event_at: value.last_event_at.map(|time| time.to_rfc3339()),
            invalid_reason: value.invalid_reason,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn parse_allowance_terms(
    asset: String,
    per_payment_amount: Option<Arc<FfiAllowanceAmountRange>>,
    period_limits: Vec<Arc<FfiAllowancePeriodLimit>>,
    lifetime_amount_limit: Option<String>,
    active_from: Option<String>,
    expires_at: Option<String>,
    allowed_payment_endpoint_identifiers: Option<Vec<String>>,
) -> Result<AllowanceTerms, PaykitFfiError> {
    let result = build_allowance_terms(
        asset,
        per_payment_amount.map(|range| range.domain_range()),
        period_limits
            .into_iter()
            .map(|limit| limit.domain_limit())
            .collect(),
        lifetime_amount_limit,
        active_from,
        expires_at,
        allowed_payment_endpoint_identifiers,
    );
    result.map_err(|_| invalid_allowance_terms())
}

#[allow(clippy::too_many_arguments)]
fn parse_allowance_terms_record(
    asset: String,
    per_payment_amount: Option<paykit_sdk::AllowanceAmountRangeRecord>,
    period_limits: Vec<paykit_sdk::AllowancePeriodLimitRecord>,
    lifetime_amount_limit: Option<String>,
    active_from: Option<String>,
    expires_at: Option<String>,
    allowed_payment_endpoint_identifiers: Option<Vec<String>>,
) -> Result<AllowanceTerms, PaykitFfiError> {
    let result = (|| {
        build_allowance_terms(
            asset,
            per_payment_amount
                .map(|range| parse_amount_range(range.minimum, range.maximum))
                .transpose()?,
            period_limits
                .into_iter()
                .map(|limit| {
                    parse_period(
                        limit.period.kind,
                        limit.period.every,
                        limit.period.unit,
                        limit.period.anchor,
                    )
                    .and_then(|period| {
                        parse_period_limit(limit.amount_limit, limit.payment_count_limit, period)
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            lifetime_amount_limit,
            active_from,
            expires_at,
            allowed_payment_endpoint_identifiers,
        )
    })();
    result.map_err(|_| invalid_allowance_terms())
}

fn invalid_allowance_terms() -> PaykitFfiError {
    validation_error("Allowance Terms are invalid")
}

#[allow(clippy::too_many_arguments)]
fn build_allowance_terms(
    asset: String,
    per_payment_amount: Option<AllowanceAmountRange>,
    period_limits: Vec<AllowancePeriodLimit>,
    lifetime_amount_limit: Option<String>,
    active_from: Option<String>,
    expires_at: Option<String>,
    allowed_payment_endpoint_identifiers: Option<Vec<String>>,
) -> Result<AllowanceTerms, PaykitFfiError> {
    let mut builder = AllowanceTerms::builder(asset);
    if let Some(range) = per_payment_amount {
        builder = builder.per_payment_amount(range);
    }
    builder = builder.period_limits(period_limits);
    if let Some(limit) = lifetime_amount_limit {
        builder = builder.lifetime_amount_limit(limit);
    }
    if let Some(active_from) = active_from {
        builder = builder.active_from(active_from);
    }
    if let Some(expires_at) = expires_at {
        builder = builder.expires_at(expires_at);
    }
    if let Some(identifiers) = allowed_payment_endpoint_identifiers {
        builder = builder.allowed_payment_endpoint_identifiers(
            identifiers
                .into_iter()
                .map(|identifier| {
                    parse_endpoint_identifier(identifier).map_err(|_| {
                        validation_error(
                            "Allowance Terms Payment Endpoint Identifier allowlist is invalid",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    builder
        .build()
        .map_err(|_| validation_error("Allowance Terms are invalid"))
}

pub(super) fn parse_amount_range(
    minimum: String,
    maximum: String,
) -> Result<AllowanceAmountRange, PaykitFfiError> {
    AllowanceAmountRange::new(minimum, maximum)
        .map_err(|_| validation_error("Allowance per-payment amount range is invalid"))
}

pub(super) fn parse_period_limit(
    amount_limit: Option<String>,
    payment_count_limit: Option<u64>,
    period: AllowancePeriod,
) -> Result<AllowancePeriodLimit, PaykitFfiError> {
    AllowancePeriodLimit::new(amount_limit, payment_count_limit, period)
        .map_err(|_| validation_error("Allowance period limit is invalid"))
}

pub(super) fn parse_period(
    kind: String,
    every: u64,
    unit: String,
    anchor: Option<String>,
) -> Result<AllowancePeriod, PaykitFfiError> {
    let unit = parse_period_unit(&unit)?;
    let result = match (kind.as_str(), anchor) {
        ("anchored", Some(anchor)) => AllowancePeriod::anchored(every, unit, anchor),
        ("rolling", None) => AllowancePeriod::rolling(every, unit),
        ("anchored", None) => {
            return Err(validation_error(
                "anchored Allowance period requires an anchor",
            ))
        }
        ("rolling", Some(_)) => {
            return Err(validation_error(
                "rolling Allowance period must not configure an anchor",
            ))
        }
        _ => return Err(validation_error("Allowance period kind is unsupported")),
    };
    result.map_err(|_| validation_error("Allowance period is invalid"))
}

fn parse_period_unit(value: &str) -> Result<AllowancePeriodUnit, PaykitFfiError> {
    match value {
        "minute" => Ok(AllowancePeriodUnit::Minute),
        "hour" => Ok(AllowancePeriodUnit::Hour),
        "day" => Ok(AllowancePeriodUnit::Day),
        "week" => Ok(AllowancePeriodUnit::Week),
        "month" => Ok(AllowancePeriodUnit::Month),
        "year" => Ok(AllowancePeriodUnit::Year),
        _ => Err(validation_error("Allowance period unit is unsupported")),
    }
}

pub(super) fn parse_allowance_id(value: String) -> Result<AllowanceId, PaykitFfiError> {
    AllowanceId::new(value).map_err(|err| validation_error(err.to_string()))
}

pub(super) fn allowance_records_to_ffi(
    records: Vec<AllowanceRecord>,
) -> Result<Vec<FfiAllowanceRecord>, PaykitFfiError> {
    records.into_iter().map(TryInto::try_into).collect()
}
