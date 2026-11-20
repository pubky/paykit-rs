use crate::{NativeWallet, PaymentOutcome, ProviderLossBoundary, WalletError};

/// A payment whose protected reconstruction still needs host attention.
#[derive(uniffi::Record)]
pub struct DoubtfulPayment {
    /// The stable payment obligation.
    pub obligation: Vec<u8>,
    /// Every original attempt implicated by the current evidence.
    pub attempts: Vec<String>,
    /// The payee identity when available.
    pub payee: Option<Vec<u8>>,
    /// The authenticated request hash when available.
    pub request_hash: Option<Vec<u8>>,
    /// The provider loss that still decides this payment, when known.
    pub before_loss: Option<ProviderLossBoundary>,
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
    /// The attempt paid. Settle its original claim through reconciliation.
    Paid,
    /// The outcome is still unknown. A subsequent attempt can duplicate the transfer.
    Unknown,
    /// The payment definitively failed.
    Failed,
}

#[uniffi::export(async_runtime = "tokio")]
impl NativeWallet {
    /// Compare the durable journal, method history and authenticated shared records.
    pub async fn reconcile(&self) -> Result<WalletReconciliation, WalletError> {
        self.with_wallet(async |app| {
            let r = app.reconcile().await?;
            Ok(WalletReconciliation {
                resolved: r.resolved.into_iter().map(Into::into).collect(),
                doubtful: r
                    .doubtful
                    .into_iter()
                    .map(|d| DoubtfulPayment {
                        obligation: d.obligation.into(),
                        attempts: d.attempts.iter().map(|i| i.to_text()).collect(),
                        payee: d.payee.map(Vec::from),
                        request_hash: d.request_hash.map(Vec::from),
                        before_loss: d.before_loss.map(Into::into),
                    })
                    .collect(),
                payer_skipped: r.payer_skipped,
            })
        })
        .await
    }

    /// Apply the user's explicit decision to the original obligation and attempt in the report.
    pub async fn resolve_confirmed_attempt(
        &self,
        obligation: Vec<u8>,
        attempt_id: String,
        resolution: ConfirmedResolution,
    ) -> Result<(), WalletError> {
        let obligation = crate::fixed(obligation, "obligation")?;
        let attempt = paykit_lib::AttemptId::parse(&attempt_id)
            .map_err(|_| WalletError::invalid("attempt ID"))?;
        let resolution = match resolution {
            ConfirmedResolution::Paid => paykit_sdk::Resolution::Done,
            ConfirmedResolution::Unknown => paykit_sdk::Resolution::Unknown,
            ConfirmedResolution::Failed => paykit_sdk::Resolution::Failed,
        };
        self.with_wallet(async |app| {
            Ok(app
                .resolve_attempt(
                    &obligation,
                    attempt,
                    resolution,
                    paykit_sdk::UserConfirmed::new(),
                )
                .await?)
        })
        .await
    }

    /// End Paykit reconciliation after the user reviewed the restored wallet and confirmed.
    /// Other protocols complete their own reconciliation independently.
    pub async fn end_confirmed_reconciliation(&self) -> Result<(), WalletError> {
        self.with_wallet(async |app| {
            Ok(app.end_reconcile(paykit_sdk::UserConfirmed::new()).await?)
        })
        .await
    }
}
