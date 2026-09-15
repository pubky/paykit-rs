use serde::{Deserialize, Serialize};

use crate::{BillingPeriod, PaymentAmount};

#[derive(Serialize, Deserialize)]
pub(crate) struct RequiredNullable<T>(Option<T>);

impl<T> RequiredNullable<T> {
    pub(crate) fn into_inner(self) -> Option<T> {
        self.0
    }
}

impl<T> From<Option<T>> for RequiredNullable<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

pub(crate) fn deserialize_optional_no_null<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PaymentAmountWire {
    pub(crate) value: String,
    pub(crate) asset: String,
}

impl TryFrom<PaymentAmountWire> for PaymentAmount {
    type Error = crate::PaykitError;
    fn try_from(wire: PaymentAmountWire) -> crate::Result<Self> {
        Self::new(wire.value, wire.asset)
    }
}

impl From<&PaymentAmount> for PaymentAmountWire {
    fn from(amount: &PaymentAmount) -> Self {
        Self {
            value: amount.value().to_owned(),
            asset: amount.asset().to_owned(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BillingPeriodWire {
    pub(crate) starts_at: String,
    pub(crate) ends_at: String,
}

impl TryFrom<BillingPeriodWire> for BillingPeriod {
    type Error = crate::PaykitError;
    fn try_from(wire: BillingPeriodWire) -> crate::Result<Self> {
        Self::new(wire.starts_at, wire.ends_at)
    }
}

impl From<&BillingPeriod> for BillingPeriodWire {
    fn from(period: &BillingPeriod) -> Self {
        Self {
            starts_at: period.starts_at().to_owned(),
            ends_at: period.ends_at().to_owned(),
        }
    }
}

impl PaymentAmountWire {
    pub(crate) fn try_into_with_label(self, label: &'static str) -> crate::Result<PaymentAmount> {
        PaymentAmount::new_with_label(self.value, self.asset, label)
    }
}

impl BillingPeriodWire {
    pub(crate) fn try_into_with_label(self, label: &str) -> crate::Result<BillingPeriod> {
        BillingPeriod::new_with_label(self.starts_at, self.ends_at, label)
    }
}
