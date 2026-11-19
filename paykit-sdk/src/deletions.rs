//! Retain a payee’s signed receipt and its credit restriction after a deletion (Paykit spec 6).

mod restore;

pub(crate) use restore::RestoredReceipts;
use std::collections::BTreeSet;

use crate::divergent::SeenVersion;
use paykit_lib::collections::Collection;
use paykit_lib::records::{ActivityRecord, ActivitySide, RecordBinding};
use paykit_lib::{derive::natural_key, Label};
use pubky_switch_client::app::{PutBack, ReceiveContext, RecordDeletion, StoreFailure};
use pubky_switch_core::enc::{b64, serde_b64, serde_b64_vec};
use serde::{Deserialize, Serialize};

use crate::activity::ActivityTarget;
use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::protocol::step_failure;
use crate::protocol::view_failure;
use crate::receipts::{receipt_binding, ReceiptBinding};
use crate::receive::{Fail, Settlement};
use crate::state::{self, key};

/// The compact identity of one observed deletion. Several deletes may name the
/// same original receipt, whose signed evidence is retained once.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayeeDeletion {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    version: SeenVersion,
    #[serde(with = "serde_b64")]
    delete: [u8; 32],
    #[serde(deserialize_with = "Option::deserialize")]
    writer: Option<[u8; 16]>,
    protected: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    notice: Option<Notice>,
    #[serde(deserialize_with = "Option::deserialize")]
    page_time_ms: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    receipt: Option<[u8; 32]>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Notice {
    Pending,
    Acknowledged,
}

fn deletion_key(record: &[u8; 32], generation: u64, version: u64, hash: &[u8; 32]) -> String {
    format!(
        "{}{}/{}/{}",
        key::payee_deletions_at(record),
        state::num(generation),
        state::num(version),
        b64(hash)
    )
}

impl PayeeDeletion {
    pub(crate) fn key(&self) -> String {
        let version = self.version.record_version();
        deletion_key(
            &self.record,
            version.generation,
            version.version,
            &self.delete,
        )
    }

    fn protected_writer(&self) -> Option<[u8; 16]> {
        self.writer.filter(|_| self.protected)
    }

    fn to_notice(&self) -> PayeeDeletionNotice {
        let version = self.version.record_version();
        PayeeDeletionNotice {
            record: self.record,
            generation: version.generation,
            version: version.version,
            delete: self.delete,
            writer: self.protected_writer(),
            page_time_ms: self.page_time_ms,
        }
    }

    fn supplement(&mut self, other: &Self) {
        self.notice = self.notice.or(other.notice);
        self.receipt = self.receipt.or(other.receipt);
    }

    pub(crate) fn restored(mut self) -> Self {
        self.page_time_ms = None;
        self.writer = None;
        self.protected = false;
        self
    }

    pub(crate) fn matches(&self, record: &[u8; 32]) -> bool {
        self.record == *record && !self.version.record_version().is_none()
    }
}

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
                    if keys.union(&more).count() > MAX_RECEIPT_SOURCES {
                        return Err(Refusal::NotNow(
                            "too many source keys for a retained receipt".into(),
                        )
                        .into());
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
        return Ok(settlement(&proof.proof)
            .map(|id| natural_key::payee_payment_record(&proof.payment_endpoint_identifier, &id)));
    }
    Ok(Some(activity.natural_key(own)?))
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
                    && (unrequested || activity.natural_key(own)? == *natural_key)
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

/// A deletion to show the user (Paykit spec 6). A verified receipt or an existing
/// deletion restriction in the exact predecessor establishes a protected deletion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayeeDeletionNotice {
    /// The record the delete named.
    #[serde(with = "serde_b64")]
    pub record: [u8; 32],
    /// The generation of the deletion.
    pub generation: u64,
    /// The sequence of the signed deletion in its generation.
    pub version: u64,
    /// The hash of the deletion the app observed.
    #[serde(with = "serde_b64")]
    pub delete: [u8; 32],
    /// The deleting App ID when the app could prove its predecessor was protected.
    #[serde(deserialize_with = "Option::deserialize")]
    pub writer: Option<[u8; 16]>,
    /// The verified service time of the original feed page, absent on a listing alone.
    #[serde(deserialize_with = "Option::deserialize")]
    pub page_time_ms: Option<u64>,
}

/// A signed receipt retained for the payee's payment history (Paykit spec 6).
#[derive(Clone, Debug)]
pub struct RetainedPayeeReceipt {
    /// The original activity record key.
    pub record: [u8; 32],
    /// The original verified receipt signed by the payee.
    pub receipt: paykit_lib::records::StoredMessage,
    /// Deletions this installation observed for this receipt. A receipt retained
    /// from an older installation can have no observed deletion.
    pub deletions: Vec<PayeeReceiptDeletion>,
}

