//! Payment Receipts (Paykit spec 5, Paykit wire 4.11): what a payee's receipt carries beyond
//! the proof it confirms ([`ReceiptDetails`]), and the receipts the payer holds in its
//! activity records ([`ReceivedReceipt`]), listed by counterparty, request or Billing Period
//! ([`Paykit::receipts`]).

use paykit_lib::bodies::{PaymentReceipt, PaymentRequest};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{ActivityRecord, ActivitySide, RecordBinding, StoredMessage};
use paykit_lib::{AttemptId, Body, Label, PaymentReference};
use pubky_dispatch_client::app::UserConfirmed;
use serde_json::{Map, Value};

use crate::activity::ActivityTarget;
use crate::error::Result;
use crate::paykit::Paykit;
use crate::requests::RequestRef;

/// A signed pair that disagrees with its signed request is invalid. If the
/// request is unavailable, its Billing Period cannot yet be decided.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiptBinding {
    Verified,
    MissingRequest,
    Invalid,
}

pub(crate) fn receipt_binding(
    record: &ActivityRecord,
    parent: Option<&ActivityRecord>,
) -> ReceiptBinding {
    if record.proof.is_none() || record.receipt.is_none() {
        return ReceiptBinding::Invalid;
    }
    let Ok(binding) = record.binding() else {
        return ReceiptBinding::Invalid;
    };
    let parent = match binding {
        RecordBinding::Unrequested => return ReceiptBinding::Verified,
        RecordBinding::Request { .. } => record,
        RecordBinding::Period { .. } => match parent {
            Some(p) if crate::requests::request_of(p).is_some() => p,
            _ => return ReceiptBinding::MissingRequest,
        },
    };
    if crate::periods::request_for_payment(parent, record, &binding)
        .and_then(|(_, request)| displayed_receipt(record, Some(request)))
        .is_some()
    {
        ReceiptBinding::Verified
    } else {
        ReceiptBinding::Invalid
    }
}

/// What a payee's receipt carries beyond the proof it confirms (Paykit wire 4.11): its
/// opaque `metadata`, and, for a payment without a request only, the payee's
/// `payment_reference` for it, as in v0.1. Its `Debug` shows neither.
#[derive(Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceiptDetails {
    /// Store the signed receipt in shared activity without sending it to the payer.
    pub store_only: bool,
    /// The user has seen that the shared settlement mark cannot be written and a
    /// different occurrence may therefore be credited again (Paykit wire 5.7).
    pub unmarked_credit_confirmed: Option<UserConfirmed>,
    /// The payee's opaque metadata.
    pub metadata: Option<Map<String, Value>>,
    /// The payee's reference for a payment without a request; a receipt of a request
    /// carries none, since the request's applies.
    pub payment_reference: Option<PaymentReference>,
    /// The user has seen that this payment may be credited already to another occurrence,
    /// a record of the payer's the app can't open or a copy that may lack records hiding
    /// it, and credits it anyway ([`crate::Refusal::MaybeCredited`]). It never overrides a
    /// credit the app can see.
    pub maybe_credited_confirmed: Option<UserConfirmed>,
}

impl std::fmt::Debug for ReceiptDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiptDetails")
            .field("store_only", &self.store_only)
            .field("unmarked_credit_confirmed", &self.unmarked_credit_confirmed)
            .field("metadata", &self.metadata.as_ref().map(Map::len))
            .field("payment_reference", &self.payment_reference.is_some())
            .field("maybe_credited_confirmed", &self.maybe_credited_confirmed)
            .finish()
    }
}

impl ReceiptDetails {
    /// A receipt with nothing beyond the proof it confirms.
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep the signed receipt in the payee's activity without queuing it for delivery.
    #[must_use]
    pub fn store_only(mut self) -> Self {
        self.store_only = true;
        self
    }

