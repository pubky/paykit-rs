//! A payee's fresh settlement decision and the bucket write that goes with its receipt
//! (Paykit spec 6 and 8.1 step 4, Paykit wire 5.7).

use paykit_lib::collections::Collection;
use paykit_lib::derive::{occurrence_tag, settlement_bucket, settlement_tag};
use paykit_lib::records::{
    CreditDecision, CurrentBucket, OccurrenceKind, RecordBinding, SettlementBucket,
};
use pubky_switch_client::app::{
    AppError, KeptReason, OpBatch, RecordRead, RecordTarget, SharedRecord,
};
use pubky_switch_client::client::Submitted;
use pubky_switch_client::ClientError;
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::{Condition, RecordVersion};

use crate::activity::ActivityTarget;
use crate::error::{Refusal, Result};
use crate::notices::SettlementSubject;
use crate::paykit::Paykit;

/// A bucket write, or the checked current write for an explicitly confirmed unmarked
/// credit. The condition belongs to the read that supplied the decision.
pub(crate) struct SettlementWrite {
    number: u16,
    payer: [u8; 32],
    target: RecordTarget,
    conditions: Vec<Condition>,
    bucket: Option<SettlementBucket>,
}

pub(crate) struct CreditConsent {
    pub(crate) loss: Option<crate::LossBoundary>,
    pub(crate) uncertain: bool,
    pub(crate) maybe_credited: bool,
    pub(crate) unmarked: bool,
}

impl CreditConsent {
    /// A loss can account for the disappearance only when this bucket's latest verified
    /// physical application precedes it. A cooperative move preserves that application.
    fn permits_lost_bucket(&self, latest_application: Option<u64>) -> bool {
        self.loss
            .zip(latest_application)
            .is_some_and(|(loss, generation)| generation < loss.generation())
            && (!self.uncertain || self.maybe_credited)
    }
}

pub(crate) enum SettlementCredit {
    SameOccurrence,
    AlreadyCredited,
    Write(Box<SettlementWrite>),
}

