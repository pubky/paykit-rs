//! Receiving (Paykit spec 6, 7.3, 8.1, 8.4; Paykit wire 5.3, 5.4): the effect of each Paykit
//! message that passed the recipient checks, written to its record.
//!
//! [`crate::PaykitProtocol`] runs check 10 and queues the message with the cursor move; this
//! writes its effect right after, in the batch that carries the deferred cursor move:
//!
//! - **payer side** (`pay`): a request opens its request record; a payee's cancellation, a
//!   receipt and a Private Payment List go into theirs. A receipt also settles the attempt
//!   it names, by evidence (Switch spec 10.2 row 3): only the payee's yes counts (Paykit spec
//!   7.3).
//! - **payee side** (`request` or a label): the payer's acceptance, rejection, cancellation
//!   and proof go into the request record under the request's label; a proof of a payment
//!   without a request into the record of its settlement, so one settlement is credited
//!   once (Paykit spec 8.4).
//! - a `resend_request` is answered (Paykit spec 9).
//!
//! A record that already reflects a message was written by another own app: nothing
//! changes. Every stored message is verified again before it is placed (Switch spec 8.2).

use paykit_lib::derive::natural_key;
use paykit_lib::records::{PaymentListRecord, StoredMessage, UnrequestedDestination};
use paykit_lib::Body;
use pubky_switch_client::app::ClaimOp;
use pubky_switch_client::app::ObligationDeadline;
use pubky_switch_core::claims::{Basis, ClaimStatus, Outcome};
use pubky_switch_core::enc::unb64;
use pubky_switch_core::message::MessagePlaintext;
use pubky_switch_core::scopes::Permission;
use pubky_switch_core::sig::SignedDoc;

use crate::activity::{ActivityTarget, Role};
use crate::error::Result;
use crate::paykit::Paykit;
use crate::protocol::{writes_payee_label, QueuedMessage};
use crate::records::{conflict, list_target};
use crate::state::{key, Tx};

/// What applying one queued message came to.
enum Applied {
    /// Its effect is written, or it had none: the queue entry goes.
    Done,
    /// It waits for a record this app is still writing: kept for the next call.
    Wait,
}

impl Paykit {
    /// Write the effects of the queued messages, oldest first. Each entry leaves the queue
    /// once its effect is written.
    pub(crate) async fn apply_effects(&mut self) -> Result<Vec<String>> {
        let queued = self.local.scan::<QueuedMessage>(key::EFFECTS).await?;
        let mut applied = Vec::new();
        for (rest, entry) in queued {
            let stored = self.stored_of(&entry);
            let outcome = match &stored {
                Some(stored) => self.apply_one(stored, &entry).await?,
                // A message that doesn't verify again isn't acted on (Switch spec 8.2).
                None => Applied::Done,
            };
            if let Applied::Done = outcome {
                self.local
                    .delete(&format!("{}{rest}", key::EFFECTS))
                    .await?;
                if let Some(stored) = stored {
                    applied.push(stored.message().msg_id.clone());
                }
            }
        }
        Ok(applied)
    }

    fn stored_of(&self, entry: &QueuedMessage) -> Option<StoredMessage> {
        let bytes = unb64(&entry.plaintext).ok()?;
        let plaintext = MessagePlaintext::decode(&bytes).ok()?;
        let cert: SignedDoc = entry.cert.clone();
        StoredMessage::new(
            entry.sender,
            &cert,
            plaintext,
            Some(entry.accepted_at_ms),
            &self.identity(),
        )
        .ok()
    }

