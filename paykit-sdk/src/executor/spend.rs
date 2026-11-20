//! Reserve a one-time payment under the current executor grant (Paykit spec 8.3).

use paykit_lib::allowances::{check_limits, EntryOutcome, Ledger, LedgerEntry};
use paykit_lib::kinds::Kind;
use paykit_lib::records::ActivityRecord;
use paykit_lib::{AllowanceId, PaymentEndpointIdentifier};
use pubky_switch_client::app::OpBatch;
use pubky_switch_core::claims::ClaimView;
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::ops::Condition;
use pubky_switch_core::shared::ApplicationRef;
use serde::{Deserialize, Serialize};

use super::checkpoint::ledger_target;
use super::ledger::reserved_ledger;
use super::restore::{LedgerRead, ReconciledLedger};
use super::ExecutorHistory;
use crate::allowances::{
    allowance_target, bind_at, named_allowance, Consent, ConsentEnd, ConsentEntry,
};
use crate::journal::JournalEntry;
use crate::marks::{DropMark, MarkFamily};
use crate::pay::Dispatch;
use crate::records::KeptMessage;
use crate::state::key;
use crate::stored::BoundNote;
use crate::{AllowanceRole, AllowanceStatus, Paykit, Refusal, Result};

/// The original grant the atomic opening claim and ledger write were conditioned on.
/// The permanent key is derived from the entry's payee and this exact Allowance ID.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalAllowance {
    pub(crate) id: String,
    generation: u64,
    seq: u64,
    #[serde(with = "serde_b64")]
    item_hash: [u8; 32],
}

impl JournalAllowance {
    pub(crate) fn id(&self) -> Result<AllowanceId> {
        Ok(AllowanceId::parse(&self.id)?)
    }

    pub(crate) fn valid(&self) -> bool {
        let r = self.revision();
        self.id().is_ok()
            && r.seq > 0
            && r.generation > 0
            && r.seq <= pubky_switch_core::json::MAX_INT
            && r.generation <= pubky_switch_core::json::MAX_INT
    }

    pub(super) fn revision(&self) -> ApplicationRef {
        ApplicationRef {
            generation: self.generation,
            seq: self.seq,
            item_hash: self.item_hash,
        }
    }
    pub(super) fn new(id: &AllowanceId, revision: ApplicationRef) -> Self {
        Self {
            id: id.as_str().into(),
            generation: revision.generation,
            seq: revision.seq,
            item_hash: revision.item_hash,
        }
    }
}

pub(crate) struct AllowanceReservation {
    key: [u8; 32],
    pub(crate) id: AllowanceId,
    checked: ReconciledLedger,
}

fn held(why: &str) -> crate::PaykitError {
    Refusal::NotNow(why.into()).into()
}

/// Entries and compacted failures retain the same obligation and attempt identity.
pub(super) fn accounts_for_attempt(
    ledger: &Ledger,
    obligation: [u8; 32],
    attempt: [u8; 16],
) -> bool {
    ledger
        .entries
        .iter()
        .any(|e| e.obligation == obligation && e.attempt_id.to_bytes() == attempt)
        || ledger
            .failed
            .iter()
            .any(|e| e.obligation == obligation && e.attempt_id.to_bytes() == attempt)
}

