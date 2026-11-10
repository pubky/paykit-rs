//! Paykit's own local state, in the protocol key space (`p/`) of the client's namespace
//! (`STATE.md` of `pubky-switch-client`).
//!
//! Paykit never writes the host store itself: every write is a client
//! [`LocalWrite`], committed by the client in one of three ways, so it lands with what it
//! belongs to:
//!
//! - with a received message's effect, its processed pair and its cursor move
//!   ([`pubky_switch_client::app::ReceiveContext::local`]);
//! - with the answer of a batch of operations, never if the batch is refused
//!   ([`pubky_switch_client::app::OpBatch::local`]);
//! - on its own, in one store commit ([`pubky_switch_client::app::App::protocol_commit`]).
//!
//! Reads go through the client too: [`pubky_switch_client::app::App::protocol_get`],
//! [`pubky_switch_client::app::App::protocol_scan`], the receive context's and, in the
//! protocol's other callbacks, a [`pubky_switch_client::app::LocalView`].
//! Every value is `{"v":1,"d":<record>}`; a reader refuses another `v` as corrupt.
//!
//! | Key (after `p/`) | Holds | Sensitive |
//! |---|---|---|
//! | `ask/<sender>/<from>` | a re-send to ask of a sender | |
//! | `ans/<seq>/<msg_id>` | a `resend_request` received and still to be answered | |
//! | `missed/<from>` | entries missed while lagging, or before a provider loss | |
//! | `j/<attempt_id>` | the payment journal: one payment, from before its `attempt` | yes |
//! | `side/<msg_id>` | the side a cancellation is sent from, kept with the batch that queues it | |
//! | `kc/<record key>` | a payee's cancellation received before its request, for the payer's request record | |
//! | `peer/<pubky>` | a counterparty with Paykit activity, and when | |
//! | `rs/<pubky>/<msg_id>` | when a message was last re-sent to a requester | |
//! | `rsd/<pubky>/<day>` | how many messages were re-sent to a requester that day | |

use pubky_switch_client::app::LocalWrite;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Result};
use crate::paykit::Paykit;

/// The schema version of Paykit's local records.
pub(crate) const LOCAL_VERSION: u32 = 1;

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

/// A local write the client commits for the protocol: put `value` at `key`.
pub(crate) fn local_put<T: Serialize>(key: &str, value: &T) -> Result<LocalWrite> {
    Ok(LocalWrite::Put {
        key: key.as_bytes().to_vec(),
        value: encode(value)?,
    })
}

fn rest_of(key: &[u8], prefix: &[u8]) -> Option<String> {
    let rest = key.get(prefix.len()..)?;
    std::str::from_utf8(rest).ok().map(str::to_owned)
}

/// Local writes to commit together.
#[derive(Default)]
pub(crate) struct Tx {
    writes: Vec<LocalWrite>,
}

impl Tx {
    /// Put `value` at `key`.
    pub(crate) fn put<T: Serialize>(&mut self, key: &str, value: &T) -> Result<&mut Self> {
        self.writes.push(local_put(key, value)?);
        Ok(self)
    }

    /// Delete `key`.
    pub(crate) fn delete(&mut self, key: &str) -> &mut Self {
        self.writes.push(LocalWrite::Delete {
            key: key.as_bytes().to_vec(),
        });
        self
    }

    /// The writes, for a batch or a receive context.
    pub(crate) fn into_writes(self) -> Vec<LocalWrite> {
        self.writes
    }
}

