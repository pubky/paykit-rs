//! Paykit's own local state, in the protocol key space (`p/`) of the client's namespace
//! (`STATE.md` of `pubky-switch-client`).
//!
//! Paykit never writes the host store itself: every write is a client
//! [`LocalWrite`] or a kept native batch ID, committed by the client so it lands with what it
//! belongs to:
//!
//! - with a received message's effect, its processed pair and its cursor move
//!   ([`pubky_switch_client::app::ReceiveContext::local`]);
//! - with the first keep of a batch, for its protocol-local ID ([`pubky_switch_client::app::OpBatch::keep_id_at`]);
//! - with the answer of a batch of operations, never if the batch is refused
//!   ([`pubky_switch_client::app::OpBatch::local`]);
//! - on its own, in one store commit ([`pubky_switch_client::app::App::protocol_commit`]).
//!
//! Reads go through the client too: [`pubky_switch_client::app::App::protocol_get`],
//! [`pubky_switch_client::app::App::protocol_scan`], the receive context's and, in the
//! protocol's other callbacks, a [`pubky_switch_client::app::LocalView`]. Only this module
//! makes them, and it binds every value it reads to its key before any caller sees it
//! ([`Stored`]), at a restore as at runtime.
//! Paykit values are `{"v":1,"d":<record>}`. An outstanding list publication holds the
//! client's raw 16-byte batch ID, or the distinct IDs retained together by a restore.
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
//! | `off/<attempt_id>` | the original batch ID for an unresolved offline recording, committed with its signed operations before first submission (Paykit spec 8.4) | yes |
//! | `j/<attempt_id>` | the payment journal: one payment, from before its `attempt` | yes |
//! | `side/<msg_id>` | the side a cancellation is sent from, kept with the batch that queues it | |
//! | `ex/<msg_id>` | why an acceptance this app sent may go while sends to its payee are held: a payment dispatched, or the user's confirmation of that blocked subscription, bound to its payee and request; kept before or with the batch that queues it (`send_holds.rs`) | |
//! | `qs/<msg_id>` | a rejection or payer's cancellation this app queued: its payee, request, signed hash and `created_at`, written in the commit that queues it, so the receive step ends its outbox copy past 30 days, telling the host as its request record decides, with that decision; gone once neither the outbox nor a report of its end the host hasn't acknowledged names it (`send_holds.rs`); restored whole, since a restored outbox can hold the copy and a restored client an unacknowledged report of its end | |
//! | `ar/<payee>/<app_id>` | a payee app's records that took part in its comparison, for its public endpoints | |
//! | `ct` | the acceptance time of the last Paykit entry the receive step passed | |
//! | `rh` | the cursor verdict, earlier holds and block windows for restored requests (`hold.rs`) | |
//! | `hc/<record key>/<hold identifier>` | the user confirmed a held request under that hold (`he`) | |
//! | `hb/<record key>/<hold identifier>` | the restore under that hold carried the backup's confirmation of an earlier hold | |
//! | `rsc/<record key>` | an Allowance whose consent this restore marked for confirmation, until the held cursor settles it | |
//! | `xr/<record key>` | an approval for execution a restore carried, which counts once its verdict is clean | |
//! | `xa/<record key>/<hold identifier>` | the user approved automatic execution of an Allowance under that hold; only the current hold's approval lets a first grant go without a new confirmation | |
//! | `pl` | the latest provider-loss boundary followed, retained across restore (`loss.rs`, Paykit spec 7.3) | |
//! | `rl` | the newest provider loss this app recorded, by the generation recovered at: written with `pl`, never lowered, and kept from the installation over an in-place restore; the shared post-loss hold stands while it is set (`loss.rs`, Paykit spec 8.1) | |
//! | `pn/<obligation>` | the user states this obligation was not paid before the kept loss; a newer loss clears these statements | |
//! | `mh` | the hold a `missed` item set on the open requests, bounded by a time: the item's last acceptance time, or, for an item whose end isn't known, a service time verified after the read that handed it (`hold.rs`) | |
//! | `mu` | set by a `missed` item whose end isn't known: every open request is held, as after a restore, until a read that completes bounds the hold by the service time verified then, into `mh`, and clears it (`hold.rs`) | |
//! | `ml` | set by a `missed` item: the shared records aren't listed by Paykit since, so a record's absence means nothing yet; cleared by Paykit's next listing (`hold.rs`); never restored | |
//! | `lp/<recipient>/<generation>/<message hash>` | outstanding original publication batch IDs, kept atomically before submission and retained across restore | yes |
//! | `lres/<recipient>` | the applied list, its publisher and original signed message, its first delivery outcome (open, undelivered or unknown) and any kept confirmation of another version, prior offers' latest possible expiry, and the interval of an outstanding withdrawal | yes |
//! | `lh/<record key>` | a Private Payment List held because its `payment_lists` record doesn't read: never written over it, which could drop its single-use marks, until the record reads again with them, or reads absent and was never seen lost (`lgone/`); deleted once the record holds the list | |
//! | `lgone/<record key>` | set in the commit in which this app sees the `payment_lists` record deleted, or holds a list over it while it doesn't read: a later absence of it, as after a forgotten delete or a restore, keeps a list held there; never cleared, and a restore keeps the installation's own as well as the backup's | yes |
//! | `kc/<record key>/<certificate hash>/<feed generation>` | a payee's cancellation of a request, kept until it is placed in the payer's request record | |
//! | `peer/<pubky>` | a counterparty with Paykit activity, and when | |
//! | `sv` | the highest service time the app verified, as the receive step and a flush last saw it, for the delivery hook's window of a held message, which judges by it, by the device clock only while none is kept; written also before every delivery Paykit starts. The client's own window for a message it queues again from signed plaintext, which ends a held answer released from its record, takes the later of the device clock and the service time instead, so under a fast device clock such an answer can be given up while a queued stop still goes (`send_holds.rs`, Paykit spec 8.1); never restored, a restore's first read writing it again | |
//! | `lr` | when the app last read the lists, on the device clock; absent before the first read and after a due read failed, when new activity waits (Paykit wire 4.1); never restored, so a restored app reads the deny list again before any payment | |
//! | `spam/<pubky>` | how many messages from a blocked or flagged sender only were counted (Paykit wire 4.1) | |
//! | `rs/<pubky>/<msg_id>` | when a message was last re-sent to a requester | |
//! | `rsd/<pubky>/<day>` | how many messages were re-sent to a requester that day | |
//! | `nl` | set by a restore: the shared records aren't listed since, so the client's copy is empty and a record's absence means nothing yet; cleared once they are listed; never restored | |
//! | `lc` | the certificate the shared records were last listed under, signed: a certificate held now other than it means the copy may lack records until Paykit lists them, since one held between may have read less (`hold.rs`); written only by a Paykit listing, in its commit; absent from a join or a restore until that listing, which leaves the records unlisted; never restored | |
//! | `ro` | records to check after a restore for possibly delivered messages, with the time limit for unreadable records and any counterparties waiting for their period's request | |
//! | `ha/<payee>/<msg_id>` | the wallet's held answer and request. A backup marks it restored so it cannot authorize delivery | |
//! | `pp` | set by a restore: a payer pass is owed, so no new attempt is made and nothing is dispatched; cleared only after a reconcile under `paykit_pay` ran that pass (`hold.rs`); never restored | |
//! | `lq/<payee>[/<app_id>]` | a `private_payment_list_request` asked of a payee's app, or of none: the version it named and when (Paykit spec 8.4) | |
//! | `nt/outgoing/<recipient>/<msg_id>/<hash>` | the disposition of an exact original delivery is unknown: after restore, or a counterparty's current list given up with an unknown outcome. Acknowledgment is retained for repeated imports; a list's notice goes once a newer version supersedes it | |
//! | `nt/<attempt_id>/<kind>`, `nt/<sender>/<msg_id>`, `nt/<tag>` | a notice for the host about a payment, a message given up, or the identity as a whole, until acknowledged (`notices.rs`); an identity-wide one is never restored and is told again at each load while what it tells stands | |
//! | `xm/<sender>/<msg_id>` | an extension message from `sender` of a kind the host registered, kept from its receive step until the host takes it (`extensions.rs`, Paykit spec 3.3); at most 64 per sender and 128 in all | yes |
//! | `dl` | the deny list as the app last read it, with this app's blocks kept unanswered (`bk/`), for the views and a payment the user starts (Paykit spec 8.1) | |
//! | `fg` | the flagged senders as the app last read them, at most `MAX_FLAGGED_COPY`; absent while not known, which holds the asks of block marks (`marks.rs`); never restored | |
//! | `bf` | the raise of the identity's block floor this app owes: the latest of the latest times of the block marks gone for the bound or their own expiry, and of a restore's first read of the lists; written to the shared `block_floor` record before the block marks ask and with any ask, and deleted in that batch; applied as a floor meanwhile (`crate::floor`) | yes |
//! | `fk` | what this app keeps of the identity's block floor: the highest floor it has seen and whether it has seen the record divergent, the durable divergence hold; both only grow, and an in-place restore keeps the higher of the backup's and the installation's (`crate::floor::FloorKept`, Paykit wire 5.6) | yes |
//! | `rr` | the block floor raises this app owes, tried and left unwritten for a cause other than the divergence hold: how many such tries in a row, the last one's time on this device's clock, the time of each raise owed then, and the floor record and whether the floor read as it was then; the wait is derived from the tries. Only a hint: one that doesn't read is taken as none (`crate::floor::RaiseRetry`, Paykit wire 5.6, Raising); never restored | |
//! | `fo/<record key>` | a request record a read opened without `manual_only`, checked against the block floor once the read is done and marked if dated before it (`crate::floor`); never restored | |
//! | `bk/<pubky>` | this app's block or unblock of `pubky` not yet shown by the list (`OwnBlockMark`): a block from before the switch is asked, an unblock only with the batch the client keeps; a block counts until a read of the list shows it, and is written again at a receive while the client neither keeps nor applied its batch, a restored one too, since the kept batch stays with the device that kept it; an unblock counts for payments and held sends only while the client still keeps its batch, never for the spam rule, and isn't restored (Paykit spec 8.1) | |
//! | `ra/<sender>` | signed recovery requests for dropped replies, their first-delivery outcomes and any unresolved original missing from a restore | |
//! | `pd/<sender>` | a mark of a sender's replies dropped for the bounds on kept replies: the earliest and the latest acceptance among them, and when it was last asked to send them again (`marks.rs`); at most `MAX_DROPPED_MARKS` in each family of marks (`pd/`, `ud/`, `kw/`, `bd/`), and a restore refuses a backup holding more | |
//! | `bd/<sender>` | a mark of a sender's messages dropped for the user's block, each as one never processed (its pair forgotten), as `pd/` holds; it asks that sender once it is neither blocked nor flagged (`marks.rs`, Paykit spec 8.1) | |
//! | `bl/<sender>/<msg_id>` | a notice of lost messages owed on a block's mark the receive step dropped for the bound while it named a request as queued: the mark's window, when the notice was raised, and the request's `msg_id` and hash; settled in the pass after the read from the outbox's answer on that request, told or dropped (`marks.rs`, `Paykit::settle_lost_owed`); at most `MAX_LOST_OWED`, the receive step holding the next entry that would owe one more; restored, and a restore refuses a backup holding more | |
//! | `bw/<hold epoch>` | a widening of the block marks, over the missed acceptance times, owed to every counterparty the deny list holds at the next read of the lists: written by a `missed` item (`marks.rs`) | |
//! | `ud/<sender>` | a mark of a sender's entries dropped unverified, as `pd/` holds (`marks.rs`) | |
//! | `kw/<sender>` | a mark of a sender's entries the client dropped while they waited for a scope key, as `pd/` holds, with the data epochs they waited for; it asks only while the app holds every key of those epochs (`marks.rs`) | |
//! | `pr/<sender>/<record key>/<msg_id>` | a payer's reply, or a payee's period details, for a request the app holds no record of yet, or whose record it holds but can't open, read or bind, kept until it does, placed only once the record names its sender, a proof of a payment without a request for the payee's record of its settlement that the app holds but can't read, placed once it reads naming its sender and removed if it names another, or a receipt for a payer record the app holds but can't read and that isn't paused as divergent, kept until it reads, and moved to `dw/` if it reads paused (`receive.rs`), or a counterparty's Allowance answer the receive step took, which it never writes into the record's response itself: a rejection placed after the read, or kept before the app holds its Allowance record, or waiting over a record seen divergent for the user's confirmation; an acceptance kept unplaced if it would fill the record's empty `response` (Paykit spec 8.3.1; `allowances.rs`); each with the feed `seq` its entry was read at, which orders it (`records::KeptOrder`) | |
//! | `dt/<event ID>` | a duty event the SDK has taken (`stored.rs`), until it is acknowledged; never restored: a restore's events are taken again, which changes nothing, since a dropped entry's time is already in its sender's mark | |
//! | `dv/<record key>` | the pause mark of a record the app has seen divergent: paused until the user confirms, or the write the user confirmed over, with the write a credit's refusal showed it at and the write a credit's confirmation was given over, which is always the write that showing names, at or before the plain confirmation: any other stored state reads as a corrupt store (`divergent.rs`); a restore installs it paused, neither shown nor confirmed for a credit | |
//! | `dw/<record key>/<role>/<sender>/<msg_id>` | a received message, by its sender and `msg_id`, whose write over a divergent record waits for the user's confirmation, or for the messages kept over the record before it, to write into its record later, in the order its inbox accepted them (by acceptance time, then the feed `seq` its entry was read at, as the kept replies are, never by `msg_id`), a receipt over a payer record that doesn't read included, written once the record also reads, and one kept in `pr/` whose record read paused; never one from anyone but the counterparty of a record that reads; given up at its 30 days, also one that no longer points to its record (`divergent.rs`) | |
//! | `se/<sender>/<msg_id>` | the message of an entry the client stored for good, with the entry's ID and message hash and the feed `seq` it was read at, to write into its record later (`stored.rs`); never restored, since the client tells of the entries it restores again | |
//! | `ae/<record key>/<msg_id>` | a counterparty's `allowance_end`, kept as the stop until the Allowance record holds an end or a rejection; every distinct end is kept, each under its `msg_id` and with the feed `seq` that orders it, until the record shows which one names what it holds (`allowances.rs`, Paykit spec 8.3) | |
//! | `lv/<record key>` | the payer's pending or verified executor grant and its later accepted ledger | |
//! | `lw/<record key>/<proposal hash>` | signed ledger predecessor and exact proposed value, staged before submission | |
//! | `lb/<record key>/<proposal hash>` | native batch IDs committed before a ledger write leaves the wallet, retained across restore until resolved | |
//! | `lx/<ledger key>/<application hash>` | the original applied writes that account for compacted spending and the opening for a reservation batch | |
//! | `ac/<record key>` | the consent list: an Allowance this app may pay under, or removed from it for good (`allowances.rs`, Paykit spec 8.3); a restore marks every entry to confirm again | |
//! | `ax/<record key>` | one signed input whose conflict check outlived placement, its pending record version when applicable, and whether further candidates remain unresolved (Paykit spec 8.1, 8.3.1) | |
//! | `cf/<record key>` | a second, different proposal or response of an Allowance, with the feed `seq` its entry was read at, kept as the stop until the Allowance record holds `conflicted` (`allowances.rs`, Paykit spec 8.3) | |
//! | `mc` | set by a `missed` item: every entry of the consent list is to be confirmed again, as after a restore; nothing is consented while it is set, and the next receive marks each entry so and clears it (`allowances.rs`) | |
//! | `en/<msg_id>` | the executor app an `executor_notice` is wrapped to, kept with the batch that queues it | |
//! | `eo/<record key>` | this app's own crossing end of an Allowance, recorded while sends to its counterparty were held, whose message is sent from the record once they aren't (`allowances.rs`, `Paykit::send_owed_ends`); never restored | |

