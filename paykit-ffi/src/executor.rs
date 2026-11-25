//! Payments under an Allowance for Kotlin and Swift: the executor, reservations and the ledger (Paykit spec 8.3).

use crate::{
    fixed, AllowanceHold, DispatchAlarm, NativeWallet, PaymentResult, RequestId, WalletError,
};
use paykit_sdk::UserConfirmed;
use std::sync::Arc;

fn confirmed(user_confirmed: bool) -> Result<UserConfirmed, WalletError> {
    user_confirmed
        .then(UserConfirmed::new)
        .ok_or_else(|| WalletError::invalid("user_confirmed"))
}

#[uniffi::export]
impl NativeWallet {
    /// Pay an eligible one-time request under this app's accepted Allowance. The current
    /// grant, ledger, terms and complete payment-method history must authorize it. A missing
    /// payment lookup fails before journaling. The result carries the payment outcome or
    /// refusal. The opening claim, reservation and request association share one batch
    /// (Paykit spec 8.3).
    pub async fn pay_under_allowance(
        &self,
        id: RequestId,
        allowance: Vec<u8>,
    ) -> Result<PaymentResult, WalletError> {
        let request = id.try_into()?;
        let allowance = fixed(allowance, "allowance")?;
        let rail = self.rail();
        self.with_wallet(move |app| {
            Box::pin(async move {
                if !rail.has_lookup() {
                    return Err(WalletError::PaymentMethodWithoutLookup);
                }
                Ok(app.pay_under_allowance(&request, &allowance).await.into())
            })
        })
        .await
    }

    /// Complete this app's previously authorized first grant and record consent once it
    /// applies. Retry after a lost reply, including after reopening. A repeated call checks
    /// the current grant and ledger. A later consent hold requires confirmation. After a
    /// provider loss see the host guide, "Request and Allowance status" (Paykit spec 8.3).
    pub async fn resume_allowance_executor(&self, record: Vec<u8>) -> Result<(), WalletError> {
        let record = fixed(record, "record")?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.resume_allowance_executor(&record).await?) })
        })
        .await
    }

    /// Renew this app's previously approved execution after the user confirms the displayed
    /// counterparty, terms and executor under this exact hold. A newer hold requires fresh
    /// confirmation. If the ledger cannot yet be verified, retry with the same confirmed hold.
    /// After a provider loss the grant is written again but spending stays held; see the host
    /// guide, "Request and Allowance status" (Paykit spec 8.3).
    pub async fn confirm_allowance_executor(
        &self,
        hold: Arc<AllowanceHold>,
        user_confirmed: bool,
    ) -> Result<(), WalletError> {
        let consent = confirmed(user_confirmed)?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.confirm_allowance_executor(&hold.0, consent).await?) })
        })
        .await
    }

    /// Return the Dispatch alarm that stopped this wallet after an Allowance ledger rollback.
    /// It survives reopening. Show its reason until the signer resolves the provider move
    /// or loss (Paykit spec 8.3, Dispatch spec 10.1).
    pub async fn ledger_alarm(&self) -> Result<Option<DispatchAlarm>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                // Local inspection reloads a failed commit without handing duties to the host.
                app.app_mut().pending_duty_events().await?;
                Ok(app.ledger_alarm().map(Into::into))
            })
        })
        .await
    }
}
