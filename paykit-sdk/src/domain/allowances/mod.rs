//! Allowance lifecycle derivation and atomic command use cases.
//!
//! Derived records contain private Allowance Terms. Applications must treat
//! them as sensitive local SDK state.

use std::fmt;

use chrono::{DateTime, Utc};
use paykit_lib::{AllowancePeriodKind, AllowanceRole, AllowanceTerms};
use serde::{Deserialize, Serialize};

use crate::{
    domain::outbound_private::OutboundPrivateMessageStatus, PaykitReceiverPath, PubkyPublicKey,
};

mod commands;
mod derivation;

pub(crate) use commands::{
    enqueue_allowance_acceptance, enqueue_allowance_end, enqueue_allowance_proposal,
    enqueue_allowance_rejection,
};
pub(crate) use derivation::{
    allowance_record_from_state, allowance_records_from_state, allowance_scopes,
};

/// Local party role for one Allowance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AllowanceLocalRole {
    /// Local identity grants authority and remains the Payer.
    Allower,
    /// Local identity may send qualifying Payment Requests.
    Allowee,
}

impl From<AllowanceLocalRole> for AllowanceRole {
    fn from(role: AllowanceLocalRole) -> Self {
        match role {
            AllowanceLocalRole::Allower => Self::Allower,
            AllowanceLocalRole::Allowee => Self::Allowee,
        }
    }
}

impl From<AllowanceRole> for AllowanceLocalRole {
    fn from(role: AllowanceRole) -> Self {
        match role {
            AllowanceRole::Allower => Self::Allower,
            AllowanceRole::Allowee => Self::Allowee,
        }
    }
}

/// SDK-derived Allowance lifecycle state.
///
/// This state describes consent messages only. It does not imply that the
/// Allowance is currently eligible, locally enabled, or safe to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AllowanceLifecycleState {
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
}

/// Health of the durable history used to derive one Allowance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AllowanceHistoryStatus {
    /// All retained evidence is valid and causally resolved.
    Consistent,
    /// A valid event references evidence that has not been loaded yet.
    UnresolvedReferences,
    /// Malformed, conflicting, or protocol-invalid evidence is present.
    Invalid,
    /// The exact Encrypted Link needs recovery before safe use.
    RecoveryRequired,
}

/// Inclusive per-payment amount range copied from Allowance Terms.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceAmountRangeRecord {
    /// Minimum decimal wire spelling.
    pub minimum: String,
    /// Maximum decimal wire spelling.
    pub maximum: String,
}

impl fmt::Debug for AllowanceAmountRangeRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AllowanceAmountRangeRecord(<redacted>)")
    }
}

/// Anchored or rolling period copied from Allowance Terms.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowancePeriodRecord {
    /// Canonical period kind (`anchored` or `rolling`).
    pub kind: String,
    /// Positive interval multiplier.
    pub every: u64,
    /// Canonical singular interval unit.
    pub unit: String,
    /// UTC anchor for an anchored period.
    pub anchor: Option<String>,
}

impl fmt::Debug for AllowancePeriodRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AllowancePeriodRecord(<redacted>)")
    }
}

/// Amount and/or count ceiling copied from Allowance Terms.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowancePeriodLimitRecord {
    /// Optional amount ceiling decimal spelling.
    pub amount_limit: Option<String>,
    /// Optional payment-count ceiling.
    pub payment_count_limit: Option<u64>,
    /// Period over which the ceilings apply.
    pub period: AllowancePeriodRecord,
}

impl fmt::Debug for AllowancePeriodLimitRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AllowancePeriodLimitRecord(<redacted>)")
    }
}

/// Immutable Allowance Terms copied into an SDK record.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceTermsRecord {
    /// Exact, case-sensitive asset.
    pub asset: String,
    /// Optional inclusive per-payment amount range.
    pub per_payment_amount: Option<AllowanceAmountRangeRecord>,
    /// Independently applicable period limits.
    pub period_limits: Vec<AllowancePeriodLimitRecord>,
    /// Optional lifetime amount ceiling decimal spelling.
    pub lifetime_amount_limit: Option<String>,
    /// Optional inclusive first eligible instant.
    pub active_from: Option<String>,
    /// Optional exclusive first ineligible instant.
    pub expires_at: Option<String>,
    /// Optional exact Payment Endpoint Identifier allowlist.
    pub allowed_payment_endpoint_identifiers: Option<Vec<String>>,
}

