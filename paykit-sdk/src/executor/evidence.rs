//! Verify the original Allowance applications and first grant (Paykit spec 8.3).

use paykit_lib::scopes::{certificate_in_force_at_write, verify_certificate};
use pubky_switch_client::app::{App, KeptOp, RecordContent, RecordTarget, SharedRecord};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::ident::collection_id;
use pubky_switch_core::ops::{
    check_permission, check_shape, Condition, OpKind, RecordVersion, SignedOp, Writer,
};
use pubky_switch_core::records::AppCert;
use pubky_switch_core::sealed::RecordValue;
use pubky_switch_core::shared::ApplicationRef;

struct AppliedOperation {
    op: SignedOp,
    proof: TransitionProof,
    cert: AppCert,
}

impl AppliedOperation {
    fn verify(app: &App, kept: &KeptOp) -> Option<Self> {
        let applied = Self::verify_original(app, kept)?;
        (applied.op.body().generation == app.own_identity().generation()).then_some(applied)
    }

    fn verify_original(app: &App, kept: &KeptOp) -> Option<Self> {
        let own = app.identity();
        let cert = verify_certificate(&kept.cert, &own).ok()?;
        let op = SignedOp::verify_with_cert(&kept.op, &cert, &own).ok()?;
        let key = app.own_identity().service_key_of(op.body().generation)?;
        let proof = TransitionProof::verify(kept.proof.as_ref()?, &key, &own).ok()?;
        proof.check_binds(&op).ok()?;
        let at = proof.envelope().server_time;
        if !certificate_in_force_at_write(app.own_identity(), &cert, at) {
            return None;
        }
        Some(Self { op, proof, cert })
    }

    /// Verify the original authorization, named target, signed value and resulting state.
    fn open_record(&self, app: &App, target: &RecordTarget) -> Option<RecordValue> {
        let body = self.op.body();
        let OpKind::RecordPut(put) = &body.kind else {
            return None;
        };
        check_shape(body).ok()?;
        check_permission(&body.kind, Writer::App(&self.cert)).ok()?;
        if put.key != app.record_key(target).ok()?
            || put.collection != collection_id(&target.collection.name, &app.identity())
            || put.policy != target.collection.policy
        {
            return None;
        }
        let sealed = &put.value_sealed;
        let key = app.scope_key(sealed.data_epoch, &sealed.scope)?;
        let value = sealed.open(&key, &app.identity(), &put.key).ok()?;
        let deleted = self.proof.body().state.get("deleted")?.as_bool()?;
        (deleted == matches!(value, RecordValue::Delete)).then_some(value)
    }
}

/// The pending condition must select this original write before acceptance. An upload
/// changes its installation position and retains the original item hash (Switch wire 6.3).
fn pins_selected_proposal(
    pending: &[u8; 32],
    original: ApplicationRef,
    condition: &Condition,
    first: RecordVersion,
) -> bool {
    let selected = RecordVersion::new(original.generation, original.seq);
    match condition {
        Condition::RecordCurrent {
            key,
            revision,
            deleted: false,
        } => {
            let installed = RecordVersion::new(revision.generation, revision.seq);
            key == pending
                && revision.item_hash == original.item_hash
                && selected <= installed
                && installed < first
        }
        Condition::RecordVersion { key, version } => {
            key == pending && *version == selected && selected < first
        }
        _ => false,
    }
}

/// One ordinary application in the current service history. The signed operation names
/// its predecessor; the service proof binds its version, time and item hash.
#[derive(Clone)]
pub(super) struct AppliedRecord {
    pub(super) op: SignedOp,
    pub(super) cert: AppCert,
    pub(super) revision: ApplicationRef,
    pub(super) expected: RecordVersion,
    pub(super) at_ms: u64,
    pub(super) value: Vec<u8>,
}

impl AppliedRecord {
    pub(super) fn retained(app: &App, target: &RecordTarget, kept: &KeptOp) -> Option<Self> {
        Self::from_operation(app, target, AppliedOperation::verify(app, kept)?)
    }

    /// A selected proposal may retain its original signed write after a cooperative move.
    pub(super) fn selected_proposal(
        app: &App,
        target: &RecordTarget,
        kept: &KeptOp,
    ) -> Option<Self> {
        let applied = AppliedOperation::verify_original(app, kept)?;
        let current = app.own_identity();
        if !super::in_grant_history(
            applied.op.body().generation,
            current.generation(),
            current.body().recovery_from,
        ) {
            return None;
        }
        Self::from_operation(app, target, applied)
    }

