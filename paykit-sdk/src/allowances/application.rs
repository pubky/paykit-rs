//! Match a prepared Allowance reply against the original applied operation batch.

use crate::error::{PaykitError, Result};
use paykit_lib::collections::Collection;
use pubky_switch_client::app::{App, KeptOp};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::enc::hash;
use pubky_switch_core::ops::RecordVersion;
use pubky_switch_core::ops::{BatchRef, Condition, OpBody, OpKind, SignedOp};
use pubky_switch_core::records::IdentityDoc;
use pubky_switch_core::sealed::SealedValue;
use std::collections::BTreeMap;

pub(super) type RecordWrite = (Vec<Condition>, OpKind);

/// The client picks the random batch ID during submission. Commit to every other
/// member of each operation, including the random seals and application conditions.
pub(super) fn prepared_member(
    own: [u8; 32],
    app_id: [u8; 16],
    generation: u64,
    count: u64,
    conditions: &[Condition],
    kind: &OpKind,
) -> Result<[u8; 32]> {
    // The native client can re-sign a single kept deletion across generations.
    // Its selected source and every signed guard remain exact. The original
    // acceptance and its ordered writes belong to their approved generation.
    let generation = match kind {
        OpKind::RecordPut(put) if count == 1 && put.value_sealed.looks_like_delete() => {
            put.expected_version.generation
        }
        _ => generation,
    };
    let body = OpBody {
        identity: own,
        app_id: Some(app_id),
        generation,
        batch: BatchRef { id: [0; 16], count },
        conditions: conditions.to_vec(),
        kind: kind.clone(),
    };
    Ok(hash(&body.to_json().map_err(|_| PaykitError::corrupt())?))
}

pub(super) fn prepared_application(
    own: [u8; 32],
    app_id: [u8; 16],
    generation: u64,
    operations: &[RecordWrite],
) -> Result<[u8; 32]> {
    let count = operations.len() as u64;
    let mut bytes = Vec::with_capacity(32 * operations.len());
    for (conditions, kind) in operations {
        bytes.extend(prepared_member(
            own, app_id, generation, count, conditions, kind,
        )?);
    }
    Ok(hash(&bytes))
}

pub(super) fn seals(operations: &[RecordWrite]) -> Result<Vec<SealedValue>> {
    operations
        .iter()
        .map(|(_, op)| {
            let OpKind::RecordPut(put) = op else {
                return Err(PaykitError::corrupt());
            };
            Ok(put.value_sealed.clone())
        })
        .collect()
}

/// Rebuild the checked write with its original random sealing. Every visible input and
/// the opened value must still match, including the selected pending application.
pub(super) fn restore_seals(
    app: &App,
    operations: &mut [RecordWrite],
    saved: &[SealedValue],
    commitment: [u8; 32],
) -> Result<()> {
    let held =
        || crate::error::Refusal::NotNow("the original Allowance write is not current".into());
    if operations.len() != saved.len() {
        return Err(held().into());
    }
    for ((_, op), seal) in operations.iter_mut().zip(saved) {
        let OpKind::RecordPut(put) = op else {
            return Err(PaykitError::corrupt());
        };
        if seal.data_epoch != put.value_sealed.data_epoch || seal.scope != put.policy.read_scope {
            return Err(held().into());
        }
        let scope = app
            .scope_key(seal.data_epoch, &seal.scope)
            .ok_or_else(held)?;
        let open = |value: &SealedValue| {
            value
                .open(&scope, &app.identity(), &put.key)
                .map_err(|_| PaykitError::corrupt())
        };
        if open(seal)? != open(&put.value_sealed)? {
            return Err(held().into());
        }
        put.value_sealed = seal.clone();
    }
    if prepared_application(
        app.identity(),
        app.app_id(),
        app.own_identity().generation(),
        operations,
    )? != commitment
    {
        return Err(held().into());
    }
    Ok(())
}