impl AllowanceReservation {
    pub(crate) fn request(
        &self,
        record: &ActivityRecord,
        claim: Option<&ClaimView>,
        period_start: Option<u64>,
    ) -> Result<()> {
        let (_, request) = crate::requests::request_of(record).ok_or(Refusal::NotFound)?;
        if (request.conversion.is_some() && !request.is_recurring())
            || request.amount.asset != self.checked.terms.asset
            || record.manual_only
            || period_start
                .map_or(record.allowance_id.as_ref(), |start| {
                    record.allowance_for(start)
                })
                .is_some_and(|id| id != &self.id)
        {
            return Err(held("the request does not qualify under this Allowance"));
        }
        if let Some(claim) = claim {
            let ledger = &self.checked.ledger;
            if claim.tombstone
                || !claim.attempts.iter().all(|attempt| {
                    accounts_for_attempt(ledger, claim.claim_key, attempt.attempt_id)
                })
            {
                return Err(held(
                    "an earlier attempt is not accounted for under this Allowance",
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn subscription(
        &self,
        app: &mut Paykit,
        request: &crate::RequestRef,
        record: &ActivityRecord,
    ) -> Result<()> {
        self.request(record, None, None)?;
        let (stored, body) = crate::requests::request_of(record).ok_or(Refusal::NotFound)?;
        let terms = &self.checked.terms;
        if terms
            .active_from
            .as_ref()
            .is_some_and(|t| self.checked.at < t.ms())
            || terms
                .expires_at
                .as_ref()
                .is_some_and(|t| self.checked.at >= t.ms())
            || terms
                .per_payment_amount
                .as_ref()
                .is_some_and(|r| body.amount.value < r.minimum || body.amount.value > r.maximum)
        {
            return Err(held("the subscription is outside the Allowance's terms"));
        }
        if let Some(ids) = self.endpoints() {
            let pricing = crate::pay::Pricing::of_request(
                &body.amount,
                *request,
                None,
                body.conversion.as_ref(),
                pubky_switch_client::app::Initiator::Automatic,
            )
            .under_allowance();
            Box::pin(app.request_destination(
                &request.payee,
                stored,
                body,
                &crate::PayOptions::default(),
                Some(ids),
                &pricing,
                &mut None,
                &mut Vec::new(),
            ))
            .await?;
        }
        Ok(())
    }

    pub(crate) fn active_window(&self) -> Option<Condition> {
        let terms = &self.checked.terms;
        let start = terms.active_from.as_ref().map(|t| t.ms());
        let end = terms.expires_at.as_ref().map(|t| t.ms());
        (start.is_some() || end.is_some()).then_some(Condition::ServerTimeIn { start, end })
    }

    pub(crate) fn endpoints(&self) -> Option<&[PaymentEndpointIdentifier]> {
        self.checked
            .terms
            .allowed_payment_endpoint_identifiers
            .as_deref()
    }

    pub(crate) fn grant(&self) -> Condition {
        self.checked.grant
    }

    pub(crate) async fn reserve(
        self,
        app: &mut Paykit,
        entry: &mut JournalEntry,
        batch: OpBatch,
    ) -> Result<OpBatch> {
        let mut checked = self.checked;
        let amount = entry
            .paid_amount()?
            .ok_or_else(|| held("the transfer amount is unknown"))?;
        let endpoint = entry.destination.endpoint_id()?;
        checked
            .terms
            .check_payment(&amount, &endpoint, checked.at)
            .map_err(|_| held("the transfer does not satisfy the Allowance terms"))?;
        check_limits(&checked.terms, &checked.usage, &amount.value)
            .map_err(|limit| held(&format!("the {} limit has no room", limit.as_str())))?;
        checked.ledger = reserved_ledger(
            &checked.terms,
            checked.ledger,
            LedgerEntry {
                obligation: entry.obligation,
                attempt_id: entry.attempt()?,
                value: amount.value,
                asset: amount.asset,
                at: checked.at,
                outcome: None,
            },
        )?;
        let Condition::RecordCurrent {
            key,
            revision,
            deleted: false,
        } = checked.grant
        else {
            return Err(crate::PaykitError::corrupt());
        };
        if key != self.key {
            return Err(crate::PaykitError::corrupt());
        }
        entry.allowance = Some(JournalAllowance::new(&self.id, revision));
        let mut end = checked.terms.entry_window_end(checked.at)?;
        if let Some(expiry) = &checked.terms.expires_at {
            end = end.min(expiry.ms());
        }
        let window = Condition::ServerTimeIn {
            start: Some(checked.at),
            end: Some(end),
        };
        checked.stage(app, batch, vec![window]).await
    }
}

impl Paykit {
    /// An incorporated reply supplies the exact application of this app's own
    /// prepared write. Retain it before the first dispatch or return from the outcome.
    pub(crate) async fn keep_executor_reply(
        &mut self,
        entry: &JournalEntry,
        proofs: &[pubky_switch_core::chain::TransitionProof],
    ) -> Result<bool> {
        let Some(under) = &entry.allowance else {
            return Ok(true);
        };
        let id = under.id()?;
        let target = ledger_target(&entry.payee, &id);
        let name = pubky_switch_core::enc::b64(&self.app.record_key(&target)?);
        let Some(revision) = proofs
            .iter()
            .find(|p| p.body().object == name)
            .and_then(|p| p.record_revision().ok())
        else {
            return Ok(false);
        };
        let key = self.app.record_key(&allowance_target(&entry.payee, &id))?;
        Ok(self
            .executor_base(&key, LedgerRead::Applied(revision))
            .await?
            .is_some())
    }

    /// Recover the original Allowance of an own paid attempt from its accepted
    /// reservation and authenticated claim before signing its payment proof.
    pub(crate) async fn restored_payment_allowance(
        &mut self,
        payee: &[u8; 32],
        id: &AllowanceId,
        obligation: &[u8; 32],
        attempt: paykit_lib::AttemptId,
    ) -> Result<Option<JournalAllowance>> {
        let key = self.app.record_key(&allowance_target(payee, id))?;
        let Some(checked) = self.executor_base(&key, LedgerRead::Inventory).await? else {
            return Ok(None);
        };
        let Some(entry) = checked
            .ledger
            .entries
            .iter()
            .find(|e| e.attempt_id == attempt && &e.obligation == obligation)
        else {
            return Ok(None);
        };
        let Some(read) = self.app.read_claim(obligation).await? else {
            return Ok(None);
        };
        let Some(application) = super::evidence::AppliedRecord::retained(
            &self.app,
            &ledger_target(payee, id),
            &checked.evidence,
        ) else {
            return Ok(None);
        };
        let evidence = self.app.shared_evidence().await?;
        if !evidence.ops.iter().any(|op| {
            application
                .accepted_opening(&self.app, op, entry, &read, &checked.grant)
                .is_some()
        }) {
            return Ok(None);
        }
        let Condition::RecordCurrent { revision, .. } = checked.grant else {
            return Ok(None);
        };
        Ok(Some(JournalAllowance::new(id, revision)))
    }

    /// A verified current base may record an outcome together with the claim. An
    /// unverifiable base grants no ledger write (Paykit spec 8.3).
    pub(crate) async fn executor_outcome(
        &mut self,
        entry: &JournalEntry,
        paid: bool,
        batch: OpBatch,
    ) -> Result<OpBatch> {
        let Some(under) = &entry.allowance else {
            return Ok(batch);
        };
        let id = under.id()?;
        let key = self.app.record_key(&allowance_target(&entry.payee, &id))?;
        let Some(mut checked) = Box::pin(self.executor_base(&key, LedgerRead::Outcome)).await?
        else {
            return Ok(batch);
        };
        if !matches!(checked.grant, Condition::RecordCurrent { revision, .. } if revision == under.revision())
        {
            return Ok(batch);
        }
        let attempt = entry.attempt()?;
        let Some(reserved) = checked
            .ledger
            .entries
            .iter_mut()
            .find(|e| e.attempt_id == attempt && e.obligation == entry.obligation)
        else {
            return Ok(batch);
        };
        let outcome = if paid {
            EntryOutcome::Done
        } else {
            EntryOutcome::Failed
        };
        if reserved.outcome == Some(EntryOutcome::Done) || reserved.outcome == Some(outcome) {
            return Ok(batch);
        }
        reserved.outcome = Some(outcome);
        checked.stage(self, batch, Vec::new()).await
    }

    pub(crate) async fn allowance_reservation(
        &mut self,
        record: &[u8; 32],
        payee: &[u8; 32],
    ) -> Result<AllowanceReservation> {
        let stopped =
            || held("the accepted executor grant, ledger or wallet history is unavailable");
        let view = self.allowance(record).await?;
        if !self.holds("paykit_allowance_execute")
            || view.counterparty != *payee
            || view.role != Some(AllowanceRole::Allower)
            || view.status != AllowanceStatus::Accepted
            || view.executor_app_id != Some(self.app_id())
            || !view.consented
            || view.blocked != Some(false)
            || self.pause_at(record).await?.blocks_automatic()
            || self
                .executor_replies_pending(record, payee, &view.allowance_id, false)
                .await?
        {
            return Err(stopped());
        }
        let target = ledger_target(payee, &view.allowance_id);
        if self
            .pause_at(&self.app.record_key(&target)?)
            .await?
            .blocks_automatic()
        {
            return Err(stopped());
        }
        let checked = self
            .executor_base(record, LedgerRead::Inventory)
            .await?
            .ok_or_else(stopped)?;
        Ok(AllowanceReservation {
            key: *record,
            id: view.allowance_id,
            checked,
        })
    }

    pub(crate) async fn executor_dispatch_held(
        &mut self,
        entry: &JournalEntry,
        kind: Dispatch,
    ) -> Result<bool> {
        let Some(under) = &entry.allowance else {
            return Ok(false);
        };
        if !self.holds("paykit_allowance_execute") {
            return Ok(true);
        }
        let id = under.id()?;
        let target = allowance_target(&entry.payee, &id);
        let key = self.app.record_key(&target)?;
        let ledger_key = self.app.record_key(&ledger_target(&entry.payee, &id))?;
        let Ok((checked, _)) = self.current_executor_record(&target).await else {
            return Ok(true);
        };
        let repeat = matches!(kind, Dispatch::Repeat { .. });
        if repeat {
            let Some(record) = bind_at(&checked.value, &self.identity(), &entry.payee, &id) else {
                return Ok(true);
            };
            let Some(history) = self
                .local_get::<ExecutorHistory>(&key::executor_checkpoint(&key))
                .await?
            else {
                return Ok(true);
            };
            let Some(opening) = Box::pin(self.approved_opening(&key)).await? else {
                return Ok(true);
            };
            let Some(original) = history
                .selected(&self.app, &key, &opening)
                .and_then(|c| c.accepted(&self.app, &key, &opening))
            else {
                return Ok(true);
            };
            let consent = self.local_get::<ConsentEntry>(&key::consent(&key)).await?;
            if original.grant.revision != under.revision()
                || checked.revision.generation != under.revision().generation
                || (checked.revision != under.revision()
                    && checked.revision.seq <= under.revision().seq)
                || original.record.check_later_write(&record).is_err()
                || record.end.is_some()
                || !matches!(
                    record.response.as_ref().map(|m| m.body().kind()),
                    Some(Kind::AllowanceAcceptance)
                )
                || self.consents_unconfirmed().await?
                || !matches!(
                    consent.map(|e| e.consent),
                    Some(
                        Consent::Held {
                            confirm_again: false
                        } | Consent::Removed {
                            reason: ConsentEnd::HandedOff | ConsentEnd::Conflicted
                        }
                    )
                )
                || self.allowance_stopped(&key).await?
            {
                return Ok(true);
            }
        } else if checked.revision != under.revision() || !self.consents(&key).await? {
            return Ok(true);
        }
        Ok(self.pause_at(&key).await?.blocks_automatic()
            || self.pause_at(&ledger_key).await?.blocks_automatic()
            || self
                .executor_replies_pending(&key, &entry.payee, &id, repeat)
                .await?)
    }

    async fn executor_replies_pending(
        &mut self,
        record: &[u8; 32],
        peer: &[u8; 32],
        id: &AllowanceId,
        repeat: bool,
    ) -> Result<bool> {
        if self
            .local_get::<DropMark>(&MarkFamily::Replies.key(peer))
            .await?
            .is_some()
        {
            return Ok(true);
        }
        let holds =
            |kind: Kind| !repeat || matches!(kind, Kind::AllowanceEnd | Kind::AllowanceRejection);
        for (_, value) in self
            .local_scan::<KeptMessage>(&key::kept_reply(peer, record, ""))
            .await?
        {
            if holds(value.stored(&self.identity())?.body().kind()) {
                return Ok(true);
            }
        }
        let notes = key::stored_notes_of(peer);
        for (_, BoundNote { note, message }) in self.local_scan(&notes).await? {
            if !note.placed
                && named_allowance(message.body()) == Some(id)
                && holds(message.body().kind())
            {
                return Ok(true);
            }
        }
        for info in self.app.parked_entries().await? {
            if !info.readable {
                return Ok(true);
            }
            let Some(parked) = self.app.parked_entry(info.stored_id).await? else {
                continue;
            };
            if parked.sender == *peer
                && parked.message.as_ref().is_none_or(|m| {
                    [
                        Kind::AllowanceProposal,
                        Kind::AllowanceAcceptance,
                        Kind::AllowanceRejection,
                        Kind::AllowanceEnd,
                    ]
                    .into_iter()
                    .any(|kind| m.kind == kind.as_str() && holds(kind))
                })
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
