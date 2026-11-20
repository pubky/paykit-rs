//! The restored executor's own previously accepted grant and complete ledger.

use paykit_lib::allowances::{check_successor, Ledger};
use paykit_lib::derive::natural_key;
use paykit_lib::records::{bind_ledger, AllowanceRecord};
use paykit_lib::{AllowanceId, AttemptId, Body, Collection};
use pubky_switch_client::app::{App, KeptOp, RecordTarget};
use pubky_switch_core::enc::{hash, serde_b64};
use pubky_switch_core::ops::Condition;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

use super::approval::ApprovedOpening;
use super::evidence::AppliedRecord;
use crate::activity::switch_collection;
use crate::allowances::allowance_target;

/// Signed originals the executor accepted before the backup. The grant names the initial
/// ledger application. A later accepted checkpoint keeps its complete signed value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutorCheckpoint {
    /// Set by the restore transaction. A first-use grant or subsequent signed service
    /// observations alone cannot clear the need for independent wallet history.
    pub(crate) restored: bool,
    #[serde(with = "serde_b64")]
    pub(crate) counterparty: [u8; 32],
    pub(crate) allowance_id: String,
    pub(super) grant: GrantEvidence,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(super) later: Option<KeptOp>,
    pub(super) signers: ReservationSigners,
}

/// Certificates of accepted reservation batches, shared by the openings they signed.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ReservationSigners(Vec<ReservationSigner>);

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReservationSigner {
    cert: SignedDoc,
    /// Attempt ID to the hash of the exact signed opening operation in that batch.
    openings: BTreeMap<String, [u8; 32]>,
}

impl ReservationSigners {
    pub(super) fn certificates(&self) -> impl Iterator<Item = &SignedDoc> {
        self.0.iter().map(|signer| &signer.cert)
    }

    pub(super) fn keep(&mut self, id: String, opening: [u8; 32], cert: &SignedDoc) -> Option<()> {
        for signer in &self.0 {
            if let Some(hash) = signer.openings.get(&id) {
                return (hash == &opening && &signer.cert == cert).then_some(());
            }
        }
        if let Some(signer) = self.0.iter_mut().find(|s| &s.cert == cert) {
            signer.openings.insert(id, opening);
        } else {
            self.0.push(ReservationSigner {
                cert: cert.clone(),
                openings: BTreeMap::from([(id, opening)]),
            });
        }
        Some(())
    }

