//! The hold after a restore (Paykit spec 7.2, "What a restore loses"), one-time branch.
//!
//! A backup can predate a payee's cancellation of a request it holds open: the restored app
//! would see the request payable. So every restore from a backup holds every open request,
//! as the restore itself sets it: [`crate::protocol::PaykitProtocol`]'s restore step adds the
//! hold to the writes it installs, in the restore's one commit, so no restore can leave it
//! out; it is no part of what a backup carries, and a confirmation an older backup holds is
//! dropped by the restore. While a request is held:
//!
//! - no automatic attempt is made on it (part 1 makes none);
//! - a manual one only once the user has seen that the payee may have cancelled it
//!   ([`crate::PayOptions::possibly_cancelled_confirmed`]), which ends the hold for that
//!   request;
//! - a cancellation the record shows, or the app keeps, stops it for good, and no
//!   confirmation overrides it (Paykit spec 8.1).
//!
//! After the restore, at the reconciliation it requires, once the records are read again,
//! the app asks every counterparty with an open request to re-send what its inbox accepted
//! from the acceptance time of the last entry the backup's cursor had passed, less the
//! clock tolerance (Paykit spec 9). A cancellation that comes back is kept
//! and placed as any other.

use paykit_lib::records::ActivityRecord;
use pubky_switch_client::app::AppError;
use pubky_switch_client::app::Protocol as _;
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::records::{AppCert, CLOCK_TOLERANCE_MS};
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::protocol::QueuedAsk;
use crate::requests::{cancelled, rejected, RequestStatus};
use crate::state::{key, Tx};

/// The hold a restore set (`p/rh`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestoreHold {
    /// From when the counterparties are asked to re-send: the acceptance time of the last
    /// entry the backup's cursor passed, less the clock tolerance.
    pub(crate) resend_from_ms: u64,
    /// The re-sends are queued.
    pub(crate) asks_queued: bool,
}

impl RestoreHold {
    /// The hold of a restore whose backup's cursor had passed entries accepted up to
    /// `cursor_time_ms`.
    pub(crate) fn of(cursor_time_ms: u64) -> Self {
        Self {
            resend_from_ms: cursor_time_ms.saturating_sub(CLOCK_TOLERANCE_MS),
            asks_queued: false,
        }
    }
}

/// Why the shared records aren't listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unlisted {
    /// A restore set `p/nl`: the client owes a listing too, which its next read makes
    /// before the feed (Switch spec 12.4 step 2.1).
    Restore,
    /// The certificate the app holds reads collections, or members of one, that the one
    /// the records were last listed under (`p/lc`) didn't (Switch spec 8.3): the client owes
    /// no listing for it, so only one Paykit makes ([`Paykit::list_records`]) covers it.
    Certificate,
}

impl Paykit {
    /// The certificate the shared records were last listed under (`p/lc`), if one is kept.
    async fn listed_cert(&self) -> Result<Option<AppCert>> {
        let Some(signed) = self.local_get::<SignedDoc>(key::LISTED_CERT).await? else {
            return Ok(None);
        };
        AppCert::verify(&signed, &self.identity())
            .map(Some)
            .map_err(|_| PaykitError::corrupt())
    }

    /// Why the shared records aren't listed, if they aren't: a restore's mark, or the
    /// certificate held now reading more than the one last listed under. The second is
    /// read from what the store holds, the client's certificate and `p/lc`, so a read that
    /// takes the certificate and fails, a crash or a restart before the next listing loses
    /// nothing.
    pub(crate) async fn unlisted(&self) -> Result<Option<Unlisted>> {
        if self
            .local_get::<bool>(key::RECORDS_UNLISTED)
            .await?
            .is_some()
        {
            return Ok(Some(Unlisted::Restore));
        }
        let widened = self.listed_cert().await?.is_some_and(|listed| {
            crate::protocol::reads_more(&self.protocol.collections(), &listed, self.app.cert())
        });
        Ok(widened.then_some(Unlisted::Certificate))
    }

    /// Whether the app holds the scope keys its certificate's scopes have, at the current
    /// data epoch: a certificate just taken that reads more gets them with the key bundle
    /// the next read fetches (Switch spec 3.5), and a listing before then keeps values it
    /// can't open, so it waits for them.
    fn holds_scope_keys(&self) -> bool {
        let epoch = self.app.data_epoch();
        self.app
            .cert()
            .scopes()
            .iter()
            .filter(|scope| self.protocol.has_keys(scope))
            .all(|scope| self.app.scope_key(epoch, scope).is_some())
    }