/// Keep the original operations, certificates and signed service proofs of complete,
/// ordered batches. A missing or superseded member cannot prove the whole application.
pub(super) struct ConfirmedReply {
    pub(super) members: Vec<KeptOp>,
    pub(super) pending_key: [u8; 32],
    pub(super) retirement: RecordVersion,
}

#[derive(Default)]
pub(super) struct AppliedApplications {
    pub(super) replies: BTreeMap<([u8; 32], [u8; 32]), ConfirmedReply>,
    pub(super) pending_creates: BTreeMap<([u8; 32], [u8; 32]), RecordVersion>,
}

pub(super) fn applied_applications(
    held: Vec<KeptOp>,
    doc: &IdentityDoc,
    app_id: [u8; 16],
) -> Result<AppliedApplications> {
    let own = doc.identity();
    let mut applied = AppliedApplications::default();
    let mut batches = BTreeMap::new();
    for member in held {
        let Some(signed_proof) = &member.proof else {
            continue;
        };
        let Ok(cert) = paykit_lib::scopes::verify_certificate(&member.cert, &own) else {
            continue;
        };
        if cert.app_id() != app_id {
            continue;
        }
        let Ok(op) = SignedOp::verify(&member.op, &cert.signing_key(), &own, Some(&app_id)) else {
            continue;
        };
        let body = op.body();
        let OpKind::RecordPut(put) = &body.kind else {
            continue;
        };
        // Single pending writes can have applied before an orderly change. The
        // original acceptance remains in the generation the payer approved.
        let Some(service) = doc.service_key_of(body.generation) else {
            continue;
        };
        if !(1..=3).contains(&body.batch.count)
            || (body.batch.count > 1 && body.generation != doc.generation())
            || !put
                .policy
                .write_scopes
                .iter()
                .any(|scope| cert.holds_scope(scope))
        {
            continue;
        }
        let Ok(proof) = TransitionProof::verify(signed_proof, &service, &own) else {
            continue;
        };
        let env = proof.envelope();
        if proof.check_binds(&op).is_err()
            || !paykit_lib::scopes::certificate_in_force_at_write(doc, &cert, env.server_time)
        {
            continue;
        }
        let commitment = prepared_member(
            own,
            app_id,
            body.generation,
            body.batch.count,
            &body.conditions,
            &body.kind,
        )?;
        let Ok(revision) = proof.record_revision() else {
            continue;
        };
        let version = RecordVersion::new(revision.generation, revision.seq);
        let pending = put.policy == Collection::AllowanceProposals.fixed_policy();
        let retirement =
            (pending && put.value_sealed.looks_like_delete()).then_some((put.key, version));
        if body.batch.count == 1
            && pending
            && put.expected_version == RecordVersion::NONE
            && !put.value_sealed.looks_like_delete()
        {
            applied
                .pending_creates
                .insert((put.key, commitment), version);
        }
        batches
            .entry((
                body.generation,
                cert.hash(),
                cert.signing_key(),
                body.batch.id,
                body.batch.count,
            ))
            .or_insert_with(BTreeMap::new)
            .insert(env.seq, (commitment, member, retirement));
    }
    for ((_, _, signing_key, _, count), members) in batches {
        if members.len() != count as usize {
            continue;
        }
        let Some((&first, _)) = members.first_key_value() else {
            continue;
        };
        if !members
            .keys()
            .enumerate()
            .all(|(i, seq)| first.checked_add(i as u64) == Some(*seq))
        {
            continue;
        }
        let Some((pending_key, retirement)) =
            members.last_key_value().and_then(|(_, entry)| entry.2)
        else {
            continue;
        };
        let mut bytes = Vec::with_capacity(32 * members.len());
        let mut kept = Vec::with_capacity(members.len());
        for (digest, member, _) in members.into_values() {
            bytes.extend(digest);
            kept.push(member);
        }
        applied.replies.insert(
            (signing_key, hash(&bytes)),
            ConfirmedReply {
                members: kept,
                pending_key,
                retirement,
            },
        );
    }
    Ok(applied)
}