    /// Permit receipt-only credit after showing the risk of a second credit when
    /// a full bucket, the value limit or the storage quota prevents a mark.
    #[must_use]
    pub fn with_unmarked_credit_confirmed(mut self, confirmed: UserConfirmed) -> Self {
        self.unmarked_credit_confirmed = Some(confirmed);
        self
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

    /// The user has seen that this payment may be credited already to another occurrence,
    /// and credits it anyway ([`crate::Refusal::MaybeCredited`]): a record that never opens
    /// then can't hold the credit for good. A credit the app can see still refuses it.
    #[must_use]
    pub fn with_maybe_credited_confirmed(mut self, confirmed: UserConfirmed) -> Self {
        self.maybe_credited_confirmed = Some(confirmed);
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
    /// The Payment Reference: the request's for a receipt of a request, a Billing Period's
    /// included, whose request is read from the payer's request record; or, for a payment
    /// without one, the payee's reference the receipt carries, if any.
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Those with this Payment Reference: their request's, or the payee's for a payment
    /// without a request, as v0.1 indexed receipts by it.
    Reference(PaymentReference),
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
            Self::Reference(reference) => {
                receipt.payment_reference.as_deref() == Some(reference.as_str())
            }
        }
    }
}

/// The request record of the request a period record names (Paykit wire 5.3): a period
/// record holds no request, so its request's members are read there. On the payer's side it
/// is the record keyed by the payee, the record's counterparty, and the `request_hash`; on
/// the payee's, the record keyed by `own` and the `request_hash`, under the period record's
/// `label`. `None` for a record that holds its request itself, or names none.
pub(crate) fn period_request_target(
    record: &ActivityRecord,
    own: &[u8; 32],
    label: Option<&Label>,
) -> Option<ActivityTarget> {
    let RecordBinding::Period {
        request_hash: hash, ..
    } = record.binding().ok()?
    else {
        return None;
    };
    Some(match record.side {
        ActivitySide::Payer => {
            ActivityTarget::payer(natural_key::request_record(&record.counterparty, &hash))
        }
        ActivitySide::Payee => {
            ActivityTarget::payee(natural_key::request_record(own, &hash), label)
        }
    })
}

/// The receipt for a bound activity (Paykit spec 3.4, Paykit wire 4.11 and 5.3).
/// A period keeps its signed request in the parent. Check its selling scope there
/// before using the receipt. A payee also needs the proof to credit the payment.
pub(crate) fn displayed_receipt<'a>(
    record: &'a ActivityRecord,
    period_request: Option<&PaymentRequest>,
) -> Option<&'a StoredMessage> {
    let receipt = record.receipt.as_ref()?;
    if record.side == ActivitySide::Payee && !record.shows_paid() {
        return None;
    }
    let binding = record.binding().ok()?;
    let request = match binding {
        RecordBinding::Unrequested => return Some(receipt),
        RecordBinding::Request { .. } => {
            crate::periods::request_for_payment(record, record, &binding)?.1
        }
        RecordBinding::Period { .. } => period_request?,
    };
    receipt.check_payee_sender(request).ok()?;
    Some(receipt)
}

/// The receipt `record`, held at `key`, stores, as the payer sees it. `request` is the
/// request it pays: the one `record` holds, or, for a period record, the one its request
/// record holds ([`period_request_target`]). A receipt from an app the request's label
/// doesn't let send it shows nothing (Paykit spec 3.4, Paykit wire 5.3).
pub(crate) fn received_receipt(
    key: [u8; 32],
    record: &ActivityRecord,
    request: Option<&PaymentRequest>,
) -> Option<ReceivedReceipt> {
    let stored = displayed_receipt(record, request)?;
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
        payee: stored.sender(),
        request_hash: stored.message().refs.get(1).copied(),
        billing_period: *billing_period,
        attempt_id: *attempt_id,
        msg_id: stored.message().msg_id.clone(),
        accepted_at_ms: stored.accepted_at(),
        metadata: metadata.clone(),
        payment_reference: payment_reference
            .as_ref()
            .or_else(|| request.map(|request| &request.payment_reference))
            .map(|reference| reference.as_str().to_owned()),
    })
}

impl Paykit {
    /// The receipt `record`, held at `key`, stores, with the request it pays read from the
    /// record, or for a period record from its request record (Paykit wire 5.3).
    ///
    /// # Errors
    ///
    /// A store failure.
    pub(crate) async fn received_receipt_of(
        &self,
        key: [u8; 32],
        record: &ActivityRecord,
    ) -> Result<Option<ReceivedReceipt>> {
        if record.receipt.is_none() {
            return Ok(None);
        }
        let held = match period_request_target(record, &self.identity(), None) {
            Some(target) => self.held_activity(&target).await?,
            None => None,
        };
        let binding = record.binding()?;
        let parent = held.as_ref().unwrap_or(record);
        let request = match binding {
            RecordBinding::Unrequested => None,
            _ => {
                let Some((_, request)) =
                    crate::periods::request_for_payment(parent, record, &binding)
                else {
                    return Ok(None);
                };
                Some(request)
            }
        };
        Ok(received_receipt(key, record, request))
    }

