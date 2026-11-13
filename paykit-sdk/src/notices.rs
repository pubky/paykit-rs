//! Notices: facts about this app's payments and messages that the user must be told and that
//! no call's result carries, kept in Paykit's local state (`p/nt/`) until the host
//! acknowledges them, and carried in the app's backups (Paykit spec 7.2).
//!
//! A notice is raised once per attempt and kind, or per message given up: raising it again,
//! as a re-sent receipt or a
//! later resume would, changes nothing, and an acknowledged one stays acknowledged.

use paykit_lib::AttemptId;
use pubky_switch_core::enc::{serde_b64, unb64_array};
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::journal::JournalEntry;
use crate::paykit::Paykit;
use crate::state::{key, StateKey};

/// What a notice tells the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoticeKind {
    /// "Marked failed, was paid": the payee's receipt confirms a payment whose attempt was
    /// written `failed`, and it arrived once the claim was gone, 30 days after its deadline,
    /// so it can't be recorded on the claim (Paykit spec 7.2, "What a payment method
    /// declares"). An app that gets the receipt and holds the failure raises it. An app
    /// holds the failure when it has it from a stable source: the attempt's owner from its
    /// own journal, or any own app, the owner included, from a view of the claim, kept or
    /// read, that shows the attempt `failed`. An owner restored from a backup that predates
    /// the failure has no journal entry for it, and uses such a view as any other own app
    /// does. The record's `status` is informational and never decides it. An own app that
    /// never saw the failure, such as one that joined after the service deleted the claim,
    /// can't tell and raises nothing. Either way the app records the receipt in its
    /// activity record.
    PaidAfterFailed,
    /// The payment is held: the app's certificate no longer holds `pay`, so the app reads
    /// and writes no claim and no payer record, and the attempt waits, open, for an app
    /// holding `pay` (Paykit spec 3.1, 7.3). The payment reports only
    /// [`crate::PayOutcome::Pending`] or [`crate::PayOutcome::PaidOutcomeUnwritten`]
    /// meanwhile.
    HeldWithoutPay,
    /// A message whose effect the switch refused for good, kept to write into its record
    /// later, wasn't written within [`crate::KEPT_MESSAGE_MS`] of its acceptance: the SDK gave
    /// it up (Switch spec 8.2). Its entry stays stored, for the host
    /// ([`pubky_switch_client::app::App::parked_entries`]), and a request it was no longer
    /// shows as [`crate::RequestStatus::Unrecorded`]. Acknowledging the notice
    /// ([`Paykit::acknowledge_notice`]) forgets the entry first
    /// ([`pubky_switch_client::app::App::forget_parked`]); if that fails, the notice stays
    /// and the error is returned, so an entry is never left stored once its notice is gone.
    MessageGivenUp,
    /// Messages a blocked counterparty's inbox accepted from
    /// [`Notice::lost_from_ms`] to [`Notice::lost_until_ms`], which the app dropped for the
    /// user's block to ask for again once the counterparty is unblocked, can no longer come
    /// back: its sender keeps them no longer, or the bound on marks dropped this one (Paykit
    /// spec 8.1, "Blocked counterparties"). The app never kept them, so the notice holds no
    /// content: a payment among them shows only in the payment method.
    BlockedMessagesLost,
}

impl NoticeKind {
    /// The kind's name in a notice's key.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::PaidAfterFailed => "paid_after_failed",
            Self::HeldWithoutPay => "held_without_pay",
            Self::MessageGivenUp => "given_up",
            Self::BlockedMessagesLost => "blocked_lost",
        }
    }

    /// The kind a key names.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "paid_after_failed" => Some(Self::PaidAfterFailed),
            "held_without_pay" => Some(Self::HeldWithoutPay),
            "given_up" => Some(Self::MessageGivenUp),
            "blocked_lost" => Some(Self::BlockedMessagesLost),
            _ => None,
        }
    }
}

