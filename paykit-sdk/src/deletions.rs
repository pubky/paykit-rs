//! Retain a payee's verified receipts and their credit restriction (Paykit spec 6, "Verified
//! receipts").

mod restore;

pub(crate) use restore::RestoredReceipts;
use std::collections::BTreeSet;

use paykit_lib::collections::Collection;
use paykit_lib::records::{ActivityRecord, ActivitySide, RecordBinding};
use paykit_lib::{derive::natural_key, Label};
use pubky_switch_client::app::ReceiveContext;
use pubky_switch_core::enc::{serde_b64, serde_b64_vec};
use serde::{Deserialize, Serialize};

use crate::activity::ActivityTarget;
use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::receipts::{receipt_binding, ReceiptBinding};
use crate::receive::{Fail, Settlement};
use crate::state::{self, key};

/// A method may be temporarily unavailable when an installation restores its receipts.
/// Keep at most 256 bytes of distinct candidate keys for one signed receipt.
const MAX_RECEIPT_SOURCES: usize = 8;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ReceiptSource {
    Bound {
        #[serde(with = "serde_b64")]
        record: [u8; 32],
        #[serde(with = "serde_b64_vec")]
        natural_key: Vec<u8>,
    },
    Pending(BTreeSet<[u8; 32]>),
}

impl ReceiptSource {
    fn bound(&self) -> Option<[u8; 32]> {
        match self {
            Self::Bound { record, .. } => Some(*record),
            Self::Pending(_) => None,
        }
    }

    fn contains(&self, key: &[u8; 32]) -> bool {
        match self {
            Self::Bound { record, .. } => key == record,
            Self::Pending(keys) => keys.contains(key),
        }
    }

    fn merge(&mut self, other: Self) -> Result<()> {
        if let Self::Pending(keys) = self {
            match other {
                Self::Bound { .. } => *self = other,
                Self::Pending(more) => {
                    // More source keys than a retained receipt holds: its credit history
                    // can't be kept (Paykit spec 6).
                    if keys.union(&more).count() > MAX_RECEIPT_SOURCES {
                        return Err(Refusal::CreditHistoryLost.into());
                    }
                    keys.extend(more);
                }
            }
        }
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
enum SourceDecision {
    Bound,
    MethodUnavailable,
    Misplaced,
}

/// A payee's unrequested payment needs its method's settlement ID to derive its
/// original activity key. Its signed messages can be checked independently.
fn payee_natural_key(
    activity: &ActivityRecord,
    own: &[u8; 32],
    settlement: Settlement<'_>,
) -> Result<Option<Vec<u8>>> {
    if matches!(activity.binding()?, RecordBinding::Unrequested) {
        let Some(paykit_lib::Body::PaymentProof(proof)) = activity.proof.as_ref().map(|p| p.body())
        else {
            return Err(PaykitError::corrupt());
        };
        return Ok(settlement(&proof.payment_endpoint_identifier, &proof.proof)
            .map(|id| natural_key::payee_payment_record(&proof.payment_endpoint_identifier, &id)));
    }
    Ok(Some(activity.natural_key_with(own, settlement)?))
}

/// A signed receipt and the material needed to check its original occurrence.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReceiptEvidence {
    source: ReceiptSource,
    #[serde(with = "serde_b64")]
    hash: [u8; 32],
    #[serde(with = "serde_b64_vec")]
    value: Vec<u8>,
    #[serde(deserialize_with = "Option::deserialize")]
    label: Option<String>,
    #[serde(with = "serde_b64_vec")]
    parent: Vec<u8>,
}

impl ReceiptEvidence {
    fn of(
        source: ReceiptSource,
        activity: &ActivityRecord,
        label: Option<&Label>,
        parent: Vec<u8>,
    ) -> Result<Option<Self>> {
        let (Some(_), Some(receipt)) = (&activity.proof, &activity.receipt) else {
            return Ok(None);
        };
        Ok(Some(Self {
            source,
            hash: receipt.hash(),
            value: Self::signed_roles(activity)?,
            label: label.map(|l| l.as_str().to_owned()),
            parent,
        }))
    }

    fn signed_roles(activity: &ActivityRecord) -> Result<Vec<u8>> {
        // Retain only the signed messages that identify this credit. Other activity,
        // including a later stop or permission, belongs to the selected shared record.
        let mut signed = ActivityRecord::new(ActivitySide::Payee, activity.counterparty);
        signed.request.clone_from(&activity.request);
        signed.proof.clone_from(&activity.proof);
        signed.receipt.clone_from(&activity.receipt);
        if activity
            .proof
            .as_ref()
            .is_some_and(|p| p.message().refs.get(1).is_some())
        {
            signed.period_details.clone_from(&activity.period_details);
        }
        Ok(signed.to_json()?)
    }

