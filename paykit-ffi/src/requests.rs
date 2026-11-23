//! Original one-time payment requests, choices and outcomes for the host.
use crate::{
    fixed, Amount, NativeWallet, PaymentOutcome, PaymentRefusal, Receipt, RequestId, WalletError,
};
use paykit_sdk::{PayOptions, PaymentMode, Refusal, UserConfirmed};
use std::sync::Arc;

/// A request the payer holds.
#[derive(uniffi::Record)]
pub struct OneTimeRequest {
    /// The payee and authenticated request hash used to select this payment.
    pub id: RequestId,
    /// The record key of the payer's request record.
    pub record: Vec<u8>,
    /// The amount requested.
    pub amount: Amount,
    /// The payment reference.
    pub payment_reference: String,
    /// The label of the selling scope it was sent under.
    pub label: Option<String>,
    /// The proposal expiry in Unix milliseconds, when present.
    pub proposal_expires_at_ms: Option<u64>,
    /// What the user sees.
    pub status: OneTimeRequestStatus,
    /// The original restored hold. Pass it back only after showing the risk of cancellation.
    pub held: Option<Arc<PaymentRefusal>>,
    /// A formerly blocked request can be paid only after an explicit user action.
    pub manual_only: bool,
    /// Its request record is paused after a view showed it divergent, or its latest view shows it
    /// divergent (Paykit spec 6): shown whatever the status (`Paykit::divergent_records`).
    pub divergent: bool,
    /// The payee's receipt the record holds, if any (Paykit wire 4.11), as this app's copy shows
    /// it: for display.
    pub receipt: Option<Receipt>,
}
/// What the payer sees for a request (Paykit spec 8.5).
#[derive(uniffi::Enum)]
pub enum OneTimeRequestStatus {
    /// Nothing done yet.
    Open,
    /// This app's attempt is open.
    BeingPaidHere,
    /// "Being handled by your other wallet": another own app's attempt is open.
    BeingHandledByOtherWallet,
    /// An attempt has an unknown outcome, is orphaned, or the payer's one-time request
    /// lost its payment history to deletion. The wallet offers no confirmation to pay a
    /// deletion-marked request (Paykit spec 6, 7.1).
    MayBePaid,
    /// SETTLED, or the record holds the proof of a payment: "Paid" only then.
    Paid,
    /// Rejected by this identity.
    Rejected,
    /// Cancelled by either side.
    Cancelled,
    /// Its proposal expired unpaid.
    Expired,
    /// Held after a restore or a `missed` item: the payee may have cancelled it since the backup,
    /// or among the entries the service deleted, and it is paid only once the user has seen that
    /// (Paykit spec 7.2, `PayOptions::possibly_cancelled_confirmed`,
    /// `Paykit::confirm_possibly_cancelled`).
    Held,
    /// Received, but its record couldn't be written: the switch refused the write for good, such
    /// as over a quota (Switch spec 8.2).
    Unrecorded,
    /// From a payee the identity's deny list holds, as the app last read it: paused, and paid
    /// only once the user confirms (Paykit spec 8.1, `Refusal::Blocked`).
    Blocked,
    /// Its request record is divergent after a provider loss, or was seen so, and the user hasn't
    /// confirmed it: the payee may have cancelled it.
    Divergent,
    /// A native case that this binding does not recognize.
    Other,
}
impl From<paykit_sdk::RequestStatus> for OneTimeRequestStatus {
    fn from(s: paykit_sdk::RequestStatus) -> Self {
        use paykit_sdk::RequestStatus as R;
        match s {
            R::Open => Self::Open,
            R::BeingPaidHere => Self::BeingPaidHere,
            R::BeingHandledByOtherWallet => Self::BeingHandledByOtherWallet,
            R::MayBePaid => Self::MayBePaid,
            R::Paid => Self::Paid,
            R::Rejected => Self::Rejected,
            R::Cancelled => Self::Cancelled,
            R::Expired => Self::Expired,
            R::Held => Self::Held,
            R::Unrecorded => Self::Unrecorded,
            R::Blocked => Self::Blocked,
            R::Divergent => Self::Divergent,
            _ => Self::Other,
        }
    }
}
/// A manual payment's choices.
#[derive(Default, uniffi::Record)]
pub struct PaymentOptions {
    /// The Payment Endpoint Identifier to pay; the first usable one when `None`.
    pub endpoint: Option<String>,
    /// The payee app whose Private Payment List to pay, for a destination that isn't the
    /// request's own; any current one when `None`.
    pub list_app: Option<Vec<u8>>,
    /// The user explicitly chose to pay using public endpoints.
    pub public_endpoints_confirmed: bool,
    /// The user has seen that the payee's switch couldn't confirm the payee's apps, and pays
    /// anyway (`Refusal::IdentityUnconfirmed`; Switch spec 4.3, Paykit spec 7.2).
    pub unconfirmed_identity_confirmed: bool,
    /// Exact refusals the user has reviewed and explicitly confirmed for this payment.
    pub confirmed_refusals: Vec<Arc<PaymentRefusal>>,
}
impl TryFrom<PaymentOptions> for PayOptions {
    type Error = WalletError;
    fn try_from(o: PaymentOptions) -> Result<Self, Self::Error> {
        let mut p = Self::default();
        p.endpoint = o
            .endpoint
            .as_deref()
            .map(paykit_lib::PaymentEndpointIdentifier::new)
            .transpose()
            .map_err(|_| WalletError::invalid("endpoint identifier"))?;
        p.list_app = o.list_app.map(|id| fixed(id, "list app")).transpose()?;
        if o.public_endpoints_confirmed {
            p.mode = PaymentMode::Public(UserConfirmed::new())
        }
        if o.unconfirmed_identity_confirmed {
            p.unconfirmed_identity_confirmed = Some(UserConfirmed::new())
        }
        for confirmed in o.confirmed_refusals {
            match &confirmed.0 {
                Refusal::HeldAfterRestore { held } => {
                    p.possibly_cancelled_confirmed = Some(*held.clone())
                }
                Refusal::MayBePaid {
                    doubt: Some(doubt), ..
                } => p.may_be_paid_confirmed = Some(*doubt.clone()),
                Refusal::Blocked { payment } => p.blocked_confirmed = Some(*payment.clone()),
                Refusal::Transfer { quote, .. } => p.transfer_confirmed = Some(*quote.clone()),
                Refusal::PricedByRate { payment } => p.rate_authorized = Some(*payment.clone()),
                _ => return Err(WalletError::invalid("payment confirmation")),
            }
        }
        Ok(p)
    }
}

