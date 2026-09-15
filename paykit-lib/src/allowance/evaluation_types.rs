//! Inputs and outcomes for stateless Allowance evaluation.

use super::AllowanceTerms;
use crate::{PaymentAmount, PaymentEndpointIdentifier, PaymentRequestTerms, Result};
use chrono::{DateTime, Utc};

/// Why the common Allowance rules did not admit a candidate payment.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AllowanceEvaluationBlock {
    /// The caller supplied structurally invalid Payment Request terms.
    #[error("Payment Request terms are invalid")]
    InvalidRequest,
    /// Request and Allowance assets differ, including case.
    #[error("Payment Request asset does not match the Allowance")]
    AssetMismatch,
    /// Request amount is outside the inclusive per-payment range.
    #[error("Payment Request amount is outside the Allowance range")]
    AmountOutsideRange,
    /// The request and Allowance have no common endpoint identifier.
    #[error("No Payment Endpoint Identifier is allowed by both request and Allowance")]
    NoEligibleEndpoint,
    /// The inclusive active time has not been reached.
    #[error("Allowance is not active yet")]
    NotActive,
    /// The exclusive expiry has been reached.
    #[error("Allowance has expired")]
    Expired,
    /// Evaluation would move trusted time behind its durable watermark.
    #[error("Allowance evaluation time is earlier than its watermark")]
    ClockRollback,
    /// Retained usage was admitted later than this evaluation time.
    #[error("Allowance usage contains a future admission time")]
    FutureUsage,
    /// Accounting evidence uses a different asset from the Allowance.
    #[error("Allowance usage asset does not match its terms")]
    UsageAssetMismatch,
    /// One configured period amount ceiling would be exceeded.
    #[error("Allowance period {index} amount ceiling would be exceeded")]
    PeriodAmountLimit {
        /// Index into the original `period_limits` array.
        index: usize,
    },
    /// One configured period count ceiling would be exceeded.
    #[error("Allowance period {index} payment count would be exceeded")]
    PeriodCountLimit {
        /// Index into the original `period_limits` array.
        index: usize,
    },
    /// The lifetime amount ceiling would be exceeded.
    #[error("Allowance lifetime amount ceiling would be exceeded")]
    LifetimeAmountLimit,
    /// A time boundary, duration, or count cannot be represented safely.
    #[error("Allowance arithmetic or UTC boundary cannot be represented")]
    ArithmeticOverflow,
}

impl AllowanceEvaluationBlock {
    /// Stable machine-readable code without private values.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::AssetMismatch => "asset_mismatch",
            Self::AmountOutsideRange => "amount_outside_range",
            Self::NoEligibleEndpoint => "no_eligible_endpoint",
            Self::NotActive => "not_active",
            Self::Expired => "expired",
            Self::ClockRollback => "clock_rollback",
            Self::FutureUsage => "future_usage",
            Self::UsageAssetMismatch => "usage_asset_mismatch",
            Self::PeriodAmountLimit { .. } => "period_amount_limit",
            Self::PeriodCountLimit { .. } => "period_count_limit",
            Self::LifetimeAmountLimit => "lifetime_amount_limit",
            Self::ArithmeticOverflow => "arithmetic_overflow",
        }
    }
}

/// One committed automatic payment or unresolved automatic reservation.
///
/// Both consume one count and the exact original amount. Exclude released
/// reservations and manual payments; the SDK must deduplicate payment keys.
#[derive(Clone, PartialEq, Eq)]
pub struct AllowanceUsageEntry {
    amount: PaymentAmount,
    admitted_at: DateTime<Utc>,
}

impl std::fmt::Debug for AllowanceUsageEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AllowanceUsageEntry(<redacted>)")
    }
}

impl AllowanceUsageEntry {
    /// Validate an amount and retain its original admission time and decimal text.
    pub fn new(amount: PaymentAmount, admitted_at: DateTime<Utc>) -> Result<Self> {
        amount.validate_with_label("Allowance usage amount")?;
        if admitted_at.timestamp_subsec_nanos() >= 1_000_000_000 {
            return Err(crate::PaykitError::Validation(
                "Allowance usage time cannot represent a leap second".into(),
            ));
        }
        Ok(Self {
            amount,
            admitted_at,
        })
    }

    /// Exact original Payment Amount; no fee or refund adjustment is implied.
    pub fn amount(&self) -> &PaymentAmount {
        &self.amount
    }

    /// Original wallet admission time; settlement and retries do not change it.
    pub fn admitted_at(&self) -> DateTime<Utc> {
        self.admitted_at
    }
}

/// Explicit inputs to common eligibility math, without lifecycle or payment authority.
///
/// `usage` must contain all committed and unresolved automatic usage for these
/// exact Allowance Terms, once per semantic payment key. The SDK must establish
/// complete, current accounting and watermark evidence before calling this API;
/// an empty slice cannot prove that an Allowance has never been used.
pub struct AllowanceEvaluationInput<'a> {
    /// Immutable shared Allowance Terms.
    pub terms: &'a AllowanceTerms,
    /// Immutable Payment Request terms.
    pub request: &'a PaymentRequestTerms,
    /// Wallet-supplied trusted UTC time.
    pub trusted_time: DateTime<Utc>,
    /// Persisted nondecreasing evaluation-time watermark, never a guessed default.
    pub watermark: DateTime<Utc>,
    /// Complete committed usage and unresolved reservations, excluding the candidate.
    pub usage: &'a [AllowanceUsageEntry],
}

/// Successful common-rule evaluation; this does not authorize or reserve payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowanceEvaluation {
    /// Identifiers permitted by both request and Allowance, in request order.
    pub eligible_payment_endpoint_identifiers: Vec<PaymentEndpointIdentifier>,
}
