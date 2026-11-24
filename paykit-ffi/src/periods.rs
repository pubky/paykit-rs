//! Recurring Payment Requests, subscriptions and Billing Periods (Paykit spec 8.2).

use crate::{
    fixed, lists::list_endpoints, Amount, BillingPeriod, ConversionRate, ListEndpoint,
    MessageDelivery, NativeWallet, PaymentOptions, PaymentRefusal, PaymentResult, PeriodView,
    Receipt, Recurrence, RequestId, WalletError,
};
use paykit_lib::{bodies, periods::PeriodRef, PaymentEndpointIdentifier, Rate};
use paykit_sdk::{PeriodPrice, Refusal, RequestStatus};
use pubky_switch_core::time::MAX_TIME_MS;
use std::sync::Arc;

/// A recurring request as of the payer's last read (Paykit spec 8.2, 8.5).
#[derive(uniffi::Record)]
pub struct RecurringRequest {
    /// The payee and authenticated request hash (Paykit wire 1.5).
    pub id: RequestId,
    /// The payer's request record key (Paykit wire 5.3).
    pub record: Vec<u8>,
    /// The amount requested for each period (Paykit wire 4.5).
    pub amount: Amount,
    /// The payee's Payment Reference (Paykit wire 4.5).
    pub payment_reference: String,
    /// The selling scope's label, when present (Paykit spec 3.4).
    pub label: Option<String>,
    /// The proposal expiry in Unix milliseconds, when present (Paykit wire 4.5).
    pub proposal_expires_at_ms: Option<u64>,
    /// The current state of the request (Paykit spec 8.2, 8.5).
    pub status: RecurringRequestStatus,
    /// The exact hold to show before the user confirms acceptance or payment (Paykit spec 7.2).
    pub held: Option<Arc<PaymentRefusal>>,
    /// Whether the request can only be paid by hand (Paykit spec 8.3).
    pub manual_only: bool,
    /// Whether the request record shows divergence or was paused for it (Paykit spec 6).
    pub divergent: bool,
    /// Any receipt held on the request record (Paykit wire 4.11, 5.3).
    pub receipt: Option<Receipt>,
    /// The recurrence defining this request's periods (Paykit wire 4.4).
    pub recurrence: Recurrence,
    /// The period records the payer holds, in period order. Each period is paid at the
    /// request's amount (Paykit spec 8.2, Paykit wire 5.3).
    pub periods: Vec<PeriodView>,
}

/// What the payer sees for the recurring request as a whole (Paykit spec 8.2, 8.5).
#[derive(uniffi::Enum)]
pub enum RecurringRequestStatus {
    /// The proposal is open (Paykit spec 8.2).
    Open,
    /// This wallet has an open attempt (Paykit spec 8.5).
    BeingPaidHere,
    /// Another own wallet has an open attempt (Paykit spec 8.5).
    BeingHandledByOtherWallet,
    /// The payment history leaves doubt about an attempt (Paykit spec 7.1, 8.5).
    MayBePaid,
    /// The SDK shows payment on this record (Paykit spec 8.5).
    Paid,
    /// The subscription is accepted and running (Paykit spec 8.2).
    Subscribed,
    /// The accepted subscription's last period has ended (Paykit spec 8.2).
    Ended,
    /// The payer rejected the proposal (Paykit spec 8.2).
    Rejected,
    /// Either party cancelled the request (Paykit spec 8.2).
    Cancelled,
    /// The proposal expired before acceptance (Paykit spec 8.2).
    Expired,
    /// A restore or missed item requires confirmation (Paykit spec 7.2).
    Held,
    /// The received request still awaits its record (Paykit spec 8.1).
    Unrecorded,
    /// The payee is on the payer's deny list (Paykit spec 8.1).
    Blocked,
    /// The request record is paused after divergence (Paykit spec 6).
    Divergent,
    /// Another SDK status for this request (Paykit spec 8.5).
    Other,
}

