//! Reserve a one-time payment under the current executor grant (Paykit spec 8.3).

use paykit_lib::allowances::{
    check_limits, EntryOutcome, Ledger, LedgerEntry, LimitKind, TermsRefusal,
};
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
    allowance_target, bind_at, named_allowance, Consent, ConsentEnd, ConsentEntry, ConsentState,
};
use crate::journal::JournalEntry;
use crate::marks::{DropMark, MarkFamily};
use crate::pay::Dispatch;
use crate::records::KeptMessage;
use crate::state::key;
use crate::stored::BoundNote;
use crate::{AllowanceRole, AllowanceStatus, AllowanceView, Paykit, Refusal, Result};

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

/// A refusal no later read changes for this request: it may still be paid by hand.
fn not_automatic(why: &str) -> crate::PaykitError {
    Refusal::NotAutomatic(why.into()).into()
}

/// Where an Allowance stands for a payment by this app as its executor (Paykit spec 8.3,
/// "Executor", "Ends reach the executor"): the one classification the payment paths, the
/// executor's grant paths and [`crate::AllowanceView::confirmation_needed`] follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Nothing in the Allowance's own state stops this app paying under it.
    Payable,
    /// Accepted, and this app hasn't taken up its execution yet
    /// ([`Paykit::resume_allowance_executor`]).
    NotTakenUp,
    /// The user must confirm it again, which [`Paykit::confirm_allowance_executor`] lifts.
    Confirm,
    /// A later read, answer or the user's own action may change it.
    Temporary(&'static str),
    /// Nothing later changes it.
    Final(Refusal),
}

impl Standing {
    /// The refusal a payment under the Allowance `hold` names gets, if any.
    pub(crate) fn refusal(self, hold: &crate::AllowanceHold) -> Option<Refusal> {
        match self {
            Self::Payable => None,
            Self::NotTakenUp => Some(Refusal::NotNow(
                "this app hasn't taken up the Allowance's execution".into(),
            )),
            Self::Confirm => Some(Refusal::AllowanceConfirmationNeeded { hold: hold.clone() }),
            Self::Temporary(why) => Some(Refusal::NotNow(why.into())),
            Self::Final(refusal) => Some(refusal),
        }
    }
}

/// Where the Allowance `view` shows stands for `app`, given its consent to execute it. In
/// this order: the Allowance's status, then whether this identity pays under it and this
/// app may execute it, its consent removed, its executor and its expiry, then the
/// counterparty's block and the knowledge of the deny list, and only then the user's
/// confirmation, so the user is asked only when confirming lifts the refusal.
pub(crate) fn standing(app: &Paykit, view: &AllowanceView, consent: ConsentState) -> Standing {
    match view.status {
        AllowanceStatus::Accepted => {}
        AllowanceStatus::Ended => return Standing::Final(Refusal::AllowanceEnded),
        AllowanceStatus::Conflicted => return Standing::Final(Refusal::AllowanceConflicted),
        AllowanceStatus::Rejected => return Standing::Final(Refusal::Rejected),
        _ => return Standing::Temporary("the Allowance isn't accepted yet"),
    }
    if view.role != Some(AllowanceRole::Allower) {
        return Standing::Final(Refusal::Usage(
            "this identity doesn't pay under this Allowance".into(),
        ));
    }
    if !app.holds("paykit_allowance_execute") {
        return Standing::Final(Refusal::ScopeMissing("paykit_allowance_execute".into()));
    }
    if let ConsentState::Removed(reason) = consent {
        return Standing::Final(removed_refusal(reason));
    }
    let own = match view.executor_app_id {
        Some(executor) if executor == app.app_id() => true,
        Some(_) => {
            return Standing::Final(Refusal::Usage("another app executes this Allowance".into()))
        }
        None => false,
    };
    // An Allowance that expired at its terms' `expires_at` has ended (Paykit spec 8.3).
    let expired = view
        .terms
        .as_ref()
        .and_then(|terms| terms.expires_at.as_ref())
        .zip(app.service_time())
        .is_some_and(|(at, now)| now >= at.ms());
    if expired {
        return Standing::Final(Refusal::AllowanceEnded);
    }
    // After a provider loss an Allowance last confirmed in an earlier generation needs a new
    // grant at this one (Paykit spec 8.3, "After provider loss"), which this version doesn't
    // write: it pays nothing automatically, and confirming wouldn't change that.
    if view.granted_before_loss {
        return Standing::Final(Refusal::NotAutomatic(GRANTED_BEFORE_LOSS.into()));
    }
    // Blocking pauses what is open with the counterparty and revokes nothing (Paykit spec
    // 8.1): unblocking ends the pause.
    match view.blocked {
        Some(false) => {}
        Some(true) => return Standing::Temporary("the counterparty is blocked"),
        None => return Standing::Temporary("the deny list isn't known"),
    }
    match (consent, own) {
        (ConsentState::Held, true) => Standing::Payable,
        (ConsentState::ConfirmAgain, _) => Standing::Confirm,
        // The grant path lands the kept batch: the host calls again.
        (ConsentState::GrantKept, _) => {
            Standing::Temporary("the executor grant's write waits to be sent again")
        }
        // Applied here: resuming writes the consent the user approved under this hold, and
        // without that approval the user confirms it.
        (ConsentState::Unconsented { approved: false }, _) => Standing::Confirm,
        (
            ConsentState::Held | ConsentState::Unrecorded | ConsentState::Unconsented { .. },
            false,
        )
        | (ConsentState::Unconsented { approved: true }, true) => Standing::NotTakenUp,
        // The grant came from a backup taken before this device's consent was written.
        (ConsentState::Unrecorded, true) => {
            Standing::Temporary("this app's grant is held, but its consent isn't on this device")
        }
        (ConsentState::Waiting(why), _) => Standing::Temporary(why),
        (ConsentState::Removed(reason), _) => Standing::Final(removed_refusal(reason)),
    }
}

