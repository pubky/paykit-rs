//! Paying (Paykit spec 7.1, 7.2, 8.1, 8.4, 8.5): the paying order, with the payment journal.
//!
//! 1. **Attempt.** The transfer is checked first, then, before any write, the own
//!    `identity.json` (read again, and confirmed within the last 60 s) and the payee's; the
//!    payment is journaled; then the `attempt`
//!    is written on the obligation's claim, conditioned on the version of the request record
//!    the app read and carrying the request's deadline. A payment without a request writes
//!    its `payer_activity` record in the same batch, and a single-use endpoint its
//!    `payment_lists` mark. The client checks the attempt's proof: only a verified answer
//!    showing this app's own open attempt lets money move.
//! 2. **Check, then pay.** The own `identity.json` is still confirmed and the destination checked
//!    ([`crate::destination`]); a failure writes `failed` instead of paying. The start of the
//!    payment is journaled, then the rail pays with the attempt ID as its idempotency key,
//!    and a one-time request's acceptance goes out at the same time.
//! 3. **Outcome.** `done` (basis `owner`) or `failed` on the claim and the payer's activity
//!    record, with the acceptance and the proof, in one batch, which also queues the proof
//!    and finishes the journal entry once applied.
//! 4. **Proof**, to the payee, sent once that batch is applied.
//!
//! Every step commits before the next acts, so [`Paykit::resume_payments`] finishes a
//! payment a crash interrupted without paying twice (Paykit spec 7.2, "Starting to pay", "No
//! second execution").

use paykit_lib::bodies::{PaymentProof, PaymentRequestAcceptance, RequestDestination};
use paykit_lib::derive::{natural_key, payment_obligation};
use paykit_lib::kinds::Kind;
use paykit_lib::records::{
    ActivityRecord, ActivitySide, DestinationSource, PaymentListRecord, StoredMessage, UsedEndpoint,
};
use paykit_lib::{AttemptId, Body, Label, PaymentAmount, PaymentEndpointIdentifier};
use pubky_switch_client::app::{
    claim_conflict, AppError, ClaimOp, Initiator, ObligationDeadline, OpBatch,
    Refusal as ClientRefusal, SignedMessage, UserConfirmed, REVALIDATION_MS,
};
use pubky_switch_core::claims::{Basis, ClaimStatus, ClaimView, Outcome};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::message::MessagePlaintext;
use pubky_switch_core::ops::{Condition, OpKind};

use crate::activity::{self, ActivityTarget, Role};
use crate::destination::refused;
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::{Destination, JournalEntry, KeptAmount, PaymentView, Stage};
use crate::paykit::Paykit;
use crate::rail::{RailOutcome, RailPayment};
use crate::records::list_target;
use crate::requests::{cancelled, rejected, request_of, RequestRef};
use crate::state::{self, key};

/// How long after the latest time it has seen a payment without a request's attempt pins its
/// deadline: 10 minutes (Paykit spec 8.4).
pub const UNREQUESTED_DEADLINE_MS: u64 = 10 * 60_000;

/// Private or public payment, chosen explicitly (Paykit spec 8.4): a payer never changes from
/// one to the other on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaymentMode {
    /// A Private Payment List endpoint.
    #[default]
    Private,
    /// The payee's public endpoints, chosen by the user.
    Public(UserConfirmed),
}

/// A manual payment's choices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PayOptions {
    /// The Payment Endpoint Identifier to pay; the first usable one when `None`.
    pub endpoint: Option<PaymentEndpointIdentifier>,
    /// The payee app whose Private Payment List to pay, for a destination that isn't the
    /// request's own; any current one when `None`.
    pub list_app: Option<[u8; 16]>,
    /// Private or public, for a destination that isn't the request's own.
    pub mode: PaymentMode,
    /// The user has seen "may already be paid" (Paykit spec 7.1).
    pub may_be_paid_confirmed: Option<UserConfirmed>,
    /// The user has seen that the destination transfers another amount, or a fee above the
    /// app's cap, and pays it anyway (Paykit spec 7.2 step 1).
    pub transfer_confirmed: Option<UserConfirmed>,
    /// The user has seen that the payee's switch couldn't confirm the payee's apps, and pays
    /// anyway ([`Refusal::IdentityUnconfirmed`]; Switch spec 4.3, Paykit spec 7.2).
    pub unconfirmed_identity_confirmed: Option<UserConfirmed>,
}

/// The result of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayOutcome {
    /// Paid: `done` written, the proof queued.
    Paid {
        /// The attempt.
        attempt_id: AttemptId,
        /// The proof's `msg_id`.
        proof_msg_id: Option<String>,
    },
    /// Paid, but the outcome couldn't be written: the app isn't current any more. Its
    /// attempt is reconciled through the receipt or by another app (Paykit spec 7.3).
    PaidUnreported {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// Definitively not paid: `failed` written.
    Failed {
        /// The attempt.
        attempt_id: AttemptId,
        /// Why, without content.
        reason: String,
    },
    /// Nothing yet: the attempt stays open until the payment method reports (Paykit spec
    /// 7.2, "Failed means definitively not paid").
    Pending {
        /// The attempt.
        attempt_id: AttemptId,
    },
    /// The payment method's history can't tell: "may be paid", resolved only by a receipt or
    /// by the user (Paykit spec 7.2, 7.3).
    Uncertain {
        /// The attempt.
        attempt_id: AttemptId,
    },
}

