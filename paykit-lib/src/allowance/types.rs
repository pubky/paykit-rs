use std::{collections::HashSet, fmt, hash::Hash};

use crate::{
    validation::{
        parse_utc_timestamp, validate_asset_text, validate_decimal_text, validate_uuid_v4,
    },
    EventId, PaykitError, PaymentEndpointIdentifier, PrivateMessageKind, Result,
};

/// Protocol version of every Allowance Event Message this module produces.
const ALLOWANCE_V1_VERSION: u8 = 1;

/// SECURITY / REDACTION: Allowance Terms are private counterparty data and
/// must never reach Debug or log output, so every terms-bearing type prints a
/// fixed `Name(<redacted>)` marker instead of its fields.
macro_rules! redacted_debug {
    ($($ty:ident),* $(,)?) => {$(
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($ty), "(<redacted>)"))
            }
        }
    )*};
}

redacted_debug!(
    AllowanceAmountRange,
    AllowancePeriod,
    AllowancePeriodLimit,
    AllowanceTerms,
    AllowanceTermsBuilder,
);

/// UUID-v4 identifier shared by one Allowance lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AllowanceId(String);

impl AllowanceId {
    /// Create an Allowance ID, canonicalizing a valid UUID-v4 spelling.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        validate_uuid_v4(id.into(), "Allowance ID").map(Self)
    }

    /// Generate a fresh Allowance ID.
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Access the canonical lowercase, hyphenated UUID string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AllowanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for AllowanceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A party's role in one Allowance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllowanceRole {
    /// Party granting authority for automatic payment handling.
    Allower,
    /// Authenticated Payment Request sender whose requests may use the authority.
    Allowee,
}

impl AllowanceRole {
    /// Return the canonical wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allower => "allower",
            Self::Allowee => "allowee",
        }
    }

    /// Return the counterparty role.
    pub(crate) fn counterparty(self) -> Self {
        match self {
            Self::Allower => Self::Allowee,
            Self::Allowee => Self::Allower,
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "allower" => Ok(Self::Allower),
            "allowee" => Ok(Self::Allowee),
            _ => Err(PaykitError::Validation(
                "Allowance proposer_role must be allower or allowee".into(),
            )),
        }
    }
}

impl fmt::Display for AllowanceRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Inclusive per-payment amount range, preserving decimal wire spellings.
#[derive(Clone, PartialEq, Eq)]
pub struct AllowanceAmountRange {
    minimum: String,
    maximum: String,
}

impl AllowanceAmountRange {
    /// Create an inclusive range whose minimum is no greater than its maximum.
    pub fn new(minimum: impl Into<String>, maximum: impl Into<String>) -> Result<Self> {
        let minimum = minimum.into();
        let maximum = maximum.into();
        validate_decimal_text(&minimum, "Allowance minimum")?;
        validate_decimal_text(&maximum, "Allowance maximum")?;
        if compare_decimals(&minimum, &maximum).is_gt() {
            return Err(PaykitError::Validation(
                "Allowance minimum must not exceed maximum".into(),
            ));
        }
        Ok(Self { minimum, maximum })
    }

    /// Access the minimum decimal spelling.
    pub fn minimum(&self) -> &str {
        &self.minimum
    }

    /// Access the maximum decimal spelling.
    pub fn maximum(&self) -> &str {
        &self.maximum
    }
}

/// Usage-period shape used by an Allowance limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AllowancePeriodKind {
    /// Periods aligned to a fixed UTC anchor.
    Anchored,
    /// Fixed-duration window ending at the evaluation instant.
    Rolling,
}

impl AllowancePeriodKind {
    /// Return the canonical wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anchored => "anchored",
            Self::Rolling => "rolling",
        }
    }
}

/// Unit used by an Allowance period.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AllowancePeriodUnit {
    /// Minute interval.
    Minute,
    /// Hour interval.
    Hour,
    /// Day interval.
    Day,
    /// Week interval.
    Week,
    /// Calendar-month interval (anchored periods only).
    Month,
    /// Calendar-year interval (anchored periods only).
    Year,
}

impl AllowancePeriodUnit {
    /// Return the canonical singular wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "minute" => Ok(Self::Minute),
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            "year" => Ok(Self::Year),
            _ => Err(PaykitError::Validation(
                "Allowance period unit is unsupported".into(),
            )),
        }
    }
}

