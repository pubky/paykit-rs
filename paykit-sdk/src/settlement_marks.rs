//! Settlement marks retained by this payee after taking a shared bucket (Paykit wire 5.7).

use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, SETTLEMENT_BUCKETS};
use paykit_lib::records::{
    SeenMarks, SeenMerge, SettlementBucket, SettlementMark, SETTLEMENT_MARK_LEN,
};
use pubky_dispatch_client::app::{
    KeptReason, LocalWrite, ReceiveContext, RecordObservation, RecordRead, RecordTarget,
    SharedRecord, StoreFailure,
};
use pubky_dispatch_core::enc::{b64, serde_b64};
use pubky_dispatch_core::ops::Condition;
use pubky_dispatch_core::shared::ApplicationRef;
use serde::{Deserialize, Serialize};

use crate::activity::family;
use crate::error::Result;
use crate::notices::{KeptNotice, SettlementSubject};
use crate::paykit::Paykit;
use crate::protocol::{step_failure, store_failure, view_failure};
use crate::state::{self, key};

/// The wire merge can report conflicting occurrences even when it produces no new value.
struct BucketRepair {
    write: Option<SettlementBucket>,
    twice: Vec<[u8; paykit_lib::derive::SETTLEMENT_TAG_LEN]>,
    show_writer: bool,
    full: bool,
}

impl From<SeenMerge> for BucketRepair {
    fn from(outcome: SeenMerge) -> Self {
        match outcome {
            SeenMerge::Holds { twice } => Self {
                write: None,
                twice,
                show_writer: false,
                full: false,
            },
            SeenMerge::WriteBack {
                bucket,
                twice,
                show_writer,
            } => Self {
                write: Some(bucket),
                twice,
                show_writer,
                full: false,
            },
            SeenMerge::OverCapacity {
                bucket,
                twice,
                show_writer,
            } => Self {
                write: bucket,
                twice,
                show_writer,
                full: true,
            },
        }
    }
}

/// At most one item per permanent bucket. A covering read is required before writing.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BucketHistory {
    pub(crate) number: u16,
    pub(crate) generation: u64,
    pub(crate) repair: bool,
}

impl BucketHistory {
    pub(crate) fn include(&mut self, other: Self) {
        self.generation = self.generation.max(other.generation);
        self.repair |= other.repair;
    }
}

struct BucketObservation<'a> {
    bucket: &'a SettlementBucket,
    number: u16,
    key: &'a [u8; 32],
    writer: [u8; 16],
    application: ApplicationRef,
    // The generation in which this was seen live, when the observation covers it.
    live: Option<u64>,
}

impl BucketObservation<'_> {
    fn changes(
        &self,
        old: &SeenMarks,
        previous: Option<BucketHistory>,
    ) -> Result<(Vec<LocalWrite>, Vec<SettlementSubject>)> {
        let mut writes = Vec::new();
        let prefix = prefix(self.key);
        for mark in self.bucket.marks() {
            if old.marks().binary_search(mark).is_err() {
                let bytes = mark.to_bytes();
                let observed = ObservedMark {
                    bucket: self.number,
                    mark: bytes,
                };
                writes.push(state::local_put(
                    &format!("{prefix}{}", b64(&bytes)),
                    &observed,
                )?);
            }
        }
        let mut history = BucketHistory {
            number: self.number,
            generation: self.application.generation,
            repair: false,
        };
        if let Some(previous) = previous {
            history.include(previous);
        }
        let plan: BucketRepair = self.bucket.merged_with_seen(old)?.into();
        let mut notices = plan
            .twice
            .into_iter()
            .map(|settlement| SettlementSubject::Conflicting {
                bucket: self.number,
                settlement,
            })
            .collect::<Vec<_>>();
        if let Some(live) = self.live {
            history.generation = history.generation.max(live);
            history.repair = plan.write.is_some();
            if plan.show_writer {
                notices.push(SettlementSubject::Changed {
                    bucket: self.number,
                    writer: self.writer,
                    application: self.application,
                });
            }
            if plan.full {
                notices.push(SettlementSubject::Bucket(self.number));
            }
        } else if !writes.is_empty() {
            history.repair = true;
        }
        if previous != Some(history) {
            writes.push(state::local_put(
                &key::settlement_bucket(self.key),
                &history,
            )?);
        }
        Ok((writes, notices))
    }
}

fn notice_write(subject: SettlementSubject, at: u64) -> Result<(String, LocalWrite)> {
    let name = format!("{}{}", key::SETTLEMENT_NOTICES, subject.path());
    let put = state::local_put(&name, &KeptNotice::settlement(subject, None, at))?;
    Ok((name, put))
}

