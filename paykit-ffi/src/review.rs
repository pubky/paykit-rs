use crate::{Amount, PaymentRefusal, RequestId};
use paykit_sdk::{RateSource, Refusal};
use std::sync::Arc;

/// The provider loss that decides a payment doubt (Paykit spec 7.3).
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
    /// The original obligation and loss for an explicit not-paid statement. A notice that
    /// the loss changed has no payment to confirm: read the doubtful payment again.
    pub payment: Option<Arc<PaymentBeforeLoss>>,
}
impl From<paykit_sdk::LossBoundary> for ProviderLossBoundary {
    fn from(v: paykit_sdk::LossBoundary) -> Self {
        Self {
            generation: v.generation(),
            declared_at_ms: v.declared_at_ms(),
            uncertain_until_ms: v.uncertain_until_ms(),
            payment: None,
        }
    }
}
/// An obligation and its original deciding provider loss, as shown by the wallet.
#[derive(uniffi::Object)]
pub struct PaymentBeforeLoss {
    pub(crate) obligation: [u8; 32],
    pub(crate) loss: paykit_sdk::LossBoundary,
}
impl ProviderLossBoundary {
    pub(crate) fn for_payment(obligation: [u8; 32], loss: paykit_sdk::LossBoundary) -> Self {
        Self {
            payment: Some(Arc::new(PaymentBeforeLoss { obligation, loss })),
            ..Self::from(loss)
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
    /// The payee may have cancelled the original request during the gap.
    HeldAfterRestore {
        /// The held activity record requiring the user's confirmation.
        record: Vec<u8>,
    },
    /// The payee is on the user's deny list. The user must see this exact payment before confirming it.
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
    /// An earlier attempt may have paid the obligation. Only a supplied doubt can be confirmed.
    MayBePaid {
        /// What the user may pay over.
        doubt: Option<PriorPaymentDoubt>,
    },
    /// The provider loss that decides the doubt changed before confirmation.
    LossChanged {
        /// The current provider-loss boundary the host must review.
        boundary: Option<ProviderLossBoundary>,
    },
    /// The method's proposed transfer requires the user's approval.
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
    /// This payment requires authorization of the original rate, destination and resulting amount.
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
    /// The delivery of the counterparty's current Private Payment List is unknown. Only the
    /// user can authorize another version.
    ListDeliveryUnknown {
        /// The counterparty the list was sent to.
        recipient: Vec<u8>,
        /// The publishing identity generation of the list's version.
        generation: u64,
        /// The list version's counter.
        counter: u64,
        /// The list message's ID.
        msg_id: String,
        /// Whether the list was empty, withdrawing an earlier one.
        withdrawal: bool,
        /// For a withdrawal, when an earlier list may last be paid; otherwise when the list
        /// expires. `None` when it can't be bounded.
        payable_until_ms: Option<u64>,
    },
    /// The host must show the affected records and their observed versions before asking for confirmation.
    Divergent {
        /// The records the user is asked to confirm.
        records: Vec<Divergence>,
    },
    /// The app holds no such request, record or payment.
    NotFound,
    /// The certificate lacks a scope required for this call.
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
    /// Another wallet owns an open attempt on this obligation.
    BeingHandledByOtherWallet,
    /// The attempt's deadline has passed, or the request's proposal expired (Paykit spec 7.1).
    DeadlinePassed,
    /// This request is payable per Billing Period.
    Recurring,
    /// The settlement may already have been credited.
    MaybeCredited,
    /// The payment was verified, but its verified payment time lies outside what the request
    /// allows: before the request less the method's tolerance, or past its deadline or the
    /// validity of its destination or rates (Paykit spec 7.1, 8.2). Nothing is credited.
    PaymentTimeOutside {
        /// The verified payment time, in Unix milliseconds.
        paid_at_ms: u64,
    },
    /// The recurring request is accepted already, by this app or another own app (Paykit spec
    /// 8.2): it is neither accepted nor rejected again, only cancelled.
    Subscribed,
    /// The payer must accept the recurring request before paying a Billing Period (Paykit spec 8.2).
    NotSubscribed,
    /// Automatic payment must wait for the period. Paying early requires explicit confirmation.
    PeriodNotStarted,
    /// The occurrence carries a manual-only mark or is funded under an Allowance.
    ManualOnly,
    /// The subscription or service time does not authorize automatic payment.
    NotAutomatic,
    /// All retained period slots may have an open attempt.
    AttemptedPeriodsFull,
    /// The destination failed the payment checks.
    Destination,
    /// An attempt was written and failed before dispatch. Inspect attemptedFailure for the original refusal.
    AttemptFailed,
    /// The wallet could not find a usable destination.
    NoDestination,
    /// The method cannot pay the selected endpoint.
    UnsupportedEndpoint,
    /// A usable Private Payment List is required.
    WaitingForList,
    /// The wallet could not read or revalidate the identity required for payment.
    IdentityUnavailable,
    /// The payee identity needs explicit confirmation.
    IdentityUnconfirmed,
    /// The claim's attempt is open: a rejection or cancellation is refused while a payment is in
    /// progress (Paykit spec 8.1).
    PaymentInProgress,
    /// The current identity does not authorize this installation or attempt.
    NotCurrent,
    /// The call doesn't apply now: the text says why.
    NotNow,
    /// The identity's block floor is held after it was seen divergent: no unblock or widening
    /// that needs a raise goes, and retrying changes nothing.
    FloorHeld,
    /// A value the protocol or the host gave breaks a rule: the text names it.
    Usage,
    /// A payment action is held by its record. If the payer's payment history was lost to
    /// deletion, the obligation stays held and the user cannot confirm another payment.
    RecordHeld,
    /// Paykit refused the app's certificate.
    CertificateRefused,
    /// The wallet's own Switch service did not answer.
    SwitchUnavailable,
    /// A native case that this binding does not recognize.
    Unknown,
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
                    before_loss: d
                        .before_loss
                        .map(|loss| ProviderLossBoundary::for_payment(d.obligation, loss)),
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
                    RateSource::Request => None,
                    _ => return D::Unknown,
                },
                rate: Amount {
                    asset: p.rate.asset().as_str().into(),
                    value: p.rate.value().as_str().into(),
                },
                amount: (&p.amount).into(),
            },
            Refusal::SettlementBucketFull { bucket } => D::SettlementBucketFull { bucket: *bucket },
            Refusal::ListDeliveryUnknown(list) => D::ListDeliveryUnknown {
                recipient: list.recipient().into(),
                generation: list.version().generation(),
                counter: list.version().counter(),
                msg_id: list.msg_id().to_owned(),
                withdrawal: list.withdrawal(),
                payable_until_ms: list.payable_until_ms(),
            },
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
            Refusal::PaymentTimeOutside { paid_at_ms } => D::PaymentTimeOutside {
                paid_at_ms: *paid_at_ms,
            },
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
            Refusal::FloorHeld => D::FloorHeld,
            Refusal::Usage(_) => D::Usage,
            Refusal::RecordHeld(_) => D::RecordHeld,
            Refusal::CertificateRefused(_) => D::CertificateRefused,
            Refusal::SwitchUnavailable(_) => D::SwitchUnavailable,
            _ => D::Unknown,
        }
    }
}

