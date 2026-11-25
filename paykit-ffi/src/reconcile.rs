use crate::{NativeWallet, PaymentOutcome, ProviderLossBoundary, WalletError};
use paykit_lib::AttemptId;
use std::sync::Arc;

/// An original attempt and the obligation it pays, as the wallet reports it: among a doubtful
/// payment's attempts, or on a payment entry of the journal. `resolve_confirmed_attempt`
/// takes it back, so an attempt is never paired with another obligation (Paykit spec 7.3).
#[derive(uniffi::Object)]
pub struct DoubtfulAttempt {
    obligation: [u8; 32],
    id: AttemptId,
}

impl DoubtfulAttempt {
    pub(crate) fn new(obligation: [u8; 32], id: AttemptId) -> Arc<Self> {
        Arc::new(Self { obligation, id })
    }
}

#[uniffi::export]
impl DoubtfulAttempt {
    /// The attempt ID as text for display (Paykit wire 1.3).
    pub fn attempt_id(&self) -> String {
        self.id.to_text()
    }
}

/// A payment that may already have been made (Paykit spec 7.3).
#[derive(uniffi::Record)]
pub struct DoubtfulPayment {
    /// The 32-byte claim key of this obligation (Paykit wire 1.5).
    pub obligation: Vec<u8>,
    /// The unresolved attempts. A provider loss alone can leave this empty (Paykit spec 7.3).
    pub attempts: Vec<Arc<DoubtfulAttempt>>,
    /// The payee identity when the payer's records identify it (Paykit spec 7.3).
    pub payee: Option<Vec<u8>>,
    /// The authenticated request hash when available (Paykit spec 7.3).
    pub request_hash: Option<Vec<u8>>,
    /// The provider loss that still decides this payment, when known (Paykit spec 7.3).
    pub before_loss: Option<ProviderLossBoundary>,
}

impl From<paykit_sdk::Doubtful> for DoubtfulPayment {
    fn from(payment: paykit_sdk::Doubtful) -> Self {
        let obligation = payment.obligation;
        Self {
            obligation: obligation.into(),
            attempts: payment
                .attempts
                .into_iter()
                .map(|id| DoubtfulAttempt::new(obligation, id))
                .collect(),
            payee: payment.payee.map(Vec::from),
            request_hash: payment.request_hash.map(Vec::from),
            before_loss: payment
                .before_loss
                .map(|loss| ProviderLossBoundary::for_payment(obligation, loss)),
        }
    }
}

/// Results of the SDK's account-wide reconciliation on this wallet.
#[derive(uniffi::Record)]
pub struct WalletReconciliation {
    /// Outcomes decided by the wallet.
    pub resolved: Vec<PaymentOutcome>,
    /// Obligations that could not be decided.
    pub doubtful: Vec<DoubtfulPayment>,
    /// The payer's records were outside this wallet's usable scope or coverage.
    pub payer_skipped: bool,
}

/// The outcome the user confirmed for an unresolved original attempt (Paykit spec 7.3).
#[derive(uniffi::Enum)]
pub enum ConfirmedResolution {
    /// The attempt paid. Record it as paid (Paykit spec 7.3).
    Paid,
    /// The outcome is unknown. A later attempt can duplicate the transfer (Paykit spec 7.3).
    Unknown,
    /// The attempt definitively failed (Paykit spec 7.3).
    Failed,
}

#[uniffi::export]
impl NativeWallet {
    /// Reconcile the journal and claims against method history and shared records.
    /// Return the outcomes and payments still in doubt (Paykit spec 7.3).
    pub async fn reconcile(&self) -> Result<WalletReconciliation, WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move {
                let r = app.reconcile().await?;
                Ok(WalletReconciliation {
                    resolved: r.resolved.into_iter().map(Into::into).collect(),
                    doubtful: r.doubtful.into_iter().map(Into::into).collect(),
                    payer_skipped: r.payer_skipped,
                })
            })
        })
        .await
    }

    /// The user states that this one-time obligation was not paid before the loss they saw.
    /// Show that another wallet may already have paid it. A loss alone can leave zero attempts
    /// in the reconciliation report. If the loss changed, show the new report before asking again.
    pub async fn confirm_not_paid_before_loss(
        &self,
        payment: std::sync::Arc<crate::PaymentBeforeLoss>,
    ) -> Result<(), WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .confirm_not_paid_before_loss(
                        &payment.obligation,
                        &payment.loss,
                        paykit_sdk::UserConfirmed::new(),
                    )
                    .await?)
            })
        })
        .await
    }

    /// List the observed divergent records, including those whose held messages still await receive.
    pub async fn divergent_records(&self) -> Result<Vec<crate::Divergence>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .divergent_records()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            })
        })
        .await
    }

    /// The user permits this wallet to proceed over the exact record version and warning shown.
    /// Run receive until its held writes are drained, then retry the original action. If the
    /// record changed, show the new observation and ask again.
    pub async fn confirm_divergent(
        &self,
        observation: std::sync::Arc<crate::DivergentObservation>,
    ) -> Result<(), WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .confirm_divergent(&observation.0, paykit_sdk::UserConfirmed::new())
                    .await?)
            })
        })
        .await
    }

    /// Record the user's decision for this reported attempt after showing that it may
    /// already have paid. A paid decision also updates the payer's activity record.
    /// If the claim has lost the attempt, only an open restored handoff can still be
    /// resolved as paid or failed. Call it only with the user's answer (Paykit spec 7.2, 7.3).
    pub async fn resolve_confirmed_attempt(
        &self,
        attempt: Arc<DoubtfulAttempt>,
        resolution: ConfirmedResolution,
    ) -> Result<(), WalletError> {
        use paykit_sdk::Resolution;
        let resolution = match resolution {
            ConfirmedResolution::Paid => Resolution::Done,
            ConfirmedResolution::Unknown => Resolution::Unknown,
            ConfirmedResolution::Failed => Resolution::Failed,
        };
        // The call itself is the user's confirmed answer, as its name says.
        let consent = paykit_sdk::UserConfirmed::new();
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .resolve_attempt(&attempt.obligation, attempt.id, resolution, consent)
                    .await?)
            })
        })
        .await
    }

    /// End Paykit reconciliation after the user reviewed the restored wallet and confirmed.
    /// Other protocols complete their own reconciliation independently.
    pub async fn end_confirmed_reconciliation(&self) -> Result<(), WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.end_reconcile(paykit_sdk::UserConfirmed::new()).await?) })
        })
        .await
    }
}
