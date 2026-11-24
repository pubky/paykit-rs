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

/// A payment the Allowance's terms don't cover: final for automatic payment, though the
/// user may still pay it by hand (Paykit spec 8.3).
fn not_automatic(why: &str) -> crate::PaykitError {
    Refusal::NotAutomatic(why.into()).into()
}

/// Whether a payment of `value` never fits a period limit, whatever later outcomes do: a
/// period that admits no payment, or the payment alone above a period's amount limit
/// (Paykit spec 8.3).
fn period_never_fits(
    terms: &paykit_lib::allowances::AllowanceTerms,
    value: &paykit_lib::Decimal,
) -> bool {
    terms.period_limits.iter().any(|period| {
        period.payment_count_limit == Some(0)
            || period
                .amount_limit
                .as_ref()
                .is_some_and(|cap| value.scaled() > cap.scaled())
    })
}

/// Where the lifetime limit leaves a payment (Paykit spec 8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LifetimeRoom {
    /// It fits now.
    Now,
    /// It fits once the entries still reserved give their room back, by their outcomes,
    /// with no one acting.
    Automatic,
    /// It fits only once the user also releases another app's failed attempts.
    Manual,
    /// It never fits.
    Never,
}

/// [`LifetimeRoom`] for a payment of `value`, from the same counting as `usage`:
/// `self_freeing` is the amount of the reservations an outcome may still give back with no
/// one acting ([`self_freeing`]), and `releasable` the room only the user frees, the failed
/// attempts of other apps the user may release, which count as spent until then. An entry
/// spent or released already frees nothing more.
fn lifetime_room(
    terms: &paykit_lib::allowances::AllowanceTerms,
    usage: &paykit_lib::allowances::Usage,
    self_freeing: u128,
    releasable: u128,
    value: &paykit_lib::Decimal,
) -> LifetimeRoom {
    let Some(cap) = &terms.lifetime_amount_limit else {
        return LifetimeRoom::Now;
    };
    let used = usage.lifetime().scaled();
    let fits = |freed: u128| {
        used.saturating_sub(freed)
            .checked_add(value.scaled())
            .is_some_and(|sum| sum <= cap.scaled())
    };
    if fits(0) {
        LifetimeRoom::Now
    } else if fits(self_freeing) {
        LifetimeRoom::Automatic
    } else if fits(self_freeing.saturating_add(releasable)) {
        LifetimeRoom::Manual
    } else {
        LifetimeRoom::Never
    }
}

/// The amount of the reservations whose room an outcome may still give back with no one
/// acting (Paykit spec 8.3), judged entry by entry: of the entries the usage holds reserved
/// that the method's history doesn't show (`before_call`), those this app's journal doesn't
/// hold at or past the method's call (`past_call`). A payment at the call or past it counts
/// whether it pays or fails, so its reservation never frees. One of another app, which this
/// app's journal doesn't hold, may still free itself.
fn self_freeing(
    before_call: &[(paykit_lib::AttemptId, u128)],
    past_call: impl Fn(&paykit_lib::AttemptId) -> bool,
) -> u128 {
    before_call
        .iter()
        .filter(|(attempt_id, _)| !past_call(attempt_id))
        .fold(0u128, |sum, (_, value)| sum.saturating_add(*value))
}

/// Whether this app's journal holds `attempt` at or past the payment method's call: any
/// entry of it but one of this installation still before the call. An entry back from a
/// backup may have been at the call, which the backup can't tell (Paykit spec 7.2).
fn past_the_call(journal: &[JournalEntry], attempt: &paykit_lib::AttemptId) -> bool {
    journal.iter().any(|own| {
        own.attempt().is_ok_and(|id| id == *attempt)
            && (own.restored
                || !matches!(
                    own.stage,
                    crate::journal::Stage::Recorded | crate::journal::Stage::Attempted
                ))
    })
}