impl From<RequestStatus> for RecurringRequestStatus {
    fn from(status: RequestStatus) -> Self {
        match status {
            RequestStatus::Open => Self::Open,
            RequestStatus::BeingPaidHere => Self::BeingPaidHere,
            RequestStatus::BeingHandledByOtherWallet => Self::BeingHandledByOtherWallet,
            RequestStatus::MayBePaid => Self::MayBePaid,
            RequestStatus::Paid => Self::Paid,
            RequestStatus::Subscribed => Self::Subscribed,
            RequestStatus::Ended => Self::Ended,
            RequestStatus::Rejected => Self::Rejected,
            RequestStatus::Cancelled => Self::Cancelled,
            RequestStatus::Expired => Self::Expired,
            RequestStatus::Held => Self::Held,
            RequestStatus::Unrecorded => Self::Unrecorded,
            RequestStatus::Blocked => Self::Blocked,
            RequestStatus::Divergent => Self::Divergent,
            _ => Self::Other,
        }
    }
}

impl TryFrom<BillingPeriod> for PeriodRef {
    type Error = WalletError;

    fn try_from(period: BillingPeriod) -> Result<Self, Self::Error> {
        if period.ends_at <= period.starts_at || period.ends_at > MAX_TIME_MS {
            return Err(WalletError::invalid("billing period"));
        }
        Ok(Self {
            starts_at: period.starts_at,
            ends_at: period.ends_at,
        })
    }
}

/// The destination, Conversion Rates or both that the payee supplies for a period (Paykit wire 4.9).
#[derive(uniffi::Record)]
pub struct PeriodDetails {
    /// One of the request's Billing Periods (Paykit wire 4.4).
    pub billing_period: BillingPeriod,
    /// The payee's Payment Endpoint identifiers and payloads (Paykit wire 4.9).
    pub payment_endpoints: Option<Vec<ListEndpoint>>,
    /// The period's cross-asset Conversion Rates (Paykit wire 4.9).
    pub rates: Option<Vec<ConversionRate>>,
    /// Inclusive expiry in Unix milliseconds (Paykit wire 4.9).
    pub expires_at_ms: u64,
}

impl TryFrom<PeriodDetails> for bodies::PeriodDetails {
    type Error = WalletError;

    fn try_from(details: PeriodDetails) -> Result<Self, Self::Error> {
        if details.expires_at_ms > MAX_TIME_MS {
            return Err(WalletError::invalid("expires_at_ms"));
        }
        Ok(Self {
            billing_period: details.billing_period.try_into()?,
            payment_endpoints: details.payment_endpoints.map(list_endpoints).transpose()?,
            rates: details
                .rates
                .map(|rates| {
                    rates
                        .into_iter()
                        .map(Rate::try_from)
                        .collect::<Result<_, _>>()
                })
                .transpose()?,
            expires_at: details.expires_at_ms,
        })
    }
}

/// The exact refusals shown to the user before accepting a subscription (Paykit spec 7.2, 8.1).
#[derive(Default, uniffi::Record)]
pub struct AcceptOptions {
    /// The restored or missed-item hold the user confirmed (Paykit spec 7.2).
    pub possibly_cancelled_confirmed: Option<Arc<PaymentRefusal>>,
    /// The blocked payee's subscription the user confirmed (Paykit spec 8.1).
    pub blocked_confirmed: Option<Arc<PaymentRefusal>>,
}

impl TryFrom<AcceptOptions> for paykit_sdk::AcceptOptions {
    type Error = WalletError;

    fn try_from(options: AcceptOptions) -> Result<Self, Self::Error> {
        let mut confirmed = Self::default();
        if let Some(refusal) = options.possibly_cancelled_confirmed {
            let Refusal::HeldAfterRestore { held } = &refusal.0 else {
                return Err(WalletError::invalid("possibly_cancelled_confirmed"));
            };
            confirmed.possibly_cancelled_confirmed = Some((**held).clone());
        }
        if let Some(refusal) = options.blocked_confirmed {
            let Refusal::Blocked { payment } = &refusal.0 else {
                return Err(WalletError::invalid("blocked_confirmed"));
            };
            confirmed.blocked_confirmed = Some((**payment).clone());
        }
        Ok(confirmed)
    }
}