    pub(crate) fn key(&self) -> String {
        key::retained_receipt(&self.hash)
    }

    pub(crate) fn check(
        &self,
        own: &[u8; 32],
        record_key: impl Fn(&pubky_switch_client::app::RecordTarget) -> Result<[u8; 32]>,
    ) -> Result<()> {
        let label = self.label.as_deref().map(Label::parse).transpose()?;
        let activity = ActivityRecord::read(&self.value, own, ActivitySide::Payee)?;
        activity.check_label(label.as_ref())?;
        let unrequested = matches!(activity.binding()?, RecordBinding::Unrequested);
        let source_valid = match &self.source {
            ReceiptSource::Bound {
                record,
                natural_key,
            } => {
                let target = ActivityTarget::payee(natural_key.clone(), label.as_ref());
                record_key(&target.record_target())? == *record
                    // A record bound to a request or period is keyed by it, never by a
                    // settlement, so no reader is needed for it.
                    && (unrequested || activity.natural_key_with(own, &|_, _| None)? == *natural_key)
            }
            ReceiptSource::Pending(keys) => {
                unrequested && (1..=MAX_RECEIPT_SOURCES).contains(&keys.len())
            }
        };
        if !source_valid
            || activity.proof.is_none()
            || activity.receipt.as_ref().map(|r| r.hash()) != Some(self.hash)
            || self.value != Self::signed_roles(&activity)?
        {
            return Err(PaykitError::corrupt());
        }
        if !self.parent.is_empty() {
            let parent = ActivityRecord::read(&self.parent, own, ActivitySide::Payee)?;
            parent.check_label(label.as_ref())?;
            if !matches!(activity.binding()?, RecordBinding::Period { .. })
                || receipt_binding(&activity, Some(&parent)) != ReceiptBinding::Verified
                || self.parent != signed_request(&parent)?
            {
                return Err(PaykitError::corrupt());
            }
        }
        Ok(())
    }

    /// Bind the signed proof to an original source key with the method currently
    /// installed. An imported natural key cannot attest to an unavailable method.
    fn resolve(
        &mut self,
        own: &[u8; 32],
        settlement: Settlement<'_>,
        record_key: impl Fn(&pubky_switch_client::app::RecordTarget) -> Result<[u8; 32]>,
        imported: bool,
    ) -> Result<(SourceDecision, ActivityRecord)> {
        let activity = ActivityRecord::read(&self.value, own, ActivitySide::Payee)?;
        let Some(natural_key) = payee_natural_key(&activity, own, settlement)? else {
            if imported {
                if let Some(key) = self.source.bound() {
                    self.source = ReceiptSource::Pending(BTreeSet::from([key]));
                }
            }
            return Ok((SourceDecision::MethodUnavailable, activity));
        };
        let label = self.label.as_deref().map(Label::parse).transpose()?;
        let target = ActivityTarget::payee(natural_key.clone(), label.as_ref());
        let record = record_key(&target.record_target())?;
        if !self.source.contains(&record) {
            return Ok((SourceDecision::Misplaced, activity));
        }
        self.source = ReceiptSource::Bound {
            record,
            natural_key,
        };
        Ok((SourceDecision::Bound, activity))
    }

    fn supplement(&mut self, other: Self) -> Result<()> {
        self.source.merge(other.source)?;
        if self.parent.is_empty() {
            self.parent = other.parent;
        }
        Ok(())
    }
}

/// A deletion restriction in current signed activity, without an observation of
/// the deleting operation itself.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreditHold {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    #[serde(with = "serde_b64")]
    marker: [u8; 32],
}

impl CreditHold {
    pub(crate) fn key(&self) -> String {
        key::credit_hold(&self.record)
    }
}

/// A signed receipt retained for the payee's payment history (Paykit spec 6).
#[derive(Clone, Debug)]
pub struct RetainedPayeeReceipt {
    /// The original activity record key.
    pub record: [u8; 32],
    /// The original verified receipt signed by the payee.
    pub receipt: paykit_lib::records::StoredMessage,
}

fn period_parent(
    own: &[u8; 32],
    record: &ActivityRecord,
    label: Option<&Label>,
) -> Option<ActivityTarget> {
    crate::receipts::period_request_target(record, own, label)
}

fn signed_request(parent: &ActivityRecord) -> Result<Vec<u8>> {
    let mut signed = ActivityRecord::new(ActivitySide::Payee, parent.counterparty);
    signed.request.clone_from(&parent.request);
    Ok(signed.to_json()?)
}

fn usable(r: &pubky_switch_client::app::SharedRecord, label: Option<&Label>) -> bool {
    !r.not_taken
        && !r.flagged
        && !r.different_write_at_held_version
        && !r.divergent
        && r.policy == Collection::PayeeActivity.policy(label)
}

