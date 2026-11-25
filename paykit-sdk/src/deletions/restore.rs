//! Reconcile positive receipt evidence and restrictions at an in-place restore.
use std::collections::{btree_map::Entry, BTreeMap};

use super::{
    payee_natural_key, period_parent, receipt_binding, signed_request, CreditHold, PayeeDeletion,
    ReceiptBinding, ReceiptEvidence, ReceiptSource, SourceDecision,
};
use crate::error::{PaykitError, Result};
use crate::receive::Settlement;
use crate::state;
use paykit_lib::collections::Collection;
use paykit_lib::records::{ActivityRecord, ActivitySide};
use paykit_lib::Label;
use pubky_dispatch_client::app::{LocalWrite, RestoreContext};

/// Positive credit evidence and restrictions selected before the atomic restore.
/// Imported notice assertions never strengthen the previous installation's facts.
#[derive(Default)]
pub(crate) struct RestoredReceipts {
    deletions: BTreeMap<String, PayeeDeletion>,
    receipts: BTreeMap<String, ReceiptEvidence>,
    holds: BTreeMap<String, CreditHold>,
}

impl RestoredReceipts {
    pub(crate) fn local(
        &mut self,
        key: &[u8],
        value: &[u8],
        family: &state::StateKey,
        binder: &state::Binder<'_>,
        settlement: Settlement<'_>,
        previous: bool,
    ) -> Result<()> {
        match family {
            state::StateKey::PayeeDeletion { .. } => {
                let d = state::restored::<PayeeDeletion>(key, value, binder)?;
                let d = if previous { d } else { d.restored() };
                self.deletions
                    .entry(d.key())
                    .and_modify(|held| held.supplement(&d))
                    .or_insert(d);
            }
            state::StateKey::RetainedReceipt(..) => {
                let mut r = state::restored::<ReceiptEvidence>(key, value, binder)?;
                let (decision, _) = r.resolve(
                    binder.identity(),
                    settlement,
                    |t| binder.record_key(t),
                    !previous,
                )?;
                if decision != SourceDecision::Misplaced {
                    self.receipt(r)?;
                }
            }
            state::StateKey::CreditHold(_) => {
                let h = state::restored::<CreditHold>(key, value, binder)?;
                self.holds.entry(h.key()).or_insert(h);
            }
            _ => return Err(PaykitError::corrupt()),
        }
        Ok(())
    }

    fn receipt(&mut self, r: ReceiptEvidence) -> Result<()> {
        match self.receipts.entry(r.key()) {
            Entry::Vacant(v) => {
                v.insert(r);
            }
            Entry::Occupied(mut o) => o.get_mut().supplement(r)?,
        }
        Ok(())
    }

    pub(crate) fn previous_activity(
        &mut self,
        restoring: &RestoreContext<'_>,
        binder: &state::Binder<'_>,
        settlement: Settlement<'_>,
    ) -> Result<()> {
        let own = restoring.identity();
        let mut periods = BTreeMap::<_, Vec<_>>::new();
        for held in restoring.previous_records_of(Collection::PayeeActivity.name()) {
            let held = held?;
            let Some(label) = Collection::PayeeActivity.policy_label(&held.policy) else {
                continue;
            };
            if held.policy != Collection::PayeeActivity.policy(label.as_ref()) {
                continue;
            }
            let Some(value) = held.content.value() else {
                continue;
            };
            let Ok(activity) = ActivityRecord::read(value, &own, ActivitySide::Payee) else {
                continue;
            };
            if activity.check_label(label.as_ref()).is_err() {
                continue;
            }
            let Ok(natural_key) = payee_natural_key(&activity, &own, settlement) else {
                continue;
            };
            let Some(natural_key) = natural_key else {
                let source = ReceiptSource::Pending([held.key].into());
                if let Some(receipt) =
                    ReceiptEvidence::of(source, &activity, label.as_ref(), Vec::new())?
                {
                    self.receipt(receipt)?;
                }
                continue;
            };
            let target =
                crate::activity::ActivityTarget::payee(natural_key.clone(), label.as_ref());
            if binder.record_key(&target.record_target())? != held.key {
                continue;
            }
            if let Some(marker) = activity.lost_to_delete {
                let h = CreditHold {
                    record: held.key,
                    marker,
                };
                self.holds.insert(h.key(), h);
            }
            let Some(e) = ReceiptEvidence::of(
                ReceiptSource::Bound {
                    record: held.key,
                    natural_key,
                },
                &activity,
                label.as_ref(),
                Vec::new(),
            )?
            else {
                continue;
            };
            if let Some(target) = period_parent(&own, &activity, label.as_ref()) {
                let k = binder.record_key(&target.record_target())?;
                periods.entry(k).or_default().push((e, activity));
            } else if receipt_binding(&activity, None) == ReceiptBinding::Verified {
                self.receipt(e)?;
            }
        }
        if !periods.is_empty() {
            // Check only the signed parents of the period receipts encountered above.
            // No other contents of the previous collection enter the selected layer.
            for held in restoring.previous_records_of(Collection::PayeeActivity.name()) {
                let held = held?;
                let Some(value) = held.content.value() else {
                    continue;
                };
                let Some(waiting) = periods.remove(&held.key) else {
                    continue;
                };
                for (mut e, activity) in waiting {
                    let label = e.label.as_deref().map(Label::parse).transpose()?;
                    let parent = (held.policy == Collection::PayeeActivity.policy(label.as_ref()))
                        .then(|| {
                            crate::records::bind_activity(
                                value,
                                &own,
                                ActivitySide::Payee,
                                label.as_ref(),
                                &held.key,
                                |t| binder.record_key(t).ok(),
                                settlement,
                            )
                        })
                        .flatten();
                    match receipt_binding(&activity, parent.as_ref()) {
                        ReceiptBinding::Verified => {
                            if let Some(parent) = parent {
                                e.parent = signed_request(&parent)?;
                            }
                            self.receipt(e)?;
                        }
                        ReceiptBinding::MissingRequest => self.receipt(e)?,
                        ReceiptBinding::Invalid => {}
                    }
                }
            }
            // These signed pairs still restrict a second credit of the same settlement.
            // They can only be shown once the original signed request verifies their period.
            for (e, _) in periods.into_values().flatten() {
                self.receipt(e)?;
            }
        }
        Ok(())
    }

    pub(crate) fn writes(self) -> Result<Vec<LocalWrite>> {
        self.deletions
            .into_iter()
            .map(|(k, v)| state::local_put(&k, &v))
            .chain(
                self.receipts
                    .into_iter()
                    .map(|(k, v)| state::local_put(&k, &v)),
            )
            .chain(
                self.holds
                    .into_iter()
                    .map(|(k, v)| state::local_put(&k, &v)),
            )
            .collect()
    }
}