    /// The payees' receipts the payer's records hold, `of` the payee, request or Billing
    /// Period asked for (Paykit wire 4.11, 5.3): one per record, the receipt of the proof it
    /// holds. A record that doesn't bind is left out (Paykit wire 5.3).
    ///
    /// # Errors
    ///
    /// Not now while the shared records aren't listed since a restore; a store failure.
    pub async fn receipts(&self, of: ReceiptsOf) -> Result<Vec<ReceivedReceipt>> {
        self.refuse_while_unlisted().await?;
        let mut out = Vec::new();
        for held in self
            .app
            .records_of(Collection::PayerActivity.name())
            .await?
        {
            let Some(record) = self.bind_held(&held, ActivitySide::Payer, None) else {
                continue;
            };
            if let Some(receipt) = self
                .received_receipt_of(held.key, &record)
                .await?
                .filter(|r| of.takes(r))
            {
                out.push(receipt);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use paykit_lib::bodies::PaymentReceipt;
    use paykit_lib::derive::natural_key;
    use paykit_lib::periods::PeriodRef;
    use paykit_lib::records::{ActivityRecord, ActivitySide};
    use paykit_lib::{AttemptId, Body};
    use serde_json::json;

    use super::{period_request_target, received_receipt};
    use crate::activity::ActivityTarget;
    use crate::requests::request_of;
    use crate::testkit::{object, Party};

    /// Bob's request to alice, the payer's request record holding it, and a period record
    /// of it holding bob's receipt of one Billing Period, as another own app wrote them.
    fn period_records() -> (ActivityRecord, ActivityRecord, PeriodRef) {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let request = bob.message(
            &alice,
            "paykit.payment_request",
            vec![],
            object(json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "invoice-7",
                "proposal_expires_at": null
            })),
        );
        let mut request_record = ActivityRecord::new(ActivitySide::Payer, bob.id());
        request_record.request = Some(request.clone());
        let period = PeriodRef {
            starts_at: 1_000,
            ends_at: 2_000,
        };
        let receipt = bob.message(
            &alice,
            "paykit.payment_receipt",
            vec![[7; 32], request.hash()],
            PaymentReceipt {
                attempt_id: AttemptId::from_bytes([3; 16]),
                billing_period: Some(period),
                metadata: None,
                payment_reference: None,
            }
            .to_body(2)
            .unwrap(),
        );
        let mut period_record = ActivityRecord::new(ActivitySide::Payer, bob.id());
        period_record.request_hash = Some(request.hash());
        period_record.billing_period = Some(period);
        period_record.receipt = Some(receipt);
        (request_record, period_record, period)
    }

    #[test]
    fn test_receipts_of_a_period_carry_the_requests_reference() {
        let (request_record, period_record, period) = period_records();
        let hash = period_record.request_hash.unwrap();
        // The period record holds no request: its request is read from the request record.
        assert!(
            period_request_target(&period_record, &[1; 32], None)
                == Some(ActivityTarget::payer(natural_key::request_record(
                    &period_record.counterparty,
                    &hash
                )))
        );
        let request = request_of(&request_record).map(|(_, body)| body);
        let receipt = received_receipt([9; 32], &period_record, request).unwrap();
        assert_eq!(receipt.payment_reference.as_deref(), Some("invoice-7"));
        assert_eq!(receipt.billing_period, Some(period));
        assert_eq!(receipt.request_hash, Some(hash));
        assert!(matches!(
            period_record.receipt.as_ref().unwrap().body(),
            Body::PaymentReceipt(_)
        ));
    }

    #[test]
    fn test_receipts_of_a_request_record_name_no_other_record() {
        let (request_record, _, _) = period_records();
        assert!(period_request_target(&request_record, &[1; 32], None).is_none());
    }

    #[test]
    fn test_receipts_payee_period_record_names_the_payees_request_record() {
        let (_, mut period_record, _) = period_records();
        period_record.side = ActivitySide::Payee;
        let own = [1; 32];
        let hash = period_record.request_hash.unwrap();
        assert!(
            period_request_target(&period_record, &own, None)
                == Some(ActivityTarget::payee(
                    natural_key::request_record(&own, &hash),
                    None
                ))
        );
    }
}
