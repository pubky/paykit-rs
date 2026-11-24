//! The original application and opening claim of a reservation (Paykit spec 8.3).

use paykit_lib::allowances::{ClaimState, EntryOutcome, LedgerEntry};
use paykit_lib::scopes::{certificate_in_force_at_write, verify_certificate};
use paykit_lib::AttemptId;
use pubky_switch_client::app::{App, KeptOp, RecordContent, RecordTarget, SharedRecord};
use pubky_switch_core::chain::{check_attempt_proof, TransitionProof};
use pubky_switch_core::claims::{ClaimStatus, ClaimType, ClaimView, Outcome};
use pubky_switch_core::ident::collection_id;
use pubky_switch_core::ops::{
    check_permission, check_shape, Condition, OpKind, RecordVersion, SignedOp, Writer,
};
use pubky_switch_core::records::AppCert;
use pubky_switch_core::sealed::RecordValue;
use pubky_switch_core::shared::ApplicationRef;
use std::collections::{HashMap, HashSet};

use super::ReservationClaim;

/// What an applied operation is verified for ([`AppliedOperation::applied`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Purpose {
    /// Evidence of what happened: of a generation in the grant's history.
    Evidence,
    /// It authorizes or dispatches a new attempt: of the current generation.
    Authorizes,
    /// Evidence already verified and retained under this installation's grant checkpoint.
    /// Once the signer drops its generation's service key (Switch spec 4.3),
    /// the retained proof can still date the write for the grant's accounting. The writer's
    /// signature, the proof's binding to the operation, the certificate in force and the
    /// grant's history are all still checked.
    Established,
}

impl Purpose {
    /// Whether an operation of `generation` serves this purpose, the account being at
    /// `current` with the last provider loss at `recovery_from` ([`AppliedOperation::applied`]).
    fn admits(self, generation: u64, current: u64, recovery_from: Option<u64>) -> bool {
        match self {
            Self::Evidence | Self::Established => {
                super::in_grant_history(generation, current, recovery_from)
            }
            Self::Authorizes => generation == current,
        }
    }
}

struct AppliedOperation {
    op: SignedOp,
    proof: TransitionProof,
    cert: AppCert,
}

impl AppliedOperation {
    /// `kept` verified for `purpose`: the one place the executor decides which generation an
    /// applied operation must be of.
    ///
    /// - **Evidence of what happened** ([`Purpose::Evidence`]): a record write, an attempt's
    ///   opening or outcome, a compaction receipt. Its transition proof keeps dating it,
    ///   verified with its own generation's key as the current `identity.json` lists it,
    ///   through an orderly generation change or a cooperative move (Pubky Switch spec,
    ///   Sections 4.3 and 12.5). It counts while that generation is in the grant's history,
    ///   after the last provider loss, which nothing carries across (Paykit spec 8.3, "After
    ///   provider loss").
    /// - **What authorizes or dispatches a new attempt** ([`Purpose::Authorizes`]): acted on
    ///   only in the current generation, so a stale instance of an earlier generation
    ///   authorizes nothing (Pubky Switch spec, Section 12.5).
    fn applied(app: &App, kept: &KeptOp, purpose: Purpose) -> Option<Self> {
        Self::verify_for(app, kept, purpose)?.fit_for(app, purpose)
    }

    /// Whether this applied operation's generation fits `purpose` ([`Self::applied`]).
    fn fit_for(self, app: &App, purpose: Purpose) -> Option<Self> {
        let current = app.own_identity();
        purpose
            .admits(
                self.op.body().generation,
                current.generation(),
                current.body().recovery_from,
            )
            .then_some(self)
    }

    fn verify_for(app: &App, kept: &KeptOp, purpose: Purpose) -> Option<Self> {
        let cert = verify_certificate(&kept.cert, &app.identity()).ok()?;
        Self::with_certificate_for(app, kept, cert, purpose)
    }