/// A record accepted as the current value of a settlement bucket.
pub(crate) fn readable(record: &SharedRecord) -> bool {
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
        payer: [u8; 32],
        binding: RecordBinding,
        consent: CreditConsent,
    ) -> Result<SettlementCredit> {
        let tag = settlement_tag(self.app.naming_key(), id);
        let number = settlement_bucket(&tag);
        let target = crate::settlement_marks::target(number);
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

        let read = self.read_settlement_bucket(number, &target, &key).await?;
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
                reason: KeptReason::NotFound,
                ..
            } => CurrentBucket::Absent,
            // Retained marks still apply when the service cannot supply the current write.
            _ => CurrentBucket::Unopened,
        };
        let kind = match binding {
            RecordBinding::Request { .. } => OccurrenceKind::OneTimeRequest,
            RecordBinding::Period { .. } => OccurrenceKind::BillingPeriod,
            RecordBinding::Unrequested => OccurrenceKind::PaymentWithoutRequest,
        };
        let occurrence = occurrence_tag(self.app.naming_key(), &activity.natural_key);
        let decide = |current| {
            SettlementBucket::decide_credit(
                &tag,
                kind,
                occurrence,
                self.app.naming_key(),
                current,
                &seen,
            )
        };
        let mut decision = decide(current)?;
        if decision == CreditDecision::Lost {
            let latest_application = self.latest_settlement_application(number).await?;
            if consent.permits_lost_bucket(latest_application) {
                decision = decide(CurrentBucket::AbsentAfterLoss)?;
            }
        }
        let bucket = match decision {
            CreditDecision::Marked(mark) if mark.kind == kind && mark.occurrence == occurrence => {
                return Ok(SettlementCredit::SameOccurrence)
            }
            CreditDecision::Marked(_) => return Ok(SettlementCredit::AlreadyCredited),
            CreditDecision::Credit { bucket, .. } => Some(bucket),
            CreditDecision::Full => {
                self.raise_settlement_notice(SettlementSubject::Bucket(number), payer)
                    .await?;
                if !consent.unmarked {
                    return Err(Refusal::SettlementBucketFull { bucket: number }.into());
                }
                None
            }
            CreditDecision::NeedsUser | CreditDecision::Lost => {
                self.raise_settlement_notice(SettlementSubject::Tag(tag), payer)
                    .await?;
                if !consent.maybe_credited || version.is_none() || decision == CreditDecision::Lost
                {
                    return Err(Refusal::MaybeCredited.into());
                }
                None
            }
        };
        let conditions = match current {
            // NotFound is a hosted, covering answer. The client can still hold the old
            // generation's live value, which cannot be the base of a create here.
            CurrentBucket::Absent => vec![Condition::RecordVersion {
                key,
                version: RecordVersion::NONE,
            }],
            _ => bucket_conditions(
                key,
                version.ok_or(Refusal::MaybeCredited)?,
                self.app.record_condition(&key).await?,
            )?,
        };
        Ok(SettlementCredit::Write(Box::new(SettlementWrite {
            number,
            payer,
            conditions,
            target,
            bucket,
        })))
    }

    /// Apply the receipt and the requested delivery in one batch. After a definite
    /// size refusal the user can omit the bucket and still compare its checked state.
    pub(crate) async fn submit_settlement_credit(
        &mut self,
        batch: OpBatch,
        write: SettlementWrite,
        unmarked_confirmed: bool,
        delivery_marker: Option<&str>,
    ) -> Result<Submitted> {
        let receipt_only =
            |batch: OpBatch| write.conditions.iter().copied().fold(batch, OpBatch::when);
        let Some(bucket) = write.bucket else {
            return Ok(self.submit(receipt_only(batch)).await?);
        };
        let (mut conditions, mut operation) = self
            .app
            .record_op(
                &write.target,
                Some(&bucket.to_json()?),
                write.conditions.clone(),
            )
            .await?;
        let creates_bucket = matches!(
            write.conditions.as_slice(),
            [Condition::RecordVersion {
                version: RecordVersion::NONE,
                ..
            }]
        );
        if creates_bucket {
            if let pubky_switch_core::ops::OpKind::RecordPut(put) = &mut operation {
                // A covering hosted absence can coexist with a retained pre-loss
                // value. Create at strict zero with its actual blinded name inputs.
                put.expected_version = RecordVersion::NONE;
                put.id_inputs = Some(pubky_switch_core::ops::IdInputs {
                    collection: write.target.collection.name.clone(),
                    blinded_key: self.app.naming_key().blind(&write.target.natural_key),
                });
                conditions.clone_from(&write.conditions);
            }
        }
        let error = match self
            .submit(batch.clone().entry((conditions, operation)))
            .await
        {
            Ok(submitted) => return Ok(submitted),
            Err(error) => error,
        };
        if let AppError::Kept { error: cause, .. } = &error {
            // The receipt updates its checked live occurrence. Only the new bucket
            // needs another record slot (Paykit wire 5.7, Switch wire 6.4 and 9.3).
            if creates_bucket && refused_record_key(cause) {
                // A failed notice write must still return the original pending batch ID.
                let _ = self
                    .raise_settlement_notice(SettlementSubject::Bucket(write.number), write.payer)
                    .await;
            }
        } else if matches!(&error, AppError::Switch(answer)
            if matches!(answer.code(), Some(ErrorCode::QuotaExceeded | ErrorCode::TooLarge)))
        {
            self.raise_settlement_notice(SettlementSubject::Bucket(write.number), write.payer)
                .await?;
            if unmarked_confirmed {
                self.retire_refused_receipt_batch(delivery_marker).await?;
                return Ok(self.submit(receipt_only(batch)).await?);
            }
            return Err(Refusal::SettlementBucketFull {
                bucket: write.number,
            }
            .into());
        }
        Err(error.into())
    }

    /// A definite refusal releases the original batch's marker before a fresh decision.
    /// An interrupted call can still have applied and retains its exact ID.
    pub(crate) async fn retire_refused_receipt_batch(
        &mut self,
        marker: Option<&str>,
    ) -> Result<()> {
        use pubky_switch_client::app::{KeptBatchDrop, KeptBatchStatus};
        let Some(key) = marker else { return Ok(()) };
        let Some(state) = self
            .local_get::<crate::receipt_delivery::ReceiptBatch>(key)
            .await?
        else {
            return Ok(());
        };
        if matches!(
            self.app.kept_batch(state.id()).await?,
            Some(KeptBatchStatus::Dropped {
                reason: KeptBatchDrop::Unsent | KeptBatchDrop::Refused { .. },
                ..
            })
        ) {
            self.local_delete(key).await?;
        }
        Ok(())
    }
}

