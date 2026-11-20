use crate::{Amount, Divergence, PaymentRefusal, RequestId};
use paykit_sdk::{RateSource, Refusal};

/// A provider loss, as Paykit keeps the one the app followed last (`p/pl`) for the doubt it
/// leaves (Paykit spec 7.3), and as a doubt after a loss names the loss that decides it
/// (`PaymentDoubt::before_loss`, `Doubtful::before_loss`).
#[derive(uniffi::Record)]
pub struct ProviderLossBoundary {
    /// The generation the loss recovered at: of two losses, the newer is the one at the later
    /// generation.
    pub generation: u64,
    /// When the loss was declared, in Unix milliseconds of the service; 0 while the client
    /// doesn't know it.
    pub declared_at_ms: u64,
    /// Until when, in Unix milliseconds of the service, an attempt could have been authorized at
    /// the old service.
    pub uncertain_until_ms: u64,
}
impl From<paykit_sdk::LossBoundary> for ProviderLossBoundary {
    fn from(v: paykit_sdk::LossBoundary) -> Self {
        Self {
            generation: v.generation(),
            declared_at_ms: v.declared_at_ms(),
            uncertain_until_ms: v.uncertain_until_ms(),
        }
    }
}
/// Evidence that an earlier attempt may already have paid this obligation.
#[derive(uniffi::Record)]
pub struct PriorPaymentDoubt {
    /// The stable obligation whose attempts are in doubt.
    pub obligation: Vec<u8>,
    /// The claim step that was checked, if available.
    pub step: Option<u64>,
    /// Restored attempts whose outcomes still matter to the decision.
    pub restored: Vec<String>,
    /// Attempts with unknown outcomes.
    pub unknown: Vec<String>,
    /// Attempts whose original claim or journal evidence is incomplete.
    pub orphaned: Vec<String>,
    /// The next payment could duplicate the prior transfer.
    pub next_possibly_duplicate: bool,
    /// The activity record or its history diverges.
    pub divergent: bool,
    /// The evidence falls after a recorded distrust boundary.
    pub after_distrust: bool,
    /// The provider pin requires review before use.
    pub early_pin: bool,
    /// The declared loss interval that affects these attempts, if any.
    pub before_loss: Option<ProviderLossBoundary>,
}
/// The SDK refusal, including the exact quote, restored obligation or other evidence for review.
#[derive(uniffi::Enum)]
pub enum RefusalKind {
    /// The original request could have been cancelled while the wallet lacked coverage.
    HeldAfterRestore {
        /// The held activity record requiring the user's confirmation.
        record: Vec<u8>,
    },
    /// The identity's deny list holds the payee: what is open with it is paused, and its request
    /// is paid only once the user confirms (Paykit spec 8.1, "Blocked counterparties"). `payment`
    /// is what the user is shown; paying anyway carries it back
    /// (`PayOptions::with_blocked_confirmed`).
    Blocked {
        /// The payee's identity key.
        payee: Vec<u8>,
        /// The original request associated with the refusal.
        request: Option<RequestId>,
        /// The named Billing Period when this payment belongs to a recurring request.
        period: Option<crate::BillingPeriod>,
        /// The amount of the proposed payment to a blocked payee.
        amount: Amount,
    },
    /// The obligation may already be paid: an `unknown` or orphaned attempt, a next attempt that
    /// would be flagged `possibly_duplicate`, a pinned deadline other than the app's own, an
    /// attempt a restore left unresolved, or an obligation payable before a provider loss (Paykit
    /// spec 7.3).
    MayBePaid {
        /// What the user may pay over.
        doubt: Option<PriorPaymentDoubt>,
    },
    /// The user's answer names a provider loss that no longer decides the doubt it answers
    /// (Paykit spec 7.3): a newer loss is kept, or followed before its step has run, since the
    /// doubt was shown. `decisive` is the loss that decides now, `None` for none; the host shows
    /// the doubt again under it.
    LossChanged {
        /// The current provider-loss boundary the host must review.
        boundary: Option<ProviderLossBoundary>,
    },
    /// The payment method would transfer another amount or asset, an amount it can't tell, or a
    /// fee above the app's cap, and the user didn't choose it (Paykit spec 7.2 step 1). `quote`
    /// is what it would transfer, to show the user; paying it anyway carries it back
    /// (`PayOptions::with_transfer_confirmed`).
    Transfer {
        /// The payee's identity key.
        payee: Vec<u8>,
        /// The 16-byte Switch App ID.
        app_id: Vec<u8>,
        /// The selected Payment Endpoint Identifier.
        endpoint: String,
        /// Hash of the exact selected Payment Endpoint Payload.
        payload_hash: Vec<u8>,
        /// The amount in the original request.
        requested: Amount,
        /// The exact transfer the payment method proposes, if known.
        amount: Option<Amount>,
        /// The method's maximum fee, if known.
        max_fee: Option<String>,
    },
    /// The selected payment requires confirmation of the original rate and transfer amount.
    PricedByRate {
        /// The original request associated with the refusal.
        request: RequestId,
        /// The named Billing Period when this payment belongs to a recurring request.
        period: Option<crate::BillingPeriod>,
        /// Hash of the signed message that supplies the rate.
        source_hash: Vec<u8>,
        /// The 16-byte Switch App ID.
        app_id: Vec<u8>,
        /// The selected Payment Endpoint Identifier.
        endpoint: String,
        /// Hash of the exact selected Payment Endpoint Payload.
        payload_hash: Vec<u8>,
        /// The amount in the original request.
        requested: Amount,
        /// The signed period details message ID if those details supplied the rate.
        detail_message_id: Option<String>,
        /// The exact quoted rate expressed in the destination asset.
        rate: Amount,
        /// The exact amount resulting from the quoted rate.
        amount: Amount,
    },
    /// The settlement mark cannot be stored.
    SettlementBucketFull {
        /// The settlement bucket that could not accept the mark.
        bucket: u16,
    },
    /// A record the call acts on is divergent after a provider loss, or was seen so, and the user
    /// hasn't confirmed it (Paykit spec 6, "Divergent records"): its upload may hold what the
    /// current write lacks, such as a cancellation of the request, a receipt, or a newer list in
    /// which the payee withdrew a destination.
    Divergent {
        /// The records the user is asked to confirm.
        records: Vec<Divergence>,
    },
    /// The app holds no such request, record or payment.
    NotFound,
    /// The call needs a scope the app's certificate doesn't hold, such as `paykit_pay` to pay or
    /// `paykit_request` to send a request (Paykit spec 3.1).
    ScopeMissing {
        /// The missing Paykit scope.
        scope: String,
    },
    /// A labelled selling app sends every request under one of its labels, and only under a label
    /// it holds (Paykit spec 3.1, 8.1).
    Label,
    /// The request was rejected by this identity (Paykit spec 8.1).
    Rejected,
    /// The request was cancelled by either side (Paykit spec 8.1).
    Cancelled,
    /// The Allowance has ended, by either side or by its proposal's withdrawal, or this app keeps
    /// the counterparty's end as its stop: nothing more is done under it (Paykit spec 8.3).
    AllowanceEnded,
    /// The Allowance is conflicted: a second, different proposal or response came, so nothing is
    /// answered, ended, granted or paid under it, for good (Paykit spec 8.3, "Conflicted").
    AllowanceConflicted,
    /// A payee activity record lost credit history to a delete.
    CreditHistoryLost,
    /// The obligation is paid: its claim is SETTLED, or its record holds the proof of a payment
    /// (Paykit spec 7.2).
    AlreadyPaid,
    /// Another own app has an open attempt on the obligation: "being handled by your other
    /// wallet" (Paykit spec 7.1, 8.5).
    BeingHandledByOtherWallet,
    /// The attempt's deadline has passed, or the request's proposal expired (Paykit spec 7.1).
    DeadlinePassed,
    /// A recurring request is paid per Billing Period (Paykit spec 8.2): `pay_request` refuses
    /// it, and `Paykit::pay_period` pays it.
    Recurring,
    /// The settlement may already have been credited.
    MaybeCredited,
    /// The recurring request is accepted already, by this app or another own app (Paykit spec
    /// 8.2): it is neither accepted nor rejected again, only cancelled.
    Subscribed,
    /// The payer must accept the recurring request before paying a Billing Period (Paykit spec 8.2).
    NotSubscribed,
    /// The Billing Period hasn't started on the latest service time verified: it is paid only by
    /// the user, after confirming paying it early (`PayOptions::with_early_payment_confirmed`;
    /// Paykit spec 8.2, "Not before its start").
    PeriodNotStarted,
    /// The occurrence carries a manual-only mark or is funded under an Allowance.
    ManualOnly,
    /// The authenticated subscription and service time do not authorize this automatic Billing
    /// Period.
    NotAutomatic,
    /// The request record lists 16 Billing Periods that may have an open attempt and none of them
    /// has finished: no attempt is made on another period until one does (Paykit spec 8.2,
    /// "Attempted periods").
    AttemptedPeriodsFull,
    /// The destination check failed (Paykit spec 4, 7.2): the text names the rule.
    Destination,
    /// The attempt was written, and a check before money moved then refused the payment: the app
    /// wrote `failed` on it instead of paying, and nothing was paid (Paykit spec 7.2 step 1).
    AttemptFailed,
    /// No usable destination: no endpoint of the request, no usable Private Payment List, no
    /// usable public endpoint, or none the caller named (Paykit spec 4, 8.1, 8.4).
    NoDestination,
    /// The destination offers endpoints the payment may use, but the payment method pays none of
    /// them (`Rail::select_endpoints`), or the caller named one it doesn't pay.
    UnsupportedEndpoint,
    /// No usable Private Payment List from the payee: none held, or each one expired, of a
    /// version used up by a single-use endpoint of it reserved, or of a generation above the
    /// payee's verified `identity.json` (Paykit spec 8.4, "No usable list"; Paykit wire 4.13);
    /// also when lists in use offer only endpoints the payment method, the conversion terms, the
    /// caller's endpoint or a request's accepted identifiers leave unusable, beside one used up,
    /// or, for the first two, beside one expired, whose newer list may serve
    /// (`Rail::select_endpoints`).
    WaitingForList,
    /// The payee's `identity.json` couldn't be read, or the app's own couldn't be revalidated,
    /// before money moves (Paykit spec 4, 7.2).
    IdentityUnavailable,
    /// The payee identity needs explicit confirmation.
    IdentityUnconfirmed,
    /// The claim's attempt is open: a rejection or cancellation is refused while a payment is in
    /// progress (Paykit spec 8.1).
    PaymentInProgress,
    /// The own `identity.json` doesn't authorize the payment: it doesn't list this app with the
    /// certificate it holds, or names another service or generation than the one the attempt was
    /// written at, after a provider change (Paykit spec 4, Switch spec 12.5, 12.6).
    NotCurrent,
    /// The call doesn't apply now: the text says why.
    NotNow,
    /// A value the protocol or the host gave breaks a rule: the text names it.
    Usage,
    /// `done` waits for the payer's activity record, which it never goes without (Paykit spec 7.2
    /// step 3): the record holds a value that doesn't read, with no copy this app trusts, or is
    /// gone with nothing to rebuild it from.
    RecordHeld,
    /// The app's certificate fails Paykit's checks (Paykit wire 3.1), though the Switch took it:
    /// at a join, before anything is written, or after a renewal the client adopted.
    CertificateRefused,
    /// The app's own switch doesn't answer: the network, a `503`, a `5xx` its key didn't sign,
    /// or, while the account may have been erased, the wait before the app takes it as gone
    /// (`AppError::NotAnsweringAsAccount`, Switch spec 12.7).
    SwitchUnavailable,
    /// A native case that this binding does not recognize.
    Other,
}
#[uniffi::export]
impl PaymentRefusal {
    /// Return the reason and the evidence carried by this refusal.
    pub fn kind(&self) -> RefusalKind {
        use RefusalKind as D;
        match &self.0 {
            Refusal::HeldAfterRestore { held } => D::HeldAfterRestore {
                record: held.record().into(),
            },
            Refusal::Blocked { payment: p } => D::Blocked {
                payee: p.payee.into(),
                request: p.request.map(Into::into),
                period: p.period.map(Into::into),
                amount: (&p.amount).into(),
            },
            Refusal::MayBePaid { doubt, .. } => D::MayBePaid {
                doubt: doubt.as_ref().map(|d| PriorPaymentDoubt {
                    obligation: d.obligation.into(),
                    step: d.step,
                    restored: d.restored.iter().map(|i| i.to_text()).collect(),
                    unknown: d.unknown.iter().map(|i| i.to_text()).collect(),
                    orphaned: d.orphaned.iter().map(|i| i.to_text()).collect(),
                    next_possibly_duplicate: d.next_possibly_duplicate,
                    divergent: d.divergent,
                    after_distrust: d.after_distrust,
                    early_pin: d.early_pin,
                    before_loss: d.before_loss.map(Into::into),
                }),
            },
            Refusal::LossChanged { decisive } => D::LossChanged {
                boundary: decisive.map(Into::into),
            },
            Refusal::Transfer { quote: q, .. } => D::Transfer {
                payee: q.payee.into(),
                app_id: q.app_id.into(),
                endpoint: q.endpoint_id.as_str().into(),
                payload_hash: q.payload_hash.into(),
                requested: (&q.amount).into(),
                amount: q.transfer.amount.as_ref().map(Into::into),
                max_fee: q.transfer.max_fee.as_ref().map(|v| v.as_str().into()),
            },
            Refusal::PricedByRate { payment: p } => D::PricedByRate {
                request: p.request.into(),
                period: p.billing_period.map(Into::into),
                source_hash: p.rate_source_hash.into(),
                app_id: p.app_id.into(),
                endpoint: p.endpoint_id.as_str().into(),
                payload_hash: p.payload_hash.into(),
                requested: (&p.requested).into(),
                detail_message_id: match &p.source {
                    RateSource::PeriodDetails { msg_id, .. } => Some(msg_id.clone()),
                    _ => None,
                },
                rate: Amount {
                    asset: p.rate.asset().as_str().into(),
                    value: p.rate.value().as_str().into(),
                },
                amount: (&p.amount).into(),
            },
            Refusal::SettlementBucketFull { bucket } => D::SettlementBucketFull { bucket: *bucket },
            Refusal::Divergent { records } => D::Divergent {
                records: records.iter().cloned().map(Into::into).collect(),
            },
            Refusal::NotFound => D::NotFound,
            Refusal::ScopeMissing(scope) => D::ScopeMissing {
                scope: scope.clone(),
            },
            Refusal::Label(_) => D::Label,
            Refusal::Rejected => D::Rejected,
            Refusal::Cancelled => D::Cancelled,
            Refusal::AllowanceEnded => D::AllowanceEnded,
            Refusal::AllowanceConflicted => D::AllowanceConflicted,
            Refusal::CreditHistoryLost => D::CreditHistoryLost,
            Refusal::AlreadyPaid => D::AlreadyPaid,
            Refusal::BeingHandledByOtherWallet => D::BeingHandledByOtherWallet,
            Refusal::DeadlinePassed => D::DeadlinePassed,
            Refusal::Recurring => D::Recurring,
            Refusal::MaybeCredited => D::MaybeCredited,
            Refusal::Subscribed => D::Subscribed,
            Refusal::NotSubscribed => D::NotSubscribed,
            Refusal::PeriodNotStarted => D::PeriodNotStarted,
            Refusal::ManualOnly => D::ManualOnly,
            Refusal::NotAutomatic(_) => D::NotAutomatic,
            Refusal::AttemptedPeriodsFull => D::AttemptedPeriodsFull,
            Refusal::Destination(_) => D::Destination,
            Refusal::AttemptFailed(_) => D::AttemptFailed,
            Refusal::NoDestination(_) => D::NoDestination,
            Refusal::UnsupportedEndpoint(_) => D::UnsupportedEndpoint,
            Refusal::WaitingForList => D::WaitingForList,
            Refusal::IdentityUnavailable(_) => D::IdentityUnavailable,
            Refusal::IdentityUnconfirmed => D::IdentityUnconfirmed,
            Refusal::PaymentInProgress => D::PaymentInProgress,
            Refusal::NotCurrent(_) => D::NotCurrent,
            Refusal::NotNow(_) => D::NotNow,
            Refusal::Usage(_) => D::Usage,
            Refusal::RecordHeld(_) => D::RecordHeld,
            Refusal::CertificateRefused(_) => D::CertificateRefused,
            Refusal::SwitchUnavailable(_) => D::SwitchUnavailable,
            _ => D::Other,
        }
    }
}