impl fmt::Debug for AllowanceTermsRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AllowanceTermsRecord(<redacted>)")
    }
}

impl From<&AllowanceTerms> for AllowanceTermsRecord {
    fn from(terms: &AllowanceTerms) -> Self {
        Self {
            asset: terms.asset().to_owned(),
            per_payment_amount: terms.per_payment_amount().map(|range| {
                AllowanceAmountRangeRecord {
                    minimum: range.minimum().to_owned(),
                    maximum: range.maximum().to_owned(),
                }
            }),
            period_limits: terms
                .period_limits()
                .iter()
                .map(|limit| AllowancePeriodLimitRecord {
                    amount_limit: limit.amount_limit().map(str::to_owned),
                    payment_count_limit: limit.payment_count_limit(),
                    period: AllowancePeriodRecord {
                        kind: match limit.period().kind() {
                            AllowancePeriodKind::Anchored => "anchored",
                            AllowancePeriodKind::Rolling => "rolling",
                        }
                        .to_owned(),
                        every: limit.period().every(),
                        unit: limit.period().unit().as_str().to_owned(),
                        anchor: limit.period().anchor().map(str::to_owned),
                    },
                })
                .collect(),
            lifetime_amount_limit: terms.lifetime_amount_limit().map(str::to_owned),
            active_from: terms.active_from().map(str::to_owned),
            expires_at: terms.expires_at().map(str::to_owned),
            allowed_payment_endpoint_identifiers: terms.allowed_payment_endpoint_identifiers().map(
                |identifiers| {
                    identifiers
                        .iter()
                        .map(|identifier| identifier.as_str().to_owned())
                        .collect()
                },
            ),
        }
    }
}

/// Filter for listing SDK-derived Allowances.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceFilter {
    /// Restrict results to one counterparty. Without a receiver path, every
    /// exact link for that counterparty is included.
    pub counterparty: Option<PubkyPublicKey>,
    /// Restrict results to one counterparty receiver/runtime folder.
    pub counterparty_receiver_path: Option<PaykitReceiverPath>,
    /// Restrict results to one local Allowance role.
    pub local_role: Option<AllowanceLocalRole>,
    /// Restrict results to lifecycle states. An empty list means all states.
    pub states: Vec<AllowanceLifecycleState>,
    /// Restrict results to history-health states. An empty list means all.
    pub history_statuses: Vec<AllowanceHistoryStatus>,
}

impl AllowanceFilter {
    pub(crate) fn matches(&self, record: &AllowanceRecord) -> bool {
        self.counterparty
            .as_ref()
            .is_none_or(|counterparty| &record.counterparty == counterparty)
            && self
                .counterparty_receiver_path
                .as_ref()
                .is_none_or(|path| &record.counterparty_receiver_path == path)
            && self
                .local_role
                .is_none_or(|role| record.local_role == Some(role))
            && (self.states.is_empty() || self.states.contains(&record.state))
            && (self.history_statuses.is_empty()
                || self.history_statuses.contains(&record.history_status))
    }
}

