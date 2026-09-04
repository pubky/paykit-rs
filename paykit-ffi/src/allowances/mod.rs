use std::{fmt, sync::Arc};

use paykit_lib::AllowanceTerms;

use crate::{private_lists::FfiOutboundPrivateMessageStatus, sdk::FfiPaykitSdk, PaykitFfiError};

mod conversions;
#[cfg(test)]
mod tests;

use conversions::parse_allowance_id;

/// Local party role for one Allowance.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiAllowanceLocalRole {
    /// Local identity grants authority and remains the Payer.
    Allower,
    /// Local identity may send qualifying Payment Requests.
    Allowee,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// SDK-derived Allowance consent lifecycle state.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiAllowanceLifecycleState {
    /// One proposal is known and has no controlling response.
    Proposed,
    /// The proposal recipient accepted the immutable terms.
    Accepted,
    /// The proposal recipient rejected the proposal.
    Rejected,
    /// A valid unilateral End is present.
    Ended,
    /// Multiple distinct proposals reused the same Allowance ID.
    Conflicted,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Health of the durable history used to derive one Allowance.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiAllowanceHistoryStatus {
    /// All retained evidence is valid and causally resolved.
    Consistent,
    /// A valid event references evidence that has not been loaded yet.
    UnresolvedReferences,
    /// Malformed, conflicting, or protocol-invalid evidence is present.
    Invalid,
    /// The exact Encrypted Link needs recovery before safe use.
    RecoveryRequired,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

macro_rules! impl_redacted_formatting {
    ($type:ty, $name:literal) => {
        impl fmt::Debug for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!($name, "(<redacted>)"))
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(self, f)
            }
        }
    };
}

/// Inclusive per-payment amount range for Allowance Terms.
///
/// The object and values returned by its getters are sensitive. Its exported
/// default string and debug output are redacted.
#[uniffi::export(Debug, Display)]
#[derive(uniffi::Object)]
pub struct FfiAllowanceAmountRange {
    range: paykit_lib::AllowanceAmountRange,
}

impl_redacted_formatting!(FfiAllowanceAmountRange, "AllowanceAmountRange");

impl FfiAllowanceAmountRange {
    pub(crate) fn from_validated_range(range: paykit_lib::AllowanceAmountRange) -> Self {
        Self { range }
    }

    pub(crate) fn domain_range(&self) -> paykit_lib::AllowanceAmountRange {
        self.range.clone()
    }
}

#[uniffi::export]
impl FfiAllowanceAmountRange {
    /// Validate and create an inclusive per-payment amount range.
    #[uniffi::constructor]
    pub fn new(minimum: String, maximum: String) -> Result<Self, PaykitFfiError> {
        conversions::parse_amount_range(minimum, maximum).map(Self::from_validated_range)
    }

    /// Return the minimum decimal wire spelling.
    pub fn minimum(&self) -> String {
        self.range.minimum().to_owned()
    }

    /// Return the maximum decimal wire spelling.
    pub fn maximum(&self) -> String {
        self.range.maximum().to_owned()
    }
}

/// Anchored or rolling period for an Allowance usage limit.
///
/// The object and values returned by its getters are sensitive. Its exported
/// default string and debug output are redacted.
#[uniffi::export(Debug, Display)]
#[derive(uniffi::Object)]
pub struct FfiAllowancePeriod {
    period: paykit_lib::AllowancePeriod,
}

impl_redacted_formatting!(FfiAllowancePeriod, "AllowancePeriod");

impl FfiAllowancePeriod {
    pub(crate) fn from_validated_period(period: paykit_lib::AllowancePeriod) -> Self {
        Self { period }
    }

    pub(crate) fn domain_period(&self) -> paykit_lib::AllowancePeriod {
        self.period.clone()
    }
}