/// A record version (Switch wire 1.7, 6.2): the (generation, `seq`) of the write that
/// produced it, given by the service.
#[derive(uniffi::Record)]
pub struct Version {
    /// The generation of the write.
    pub generation: u64,
    /// The `seq` of the write in its generation.
    pub version: u64,
}
impl From<pubky_switch_core::ops::RecordVersion> for Version {
    fn from(v: pubky_switch_core::ops::RecordVersion) -> Self {
        Self {
            generation: v.generation,
            version: v.version,
        }
    }
}

/// A record the app has seen divergent, as the host shows it (Paykit spec 6): from
/// `Paykit::divergent_records`, a receive's summary, or a refusal (`Refusal::Divergent`).
/// After showing these fields, pass the original observation to the wallet for confirmation.
#[derive(uniffi::Record)]
pub struct Divergence {
    /// The record key.
    pub record: Vec<u8>,
    /// Its collection: what the upload may hold a newer copy of, such as a cancellation in a
    /// request record or a newer list in a `payment_lists` record.
    pub collection: String,
    /// Paused: nothing is paid, accepted or credited on it automatically, and by hand only once
    /// the user confirms (`Paykit::confirm_divergent`).
    pub paused: bool,
    /// The latest view the app read shows it divergent now.
    pub divergent: bool,
    /// The write of the record the app holds, the one shown; `None` for a record it holds no
    /// more. `Paykit::confirm_divergent` confirms only that write.
    pub version: Option<Version>,
    /// Shown for a credit (Paykit spec 8.1 step 4): the record may hold a credit of a settlement
    /// its current write lacks, a receipt in its upload, so the app can't tell a proof isn't
    /// credited already.
    pub may_hold_credit: bool,
    /// The user has confirmed it, and the messages kept over it, receipts included, are written
    /// at the next receive (`Paykit::receive`): until then nothing is paid, accepted or credited
    /// on it, by hand or automatically.
    pub writes_waiting: bool,
    /// The original SDK observation. Return it only after showing this record to the user.
    pub observation: Arc<DivergentObservation>,
}
/// The original divergent record, including its observed write and credit warning.
#[derive(uniffi::Object)]
pub struct DivergentObservation(pub(crate) paykit_sdk::DivergentRecord);

impl From<paykit_sdk::DivergentRecord> for Divergence {
    fn from(v: paykit_sdk::DivergentRecord) -> Self {
        Self {
            record: v.record.into(),
            collection: v.collection.name().into(),
            paused: v.paused,
            divergent: v.divergent,
            version: v.version.map(Into::into),
            may_hold_credit: v.may_hold_credit,
            writes_waiting: v.writes_waiting,
            observation: Arc::new(DivergentObservation(v)),
        }
    }
}
