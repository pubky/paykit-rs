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
//! protocol's other callbacks, a [`pubky_switch_client::app::LocalView`]. Only this module
//! makes them, and it binds every value it reads to its key before any caller sees it
//! ([`Stored`]), at a restore as at runtime.
//! Every value is `{"v":1,"d":<record>}`; a reader refuses another `v` as corrupt.
//!
//! The whole of it is Paykit's layer in the app's backups ([`crate::PaykitBackup`]), which a
//! restore installs, the journal checked and marked restored, in the commit of the
//! client's own state (Switch spec 12.4).
//!
//! | Key (after `p/`) | Holds | Sensitive |
//! |---|---|---|
//! | `ask/<sender>/<from>` | a re-send to ask of a sender, kept while sends to it are held back (`resend.rs`) | |
//! | `ans/<seq>/<msg_id>` | a `resend_request` received and still to be answered | |
//! | `missed/<from>` | entries missed before a provider loss: every counterparty with Paykit activity is asked to re-send | |
//! | `mi/<from>` | entries missed while lagging, a `missed` item's: the counterparties of open requests and unfinished payments are asked to re-send; kept, with those asked already, while a record whose counterparty it may ask is sealed | |
//! | `j/<attempt_id>` | the payment journal: one payment, from before its `attempt` | yes |
//! | `side/<msg_id>` | the side a cancellation is sent from, kept with the batch that queues it | |
//! | `ar/<payee>/<app_id>` | a payee app's records that took part in its comparison, for its public endpoints | |
//! | `ct` | the acceptance time of the last Paykit entry the receive step passed | |
//! | `rh` | the hold a restore set on the open requests (`hold.rs`); never backed up | |
//! | `hc/<record key>` | the user confirmed a held request; never backed up | |
//! | `lh/<record key>` | a Private Payment List held because its `payment_lists` record doesn't read: never written over it, which could drop its single-use marks, until the record reads | |
//! | `kc/<record key>` | a payee's cancellation of a request, kept until it is placed in the payer's request record | |
//! | `peer/<pubky>` | a counterparty with Paykit activity, and when | |
//! | `lr` | when the app last read the lists, on the device clock; absent before the first read and after a due read failed, when new activity waits (Paykit wire 4.1); never restored, so a restored app reads the deny list again before any payment | |
//! | `spam/<pubky>` | how many messages from a blocked or flagged sender only were counted (Paykit wire 4.1) | |
//! | `rs/<pubky>/<msg_id>` | when a message was last re-sent to a requester | |
//! | `rsd/<pubky>/<day>` | how many messages were re-sent to a requester that day | |
//! | `nl` | set by a restore: the shared records aren't listed since, so the client's copy is empty and a record's absence means nothing yet; cleared once they are listed; never restored | |
//! | `lc` | the certificate the shared records were last listed under, signed: a certificate held now other than it means the copy may lack records until Paykit lists them, since one held between may have read less (`hold.rs`); written only by a Paykit listing, in its commit; absent from a join or a restore until that listing, which leaves the records unlisted; never restored | |
//! | `ro` | set by a restore: this app's own messages its activity and Allowance records store to be sent are queued again once the records are listed (`hold.rs`, `Paykit::queue_own_again`); after a pass, the counterparties whose messages wait: those whose sends were held then, and those whose period details wait for their request record to show whether a later stop covers them; or none named, every record judged again at the next receive, after a pass that found an activity or Allowance record it couldn't read within 30 days of the restore; and the restore's time, which `Paykit::restore` writes right after the restore and the mark keeps; never restored | |
//! | `ha/<payee>/<msg_id>` | a rejection or payer's cancellation recorded while sends to its payee were held, its message stored in the request record and not queued, written with the record's batch: queued from the record once sends to the payee aren't held, or given up with a notice past its window (`hold.rs`, `Paykit::queue_own_again`) | |
//! | `pp` | set by a restore: a payer pass is owed, so no new attempt is made and nothing is dispatched; cleared only after a reconcile under `pay` ran that pass (`hold.rs`); never restored | |
//! | `lq/<payee>[/<app_id>]` | a `private_payment_list_request` asked of a payee's app, or of none: the version it named and when (Paykit spec 8.4) | |
//! | `nt/<attempt_id>/<kind>`, `nt/<sender>/<msg_id>` | a notice for the host about a payment, or a message given up, until acknowledged (`notices.rs`) | |
//! | `dl` | the deny list as the app last read it, with this app's blocks kept unanswered (`bk/`), for the views and a payment the user starts (Paykit spec 8.1) | |
//! | `bk/<pubky>` | this app's block or unblock of `pubky` not yet shown by the list (`OwnBlockMark`): a block from before the switch is asked, an unblock only with the batch the client keeps; a block counts until a read of the list shows it, and is written again at a receive while the client neither keeps nor applied its batch, a restored one too, since the kept batch stays with the device that kept it; an unblock counts for payments and held sends only while the client still keeps its batch, never for the spam rule, and isn't restored (Paykit spec 8.1) | |
//! | `pd/<sender>` | a mark of a sender's replies dropped for the bounds on kept replies: the earliest and the latest acceptance among them, and when it was last asked to send them again (`marks.rs`) | |
//! | `ud/<sender>` | a mark of a sender's entries dropped unverified, as `pd/` holds (`marks.rs`) | |
//! | `kw/<sender>` | a mark of a sender's entries the client dropped while they waited for a scope key, as `pd/` holds, with the data epochs they waited for; it asks only while the app holds every key of those epochs (`marks.rs`) | |
//! | `pr/<sender>/<record key>/<msg_id>` | a payer's reply, or a payee's period details, for a request the app holds no record of yet, or whose record it holds but can't open, read or bind, kept until it does, placed only once the record names its sender, a proof of a payment without a request for the payee's record of its settlement that the app holds but can't read, placed once it reads naming its sender and removed if it names another, or a receipt for a payer record the app holds but can't read and that isn't paused as divergent, kept until it reads, and moved to `dw/` if it reads paused (`receive.rs`), or a counterparty's Allowance answer before the app holds its Allowance record, or an acceptance waiting over a record seen divergent for the user's confirmation (`allowances.rs`); each with the feed `seq` its entry was read at, which orders it (`records::KeptOrder`) | |
//! | `dt/<event ID>` | a duty event the SDK has taken (`stored.rs`), until it is acknowledged; never restored: a restore's events are taken again, which changes nothing, since a dropped entry's time is already in its sender's mark | |
//! | `dv/<record key>` | the pause mark of a record the app has seen divergent: paused until the user confirms, or the write the user confirmed over, with the write a credit's refusal showed it at and the write a credit's confirmation was given over, which is always the write that showing names, at or before the plain confirmation: any other stored state reads as a corrupt store (`divergent.rs`); a restore installs it paused, neither shown nor confirmed for a credit | |
//! | `dw/<record key>/<role>/<sender>/<msg_id>` | a received message, by its sender and `msg_id`, whose write over a divergent record waits for the user's confirmation, or for the messages kept over the record before it, to write into its record later, in the order its inbox accepted them (by acceptance time, then the feed `seq` its entry was read at, as the kept replies are, never by `msg_id`), a receipt over a payer record that doesn't read included, written once the record also reads, and one kept in `pr/` whose record read paused; never one from anyone but the counterparty of a record that reads; given up at its 30 days, also one that no longer points to its record (`divergent.rs`) | |
//! | `se/<sender>/<msg_id>` | the message of an entry the client stored for good, with the entry's ID and message hash and the feed `seq` it was read at, to write into its record later (`stored.rs`); never restored, since the client tells of the entries it restores again | |
//! | `ae/<record key>/<msg_id>` | a counterparty's `allowance_end`, kept as the stop until the Allowance record holds an end or a rejection; every distinct end is kept, each under its `msg_id` and with the feed `seq` that orders it, until the record shows which one names what it holds (`allowances.rs`, Paykit spec 8.3) | |
//! | `ac/<record key>` | the consent list: an Allowance this app may pay under, or removed from it for good (`allowances.rs`, Paykit spec 8.3); a restore marks every entry to confirm again | |
//! | `cf/<record key>` | a second, different proposal or response of an Allowance, kept as the stop until the Allowance record holds `conflicted` (`allowances.rs`, Paykit spec 8.3) | |
//! | `mc` | set by a `missed` item: every entry of the consent list is to be confirmed again, as after a restore; nothing is consented while it is set, and the next receive marks each entry so and clears it (`allowances.rs`); never restored | |
//! | `en/<msg_id>` | the executor app an `executor_notice` is wrapped to, kept with the batch that queues it | |
//! | `eo/<record key>` | this app's own crossing end of an Allowance, recorded while sends to its counterparty were held, whose message is sent from the record once they aren't (`allowances.rs`, `Paykit::send_owed_ends`); never restored | |