/// A notice for the host to show, until it acknowledges it ([`Paykit::acknowledge_notice`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Notice {
    /// Its stable ID, which [`Paykit::acknowledge_notice`] takes.
    pub id: String,
    /// What it tells.
    pub kind: NoticeKind,
    /// The attempt it is about; `None` for a notice about a message.
    pub attempt_id: Option<AttemptId>,
    /// The message it is about, by its `msg_id`; `None` for a notice about an attempt.
    pub msg_id: Option<String>,
    /// The counterparty: the payee of an attempt, or the message's sender.
    pub counterparty: [u8; 32],
    /// The request paid or named, `None` for none.
    pub request_hash: Option<[u8; 32]>,
    /// When it was raised, in Unix milliseconds: the highest service time the app had
    /// verified then, 0 before any.
    pub raised_at_ms: u64,
    /// For [`NoticeKind::BlockedMessagesLost`]: the earliest acceptance time of what can no
    /// longer come back.
    pub lost_from_ms: Option<u64>,
    /// For [`NoticeKind::BlockedMessagesLost`]: the latest such time.
    pub lost_until_ms: Option<u64>,
}

/// A notice as kept (`p/nt/<attempt_id>/<kind>`, or `p/nt/<sender>/<msg_id>` for a message
/// given up): what its key names repeats in it, so a restore binds the value to its key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptNotice {
    pub(crate) kind: NoticeKind,
    pub(crate) attempt_id: Option<String>,
    pub(crate) msg_id: Option<String>,
    #[serde(with = "serde_b64")]
    pub(crate) counterparty: [u8; 32],
    pub(crate) request_hash: Option<String>,
    pub(crate) raised_at_ms: u64,
    pub(crate) acknowledged: bool,
    /// For a message given up: the entry the client keeps stored for it, which the
    /// notice's acknowledgement forgets ([`Paykit::acknowledge_notice`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stored: Option<StoredHandle>,
    /// For a message given up with no entry stored, where it came from; absent for one with
    /// its entry's handle (`stored`), as every notice the stored entries raise is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<GivenUpOrigin>,
    /// For messages a blocked counterparty sent that can no longer come back: their window
    /// of acceptance times, earliest and latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lost: Option<(u64, u64)>,
}

/// Where a message given up with no entry stored came from: no entry, so no handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GivenUpOrigin {
    /// The client dropped its entry, stored while it waited for a scope key, past the
    /// bounds of Switch spec 8.2: it was never processed, so a copy its sender sends again is
    /// taken as a new message.
    KeyWait,
    /// A payer's reply kept for its record (`pr/`) was given up.
    KeptReply,
}

/// What a message given up leaves behind ([`KeptNotice::given_up`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GivenUpFrom {
    /// An entry the client keeps stored, which the acknowledgement forgets.
    Stored(StoredHandle),
    /// No entry: the message came from there.
    Origin(GivenUpOrigin),
}

/// The handle of an entry the client keeps stored (Switch spec 8.2): its local ID, and the
/// message hash [`pubky_switch_client::app::App::forget_parked`] checks it by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredHandle {
    pub(crate) stored_id: u64,
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
}