/// One positive observation. The physical bucket and the complete mark are bound to its key.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservedMark {
    pub(crate) bucket: u16,
    #[serde(with = "serde_b64")]
    pub(crate) mark: [u8; SETTLEMENT_MARK_LEN],
}

pub(crate) fn target(bucket: u16) -> RecordTarget {
    RecordTarget::new(
        family(Collection::Settlements),
        &natural_key::settlement_bucket(bucket),
    )
}

fn prefix(key: &[u8; 32]) -> String {
    format!("{}{}/", key::SETTLEMENT_MARKS, b64(key))
}

pub(crate) fn observe(
    observation: &RecordObservation<'_>,
    cx: &ReceiveContext<'_>,
) -> std::result::Result<Vec<LocalWrite>, StoreFailure> {
    let record = observation.record;
    if record.collection != Collection::Settlements.name()
        || record.policy != Collection::Settlements.fixed_policy()
    {
        return Ok(Vec::new());
    }
    let Some(value) = record.content.value() else {
        return Ok(Vec::new());
    };
    // The encrypted value carries no bucket number. Only one of the 1,024
    // natural keys under this identity and permanent policy can bind it.
    for number in 0..SETTLEMENT_BUCKETS {
        if cx.record_key(&target(number)).map_err(store_failure)? != record.key {
            continue;
        }
        let Ok(bucket) =
            SettlementBucket::bind_with_blinding(value, number, &record.key, |input| {
                cx.blind(input)
            })
        else {
            return Ok(Vec::new());
        };
        let saved =
            state::cx_scan::<ObservedMark>(cx, &prefix(&record.key)).map_err(step_failure)?;
        let old = seen(number, saved).map_err(view_failure)?;
        let previous = state::cx_get::<BucketHistory>(cx, &key::settlement_bucket(&record.key))
            .map_err(step_failure)?;
        let live = if observation.current && crate::settlements::readable(record) {
            let loss = state::cx_get::<crate::LossBoundary>(cx, key::LOSS_BOUNDARY)
                .map_err(step_failure)?;
            Some(loss.map_or(observation.application.generation, |loss| loss.generation()))
        } else {
            None
        };
        let source = BucketObservation {
            bucket: &bucket,
            number,
            key: &record.key,
            writer: record.writer,
            application: observation.application,
            live,
        };
        let (mut writes, notices) = source.changes(&old, previous).map_err(view_failure)?;
        for subject in notices {
            let (name, put) = notice_write(subject, cx.highest_service_time().unwrap_or(0))
                .map_err(view_failure)?;
            if state::cx_get::<KeptNotice>(cx, &name)
                .map_err(step_failure)?
                .is_none()
            {
                writes.push(put);
            }
        }
        return Ok(writes);
    }
    Ok(Vec::new())
}

fn seen(number: u16, saved: Vec<(String, ObservedMark)>) -> Result<SeenMarks> {
    let marks = saved
        .into_iter()
        .map(|(_, saved)| SettlementMark::from_bytes(&saved.mark))
        .collect::<paykit_lib::Result<_>>()?;
    Ok(SeenMarks::from_retained(number, marks)?)
}

impl Paykit {
    pub(crate) async fn seen_settlement_marks(
        &self,
        bucket: u16,
        key: &[u8; 32],
    ) -> Result<SeenMarks> {
        seen(bucket, self.local_scan::<ObservedMark>(&prefix(key)).await?)
    }

    pub(crate) async fn latest_settlement_application(&self, number: u16) -> Result<Option<u64>> {
        let key = self.app.record_key(&target(number))?;
        Ok(self
            .local_get::<BucketHistory>(&key::settlement_bucket(&key))
            .await?
            .map(|history| history.generation))
    }

    /// The writer of an opened upload and the checked physical application can be
    /// known even when the old service's version proof isn't available. Warnings,
    /// marks and repair decisions use the same merge as the record observer.
    async fn upload_changes(&self, number: u16, record: &SharedRecord) -> Result<Vec<LocalWrite>> {
        if record.authenticated
            || !crate::settlements::readable(record)
            || record.policy != Collection::Settlements.fixed_policy()
        {
            return Ok(Vec::new());
        }
        let key = self.app.record_key(&target(number))?;
        if record.key != key {
            return Ok(Vec::new());
        }
        let Condition::RecordCurrent {
            key: physical,
            revision,
            deleted: false,
        } = self.app.record_condition(&key).await?
        else {
            return Ok(Vec::new());
        };
        if physical != key {
            return Ok(Vec::new());
        }
        let Some(opened) = record.content.value().and_then(|bytes| {
            SettlementBucket::bind(bytes, number, self.app.naming_key(), &key).ok()
        }) else {
            return Ok(Vec::new());
        };
        let old = self.seen_settlement_marks(number, &key).await?;
        let previous = self
            .local_get::<BucketHistory>(&key::settlement_bucket(&key))
            .await?;
        let source = BucketObservation {
            bucket: &opened,
            number,
            key: &key,
            writer: record.writer,
            application: revision,
            live: Some(self.app.own_identity().generation()),
        };
        let (mut writes, notices) = source.changes(&old, previous)?;
        for subject in notices {
            let (name, put) = notice_write(subject, self.app.highest_service_time().unwrap_or(0))?;
            if self.local_get::<KeptNotice>(&name).await?.is_none() {
                writes.push(put);
            }
        }
        Ok(writes)
    }