/// The verified service answer for this submission (Switch wire 7.6 and 9.3).
fn refused_record_key(cause: &AppError) -> bool {
    matches!(cause, AppError::Switch(ClientError::Switch {
        status: 429, code: ErrorCode::QuotaExceeded, current, ..
    }) if current.get("limit").and_then(serde_json::Value::as_str) == Some("record_keys"))
}

/// The put and a receipt-only write both compare the version that supplied the
/// decision. A recovered write also compares its physical application.
pub(crate) fn bucket_conditions(
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
    use paykit_lib::derive::natural_key;
    use paykit_lib::records::{SeenMarks, SettlementMark};
    use pubky_switch_core::ident::NamingKey;
    use pubky_switch_core::shared::ApplicationRef;

    #[test]
    fn test_only_the_signed_record_key_limit_identifies_a_new_bucket_refusal() {
        let answer = |code, limit| {
            AppError::Switch(ClientError::Switch {
                status: 429,
                code,
                current: serde_json::json!({ "limit": limit }),
                message: String::new(),
                known_version: None,
                retry_after: None,
            })
        };
        assert!(refused_record_key(&answer(
            ErrorCode::QuotaExceeded,
            "record_keys"
        )));
        assert!(!refused_record_key(&answer(
            ErrorCode::RateLimited,
            "record_keys"
        )));
        assert!(!refused_record_key(&answer(
            ErrorCode::QuotaExceeded,
            "storage"
        )));
        assert!(!refused_record_key(&answer(
            ErrorCode::QuotaExceeded,
            "contacts"
        )));
    }

    #[test]
    fn test_only_an_intervening_loss_can_release_an_absent_bucket() {
        let loss = crate::LossBoundary {
            generation: 4,
            declared_at_ms: 50,
            uncertain_until_ms: 100,
        };
        for (boundary, latest, paid_at, confirmed, expected) in [
            (None, Some(2), 101, true, false),
            (Some(loss), None, 101, true, false),
            (Some(loss), Some(4), 101, true, false),
            (Some(loss), Some(5), 101, true, false),
            (Some(loss), Some(2), 99, false, false),
            (Some(loss), Some(2), 100, false, false),
            (Some(loss), Some(2), 100, true, true),
            (Some(loss), Some(2), 101, false, true),
        ] {
            let choice = CreditConsent {
                loss: boundary,
                uncertain: boundary.is_some_and(|loss| loss.settlement_uncertain(paid_at)),
                maybe_credited: confirmed,
                unmarked: false,
            };
            assert_eq!(choice.permits_lost_bucket(latest), expected);
        }
    }

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
            let receipt_conditions = bucket_conditions(key, version, compare).unwrap();
            assert_eq!(receipt_conditions, [compare]);
            let application = |v: RecordVersion| ApplicationRef {
                generation: v.generation,
                seq: v.version,
                item_hash: [4; 32],
            };
            let before = (!version.is_none()).then(|| (version, application(version)));
            assert!(pubky_switch_core::shared::version_holds(
                &key,
                version,
                before,
                &receipt_conditions
            ));
            let next = RecordVersion::new(1, 49);
            // The same receipt-only comparison no longer holds after the first request wins.
            assert!(!pubky_switch_core::shared::version_holds(
                &key,
                version,
                Some((next, application(next))),
                &receipt_conditions
            ));
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