/// A payment without a request (Paykit spec 8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnrequestedPayment {
    /// The payee.
    pub payee: [u8; 32],
    /// The amount.
    pub amount: PaymentAmount,
    /// The choices.
    pub options: PayOptions,
}

/// What one attempt needs beyond its journal entry.
struct Plan {
    entry: JournalEntry,
    record: ActivityTarget,
    carrier: StoredMessage,
    step: u64,
    /// The `payment_lists` record whose single-use mark the attempt's batch writes.
    reserve: Option<PaymentListRecord>,
    /// The record a payment without a request creates in the attempt's batch.
    create: Option<ActivityRecord>,
    /// Condition the attempt on the version of the request record read.
    on_record: bool,
    /// Who starts the payment: the user, or an executor (Switch spec 4.3).
    by: Initiator,
    /// The user's confirmation of a payee identity its switch couldn't confirm.
    unconfirmed: Option<UserConfirmed>,
}

fn not_now(refusal: ClientRefusal) -> PaykitError {
    Refusal::NotNow(refusal.to_string()).into()
}

/// How many times the outcome's batch is built again after `409 conflict`.
const OUTCOME_TRIES: usize = 3;

/// The deadline the app computes itself for an obligation (Switch wire 6.8): the attempt's,
/// or none.
pub(crate) fn own_deadline(deadline: Option<u64>) -> ObligationDeadline {
    match deadline {
        Some(at) => ObligationDeadline::At(at),
        None => ObligationDeadline::NoDeadline,
    }
}

impl Paykit {
    /// Pay a one-time request by hand (Paykit spec 7.2, 8.1 step 3).
    ///
    /// # Errors
    ///
    /// A [`Refusal`] for a request that isn't payable: rejected, cancelled, paid, being paid
    /// by another own app, may already be paid, expired, recurring, without a usable
    /// destination, or a destination refused by the checks before money moves (then
    /// `failed` is written); [`PaykitError::Conflict`] when the request record changed
    /// meanwhile; the client's errors. A failure after money may have moved is never an
    /// error: it is [`PayOutcome::Pending`] or [`PayOutcome::Uncertain`].
    pub async fn pay_request(
        &mut self,
        request: &RequestRef,
        options: PayOptions,
    ) -> Result<PayOutcome> {
        self.require_pay()?;
        self.app.may_attempt(false).map_err(not_now)?;
        let (target, record) = self.payer_request(request).await?;
        let (stored, body) = request_of(&record).ok_or(Refusal::NotFound)?;
        let (stored, body) = (stored.clone(), body.clone());
        if body.is_recurring() {
            return Err(Refusal::Recurring.into());
        }
        if record.shows_paid() {
            return Err(Refusal::AlreadyPaid.into());
        }
        if rejected(&record) {
            return Err(Refusal::Rejected.into());
        }
        // A payer that has recorded a cancellation makes no new attempt (Paykit spec 8.1).
        if cancelled(&record) {
            return Err(Refusal::Cancelled.into());
        }
        let obligation = self.obligation(&request.payee, &request.request_hash, 0);
        let deadline = body.attempt_deadline(None)?;
        // No attempt once the deadline is at or before the highest service time verified
        // (Paykit spec 7.1); the service refuses one from its own clock anyway.
        if self.deadline_passed(deadline) {
            return Err(Refusal::DeadlinePassed.into());
        }
        self.refuse_open_entry(&obligation, &options).await?;
        let step = self.claim_step(&obligation, deadline, &options).await?;
        let (destination, carrier, reserve) = match &body.destination {
            RequestDestination::Endpoints(endpoints) => {
                let allowed = |id: &PaymentEndpointIdentifier| {
                    body.accepted_payment_endpoint_identifiers
                        .as_ref()
                        .is_none_or(|ids| ids.contains(id))
                };
                let chosen = match &options.endpoint {
                    Some(id) => endpoints.get_key_value(id),
                    None => endpoints.iter().find(|(id, _)| allowed(id)),
                };
                let Some((id, payload)) = chosen.filter(|(id, _)| allowed(id)) else {
                    return Err(Refusal::NoDestination(
                        "Paykit spec 8.1: the request has no such endpoint".into(),
                    )
                    .into());
                };
                let destination = Destination::new(
                    stored.message().from.app_id,
                    id,
                    payload,
                    DestinationSource::Request,
                    None,
                    false,
                );
                (destination, stored.clone(), None)
            }
            RequestDestination::RequiredApp(app) => {
                self.list_destination(&request.payee, Some(*app), &options)
                    .await?
            }
            RequestDestination::Any => {
                self.list_destination(&request.payee, options.list_app, &options)
                    .await?
            }
        };
        self.check_transfer(&destination, &body.amount, &options)?;
        let entry = JournalEntry {
            attempt_id: AttemptId::generate()?.to_text(),
            obligation,
            payee: request.payee,
            request_hash: Some(pubky_switch_core::enc::b64(&request.request_hash)),
            natural_key: target.natural_key.clone(),
            amount: KeptAmount::of(&body.amount),
            destination,
            deadline,
            stage: Stage::Recorded,
            step: None,
            label: body.label.as_ref().map(|l| l.as_str().to_owned()),
            payment_reference: Some(body.payment_reference.as_str().to_owned()),
            acceptance_owed: record.response.is_none(),
            acceptance_msg_id: None,
            acceptance_plaintext: None,
            rail_proof: None,
            paid_at_ms: None,
            proof_msg_id: None,
            restored: false,
            journaled_at_ms: self.now(),
        };
        self.execute(Plan {
            entry,
            record: target,
            carrier,
            step,
            reserve,
            create: None,
            on_record: true,
            by: Initiator::User,
            unconfirmed: options.unconfirmed_identity_confirmed,
        })
        .await
    }

