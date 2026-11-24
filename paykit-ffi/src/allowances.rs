//! Allowance proposals, answers, ends and records (Paykit spec 8.3).

use crate::{fixed, Amount, MessageDelivery, NativeWallet, WalletError};
use paykit_lib::{allowances as rules, periods::Unit, Asset, Decimal, PaymentEndpointIdentifier};
use paykit_sdk::{self as sdk, UserConfirmed};
use pubky_switch_core::time::parse_rfc3339;
use serde_json::json;
use std::sync::Arc;

/// This identity's role in the proposal (Paykit wire 4.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum AllowanceRole {
    /// The party whose apps pay (Paykit spec 8.3).
    Allower,
    /// The party paid (Paykit spec 8.3).
    Allowee,
    /// A role unknown to this binding, for display only (Paykit wire 4.15).
    Other,
}

impl From<sdk::AllowanceRole> for AllowanceRole {
    fn from(role: sdk::AllowanceRole) -> Self {
        match role {
            sdk::AllowanceRole::Allower => Self::Allower,
            sdk::AllowanceRole::Allowee => Self::Allowee,
            _ => Self::Other,
        }
    }
}

impl TryFrom<AllowanceRole> for sdk::AllowanceRole {
    type Error = WalletError;

    fn try_from(role: AllowanceRole) -> Result<Self, Self::Error> {
        match role {
            AllowanceRole::Allower => Ok(Self::Allower),
            AllowanceRole::Allowee => Ok(Self::Allowee),
            AllowanceRole::Other => Err(WalletError::invalid("role")),
        }
    }
}

/// The inclusive bounds for one payment (Paykit wire 4.15).
#[derive(Clone, uniffi::Record)]
pub struct PerPaymentRange {
    /// The smallest payment allowed (Paykit wire 4.15).
    pub minimum: Amount,
    /// The largest payment allowed (Paykit wire 4.15).
    pub maximum: Amount,
}

/// The window in which a period limit counts payments (Paykit wire 4.15).
#[derive(Clone, uniffi::Enum, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LimitPeriod {
    /// The calendar or fixed period containing the evaluation time (Paykit wire 4.15).
    Anchored {
        /// The number of units per period, at least one (Paykit wire 4.15).
        every: u64,
        /// Minute, hour, day, week, month or year in wire spelling (Paykit wire 4.4).
        unit: String,
        /// The signed RFC 3339 spelling of the anchor (Paykit wire 4.15).
        anchor: String,
    },
    /// The most recent fixed interval (Paykit wire 4.15).
    Rolling {
        /// The number of units counted, at least one (Paykit wire 4.15).
        every: u64,
        /// Minute, hour, day or week in wire spelling (Paykit wire 4.15).
        unit: String,
    },
}

impl From<rules::LimitPeriod> for LimitPeriod {
    fn from(period: rules::LimitPeriod) -> Self {
        match period {
            rules::LimitPeriod::Anchored(limit) => Self::Anchored {
                every: limit.rule().every(),
                unit: limit.rule().unit().as_str().into(),
                anchor: limit.anchor().as_str().into(),
            },
            rules::LimitPeriod::Rolling { every, unit } => Self::Rolling {
                every,
                unit: unit.as_str().into(),
            },
        }
    }
}

impl LimitPeriod {
    /// Parse the anchor and unit, naming the field that fails. The limit's rules are the
    /// terms reader's (Paykit wire 4.15).
    fn parse(&self) -> Result<(), WalletError> {
        let unit = match self {
            Self::Anchored { unit, anchor, .. } => {
                parse_rfc3339(anchor)
                    .map_err(|_| WalletError::invalid("period_limits.period.anchor"))?;
                unit
            }
            Self::Rolling { unit, .. } => unit,
        };
        Unit::parse(unit).map_err(|_| WalletError::invalid("period_limits.period.unit"))?;
        Ok(())
    }
}

/// The amount limit, payment count limit or both for one period (Paykit wire 4.15).
#[derive(Clone, uniffi::Record)]
pub struct PeriodLimit {
    /// The greatest amount counted, inclusive (Paykit wire 4.15).
    pub amount_limit: Option<Amount>,
    /// The greatest number of payments counted, inclusive (Paykit wire 4.15).
    pub payment_count_limit: Option<u64>,
    /// The period over which to count (Paykit wire 4.15).
    pub period: LimitPeriod,
}

