use std::{collections::HashMap, fmt};

use super::StorageState;
use crate::PubkyPublicKey;

// Count index rebuilds, so tests can prove a call reused the index.
#[cfg(test)]
thread_local! {
    pub(crate) static LINK_RECORD_INDEX_REBUILDS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Per-link record counts derived from a [`StorageState`].
///
/// The index is a cache kept beside the state and never stored, so the
/// encoded state blob is unchanged. It answers how many private stream items
/// and outbound private messages a counterparty holds without visiting any
/// record. It carries no public API and compares equal to every other index,
/// so two states with the same records stay equal whatever it holds.
#[derive(Clone, Default)]
pub struct LinkRecordIndex {
    counts: Option<LinkRecordCounts>,
}

#[derive(Clone)]
struct LinkRecordCounts {
    per_link: HashMap<PubkyPublicKey, usize>,
    stamp: RecordStamp,
}

/// Length and last id of both record lists.
///
/// Ids only grow and records are only appended or removed, so any append or
/// removal made outside the index's own updates changes the stamp, and the
/// index is rebuilt instead of read.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RecordStamp {
    stream_items: (usize, Option<u64>),
    outbound_messages: (usize, Option<u64>),
}

impl RecordStamp {
    fn of(state: &StorageState) -> Self {
        Self {
            stream_items: (
                state.private_stream_items.len(),
                state
                    .private_stream_items
                    .last()
                    .map(|item| item.stream_item_id),
            ),
            outbound_messages: (
                state.outbound_private_messages.len(),
                state
                    .outbound_private_messages
                    .last()
                    .map(|message| message.outbound_message_id),
            ),
        }
    }
}

impl LinkRecordIndex {
    fn build(state: &StorageState) -> LinkRecordCounts {
        #[cfg(test)]
        LINK_RECORD_INDEX_REBUILDS.with(|rebuilds| rebuilds.set(rebuilds.get() + 1));
        let mut per_link = HashMap::new();
        let counterparties = state
            .private_stream_items
            .iter()
            .map(|item| &item.counterparty)
            .chain(
                state
                    .outbound_private_messages
                    .iter()
                    .map(|message| &message.counterparty),
            );
        for counterparty in counterparties {
            *per_link.entry(counterparty.clone()).or_insert(0) += 1;
        }
        LinkRecordCounts {
            per_link,
            stamp: RecordStamp::of(state),
        }
    }
}

impl StorageState {
    /// Rebuild the link record index unless it still matches the records.
    pub(crate) fn refresh_link_record_index(&mut self) {
        let stamp = RecordStamp::of(self);
        if self
            .link_record_index
            .counts
            .as_ref()
            .is_some_and(|counts| counts.stamp == stamp)
        {
            return;
        }
        self.link_record_index.counts = Some(LinkRecordIndex::build(self));
    }

    /// Install per-link counts that a caller took from the records in a pass
    /// it already made, such as validation on decode.
    pub(crate) fn install_link_record_counts(&mut self, per_link: HashMap<PubkyPublicKey, usize>) {
        self.link_record_index.counts = Some(LinkRecordCounts {
            per_link,
            stamp: RecordStamp::of(self),
        });
    }

    /// Records one counterparty holds, when the index matches the records.
    pub(crate) fn indexed_link_records(&self, counterparty: &PubkyPublicKey) -> Option<usize> {
        let counts = self.link_record_index.counts.as_ref()?;
        (counts.stamp == RecordStamp::of(self))
            .then(|| counts.per_link.get(counterparty).copied().unwrap_or(0))
    }

    /// Update the index for a record just appended for `counterparty`.
    ///
    /// `before` is the state's stamp before the append. An index that did not
    /// match it was already stale, so it is dropped and rebuilt on next use.
    fn index_appended_record(&mut self, before: RecordStamp, counterparty: &PubkyPublicKey) {
        let after = RecordStamp::of(self);
        match self.link_record_index.counts.as_mut() {
            Some(counts) if counts.stamp == before => {
                *counts.per_link.entry(counterparty.clone()).or_insert(0) += 1;
                counts.stamp = after;
            }
            _ => self.link_record_index.counts = None,
        }
    }

    /// Append a private stream item and keep the link record index current.
    pub(crate) fn push_private_stream_item(&mut self, item: super::PrivateStreamItemRecord) {
        let before = RecordStamp::of(self);
        let counterparty = item.counterparty.clone();
        self.private_stream_items.push(item);
        self.index_appended_record(before, &counterparty);
    }

    /// Append an outbound private message and keep the link record index
    /// current.
    pub(crate) fn push_outbound_private_message(
        &mut self,
        message: super::OutboundPrivateMessageRecord,
    ) {
        let before = RecordStamp::of(self);
        let counterparty = message.counterparty.clone();
        self.outbound_private_messages.push(message);
        self.index_appended_record(before, &counterparty);
    }

    /// Exact per-link counts from the index, for tests that check it against
    /// the records.
    #[cfg(test)]
    pub(crate) fn link_record_index_counts(&self) -> Option<HashMap<PubkyPublicKey, usize>> {
        let counts = self.link_record_index.counts.as_ref()?;
        (counts.stamp == RecordStamp::of(self)).then(|| counts.per_link.clone())
    }
}

impl PartialEq for LinkRecordIndex {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for LinkRecordIndex {}

impl fmt::Debug for LinkRecordIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkRecordIndex")
    }
}
