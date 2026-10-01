use super::super::*;

pub(in crate::backup) fn keyed_by_counterparty<T>(
    records: Vec<T>,
    label: &str,
) -> Result<HashMap<PubkyPublicKey, T>>
where
    T: HasCounterparty,
{
    keyed_by_tuple(records, |record| record.counterparty().clone(), label)
}

pub(in crate::backup) trait HasCounterparty {
    fn counterparty(&self) -> &PubkyPublicKey;
}

impl HasCounterparty for LinkedPeerRecord {
    fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }
}

impl HasCounterparty for EncryptedLinkStateRecord {
    fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }
}

pub(in crate::backup) fn keyed_by_tuple<K, T, F>(
    records: Vec<T>,
    key: F,
    label: &str,
) -> Result<HashMap<K, T>>
where
    K: Eq + std::hash::Hash + fmt::Debug,
    F: Fn(&T) -> K,
{
    let mut keyed = HashMap::new();
    for record in records {
        let key = key(&record);
        if keyed.insert(key, record).is_some() {
            return Err(PaykitSdkError::Protocol {
                context: format!("duplicate {label} backup key"),
                source: None,
            });
        }
    }
    Ok(keyed)
}

pub(in crate::backup) fn unique_outbound_messages(
    mut records: Vec<OutboundPrivateMessageRecord>,
) -> Result<Vec<OutboundPrivateMessageRecord>> {
    let mut ids = HashSet::new();
    for record in &records {
        if !ids.insert(record.outbound_message_id) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "duplicate outbound Private Application Message id {}",
                    record.outbound_message_id
                ),
                source: None,
            });
        }
    }
    records.sort_by_key(|record| record.outbound_message_id);
    Ok(records)
}

pub(in crate::backup) fn unique_private_stream_items(
    mut records: Vec<PrivateStreamItemRecord>,
) -> Result<Vec<PrivateStreamItemRecord>> {
    let mut ids = HashSet::new();
    for record in &records {
        if !ids.insert(record.stream_item_id) {
            return Err(PaykitSdkError::Protocol {
                context: format!("duplicate private stream item id {}", record.stream_item_id),
                source: None,
            });
        }
    }
    records.sort_by_key(|record| record.stream_item_id);
    Ok(records)
}

pub(in crate::backup) fn next_outbound_id(records: &[OutboundPrivateMessageRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.outbound_message_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}

pub(in crate::backup) fn next_receive_batch_id(records: &[PrivateStreamItemRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.receive_batch_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}

pub(in crate::backup) fn next_private_stream_item_id(records: &[PrivateStreamItemRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.stream_item_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}