    /// At a join and a load: a store that keeps no certificate last listed under takes the
    /// one held now. After a join that is right, the client's first read listing under it;
    /// a crash between the join's commit and this write leaves it to the next load. A store
    /// an earlier build wrote has none either: its first load here takes the certificate it
    /// holds as listed under, so a wider one an earlier build took before then, without a
    /// listing since, isn't seen as wider. Nothing is written under a certificate Paykit
    /// refuses.
    pub(crate) async fn settle_listed_cert(&mut self) -> Result<()> {
        if self.certificate_refused().is_some() || self.listed_cert().await?.is_some() {
            return Ok(());
        }
        let signed = self.app.cert().signed().clone();
        self.local_put(key::LISTED_CERT, &signed).await
    }

    /// Whether the request record at `record_key`, `record`, is held after a restore: a
    /// restore set the hold, the user hasn't confirmed this request since, and it is open.
    /// Every open request is held, one first seen after the restore too: no time the app
    /// can see proves a request new, since its creation time is the counterparty's and a
    /// request from before the backup can come again with a later acceptance time (Paykit
    /// spec 7.2, "What a restore loses": nothing releases the hold automatically).
    pub(crate) async fn held(
        &self,
        record_key: &[u8; 32],
        record: &ActivityRecord,
    ) -> Result<bool> {
        if self
            .local_get::<RestoreHold>(key::RESTORE_HOLD)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        if record.shows_paid() || cancelled(record) || rejected(record) {
            return Ok(false);
        }
        Ok(self
            .local_get::<bool>(&key::hold_confirmed(record_key))
            .await?
            .is_none())
    }

    /// A restored cursor time (`ct`) above the backup's highest verified service time plus
    /// the clock tolerance is clamped to that bound, with the hold's re-send time: a time
    /// ahead of any the backup verified would ask counterparties to re-send from the future,
    /// so nothing would come back (Paykit spec 7.2, "What a restore loses"). Clamped, not
    /// refused: the time decides only where the re-sends start, and a restore must not fail
    /// on it. Without a verified time the device clock is the bound.
    pub(crate) async fn bound_restored_cursor_time(&mut self) -> Result<()> {
        let Some(cursor) = self.local_get::<u64>(key::CURSOR_TIME).await? else {
            return Ok(());
        };
        let bound = self
            .service_time()
            .unwrap_or_else(|| self.now())
            .saturating_add(CLOCK_TOLERANCE_MS);
        if cursor <= bound {
            return Ok(());
        }
        let mut tx = Tx::default();
        tx.put(key::CURSOR_TIME, &bound)?;
        if let Some(mut hold) = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await? {
            hold.resend_from_ms = RestoreHold::of(bound).resend_from_ms;
            tx.put(key::RESTORE_HOLD, &hold)?;
        }
        self.local_commit(tx).await
    }

    /// The shared records known, not an empty copy: after a restore the client's copy is
    /// empty until they are listed again (Switch spec 12.4 step 2.1), and after a
    /// certificate that reads more it lacks what it adds, so while either mark is set they
    /// are listed first. A listing the network or the switch couldn't
    /// answer now refuses the call as not now: a record's absence then decides nothing,
    /// never "gone". Any other failure (a store failure, an alarm, the switch's refusal for
    /// good) is that error: the call is refused all the same.
    pub(crate) async fn records_known(&mut self) -> Result<()> {
        match self.unlisted().await? {
            None => return Ok(()),
            Some(Unlisted::Certificate) if !self.holds_scope_keys() => return Err(unlisted()),
            Some(_) => {}
        }
        self.list_records().await.map_err(|error| {
            if unanswered(&error) {
                unlisted()
            } else {
                error
            }
        })
    }

    /// List every collection the app's certificate reads, then clear the restore's mark and
    /// keep that certificate as the one listed under, in one commit: a crash before it
    /// leaves the records to be listed again. Only a receive takes a certificate, so none
    /// is taken during the listing.
    pub(crate) async fn list_records(&mut self) -> Result<()> {
        let signed = self.app.cert().signed().clone();
        self.app.list_collections().await?;
        let mut tx = Tx::default();
        tx.delete(key::RECORDS_UNLISTED);
        tx.put(key::LISTED_CERT, &signed)?;
        self.local_commit(tx).await
    }

    /// The client listed the shared records at a receive, a restore's listing included: the
    /// restore's mark goes. The certificate listed under stays as it was, since the read
    /// may have taken a wider one after its listing ([`Unlisted::Certificate`]).
    pub(crate) async fn records_listed(&mut self) -> Result<()> {
        if self
            .local_get::<bool>(key::RECORDS_UNLISTED)
            .await?
            .is_some()
        {
            self.local_delete(key::RECORDS_UNLISTED).await?;
        }
        Ok(())
    }

    /// At a receive, after a certificate that reads more: the records it adds are listed
    /// now, so the calls that only read see them. A listing that fails leaves the records
    /// unlisted, for the next receive or write, and its error's text is returned for the
    /// host ([`crate::ReceiveSummary::records_unlisted`]): the read went through, so its
    /// report isn't lost to it. A store failure or an alarm is the receive's error.
    pub(crate) async fn list_widened(&mut self) -> Result<Option<String>> {
        if self.unlisted().await? != Some(Unlisted::Certificate) || !self.holds_scope_keys() {
            return Ok(None);
        }
        match self.list_records().await {
            Ok(()) => Ok(None),
            Err(error) if local_failure(&error) => Err(error),
            Err(error) => Ok(Some(error.to_string())),
        }
    }