/// Terms whose start is still ahead: the same call once it has passed clears this (Paykit
/// spec 8.3).
fn terms_not_active() -> crate::PaykitError {
    waits(crate::NotNowReason::CallAgain(
        "the Allowance's terms aren't active yet",
    ))
}

/// A payment that waits: a later read, an earlier attempt's answer or room in a limit
/// clears it (Paykit spec 8.3).
fn waits(reason: crate::NotNowReason) -> crate::PaykitError {
    Refusal::NotNow(reason).into()
}

/// The refusal of a payment the first failing `limit` has no room for (Paykit spec 8.3),
/// given whether a period never fits it and where the lifetime limit leaves it.
fn limit_refusal(limit: LimitKind, period_never: bool, room: LifetimeRoom) -> crate::PaykitError {
    let why = format!("the {} limit has no room", limit.as_str());
    match limit {
        LimitKind::PerPayment => not_automatic(&why),
        // Usage of other terms: no window or release mends it (Paykit spec 8.3).
        LimitKind::InconsistentUsage => execution_held(INCONSISTENT_USAGE).into(),
        _ if period_never || room == LifetimeRoom::Never => not_automatic(&why),
        // Room only the user gives back, releasing another app's failed attempt
        // ([`crate::Paykit::reserved_by_other_apps`] lists it): the user's to answer, also
        // when a period limit fails first, since no period roll makes lifetime room.
        _ if room == LifetimeRoom::Manual => execution_held(RELEASE_HELD).into(),
        // A window rolling, or a reserved entry's outcome, gives the room back.
        _ => waits(crate::NotNowReason::AllowanceLimit {
            limit: limit.as_str().into(),
        }),
    }
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
    /// A later read, answer or the user's own action may change it: the typed refusal says
    /// which.
    Temporary(Refusal),
    /// Nothing later changes it.
    Final(Refusal),
}

