//! Paykit's own local state, in the protocol key space (`p/`) of the client's namespace
//! (`STATE.md` of `pubky-switch-client`).
//!
//! The client commits the protocol's local writes with a received message's processed pair
//! and cursor ([`pubky_switch_client::app::LocalWrite`]); everything else Paykit keeps is
//! committed here, one write batch per step, under keys that never meet the client's own.
//! Every value is `{"v":1,"d":<record>}`; a reader refuses another `v` as corrupt.
//!
//! | Key (after `p/`) | Holds | Sensitive |
//! |---|---|---|
//! | `fx/<seq>/<msg_id>` | a received message whose effect is still to be written | yes |
//! | `ask/<sender>/<from>` | a re-send to ask of a sender | |
//! | `missed/<from>` | entries missed while lagging, or before a provider loss | |
//! | `j/<attempt_id>` | the payment journal: one payment, from before its `attempt` | yes |
//! | `side/<msg_id>` | the side a queued cancellation is sent from | |
//! | `peer/<pubky>` | a counterparty with Paykit activity, and when | |
//! | `reply/<record>` | a rejection or cancellation recorded and still to be sent | |
//! | `req/<msg_id>` | a request sent whose record is still to be written | |
//! | `rs/<pubky>/<msg_id>` | when a message was last re-sent to a requester | |
//! | `rsd/<pubky>/<day>` | how many messages were re-sent to a requester that day | |
//! | `ident/<pubky>` | a counterparty's `identity.json`, and when it was read | |

use pubky_switch_client::app::{LocalWrite, PROTOCOL_PREFIX};
use pubky_switch_client::store::{StateStore, StoreHandle, WriteBatch};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Result};

/// The schema version of Paykit's local records.
pub(crate) const LOCAL_VERSION: u32 = 1;

/// The most entries one scan returns; scans page through larger sets.
const SCAN_PAGE: u32 = 256;

#[derive(Serialize)]
struct Out<'a, T: Serialize> {
    v: u32,
    d: &'a T,
}

#[derive(Deserialize)]
struct In<T> {
    v: u32,
    d: T,
}

/// A local record's bytes.
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(&Out {
        v: LOCAL_VERSION,
        d: value,
    })
    .map_err(|_| PaykitError::corrupt())
}

/// Read a local record's bytes.
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let read: In<T> = serde_json::from_slice(bytes).map_err(|_| PaykitError::corrupt())?;
    if read.v != LOCAL_VERSION {
        return Err(PaykitError::corrupt());
    }
    Ok(read.d)
}

/// The full store key of a protocol key.
fn full(key: &str) -> Vec<u8> {
    [PROTOCOL_PREFIX, key.as_bytes()].concat()
}

/// A local write the client commits for the protocol: put `value` at `key`.
pub(crate) fn local_put<T: Serialize>(key: &str, value: &T) -> Result<LocalWrite> {
    Ok(LocalWrite::Put {
        key: key.as_bytes().to_vec(),
        value: encode(value)?,
    })
}

/// Read a protocol key synchronously, from the protocol's callbacks.
pub(crate) fn get_sync<T: DeserializeOwned>(
    store: &dyn StateStore,
    ns: &str,
    key: &str,
) -> Option<T> {
    let bytes = store.get(ns, &full(key)).ok()??;
    decode(&bytes).ok()
}

/// Scan a protocol prefix synchronously: the key's rest after the prefix, with its record.
/// Entries that don't read are left out.
pub(crate) fn scan_sync<T: DeserializeOwned>(
    store: &dyn StateStore,
    ns: &str,
    prefix: &str,
) -> Vec<(String, T)> {
    let prefix_bytes = full(prefix);
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let Ok(page) = store.scan(ns, &prefix_bytes, after.as_deref(), SCAN_PAGE) else {
            return out;
        };
        let full_page = page.len() >= SCAN_PAGE as usize;
        for (key, value) in page {
            if let (Some(rest), Ok(record)) = (rest_of(&key, &prefix_bytes), decode(&value)) {
                out.push((rest, record));
            }
            after = Some(key);
        }
        if !full_page {
            return out;
        }
    }
}

fn rest_of(key: &[u8], prefix: &[u8]) -> Option<String> {
    let rest = key.get(prefix.len()..)?;
    std::str::from_utf8(rest).ok().map(str::to_owned)
}

/// Paykit's local state of one app, as the SDK's calls read and write it.
#[derive(Clone)]
pub(crate) struct Local {
    store: StoreHandle,
    ns: String,
}

impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Local({})", self.ns)
    }
}

/// A batch of local writes, committed at once.
#[derive(Default)]
pub(crate) struct Tx {
    writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl Tx {
    /// Put `value` at `key`.
    pub(crate) fn put<T: Serialize>(&mut self, key: &str, value: &T) -> Result<&mut Self> {
        self.writes.push((full(key), Some(encode(value)?)));
        Ok(self)
    }

    /// Delete `key`.
    pub(crate) fn delete(&mut self, key: &str) -> &mut Self {
        self.writes.push((full(key), None));
        self
    }
}

impl Local {
    /// The local state in `ns`, the client's namespace of the app.
    pub(crate) fn new(store: StoreHandle, ns: String) -> Self {
        Self { store, ns }
    }

    /// The host store, for the protocol's synchronous reads.
    pub(crate) fn store(&self) -> &StoreHandle {
        &self.store
    }

    /// The namespace.
    pub(crate) fn ns(&self) -> &str {
        &self.ns
    }