/// The terms signed by the proposer. All amounts use the terms' asset (Paykit wire 4.15).
#[derive(Clone, uniffi::Record)]
pub struct AllowanceTerms {
    /// The asset of eligible requests (Paykit wire 4.15).
    pub asset: String,
    /// The inclusive range allowed per payment (Paykit wire 4.15).
    pub per_payment_amount: Option<PerPaymentRange>,
    /// The unique period limits in their signed order (Paykit wire 4.15).
    pub period_limits: Vec<PeriodLimit>,
    /// The inclusive lifetime spending limit (Paykit wire 4.15).
    pub lifetime_amount_limit: Option<Amount>,
    /// The first eligible instant, in its signed RFC 3339 spelling (Paykit wire 4.15).
    pub active_from: Option<String>,
    /// The first ineligible instant, in its signed RFC 3339 spelling (Paykit wire 4.15).
    pub expires_at: Option<String>,
    /// The eligible Payment Endpoint Identifiers, when restricted (Paykit wire 4.15).
    pub allowed_payment_endpoint_identifiers: Option<Vec<String>>,
}

impl From<rules::AllowanceTerms> for AllowanceTerms {
    fn from(terms: rules::AllowanceTerms) -> Self {
        let amount = |value: Decimal| Amount {
            asset: terms.asset.as_str().into(),
            value: value.as_str().into(),
        };
        Self {
            asset: terms.asset.as_str().into(),
            per_payment_amount: terms.per_payment_amount.map(|range| PerPaymentRange {
                minimum: amount(range.minimum),
                maximum: amount(range.maximum),
            }),
            period_limits: terms
                .period_limits
                .into_iter()
                .map(|limit| PeriodLimit {
                    amount_limit: limit.amount_limit.map(amount),
                    payment_count_limit: limit.payment_count_limit,
                    period: limit.period.into(),
                })
                .collect(),
            lifetime_amount_limit: terms.lifetime_amount_limit.map(amount),
            active_from: terms.active_from.map(|time| time.as_str().into()),
            expires_at: terms.expires_at.map(|time| time.as_str().into()),
            allowed_payment_endpoint_identifiers: terms
                .allowed_payment_endpoint_identifiers
                .map(|ids| ids.into_iter().map(|id| id.as_str().into()).collect()),
        }
    }
}

impl TryFrom<AllowanceTerms> for rules::AllowanceTerms {
    type Error = WalletError;

    /// Parse each member, naming the field that fails, then read the terms as the wire does,
    /// which applies every rule of the terms and keeps each amount and time in the spelling
    /// the host gave it. Terms that parse but break a rule are `InvalidInput` for `terms`
    /// (Paykit wire 4.15).
    fn try_from(terms: AllowanceTerms) -> Result<Self, Self::Error> {
        let asset = Asset::new(&terms.asset).map_err(|_| WalletError::invalid("asset"))?;
        let decimal = |amount: Amount, field| {
            if amount.asset != asset.as_str() {
                return Err(WalletError::invalid(field));
            }
            Decimal::new(&amount.value).map_err(|_| WalletError::invalid(field))
        };
        let range = terms
            .per_payment_amount
            .map(|range| {
                let minimum = decimal(range.minimum, "per_payment_amount.minimum")?;
                let maximum = decimal(range.maximum, "per_payment_amount.maximum")?;
                Ok(json!({ "minimum": minimum.as_str(), "maximum": maximum.as_str() }))
            })
            .transpose()?;
        let lifetime = terms
            .lifetime_amount_limit
            .map(|limit| decimal(limit, "lifetime_amount_limit"))
            .transpose()?;
        let mut periods = Vec::with_capacity(terms.period_limits.len());
        for limit in terms.period_limits {
            limit.period.parse()?;
            let amount = limit
                .amount_limit
                .map(|limit| decimal(limit, "period_limits.amount_limit"))
                .transpose()?;
            periods.push(json!({
                "amount_limit": amount.as_ref().map(Decimal::as_str),
                "payment_count_limit": limit.payment_count_limit,
                "period": limit.period,
            }));
        }
        for (time, field) in [
            (&terms.active_from, "active_from"),
            (&terms.expires_at, "expires_at"),
        ] {
            if let Some(time) = time {
                parse_rfc3339(time).map_err(|_| WalletError::invalid(field))?;
            }
        }
        if let Some(ids) = &terms.allowed_payment_endpoint_identifiers {
            if ids.iter().any(|id| PaymentEndpointIdentifier::new(id).is_err()) {
                return Err(WalletError::invalid("allowed_payment_endpoint_identifiers"));
            }
        }
        Self::from_json(&json!({
            "asset": terms.asset,
            "per_payment_amount": range,
            "period_limits": periods,
            "lifetime_amount_limit": lifetime.as_ref().map(Decimal::as_str),
            "active_from": terms.active_from,
            "expires_at": terms.expires_at,
            "allowed_payment_endpoint_identifiers": terms.allowed_payment_endpoint_identifiers,
        }))
        .map_err(|_| WalletError::invalid("terms"))
    }
}

