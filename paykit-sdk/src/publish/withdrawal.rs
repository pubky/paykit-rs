//! First-delivery evidence and the withdrawal interval for a recipient (Paykit spec 8.4).

use super::{counter_unavailable, target, ListReservation, DAY, HOUR};
use crate::error::Result;
use crate::state::{key, Tx};
use crate::Paykit;
use paykit_lib::bodies::PRIVATE_LIST_MAX_AGE_MS;
use paykit_lib::records::ListPublicationRecord;
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
        if self.revision.is_none() || (self.undelivered && accepted.is_none()) {
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
        if !self.undelivered || matches!(self.predecessors, Payable::Until(end) if end <= now) {
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

    /// Keep definite first non-delivery with the original issue, before acknowledging
    /// its native duty. A failed resend, permanent refusal or unknown outcome cannot
    /// authorize a new empty list.
    pub(crate) async fn take_undelivered_list(&self, tx: &mut Tx, id: &str) -> Result<bool> {
        for (_, mut issue) in self
            .local_scan::<ListReservation>(key::LIST_RESERVATIONS)
            .await?
        {
            if issue.id == id {
                issue.undelivered = true;
                tx.put(&key::list_reservation(&issue.recipient), &issue)?;
                return Ok(true);
            }
        }
        Ok(false)
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
            undelivered: true,
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
        empty.undelivered = false;
        assert!(!empty.withdrawal_due(2 * HOUR + 1));
        empty.predecessors = Payable::Unknown;
        assert!(!empty.withdrawal_due(3 * DAY));
        empty.undelivered = true;
        assert!(empty.withdrawal_due(3 * DAY));
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
        prior.undelivered = false;
        assert_eq!(prior.payable_after(None), Payable::Until(5 * DAY));
        prior.expires = 60 * DAY;
        assert_eq!(prior.payable_after(Some(DAY)), Payable::Until(31 * DAY));
        prior.predecessors = Payable::Unknown;
        assert_eq!(prior.payable_after(Some(DAY)), Payable::Unknown);
    }
}