    /// The record at `key`.
    pub(crate) async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.store.get(&self.ns, &full(key)).await? {
            None => Ok(None),
            Some(bytes) => decode(&bytes).map(Some),
        }
    }

    /// Every record under `prefix`, with the key's rest after the prefix.
    pub(crate) async fn scan<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        let prefix_bytes = full(prefix);
        let mut out = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let page = self
                .store
                .scan(&self.ns, &prefix_bytes, after.as_deref(), SCAN_PAGE)
                .await?;
            let full_page = page.len() >= SCAN_PAGE as usize;
            for (key, value) in page {
                let rest = rest_of(&key, &prefix_bytes).ok_or_else(PaykitError::corrupt)?;
                out.push((rest, decode(&value)?));
                after = Some(key);
            }
            if !full_page {
                return Ok(out);
            }
        }
    }

    /// Commit `tx` in one write batch.
    pub(crate) async fn commit(&self, tx: Tx) -> Result<()> {
        if tx.writes.is_empty() {
            return Ok(());
        }
        let mut batch = WriteBatch::new();
        for (key, value) in tx.writes {
            let added = match value {
                Some(value) => batch.put(&self.ns, key, value).map(|_| ()),
                None => batch.delete(&self.ns, key).map(|_| ()),
            };
            added.map_err(|error| {
                PaykitError::from(pubky_switch_client::error::StoreError::Other(
                    error.to_string(),
                ))
            })?;
        }
        Ok(self.store.commit(batch).await?)
    }

    /// Put one record and commit it.
    pub(crate) async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let mut tx = Tx::default();
        tx.put(key, value)?;
        self.commit(tx).await
    }

    /// Delete one record and commit it.
    pub(crate) async fn delete(&self, key: &str) -> Result<()> {
        let mut tx = Tx::default();
        tx.delete(key);
        self.commit(tx).await
    }
}

/// A number in a key, zero-padded to 20 digits so keys sort numerically.
pub(crate) fn num(n: u64) -> String {
    format!("{n:020}")
}

/// Key builders.
pub(crate) mod key {
    use paykit_lib::AttemptId;
    use pubky_switch_core::enc::b64;

    use super::num;

    pub(crate) const EFFECTS: &str = "fx/";
    pub(crate) const ASKS: &str = "ask/";
    pub(crate) const MISSED: &str = "missed/";
    pub(crate) const JOURNAL: &str = "j/";
    pub(crate) const PEERS: &str = "peer/";
    pub(crate) const REPLIES: &str = "reply/";
    pub(crate) const REQUESTS: &str = "req/";

    pub(crate) fn effect(seq: u64, msg_id: &str) -> String {
        format!("{EFFECTS}{}/{msg_id}", num(seq))
    }

    pub(crate) fn ask(sender: &[u8; 32], from: u64) -> String {
        format!("{ASKS}{}/{}", b64(sender), num(from))
    }

    pub(crate) fn missed(from: u64) -> String {
        format!("{MISSED}{}", num(from))
    }

    pub(crate) fn journal(attempt_id: &AttemptId) -> String {
        format!("{JOURNAL}{}", attempt_id.to_text())
    }

    pub(crate) fn side(msg_id: &str) -> String {
        format!("side/{msg_id}")
    }

    pub(crate) fn peer(pubky: &[u8; 32]) -> String {
        format!("{PEERS}{}", b64(pubky))
    }

    pub(crate) fn reply(record: &[u8; 32]) -> String {
        format!("{REPLIES}{}", b64(record))
    }

    pub(crate) fn request(msg_id: &str) -> String {
        format!("{REQUESTS}{msg_id}")
    }

    pub(crate) fn resent(requester: &[u8; 32], msg_id: &str) -> String {
        format!("rs/{}/{msg_id}", b64(requester))
    }

    pub(crate) fn resent_day(requester: &[u8; 32], day: u64) -> String {
        format!("rsd/{}/{}", b64(requester), num(day))
    }

    pub(crate) fn identity(pubky: &[u8; 32]) -> String {
        format!("ident/{}", b64(pubky))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pubky_switch_client::store::MemoryStore;

    use super::*;

    #[test]
    fn test_state_records_refuse_another_version() {
        let bytes = encode(&7u64).unwrap();
        assert_eq!(decode::<u64>(&bytes).unwrap(), 7);
        assert!(decode::<u64>(br#"{"v":2,"d":7}"#).is_err());
        assert!(decode::<u64>(b"not json").is_err());
    }

    #[tokio::test]
    async fn test_state_writes_only_under_the_protocol_prefix() {
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new());
        let local = Local::new(StoreHandle::inline(Arc::clone(&store)), "client/x".into());
        local.put("peer/a", &1u64).await.unwrap();
        local.put("peer/b", &2u64).await.unwrap();
        assert!(store.get("client/x", b"p/peer/a").unwrap().is_some());
        let scanned: Vec<(String, u64)> = local.scan("peer/").await.unwrap();
        assert_eq!(scanned, vec![("a".into(), 1), ("b".into(), 2)]);
        assert_eq!(
            scan_sync::<u64>(store.as_ref(), "client/x", "peer/").len(),
            2
        );
        assert_eq!(
            get_sync::<u64>(store.as_ref(), "client/x", "peer/b"),
            Some(2)
        );
        local.delete("peer/a").await.unwrap();
        assert_eq!(local.get::<u64>("peer/a").await.unwrap(), None);
    }

    #[test]
    fn test_state_numbered_keys_sort() {
        assert!(key::effect(9, "m") < key::effect(10, "m"));
        assert!(key::missed(2) < key::missed(11));
    }
}
