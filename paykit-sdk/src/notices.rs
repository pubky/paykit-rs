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
    /// declares"). The receipt is in the payer's activity record.
    PaidAfterFailed,
    /// The payment is held: the app's certificate no longer holds `pay`, so the app reads
    /// and writes no claim and no payer record, and the attempt waits, open, for an app
    /// holding `pay` (Paykit spec 3.1, 7.3). The payment reports only
    /// [`crate::PayOutcome::Pending`] or [`crate::PayOutcome::PaidOutcomeUnwritten`]
    /// meanwhile.
    HeldWithoutPay,
    /// A message whose effect the switch refused for good, kept to write into its record
    /// later, wasn't written within [`crate::KEPT_REPLY_MS`] of its acceptance: the SDK gave
    /// it up (Switch spec 8.2). Its entry stays stored, for the host
    /// ([`pubky_switch_client::app::App::parked_entries`]), and a request it was no longer
    /// shows as [`crate::RequestStatus::Unrecorded`].
    MessageGivenUp,
}

impl NoticeKind {
    /// The kind's name in a notice's key.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::PaidAfterFailed => "paid_after_failed",
            Self::HeldWithoutPay => "held_without_pay",
            Self::MessageGivenUp => "given_up",
        }
    }

    /// The kind a key names.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "paid_after_failed" => Some(Self::PaidAfterFailed),
            "held_without_pay" => Some(Self::HeldWithoutPay),
            "given_up" => Some(Self::MessageGivenUp),
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
        }
    }

    /// The notice that message `stored` from `sender` was given up, raised at `at`.
    pub(crate) fn given_up(
        sender: [u8; 32],
        msg_id: &str,
        request_hash: Option<[u8; 32]>,
        at: u64,
    ) -> Self {
        Self {
            kind: NoticeKind::MessageGivenUp,
            attempt_id: None,
            msg_id: Some(msg_id.to_owned()),
            counterparty: sender,
            request_hash: request_hash.map(|hash| pubky_switch_core::enc::b64(&hash)),
            raised_at_ms: at,
            acknowledged: false,
        }
    }

    /// The notice read whole, at the attempt and kind its key names: one that doesn't is a
    /// corrupt store.
    pub(crate) fn check(&self, attempt: &AttemptId, kind: NoticeKind) -> Result<()> {
        if self.kind != kind
            || kind == NoticeKind::MessageGivenUp
            || self.attempt_id.as_deref() != Some(attempt.to_text().as_str())
            || self.msg_id.is_some()
        {
            return Err(PaykitError::corrupt());
        }
        self.check_hash()
    }

    /// The notice of a message given up read whole, at the sender and `msg_id` its key
    /// names: one that doesn't is a corrupt store.
    pub(crate) fn check_message(&self, sender: &[u8; 32], msg_id: &str) -> Result<()> {
        if self.kind != NoticeKind::MessageGivenUp
            || self.attempt_id.is_some()
            || self.msg_id.as_deref() != Some(msg_id)
            || self.counterparty != *sender
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
        })
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
            Some(StateKey::Notice { .. } | StateKey::MessageNotice { .. })
        ) {
            return Err(Refusal::NotFound.into());
        }
        let mut kept = self
            .local_get::<KeptNotice>(&key)
            .await?
            .ok_or(Refusal::NotFound)?;
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
            Some(attempt) => self
                .local_get::<JournalEntry>(&key::journal(&attempt))
                .await?
                .is_some(),
            None => false,
        };
        if !held {
            return self.local_delete(&key).await;
        }
        kept.acknowledged = true;
        self.local_put(&key, &kept).await
    }

    /// Raise the notice of `kind` about the payment `entry` journals, unless it is kept
    /// already.
    pub(crate) async fn raise_notice(
        &mut self,
        kind: NoticeKind,
        entry: &JournalEntry,
    ) -> Result<()> {
        let key = key::notice(&entry.attempt()?, kind);
        if self.local_get::<KeptNotice>(&key).await?.is_some() {
            return Ok(());
        }
        let at = self.service_time().unwrap_or(0);
        self.local_put(&key, &KeptNotice::of(kind, entry, at)).await
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
        let notice = KeptNotice::given_up([1; 32], id, Some([2; 32]), 3);
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