use paykit_lib::bodies::ResendRequest;
use paykit_lib::derive::{natural_key, request_obligation_blinded};
use paykit_lib::scopes::verify_certificate;
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{LocalView, LocalWrite, ReceiveContext, RecordTarget};
use pubky_switch_core::ident::record_key_from_blinded;
use pubky_switch_core::records::AppCert;
use pubky_switch_core::sig::SignedDoc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::activity::{ActivityTarget, KeptSide};
use crate::allowances::{allowance_target, ConsentEntry, NoticeTarget, OwedEnd};
use crate::app_records::SeenRecords;
use crate::error::{PaykitError, Result};
use crate::hold::HeldAnswer;
use crate::hold::MissedHold;
use crate::hold::OwnResendOwed;
use crate::hold::RestoreHold;
use crate::journal::JournalEntry;
use crate::marks::{DropMark, KeyWaitMark};
use crate::notices::KeptNotice;
use crate::pay::ListAsk;
use crate::paykit::Paykit;
use crate::protocol::{MissedItemMark, QueuedAsk};
use crate::receive::{Fail, QueuedAnswer};
use crate::records::{list_target, KeptMessage};
use crate::stored::StoredNote;

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

/// Read a local record's bytes. Private: every read binds the value to its key, through
/// [`bound`].
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
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
    /// What binds a value read here to its key: this app's identity, and its naming key's
    /// blinding through the client.
    fn binding<R>(&self, read: impl FnOnce(&Binder<'_>) -> R) -> R {
        let blind = |input: &[u8]| self.app.blind(input);
        read(&Binder::new(self.identity(), self.app.cert(), &blind))
    }

    /// The value at protocol key `key`, read whole and bound to it ([`Stored::bind`]): `Err`
    /// for a store failure, and a corrupt store for a value that doesn't read, isn't its
    /// family's, or names another key than `key`. Never `None` for one that doesn't bind.
    pub(crate) async fn local_get<T: Stored>(&self, key: &str) -> Result<Option<T>> {
        match self.app.protocol_get(key.as_bytes()).await? {
            None => Ok(None),
            Some(bytes) => self
                .binding(|binder| bound(key.as_bytes(), &bytes, binder))
                .map(Some),
        }
    }

    /// Every value under protocol prefix `prefix`, with the key's rest after the prefix, each
    /// read whole and bound to its own key as [`Paykit::local_get`] reads one.
    pub(crate) async fn local_scan<T: Stored>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        let held = self.app.protocol_scan(prefix.as_bytes()).await?;
        self.binding(|binder| {
            let mut out = Vec::with_capacity(held.len());
            for (key, value) in &held {
                let rest = rest_of(key, prefix.as_bytes()).ok_or_else(PaykitError::corrupt)?;
                out.push((rest, bound(key, value, binder)?));
            }
            Ok(out)
        })
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

/// What binding a value read back to its key takes: this app's identity, its certificate,
/// which verifies the messages a value keeps that this app signed, and its naming key's
/// blinding, which derives the record keys and claim keys a value names. A [`LocalView`]
/// has neither certificate nor blinding: a value whose binding needs them doesn't read
/// there.
pub(crate) struct Binder<'a> {
    own: [u8; 32],
    cert: Option<&'a AppCert>,
    blind: Option<Blind<'a>>,
}

/// The naming key's blinding (Switch spec 13.3).
type Blind<'a> = &'a dyn Fn(&[u8]) -> [u8; 32];

impl<'a> Binder<'a> {
    /// The binding of the app `own`, under `cert`, blinding with `blind`.
    pub(crate) fn new(own: [u8; 32], cert: &'a AppCert, blind: Blind<'a>) -> Self {
        Self {
            own,
            cert: Some(cert),
            blind: Some(blind),
        }
    }

    /// The binding of a reader without the certificate or the naming key, a [`LocalView`].
    fn unblinded(own: [u8; 32]) -> Self {
        Self {
            own,
            cert: None,
            blind: None,
        }
    }

    fn blind(&self) -> Result<Blind<'a>> {
        self.blind.ok_or_else(PaykitError::corrupt)
    }

    /// The record key of `target`, as the client derives it (Switch spec 13.3).
    fn record_key(&self, target: &RecordTarget) -> Result<[u8; 32]> {
        let policy = target
            .collection
            .policy
            .hash()
            .map_err(|_| PaykitError::corrupt())?;
        Ok(record_key_from_blinded(
            &target.collection.name,
            &self.blind()?(&target.natural_key),
            &policy,
        ))
    }
}

mod sealed {
    /// Only `state.rs` implements [`super::Stored`], beside the reader.
    pub trait Sealed {}
}

/// A value of Paykit's local state, as its families hold it, and its binding to the key it
/// is read at. Every read goes through [`Paykit::local_get`], [`Paykit::local_scan`],
/// [`cx_get`], [`view_get`], [`view_scan`] or, at a restore, [`restored`], each of which
/// binds what it decodes; the type is sealed, and [`decode`] private, so no value of
/// Paykit's state is read anywhere without its binding.
pub(crate) trait Stored: sealed::Sealed + DeserializeOwned {
    /// Bind the value read at `key`, of family `family`, to it: a corrupt store for a value
    /// of another family, or one naming anything other than what its key names.
    fn bind(&self, key: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()>;
}

/// The value `bytes` read at `key`, bound to it ([`Stored::bind`]).
fn bound<T: Stored>(key: &[u8], bytes: &[u8], binder: &Binder<'_>) -> Result<T> {
    let text = std::str::from_utf8(key).map_err(|_| PaykitError::corrupt())?;
    let family = StateKey::parse(key).ok_or_else(PaykitError::corrupt)?;
    let value = decode::<T>(bytes)?;
    value.bind(text, &family, binder)?;
    Ok(value)
}

/// The value at `key`, read through the receive context and bound to it as
/// [`Paykit::local_get`] binds it. A value that doesn't bind is a corrupt store, which parks
/// the entry (Switch spec 8.2).
pub(crate) fn cx_get<T: Stored>(
    cx: &ReceiveContext<'_>,
    key: &str,
) -> std::result::Result<Option<T>, Fail> {
    let Some(bytes) = cx.local_get(key.as_bytes())? else {
        return Ok(None);
    };
    let blind = |input: &[u8]| cx.blind(input);
    bound(
        key.as_bytes(),
        &bytes,
        &Binder::new(cx.identity(), cx.cert(), &blind),
    )
    .map(Some)
    .map_err(|_| Fail::corrupt())
}

/// Every value under `prefix`, read through the receive context, with its full key, each
/// bound to its own key as [`cx_get`] binds one.
pub(crate) fn cx_scan<T: Stored>(
    cx: &ReceiveContext<'_>,
    prefix: &str,
) -> std::result::Result<Vec<(String, T)>, Fail> {
    let blind = |input: &[u8]| cx.blind(input);
    let binder = Binder::new(cx.identity(), cx.cert(), &blind);
    let mut out = Vec::new();
    for (key, bytes) in cx.local_scan(prefix.as_bytes())? {
        let value = bound(&key, &bytes, &binder).map_err(|_| Fail::corrupt())?;
        let key = String::from_utf8(key).map_err(|_| Fail::corrupt())?;
        out.push((key, value));
    }
    Ok(out)
}

/// The value at `key`, read through a [`LocalView`] and bound to it.
pub(crate) fn view_get<T: Stored>(view: &LocalView<'_>, key: &str) -> Result<Option<T>> {
    let Some(bytes) = view.local_get(key.as_bytes())? else {
        return Ok(None);
    };
    bound(key.as_bytes(), &bytes, &Binder::unblinded(view.identity())).map(Some)
}

/// Every value under `prefix`, read through a [`LocalView`], with the key's rest after the
/// prefix, each bound to its own key.
pub(crate) fn view_scan<T: Stored>(view: &LocalView<'_>, prefix: &str) -> Result<Vec<(String, T)>> {
    let binder = Binder::unblinded(view.identity());
    let mut out = Vec::new();
    for (key, value) in view.local_scan(prefix.as_bytes())? {
        let rest = rest_of(&key, prefix.as_bytes()).ok_or_else(PaykitError::corrupt)?;
        out.push((rest, bound(&key, &value, &binder)?));
    }
    Ok(out)
}

/// A value of a backup's Paykit layer at `key`, bound to it as every runtime read binds it.
pub(crate) fn restored<T: Stored>(key: &[u8], value: &[u8], binder: &Binder<'_>) -> Result<T> {
    bound(key, value, binder)
}

fn corrupt() -> Result<()> {
    Err(PaykitError::corrupt())
}

impl sealed::Sealed for u64 {}

/// A time or a count. Only a `missed` item's names anything: its time, which the key names.
impl Stored for u64 {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            // The re-sends are asked from the value, so it is the time its key names.
            StateKey::Missed { from_ms } if self == from_ms => Ok(()),
            StateKey::CursorTime
            | StateKey::Peer(_)
            | StateKey::Spam(_)
            | StateKey::Resent { .. }
            | StateKey::ResentDay { .. }
            | StateKey::ListsRead => Ok(()),
            _ => corrupt(),
        }
    }
}

