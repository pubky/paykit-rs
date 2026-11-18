//! Settlement marks retained by this payee after taking a shared bucket (Paykit wire 5.7).

use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, SETTLEMENT_BUCKETS};
use paykit_lib::records::{SeenMarks, SettlementBucket, SettlementMark, SETTLEMENT_MARK_LEN};
use pubky_switch_client::app::{
    LocalWrite, ReceiveContext, RecordObservation, RecordTarget, StoreFailure,
};
use pubky_switch_core::enc::{b64, serde_b64};
use serde::{Deserialize, Serialize};

use crate::activity::family;
use crate::error::Result;
use crate::paykit::Paykit;
use crate::protocol::{store_failure, view_failure};
use crate::state::{self, key, Binder};

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
        let prefix = prefix(&record.key);
        let blind = |input: &[u8]| cx.blind(input);
        let binder = Binder::new(cx.identity(), cx.cert(), &blind);
        let mut writes = Vec::new();
        for mark in bucket.marks() {
            let bytes = mark.to_bytes();
            let name = format!("{prefix}{}", b64(&bytes));
            if let Some(held) = cx.local_get(name.as_bytes()).map_err(store_failure)? {
                state::restored::<ObservedMark>(name.as_bytes(), &held, &binder)
                    .map_err(view_failure)?;
            } else {
                let observed = ObservedMark {
                    bucket: number,
                    mark: bytes,
                };
                writes.push(state::local_put(&name, &observed).map_err(view_failure)?);
            }
        }
        return Ok(writes);
    }
    Ok(Vec::new())
}

impl Paykit {
    pub(crate) async fn seen_settlement_marks(
        &self,
        bucket: u16,
        key: &[u8; 32],
    ) -> Result<SeenMarks> {
        let saved = self.local_scan::<ObservedMark>(&prefix(key)).await?;
        let marks = saved
            .into_iter()
            .map(|(_, saved)| SettlementMark::from_bytes(&saved.mark))
            .collect::<paykit_lib::Result<_>>()?;
        Ok(SeenMarks::from_retained(bucket, marks)?)
    }
}
