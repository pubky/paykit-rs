//! Authenticated issues seen at a permanent publication key (Paykit wire 5.8).

use super::{counter_unavailable, target};
use crate::error::Result;
use crate::state::{self, key};
use crate::Paykit;
use paykit_lib::records::{ListPublicationRecord, UnknownMembers};
use pubky_switch_client::app::{LocalWrite, ReceiveContext, RecordObservation, StoreFailure};
use serde::{Deserialize, Serialize};

/// A conflicting counter signed by this publishing app. Both issues name the same
/// publisher, recipient and generation. Neither the report nor its acknowledgment
/// authorizes reserving another version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PublicationConflict {
    /// Hash of the authenticated service application that showed the refused issue.
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    pub application_hash: [u8; 32],
    /// Another distinct conflicting application at this key was also observed before
    /// acknowledgment. The report identifies one exact application and its two issues.
    pub other_applications: bool,
    /// The bound issue this app held when it observed the conflict.
    #[serde(with = "issue")]
    pub held: ListPublicationRecord,
    /// The later observed issue with a lower counter or another message at the same counter.
    #[serde(with = "issue")]
    pub refused: ListPublicationRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationHistory {
    #[serde(with = "issue")]
    pub(crate) floor: ListPublicationRecord,
    /// First verified service time at which a request found this exact counter
    /// ahead of the local signed publication. Reading it again cannot renew the gap.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(super) missing_since: Option<u64>,
    conflict: Option<PublicationConflict>,
    acknowledged: bool,
}

// The wire codec validates the UUID, counter, generation and fixed-size identifiers.
// A historical issue needs only the known fields. Unknown members stay on the
// current service value and are carried into its successor.
mod issue {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &ListPublicationRecord,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let bytes = value.to_json().map_err(serde::ser::Error::custom)?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<ListPublicationRecord, D::Error> {
        let json = serde_json::Value::deserialize(deserializer)?;
        let bytes = serde_json::to_vec(&json).map_err(serde::de::Error::custom)?;
        let value = ListPublicationRecord::read(&bytes).map_err(serde::de::Error::custom)?;
        if value.unknown != UnknownMembers::default() {
            return Err(serde::de::Error::custom(
                "unknown publication history member",
            ));
        }
        Ok(value)
    }
}

impl PublicationHistory {
    fn new(floor: ListPublicationRecord) -> Self {
        Self {
            floor,
            missing_since: None,
            conflict: None,
            acknowledged: false,
        }
    }

    pub(super) fn qualified_issues(&self) -> impl Iterator<Item = &ListPublicationRecord> {
        std::iter::once(&self.floor).chain(self.conflict.iter().map(|report| &report.held))
    }

    pub(crate) fn check_binding(
        &self,
        record_key: &[u8; 32],
        binder: &state::Binder<'_>,
        publisher: [u8; 16],
    ) -> Result<()> {
        let floor = &self.floor;
        if floor.publisher != publisher
            || floor.recipient == *binder.identity()
            || binder.record_key(&target(&publisher, &floor.recipient, floor.generation))?
                != *record_key
        {
            return Err(counter_unavailable().into());
        }
        if self.acknowledged && self.conflict.is_none() {
            return Err(counter_unavailable().into());
        }
        if let Some(report) = &self.conflict {
            let same_key = |r: &ListPublicationRecord| {
                r.publisher == publisher
                    && r.recipient == floor.recipient
                    && r.generation == floor.generation
            };
            if !same_key(&report.held)
                || !same_key(&report.refused)
                || report.held.check_later_write(floor).is_err()
                || report.held.check_later_write(&report.refused).is_ok()
            {
                return Err(counter_unavailable().into());
            }
        }
        Ok(())
    }

    pub(crate) fn check_current(&self, current: &ListPublicationRecord) -> Result<()> {
        // A newly held value can arrive without covering pending uploads. Its numbers
        // alone do not give it the standing of this owner's qualified observation.
        self.floor.check_later_write(current)?;
        if self.floor.counter != current.counter {
            return Err(counter_unavailable().into());
        }
        Ok(())
    }

    fn observe(&mut self, current: ListPublicationRecord, application_hash: [u8; 32]) {
        if self.floor.check_later_write(&current).is_ok() {
            if self.floor.counter != current.counter {
                self.missing_since = None;
            }
            self.floor = current;
        } else if self
            .conflict
            .as_ref()
            .is_none_or(|report| report.application_hash != application_hash)
        {
            self.conflict = Some(PublicationConflict {
                application_hash,
                other_applications: self.conflict.is_some() && !self.acknowledged,
                held: self.floor.clone(),
                refused: current,
            });
            self.acknowledged = false;
        }
    }