/// This app's block or unblock of the identity its key names (`bk/<pubky>`), not yet shown
/// by a read of the lists (Paykit spec 8.1, "a block or unblock the app wrote counts at
/// once", for payment decisions). The spam rule counts only its block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum OwnBlockMark {
    /// A block or unblock with the client's kept batch it rides on: `None` only for a
    /// block, not yet answered, or answered with no batch kept.
    Sent {
        /// A block (`true`) or an unblock.
        blocked: bool,
        /// The batch the client keeps it in.
        batch: Option<[u8; 16]>,
    },
    /// A mark as written before marks named their batch: a block (`true`) or an unblock.
    Bare(bool),
}

impl OwnBlockMark {
    /// A block (`true`) or an unblock.
    pub(crate) fn blocked(&self) -> bool {
        match self {
            Self::Sent { blocked, .. } | Self::Bare(blocked) => *blocked,
        }
    }

    /// The batch the client keeps it in, if it named one.
    pub(crate) fn batch(&self) -> Option<[u8; 16]> {
        match self {
            Self::Sent { batch, .. } => *batch,
            Self::Bare(_) => None,
        }
    }

    /// The batch an unblock rides on, the one the client must still keep for it to count.
    pub(crate) fn unblock_batch(&self) -> Option<[u8; 16]> {
        match self {
            Self::Sent {
                blocked: false,
                batch,
            } => *batch,
            _ => None,
        }
    }

    /// What the mark counts as now for payments and held sends, given whether the client
    /// still keeps the batch of an unblock (`batch_kept`), as the client answers it:
    /// `Some(true)` for a block, which always counts, failing closed; `Some(false)` for an
    /// unblock whose batch the client keeps; `None` for an unblock that counts no more: its
    /// batch dropped or ended, or one named no batch, as a mark of the earlier form, so the
    /// list decides. The spam rule asks only [`OwnBlockMark::blocked`]: an unblock never
    /// overrides the client's copy of the lists there.
    pub(crate) fn counts(&self, batch_kept: bool) -> Option<bool> {
        if self.blocked() {
            return Some(true);
        }
        (self.unblock_batch().is_some() && batch_kept).then_some(false)
    }
}

impl sealed::Sealed for OwnBlockMark {}

