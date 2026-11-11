//! Notices: facts about this app's payments that the user must be told and that no call's
//! result carries, kept in Paykit's local state (`p/nt/<attempt_id>/<kind>`) until the host
//! acknowledges them, and carried in the app's backups (Paykit spec 7.2).
//!
//! A notice is raised once per attempt and kind: raising it again, as a re-sent receipt or a
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
}

impl NoticeKind {
    /// The kind's name in a notice's key.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::PaidAfterFailed => "paid_after_failed",
            Self::HeldWithoutPay => "held_without_pay",
        }
    }

    /// The kind a key names.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "paid_after_failed" => Some(Self::PaidAfterFailed),
            "held_without_pay" => Some(Self::HeldWithoutPay),
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
    /// The attempt it is about.
    pub attempt_id: AttemptId,
    /// The payee.
    pub payee: [u8; 32],
    /// The request paid, `None` for a payment without one.
    pub request_hash: Option<[u8; 32]>,
    /// When it was raised, in Unix milliseconds: the highest service time the app had
    /// verified then, or its device clock before any.
    pub raised_at_ms: u64,
}

/// A notice as kept (`p/nt/<attempt_id>/<kind>`): its attempt and kind repeat the key's, so
/// a restore binds the value to its key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptNotice {
    pub(crate) kind: NoticeKind,
    pub(crate) attempt_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) payee: [u8; 32],
    pub(crate) request_hash: Option<String>,
    pub(crate) raised_at_ms: u64,
    pub(crate) acknowledged: bool,
}

impl KeptNotice {
    /// A notice of `kind` about the payment `entry` journals, raised at `at`.
    pub(crate) fn of(kind: NoticeKind, entry: &JournalEntry, at: u64) -> Self {
        Self {
            kind,
            attempt_id: entry.attempt_id.clone(),
            payee: entry.payee,
            request_hash: entry.request_hash.clone(),
            raised_at_ms: at,
            acknowledged: false,
        }
    }

    /// The notice read whole, at the attempt and kind its key names: one that doesn't is a
    /// corrupt store.
    pub(crate) fn check(&self, attempt: &AttemptId, kind: NoticeKind) -> Result<()> {
        let request = self
            .request_hash
            .as_deref()
            .map(unb64_array::<32>)
            .transpose();
        if self.kind != kind || self.attempt_id != attempt.to_text() || request.is_err() {
            return Err(PaykitError::corrupt());
        }
        Ok(())
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
            attempt_id: AttemptId::parse(&self.attempt_id).map_err(|_| PaykitError::corrupt())?,
            payee: self.payee,
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
    /// raising it again changes nothing.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for an ID no notice has; a store failure.
    pub async fn acknowledge_notice(&mut self, id: &str) -> Result<()> {
        let key = format!("{}{id}", key::NOTICES);
        if !matches!(
            StateKey::parse(key.as_bytes()),
            Some(StateKey::Notice { .. })
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
        let at = self.service_time().unwrap_or_else(|| self.now());
        self.local_put(&key, &KeptNotice::of(kind, entry, at)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(attempt: &AttemptId) -> KeptNotice {
        KeptNotice {
            kind: NoticeKind::PaidAfterFailed,
            attempt_id: attempt.to_text(),
            payee: [1; 32],
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
}
