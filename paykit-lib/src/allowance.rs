//! Stateless Allowance lifecycle wire protocol and common eligibility math.
//!
//! This module validates, parses, serializes, and sends Allowance Event
//! Messages and evaluates common rules against explicit caller-provided inputs.
//! It does not derive lifecycle state, read a clock, persist usage, reserve
//! capacity, select an Allowance, or authorize payment.

mod amounts;
mod api;
mod evaluation;
mod evaluation_types;
mod periods;
mod types;
mod wire;

pub use amounts::{add_decimal_amounts, compare_decimal_amounts};
pub use evaluation::{check_allowance_time, evaluate_allowance, match_allowance_request};
pub use evaluation_types::{
    AllowanceEvaluation, AllowanceEvaluationBlock, AllowanceEvaluationInput, AllowanceUsageEntry,
};
pub use periods::{allowance_period_window, AllowancePeriodWindow};

pub use api::{
    parse_allowance_event_message, send_allowance_acceptance, send_allowance_end,
    send_allowance_proposal, send_allowance_rejection, serialize_allowance_event,
};
pub use types::{
    AllowanceAcceptance, AllowanceAmountRange, AllowanceEnd, AllowanceEvent, AllowanceEventMessage,
    AllowanceId, AllowancePeriod, AllowancePeriodKind, AllowancePeriodLimit, AllowancePeriodUnit,
    AllowanceProposal, AllowanceRejection, AllowanceRole, AllowanceTerms, AllowanceTermsBuilder,
};

#[cfg(test)]
mod evaluation_tests;

/// Shared fixtures for Allowance tests across this crate.
#[cfg(test)]
pub(crate) mod test_fixtures {
    use super::{AllowanceId, AllowanceProposal, AllowanceRole, AllowanceTerms};
    use crate::EventId;

    pub(crate) const EVENT_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201";
    pub(crate) const ALLOWANCE_ID: &str = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44";

    pub(crate) fn event_id(value: &str) -> EventId {
        EventId::new(value).unwrap()
    }

    pub(crate) fn allowance_id() -> AllowanceId {
        AllowanceId::new(ALLOWANCE_ID).unwrap()
    }

    /// Proposal with the smallest Allowance Terms the validator accepts.
    pub(crate) fn minimal_proposal() -> AllowanceProposal {
        proposal_with_terms(
            AllowanceTerms::builder("btc")
                .lifetime_amount_limit("1")
                .build()
                .unwrap(),
        )
    }

    /// Allower-proposed event with the fixed fixture IDs and `terms`.
    pub(crate) fn proposal_with_terms(terms: AllowanceTerms) -> AllowanceProposal {
        AllowanceProposal::new(
            event_id(EVENT_ID),
            allowance_id(),
            AllowanceRole::Allower,
            terms,
        )
    }
}