impl Standing {
    /// The refusal a payment under the Allowance `hold` names gets, if any.
    pub(crate) fn refusal(self, hold: &crate::AllowanceHold) -> Option<Refusal> {
        match self {
            Self::Payable => None,
            Self::NotTakenUp => Some(execution_held(NOT_TAKEN_UP)),
            Self::Confirm => Some(Refusal::AllowanceConfirmationNeeded { hold: hold.clone() }),
            Self::Temporary(refusal) => Some(refusal),
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
    if let Some(standing) = status_standing(view.status, view.proposed_here) {
        return standing;
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
    // After a provider loss an Allowance set up in an earlier generation and never granted
    // here can't be granted now (Paykit spec 8.3, Acceptance): it pays nothing automatically.
    if view.granted_before_loss && !own {
        return Standing::Final(Refusal::NotAutomatic(SET_UP_BEFORE_LOSS.into()));
    }
    // Blocking pauses what is open with the counterparty and revokes nothing (Paykit spec
    // 8.1): unblocking ends the pause.
    match view.blocked {
        Some(false) => {}
        Some(true) => {
            return Standing::Temporary(Refusal::HeldForUser(
                crate::HeldForUserReason::CounterpartyBlocked("the counterparty is blocked"),
            ))
        }
        None => return Standing::Temporary(read_again("the deny list isn't known")),
    }
    // One this app executes, last confirmed before the loss, waits for the user's
    // confirmation again, which writes its grant at the current generation; that grant
    // establishes no complete usage authority, so its usage waits for a repair (Paykit spec
    // 8.3, "After provider loss"). Neither lifts the shared hold on one-time payments.
    // A known wait comes first: confirming wouldn't lift it.
    // A missed item or a record not held as executing needs: a flagged record can stay so,
    // so it is shown, not waited on.
    if let ConsentState::Waiting(why) = consent {
        return Standing::Temporary(why);
    }
    // The regrant is a write over the record: while that waits for the user's confirmation
    // of the record, or for the messages kept over it, so does the Allowance's (Paykit spec 6).
    if view.granted_before_loss {
        return match view.record_write_waits {
            true => Standing::Temporary(execution_held(WAITS_FOR_RECORD_WRITE)),
            false => Standing::Confirm,
        };
    }
    // The regrant written, a newer hold asks the user again first; then its usage waits.
    if view.regranted_after_loss {
        return match consent {
            ConsentState::ConfirmAgain => Standing::Confirm,
            _ => Standing::Temporary(Refusal::HeldForUser(
                crate::HeldForUserReason::AllowanceNotResumed,
            )),
        };
    }
    match (consent, own) {
        (ConsentState::Held, true) => Standing::Payable,
        // Asked again only where this app holds a grant it can stand on: confirming lifts
        // nothing on a record whose executor doesn't verify.
        (ConsentState::ConfirmAgain, true) => Standing::Confirm,
        (ConsentState::ConfirmAgain, false) => Standing::NotTakenUp,
        // The grant path lands the kept batch: the host calls again.
        (ConsentState::GrantKept, _) => Standing::Temporary(Refusal::NotNow(
            crate::NotNowReason::EarlierWriteUnresolved(
                "the executor grant's write waits to be sent again",
            ),
        )),
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
            Standing::Temporary(execution_held(CONSENT_NOT_ON_DEVICE))
        }
        (ConsentState::Waiting(why), _) => Standing::Temporary(why),
        (ConsentState::Removed(reason), _) => Standing::Final(removed_refusal(reason)),
    }
}

/// Where an Allowance stands by its status alone: `None` for an accepted one, which the
/// rest of [`standing`] decides.
fn status_standing(status: AllowanceStatus, proposed_here: bool) -> Option<Standing> {
    Some(match status {
        AllowanceStatus::Accepted => return None,
        AllowanceStatus::Ended => Standing::Final(Refusal::AllowanceEnded),
        AllowanceStatus::Conflicted => Standing::Final(Refusal::AllowanceConflicted),
        AllowanceStatus::Rejected => Standing::Final(Refusal::Rejected),
        AllowanceStatus::AwaitingConflictCheck => {
            Standing::Temporary(execution_held(CONFLICT_CHECK))
        }
        // A proposal of this identity waits for the counterparty's answer; one the
        // counterparty proposed waits for this user's.
        // An answer may never come, or the proposal may have lapsed: shown, not waited on.
        _ if proposed_here => {
            Standing::Temporary(execution_held("the Allowance isn't accepted yet"))
        }
        _ => Standing::Temporary(execution_held(
            "the Allowance waits for this wallet's acceptance",
        )),
    })
}

/// Why an Allowance set up before a provider loss, and never granted here, pays nothing
/// automatically.
pub(crate) const SET_UP_BEFORE_LOSS: &str = "the Allowance was set up before a provider loss and never granted here, so it can't be granted now: the user can end it and set up a new one";

/// Why an Allowance last confirmed before a provider loss waits before asking the user.
pub(crate) const WAITS_FOR_RECORD_WRITE: &str = "the Allowance record is divergent or has messages to write first: the user confirms the record as shown, a newer write ends it, or the next receive writes the messages";

/// Why a payment waits for the user to release another app's failed attempt.
const RELEASE_HELD: &str = "the lifetime limit has no room while another app's failed attempt holds it: the user can release that attempt (reserved_by_other_apps)";

/// Why a payment waits on usage that doesn't follow the Allowance's terms.
const INCONSISTENT_USAGE: &str =
    "the Allowance's usage doesn't follow its terms: the user ends it and sets up a new one";

/// Why an accepted Allowance waits for its conflict check.
const CONFLICT_CHECK: &str = "the Allowance waits for its conflict check";

/// Why this app doesn't pay under an Allowance it hasn't taken up the execution of.
const NOT_TAKEN_UP: &str = "this app hasn't taken up the Allowance's execution";

/// Why this app doesn't pay under a grant it holds from a backup taken before its consent.
const CONSENT_NOT_ON_DEVICE: &str =
    "this app's grant is held, but its consent isn't on this device";

/// A wait that a later read clears; `why` names it.
fn read_again(why: &'static str) -> Refusal {
    Refusal::NotNow(crate::NotNowReason::ReadAgain(why))
}

/// A hold on this app's execution of the Allowance that the user or the host acts on;
/// `why` names it.
fn execution_held(why: &'static str) -> Refusal {
    Refusal::HeldForUser(crate::HeldForUserReason::AllowanceExecutionHeld(why))
}

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
                // A claim gone, or an attempt another app or the user made outside the ledger:
                // no read adds it, so it is shown, and the user may pay by hand.
                return Err(execution_held(
                    "an earlier attempt is not accounted for under this Allowance",
                )
                .into());
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
            return Err(terms_not_active());
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
            .ok_or_else(|| not_automatic("the transfer amount is unknown"))?;
        let endpoint = entry.destination.endpoint_id()?;
        checked
            .terms
            .check_payment(&amount, &endpoint, checked.at)
            .map_err(|refused| match refused {
                TermsRefusal::NotActive if expired => Refusal::AllowanceEnded.into(),
                TermsRefusal::NotActive => terms_not_active(),
                _ => not_automatic("the transfer does not satisfy the Allowance terms"),
            })?;
        if let Err(limit) = check_limits(&checked.terms, &checked.usage, &amount.value) {
            let journal = Box::pin(app.journal()).await?;
            let room = lifetime_room(
                &checked.terms,
                &checked.usage,
                self_freeing(&checked.reserved_before_call, |attempt| {
                    past_the_call(&journal, attempt)
                }),
                checked.facts.releasable().unwrap_or(0),
                &amount.value,
            );
            return Err(limit_refusal(
                limit,
                period_never_fits(&checked.terms, &amount.value),
                room,
            ));
        }
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
        let stopped = || {
            crate::PaykitError::from(execution_held(
                "the accepted executor grant, ledger or wallet history is unavailable",
            ))
        };
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
            // A reply's mark can outlive its delivery: shown, not waited on.
            return Err(execution_held("a reply to the Allowance waits to be written").into());
        }
        let target = ledger_target(payee, &view.allowance_id);
        if self.pause_at(record).await?.blocks_automatic()
            || self
                .pause_at(&self.app.record_key(&target)?)
                .await?
                .blocks_automatic()
        {
            return Err(Refusal::HeldForUser(crate::HeldForUserReason::HeldFromAutomatic).into());
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
        let (checked, _) = match self.current_executor_record(&target).await {
            Ok(read) => read,
            Err(error) => {
                crate::pay::unless_stop(error)?;
                return Ok(true);
            }
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
                // The record moves on only to a later write: a later generation of the same
                // history continues the sequence (Pubky Switch spec, Section 12.5).
                || checked.revision.generation < under.revision().generation
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
    fn test_spend_limit_refusal_holds_for_the_user_whatever_limit_fails_first() {
        let refusal =
            |limit, period_never, room| limit_refusal(limit, period_never, room).refusal().cloned();
        let held = Some(execution_held(RELEASE_HELD));
        // Period cap 50, lifetime cap 100, 60 spent in earlier periods and 30 held by another
        // app's failed attempt: a payment of 30 fails the period amount first, but only the
        // user's release makes lifetime room, which no period roll gives.
        for limit in [
            LimitKind::PeriodAmount,
            LimitKind::PeriodCount,
            LimitKind::Lifetime,
        ] {
            assert_eq!(
                refusal(limit, false, LifetimeRoom::Manual),
                held,
                "{limit:?}"
            );
        }
        // A period that rolls or a reservation that ends gives the room back.
        assert_eq!(
            refusal(LimitKind::PeriodAmount, false, LifetimeRoom::Automatic),
            Some(Refusal::NotNow(crate::NotNowReason::AllowanceLimit {
                limit: "period_amount".into()
            }))
        );
        assert_eq!(
            refusal(LimitKind::PeriodAmount, false, LifetimeRoom::Never),
            Some(Refusal::NotAutomatic(
                "the period_amount limit has no room".into()
            ))
        );
    }

    #[test]
    fn test_spend_lifetime_room_frees_only_what_is_still_reserved() {
        use paykit_lib::allowances::{
            usage, AllowanceTerms, AttemptEvidence, ClaimState, EntryOutcome, Ledger, LedgerEntry,
        };
        use paykit_lib::{AttemptId, Decimal};
        let terms = AllowanceTerms::from_json(&serde_json::json!({
            "asset": "btc", "lifetime_amount_limit": "100", "period_limits": [],
            "per_payment_amount": null, "active_from": null, "expires_at": null,
            "allowed_payment_endpoint_identifiers": null
        }))
        .unwrap();
        let entry = |id: u8, value: &str, outcome| LedgerEntry {
            obligation: [id; 32],
            attempt_id: AttemptId::from_bytes([id; 16]),
            value: Decimal::parse(value).unwrap(),
            asset: paykit_lib::Asset::parse("btc").unwrap(),
            at: 1_000,
            outcome,
        };
        let evidence = |claim, paid, never_dispatched| AttemptEvidence {
            claim: Some(claim),
            paid,
            never_dispatched,
        };
        // `at_method`: the attempts the method's history shows or the journal holds at or
        // past the call.
        let room_with = |entries: &[LedgerEntry],
                         facts: &[(u8, AttemptEvidence)],
                         at_method: &[u8],
                         releasable: &str,
                         value: &str| {
            let ledger = Ledger {
                entries: entries.to_vec(),
                ..Ledger::empty(
                    [5; 32],
                    AllowanceId::parse("d0efffb7-191b-4709-b6c7-e343af5500b4").unwrap(),
                    [9; 32],
                )
            };
            let facts: std::collections::HashMap<_, _> = facts
                .iter()
                .map(|(id, fact)| (AttemptId::from_bytes([*id; 16]), *fact))
                .collect();
            let used = usage(&terms, &ledger, &facts, 2_000).unwrap();
            let at_method: Vec<_> = at_method
                .iter()
                .map(|id| AttemptId::from_bytes([*id; 16]))
                .collect();
            // As the reconciliation lists them: counted reserved, not in the history.
            let before_call: Vec<_> = ledger
                .entries
                .iter()
                .filter(|entry| {
                    paykit_lib::allowances::counts(
                        entry,
                        facts.get(&entry.attempt_id).copied().unwrap_or_default(),
                    ) == paykit_lib::allowances::Counting::Reserved
                })
                .map(|entry| (entry.attempt_id, entry.value.scaled()))
                .collect();
            lifetime_room(
                &terms,
                &used,
                self_freeing(&before_call, |attempt| at_method.contains(attempt)),
                Decimal::parse(releasable).unwrap().scaled(),
                &Decimal::parse(value).unwrap(),
            )
        };
        let room =
            |entries: &[LedgerEntry],
             facts: &[(u8, AttemptEvidence)],
             releasable: &str,
             value: &str| room_with(entries, facts, &[], releasable, value);
        use LifetimeRoom::{Automatic, Manual, Never};
        // 70 dispatched and failed by another app, spent for good, and 10 reserved: 40
        // never fits; 25 fits once the 10 come back.
        let entries = [entry(1, "70", None), entry(2, "10", None)];
        let facts = [
            (
                1,
                evidence(ClaimState::Failed { by_owner: false }, false, false),
            ),
            (2, evidence(ClaimState::Open, false, false)),
        ];
        assert_eq!(room(&entries, &facts, "0", "40"), Never);
        assert_eq!(room(&entries, &facts, "0", "25"), Automatic);
        // The 10 reserved, once at the method's call, are spent whatever they answer: 25
        // never fits.
        assert_eq!(room_with(&entries, &facts, &[2], "0", "25"), Never);
        // 70 the method's history shows paid while its claim is still open, and 10 reserved
        // before the call: the 70 are spent and at the method, and that takes nothing from
        // the 10, which may still come back. 25 waits for them; 40 never fits.
        let paid_by_history = [
            (1, evidence(ClaimState::Open, true, false)),
            (2, evidence(ClaimState::Open, false, false)),
        ];
        assert_eq!(
            room_with(&entries, &paid_by_history, &[1], "0", "25"),
            Automatic
        );
        assert_eq!(
            room_with(&entries, &paid_by_history, &[1], "0", "40"),
            Never
        );
        // 70 the method reported failed, its claim's outcome not written yet: still counted
        // reserved, but at the method, so nothing gives it back and 50 never fits.
        let failed_unwritten = [(1, evidence(ClaimState::Open, false, false))];
        assert_eq!(
            room_with(&entries[..1], &failed_unwritten, &[1], "0", "50"),
            Never
        );
        assert_eq!(
            room_with(&entries[..1], &failed_unwritten, &[], "0", "50"),
            Automatic
        );
        // 70 paid on the method with its ledger outcome unwritten, and 10 in flight: the 70
        // are spent, so 40 never fits.
        let facts = [
            (1, evidence(ClaimState::Open, true, false)),
            (2, evidence(ClaimState::Open, false, false)),
        ];
        assert_eq!(room(&entries, &facts, "0", "40"), Never);
        // 80 paid, 50 released and 10 reserved: the released 50 frees nothing more, so 25
        // never fits, and 15 fits once the 10 come back.
        let entries = [
            entry(1, "80", Some(EntryOutcome::Done)),
            entry(2, "50", None),
            entry(3, "10", None),
        ];
        let facts = [
            (
                2,
                evidence(ClaimState::Failed { by_owner: false }, false, true),
            ),
            (3, evidence(ClaimState::Open, false, false)),
        ];
        assert_eq!(room(&entries, &facts, "0", "25"), Never);
        assert_eq!(room(&entries, &facts, "0", "15"), Automatic);
        // 60 paid, 25 in flight and 10 held by another app's failed attempt the user can
        // release: 40 needs both, so the user is asked; 30 fits once the 25 come back; 50
        // never fits.
        let entries = [
            entry(1, "60", Some(EntryOutcome::Done)),
            entry(2, "25", None),
            entry(3, "10", None),
        ];
        let facts = [
            (2, evidence(ClaimState::Open, false, false)),
            (
                3,
                evidence(ClaimState::Failed { by_owner: false }, false, false),
            ),
        ];
        assert_eq!(room(&entries, &facts, "10", "40"), Manual);
        assert_eq!(room(&entries, &facts, "10", "30"), Automatic);
        assert_eq!(room(&entries, &facts, "10", "50"), Never);
    }

    #[test]
    fn test_spend_pending_conflict_check_waits_and_stops_stay_final() {
        use crate::allowances::shown_status;
        // An accepted Allowance whose record could still establish a conflict waits for
        // the check, with its own reason, not as one not accepted yet.
        assert_eq!(
            status_standing(shown_status(AllowanceStatus::Accepted, true), true),
            Some(Standing::Temporary(execution_held(CONFLICT_CHECK)))
        );
        assert_eq!(
            status_standing(shown_status(AllowanceStatus::Accepted, false), true),
            None
        );
        // An ended, rejected or conflicted record stays final with conflict evidence still
        // unresolved (Paykit spec 8.3).
        for (status, refusal) in [
            (AllowanceStatus::Ended, Refusal::AllowanceEnded),
            (AllowanceStatus::Rejected, Refusal::Rejected),
            (AllowanceStatus::Conflicted, Refusal::AllowanceConflicted),
        ] {
            assert_eq!(
                status_standing(shown_status(status, true), true),
                Some(Standing::Final(refusal))
            );
        }
    }

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