/// The price authorized by the wallet's own recurring-payment policy (Paykit spec 7.2, 8.2).
#[derive(uniffi::Enum)]
pub enum PeriodPriceChoice {
    /// Pay in the requested asset at the requested amount (Paykit spec 8.2).
    RequestedAmount,
    /// Request the signed rate quote for one endpoint without paying (Paykit spec 7.2).
    QuoteFor {
        /// The Payment Endpoint Identifier (Paykit wire 1.4).
        endpoint: String,
    },
    /// Pay only the exact rate quote the user or their bounded policy approved (Paykit spec 7.2).
    AtRate {
        /// The original priced-by-rate refusal (Paykit spec 7.2).
        quote: Arc<PaymentRefusal>,
    },
}

impl PeriodPriceChoice {
    fn price(&self) -> Result<PeriodPrice<'_>, WalletError> {
        Ok(match self {
            Self::RequestedAmount => PeriodPrice::RequestedAmount,
            Self::QuoteFor { endpoint } => PeriodPrice::QuoteFor(
                PaymentEndpointIdentifier::new(endpoint)
                    .map_err(|_| WalletError::invalid("endpoint identifier"))?,
            ),
            Self::AtRate { quote } => {
                let Refusal::PricedByRate { payment } = &quote.0 else {
                    return Err(WalletError::invalid("quote"));
                };
                PeriodPrice::AtRate(payment)
            }
        })
    }
}