    /// For a call that only reads: refused as not now while the records aren't listed since
    /// a restore, or since a certificate that reads more, rather than showing the copy that
    /// lacks them as the records.
    pub(crate) async fn refuse_while_unlisted(&self) -> Result<()> {
        if self.unlisted().await?.is_some() {
            return Err(unlisted());
        }
        Ok(())
    }

    /// Whether a reconcile still owes the payer pass since a restore: the restore sets the
    /// mark in its own commit, and only a [`Paykit::reconcile`] that ran the payer pass,
    /// under `pay`, clears it. The backup doesn't hold the attempts made after it, and only
    /// that pass finds them on the payer's obligations (Paykit spec 7.3), so until then no
    /// new attempt is made and nothing is dispatched, whatever the host does meanwhile, such
    /// as [`Paykit::end_reconcile`] after a reconcile without `pay` skipped the pass.
    pub(crate) async fn payer_pass_owed(&self) -> Result<bool> {
        Ok(self
            .local_get::<bool>(key::PAYER_PASS_OWED)
            .await?
            .is_some())
    }

    /// Refused as not now while [`Paykit::payer_pass_owed`].
    pub(crate) async fn refuse_while_payer_pass_owed(&self) -> Result<()> {
        if self.payer_pass_owed().await? {
            return Err(payer_pass_refusal().into());
        }
        Ok(())
    }

    /// A reconcile ran the payer pass: the restore's mark goes.
    pub(crate) async fn payer_pass_ran(&mut self) -> Result<()> {
        if self.payer_pass_owed().await? {
            self.local_delete(key::PAYER_PASS_OWED).await?;
        }
        Ok(())
    }

    /// Queue, once after a restore, a `resend_request` to every counterparty with an open
    /// request, from the restore's re-send time; [`Paykit::receive`] sends them. The start is
    /// bounded where it is used as well as at the restore and the load: no ordering of a
    /// load, a renewal and a crash leaves the re-sends starting in the future.
    pub(crate) async fn queue_restore_asks(&mut self) -> Result<()> {
        let Some(mut hold) = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await? else {
            return Ok(());
        };
        let bound = self
            .service_time()
            .unwrap_or_else(|| self.now())
            .saturating_add(CLOCK_TOLERANCE_MS);
        hold.resend_from_ms = hold
            .resend_from_ms
            .min(RestoreHold::of(bound).resend_from_ms);
        if hold.asks_queued {
            return Ok(());
        }
        let mut counterparties = std::collections::BTreeSet::new();
        for view in self.requests().await? {
            let open = !matches!(
                view.status,
                RequestStatus::Paid
                    | RequestStatus::Cancelled
                    | RequestStatus::Rejected
                    | RequestStatus::Expired
            );
            if open {
                counterparties.insert(view.request.payee);
            }
        }
        let mut tx = Tx::default();
        for counterparty in counterparties {
            tx.put(
                &key::ask(&counterparty, hold.resend_from_ms),
                &QueuedAsk {
                    sender: counterparty,
                    from_ms: hold.resend_from_ms,
                    until_ms: None,
                },
            )?;
        }
        hold.asks_queued = true;
        tx.put(key::RESTORE_HOLD, &hold)?;
        self.local_commit(tx).await
    }
}

/// The refusal while the shared records aren't listed since a restore.
/// The refusal while a reconcile owes the payer pass since a restore.
pub(crate) fn payer_pass_refusal() -> Refusal {
    Refusal::NotNow(
        "Paykit spec 7.3: no reconcile under pay has read the payer's obligations since the \
         restore"
            .into(),
    )
}

fn unlisted() -> crate::error::PaykitError {
    Refusal::NotNow(
        "Switch spec 8.3, 12.4 step 2.1: the shared records aren't listed since the restore or \
         the certificate that reads more"
            .into(),
    )
    .into()
}

/// A listing the network or the switch couldn't answer now, as a payment's switch read
/// counts it ([`crate::pay::switch_unavailable`]), or a `429`: a record's absence then
/// decides nothing.
fn unanswered(error: &PaykitError) -> bool {
    matches!(error, PaykitError::Client(client) if crate::pay::switch_unavailable(client))
        || error.code() == Some(ErrorCode::RateLimited)
}

/// A failure of this app's own: its store or an alarm.
fn local_failure(error: &PaykitError) -> bool {
    matches!(error, PaykitError::Client(client) if matches!(client.unkept(), AppError::Store(_) | AppError::Alarm(_)))
}
