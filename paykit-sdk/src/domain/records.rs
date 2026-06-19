//! Shared SDK record field types.

use serde::{Deserialize, Serialize};

/// Durable Payment Amount fields copied into SDK records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmountRecord {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

impl From<&paykit_lib::PaymentAmount> for AmountRecord {
    fn from(amount: &paykit_lib::PaymentAmount) -> Self {
        Self {
            value: amount.value.clone(),
            asset: amount.asset.clone(),
        }
    }
}

/// Billing Period fields copied into SDK records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingPeriodRecord {
    /// RFC3339 UTC timestamp using `Z`.
    pub starts_at: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub ends_at: String,
}

impl From<&paykit_lib::BillingPeriod> for BillingPeriodRecord {
    fn from(period: &paykit_lib::BillingPeriod) -> Self {
        Self {
            starts_at: period.starts_at.clone(),
            ends_at: period.ends_at.clone(),
        }
    }
}
