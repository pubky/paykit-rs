//! The approving app's retained first executor grant (Paykit spec 8.3).

use paykit_lib::derive::natural_key;
use paykit_lib::records::AllowanceRecord;
use paykit_lib::{AllowanceId, Body, Collection};
use pubky_switch_client::app::{App, KeptOp, RecordTarget};
use pubky_switch_core::enc::{hash, serde_b64};
use pubky_switch_core::ops::Condition;
use serde::{Deserialize, Serialize};

use super::approval::ApprovedOpening;
use super::evidence::AppliedRecord;
use crate::activity::switch_collection;
use crate::allowances::allowance_target;

/// The approving app keeps the signed first grant alongside its original acceptance.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutorCheckpoint {
    #[serde(with = "serde_b64")]
    pub(crate) counterparty: [u8; 32],
    pub(crate) allowance_id: String,
    pub(super) grant: GrantEvidence,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum GrantEvidence {
    /// Installed by the Switch client's applied-local transaction for this grant batch.
    AwaitingProof {
        #[serde(with = "serde_b64")]
        value_hash: [u8; 32],
    },
    Verified {
        write: KeptOp,
    },
}

/// The installed and selected backups can retain different accepted writes. Preserve each
/// independently bound grant until the original application can be established.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutorHistory(pub(crate) Vec<ExecutorCheckpoint>);

impl ExecutorHistory {
    pub(crate) fn bound(
        &self,
        binder: &crate::state::Binder<'_>,
        record_key: &[u8; 32],
        executor: [u8; 16],
    ) -> bool {
        !self.0.is_empty()
            && self.0.iter().all(|c| {
                let Ok(id) = AllowanceId::parse(&c.allowance_id) else {
                    return false;
                };
                let target = allowance_target(&c.counterparty, &id);
                if binder.record_key(&target).ok().as_ref() != Some(record_key) {
                    return false;
                }
                let write = |kept: &KeptOp, key: &[u8; 32]| -> Option<()> {
                    let cert =
                        paykit_lib::scopes::verify_certificate(&kept.cert, binder.identity())
                            .ok()?;
                    let op = pubky_switch_core::ops::SignedOp::verify_with_cert(
                        &kept.op,
                        &cert,
                        binder.identity(),
                    )
                    .ok()?;
                    let p = pubky_switch_core::chain::TransitionProof::peek(kept.proof.as_ref()?)
                        .ok()?;
                    p.check_binds(&op).ok()?;
                    p.record_revision().ok()?;
                    let pubky_switch_core::ops::OpKind::RecordPut(put) = &op.body().kind else {
                        return None;
                    };
                    (put.key == *key && cert.app_id() == executor).then_some(())
                };
                match &c.grant {
                    GrantEvidence::AwaitingProof { .. } => true,
                    GrantEvidence::Verified { write: kept } => write(kept, record_key).is_some(),
                }
            })
    }

    pub(crate) fn include(&mut self, other: Self) {
        for checkpoint in other.0 {
            if !self.0.contains(&checkpoint) {
                self.0.push(checkpoint);
            }
        }
    }

    pub(super) fn selected(
        &self,
        app: &App,
        key: &[u8; 32],
        opening: &ApprovedOpening,
    ) -> Option<ExecutorCheckpoint> {
        let first = self.0.first()?;
        let Some(verified) = self
            .0
            .iter()
            .find(|source| matches!(source.grant, GrantEvidence::Verified { .. }))
        else {
            return self
                .0
                .iter()
                .all(|source| source == first)
                .then(|| first.clone());
        };
        let original = verified.accepted(app, key, opening)?;
        let value_hash = hash(&original.grant.value);
        for source in &self.0 {
            if source.counterparty != verified.counterparty
                || source.allowance_id != verified.allowance_id
            {
                return None;
            }
            match &source.grant {
                // Restore may combine the applied-local marker and the later proof of
                // that same signed original. A matching hash alone grants no authority.
                GrantEvidence::AwaitingProof {
                    value_hash: pending,
                } if *pending == value_hash => {}
                GrantEvidence::Verified { .. }
                    if source.accepted(app, key, opening)?.grant.revision
                        == original.grant.revision => {}
                _ => return None,
            }
        }
        Some(verified.clone())
    }
}

pub(super) fn ledger_target(counterparty: &[u8; 32], id: &AllowanceId) -> RecordTarget {
    RecordTarget::new(
        switch_collection(Collection::AllowanceLedgers, None),
        &natural_key::allowance(counterparty, id),
    )
}

pub(super) struct AcceptedGrant {
    pub(super) grant: AppliedRecord,
}

impl ExecutorCheckpoint {
    pub(super) fn accepted(
        &self,
        app: &App,
        permanent_key: &[u8; 32],
        opening: &ApprovedOpening,
    ) -> Option<AcceptedGrant> {
        let id = AllowanceId::parse(&self.allowance_id).ok()?;
        let target = allowance_target(&self.counterparty, &id);
        let ledger_target = ledger_target(&self.counterparty, &id);
        let ledger_key = app.record_key(&ledger_target).ok()?;
        if app.record_key(&target).ok()? != *permanent_key {
            return None;
        }
        let own = app.identity();
        let GrantEvidence::Verified { write } = &self.grant else {
            return None;
        };
        let grant = AppliedRecord::retained(app, &target, write)?;
        let initial = &opening.ledger;
        let predecessor = &opening.permanent;
        if !grant.follows(predecessor) || !opening.grant_pins_pending(&grant) {
            return None;
        }
        let original = &opening.record;
        let record =
            AllowanceRecord::bind(&grant.value, &own, app.naming_key(), permanent_key).ok()?;
        let mut without_grant = record.clone();
        without_grant.grant = None;
        without_grant.grants.clear();
        if without_grant.to_json().ok()? != original.to_json().ok()? {
            return None;
        }
        let (signed, _) = record.current_grant(&own)?;
        let Body::AllowanceProposal(proposed) = record.proposal.body() else {
            return None;
        };
        let grant_entry = record.grants.last()?;
        if !initial.cert.holds_scope("paykit_allowance")
            || !grant.cert.holds_scope("paykit_allowance_execute")
            || record.grants.len() != 1
            || record.grant.as_ref() != Some(signed)
            || record.end.is_some()
            || record.conflicted.is_some()
            || grant_entry.built_on != grant.expected
            || signed.app_id != app.app_id()
            || grant.op.body().app_id != Some(app.app_id())
            || signed.confirmed_generation != grant.revision.generation
            || !super::in_grant_history(
                proposed.allower_generation,
                grant.revision.generation,
                app.own_identity().body().recovery_from,
            )
            || opening.deletion.seq >= grant.revision.seq
            || !grant
                .op
                .body()
                .conditions
                .contains(&Condition::RecordCurrent {
                    key: ledger_key,
                    revision: initial.revision,
                    deleted: false,
                })
        {
            return None;
        }
        Some(AcceptedGrant { grant })
    }
}
