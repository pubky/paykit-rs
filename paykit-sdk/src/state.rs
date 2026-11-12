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
//! The whole of it is Paykit's layer in the app's backups ([`crate::PaykitBackup`]), which a
//! restore installs, the journal checked and marked restored, in the commit of the
//! client's own state (Switch spec 12.4).
//!
//! | Key (after `p/`) | Holds | Sensitive |
//! |---|---|---|
//! | `ask/<sender>/<from>` | a re-send to ask of a sender | |
//! | `ans/<seq>/<msg_id>` | a `resend_request` received and still to be answered | |
//! | `missed/<from>` | entries missed while lagging, or before a provider loss | |
//! | `j/<attempt_id>` | the payment journal: one payment, from before its `attempt` | yes |
//! | `side/<msg_id>` | the side a cancellation is sent from, kept with the batch that queues it | |
//! | `ar/<payee>/<app_id>` | a payee app's records that took part in its comparison, for its public endpoints | |
//! | `ct` | the acceptance time of the last Paykit entry the receive step passed | |
//! | `rh` | the hold a restore set on the open requests (`hold.rs`); never backed up | |
//! | `hc/<record key>` | the user confirmed a held request; never backed up | |
//! | `lh/<record key>` | a Private Payment List held because its `payment_lists` record doesn't read: never written over it, which could drop its single-use marks, until the record reads | |
//! | `kc/<record key>` | a payee's cancellation of a one-time request, kept until it is placed in the payer's request record | |
//! | `peer/<pubky>` | a counterparty with Paykit activity, and when | |
//! | `lr` | when the app last read the lists, on the device clock; absent before the first read and after a due read failed, when new activity waits (Paykit wire 4.1) | |
//! | `spam/<pubky>` | how many messages from a blocked or flagged sender only were counted (Paykit wire 4.1) | |
//! | `rs/<pubky>/<msg_id>` | when a message was last re-sent to a requester | |
//! | `rsd/<pubky>/<day>` | how many messages were re-sent to a requester that day | |
//! | `nl` | set by a restore: the shared records aren't listed since, so the client's copy is empty and a record's absence means nothing yet; cleared once they are listed; never restored | |
//! | `lc` | the certificate the shared records were last listed under, signed: a certificate held now other than it means the copy may lack records until Paykit lists them, since one held between may have read less (`hold.rs`); written only by a Paykit listing, in its commit; absent from a join or a restore until that listing, which leaves the records unlisted; never restored | |
//! | `pp` | set by a restore: a payer pass is owed, so no new attempt is made and nothing is dispatched; cleared only after a reconcile under `pay` ran that pass (`hold.rs`); never restored | |
//! | `lq/<payee>[/<app_id>]` | a `private_payment_list_request` asked of a payee's app, or of none: the version it named and when (Paykit spec 8.4) | |
//! | `nt/<attempt_id>/<kind>`, `nt/<sender>/<msg_id>` | a notice for the host about a payment, or a message given up, until acknowledged (`notices.rs`) | |
//! | `dl` | the deny list as the app last read it, for the views (Paykit spec 8.1) | |
//! | `pd/<sender>` | a mark of a sender's replies dropped for the bounds on kept replies: the earliest and the latest acceptance among them, and when it was last asked to send them again (`marks.rs`) | |
//! | `ud/<sender>` | a mark of a sender's entries dropped unverified, as `pd/` holds (`marks.rs`) | |
//! | `pr/<sender>/<record key>/<msg_id>` | a payer's reply to a request the app holds no record of yet, kept until it does (`receive.rs`) | |
//! | `dt/<event ID>` | a duty event the SDK has taken (`stored.rs`), until it is acknowledged; never restored: a restore's events are taken again, which changes nothing, since a dropped entry's time is already in its sender's mark | |
//! | `se/<sender>/<msg_id>` | the message of an entry the client stored for good, to write into its record later (`stored.rs`); never restored, since the client tells of the entries it restores again | |
//! | `ae/<record key>` | a counterparty's `allowance_end`, kept as the stop until the Allowance record holds an end or a rejection (`allowances.rs`, Paykit spec 8.3) | |
//! | `ac/<record key>` | the consent list: an Allowance this app may pay under, or removed from it for good (`allowances.rs`, Paykit spec 8.3); a restore marks every entry to confirm again | |
//! | `mc` | set by a `missed` item: every entry of the consent list is to be confirmed again, as after a restore; nothing is consented while it is set, and the next receive marks each entry so and clears it (`allowances.rs`); never restored | |
//! | `en/<msg_id>` | the executor app an `executor_notice` is wrapped to, kept with the batch that queues it | |