pub(crate) fn held_on_receive(
    cx: &ReceiveContext<'_>,
    record: &[u8; 32],
) -> std::result::Result<bool, Fail> {
    Ok(state::cx_get::<CreditHold>(cx, &key::credit_hold(record))?.is_some())
}

impl Paykit {
    pub(crate) async fn payee_deletion_held(&self, record: &[u8; 32]) -> Result<bool> {
        Ok(self
            .local_get::<CreditHold>(&key::credit_hold(record))
            .await?
            .is_some())
    }

    async fn checked_receipt(
        &mut self,
        evidence: &mut ReceiptEvidence,
    ) -> Result<Option<(ActivityRecord, ReceiptBinding)>> {
        let was_pending = evidence.source.bound().is_none();
        let (decision, activity) = evidence.resolve(
            &self.identity(),
            &|endpoint, proof| self.rail.settlement_id(endpoint, proof),
            |t| Ok(self.app.record_key(t)?),
            false,
        )?;
        if decision != SourceDecision::Bound {
            return Ok(None);
        }
        if was_pending {
            self.local_put(&evidence.key(), evidence).await?;
        }
        let label = evidence.label.as_deref().map(Label::parse).transpose()?;
        let read = |bytes: &[u8], key: &[u8; 32]| {
            crate::records::bind_activity(
                bytes,
                &self.identity(),
                ActivitySide::Payee,
                label.as_ref(),
                key,
                |t| self.app.record_key(t).ok(),
                &|endpoint, proof| self.rail.settlement_id(endpoint, proof),
            )
        };
        let parent =
            if let Some(target) = period_parent(&self.identity(), &activity, label.as_ref()) {
                let k = self.activity_key(&target)?;
                match read(&evidence.parent, &k) {
                    Some(parent) => Some(parent),
                    None => self
                        .app
                        .shared_record(&k)
                        .await?
                        .filter(|r| usable(r, label.as_ref()))
                        .and_then(|r| r.content.value().and_then(|v| read(v, &k))),
                }
            } else {
                None
            };
        let status = receipt_binding(&activity, parent.as_ref());
        Ok(Some((activity, status)))
    }

    pub(crate) fn credits_settlement(&self, other: &ActivityRecord, settlement: &[u8]) -> bool {
        other.receipt.is_some()
            && other.proof.as_ref().is_some_and(|p| {
                matches!(p.body(), paykit_lib::Body::PaymentProof(proof)
                if self.rail.settlement_id(&proof.payment_endpoint_identifier, &proof.proof).as_deref() == Some(settlement))
            })
    }

    pub(crate) async fn retained_credit(
        &mut self,
        already_shown: Option<[u8; 32]>,
        settlement: &[u8],
    ) -> Result<bool> {
        let mut incomplete = false;
        for (_, mut evidence) in self
            .local_scan::<ReceiptEvidence>(key::RETAINED_RECEIPTS)
            .await?
        {
            if Some(evidence.hash) == already_shown {
                continue;
            }
            let Some((activity, status)) = self.checked_receipt(&mut evidence).await? else {
                continue;
            };
            if self.credits_settlement(&activity, settlement) {
                match status {
                    ReceiptBinding::Verified => return Ok(true),
                    ReceiptBinding::MissingRequest => incomplete = true,
                    ReceiptBinding::Invalid => {}
                }
            }
        }
        if incomplete {
            return Err(
                Refusal::HeldForUser(crate::HeldForUserReason::RecordUnreadable(
                    "the signed request of a retained receipt is unavailable",
                ))
                .into(),
            );
        }
        Ok(false)
    }

    /// The original verified receipts this app retained, including a receipt recovered
    /// from its previous installation. Each signed receipt appears once. When a
    /// payment method becomes available, its verified source key replaces the
    /// pending keys before the receipt is shown.
    ///
    /// # Errors
    ///
    /// A local store failure.
    pub async fn retained_payee_receipts(&mut self) -> Result<Vec<RetainedPayeeReceipt>> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        let mut result = Vec::new();
        for (_, mut e) in self
            .local_scan::<ReceiptEvidence>(key::RETAINED_RECEIPTS)
            .await?
        {
            let label = e.label.as_deref().map(Label::parse).transpose()?;
            if !self
                .app
                .cert()
                .holds_scope(&Collection::PayeeActivity.policy(label.as_ref()).read_scope)
            {
                continue;
            }
            if let Some((activity, ReceiptBinding::Verified)) = self.checked_receipt(&mut e).await?
            {
                if let Some(receipt) = activity.receipt {
                    let record = e.source.bound().ok_or_else(PaykitError::corrupt)?;
                    result.push(RetainedPayeeReceipt { record, receipt });
                }
            }
        }
        Ok(result)
    }
}