//! | `eb/<msg_id>` | native batch ID for a crossing end waiting for its record application before queueing; removed with the applied result or terminal retirement | |
//! | `end/<msg_id>` | origin of the exact signed Allowance end and its target and references, written before submission and retained through its delivery lifetime | |

use paykit_lib::bodies::ResendRequest;
use paykit_lib::derive::natural_key;
use paykit_lib::scopes::verify_certificate;
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{LocalView, LocalWrite, ReceiveContext, RecordTarget};
use pubky_switch_core::ident::record_key_from_blinded;
use pubky_switch_core::records::AppCert;
use pubky_switch_core::sig::SignedDoc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::activity::{ActivityTarget, KeptSide};
use crate::allowances::{allowance_target, ConsentEntry, EndOrigin, NoticeTarget, OwedEnd};
use crate::app_records::SeenRecords;
use crate::error::{PaykitError, Result};
use crate::hold::HeldAnswer;
use crate::hold::MissedHold;
use crate::hold::OwnResendOwed;
use crate::hold::RestoreHold;
use crate::journal::JournalEntry;
use crate::loss::LossBoundary;
use crate::marks::{DropMark, KeyWaitMark};
use crate::notices::KeptNotice;
use crate::notices::SettlementSubject;
use crate::pay::ListAsk;
use crate::paykit::Paykit;
use crate::protocol::{MissedItemMark, QueuedAsk};
use crate::receive::{Fail, QueuedAnswer};
use crate::records::{list_target, KeptMessage};
use crate::stored::{BoundNote, StoredNote};

/// The schema version of Paykit's local records.
pub(crate) const LOCAL_VERSION: u32 = 1;

#[derive(Serialize)]
struct Out<'a, T: Serialize> {
    v: u32,
    d: &'a T,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub(crate) fn binding<R>(&self, read: impl FnOnce(&Binder<'_>) -> R) -> R {
        let blind = |input: &[u8]| self.app.blind(input);
        read(&Binder::new(self.identity(), self.app.cert(), &blind))
    }

    /// The value at protocol key `key`, read whole and bound to it ([`Stored::bind`]): `Err`
    /// for a store failure, and a corrupt store for a value that doesn't read, isn't its
    /// family's, or names another key than `key`. Never `None` for one that doesn't bind.
    pub(crate) async fn local_get<T: Stored>(&self, key: &str) -> Result<Option<T>> {
        match self
            .app
            .protocol_get(crate::protocol::BACKUP_NAME, key.as_bytes())
            .await?
        {
            None => Ok(None),
            Some(bytes) => self
                .binding(|binder| bound(key.as_bytes(), &bytes, binder))
                .map(Some),
        }
    }

    /// Every value under protocol prefix `prefix`, with the key's rest after the prefix, each
    /// read whole and bound to its own key as [`Paykit::local_get`] reads one.
    pub(crate) async fn local_scan<T: BoundRead>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        let held = self
            .app
            .protocol_scan(crate::protocol::BACKUP_NAME, prefix.as_bytes())
            .await?;
        self.binding(|binder| {
            let mut out = Vec::with_capacity(held.len());
            for (key, value) in &held {
                let rest = rest_of(key, prefix.as_bytes()).ok_or_else(PaykitError::corrupt)?;
                out.push((rest, bound(key, value, binder)?));
            }
            Ok(out)
        })
    }

    pub(crate) async fn publication_batches(
        &self,
        recipient: &[u8; 32],
    ) -> Result<Vec<(PublicationKey, PublicationIds)>> {
        let entries = self
            .app
            .protocol_scan(
                crate::protocol::BACKUP_NAME,
                key::publication_prefix(recipient).as_bytes(),
            )
            .await?;
        entries
            .into_iter()
            .map(|(key, bytes)| publication_entry(&key, &bytes, &self.identity()))
            .collect()
    }

    pub(crate) async fn retain_publication_ids(
        &mut self,
        key: &PublicationKey,
        ids: &PublicationIds,
    ) -> Result<()> {
        let write = key.write(ids)?;
        self.app
            .protocol_commit(crate::protocol::BACKUP_NAME, &[write])
            .await?;
        Ok(())
    }

    /// Commit `tx` on its own, in one store commit.
    pub(crate) async fn local_commit(&mut self, tx: Tx) -> Result<()> {
        if tx.writes.is_empty() {
            return Ok(());
        }
        Ok(self
            .app
            .protocol_commit(crate::protocol::BACKUP_NAME, &tx.writes)
            .await?)
    }

    /// Put one record and commit it.
    pub(crate) async fn local_put(&mut self, key: &str, value: &impl Serialize) -> Result<()> {
        let mut tx = Tx::default();
        tx.put(key, value)?;
        self.local_commit(tx).await
    }

    /// Delete one record and commit it.
    /// Whether a value is kept at `key`, unread: for one that only paces work and is
    /// deleted whether or not it reads.
    pub(crate) async fn local_holds(&self, key: &str) -> Result<bool> {
        Ok(self
            .app
            .protocol_get(crate::protocol::BACKUP_NAME, key.as_bytes())
            .await?
            .is_some())
    }

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
    app_id: Option<[u8; 16]>,
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
            app_id: Some(cert.app_id()),
            cert: Some(cert),
            blind: Some(blind),
        }
    }

    /// The binding of a reader without the certificate or the naming key, a [`LocalView`].
    pub(crate) fn unblinded(own: [u8; 32]) -> Self {
        Self {
            own,
            app_id: None,
            cert: None,
            blind: None,
        }
    }

    fn view(view: &LocalView<'_>) -> Self {
        Self {
            app_id: Some(view.app_id()),
            ..Self::unblinded(view.identity())
        }
    }

    pub(crate) fn app_id(&self) -> Option<[u8; 16]> {
        self.app_id
    }

    pub(crate) fn identity(&self) -> &[u8; 32] {
        &self.own
    }

    fn blind(&self) -> Result<Blind<'a>> {
        self.blind.ok_or_else(PaykitError::corrupt)
    }

    /// The record key of `target`, as the client derives it (Switch spec 13.3).
    pub(crate) fn record_key(&self, target: &RecordTarget) -> Result<[u8; 32]> {
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
    /// Decode the bytes written by the owner of this state family.
    fn read_bytes(bytes: &[u8], _: &Binder<'_>) -> Result<Self> {
        decode(bytes)
    }

    /// Bind the value read at `key`, of family `family`, to it: a corrupt store for a value
    /// of another family, or one naming anything other than what its key names.
    fn bind(&self, key: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()>;
}

/// A typed read of a state family at its physical key.
pub(crate) trait BoundRead: sealed::Sealed + Sized {
    fn read_at(key: &str, family: &StateKey, bytes: &[u8], binder: &Binder<'_>) -> Result<Self>;
}

impl<T: Stored> BoundRead for T {
    fn read_at(key: &str, family: &StateKey, bytes: &[u8], binder: &Binder<'_>) -> Result<Self> {
        let value = T::read_bytes(bytes, binder)?;
        value.bind(key, family, binder)?;
        Ok(value)
    }
}

/// Read and bind a value using the physical key returned by the store.
pub(crate) fn bound<T: BoundRead>(key: &[u8], bytes: &[u8], binder: &Binder<'_>) -> Result<T> {
    let text = std::str::from_utf8(key).map_err(|_| PaykitError::corrupt())?;
    let family = StateKey::parse(key).ok_or_else(PaykitError::corrupt)?;
    T::read_at(text, &family, bytes, binder)
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
pub(crate) fn cx_scan<T: BoundRead>(
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

/// Read the owning protocol's exact value with the delivery's current certificate and naming key.
pub(crate) fn delivery_get<T: Stored>(
    delivery: &pubky_switch_client::app::DeliveryView<'_>,
    key: &str,
) -> Result<Option<T>> {
    let Some(bytes) = delivery.local().local_get(key.as_bytes())? else {
        return Ok(None);
    };
    let blind = |input: &[u8]| delivery.blind(input);
    let binder = Binder::new(delivery.local().identity(), delivery.cert(), &blind);
    bound(key.as_bytes(), &bytes, &binder).map(Some)
}

/// The value at `key`, read through a [`LocalView`] and bound to it.
pub(crate) fn view_get<T: Stored>(view: &LocalView<'_>, key: &str) -> Result<Option<T>> {
    let Some(bytes) = view.local_get(key.as_bytes())? else {
        return Ok(None);
    };
    bound(key.as_bytes(), &bytes, &Binder::view(view)).map(Some)
}

/// Every value under `prefix`, read through a [`LocalView`], with the key's rest after the
/// prefix, each bound to its own key.
pub(crate) fn view_scan<T: Stored>(view: &LocalView<'_>, prefix: &str) -> Result<Vec<(String, T)>> {
    bound_view_scan(view, prefix, &Binder::view(view))
}

/// Read the same protocol namespace with the key and certificate available to its caller.
pub(crate) fn bound_view_scan<T: BoundRead>(
    view: &LocalView<'_>,
    prefix: &str,
    binder: &Binder<'_>,
) -> Result<Vec<(String, T)>> {
    let mut out = Vec::new();
    for (key, value) in view.local_scan(prefix.as_bytes())? {
        let rest = rest_of(&key, prefix.as_bytes()).ok_or_else(PaykitError::corrupt)?;
        out.push((rest, bound(&key, &value, binder)?));
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

impl sealed::Sealed for [u8; 16] {}

pub(crate) type LedgerBatchIds = Vec<[u8; 16]>;

/// The native batch ID, or distinct IDs retained together by a restore.
fn batch_ids(bytes: &[u8]) -> Result<&[[u8; 16]]> {
    let (ids, tail) = bytes.as_chunks::<16>();
    if ids.is_empty() || !tail.is_empty() || ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(PaykitError::corrupt());
    }
    Ok(ids)
}

impl sealed::Sealed for LedgerBatchIds {}

impl Stored for LedgerBatchIds {
    fn read_bytes(bytes: &[u8], _: &Binder<'_>) -> Result<Self> {
        Ok(batch_ids(bytes)?.to_vec())
    }

    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        if matches!(family, StateKey::LedgerBatch { .. }) {
            Ok(())
        } else {
            corrupt()
        }
    }
}

/// The original batch ID is written by the Switch client as exactly sixteen bytes.
impl Stored for [u8; 16] {
    fn read_bytes(bytes: &[u8], _: &Binder<'_>) -> Result<Self> {
        bytes.try_into().map_err(|_| PaykitError::corrupt())
    }

    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::OfflineBatch(_)
            | StateKey::EndBatch(_)
            | StateKey::FloorBatch
            | StateKey::GrantBatch(_) => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::floor::FloorKept {}

impl Stored for crate::floor::FloorKept {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        // No writer keeps an empty one.
        if matches!(family, StateKey::FloorKept) && !self.is_empty() {
            Ok(())
        } else {
            corrupt()
        }
    }
}

impl sealed::Sealed for crate::floor::RaiseRetry {}

impl Stored for crate::floor::RaiseRetry {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        if matches!(family, StateKey::RaiseRetry) && self.reads() {
            Ok(())
        } else {
            corrupt()
        }
    }
}

impl sealed::Sealed for crate::floor::PendingUnblocks {}

impl Stored for crate::floor::PendingUnblocks {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        if matches!(family, StateKey::BlockFloor)
            && !self.senders.is_empty()
            && self.senders.windows(2).all(|pair| pair[0] < pair[1])
        {
            Ok(())
        } else {
            corrupt()
        }
    }
}

impl sealed::Sealed for crate::receipt_delivery::ReceiptBatch {}

impl Stored for crate::receipt_delivery::ReceiptBatch {
    fn read_bytes(bytes: &[u8], _: &Binder<'_>) -> Result<Self> {
        match bytes.try_into() {
            Ok(id) => Ok(Self::Awaiting(id)),
            Err(_) => Ok(Self::Applied(decode(bytes)?)),
        }
    }

    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::ReceiptBatch { record, .. } = family else {
            return corrupt();
        };
        if let Self::Applied(applied) = self {
            let cert = binder.cert.ok_or_else(PaykitError::corrupt)?;
            if applied
                .original(binder.identity(), &cert.app_id(), record)
                .is_none()
            {
                return corrupt();
            }
        }
        Ok(())
    }
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
            | StateKey::ListsRead
            | StateKey::ServiceTime => Ok(()),
            // A generation recovered at follows another.
            StateKey::LossRecorded if *self > 0 => Ok(()),
            _ => corrupt(),
        }
    }
}

/// This app's block or unblock of the identity its key names (`bk/<pubky>`), not yet shown
/// by a read of the lists (Paykit spec 8.1, "a block or unblock the app wrote counts at
/// once", for payment decisions). The spam rule counts only its block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnBlockMark {
    /// A block (`true`) or an unblock.
    pub(crate) blocked: bool,
    /// The client's kept batch it rides on: `None` only for a block, not yet answered, or
    /// answered with no batch kept.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) batch: Option<[u8; 16]>,
}

impl OwnBlockMark {
    /// A block (`true`) or an unblock.
    pub(crate) fn blocked(&self) -> bool {
        self.blocked
    }

    /// The batch the client keeps it in, if it named one.
    pub(crate) fn batch(&self) -> Option<[u8; 16]> {
        self.batch
    }

    /// The batch an unblock rides on, the one the client must still keep for it to count.
    pub(crate) fn unblock_batch(&self) -> Option<[u8; 16]> {
        if self.blocked {
            None
        } else {
            self.batch
        }
    }