/// An observed deletion associated with a retained receipt.
#[derive(Clone, Debug)]
pub struct PayeeReceiptDeletion {
    /// The signed item hash of the deletion.
    pub delete: [u8; 32],
    /// The deletion's generation and sequence.
    pub version: pubky_switch_core::ops::RecordVersion,
    /// The deleting app when the signed predecessor held this receipt.
    pub writer: Option<[u8; 16]>,
    /// The original verified feed-page time.
    pub page_time_ms: Option<u64>,
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

/// Observe a deletion. The signed record and the credit hold stay on this app.
pub(crate) fn observe_payee_deletion(
    deletion: &RecordDeletion<'_>,
    cx: &ReceiveContext<'_>,
    settlement: Settlement<'_>,
) -> std::result::Result<Option<PutBack>, StoreFailure> {
    if deletion.collection != Collection::PayeeActivity.name() {
        return Ok(None);
    }
    let mut observation = PayeeDeletion {
        record: *deletion.key,
        version: deletion.version.into(),
        delete: deletion.item_hash,
        writer: Some(deletion.writer),
        protected: false,
        notice: None,
        page_time_ms: deletion.page_time_ms,
        receipt: None,
    };
    let local_key = observation.key();
    let previous =
        state::cx_get::<PayeeDeletion>(cx, &local_key).map_err(Fail::into_store_failure)?;
    let label = match deletion.policy {
        Some(policy) => {
            let Some(label) = Collection::PayeeActivity.policy_label(policy) else {
                return Ok(None);
            };
            if *policy != Collection::PayeeActivity.policy(label.as_ref()) {
                return Ok(None);
            }
            Some(label)
        }
        None => None,
    };
    let mut observed = Vec::new();
    if let Some(label) = label {
        let read = |bytes: &[u8]| {
            crate::records::bind_activity(
                bytes,
                &cx.identity(),
                ActivitySide::Payee,
                label.as_ref(),
                deletion.key,
                |target| cx.record_key(target).ok(),
                settlement,
            )
        };
        let source = deletion
            .exact_value
            .and_then(|v| read(v).map(|r| (r, true)))
            .or_else(|| deletion.seen.and_then(|v| read(v).map(|r| (r, false))));
        if let Some((record, exact)) = source {
            if exact
                && record.receipt.is_none()
                && record.lost_to_delete.is_none()
                && !held_on_receive(cx, deletion.key).map_err(Fail::into_store_failure)?
                && !state::cx_scan::<ReceiptEvidence>(cx, key::RETAINED_RECEIPTS)
                    .map_err(Fail::into_store_failure)?
                    .iter()
                    .any(|(_, r)| r.source.bound() == Some(*deletion.key))
            {
                return Ok(None);
            }
            let mut parent = Vec::new();
            let protected =
                if let Some(target) = period_parent(&cx.identity(), &record, label.as_ref()) {
                    let target = target.record_target();
                    let key = cx
                        .record_key(&target)
                        .map_err(crate::protocol::store_failure)?;
                    let held = cx.record(&target).map_err(crate::protocol::store_failure)?;
                    let request = held
                        .as_ref()
                        .filter(|h| usable(h, label.as_ref()))
                        .and_then(|h| h.content.value())
                        .and_then(|bytes| {
                            crate::records::bind_activity(
                                bytes,
                                &cx.identity(),
                                ActivitySide::Payee,
                                label.as_ref(),
                                &key,
                                |t| cx.record_key(t).ok(),
                                settlement,
                            )
                        });
                    if let Some(p) = &request {
                        parent = signed_request(p).map_err(view_failure)?;
                    }
                    receipt_binding(&record, request.as_ref())
                } else {
                    receipt_binding(&record, None)
                };
            if let Some(evidence) = ReceiptEvidence::of(
                ReceiptSource::Bound {
                    record: *deletion.key,
                    natural_key: record
                        .natural_key_with(&cx.identity(), settlement)
                        .map_err(|e| view_failure(e.into()))?,
                },
                &record,
                label.as_ref(),
                parent,
            )
            .map_err(view_failure)?
            {
                if protected != ReceiptBinding::Invalid {
                    observation.receipt = Some(evidence.hash);
                    let k = evidence.key();
                    let existing = state::cx_get::<ReceiptEvidence>(cx, &k)
                        .map_err(Fail::into_store_failure)?;
                    if existing.as_ref().is_none_or(|e| {
                        e.source.bound() != evidence.source.bound()
                            || (e.parent.is_empty() && !evidence.parent.is_empty())
                    }) {
                        observed.push(state::local_put(&k, &evidence).map_err(view_failure)?);
                    }
                }
            }
            observation.notice = Some(Notice::Pending);
            observation.protected =
                exact && (protected == ReceiptBinding::Verified || record.lost_to_delete.is_some());
        }
    }
    if let Some(mut previous) = previous {
        previous.page_time_ms = previous.page_time_ms.or(observation.page_time_ms);
        previous.writer = Some(deletion.writer);
        previous.protected |= observation.protected;
        previous.supplement(&observation);
        observation = previous;
    }
    observed.push(state::local_put(&local_key, &observation).map_err(view_failure)?);
    Ok(Some(PutBack::Observe { observed }))
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
    Ok(
        state::cx_get::<CreditHold>(cx, &key::credit_hold(record))?.is_some()
            || !state::cx_scan::<PayeeDeletion>(cx, &key::payee_deletions_at(record))?.is_empty(),
    )
}

impl Paykit {
    pub(crate) async fn payee_deletion_held(&self, record: &[u8; 32]) -> Result<bool> {
        Ok(self
            .local_get::<CreditHold>(&key::credit_hold(record))
            .await?
            .is_some()
            || !self
                .local_scan::<PayeeDeletion>(&key::payee_deletions_at(record))
                .await?
                .is_empty())
    }