use paykit_lib::AttemptId;
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
    pub(crate) const CURSOR_TIME: &str = "ct";
    pub(crate) const RESTORE_HOLD: &str = "rh";
    pub(crate) const MISSED_HOLD: &str = "mh";
    pub(crate) const HOLD_CONFIRMED: &str = "hc/";
    pub(crate) const LISTS_READ: &str = "lr";
    pub(crate) const HELD_LISTS: &str = "lh/";
    pub(crate) const RECORDS_UNLISTED: &str = "nl";
    pub(crate) const MISSED_UNLISTED: &str = "ml";
    pub(crate) const MISSED_CONSENTS: &str = "mc";
    pub(crate) const PAYER_PASS_OWED: &str = "pp";
    pub(crate) const LISTED_CERT: &str = "lc";
    pub(crate) const NOTICES: &str = "nt/";
    pub(crate) const STORED_NOTES: &str = "se/";
    pub(crate) const KEPT_REPLIES: &str = "pr/";
    pub(crate) const DROPPED_REPLIES: &str = "pd/";
    pub(crate) const DROPPED_UNVERIFIED: &str = "ud/";
    pub(crate) const ALLOWANCE_ENDS: &str = "ae/";
    pub(crate) const CONSENTS: &str = "ac/";

    /// A counterparty's end of the Allowance whose record is `record_key`, kept as the stop.
    pub(crate) fn allowance_end(record_key: &[u8; 32]) -> String {
        format!("{ALLOWANCE_ENDS}{}", b64(record_key))
    }

    /// The consent list's entry of the Allowance whose record is `record_key`.
    pub(crate) fn consent(record_key: &[u8; 32]) -> String {
        format!("{CONSENTS}{}", b64(record_key))
    }

    /// The executor app the `executor_notice` `msg_id` is wrapped to.
    pub(crate) fn notice_target(msg_id: &str) -> String {
        format!("en/{msg_id}")
    }

    /// The mark of replies from `sender` dropped for the bounds.
    pub(crate) fn dropped_replies(sender: &[u8; 32]) -> String {
        format!("{DROPPED_REPLIES}{}", b64(sender))
    }

    /// The mark of entries from `sender` dropped unverified.
    pub(crate) fn dropped_unverified(sender: &[u8; 32]) -> String {
        format!("{DROPPED_UNVERIFIED}{}", b64(sender))
    }
    pub(crate) const DENIED: &str = "dl";
    pub(crate) const TAKEN: &str = "dt/";

    /// The mark of the duty event `id`, taken.
    pub(crate) fn taken(id: &pubky_switch_client::app::EventId) -> String {
        format!("{TAKEN}{id}")
    }

    /// A payer's reply `msg_id` from `sender`, kept for the payee's record `record_key`.
    pub(crate) fn kept_reply(sender: &[u8; 32], record_key: &[u8; 32], msg_id: &str) -> String {
        format!("{}{}/{msg_id}", kept_replies_of(sender), b64(record_key))
    }

    /// The prefix of the replies kept from `sender`.
    pub(crate) fn kept_replies_of(sender: &[u8; 32]) -> String {
        format!("{KEPT_REPLIES}{}/", b64(sender))
    }

    /// The note of a stored entry's message `msg_id` from `sender`.
    pub(crate) fn stored_note(sender: &[u8; 32], msg_id: &str) -> String {
        format!("{STORED_NOTES}{}/{msg_id}", b64(sender))
    }

    /// The notice that `sender`'s message `msg_id` was given up.
    pub(crate) fn message_notice(sender: &[u8; 32], msg_id: &str) -> String {
        format!("{NOTICES}{}/{msg_id}", b64(sender))
    }

    /// The notice of `kind` about `attempt`.
    pub(crate) fn notice(attempt: &AttemptId, kind: crate::notices::NoticeKind) -> String {
        format!("{NOTICES}{}", crate::notices::notice_id(attempt, kind))
    }

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

    pub(crate) fn seen_records(payee: &[u8; 32], app_id: &[u8; 16]) -> String {
        format!("ar/{}/{}", b64(payee), b64(app_id))
    }

    pub(crate) fn hold_confirmed(record_key: &[u8; 32]) -> String {
        format!("{HOLD_CONFIRMED}{}", b64(record_key))
    }

    pub(crate) fn held_list(record_key: &[u8; 32]) -> String {
        format!("{HELD_LISTS}{}", b64(record_key))
    }

    pub(crate) fn kept_cancellation(record_key: &[u8; 32]) -> String {
        format!("{KEPT_CANCELLATIONS}{}", b64(record_key))
    }

    pub(crate) fn peer(pubky: &[u8; 32]) -> String {
        format!("{PEERS}{}", b64(pubky))
    }

    pub(crate) fn spam(sender: &[u8; 32]) -> String {
        format!("spam/{}", b64(sender))
    }

    pub(crate) fn resent(requester: &[u8; 32], msg_id: &str) -> String {
        format!("rs/{}/{msg_id}", b64(requester))
    }

    pub(crate) fn resent_day(requester: &[u8; 32], day: u64) -> String {
        format!("rsd/{}/{}", b64(requester), num(day))
    }

    /// A list request asked of `payee`, of its app `app_id` or of none.
    pub(crate) fn list_ask(payee: &[u8; 32], app_id: Option<&[u8; 16]>) -> String {
        match app_id {
            Some(app) => format!("lq/{}/{}", b64(payee), b64(app)),
            None => format!("lq/{}", b64(payee)),
        }
    }
}