    fn from_operation(app: &App, target: &RecordTarget, applied: AppliedOperation) -> Option<Self> {
        let RecordValue::Json(value) = applied.open_record(app, target)? else {
            return None;
        };
        let revision = applied.proof.record_revision().ok()?;
        let at_ms = applied.proof.envelope().server_time;
        let OpKind::RecordPut(put) = &applied.op.body().kind else {
            return None;
        };
        let expected = put.expected_version;
        Some(Self {
            op: applied.op,
            cert: applied.cert,
            revision,
            expected,
            at_ms,
            value,
        })
    }

    pub(super) fn current(
        app: &App,
        target: &RecordTarget,
        fresh: &SharedRecord,
        kept: &KeptOp,
    ) -> Option<Self> {
        if !fresh.authenticated
            || fresh.doubtful
            || fresh.flagged
            || fresh.not_taken
            || fresh.different_write_at_held_version
            || fresh.divergent
            || fresh.collection != target.collection.name
            || fresh.policy != target.collection.policy
            || fresh.key != app.record_key(target).ok()?
        {
            return None;
        }
        let RecordContent::Value(bytes) = &fresh.content else {
            return None;
        };
        let applied = Self::retained(app, target, kept)?;
        if applied.version() != fresh.version
            || Some(applied.at_ms) != fresh.written_at_ms
            || applied.op.body().app_id != Some(fresh.writer)
            || &applied.value != bytes
        {
            return None;
        }
        Some(applied)
    }

    pub(super) fn created_together(&self, ledger: &Self) -> bool {
        let batch = self.op.body().batch;
        self.expected == RecordVersion::NONE
            && ledger.expected == RecordVersion::NONE
            && self.cert.holds_scope("paykit_allowance")
            && self.cert.hash() == ledger.cert.hash()
            && self.revision.generation == ledger.revision.generation
            && self.at_ms == ledger.at_ms
            && self.op.body().app_id == ledger.op.body().app_id
            && batch == ledger.op.body().batch
            && batch.count == 3
            && self.revision.seq.checked_add(1) == Some(ledger.revision.seq)
    }

    /// The signed deletion is the third member of the accepting app's batch. Its
    /// predecessor must be the signed proposal placement selected by that app.
    pub(super) fn original_deletion(
        &self,
        app: &App,
        pending: &RecordTarget,
        selected: &Self,
        selected_condition: &Condition,
        kept: &KeptOp,
    ) -> Option<ApplicationRef> {
        let deleted = AppliedOperation::verify(app, kept)?;
        let RecordValue::Delete = deleted.open_record(app, pending)? else {
            return None;
        };
        let body = deleted.op.body();
        let OpKind::RecordPut(put) = &body.kind else {
            return None;
        };
        let reference = deleted.proof.record_revision().ok()?;
        let first = RecordVersion::new(self.revision.generation, self.revision.seq.checked_sub(1)?);
        (pins_selected_proposal(&put.key, selected.revision, selected_condition, first)
            && body.conditions.contains(selected_condition)
            && put.expected_version == selected.version()
            && deleted.cert.hash() == self.cert.hash()
            && deleted.cert.app_id() == app.app_id()
            && self.op.body().batch == body.batch
            && self.revision.generation == reference.generation
            && self.revision.seq.checked_add(1) == Some(reference.seq)
            && self.at_ms == deleted.proof.envelope().server_time)
            .then_some(reference)
    }

    /// The writer pinned the preceding record application, including its operation hash.
    pub(super) fn follows(&self, previous: &Self) -> bool {
        let OpKind::RecordPut(put) = &self.op.body().kind else {
            return false;
        };
        self.expected == previous.version()
            && self
                .op
                .body()
                .conditions
                .contains(&Condition::RecordCurrent {
                    key: put.key,
                    revision: previous.revision,
                    deleted: false,
                })
    }

    pub(super) fn version(&self) -> RecordVersion {
        RecordVersion {
            generation: self.revision.generation,
            version: self.revision.seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uploaded_proposal_condition_retains_the_original_item_hash() {
        let key = [1; 32];
        let original = ApplicationRef {
            generation: 1,
            seq: 5,
            item_hash: [2; 32],
        };
        let first = RecordVersion::new(2, 9);
        let installed = |seq, item_hash| Condition::RecordCurrent {
            key,
            revision: ApplicationRef {
                generation: 2,
                seq,
                item_hash,
            },
            deleted: false,
        };
        assert!(pins_selected_proposal(
            &key,
            original,
            &installed(7, original.item_hash),
            first
        ));
        assert!(!pins_selected_proposal(
            &key,
            original,
            &installed(7, [3; 32]),
            first
        ));
        assert!(!pins_selected_proposal(
            &key,
            original,
            &installed(9, original.item_hash),
            first
        ));
    }
}
