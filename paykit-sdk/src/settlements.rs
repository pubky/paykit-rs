//! A payee's fresh settlement decision and the bucket write that goes with its receipt
//! (Paykit spec 6 and 8.1 step 4, Paykit wire 5.7).

use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, occurrence_tag, settlement_bucket, settlement_tag};
use paykit_lib::records::{
    CreditDecision, CurrentBucket, OccurrenceKind, RecordBinding, SettlementBucket,
};
use pubky_switch_client::app::{KeptReason, OpBatch, RecordRead, RecordTarget, SharedRecord};
use pubky_switch_client::client::Submitted;
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::{Condition, RecordVersion};

use crate::activity::{family, ActivityTarget};
use crate::error::{Refusal, Result};
use crate::paykit::Paykit;

/// A bucket write, or the checked current write for an explicitly confirmed unmarked
/// credit. The condition belongs to the read that supplied the decision.
pub(crate) struct SettlementWrite {
    number: u16,
    target: RecordTarget,
    conditions: Vec<Condition>,
    bucket: Option<SettlementBucket>,
}

pub(crate) enum SettlementCredit {
    SameOccurrence,
    AlreadyCredited,
    Write(SettlementWrite),
}

/// A record accepted as the current value of a settlement bucket.
fn readable(record: &SharedRecord) -> bool {
    !record.flagged
        && !record.not_taken
        && !record.different_write_at_held_version
        && !record.doubtful
        && !record.divergent
}

impl Paykit {
    pub(crate) async fn settlement_credit(
        &mut self,
        id: &[u8],
        activity: &ActivityTarget,
        binding: RecordBinding,
        confirmed: bool,
        unmarked_confirmed: bool,
    ) -> Result<SettlementCredit> {
        let tag = settlement_tag(self.app.naming_key(), id);
        let number = settlement_bucket(&tag);
        let target = RecordTarget::new(
            family(Collection::Settlements),
            &natural_key::settlement_bucket(number),
        );
        let key = self.app.record_key(&target)?;
        let mut seen = self.seen_settlement_marks(number, &key).await?;
        if let Some(record) = self
            .app
            .shared_record(&key)
            .await?
            .filter(|record| record.policy == Collection::Settlements.fixed_policy())
        {
            if let Some(bucket) = record.content.value().and_then(|json| {
                SettlementBucket::bind(json, number, self.app.naming_key(), &key).ok()
            }) {
                seen.add(&bucket)?;
            }
        }

        let read = self.app.read_record_fresh(&target).await?;
        let version = match &read {
            RecordRead::Fresh(record) => Some(record.version),
            _ => None,
        };
        let current = match &read {
            RecordRead::Fresh(record)
                if readable(record) && record.policy == Collection::Settlements.fixed_policy() =>
            {
                match record.content.value() {
                    Some(json) => CurrentBucket::Opened {
                        json,
                        key: &record.key,
                    },
                    None => CurrentBucket::Unopened,
                }
            }
            RecordRead::Fresh(_) => CurrentBucket::Unopened,
            RecordRead::Kept {
                record: None,
                reason: KeptReason::NotFound,
            } => CurrentBucket::Absent,
            // A cached write after a failed read cannot supply an application-time
            // comparison for a receipt, even one the user explicitly confirmed.
            _ => return Err(Refusal::MaybeCredited.into()),
        };
        let kind = match binding {
            RecordBinding::Request { .. } => OccurrenceKind::OneTimeRequest,
            RecordBinding::Period { .. } => OccurrenceKind::BillingPeriod,
            RecordBinding::Unrequested => OccurrenceKind::PaymentWithoutRequest,
        };
        let occurrence = occurrence_tag(self.app.naming_key(), &activity.natural_key);
        let decision = SettlementBucket::decide_credit(
            &tag,
            kind,
            occurrence,
            self.app.naming_key(),
            current,
            &seen,
        )?;
        let bucket = match decision {
            CreditDecision::Marked(mark) if mark.kind == kind && mark.occurrence == occurrence => {
                return Ok(SettlementCredit::SameOccurrence)
            }
            CreditDecision::Marked(_) => return Ok(SettlementCredit::AlreadyCredited),
            CreditDecision::Credit { bucket, .. } => Some(bucket),
            CreditDecision::Full if unmarked_confirmed => None,
            CreditDecision::Full => {
                return Err(Refusal::SettlementBucketFull { bucket: number }.into())
            }
            CreditDecision::NeedsUser if confirmed => None,
            CreditDecision::NeedsUser | CreditDecision::Lost => {
                return Err(Refusal::MaybeCredited.into())
            }
        };
        let current = self.app.record_condition(&key).await?;
        let conditions = bucket_conditions(key, version.unwrap_or(RecordVersion::NONE), current)?;
        Ok(SettlementCredit::Write(SettlementWrite {
            number,
            conditions,
            target,
            bucket,
        }))
    }

