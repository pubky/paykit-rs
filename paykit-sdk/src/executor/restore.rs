//! Check the ordinary direct successor of the backed-up executor ledger (Paykit spec 8.3).

use std::collections::{hash_map::Entry, HashMap};

use paykit_lib::allowances::{
    check_successor, usage, AllowanceTerms, ClaimState, CompactedFailed, EntryOutcome, Ledger,
    LedgerEntry, Usage,
};
use paykit_lib::records::{bind_ledger, ActivityRecord, RecordBinding, StoredMessage};
use paykit_lib::{AllowanceId, Asset, Body, EndpointAmount, PaymentAmount};
use pubky_switch_client::app::{Alarm, KeptOp, KeptReason, RecordRead};
use pubky_switch_core::ops::Condition;
use pubky_switch_core::shared::ApplicationRef;

use super::approval::ApprovedOpening;
use super::checkpoint::{ledger_target, ExecutorCheckpoint};
use super::compaction::{CompactionRead, LedgerCompaction, Retired};
use super::evidence::{AppliedRecord, Purpose};
use super::ExecutorHistory;
use super::{reconciled_usage, wallet_inventory, Accounting};
use crate::allowances::allowance_target;
use crate::state::key;
use crate::{Paykit, Result};

const MISSING_LEDGER: &str =
    "the current service is missing the executor's accepted permanent ledger";
const EARLIER_LEDGER: &str = "the ledger write is based before the executor's accepted version";
const MISSING_PAYMENT: &str =
    "the service ledger omits a paid Allowance attempt opened after the accepted write";

/// The SDK keeps the authenticated application it accepted. The spending batch must still
/// condition on both current records and atomically open the new attempt and reservation.
pub(super) struct ReconciledLedger {
    pub(super) facts: HashMap<paykit_lib::AttemptId, paykit_lib::allowances::AttemptEvidence>,
    pub(super) signers: super::checkpoint::ReservationSigners,
    pub(super) applied_proposals: Vec<String>,
    pub(super) ledger: Ledger,
    pub(super) usage: Usage,
    pub(super) evidence: KeptOp,
    pub(super) grant: Condition,
    pub(super) current: Condition,
    pub(super) terms: AllowanceTerms,
    pub(super) at: u64,
    pub(super) compaction: Option<LedgerCompaction>,
    pub(super) at_initial_ledger: bool,
}

fn compacted_transfer_matches(
    obligation: [u8; 32],
    amount: &PaymentAmount,
    retired: Option<&LedgerEntry>,
    failed: Option<&CompactedFailed>,
    grant_asset: &Asset,
) -> bool {
    let saved = match (retired, failed) {
        (Some(entry), None) => (entry.obligation, &entry.asset, &entry.value),
        (None, Some(entry)) => (entry.obligation, grant_asset, &entry.value),
        _ => return false,
    };
    saved == (obligation, &amount.asset, &amount.value)
}

/// How the claim of an attempt the ledger holds reads for counting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaimClass {
    /// A state verified against a signed opening: a failure gives way to a later signed
    /// `done` (Paykit spec 8.3, `failed` releases).
    Verified(super::ReservationClaim),
    /// No opening verifies it: what the read, or a signed `done` held without its opening,
    /// shows.
    Shown(super::ClaimShows),
}

/// [`ClaimClass`] of `attempt` on `obligation`, from its claim `read` (none when the claim is
/// gone), the signed `openings` that bind it, each with its sequence, and `verifies`, which
/// says whether this app holds a signed outcome for a state after an opening's sequence. A
/// read can lag behind the claim, so a later signed `done` wins over a failure it shows, and
/// a signed `done` counts even without its opening.
fn classify_claim(
    read: Option<&pubky_switch_client::app::ClaimRead>,
    attempt: paykit_lib::AttemptId,
    obligation: [u8; 32],
    openings: &[(super::ReservationClaim, u64)],
    verifies: impl Fn(super::ReservationClaim, u64) -> bool,
) -> ClaimClass {
    for &(claim, opened) in openings {
        if let Some(state) = super::evidence::settled_state(claim, |state| verifies(state, opened))
        {
            return ClaimClass::Verified(state);
        }
    }
    let signed_done = verifies(
        super::ReservationClaim {
            obligation,
            state: ClaimState::Done,
        },
        0,
    );
    ClaimClass::Shown(match read {
        Some(read) => unverified_claim(read, attempt, signed_done),
        None if signed_done => super::ClaimShows::SignedDone,
        None => super::ClaimShows::Gone,
    })
}

/// What a claim read shows of `attempt` when no signed opening verifies it: a `done` it shows,
/// or a signed `done` held without its opening (`signed_done`), which counts the attempt
/// spent; anything else leaves the failure the ledger holds as it is (Paykit spec 8.3,
/// `failed` releases).
fn unverified_claim(
    read: &pubky_switch_client::app::ClaimRead,
    attempt: paykit_lib::AttemptId,
    signed_done: bool,
) -> super::ClaimShows {
    use pubky_switch_core::claims::Outcome;
    let shown = read
        .view
        .attempt(&attempt.to_bytes())
        .map(|shown| shown.outcome);
    match (signed_done, shown) {
        (true, _) => super::ClaimShows::SignedDone,
        (false, Some(Outcome::Done)) => super::ClaimShows::ShownDone,
        (false, Some(Outcome::Open | Outcome::Unknown | Outcome::Failed) | None) => {
            super::ClaimShows::Gone
        }
    }
}

/// A settled tombstone can outlive its signed opening. It corroborates a Done entry
/// already accepted under this grant, never a new reservation (Paykit spec 8.3).
/// The caller must also verify the dated payer proof and the method's exact transfer.
fn settled_after_accepted_payment(
    read: &pubky_switch_client::app::ClaimRead,
    previous: Option<&LedgerEntry>,
    accepted_at: u64,
    entry: &LedgerEntry,
    executor: [u8; 16],
) -> bool {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_switch_core::claims::{ClaimStatus, Outcome};

    let claim = &read.view;
    claim.tombstone
        && read.entries.is_empty()
        && claim.status == ClaimStatus::Settled
        && !claim.rebuilt
        && !claim.divergent
        && claim.claim_key == entry.obligation
        && entry.outcome == Some(EntryOutcome::Done)
        && previous == Some(entry)
        && claim
            .attempt(&entry.attempt_id.to_bytes())
            .is_some_and(|attempt| {
                attempt.owner == executor
                    && attempt.outcome == Outcome::Done
                    && attempt.resolved_by == Some(executor)
                    && !attempt.duplicate
                    && !attempt.possibly_duplicate
                    && entry.at <= attempt.opened_at
                    && attempt.opened_at <= accepted_at
            })
}

/// The original request and payer proof agree on the destination and any Billing Period.
/// Establish this before amount or Allowance attribution (Paykit wire 4.5, 4.9–4.10).
struct RequestedPayment<'a> {
    request: &'a StoredMessage,
    body: &'a paykit_lib::bodies::PaymentRequest,
    proof: &'a StoredMessage,
    paid: &'a paykit_lib::bodies::PaymentProof,
    detail_rates: Option<&'a [paykit_lib::Rate]>,
}