/// Every form a run writes; an unblock with no batch is no such form: an unblock is marked
/// only with the batch the client keeps it in.
impl Stored for OwnBlockMark {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match (family, self) {
            (
                StateKey::OwnBlock(_),
                Self::Sent {
                    blocked: false,
                    batch: None,
                },
            ) => corrupt(),
            (StateKey::OwnBlock(_), _) => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for bool {}

/// A mark, which every writer sets `true`.
impl Stored for bool {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::RecordsUnlisted
            | StateKey::MissedUnlisted
            | StateKey::MissedConsents
            | StateKey::PayerPassOwed
            | StateKey::HoldConfirmed(_)
            | StateKey::Taken(_)
                if *self =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for RestoreHold {}

/// A restore's hold, which names nothing.
impl Stored for RestoreHold {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::RestoreHold => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for SignedDoc {}

/// The certificate the records were last listed under: this identity's.
impl Stored for SignedDoc {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::ListedCert => AppCert::verify(self, &binder.own)
                .map(|_| ())
                .map_err(|_| PaykitError::corrupt()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for QueuedAsk {}

/// A re-send to ask: of the sender, from the time, its key names.
impl Stored for QueuedAsk {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::Ask { sender, from_ms }
                if self.sender == *sender && self.from_ms == *from_ms =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for QueuedAnswer {}

/// A `resend_request` received and queued to answer (Paykit spec 9): at the `seq` and
/// `msg_id` its key names, its body a `resend_request`'s, and its certificate the
/// requester's.
impl Stored for QueuedAnswer {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::Answer { seq, msg_id } if self.seq == *seq && self.msg_id == *msg_id => {
                ResendRequest::from_body(&self.body).map_err(|_| PaykitError::corrupt())?;
                verify_certificate(&self.cert, &self.requester)
                    .map(|_| ())
                    .map_err(|_| PaykitError::corrupt())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for KeptSide {}

/// The side a message was sent from: of the message its key names.
impl Stored for KeptSide {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::Side(msg_id) if self.msg_id == *msg_id => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for SeenRecords {}

/// A payee app's records seen (Switch spec 4.2): each the payee's, as it signed it, of the
/// app its key names.
impl Stored for SeenRecords {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::SeenRecords { payee, app_id } if self.verifies(payee, app_id) => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for ListAsk {}

/// A list request asked: of the payee, and the app or none, its key names.
impl Stored for ListAsk {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::ListAsk { payee, app_id }
                if self.payee == *payee && self.app_id == *app_id =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for KeptMessage {}

/// A message kept: verified for this app, and of the record its key names.
///
/// - At `kc/<record key>`, a `paykit.payment_request_cancellation` its inbox accepted, kept
///   under the record key of the payer's record of the request it cancels, from its sender
///   and the first of its `refs` (Paykit spec 8.1).
/// - At `pr/<sender>/<record key>/<msg_id>`, a payer's acceptance, rejection, cancellation
///   or proof, or a payee's period details, its inbox accepted, from that sender and of that
///   `msg_id`, kept under the record key of the request record it waits for, of the request
///   it names first: this app's payee record, under its label, for a reply; the payer's
///   record of the sender's request for details (Switch spec 8.2, Paykit spec 8.1, 8.2); or
///   a `paykit.payment_receipt`, kept under the record key of the payer's record it names
///   ([`crate::receive::receipt_target`]); or a proof of a payment without a request, from
///   that sender and of that `msg_id`, whose record, the payee's of its settlement, is
///   proven against the rail before it is placed.
/// - At `lh/<record key>`, a `paykit.private_payment_list`, kept under the record key of its
///   payee app's `payment_lists` record, from its sender and App ID (Paykit wire 5.4).
/// - At `pr/<sender>/<record key>/<msg_id>` too, an Allowance's acceptance or rejection its
///   inbox accepted before this app held the proposal's record, or an acceptance that waits
///   over that record for the user's confirmation, from that sender and of that `msg_id`,
///   kept under the record key of the Allowance record it names, with its feed `seq` (Paykit
///   spec 6, 8.1, 8.3).
/// - At `ae/<record key>/<msg_id>`, a counterparty's `paykit.allowance_end` its inbox
///   accepted, of that `msg_id`, with its feed `seq`, kept as the stop under the record key of
///   the Allowance record of its sender and the Allowance it names (Paykit spec 8.3, "Ends
///   reach the executor").
/// - At `cf/<record key>`, a second, different proposal, acceptance or rejection its inbox
///   accepted, with its acceptance time, kept as the stop under the record key of the
///   Allowance record of its sender and the Allowance it names (Paykit spec 8.3,
///   "Conflicted").
impl Stored for KeptMessage {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let message = self.stored(&binder.own)?;
        let first = || {
            message
                .message()
                .refs
                .first()
                .copied()
                .ok_or_else(PaykitError::corrupt)
        };
        let (target, record_key) = match (family, message.body()) {
            // A proof of a payment without a request kept for the payee's record of its
            // settlement, which didn't read: the record is the one the rail's settlement of
            // the proof names, which no binder can read, so it is proven before the proof is
            // placed (`Paykit::place_kept_replies`): one kept for another record is never
            // written, and is given up at its 30 days.
            (StateKey::KeptReply { sender, msg_id, .. }, Body::PaymentProof(_))
                if message.message().refs.is_empty()
                    && message.sender() == *sender
                    && message.message().msg_id == *msg_id
                    && message.accepted_at().is_some() =>
            {
                return Ok(());
            }
            (StateKey::KeptCancellation(record_key), Body::PaymentRequestCancellation(_))
                if message.accepted_at().is_some() =>
            {
                let target = ActivityTarget::payer(natural_key::request_record(
                    &message.sender(),
                    &first()?,
                ));
                (target.record_target(), record_key)
            }
            (
                StateKey::KeptReply {
                    sender,
                    record,
                    msg_id,
                },
                Body::PaymentRequestAcceptance(_)
                | Body::PaymentRequestRejection(_)
                | Body::PaymentRequestCancellation(_)
                | Body::PaymentProof(_)
                | Body::PaymentPeriodDetails(_),
            ) if message.sender() == *sender
                && message.message().msg_id == *msg_id
                && message.accepted_at().is_some() =>
            {
                let target = crate::receive::kept_request_target(&binder.own, &message, &first()?);
                (target.record_target(), record)
            }
            // A receipt kept for the payer's record it names, which didn't read.
            (
                StateKey::KeptReply {
                    sender,
                    record,
                    msg_id,
                },
                Body::PaymentReceipt(receipt),
            ) if message.sender() == *sender
                && message.message().msg_id == *msg_id
                && message.accepted_at().is_some() =>
            {
                let target =
                    crate::receive::receipt_target(&message, &binder.own, &receipt.attempt_id);
                (target.record_target(), record)
            }
            (StateKey::HeldList(record_key), Body::PrivatePaymentList(_))
                if message.accepted_at().is_some() =>
            {
                (
                    list_target(&message.sender(), &message.message().from.app_id),
                    record_key,
                )
            }
            (
                StateKey::KeptReply {
                    sender,
                    record,
                    msg_id,
                },
                Body::AllowanceAcceptance(answer) | Body::AllowanceRejection(answer),
            ) if message.sender() == *sender
                && message.message().msg_id == *msg_id
                && message.accepted_at().is_some() =>
            {
                (allowance_target(sender, &answer.allowance_id), record)
            }
            (
                StateKey::AllowanceEnd {
                    record: record_key,
                    msg_id,
                },
                Body::AllowanceEnd(end),
            ) if message.message().msg_id == *msg_id && message.accepted_at().is_some() => (
                allowance_target(&message.sender(), &end.allowance_id),
                record_key,
            ),
            (StateKey::Conflict(record_key), Body::AllowanceProposal(proposal))
                if message.accepted_at().is_some() =>
            {
                (
                    allowance_target(&message.sender(), &proposal.allowance_id),
                    record_key,
                )
            }
            (
                StateKey::Conflict(record_key),
                Body::AllowanceAcceptance(answer) | Body::AllowanceRejection(answer),
            ) if message.accepted_at().is_some() => (
                allowance_target(&message.sender(), &answer.allowance_id),
                record_key,
            ),
            _ => return corrupt(),
        };
        if binder.record_key(&target)? != *record_key {
            return corrupt();
        }
        // A member of a bounded pool, the kept replies or the kept stops, is ordered by when
        // its inbox accepted it, by the feed `seq` its entry was read at within one
        // millisecond (`records::KeptOrder`): which one the bounds drop, which of an
        // Allowance's ends stops it and which of its answers is the record's response. One
        // without its `seq` can't be ordered; a held list and a conflict are ordered by
        // nothing, and no writer gives them one.
        let pooled = matches!(
            family,
            StateKey::KeptCancellation(_)
                | StateKey::KeptReply { .. }
                | StateKey::AllowanceEnd { .. }
        );
        if pooled != self.seq().is_some() {
            return corrupt();
        }
        Ok(())
    }
}

impl sealed::Sealed for KeptNotice {}

/// A notice: of the attempt and kind, or the sender and `msg_id`, its key names
/// ([`crate::notices::bind_notice`]).
impl Stored for KeptNotice {
    fn bind(&self, key: &str, _: &StateKey, _: &Binder<'_>) -> Result<()> {
        crate::notices::bind_notice(key, self)
    }
}

impl sealed::Sealed for StoredNote {}

/// A stored entry's note: of the sender and `msg_id` its key names, its message verified as
/// theirs ([`StoredNote::bound`]).
impl Stored for StoredNote {
    fn bind(&self, key: &str, _: &StateKey, binder: &Binder<'_>) -> Result<()> {
        self.bound(key, &binder.own).map(|_| ())
    }
}

impl sealed::Sealed for JournalEntry {}

/// A journal entry (Paykit spec 7.2), checked as a restore checks it
/// ([`JournalEntry::check_restored`]): of the attempt its key names; its claim key the one
/// its payee and request blind to; the request it keeps that payee's request of that hash,
/// amount, reference and label; its carrier verified for the payee; its acceptance this
/// app's, of that request, at the `msg_id` it keeps; and its record the payer's record of
/// that request, or, for a payment without a request, of this app's attempt the key names.
/// An entry naming another attempt, claim, record or message would read and write one
/// payment while its key names another.
impl Stored for JournalEntry {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::Journal(attempt) = family else {
            return corrupt();
        };
        if self.attempt()? != *attempt {
            return corrupt();
        }
        let blind = binder.blind()?;
        let cert = binder.cert.ok_or_else(PaykitError::corrupt)?;
        self.check_restored(&binder.own, cert, |payee, request_hash| {
            request_obligation_blinded(&blind(payee), &blind(request_hash), 0)
        })?;
        if self.request_hash()?.is_none()
            && self.natural_key != natural_key::payer_payment_record(&binder.own, attempt)
        {
            return corrupt();
        }
        Ok(())
    }
}

impl sealed::Sealed for MissedItemMark {}

/// A `missed` item's mark: of the time its key names, the counterparties it asked each once,
/// in order, as the asks leave them (`resend.rs`).
impl Stored for MissedItemMark {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::MissedItem { from_ms }
                if self.from_ms == *from_ms
                    && self.asked.windows(2).all(|pair| pair[0] < pair[1]) =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for MissedHold {}

/// A `missed` item's hold, which names nothing.
impl Stored for MissedHold {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::MissedHold => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for OwnResendOwed {}

/// The restore's re-send owed: the counterparties still waiting, if a pass ran, ascending,
/// each once, and the restore's time, which name no key.
impl Stored for OwnResendOwed {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::OwnResendOwed
                if self
                    .waiting
                    .as_ref()
                    .is_none_or(|waiting| waiting.windows(2).all(|pair| pair[0] < pair[1])) =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for HeldAnswer {}

/// A held answer: of the payee and message its key names.
impl Stored for HeldAnswer {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::HeldAnswer { payee, msg_id }
                if self.payee == *payee && self.msg_id == *msg_id =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for Vec<[u8; 32]> {}

/// The copy of the deny list: identities, which name no key.
impl Stored for Vec<[u8; 32]> {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::Denied => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for DropMark {}

/// A mark of a sender's dropped messages: of the sender its key names, its times not crossed
/// (`marks.rs`). The request it queued is checked against the outbox when it is read
/// (`Paykit::delivered_request`): its hash is the proof of the pointer.
impl Stored for DropMark {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::DroppedReplies { sender } | StateKey::DroppedUnverified { sender }
                if self.sender == *sender && self.earliest_ms <= self.latest_ms =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for KeyWaitMark {}

/// A mark of a sender's entries dropped while they waited for a scope key: of the sender its
/// key names, its times not crossed, never due by its flag (it is due while the app holds
/// its keys), and naming at least one data epoch, ascending, each once (`marks.rs`). Its
/// request is checked as a [`DropMark`]'s is.
impl Stored for KeyWaitMark {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        let mark = &self.mark;
        match family {
            StateKey::KeyWaits { sender }
                if mark.sender == *sender
                    && mark.earliest_ms <= mark.latest_ms
                    && !mark.due
                    && !self.epochs.is_empty()
                    && self.epochs.windows(2).all(|pair| pair[0] < pair[1]) =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::divergent::DivergentMark {}

/// A divergent record's pause mark: of the record its key names, of one of Paykit's
/// collections ([`crate::divergent::DivergentMark::bind_to`]).
impl Stored for crate::divergent::DivergentMark {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        let StateKey::Divergent(record) = family else {
            return corrupt();
        };
        self.bind_to(record)
    }
}

impl sealed::Sealed for crate::divergent::DivergentWrite {}

/// A message kept over a divergent record: of the record, role, sender and `msg_id` its key
/// names,
/// its message verified for this app and naming that record
/// ([`crate::divergent::DivergentWrite::bind_to`]).
impl Stored for crate::divergent::DivergentWrite {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::DivergentWrite {
            record,
            role,
            sender,
            msg_id,
        } = family
        else {
            return corrupt();
        };
        self.bind_to(record, *role, sender, msg_id, &binder.own, &|target| {
            binder.record_key(target)
        })
    }
}

impl sealed::Sealed for ConsentEntry {}

/// An entry of the consent list: of the Allowance record its key names, which its
/// counterparty and Allowance ID derive (Paykit spec 8.3). An entry naming another
/// Allowance would let the executor pay under one its user never consented to.
impl Stored for ConsentEntry {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::Consent(record) = family else {
            return corrupt();
        };
        let allowance_id = self.allowance_id()?;
        if binder.record_key(&allowance_target(&self.counterparty, &allowance_id))? != *record {
            return corrupt();
        }
        Ok(())
    }
}

impl sealed::Sealed for OwedEnd {}

/// An own crossing end owed while sends were held: of the Allowance record its key names,
/// with a `msg_id` a message carries (`allowances.rs`, [`OwedEnd`]).
impl Stored for OwedEnd {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::OwedEnd(record)
                if self.record == *record
                    && pubky_switch_core::json::check_uuid_v4(&self.msg_id).is_ok() =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for NoticeTarget {}

/// The executor app an `executor_notice` is wrapped to: of the notice's `msg_id` its key
/// names, written in the batch that queues that notice (`allowances.rs`).
impl Stored for NoticeTarget {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::NoticeTarget(msg_id) if self.msg_id == *msg_id => Ok(()),
            _ => corrupt(),
        }
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
    pub(crate) const MISSED_ITEMS: &str = "mi/";
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
    pub(crate) const OWN_RESEND_OWED: &str = "ro";
    pub(crate) const HELD_ANSWERS: &str = "ha/";
    pub(crate) const LISTED_CERT: &str = "lc";
    pub(crate) const NOTICES: &str = "nt/";
    pub(crate) const STORED_NOTES: &str = "se/";
    pub(crate) const KEPT_REPLIES: &str = "pr/";
    pub(crate) const DROPPED_REPLIES: &str = "pd/";
    pub(crate) const DROPPED_UNVERIFIED: &str = "ud/";
    pub(crate) const KEY_WAITS: &str = "kw/";
    pub(crate) const DIVERGENT: &str = "dv/";
    pub(crate) const DIVERGENT_WRITES: &str = "dw/";

    /// The pause mark of the divergent record `record_key`.
    pub(crate) fn divergent(record_key: &[u8; 32]) -> String {
        format!("{DIVERGENT}{}", b64(record_key))
    }

    /// The prefix of every message kept to write over the divergent record `record_key`.
    pub(crate) fn divergent_writes_at(record_key: &[u8; 32]) -> String {
        format!("{DIVERGENT_WRITES}{}/", b64(record_key))
    }

    /// The prefix of the messages of `role` kept to write over the divergent record
    /// `record_key`.
    pub(crate) fn divergent_writes_of(
        record_key: &[u8; 32],
        role: crate::divergent::WriteRole,
    ) -> String {
        format!("{DIVERGENT_WRITES}{}/{}/", b64(record_key), role.tag())
    }

    /// The message `msg_id` from `sender` of `role` kept to write over the divergent record
    /// `record_key`: a `msg_id` is the sender's own, so the key names both.
    pub(crate) fn divergent_write(
        record_key: &[u8; 32],
        role: crate::divergent::WriteRole,
        sender: &[u8; 32],
        msg_id: &str,
    ) -> String {
        format!(
            "{}{}/{msg_id}",
            divergent_writes_of(record_key, role),
            b64(sender)
        )
    }

    pub(crate) const ALLOWANCE_ENDS: &str = "ae/";
    pub(crate) const CONFLICTS: &str = "cf/";
    pub(crate) const CONSENTS: &str = "ac/";
    pub(crate) const NOTICE_TARGETS: &str = "en/";
    pub(crate) const OWED_ENDS: &str = "eo/";

    /// This app's own crossing end of the Allowance whose record is `record_key`, recorded
    /// while sends to the counterparty were held, whose message is owed.
    pub(crate) fn owed_end(record_key: &[u8; 32]) -> String {
        format!("{OWED_ENDS}{}", b64(record_key))
    }

    /// A counterparty's end `msg_id` of the Allowance whose record is `record_key`, kept as
    /// the stop.
    pub(crate) fn allowance_end(record_key: &[u8; 32], msg_id: &str) -> String {
        format!("{}{msg_id}", allowance_ends(record_key))
    }

    /// The prefix of every end kept as the stop of the Allowance whose record is
    /// `record_key`.
    pub(crate) fn allowance_ends(record_key: &[u8; 32]) -> String {
        format!("{ALLOWANCE_ENDS}{}/", b64(record_key))
    }

    /// A conflicting proposal or response of the Allowance whose record is `record_key`,
    /// kept as the stop.
    pub(crate) fn conflict(record_key: &[u8; 32]) -> String {
        format!("{CONFLICTS}{}", b64(record_key))
    }

    /// The consent list's entry of the Allowance whose record is `record_key`.
    pub(crate) fn consent(record_key: &[u8; 32]) -> String {
        format!("{CONSENTS}{}", b64(record_key))
    }

    /// The executor app the `executor_notice` `msg_id` is wrapped to.
    pub(crate) fn notice_target(msg_id: &str) -> String {
        format!("{NOTICE_TARGETS}{msg_id}")
    }

    /// The mark of replies from `sender` dropped for the bounds.
    pub(crate) fn dropped_replies(sender: &[u8; 32]) -> String {
        format!("{DROPPED_REPLIES}{}", b64(sender))
    }

    /// The mark of entries from `sender` dropped unverified.
    pub(crate) fn dropped_unverified(sender: &[u8; 32]) -> String {
        format!("{DROPPED_UNVERIFIED}{}", b64(sender))
    }

    /// The mark of `sender`'s entries dropped while they waited for a scope key.
    pub(crate) fn key_waits(sender: &[u8; 32]) -> String {
        format!("{KEY_WAITS}{}", b64(sender))
    }
    pub(crate) const DENIED: &str = "dl";
    pub(crate) const OWN_BLOCKS: &str = "bk/";
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

    /// The prefix of the notes of stored entries' messages from `sender`.
    pub(crate) fn stored_notes_of(sender: &[u8; 32]) -> String {
        format!("{STORED_NOTES}{}/", b64(sender))
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

    pub(crate) fn missed_item(from: u64) -> String {
        format!("{MISSED_ITEMS}{}", num(from))
    }

    pub(crate) fn journal(attempt_id: &AttemptId) -> String {
        format!("{JOURNAL}{}", attempt_id.to_text())
    }

    /// A rejection or payer's cancellation `msg_id` to `payee`, recorded while sends to it
    /// were held and not queued yet.
    pub(crate) fn held_answer(payee: &[u8; 32], msg_id: &str) -> String {
        format!("{HELD_ANSWERS}{}/{msg_id}", b64(payee))
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

    /// This app's block or unblock of `pubky` not yet shown by a read of the lists
    /// ([`OwnBlockMark`]). A block is marked before the switch is asked, so a crash before
    /// the call leaves it counted; its mark goes once the switch took it, or once a read of
    /// the lists shows it, and until then a block whose batch the client neither keeps nor
    /// applied is written again at each receive. An unblock counts for payments and held
    /// sends only while the client keeps its batch, and never for the spam rule.
    pub(crate) fn own_block(pubky: &[u8; 32]) -> String {
        format!("{OWN_BLOCKS}{}", b64(pubky))
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
    /// `missed/<from>`: the re-sends to ask for after a provider loss.
    Missed { from_ms: u64 },
    /// `mi/<from>`: a `missed` item's re-sends to ask for.
    MissedItem { from_ms: u64 },
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
    /// `ro`: the restore's re-send of this app's own messages is owed.
    OwnResendOwed,
    /// `lc`: the certificate the shared records were last listed under.
    ListedCert,
    /// `ha/<payee>/<msg_id>`: an answer recorded while sends to its payee were held.
    HeldAnswer { payee: [u8; 32], msg_id: String },
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
    /// `bk/<pubky>`: this app's block or unblock of `pubky` kept unanswered.
    OwnBlock([u8; 32]),
    /// `dt/<event ID>`: a duty event taken.
    Taken(String),
    /// `pd/<sender>`: the mark of a sender's replies dropped for the bounds.
    DroppedReplies { sender: [u8; 32] },
    /// `ud/<sender>`: the mark of a sender's entries dropped unverified.
    DroppedUnverified { sender: [u8; 32] },
    /// `kw/<sender>`: the mark of a sender's entries dropped while they waited for a key.
    KeyWaits { sender: [u8; 32] },
    /// `pr/<sender>/<record key>/<msg_id>`: a payer's reply kept for its record.
    KeptReply {
        sender: [u8; 32],
        record: [u8; 32],
        msg_id: String,
    },
    /// `se/<sender>/<msg_id>`: a stored entry's message to write later.
    StoredNote { sender: [u8; 32], msg_id: String },
    /// `nt/<sender>/<msg_id>`: a notice for the host about a message given up.
    MessageNotice { sender: [u8; 32], msg_id: String },
    /// `dv/<record key>`: the pause mark of a divergent record.
    Divergent([u8; 32]),
    /// `dw/<record key>/<role>/<sender>/<msg_id>`: a message kept to write over a divergent
    /// record.
    DivergentWrite {
        record: [u8; 32],
        role: crate::divergent::WriteRole,
        sender: [u8; 32],
        msg_id: String,
    },
    /// `nt/<attempt_id>/<kind>`: a notice for the host.
    Notice {
        attempt: AttemptId,
        kind: crate::notices::NoticeKind,
    },
    /// `ae/<record key>/<msg_id>`: a counterparty's Allowance end kept as the stop.
    AllowanceEnd { record: [u8; 32], msg_id: String },
    /// `ac/<record key>`: an entry of the consent list.
    Consent([u8; 32]),
    /// `cf/<record key>`: a conflicting proposal or response kept as the stop.
    Conflict([u8; 32]),
    /// `en/<msg_id>`: the executor app an `executor_notice` is wrapped to.
    NoticeTarget(String),
    /// `eo/<record key>`: this app's own crossing end, recorded while sends were held, whose
    /// message is owed.
    OwedEnd([u8; 32]),
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
            "ro" => Self::OwnResendOwed,
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
                    "mi" => Self::MissedItem {
                        from_ms: number(rest)?,
                    },
                    "peer" => Self::Peer(b64_32(rest)?),
                    "kc" => Self::KeptCancellation(b64_32(rest)?),
                    "bk" => Self::OwnBlock(b64_32(rest)?),
                    "hc" => Self::HoldConfirmed(b64_32(rest)?),
                    "lh" => Self::HeldList(b64_32(rest)?),
                    "side" => Self::Side(msg_id(rest)?),
                    "ha" => {
                        let (payee, id) = rest.split_once('/')?;
                        Self::HeldAnswer {
                            payee: b64_32(payee)?,
                            msg_id: msg_id(id)?,
                        }
                    }
                    "ar" => {
                        let (payee, app) = rest.split_once('/')?;
                        Self::SeenRecords {
                            payee: b64_32(payee)?,
                            app_id: b64_16(app)?,
                        }
                    }
                    "spam" => Self::Spam(b64_32(rest)?),
                    "ae" => {
                        let (record, id) = rest.split_once('/')?;
                        Self::AllowanceEnd {
                            record: b64_32(record)?,
                            msg_id: msg_id(id)?,
                        }
                    }
                    "ac" => Self::Consent(b64_32(rest)?),
                    "cf" => Self::Conflict(b64_32(rest)?),
                    "en" => Self::NoticeTarget(msg_id(rest)?),
                    "eo" => Self::OwedEnd(b64_32(rest)?),
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
                    "kw" => Self::KeyWaits {
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
                    "dv" => Self::Divergent(b64_32(rest)?),
                    "dw" => {
                        let (record, rest) = rest.split_once('/')?;
                        let (role, rest) = rest.split_once('/')?;
                        let (sender, id) = rest.split_once('/')?;
                        Self::DivergentWrite {
                            record: b64_32(record)?,
                            role: crate::divergent::WriteRole::from_tag(role)?,
                            sender: b64_32(sender)?,
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

    /// The source of every module but this one, comments dropped: `(path, code)`.
    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let code: String = text
                        .lines()
                        .filter(|line| !line.trim_start().starts_with("//"))
                        .map(|line| format!("{line}\n"))
                        .collect();
                    out.push((path.display().to_string(), code));
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        out.retain(|(path, _)| !path.ends_with("src/state.rs"));
        out
    }

    /// The text of each call of `.name(` in `code`, up to its closing parenthesis.
    fn calls<'a>(code: &'a str, name: &str) -> Vec<&'a str> {
        let needle = format!(".{name}(");
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(at) = code[from..].find(&needle) {
            let start = from + at + needle.len();
            let mut depth = 1usize;
            let mut end = start;
            for (i, c) in code[start..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = start + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.push(&code[start..end]);
            from = end.max(start);
        }
        out
    }

    #[test]
    fn test_state_no_raw_read_of_paykit_state_outside_its_reader() {
        // Paykit's own state is read only here, each value bound to its key (`Stored`): the
        // client's byte reads of the protocol's state appear in no other module, and the
        // backup layer's raw pairs only at the restore, which binds each with `restored`.
        for (path, code) in sources() {
            for raw in ["protocol_get(", "protocol_scan("] {
                assert!(!code.contains(raw), "{path}: a raw read `{raw}`");
            }
            if !path.ends_with("src/protocol.rs") {
                assert!(
                    !code.contains("decode_local_state"),
                    "{path}: a raw read of a backup's layer"
                );
            }
            for name in ["local_get", "local_scan"] {
                for args in calls(&code, name) {
                    assert!(
                        !args.contains("as_bytes") && !args.contains("b\""),
                        "{path}: a raw `{name}({args})` of the client's bytes"
                    );
                }
            }
        }
    }

    #[test]
    fn test_state_raw_read_scan_finds_a_raw_call() {
        let code = "let v = cx\n    .local_get(key::journal(&a).as_bytes())?;\nview.local_scan(b\"peer/\")";
        let found = calls(code, "local_get");
        assert_eq!(found, ["key::journal(&a).as_bytes()"]);
        assert_eq!(calls(code, "local_scan"), ["b\"peer/\""]);
        assert!(calls("self.local_get::<u64>(key::CURSOR_TIME)", "local_get").is_empty());
    }

    /// `value` read at `key` as a `T`, through a reader without the naming key.
    fn reads<T: Stored>(key: &str, value: serde_json::Value) -> bool {
        let bytes = serde_json::to_vec(&serde_json::json!({ "v": 1, "d": value })).unwrap();
        bound::<T>(key.as_bytes(), &bytes, &Binder::unblinded([1; 32])).is_ok()
    }

    #[test]
    fn test_state_own_block_mark_of_the_earlier_form_counts_only_as_a_block() {
        // A mark written before marks named their batch: a block counts, failing closed; an
        // unblock can't be tied to a batch the client keeps, so it counts no more.
        let read = |value: serde_json::Value| -> OwnBlockMark {
            let bytes = serde_json::to_vec(&serde_json::json!({ "v": 1, "d": value })).unwrap();
            bound::<OwnBlockMark>(
                key::own_block(&[1; 32]).as_bytes(),
                &bytes,
                &Binder::unblinded([1; 32]),
            )
            .unwrap()
        };
        assert_eq!(read(serde_json::json!(true)).counts(false), Some(true));
        assert_eq!(read(serde_json::json!(true)).counts(true), Some(true));
        assert_eq!(read(serde_json::json!(false)).counts(true), None);
        // One with its batch: an unblock counts only while the client keeps the batch.
        let unblock = read(serde_json::json!({ "blocked": false, "batch": vec![7u8; 16] }));
        assert_eq!(unblock.unblock_batch(), Some([7; 16]));
        assert_eq!(unblock.counts(true), Some(false));
        assert_eq!(unblock.counts(false), None);
        let block = read(serde_json::json!({ "blocked": true, "batch": null }));
        assert_eq!(block.counts(false), Some(true));
    }

    #[test]
    fn test_state_restore_hold_written_without_its_asked_reads() {
        // A restore's hold as a run before the `asked` member wrote it, mid-hold when the
        // store is upgraded: a valid run's form, so it reads, having asked none.
        let hold = serde_json::json!({ "resend_from_ms": 1, "asks_queued": false });
        assert!(reads::<RestoreHold>(key::RESTORE_HOLD, hold));
    }

    #[test]
    fn test_state_marks_bound_to_their_sender_with_times_not_crossed() {
        let mark = |sender: u8, earliest: u64, latest: u64| {
            serde_json::json!({
                "sender": pubky_switch_core::enc::b64(&[sender; 32]),
                "earliest_ms": earliest, "latest_ms": latest, "due": false,
                "asked_at_ms": null, "queued": null,
            })
        };
        let at = key::dropped_replies(&[1; 32]);
        assert!(reads::<DropMark>(&at, mark(1, 5, 6)));
        assert!(reads::<DropMark>(
            &key::dropped_unverified(&[1; 32]),
            mark(1, 5, 6)
        ));
        assert!(!reads::<DropMark>(&at, mark(2, 5, 6)));
        assert!(!reads::<DropMark>(&at, mark(1, 7, 6)));
        assert!(!reads::<DropMark>(&key::peer(&[1; 32]), mark(1, 5, 6)));
        // A mark of key waits: its own type at its own family, naming its epochs ascending,
        // each once, never due by its flag.
        let waits = |sender: u8, earliest: u64, latest: u64, due: bool, epochs: &[u64]| {
            let mut value = mark(sender, earliest, latest);
            value["due"] = serde_json::json!(due);
            value["epochs"] = serde_json::json!(epochs);
            value
        };
        let kw = key::key_waits(&[1; 32]);
        assert!(reads::<KeyWaitMark>(&kw, waits(1, 5, 6, false, &[2, 4])));
        assert!(!reads::<KeyWaitMark>(&kw, waits(2, 5, 6, false, &[2])));
        assert!(!reads::<KeyWaitMark>(&kw, waits(1, 7, 6, false, &[2])));
        assert!(!reads::<KeyWaitMark>(&kw, waits(1, 5, 6, true, &[2])));
        assert!(!reads::<KeyWaitMark>(&kw, waits(1, 5, 6, false, &[])));
        assert!(!reads::<KeyWaitMark>(&kw, waits(1, 5, 6, false, &[4, 2])));
        assert!(!reads::<KeyWaitMark>(&kw, waits(1, 5, 6, false, &[2, 2])));
        assert!(!reads::<KeyWaitMark>(&at, waits(1, 5, 6, false, &[2])));
        assert!(!reads::<DropMark>(&kw, mark(1, 5, 6)));
        // An own block or unblock, in either form, and the deny list's copy.
        for mark in [
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!({ "blocked": false, "batch": vec![7u8; 16] }),
            serde_json::json!({ "blocked": true, "batch": null }),
        ] {
            assert!(reads::<OwnBlockMark>(&key::own_block(&[1; 32]), mark));
        }
        assert!(!reads::<bool>(
            &key::own_block(&[1; 32]),
            serde_json::json!(false)
        ));
        // An unblock is marked only with its batch: one without is no run's.
        assert!(!reads::<OwnBlockMark>(
            &key::own_block(&[1; 32]),
            serde_json::json!({ "blocked": false, "batch": null })
        ));
        assert!(reads::<OwnBlockMark>(
            &key::own_block(&[1; 32]),
            serde_json::json!({ "blocked": true, "batch": vec![7u8; 16] })
        ));
        assert!(reads::<Vec<[u8; 32]>>(key::DENIED, serde_json::json!([])));
        assert!(!reads::<Vec<[u8; 32]>>(
            key::CURSOR_TIME,
            serde_json::json!([])
        ));
        assert!(reads::<bool>(key::MISSED_UNLISTED, serde_json::json!(true)));
        assert!(reads::<MissedHold>(
            key::MISSED_HOLD,
            serde_json::json!({ "bound_ms": null })
        ));
        assert!(!reads::<MissedHold>(
            key::RESTORE_HOLD,
            serde_json::json!({ "bound_ms": null })
        ));
    }

    #[test]
    fn test_state_values_bound_to_their_family_and_key() {
        let pubky = [3u8; 32];
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let other = "223e4567-e89b-42d3-a456-426614174000";
        let sender = pubky_switch_core::enc::b64(&pubky);
        // Times and counts, at their families only; a `missed` item at its key's time.
        assert!(reads::<u64>(&key::peer(&pubky), serde_json::json!(4)));
        assert!(reads::<u64>(key::CURSOR_TIME, serde_json::json!(4)));
        assert!(reads::<u64>(&key::missed(5), serde_json::json!(5)));
        assert!(!reads::<u64>(&key::missed(5), serde_json::json!(7)));
        assert!(!reads::<u64>(key::RECORDS_UNLISTED, serde_json::json!(1)));
        // Marks, set only `true`, at their families only.
        assert!(reads::<bool>(key::PAYER_PASS_OWED, serde_json::json!(true)));
        assert!(!reads::<bool>(
            key::PAYER_PASS_OWED,
            serde_json::json!(false)
        ));
        assert!(!reads::<bool>(&key::peer(&pubky), serde_json::json!(true)));
        // The restore's re-send owed at its key only, its waiting counterparties ascending.
        let owed = |waiting: serde_json::Value| serde_json::json!({ "waiting": waiting });
        assert!(reads::<OwnResendOwed>(
            key::OWN_RESEND_OWED,
            owed(serde_json::Value::Null)
        ));
        let one = vec![1u8; 32];
        let two = vec![2u8; 32];
        assert!(reads::<OwnResendOwed>(
            key::OWN_RESEND_OWED,
            owed(serde_json::json!([one, two]))
        ));
        assert!(!reads::<OwnResendOwed>(
            key::OWN_RESEND_OWED,
            owed(serde_json::json!([two, one]))
        ));
        assert!(!reads::<OwnResendOwed>(
            key::PAYER_PASS_OWED,
            owed(serde_json::Value::Null)
        ));
        // With the restore's time; a mark written before that field reads as none kept.
        let stamped = serde_json::json!({ "waiting": [one], "restored_at_ms": 7 });
        assert!(reads::<OwnResendOwed>(
            key::OWN_RESEND_OWED,
            stamped.clone()
        ));
        let read: OwnResendOwed = serde_json::from_value(stamped).unwrap();
        assert_eq!(read.restored_at_ms, Some(7));
        let older: OwnResendOwed =
            serde_json::from_value(serde_json::json!({ "waiting": [one] })).unwrap();
        assert_eq!(older.restored_at_ms, None);
        // A restore's hold at its key only.
        let hold = serde_json::json!({ "resend_from_ms": 1, "asks_queued": false, "asked": [] });
        assert!(reads::<RestoreHold>(key::RESTORE_HOLD, hold.clone()));
        assert!(!reads::<RestoreHold>(key::CURSOR_TIME, hold));
        // A queued ask of the sender and time its key names.
        let ask =
            |from: u64| serde_json::json!({ "sender": sender, "from_ms": from, "until_ms": null });
        assert!(reads::<QueuedAsk>(&key::ask(&pubky, 5), ask(5)));
        assert!(!reads::<QueuedAsk>(&key::ask(&pubky, 5), ask(6)));
        assert!(!reads::<QueuedAsk>(&key::ask(&[4; 32], 5), ask(5)));
        // A side of the message its key names.
        let side = |msg_id: &str| serde_json::json!({ "msg_id": msg_id, "side": "payer" });
        assert!(reads::<KeptSide>(&key::side(id), side(id)));
        assert!(!reads::<KeptSide>(&key::side(id), side(other)));
        // A held answer of the payee and message its key names.
        let held = |payee: &str, msg_id: &str| serde_json::json!({ "payee": payee, "msg_id": msg_id, "request_hash": sender });
        assert!(reads::<HeldAnswer>(
            &key::held_answer(&pubky, id),
            held(&sender, id)
        ));
        assert!(!reads::<HeldAnswer>(
            &key::held_answer(&pubky, id),
            held(&sender, other)
        ));
        assert!(!reads::<HeldAnswer>(
            &key::held_answer(&pubky, id),
            held(&pubky_switch_core::enc::b64(&[4; 32]), id)
        ));
        assert!(!reads::<HeldAnswer>(&key::side(id), held(&sender, id)));
        // A list ask of the payee and app its key names.
        let asked = |app: Option<[u8; 16]>| {
            serde_json::json!({
                "payee": sender, "app_id": app, "held_version": null, "asked_at_ms": 1,
            })
        };
        assert!(reads::<ListAsk>(&key::list_ask(&pubky, None), asked(None)));
        assert!(!reads::<ListAsk>(
            &key::list_ask(&pubky, None),
            asked(Some([2; 16]))
        ));
        assert!(!reads::<ListAsk>(
            &key::list_ask(&[4; 32], None),
            asked(None)
        ));
        // The `missed` item's consent mark, set only `true`.
        assert!(reads::<bool>(key::MISSED_CONSENTS, serde_json::json!(true)));
        assert!(!reads::<bool>(
            key::MISSED_CONSENTS,
            serde_json::json!(false)
        ));
        // An executor notice's target, of the notice its key names.
        let target = |msg_id: &str| serde_json::json!({ "msg_id": msg_id, "app_id": "AAAAAAAAAAAAAAAAAAAAAA" });
        assert!(reads::<NoticeTarget>(&key::notice_target(id), target(id)));
        assert!(!reads::<NoticeTarget>(
            &key::notice_target(id),
            target(other)
        ));
        assert!(!reads::<NoticeTarget>(&key::side(id), target(id)));
        // An owed crossing end, of the record its key names, with a `msg_id`.
        let owed = |record: [u8; 32], msg_id: &str| serde_json::json!({ "record": pubky_switch_core::enc::b64(&record), "msg_id": msg_id });
        assert!(reads::<OwedEnd>(
            &key::owed_end(&[6; 32]),
            owed([6; 32], id)
        ));
        assert!(!reads::<OwedEnd>(
            &key::owed_end(&[6; 32]),
            owed([7; 32], id)
        ));
        assert!(!reads::<OwedEnd>(
            &key::owed_end(&[6; 32]),
            owed([6; 32], "not-a-msg-id")
        ));
        assert!(!reads::<OwedEnd>(
            &key::consent(&[6; 32]),
            owed([6; 32], id)
        ));
        // A key the SDK doesn't write reads nothing, nor does a certificate that isn't one.
        assert!(!reads::<u64>("zz/1", serde_json::json!(1)));
        assert!(!reads::<SignedDoc>(
            key::LISTED_CERT,
            serde_json::json!({ "x": 1 })
        ));
    }

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
            (key::missed_item(9), StateKey::MissedItem { from_ms: 9 }),
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
            (key::OWN_RESEND_OWED.into(), StateKey::OwnResendOwed),
            (key::LISTED_CERT.into(), StateKey::ListedCert),
            (key::hold_confirmed(&pubky), StateKey::HoldConfirmed(pubky)),
            (key::LISTS_READ.into(), StateKey::ListsRead),
            (key::DENIED.into(), StateKey::Denied),
            (key::own_block(&pubky), StateKey::OwnBlock(pubky)),
            (
                format!("{}{}-7", key::TAKEN, "05".repeat(16)),
                StateKey::Taken(format!("{}-7", "05".repeat(16))),
            ),
            (key::held_list(&pubky), StateKey::HeldList(pubky)),
            (key::side(id), StateKey::Side(id.into())),
            (
                key::held_answer(&pubky, id),
                StateKey::HeldAnswer {
                    payee: pubky,
                    msg_id: id.into(),
                },
            ),
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
            (key::key_waits(&pubky), StateKey::KeyWaits { sender: pubky }),
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
            (key::divergent(&pubky), StateKey::Divergent(pubky)),
            (
                key::divergent_write(&pubky, crate::divergent::WriteRole::List, &pubky, id),
                StateKey::DivergentWrite {
                    record: pubky,
                    role: crate::divergent::WriteRole::List,
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
            (
                key::allowance_end(&pubky, id),
                StateKey::AllowanceEnd {
                    record: pubky,
                    msg_id: id.into(),
                },
            ),
            (key::consent(&pubky), StateKey::Consent(pubky)),
            (key::conflict(&pubky), StateKey::Conflict(pubky)),
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
            "mi/1",
            "j/",
            "ctx",
            "side/123E4567-E89B-42D3-A456-426614174000",
            "lq/AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM/not-an-app",
            "lq/",
            "nt/BQUFBQUFBQUFBQUFBQUFBQ/paid",
            "dt/05-7",
            "nt/BQUFBQUFBQUFBQUFBQUFBQ",
            "dv/not-base64",
            "dw/AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM/cancellation/123e4567-e89b-42d3-a456-426614174000",
            "dw/AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM/proof",
            "dw/AwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwM",
            "ae/not-base64",
            "ac/",
            "cf/not-base64",
            "en/not-a-uuid",
        ] {
            assert_eq!(StateKey::parse(key.as_bytes()), None, "{key}");
        }
    }
}