/// Paykit's local state, closed: every key the SDK writes, as its builder in [`key`] spells
/// it, parsed back whole. A restore takes only keys that parse here, each value checked
/// against what its key names (`protocol.rs`, `restore_paykit_layer`); a test checks that
/// every builder's key parses to its own family, so the two can't drift.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StateKey {
    /// `j/<attempt ID>`: a journal entry.
    Journal(AttemptId),
    /// `ask/<sender>/<from>`: a `resend_request` to send.
    Ask { sender: [u8; 32], from_ms: u64 },
    /// `ans/<seq>/<msg_id>`: a `resend_request` received, to answer.
    Answer { seq: u64, msg_id: String },
    /// `missed/<from>`: a `missed` item's re-sends to ask for.
    Missed { from_ms: u64 },
    /// `peer/<pubky>`: a counterparty with Paykit activity.
    Peer([u8; 32]),
    /// `kc/<record key>`: a payee's cancellation kept to place.
    KeptCancellation([u8; 32]),
    /// `ct`: the acceptance time of the last entry the cursor passed.
    CursorTime,
    /// `rh`: the hold a restore set.
    RestoreHold,
    /// `mh`: the hold a `missed` item set.
    MissedHold,
    /// `nl`: the shared records aren't listed since a restore.
    RecordsUnlisted,
    /// `ml`: the shared records aren't listed by Paykit since a `missed` item.
    MissedUnlisted,
    /// `mc`: the consent list is to be confirmed again since a `missed` item.
    MissedConsents,
    /// `pp`: no reconcile ran the payer pass since a restore.
    PayerPassOwed,
    /// `lc`: the certificate the shared records were last listed under.
    ListedCert,
    /// `hc/<record key>`: the user's confirmation of a held request.
    HoldConfirmed([u8; 32]),
    /// `lr`: when the lists were last read.
    ListsRead,
    /// `lh/<record key>`: a list held over a record that didn't read.
    HeldList([u8; 32]),
    /// `side/<msg_id>`: the side of a message the app sent.
    Side(String),
    /// `ar/<payee>/<app>`: a payee app's records a reader has seen.
    SeenRecords { payee: [u8; 32], app_id: [u8; 16] },
    /// `spam/<sender>`: the messages the spam rule only counted.
    Spam([u8; 32]),
    /// `rs/<requester>/<msg_id>`: when a message was re-sent to a requester.
    Resent { requester: [u8; 32], msg_id: String },
    /// `rsd/<requester>/<day>`: how many messages were re-sent to a requester that day.
    ResentDay { requester: [u8; 32], day: u64 },
    /// `lq/<payee>[/<app>]`: a list request asked of a payee, of one of its apps or none.
    ListAsk {
        payee: [u8; 32],
        app_id: Option<[u8; 16]>,
    },
    /// `dl`: the deny list as last read.
    Denied,
    /// `dt/<event ID>`: a duty event taken.
    Taken(String),
    /// `pd/<sender>`: the mark of a sender's replies dropped for the bounds.
    DroppedReplies { sender: [u8; 32] },
    /// `ud/<sender>`: the mark of a sender's entries dropped unverified.
    DroppedUnverified { sender: [u8; 32] },
    /// `pr/<record key>/<msg_id>`: a payer's reply kept for its record.
    KeptReply {
        sender: [u8; 32],
        record: [u8; 32],
        msg_id: String,
    },
    /// `se/<sender>/<msg_id>`: a stored entry's message to write later.
    StoredNote { sender: [u8; 32], msg_id: String },
    /// `nt/<sender>/<msg_id>`: a notice for the host about a message given up.
    MessageNotice { sender: [u8; 32], msg_id: String },
    /// `nt/<attempt_id>/<kind>`: a notice for the host.
    Notice {
        attempt: AttemptId,
        kind: crate::notices::NoticeKind,
    },
    /// `ae/<record key>`: a counterparty's Allowance end kept as the stop.
    AllowanceEnd([u8; 32]),
    /// `ac/<record key>`: an entry of the consent list.
    Consent([u8; 32]),
    /// `en/<msg_id>`: the executor app an `executor_notice` is wrapped to.
    NoticeTarget(String),
}