    async fn apply_one(
        &mut self,
        stored: &StoredMessage,
        entry: &QueuedMessage,
    ) -> Result<Applied> {
        let sender = stored.sender();
        let message = stored.message();
        match stored.body() {
            Body::PaymentRequest(_) if self.holds("pay") => {
                let target =
                    ActivityTarget::payer(natural_key::request_record(&sender, &stored.hash()));
                self.place(&target, sender, Role::Request, stored.clone(), Vec::new())
                    .await?;
                let mut tx = Tx::default();
                self.note_peer(&mut tx, &sender)?;
                self.local.commit(tx).await?;
                Ok(Applied::Done)
            }
            Body::PaymentRequestAcceptance(_)
            | Body::PaymentRequestRejection(_)
            | Body::PaymentProof(_)
                if self.acts_as_payee() =>
            {
                self.apply_payer_reply(stored).await
            }
            Body::PaymentRequestCancellation(_) => {
                let Some(request) = message.refs.first().copied() else {
                    return Ok(Applied::Done);
                };
                let payer_target =
                    ActivityTarget::payer(natural_key::request_record(&sender, &request));
                let payer_side = self.holds("pay") && self.activity(&payer_target).await?.is_some();
                if payer_side {
                    self.place(
                        &payer_target,
                        sender,
                        Role::Cancellation,
                        stored.clone(),
                        Vec::new(),
                    )
                    .await?;
                    Ok(Applied::Done)
                } else if self.acts_as_payee() {
                    self.apply_payer_reply(stored).await
                } else {
                    Ok(Applied::Done)
                }
            }
            Body::PaymentReceipt(receipt) if self.holds("pay") => {
                self.apply_receipt(stored, receipt.attempt_id).await
            }
            Body::PrivatePaymentList(_) if self.holds("pay") => {
                self.apply_list(stored).await?;
                Ok(Applied::Done)
            }
            Body::ResendRequest(resend) => {
                self.answer_resend(&sender, &entry.cert, resend).await?;
                Ok(Applied::Done)
            }
            _ => Ok(Applied::Done),
        }
    }

    pub(crate) fn acts_as_payee(&self) -> bool {
        paykit_lib::scopes::acts_as_payee(self.app.cert().scopes())
    }

    /// A payer's acceptance, rejection, cancellation or proof, at the payee (Paykit spec 3.4,
    /// 6, 8.4).
    async fn apply_payer_reply(&mut self, stored: &StoredMessage) -> Result<Applied> {
        let sender = stored.sender();
        let own = self.identity();
        let label = stored.body().label().cloned();
        // An app writes only records its scopes cover: a labelled selling app its own
        // label's (Paykit spec 3.1, 6).
        if !writes_payee_label(self.app.cert().scopes(), label.as_ref()) {
            return Ok(Applied::Done);
        }
        let role = match stored.body() {
            Body::PaymentProof(_) => Role::Proof,
            Body::PaymentRequestCancellation(_) => Role::Cancellation,
            _ => Role::Response,
        };
        let target = match stored.message().refs.first() {
            Some(request) => {
                ActivityTarget::payee(natural_key::request_record(&own, request), label.as_ref())
            }
            None => {
                // A payment without a request: keyed by its settlement, so every proof of one
                // settlement names one record and is credited once (Paykit spec 8.4). A
                // proof whose method gives no settlement ID isn't credited.
                let Body::PaymentProof(proof) = stored.body() else {
                    return Ok(Applied::Done);
                };
                let Some(settlement) = proof.settlement_id() else {
                    return Ok(Applied::Done);
                };
                let target = ActivityTarget::payee(
                    natural_key::payee_payment_record(
                        &proof.payment_endpoint_identifier,
                        &settlement,
                    ),
                    None,
                );
                self.place(&target, sender, role, stored.clone(), Vec::new())
                    .await?;
                return Ok(Applied::Done);
            }
        };
        if self.activity(&target).await?.is_none() {
            // The request's record isn't written yet: wait while this app still owes one.
            let owed = !self
                .local
                .scan::<serde_json::Value>(key::REQUESTS)
                .await?
                .is_empty();
            return Ok(if owed { Applied::Wait } else { Applied::Done });
        }
        self.place(&target, sender, role, stored.clone(), Vec::new())
            .await?;
        Ok(Applied::Done)
    }

    /// A receipt (Paykit spec 7.3, 8.4): stored in the payer's record, and the attempt it
    /// names settled by evidence when it is open, unknown or failed (Switch spec 10.2 row 3).
    async fn apply_receipt(
        &mut self,
        stored: &StoredMessage,
        attempt_id: paykit_lib::AttemptId,
    ) -> Result<Applied> {
        let sender = stored.sender();
        let own = self.identity();
        let (target, obligation) = match stored.message().refs.get(1) {
            Some(request) => (
                ActivityTarget::payer(natural_key::request_record(&sender, request)),
                paykit_lib::derive::request_obligation(self.naming(), &sender, request, 0),
            ),
            None => (
                ActivityTarget::payer(natural_key::payer_payment_record(&own, &attempt_id)),
                paykit_lib::derive::payment_obligation(&attempt_id),
            ),
        };
        // The payer matches a receipt through its record, whose counterparty must be the
        // receipt's sender (Paykit spec 8.4).
        let Some(record) = self.activity(&target).await? else {
            return Ok(Applied::Done);
        };
        if record.counterparty != sender {
            return Ok(Applied::Done);
        }
        self.place(&target, sender, Role::Receipt, stored.clone(), Vec::new())
            .await?;
        self.settle_by_receipt(&obligation, attempt_id, stored.hash())
            .await?;
        Ok(Applied::Done)
    }