/// A signed proposal and its delivery state (Paykit spec 8.3.1).
#[derive(uniffi::Record)]
pub struct SentAllowance {
    /// The stable key for reading or acting on the proposal (Paykit spec 8.3.1).
    pub record: Vec<u8>,
    /// The fresh Allowance ID (Paykit wire 4.15).
    pub allowance_id: String,
    /// The signed proposal's message ID (Switch wire 5.1).
    pub msg_id: String,
    /// Where the proposal is in delivery (Switch spec 11).
    pub delivery: MessageDelivery,
}

impl From<sdk::SentAllowance> for SentAllowance {
    fn from(sent: sdk::SentAllowance) -> Self {
        Self {
            record: sent.record.into(),
            allowance_id: sent.allowance_id.as_str().into(),
            msg_id: sent.msg_id,
            delivery: sent.delivery.into(),
        }
    }
}

/// Where the Allowance stands according to its signed messages and local stops (Paykit spec 8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum AllowanceStatus {
    /// The proposal awaits an answer (Paykit spec 8.3.1).
    Proposed,
    /// The recipient accepted the terms (Paykit spec 8.3.1).
    Accepted,
    /// The recipient rejected the proposal (Paykit spec 8.3.1).
    Rejected,
    /// Either party ended it or the proposer withdrew it (Paykit spec 8.3).
    Ended,
    /// Competing proposals or replies permanently prevent payment (Paykit spec 8.3).
    Conflicted,
    /// Ordinary placement expired and a conflict check is outstanding (Paykit spec 8.3).
    AwaitingConflictCheck,
    /// A status unknown to this binding (Paykit spec 8.3).
    Other,
}

impl From<sdk::AllowanceStatus> for AllowanceStatus {
    fn from(status: sdk::AllowanceStatus) -> Self {
        use sdk::AllowanceStatus as S;
        match status {
            S::Proposed => Self::Proposed,
            S::Accepted => Self::Accepted,
            S::Rejected => Self::Rejected,
            S::Ended => Self::Ended,
            S::Conflicted => Self::Conflicted,
            S::AwaitingConflictCheck => Self::AwaitingConflictCheck,
            _ => Self::Other,
        }
    }
}

/// The original hold under which the user saw this Allowance (Paykit spec 8.3).
#[derive(uniffi::Object)]
pub struct AllowanceHold(pub(crate) sdk::AllowanceHold);

#[uniffi::export]
impl AllowanceHold {
    /// The record key of the Allowance shown under this hold (Paykit spec 8.3).
    pub fn record(&self) -> Vec<u8> {
        self.0.record().into()
    }
}

/// The Allowance as this wallet's records show it (Paykit spec 6, 8.3).
#[derive(uniffi::Record)]
pub struct AllowanceView {
    /// The stable Allowance record key (Paykit spec 8.3.1).
    pub record: Vec<u8>,
    /// The other identity (Paykit wire 5.4).
    pub counterparty: Vec<u8>,
    /// The ID named by the signed messages (Paykit wire 4.15).
    pub allowance_id: String,
    /// This identity's role, when the proposal is available (Paykit wire 4.15).
    pub role: Option<AllowanceRole>,
    /// Whether this identity proposed the Allowance (Paykit spec 8.3.1).
    pub proposed_here: bool,
    /// The proposed terms, when the wallet holds the proposal (Paykit wire 4.15).
    pub terms: Option<AllowanceTerms>,
    /// The state derived from the messages and local stops (Paykit spec 8.3).
    pub status: AllowanceStatus,
    /// The executor App ID named by the verified current grant, if any (Paykit spec 8.3).
    pub executor_app_id: Option<Vec<u8>>,
    /// Whether this app holds current consent (Paykit spec 8.3).
    pub consented: bool,
    /// Whether the user must confirm this Allowance again (Paykit spec 8.3).
    pub confirmation_needed: bool,
    /// The original hold for this view and its confirmation (Paykit spec 8.3).
    pub hold: Arc<AllowanceHold>,
    /// Whether the counterparty is blocked, when the deny list is known (Paykit spec 8.1).
    pub blocked: Option<bool>,
}

impl From<sdk::AllowanceView> for AllowanceView {
    fn from(view: sdk::AllowanceView) -> Self {
        Self {
            record: view.record.into(),
            counterparty: view.counterparty.into(),
            allowance_id: view.allowance_id.as_str().into(),
            role: view.role.map(Into::into),
            proposed_here: view.proposed_here,
            terms: view.terms.map(Into::into),
            status: view.status.into(),
            executor_app_id: view.executor_app_id.map(Into::into),
            consented: view.consented,
            confirmation_needed: view.confirmation_needed,
            hold: Arc::new(AllowanceHold(view.hold)),
            blocked: view.blocked,
        }
    }
}

