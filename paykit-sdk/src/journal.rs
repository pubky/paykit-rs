//! The payment journal (Paykit spec 2, 7.2): one entry per payment, recorded before its
//! `attempt` is written, and moved through the paying order one committed stage at a time,
//! so that a crash at any point is resumed without a second payment.
//!
//! Crash rules (Paykit spec 7.2):
//!
//! - **Before paying** (`Recorded`, `Attempted`): no money moved. An own attempt still open is
//!   written `failed`.
//! - **Paying**: the payment may have been submitted. It is resolved from the payment
//!   method's history, never carried out again unless the method is idempotent on the
//!   attempt ID, the claim still shows the attempt open, and every check before a first
//!   dispatch passes again; otherwise it stays open, "may be paid".
//! - **After paying** (`RailPaid`, `RailFailed`): the outcome, the activity record and the
//!   proof go in one batch, which also moves the entry to `Finished` once applied.
//! - **A restored journal** may predate a payment made after its backup, so its open attempts
//!   are resolved from a history that survives a restore, or left "may be paid" (Pubky Switch
//!   spec, Section 12.4).
//!
//! Sensitive: an entry holds the destination's payload, the message carrying it and the
//! rail's proof.

use paykit_lib::records::DestinationSource;
use paykit_lib::{
    Asset, AttemptId, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
};
use pubky_switch_core::enc::{serde_b64, serde_b64_vec};
use pubky_switch_core::records::AppRecord;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::destination::Carrier;
use crate::error::{PaykitError, Result};
use crate::records::KeptMessage;

/// An optional byte string as base64url, or null.
mod serde_b64_opt {
    use pubky_switch_core::enc::{b64, unb64};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(bytes) => serializer.serialize_some(&b64(bytes)),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| unb64(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// Where a payment stands in the paying order (Paykit spec 7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
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
    /// Finished: the outcome, the activity record and, for a payment, the proof were written
    /// in one batch; or the failure was written before money could move.
    Finished,
    /// Given up before any money could move: the attempt wasn't written.
    Abandoned,
}

/// Where a payment goes, and where the destination came from (Paykit spec 7.2, Paykit wire
/// 5.3). Its `Debug` shows no payload.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Destination {
    #[serde(with = "serde_b64")]
    pub(crate) app_id: [u8; 16],
    pub(crate) endpoint_id: String,
    pub(crate) payload: String,
    pub(crate) source: String,
    pub(crate) list_version: Option<u64>,
    pub(crate) single_use: bool,
}

impl std::fmt::Debug for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Destination({}, {})", self.endpoint_id, self.source)
    }
}

impl Destination {
    pub(crate) fn new(
        app_id: [u8; 16],
        endpoint_id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        source: DestinationSource,
        list_version: Option<u64>,
        single_use: bool,
    ) -> Self {
        Self {
            app_id,
            endpoint_id: endpoint_id.as_str().to_owned(),
            payload: payload.as_str().to_owned(),
            source: match source {
                DestinationSource::Request => "request",
                DestinationSource::List => "list",
                DestinationSource::Public => "public",
            }
            .to_owned(),
            list_version,
            single_use,
        }
    }

    pub(crate) fn endpoint_id(&self) -> Result<PaymentEndpointIdentifier> {
        Ok(PaymentEndpointIdentifier::parse(&self.endpoint_id)?)
    }

    pub(crate) fn payload(&self) -> Result<PaymentEndpointPayload> {
        Ok(PaymentEndpointPayload::parse(&self.payload)?)
    }

    pub(crate) fn source(&self) -> DestinationSource {
        match self.source.as_str() {
            "list" => DestinationSource::List,
            "public" => DestinationSource::Public,
            _ => DestinationSource::Request,
        }
    }
}

/// An amount as kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptAmount {
    pub(crate) value: String,
    pub(crate) asset: String,
}

impl KeptAmount {
    pub(crate) fn of(amount: &PaymentAmount) -> Self {
        Self {
            value: amount.value.as_str().to_owned(),
            asset: amount.asset.as_str().to_owned(),
        }
    }

    pub(crate) fn amount(&self) -> Result<PaymentAmount> {
        Ok(PaymentAmount {
            value: Decimal::parse(&self.value)?,
            asset: Asset::parse(&self.asset)?,
        })
    }
}

