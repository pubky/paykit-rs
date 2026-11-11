//! Payment Receipts (Paykit spec 5, Paykit wire 4.11): what a payee's receipt carries beyond
//! the proof it confirms ([`ReceiptDetails`]), and the receipts the payer holds in its
//! activity records ([`ReceivedReceipt`]), listed by counterparty, request or Billing Period
//! ([`Paykit::receipts`]).

use paykit_lib::bodies::PaymentReceipt;
use paykit_lib::collections::Collection;
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{ActivityRecord, ActivitySide};
use paykit_lib::{AttemptId, Body, PaymentReference};
use serde_json::{Map, Value};

use crate::error::Result;
use crate::paykit::Paykit;
use crate::requests::RequestRef;

/// What a payee's receipt carries beyond the proof it confirms (Paykit wire 4.11): its
/// opaque `metadata`, and, for a payment without a request only, the payee's
/// `payment_reference` for it, as in v0.1. Its `Debug` shows neither.
#[derive(Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceiptDetails {
    /// The payee's opaque metadata.
    pub metadata: Option<Map<String, Value>>,
    /// The payee's reference for a payment without a request; a receipt of a request
    /// carries none, since the request's applies.
    pub payment_reference: Option<PaymentReference>,
}

impl std::fmt::Debug for ReceiptDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiptDetails")
            .field("metadata", &self.metadata.as_ref().map(Map::len))
            .field("payment_reference", &self.payment_reference.is_some())
            .finish()
    }
}

impl ReceiptDetails {
    /// A receipt with nothing beyond the proof it confirms.
    pub fn new() -> Self {
        Self::default()
    }

    /// The payee's opaque metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Map<String, Value>) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// The payee's reference for a payment without a request.
    #[must_use]
    pub fn with_payment_reference(mut self, payment_reference: PaymentReference) -> Self {
        self.payment_reference = Some(payment_reference);
        self
    }
}

/// A payee's receipt the payer holds in its activity record (Paykit wire 4.11, 5.3). Its
/// `Debug` shows neither the metadata nor the payment reference.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceivedReceipt {
    /// The record key of the payer's activity record that holds it.
    pub record: [u8; 32],
    /// The payee.
    pub payee: [u8; 32],
    /// The request paid, `None` for a payment without one.
    pub request_hash: Option<[u8; 32]>,
    /// The Billing Period paid, for a recurring request.
    pub billing_period: Option<PeriodRef>,
    /// The attempt the receipt confirms.
    pub attempt_id: AttemptId,
    /// The receipt's `msg_id`.
    pub msg_id: String,
    /// When the payer's inbox accepted it, service time; `None` when the record doesn't
    /// say.
    pub accepted_at_ms: Option<u64>,
    /// The payee's opaque metadata.
    pub metadata: Option<Map<String, Value>>,
    /// The payee's reference, for a payment without a request.
    pub payment_reference: Option<String>,
}

impl std::fmt::Debug for ReceivedReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceivedReceipt")
            .field("record", &self.record)
            .field("request_hash", &self.request_hash)
            .field("billing_period", &self.billing_period)
            .field("attempt_id", &self.attempt_id)
            .field("msg_id", &self.msg_id)
            .field("metadata", &self.metadata.as_ref().map(Map::len))
            .field("payment_reference", &self.payment_reference.is_some())
            .finish()
    }
}

/// Which receipts [`Paykit::receipts`] lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiptsOf {
    /// Every receipt.
    All,
    /// Those from this payee.
    Payee([u8; 32]),
    /// Those of this request, any of its Billing Periods included.
    Request(RequestRef),
    /// Those of this Billing Period of this request.
    Period(RequestRef, PeriodRef),
}

impl ReceiptsOf {
    fn takes(&self, receipt: &ReceivedReceipt) -> bool {
        let request = |r: &RequestRef| {
            receipt.payee == r.payee && receipt.request_hash == Some(r.request_hash)
        };
        match self {
            Self::All => true,
            Self::Payee(payee) => receipt.payee == *payee,
            Self::Request(r) => request(r),
            Self::Period(r, period) => request(r) && receipt.billing_period == Some(*period),
        }
    }
}

/// The receipt `record`, held at `key`, stores, as the payer sees it.
pub(crate) fn received_receipt(key: [u8; 32], record: &ActivityRecord) -> Option<ReceivedReceipt> {
    let stored = record.receipt.as_ref()?;
    let Body::PaymentReceipt(PaymentReceipt {
        attempt_id,
        billing_period,
        metadata,
        payment_reference,
    }) = stored.body()
    else {
        return None;
    };
    Some(ReceivedReceipt {
        record: key,
        payee: record.counterparty,
        request_hash: stored.message().refs.get(1).copied(),
        billing_period: *billing_period,
        attempt_id: *attempt_id,
        msg_id: stored.message().msg_id.clone(),
        accepted_at_ms: stored.accepted_at(),
        metadata: metadata.clone(),
        payment_reference: payment_reference
            .as_ref()
            .map(|reference| reference.as_str().to_owned()),
    })
}

impl Paykit {
    /// The payees' receipts the payer's records hold, `of` the payee, request or Billing
    /// Period asked for (Paykit wire 4.11, 5.3): one per record, the receipt of the proof it
    /// holds. A record that doesn't bind is left out (Paykit wire 5.3).
    ///
    /// # Errors
    ///
    /// Not now while the shared records aren't listed since a restore; a store failure.
    pub async fn receipts(&self, of: ReceiptsOf) -> Result<Vec<ReceivedReceipt>> {
        self.refuse_while_unlisted().await?;
        let own = self.identity();
        let mut out = Vec::new();
        for held in self
            .app
            .records_of(Collection::PayerActivity.name())
            .await?
        {
            let Some(value) = held.content.value() else {
                continue;
            };
            let Some(record) = crate::records::bind_activity(
                value,
                &own,
                ActivitySide::Payer,
                None,
                &held.key,
                |t| self.app.record_key(t).ok(),
                &|p| self.rail.settlement_id(p),
            ) else {
                continue;
            };
            if let Some(receipt) = received_receipt(held.key, &record).filter(|r| of.takes(r)) {
                out.push(receipt);
            }
        }
        Ok(out)
    }
}