#[uniffi::export]
impl FfiAllowancePeriod {
    /// Validate and create an anchored or rolling Allowance period.
    #[uniffi::constructor]
    pub fn new(
        kind: String,
        every: u64,
        unit: String,
        anchor: Option<String>,
    ) -> Result<Self, PaykitFfiError> {
        conversions::parse_period(kind, every, unit, anchor).map(Self::from_validated_period)
    }

    /// Return the canonical period kind: `anchored` or `rolling`.
    pub fn kind(&self) -> String {
        self.period.kind().as_str().to_owned()
    }

    /// Return the positive interval multiplier.
    pub fn every(&self) -> u64 {
        self.period.every()
    }

    /// Return the canonical singular interval unit.
    pub fn unit(&self) -> String {
        self.period.unit().as_str().to_owned()
    }

    /// Return the UTC anchor for an anchored period.
    pub fn anchor(&self) -> Option<String> {
        self.period.anchor().map(str::to_owned)
    }
}

/// Amount and/or payment-count ceiling applied over one Allowance period.
///
/// The object and values returned by its getters are sensitive. Its exported
/// default string and debug output are redacted.
#[uniffi::export(Debug, Display)]
#[derive(uniffi::Object)]
pub struct FfiAllowancePeriodLimit {
    limit: paykit_lib::AllowancePeriodLimit,
}

impl_redacted_formatting!(FfiAllowancePeriodLimit, "AllowancePeriodLimit");

impl FfiAllowancePeriodLimit {
    pub(crate) fn from_validated_limit(limit: paykit_lib::AllowancePeriodLimit) -> Self {
        Self { limit }
    }

    pub(crate) fn domain_limit(&self) -> paykit_lib::AllowancePeriodLimit {
        self.limit.clone()
    }
}

#[uniffi::export]
impl FfiAllowancePeriodLimit {
    /// Validate and create an Allowance period limit.
    #[uniffi::constructor]
    pub fn new(
        amount_limit: Option<String>,
        payment_count_limit: Option<u64>,
        period: Arc<FfiAllowancePeriod>,
    ) -> Result<Self, PaykitFfiError> {
        conversions::parse_period_limit(amount_limit, payment_count_limit, period.domain_period())
            .map(Self::from_validated_limit)
    }

    /// Return the optional amount ceiling decimal spelling.
    pub fn amount_limit(&self) -> Option<String> {
        self.limit.amount_limit().map(str::to_owned)
    }

    /// Return the optional payment-count ceiling.
    pub fn payment_count_limit(&self) -> Option<u64> {
        self.limit.payment_count_limit()
    }

    /// Return the period over which the ceilings apply.
    pub fn period(&self) -> Arc<FfiAllowancePeriod> {
        Arc::new(FfiAllowancePeriod::from_validated_period(
            self.limit.period().clone(),
        ))
    }
}

/// Immutable private Allowance Terms with redacted default formatting.
///
/// Applications must treat the object and every value returned by its getters
/// as sensitive. Do not include them in ordinary platform logs or diagnostics.
#[uniffi::export(Debug, Display)]
#[derive(uniffi::Object)]
pub struct FfiAllowanceTerms {
    terms: AllowanceTerms,
}

impl_redacted_formatting!(FfiAllowanceTerms, "AllowanceTerms");

impl FfiAllowanceTerms {
    pub(crate) fn from_validated_terms(terms: AllowanceTerms) -> Self {
        Self { terms }
    }

    pub(crate) fn domain_terms(&self) -> AllowanceTerms {
        self.terms.clone()
    }
}

