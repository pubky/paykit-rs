use crate::{fixed, Amount, NativeWallet, WalletError};
use paykit_sdk::{PayOutcome, RequestRef};
use std::sync::Arc;

/// A request, by its payee and its request hash (Paykit wire 1.5).
#[derive(Clone, uniffi::Record)]
pub struct RequestId {
    /// The payee identity.
    pub payee: Vec<u8>,
    /// The request hash.
    pub request_hash: Vec<u8>,
}
impl From<RequestRef> for RequestId {
    fn from(r: RequestRef) -> Self {
        Self {
            payee: r.payee.into(),
            request_hash: r.request_hash.into(),
        }
    }
}
impl TryFrom<RequestId> for RequestRef {
    type Error = WalletError;
    fn try_from(id: RequestId) -> Result<Self, Self::Error> {
        Ok(Self {
            payee: fixed(id.payee, "payee")?,
            request_hash: fixed(id.request_hash, "request hash")?,
        })
    }
}
/// A payee's receipt the payer holds in its activity record (Paykit wire 4.11, 5.3).
#[derive(uniffi::Record)]
pub struct Receipt {
    /// The record key of the payer's activity record that holds it.
    pub record: Vec<u8>,
    /// The payee.
    pub payee: Vec<u8>,
    /// The request paid, `None` for a payment without one.
    pub request_hash: Option<Vec<u8>>,
    /// The named Billing Period when this payment belongs to a recurring request.
    pub period: Option<BillingPeriod>,
    /// The attempt the receipt confirms.
    pub attempt_id: String,
    /// The original signed receipt's message ID.
    pub message_id: String,
    /// When the payer's inbox accepted it, service time; `None` when the record doesn't say.
    pub accepted_at_ms: Option<u64>,
    /// The Payment Reference: the request's for a receipt of a request, a Billing Period's
    /// included, whose request is read from the payer's request record; or, for a payment without
    /// one, the payee's reference the receipt carries, if any.
    pub payment_reference: Option<String>,
    /// Optional seller metadata encoded as a JSON object.
    pub metadata_json: Option<Vec<u8>>,
}
/// A named Billing Period, `{ "starts_at", "ends_at" }`, as a message carries it, before it
/// is checked against a recurrence (Paykit wire 4.4).
#[derive(Clone, uniffi::Record)]
pub struct BillingPeriod {
    /// `starts_at`, in Unix milliseconds.
    pub starts_at: u64,
    /// `ends_at`, in Unix milliseconds, after `starts_at`.
    pub ends_at: u64,
}
impl From<paykit_lib::periods::PeriodRef> for BillingPeriod {
    fn from(r: paykit_lib::periods::PeriodRef) -> Self {
        Self {
            starts_at: r.starts_at,
            ends_at: r.ends_at,
        }
    }
}
impl TryFrom<paykit_sdk::ReceivedReceipt> for Receipt {
    type Error = WalletError;
    fn try_from(r: paykit_sdk::ReceivedReceipt) -> Result<Self, Self::Error> {
        Ok(Self {
            record: r.record.into(),
            payee: r.payee.into(),
            request_hash: r.request_hash.map(Vec::from),
            period: r.billing_period.map(Into::into),
            attempt_id: r.attempt_id.to_text(),
            message_id: r.msg_id,
            accepted_at_ms: r.accepted_at_ms,
            payment_reference: r.payment_reference,
            metadata_json: r
                .metadata
                .map(|m| serde_json::to_vec(&m))
                .transpose()
                .map_err(|_| WalletError::invalid("receipt metadata"))?,
        })
    }
}