impl KeptNotice {
    /// A notice of `kind` about the payment `entry` journals, raised at `at`.
    pub(crate) fn of(kind: NoticeKind, entry: &JournalEntry, at: u64) -> Self {
        Self {
            kind,
            attempt_id: Some(entry.attempt_id.clone()),
            msg_id: None,
            counterparty: entry.payee,
            request_hash: entry.request_hash.clone(),
            raised_at_ms: at,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    /// The notice "marked failed, was paid" of `attempt`, paid to `payee` for the request
    /// `request_hash`, if any, raised at `at`.
    pub(crate) fn late(
        attempt: AttemptId,
        payee: [u8; 32],
        request_hash: Option<&[u8; 32]>,
        at: u64,
    ) -> Self {
        Self {
            kind: NoticeKind::PaidAfterFailed,
            attempt_id: Some(attempt.to_text()),
            msg_id: None,
            counterparty: payee,
            request_hash: request_hash.map(|hash| pubky_switch_core::enc::b64(hash)),
            raised_at_ms: at,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    /// The notice that message `msg_id` from `sender` was given up, raised at `at`: with its
    /// entry's handle when the client keeps one stored, or else with where it came from.
    pub(crate) fn given_up(
        sender: [u8; 32],
        msg_id: &str,
        request_hash: Option<[u8; 32]>,
        at: u64,
        from: GivenUpFrom,
    ) -> Self {
        let (stored, origin) = match from {
            GivenUpFrom::Stored(handle) => (Some(handle), None),
            GivenUpFrom::Origin(origin) => (None, Some(origin)),
        };
        Self {
            kind: NoticeKind::MessageGivenUp,
            attempt_id: None,
            msg_id: Some(msg_id.to_owned()),
            counterparty: sender,
            request_hash: request_hash.map(|hash| pubky_switch_core::enc::b64(&hash)),
            raised_at_ms: at,
            acknowledged: false,
            stored,
            origin,
            lost: None,
        }
    }

    /// Whether the message given up stays processed, so a copy sent again has no effect while
    /// the notice is kept: every give-up but the client's drop of an entry waiting for a
    /// key, which was never processed ([`GivenUpOrigin::KeyWait`]).
    pub(crate) fn keeps_processed(&self) -> bool {
        self.origin != Some(GivenUpOrigin::KeyWait)
    }

    /// The notice read whole, at the attempt and kind its key names: one that doesn't is a
    /// corrupt store.
    pub(crate) fn check(&self, attempt: &AttemptId, kind: NoticeKind) -> Result<()> {
        if self.kind != kind
            || kind == NoticeKind::MessageGivenUp
            || self.attempt_id.as_deref() != Some(attempt.to_text().as_str())
            || self.msg_id.is_some()
            || self.stored.is_some()
            || self.origin.is_some()
            || self.lost.is_some()
        {
            return Err(PaykitError::corrupt());
        }
        self.check_hash()
    }

    /// The notice that what `counterparty`'s inbox accepted from `from` to `until`, dropped
    /// for the user's block, can no longer come back, raised at `at`.
    pub(crate) fn blocked_lost(counterparty: [u8; 32], from: u64, until: u64, at: u64) -> Self {
        Self {
            kind: NoticeKind::BlockedMessagesLost,
            attempt_id: None,
            msg_id: None,
            counterparty,
            request_hash: None,
            raised_at_ms: at,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: Some((from, until.max(from))),
        }
    }

    /// The notice of lost messages read whole, at the counterparty and start its key names:
    /// one that doesn't is a corrupt store. It is deleted at its acknowledgement.
    pub(crate) fn check_lost(&self, counterparty: &[u8; 32], from: u64) -> Result<()> {
        let window_read = self
            .lost
            .is_some_and(|(start, end)| start == from && end >= start);
        if self.kind != NoticeKind::BlockedMessagesLost
            || self.attempt_id.is_some()
            || self.msg_id.is_some()
            || self.request_hash.is_some()
            || self.counterparty != *counterparty
            || self.stored.is_some()
            || self.origin.is_some()
            || !window_read
            || self.acknowledged
        {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }

    /// The notice of a message given up read whole, at the sender and `msg_id` its key
    /// names: one that doesn't is a corrupt store.
    pub(crate) fn check_message(&self, sender: &[u8; 32], msg_id: &str) -> Result<()> {
        // A message notice is deleted at its acknowledgement, after its entry is forgotten,
        // so a valid run keeps none acknowledged. It holds its entry's handle exactly when it
        // names no origin: one raised for a stored entry always has it, and one with no
        // entry stored (a key wait's, a kept reply's) names where it came from instead.
        if self.kind != NoticeKind::MessageGivenUp
            || self.attempt_id.is_some()
            || self.msg_id.as_deref() != Some(msg_id)
            || self.counterparty != *sender
            || self.stored.is_some() == self.origin.is_some()
            || self.lost.is_some()
            || self.acknowledged
        {
            return Err(PaykitError::corrupt());
        }
        self.check_hash()
    }

    fn check_hash(&self) -> Result<()> {
        self.request_hash
            .as_deref()
            .map(unb64_array::<32>)
            .transpose()
            .map(|_| ())
            .map_err(|_| PaykitError::corrupt())
    }

    fn view(&self, id: String) -> Result<Notice> {
        let request_hash = self
            .request_hash
            .as_deref()
            .map(unb64_array::<32>)
            .transpose()
            .map_err(|_| PaykitError::corrupt())?;
        Ok(Notice {
            id,
            kind: self.kind,
            attempt_id: self
                .attempt_id
                .as_deref()
                .map(AttemptId::parse)
                .transpose()
                .map_err(|_| PaykitError::corrupt())?,
            msg_id: self.msg_id.clone(),
            counterparty: self.counterparty,
            request_hash,
            raised_at_ms: self.raised_at_ms,
            lost_from_ms: self.lost.map(|(from, _)| from),
            lost_until_ms: self.lost.map(|(_, until)| until),
        })
    }
}

/// Bind `notice`, read at `key` (`nt/...`), to what the key names, as a restore checks it:
/// a value at another notice's key, or under a key that names no notice, is a corrupt store.
pub(crate) fn bind_notice(key: &str, notice: &KeptNotice) -> Result<()> {
    match StateKey::parse(key.as_bytes()) {
        Some(StateKey::Notice { attempt, kind }) => notice.check(&attempt, kind),
        Some(StateKey::MessageNotice { sender, msg_id }) => notice.check_message(&sender, &msg_id),
        Some(StateKey::BlockedLost {
            counterparty,
            from_ms,
        }) => notice.check_lost(&counterparty, from_ms),
        _ => Err(PaykitError::corrupt()),
    }
}

/// The ID of the notice of `kind` about `attempt`: its key after the family's prefix.
pub(crate) fn notice_id(attempt: &AttemptId, kind: NoticeKind) -> String {
    format!("{}/{}", attempt.to_text(), kind.tag())
}

impl Paykit {
    /// The notices the host hasn't acknowledged, oldest first.
    ///
    /// # Errors
    ///
    /// A store failure, or a kept notice that doesn't read.
    pub async fn notices(&self) -> Result<Vec<Notice>> {
        let mut out = Vec::new();
        // Each read whole and bound to its key ([`bind_notice`]).
        for (id, kept) in self.local_scan::<KeptNotice>(key::NOTICES).await? {
            if !kept.acknowledged {
                out.push(kept.view(id)?);
            }
        }
        out.sort_by_key(|notice| notice.raised_at_ms);
        Ok(out)
    }

    /// Acknowledge the notice `id`, once the user has seen it: it is no longer listed, and
    /// raising it again changes nothing. It is kept, acknowledged, only as long as the
    /// journal holds its payment.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for an ID no notice has; a store failure.
    pub async fn acknowledge_notice(&mut self, id: &str) -> Result<()> {
        let key = format!("{}{id}", key::NOTICES);
        if !matches!(
            StateKey::parse(key.as_bytes()),
            Some(
                StateKey::Notice { .. }
                    | StateKey::MessageNotice { .. }
                    | StateKey::BlockedLost { .. }
            )
        ) {
            return Err(Refusal::NotFound.into());
        }
        let mut kept = self.kept_notice(&key).await?.ok_or(Refusal::NotFound)?;
        if kept.acknowledged {
            return Ok(());
        }
        // Kept acknowledged while the journal holds its payment, which could raise it again;
        // dropped with it, or now when it is gone (`Paykit::prune_journal`). A message given
        // up is never raised again: its notice goes now.
        let attempt = kept
            .attempt_id
            .as_deref()
            .map(AttemptId::parse)
            .transpose()
            .map_err(|_| PaykitError::corrupt())?;
        let held = match attempt {
            Some(attempt) => self.journal_entry(&attempt).await?.is_some(),
            None => false,
        };
        // The entry a message given up left stored is forgotten first: a failure keeps the
        // notice, for the host to acknowledge again.
        if let Some(stored) = &kept.stored {
            self.forget_stored(stored, &kept).await?;
        }
        if !held {
            return self.local_delete(&key).await;
        }
        kept.acknowledged = true;
        self.local_put(&key, &kept).await
    }

    /// The notice kept at `key`, read whole and bound to it ([`bind_notice`]).
    ///
    /// # Errors
    ///
    /// A store failure, or a value that doesn't read or isn't the notice its key names.
    pub(crate) async fn kept_notice(&self, key: &str) -> Result<Option<KeptNotice>> {
        self.local_get::<KeptNotice>(key).await
    }

    /// Forget the entry `stored` names, if the client still keeps it: by its local ID and
    /// hash, or, after a restore onto another store, which tells of its entries again, by
    /// its hash. One gone already, such as one the host forgot itself, is not looked for.
    /// The entry found is bound to `notice`: one holding another message than the sender's
    /// `msg_id` the notice names is a corrupt store, and nothing is forgotten.
    async fn forget_stored(&mut self, stored: &StoredHandle, notice: &KeptNotice) -> Result<()> {
        let entries = self.app.parked_entries().await?;
        let entry = entries
            .iter()
            .find(|e| e.stored_id == stored.stored_id && e.hash == stored.hash)
            .or_else(|| entries.iter().find(|e| e.hash == stored.hash));
        let Some(entry) = entry.cloned() else {
            return Ok(());
        };
        let message = self.app.parked_entry(entry.stored_id).await?;
        let names = message.as_ref().is_some_and(|parked| {
            parked.sender == notice.counterparty
                && parked.message.as_ref().map(|m| m.msg_id.as_str()) == notice.msg_id.as_deref()
        });
        if !names {
            return Err(PaykitError::corrupt());
        }
        self.app.forget_parked(entry.stored_id, &entry.hash).await?;
        Ok(())
    }

    /// Raise the notice of `kind` about the payment `entry` journals, unless it is kept
    /// already.
    pub(crate) async fn raise_notice(
        &mut self,
        kind: NoticeKind,
        entry: &JournalEntry,
    ) -> Result<()> {
        let key = key::notice(&entry.attempt()?, kind);
        if self.kept_notice(&key).await?.is_some() {
            return Ok(());
        }
        let at = self.service_time().unwrap_or(0);
        self.local_put(&key, &KeptNotice::of(kind, entry, at)).await
    }

    /// Drop the notice of `kind` about `entry`'s payment, acknowledged or not, once what it
    /// says no longer holds: raised again, it is listed again.
    pub(crate) async fn drop_notice(
        &mut self,
        kind: NoticeKind,
        entry: &JournalEntry,
    ) -> Result<()> {
        let key = key::notice(&entry.attempt()?, kind);
        if self.kept_notice(&key).await?.is_some() {
            self.local_delete(&key).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(attempt: &AttemptId) -> KeptNotice {
        KeptNotice {
            kind: NoticeKind::PaidAfterFailed,
            attempt_id: Some(attempt.to_text()),
            msg_id: None,
            counterparty: [1; 32],
            request_hash: Some(pubky_switch_core::enc::b64(&[2; 32])),
            raised_at_ms: 3,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    #[test]
    fn test_notices_kept_notice_bound_to_its_key() {
        let attempt = AttemptId::from_bytes([5; 16]);
        let notice = kept(&attempt);
        assert!(notice.check(&attempt, NoticeKind::PaidAfterFailed).is_ok());
        let view = notice.view(notice_id(&attempt, notice.kind)).unwrap();
        assert_eq!(view.request_hash, Some([2; 32]));
        assert_eq!(
            StateKey::parse(key::notice(&attempt, notice.kind).as_bytes()),
            Some(StateKey::Notice {
                attempt,
                kind: NoticeKind::PaidAfterFailed
            })
        );
    }

    #[test]
    fn test_notices_kept_notice_at_another_key_refused() {
        let attempt = AttemptId::from_bytes([5; 16]);
        let notice = kept(&attempt);
        assert!(notice.check(&attempt, NoticeKind::HeldWithoutPay).is_err());
        assert!(notice
            .check(&AttemptId::from_bytes([6; 16]), NoticeKind::PaidAfterFailed)
            .is_err());
        let bad = KeptNotice {
            request_hash: Some("not-a-hash".into()),
            ..notice
        };
        assert!(bad.check(&attempt, NoticeKind::PaidAfterFailed).is_err());
    }

    #[test]
    fn test_notices_message_notice_bound_to_its_sender_and_message() {
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let handle = StoredHandle {
            stored_id: 4,
            hash: [5; 32],
        };
        let notice =
            KeptNotice::given_up([1; 32], id, Some([2; 32]), 3, GivenUpFrom::Stored(handle));
        assert!(notice.check_message(&[1; 32], id).is_ok());
        assert!(notice.check_message(&[9; 32], id).is_err());
        assert!(notice
            .check_message(&[1; 32], "223e4567-e89b-42d3-a456-426614174000")
            .is_err());
        assert!(notice
            .check(&AttemptId::from_bytes([5; 16]), NoticeKind::MessageGivenUp)
            .is_err());
    }
}