/// The result of the payment decision, alongside the enclosing per-app failures.
#[derive(uniffi::Enum)]
pub enum PaymentDecision {
    /// The SDK recorded a payment outcome.
    Outcome {
        /// The outcome for the original attempt.
        outcome: PaymentOutcome,
    },
    /// The SDK returned a refusal or operational failure.
    Error {
        /// The payment refusal or failure.
        error: WalletError,
    },
}
/// The payment outcome or error and any payee apps whose public endpoints the call could not
/// use, with the reason for each (Paykit spec 4).
#[derive(uniffi::Record)]
pub struct PaymentResult {
    /// The original payment outcome or failure.
    pub decision: PaymentDecision,
    /// The payee's apps whose public endpoints couldn't be used in this call's read, in
    /// `PaymentMode::Public`; empty for a call that read no public endpoints.
    pub public_failures: Vec<PublicEndpointFailure>,
}
/// A payee app whose public endpoints couldn't be used in one read (Paykit spec 4): its
/// failure is its own, and the other apps' usable endpoints stay available.
#[derive(uniffi::Record)]
pub struct PublicEndpointFailure {
    /// The payee app.
    pub app_id: Vec<u8>,
    /// The reason this app's public endpoints were unavailable.
    pub kind: PublicFailureKind,
    /// Why its endpoints couldn't be used.
    pub reason: String,
}
/// Why a payee app's public endpoints couldn't be used (Paykit spec 4).
#[derive(uniffi::Enum)]
pub enum PublicFailureKind {
    /// The payee's Homeserver couldn't serve the app's record now.
    Homeserver,
    /// The reader refuses the record: it doesn't verify as the app's (Switch wire 3.2), its
    /// Paykit member doesn't read (Paykit wire 3.2), the app isn't current in the payee's
    /// `identity.json` with the record's certificate or that certificate doesn't allow
    /// `paykit_request`, the app's records conflict (Switch spec 4.2), or its newest records are
    /// more than the reader keeps of them.
    Refused,
    /// The record has expired: past its `expires_at`, or its `updated_at` plus the identity's
    /// `record_max_age`, or made ahead of the payer's clock (Paykit spec 4).
    Expired,
    /// The app's usable endpoints don't fit in what is left of `MAX_PUBLIC_ENDPOINTS` and
    /// `MAX_PUBLIC_PAYLOAD_BYTES` after the apps before it: none of them is taken.
    OverBound,
    /// A native case that this binding does not recognize.
    Other,
}
impl From<paykit_sdk::PayResult> for PaymentResult {
    fn from(r: paykit_sdk::PayResult) -> Self {
        use paykit_sdk::app_records::PublicAppFailureReason as F;
        Self {
            decision: match r.outcome {
                Ok(outcome) => PaymentDecision::Outcome {
                    outcome: outcome.into(),
                },
                Err(error) => PaymentDecision::Error {
                    error: error.into(),
                },
            },
            public_failures: r
                .public_failures
                .into_iter()
                .map(|f| PublicEndpointFailure {
                    app_id: f.app_id.into(),
                    reason: f.reason.to_string(),
                    kind: match f.reason {
                        F::Homeserver(_) => PublicFailureKind::Homeserver,
                        F::Refused(_) => PublicFailureKind::Refused,
                        F::Expired(_) => PublicFailureKind::Expired,
                        F::OverBound => PublicFailureKind::OverBound,
                        _ => PublicFailureKind::Other,
                    },
                })
                .collect(),
        }
    }
}