    pub(crate) fn merge(&mut self, other: Self) -> Result<()> {
        if self.floor.counter <= other.floor.counter {
            self.floor.check_later_write(&other.floor)?;
            self.missing_since = if self.floor.counter == other.floor.counter {
                self.missing_since
                    .into_iter()
                    .chain(other.missing_since)
                    .min()
            } else {
                other.missing_since
            };
            self.floor = other.floor;
        } else {
            other.floor.check_later_write(&self.floor)?;
        }
        if let Some(report) = other.conflict {
            match &mut self.conflict {
                Some(kept) if kept.application_hash == report.application_hash => {
                    if kept.refused != report.refused {
                        return Err(counter_unavailable().into());
                    }
                    if kept.held.counter <= report.held.counter {
                        kept.held.check_later_write(&report.held)?;
                        kept.held = report.held;
                    }
                    if kept.other_applications || report.other_applications {
                        self.acknowledged &= other.acknowledged;
                    } else {
                        self.acknowledged |= other.acknowledged;
                    }
                    kept.other_applications |= report.other_applications;
                }
                Some(kept) if !self.acknowledged => {
                    kept.other_applications |= !other.acknowledged;
                }
                _ => {
                    self.conflict = Some(report);
                    self.acknowledged = other.acknowledged;
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn observe(
    observation: &RecordObservation<'_>,
    cx: &ReceiveContext<'_>,
) -> std::result::Result<Vec<LocalWrite>, StoreFailure> {
    let record = observation.record;
    if !observation.current
        || record.collection != super::COLLECTION
        || record.writer != cx.cert().app_id()
        || !cx.cert().holds_scope(super::SCOPE)
    {
        return Ok(Vec::new());
    }
    let Ok(mut current) = super::bound_publication(record, |target| cx.record_key(target)) else {
        return Ok(Vec::new());
    };
    if current.recipient == cx.identity() {
        return Ok(Vec::new());
    }
    current.unknown = UnknownMembers::default();
    let at = key::publication_history(&record.key);
    let previous = observed_local::<PublicationHistory>(cx, &at)?;
    let applied =
        observed_local::<super::ListReservation>(cx, &key::list_reservation(&current.recipient))?
            .filter(|issue| issue.generation == current.generation)
            .map(|issue| ListPublicationRecord {
                publisher: current.publisher,
                recipient: issue.recipient,
                generation: issue.generation,
                counter: issue.counter,
                msg_id: issue.id,
                message_hash: issue.hash,
                unknown: UnknownMembers::default(),
            });
    let mut next = previous.clone().unwrap_or_else(|| {
        PublicationHistory::new(applied.clone().unwrap_or_else(|| current.clone()))
    });
    if let Some(applied) = applied.filter(|issue| issue.counter > next.floor.counter) {
        next.floor = applied;
        next.missing_since = None;
    }
    next.observe(current, observation.application.item_hash);
    if previous.as_ref() == Some(&next) {
        return Ok(Vec::new());
    }
    Ok(vec![
        state::local_put(&at, &next).map_err(crate::protocol::view_failure)?
    ])
}

fn observed_local<T: state::Stored>(
    cx: &ReceiveContext<'_>,
    at: &str,
) -> std::result::Result<Option<T>, StoreFailure> {
    state::cx_get(cx, at).map_err(|error| match error {
        crate::receive::Fail::Store(error) => error,
        _ => StoreFailure::of_entry(pubky_switch_client::error::StoreError::Corrupt),
    })
}

impl Paykit {
    /// An outstanding report of conflicting authenticated issues at each of this app's publication
    /// keys. A later consistent current counter permits ordinary publication and leaves
    /// the report available until the host acknowledges the exact observation.
    ///
    /// # Errors
    ///
    /// A store failure or an invalid binding in retained state.
    pub async fn publication_conflicts(&self) -> Result<Vec<PublicationConflict>> {
        Ok(self
            .local_scan::<PublicationHistory>(key::PUBLICATION_HISTORY)
            .await?
            .into_iter()
            .filter_map(|(_, history)| history.conflict.filter(|_| !history.acknowledged))
            .collect())
    }

    /// Acknowledge a displayed conflict. The floor and any newer report are retained.
    ///
    /// # Errors
    ///
    /// A store failure or a counter key that cannot be derived.
    pub async fn acknowledge_publication_conflict(
        &mut self,
        report: &PublicationConflict,
    ) -> Result<()> {
        let issue = &report.held;
        let key = self.app.record_key(&target(
            &issue.publisher,
            &issue.recipient,
            issue.generation,
        ))?;
        let at = key::publication_history(&key);
        if let Some(mut history) = self.local_get::<PublicationHistory>(&at).await? {
            if !history.acknowledged && history.conflict.as_ref() == Some(report) {
                history.acknowledged = true;
                self.local_put(&at, &history).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(counter: u64, second: bool) -> ListPublicationRecord {
        ListPublicationRecord {
            publisher: [1; 16],
            recipient: [2; 32],
            generation: 3,
            counter,
            msg_id: if second {
                "3198762f-3816-4cd4-86e2-4a8dc40fc737"
            } else {
                "a5dbfc07-4d85-4b39-bd47-29ff28493ce1"
            }
            .into(),
            message_hash: [u8::from(second); 32],
            unknown: UnknownMembers::default(),
        }
    }

    #[test]
    fn test_history_replays_the_exact_issue_and_reports_each_kind_of_conflict() {
        let floor = issue(10, false);
        let other = issue(10, true);
        let mut changed_id = floor.clone();
        changed_id.msg_id = other.msg_id.clone();
        let mut changed_hash = floor.clone();
        changed_hash.message_hash = other.message_hash;
        for refused in [issue(9, false), changed_id, changed_hash] {
            let mut history = PublicationHistory::new(floor.clone());
            history.observe(floor.clone(), [1; 32]);
            assert!(history.conflict.is_none());
            history.observe(refused.clone(), [2; 32]);
            assert!(history.check_current(&refused).is_err());
            assert!(history.check_current(&issue(11, true)).is_err());
            assert_eq!(history.floor, floor);
            let report = PublicationConflict {
                application_hash: [2; 32],
                other_applications: false,
                held: floor.clone(),
                refused,
            };
            assert_eq!(history.conflict.as_ref(), Some(&report));
            let persisted = serde_json::to_vec(&history).unwrap();
            let mut reopened: PublicationHistory = serde_json::from_slice(&persisted).unwrap();
            reopened.observe(floor.clone(), [3; 32]);
            assert_eq!(reopened, history);
            reopened.acknowledged = true;
            reopened.observe(report.refused.clone(), [2; 32]);
            assert!(reopened.acknowledged);
            reopened.observe(issue(11, true), [4; 32]);
            assert_eq!(reopened.floor, issue(11, true));
            reopened.check_current(&issue(11, true)).unwrap();
            assert_eq!(reopened.conflict, Some(report));
        }
    }

    #[test]
    fn test_history_restore_binds_a_report_to_its_authenticated_publisher_and_key() {
        let publisher = crate::testkit::Party::new(
            "merchant",
            "publisher",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let blind = |natural: &[u8]| pubky_switch_core::enc::hash(natural);
        let binder = state::Binder::new(publisher.id(), &publisher.cert, &blind);
        let mut held = issue(8, false);
        held.publisher = publisher.cert.app_id();
        let target = target(&held.publisher, &held.recipient, held.generation);
        let physical = binder.record_key(&target).unwrap();
        let at = key::publication_history(&physical);
        let mut history = PublicationHistory::new(held.clone());
        let mut lower = held.clone();
        lower.counter = 7;
        history.observe(lower, [5; 32]);
        let check = |entry: &PublicationHistory, key: &str| {
            let bytes = state::encode(entry).unwrap();
            state::restored::<PublicationHistory>(key.as_bytes(), &bytes, &binder)
        };
        assert_eq!(check(&history, &at).unwrap(), history);
        assert!(check(&history, &key::publication_history(&[8; 32])).is_err());
        let mut wrong = history.clone();
        wrong.floor.publisher = [9; 16];
        assert!(check(&wrong, &at).is_err());
        let mut wrong = history.clone();
        wrong.conflict.as_mut().unwrap().refused.recipient = [4; 32];
        assert!(check(&wrong, &at).is_err());
        let mut wrong = history;
        wrong.conflict.as_mut().unwrap().refused = held;
        assert!(check(&wrong, &at).is_err());
    }

    #[test]
    fn test_history_restore_compares_applied_and_observed_issues_before_installation() {
        use super::super::{ListReservation, Payable, RestoredPublications, Withdrawal};

        let publisher = crate::testkit::Party::new(
            "merchant",
            "publisher",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let blind = |input: &[u8]| pubky_switch_core::enc::hash(input);
        let binder = state::Binder::new(publisher.id(), &publisher.cert, &blind);
        let mut actual = issue(9, false);
        actual.publisher = publisher.cert.app_id();
        let reservation = ListReservation {
            answer: Default::default(),
            missing: None,
            expires: 1000,
            publisher: actual.publisher,
            predecessors: Payable::Unknown,
            delivery: crate::publish::FirstDelivery::Open,
            confirmed: false,
            withdrawal: Some(Withdrawal {
                first: 0,
                issued: 0,
            }),
            recipient: actual.recipient,
            choice: [7; 16],
            revision: None,
            generation: actual.generation,
            counter: actual.counter,
            id: actual.msg_id.clone(),
            hash: actual.message_hash,
        };
        let reservation_at = key::list_reservation(&actual.recipient);
        let reservation_bytes = state::encode(&reservation).unwrap();
        let physical = binder
            .record_key(&target(
                &actual.publisher,
                &actual.recipient,
                actual.generation,
            ))
            .unwrap();
        let history_at = key::publication_history(&physical);
        let mut surprises = Vec::new();
        for (case, refuse) in [
            ("same", false),
            ("id", true),
            ("hash", true),
            ("newer", false),
        ] {
            let mut observed = actual.clone();
            match case {
                "id" => observed.msg_id = issue(9, true).msg_id,
                "hash" => observed.message_hash = [77; 32],
                "newer" => {
                    observed.counter += 1;
                    observed.message_hash = [77; 32];
                }
                _ => {}
            }
            let history = state::encode(&PublicationHistory::new(observed)).unwrap();
            for observed_first in [false, true] {
                let mut restored = RestoredPublications::default();
                let applied = |r: &mut RestoredPublications| {
                    r.applied(reservation_at.as_bytes(), &reservation_bytes, &binder)
                };
                let seen = |r: &mut RestoredPublications| {
                    r.observed(history_at.as_bytes(), &history, &binder)
                };
                let result = if observed_first {
                    seen(&mut restored).and_then(|()| applied(&mut restored))
                } else {
                    applied(&mut restored).and_then(|()| seen(&mut restored))
                }
                .and_then(|()| restored.writes());
                if result.is_err() != refuse {
                    surprises.push((case, observed_first, result.is_err()));
                }
            }
        }
        assert!(
            surprises.is_empty(),
            "unexpected restore dispositions (case, observed first, refused): {surprises:?}"
        );

        // A higher applied reservation in the other installation still cannot erase
        // what the earlier reservation establishes at its own pair.
        let higher = ListReservation {
            counter: actual.counter + 1,
            id: issue(10, true).msg_id,
            hash: [9; 32],
            ..reservation
        };
        let mut alternate = actual;
        alternate.message_hash = [77; 32];
        let history = state::encode(&PublicationHistory::new(alternate)).unwrap();
        let mut restored = RestoredPublications::default();
        restored
            .applied(reservation_at.as_bytes(), &reservation_bytes, &binder)
            .unwrap();
        restored
            .applied(
                reservation_at.as_bytes(),
                &state::encode(&higher).unwrap(),
                &binder,
            )
            .unwrap();
        assert!(restored
            .observed(history_at.as_bytes(), &history, &binder)
            .is_err());
    }

    #[test]
    fn test_history_restore_keeps_the_greater_qualified_issue_and_the_report() {
        let mut higher = PublicationHistory::new(issue(10, false));
        higher.observe(issue(9, false), [5; 32]);
        let lower = PublicationHistory::new(issue(7, true));
        for (mut a, b) in [(lower.clone(), higher.clone()), (higher.clone(), lower)] {
            a.merge(b).unwrap();
            assert_eq!(a, higher);
        }
        let mut elsewhere = PublicationHistory::new(issue(10, false));
        elsewhere.observe(issue(8, true), [6; 32]);
        let mut merged = higher.clone();
        merged.merge(elsewhere.clone()).unwrap();
        assert!(merged.conflict.unwrap().other_applications);
        elsewhere.observe(issue(9, false), [5; 32]);
        assert!(elsewhere.conflict.unwrap().other_applications);
        assert!(higher
            .merge(PublicationHistory::new(issue(10, true)))
            .is_err());
    }
}