    pub(crate) async fn read_settlement_bucket(
        &mut self,
        number: u16,
        target: &RecordTarget,
        key: &[u8; 32],
    ) -> Result<RecordRead> {
        let read = self.app.read_record_fresh(target).await?;
        let name = key::settlement_bucket(key);
        if let RecordRead::Fresh(record) = &read {
            let writes = self.upload_changes(number, record).await?;
            if !writes.is_empty() {
                self.app
                    .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
                    .await?;
            }
        }
        if let Some(mut history) = self.local_get::<BucketHistory>(&name).await? {
            match &read {
                RecordRead::Fresh(record)
                    if record.authenticated
                        && crate::settlements::readable(record)
                        && record.policy == target.collection.policy
                        && record.content.value().is_some() =>
                {
                    // This covering read saw the value in the current service generation.
                    let live = self.app.own_identity().generation();
                    if live > history.generation {
                        history.generation = live;
                        self.local_put(&name, &history).await?;
                    }
                }
                RecordRead::Kept {
                    reason: KeptReason::NotFound,
                    ..
                } => {
                    let loss = self.settlement_loss().await?;
                    if loss.is_none_or(|loss| history.generation >= loss.generation()) {
                        let reason = "Paykit spec 6: a permanent settlement bucket seen live since the last provider loss reads absent";
                        let alarm = self
                            .app
                            .raise_rollback(crate::protocol::BACKUP_NAME, reason)
                            .await?;
                        return Err(pubky_dispatch_client::app::AppError::Alarm(alarm).into());
                    }
                    if history.repair {
                        // No present value needs repair. A later observation compares every retained mark again.
                        history.repair = false;
                        self.local_put(&name, &history).await?;
                    }
                }
                _ => {}
            }
        }
        Ok(read)
    }

    pub(crate) async fn repair_settlement_buckets(&mut self) -> Result<()> {
        if !self.holds(paykit_lib::scopes::SETTLEMENTS) {
            return Ok(());
        }
        let recovered = self.settlement_loss().await?.is_some();
        for (_, history) in self
            .local_scan::<BucketHistory>(key::SETTLEMENT_BUCKETS)
            .await?
        {
            if !history.repair {
                if !recovered {
                    continue;
                }
                let key = self.app.record_key(&target(history.number))?;
                let Some(held) = self.app.shared_record(&key).await? else {
                    continue;
                };
                // A recovery upload with no old-version proof need not trigger the
                // signed-version observer. Local evidence only schedules the fresh read.
                if self.upload_changes(history.number, &held).await?.is_empty() {
                    continue;
                }
            }
            Box::pin(self.repair_settlement_bucket(history.number)).await?;
        }
        Ok(())
    }

    async fn repair_settlement_bucket(&mut self, number: u16) -> Result<()> {
        let target = target(number);
        let key = self.app.record_key(&target)?;
        let RecordRead::Fresh(current) = self.read_settlement_bucket(number, &target, &key).await?
        else {
            return Ok(());
        };
        if !crate::settlements::readable(&current) || current.policy != target.collection.policy {
            return Ok(());
        }
        let Some(bytes) = current.content.value() else {
            return Ok(());
        };
        let Ok(opened) = SettlementBucket::bind(bytes, number, self.app.naming_key(), &key) else {
            return Ok(());
        };
        let seen = self.seen_settlement_marks(number, &key).await?;
        let plan: BucketRepair = opened.merged_with_seen(&seen)?.into();
        if let Some(value) = plan.write {
            let condition = self.app.record_condition(&key).await?;
            if !current.authenticated
                && !matches!(condition, Condition::RecordCurrent { key: selected, deleted: false, .. } if selected == key)
            {
                return Ok(());
            }
            let conditions =
                crate::settlements::bucket_conditions(key, current.version, condition)?;
            // The observer of the covering write retires this duty. A failed comparison
            // or a lost reply leaves the authenticated marks available to the next read.
            self.app
                .put_record(&target, &value.to_json()?, conditions)
                .await?;
        }
        Ok(())
    }
}