impl fmt::Display for AllowancePeriodUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Validated anchored or rolling period used by an Allowance limit.
///
/// The period is anchored exactly when `anchor` is present.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AllowancePeriod {
    every: u64,
    unit: AllowancePeriodUnit,
    anchor: Option<String>,
}

impl AllowancePeriod {
    /// Create an anchored period.
    pub fn anchored(
        every: u64,
        unit: AllowancePeriodUnit,
        anchor: impl Into<String>,
    ) -> Result<Self> {
        if every == 0 {
            return Err(PaykitError::Validation(
                "Allowance period every must be positive".into(),
            ));
        }
        let anchor = anchor.into();
        parse_utc_timestamp(&anchor, "Allowance period anchor")?;
        Ok(Self {
            every,
            unit,
            anchor: Some(anchor),
        })
    }

    /// Create a rolling fixed-duration period.
    ///
    /// Rolling month and year periods are rejected because their duration is
    /// calendar-dependent.
    pub fn rolling(every: u64, unit: AllowancePeriodUnit) -> Result<Self> {
        if every == 0 {
            return Err(PaykitError::Validation(
                "Allowance period every must be positive".into(),
            ));
        }
        if matches!(unit, AllowancePeriodUnit::Month | AllowancePeriodUnit::Year) {
            return Err(PaykitError::Validation(
                "rolling Allowance periods do not support month or year".into(),
            ));
        }
        Ok(Self {
            every,
            unit,
            anchor: None,
        })
    }

    /// Access the period kind.
    pub fn kind(&self) -> AllowancePeriodKind {
        if self.anchor.is_some() {
            AllowancePeriodKind::Anchored
        } else {
            AllowancePeriodKind::Rolling
        }
    }

    /// Access the positive interval multiplier.
    pub fn every(&self) -> u64 {
        self.every
    }

    /// Access the interval unit.
    pub fn unit(&self) -> AllowancePeriodUnit {
        self.unit
    }

    /// Access the UTC anchor for an anchored period.
    pub fn anchor(&self) -> Option<&str> {
        self.anchor.as_deref()
    }
}

/// Amount and/or payment-count ceiling applied over one period.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AllowancePeriodLimit {
    amount_limit: Option<String>,
    payment_count_limit: Option<u64>,
    period: AllowancePeriod,
}

impl AllowancePeriodLimit {
    /// Create a period limit. At least one ceiling must be present.
    pub fn new(
        amount_limit: Option<String>,
        payment_count_limit: Option<u64>,
        period: AllowancePeriod,
    ) -> Result<Self> {
        if amount_limit.is_none() && payment_count_limit.is_none() {
            return Err(PaykitError::Validation(
                "Allowance period limit must configure an amount or payment count".into(),
            ));
        }
        if let Some(amount) = &amount_limit {
            validate_decimal_text(amount, "Allowance period amount_limit")?;
        }
        Ok(Self {
            amount_limit,
            payment_count_limit,
            period,
        })
    }

    /// Access the optional amount ceiling decimal spelling.
    pub fn amount_limit(&self) -> Option<&str> {
        self.amount_limit.as_deref()
    }

    /// Access the optional payment-count ceiling.
    pub fn payment_count_limit(&self) -> Option<u64> {
        self.payment_count_limit
    }

    /// Access the period definition.
    pub fn period(&self) -> &AllowancePeriod {
        &self.period
    }
}

/// Immutable constraints proposed for one Allowance.
#[derive(Clone, PartialEq, Eq)]
pub struct AllowanceTerms {
    pub(super) asset: String,
    pub(super) per_payment_amount: Option<AllowanceAmountRange>,
    pub(super) period_limits: Vec<AllowancePeriodLimit>,
    pub(super) lifetime_amount_limit: Option<String>,
    pub(super) active_from: Option<String>,
    pub(super) expires_at: Option<String>,
    pub(super) allowed_payment_endpoint_identifiers: Option<Vec<PaymentEndpointIdentifier>>,
}

impl AllowanceTerms {
    /// Start building Allowance Terms for an exact, case-sensitive asset.
    pub fn builder(asset: impl Into<String>) -> AllowanceTermsBuilder {
        AllowanceTermsBuilder::new(asset)
    }