impl StateKey {
    /// The family of `key`, read whole and strictly: a base64 member in its one spelling,
    /// a number as [`num`] writes it, a `msg_id` a lower-case UUID v4. `None` for any key
    /// the SDK doesn't write.
    pub(crate) fn parse(key: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(key).ok()?;
        let b64_32 = |s: &str| {
            pubky_switch_core::enc::unb64_array::<32>(s)
                .ok()
                .filter(|bytes| pubky_switch_core::enc::b64(bytes) == s)
        };
        let b64_16 = |s: &str| {
            pubky_switch_core::enc::unb64_array::<16>(s)
                .ok()
                .filter(|bytes| pubky_switch_core::enc::b64(bytes) == s)
        };
        let number = |s: &str| {
            (s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<u64>().ok())
                .flatten()
        };
        let msg_id = |s: &str| {
            pubky_switch_core::json::check_uuid_v4(s)
                .is_ok()
                .then(|| s.to_owned())
        };
        Some(match text {
            "ct" => Self::CursorTime,
            "rh" => Self::RestoreHold,
            "mh" => Self::MissedHold,
            "nl" => Self::RecordsUnlisted,
            "ml" => Self::MissedUnlisted,
            "mc" => Self::MissedConsents,
            "pp" => Self::PayerPassOwed,
            "lc" => Self::ListedCert,
            "lr" => Self::ListsRead,
            "dl" => Self::Denied,
            _ => {
                let (family, rest) = text.split_once('/')?;
                match family {
                    "j" => {
                        let attempt = AttemptId::parse(rest).ok()?;
                        (attempt.to_text() == rest).then_some(Self::Journal(attempt))?
                    }
                    "ask" => {
                        let (sender, from) = rest.split_once('/')?;
                        Self::Ask {
                            sender: b64_32(sender)?,
                            from_ms: number(from)?,
                        }
                    }
                    "ans" => {
                        let (seq, id) = rest.split_once('/')?;
                        Self::Answer {
                            seq: number(seq)?,
                            msg_id: msg_id(id)?,
                        }
                    }
                    "missed" => Self::Missed {
                        from_ms: number(rest)?,
                    },
                    "peer" => Self::Peer(b64_32(rest)?),
                    "kc" => Self::KeptCancellation(b64_32(rest)?),
                    "hc" => Self::HoldConfirmed(b64_32(rest)?),
                    "lh" => Self::HeldList(b64_32(rest)?),
                    "side" => Self::Side(msg_id(rest)?),
                    "ar" => {
                        let (payee, app) = rest.split_once('/')?;
                        Self::SeenRecords {
                            payee: b64_32(payee)?,
                            app_id: b64_16(app)?,
                        }
                    }
                    "spam" => Self::Spam(b64_32(rest)?),
                    "ae" => Self::AllowanceEnd(b64_32(rest)?),
                    "ac" => Self::Consent(b64_32(rest)?),
                    "en" => Self::NoticeTarget(msg_id(rest)?),
                    "rs" => {
                        let (requester, id) = rest.split_once('/')?;
                        Self::Resent {
                            requester: b64_32(requester)?,
                            msg_id: msg_id(id)?,
                        }
                    }
                    "rsd" => {
                        let (requester, day) = rest.split_once('/')?;
                        Self::ResentDay {
                            requester: b64_32(requester)?,
                            day: number(day)?,
                        }
                    }
                    "lq" => match rest.split_once('/') {
                        Some((payee, app)) => Self::ListAsk {
                            payee: b64_32(payee)?,
                            app_id: Some(b64_16(app)?),
                        },
                        None => Self::ListAsk {
                            payee: b64_32(rest)?,
                            app_id: None,
                        },
                    },
                    "dt" => {
                        let id = rest.parse::<pubky_switch_client::app::EventId>().ok()?;
                        (id.to_string() == rest).then(|| Self::Taken(rest.to_owned()))?
                    }
                    "pd" => Self::DroppedReplies {
                        sender: b64_32(rest)?,
                    },
                    "ud" => Self::DroppedUnverified {
                        sender: b64_32(rest)?,
                    },
                    "pr" => {
                        let (sender, rest) = rest.split_once('/')?;
                        let (record, id) = rest.split_once('/')?;
                        Self::KeptReply {
                            sender: b64_32(sender)?,
                            record: b64_32(record)?,
                            msg_id: msg_id(id)?,
                        }
                    }
                    "se" => {
                        let (sender, id) = rest.split_once('/')?;
                        Self::StoredNote {
                            sender: b64_32(sender)?,
                            msg_id: msg_id(id)?,
                        }
                    }
                    "nt" => {
                        let (attempt, tag) = rest.split_once('/')?;
                        if let Some(sender) = b64_32(attempt) {
                            return Some(Self::MessageNotice {
                                sender,
                                msg_id: msg_id(tag)?,
                            });
                        }
                        let parsed = AttemptId::parse(attempt).ok()?;
                        (parsed.to_text() == attempt).then_some(())?;
                        Self::Notice {
                            attempt: parsed,
                            kind: crate::notices::NoticeKind::from_tag(tag)?,
                        }
                    }
                    _ => return None,
                }
            }
        })
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

    #[test]
    fn test_state_key_every_builder_parses_to_its_family() {
        let pubky = [3u8; 32];
        let app = [4u8; 16];
        let attempt = AttemptId::from_bytes([5; 16]);
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let cases: Vec<(String, StateKey)> = vec![
            (key::journal(&attempt), StateKey::Journal(attempt)),
            (
                key::ask(&pubky, 7),
                StateKey::Ask {
                    sender: pubky,
                    from_ms: 7,
                },
            ),
            (
                key::answer(8, id),
                StateKey::Answer {
                    seq: 8,
                    msg_id: id.into(),
                },
            ),
            (key::missed(9), StateKey::Missed { from_ms: 9 }),
            (key::peer(&pubky), StateKey::Peer(pubky)),
            (
                key::kept_cancellation(&pubky),
                StateKey::KeptCancellation(pubky),
            ),
            (key::CURSOR_TIME.into(), StateKey::CursorTime),
            (key::RESTORE_HOLD.into(), StateKey::RestoreHold),
            (key::MISSED_HOLD.into(), StateKey::MissedHold),
            (key::RECORDS_UNLISTED.into(), StateKey::RecordsUnlisted),
            (key::MISSED_UNLISTED.into(), StateKey::MissedUnlisted),
            (key::MISSED_CONSENTS.into(), StateKey::MissedConsents),
            (key::PAYER_PASS_OWED.into(), StateKey::PayerPassOwed),
            (key::LISTED_CERT.into(), StateKey::ListedCert),
            (key::hold_confirmed(&pubky), StateKey::HoldConfirmed(pubky)),
            (key::LISTS_READ.into(), StateKey::ListsRead),
            (key::DENIED.into(), StateKey::Denied),
            (
                format!("{}{}-7", key::TAKEN, "05".repeat(16)),
                StateKey::Taken(format!("{}-7", "05".repeat(16))),
            ),
            (key::held_list(&pubky), StateKey::HeldList(pubky)),
            (key::side(id), StateKey::Side(id.into())),
            (
                key::seen_records(&pubky, &app),
                StateKey::SeenRecords {
                    payee: pubky,
                    app_id: app,
                },
            ),
            (key::spam(&pubky), StateKey::Spam(pubky)),
            (
                key::resent(&pubky, id),
                StateKey::Resent {
                    requester: pubky,
                    msg_id: id.into(),
                },
            ),
            (
                key::resent_day(&pubky, 10),
                StateKey::ResentDay {
                    requester: pubky,
                    day: 10,
                },
            ),
            (
                key::list_ask(&pubky, Some(&app)),
                StateKey::ListAsk {
                    payee: pubky,
                    app_id: Some(app),
                },
            ),
            (
                key::list_ask(&pubky, None),
                StateKey::ListAsk {
                    payee: pubky,
                    app_id: None,
                },
            ),
            (
                key::kept_reply(&pubky, &pubky, id),
                StateKey::KeptReply {
                    sender: pubky,
                    record: pubky,
                    msg_id: id.into(),
                },
            ),
            (
                key::dropped_replies(&pubky),
                StateKey::DroppedReplies { sender: pubky },
            ),
            (
                key::dropped_unverified(&pubky),
                StateKey::DroppedUnverified { sender: pubky },
            ),
            (
                key::stored_note(&pubky, id),
                StateKey::StoredNote {
                    sender: pubky,
                    msg_id: id.into(),
                },
            ),
            (
                key::notice(&attempt, crate::notices::NoticeKind::PaidAfterFailed),
                StateKey::Notice {
                    attempt,
                    kind: crate::notices::NoticeKind::PaidAfterFailed,
                },
            ),
            (
                key::message_notice(&pubky, id),
                StateKey::MessageNotice {
                    sender: pubky,
                    msg_id: id.into(),
                },
            ),
            (
                key::notice(&attempt, crate::notices::NoticeKind::HeldWithoutPay),
                StateKey::Notice {
                    attempt,
                    kind: crate::notices::NoticeKind::HeldWithoutPay,
                },
            ),
            (key::allowance_end(&pubky), StateKey::AllowanceEnd(pubky)),
            (key::consent(&pubky), StateKey::Consent(pubky)),
            (key::notice_target(id), StateKey::NoticeTarget(id.into())),
        ];
        for (built, family) in cases {
            assert_eq!(StateKey::parse(built.as_bytes()), Some(family), "{built}");
        }
    }

    #[test]
    fn test_state_key_refuses_what_no_builder_writes() {
        for key in [
            "zz/1",
            "peer/not-base64",
            "peer/",
            "ask/AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM/7",
            "ans/00000000000000000001/not-a-uuid",
            "missed/1",
            "j/",
            "ctx",
            "side/123E4567-E89B-42D3-A456-426614174000",
            "lq/AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM/not-an-app",
            "lq/",
            "nt/BQUFBQUFBQUFBQUFBQUFBQ/paid",
            "dt/05-7",
            "nt/BQUFBQUFBQUFBQUFBQUFBQ",
            "ae/not-base64",
            "ac/",
            "en/not-a-uuid",
        ] {
            assert_eq!(StateKey::parse(key.as_bytes()), None, "{key}");
        }
    }
}