/// One payment in the journal (`p/j/<attempt_id>`). Sensitive.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct JournalEntry {
    pub(crate) attempt_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) obligation: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) payee: [u8; 32],
    /// The paid request, `None` for a payment without one.
    pub(crate) request_hash: Option<String>,
    /// The natural key of the payer's activity record.
    #[serde(with = "serde_b64_vec")]
    pub(crate) natural_key: Vec<u8>,
    pub(crate) amount: KeptAmount,
    pub(crate) destination: Destination,
    pub(crate) deadline: Option<u64>,
    pub(crate) stage: Stage,
    /// The claim's step after the attempt, once written.
    pub(crate) step: Option<u64>,
    /// The request's label, which the payer's replies copy.
    pub(crate) label: Option<String>,
    pub(crate) payment_reference: Option<String>,
    /// The acceptance is owed with the payment (Paykit spec 7.2 step 2).
    pub(crate) acceptance_owed: bool,
    pub(crate) acceptance_msg_id: Option<String>,
    /// The acceptance's signed plaintext, for its stored copy in the request record, which
    /// the outcome's batch writes (Paykit spec 7.2 step 3).
    #[serde(default, with = "serde_b64_opt")]
    pub(crate) acceptance_plaintext: Option<Vec<u8>>,
    /// The rail's proof, once paid.
    pub(crate) rail_proof: Option<Map<String, Value>>,
    pub(crate) paid_at_ms: Option<u64>,
    pub(crate) proof_msg_id: Option<String>,
    /// The entry came back from a backup.
    pub(crate) restored: bool,
    /// Device time when journaled: the earliest a dispatch could start.
    pub(crate) journaled_at_ms: u64,
    /// The verified message carrying the destination, for the destination check before a
    /// repeat (Paykit spec 7.2 step 1). Neither it nor `carrier_record` in an entry kept
    /// before carriers were journaled: such an entry is never dispatched again.
    #[serde(default)]
    pub(crate) carrier: Option<KeptMessage>,
    /// The signed app record carrying a public endpoint, for the same check.
    #[serde(default)]
    pub(crate) carrier_record: Option<SignedDoc>,
    /// The attempt's transition proof, for the service and generation it was written at
    /// (Paykit spec 4, "Fresh before paying"). An entry without one is never dispatched again.
    #[serde(default)]
    pub(crate) attempt_proof: Option<SignedDoc>,
}

impl std::fmt::Debug for JournalEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JournalEntry({:?})", self.stage)
    }
}

impl JournalEntry {
    pub(crate) fn attempt(&self) -> Result<AttemptId> {
        AttemptId::parse(&self.attempt_id).map_err(|_| PaykitError::corrupt())
    }

    pub(crate) fn request_hash(&self) -> Result<Option<[u8; 32]>> {
        self.request_hash
            .as_deref()
            .map(|h| {
                pubky_switch_core::enc::unb64_array::<32>(h).map_err(|_| PaykitError::corrupt())
            })
            .transpose()
    }

    /// Keep the destination's carrier in the entry.
    pub(crate) fn keep_carrier(&mut self, carrier: &Carrier) {
        match carrier {
            Carrier::Message(message) => self.carrier = Some(KeptMessage::of(message)),
            Carrier::Record(record) => self.carrier_record = Some(record.signed().clone()),
        }
    }

    /// The destination's carrier, verified again for `own`; `None` when the entry keeps none.
    pub(crate) fn kept_carrier(&self, own: &[u8; 32]) -> Result<Option<Carrier>> {
        if let Some(kept) = &self.carrier {
            return Ok(Some(Carrier::Message(Box::new(kept.stored(own)?))));
        }
        match &self.carrier_record {
            Some(signed) => AppRecord::verify(signed, &self.payee)
                .map(|record| Some(Carrier::Record(Box::new(record))))
                .map_err(|_| PaykitError::corrupt()),
            None => Ok(None),
        }
    }

    /// Whether the entry still has work: not finished, not abandoned.
    pub(crate) fn open(&self) -> bool {
        !matches!(self.stage, Stage::Finished | Stage::Abandoned)
    }
}

/// A payment in the journal, as the host sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentView {
    /// The attempt ID, the payment method's idempotency key.
    pub attempt_id: AttemptId,
    /// The claim key of the obligation paid.
    pub obligation: [u8; 32],
    /// The payee.
    pub payee: [u8; 32],
    /// The request paid, `None` for a payment without one.
    pub request_hash: Option<[u8; 32]>,
    /// The amount.
    pub amount: PaymentAmount,
    /// The identifier paid.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// Where it stands.
    pub stage: Stage,
    /// The entry came back from a backup.
    pub restored: bool,
}

impl JournalEntry {
    pub(crate) fn view(&self) -> Result<PaymentView> {
        Ok(PaymentView {
            attempt_id: self.attempt()?,
            obligation: self.obligation,
            payee: self.payee,
            request_hash: self.request_hash()?,
            amount: self.amount.amount()?,
            endpoint_id: self.destination.endpoint_id()?,
            stage: self.stage,
            restored: self.restored,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_journal_destination_debug_hides_the_payload() {
        let destination = Destination::new(
            [1; 16],
            &PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            &PaymentEndpointPayload::new("lnbc-secret").unwrap(),
            DestinationSource::Request,
            None,
            false,
        );
        let text = format!("{destination:?}");
        assert!(!text.contains("secret"), "{text}");
        assert_eq!(destination.source(), DestinationSource::Request);
        assert_eq!(destination.payload().unwrap().as_str(), "lnbc-secret");
    }

    #[test]
    fn test_journal_amount_round_trips_and_refuses_garbage() {
        let amount = PaymentAmount {
            value: Decimal::new("0.00025").unwrap(),
            asset: Asset::new("btc").unwrap(),
        };
        assert_eq!(KeptAmount::of(&amount).amount().unwrap(), amount);
        let bad = KeptAmount {
            value: "-1".into(),
            asset: "btc".into(),
        };
        assert!(bad.amount().is_err());
    }
}