impl<'a> RequestedPayment<'a> {
    fn bind(
        activity: &'a ActivityRecord,
        request: &'a StoredMessage,
        proof: &'a StoredMessage,
        occurrence: Option<paykit_lib::periods::PeriodRef>,
    ) -> Option<Self> {
        use paykit_lib::bodies::RequestDestination;

        let Body::PaymentRequest(body) = request.body() else {
            return None;
        };
        let Body::PaymentProof(paid) = proof.body() else {
            return None;
        };
        let period = paid
            .check_against(body)
            .ok()?
            .map(paykit_lib::periods::PeriodRef::from);
        let request_hash = request.hash();
        let endpoint = &paid.payment_endpoint_identifier;
        if period != occurrence
            || proof.message().refs.first() != Some(&request_hash)
            || !crate::periods::destination_app_permitted(body, paid.payment_app_id)
            || body
                .accepted_payment_endpoint_identifiers
                .as_ref()
                .is_some_and(|ids| !ids.contains(endpoint))
        {
            return None;
        }
        let later = match period {
            Some(period) => crate::periods::needs_period_destination(body, period).ok()?,
            None => false,
        };
        let selected = match proof.message().refs.as_slice() {
            [_] if !later => None,
            [_, hash] if period.is_some() => {
                let selected = activity.period_details.as_ref()?;
                let Body::PaymentPeriodDetails(details) = selected.body() else {
                    return None;
                };
                if selected.hash() != *hash
                    || selected.message().refs != [request_hash]
                    || selected.sender() != request.sender()
                    || selected.check_payee_sender(body).is_err()
                    || details.check_against(body).is_err()
                    || Some(details.billing_period) != period
                {
                    return None;
                }
                if later {
                    if paid.list_version.is_some()
                        || selected.cert().app_id() != paid.payment_app_id
                        || !details.payment_endpoints.as_ref()?.contains_key(endpoint)
                    {
                        return None;
                    }
                } else if details.rates.is_none() {
                    return None;
                }
                Some(details)
            }
            _ => return None,
        };
        if let (false, RequestDestination::Endpoints(endpoints)) = (later, &body.destination) {
            if paid.list_version.is_some()
                || request.cert().app_id() != paid.payment_app_id
                || !endpoints.contains_key(endpoint)
            {
                return None;
            }
        }
        Some(Self {
            request,
            body,
            proof,
            paid,
            detail_rates: selected.and_then(|d| d.rates.as_deref()),
        })
    }

    fn requested_amount(&self) -> Option<&'a PaymentAmount> {
        if !self.body.is_recurring() {
            return self.body.conversion.is_none().then_some(&self.body.amount);
        }
        let endpoint = &self.paid.payment_endpoint_identifier;
        let rates = self
            .detail_rates
            .or_else(|| crate::conversion::fixed_rates(self.body.conversion.as_ref()));
        (endpoint.asset_segment() == Some(self.body.amount.asset.as_str())
            && rates.is_none_or(|rates| {
                matches!(paykit_lib::rate_for_endpoint(rates, endpoint), Ok(None))
            }))
        .then_some(&self.body.amount)
    }

    /// A seller receipt can establish an expired journal's fixed or plain transfer
    /// when every allowed endpoint precision produces the wallet's exact amount.
    fn receipted_amount(&self, activity: &ActivityRecord, transfer: &PaymentAmount) -> bool {
        if !self.body.is_recurring() || self.detail_rates.is_some() {
            return false;
        }
        let Some(receipt) = crate::receipts::displayed_receipt(activity, Some(self.body)) else {
            return false;
        };
        if receipt.message().refs != [self.proof.hash(), self.request.hash()] {
            return false;
        }
        let rates = crate::conversion::fixed_rates(self.body.conversion.as_ref());
        let price = |digits| {
            matches!(paykit_lib::amount_for_endpoint(&self.body.amount, rates,
                &self.paid.payment_endpoint_identifier, digits), Ok(EndpointAmount::ByRate{amount,..}
                | EndpointAmount::RequestedAsset(amount)) if &amount == transfer)
        };
        price(0) && price(paykit_lib::amount::MAX_FRACTION_DIGITS as u32)
    }
}

/// How this executor established the application it is about to incorporate.
#[derive(Clone, Copy)]
pub(super) enum LedgerRead {
    Inventory,
    Outcome,
    Applied(ApplicationRef),
}

#[derive(Clone, Copy)]
pub(super) enum LedgerChange<'a> {
    Reservation(&'a LedgerEntry),
    Outcome(&'a LedgerEntry),
}

/// Rebuild the complete executor write before accepting its accompanying claim.
pub(super) fn ledger_change<'a>(
    held: &Ledger,
    current: &'a Ledger,
    terms: &AllowanceTerms,
) -> Result<Option<LedgerChange<'a>>> {
    if let Some(entry) = current.entries.last().filter(|entry| {
        entry.outcome.is_none()
            && !held
                .entries
                .iter()
                .any(|e| e.attempt_id == entry.attempt_id)
            && !held.failed.iter().any(|e| e.attempt_id == entry.attempt_id)
    }) {
        let expected = super::ledger::reserved_ledger(terms, held.clone(), entry.clone())?;
        // A removed entry takes its unknown members with it on the wire (Paykit wire 5).
        return Ok(
            (expected.to_json()? == current.to_json()?).then_some(LedgerChange::Reservation(entry))
        );
    }
    let mut expected = held.clone();
    for (previous, entry) in expected.entries.iter_mut().zip(&current.entries) {
        if previous.outcome != entry.outcome {
            previous.outcome = entry.outcome;
            return Ok((expected == *current).then_some(LedgerChange::Outcome(entry)));
        }
    }
    Ok(None)
}

impl Paykit {
    /// The Switch alarm that stopped this wallet after an Allowance ledger rollback
    /// (Paykit spec 8.3, Switch spec 10.1). Show its reason until the signer resolves
    /// the provider move or loss. The alarm survives reopening the wallet.
    pub fn ledger_alarm(&self) -> Option<Alarm> {
        self.app
            .alarm()
            .filter(|alarm| {
                matches!(alarm, Alarm::ProtocolRollback { protocol, reason }
                if protocol == crate::protocol::BACKUP_NAME
                    && matches!(reason.as_str(), MISSING_LEDGER | EARLIER_LEDGER | MISSING_PAYMENT))
            })
            .cloned()
    }