    /// What the mark counts as now for payments and held sends, given whether the client
    /// still keeps the batch of an unblock (`batch_kept`), as the client answers it:
    /// `Some(true)` for a block, which always counts, failing closed; `Some(false)` for an
    /// unblock whose batch the client keeps; `None` for an unblock that counts no more: its
    /// batch dropped or ended, so the list decides. The spam rule asks only [`OwnBlockMark::blocked`]: an unblock never
    /// overrides the client's copy of the lists there.
    pub(crate) fn counts(&self, batch_kept: bool) -> Option<bool> {
        if self.blocked() {
            return Some(true);
        }
        (self.unblock_batch().is_some() && batch_kept).then_some(false)
    }
}

impl sealed::Sealed for crate::extensions::ExtensionMessage {}

/// An extension message kept for the host (`xm/<sender>/<msg_id>`): from the sender and of
/// the `msg_id` its key names, of an extension kind (Paykit wire 4.3).
impl Stored for crate::extensions::ExtensionMessage {
    fn read_bytes(bytes: &[u8], binder: &Binder<'_>) -> Result<Self> {
        decode::<crate::extensions::KeptExtension>(bytes)?.open(binder.identity())
    }

    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::Extension { sender, msg_id }
                if *sender == self.sender && msg_id == &self.msg_id =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
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
                Self {
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
            StateKey::ListDiscovery
            | StateKey::RecordsUnlisted
            | StateKey::MissedUnlisted
            | StateKey::MissedEndUnknown
            | StateKey::MissedConsents
            | StateKey::PayerPassOwed
            | StateKey::HoldConfirmed { .. }
            | StateKey::HoldConfirmedBefore { .. }
            | StateKey::RestoreConsent(_)
            | StateKey::ExecutionApproval { .. }
            | StateKey::RestoreApproval(_)
            | StateKey::NotPaidBeforeLoss(_)
            | StateKey::FloorOpened(_)
            | StateKey::ListRecordLost(_)
            | StateKey::Taken(_)
                if *self =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::hold::HoldEpoch {}

impl Stored for crate::hold::HoldEpoch {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::HoldEpoch => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for RestoreHold {}

impl Stored for RestoreHold {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::RestoreHold if self.is_valid() => Ok(()),
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

impl sealed::Sealed for crate::send_holds::QueuedStop {}

/// A stop this app queued: of the message its key names.
impl Stored for crate::send_holds::QueuedStop {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::QueuedStop(msg_id) if self.msg_id == *msg_id => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::send_holds::SendExemption {}

/// Why an acceptance may go while sends are held: of the message its key names.
impl Stored for crate::send_holds::SendExemption {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::SendExemption(msg_id) if self.msg_id == *msg_id => Ok(()),
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

impl sealed::Sealed for crate::publish::history::PublicationHistory {}
impl Stored for crate::publish::history::PublicationHistory {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::PublicationHistory(key) => {
                let cert = binder.cert.ok_or_else(PaykitError::corrupt)?;
                self.check_binding(key, binder, cert.app_id())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::publish::ListSettings {}
impl Stored for crate::publish::ListSettings {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        if !matches!(family, StateKey::ListSettings) {
            return corrupt();
        }
        self.check()
    }
}

impl sealed::Sealed for crate::publish::ListRecipient {}
impl Stored for crate::publish::ListRecipient {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::ListRecipient(id) if self.recipient == *id && id != binder.identity() => {
                self.check()
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::publish::ListReservation {}
impl Stored for crate::publish::ListReservation {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::ListReservation(id)
                if self.recipient == *id
                    && id != binder.identity()
                    && binder.app_id().is_none_or(|app| self.publisher == app) =>
            {
                self.check()
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for ListAsk {}

/// A list request asked: of the payee, and the app or none, its key names.
/// A held version names the app whose list was held (Paykit spec 8.4).
impl Stored for ListAsk {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::ListAsk { payee, app_id }
                if self.payee == *payee
                    && self.app_id == *app_id
                    && (self.held_version.is_none() || self.app_id.is_some()) =>
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
/// - At `kc/<record key>/<certificate hash>/<feed generation>`, a `paykit.payment_request_cancellation` its inbox accepted, kept
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
/// - At `pr/<sender>/<record key>/<msg_id>` too, an Allowance's acceptance or rejection the
///   receive step took, from that sender and of that `msg_id`, kept under the record key of
///   the Allowance record it names, with its feed `seq`. The app tries to place the answer
///   after the read, once it holds the record. It keeps an acceptance unplaced if it would
///   fill the record's empty `response` (Paykit spec 8.1, 8.3.1).
/// - At `ae/<record key>/<msg_id>`, a counterparty's `paykit.allowance_end` its inbox
///   accepted, of that `msg_id`, with its feed `seq`, kept as the stop under the record key of
///   the Allowance record of its sender and the Allowance it names (Paykit spec 8.3, "Ends
///   reach the executor").
/// - At `cf/<record key>`, a second, different proposal, acceptance or rejection its inbox
///   accepted, with its acceptance time and its feed `seq`, kept as the stop under the
///   record key of the Allowance record of its sender and the Allowance it names (Paykit
///   spec 8.3, "Conflicted").
impl Stored for KeptMessage {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        // A member of a bounded pool, the kept replies or the kept stops, is ordered by when
        // its inbox accepted it, by the feed `seq` its entry was read at within one
        // millisecond (`records::KeptOrder`): which one the bounds drop, which of an
        // Allowance's ends stops it and which of its answers is the record's response. A
        // conflict's stop is ordered the same way among its record's answers: an answer
        // accepted after it never takes the response. One without its acceptance time or its
        // `seq` can't be ordered, whatever its kind, so this is checked first, before any
        // kind's own check can take the value; a held list is ordered by nothing, and no
        // writer gives it a `seq`.
        let pooled = matches!(
            family,
            StateKey::KeptCancellation { .. }
                | StateKey::KeptReply { .. }
                | StateKey::AllowanceEnd { .. }
                | StateKey::Conflict(_)
        );
        if pooled {
            self.order()?;
        } else if self.seq().is_some() {
            return corrupt();
        }
        if (self.pending_create.is_some() || self.ended_acceptance.is_some())
            && !matches!(family, StateKey::AllowanceEnd { .. })
        {
            return corrupt();
        }
        let message = self.stored(&binder.own)?;
        if (matches!(family, StateKey::KeptReply { .. })
            || (matches!(family, StateKey::Conflict(_))
                && crate::allowances::is_allowance_response(message.body())))
            && self.position().is_none()
        {
            return corrupt();
        }
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
            (
                StateKey::KeptCancellation {
                    record: record_key,
                    cert,
                    generation,
                },
                Body::PaymentRequestCancellation(_),
            ) if self.received_at()?.1.generation == *generation
                && message.cert().hash() == *cert =>
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
                Body::AllowanceProposal(proposal),
            ) if message.sender() == *sender
                && message.message().msg_id == *msg_id
                && message.accepted_at().is_some() =>
            {
                (allowance_target(sender, &proposal.allowance_id), record)
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
        Ok(())
    }
}

impl sealed::Sealed for crate::allowances::ConflictEvidence {}

impl Stored for crate::allowances::ConflictEvidence {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::AllowanceEvidence(record) = family else {
            return corrupt();
        };
        self.bind(record, binder)
    }
}

impl sealed::Sealed for crate::allowances::PendingConflict {}

impl Stored for crate::allowances::PendingConflict {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::PendingConflict(key) = family else {
            return corrupt();
        };
        self.bind(key, binder)
    }
}

impl sealed::Sealed for crate::allowances::PendingRetirement {}

impl Stored for crate::allowances::PendingRetirement {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::PendingRetirement(key) = family else {
            return corrupt();
        };
        self.bind(key, binder)
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
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        self.bound(family, &binder.own).map(|_| ())
    }
}

impl sealed::Sealed for BoundNote {}

impl BoundRead for BoundNote {
    fn read_at(_: &str, family: &StateKey, bytes: &[u8], binder: &Binder<'_>) -> Result<Self> {
        let note = StoredNote::read_bytes(bytes, binder)?;
        let message = note.bound(family, binder.identity())?;
        Ok(Self { note, message })
    }
}

impl sealed::Sealed for JournalEntry {}

/// A journal entry (Paykit spec 7.2), checked as a restore checks it
/// ([`JournalEntry::check_restored`]): of the attempt its key names; its claim key the one
/// its payee, request and Billing Period blind to, or, for a payment without a request, its
/// attempt ID; the request it keeps that payee's request of that hash, amount, reference
/// and label; its carrier verified for the payee; its acceptance this
/// app's, of that request, at the `msg_id` it keeps; and its record the payer's record of
/// that request, or, for a payment without a request, of this app's attempt the key names.
/// An entry naming another attempt, claim, record or message would read and write one
/// payment while its key names another.
impl Stored for JournalEntry {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::Journal(attempt) = family else {
            return corrupt();
        };
        if let Some(under) = &self.allowance {
            if self.user_started || self.request_hash()?.is_none() || !under.valid() {
                return corrupt();
            }
        }
        if self.attempt()? != *attempt {
            return corrupt();
        }
        let blind = binder.blind()?;
        let cert = binder.cert.ok_or_else(PaykitError::corrupt)?;
        self.check_restored(&binder.own, cert, |input| blind(input))?;
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

impl sealed::Sealed for LossBoundary {}

/// The provider loss kept, which names nothing: recovered at a generation after another, its
/// residual window ending no earlier than its declaration.
impl Stored for LossBoundary {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::LossBoundary if self.valid() => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for OwnResendOwed {}

/// Counterparties still waiting for the restored-record scan are in ascending order,
/// each once. The scan clears its mark once all have been checked (Paykit spec 7.2).
impl Stored for OwnResendOwed {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::OwnResendOwed
                if self.waiting.as_ref().is_none_or(|waiting| {
                    !waiting.is_empty() && waiting.windows(2).all(|pair| pair[0] < pair[1])
                }) =>
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
            StateKey::Flagged if self.len() <= crate::marks::MAX_FLAGGED_COPY => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::reply_asks::ReplyAsks {}

impl Stored for crate::reply_asks::ReplyAsks {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::ReplyAsks { sender } => self.check(sender, binder),
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
                if self.sender == *sender
                    && self.earliest_ms <= self.latest_ms
                    && self.pending_forget.is_empty()
                    && !self.speculative
                    && !self.late_told =>
            {
                Ok(())
            }
            // A block's mark: its pairs still to forget are message IDs as the client gives
            // them, each once, within the bound.
            StateKey::DroppedBlocked { sender }
                if self.sender == *sender
                    && self.earliest_ms <= self.latest_ms
                    && self.pending_forget.len() <= crate::marks::MAX_PENDING_FORGETS
                    && self
                        .pending_forget
                        .iter()
                        .all(|id| pubky_switch_core::json::check_uuid_v4(id).is_ok())
                    && self
                        .pending_forget
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == self.pending_forget.len() =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::marks::MissedWidening {}

impl Stored for crate::marks::MissedWidening {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        if matches!(family, StateKey::BlockedWidening(id) if id == self.id.as_bytes())
            && !self.windows.is_empty()
            && self
                .windows
                .iter()
                .all(|w| w.from_ms <= w.until_ms && w.floor_ms <= w.until_ms)
            && self.deny.as_ref().map_or_else(
                || self.windows.len() == 1,
                |deny| deny.windows(2).all(|p| p[0] < p[1]),
            )
        {
            Ok(())
        } else {
            corrupt()
        }
    }
}

impl sealed::Sealed for crate::marks::LostOwed {}

/// A notice of lost messages owed (`bl/<sender>/<msg_id>`): of the sender and request its key
/// names, its window's times not crossed. The request is checked against the outbox when the
/// notice is settled (`Paykit::delivered_request`): its hash is the proof of the pointer.
impl Stored for crate::marks::LostOwed {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::LostOwed { sender, msg_id }
                if self.sender == *sender
                    && self.msg_id == *msg_id
                    && self.earliest_ms <= self.latest_ms =>
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
                    && mark.pending_forget.is_empty()
                    && !mark.speculative
                    && !mark.late_told
                    && !self.epochs.is_empty()
                    && self.epochs.windows(2).all(|pair| pair[0] < pair[1]) =>
            {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::settlement_marks::BucketHistory {}

impl Stored for crate::settlement_marks::BucketHistory {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::SettlementBucket(record) = family else {
            return corrupt();
        };
        if self.number >= paykit_lib::derive::SETTLEMENT_BUCKETS
            || self.generation == 0
            || binder.record_key(&crate::settlement_marks::target(self.number))? != *record
        {
            return corrupt();
        }
        Ok(())
    }
}

impl sealed::Sealed for crate::settlement_marks::ObservedMark {}

impl Stored for crate::settlement_marks::ObservedMark {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::SettlementMark { record, mark } = family else {
            return corrupt();
        };
        if self.bucket >= paykit_lib::derive::SETTLEMENT_BUCKETS
            || self.mark != *mark
            || paykit_lib::records::SettlementMark::from_bytes(mark).is_err()
            || binder.record_key(&crate::settlement_marks::target(self.bucket))? != *record
        {
            return corrupt();
        }
        Ok(())
    }
}

impl sealed::Sealed for crate::deletions::PayeeDeletion {}

impl Stored for crate::deletions::PayeeDeletion {
    fn bind(&self, key: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        if matches!(family, StateKey::PayeeDeletion{record,..} if self.matches(record))
            && key == self.key()
        {
            Ok(())
        } else {
            corrupt()
        }
    }
}

impl sealed::Sealed for crate::deletions::ReceiptEvidence {}
impl Stored for crate::deletions::ReceiptEvidence {
    fn bind(&self, key: &str, _: &StateKey, binder: &Binder<'_>) -> Result<()> {
        if key != self.key() {
            return corrupt();
        }
        self.check(&binder.own, |target| binder.record_key(target))
    }
}

impl sealed::Sealed for crate::deletions::CreditHold {}
impl Stored for crate::deletions::CreditHold {
    fn bind(&self, key: &str, _: &StateKey, _: &Binder<'_>) -> Result<()> {
        if key == self.key() {
            Ok(())
        } else {
            corrupt()
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

impl sealed::Sealed for crate::executor::ExecutorHistory {}

impl Stored for crate::executor::ExecutorHistory {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::ExecutorCheckpoint(key) = family else {
            return corrupt();
        };
        let cert = binder.cert.ok_or_else(PaykitError::corrupt)?;
        if !self.bound(binder, key, cert.app_id()) {
            return corrupt();
        }
        Ok(())
    }
}

impl sealed::Sealed for crate::executor::LedgerProposal {}

impl Stored for crate::executor::LedgerProposal {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::LedgerProposal { record, digest } if self.bound(binder, record, digest) => {
                Ok(())
            }
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for crate::executor::LedgerCompaction {}

impl Stored for crate::executor::LedgerCompaction {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::LedgerCompaction {
            ledger,
            application,
        } = family
        else {
            return corrupt();
        };
        let own = binder.cert.ok_or_else(PaykitError::corrupt)?;
        if !self.bound(ledger, application, &binder.own, own.app_id()) {
            return corrupt();
        }
        Ok(())
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

impl sealed::Sealed for crate::allowances::OriginalAllowance {}
impl Stored for crate::allowances::OriginalAllowance {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::OriginalAllowance(record) = family else {
            return corrupt();
        };
        self.check(record, binder)
    }
}

impl sealed::Sealed for crate::allowances::PreparedPendingReply {}

impl Stored for crate::allowances::PreparedPendingReply {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::PendingReply {
            record,
            reply: hash,
        } = family
        else {
            return corrupt();
        };
        let reply = self.message.stored(&self.peer)?;
        let id =
            crate::allowances::named_allowance(reply.body()).ok_or_else(PaykitError::corrupt)?;
        let target = crate::allowances::allowance_target(&self.peer, id);
        if let Some(q) = &self.fenced_acceptance {
            let q = q.stored(binder.identity())?;
            if !matches!(reply.body(), Body::AllowanceEnd(_))
                || !matches!(q.body(), Body::AllowanceAcceptance(a) if &a.allowance_id == id && a.executor_app_id.is_none())
                || q.sender() != self.peer
                || q.accepted_at().is_none()
                || q.message().refs != reply.message().refs
            {
                return corrupt();
            }
        }
        if self.source.record_version().is_none()
            || reply.hash() != *hash
            || !self.seals.last().is_some_and(|s| s.looks_like_delete())
            || !self.seals.iter().enumerate().all(|(index, s)| {
                let valid = s
                    .check_shape(
                        &paykit_lib::collections::Collection::AllowanceProposals
                            .fixed_policy()
                            .read_scope,
                    )
                    .is_ok();
                valid && (s.looks_like_delete() == (index + 1 == self.seals.len()))
            })
            || if reply.body().kind() == paykit_lib::kinds::Kind::AllowanceAcceptance {
                !(2..=3).contains(&self.seals.len())
            } else {
                self.seals.len() != 1
            }
            || reply.sender() != binder.own
            || binder
                .cert
                .is_none_or(|cert| reply.message().from.app_id != cert.app_id())
            || reply.accepted_at().is_some()
            || reply.message().refs.len() != 1
            || !matches!(
                reply.body(),
                Body::AllowanceAcceptance(_) | Body::AllowanceRejection(_) | Body::AllowanceEnd(_)
            )
            || binder.record_key(&target)? != *record
        {
            return corrupt();
        }
        Ok(())
    }
}

impl sealed::Sealed for crate::allowances::AppliedReply {}

impl Stored for crate::allowances::AppliedReply {
    fn bind(&self, _: &str, family: &StateKey, _: &Binder<'_>) -> Result<()> {
        match family {
            StateKey::AppliedReply(hash) if self.reply == *hash => Ok(()),
            _ => corrupt(),
        }
    }
}

impl sealed::Sealed for NoticeTarget {}

impl sealed::Sealed for EndOrigin {}

/// An exact own end's origin: the key names its ID, and its identity, app and canonical
/// Allowance tuple belong to this app. A crossing names the acceptance; a withdrawal names only its proposal.
/// Its hash and references must also match the signed message before they can be used as
/// that message's evidence (Paykit spec 8.1, 8.3).
impl Stored for EndOrigin {
    fn bind(&self, _: &str, family: &StateKey, binder: &Binder<'_>) -> Result<()> {
        let StateKey::EndOrigin(msg_id) = family else {
            return corrupt();
        };
        let cert = binder.cert.ok_or_else(PaykitError::corrupt)?;
        let allowance_id = paykit_lib::AllowanceId::parse(&self.allowance_id)
            .map_err(|_| PaykitError::corrupt())?;
        if self.msg_id != *msg_id
            || self.own != binder.own
            || self.app_id != cert.app_id()
            || self.counterparty == self.own
            || !self.cause.permits(self.acceptance.is_some())
            || self.acceptance == Some(self.proposal)
            || binder.record_key(&allowance_target(&self.counterparty, &allowance_id))?
                != self.record
        {
            return corrupt();
        }
        Ok(())
    }
}

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

/// Which signed private list a kept batch would publish.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PublicationKey {
    pub(crate) recipient: [u8; 32],
    pub(crate) generation: u64,
    pub(crate) hash: [u8; 32],
}

pub(crate) type PublicationIds = std::collections::BTreeSet<[u8; 16]>;

impl PublicationKey {
    pub(crate) fn key(&self) -> String {
        format!(
            "{}{}/{}",
            key::publication_prefix(&self.recipient),
            num(self.generation),
            pubky_switch_core::enc::b64(&self.hash)
        )
    }

    pub(crate) fn write(&self, ids: &PublicationIds) -> Result<LocalWrite> {
        let key = self.key().into_bytes();
        if ids.is_empty() {
            return Ok(LocalWrite::Delete { key });
        }
        if ids.len() as u64 > pubky_switch_client::store::MAX_VALUE_BYTES / 16 {
            return Err(PaykitError::corrupt());
        }
        Ok(LocalWrite::Put {
            key,
            value: ids.iter().flatten().copied().collect(),
        })
    }
}

pub(crate) fn publication_entry(
    key: &[u8],
    bytes: &[u8],
    own: &[u8; 32],
) -> Result<(PublicationKey, PublicationIds)> {
    let Some(StateKey::Publication(original)) = StateKey::parse(key) else {
        return Err(PaykitError::corrupt());
    };
    if &original.recipient == own {
        return Err(PaykitError::corrupt());
    }
    Ok((original, batch_ids(bytes)?.iter().copied().collect()))
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

    pub(crate) const RETAINED_RECEIPTS: &str = "rc/";
    pub(crate) const CREDIT_HOLDS: &str = "ch/";
    pub(crate) const SETTLEMENT_NOTICES: &str = "nt/settlement/";
    pub(crate) const SETTLEMENT_MARKS: &str = "sm/";
    pub(crate) const SETTLEMENT_BUCKETS: &str = "sb/";
    pub(crate) fn settlement_bucket(key: &[u8; 32]) -> String {
        format!("{SETTLEMENT_BUCKETS}{}", b64(key))
    }
    pub(crate) fn receipt_batch(
        record: &[u8; 32],
        hash: &[u8; 32],
        id: &str,
        imported: bool,
    ) -> String {
        let prefix = if imported { "ir" } else { "ri" };
        format!("{prefix}/{}/{}/{id}", b64(record), b64(hash))
    }
    pub(crate) fn retained_receipt(hash: &[u8; 32]) -> String {
        format!("{RETAINED_RECEIPTS}{}", b64(hash))
    }
    pub(crate) fn credit_hold(record: &[u8; 32]) -> String {
        format!("{CREDIT_HOLDS}{}", b64(record))
    }
    pub(crate) const PAYEE_DELETIONS: &str = "del/";

    pub(crate) fn payee_deletions_at(record: &[u8; 32]) -> String {
        format!("{PAYEE_DELETIONS}{}/", b64(record))
    }

    pub(crate) const ASKS: &str = "ask/";
    pub(crate) const ANSWERS: &str = "ans/";
    pub(crate) const MISSED: &str = "missed/";
    pub(crate) const MISSED_ITEMS: &str = "mi/";
    pub(crate) const JOURNAL: &str = "j/";
    pub(crate) const PEERS: &str = "peer/";
    pub(crate) const KEPT_CANCELLATIONS: &str = "kc/";
    pub(crate) const CURSOR_TIME: &str = "ct";
    pub(crate) const RESTORE_HOLD: &str = "rh";
    /// The identity of the latest restore or missed hold.
    pub(crate) const HOLD_EPOCH: &str = "he";
    pub(crate) const MISSED_HOLD: &str = "mh";
    pub(crate) const LOSS_BOUNDARY: &str = "pl";
    /// The newest provider loss this app recorded, by the generation recovered at
    /// (`crate::loss`, the shared post-loss hold).
    pub(crate) const LOSS_RECORDED: &str = "rl";
    /// A floor raise tried and left unwritten (`crate::floor::RaiseRetry`).
    pub(crate) const RAISE_RETRY: &str = "rr";
    pub(crate) const NOT_PAID_BEFORE_LOSS: &str = "pn/";
    /// A `missed` item's hold whose end isn't known yet (`Paykit::bound_missed_hold`).
    pub(crate) const MISSED_END_UNKNOWN: &str = "mu";
    pub(crate) const HOLD_CONFIRMED: &str = "hc/";
    pub(crate) const HOLD_CONFIRMED_BEFORE: &str = "hb/";
    pub(crate) const RESTORE_CONSENTS: &str = "rsc/";
    pub(crate) const EXECUTION_APPROVALS: &str = "xa/";
    pub(crate) const RESTORE_APPROVALS: &str = "xr/";
    pub(crate) const LIST_SETTINGS: &str = "list";
    pub(crate) const PUBLICATIONS: &str = "lp/";
    pub(crate) const LIST_RESERVATIONS: &str = "lres/";
    pub(crate) const PUBLICATION_HISTORY: &str = "lhi/";
    pub(crate) const LIST_RECIPIENTS: &str = "lrec/";
    pub(crate) const LIST_DISCOVERY: &str = "ld";
    pub(crate) const LISTS_READ: &str = "lr";
    pub(crate) const SERVICE_TIME: &str = "sv";
    pub(crate) const HELD_LISTS: &str = "lh/";
    pub(crate) const LISTS_LOST: &str = "lgone/";
    pub(crate) const RECORDS_UNLISTED: &str = "nl";
    pub(crate) const MISSED_UNLISTED: &str = "ml";
    pub(crate) const MISSED_CONSENTS: &str = "mc";
    pub(crate) const PAYER_PASS_OWED: &str = "pp";
    pub(crate) const OWN_RESEND_OWED: &str = "ro";
    pub(crate) const HELD_ANSWERS: &str = "ha/";
    /// `qs/<msg_id>`: the rejections and payer's cancellations this app queued.
    pub(crate) const QUEUED_STOPS: &str = "qs/";
    /// `ex/<msg_id>`: why an outgoing acceptance may go while sends are held.
    pub(crate) const SEND_EXEMPTIONS: &str = "ex/";
    pub(crate) const LISTED_CERT: &str = "lc";
    pub(crate) const NOTICES: &str = "nt/";
    pub(crate) const RESTORED_DELIVERIES: &str = "nt/outgoing/";
    pub(crate) const EXTENSIONS: &str = "xm/";
    pub(crate) const STORED_NOTES: &str = "se/";
    pub(crate) const KEPT_REPLIES: &str = "pr/";
    pub(crate) const REPLY_ASKS: &str = "ra/";
    pub(crate) const DROPPED_REPLIES: &str = "pd/";
    pub(crate) const DROPPED_UNVERIFIED: &str = "ud/";
    pub(crate) const DROPPED_BLOCKED: &str = "bd/";
    pub(crate) const BLOCKED_WIDENINGS: &str = "bw/";
    /// The notices of lost messages owed on block marks the receive step dropped for the
    /// bound (`crate::marks::LostOwed`).
    pub(crate) const LOST_OWED: &str = "bl/";
    pub(crate) const FLAGGED: &str = "fg";
    /// The raise of the block floor this app owes (`crate::floor`).
    pub(crate) const FLOOR_BATCH: &str = "fb";
    /// What this app keeps of the block floor (`crate::floor::FloorKept`).
    pub(crate) const FLOOR_KEPT: &str = "fk";
    pub(crate) const BLOCK_FLOOR: &str = "bf";
    /// The request records this read opened without `manual_only`, to check against the
    /// block floor once it is done (`crate::floor`).
    pub(crate) const FLOOR_OPENED: &str = "fo/";
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
    pub(crate) const END_ORIGINS: &str = "end/";

    pub(crate) fn end_origin(msg_id: &str) -> String {
        format!("{END_ORIGINS}{msg_id}")
    }

    pub(crate) fn end_batch(msg_id: &str) -> String {
        format!("eb/{msg_id}")
    }

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

    /// One bounded conflict hold per physical Allowance or pending proposal record.
    pub(crate) const ALLOWANCE_EVIDENCE: &str = "ax/";

    pub(crate) fn allowance_evidence(record: &[u8; 32]) -> String {
        format!("{ALLOWANCE_EVIDENCE}{}", b64(record))
    }

    pub(crate) const PENDING_CONFLICTS: &str = "pc/";

    pub(crate) const PENDING_REPLIES: &str = "ap/";
    pub(crate) const ORIGINAL_ALLOWANCES: &str = "ao/";

    pub(crate) fn original_allowance(record: &[u8; 32]) -> String {
        format!("{ORIGINAL_ALLOWANCES}{}", b64(record))
    }

    pub(crate) const APPLIED_REPLIES: &str = "aq/";

    pub(crate) fn pending_replies(record: &[u8; 32]) -> String {
        format!("{PENDING_REPLIES}{}/", b64(record))
    }

    pub(crate) fn pending_reply(record: &[u8; 32], reply: &[u8; 32]) -> String {
        format!("{}{}", pending_replies(record), b64(reply))
    }

    pub(crate) fn applied_reply(hash: &[u8; 32]) -> String {
        format!("{APPLIED_REPLIES}{}", b64(hash))
    }

    pub(crate) const PENDING_RETIREMENTS: &str = "pce/";

    /// The latest verified deletion of this physical pending key.
    pub(crate) fn pending_retirement(record: &[u8; 32]) -> String {
        format!("{PENDING_RETIREMENTS}{}", b64(record))
    }

    /// A conflict in this pending Allowance lifetime.
    pub(crate) fn pending_conflict(record: &[u8; 32]) -> String {
        format!("{PENDING_CONFLICTS}{}", b64(record))
    }

    /// A conflicting proposal or response of the Allowance whose record is `record_key`,
    /// kept as the stop.
    pub(crate) fn conflict(record_key: &[u8; 32]) -> String {
        format!("{CONFLICTS}{}", b64(record_key))
    }

    /// The payer's pending or verified executor grants.
    pub(crate) const EXECUTOR_CHECKPOINTS: &str = "lv/";

    /// The payer's grant and later accepted ledger for this Allowance.
    pub(crate) fn executor_checkpoint(record: &[u8; 32]) -> String {
        format!("{EXECUTOR_CHECKPOINTS}{}", b64(record))
    }

    /// The batch ID of this app's first grant of the Allowance at `record`, while the client
    /// keeps it to send again; the batch deletes it when it applies.
    pub(crate) fn grant_batch(record: &[u8; 32]) -> String {
        format!("gb/{}", b64(record))
    }

    /// Staged complete values and the native batches that actually submitted them.
    pub(crate) const LEDGER_PROPOSALS: &str = "lw/";
    pub(crate) const LEDGER_BATCHES: &str = "lb/";
    pub(crate) fn ledger_batches(record: &[u8; 32]) -> String {
        format!("{LEDGER_BATCHES}{}/", b64(record))
    }
    /// Original applied ledger transitions retained for compacted spending.
    pub(crate) const LEDGER_COMPACTIONS: &str = "lx/";

    pub(crate) fn ledger_compactions(ledger: &[u8; 32]) -> String {
        format!("{LEDGER_COMPACTIONS}{}/", b64(ledger))
    }

    /// The consent list's entry of the Allowance whose record is `record_key`.
    pub(crate) fn consent(record_key: &[u8; 32]) -> String {
        format!("{CONSENTS}{}", b64(record_key))
    }

    /// The executor app the `executor_notice` `msg_id` is wrapped to.
    pub(crate) fn notice_target(msg_id: &str) -> String {
        format!("{NOTICE_TARGETS}{msg_id}")
    }

    /// First-delivery evidence for requests for the sender's dropped replies.
    pub(crate) fn reply_asks(sender: &[u8; 32]) -> String {
        format!("{REPLY_ASKS}{}", b64(sender))
    }

    /// The mark of replies from `sender` dropped for the bounds.
    pub(crate) fn dropped_replies(sender: &[u8; 32]) -> String {
        format!("{DROPPED_REPLIES}{}", b64(sender))
    }

    /// The mark of entries from `sender` dropped unverified.
    pub(crate) fn dropped_unverified(sender: &[u8; 32]) -> String {
        format!("{DROPPED_UNVERIFIED}{}", b64(sender))
    }

    /// The mark of messages from `sender` dropped for the user's block.
    pub(crate) fn dropped_blocked(sender: &[u8; 32]) -> String {
        format!("{DROPPED_BLOCKED}{}", b64(sender))
    }

    /// A widening of the block marks from `from_ms`, owed to every counterparty the deny
    /// list holds at the next read of the lists.
    pub(crate) fn blocked_widening(id: &[u8; 16]) -> String {
        format!("{BLOCKED_WIDENINGS}{}", b64(id))
    }

    /// The notice of lost messages owed on `sender`'s block mark, dropped for the bound while
    /// it named the request `msg_id` as queued.
    pub(crate) fn lost_owed(sender: &[u8; 32], msg_id: &str) -> String {
        format!("{LOST_OWED}{}/{msg_id}", b64(sender))
    }

    /// The notice that what `counterparty`'s inbox accepted from `from_ms`, dropped for the
    /// user's block, can no longer come back.
    pub(crate) fn blocked_lost(counterparty: &[u8; 32], from_ms: u64) -> String {
        format!("{NOTICES}{}/lost-{from_ms:020}", b64(counterparty))
    }

    /// A lost recovery window, separated by family so simultaneous losses do not overwrite.
    pub(crate) fn dropped_lost(counterparty: &[u8; 32], family: &str, from_ms: u64) -> String {
        format!(
            "{NOTICES}{}/dropped-{family}-{from_ms:020}",
            b64(counterparty)
        )
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

    /// A restored outgoing message, bound to its recipient, ID and signed JSON hash.
    pub(crate) fn restored_delivery(to: &[u8; 32], id: &str, hash: &[u8; 32]) -> String {
        format!("{RESTORED_DELIVERIES}{}/{id}/{}", b64(to), b64(hash))
    }

    /// The extension message `msg_id` from `sender`, kept until the host takes it.
    pub(crate) fn extension(sender: &[u8; 32], msg_id: &str) -> String {
        format!("{EXTENSIONS}{}/{msg_id}", b64(sender))
    }

    /// The notice of `kind` about `attempt`.
    /// The identity-wide notice of `kind` (`crate::notices`).
    pub(crate) fn identity_notice(kind: crate::notices::NoticeKind) -> String {
        format!("{NOTICES}{}", kind.tag())
    }

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

    pub(crate) fn offline_batch(attempt: &AttemptId) -> String {
        format!("off/{}", attempt.to_text())
    }

    /// A rejection or payer's cancellation `msg_id` to `payee`, recorded while sends to it
    /// were held and not queued yet.
    pub(crate) fn held_answer(payee: &[u8; 32], msg_id: &str) -> String {
        format!("{HELD_ANSWERS}{}/{msg_id}", b64(payee))
    }

    pub(crate) fn side(msg_id: &str) -> String {
        format!("side/{msg_id}")
    }

    pub(crate) fn send_exemption(msg_id: &str) -> String {
        format!("ex/{msg_id}")
    }

    pub(crate) fn queued_stop(msg_id: &str) -> String {
        format!("{QUEUED_STOPS}{msg_id}")
    }

    pub(crate) fn seen_records(payee: &[u8; 32], app_id: &[u8; 16]) -> String {
        format!("ar/{}/{}", b64(payee), b64(app_id))
    }

    pub(crate) fn floor_opened(record_key: &[u8; 32]) -> String {
        format!("{FLOOR_OPENED}{}", b64(record_key))
    }

    pub(crate) fn hold_confirmed(record_key: &[u8; 32], epoch: &crate::hold::HoldEpoch) -> String {
        format!(
            "{HOLD_CONFIRMED}{}/{}",
            b64(record_key),
            b64(epoch.as_bytes())
        )
    }

    pub(crate) fn hold_confirmed_before(
        record_key: &[u8; 32],
        epoch: &crate::hold::HoldEpoch,
    ) -> String {
        format!(
            "{HOLD_CONFIRMED_BEFORE}{}/{}",
            b64(record_key),
            b64(epoch.as_bytes())
        )
    }

    /// `xa/<record key>/<hold identifier>`: the user approved automatic execution of the
    /// Allowance at `record_key` under that hold (`he`), or `none` before any hold.
    pub(crate) fn execution_approval(
        record_key: &[u8; 32],
        hold: Option<&crate::hold::HoldEpoch>,
    ) -> String {
        format!(
            "{EXECUTION_APPROVALS}{}/{}",
            b64(record_key),
            hold.map_or_else(|| String::from("none"), |hold| b64(hold.as_bytes()))
        )
    }

    /// `xr/<record key>`: an approval for execution a restore carried, until its verdict.
    pub(crate) fn restore_approval(record_key: &[u8; 32]) -> String {
        format!("{RESTORE_APPROVALS}{}", b64(record_key))
    }

    pub(crate) fn restore_consent(record_key: &[u8; 32]) -> String {
        format!("{RESTORE_CONSENTS}{}", b64(record_key))
    }

    pub(crate) fn held_list(record_key: &[u8; 32]) -> String {
        format!("{HELD_LISTS}{}", b64(record_key))
    }

    /// Set once this app has seen the `payment_lists` record at `record_key` deleted or
    /// unreadable, and never cleared: a later absence of it says nothing of the marks it
    /// held.
    pub(crate) fn list_record_lost(record_key: &[u8; 32]) -> String {
        format!("{LISTS_LOST}{}", b64(record_key))
    }

    /// This app's block or unblock of `pubky` not yet shown by a read of the lists
    /// ([`OwnBlockMark`]). A block is marked before the switch is asked, so a crash before
    /// the call leaves it counted; its mark goes once the switch took it, or once a read of
    /// the lists shows it, and until then a block whose batch the client neither keeps nor
    /// applied is written again at each receive. An unblock counts for payments and held
    /// sends only while the client keeps its batch, and never for the spam rule.
    pub(crate) fn list_recipient(recipient: &[u8; 32]) -> String {
        format!(
            "{LIST_RECIPIENTS}{}",
            pubky_switch_core::enc::b64(recipient)
        )
    }

    pub(crate) fn publication_prefix(recipient: &[u8; 32]) -> String {
        format!("{PUBLICATIONS}{}/", b64(recipient))
    }

    pub(crate) fn publication_history(record: &[u8; 32]) -> String {
        format!("{PUBLICATION_HISTORY}{}", b64(record))
    }

    pub(crate) fn list_reservation(recipient: &[u8; 32]) -> String {
        format!("{LIST_RESERVATIONS}{}", b64(recipient))
    }

    pub(crate) fn own_block(pubky: &[u8; 32]) -> String {
        format!("{OWN_BLOCKS}{}", b64(pubky))
    }

    /// The earliest cancellation under this signing certificate for the payer's request record (Paykit spec 8.1, Messages that cross).
    pub(crate) fn kept_cancellation(
        record_key: &[u8; 32],
        cert: &[u8; 32],
        generation: u64,
    ) -> String {
        format!(
            "{}{}/{}",
            kept_cancellations(record_key),
            b64(cert),
            num(generation)
        )
    }

    /// The prefix of every cancellation kept as a stop of the request record `record_key`.
    pub(crate) fn kept_cancellations(record_key: &[u8; 32]) -> String {
        format!("{KEPT_CANCELLATIONS}{}/", b64(record_key))
    }

    pub(crate) fn peer(pubky: &[u8; 32]) -> String {
        format!("{PEERS}{}", b64(pubky))
    }

    pub(crate) fn spam(sender: &[u8; 32]) -> String {
        format!("spam/{}", b64(sender))
    }

    pub(crate) fn not_paid_before_loss(obligation: &[u8; 32]) -> String {
        format!("{NOT_PAID_BEFORE_LOSS}{}", b64(obligation))
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
    ListSettings,
    ListRecipient([u8; 32]),
    ListReservation([u8; 32]),
    PublicationHistory([u8; 32]),
    Publication(PublicationKey),
    ListDiscovery,
    /// A warning once per bucket or settlement.
    SettlementNotice(SettlementSubject),
    /// The last verified physical application and its outstanding repair.
    SettlementBucket([u8; 32]),
    /// A complete mark observed at the permanently bound settlement bucket key.
    SettlementMark {
        record: [u8; 32],
        mark: [u8; paykit_lib::records::SETTLEMENT_MARK_LEN],
    },
    /// A signed receipt, bound to its record and signed receipt hash.
    RetainedReceipt([u8; 32]),
    /// A credit restriction on a record from the previous installation.
    CreditHold([u8; 32]),
    /// One observed deletion and the state of its warning.
    PayeeDeletion {
        record: [u8; 32],
        generation: u64,
        version: u64,
        delete: [u8; 32],
    },
    /// `j/<attempt ID>`: a journal entry.
    Journal(AttemptId),
    /// The original Switch batch ID of a paid offline attempt.
    OfflineBatch(AttemptId),
    /// A requested receipt delivery, awaiting its answer or holding its verified application.
    ReceiptBatch {
        record: [u8; 32],
        hash: [u8; 32],
        msg_id: String,
        imported: bool,
    },
    /// `ask/<sender>/<from>`: a `resend_request` to send.
    Ask {
        sender: [u8; 32],
        from_ms: u64,
    },
    /// `ans/<seq>/<msg_id>`: a `resend_request` received, to answer.
    Answer {
        seq: u64,
        msg_id: String,
    },
    /// `missed/<from>`: the re-sends to ask for after a provider loss.
    Missed {
        from_ms: u64,
    },
    /// `mi/<from>`: a `missed` item's re-sends to ask for.
    MissedItem {
        from_ms: u64,
    },
    /// `peer/<pubky>`: a counterparty with Paykit activity.
    Peer([u8; 32]),
    /// `kc/<record key>/<certificate hash>/<feed generation>`: a payee's cancellation kept to place, one per original signing certificate and feed generation.
    KeptCancellation {
        record: [u8; 32],
        cert: [u8; 32],
        generation: u64,
    },
    /// `ct`: the acceptance time of the last entry the cursor passed.
    CursorTime,
    /// `pl`: the last provider loss followed.
    LossBoundary,
    /// `pn/<obligation>`: not paid before the kept loss.
    NotPaidBeforeLoss([u8; 32]),
    /// `rh`: the hold a restore set.
    RestoreHold,
    /// `he`: the identity of the latest restore or missed hold.
    HoldEpoch,
    /// `mh`: the hold a `missed` item set.
    MissedHold,
    /// `mu`: a `missed` item's hold whose end isn't known yet.
    MissedEndUnknown,
    /// `nl`: the shared records aren't listed since a restore.
    RecordsUnlisted,
    /// `ml`: the shared records aren't listed by Paykit since a `missed` item.
    MissedUnlisted,
    /// `mc`: the consent list is to be confirmed again since a `missed` item.
    MissedConsents,
    /// `pp`: no reconcile ran the payer pass since a restore.
    PayerPassOwed,
    /// `ro` marks a pending scan for this app's outgoing messages after restore.
    OwnResendOwed,
    /// `lc`: the certificate the shared records were last listed under.
    ListedCert,
    /// `ha/<payee>/<msg_id>`: an answer recorded while sends to its payee were held.
    HeldAnswer {
        payee: [u8; 32],
        msg_id: String,
    },
    /// `hc/<record key>/<hold identifier>`: the user's confirmation of a held request,
    /// under the hold identifier it was given under.
    HoldConfirmed {
        /// The request's record key.
        record: [u8; 32],
        /// The hold identifier (`he`) the confirmation was given under.
        epoch: [u8; 16],
    },
    /// `hb/<record key>/<hold identifier>`: a request confirmation carried from before the
    /// restore, under the identifier of the restore that carried it.
    HoldConfirmedBefore {
        /// The request's record key.
        record: [u8; 32],
        /// The hold identifier the restore set.
        epoch: [u8; 16],
    },
    /// An Allowance consent held specifically by this restore.
    RestoreConsent([u8; 32]),
    /// `xr/<record key>`: an approval for execution a restore carried from its backup,
    /// which counts only once the restore's verdict is clean.
    RestoreApproval([u8; 32]),
    /// `xa/<record key>/<hold identifier>`: the user's approval of automatic execution of
    /// an Allowance, under the hold it was given under (`None` before any hold).
    ExecutionApproval {
        /// The Allowance record's key.
        record: [u8; 32],
        /// The hold identifier the approval was given under.
        hold: Option<[u8; 16]>,
    },
    /// `lr`: when the lists were last read.
    ListsRead,
    /// `sv`: the highest service time verified, for the delivery hook.
    ServiceTime,
    /// `lh/<record key>`: a list held over a record that didn't read.
    HeldList([u8; 32]),
    /// `lgone/<record key>`: a `payment_lists` record seen deleted or unreadable.
    ListRecordLost([u8; 32]),
    /// `side/<msg_id>`: the side of a message the app sent.
    Side(String),
    /// `ex/<msg_id>`: why an acceptance the app sent may go while sends are held.
    SendExemption(String),
    /// `qs/<msg_id>`: a rejection or payer's cancellation the app queued.
    QueuedStop(String),
    /// `ar/<payee>/<app>`: a payee app's records a reader has seen.
    SeenRecords {
        payee: [u8; 32],
        app_id: [u8; 16],
    },
    /// `spam/<sender>`: the messages the spam rule only counted.
    Spam([u8; 32]),
    /// `rs/<requester>/<msg_id>`: when a message was re-sent to a requester.
    Resent {
        requester: [u8; 32],
        msg_id: String,
    },
    /// `rsd/<requester>/<day>`: how many messages were re-sent to a requester that day.
    ResentDay {
        requester: [u8; 32],
        day: u64,
    },
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
    DroppedReplies {
        sender: [u8; 32],
    },
    /// Outstanding and last settled requests for a sender's dropped replies.
    ReplyAsks {
        sender: [u8; 32],
    },
    /// `ud/<sender>`: the mark of a sender's entries dropped unverified.
    DroppedUnverified {
        sender: [u8; 32],
    },
    /// `bd/<sender>`: the mark of a sender's messages dropped for the user's block.
    DroppedBlocked {
        sender: [u8; 32],
    },
    /// `bw/<hold epoch>`: a widening of the block marks owed at the next read of the lists.
    BlockedWidening([u8; 16]),
    /// `bl/<sender>/<msg_id>`: the notice of lost messages owed on a sender's block mark the
    /// receive step dropped for the bound, naming `msg_id` as queued.
    LostOwed {
        sender: [u8; 32],
        msg_id: String,
    },
    /// `fg`: the flagged senders as the app last read them.
    Flagged,
    /// `bf`: the raise of the block floor this app owes.
    BlockFloor,
    /// `fk`: what this app keeps of the block floor.
    FloorKept,
    /// `nt/<tag>`: an identity-wide notice of the kind the tag names.
    IdentityNotice(crate::notices::NoticeKind),
    /// `rl`: the newest provider loss this app recorded.
    LossRecorded,
    /// `rr`: a floor raise tried and left unwritten.
    RaiseRetry,
    /// The local identifier of an unanswered floor write.
    FloorBatch,
    /// `fo/<record key>`: a request record a read opened without `manual_only`, to check
    /// against the block floor once the read is done.
    FloorOpened([u8; 32]),
    /// `nt/<counterparty>/lost-<from_ms>`: the notice that messages dropped for a block
    /// can no longer come back.
    BlockedLost {
        counterparty: [u8; 32],
        from_ms: u64,
    },
    /// A bounded recovery family's unasked window, retained until host acknowledgement.
    DroppedLost {
        counterparty: [u8; 32],
        family: String,
        from_ms: u64,
    },
    /// `kw/<sender>`: the mark of a sender's entries dropped while they waited for a key.
    KeyWaits {
        sender: [u8; 32],
    },
    /// `pr/<sender>/<record key>/<msg_id>`: a payer's reply kept for its record.
    KeptReply {
        sender: [u8; 32],
        record: [u8; 32],
        msg_id: String,
    },
    /// `se/<sender>/<msg_id>`: a stored entry's message to write later.
    StoredNote {
        sender: [u8; 32],
        msg_id: String,
    },
    /// A notice of a possibly delivered outgoing message (Paykit spec 7.2).
    RestoredDelivery {
        to: [u8; 32],
        msg_id: String,
        hash: [u8; 32],
    },
    /// `nt/<sender>/<msg_id>`: a notice for the host about a message given up.
    MessageNotice {
        sender: [u8; 32],
        msg_id: String,
    },
    /// `xm/<sender>/<msg_id>`: an extension message kept until the host takes it.
    Extension {
        sender: [u8; 32],
        msg_id: String,
    },
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
    AllowanceEnd {
        record: [u8; 32],
        msg_id: String,
    },
    /// `ac/<record key>`: an entry of the consent list.
    Consent([u8; 32]),
    /// An executor grant with the later ledger accepted by the payer.
    ExecutorCheckpoint([u8; 32]),
    /// A staged ledger successor, bound to its Allowance and content.
    LedgerProposal {
        record: [u8; 32],
        digest: [u8; 32],
    },
    /// The native IDs that submitted this exact proposal.
    LedgerBatch {
        record: [u8; 32],
        digest: [u8; 32],
    },
    /// The original applied ledger writes on either side of a paid compaction.
    LedgerCompaction {
        ledger: [u8; 32],
        application: [u8; 32],
    },
    /// `pc/<record key>`: a conflict kept for one pending Allowance lifetime.
    PendingConflict([u8; 32]),
    /// A bounded unresolved conflict at a permanent or pending Allowance key.
    AllowanceEvidence([u8; 32]),
    /// An original reply prepared for this canonical Allowance key.
    PendingReply {
        record: [u8; 32],
        reply: [u8; 32],
    },
    /// A signed reply whose exact proposal deletion was applied.
    AppliedReply([u8; 32]),
    OriginalAllowance([u8; 32]),
    /// The latest verified end of a conflict at the named pending key.
    PendingRetirement([u8; 32]),
    /// `cf/<record key>`: a conflicting proposal or response kept as the stop.
    Conflict([u8; 32]),
    /// `en/<msg_id>`: the executor app an `executor_notice` is wrapped to.
    NoticeTarget(String),
    /// `eo/<record key>`: this app's own crossing end, recorded while sends were held, whose
    /// message is owed.
    OwedEnd([u8; 32]),
    /// The exact own signed end recorded by the producer.
    EndOrigin(String),
    /// The unresolved native batch that will record an automatic end before queueing.
    EndBatch(String),
    /// The unresolved native batch of this app's first grant of an Allowance.
    GrantBatch([u8; 32]),
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
            "ld" => Self::ListDiscovery,
            "list" => Self::ListSettings,
            "ct" => Self::CursorTime,
            "rh" => Self::RestoreHold,
            "he" => Self::HoldEpoch,
            "mh" => Self::MissedHold,
            "pl" => Self::LossBoundary,
            "rl" => Self::LossRecorded,
            "rr" => Self::RaiseRetry,
            "mu" => Self::MissedEndUnknown,
            "nl" => Self::RecordsUnlisted,
            "ml" => Self::MissedUnlisted,
            "mc" => Self::MissedConsents,
            "pp" => Self::PayerPassOwed,
            "ro" => Self::OwnResendOwed,
            "lc" => Self::ListedCert,
            "lr" => Self::ListsRead,
            "sv" => Self::ServiceTime,
            "dl" => Self::Denied,
            "fg" => Self::Flagged,
            "fk" => Self::FloorKept,
            "bf" => Self::BlockFloor,
            "fb" => Self::FloorBatch,
            _ => {
                let (family, rest) = text.split_once('/')?;
                match family {
                    "lrec" => Self::ListRecipient(b64_32(rest)?),
                    "lp" => {
                        let (recipient, rest) = rest.split_once('/')?;
                        let (generation, hash) = rest.split_once('/')?;
                        let generation = number(generation)?;
                        paykit_lib::ListVersion::new(generation, 1).ok()?;
                        Self::Publication(PublicationKey {
                            recipient: b64_32(recipient)?,
                            generation,
                            hash: b64_32(hash)?,
                        })
                    }
                    "lres" => Self::ListReservation(b64_32(rest)?),
                    "lhi" => Self::PublicationHistory(b64_32(rest)?),
                    "sb" => Self::SettlementBucket(b64_32(rest)?),
                    "sm" => {
                        let (record, mark) = rest.split_once('/')?;
                        let bytes = pubky_switch_core::enc::unb64_array(mark).ok()?;
                        if pubky_switch_core::enc::b64(&bytes) != mark {
                            return None;
                        }
                        Self::SettlementMark {
                            record: b64_32(record)?,
                            mark: bytes,
                        }
                    }
                    "rc" => Self::RetainedReceipt(b64_32(rest)?),
                    "ri" | "ir" => {
                        let (record, rest) = rest.split_once('/')?;
                        let (hash, id) = rest.split_once('/')?;
                        Self::ReceiptBatch {
                            record: b64_32(record)?,
                            hash: b64_32(hash)?,
                            msg_id: msg_id(id)?,
                            imported: text.starts_with("ir/"),
                        }
                    }
                    "ch" => Self::CreditHold(b64_32(rest)?),
                    "del" => {
                        let (record, rest) = rest.split_once('/')?;
                        let (generation, rest) = rest.split_once('/')?;
                        let (version, hash) = rest.split_once('/')?;
                        Self::PayeeDeletion {
                            record: b64_32(record)?,
                            generation: number(generation)?,
                            version: number(version)?,
                            delete: b64_32(hash)?,
                        }
                    }
                    "j" | "off" => {
                        let attempt = AttemptId::parse(rest).ok()?;
                        if attempt.to_text() != rest {
                            return None;
                        }
                        if family == "j" {
                            Self::Journal(attempt)
                        } else {
                            Self::OfflineBatch(attempt)
                        }
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
                    "kc" => {
                        let (record, rest) = rest.split_once('/')?;
                        let (cert, generation) = rest.split_once('/')?;
                        Self::KeptCancellation {
                            record: b64_32(record)?,
                            cert: b64_32(cert)?,
                            generation: number(generation)?,
                        }
                    }
                    "bk" => Self::OwnBlock(b64_32(rest)?),
                    "hc" | "hb" => {
                        let (record, epoch) = rest.split_once('/')?;
                        let (record, epoch) = (b64_32(record)?, b64_16(epoch)?);
                        if family == "hc" {
                            Self::HoldConfirmed { record, epoch }
                        } else {
                            Self::HoldConfirmedBefore { record, epoch }
                        }
                    }
                    "rsc" => Self::RestoreConsent(b64_32(rest)?),
                    "xr" => Self::RestoreApproval(b64_32(rest)?),
                    "xa" => {
                        let (record, hold) = rest.split_once('/')?;
                        Self::ExecutionApproval {
                            record: b64_32(record)?,
                            hold: if hold == "none" {
                                None
                            } else {
                                Some(b64_16(hold)?)
                            },
                        }
                    }
                    "pn" => Self::NotPaidBeforeLoss(b64_32(rest)?),
                    "fo" => Self::FloorOpened(b64_32(rest)?),
                    "lh" => Self::HeldList(b64_32(rest)?),
                    "lgone" => Self::ListRecordLost(b64_32(rest)?),
                    "side" => Self::Side(msg_id(rest)?),
                    "ex" => Self::SendExemption(msg_id(rest)?),
                    "qs" => Self::QueuedStop(msg_id(rest)?),
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
                    "lv" => Self::ExecutorCheckpoint(b64_32(rest)?),
                    "lw" | "lb" => {
                        let (r, h) = rest.split_once('/')?;
                        let (record, digest) = (b64_32(r)?, b64_32(h)?);
                        if text.starts_with("lw/") {
                            Self::LedgerProposal { record, digest }
                        } else {
                            Self::LedgerBatch { record, digest }
                        }
                    }
                    "lx" => {
                        let (ledger, application) = rest.split_once('/')?;
                        Self::LedgerCompaction {
                            ledger: b64_32(ledger)?,
                            application: b64_32(application)?,
                        }
                    }
                    "ax" => Self::AllowanceEvidence(b64_32(rest)?),
                    "pc" => Self::PendingConflict(b64_32(rest)?),
                    "ap" => {
                        let (record, reply) = rest.split_once('/')?;
                        Self::PendingReply {
                            record: b64_32(record)?,
                            reply: b64_32(reply)?,
                        }
                    }
                    "ao" => Self::OriginalAllowance(b64_32(rest)?),
                    "aq" => Self::AppliedReply(b64_32(rest)?),
                    "pce" => Self::PendingRetirement(b64_32(rest)?),
                    "cf" => Self::Conflict(b64_32(rest)?),
                    "en" => Self::NoticeTarget(msg_id(rest)?),
                    "end" => Self::EndOrigin(msg_id(rest)?),
                    "eb" => Self::EndBatch(msg_id(rest)?),
                    "gb" => Self::GrantBatch(b64_32(rest)?),
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
                    "ra" => Self::ReplyAsks {
                        sender: b64_32(rest)?,
                    },
                    "pd" => Self::DroppedReplies {
                        sender: b64_32(rest)?,
                    },
                    "ud" => Self::DroppedUnverified {
                        sender: b64_32(rest)?,
                    },
                    "bd" => Self::DroppedBlocked {
                        sender: b64_32(rest)?,
                    },
                    "bw" => {
                        Self::BlockedWidening(pubky_switch_core::enc::unb64_array::<16>(rest).ok()?)
                    }
                    "bl" => {
                        let (sender, id) = rest.split_once('/')?;
                        Self::LostOwed {
                            sender: b64_32(sender)?,
                            msg_id: msg_id(id)?,
                        }
                    }
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
                    "xm" => {
                        let (sender, id) = rest.split_once('/')?;
                        Self::Extension {
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
                        if let Some(kind) = crate::notices::NoticeKind::identity_from_tag(rest) {
                            return Some(Self::IdentityNotice(kind));
                        }
                        if let Some(subject) = rest.strip_prefix("settlement/") {
                            return crate::notices::SettlementSubject::parse(subject)
                                .map(Self::SettlementNotice);
                        }
                        let (attempt, tag) = rest.split_once('/')?;
                        if attempt == "outgoing" {
                            let (to, rest) = tag.split_once('/')?;
                            let (id, hash) = rest.split_once('/')?;
                            return Some(Self::RestoredDelivery {
                                to: b64_32(to)?,
                                msg_id: msg_id(id)?,
                                hash: b64_32(hash)?,
                            });
                        }
                        if let (Some(counterparty), Some(from)) =
                            (b64_32(attempt), tag.strip_prefix("lost-"))
                        {
                            return Some(Self::BlockedLost {
                                counterparty,
                                from_ms: number(from)?,
                            });
                        }
                        if let (Some(counterparty), Some(rest)) =
                            (b64_32(attempt), tag.strip_prefix("dropped-"))
                        {
                            let (family, from) = rest.split_once('-')?;
                            if !matches!(family, "pd" | "ud" | "kw" | "bd") {
                                return None;
                            }
                            return Some(Self::DroppedLost {
                                counterparty,
                                family: family.into(),
                                from_ms: number(from)?,
                            });
                        }
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
    fn test_restored_publication_keeps_each_batch_of_each_original() {
        let own = [8; 32];
        let binder = Binder::unblinded(own);
        let original = PublicationKey {
            recipient: [2; 32],
            generation: 3,
            hash: [4; 32],
        };
        let other = PublicationKey {
            hash: [5; 32],
            ..original.clone()
        };
        let mut restored = crate::publish::RestoredPublications::default();
        for (key, id) in [
            (&original, [7; 16]),
            (&other, [9; 16]),
            (&original, [6; 16]),
            (&original, [7; 16]),
        ] {
            restored
                .pending(key.key().as_bytes(), &id, &binder)
                .unwrap();
        }
        let entries: std::collections::BTreeMap<_, _> = restored
            .writes()
            .unwrap()
            .into_iter()
            .filter_map(|w| {
                let LocalWrite::Put { key, value } = w else {
                    return None;
                };
                key.starts_with(key::PUBLICATIONS.as_bytes())
                    .then(|| publication_entry(&key, &value, &own).unwrap())
            })
            .collect();
        assert_eq!(entries[&original], [[6; 16], [7; 16]].into());
        assert_eq!(entries[&other], [[9; 16]].into());
        let key = original.key();
        assert!(publication_entry(key.as_bytes(), &[7; 15], &own).is_err());
        assert!(publication_entry(key.as_bytes(), &[7; 32], &own).is_err());
        assert!(publication_entry(key.as_bytes(), &[7; 16], &original.recipient).is_err());
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

    #[test]
    fn test_offline_batch_id_requires_its_key_and_native_width() {
        let attempt = AttemptId::from_bytes([5; 16]);
        let marker = key::offline_batch(&attempt);
        let binder = Binder::unblinded([1; 32]);
        let read = |key: &str, bytes: &[u8]| restored::<[u8; 16]>(key.as_bytes(), bytes, &binder);
        assert_eq!(read(&marker, &[7; 16]).unwrap(), [7; 16]);
        assert!(read(&marker, &[7; 15]).is_err());
        assert!(read(&key::journal(&attempt), &[7; 16]).is_err());
    }

    #[test]
    fn test_ledger_batch_ids_bind_to_a_proposal_and_remain_distinct_across_restore() {
        let record = [3; 32];
        let hash = pubky_switch_core::enc::b64(&[4; 32]);
        let key = format!("{}{hash}", key::ledger_batches(&record));
        let binder = Binder::unblinded([1; 32]);
        let read = |key: &str, ids: &[u8]| restored::<LedgerBatchIds>(key.as_bytes(), ids, &binder);
        assert_eq!(
            read(&key, &[[1; 16], [2; 16]].concat()).unwrap(),
            [[1; 16], [2; 16]]
        );
        assert!(read(&key, &[0; 15]).is_err());
        assert!(read(&key, &[]).is_err());
        assert!(read(&key, &[[1; 16], [1; 16]].concat()).is_err());
        assert!(read(&key, &[[2; 16], [1; 16]].concat()).is_err());
        assert!(read(&key::executor_checkpoint(&record), &[1; 16]).is_err());
    }

    /// `value` read at `key` as a `T`, through a reader without the naming key.
    fn reads<T: Stored>(key: &str, value: serde_json::Value) -> bool {
        let bytes = serde_json::to_vec(&serde_json::json!({ "v": 1, "d": value })).unwrap();
        bound::<T>(key.as_bytes(), &bytes, &Binder::unblinded([1; 32])).is_ok()
    }

    #[test]
    fn test_loss_current_boundary_and_statement_bind_to_their_keys() {
        let loss = |generation, declared, until| {
            serde_json::json!({
                "generation": generation, "declared_at_ms": declared, "uncertain_until_ms": until
            })
        };
        assert!(reads::<LossBoundary>(key::LOSS_BOUNDARY, loss(2, 5, 9)));
        assert!(reads::<LossBoundary>(key::LOSS_BOUNDARY, loss(2, 5, 5)));
        assert!(reads::<LossBoundary>(key::LOSS_BOUNDARY, loss(2, 0, 9)));
        assert!(!reads::<LossBoundary>(key::LOSS_BOUNDARY, loss(2, 9, 5)));
        assert!(!reads::<LossBoundary>(key::LOSS_BOUNDARY, loss(0, 5, 9)));
        assert!(!reads::<LossBoundary>(key::MISSED_HOLD, loss(2, 5, 9)));
        let statement = key::not_paid_before_loss(&[2; 32]);
        assert!(reads::<bool>(&statement, serde_json::json!(true)));
        assert!(!reads::<bool>(&statement, serde_json::json!(false)));
    }

    #[test]
    fn test_loss_recorded_binds_a_generation_after_another_only() {
        assert!(reads::<u64>(key::LOSS_RECORDED, serde_json::json!(2)));
        assert!(!reads::<u64>(key::LOSS_RECORDED, serde_json::json!(0)));
        assert!(!reads::<u64>(key::DENIED, serde_json::json!(2)));
    }

    #[test]
    fn test_loss_current_boundary_requires_exact_members() {
        let loss = serde_json::json!({
            "generation": 2, "declared_at_ms": 5, "uncertain_until_ms": 9
        });
        let mut unknown = loss.clone();
        unknown["extra"] = serde_json::json!(true);
        assert!(!reads::<LossBoundary>(key::LOSS_BOUNDARY, unknown));
        for member in ["generation", "declared_at_ms", "uncertain_until_ms"] {
            let mut missing = loss.clone();
            missing.as_object_mut().unwrap().remove(member);
            assert!(!reads::<LossBoundary>(key::LOSS_BOUNDARY, missing));
            let mut null = loss.clone();
            null[member] = serde_json::Value::Null;
            assert!(!reads::<LossBoundary>(key::LOSS_BOUNDARY, null));
            let bytes = format!(
                r#"{{"v":1,"d":{{"generation":2,"declared_at_ms":5,"uncertain_until_ms":9,"{member}":7}}}}"#
            );
            assert!(bound::<LossBoundary>(
                key::LOSS_BOUNDARY.as_bytes(),
                bytes.as_bytes(),
                &Binder::unblinded([1; 32])
            )
            .is_err());
        }
    }

    #[test]
    fn test_floor_kept_binds_to_its_key_and_is_never_empty() {
        use crate::floor::FloorKept;
        let kept = serde_json::json!({ "seen_ms": 70, "live_in": 2, "divergent": true });
        assert!(reads::<FloorKept>(key::FLOOR_KEPT, kept.clone()));
        assert!(reads::<FloorKept>(
            key::FLOOR_KEPT,
            serde_json::json!({ "seen_ms": null, "live_in": null, "divergent": true })
        ));
        assert!(!reads::<FloorKept>(key::BLOCK_FLOOR, kept.clone()));
        assert!(!reads::<FloorKept>(
            key::FLOOR_KEPT,
            serde_json::json!({ "seen_ms": null, "live_in": null, "divergent": false })
        ));
        // Every member is present: none missing, none unknown.
        for member in ["seen_ms", "live_in", "divergent"] {
            let mut missing = kept.clone();
            missing.as_object_mut().unwrap().remove(member);
            assert!(!reads::<FloorKept>(key::FLOOR_KEPT, missing), "{member}");
        }
        let mut unknown = kept;
        unknown["extra"] = serde_json::json!(true);
        assert!(!reads::<FloorKept>(key::FLOOR_KEPT, unknown));
    }

    #[test]
    fn test_hold_epoch_is_bound_to_its_key() {
        let epoch = crate::hold::HoldEpoch::new().unwrap();
        let body = serde_json::to_value(epoch).unwrap();
        assert!(reads::<crate::hold::HoldEpoch>(
            key::HOLD_EPOCH,
            body.clone()
        ));
        assert!(!reads::<crate::hold::HoldEpoch>(key::RESTORE_HOLD, body));
        assert!(!reads::<crate::hold::HoldEpoch>(
            key::HOLD_EPOCH,
            serde_json::json!("short")
        ));
    }

    #[test]
    fn test_hold_epoch_confirmations_are_keyed_by_their_hold() {
        let record = [7; 32];
        let epoch = crate::hold::HoldEpoch::new().unwrap();
        for (key, family) in [
            (
                key::hold_confirmed(&record, &epoch),
                StateKey::HoldConfirmed {
                    record,
                    epoch: *epoch.as_bytes(),
                },
            ),
            (
                key::hold_confirmed_before(&record, &epoch),
                StateKey::HoldConfirmedBefore {
                    record,
                    epoch: *epoch.as_bytes(),
                },
            ),
        ] {
            assert_eq!(StateKey::parse(key.as_bytes()), Some(family));
            assert!(reads::<bool>(&key, serde_json::json!(true)));
            assert!(!reads::<crate::hold::HoldEpoch>(
                &key,
                serde_json::to_value(epoch).unwrap()
            ));
        }
        // A key per record with no hold identifier is not a key the SDK writes.
        for prefix in [key::HOLD_CONFIRMED, key::HOLD_CONFIRMED_BEFORE] {
            let bare = format!("{prefix}{}", pubky_switch_core::enc::b64(&record));
            assert_eq!(StateKey::parse(bare.as_bytes()), None);
        }
    }

    #[test]
    fn test_recovery_checkpoint_empty_waiting_refused() {
        assert!(!reads::<OwnResendOwed>(
            key::OWN_RESEND_OWED,
            serde_json::json!({ "waiting": [], "restored_at_ms": null })
        ));
    }

    #[test]
    fn test_recovery_checkpoint_rejects_inconsistent_asked_list() {
        for (asks_queued, asked) in [
            (true, vec![[1; 32]]),
            (false, vec![[1; 32], [1; 32]]),
            (false, vec![[2; 32], [1; 32]]),
        ] {
            let hold = RestoreHold {
                asks_queued,
                asked,
                verdict: crate::hold::RestoreVerdict::NotClean,
                ..RestoreHold::of(0, None)
            };
            assert!(!reads::<RestoreHold>(
                key::RESTORE_HOLD,
                serde_json::to_value(hold).unwrap(),
            ));
        }
    }

    #[test]
    fn test_restore_hold_load_and_restore_require_an_owed_resend() {
        use crate::hold::RestoreVerdict::{Clean, NotClean, Open};

        let check = |hold: RestoreHold, valid: bool| {
            let bytes = encode(&hold).unwrap();
            let key = key::RESTORE_HOLD.as_bytes();
            let binder = Binder::unblinded([1; 32]);
            assert_eq!(bound::<RestoreHold>(key, &bytes, &binder).is_ok(), valid);
            assert_eq!(restored::<RestoreHold>(key, &bytes, &binder).is_ok(), valid);
        };
        let initial = RestoreHold::of(0, None);
        let carried = RestoreHold::of(0, Some(&initial));
        for verdict in [Open, Clean, NotClean] {
            check(
                RestoreHold {
                    verdict,
                    earlier_asks: true,
                    ..initial.clone()
                },
                false,
            );
        }
        for (verdict, base, partial_is_valid) in [
            (Open, &initial, false),
            (Clean, &initial, false),
            (NotClean, &initial, true),
            (Open, &carried, true),
            (Clean, &carried, true),
            (NotClean, &carried, true),
        ] {
            let hold = RestoreHold {
                verdict,
                ..base.clone()
            };
            check(hold.clone(), true);
            check(
                RestoreHold {
                    asked: vec![[1; 32]],
                    ..hold
                },
                partial_is_valid,
            );
        }
    }

    #[test]
    fn test_recovery_checkpoint_optional_time_and_waiting_preserved() {
        for waiting in [None, Some(vec![[1; 32]]), Some(vec![[1; 32], [2; 32]])] {
            for restored_at_ms in [None, Some(7)] {
                assert!(reads::<OwnResendOwed>(
                    key::OWN_RESEND_OWED,
                    serde_json::to_value(OwnResendOwed {
                        waiting: waiting.clone(),
                        restored_at_ms,
                    })
                    .unwrap()
                ));
            }
        }
    }

    #[test]
    fn test_recovery_checkpoint_partial_and_completed_asks_preserved() {
        for (asks_queued, asked) in [
            (false, vec![]),
            (false, vec![[1; 32]]),
            (false, vec![[1; 32], [2; 32]]),
            (true, vec![]),
        ] {
            assert!(reads::<RestoreHold>(
                key::RESTORE_HOLD,
                serde_json::to_value(RestoreHold {
                    resend_from_ms: 1,
                    asks_queued,
                    asked,
                    verdict: crate::hold::RestoreVerdict::NotClean,
                    ..RestoreHold::of(0, None)
                })
                .unwrap()
            ));
        }
    }

    #[test]
    fn test_state_lost_owed_bound_to_its_sender_and_request() {
        let id = "6f1c2b8e-3d4a-4f5b-9c6d-7e8f9a0b1c2d";
        let owed = |sender: u8, earliest: u64, latest: u64, msg_id: &str| {
            serde_json::json!({
                "sender": pubky_switch_core::enc::b64(&[sender; 32]),
                "earliest_ms": earliest, "latest_ms": latest, "raised_ms": 9,
                "msg_id": msg_id, "hash": pubky_switch_core::enc::b64(&[3; 32]),
            })
        };
        let at = key::lost_owed(&[1; 32], id);
        assert!(reads::<crate::marks::LostOwed>(&at, owed(1, 5, 6, id)));
        assert!(!reads::<crate::marks::LostOwed>(&at, owed(2, 5, 6, id)));
        assert!(!reads::<crate::marks::LostOwed>(&at, owed(1, 7, 6, id)));
        let other = "7a2d3c9f-4e5b-4a6c-8d7e-8f9a0b1c2d3e";
        assert!(!reads::<crate::marks::LostOwed>(&at, owed(1, 5, 6, other)));
        assert!(!reads::<crate::marks::LostOwed>(
            &key::dropped_blocked(&[1; 32]),
            owed(1, 5, 6, id)
        ));
        // Every member is written: one without the time it was raised at is no run's.
        let mut partial = owed(1, 5, 6, id);
        partial.as_object_mut().unwrap().remove("raised_ms");
        assert!(!reads::<crate::marks::LostOwed>(&at, partial));
        assert!(StateKey::parse(key::lost_owed(&[1; 32], "not-a-uuid").as_bytes()).is_none());
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
        // An own block or unblock, and the deny list's copy.
        for mark in [
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
        assert!(reads::<bool>(
            key::MISSED_END_UNKNOWN,
            serde_json::json!(true)
        ));
        assert!(!reads::<bool>(
            key::MISSED_END_UNKNOWN,
            serde_json::json!(false)
        ));
        assert!(reads::<MissedHold>(
            key::MISSED_HOLD,
            serde_json::json!({ "bound_ms": 9 })
        ));
        assert!(!reads::<MissedHold>(
            key::RESTORE_HOLD,
            serde_json::json!({ "bound_ms": 9 })
        ));
    }

    #[test]
    fn test_state_send_exemption_bound_to_its_message() {
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let other = "223e4567-e89b-42d3-a456-426614174000";
        // Why an acceptance may go while sends are held, of the message its key names, read
        // at runtime and at a restore alike.
        let binder = Binder::unblinded([1; 32]);
        for exemption in [
            crate::send_holds::SendExemption::dispatched(id),
            crate::send_holds::SendExemption::confirmed_while_blocked(id, [3; 32], [4; 32]),
        ] {
            let bytes = encode(&exemption).unwrap();
            let read = |key: &str| {
                (
                    bound::<crate::send_holds::SendExemption>(key.as_bytes(), &bytes, &binder),
                    restored::<crate::send_holds::SendExemption>(key.as_bytes(), &bytes, &binder),
                )
            };
            let (runtime, restore) = read(&key::send_exemption(id));
            assert_eq!(runtime.unwrap(), exemption);
            assert_eq!(restore.unwrap(), exemption);
            for key in [key::send_exemption(other), key::side(id)] {
                let (runtime, restore) = read(&key);
                assert!(runtime.is_err() && restore.is_err(), "{key}");
            }
        }
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
        // The restored-record scan binds to its own key and keeps counterparties in order.
        let owed = |waiting: serde_json::Value| serde_json::json!({ "waiting": waiting, "restored_at_ms": null });
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
        // With the restore's time.
        let stamped = serde_json::json!({ "waiting": [one], "restored_at_ms": 7 });
        assert!(reads::<OwnResendOwed>(
            key::OWN_RESEND_OWED,
            stamped.clone()
        ));
        let read: OwnResendOwed = serde_json::from_value(stamped).unwrap();
        assert_eq!(read.restored_at_ms, Some(7));
        // A restore's hold at its key only.
        let hold = serde_json::to_value(RestoreHold::of(1, None)).unwrap();
        assert!(reads::<RestoreHold>(key::RESTORE_HOLD, hold.clone()));
        assert!(!reads::<RestoreHold>(key::CURSOR_TIME, hold));
        // A queued ask of the sender and time its key names.
        let ask = |from: u64| serde_json::json!({ "sender": sender, "from_ms": from, "until_ms": null, "queued": null });
        assert!(reads::<QueuedAsk>(&key::ask(&pubky, 5), ask(5)));
        assert!(!reads::<QueuedAsk>(&key::ask(&pubky, 5), ask(6)));
        assert!(!reads::<QueuedAsk>(&key::ask(&[4; 32], 5), ask(5)));
        // A side of the message its key names.
        let side =
            |msg_id: &str| serde_json::json!({ "msg_id": msg_id, "side": "payer", "label": null });
        assert!(reads::<KeptSide>(&key::side(id), side(id)));
        assert!(!reads::<KeptSide>(&key::side(id), side(other)));
        // A held answer of the payee and message its key names.
        let held = |payee: &str, msg_id: &str| {
            serde_json::json!({
                "payee": payee,
                "msg_id": msg_id,
                "request_hash": sender,
                "restored": false,
            })
        };
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
    fn test_local_state_scalar_refuses_unknown_envelope_members() {
        assert!(decode::<u64>(br#"{"v":1,"d":7,"extra":{"nested":[1]}}"#).is_err());
    }

    #[test]
    fn test_local_state_envelope_requires_current_members() {
        assert_eq!(decode::<u64>(&encode(&7u64).unwrap()).unwrap(), 7);
        for bytes in [
            br#"{"d":7}"#.as_slice(),
            br#"{"v":1}"#,
            br#"{"v":null,"d":7}"#,
            br#"{"v":2,"d":7}"#,
        ] {
            assert!(decode::<u64>(bytes).is_err());
        }
    }

    #[test]
    fn test_local_state_block_requires_batch_member() {
        assert!(!reads::<OwnBlockMark>(
            &key::own_block(&[1; 32]),
            serde_json::json!({ "blocked": true }),
        ));
    }

    #[test]
    fn test_local_state_block_refuses_unknown_member() {
        assert!(!reads::<OwnBlockMark>(
            &key::own_block(&[1; 32]),
            serde_json::json!({ "blocked": true, "batch": null, "extra": {"nested": [1]} }),
        ));
    }

    #[test]
    fn test_local_state_block_preserves_current_nullable_values() {
        for mark in [
            OwnBlockMark {
                blocked: true,
                batch: None,
            },
            OwnBlockMark {
                blocked: true,
                batch: Some([7; 16]),
            },
            OwnBlockMark {
                blocked: false,
                batch: Some([7; 16]),
            },
        ] {
            let bytes = encode(&mark).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(value["d"].as_object().unwrap().contains_key("batch"));
            assert_eq!(decode::<OwnBlockMark>(&bytes).unwrap(), mark);
            assert!(reads::<OwnBlockMark>(
                &key::own_block(&[1; 32]),
                value["d"].clone()
            ));
        }
    }

    #[test]
    fn test_local_state_block_refuses_invalid_current_values() {
        let key = key::own_block(&[1; 32]);
        for value in [
            serde_json::json!({ "batch": null }),
            serde_json::json!({ "blocked": false, "batch": null }),
            serde_json::json!({ "blocked": true, "batch": vec![7; 15] }),
            serde_json::json!({ "blocked": true, "batch": "invalid" }),
        ] {
            assert!(!reads::<OwnBlockMark>(&key, value));
        }
    }

    #[test]
    fn test_state_writes_are_local_writes_read_under_the_protocol_prefix() {
        let mut tx = Tx::default();
        tx.put("peer/a", &1u64).unwrap();
        tx.put("peer/b", &2u64).unwrap();
        tx.delete("peer/c");
        let writes = tx.into_writes();
        assert_eq!(writes.len(), 3);
        // The client qualifies each key with its registered protocol name.
        let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new());
        let mut batch = pubky_switch_client::store::WriteBatch::new();
        for write in &writes {
            if let LocalWrite::Put { key, value } = write {
                batch
                    .put(
                        "client/x",
                        [
                            PROTOCOL_PREFIX,
                            crate::protocol::BACKUP_NAME.as_bytes(),
                            b"/",
                            key,
                        ]
                        .concat(),
                        value.clone(),
                    )
                    .unwrap();
            }
        }
        store.commit(batch).unwrap();
        let held = store.get("client/x", b"p/paykit/peer/b").unwrap().unwrap();
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
        let hold = crate::hold::HoldEpoch::new().unwrap();
        let app = [4u8; 16];
        let attempt = AttemptId::from_bytes([5; 16]);
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let cases: Vec<(String, StateKey)> = vec![
            (
                key::publication_history(&pubky),
                StateKey::PublicationHistory(pubky),
            ),
            (key::journal(&attempt), StateKey::Journal(attempt)),
            (
                key::offline_batch(&attempt),
                StateKey::OfflineBatch(attempt),
            ),
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
                key::kept_cancellation(&pubky, &[5; 32], 1),
                StateKey::KeptCancellation {
                    record: pubky,
                    cert: [5; 32],
                    generation: 1,
                },
            ),
            (key::CURSOR_TIME.into(), StateKey::CursorTime),
            (key::RESTORE_HOLD.into(), StateKey::RestoreHold),
            (key::HOLD_EPOCH.into(), StateKey::HoldEpoch),
            (key::MISSED_HOLD.into(), StateKey::MissedHold),
            (key::LOSS_BOUNDARY.into(), StateKey::LossBoundary),
            (
                key::not_paid_before_loss(&pubky),
                StateKey::NotPaidBeforeLoss(pubky),
            ),
            (key::MISSED_END_UNKNOWN.into(), StateKey::MissedEndUnknown),
            (key::RECORDS_UNLISTED.into(), StateKey::RecordsUnlisted),
            (key::MISSED_UNLISTED.into(), StateKey::MissedUnlisted),
            (key::MISSED_CONSENTS.into(), StateKey::MissedConsents),
            (key::PAYER_PASS_OWED.into(), StateKey::PayerPassOwed),
            (key::OWN_RESEND_OWED.into(), StateKey::OwnResendOwed),
            (key::LISTED_CERT.into(), StateKey::ListedCert),
            (
                key::hold_confirmed(&pubky, &crate::hold::HoldEpoch::from_bytes([4; 16])),
                StateKey::HoldConfirmed {
                    record: pubky,
                    epoch: [4; 16],
                },
            ),
            (
                key::hold_confirmed_before(&pubky, &crate::hold::HoldEpoch::from_bytes([4; 16])),
                StateKey::HoldConfirmedBefore {
                    record: pubky,
                    epoch: [4; 16],
                },
            ),
            (
                key::restore_consent(&pubky),
                StateKey::RestoreConsent(pubky),
            ),
            (
                key::restore_approval(&pubky),
                StateKey::RestoreApproval(pubky),
            ),
            (
                key::execution_approval(&pubky, None),
                StateKey::ExecutionApproval {
                    record: pubky,
                    hold: None,
                },
            ),
            (
                key::execution_approval(&pubky, Some(&hold)),
                StateKey::ExecutionApproval {
                    record: pubky,
                    hold: Some(*hold.as_bytes()),
                },
            ),
            (key::LISTS_READ.into(), StateKey::ListsRead),
            (key::SERVICE_TIME.into(), StateKey::ServiceTime),
            (key::DENIED.into(), StateKey::Denied),
            (key::own_block(&pubky), StateKey::OwnBlock(pubky)),
            (
                format!("{}{}-7", key::TAKEN, "05".repeat(16)),
                StateKey::Taken(format!("{}-7", "05".repeat(16))),
            ),
            (key::held_list(&pubky), StateKey::HeldList(pubky)),
            (
                key::list_record_lost(&pubky),
                StateKey::ListRecordLost(pubky),
            ),
            (key::side(id), StateKey::Side(id.into())),
            (key::send_exemption(id), StateKey::SendExemption(id.into())),
            (key::queued_stop(id), StateKey::QueuedStop(id.into())),
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
                key::dropped_blocked(&pubky),
                StateKey::DroppedBlocked { sender: pubky },
            ),
            (
                key::blocked_widening(&[7; 16]),
                StateKey::BlockedWidening([7; 16]),
            ),
            (
                key::lost_owed(&pubky, id),
                StateKey::LostOwed {
                    sender: pubky,
                    msg_id: id.into(),
                },
            ),
            (key::FLAGGED.into(), StateKey::Flagged),
            (
                key::reply_asks(&pubky),
                StateKey::ReplyAsks { sender: pubky },
            ),
            (
                key::dropped_lost(&pubky, "pd", 7),
                StateKey::DroppedLost {
                    counterparty: pubky,
                    family: "pd".into(),
                    from_ms: 7,
                },
            ),
            (key::FLOOR_KEPT.into(), StateKey::FloorKept),
            (
                key::identity_notice(crate::notices::NoticeKind::BlockFloorHeld),
                StateKey::IdentityNotice(crate::notices::NoticeKind::BlockFloorHeld),
            ),
            (
                key::identity_notice(crate::notices::NoticeKind::FloorRaiseWaiting),
                StateKey::IdentityNotice(crate::notices::NoticeKind::FloorRaiseWaiting),
            ),
            (
                key::identity_notice(crate::notices::NoticeKind::AutomaticPaymentsHeld),
                StateKey::IdentityNotice(crate::notices::NoticeKind::AutomaticPaymentsHeld),
            ),
            (key::LOSS_RECORDED.into(), StateKey::LossRecorded),
            (key::RAISE_RETRY.into(), StateKey::RaiseRetry),
            (key::BLOCK_FLOOR.into(), StateKey::BlockFloor),
            (key::FLOOR_BATCH.into(), StateKey::FloorBatch),
            (key::floor_opened(&pubky), StateKey::FloorOpened(pubky)),
            (
                key::blocked_lost(&pubky, 9),
                StateKey::BlockedLost {
                    counterparty: pubky,
                    from_ms: 9,
                },
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
                key::extension(&pubky, id),
                StateKey::Extension {
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
            (
                key::executor_checkpoint(&pubky),
                StateKey::ExecutorCheckpoint(pubky),
            ),
            (
                format!(
                    "{}{}",
                    key::ledger_compactions(&pubky),
                    pubky_switch_core::enc::b64(&[4; 32])
                ),
                StateKey::LedgerCompaction {
                    ledger: pubky,
                    application: [4; 32],
                },
            ),
            (
                key::pending_conflict(&pubky),
                StateKey::PendingConflict(pubky),
            ),
            (
                key::pending_retirement(&pubky),
                StateKey::PendingRetirement(pubky),
            ),
            (
                key::pending_reply(&pubky, &pubky),
                StateKey::PendingReply {
                    record: pubky,
                    reply: pubky,
                },
            ),
            (key::applied_reply(&pubky), StateKey::AppliedReply(pubky)),
            (
                key::original_allowance(&pubky),
                StateKey::OriginalAllowance(pubky),
            ),
            (
                key::allowance_evidence(&pubky),
                StateKey::AllowanceEvidence(pubky),
            ),
            (key::conflict(&pubky), StateKey::Conflict(pubky)),
            (key::notice_target(id), StateKey::NoticeTarget(id.into())),
            (key::end_origin(id), StateKey::EndOrigin(id.into())),
            (key::end_batch(id), StateKey::EndBatch(id.into())),
            (key::grant_batch(&pubky), StateKey::GrantBatch(pubky)),
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

    #[test]
    fn test_discovery_state_ask_version_requires_named_app() {
        let value = serde_json::to_value(ListAsk {
            payee: [3; 32],
            app_id: None,
            held_version: Some(paykit_lib::ListVersion::new(1, 9).unwrap()),
            asked_at_ms: 7,
        })
        .unwrap();
        assert!(!reads::<ListAsk>(&key::list_ask(&[3; 32], None), value));
    }

    #[test]
    fn test_discovery_state_ask_version_refuses_repeated_members() {
        let key = key::list_ask(&[3; 32], Some(&[4; 16]));
        let held = r#"{"counter":9,"generation":1}"#;
        let valid = String::from_utf8(
            encode(&ListAsk {
                payee: [3; 32],
                app_id: Some([4; 16]),
                held_version: Some(paykit_lib::ListVersion::new(1, 9).unwrap()),
                asked_at_ms: 7,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(valid.contains(held));
        let read = |version: &str| {
            let bytes = valid.replace(held, version);
            bound::<ListAsk>(
                key.as_bytes(),
                bytes.as_bytes(),
                &Binder::unblinded([1; 32]),
            )
        };
        assert!(read(held).is_ok());
        assert!(read(r#"{"generation":1,"generation":2,"counter":9}"#).is_err());
        assert!(read(r#"{"generation":1,"counter":9,"counter":8}"#).is_err());
    }

    #[test]
    fn test_discovery_state_ask_current_bindings() {
        for (app_id, held_version) in [
            (None, None),
            (Some([4; 16]), None),
            (
                Some([4; 16]),
                Some(paykit_lib::ListVersion::new(1, 9).unwrap()),
            ),
        ] {
            let value = serde_json::to_value(ListAsk {
                payee: [3; 32],
                app_id,
                held_version,
                asked_at_ms: u64::MAX,
            })
            .unwrap();
            assert!(reads::<ListAsk>(
                &key::list_ask(&[3; 32], app_id.as_ref()),
                value.clone()
            ));
            assert!(!reads::<ListAsk>(
                &key::list_ask(&[5; 32], app_id.as_ref()),
                value
            ));
        }
    }
}