impl Paykit {
    /// The record at protocol key `key`.
    pub(crate) async fn local_get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.app.protocol_get(key.as_bytes()).await? {
            None => Ok(None),
            Some(bytes) => decode(&bytes).map(Some),
        }
    }

    /// Every record under protocol prefix `prefix`, with the key's rest after the prefix.
    pub(crate) async fn local_scan<T: DeserializeOwned>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>> {
        let mut out = Vec::new();
        for (key, value) in self.app.protocol_scan(prefix.as_bytes()).await? {
            let rest = rest_of(&key, prefix.as_bytes()).ok_or_else(PaykitError::corrupt)?;
            out.push((rest, decode(&value)?));
        }
        Ok(out)
    }

    /// Commit `tx` on its own, in one store commit.
    pub(crate) async fn local_commit(&mut self, tx: Tx) -> Result<()> {
        if tx.writes.is_empty() {
            return Ok(());
        }
        Ok(self.app.protocol_commit(&tx.writes).await?)
    }

    /// Put one record and commit it.
    pub(crate) async fn local_put(&mut self, key: &str, value: &impl Serialize) -> Result<()> {
        let mut tx = Tx::default();
        tx.put(key, value)?;
        self.local_commit(tx).await
    }

    /// Delete one record and commit it.
    pub(crate) async fn local_delete(&mut self, key: &str) -> Result<()> {
        let mut tx = Tx::default();
        tx.delete(key);
        self.local_commit(tx).await
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

    pub(crate) const ASKS: &str = "ask/";
    pub(crate) const ANSWERS: &str = "ans/";
    pub(crate) const MISSED: &str = "missed/";
    pub(crate) const JOURNAL: &str = "j/";
    pub(crate) const PEERS: &str = "peer/";
    pub(crate) const KEPT_CANCELLATIONS: &str = "kc/";

    pub(crate) fn ask(sender: &[u8; 32], from: u64) -> String {
        format!("{ASKS}{}/{}", b64(sender), num(from))
    }

    pub(crate) fn answer(seq: u64, msg_id: &str) -> String {
        format!("{ANSWERS}{}/{msg_id}", num(seq))
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

    pub(crate) fn kept_cancellation(record_key: &[u8; 32]) -> String {
        format!("{KEPT_CANCELLATIONS}{}", b64(record_key))
    }

    pub(crate) fn peer(pubky: &[u8; 32]) -> String {
        format!("{PEERS}{}", b64(pubky))
    }

    pub(crate) fn resent(requester: &[u8; 32], msg_id: &str) -> String {
        format!("rs/{}/{msg_id}", b64(requester))
    }

    pub(crate) fn resent_day(requester: &[u8; 32], day: u64) -> String {
        format!("rsd/{}/{}", b64(requester), num(day))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pubky_switch_client::app::PROTOCOL_PREFIX;
    use pubky_switch_client::store::{MemoryStore, StateStore};

    use super::*;

    #[test]
    fn test_state_records_refuse_another_version() {
        let bytes = encode(&7u64).unwrap();
        assert_eq!(decode::<u64>(&bytes).unwrap(), 7);
        assert!(decode::<u64>(br#"{"v":2,"d":7}"#).is_err());
        assert!(decode::<u64>(b"not json").is_err());
    }

    #[test]
    fn test_state_writes_are_local_writes_read_under_the_protocol_prefix() {
        let mut tx = Tx::default();
        tx.put("peer/a", &1u64).unwrap();
        tx.put("peer/b", &2u64).unwrap();
        tx.delete("peer/c");
        let writes = tx.into_writes();
        assert_eq!(writes.len(), 3);
        // The client prefixes the protocol's keys with `p/`.
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new());
        let mut batch = pubky_switch_client::store::WriteBatch::new();
        for write in &writes {
            if let LocalWrite::Put { key, value } = write {
                batch
                    .put("client/x", [PROTOCOL_PREFIX, key].concat(), value.clone())
                    .unwrap();
            }
        }
        store.commit(batch).unwrap();
        let held = store.get("client/x", b"p/peer/b").unwrap().unwrap();
        assert_eq!(decode::<u64>(&held).unwrap(), 2);
        assert!(matches!(&writes[2], LocalWrite::Delete { key } if key == b"peer/c"));
    }

    #[test]
    fn test_state_numbered_keys_sort() {
        assert!(key::answer(9, "m") < key::answer(10, "m"));
        assert!(key::missed(2) < key::missed(11));
    }
}