    /// Access the exact asset string.
    pub fn asset(&self) -> &str {
        &self.asset
    }

    /// Access the optional inclusive per-payment range.
    pub fn per_payment_amount(&self) -> Option<&AllowanceAmountRange> {
        self.per_payment_amount.as_ref()
    }

    /// Access all independently applicable period limits.
    pub fn period_limits(&self) -> &[AllowancePeriodLimit] {
        &self.period_limits
    }

    /// Access the optional lifetime amount ceiling decimal spelling.
    pub fn lifetime_amount_limit(&self) -> Option<&str> {
        self.lifetime_amount_limit.as_deref()
    }

    /// Access the optional inclusive first eligible instant.
    pub fn active_from(&self) -> Option<&str> {
        self.active_from.as_deref()
    }

    /// Access the optional exclusive first ineligible instant.
    pub fn expires_at(&self) -> Option<&str> {
        self.expires_at.as_deref()
    }

    /// Access the optional non-empty Payment Endpoint Identifier allowlist.
    pub fn allowed_payment_endpoint_identifiers(&self) -> Option<&[PaymentEndpointIdentifier]> {
        self.allowed_payment_endpoint_identifiers.as_deref()
    }

    pub(super) fn validate(&self) -> Result<()> {
        validate_asset_text(&self.asset, "Allowance asset")?;
        if let Some(limit) = &self.lifetime_amount_limit {
            validate_decimal_text(limit, "Allowance lifetime_amount_limit")?;
        }
        validate_time_window(self.active_from.as_deref(), self.expires_at.as_deref())?;
        validate_unique_period_limits(&self.period_limits)?;
        validate_allowlist(self.allowed_payment_endpoint_identifiers.as_deref())?;
        if self.per_payment_amount.is_none()
            && self.period_limits.is_empty()
            && self.lifetime_amount_limit.is_none()
            && self.active_from.is_none()
            && self.expires_at.is_none()
            && self.allowed_payment_endpoint_identifiers.is_none()
        {
            return Err(PaykitError::Validation(
                "Allowance Terms must constrain authority beyond asset".into(),
            ));
        }
        Ok(())
    }
}

/// Builder for immutable [`AllowanceTerms`].
///
/// Holds unvalidated terms; [`AllowanceTermsBuilder::build`] validates them.
#[derive(Clone)]
pub struct AllowanceTermsBuilder(AllowanceTerms);

impl AllowanceTermsBuilder {
    /// Create a builder for an exact, case-sensitive asset.
    pub fn new(asset: impl Into<String>) -> Self {
        Self(AllowanceTerms {
            asset: asset.into(),
            per_payment_amount: None,
            period_limits: Vec::new(),
            lifetime_amount_limit: None,
            active_from: None,
            expires_at: None,
            allowed_payment_endpoint_identifiers: None,
        })
    }

    /// Set the inclusive per-payment amount range.
    pub fn per_payment_amount(mut self, range: AllowanceAmountRange) -> Self {
        self.0.per_payment_amount = Some(range);
        self
    }

    /// Replace the independently applicable period limits.
    pub fn period_limits(mut self, limits: Vec<AllowancePeriodLimit>) -> Self {
        self.0.period_limits = limits;
        self
    }

    /// Set the lifetime amount ceiling.
    pub fn lifetime_amount_limit(mut self, limit: impl Into<String>) -> Self {
        self.0.lifetime_amount_limit = Some(limit.into());
        self
    }

    /// Set the inclusive first eligible instant.
    pub fn active_from(mut self, active_from: impl Into<String>) -> Self {
        self.0.active_from = Some(active_from.into());
        self
    }

    /// Set the exclusive first ineligible instant.
    pub fn expires_at(mut self, expires_at: impl Into<String>) -> Self {
        self.0.expires_at = Some(expires_at.into());
        self
    }

    /// Set the exact Payment Endpoint Identifier allowlist.
    pub fn allowed_payment_endpoint_identifiers(
        mut self,
        identifiers: Vec<PaymentEndpointIdentifier>,
    ) -> Self {
        self.0.allowed_payment_endpoint_identifiers = Some(identifiers);
        self
    }