/// The result of a payment.
#[derive(uniffi::Enum)]
pub enum PaymentOutcome {
    /// Paid: `done` written, the proof queued.
    Paid {
        /// The attempt.
        attempt_id: String,
        /// The signed proof message ID, when its delivery has been recorded.
        proof_message_id: Option<String>,
    },
    /// Paid, but the outcome couldn't be written: the app isn't current any more.
    PaidUnreported {
        /// The attempt.
        attempt_id: String,
    },
    /// Definitively not paid: `failed` written.
    Failed {
        /// The attempt.
        attempt_id: String,
        /// Why, without content.
        reason: String,
    },
    /// Nothing yet: the attempt stays open until the payment method reports (Paykit spec 7.2,
    /// "Failed means definitively not paid").
    Pending {
        /// The attempt.
        attempt_id: String,
    },
    /// The payment method's history can't tell: "may be paid", resolved only by a receipt or by
    /// the user (Paykit spec 7.2, 7.3).
    Uncertain {
        /// The attempt.
        attempt_id: String,
    },
    /// Paid by the method, but the outcome's batch didn't go through (the switch didn't answer,
    /// or the write failed otherwise).
    PaidOutcomeUnwritten {
        /// The attempt.
        attempt_id: String,
    },
    /// Paid by the method.
    RecordHeld {
        /// The attempt.
        attempt_id: String,
    },
    /// Paid by the method, but the outcome waits: the payer's activity record is divergent after
    /// a provider loss and the user hasn't confirmed it, and an own write over it that adds no
    /// stop waits for that (Paykit spec 6).
    RecordDivergent {
        /// The attempt.
        attempt_id: String,
        /// The payer's activity record.
        record: Vec<u8>,
    },
    /// Over, with nothing of this call's to tell: the attempt was resolved by the user or by
    /// another app, or it finished with nothing of this call's to report. It is not a
    /// statement that no payment was made.
    ///
    /// These bindings leave it opaque: they expose no claim read and no typed resolution. A
    /// host can tell that nothing new is paid for it, whether the method gave a proof
    /// ([`PaymentEntry::paid_by_method`], while the wallet keeps the entry), and for a
    /// request, its status in `requests` and its receipts (`request_receipts`). It can't
    /// tell who resolved the attempt or how.
    Resolved {
        /// The attempt.
        attempt_id: String,
    },
    /// A native case that this binding does not recognize.
    Other {
        /// The SDK attempt ID associated with this outcome.
        attempt_id: String,
    },
}
impl From<PayOutcome> for PaymentOutcome {
    fn from(o: PayOutcome) -> Self {
        let attempt_id = o.attempt_id().to_text();
        match o {
            PayOutcome::Paid { proof_msg_id, .. } => Self::Paid {
                attempt_id,
                proof_message_id: proof_msg_id,
            },
            PayOutcome::PaidUnreported { .. } => Self::PaidUnreported { attempt_id },
            PayOutcome::Failed { reason, .. } => Self::Failed { attempt_id, reason },
            PayOutcome::Pending { .. } => Self::Pending { attempt_id },
            PayOutcome::Uncertain { .. } => Self::Uncertain { attempt_id },
            PayOutcome::PaidOutcomeUnwritten { .. } => Self::PaidOutcomeUnwritten { attempt_id },
            PayOutcome::RecordHeld { .. } => Self::RecordHeld { attempt_id },
            PayOutcome::RecordDivergent { record, .. } => Self::RecordDivergent {
                attempt_id,
                record: record.into(),
            },
            PayOutcome::Resolved { .. } => Self::Resolved { attempt_id },
            _ => Self::Other { attempt_id },
        }
    }
}
/// Where a payment stands in the paying order (Paykit spec 7.2).
#[derive(uniffi::Enum)]
pub enum PaymentStage {
    /// Recorded before the attempt write.
    Recorded,
    /// The attempt is written and its proof checked; no money moved.
    Attempted,
    /// Recorded just before the payment method's call: from here on it may be paid.
    Paying,
    /// The method reports it paid.
    RailPaid,
    /// The method reports a definitive failure.
    RailFailed,
    /// The outcome, the activity record and the proof were written in one batch, but the
    /// acceptance the payment owes isn't in the record yet: not queued, or queued
    /// (`acceptance_released`) and not yet placed.
    AcceptanceOwed,
    /// Finished: the outcome, the activity record and, for a payment, the proof were written in
    /// one batch; or the failure was written before money could move.
    Finished,
    /// Given up before any money could move: the attempt wasn't written.
    Abandoned,
    /// A native case that this binding does not recognize.
    Other,
}
impl From<paykit_sdk::Stage> for PaymentStage {
    fn from(s: paykit_sdk::Stage) -> Self {
        use paykit_sdk::Stage as S;
        match s {
            S::Recorded => Self::Recorded,
            S::Attempted => Self::Attempted,
            S::Paying => Self::Paying,
            S::RailPaid => Self::RailPaid,
            S::RailFailed => Self::RailFailed,
            S::AcceptanceOwed => Self::AcceptanceOwed,
            S::Finished => Self::Finished,
            S::Abandoned => Self::Abandoned,
            _ => Self::Other,
        }
    }
}
/// A payment in the journal, as the host sees it.
#[derive(uniffi::Record)]
pub struct PaymentEntry {
    /// The attempt ID, the payment method's idempotency key.
    pub attempt_id: String,
    /// The attempt as a handle with its obligation, for `resolve_confirmed_attempt` once the
    /// user has answered it, even when the claim no longer lists it, as after a restore or a
    /// provider loss (Paykit spec 7.2, 7.3).
    pub attempt: Arc<crate::DoubtfulAttempt>,
    /// The claim key of the obligation paid.
    pub obligation: Vec<u8>,
    /// The payee.
    pub payee: Vec<u8>,
    /// The request paid, `None` for a payment without one.
    pub request_hash: Option<Vec<u8>>,
    /// The named Billing Period when this payment belongs to a recurring request.
    pub period: Option<BillingPeriod>,
    /// The amount.
    pub amount: Amount,
    /// The selected Payment Endpoint Identifier.
    pub endpoint: String,
    /// Where it stands.
    pub stage: PaymentStage,
    /// The entry came back from a backup.
    pub restored: bool,
    /// Whether the payment method reported this payment made: the journal holds its proof.
    /// At `Finished` it tells a payment made, its outcome written, from one written failed.
    /// After a call returned a wallet-wide stop, it is how the host learns of a payment made
    /// in that call, as the call's error carries no outcome (Paykit spec 7.2).
    pub paid_by_method: bool,
    /// Paid by the method, its outcome waits: the payer's activity record holds a value that
    /// doesn't read, so `done` and the proof aren't written, and the attempt stays open (Paykit
    /// spec 7.2 step 3).
    pub record_held: bool,
    /// The exact transferred amount bound before dispatch, excluding fees. `None` when the method
    /// could not quantify the selected transfer or the entry was rebuilt only to report an
    /// outcome.
    pub paid: Option<Amount>,
    /// The rate used for a payment priced by a rate; `None` for another payment.
    pub rate: Option<Amount>,
    /// The payee's receipt of this attempt the payer's activity record holds, if any (Paykit wire
    /// 4.11), as this app's copy shows it: for display.
    pub receipt: Option<Receipt>,
}
#[uniffi::export]
impl NativeWallet {
    /// Read attempts and receipts from the wallet's durable payment journal.
    pub async fn payments(&self) -> Result<Vec<PaymentEntry>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                app.payments()
                    .await?
                    .into_iter()
                    .map(|p| {
                        Ok(PaymentEntry {
                            attempt_id: p.attempt_id.to_text(),
                            attempt: crate::DoubtfulAttempt::new(p.obligation, p.attempt_id),
                            obligation: p.obligation.into(),
                            payee: p.payee.into(),
                            request_hash: p.request_hash.map(Vec::from),
                            period: p.billing_period.map(Into::into),
                            amount: (&p.amount).into(),
                            endpoint: p.endpoint_id.as_str().into(),
                            stage: p.stage.into(),
                            restored: p.restored,
                            paid_by_method: p.paid_by_method,
                            record_held: p.record_held,
                            paid: p.paid.as_ref().map(Into::into),
                            rate: p.rate.map(|r| Amount {
                                asset: r.asset().as_str().into(),
                                value: r.value().as_str().into(),
                            }),
                            receipt: p.receipt.map(TryInto::try_into).transpose()?,
                        })
                    })
                    .collect()
            })
        })
        .await
    }
    /// Resolve original attempts according to their stored stage and method evidence.
    pub async fn resume_payments(&self) -> Result<Vec<PaymentOutcome>, WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .resume_payments()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect())
            })
        })
        .await
    }
    /// Read the seller's signed receipts for the original one-time request.
    pub async fn request_receipts(&self, id: RequestId) -> Result<Vec<Receipt>, WalletError> {
        let request = id.try_into()?;
        self.with_wallet(move |app| {
            Box::pin(async move {
                app.receipts(paykit_sdk::ReceiptsOf::Request(request))
                    .await?
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect()
            })
        })
        .await
    }
}

