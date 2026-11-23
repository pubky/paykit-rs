//! First-delivery evidence and the withdrawal interval for a recipient (Paykit spec 8.4).

use super::{
    counter_unavailable, target, ListRecipient, ListReservation, UnknownListDelivery, DAY, HOUR,
};
use crate::error::Result;
use crate::notices::{KeptNotice, NoticeKind};
use crate::state::{key, StateKey, Tx};
use crate::Paykit;
use paykit_lib::bodies::PRIVATE_LIST_MAX_AGE_MS;
use paykit_lib::records::ListPublicationRecord;
use paykit_lib::ListVersion;
use pubky_switch_client::app::RecordApplication;
use pubky_switch_core::ops::Condition;
use serde::{Deserialize, Serialize};

/// Latest possible expiry of an older nonempty list. A counter observed without its
/// original signed list gives no expiry. A newly created counter has no earlier offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(super) enum Payable {
    Until(u64),
    Unknown,
}

impl Payable {
    pub(super) fn union(self, other: Self) -> Self {
        match (self, other) {
            (Self::Until(left), Self::Until(right)) => Self::Until(left.max(right)),
            _ => Self::Unknown,
        }
    }
}

/// What the client reported about a list's first delivery (Pubky Switch spec 11).
/// `Undelivered` proves the recipient never got it. `Unknown` means it may have: no
/// replacement follows without the user's confirmation (Paykit spec 8.4, 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FirstDelivery {
    Open,
    Undelivered,
    Unknown,
}

impl FirstDelivery {
    /// Two observations of the same signed list. A possible delivery outweighs proof of
    /// a non-delivery from another give-up, which outweighs none.
    pub(super) fn with(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Undelivered, _) | (_, Self::Undelivered) => Self::Undelivered,
            _ => Self::Open,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Withdrawal {
    pub(super) first: u64,
    pub(super) issued: u64,
}

impl ListReservation {
    pub(super) fn payable_after(&self, accepted: Option<u64>) -> Payable {
        if self.revision.is_none() && accepted.is_some() {
            return Payable::Until(0);
        }
        if self.revision.is_none()
            || (self.delivery == FirstDelivery::Undelivered && accepted.is_none())
        {
            return self.predecessors;
        }
        let expires = accepted.map_or(self.expires, |at| {
            self.expires.min(at.saturating_add(PRIVATE_LIST_MAX_AGE_MS))
        });
        self.predecessors.union(Payable::Until(expires))
    }

    pub(super) fn withdrawal_due(&self, now: u64) -> bool {
        let Some(withdrawal) = self.withdrawal else {
            return false;
        };
        if self.delivery != FirstDelivery::Undelivered
            || matches!(self.predecessors, Payable::Until(end) if end <= now)
        {
            return false;
        }
        let interval = if now.saturating_sub(withdrawal.first) < DAY {
            HOUR
        } else {
            DAY
        };
        now.checked_sub(withdrawal.issued)
            .is_some_and(|elapsed| elapsed >= interval)
    }
}

impl Paykit {
    /// Read the record under the generation that actually issued the withdrawal.
    /// After a move the same batch also reserves a number under the new generation.
    pub(super) async fn withdrawal_base(
        &mut self,
        prior: &ListReservation,
    ) -> Result<(ListPublicationRecord, Condition)> {
        let target = target(&self.app_id(), &prior.recipient, prior.generation);
        let Some(RecordApplication {
            record: held,
            application,
            ..
        }) = self.app.read_record_application(&target).await?
        else {
            return Err(counter_unavailable().into());
        };
        let current = self.publication_at(&held).await?;
        if !prior.matches(&current) {
            if current.counter > prior.counter {
                self.retire_obsolete_withdrawal(prior).await?;
            }
            return Err(counter_unavailable().into());
        }
        let condition = Condition::RecordCurrent {
            key: held.key,
            revision: application,
            deleted: false,
        };
        Ok((current, condition))
    }

    pub(super) async fn retire_obsolete_withdrawal(
        &mut self,
        prior: &ListReservation,
    ) -> Result<()> {
        let at = key::list_reservation(&prior.recipient);
        if let Some(mut held) = self.local_get::<ListReservation>(&at).await? {
            if held.id == prior.id && held.hash == prior.hash && held.choice == prior.choice {
                held.withdrawal = None;
                self.local_put(&at, &held).await?;
            }
        }
        Ok(())
    }