    fn with_certificate(app: &App, kept: &KeptOp, cert: AppCert) -> Option<Self> {
        Self::with_certificate_for(app, kept, cert, Purpose::Evidence)
    }

    fn with_certificate_for(
        app: &App,
        kept: &KeptOp,
        cert: AppCert,
        purpose: Purpose,
    ) -> Option<Self> {
        let own = app.identity();
        let op = SignedOp::verify_with_cert(&kept.op, &cert, &own).ok()?;
        check_shape(op.body()).ok()?;
        check_permission(&op.body().kind, Writer::App(&cert)).ok()?;
        let signed = kept.proof.as_ref()?;
        let proof = match app.own_identity().service_key_of(op.body().generation) {
            Some(key) => TransitionProof::verify(signed, &key, &own).ok()?,
            None if purpose == Purpose::Established => TransitionProof::peek(signed).ok()?,
            None => return None,
        };
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

    /// A native claim entry can carry a more recent certificate for the same key.
    /// The service-dated result must verify under a certificate eligible at that step.
    fn outcome(
        app: &App,
        kept: &KeptOp,
        certificates: &[AppCert],
        obligation: &[u8; 32],
        id: paykit_lib::AttemptId,
        expected: EntryOutcome,
    ) -> Option<Self> {
        let header = SignedOp::peek(&kept.op).ok()?;
        let body = header.body();
        let writer = body.app_id?;
        let OpKind::Claim(fields) = &body.kind else {
            return None;
        };
        let claim = fields.check().ok()?;
        let outcome = match expected {
            EntryOutcome::Done if fields.kind == ClaimType::Done => Outcome::Done,
            EntryOutcome::Failed if fields.kind == ClaimType::Failed => Outcome::Failed,
            _ => return None,
        };
        if &claim.claim_key != obligation || claim.attempt_id != id.to_bytes() {
            return None;
        }
        // An outcome is evidence of what happened to the attempt.
        let applied = Self::applied(app, kept, Purpose::Evidence).or_else(|| {
            certificates
                .iter()
                .filter(|c| c.app_id() == writer && c.signed() != &kept.cert)
                .find_map(|c| Self::with_certificate(app, kept, c.clone()))
                .and_then(|applied| applied.fit_for(app, Purpose::Evidence))
        })?;
        let proof = applied.proof.body();
        let view = ClaimView::from_value(&proof.state).ok()?;
        let attempt = view.attempt(&claim.attempt_id)?;
        (proof.step == claim.expected_step.checked_add(1)
            && proof.step == Some(view.step)
            && &view.claim_key == obligation
            && !view.rebuilt
            && !view.divergent
            && !attempt.duplicate
            && !attempt.possibly_duplicate
            && attempt.owner == app.app_id()
            && attempt.resolved_by == Some(writer)
            && attempt.outcome == outcome
            && (outcome != Outcome::Done || view.status == ClaimStatus::Settled))
            .then_some(applied)
    }
}

/// Certificates and terminal entries available after a fresh read of the relevant claims.
pub(super) struct ClaimResults<'a> {
    app: &'a App,
    certificates: Vec<AppCert>,
    entries: HashMap<AttemptId, Vec<&'a KeptOp>>,
}

impl<'a> ClaimResults<'a> {
    pub(super) fn new(
        app: &'a App,
        ops: &'a [KeptOp],
        signers: &super::checkpoint::ReservationSigners,
        accepted: &super::checkpoint::AcceptedLedger,
        current: &AppliedRecord,
    ) -> Self {
        let mut distinct = HashSet::new();
        let certificates = signers
            .certificates()
            .chain([
                accepted.grant.cert.signed(),
                accepted.application.cert.signed(),
                current.cert.signed(),
            ])
            .chain(ops.iter().map(|op| &op.cert))
            .filter(|doc| distinct.insert(*doc))
            .filter_map(|doc| verify_certificate(doc, &app.identity()).ok())
            .collect();
        let mut entries = HashMap::<_, Vec<_>>::new();
        for op in ops {
            let Ok(header) = SignedOp::peek(&op.op) else {
                continue;
            };
            let OpKind::Claim(fields) = &header.body().kind else {
                continue;
            };
            if !matches!(fields.kind, ClaimType::Done | ClaimType::Failed) {
                continue;
            }
            if let Ok(claim) = fields.check() {
                entries
                    .entry(AttemptId::from_bytes(claim.attempt_id))
                    .or_default()
                    .push(op);
            }
        }
        Self {
            app,
            certificates,
            entries,
        }
    }