fn confirmed(user_confirmed: bool) -> Result<UserConfirmed, WalletError> {
    user_confirmed
        .then(UserConfirmed::new)
        .ok_or_else(|| WalletError::invalid("user_confirmed"))
}

#[uniffi::export]
impl NativeWallet {
    /// Propose as seller to an identity that admits this wallet. With Allowance and contacts
    /// scopes, the wallet signs a fresh proposal and writes its pending record in the batch
    /// that queues it. A blocked peer or unknown deny list refuses the send before signing.
    /// A kept batch retries the same proposal, which grants no spending authority
    /// (Paykit spec 8.3.1, Paykit wire 4.15).
    pub async fn propose_allowance(
        &self,
        to: Vec<u8>,
        role: AllowanceRole,
        terms: AllowanceTerms,
    ) -> Result<SentAllowance, WalletError> {
        let to = fixed(to, "to")?;
        let role = role.try_into()?;
        let terms = terms.try_into()?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.propose_allowance(to, role, terms).await?.into()) })
        })
        .await
    }

    /// Propose as payer with execution authority. Authorize this wallet as first executor only
    /// after the user confirms the counterparty, terms and automatic payments by this app.
    /// A false confirmation returns `InvalidInput` for `user_confirmed` (Paykit spec 8.3.1).
    pub async fn propose_executable_allowance(
        &self,
        to: Vec<u8>,
        terms: AllowanceTerms,
        user_confirmed: bool,
    ) -> Result<SentAllowance, WalletError> {
        let consent = confirmed(user_confirmed)?;
        let to = fixed(to, "to")?;
        let terms = terms.try_into()?;
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .propose_executable_allowance(to, terms, consent)
                    .await?
                    .into())
            })
        })
        .await
    }

    /// As seller, accept the current incoming proposal with Allowance and contacts scopes.
    /// Store the signed pair and remove the pending proposal atomically. A stale, stopped,
    /// conflicted or uncertain proposal is refused. Return the signed reply ID (Paykit spec 8.3.1).
    pub async fn accept_allowance(&self, record: Vec<u8>) -> Result<String, WalletError> {
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.accept_allowance(&record).await?) })
        })
        .await
    }

    /// As payer, accept the displayed proposal and authorize automatic payments by this
    /// wallet after user confirmation. A false confirmation returns `InvalidInput` for
    /// `user_confirmed`. Create the ledger at zero in the acceptance batch and return
    /// the original reply ID (Paykit spec 8.3.1).
    pub async fn accept_executable_allowance(
        &self,
        record: Vec<u8>,
        user_confirmed: bool,
    ) -> Result<String, WalletError> {
        let consent = confirmed(user_confirmed)?;
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.accept_executable_allowance(&record, consent).await?) })
        })
        .await
    }

    /// With Allowance and contacts scopes, reject a pending incoming proposal once. Record the
    /// stop even over divergence, then send it and return its message ID (Paykit spec 8.3.1,
    /// Paykit wire 4.16).
    pub async fn reject_allowance(&self, record: Vec<u8>) -> Result<String, WalletError> {
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.reject_allowance(&record).await?) })
        })
        .await
    }

    /// End an accepted Allowance or withdraw this identity's unanswered proposal, even
    /// for a blocked peer or after contacts permission is removed. Record and send the end,
    /// notify any other own executor and return its message ID (Paykit spec 8.3, Paykit wire 4.16).
    pub async fn end_allowance(&self, record: Vec<u8>) -> Result<String, WalletError> {
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| Box::pin(async move { Ok(app.end_allowance(&record).await?) }))
            .await
    }

    /// List pending and permanent Allowances that bind, including kept conflicts over
    /// deleted records. An incomplete listing returns an error (Paykit spec 6, 8.3).
    pub async fn allowances(&self) -> Result<Vec<AllowanceView>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .allowances()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            })
        })
        .await
    }

    /// Read one Allowance as the list shows it. An absent record is refused (Paykit spec 6, 8.3).
    pub async fn allowance(&self, record: Vec<u8>) -> Result<AllowanceView, WalletError> {
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.allowance(&record).await?.into()) })
        })
        .await
    }

    /// Discard exactly the selected conflicted pending proposal or one received from a
    /// blocked peer, without sending a message. A changed or uncertain record stays in
    /// place (Paykit spec 8.3.1).
    pub async fn discard_pending_allowance(&self, record: Vec<u8>) -> Result<(), WalletError> {
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.discard_pending_allowance(&record).await?) })
        })
        .await
    }
}
