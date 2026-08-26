//! Stateless Allowance lifecycle wire protocol.
//!
//! This module validates, parses, serializes, and sends Allowance Event
//! Messages. It deliberately does not derive lifecycle state, match Payment
//! Requests, read a clock, track usage, reserve capacity, or authorize payment.

mod api;
mod types;
mod wire;

pub use api::{
    parse_allowance_event_message, send_allowance_acceptance, send_allowance_end,
    send_allowance_proposal, send_allowance_rejection, serialize_allowance_event,
};
pub use types::{
    AllowanceAcceptance, AllowanceAmountRange, AllowanceEnd, AllowanceEvent, AllowanceEventMessage,
    AllowanceId, AllowancePeriod, AllowancePeriodKind, AllowancePeriodLimit, AllowancePeriodUnit,
    AllowanceProposal, AllowanceRejection, AllowanceRole, AllowanceTerms, AllowanceTermsBuilder,
};