impl RecurringRequest {
    fn from_view(
        request: paykit_sdk::RequestView,
        recurrence: paykit_lib::periods::Recurrence,
    ) -> Result<Self, WalletError> {
        Ok(Self {
            id: request.request.into(),
            record: request.record.into(),
            amount: (&request.amount).into(),
            payment_reference: request.payment_reference,
            label: request.label,
            proposal_expires_at_ms: request.proposal_expires_at_ms,
            status: request.status.into(),
            held: request.held.map(|held| {
                Arc::new(PaymentRefusal(Refusal::HeldAfterRestore {
                    held: Box::new(held),
                }))
            }),
            manual_only: request.manual_only,
            divergent: request.divergent,
            receipt: request.receipt.map(TryInto::try_into).transpose()?,
            recurrence: recurrence.into(),
            periods: request
                .periods
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}

/// A signed message and where its delivery stands. A kept batch goes at the wallet's next
/// write or read, so calling again would send a second message (Switch spec 11).
#[derive(uniffi::Record)]
pub struct SentMessage {
    /// The signed message's ID (Switch wire 5.1).
    pub msg_id: String,
    /// Where delivery stands (Switch spec 11).
    pub delivery: MessageDelivery,
}

impl From<paykit_sdk::SentMessage> for SentMessage {
    fn from(sent: paykit_sdk::SentMessage) -> Self {
        Self {
            msg_id: sent.msg_id,
            delivery: sent.delivery.into(),
        }
    }
}

#[uniffi::export]
impl NativeWallet {
    /// List recurring requests and their period records as of the last read. Receive brings
    /// the local view up to date (Paykit spec 8.1, 8.2, 8.5).
    pub async fn recurring_requests(&self) -> Result<Vec<RecurringRequest>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                app.requests()
                    .await?
                    .into_iter()
                    .filter_map(|request| {
                        request
                            .recurrence
                            .map(|rule| RecurringRequest::from_view(request, rule))
                    })
                    .collect()
            })
        })
        .await
    }

    /// Record and queue the payee's details for an eligible unpaid period of an accepted
    /// subscription. Each message stays valid until its own expiry. Return the message and
    /// its delivery, which is kept when the batch waits for the next write or read (Paykit
    /// spec 8.2, Paykit wire 4.9).
    pub async fn send_period_details(
        &self,
        record: Vec<u8>,
        details: PeriodDetails,
    ) -> Result<SentMessage, WalletError> {
        let record = fixed(record, "request record")?;
        let details = details.try_into()?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.send_period_details(&record, details).await?.into()) })
        })
        .await
    }

    /// Record and send acceptance within the proposal's expiry, returning the message and its
    /// delivery. Held and blocked requests require the user's exact confirmation. Each period
    /// is paid separately (Paykit spec 7.2, 8.1, 8.2).
    pub async fn accept_subscription(
        &self,
        id: RequestId,
        options: Option<AcceptOptions>,
    ) -> Result<SentMessage, WalletError> {
        let request = id.try_into()?;
        let options = options.map(TryInto::try_into).transpose()?;
        self.with_wallet(move |app| {
            Box::pin(async move {
                let sent = match options {
                    Some(options) => app.accept_subscription_with(&request, options).await?,
                    None => app.accept_subscription(&request).await?,
                };
                Ok(sent.into())
            })
        })
        .await
    }

    /// Record the acceptance and its approved Allowance together and return the message and
    /// its delivery. Each period still requires eligibility and available budget (Paykit spec
    /// 8.2, 8.3).
    pub async fn accept_subscription_under_allowance(
        &self,
        id: RequestId,
        allowance: Vec<u8>,
    ) -> Result<SentMessage, WalletError> {
        let request = id.try_into()?;
        let allowance = fixed(allowance, "allowance")?;
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .accept_subscription_under_allowance(&request, &allowance)
                    .await?
                    .into())
            })
        })
        .await
    }

    /// Pay one period by hand with the user's confirmations. A signed rate requires approval
    /// of its exact quote. The attempt marks the period manual-only and the proof names the
    /// period and any signed details used (Paykit spec 7.2, 8.2, 8.3).
    pub async fn pay_period(
        &self,
        id: RequestId,
        period: BillingPeriod,
        options: PaymentOptions,
    ) -> Result<PaymentResult, WalletError> {
        let request = id.try_into()?;
        let period = period.try_into()?;
        let options = options.try_into()?;
        let rail = self.rail();
        self.with_wallet(move |app| {
            Box::pin(async move {
                if !rail.has_lookup() {
                    return Err(WalletError::PaymentMethodWithoutLookup);
                }
                Ok(app.pay_period(&request, &period, options).await.into())
            })
        })
        .await
    }

    /// Pay an eligible, unfunded period under the host's policy, or request its signed rate
    /// quote. A rate-priced payment requires approval of the exact quote (Paykit spec 7.2, 8.2).
    pub async fn pay_period_automatically(
        &self,
        id: RequestId,
        period: BillingPeriod,
        price: PeriodPriceChoice,
    ) -> Result<PaymentResult, WalletError> {
        let request = id.try_into()?;
        let period = period.try_into()?;
        // The host's choice is checked before anything else (Paykit spec 7.2).
        price.price()?;
        let rail = self.rail();
        self.with_wallet(move |app| {
            Box::pin(async move {
                if !rail.has_lookup() {
                    return Err(WalletError::PaymentMethodWithoutLookup);
                }
                Ok(app
                    .pay_period_automatically(&request, &period, price.price()?)
                    .await
                    .into())
            })
        })
        .await
    }

    /// Pay the period under its subscription's selected Allowance, reserving budget and opening
    /// the claim together. Currency conversion is ineligible (Paykit spec 8.2, 8.3).
    pub async fn pay_period_under_allowance(
        &self,
        id: RequestId,
        period: BillingPeriod,
    ) -> Result<PaymentResult, WalletError> {
        let request = id.try_into()?;
        let period = period.try_into()?;
        let rail = self.rail();
        self.with_wallet(move |app| {
            Box::pin(async move {
                if !rail.has_lookup() {
                    return Err(WalletError::PaymentMethodWithoutLookup);
                }
                Ok(app
                    .pay_period_under_allowance(&request, &period)
                    .await
                    .into())
            })
        })
        .await
    }
}
