use serde::{Deserialize, Serialize};

use crate::{BillingPeriod, PaymentAmount};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PaymentAmountWire {
    pub(crate) value: String,
    pub(crate) asset: String,
}

impl From<PaymentAmountWire> for PaymentAmount {
    fn from(wire: PaymentAmountWire) -> Self {
        Self {
            value: wire.value,
            asset: wire.asset,
        }
    }
}

impl From<&PaymentAmount> for PaymentAmountWire {
    fn from(amount: &PaymentAmount) -> Self {
        Self {
            value: amount.value.clone(),
            asset: amount.asset.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BillingPeriodWire {
    pub(crate) starts_at: String,
    pub(crate) ends_at: String,
}

impl From<BillingPeriodWire> for BillingPeriod {
    fn from(wire: BillingPeriodWire) -> Self {
        Self {
            starts_at: wire.starts_at,
            ends_at: wire.ends_at,
        }
    }
}

impl From<&BillingPeriod> for BillingPeriodWire {
    fn from(period: &BillingPeriod) -> Self {
        Self {
            starts_at: period.starts_at.clone(),
            ends_at: period.ends_at.clone(),
        }
    }
}