    /// Validate all fields and build immutable Allowance Terms.
    pub fn build(self) -> Result<AllowanceTerms> {
        self.0.validate()?;
        Ok(self.0)
    }
}

/// Accessors shared by every V1 Allowance event header.
macro_rules! event_header_accessors {
    ($kind:expr) => {
        /// Access the protocol version.
        pub fn version(&self) -> u8 {
            ALLOWANCE_V1_VERSION
        }

        /// Access the Private Message Kind.
        pub fn kind(&self) -> PrivateMessageKind {
            $kind
        }

        /// Access the Event ID.
        pub fn event_id(&self) -> &EventId {
            &self.event_id
        }

        /// Access the Allowance ID.
        pub fn allowance_id(&self) -> &AllowanceId {
            &self.allowance_id
        }
    };
}

/// Header accessors plus the Proposal reference shared by response events.
macro_rules! event_accessors {
    ($kind:expr) => {
        event_header_accessors!($kind);

        /// Access the referenced Proposal Event ID.
        pub fn proposal_event_id(&self) -> &EventId {
            &self.proposal_event_id
        }
    };
}

/// Proposal Event Message for exact Allowance Terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceProposal {
    event_id: EventId,
    allowance_id: AllowanceId,
    proposer_role: AllowanceRole,
    terms: AllowanceTerms,
}

impl AllowanceProposal {
    /// Create a V1 Allowance Proposal.
    pub fn new(
        event_id: EventId,
        allowance_id: AllowanceId,
        proposer_role: AllowanceRole,
        terms: AllowanceTerms,
    ) -> Self {
        Self {
            event_id,
            allowance_id,
            proposer_role,
            terms,
        }
    }

    event_header_accessors!(PrivateMessageKind::AllowanceProposal);

    /// Access the authenticated proposal sender's assigned role.
    pub fn proposer_role(&self) -> AllowanceRole {
        self.proposer_role
    }

    /// Access the proposal recipient's assigned role.
    pub fn recipient_role(&self) -> AllowanceRole {
        self.proposer_role.counterparty()
    }

    /// Access the immutable Allowance Terms.
    pub fn terms(&self) -> &AllowanceTerms {
        &self.terms
    }
}

/// Acceptance Event Message correlated to one Allowance Proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceAcceptance {
    event_id: EventId,
    allowance_id: AllowanceId,
    proposal_event_id: EventId,
}

impl AllowanceAcceptance {
    /// Create a V1 Allowance Acceptance with a distinct causal Event ID.
    pub fn new(
        event_id: EventId,
        allowance_id: AllowanceId,
        proposal_event_id: EventId,
    ) -> Result<Self> {
        validate_distinct_event_ids(&event_id, &proposal_event_id, None)?;
        Ok(Self {
            event_id,
            allowance_id,
            proposal_event_id,
        })
    }

    event_accessors!(PrivateMessageKind::AllowanceAcceptance);

    /// Check causal references and that the authenticated sender is the proposal recipient.
    pub fn validate_for_proposal(
        &self,
        proposal: &AllowanceProposal,
        authenticated_sender_role: AllowanceRole,
    ) -> Result<()> {
        validate_response(
            &self.allowance_id,
            &self.proposal_event_id,
            proposal,
            authenticated_sender_role,
        )
    }
}

/// Rejection Event Message correlated to one Allowance Proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceRejection {
    event_id: EventId,
    allowance_id: AllowanceId,
    proposal_event_id: EventId,
}

impl AllowanceRejection {
    /// Create a V1 Allowance Rejection with a distinct causal Event ID.
    pub fn new(
        event_id: EventId,
        allowance_id: AllowanceId,
        proposal_event_id: EventId,
    ) -> Result<Self> {
        validate_distinct_event_ids(&event_id, &proposal_event_id, None)?;
        Ok(Self {
            event_id,
            allowance_id,
            proposal_event_id,
        })
    }

    event_accessors!(PrivateMessageKind::AllowanceRejection);

    /// Check causal references and that the authenticated sender is the proposal recipient.
    pub fn validate_for_proposal(
        &self,
        proposal: &AllowanceProposal,
        authenticated_sender_role: AllowanceRole,
    ) -> Result<()> {
        validate_response(
            &self.allowance_id,
            &self.proposal_event_id,
            proposal,
            authenticated_sender_role,
        )
    }
}