#[uniffi::export]
impl NativeWallet {
    /// Allow an explicitly chosen identity to start a conversation with this payer (Switch spec 7).
    pub async fn allow_connect(&self, payee: Vec<u8>) -> Result<(), WalletError> {
        let identity = fixed(payee, "payee identity")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.allow_connect(&identity).await?) })
        })
        .await
    }

    /// List one-time payment requests and their verified current status.
    pub async fn requests(&self) -> Result<Vec<OneTimeRequest>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                app.requests()
                    .await?
                    .into_iter()
                    .filter(|r| !r.recurring)
                    .map(|r| {
                        Ok(OneTimeRequest {
                            id: r.request.into(),
                            record: r.record.into(),
                            amount: (&r.amount).into(),
                            payment_reference: r.payment_reference,
                            label: r.label,
                            proposal_expires_at_ms: r.proposal_expires_at_ms,
                            held: r.held.map(|held| {
                                Arc::new(PaymentRefusal(Refusal::HeldAfterRestore {
                                    held: Box::new(held),
                                }))
                            }),
                            manual_only: r.manual_only,
                            status: r.status.into(),
                            divergent: r.divergent,
                            receipt: r.receipt.map(TryInto::try_into).transpose()?,
                        })
                    })
                    .collect()
            })
        })
        .await
    }
    /// Record the user's cancellation on the original request and return the signed message ID.
    /// A held outgoing cancellation stays with the SDK for delivery (Paykit spec 8.1).
    pub async fn cancel_request(
        &self,
        id: RequestId,
        reason: Option<String>,
    ) -> Result<String, WalletError> {
        let request = id.try_into()?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.cancel_request(&request, reason).await?) })
        })
        .await
    }
    /// Pay the original request by identity and hash under the supplied confirmations.
    pub async fn pay_request(
        &self,
        id: RequestId,
        options: PaymentOptions,
    ) -> Result<PaymentResult, WalletError> {
        let request = id.try_into()?;
        let options = options.try_into()?;
        let rail = self.rail();
        self.with_wallet(move |app| {
            Box::pin(async move {
                // Before anything is journaled, on Paykit's thread like every callback.
                if !rail.has_lookup() {
                    return Err(WalletError::PaymentMethodWithoutLookup);
                }
                Ok(app.pay_request(&request, options).await.into())
            })
        })
        .await
    }
}