    pub(super) fn original<'a>(&self, id: AttemptId, op: &'a KeptOp) -> Option<Cow<'a, KeptOp>> {
        let id = id.to_text();
        let Some((cert, hash)) = self
            .0
            .iter()
            .find_map(|s| Some((&s.cert, s.openings.get(&id)?)))
        else {
            return Some(Cow::Borrowed(op));
        };
        if &op.op.doc_hash().ok()? != hash {
            return None;
        }
        if cert == &op.cert {
            return Some(Cow::Borrowed(op));
        }
        Some(Cow::Owned(KeptOp {
            cert: cert.clone(),
            ..op.clone()
        }))
    }

    fn bound(&self, identity: &[u8; 32], executor: [u8; 16]) -> bool {
        let mut seen = HashSet::new();
        self.0.iter().all(|signer| {
            paykit_lib::scopes::verify_certificate(&signer.cert, identity).is_ok_and(|cert| {
                cert.app_id() == executor && cert.holds_scope("paykit_allowance_execute")
            }) && !signer.openings.is_empty()
                && signer.openings.keys().all(|id| {
                    AttemptId::parse(id).is_ok_and(|a| a.to_text() == *id && seen.insert(a))
                })
        })
    }

    pub(super) fn retain(&mut self, ledger: &Ledger) {
        let active: HashSet<_> = ledger
            .entries
            .iter()
            .map(|e| e.attempt_id.to_text())
            .chain(ledger.failed.iter().map(|e| e.attempt_id.to_text()))
            .collect();
        self.0.retain_mut(|signer| {
            signer.openings.retain(|id, _| active.contains(id));
            !signer.openings.is_empty()
        });
    }
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
/// independently bound history until their exact applications can be ordered.
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
                let Ok(ledger) = binder.record_key(&ledger_target(&c.counterparty, &id)) else {
                    return false;
                };
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
                let grant = match &c.grant {
                    GrantEvidence::AwaitingProof { .. } => {
                        c.later.is_none() && c.signers.0.is_empty()
                    }
                    GrantEvidence::Verified { write: kept } => write(kept, record_key).is_some(),
                };
                grant
                    && c.signers.bound(binder.identity(), executor)
                    && c.later
                        .as_ref()
                        .is_none_or(|kept| write(kept, &ledger).is_some())
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
        let mut verified = Vec::new();
        for source in &self.0 {
            if source.counterparty != first.counterparty
                || source.allowance_id != first.allowance_id
            {
                return None;
            }
            if matches!(source.grant, GrantEvidence::Verified { .. }) {
                verified.push((source, source.accepted(app, key, opening)?));
            }
        }
        let Some((_, original)) = verified.first() else {
            return self
                .0
                .iter()
                .all(|source| source == first && source.later.is_none())
                .then(|| first.clone());
        };
        // The applied-local marker can only match the authenticated original grant.
        // Its backup contains no ledger write to order with the verified checkpoints.
        let value_hash = hash(&original.grant.value);
        if self.0.iter().any(|source| {
            matches!(&source.grant, GrantEvidence::AwaitingProof { value_hash: pending }
                if *pending != value_hash || source.later.is_some())
        }) {
            return None;
        }
        verified.sort_by_key(|(_, accepted)| accepted.application.revision);
        for pair in verified.windows(2) {
            let (older, a) = &pair[0];
            let (_, b) = &pair[1];
            let Body::AllowanceProposal(proposed) = a.record.proposal.body() else {
                return None;
            };
            // The grant already pins its original zero. A retained complete checkpoint
            // under that grant can follow a later write. Two noninitial checkpoints
            // still need the exact predecessor between them (Paykit spec 8.3).
            if a.grant.revision != b.grant.revision
                || a.application.revision == b.application.revision && a.ledger != b.ledger
                || (a.application.revision != b.application.revision
                    && !(b.application.follows(&a.application)
                        || older.later.is_none() && b.application.pins_write_after(&a.application)))
                || check_successor(
                    &proposed.terms,
                    &a.ledger,
                    a.application.at_ms,
                    &b.ledger,
                    b.application.at_ms,
                )
                .is_err()
            {
                return None;
            }
        }
        let (latest, accepted) = verified.last()?;
        let mut selected = (*latest).clone();
        if verified.len() > 1 {
            for (source, _) in &verified {
                for signer in &source.signers.0 {
                    for (id, hash) in &signer.openings {
                        selected.signers.keep(id.clone(), *hash, &signer.cert)?;
                    }
                }
            }
            selected.signers.retain(&accepted.ledger);
        }
        Some(selected)
    }
}

pub(super) fn ledger_target(counterparty: &[u8; 32], id: &AllowanceId) -> RecordTarget {
    RecordTarget::new(
        switch_collection(Collection::AllowanceLedgers, None),
        &natural_key::allowance(counterparty, id),
    )
}

pub(super) struct AcceptedLedger {
    pub(super) grant: AppliedRecord,
    pub(super) record: AllowanceRecord,
    pub(super) application: AppliedRecord,
    pub(super) ledger: Ledger,
}

impl ExecutorCheckpoint {
    pub(super) fn accepted(
        &self,
        app: &App,
        permanent_key: &[u8; 32],
        opening: &ApprovedOpening,
    ) -> Option<AcceptedLedger> {
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
        let mut ledger = opening.zero.clone();
        let mut application = initial.clone();
        if let Some(held) = &self.later {
            let newer = AppliedRecord::retained(app, &ledger_target, held)?;
            if !newer.cert.holds_scope("paykit_allowance_execute")
                || !newer
                    .op
                    .body()
                    .conditions
                    .contains(&Condition::RecordCurrent {
                        key: *permanent_key,
                        revision: grant.revision,
                        deleted: false,
                    })
                || newer.op.body().app_id != Some(app.app_id())
                || newer.revision.seq <= grant.revision.seq
                || record.executor_at(&own, grant.version()).ok()? != Some(app.app_id())
            {
                return None;
            }
            let next = bind_ledger(&newer.value, app.naming_key(), &ledger_key, &record).ok()?;
            check_successor(
                &proposed.terms,
                &ledger,
                application.at_ms,
                &next,
                newer.at_ms,
            )
            .ok()?;
            ledger = next;
            application = newer;
        }
        Some(AcceptedLedger {
            grant,
            record,
            application,
            ledger,
        })
    }
}