/// Terminal End Event Message for a proposed or accepted Allowance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceEnd {
    event_id: EventId,
    allowance_id: AllowanceId,
    proposal_event_id: EventId,
    acceptance_event_id: Option<EventId>,
}

impl AllowanceEnd {
    /// Create a proposal withdrawal with a null Acceptance Event ID.
    ///
    /// The End and Proposal Event IDs must be distinct.
    pub fn withdrawal(
        event_id: EventId,
        allowance_id: AllowanceId,
        proposal_event_id: EventId,
    ) -> Result<Self> {
        Self::new(event_id, allowance_id, proposal_event_id, None)
    }

    /// Create an End for accepted authority with pairwise-distinct Event IDs.
    pub fn accepted(
        event_id: EventId,
        allowance_id: AllowanceId,
        proposal_event_id: EventId,
        acceptance_event_id: EventId,
    ) -> Result<Self> {
        Self::new(
            event_id,
            allowance_id,
            proposal_event_id,
            Some(acceptance_event_id),
        )
    }

    pub(super) fn new(
        event_id: EventId,
        allowance_id: AllowanceId,
        proposal_event_id: EventId,
        acceptance_event_id: Option<EventId>,
    ) -> Result<Self> {
        validate_distinct_event_ids(&event_id, &proposal_event_id, acceptance_event_id.as_ref())?;
        Ok(Self {
            event_id,
            allowance_id,
            proposal_event_id,
            acceptance_event_id,
        })
    }

    event_accessors!(PrivateMessageKind::AllowanceEnd);

    /// Access the Acceptance Event ID, or `None` for a proposal withdrawal.
    pub fn acceptance_event_id(&self) -> Option<&EventId> {
        self.acceptance_event_id.as_ref()
    }

    /// Validate a proposal withdrawal by the authenticated proposal sender.
    pub fn validate_withdrawal_for_proposal(
        &self,
        proposal: &AllowanceProposal,
        authenticated_sender_role: AllowanceRole,
    ) -> Result<()> {
        validate_common_correlation(&self.allowance_id, &self.proposal_event_id, proposal)?;
        if self.acceptance_event_id.is_some() {
            return Err(PaykitError::Validation(
                "Allowance withdrawal must not reference an Acceptance Event".into(),
            ));
        }
        if authenticated_sender_role != proposal.proposer_role {
            return Err(PaykitError::Validation(
                "Allowance withdrawal sender must be the proposal sender".into(),
            ));
        }
        Ok(())
    }

    /// Validate accepted authority and its causal End references.
    ///
    /// The Acceptance sender must be the proposal recipient. Accepted authority
    /// may be ended by either party, so the caller remains responsible for
    /// establishing that the End arrived on the proposal's exact authenticated
    /// Encrypted Link.
    pub fn validate_for_accepted_allowance(
        &self,
        proposal: &AllowanceProposal,
        acceptance: &AllowanceAcceptance,
        acceptance_authenticated_sender_role: AllowanceRole,
    ) -> Result<()> {
        acceptance.validate_for_proposal(proposal, acceptance_authenticated_sender_role)?;
        validate_common_correlation(&self.allowance_id, &self.proposal_event_id, proposal)?;
        if self.acceptance_event_id.as_ref() != Some(&acceptance.event_id) {
            return Err(PaykitError::Validation(
                "Allowance End must reference the bound Acceptance Event".into(),
            ));
        }
        Ok(())
    }
}

/// Any V1 Allowance lifecycle Event Message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllowanceEvent {
    /// Proposal event.
    Proposal(AllowanceProposal),
    /// Acceptance event.
    Acceptance(AllowanceAcceptance),
    /// Rejection event.
    Rejection(AllowanceRejection),
    /// End event.
    End(AllowanceEnd),
}

impl AllowanceEvent {
    /// Access the Private Message Kind.
    pub fn kind(&self) -> PrivateMessageKind {
        match self {
            Self::Proposal(event) => event.kind(),
            Self::Acceptance(event) => event.kind(),
            Self::Rejection(event) => event.kind(),
            Self::End(event) => event.kind(),
        }
    }