    /// `done` with basis `evidence` on the attempt a receipt names, while it is open,
    /// unknown or failed and the claim isn't SETTLED (Switch spec 10.2 row 3).
    pub(crate) async fn settle_by_receipt(
        &mut self,
        obligation: &[u8; 32],
        attempt_id: paykit_lib::AttemptId,
        receipt_hash: [u8; 32],
    ) -> Result<()> {
        if !self.app.cert().has_permission(Permission::ClaimsWrite) {
            return Ok(());
        }
        let Some(read) = Box::pin(self.app.read_claim(obligation)).await? else {
            return Ok(());
        };
        if read.view.status == ClaimStatus::Settled {
            return Ok(());
        }
        let open = read.view.attempts.iter().any(|a| {
            a.attempt_id == attempt_id.to_bytes()
                && matches!(
                    a.outcome,
                    Outcome::Open | Outcome::Unknown | Outcome::Failed
                )
        });
        if !open {
            return Ok(());
        }
        let op = ClaimOp::done(
            *obligation,
            read.view.step,
            attempt_id.to_bytes(),
            Basis::Evidence,
            Some(receipt_hash),
        )?;
        match Box::pin(
            self.app
                .write_claim(op, Vec::new(), ObligationDeadline::NotKnown),
        )
        .await
        {
            Ok(_) => Ok(()),
            // Another own app recorded it first, or the attempt moved: the next read shows it.
            Err(error) if error.code() == Some(pubky_switch_core::http::ErrorCode::Conflict) => {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// A Private Payment List (Paykit spec 5, 8.4; Paykit wire 5.4): the latest list of its
    /// publishing app replaces an older one, keeping the single-use endpoints marked used.
    async fn apply_list(&mut self, stored: &StoredMessage) -> Result<()> {
        let payee = stored.sender();
        let app_id = stored.message().from.app_id;
        let Body::PrivatePaymentList(list) = stored.body() else {
            return Ok(());
        };
        let version = list.version;
        let own = self.identity();
        let target = list_target(&payee, &app_id);
        Box::pin(self.app.read_record(&target)).await?;
        let stored = stored.clone();
        Box::pin(self.app.update_record(&target, Vec::new(), move |current| {
            let held = current.and_then(|v| PaymentListRecord::read(v, &own).ok());
            let mut record = match held {
                Some(held) => {
                    let msg_id = &stored.message().msg_id;
                    if held.list.message().msg_id == *msg_id || held.msg_ids.contains(msg_id) {
                        return None;
                    }
                    let held_version = match held.list.body() {
                        Body::PrivatePaymentList(l) => l.version,
                        _ => 0,
                    };
                    // A list with a lower or equal version than one held is ignored
                    // (Paykit wire 4.13).
                    if version <= held_version {
                        return None;
                    }
                    held
                }
                None => PaymentListRecord {
                    payee,
                    app_id,
                    list: stored.clone(),
                    used: Vec::new(),
                    msg_ids: Vec::new(),
                },
            };
            record.list = stored.clone();
            record.msg_ids = paykit_lib::records::bounded_msg_ids(
                &[stored.message().msg_id.clone()],
                &record.msg_ids,
            );
            record.to_json().ok()
        }))
        .await
        .map_err(conflict)?;
        let mut tx = Tx::default();
        self.note_peer(&mut tx, &payee)?;
        self.local.commit(tx).await
    }
}

/// The `unrequested` member of a payer's record of a payment without a request.
pub(crate) fn unrequested_of(
    destination: &crate::journal::Destination,
) -> Result<UnrequestedDestination> {
    Ok(UnrequestedDestination {
        app_id: destination.app_id,
        endpoint_id: destination.endpoint_id()?,
        payload: destination.payload()?,
        source: destination.source(),
        list_version: destination.list_version,
        single_use: destination.single_use,
    })
}