    /// Pay without a request (Paykit spec 8.4): to the payee's Private Payment List, or its
    /// public endpoints when the user chose them. The attempt, its `payer_activity` record and
    /// a single-use endpoint's mark go in one batch, so two own apps can't both pay one
    /// single-use endpoint.
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_request`]; [`PaykitError::Conflict`] when the `payment_lists` record
    /// changed meanwhile, after which the caller pays to the next usable endpoint.
    pub async fn pay_without_request(&mut self, payment: UnrequestedPayment) -> Result<PayOutcome> {
        self.require_pay()?;
        self.app.may_attempt(false).map_err(not_now)?;
        let options = payment.options;
        let (destination, carrier, reserve) = self
            .list_destination(&payment.payee, options.list_app, &options)
            .await?;
        self.check_transfer(&destination, &payment.amount, &options)?;
        let attempt_id = AttemptId::generate()?;
        let own = self.identity();
        let natural = natural_key::payer_payment_record(&own, &attempt_id);
        let mut record = activity::empty(ActivitySide::Payer, payment.payee);
        record.attempt_id = Some(attempt_id);
        record.unrequested = Some(crate::receive::unrequested_of(&destination)?);
        record.status = Some("paying".into());
        let entry = JournalEntry {
            attempt_id: attempt_id.to_text(),
            obligation: payment_obligation(&attempt_id),
            payee: payment.payee,
            request_hash: None,
            natural_key: natural.clone(),
            amount: KeptAmount::of(&payment.amount),
            destination,
            // 10 minutes after the latest service time the app has seen (Paykit spec 8.4).
            deadline: Some(
                self.service_time()
                    .unwrap_or_else(|| self.now())
                    .saturating_add(UNREQUESTED_DEADLINE_MS),
            ),
            stage: Stage::Recorded,
            step: None,
            label: None,
            payment_reference: None,
            acceptance_owed: false,
            acceptance_msg_id: None,
            acceptance_plaintext: None,
            rail_proof: None,
            paid_at_ms: None,
            proof_msg_id: None,
            restored: false,
            journaled_at_ms: self.now(),
        };
        self.execute(Plan {
            entry,
            record: ActivityTarget::payer(natural),
            carrier,
            step: 0,
            reserve,
            create: Some(record),
            on_record: false,
            by: Initiator::User,
            unconfirmed: options.unconfirmed_identity_confirmed,
        })
        .await
    }

    /// The payments in the journal.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn payments(&self) -> Result<Vec<PaymentView>> {
        self.journal()
            .await?
            .iter()
            .map(JournalEntry::view)
            .collect()
    }

    pub(crate) async fn journal(&self) -> Result<Vec<JournalEntry>> {
        Ok(self
            .local_scan::<JournalEntry>(key::JOURNAL)
            .await?
            .into_iter()
            .map(|(_, entry)| entry)
            .collect())
    }

    async fn save(&mut self, entry: &JournalEntry) -> Result<()> {
        self.local_put(&key::journal(&entry.attempt()?), entry)
            .await
    }

    async fn set_stage(&mut self, entry: &mut JournalEntry, stage: Stage) -> Result<()> {
        entry.stage = stage;
        self.save(entry).await
    }

    /// An open journal entry for the obligation: this app is paying it already, or a restore
    /// left it unresolved ("may be paid") unless the user confirmed (Paykit spec 7.2).
    async fn refuse_open_entry(&self, obligation: &[u8; 32], options: &PayOptions) -> Result<()> {
        for entry in self.journal().await? {
            if &entry.obligation != obligation || !entry.open() {
                continue;
            }
            if entry.restored {
                if options.may_be_paid_confirmed.is_none() {
                    return Err(Refusal::MayBePaid(
                        "Switch spec 12.4: a restored attempt is unresolved".into(),
                    )
                    .into());
                }
            } else {
                return Err(Refusal::PaymentInProgress.into());
            }
        }
        Ok(())
    }

    /// The step to attempt at, after the claim's checks (Paykit spec 7.1, 7.2): a SETTLED
    /// claim is paid; another open attempt is another wallet's; a pinned deadline other
    /// than this app's, or a doubtful claim, may already be paid.
    async fn claim_step(
        &mut self,
        obligation: &[u8; 32],
        deadline: Option<u64>,
        options: &PayOptions,
    ) -> Result<u64> {
        let Some(read) = self.app.read_claim(obligation).await? else {
            return Ok(0);
        };
        let view = read.view;
        self.refuse_claim(&view)?;
        if !view.attempts.is_empty() && view.deadline != deadline {
            return Err(Refusal::MayBePaid(
                "Paykit spec 7.1: the claim's pinned deadline isn't this app's".into(),
            )
            .into());
        }
        let doubtful = self
            .app
            .claim_doubt(obligation)
            .await?
            .is_some_and(|d| d.doubtful())
            || view.next_possibly_duplicate
            || view.attempts.iter().any(|a| a.outcome == Outcome::Unknown);
        if doubtful && options.may_be_paid_confirmed.is_none() {
            return Err(Refusal::MayBePaid("Paykit spec 7.1: a doubtful claim".into()).into());
        }
        Ok(view.step)
    }

    fn refuse_claim(&self, view: &ClaimView) -> Result<()> {
        match view.status {
            ClaimStatus::Settled => Err(Refusal::AlreadyPaid.into()),
            ClaimStatus::Attempting if view.owner == Some(self.app_id()) => {
                Err(Refusal::PaymentInProgress.into())
            }
            ClaimStatus::Attempting => Err(Refusal::BeingHandledByOtherWallet.into()),
            ClaimStatus::Free => Ok(()),
        }
    }

    /// A destination from the payee's Private Payment Lists (Paykit spec 8.4): the newest
    /// usable list of the app asked for, or of any app, and an endpoint of it that isn't a
    /// single-use endpoint already used at that version. Public endpoints are app records',
    /// not read yet.
    async fn list_destination(
        &mut self,
        payee: &[u8; 32],
        app: Option<[u8; 16]>,
        options: &PayOptions,
    ) -> Result<(Destination, StoredMessage, Option<PaymentListRecord>)> {
        if let PaymentMode::Public(_) = options.mode {
            crate::app_records::public_endpoints(payee)?;
        }
        let now = self.now();
        let mut lists = self.lists_of(payee).await?;
        lists.retain(|l| app.is_none_or(|a| a == l.app_id));
        lists.sort_by_key(|l| match l.list.body() {
            Body::PrivatePaymentList(list) => std::cmp::Reverse(list.version),
            _ => std::cmp::Reverse(0),
        });
        for record in lists {
            let Body::PrivatePaymentList(list) = record.list.body() else {
                continue;
            };
            let accepted = record.list.accepted_at().unwrap_or(0);
            if now >= list.usable_until(accepted) {
                continue;
            }
            let used = |id: &PaymentEndpointIdentifier| {
                record
                    .used
                    .iter()
                    .any(|u| u.version == list.version && &u.endpoint == id)
            };
            let chosen = list.payment_endpoints.iter().find(|(id, endpoint)| {
                options.endpoint.as_ref().is_none_or(|want| want == *id)
                    && !(endpoint.single_use && used(id))
            });
            if let Some((id, endpoint)) = chosen {
                let destination = Destination::new(
                    record.app_id,
                    id,
                    &endpoint.payload,
                    DestinationSource::List,
                    Some(list.version),
                    endpoint.single_use,
                );
                let reserve = endpoint.single_use.then(|| record.clone());
                return Ok((destination, record.list.clone(), reserve));
            }
        }
        Err(Refusal::NoDestination(
            "Paykit spec 8.4: no usable Private Payment List; ask the payee for a newer one".into(),
        )
        .into())
    }

    /// The transfer check (Paykit spec 7.2 step 1): the method transfers exactly the amount
    /// paid, and any fee the payee can influence is within the app's cap, unless the user
    /// chose otherwise.
    fn check_transfer(
        &self,
        destination: &Destination,
        amount: &PaymentAmount,
        options: &PayOptions,
    ) -> Result<()> {
        if options.transfer_confirmed.is_some() {
            return Ok(());
        }
        let transfer =
            self.rail
                .transfer(&destination.endpoint_id()?, &destination.payload()?, amount);
        if transfer.amount.as_ref() != Some(amount) {
            return Err(Refusal::Transfer(
                "the destination transfers another amount or asset, or one the method can't tell"
                    .into(),
            )
            .into());
        }
        if let Some(fee) = &transfer.max_fee {
            if self.config.fee_cap.as_ref().is_none_or(|cap| fee > cap) {
                return Err(Refusal::Transfer("a fee above the app's cap".into()).into());
            }
        }
        Ok(())
    }

    /// Steps 1 to 4 of the paying order for a planned attempt (Paykit spec 7.2).
    async fn execute(&mut self, plan: Plan) -> Result<PayOutcome> {
        let Plan {
            mut entry,
            record,
            carrier,
            step,
            reserve,
            create,
            on_record,
            by,
            unconfirmed,
        } = plan;
        let attempt_id = entry.attempt()?;
        // The own identity.json, read again and confirmed, before any write: no attempt
        // without a copy revalidated in the last 60 s (Paykit spec 4, Switch spec 12.6).
        self.own_identity_confirmed(true).await?;
        // The payee's identity.json, before any write: a payment the user started whose
        // payee's switch couldn't confirm it needs the user's confirmation first, and an
        // automatic one isn't made (Paykit spec 7.2 step 1, Switch spec 4.3).
        let request_hash = entry.request_hash()?;
        if let Err(refusal) = self
            .check_destination(
                &entry.payee,
                &entry.destination,
                &carrier,
                request_hash.as_ref(),
                by,
                unconfirmed,
            )
            .await?
        {
            return Err(refusal.into());
        }
        // The attempt ID is journaled before the write (Paykit spec 7.2).
        self.save(&entry).await?;
        let op = ClaimOp::attempt(
            entry.obligation,
            step,
            attempt_id.to_bytes(),
            self.config.orphan_after_s,
            entry.deadline,
        );
        let mut conditions = Vec::new();
        if on_record {
            let key = self.activity_key(&record)?;
            conditions.push(self.app.record_condition(&key).await?);
        }
        let mut batch = OpBatch::new()
            .op_if(op.kind(), conditions)
            .obligation_deadline(entry.obligation, own_deadline(entry.deadline));
        if let Some(mut list) = reserve {
            // A single-use endpoint is marked used in the attempt's batch (Paykit spec 7.2
            // step 1, 8.4).
            list.used.push(UsedEndpoint {
                version: entry.destination.list_version.unwrap_or(0),
                endpoint: entry.destination.endpoint_id()?,
                attempt_id,
            });
            let value = list.to_json()?;
            batch = batch.entry(
                self.app
                    .record_op(
                        &list_target(&list.payee, &list.app_id),
                        Some(&value),
                        Vec::new(),
                    )
                    .await?,
            );
        }
        if let Some(created) = &create {
            // A payment without a request: its activity record (Paykit spec 8.4).
            let value = created.to_json()?;
            batch = batch.entry(
                self.app
                    .record_op(&record.record_target(), Some(&value), Vec::new())
                    .await?,
            );
        }
        // The journal moves to `Attempted` with the batch's answer, never before.
        let mut attempted = entry.clone();
        attempted.stage = Stage::Attempted;
        batch = batch.local(state::local_put(&key::journal(&attempt_id), &attempted)?);
        // The client checks the attempt's proof: only one that shows this app's own open
        // attempt lets money move (Paykit spec 7.2; Switch wire 6.5 "Binding").
        let submitted = match self.app.submit_batch(batch).await {
            Ok(submitted) => submitted,
            Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                self.set_stage(&mut entry, Stage::Abandoned).await?;
                // The answer's service time may show the deadline passed (Paykit spec 7.1).
                if self.deadline_passed(entry.deadline) {
                    return Err(Refusal::DeadlinePassed.into());
                }
                if let Some(view) = claim_conflict(&error) {
                    self.refuse_claim(&view)?;
                }
                return Err(PaykitError::Conflict(
                    "Paykit spec 7.2: the request or list record changed meanwhile".into(),
                ));
            }
            Err(AppError::InvalidData(why)) => {
                // The attempt's proof showed another state: the client wrote it `failed`.
                self.set_stage(&mut entry, Stage::Finished).await?;
                return Err(refused(why));
            }
            Err(error @ AppError::Refused(_)) => {
                self.set_stage(&mut entry, Stage::Abandoned).await?;
                return Err(error.into());
            }
            // The answer may have been lost: the batch is kept and the journal says no money
            // moved, so a resume reads the claim and writes `failed` if the attempt is ours.
            Err(error) => return Err(error.into()),
        };
        entry.step = submitted.proofs.first().and_then(|proof| proof.body().step);
        self.set_stage(&mut entry, Stage::Attempted).await?;
        // Before any money moves: the own identity.json, still confirmed within the last
        // 60 s (read again if not), then the destination (Paykit spec 4, 7.2). Either failing
        // writes `failed` instead of paying.
        if let Err(refusal) = self.own_identity_confirmed(false).await {
            return self.fail_attempt(&mut entry, refusal).await;
        }
        // The copy checked before the write is still fresh: this reads no network unless
        // it aged meanwhile (Switch spec 4.3).
        let checked = self
            .check_destination(
                &entry.payee,
                &entry.destination,
                &carrier,
                request_hash.as_ref(),
                by,
                unconfirmed,
            )
            .await?;
        if let Err(refusal) = checked {
            return self.fail_attempt(&mut entry, refusal).await;
        }
        // Starting to pay is journaled before the payment begins (Paykit spec 7.2).
        self.set_stage(&mut entry, Stage::Paying).await?;
        self.queue_owed_acceptance(&mut entry).await?;
        self.dispatch(entry).await
    }

    /// The own `identity.json` revalidated in the last [`REVALIDATION_MS`] (Paykit spec 4,
    /// "Fresh before paying"; Switch spec 12.6 "Residual risk"): with `refresh`, it is read
    /// again first, and without it only once its last confirmation is that old. A read that
    /// confirms the copy held, or a confirmation under 60 s old, lets the payment go on;
    /// `Missing`, `Older`, `Unavailable` and an error confirm nothing.
    pub(crate) async fn own_identity_confirmed(
        &mut self,
        refresh: bool,
    ) -> std::result::Result<(), Refusal> {
        let now = self.now();
        // A confirmation the device clock now puts in the future is no evidence of a recent
        // one: the clock went back, and the copy is read again.
        let recent = |at: Option<u64>| {
            at.and_then(|at| now.checked_sub(at))
                .is_some_and(|age| age < REVALIDATION_MS)
        };
        if !refresh && recent(self.app.identity_confirmed_at()) {
            return Ok(());
        }
        let refreshed = match self.app.refresh_identity().await {
            Ok(refreshed) => refreshed.confirmed_at().is_some(),
            Err(_) => false,
        };
        if refreshed || recent(self.app.identity_confirmed_at()) {
            return Ok(());
        }
        Err(Refusal::IdentityUnavailable(
            "Paykit spec 4: the own identity.json wasn't confirmed in the last 60 s".into(),
        ))
    }

    /// Whether `deadline` is at or before the highest service time the app verified: no
    /// attempt is made then (Paykit spec 7.1).
    pub(crate) fn deadline_passed(&self, deadline: Option<u64>) -> bool {
        deadline.is_some_and(|d| self.service_time().is_some_and(|t| d <= t))
    }

    /// A check failed after the attempt and before any money moved: `failed` instead of
    /// paying (Paykit spec 7.2 step 1).
    async fn fail_attempt(&mut self, entry: &mut JournalEntry, why: Refusal) -> Result<PayOutcome> {
        self.write_outcome(entry, false).await?;
        Err(why.into())
    }

    /// The acceptance of a one-time request, sent with its payment (Paykit spec 7.2 step 2,
    /// Paykit wire 4.6): signed and journaled, then queued, so a crash between the two signs
    /// it again and never queues two.
    async fn queue_owed_acceptance(&mut self, entry: &mut JournalEntry) -> Result<()> {
        if !entry.acceptance_owed {
            return Ok(());
        }
        if let Some(msg_id) = &entry.acceptance_msg_id {
            if self.app.delivery(msg_id).await?.is_some() {
                return Ok(());
            }
        }
        let Some(request_hash) = entry.request_hash()? else {
            return Ok(());
        };
        let label = entry.label.as_deref().and_then(|l| Label::parse(l).ok());
        let signed = self.sign(
            entry.payee,
            Kind::PaymentRequestAcceptance,
            vec![request_hash],
            PaymentRequestAcceptance { label }.to_body(),
        )?;
        entry.acceptance_msg_id = Some(signed.msg_id().to_owned());
        entry.acceptance_plaintext = Some(signed.plaintext().encode());
        self.save(entry).await?;
        self.app.queue_signed(signed).await?;
        Ok(())
    }

    /// Step 2: pay through the rail with the attempt ID as the idempotency key.
    async fn dispatch(&mut self, mut entry: JournalEntry) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        let payment = RailPayment {
            endpoint_id: entry.destination.endpoint_id()?,
            payload: entry.destination.payload()?,
            amount: entry.amount.amount()?,
            idempotency_key: attempt_id,
            payment_reference: entry.payment_reference.clone(),
        };
        let outcome = self.rail.pay(payment).await;
        // The acceptance goes out while the payment runs, not awaited (Paykit spec 7.2 step
        // 2); a delivery failure leaves it queued.
        if let Some(msg_id) = entry.acceptance_msg_id.clone() {
            let _ = self.app.flush_message(&msg_id).await;
        }
        match outcome {
            Ok(outcome) => self.take_outcome(&mut entry, outcome).await,
            // Once a payment may have been submitted, an error is nothing yet: the attempt
            // stays open (Paykit spec 7.2).
            Err(_) => Ok(PayOutcome::Pending { attempt_id }),
        }
    }

    async fn take_outcome(
        &mut self,
        entry: &mut JournalEntry,
        outcome: RailOutcome,
    ) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        match outcome {
            RailOutcome::Paid { proof, paid_at_ms } => {
                entry.rail_proof = Some(proof);
                entry.paid_at_ms = paid_at_ms;
                self.set_stage(entry, Stage::RailPaid).await?;
            }
            RailOutcome::Failed => self.set_stage(entry, Stage::RailFailed).await?,
            RailOutcome::Pending => return Ok(PayOutcome::Pending { attempt_id }),
        }
        self.report(entry).await
    }

    /// Steps 3 and 4: the outcome, the activity record and the proof, in one batch, then
    /// the proof is sent.
    async fn report(&mut self, entry: &mut JournalEntry) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        let paid = entry.rail_proof.is_some();
        match self.write_outcome(entry, paid).await {
            Ok(()) => {}
            Err(error) if error.code() == Some(ErrorCode::NotCurrent) => {
                // An app that isn't current can write nothing (Paykit spec 7.3).
                self.set_stage(entry, Stage::Finished).await?;
                return Ok(if paid {
                    PayOutcome::PaidUnreported { attempt_id }
                } else {
                    PayOutcome::Failed {
                        attempt_id,
                        reason: "the app isn't current".into(),
                    }
                });
            }
            Err(error) => return Err(error),
        }
        if !paid {
            return Ok(PayOutcome::Failed {
                attempt_id,
                reason: "the payment method reported a failure".into(),
            });
        }
        if let Some(msg_id) = entry.proof_msg_id.clone() {
            let _ = self.app.flush_message(&msg_id).await;
        }
        Ok(PayOutcome::Paid {
            attempt_id,
            proof_msg_id: entry.proof_msg_id.clone(),
        })
    }

    /// Step 3 (Paykit spec 7.2): `done` (basis `owner`) or `failed` on the attempt, and the
    /// payer's activity record with the acceptance sent with the payment and the proof, in
    /// one batch, which queues the proof (step 4) and moves the journal to `Finished` once
    /// applied. An attempt already resolved, by a receipt or another app, needs no claim
    /// entry. After `409 conflict` the claim and the record are read again and the batch is
    /// built again on them, unless the journal shows an earlier batch applied meanwhile.
    async fn write_outcome(&mut self, entry: &mut JournalEntry, paid: bool) -> Result<()> {
        let attempt_id = entry.attempt()?;
        let attempt = attempt_id.to_bytes();
        let target = ActivityTarget::payer(entry.natural_key.clone());
        for _ in 0..OUTCOME_TRIES {
            // A batch kept from before may have been applied since: it finished the entry.
            if let Some(kept) = self
                .local_get::<JournalEntry>(&key::journal(&attempt_id))
                .await?
            {
                if kept.stage == Stage::Finished {
                    *entry = kept;
                    return Ok(());
                }
            }
            let step = match entry.step {
                Some(step) => Some(step),
                None => match self.attempt_state(entry).await? {
                    Some((step, Outcome::Open | Outcome::Unknown)) => Some(step),
                    _ => None,
                },
            };
            let mut batch = OpBatch::new();
            if let Some(step) = step {
                let op = if paid {
                    ClaimOp::done(entry.obligation, step, attempt, Basis::Owner, None)?
                } else {
                    ClaimOp::failed(entry.obligation, step, attempt)
                };
                batch = batch
                    .op(op.kind())
                    .obligation_deadline(entry.obligation, own_deadline(entry.deadline));
            }
            let proof = if paid {
                Some(self.sign_proof(entry)?)
            } else {
                None
            };
            if let Some(write) = self
                .outcome_record(entry, &target, paid, proof.as_ref())
                .await?
            {
                batch = batch.entry(write);
            }
            let mut finished = entry.clone();
            finished.stage = Stage::Finished;
            finished.proof_msg_id = proof.as_ref().map(|p| p.msg_id().to_owned());
            batch = batch.local(state::local_put(&key::journal(&attempt_id), &finished)?);
            if batch.is_empty() {
                // Nothing to write at the switch: the proof is queued alone.
                if let Some(proof) = proof {
                    self.app.queue_signed(proof).await?;
                }
                self.save(&finished).await?;
                *entry = finished;
                return Ok(());
            }
            if let Some(proof) = proof {
                batch = batch.queue(proof);
            }
            match self.app.submit_batch(batch).await {
                Ok(_) => {
                    *entry = finished;
                    return Ok(());
                }
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    entry.step = None;
                    self.app.read_record(&target.record_target()).await?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(PaykitError::Conflict(
            "the claim or the activity record kept changing".into(),
        ))
    }

    /// The payer's activity record after the payment (Paykit spec 7.2 step 3): the
    /// acceptance sent with it and the proof, each in its role; a payment without a request
    /// that failed is marked so. `None` when nothing changes.
    async fn outcome_record(
        &mut self,
        entry: &JournalEntry,
        target: &ActivityTarget,
        paid: bool,
        proof: Option<&SignedMessage>,
    ) -> Result<Option<(Vec<Condition>, OpKind)>> {
        let own = self.identity();
        let key = self.activity_key(target)?;
        let held = self.app.shared_record(&key).await?;
        let Some(mut record) = activity::read(
            held.as_ref().and_then(|r| r.content.value()),
            &own,
            ActivitySide::Payer,
        ) else {
            return Ok(None);
        };
        let mut changed = false;
        if let Some(bytes) = &entry.acceptance_plaintext {
            let plaintext = MessagePlaintext::decode(bytes).map_err(|_| PaykitError::corrupt())?;
            let stored =
                StoredMessage::new(own, self.app.cert().signed(), plaintext, None, &entry.payee)?;
            changed |= activity::place(&mut record, Role::Response, stored);
        }
        if let Some(proof) = proof {
            changed |= activity::place(&mut record, Role::Proof, self.own_copy(proof)?);
        }
        if !paid && record.unrequested.is_some() && record.status.as_deref() != Some("failed") {
            record.status = Some("failed".into());
            changed = true;
        }
        if !changed {
            return Ok(None);
        }
        let value = record.to_json()?;
        Ok(Some(
            self.app
                .record_op(&target.record_target(), Some(&value), Vec::new())
                .await?,
        ))
    }

    /// The claim's step and this attempt's outcome, read from the switch; `None` when the
    /// claim doesn't hold the attempt.
    async fn attempt_state(&mut self, entry: &JournalEntry) -> Result<Option<(u64, Outcome)>> {
        let attempt = entry.attempt()?.to_bytes();
        let Some(read) = self.app.read_claim(&entry.obligation).await? else {
            return Ok(None);
        };
        Ok(read
            .view
            .attempt(&attempt)
            .filter(|a| a.owner == self.app_id())
            .map(|a| (read.view.step, a.outcome)))
    }

    /// The proof (Paykit spec 7.2 step 4, Paykit wire 4.10), signed for the outcome's batch.
    fn sign_proof(&self, entry: &JournalEntry) -> Result<SignedMessage> {
        let request_hash = entry.request_hash()?;
        let proof = PaymentProof {
            attempt_id: entry.attempt()?,
            billing_period: None,
            payment_endpoint_identifier: entry.destination.endpoint_id()?,
            payment_app_id: entry.destination.app_id,
            list_version: entry.destination.list_version,
            amount: request_hash
                .is_none()
                .then(|| entry.amount.amount())
                .transpose()?,
            allowance_id: None,
            label: entry.label.as_deref().and_then(|l| Label::parse(l).ok()),
            payment_reference: None,
            proof: entry.rail_proof.clone().unwrap_or_default(),
        };
        self.sign(
            entry.payee,
            Kind::PaymentProof,
            request_hash.into_iter().collect(),
            proof.to_body(),
        )
    }

    /// Finish the payments a crash, a lost answer or a pending payment method left in the
    /// journal (Paykit spec 7.2): before money could move, an own attempt still open is
    /// written `failed`; once paying, the method's history decides, and a dispatch is never
    /// carried out again unless the method is idempotent on the attempt ID; a restored entry
    /// is resolved only from a history that survives a restore.
    ///
    /// # Errors
    ///
    /// The client's errors; the rail's errors.
    pub async fn resume_payments(&mut self) -> Result<Vec<PayOutcome>> {
        // Batches kept from before go first, so the claim read below shows them and the
        // journal holds what their answers settled.
        self.app.flush_outbox().await?;
        let mut out = Vec::new();
        for mut entry in self.journal().await? {
            if !entry.open() {
                continue;
            }
            let attempt_id = entry.attempt()?;
            match entry.stage {
                Stage::Recorded | Stage::Attempted | Stage::Paying if entry.restored => {
                    match self.attempt_state(&entry).await? {
                        Some((step, Outcome::Open | Outcome::Unknown)) => {
                            entry.step = Some(step);
                            entry.stage = Stage::Paying;
                            out.push(self.resolve_dispatched(entry).await?);
                        }
                        Some(_) => self.set_stage(&mut entry, Stage::Finished).await?,
                        None => self.set_stage(&mut entry, Stage::Abandoned).await?,
                    }
                }
                Stage::Recorded | Stage::Attempted => match self.attempt_state(&entry).await? {
                    // No payment started: the open attempt is written `failed`.
                    Some((step, Outcome::Open | Outcome::Unknown)) => {
                        entry.step = Some(step);
                        self.write_outcome(&mut entry, false).await?;
                        out.push(PayOutcome::Failed {
                            attempt_id,
                            reason: "Paykit spec 7.2: no payment started".into(),
                        });
                    }
                    Some(_) => self.set_stage(&mut entry, Stage::Finished).await?,
                    None => self.set_stage(&mut entry, Stage::Abandoned).await?,
                },
                Stage::Paying => {
                    self.queue_owed_acceptance(&mut entry).await?;
                    out.push(self.resolve_dispatched(entry).await?);
                }
                Stage::RailPaid | Stage::RailFailed => {
                    out.push(self.report(&mut entry).await?);
                }
                Stage::Finished | Stage::Abandoned => {}
            }
        }
        let _ = self.app.flush_outbox().await;
        Ok(out)
    }

    /// Resolve an attempt whose payment may have been dispatched (Paykit spec 7.2, "What a
    /// payment method declares"): any history record resolves it; an empty history means it
    /// never started only within the history's retention, and for a restored entry only if
    /// the history survives a restore; otherwise an idempotent method is asked again within
    /// its retention, and anything else stays "may be paid".
    pub(crate) async fn resolve_dispatched(
        &mut self,
        mut entry: JournalEntry,
    ) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        if let Some(outcome) = self.rail.payment(attempt_id).await? {
            return self.take_outcome(&mut entry, outcome).await;
        }
        let declaration = self.rail.declaration();
        // The age counts from when the attempt was journaled (Paykit spec 7.2). A device clock
        // now behind that time can't tell how long ago it was, so the dispatch counts as
        // outside every finite retention: an empty history proves nothing and a repeat may
        // find the key forgotten. Only a retention kept for good holds whatever the clock.
        let age = self.now().checked_sub(entry.journaled_at_ms);
        let within =
            |retention: Option<u64>| retention.is_none_or(|r| age.is_some_and(|age| age <= r));
        if declaration.history
            && within(declaration.history_retention_ms)
            && (!entry.restored || declaration.history_survives_restore)
        {
            entry.stage = Stage::Paying;
            return self.take_outcome(&mut entry, RailOutcome::Failed).await;
        }
        if !entry.restored && declaration.idempotent && within(declaration.idempotency_retention_ms)
        {
            return self.dispatch(entry).await;
        }
        Ok(PayOutcome::Uncertain { attempt_id })
    }
}