    /// Access the Event ID.
    pub fn event_id(&self) -> &EventId {
        match self {
            Self::Proposal(event) => &event.event_id,
            Self::Acceptance(event) => &event.event_id,
            Self::Rejection(event) => &event.event_id,
            Self::End(event) => &event.event_id,
        }
    }

    /// Access the Allowance ID.
    pub fn allowance_id(&self) -> &AllowanceId {
        match self {
            Self::Proposal(event) => &event.allowance_id,
            Self::Acceptance(event) => &event.allowance_id,
            Self::Rejection(event) => &event.allowance_id,
            Self::End(event) => &event.allowance_id,
        }
    }
}

/// A recognized Allowance Event Message plus its redacted parse result.
#[derive(Clone, PartialEq, Eq)]
pub struct AllowanceEventMessage {
    pub(super) kind: PrivateMessageKind,
    pub(super) event_id: Option<EventId>,
    pub(super) allowance_id: Option<AllowanceId>,
    pub(super) raw_json: String,
    pub(super) event: std::result::Result<AllowanceEvent, String>,
}

impl AllowanceEventMessage {
    /// Access the recognized Private Message Kind.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }

    /// Whether structural parsing and validation succeeded.
    pub fn is_valid(&self) -> bool {
        self.event.is_ok()
    }

    /// Access the parsed event when validation succeeded.
    pub fn parsed_event(&self) -> Option<&AllowanceEvent> {
        self.event.as_ref().ok()
    }

    /// Access the redacted validation error when validation failed.
    pub fn validation_error(&self) -> Option<&str> {
        self.event.as_ref().err().map(String::as_str)
    }

    /// Access the Event ID when the top-level value is a canonical UUID-v4.
    pub fn event_id(&self) -> Option<&EventId> {
        self.event_id.as_ref()
    }

    /// Access the Allowance ID when the top-level value is a canonical UUID-v4.
    pub fn allowance_id(&self) -> Option<&AllowanceId> {
        self.allowance_id.as_ref()
    }

    /// Access the exact raw JSON plaintext for durable storage.
    ///
    /// Treat this value as private and never log it.
    pub fn raw_json(&self) -> &str {
        &self.raw_json
    }
}

impl fmt::Debug for AllowanceEventMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AllowanceEventMessage")
            .field("kind", &self.kind)
            .field("event_id", &self.event_id)
            .field("allowance_id", &self.allowance_id)
            .field(
                "raw_json",
                &format_args!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field(
                "parsed_kind",
                &self.event.as_ref().ok().map(AllowanceEvent::kind),
            )
            .field("validation_error", &self.validation_error())
            .finish()
    }
}

fn validate_response(
    allowance_id: &AllowanceId,
    proposal_event_id: &EventId,
    proposal: &AllowanceProposal,
    authenticated_sender_role: AllowanceRole,
) -> Result<()> {
    validate_common_correlation(allowance_id, proposal_event_id, proposal)?;
    if authenticated_sender_role != proposal.recipient_role() {
        return Err(PaykitError::Validation(
            "Allowance response sender must be the proposal recipient".into(),
        ));
    }
    Ok(())
}

fn validate_distinct_event_ids(
    event_id: &EventId,
    proposal_event_id: &EventId,
    acceptance_event_id: Option<&EventId>,
) -> Result<()> {
    if event_id == proposal_event_id
        || acceptance_event_id.is_some_and(|acceptance_event_id| {
            acceptance_event_id == event_id || acceptance_event_id == proposal_event_id
        })
    {
        return Err(PaykitError::Validation(
            "Allowance lifecycle Event IDs must be pairwise distinct".into(),
        ));
    }
    Ok(())
}

fn validate_common_correlation(
    allowance_id: &AllowanceId,
    proposal_event_id: &EventId,
    proposal: &AllowanceProposal,
) -> Result<()> {
    if allowance_id != &proposal.allowance_id || proposal_event_id != &proposal.event_id {
        return Err(PaykitError::Validation(
            "Allowance event does not reference the bound proposal".into(),
        ));
    }
    Ok(())
}

fn compare_decimals(left: &str, right: &str) -> std::cmp::Ordering {
    let (left_integer, left_fraction) = decimal_parts(left);
    let (right_integer, right_fraction) = decimal_parts(right);
    left_integer
        .len()
        .cmp(&right_integer.len())
        .then_with(|| left_integer.cmp(right_integer))
        .then_with(|| compare_fraction(left_fraction, right_fraction))
}