/// Why an Allowance last confirmed before a provider loss pays nothing automatically.
pub(crate) const GRANTED_BEFORE_LOSS: &str = "the Allowance was granted before a provider loss; automatic payments under it need a new grant, which this version does not write";

/// The refusal for an Allowance taken off this app's consent list for `reason`: final, as no
/// later read puts it back (Paykit spec 8.3, "Executor", "Ends reach the executor").
pub(super) fn removed_refusal(reason: ConsentEnd) -> Refusal {
    match reason {
        ConsentEnd::Ended => Refusal::AllowanceEnded,
        ConsentEnd::Conflicted => Refusal::AllowanceConflicted,
        ConsentEnd::Rejected => Refusal::Rejected,
        ConsentEnd::HandedOff => Refusal::Usage("another app executes this Allowance".into()),
    }
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
        if record.manual_only {
            return Err(Refusal::ManualOnly.into());
        }
        if (request.conversion.is_some() && !request.is_recurring())
            || request.amount.asset != self.checked.terms.asset
            || period_start
                .map_or(record.allowance_id.as_ref(), |start| {
                    record.allowance_for(start)
                })
                .is_some_and(|id| id != &self.id)
        {
            return Err(not_automatic(
                "the request does not qualify under this Allowance",
            ));
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
        if self.expired() {
            return Err(Refusal::AllowanceEnded.into());
        }
        if terms
            .active_from
            .as_ref()
            .is_some_and(|t| self.checked.at < t.ms())
        {
            return Err(held("the Allowance's terms aren't active yet"));
        }
        if terms
            .per_payment_amount
            .as_ref()
            .is_some_and(|r| body.amount.value < r.minimum || body.amount.value > r.maximum)
        {
            return Err(not_automatic(
                "the subscription is outside the Allowance's terms",
            ));
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

    /// Whether the Allowance's terms expired at the service time this check read: it then
    /// has ended (Paykit spec 8.3).
    fn expired(&self) -> bool {
        self.checked
            .terms
            .expires_at
            .as_ref()
            .is_some_and(|t| self.checked.at >= t.ms())
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
        let expired = self.expired();
        let mut checked = self.checked;
        let amount = entry
            .paid_amount()?
            .ok_or_else(|| held("the transfer amount is unknown"))?;
        let endpoint = entry.destination.endpoint_id()?;
        checked
            .terms
            .check_payment(&amount, &endpoint, checked.at)
            .map_err(|refused| match refused {
                TermsRefusal::NotActive if expired => Refusal::AllowanceEnded.into(),
                TermsRefusal::NotActive => held("the Allowance's terms aren't active yet"),
                _ => not_automatic("the transfer does not satisfy the Allowance terms"),
            })?;
        check_limits(&checked.terms, &checked.usage, &amount.value).map_err(|limit| {
            let why = format!("the {} limit has no room", limit.as_str());
            // The per-payment range never makes room for this payment, nor the lifetime limit
            // once nothing is reserved; a reservation's release, a later period and a repair
            // of usage of other terms do.
            match limit {
                LimitKind::PerPayment => not_automatic(&why),
                // Room another app's failed attempt holds comes back once the user releases
                // it, and room a reserved entry holds once it is released.
                LimitKind::Lifetime if checked.facts.releasable() => held(
                    "the lifetime limit has no room while another app's failed attempt holds it: the user can release that attempt (reserved_by_other_apps)",
                ),
                LimitKind::Lifetime if checked.usage.reserved() == 0 => not_automatic(&why),
                _ => held(&why),
            }
        })?;
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
        entry.allowance_expires_at_ms = checked.terms.expires_at.as_ref().map(|t| t.ms());
        let mut end = checked.terms.entry_window_end(checked.at)?;
        if let Some(expiry) = &checked.terms.expires_at {
            end = end.min(expiry.ms());
        }
        let window = Condition::ServerTimeIn {
            start: Some(checked.at),
            end: Some(end),
        };
        checked.stage(app, batch, vec![window], false).await
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
            checked
                .signers
                .original(entry.attempt_id, op)
                .is_some_and(|op| {
                    application
                        .accepted_opening(&self.app, &op, entry, &read, &checked.grant)
                        .is_some()
                })
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
        checked.stage(self, batch, Vec::new(), false).await
    }

    /// The checks before a payment under the Allowance at `record` to `payee` starts (Paykit
    /// spec 8.3, "Executor"): first where the Allowance stands ([`standing`]), which refuses
    /// for good, for the user's confirmation, or for now; then a request of another
    /// counterparty, for good ([`Refusal::Usage`]); then what a later read or answer changes
    /// ([`Refusal::NotNow`]).
    pub(crate) async fn allowance_reservation(
        &mut self,
        record: &[u8; 32],
        payee: &[u8; 32],
    ) -> Result<AllowanceReservation> {
        let stopped =
            || held("the accepted executor grant, ledger or wallet history is unavailable");
        let view = self.allowance(record).await?;
        let consent = Box::pin(self.consent_state(record)).await?;
        if let Some(refusal) = standing(self, &view, consent).refusal(&view.hold) {
            return Err(refusal.into());
        }
        if view.counterparty != *payee {
            return Err(Refusal::Usage(
                "the request's payee isn't this Allowance's counterparty".into(),
            )
            .into());
        }
        if self
            .executor_replies_pending(record, payee, &view.allowance_id, false)
            .await?
        {
            return Err(held("a reply to the Allowance waits to be written"));
        }
        let target = ledger_target(payee, &view.allowance_id);
        if self.pause_at(record).await?.blocks_automatic()
            || self
                .pause_at(&self.app.record_key(&target)?)
                .await?
                .blocks_automatic()
        {
            return Err(held("the Allowance's records are paused or divergent"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spend_removed_consent_refuses_as_its_end() {
        // A removal refuses as the status path refuses the same end (Paykit spec 8.3).
        assert_eq!(removed_refusal(ConsentEnd::Ended), Refusal::AllowanceEnded);
        assert_eq!(
            removed_refusal(ConsentEnd::Conflicted),
            Refusal::AllowanceConflicted
        );
        assert_eq!(removed_refusal(ConsentEnd::Rejected), Refusal::Rejected);
        assert!(matches!(
            removed_refusal(ConsentEnd::HandedOff),
            Refusal::Usage(_)
        ));
    }
}