    /// Apply the receipt and the requested delivery in one batch. After a definite
    /// size refusal the user can omit the bucket and still compare its checked state.
    pub(crate) async fn submit_settlement_credit(
        &mut self,
        batch: OpBatch,
        write: SettlementWrite,
        unmarked_confirmed: bool,
    ) -> Result<Submitted> {
        let receipt_only =
            |batch: OpBatch| write.conditions.iter().copied().fold(batch, OpBatch::when);
        let Some(bucket) = write.bucket else {
            return Ok(self.submit(receipt_only(batch)).await?);
        };
        let entry = self
            .app
            .record_op(
                &write.target,
                Some(&bucket.to_json()?),
                write.conditions.clone(),
            )
            .await?;
        let submitted = self.submit(batch.clone().entry(entry)).await;
        if submitted.as_ref().err().is_some_and(|err| {
            err.kept_batch().is_none()
                && matches!(
                    err.code(),
                    Some(ErrorCode::QuotaExceeded | ErrorCode::TooLarge)
                )
        }) {
            if unmarked_confirmed {
                return Ok(self.submit(receipt_only(batch)).await?);
            }
            return Err(Refusal::SettlementBucketFull {
                bucket: write.number,
            }
            .into());
        }
        Ok(submitted?)
    }
}

/// The put and a receipt-only write both compare the version that supplied the
/// decision. A recovered write also compares its physical application.
fn bucket_conditions(
    key: [u8; 32],
    version: RecordVersion,
    current: Condition,
) -> Result<Vec<Condition>> {
    let expected = Condition::RecordVersion { key, version };
    match current {
        Condition::RecordVersion { .. } if current == expected => Ok(vec![expected]),
        Condition::RecordCurrent { key: actual, .. } if actual == key && !version.is_none() => {
            Ok(vec![expected, current])
        }
        _ => Err(Refusal::MaybeCredited.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paykit_lib::records::{SeenMarks, SettlementMark};
    use pubky_switch_core::ident::NamingKey;
    use pubky_switch_core::shared::ApplicationRef;

    #[test]
    fn test_distinct_occurrences_compare_one_settlement_bucket() {
        let naming = NamingKey::from_bytes([23; 32]);
        let tag = settlement_tag(&naming, b"one cleared settlement");
        let number = settlement_bucket(&tag);
        let key = SettlementBucket::record_key(number, &naming).unwrap();
        let seen = SeenMarks::new(number);
        let kind = OccurrenceKind::OneTimeRequest;
        let requests = [[1; 32], [2; 32]]
            .map(|hash| occurrence_tag(&naming, &natural_key::request_record(&[3; 32], &hash)));
        assert_ne!(requests[0], requests[1]);
        // A distinct settlement already in this bucket exercises an ordinary update.
        let mut other = tag;
        other[2] ^= 1;
        let CreditDecision::Credit {
            bucket: initial, ..
        } = SettlementBucket::decide_credit(
            &other,
            kind,
            [0; 8],
            &naming,
            CurrentBucket::Absent,
            &seen,
        )
        .unwrap()
        else {
            panic!("a new bucket can hold its first mark")
        };
        let initial = initial.to_json().unwrap();
        let decide = |occurrence, current| {
            SettlementBucket::decide_credit(&tag, kind, occurrence, &naming, current, &seen)
                .unwrap()
        };
        for (version, current) in [
            (RecordVersion::NONE, CurrentBucket::Absent),
            (
                RecordVersion::new(1, 48),
                CurrentBucket::Opened {
                    json: &initial,
                    key: &key,
                },
            ),
        ] {
            let compare = Condition::RecordVersion { key, version };
            let CreditDecision::Credit { bucket: first, .. } = decide(requests[0], current) else {
                panic!("first request is eligible")
            };
            let CreditDecision::Credit { bucket: second, .. } = decide(requests[1], current) else {
                panic!("second request is eligible before either is applied")
            };
            assert_ne!(first, second);
            assert_eq!(bucket_conditions(key, version, compare).unwrap(), [compare]);
            let next = RecordVersion::new(1, 49);
            assert!(bucket_conditions(
                key,
                version,
                Condition::RecordVersion { key, version: next }
            )
            .is_err());
            let first = first.to_json().unwrap();
            assert_eq!(
                SettlementBucket::decide_credit(
                    &tag,
                    kind,
                    requests[1],
                    &naming,
                    CurrentBucket::Opened {
                        json: &first,
                        key: &key
                    },
                    &seen
                )
                .unwrap(),
                CreditDecision::Marked(SettlementMark::new(&tag, kind, requests[0]))
            );
            let restored = Condition::RecordCurrent {
                key,
                revision: ApplicationRef {
                    generation: 2,
                    seq: 70,
                    item_hash: [7; 32],
                },
                deleted: false,
            };
            assert_eq!(
                bucket_conditions(key, next, restored).unwrap(),
                [Condition::RecordVersion { key, version: next }, restored]
            );
        }
    }
}