/// The authenticated usage of the approving wallet's retained Allowance ledger.
/// Monetary values exclude fees and use the term's asset (Paykit spec 8.3).
#[derive(uniffi::Record)]
pub struct AllowanceUsage {
    /// Lifetime amount, including outstanding reservations.
    pub lifetime: String,
    /// Usage in the same order as the original signed period limits.
    pub periods: Vec<AllowancePeriodUsage>,
    /// Outstanding reserved entries.
    pub reserved: u64,
    /// Entries counted as spent.
    pub spent: u64,
    /// Entries that have been released.
    pub released: u64,
}

/// Usage under one period limit from the original signed terms.
#[derive(uniffi::Record)]
pub struct AllowancePeriodUsage {
    /// The exact amount counted.
    pub amount: String,
    /// The number of payments counted.
    pub count: u64,
}

#[uniffi::export]
impl NativeWallet {
    /// Reconcile an existing grant against the original wallet's current ledger and
    /// complete account-wide method history. Null leaves usage unknown and spending held.
    pub async fn reconcile_allowance(
        &self,
        record: Vec<u8>,
    ) -> Result<Option<AllowanceUsage>, WalletError> {
        let record = fixed(record, "Allowance record")?;
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .reconcile_allowance(&record)
                    .await?
                    .map(|usage| AllowanceUsage {
                        lifetime: usage.lifetime().as_str().into(),
                        periods: usage
                            .periods()
                            .iter()
                            .map(|period| AllowancePeriodUsage {
                                amount: period.amount().as_str().into(),
                                count: period.count(),
                            })
                            .collect(),
                        reserved: usage.reserved(),
                        spent: usage.spent(),
                        released: usage.released(),
                    }))
            })
        })
        .await
    }
}