    /// Keep the client's report of a list's first delivery with the original issue, in
    /// the commit that takes its native duty. Definite non-delivery lets the withdrawal
    /// schedule replace an empty list. An unknown outcome is kept with a notice for the
    /// host while the list is the counterparty's current one and its outcome still
    /// matters ([`ListReservation::unknown_matters`]): only the user can authorize
    /// another version ([`Paykit::send_list_again`]; Paykit spec 8.4, 9). A failed
    /// resend or permanent refusal records nothing. Whether `id` is the recipient's
    /// current issue.
    pub(crate) async fn take_list_delivery(
        &self,
        tx: &mut Tx,
        id: &str,
        observed: FirstDelivery,
    ) -> Result<bool> {
        for (_, mut issue) in self
            .local_scan::<ListReservation>(key::LIST_RESERVATIONS)
            .await?
        {
            if issue.id != id {
                continue;
            }
            issue.delivery = issue.delivery.with(observed);
            tx.put(&key::list_reservation(&issue.recipient), &issue)?;
            if observed == FirstDelivery::Unknown {
                let current = self
                    .local_get::<ListRecipient>(&key::list_recipient(&issue.recipient))
                    .await?
                    .filter(|recipient| {
                        recipient.choice == issue.choice
                            && recipient.answers.first(&issue).is_none()
                    });
                let now = self.service_time().unwrap_or(0);
                let at = key::restored_delivery(&issue.recipient, &issue.id, &issue.hash);
                if current.is_some()
                    && issue.unknown_matters(now)
                    && self.kept_notice(&at).await?.is_none()
                {
                    let notice = KeptNotice::list_delivery_unknown(
                        &issue.recipient,
                        &issue.id,
                        &issue.hash,
                        now,
                    );
                    tx.put(&at, &notice)?;
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Drop each list notice whose list's outcome is settled: that recipient's list was
    /// superseded by a newer version, or the recipient is known to hold it, or nothing it
    /// offered or withdrew can be paid any more (Paykit spec 9, "Deliveries of unknown
    /// outcome"). None of these raises it again ([`Paykit::take_list_delivery`]), so an
    /// acknowledged notice needn't be kept for good. A backup taken before the prune can
    /// bring the notice back, unacknowledged, until the next receive prunes it again.
    pub(super) async fn prune_list_notices(&mut self) -> Result<()> {
        let now = self.service_time().unwrap_or(0);
        let mut tx = Tx::default();
        let mut settled_any = false;
        for (rest, notice) in self
            .local_scan::<KeptNotice>(key::RESTORED_DELIVERIES)
            .await?
        {
            let at = format!("{}{rest}", key::RESTORED_DELIVERIES);
            // `local_scan` read every key of the family bound to its notice already.
            let Some(StateKey::RestoredDelivery { to, msg_id, hash }) =
                StateKey::parse(at.as_bytes())
            else {
                continue;
            };
            if notice.kind != NoticeKind::ListDeliveryUnknown {
                continue;
            }
            let issue = self
                .local_get::<ListReservation>(&key::list_reservation(&to))
                .await?;
            let recipient = self
                .local_get::<ListRecipient>(&key::list_recipient(&to))
                .await?;
            let settled = match (issue, recipient) {
                (Some(issue), Some(recipient)) if issue.id == msg_id && issue.hash == hash => {
                    recipient.answers.first(&issue).is_some() || !issue.unknown_matters(now)
                }
                _ => true,
            };
            if settled {
                tx.delete(&at);
                settled_any = true;
            }
        }
        if !settled_any {
            return Ok(());
        }
        self.local_commit(tx).await
    }

    /// The recipient's current list, under its current choice, whose delivery ended
    /// unknown and whose outcome still matters, unless the recipient has accepted it
    /// since or the user already confirmed another version.
    pub(super) async fn unknown_issue(
        &mut self,
        recipient: &mut ListRecipient,
    ) -> Result<Option<(ListReservation, UnknownListDelivery)>> {
        let at = key::list_reservation(&recipient.recipient);
        let Some(mut issue) = self.local_get::<ListReservation>(&at).await? else {
            return Ok(None);
        };
        if issue.choice != recipient.choice {
            return Ok(None);
        }
        if issue.delivery == FirstDelivery::Open && self.take_prior_list_delivery(&issue.id).await?
        {
            let Some(taken) = self.local_get::<ListReservation>(&at).await? else {
                return Ok(None);
            };
            issue = taken;
        }
        let now = self.service_time().unwrap_or(0);
        if issue.delivery != FirstDelivery::Unknown
            || issue.confirmed
            || !issue.unknown_matters(now)
            || self
                .observe_list_acceptance(recipient, &issue)
                .await?
                .is_some()
        {
            return Ok(None);
        }
        let withdrawal = issue.revision.is_none();
        let shown = UnknownListDelivery {
            recipient: issue.recipient,
            version: ListVersion::new(issue.generation, issue.counter)?,
            msg_id: issue.id.clone(),
            withdrawal,
            payable_until_ms: if withdrawal {
                match issue.predecessors {
                    Payable::Until(end) => Some(end),
                    Payable::Unknown => None,
                }
            } else {
                Some(issue.expires)
            },
        };
        Ok(Some((issue, shown)))
    }
}

impl ListReservation {
    /// Whether an unknown outcome of this list can still matter at `now`: a withdrawal
    /// while an earlier offer may be paid, a nonempty list until it expires.
    pub(super) fn unknown_matters(&self, now: u64) -> bool {
        if self.revision.is_some() {
            return self.expires > now;
        }
        !matches!(self.predecessors, Payable::Until(end) if end <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(predecessors: Payable) -> ListReservation {
        ListReservation {
            publisher: [1; 16],
            recipient: [2; 32],
            generation: 1,
            counter: 2,
            choice: [3; 16],
            revision: None,
            id: "37463d05-59a1-4267-bdd5-f152b03476dc".into(),
            hash: [4; 32],
            expires: 5 * DAY,
            answer: Default::default(),
            missing: None,
            predecessors,
            delivery: FirstDelivery::Undelivered,
            confirmed: false,
            withdrawal: Some(Withdrawal {
                first: 1,
                issued: HOUR + 1,
            }),
        }
    }

    #[test]
    fn test_withdrawal_retries_hourly_then_daily_while_an_older_offer_is_payable() {
        let mut empty = issue(Payable::Until(3 * DAY));
        for (now, due) in [
            (0, false),
            (2 * HOUR, false),
            (2 * HOUR + 1, true),
            (DAY + 1, false),
            (DAY + HOUR, false),
            (DAY + HOUR + 1, true),
            (3 * DAY - 1, true),
            (3 * DAY, false),
        ] {
            assert_eq!(empty.withdrawal_due(now), due, "clock {now}");
        }
        empty.delivery = FirstDelivery::Open;
        assert!(!empty.withdrawal_due(2 * HOUR + 1));
        empty.predecessors = Payable::Unknown;
        assert!(!empty.withdrawal_due(3 * DAY));
        empty.delivery = FirstDelivery::Undelivered;
        assert!(empty.withdrawal_due(3 * DAY));
        empty.delivery = FirstDelivery::Unknown;
        assert!(!empty.withdrawal_due(3 * DAY));
    }

    #[test]
    fn test_withdrawal_unknown_outcome_matters_while_something_can_be_paid() {
        let mut empty = issue(Payable::Until(3 * DAY));
        assert!(empty.unknown_matters(3 * DAY - 1));
        assert!(!empty.unknown_matters(3 * DAY));
        empty.predecessors = Payable::Unknown;
        assert!(empty.unknown_matters(60 * DAY));
        let mut list = issue(Payable::Until(0));
        list.revision = Some([8; 16]);
        list.withdrawal = None;
        assert!(list.unknown_matters(5 * DAY - 1));
        assert!(!list.unknown_matters(5 * DAY));
    }

    #[test]
    fn test_withdrawal_delivery_keeps_a_possible_delivery_over_proof_of_none() {
        use FirstDelivery::{Open, Undelivered, Unknown};
        for (left, right, expected) in [
            (Open, Open, Open),
            (Open, Undelivered, Undelivered),
            (Undelivered, Unknown, Unknown),
            (Open, Unknown, Unknown),
        ] {
            assert_eq!(left.with(right), expected);
            assert_eq!(right.with(left), expected);
        }
    }

    #[test]
    fn test_publication_retains_the_longest_possible_earlier_offer() {
        let mut prior = issue(Payable::Until(7 * DAY));
        assert_eq!(prior.payable_after(None), Payable::Until(7 * DAY));
        assert_eq!(prior.payable_after(Some(DAY)), Payable::Until(0));
        prior.revision = Some([8; 16]);
        prior.withdrawal = None;
        assert_eq!(prior.payable_after(None), Payable::Until(7 * DAY));
        prior.predecessors = Payable::Until(0);
        assert_eq!(prior.payable_after(None), Payable::Until(0));
        prior.delivery = FirstDelivery::Open;
        assert_eq!(prior.payable_after(None), Payable::Until(5 * DAY));
        prior.expires = 60 * DAY;
        assert_eq!(prior.payable_after(Some(DAY)), Payable::Until(31 * DAY));
        prior.predecessors = Payable::Unknown;
        assert_eq!(prior.payable_after(Some(DAY)), Payable::Unknown);
    }
}