    pub(super) fn confirms(
        &self,
        id: paykit_lib::AttemptId,
        claim: ReservationClaim,
        opened: u64,
        before: Option<&AppliedRecord>,
    ) -> bool {
        let expected = match claim.state {
            ClaimState::Open | ClaimState::Unknown => return true,
            ClaimState::Done => EntryOutcome::Done,
            ClaimState::Failed { .. } => EntryOutcome::Failed,
        };
        self.entries.get(&id).is_some_and(|entries| {
            entries.iter().any(|op| {
                let Some(result) = AppliedOperation::outcome(
                    self.app,
                    op,
                    &self.certificates,
                    &claim.obligation,
                    id,
                    expected,
                ) else {
                    return false;
                };
                let proof = result.proof.envelope();
                let owner = result.cert.app_id() == self.app.app_id();
                !matches!(claim.state, ClaimState::Failed { by_owner } if owner != by_owner)
                    && opened < proof.seq
                    // The sequence continues across an orderly generation change.
                    && before.is_none_or(|copy| {
                        proof.generation <= copy.revision.generation
                            && proof.seq < copy.revision.seq
                            && proof.server_time <= copy.at_ms
                    })
            })
        })
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

/// A write of this app's own, retained with its proof and verified as evidence of its
/// generation ([`AppliedOperation::applied`] with [`Purpose::Evidence`]): the proof verifies
/// with the service key the current `identity.json` lists for that generation, and the
/// generation is in the grant's history, after the last provider loss.
///
/// After an orderly generation change, the Switch client authenticates the version of a
/// record read with no copy held (after a restore, or a move that dropped proofs) only for
/// a proof of the current generation (Pubky Switch spec, Section 12.5). This app's own
/// retained proof of an earlier generation still dates and places its own write: a read
/// naming exactly its version, time and writer shows that write. It authenticates no other
/// write, and no other app's.
#[derive(Clone, Copy)]
pub(crate) struct OwnWrite {
    version: RecordVersion,
    at_ms: u64,
    writer: [u8; 16],
}

impl OwnWrite {
    /// The write at `version` and `at_ms` by `writer`, when that is `own`, this app: another
    /// app's write is never vouched for by this app's evidence.
    pub(crate) fn of(
        version: RecordVersion,
        at_ms: u64,
        writer: Option<[u8; 16]>,
        own: [u8; 16],
    ) -> Option<Self> {
        let writer = writer.filter(|id| *id == own)?;
        Some(Self {
            version,
            at_ms,
            writer,
        })
    }

    /// Whether the read `held` shows this write: exactly this write's version, time and
    /// writer, whether or not the client authenticated the version.
    pub(crate) fn stands_for(&self, held: &SharedRecord) -> bool {
        held.version == self.version
            && held.written_at_ms == Some(self.at_ms)
            && held.writer == self.writer
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
    pub(super) fn opens_in_window(
        &self,
        terms: &paykit_lib::allowances::AllowanceTerms,
        entry: &LedgerEntry,
    ) -> crate::Result<bool> {
        let end = terms.entry_window_end(entry.at)?;
        Ok(entry.at <= self.at_ms
            && self.op.body().conditions.iter().any(|condition| {
                matches!(condition, Condition::ServerTimeIn { start: Some(start), end: Some(limit) }
                if *start == entry.at && *limit <= end && self.at_ms < *limit)
            }))
    }

    pub(super) fn signed_opening(
        &self,
        app: &App,
        kept: &KeptOp,
        entry: &LedgerEntry,
        grant: &Condition,
        purpose: Purpose,
    ) -> bool {
        let Some(applied) = AppliedOperation::applied(app, kept, purpose) else {
            return false;
        };
        let b = applied.op.body();
        let OpKind::Claim(fields) = &b.kind else {
            return false;
        };
        let Ok(claim) = fields.check() else {
            return false;
        };
        let proof = applied.proof.envelope();
        b.app_id == Some(app.app_id())
            && b.conditions.contains(grant)
            && b.batch == self.op.body().batch
            && proof.server_time == self.at_ms
            && proof.seq != self.revision.seq
            && proof.seq.abs_diff(self.revision.seq) < b.batch.count
            && fields.kind == ClaimType::Attempt
            && claim.claim_key == entry.obligation
            && claim.attempt_id == entry.attempt_id.to_bytes()
            && check_attempt_proof(&applied.proof, &claim, &app.app_id(), b.generation).is_ok()
    }

    pub(super) fn opening_after(
        &self,
        app: &App,
        kept: &KeptOp,
        grant: &Condition,
        obligation: &[u8; 32],
        id: paykit_lib::AttemptId,
    ) -> bool {
        let Some(applied) = AppliedOperation::applied(app, kept, Purpose::Evidence) else {
            return false;
        };
        let b = applied.op.body();
        let OpKind::Claim(fields) = &b.kind else {
            return false;
        };
        let Ok(claim) = fields.check() else {
            return false;
        };
        // The sequence continues across an orderly generation change.
        b.app_id == Some(app.app_id())
            && b.conditions.contains(grant)
            && applied.proof.envelope().seq > self.revision.seq
            && b.generation >= self.revision.generation
            && fields.kind == ClaimType::Attempt
            && &claim.claim_key == obligation
            && claim.attempt_id == id.to_bytes()
            && check_attempt_proof(&applied.proof, &claim, &app.app_id(), b.generation).is_ok()
    }

    /// As [`Self::retained`], for evidence of an established grant ([`Purpose::Established`]).
    pub(super) fn established(app: &App, target: &RecordTarget, kept: &KeptOp) -> Option<Self> {
        Self::retained_for(app, target, kept, Purpose::Established)
    }

    /// `kept` verified for `purpose`: [`Self::retained`] or [`Self::established`].
    pub(super) fn retained_for(
        app: &App,
        target: &RecordTarget,
        kept: &KeptOp,
        purpose: Purpose,
    ) -> Option<Self> {
        Self::from_operation(app, target, AppliedOperation::applied(app, kept, purpose)?)
    }

    pub(super) fn retained(app: &App, target: &RecordTarget, kept: &KeptOp) -> Option<Self> {
        Self::retained_for(app, target, kept, Purpose::Evidence)
    }

    /// A selected proposal may retain its original signed write after a cooperative move.
    pub(super) fn selected_proposal(
        app: &App,
        target: &RecordTarget,
        kept: &KeptOp,
        purpose: Purpose,
    ) -> Option<Self> {
        let applied = AppliedOperation::verify_for(app, kept, purpose)?;
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

    /// This write, as [`OwnWrite`] names it, when this app wrote it: another app's write
    /// is never vouched for by this app's evidence.
    pub(super) fn own_write(&self, app: &App) -> Option<OwnWrite> {
        OwnWrite::of(
            self.version(),
            self.at_ms,
            self.op.body().app_id,
            app.app_id(),
        )
    }

    pub(super) fn current(
        app: &App,
        target: &RecordTarget,
        fresh: &SharedRecord,
        kept: &KeptOp,
    ) -> Option<Self> {
        if fresh.doubtful
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
        let shown = fresh.authenticated
            || applied
                .own_write(app)
                .is_some_and(|own| own.stands_for(fresh));
        if !shown
            || applied.version() != fresh.version
            || Some(applied.at_ms) != fresh.written_at_ms
            || applied.op.body().app_id != Some(fresh.writer)
            || &applied.value != bytes
        {
            return None;
        }
        Some(applied)
    }

    /// The service application dates this signed payment proof independently of the
    /// clock in its message and the independent clock used by the payment method.
    pub(super) fn dates_payment_proof(
        &self,
        app: &App,
        proof: &paykit_lib::records::StoredMessage,
    ) -> bool {
        use paykit_lib::records::{ActivityRecord, ActivitySide};
        let cert = proof.cert();
        if self.at_ms < cert.issued_at()
            || cert.expired_at(self.at_ms)
            || !app
                .own_identity()
                .standing(&cert.app_id(), &cert.hash())
                .counts_at(self.at_ms)
        {
            return false;
        }
        ActivityRecord::read(&self.value, &app.identity(), ActivitySide::Payer)
            .ok()
            .and_then(|activity| activity.proof)
            .is_some_and(|seen| seen.hash() == proof.hash() && seen.cert().hash() == cert.hash())
    }

    /// Acceptance creates the permanent Allowance and its ledger in the same batch.
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
        purpose: Purpose,
    ) -> Option<(ApplicationRef, OwnWrite)> {
        let deleted = AppliedOperation::applied(app, kept, purpose)?;
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
            .then_some((
                reference,
                OwnWrite {
                    version: RecordVersion::new(reference.generation, reference.seq),
                    at_ms: self.at_ms,
                    writer: app.app_id(),
                },
            ))
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

    /// A separately retained complete ledger can pin a write after its original zero.
    /// This cannot establish the authority of a newly observed service write.
    pub(super) fn pins_write_after(&self, original: &Self) -> bool {
        let OpKind::RecordPut(put) = &self.op.body().kind else {
            return false;
        };
        // Versions order by generation, then sequence, which an orderly change continues
        // (Pubky Switch spec, Section 12.5); both writes were retained as evidence of a
        // generation in the grant's history.
        self.at_ms >= original.at_ms
            && original.version() < self.expected
            && self.expected < self.version()
            && self.op.body().conditions.iter().any(|condition| {
                matches!(condition, Condition::RecordCurrent { key, revision, deleted: false }
                    if *key == put.key
                        && RecordVersion::new(revision.generation, revision.seq) == self.expected)
            })
    }

    pub(super) fn version(&self) -> RecordVersion {
        RecordVersion {
            generation: self.revision.generation,
            version: self.revision.seq,
        }
    }

    /// A current claim read settles the claim state. The independently retained opening
    /// and its proof must name the ledger writer, generation, application batch and time.
    /// `just_made`: the reservation this app is making now, whose opening authorizes its
    /// dispatch; otherwise one that already applied, whose opening is evidence of what
    /// happened ([`AppliedOperation::applied`]).
    pub(super) fn reservation(
        &self,
        app: &App,
        kept: &KeptOp,
        entry: &LedgerEntry,
        read: &pubky_switch_client::app::ClaimRead,
        just_made: bool,
    ) -> Option<(ReservationClaim, u64)> {
        let purpose = if just_made {
            Purpose::Authorizes
        } else {
            Purpose::Evidence
        };
        self.opening_for(
            app,
            kept,
            &entry.obligation,
            entry.attempt_id,
            read,
            None,
            purpose,
        )
    }

    /// The complete ledger already accepted this reservation. Its original opening
    /// continues to establish the owner and outcome after a later ledger write.
    pub(super) fn accepted_opening(
        &self,
        app: &App,
        kept: &KeptOp,
        entry: &LedgerEntry,
        read: &pubky_switch_client::app::ClaimRead,
        grant: &Condition,
    ) -> Option<ReservationClaim> {
        self.opening(
            app,
            kept,
            &entry.obligation,
            entry.attempt_id,
            read,
            Some((grant, entry.at)),
        )
        .map(|(claim, _)| claim)
    }

    pub(super) fn opening(
        &self,
        app: &App,
        kept: &KeptOp,
        obligation: &[u8; 32],
        id: paykit_lib::AttemptId,
        read: &pubky_switch_client::app::ClaimRead,
        accepted_under: Option<(&Condition, u64)>,
    ) -> Option<(ReservationClaim, u64)> {
        // Counting an attempt that applied: its opening is evidence of what happened.
        self.opening_for(
            app,
            kept,
            obligation,
            id,
            read,
            accepted_under,
            Purpose::Evidence,
        )
    }

    /// Check the signed opening against the claim read for this reservation (Paykit spec 8.3).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn opening_for(
        &self,
        app: &App,
        kept: &KeptOp,
        obligation: &[u8; 32],
        id: paykit_lib::AttemptId,
        read: &pubky_switch_client::app::ClaimRead,
        accepted_under: Option<(&Condition, u64)>,
        purpose: Purpose,
    ) -> Option<(ReservationClaim, u64)> {
        // The native upload inventory can attach a renewed certificate with the same
        // signing key. The certificate of the accepted ledger also proves an opening
        // only when it verifies that operation and was in force at its proved time.
        let applied = AppliedOperation::applied(app, kept, purpose).or_else(|| {
            let applied =
                AppliedOperation::with_certificate_for(app, kept, self.cert.clone(), purpose)?
                    .fit_for(app, purpose)?;
            (applied.op.body().batch == self.op.body().batch
                && applied.proof.envelope().server_time == self.at_ms)
                .then_some(applied)
        })?;
        let b = applied.op.body();
        let OpKind::Claim(fields) = &b.kind else {
            return None;
        };
        let writer = self.op.body().app_id?;
        let claim = fields.check().ok()?;
        let at = applied.proof.envelope().server_time;
        let opened = read.view.attempt(&claim.attempt_id)?;
        let bound = match accepted_under {
            Some((grant, earliest)) => {
                b.conditions.contains(grant) && earliest <= at && at <= self.at_ms
            }
            None => {
                b.batch == self.op.body().batch
                    && at == self.at_ms
                    && self.revision.seq != applied.proof.envelope().seq
                    && self.revision.seq.abs_diff(applied.proof.envelope().seq) < b.batch.count
            }
        };
        if fields.kind != ClaimType::Attempt
            || b.app_id != Some(writer)
            || !bound
            || opened.opened_at != at
            || opened.owner != writer
            || &claim.claim_key != obligation
            || &read.view.claim_key != obligation
            || read.view.rebuilt
            || read.view.divergent
            || opened.duplicate
            || opened.possibly_duplicate
            || paykit_lib::AttemptId::from_bytes(claim.attempt_id) != id
        {
            return None;
        }
        check_attempt_proof(&applied.proof, &claim, &writer, b.generation).ok()?;
        let state = match opened.outcome {
            Outcome::Open => ClaimState::Open,
            Outcome::Done => ClaimState::Done,
            Outcome::Unknown => ClaimState::Unknown,
            Outcome::Failed => ClaimState::Failed {
                by_owner: opened.resolved_by == Some(writer),
            },
        };
        Some((
            ReservationClaim {
                obligation: claim.claim_key,
                state,
            },
            applied.proof.envelope().seq,
        ))
    }

    pub(super) fn applied_outcome(
        &self,
        app: &App,
        kept: &KeptOp,
        entry: &LedgerEntry,
        read: &pubky_switch_client::app::ClaimRead,
    ) -> Option<()> {
        let applied = AppliedOperation::outcome(
            app,
            kept,
            std::slice::from_ref(&self.cert),
            &entry.obligation,
            entry.attempt_id,
            entry.outcome?,
        )?;
        let b = applied.op.body();
        let writer = self.op.body().app_id?;
        let attempt = read.view.attempt(&entry.attempt_id.to_bytes())?;
        let matches = matches!(
            (entry.outcome?, attempt.outcome),
            (EntryOutcome::Done, Outcome::Done) | (EntryOutcome::Failed, Outcome::Failed)
        );
        (matches
            && read.view.claim_key == entry.obligation
            && !read.view.divergent
            && !read.view.rebuilt
            && !attempt.duplicate
            && !attempt.possibly_duplicate
            && attempt.owner == writer
            && attempt.resolved_by == Some(writer)
            && b.app_id == Some(writer)
            && b.batch == self.op.body().batch
            && applied.proof.envelope().server_time == self.at_ms
            && self.revision.seq != applied.proof.envelope().seq
            && self.revision.seq.abs_diff(applied.proof.envelope().seq) < b.batch.count)
            .then_some(())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_evidence_authorization_admits_only_the_current_generation() {
        // An opening answered in an earlier generation authorizes no new attempt, after an
        // orderly change or a move as after a loss (Pubky Switch spec, Section 12.5).
        assert!(Purpose::Authorizes.admits(3, 3, None));
        assert!(!Purpose::Authorizes.admits(2, 3, None));
        assert!(!Purpose::Authorizes.admits(2, 3, Some(2)));
        assert!(!Purpose::Authorizes.admits(4, 3, None));
    }

    #[test]
    fn test_evidence_own_write_is_only_this_apps() {
        // Another app's write, or one naming no app, is never this app's own write.
        let version = RecordVersion::new(1, 7);
        assert!(OwnWrite::of(version, 1_000, Some([3; 16]), [3; 16]).is_some());
        assert!(OwnWrite::of(version, 1_000, Some([4; 16]), [3; 16]).is_none());
        assert!(OwnWrite::of(version, 1_000, None, [3; 16]).is_none());
    }

    #[test]
    fn test_evidence_own_write_stands_only_for_its_exact_read() {
        // A read shows this app's own write only when it names exactly its version, time
        // and writer, whether the client authenticated the version or not.
        let own = OwnWrite {
            version: RecordVersion::new(1, 7),
            at_ms: 1_000,
            writer: [3; 16],
        };
        let read = |authenticated, version, written_at_ms, writer| SharedRecord {
            key: [0; 32],
            collection: String::new(),
            policy: paykit_lib::collections::Collection::AllowanceLedgers.fixed_policy(),
            version,
            authenticated,
            writer,
            written_at_ms,
            content: RecordContent::Deleted,
            doubtful: false,
            flagged: false,
            not_taken: false,
            different_write_at_held_version: false,
            divergent: false,
        };
        assert!(own.stands_for(&read(false, RecordVersion::new(1, 7), Some(1_000), [3; 16])));
        assert!(own.stands_for(&read(true, RecordVersion::new(1, 7), Some(1_000), [3; 16])));
        assert!(!own.stands_for(&read(true, RecordVersion::new(2, 9), Some(5), [4; 16])));
        for (version, at, writer) in [
            (RecordVersion::new(1, 8), Some(1_000), [3; 16]),
            (RecordVersion::new(1, 6), Some(1_000), [3; 16]),
            (RecordVersion::new(2, 7), Some(1_000), [3; 16]),
            (RecordVersion::new(1, 7), Some(1_001), [3; 16]),
            (RecordVersion::new(1, 7), Some(999), [3; 16]),
            (RecordVersion::new(1, 7), None, [3; 16]),
            (RecordVersion::new(1, 7), Some(1_000), [4; 16]),
        ] {
            assert!(!own.stands_for(&read(false, version, at, writer)));
        }
    }

    #[test]
    fn test_evidence_admits_the_grant_history_after_the_last_loss() {
        // Evidence of what happened counts from an earlier generation of the same history,
        // never from one a provider loss ended, nor from a later one (Paykit spec 8.3).
        assert!(Purpose::Evidence.admits(3, 3, None));
        assert!(Purpose::Evidence.admits(1, 3, None));
        assert!(Purpose::Evidence.admits(3, 4, Some(2)));
        assert!(!Purpose::Evidence.admits(2, 4, Some(2)));
        assert!(!Purpose::Evidence.admits(1, 4, Some(2)));
        assert!(!Purpose::Evidence.admits(5, 4, None));
    }
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