fn decimal_parts(value: &str) -> (&str, &str) {
    let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    (integer, fraction.trim_end_matches('0'))
}

fn compare_fraction(left: &str, right: &str) -> std::cmp::Ordering {
    let width = left.len().max(right.len());
    left.bytes()
        .chain(std::iter::repeat(b'0'))
        .take(width)
        .cmp(right.bytes().chain(std::iter::repeat(b'0')).take(width))
}

fn validate_time_window(active_from: Option<&str>, expires_at: Option<&str>) -> Result<()> {
    let active = active_from
        .map(|value| parse_utc_timestamp(value, "Allowance active_from"))
        .transpose()?;
    let expires = expires_at
        .map(|value| parse_utc_timestamp(value, "Allowance expires_at"))
        .transpose()?;
    if let (Some(active), Some(expires)) = (active, expires) {
        if expires <= active {
            return Err(PaykitError::Validation(
                "Allowance expires_at must be after active_from".into(),
            ));
        }
    }
    Ok(())
}

fn validate_unique_period_limits(limits: &[AllowancePeriodLimit]) -> Result<()> {
    if !all_unique(limits) {
        return Err(PaykitError::Validation(
            "Allowance period_limits must contain unique entries".into(),
        ));
    }
    Ok(())
}

fn validate_allowlist(identifiers: Option<&[PaymentEndpointIdentifier]>) -> Result<()> {
    let Some(identifiers) = identifiers else {
        return Ok(());
    };
    if identifiers.is_empty() {
        return Err(PaykitError::Validation(
            "Allowance endpoint allowlist must not be empty".into(),
        ));
    }
    if !all_unique(identifiers) {
        return Err(PaykitError::Validation(
            "Allowance endpoint allowlist must contain unique identifiers".into(),
        ));
    }
    Ok(())
}