    async fn checked_receipt(
        &mut self,
        evidence: &mut ReceiptEvidence,
    ) -> Result<Option<(ActivityRecord, ReceiptBinding)>> {
        let was_pending = evidence.source.bound().is_none();
        let (decision, activity) = evidence.resolve(
            &self.identity(),
            &|p| self.rail.settlement_id(p),
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
                &|p| self.rail.settlement_id(p),
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
                if self.rail.settlement_id(&proof.proof).as_deref() == Some(settlement))
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
            return Err(Refusal::NotNow(
                "the signed request of a retained receipt is unavailable".into(),
            )
            .into());
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
        let mut by_receipt = std::collections::BTreeMap::<_, Vec<_>>::new();
        for (_, d) in self
            .local_scan::<PayeeDeletion>(key::PAYEE_DELETIONS)
            .await?
        {
            if let Some(hash) = d.receipt {
                by_receipt
                    .entry((d.record, hash))
                    .or_default()
                    .push(PayeeReceiptDeletion {
                        delete: d.delete,
                        version: d.version.record_version(),
                        writer: d.protected_writer(),
                        page_time_ms: d.page_time_ms,
                    });
            }
        }
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
                    result.push(RetainedPayeeReceipt {
                        record,
                        receipt,
                        deletions: by_receipt.remove(&(record, e.hash)).unwrap_or_default(),
                    });
                }
            }
        }
        Ok(result)
    }

    /// Protected activity deletions to show once per deleting app, or once per deletion
    /// when only an older copy is available (Paykit spec 6).
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn payee_deletion_notices(&self) -> Result<Vec<PayeeDeletionNotice>> {
        // Group only after each complete deletion has retained its own acknowledgment.
        // An older receipt does not establish who deleted protected activity.
        let all = self
            .local_scan::<PayeeDeletion>(key::PAYEE_DELETIONS)
            .await?;
        let mut writers: std::collections::BTreeSet<_> = all
            .iter()
            .filter(|(_, d)| d.notice == Some(Notice::Acknowledged))
            .filter_map(|(_, d)| d.protected_writer())
            .collect();
        Ok(all
            .into_iter()
            .filter(|(_, d)| d.notice == Some(Notice::Pending))
            .filter(|(_, d)| d.protected_writer().is_none_or(|w| writers.insert(w)))
            .map(|(_, d)| d.to_notice())
            .collect())
    }

    /// Stop showing the deletion notice the user has seen.
    ///
    /// # Errors
    ///
    /// A store failure or a notice that is no longer the one shown.
    pub async fn acknowledge_payee_deletion(&mut self, notice: &PayeeDeletionNotice) -> Result<()> {
        let key = deletion_key(
            &notice.record,
            notice.generation,
            notice.version,
            &notice.delete,
        );
        let mut held = self
            .local_get::<PayeeDeletion>(&key)
            .await?
            .ok_or(Refusal::NotFound)?;
        if held.notice.is_none() {
            return Err(Refusal::NotFound.into());
        }
        held.notice = Some(Notice::Acknowledged);
        self.local_put(&key, &held).await
    }
}
