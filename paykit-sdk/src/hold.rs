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

use paykit_lib::collections::Collection;
use paykit_lib::records::ActivityRecord;
use pubky_switch_client::app::Protocol as _;
use pubky_switch_client::app::{AppError, RecordContent, Unopened};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::records::{AppCert, CLOCK_TOLERANCE_MS};
use pubky_switch_core::scopes::Permission;
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

/// Which side waits for a wider certificate's scope keys ([`Paykit::scope_keys_awaited`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeysAwaited {
    /// The payer's records, claims or `pay` key: a resume writes nothing to the switch.
    PayerSide,
    /// Only records the payer's side doesn't read: a resume goes on.
    PayeeSideOnly,
}

/// Why the shared records aren't listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unlisted {
    /// A restore set `p/nl`: the client owes a listing too, which its next read makes
    /// before the feed (Switch spec 12.4 step 2.1).
    Restore,
    /// The certificate the app holds reads collections, or members of one, that the one
    /// the records were last listed under (`p/lc`) didn't (Switch spec 8.3). The client
    /// owes a listing for it too, made at the start of a later read once it holds the
    /// scope keys, but tells no caller when it made it: a listing Paykit makes
    /// ([`Paykit::list_records`]) ends this.
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
        Ok(self.widened_from().await?.map(|_| Unlisted::Certificate))
    }

    /// The certificate the records were last listed under, when the one held now reads
    /// collections, or members of one, it didn't. With none kept ([`Paykit::follow_listed_cert`]),
    /// a certificate that passes now counts as reading more than nothing: its records are
    /// listed once, as at a join.
    async fn widened_from(&self) -> Result<Option<Option<AppCert>>> {
        match self.listed_cert().await? {
            None if self.certificate_refused().is_none() => Ok(Some(None)),
            None => Ok(None),
            Some(listed) => Ok(crate::protocol::reads_more(
                &self.protocol.collections(),
                &listed,
                self.app.cert(),
            )
            .then_some(Some(listed))),
        }
    }

    /// Whether the app waits for a wider certificate's scope keys, which the signer stores
    /// when it is next online (Switch spec 3.5), and on which side: the records it adds
    /// can't be listed until they come. The payer's side waits when the app lacks the
    /// current key of `pay`, or the certificate reads more of `payer_activity`,
    /// `payment_lists` or the claims than the one listed under; otherwise only the payee's
    /// side does, which a payment's resume doesn't touch.
    pub(crate) async fn scope_keys_awaited(&self) -> Result<Option<KeysAwaited>> {
        if self.holds_scope_keys() {
            return Ok(None);
        }
        let Some(widened) = self.widened_from().await? else {
            return Ok(None);
        };
        let held = self.app.cert();
        // Nothing listed yet: every side waits.
        let Some(listed) = widened else {
            return Ok(Some(KeysAwaited::PayerSide));
        };
        let payer_collections = [
            crate::activity::family(Collection::PayerActivity),
            crate::activity::family(Collection::PaymentLists),
        ];
        // Without the current `pay` key the payer's records can't be written: a data epoch
        // rotation besides a widening on the payee's side leaves the app so, the certificate
        // reading no more of the payer's side.
        let pay_key = self.app.scope_key(self.app.data_epoch(), "pay").is_some();
        // Claims read now and not under the certificate listed under. Today this follows from
        // the payer collections' term or from `pay` not held, where a resume reads nothing:
        // `claims.read` comes with `pay`, which reads `payer_activity`, or with `allowance`
        // (paykit-lib `scopes.rs`), which doesn't hold `pay`. It is kept so the rule doesn't
        // lean on that table.
        let claims = !listed.has_permission(Permission::ClaimsRead)
            && held.has_permission(Permission::ClaimsRead);
        let payer =
            !pay_key || claims || crate::protocol::reads_more(&payer_collections, &listed, held);
        Ok(Some(if payer {
            KeysAwaited::PayerSide
        } else {
            KeysAwaited::PayeeSideOnly
        }))
    }

    /// Whether the payer's side waits ([`KeysAwaited`], `keys` as
    /// [`Paykit::scope_keys_awaited`] gave it): the payer's records, claims or `pay` key
    /// wait, or keys wait while a restore's copy isn't listed yet, which lacks every record.
    /// Nothing built on the payer's records is written then, at a resume or a receive.
    pub(crate) async fn payer_waits(&self, keys: Option<KeysAwaited>) -> Result<bool> {
        Ok(keys == Some(KeysAwaited::PayerSide)
            || (keys.is_some() && self.unlisted().await? == Some(Unlisted::Restore)))
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

    /// Keep the certificate held now as the one listed under, when nothing is lost by it:
    /// when none is kept and `none_listed`, or when the one kept reads everything the held
    /// one does. A narrowed certificate is followed so, so a later certificate that reads
    /// more again is seen as wider. Run at a join, a load and after each receive; one write,
    /// which a crash leaves to the next load. Nothing is written under a certificate Paykit
    /// refuses.
    ///
    /// Only a join takes the certificate when none is kept (`none_listed`): the client's
    /// first read lists under the join's certificate. A load and a receive keep none then,
    /// whatever the reason none is kept: a join that crashed before its write, a load
    /// under a certificate Paykit refused, a receive that took a passing one after it, a
    /// restart before the listing, or a store an earlier build wrote. The records then count
    /// as unlisted ([`Paykit::unlisted`]) and are listed once, and that listing keeps it.
    pub(crate) async fn follow_listed_cert(&mut self, none_listed: bool) -> Result<()> {
        if self.certificate_refused().is_some() {
            return Ok(());
        }
        let held = self.app.cert();
        let follow = match self.listed_cert().await? {
            None => none_listed,
            Some(listed) => {
                listed.hash() != held.hash()
                    && !crate::protocol::reads_more(&self.protocol.collections(), &listed, held)
            }
        };
        if !follow {
            return Ok(());
        }
        let signed = held.signed().clone();
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
        self.refuse_while_scope_keys_missing().await?;
        if self.unlisted().await?.is_none() {
            return Ok(());
        }
        self.list_records().await.map_err(|error| {
            if unanswered(&error) {
                unlisted()
            } else {
                error
            }
        })
    }

    /// Refused as not now while a wider certificate's scope keys are still missing: the
    /// records it adds can't be listed yet, and a call that acts on what they hold waits,
    /// with a reason of its own, so the host can tell the user the signer must come online.
    pub(crate) async fn refuse_while_scope_keys_missing(&self) -> Result<()> {
        if self.scope_keys_awaited().await?.is_some() {
            return Err(keys_awaited());
        }
        Ok(())
    }

    /// List every collection the app's certificate reads, then clear the restore's mark and
    /// keep that certificate as the one listed under, in one commit: a crash before it
    /// leaves the records to be listed again. Refused while a wider certificate's scope
    /// keys are missing ([`Paykit::refuse_while_scope_keys_missing`]): a listing before them
    /// keeps values the app can't open, and keeping the certificate then would mark them
    /// listed for good. Every listing of Paykit's comes here. Only a receive takes a
    /// certificate, so none is taken during the listing.
    pub(crate) async fn list_records(&mut self) -> Result<()> {
        self.refuse_while_scope_keys_missing().await?;
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
        self.refuse_while_scope_keys_missing().await?;
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

/// The refusal while a wider certificate's scope keys are missing.
fn keys_awaited() -> crate::error::PaykitError {
    Refusal::NotNow(
        "Switch spec 3.5: waiting for the scope keys of the certificate taken, which the \
         signer stores when it is next online"
            .into(),
    )
    .into()
}

/// Whether a record's value isn't open for now only: sealed under a scope key the app
/// doesn't hold yet (Switch spec 3.5). The client opens it once the key comes, so a value
/// that is only waiting is never treated as one that doesn't read: nothing offers to write
/// over it, which would drop what it holds unseen, such as single-use marks.
pub(crate) fn awaits_key(content: &RecordContent) -> bool {
    matches!(
        content,
        RecordContent::Unreadable(Unopened::KeyNotHeld { .. })
    )
}

/// The refusal of a write over a value that is only waiting for its scope key
/// ([`awaits_key`]).
pub(crate) fn value_awaits_key() -> crate::error::PaykitError {
    Refusal::NotNow(
        "Switch spec 3.5: waiting for the scope keys the record's value is sealed under, which \
         the signer stores when it is next online"
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

#[cfg(test)]
mod tests {
    use pubky_switch_client::error::{ClientError, StoreError, TransportError};

    use super::*;

    fn switch(code: ErrorCode) -> PaykitError {
        PaykitError::Client(AppError::Switch(ClientError::Switch {
            status: code.status(),
            code,
            message: "scripted".into(),
            current: serde_json::Value::Null,
            known_version: None,
            retry_after: None,
        }))
    }

    #[test]
    fn test_listing_unanswered_as_a_switch_read_counts_it() {
        let lost = PaykitError::Client(AppError::Transport(TransportError::Connect("down".into())));
        assert!(unanswered(&lost));
        assert!(unanswered(&PaykitError::Client(
            AppError::NotAnsweringAsAccount { since_ms: 1 }
        )));
        assert!(unanswered(&PaykitError::Client(AppError::Switch(
            ClientError::BadServiceSignature { status: 502 }
        ))));
        assert!(unanswered(&switch(ErrorCode::RateLimited)));
        assert!(unanswered(&switch(ErrorCode::Unavailable)));
        assert!(!unanswered(&switch(ErrorCode::PermissionMissing)));
    }

    #[test]
    fn test_listing_failure_of_the_app_own_is_the_store_or_an_alarm() {
        assert!(local_failure(&PaykitError::Client(AppError::Store(
            StoreError::Unavailable
        ))));
        assert!(!local_failure(&switch(ErrorCode::PermissionMissing)));
        assert!(!local_failure(&PaykitError::Client(AppError::InvalidData(
            "bad".into()
        ))));
    }
}