    /// Reconcile this app's accepted Allowance ledger following a restore (Paykit spec
    /// 8.3). Returns the verified usage, counting outstanding reservations in full. When
    /// the wallet lacks complete account history or the current signed grant, ledger and
    /// claim cannot be reconciled with its checkpoint, automatic use remains held.
    ///
    /// # Errors
    ///
    /// The stored or signed evidence cannot be read, the client raises a rollback alarm,
    /// or the service or storage operation fails. A successful reconciliation durably
    /// accepts the complete current ledger before reporting its usage.
    pub async fn reconcile_allowance(&mut self, record: &[u8; 32]) -> Result<Option<Usage>> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        Ok(Box::pin(self.executor_base(record, LedgerRead::Inventory))
            .await?
            .map(|checked| checked.usage))
    }

    pub(super) async fn executor_base(
        &mut self,
        record: &[u8; 32],
        source_kind: LedgerRead,
    ) -> Result<Option<ReconciledLedger>> {
        let local = key::executor_checkpoint(record);
        let Some(history) = self.local_get::<ExecutorHistory>(&local).await? else {
            return Ok(None);
        };
        Box::pin(self.executor_from_history(record, source_kind, history)).await
    }

    pub(super) async fn executor_from_history(
        &mut self,
        record: &[u8; 32],
        source_kind: LedgerRead,
        history: ExecutorHistory,
    ) -> Result<Option<ReconciledLedger>> {
        let Some(opening) = Box::pin(self.approved_opening(record)).await? else {
            return Ok(None);
        };
        let Some(source) = history.selected(&self.app, record, &opening) else {
            return Ok(None);
        };
        let mut checkpoint = Box::pin(self.finish_executor_grant(record, source, &opening)).await?;
        let Some(checked) =
            Box::pin(self.restore_executor_ledger(record, &checkpoint, &opening, source_kind))
                .await?
        else {
            return Ok(None);
        };
        if !checked.applied_proposals.is_empty()
            || history.0.len() > 1
            || (!checked.at_initial_ledger && checkpoint.later.as_ref() != Some(&checked.evidence))
        {
            Box::pin(self.keep_executor_checkpoint(record, &mut checkpoint, &checked)).await?;
        }
        Ok(Some(checked))
    }

    async fn keep_executor_checkpoint(
        &mut self,
        record: &[u8; 32],
        checkpoint: &mut ExecutorCheckpoint,
        checked: &ReconciledLedger,
    ) -> Result<()> {
        let mut tx = crate::state::Tx::default();
        for suffix in &checked.applied_proposals {
            tx.delete(&format!("{}{suffix}", key::LEDGER_PROPOSALS));
            tx.delete(&format!("{}{suffix}", key::LEDGER_BATCHES));
        }
        if let Some(receipt) = &checked.compaction {
            let Condition::RecordCurrent { key, .. } = checked.current else {
                return Err(crate::PaykitError::corrupt());
            };
            tx.put(&receipt.key(&key)?, receipt)?;
        }
        checkpoint.signers = checked.signers.clone();
        if !checked.at_initial_ledger {
            checkpoint.later = Some(checked.evidence.clone());
        }
        tx.put(
            &key::executor_checkpoint(record),
            &ExecutorHistory(vec![checkpoint.clone()]),
        )?;
        self.local_commit(tx).await
    }

    async fn restore_executor_ledger(
        &mut self,
        permanent_key: &[u8; 32],
        checkpoint: &ExecutorCheckpoint,
        approval: &ApprovedOpening,
        source: LedgerRead,
    ) -> Result<Option<ReconciledLedger>> {
        let Some(accepted) = checkpoint.accepted(&self.app, permanent_key, approval) else {
            return Ok(None);
        };
        let id = AllowanceId::parse(&checkpoint.allowance_id)?;
        let allowance = allowance_target(&checkpoint.counterparty, &id);
        let ledger_target = ledger_target(&checkpoint.counterparty, &id);
        let ledger_key = self.app.record_key(&ledger_target)?;
        let current_grant = match Box::pin(self.app.read_record_fresh(&allowance)).await? {
            RecordRead::Fresh(current_grant) => current_grant,
            RecordRead::Kept { reason, .. } => {
                return crate::pay::kept_read_stop(&reason).map_or(Ok(None), Err)
            }
            _ => return Ok(None),
        };
        let current_ledger = match Box::pin(self.app.read_record_fresh(&ledger_target)).await? {
            RecordRead::Fresh(value) => value,
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                ..
            } => {
                let alarm = self
                    .app
                    .raise_rollback(crate::protocol::BACKUP_NAME, MISSING_LEDGER)
                    .await?;
                return Err(pubky_switch_client::app::AppError::Alarm(alarm).into());
            }
            RecordRead::Kept { reason, .. } => {
                return crate::pay::kept_read_stop(&reason).map_or(Ok(None), Err)
            }
            _ => return Ok(None),
        };
        let originals = self.app.shared_evidence().await?;
        let Some(grant) = originals
            .ops
            .iter()
            .find_map(|op| AppliedRecord::current(&self.app, &allowance, &current_grant, op))
        else {
            return Ok(None);
        };
        // An intervening grant, handoff, end or other Allowance change requires its own
        // authorization and accounting checks. This path covers the same grant throughout.
        if grant.revision != accepted.grant.revision {
            return Ok(None);
        }
        let Some((current, evidence)) = originals.ops.iter().find_map(|op| {
            Some((
                AppliedRecord::current(&self.app, &ledger_target, &current_ledger, op)?,
                op,
            ))
        }) else {
            return Ok(None);
        };
        let mut claims = HashMap::new();
        let Ok(ledger) = bind_ledger(
            &current.value,
            self.app.naming_key(),
            &ledger_key,
            &accepted.record,
        ) else {
            return Ok(None);
        };
        let Body::AllowanceProposal(proposed) = accepted.record.proposal.body() else {
            return Ok(None);
        };
        let exact_grant = Condition::RecordCurrent {
            key: *permanent_key,
            revision: grant.revision,
            deleted: false,
        };
        if current.revision != accepted.application.revision
            && current.expected < accepted.application.version()
        {
            let alarm = self
                .app
                .raise_rollback(crate::protocol::BACKUP_NAME, EARLIER_LEDGER)
                .await?;
            return Err(pubky_switch_client::app::AppError::Alarm(alarm).into());
        }
        let Some(applied_proposals) =
            Box::pin(self.prepared_ledger(permanent_key, &accepted, &current, &exact_grant))
                .await?
        else {
            return Ok(None);
        };
        let mut signers = checkpoint.signers.clone();
        let mut compaction = None;
        let mut reads = HashMap::new();
        let mut unverified = HashMap::new();
        let mut recovered_copy = false;
        if current.revision != accepted.application.revision {
            if !current.follows(&accepted.application)
                || !current.cert.holds_scope("paykit_allowance_execute")
                || current.op.body().app_id != Some(self.app_id())
                || !current.op.body().conditions.contains(&exact_grant)
                || current.at_ms < accepted.application.at_ms
                || current.at_ms < grant.at_ms
                || check_successor(
                    &proposed.terms,
                    &accepted.ledger,
                    accepted.application.at_ms,
                    &ledger,
                    current.at_ms,
                )
                .is_err()
            {
                return Ok(None);
            }
            let mut reservation = None;
            if applied_proposals.current_standalone != Some(true) {
                match ledger_change(&accepted.ledger, &ledger, &proposed.terms)? {
                    Some(LedgerChange::Reservation(entry)) => {
                        if !current.opens_in_window(&proposed.terms, entry)? {
                            return Ok(None);
                        }
                        let Some(claim) = self.app.read_claim(&entry.obligation).await? else {
                            return Ok(None);
                        };
                        let with_claim = self.app.shared_evidence().await?;
                        let Some(((opening, seq), original)) =
                            with_claim.ops.iter().find_map(|op| {
                                // The reservation this read was asked to establish is the
                                // one being made; any other already applied.
                                let just_made = matches!(source, LedgerRead::Applied(_));
                                Some((
                                    current.reservation(&self.app, op, entry, &claim, just_made)?,
                                    op,
                                ))
                            })
                        else {
                            return Ok(None);
                        };
                        // The state to count is the verified one: a later signed `done` wins
                        // over a read that still shows the failure (Paykit spec 8.3, `failed`
                        // releases), and a state that doesn't verify establishes nothing.
                        let results = super::evidence::ClaimResults::new(
                            &self.app,
                            &with_claim.ops,
                            &signers,
                            &accepted,
                            &current,
                        );
                        let ClaimClass::Verified(opening) = classify_claim(
                            Some(&claim),
                            entry.attempt_id,
                            entry.obligation,
                            &[(opening, seq)],
                            |state, opened| results.confirms(entry.attempt_id, state, opened, None),
                        ) else {
                            return Ok(None);
                        };
                        // The service proved both operations in the same reservation batch.
                        if signers
                            .keep(
                                entry.attempt_id.to_text(),
                                original.op.doc_hash()?,
                                current.cert.signed(),
                            )
                            .is_none()
                        {
                            return Ok(None);
                        }
                        if ledger.count > accepted.ledger.count {
                            reservation = Some(KeptOp {
                                cert: current.cert.signed().clone(),
                                ..original.clone()
                            });
                        }
                        claims.insert(entry.attempt_id, opening);
                        reads.insert(entry.obligation, Some(claim));
                    }
                    Some(LedgerChange::Outcome(entry)) => {
                        let claim = self.app.read_claim(&entry.obligation).await?;
                        let joined = if let Some(claim) = &claim {
                            self.app.shared_evidence().await?.ops.iter().any(|op| {
                                current
                                    .applied_outcome(&self.app, op, entry, claim)
                                    .is_some()
                            })
                        } else {
                            false
                        };
                        reads.insert(entry.obligation, claim);
                        recovered_copy = !joined;
                    }
                    None => recovered_copy = true,
                }
                if recovered_copy
                    && (applied_proposals.current_standalone.is_some()
                        || !Box::pin(self.copied_successor(
                            &accepted,
                            &signers,
                            &current,
                            &ledger,
                            &exact_grant,
                        ))
                        .await?)
                {
                    return Ok(None);
                }
            }
            if ledger.count > accepted.ledger.count {
                compaction = Some(LedgerCompaction {
                    before: checkpoint
                        .later
                        .as_ref()
                        .unwrap_or(&approval.ledger_write)
                        .clone(),
                    after: evidence.clone(),
                    opening: reservation,
                });
            }
        }
        let Some(at) = self.service_time() else {
            return Ok(None);
        };
        let needs_inventory = match source {
            LedgerRead::Inventory => true,
            LedgerRead::Outcome => {
                checkpoint.restored || current.revision != accepted.application.revision
            }
            LedgerRead::Applied(revision) if revision == current.revision => recovered_copy,
            LedgerRead::Applied(_) => return Ok(None),
        };
        if compaction.is_some() {
            signers.retain(&ledger);
        }
        let result = |usage, ledger, grant, compaction, facts| ReconciledLedger {
            signers,
            facts,
            applied_proposals: applied_proposals.suffixes,
            compaction,
            at_initial_ledger: current.revision == approval.ledger.revision,
            at,
            terms: proposed.terms.clone(),
            ledger,
            usage,
            evidence: evidence.clone(),
            grant,
            current: Condition::RecordCurrent {
                key: ledger_key,
                revision: current.revision,
                deleted: false,
            },
        };
        if !needs_inventory {
            let Ok(counted) = usage(&proposed.terms, &ledger, &HashMap::new(), at) else {
                return Ok(None);
            };
            return Ok(Some(result(
                counted,
                ledger,
                exact_grant,
                compaction,
                Default::default(),
            )));
        }
        let Some(inventory) = wallet_inventory(&*self.rail, self.identity()).await else {
            return Ok(None);
        };
        let mut compactions: Vec<_> = if ledger.count == 0 {
            Vec::new()
        } else {
            self.local_scan::<LedgerCompaction>(&key::ledger_compactions(&ledger_key))
                .await?
                .into_iter()
                .map(|(_, row)| (row, Purpose::Established))
                .collect()
        };
        if let Some(receipt) = &compaction {
            if !compactions.iter().any(|(held, _)| held == receipt) {
                compactions.push((receipt.clone(), Purpose::Evidence));
            }
        }
        // An open claim reserves its entry even before dispatch (Paykit spec 8.3).
        // Terminal entries only need a claim read when attributing a wallet attempt.
        let dispatched: HashMap<_, _> = inventory.iter().map(|d| (d.attempt_id, d)).collect();
        let mut settled_payments = Vec::new();
        let entries: Vec<_> = ledger
            .entries
            .iter()
            .filter(|entry| {
                !claims.contains_key(&entry.attempt_id)
                    && (entry.outcome != Some(EntryOutcome::Done)
                        || dispatched.contains_key(&entry.attempt_id))
            })
            .collect();
        let failed = &ledger.failed;
        for obligation in entries
            .iter()
            .map(|e| e.obligation)
            .chain(failed.iter().map(|e| e.obligation))
        {
            if let Entry::Vacant(slot) = reads.entry(obligation) {
                slot.insert(self.app.read_claim(&obligation).await?);
            }
        }
        if !entries.is_empty() || !failed.is_empty() {
            let ops = self.app.shared_evidence().await?;
            let results = super::evidence::ClaimResults::new(
                &self.app,
                &ops.ops,
                &checkpoint.signers,
                &accepted,
                &current,
            );
            let accepted_entries: HashMap<_, _> = accepted
                .ledger
                .entries
                .iter()
                .map(|e| (e.attempt_id, e))
                .collect();
            let openings = || {
                ops.ops.iter().map(|op| (op, Purpose::Evidence)).chain(
                    compactions.iter().filter_map(|(receipt, purpose)| {
                        receipt.opening.as_ref().map(|op| (op, *purpose))
                    }),
                )
            };
            for entry in entries {
                let read = reads.get(&entry.obligation).and_then(Option::as_ref);
                let candidates: Vec<_> = read
                    .map(|read| {
                        openings()
                            .filter_map(|(op, purpose)| {
                                let op = checkpoint.signers.original(entry.attempt_id, op)?;
                                accepted.application.opening_for(
                                    &self.app,
                                    &op,
                                    &entry.obligation,
                                    entry.attempt_id,
                                    read,
                                    Some((&exact_grant, entry.at)),
                                    purpose,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let class = classify_claim(
                    read,
                    entry.attempt_id,
                    entry.obligation,
                    &candidates,
                    |claim, seq| results.confirms(entry.attempt_id, claim, seq, None),
                );
                let shown = match class {
                    ClaimClass::Verified(claim) => {
                        claims.insert(entry.attempt_id, claim);
                        continue;
                    }
                    ClaimClass::Shown(shown) => shown,
                };
                let Some(read) = read else {
                    if shown != super::ClaimShows::Gone {
                        unverified.insert(entry.attempt_id, shown);
                    }
                    continue;
                };
                if let Some(payment) = dispatched.get(&entry.attempt_id).filter(|payment| {
                    matches!(payment.outcome, crate::rail::RailOutcome::Paid(_))
                        && settled_after_accepted_payment(
                            read,
                            accepted_entries.get(&entry.attempt_id).copied(),
                            accepted.application.at_ms,
                            entry,
                            self.app_id(),
                        )
                }) {
                    claims.insert(
                        entry.attempt_id,
                        super::ReservationClaim {
                            obligation: entry.obligation,
                            state: paykit_lib::allowances::ClaimState::Done,
                        },
                    );
                    settled_payments.push((*payment).clone());
                } else if read.view.rebuilt
                    || read.view.divergent
                    || read.view.attempt(&entry.attempt_id.to_bytes()).is_some()
                {
                    // An uncertain view or an attempt without its bound opening doesn't
                    // establish that this claim has disappeared.
                    return Ok(None);
                } else if shown != super::ClaimShows::Gone {
                    unverified.insert(entry.attempt_id, shown);
                }
            }
            for entry in failed {
                let read = reads.get(&entry.obligation).and_then(Option::as_ref);
                let candidates: Vec<_> = read
                    .map(|read| {
                        openings()
                            .filter_map(|(op, purpose)| {
                                let op = checkpoint.signers.original(entry.attempt_id, op)?;
                                current.opening_for(
                                    &self.app,
                                    &op,
                                    &entry.obligation,
                                    entry.attempt_id,
                                    read,
                                    Some((&exact_grant, 0)),
                                    purpose,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // What the claim shows counts as the shared rule says; nothing is decided
                // here, before the proofs are read and copied (Paykit spec 8.3).
                match classify_claim(
                    read,
                    entry.attempt_id,
                    entry.obligation,
                    &candidates,
                    |claim, seq| results.confirms(entry.attempt_id, claim, seq, None),
                ) {
                    ClaimClass::Verified(claim) => {
                        claims.insert(entry.attempt_id, claim);
                    }
                    ClaimClass::Shown(super::ClaimShows::Gone) => {}
                    ClaimClass::Shown(shown) => {
                        unverified.insert(entry.attempt_id, shown);
                    }
                }
            }
        }
        let (facts, undated) =
            Box::pin(self.allowance_outcomes(&ledger, &claims, &dispatched)).await?;
        let undated: std::collections::HashSet<_> = undated.into_iter().collect();
        let mut retired = if ledger.count == 0 {
            HashMap::new()
        } else {
            let read = CompactionRead {
                target: &ledger_target,
                key: ledger_key,
                accepted: &accepted,
                grant: exact_grant,
                current: &current,
                ledger: &ledger,
            };
            let Some(retired) = read.payments(&self.app, &compactions) else {
                return Ok(None);
            };
            retired
        };
        // The count and the outcomes copied into the ledger take the same verdicts.
        let Some(Accounting {
            counted,
            unclassified,
            writes,
        }) = reconciled_usage(
            inventory,
            &proposed.terms,
            &ledger,
            &claims,
            &unverified,
            &mut retired,
            facts,
            &undated,
            at,
        )
        else {
            return Ok(None);
        };
        if !unclassified.is_empty() || !settled_payments.is_empty() {
            let records = self.payer_records().await?;
            for dispatch in unclassified.into_iter().chain(settled_payments) {
                let crate::rail::RailOutcome::Paid(method) = &dispatch.outcome else {
                    return Ok(None);
                };
                let mut related = records.iter().filter_map(|activity| {
                    let proof = activity.proof.as_ref()?;
                    let Body::PaymentProof(paid) = proof.body() else {
                        return None;
                    };
                    (paid.attempt_id == dispatch.attempt_id).then_some((activity, proof, paid))
                });
                let Some((activity, proof, paid)) = related.next() else {
                    return Ok(None);
                };
                if related.next().is_some()
                    || proof.sender() != self.identity()
                    || paid.proof != method.proof
                {
                    return Ok(None);
                }
                let natural = activity.natural_key_with(&self.identity(), &|endpoint, proof| {
                    self.rail.settlement_id(endpoint, proof)
                })?;
                let target = crate::activity::ActivityTarget::payer(natural).record_target();
                if let RecordRead::Kept { reason, .. } = self.app.read_record_fresh(&target).await?
                {
                    if let Some(stop) = crate::pay::kept_read_stop(&reason) {
                        return Err(stop);
                    }
                }
                let dated = self.app.shared_evidence().await?.ops.iter().any(|op| {
                    AppliedRecord::retained(&self.app, &target, op)
                        .is_some_and(|write| write.dates_payment_proof(&self.app, proof))
                });
                if !dated {
                    return Ok(None);
                }
                let binding = activity.binding()?;
                let (request, occurrence) = match binding {
                    RecordBinding::Request { .. } => (
                        crate::periods::request_for_payment(activity, activity, &binding),
                        None,
                    ),
                    RecordBinding::Period { billing_period, .. } => (
                        records.iter().find_map(|parent| {
                            crate::periods::request_for_payment(parent, activity, &binding)
                        }),
                        Some(billing_period),
                    ),
                    RecordBinding::Unrequested => (None, None),
                };
                let requested = request.and_then(|(original, _)| {
                    RequestedPayment::bind(activity, original, proof, occurrence)
                });
                if binding != RecordBinding::Unrequested && requested.is_none() {
                    return Ok(None);
                }
                let signed_amount = match &requested {
                    Some(verified) => verified.requested_amount(),
                    None => paid.amount.as_ref(),
                };
                let obligation = requested.as_ref().map(|payment| {
                    self.obligation(
                        &activity.counterparty,
                        &payment.request.hash(),
                        occurrence.map_or(0, |p| p.start_secs()),
                    )
                });
                let this_allowance = activity.counterparty == checkpoint.counterparty
                    && paid.allowance_id.as_ref() == Some(&id);
                if let Some(entry) = ledger
                    .entries
                    .iter()
                    .find(|e| e.attempt_id == dispatch.attempt_id)
                {
                    // Only a paid tombstone reaches this branch. Its accepted reservation
                    // pins the amount already matched to the method, including rounding.
                    if !this_allowance
                        || proof.cert().app_id() != self.app_id()
                        || obligation != Some(entry.obligation)
                        || signed_amount.is_none_or(|amount| amount.asset != entry.asset)
                    {
                        return Ok(None);
                    }
                    continue;
                }
                let failed = ledger
                    .failed
                    .iter()
                    .find(|entry| entry.attempt_id == dispatch.attempt_id);
                let compacted = retired.get(&dispatch.attempt_id);
                let (compacted, failed) = match (compacted, failed) {
                    (Some(Retired::PreviouslyFailed(_)), Some(_)) => return Ok(None),
                    (Some(Retired::PreviouslyFailed(entry)), None) => (None, Some(entry)),
                    (Some(Retired::Paid(entry)), failed) => (Some(entry), failed),
                    (None, failed) => (None, failed),
                };
                if compacted.is_some() || failed.is_some() {
                    if !this_allowance
                        || signed_amount
                            .is_none_or(|nominal| nominal.asset != dispatch.amount.asset)
                        || !obligation.is_some_and(|obligation| {
                            compacted_transfer_matches(
                                obligation,
                                &dispatch.amount,
                                compacted,
                                failed,
                                &proposed.terms.asset,
                            )
                        })
                    {
                        return Ok(None);
                    }
                    retired.remove(&dispatch.attempt_id);
                    continue;
                }
                if signed_amount != Some(&dispatch.amount) {
                    let Some(obligation) = obligation.filter(|_| !this_allowance) else {
                        return Ok(None);
                    };
                    let entry = self
                        .journal_entry_on(&dispatch.attempt_id, &obligation)
                        .await?;
                    let amount_proven = match entry {
                        Some(entry) if entry.transfer_bound.is_some() => {
                            entry.paid_amount()?.as_ref() == Some(&dispatch.amount)
                                && self.journal_matches_paid_proof(&entry, proof)?
                        }
                        _ => requested.as_ref().is_some_and(|verified| {
                            verified.receipted_amount(activity, &dispatch.amount)
                        }),
                    };
                    if !amount_proven {
                        return Ok(None);
                    }
                }
                if !this_allowance {
                    continue;
                }
                if let Some(obligation) = obligation {
                    // The opening after the accepted ledger excludes lawful compaction
                    // before that checkpoint. A direct reservation successor only
                    // compacts existing final entries by the ordinary ledger rule.
                    if !reads.contains_key(&obligation) {
                        let _ = self.app.read_claim(&obligation).await?;
                    }
                    if self.app.shared_evidence().await?.ops.iter().any(|op| {
                        accepted.application.opening_after(
                            &self.app,
                            op,
                            &exact_grant,
                            &obligation,
                            dispatch.attempt_id,
                        )
                    }) {
                        let alarm = self
                            .app
                            .raise_rollback(crate::protocol::BACKUP_NAME, MISSING_PAYMENT)
                            .await?;
                        return Err(pubky_switch_client::app::AppError::Alarm(alarm).into());
                    }
                }
                return Ok(None);
            }
        }
        // A later signed claim can confirm a compacted failure even after method
        // history has expired. Its accepted transition already accounts for it once.
        if retired
            .values()
            .any(|entry| matches!(entry, Retired::Paid(_)))
        {
            return Ok(None);
        }
        Ok(Some(result(
            counted,
            ledger,
            exact_grant,
            compaction,
            writes,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paykit_lib::allowances::EntryOutcome;
    use paykit_lib::{Asset, AttemptId, Decimal};

    #[test]
    fn test_journal_less_period_receipt_uses_the_original_requests_selling_scope() {
        check_receipted_period(TestPeriod::FirstFixed, "subscriptions", None, true);
    }

    #[test]
    fn test_journal_less_fixed_rate_period_with_a_signed_destination_checks_receipt_scope() {
        check_receipted_period(TestPeriod::LaterFixed, "subscriptions", None, true);
    }

    #[test]
    fn test_journal_less_later_destination_proof_matches_the_original_label_and_reference() {
        check_receipted_period(
            TestPeriod::LaterFixed,
            "subscriptions",
            Some("subscription-1"),
            true,
        );
        check_receipted_period(
            TestPeriod::LaterFixed,
            "shipping",
            Some("subscription-1"),
            false,
        );
        check_receipted_period(
            TestPeriod::LaterFixed,
            "subscriptions",
            Some("subscription-2"),
            false,
        );
    }

    #[test]
    fn test_pruned_same_asset_period_needs_its_request_and_signed_destination() {
        check_receipted_period(
            TestPeriod::SameAsset,
            "subscriptions",
            Some("subscription-1"),
            true,
        );
        check_receipted_period(
            TestPeriod::SameAsset,
            "shipping",
            Some("subscription-1"),
            false,
        );
        check_receipted_period(
            TestPeriod::SameAsset,
            "subscriptions",
            Some("different"),
            false,
        );
    }

    #[test]
    fn test_one_time_history_checks_the_proofs_original_request() {
        use crate::testkit::{object, Party};
        use paykit_lib::records::ActivitySide;
        use serde_json::json;

        let payer = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let shop = Party::new(
            "bob",
            "counter",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let request = shop.message(
            &payer,
            "paykit.payment_request",
            vec![],
            object(json!({
                "amount": {"asset": "btc", "value": "2"}, "proposal_expires_at": null,
                "payment_reference": "order-2"
            })),
        );
        let record = ActivityRecord::new(ActivitySide::Payer, shop.id());
        for (reference, matches) in [("order-2", true), ("other-order", false)] {
            let proof = payer.message(&shop, "paykit.payment_proof", vec![request.hash()], object(json!({
                "attempt_id": AttemptId::from_bytes([2; 16]).to_text(),
                "payment_reference": reference, "payment_endpoint_identifier": "btc-lightning-bolt11",
                "payment_app_id": pubky_switch_core::enc::zbase32_encode(&shop.app_id()), "proof": {}
            })));
            assert_eq!(
                RequestedPayment::bind(&record, &request, &proof, None)
                    .and_then(|verified| verified.requested_amount())
                    .is_some(),
                matches
            );
        }
    }

    #[derive(Clone, Copy)]
    enum TestPeriod {
        FirstFixed,
        LaterFixed,
        SameAsset,
    }

    fn check_receipted_period(
        example: TestPeriod,
        proof_label: &str,
        payment_reference: Option<&str>,
        proof_matches: bool,
    ) {
        use crate::testkit::{object, Party, NOW};
        use paykit_lib::bodies::{PaymentReceipt, PeriodDetails};
        use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
        use paykit_lib::records::ActivitySide;
        use serde_json::json;

        let later_destination = !matches!(example, TestPeriod::FirstFixed);
        let same_asset = matches!(example, TestPeriod::SameAsset);
        let payer = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let shop = Party::new(
            "bob",
            "subscriptions",
            &[
                "paykit_request:subscriptions",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
        let schedule =
            Recurrence::new(AnchoredRule::new(NOW, 1, Unit::Hour).unwrap(), NOW, None).unwrap();
        let period = PeriodRef::from(schedule.periods(2).unwrap()[usize::from(later_destination)]);
        let mut subscription = object(json!({
            "amount": { "asset": "usd", "value": "1" },
            "payment_reference": "subscription-1", "proposal_expires_at": null,
            "label": "subscriptions", "recurrence": schedule.to_json(),
            "conversion": { "type": "fixed", "rates": [{ "asset": "btc", "value": "2" }] }
        }));
        if same_asset {
            subscription.insert("amount".into(), json!({"asset":"btc", "value":"2"}));
            subscription.remove("conversion");
        }
        if later_destination {
            subscription.insert("per_period_endpoints".into(), json!(true));
            subscription.insert(
                "required_app_id".into(),
                json!(pubky_switch_core::enc::zbase32_encode(&shop.app_id())),
            );
            subscription.insert(
                "accepted_payment_endpoint_identifiers".into(),
                json!(["btc-lightning-bolt11"]),
            );
        }
        let request = shop.message(&payer, "paykit.payment_request", vec![], subscription);
        let selected = later_destination.then(|| {
            let details = PeriodDetails {
                billing_period: period,
                payment_endpoints: Some(std::collections::BTreeMap::from([(
                    paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
                    paykit_lib::PaymentEndpointPayload::new("lnbc1period").unwrap(),
                )])),
                rates: None,
                expires_at: period.ends_at,
            }
            .to_body()
            .unwrap();
            shop.message(
                &payer,
                "paykit.payment_period_details",
                vec![request.hash()],
                details,
            )
        });
        let mut references = vec![request.hash()];
        if let Some(detail) = &selected {
            references.push(detail.hash());
        }
        let attempt = AttemptId::from_bytes([3; 16]);
        let mut proof_body = object(json!({
            "attempt_id": attempt.to_text(), "billing_period": period.to_json(),
            "payment_endpoint_identifier": "btc-lightning-bolt11",
            "payment_app_id": pubky_switch_core::enc::zbase32_encode(&shop.app_id()),
            "label": proof_label, "proof": {}
        }));
        if let Some(reference) = payment_reference {
            proof_body.insert("payment_reference".into(), json!(reference));
        }
        let proof = payer.message(&shop, "paykit.payment_proof", references, proof_body);
        let transfer = PaymentAmount {
            asset: Asset::new("btc").unwrap(),
            value: Decimal::new("2").unwrap(),
        };
        let body = PaymentReceipt {
            attempt_id: attempt,
            billing_period: Some(period),
            metadata: None,
            payment_reference: None,
        }
        .to_body(2)
        .unwrap();
        let mut parent = ActivityRecord::new(ActivitySide::Payer, shop.id());
        parent.request = Some(request.clone());
        let mut activity = ActivityRecord::new(ActivitySide::Payer, shop.id());
        activity.request_hash = Some(request.hash());
        activity.billing_period = Some(period);
        activity.proof = Some(proof.clone());
        activity.period_details = selected;
        if same_asset {
            // After the journal is gone the method amount equals the signed nominal amount.
            // Validate the request and selected carrier before that equality can classify it.
            let Body::PaymentRequest(spec) = request.body() else {
                unreachable!()
            };
            assert_eq!(spec.amount, transfer);
            assert_eq!(
                RequestedPayment::bind(&activity, &request, &proof, Some(period))
                    .and_then(|payment| payment.requested_amount()),
                proof_matches.then_some(&transfer)
            );
            let selected = activity.period_details.take();
            assert!(RequestedPayment::bind(&activity, &request, &proof, Some(period)).is_none());
            activity.period_details = selected;
        }
        for (scope, allowed) in [
            ("paykit_request:subscriptions", true),
            ("paykit_request", true),
            ("paykit_request:shipping", false),
        ] {
            let counter = Party::new(
                "bob",
                scope,
                &[scope, "paykit_settlements", "paykit_contacts"],
            );
            activity.receipt = Some(counter.message(
                &payer,
                "paykit.payment_receipt",
                vec![proof.hash(), request.hash()],
                body.clone(),
            ));
            if proof_matches {
                assert_eq!(
                    crate::receipts::receipt_binding(&activity, Some(&parent))
                        == crate::receipts::ReceiptBinding::Verified,
                    allowed,
                    "{scope}"
                );
            }
            assert_eq!(
                RequestedPayment::bind(&activity, &request, &proof, Some(period))
                    .is_some_and(|verified| verified.receipted_amount(&activity, &transfer)),
                allowed && proof_matches,
                "{scope}, {proof_label}, {payment_reference:?}"
            );
        }
    }

    #[test]
    fn test_restore_classifies_every_claim_read_for_counting() {
        use super::super::{ClaimShows, ReservationClaim};
        use pubky_switch_client::app::ClaimRead;
        use pubky_switch_core::claims::{AttemptView, ClaimStatus, ClaimView, Outcome};

        let attempt = AttemptId::from_bytes([4; 16]);
        let obligation = [2; 32];
        let claim = |state| ReservationClaim { obligation, state };
        let failed = ClaimState::Failed { by_owner: false };
        let read = |outcome: Option<Outcome>, rebuilt: bool, divergent: bool| ClaimRead {
            entries: Vec::new(),
            view: ClaimView {
                claim_key: obligation,
                status: ClaimStatus::Settled,
                owner: None,
                attempt_id: None,
                step: 2,
                attempts: outcome
                    .map(|outcome| AttemptView {
                        attempt_id: attempt.to_bytes(),
                        owner: [7; 16],
                        outcome,
                        opened_at: 1002,
                        orphan_after: 60,
                        orphaned: false,
                        possibly_duplicate: false,
                        duplicate: false,
                        resolved_by: Some([9; 16]),
                    })
                    .into_iter()
                    .collect(),
                tombstone: false,
                deadline: None,
                divergent,
                rebuilt,
                next_possibly_duplicate: false,
            },
        };
        // `held`: the outcomes this app holds signed; an open or `unknown` state always
        // verifies, as the real check says.
        let class = |read: Option<&ClaimRead>,
                     openings: &[(ReservationClaim, u64)],
                     held: &[ClaimState]| {
            classify_claim(read, attempt, obligation, openings, |shown, _| {
                matches!(shown.state, ClaimState::Open | ClaimState::Unknown)
                    || held.contains(&shown.state)
            })
        };
        let shows_failed = read(Some(Outcome::Failed), false, false);
        let opened = [(claim(failed), 3)];
        // A lagging read shows the failure while this app holds the later signed `done`.
        assert_eq!(
            class(Some(&shows_failed), &opened, &[failed, ClaimState::Done]),
            ClaimClass::Verified(claim(ClaimState::Done))
        );
        assert_eq!(
            class(Some(&shows_failed), &opened, &[failed]),
            ClaimClass::Verified(claim(failed))
        );
        assert_eq!(
            class(Some(&shows_failed), &[(claim(ClaimState::Open), 3)], &[]),
            ClaimClass::Verified(claim(ClaimState::Open))
        );
        // A stale read still shows the attempt open or `unknown` while this app holds the
        // later signed `done`: the attempt counts paid.
        for stale in [ClaimState::Open, ClaimState::Unknown] {
            assert_eq!(
                class(
                    Some(&read(Some(Outcome::Open), false, false)),
                    &[(claim(stale), 3)],
                    &[ClaimState::Done]
                ),
                ClaimClass::Verified(claim(ClaimState::Done)),
                "{stale:?}"
            );
        }
        // A signed `done` without its opening counts, whether the claim reads or is gone.
        assert_eq!(
            class(Some(&shows_failed), &[], &[ClaimState::Done]),
            ClaimClass::Shown(ClaimShows::SignedDone)
        );
        assert_eq!(
            class(None, &[], &[ClaimState::Done]),
            ClaimClass::Shown(ClaimShows::SignedDone)
        );
        // A `done` only the read shows, without its opening.
        assert_eq!(
            class(Some(&read(Some(Outcome::Done), false, false)), &[], &[]),
            ClaimClass::Shown(ClaimShows::ShownDone)
        );
        // Anything else without a verified opening leaves the recorded failure as it is: a
        // divergent or rebuilt view, or one showing the attempt open, `unknown` or failed.
        for other in [
            read(Some(Outcome::Failed), true, false),
            read(Some(Outcome::Failed), false, true),
            read(None, true, false),
            read(Some(Outcome::Open), false, false),
            read(Some(Outcome::Unknown), false, false),
        ] {
            assert_eq!(
                class(Some(&other), &[], &[]),
                ClaimClass::Shown(ClaimShows::Gone),
                "{:?}",
                other.view
            );
        }
        // The failure the ledger holds, shown without its opening, or nothing at all.
        assert_eq!(
            class(Some(&shows_failed), &opened, &[]),
            ClaimClass::Shown(ClaimShows::Gone)
        );
        assert_eq!(
            class(Some(&read(None, false, false)), &[], &[]),
            ClaimClass::Shown(ClaimShows::Gone)
        );
        assert_eq!(class(None, &[], &[]), ClaimClass::Shown(ClaimShows::Gone));
    }

    #[test]
    fn test_settled_claim_needs_an_exact_previously_accepted_payment() {
        use pubky_switch_client::app::ClaimRead;
        use pubky_switch_core::claims::{AttemptView, ClaimStatus, ClaimView, Outcome};

        let executor = [7; 16];
        let entry = LedgerEntry {
            obligation: [2; 32],
            attempt_id: AttemptId::from_bytes([4; 16]),
            asset: Asset::new("btc").unwrap(),
            value: Decimal::new("70").unwrap(),
            at: 1000,
            outcome: Some(EntryOutcome::Done),
        };
        let mut read = ClaimRead {
            entries: Vec::new(),
            view: ClaimView {
                claim_key: entry.obligation,
                status: ClaimStatus::Settled,
                owner: None,
                attempt_id: None,
                step: 2,
                attempts: vec![AttemptView {
                    attempt_id: entry.attempt_id.to_bytes(),
                    owner: executor,
                    outcome: Outcome::Done,
                    opened_at: 1002,
                    orphan_after: 60,
                    orphaned: false,
                    possibly_duplicate: false,
                    duplicate: false,
                    resolved_by: Some(executor),
                }],
                tombstone: true,
                deadline: None,
                divergent: false,
                rebuilt: false,
                next_possibly_duplicate: false,
            },
        };
        let matches = |read: &ClaimRead, previous: Option<&LedgerEntry>, current: &LedgerEntry| {
            settled_after_accepted_payment(read, previous, 1004, current, executor)
        };
        assert!(matches(&read, Some(&entry), &entry));
        assert!(!matches(&read, None, &entry));
        let mut different = entry.clone();
        different.value = Decimal::new("30").unwrap();
        assert!(!matches(&read, Some(&entry), &different));
        different = entry.clone();
        different.outcome = None;
        assert!(!matches(&read, Some(&different), &different));
        read.view.attempts[0].duplicate = true;
        assert!(!matches(&read, Some(&entry), &entry));
        read.view.attempts[0].duplicate = false;
        read.view.attempts[0].owner = [8; 16];
        assert!(!matches(&read, Some(&entry), &entry));
        read.view.attempts[0].owner = executor;
        read.view.rebuilt = true;
        assert!(!matches(&read, Some(&entry), &entry));
    }

    #[test]
    fn test_compacted_reservation_uses_its_original_transfer_amount() {
        let asset = Asset::new("btc").unwrap();
        let failed = CompactedFailed {
            obligation: [6; 32],
            attempt_id: AttemptId::from_bytes([5; 16]),
            value: Decimal::new("0.00000002").unwrap(),
        };
        let transferred = PaymentAmount {
            asset: asset.clone(),
            value: failed.value.clone(),
        };
        let nominal = PaymentAmount {
            asset: asset.clone(),
            value: Decimal::new("0.000000015").unwrap(),
        };
        let settled = LedgerEntry {
            obligation: failed.obligation,
            attempt_id: failed.attempt_id,
            value: transferred.value.clone(),
            asset: asset.clone(),
            at: 1_000,
            outcome: Some(EntryOutcome::Done),
        };
        assert!(compacted_transfer_matches(
            settled.obligation,
            &transferred,
            Some(&settled),
            None,
            &asset,
        ));
        assert!(!compacted_transfer_matches(
            settled.obligation,
            &nominal,
            Some(&settled),
            None,
            &asset,
        ));
        assert!(compacted_transfer_matches(
            failed.obligation,
            &transferred,
            None,
            Some(&failed),
            &asset
        ));
        assert!(!compacted_transfer_matches(
            [7; 32],
            &transferred,
            None,
            Some(&failed),
            &asset
        ));
        assert!(!compacted_transfer_matches(
            failed.obligation,
            &nominal,
            None,
            Some(&failed),
            &asset
        ));
        assert!(!compacted_transfer_matches(
            failed.obligation,
            &transferred,
            None,
            Some(&failed),
            &Asset::new("usd").unwrap()
        ));
        assert!(!compacted_transfer_matches(
            failed.obligation,
            &transferred,
            None,
            None,
            &asset
        ));
    }

    #[test]
    fn test_applied_reservation_compacts_old_entries_into_the_exact_totals() {
        let terms = AllowanceTerms::from_json(&serde_json::json!({
            "asset":"btc","lifetime_amount_limit":"10000","period_limits":[],
            "per_payment_amount":null,"active_from":null,"expires_at":null,
            "allowed_payment_endpoint_identifiers":null
        }))
        .unwrap();
        let mut held = Ledger::empty(
            [7; 32],
            AllowanceId::new("4b3fb324-091b-47da-a7ad-46b3a07d0409").unwrap(),
            [8; 32],
        );
        let entry = |n: u128, at, outcome| LedgerEntry {
            obligation: [6; 32],
            attempt_id: AttemptId::from_bytes(n.to_be_bytes()),
            value: Decimal::new("1").unwrap(),
            asset: Asset::new("btc").unwrap(),
            at,
            outcome,
        };
        // These completed within one window, before any entry could move into totals.
        while held.to_json().unwrap().len() < 197_000 {
            for _ in 0..50 {
                let n = held.entries.len() as u128;
                let outcome = if n == 0 {
                    EntryOutcome::Failed
                } else {
                    EntryOutcome::Done
                };
                held.entries.push(entry(n, 1_000, Some(outcome)));
            }
        }
        assert!(held.to_json().unwrap().len() < 256 * 1024 - 4);
        let next = entry(u128::MAX, 100_000_000, None);
        let mut applied =
            super::super::ledger::reserved_ledger(&terms, held.clone(), next.clone()).unwrap();
        assert_eq!(applied.entries, vec![next.clone()]);
        assert_eq!(applied.count, (held.entries.len() - 1) as u64);
        assert_eq!(applied.failed.len(), 1);
        let removed = super::super::compaction::retired_paid(held.clone(), &applied);
        assert_eq!(removed, held.entries[1..]);
        assert_eq!(removed.len() as u64, applied.count);
        assert_eq!(
            removed.iter().map(|e| e.value.scaled()).sum::<u128>(),
            applied.spent.as_ref().unwrap().scaled()
        );
        let failed = &held.entries[0];
        let belongs = super::super::spend::accounts_for_attempt;
        assert!(belongs(
            &held,
            failed.obligation,
            failed.attempt_id.to_bytes()
        ));
        assert!(belongs(
            &applied,
            failed.obligation,
            failed.attempt_id.to_bytes()
        ));
        assert!(!belongs(&applied, [9; 32], failed.attempt_id.to_bytes()));
        assert!(
            matches!(ledger_change(&held, &applied, &terms).unwrap(), Some(LedgerChange::Reservation(e)) if e == &next)
        );
        applied.count += 1;
        assert!(ledger_change(&held, &applied, &terms).unwrap().is_none());
        applied.count -= 1;
        applied.failed.clear();
        assert!(ledger_change(&held, &applied, &terms).unwrap().is_none());
    }
}