/// SDK-derived record for one Allowance on one exact Encrypted Link.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowanceRecord {
    /// Counterparty associated with the authenticated private history.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder associated with the history.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Stable Allowance ID.
    pub allowance_id: String,
    /// Local role derived from the authenticated proposal source.
    pub local_role: Option<AllowanceLocalRole>,
    /// Derived consent lifecycle state.
    pub state: AllowanceLifecycleState,
    /// Health of the evidence used for derivation.
    pub history_status: AllowanceHistoryStatus,
    /// Proposal Event ID.
    pub proposal_event_id: Option<String>,
    /// Immutable proposed terms.
    pub terms: Option<AllowanceTermsRecord>,
    /// Inbound stream item carrying the proposal, when received.
    pub proposal_stream_item_id: Option<u64>,
    /// Outbound message carrying the proposal, when locally queued.
    pub proposal_outbound_message_id: Option<u64>,
    /// Local delivery status of an outbound proposal.
    pub proposal_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Controlling Acceptance Event ID.
    pub acceptance_event_id: Option<String>,
    /// Inbound stream item carrying the controlling acceptance.
    pub acceptance_stream_item_id: Option<u64>,
    /// Outbound message carrying the controlling acceptance.
    pub acceptance_outbound_message_id: Option<u64>,
    /// Local delivery status of an outbound acceptance.
    pub acceptance_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Controlling Rejection Event ID.
    pub rejection_event_id: Option<String>,
    /// Inbound stream item carrying the controlling rejection.
    pub rejection_stream_item_id: Option<u64>,
    /// Outbound message carrying the controlling rejection.
    pub rejection_outbound_message_id: Option<u64>,
    /// Local delivery status of an outbound rejection.
    pub rejection_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Valid End Event ID retained deterministically without implying a total
    /// order across the two sending directions.
    pub end_event_id: Option<String>,
    /// Inbound stream item carrying the retained End.
    pub end_stream_item_id: Option<u64>,
    /// Outbound message carrying the retained End.
    pub end_outbound_message_id: Option<u64>,
    /// Local delivery status of an outbound End.
    pub end_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Causal Event IDs not yet present in durable history.
    pub pending_causal_event_ids: Vec<String>,
    /// Event IDs whose reuse or proposal collision taints this Allowance.
    pub conflict_event_ids: Vec<String>,
    /// Last inbound stream item associated with this Allowance.
    pub last_stream_item_id: Option<u64>,
    /// Last outbound message associated with this Allowance.
    pub last_outbound_message_id: Option<u64>,
    /// Delivery status of the last associated outbound message.
    pub last_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Latest local record time, used only for presentation ordering.
    pub last_event_at: Option<DateTime<Utc>>,
    /// Redaction-safe reason for invalid history, when available.
    pub invalid_reason: Option<String>,
}

impl AllowanceRecord {
    pub(crate) fn new(
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        allowance_id: String,
    ) -> Self {
        Self {
            counterparty,
            counterparty_receiver_path,
            allowance_id,
            local_role: None,
            state: AllowanceLifecycleState::Proposed,
            history_status: AllowanceHistoryStatus::Consistent,
            proposal_event_id: None,
            terms: None,
            proposal_stream_item_id: None,
            proposal_outbound_message_id: None,
            proposal_outbound_status: None,
            acceptance_event_id: None,
            acceptance_stream_item_id: None,
            acceptance_outbound_message_id: None,
            acceptance_outbound_status: None,
            rejection_event_id: None,
            rejection_stream_item_id: None,
            rejection_outbound_message_id: None,
            rejection_outbound_status: None,
            end_event_id: None,
            end_stream_item_id: None,
            end_outbound_message_id: None,
            end_outbound_status: None,
            pending_causal_event_ids: Vec::new(),
            conflict_event_ids: Vec::new(),
            last_stream_item_id: None,
            last_outbound_message_id: None,
            last_outbound_status: None,
            last_event_at: None,
            invalid_reason: None,
        }
    }
}

impl fmt::Debug for AllowanceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AllowanceRecord")
            .field("counterparty", &self.counterparty)
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
            .field("allowance_id", &self.allowance_id)
            .field("local_role", &self.local_role)
            .field("state", &self.state)
            .field("history_status", &self.history_status)
            .field("proposal_event_id", &self.proposal_event_id)
            .field("terms", &self.terms.as_ref().map(|_| "<redacted>"))
            .field("acceptance_event_id", &self.acceptance_event_id)
            .field("rejection_event_id", &self.rejection_event_id)
            .field("end_event_id", &self.end_event_id)
            .field("pending_causal_event_ids", &self.pending_causal_event_ids)
            .field("conflict_event_ids", &self.conflict_event_ids)
            .field("last_stream_item_id", &self.last_stream_item_id)
            .field("last_outbound_message_id", &self.last_outbound_message_id)
            .field("last_outbound_status", &self.last_outbound_status)
            .field("last_event_at", &self.last_event_at)
            .field(
                "invalid_reason",
                &self
                    .invalid_reason
                    .as_ref()
                    .map(|reason| format!("<redacted:{} bytes>", reason.len())),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests;