fn all_unique<T: Eq + Hash>(items: &[T]) -> bool {
    let mut seen = HashSet::with_capacity(items.len());
    items.iter().all(|item| seen.insert(item))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowance::test_fixtures::{minimal_proposal, proposal_with_terms};

    #[test]
    fn test_amount_range_compares_decimals_exactly() {
        assert!(AllowanceAmountRange::new(
            "999999999999999999999999.9",
            "1000000000000000000000000"
        )
        .is_ok());
        assert!(AllowanceAmountRange::new("1.000", "1").is_ok());
        assert!(AllowanceAmountRange::new(".51", ".509").is_err());
        assert!(AllowanceAmountRange::new("1", "-1").is_err());
    }

    #[test]
    fn test_period_constructors_reject_invalid_shapes() {
        assert!(AllowancePeriod::rolling(0, AllowancePeriodUnit::Day).is_err());
        assert!(AllowancePeriod::rolling(1, AllowancePeriodUnit::Month).is_err());
        assert!(AllowancePeriod::anchored(
            1,
            AllowancePeriodUnit::Month,
            "2026-01-31T00:00:00+00:00"
        )
        .is_err());
    }

    #[test]
    fn test_terms_reject_unconstrained_and_duplicate_rules() {
        assert!(AllowanceTerms::builder("btc").build().is_err());

        let period = AllowancePeriod::rolling(1, AllowancePeriodUnit::Day).unwrap();
        let limit = AllowancePeriodLimit::new(Some("1".into()), None, period).unwrap();
        assert!(AllowanceTerms::builder("btc")
            .period_limits(vec![limit.clone(), limit])
            .build()
            .is_err());

        let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt12").unwrap();
        assert!(AllowanceTerms::builder("btc")
            .allowed_payment_endpoint_identifiers(vec![endpoint.clone(), endpoint])
            .build()
            .is_err());
    }

    #[test]
    fn test_response_and_end_correlation_checks_roles_and_ids() {
        let proposal = minimal_proposal();
        let acceptance = AllowanceAcceptance::new(
            EventId::new_v4(),
            proposal.allowance_id.clone(),
            proposal.event_id.clone(),
        )
        .unwrap();
        assert!(acceptance
            .validate_for_proposal(&proposal, AllowanceRole::Allowee)
            .is_ok());
        assert!(acceptance
            .validate_for_proposal(&proposal, AllowanceRole::Allower)
            .is_err());
        assert!(AllowanceAcceptance::new(
            EventId::new_v4(),
            AllowanceId::new_v4(),
            proposal.event_id.clone(),
        )
        .unwrap()
        .validate_for_proposal(&proposal, AllowanceRole::Allowee)
        .is_err());
        assert!(AllowanceAcceptance::new(
            EventId::new_v4(),
            proposal.allowance_id.clone(),
            EventId::new_v4(),
        )
        .unwrap()
        .validate_for_proposal(&proposal, AllowanceRole::Allowee)
        .is_err());

        let end = AllowanceEnd::accepted(
            EventId::new_v4(),
            proposal.allowance_id.clone(),
            proposal.event_id.clone(),
            acceptance.event_id.clone(),
        )
        .unwrap();
        assert!(end
            .validate_for_accepted_allowance(&proposal, &acceptance, AllowanceRole::Allowee)
            .is_ok());
        assert!(end
            .validate_for_accepted_allowance(&proposal, &acceptance, AllowanceRole::Allower)
            .is_err());

        let mismatched_end = AllowanceEnd::accepted(
            EventId::new_v4(),
            proposal.allowance_id.clone(),
            proposal.event_id.clone(),
            EventId::new_v4(),
        )
        .unwrap();
        assert!(mismatched_end
            .validate_for_accepted_allowance(&proposal, &acceptance, AllowanceRole::Allowee)
            .is_err());

        let withdrawal = AllowanceEnd::withdrawal(
            EventId::new_v4(),
            proposal.allowance_id.clone(),
            proposal.event_id.clone(),
        )
        .unwrap();
        assert!(withdrawal
            .validate_withdrawal_for_proposal(&proposal, AllowanceRole::Allower)
            .is_ok());
        assert!(withdrawal
            .validate_withdrawal_for_proposal(&proposal, AllowanceRole::Allowee)
            .is_err());
        assert!(end
            .validate_withdrawal_for_proposal(&proposal, AllowanceRole::Allower)
            .is_err());
    }

    #[test]
    fn test_response_constructors_reject_reused_proposal_event_id() {
        let proposal = minimal_proposal();

        assert!(matches!(
            AllowanceAcceptance::new(
                proposal.event_id.clone(),
                proposal.allowance_id.clone(),
                proposal.event_id.clone(),
            ),
            Err(PaykitError::Validation(_))
        ));
        assert!(matches!(
            AllowanceRejection::new(
                proposal.event_id.clone(),
                proposal.allowance_id.clone(),
                proposal.event_id.clone(),
            ),
            Err(PaykitError::Validation(_))
        ));
    }

    #[test]
    fn test_end_constructors_require_pairwise_distinct_event_ids() {
        let proposal = minimal_proposal();
        let acceptance_event_id = EventId::new_v4();

        assert!(matches!(
            AllowanceEnd::withdrawal(
                proposal.event_id.clone(),
                proposal.allowance_id.clone(),
                proposal.event_id.clone(),
            ),
            Err(PaykitError::Validation(_))
        ));
        for (event_id, proposal_event_id, acceptance_event_id) in [
            (
                proposal.event_id.clone(),
                proposal.event_id.clone(),
                acceptance_event_id.clone(),
            ),
            (
                acceptance_event_id.clone(),
                proposal.event_id.clone(),
                acceptance_event_id.clone(),
            ),
            (
                EventId::new_v4(),
                proposal.event_id.clone(),
                proposal.event_id.clone(),
            ),
        ] {
            assert!(matches!(
                AllowanceEnd::accepted(
                    event_id,
                    proposal.allowance_id.clone(),
                    proposal_event_id,
                    acceptance_event_id,
                ),
                Err(PaykitError::Validation(_))
            ));
        }
    }

    #[test]
    fn test_debug_redacts_terms() {
        let terms = AllowanceTerms::builder("SENTINEL_ASSET")
            .lifetime_amount_limit("123456789")
            .build()
            .unwrap();
        let debug = format!("{:?}", proposal_with_terms(terms));
        assert!(!debug.contains("SENTINEL_ASSET"));
        assert!(!debug.contains("123456789"));
    }
}