#[uniffi::export]
impl FfiAllowanceTerms {
    /// Validate and create immutable Allowance Terms.
    #[uniffi::constructor]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        asset: String,
        per_payment_amount: Option<Arc<FfiAllowanceAmountRange>>,
        period_limits: Vec<Arc<FfiAllowancePeriodLimit>>,
        lifetime_amount_limit: Option<String>,
        active_from: Option<String>,
        expires_at: Option<String>,
        allowed_payment_endpoint_identifiers: Option<Vec<String>>,
    ) -> Result<Self, PaykitFfiError> {
        conversions::build_allowance_terms(
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
        )
        .map(Self::from_validated_terms)
    }

    /// Return the exact, case-sensitive asset.
    pub fn asset(&self) -> String {
        self.terms.asset().to_owned()
    }

    /// Return the optional inclusive per-payment amount range.
    pub fn per_payment_amount(&self) -> Option<Arc<FfiAllowanceAmountRange>> {
        self.terms
            .per_payment_amount()
            .map(|range| Arc::new(FfiAllowanceAmountRange::from_validated_range(range.clone())))
    }

    /// Return every independently applicable period limit.
    pub fn period_limits(&self) -> Vec<Arc<FfiAllowancePeriodLimit>> {
        self.terms
            .period_limits()
            .iter()
            .map(|limit| Arc::new(FfiAllowancePeriodLimit::from_validated_limit(limit.clone())))
            .collect()
    }

    /// Return the optional lifetime amount ceiling decimal spelling.
    pub fn lifetime_amount_limit(&self) -> Option<String> {
        self.terms.lifetime_amount_limit().map(str::to_owned)
    }

    /// Return the optional inclusive first eligible instant.
    pub fn active_from(&self) -> Option<String> {
        self.terms.active_from().map(str::to_owned)
    }

    /// Return the optional exclusive first ineligible instant.
    pub fn expires_at(&self) -> Option<String> {
        self.terms.expires_at().map(str::to_owned)
    }

    /// Return the optional exact Payment Endpoint Identifier allowlist.
    pub fn allowed_payment_endpoint_identifiers(&self) -> Option<Vec<String>> {
        self.terms
            .allowed_payment_endpoint_identifiers()
            .map(|identifiers| {
                identifiers
                    .iter()
                    .map(|identifier| identifier.as_str().to_owned())
                    .collect()
            })
    }
}

/// Filter for listing SDK-derived Allowances.
#[derive(uniffi::Record, Clone, Debug, Default, PartialEq, Eq)]
pub struct FfiAllowanceFilter {
    /// Restrict results to one counterparty.
    pub counterparty: Option<String>,
    /// Restrict results to one counterparty receiver/runtime folder.
    pub counterparty_receiver_path: Option<String>,
    /// Restrict results to one local Allowance role.
    pub local_role: Option<FfiAllowanceLocalRole>,
    /// Restrict results to lifecycle states. Empty means all states.
    pub states: Vec<FfiAllowanceLifecycleState>,
}

/// SDK-derived record for one Allowance on one exact Encrypted Link.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiAllowanceRecord {
    /// Counterparty associated with the authenticated private history.
    pub counterparty: String,
    /// Counterparty receiver/runtime folder associated with the history.
    pub counterparty_receiver_path: String,
    /// Stable Allowance ID.
    pub allowance_id: String,
    /// Local role derived from the authenticated proposal source.
    pub local_role: Option<FfiAllowanceLocalRole>,
    /// Derived consent lifecycle state.
    pub state: FfiAllowanceLifecycleState,
    /// Health of the evidence used for derivation.
    pub history_status: FfiAllowanceHistoryStatus,
    /// Proposal Event ID.
    pub proposal_event_id: Option<String>,
    /// Immutable private proposed terms.
    pub terms: Option<Arc<FfiAllowanceTerms>>,
    /// Inbound stream item carrying the proposal, when received.
    pub proposal_stream_item_id: Option<u64>,
    /// Outbound message carrying the proposal, when locally queued.
    pub proposal_outbound_message_id: Option<u64>,
    /// Local delivery status of an outbound proposal.
    pub proposal_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Controlling Acceptance Event ID.
    pub acceptance_event_id: Option<String>,
    /// Local delivery status of an outbound acceptance.
    pub acceptance_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Controlling Rejection Event ID.
    pub rejection_event_id: Option<String>,
    /// Local delivery status of an outbound rejection.
    pub rejection_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Valid End Event ID retained by the SDK.
    pub end_event_id: Option<String>,
    /// Local delivery status of an outbound End.
    pub end_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Causal Event IDs not yet present in durable history.
    pub pending_causal_event_ids: Vec<String>,
    /// Event IDs whose reuse or proposal collision taints this Allowance.
    pub conflict_event_ids: Vec<String>,
    /// Last inbound stream item associated with this Allowance.
    pub last_stream_item_id: Option<u64>,
    /// Last outbound message associated with this Allowance.
    pub last_outbound_message_id: Option<u64>,
    /// Delivery status of the last associated outbound message.
    pub last_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Latest local record time as RFC3339 text, used only for presentation ordering.
    pub last_event_at: Option<String>,
    /// Redaction-safe SDK reason for invalid history, when available.
    pub invalid_reason: Option<String>,
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Return Allowances matching a local SDK filter, newest first by local record time (presentation order only).
    pub async fn list_allowances(
        &self,
        filter: FfiAllowanceFilter,
    ) -> Result<Vec<FfiAllowanceRecord>, PaykitFfiError> {
        let filter = filter.try_into()?;
        self.runtime
            .list_allowances(filter)
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    /// Return one Allowance from one exact authenticated Encrypted Link.
    pub async fn get_allowance(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
        allowance_id: String,
    ) -> Result<Option<FfiAllowanceRecord>, PaykitFfiError> {
        let counterparty = crate::session::parse_public_key(counterparty)?;
        let receiver_path = crate::session::parse_receiver_path(counterparty_receiver_path)?;
        let allowance_id = parse_allowance_id(allowance_id)?;
        self.runtime
            .allowance_record(&counterparty, &receiver_path, &allowance_id)
            .await?
            .map(TryInto::try_into)
            .transpose()
    }

    /// Queue a new Allowance proposal and return local derived state.
    pub async fn propose_allowance(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
        local_role: FfiAllowanceLocalRole,
        terms: Arc<FfiAllowanceTerms>,
    ) -> Result<FfiAllowanceRecord, PaykitFfiError> {
        let counterparty = crate::session::parse_public_key(counterparty)?;
        let receiver_path = crate::session::parse_receiver_path(counterparty_receiver_path)?;
        let local_role = local_role.try_into()?;
        self.runtime
            .propose_allowance(
                counterparty,
                receiver_path,
                local_role,
                terms.domain_terms(),
            )
            .await?
            .try_into()
    }

    /// Queue acceptance for a received Allowance proposal.
    pub async fn accept_allowance(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
        allowance_id: String,
    ) -> Result<FfiAllowanceRecord, PaykitFfiError> {
        let counterparty = crate::session::parse_public_key(counterparty)?;
        let receiver_path = crate::session::parse_receiver_path(counterparty_receiver_path)?;
        let allowance_id = parse_allowance_id(allowance_id)?;
        self.runtime
            .accept_allowance(counterparty, receiver_path, &allowance_id)
            .await?
            .try_into()
    }

    /// Queue rejection for a received Allowance proposal.
    pub async fn reject_allowance(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
        allowance_id: String,
    ) -> Result<FfiAllowanceRecord, PaykitFfiError> {
        let counterparty = crate::session::parse_public_key(counterparty)?;
        let receiver_path = crate::session::parse_receiver_path(counterparty_receiver_path)?;
        let allowance_id = parse_allowance_id(allowance_id)?;
        self.runtime
            .reject_allowance(counterparty, receiver_path, &allowance_id)
            .await?
            .try_into()
    }

    /// Queue a proposal withdrawal or unilateral End for accepted authority.
    pub async fn end_allowance(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
        allowance_id: String,
    ) -> Result<FfiAllowanceRecord, PaykitFfiError> {
        let counterparty = crate::session::parse_public_key(counterparty)?;
        let receiver_path = crate::session::parse_receiver_path(counterparty_receiver_path)?;
        let allowance_id = parse_allowance_id(allowance_id)?;
        self.runtime
            .end_allowance(counterparty, receiver_path, &allowance_id)
            .await?
            .try_into()
    }
}
