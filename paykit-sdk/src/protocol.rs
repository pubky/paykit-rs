//! [`PaykitProtocol`]: Paykit as the client's [`Protocol`], the rules the app layer can't
//! know (Switch spec 1).
//!
//! - **Members and scopes:** the opaque members of Paykit's bodies, which scopes have keys and
//!   check 8 with labels, all from `paykit-lib` (Paykit spec 3, Paykit wire 1.1, 3.1, 4.1).
//! - **Kinds an app takes:** its record's `accepts` lists the kinds its scopes receive that
//!   this SDK handles; a sender wraps a kind to any app whose scopes receive it, by Paykit's
//!   table (Paykit spec 3.3), and narrows a payer's replies by label (Paykit spec 3.4).
//! - **Check 10** ([`Protocol::check_body`]): the kind and its body, with no effect, the one
//!   place these rules run, so a push peek and the receive step agree.
//! - **Receiving** ([`Protocol::receive_with`]): the effect of a message that passed check 10,
//!   built through the client's receive context so the record write it causes, any claim entry and
//!   local write go in the batch that moves the cursor (Switch spec 8.2; `crate::receive`).
//!   After `409 conflict` the client reads the records again and calls it once more, and the
//!   effect is built again from them.
//! - **Collections** with their fixed policies, `payee_activity` with one policy per label,
//!   and the marks that stop a record write from dropping a message in a role an app has
//!   seen (Paykit spec 6, Paykit wire 5.1, 5.3, 5.4).
//! - **Re-sends:** entries the app can't read, `missed` items and a provider loss queue
//!   `resend_request`s (Paykit spec 9).
//!
//! Security: check 8 and the label rules decide which apps send and take each kind, and the
//! record marks keep an own app from silently dropping a message another app recorded
//! (Switch spec 8.2). Destinations never come from shared state alone (Paykit spec 6).

use std::sync::{Arc, PoisonError, RwLock};

use paykit_lib::bodies::check_body;
use paykit_lib::collections::Collection;
use paykit_lib::kinds::{targets_app, Kind, Role as KindRole, Side, OPAQUE_MEMBERS};
use paykit_lib::records::{ActivitySide, PaymentListRecord, StoredMessage};
use paykit_lib::scopes::{acts_as_payee, check_certificate, PaykitScope, ScopeName};
use paykit_lib::{Label, PaykitKindRules};
use pubky_switch_client::app::{
    decode_local_state, Collection as SwitchCollection, Effect, LocalView, LocalWrite, LossActions,
    MissedEntries, OpaqueRule, Outcome, Protocol, ProtocolLayer, ProviderLoss, ReceiveContext,
    ReceivedMessage, ResendNeeded, RestoreContext, StoreFailure, WriteBase,
};
use pubky_switch_client::error::StoreError;
use pubky_switch_core::enc::{serde_b64, unb64_array};
use pubky_switch_core::message::Message;
use pubky_switch_core::receive::KindRules;
use pubky_switch_core::records::{AppCert, AppRecord};
use pubky_switch_core::scopes as switch_scopes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::activity::{self, KeptSide, SideTag};
use crate::hold::RestoreHold;
use crate::journal::JournalEntry;
use crate::marks::{DropMark, KeyWaitMark};
use crate::notices::KeptNotice;
use crate::pay::ListAsk;
use crate::rail::Rail;
use crate::receive::{self, Fail, QueuedAnswer};
use crate::records::KeptMessage;
use crate::state::{self, key, StateKey};

/// The kinds this SDK handles (Paykit wire 4.3): the one-time request flow, payments without
/// a request, the payer's side of Private Payment Lists and re-sends. Recurring requests,
/// Allowances and publishing Private Payment Lists come later. A
/// `private_payment_list_request` this SDK receives is taken with no effect: it publishes no
/// list at all yet, so the Publishing rule's answer to a request naming a used version
/// (Paykit spec 8.4) comes with list publishing, with that rule's other triggers.
pub const HANDLED_KINDS: [Kind; 9] = [
    Kind::PaymentRequest,
    Kind::PaymentRequestAcceptance,
    Kind::PaymentRequestRejection,
    Kind::PaymentRequestCancellation,
    Kind::PaymentProof,
    Kind::PaymentReceipt,
    Kind::ResendRequest,
    Kind::PrivatePaymentList,
    Kind::PrivatePaymentListRequest,
];

/// A re-send to ask of a sender (`p/ask/`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QueuedAsk {
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    pub(crate) from_ms: u64,
    pub(crate) until_ms: Option<u64>,
}

/// A `missed` item's mark (`p/mi/<from>`): the time its re-sends ask from, and the
/// counterparties asked already for it, kept while a record whose counterparty it may still
/// have to ask is sealed under a scope key the app doesn't hold yet (`resend.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MissedItemMark {
    pub(crate) from_ms: u64,
    /// In increasing order, each once.
    pub(crate) asked: Vec<[u8; 32]>,
}

/// The own app, as the protocol's callbacks need it.
#[derive(Clone)]
struct Own {
    identity: [u8; 32],
    cert: AppCert,
}

/// Paykit as the client's [`Protocol`].
///
/// One instance per app: it learns the app's identity and certificate once the app has
/// joined or loaded ([`PaykitProtocol::set_own`]); [`crate::Paykit`] does both. Its
/// callbacks read Paykit's own local state through what the client hands them (the receive
/// context, a [`LocalView`]), and never hold the naming key: they blind through the client.
pub struct PaykitProtocol {
    own: RwLock<Option<Own>>,
    /// The app's rail, which names a proof's settlement (Paykit wire 1.5); without one, the
    /// methods `paykit-lib` knows.
    rail: Option<Arc<dyn Rail>>,
}

impl std::fmt::Debug for PaykitProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PaykitProtocol")
    }
}

impl Default for PaykitProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl PaykitProtocol {
    /// The protocol of one app, before it knows the app.
    pub fn new() -> Self {
        Self {
            own: RwLock::new(None),
            rail: None,
        }
    }

    /// The protocol of one app paying and paid through `rail`, which names the settlement of
    /// a proof of its method (Paykit wire 1.5, spec 8.4).
    pub fn with_rail(rail: Arc<dyn Rail>) -> Self {
        Self {
            rail: Some(rail),
            ..Self::new()
        }
    }

    /// The settlement ID of a proof's `proof` object: the rail's, or else the one
    /// `paykit-lib` knows.
    fn settlement_id(&self, proof: &Map<String, Value>) -> Option<Vec<u8>> {
        match &self.rail {
            Some(rail) => rail.settlement_id(proof),
            None => paykit_lib::derive::settlement_id(proof),
        }
    }

    /// Set the own identity and certificate, after a join, a load or a renewal.
    pub fn set_own(&self, identity: [u8; 32], cert: AppCert) {
        let mut own = self.own.write().unwrap_or_else(PoisonError::into_inner);
        *own = Some(Own { identity, cert });
    }

    fn own(&self) -> Option<Own> {
        self.own
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The side a payer reply goes from (Paykit spec 3.4): a cancellation's is kept with the
    /// batch that queued it; without that, a labelled one is a payer's, and an unlabelled one
    /// from an app that acts only as payee is the payee's.
    fn reply_side(&self, kind: Kind, label: Option<&Label>, kept: Option<SideTag>) -> Side {
        if kind != Kind::PaymentRequestCancellation {
            return Side::Payer;
        }
        match kept {
            Some(SideTag::Payee) => Side::Payee,
            Some(SideTag::Payer) => Side::Payer,
            None if label.is_some() => Side::Payer,
            None => match self.own() {
                Some(own)
                    if acts_as_payee(own.cert.scopes())
                        && !own.cert.scopes().iter().any(|s| s == "pay") =>
                {
                    Side::Payee
                }
                _ => Side::Payer,
            },
        }
    }

    fn narrows(&self, message: &Message, recipient: &AppRecord, kept: Option<SideTag>) -> bool {
        let Some(kind) = Kind::from_kind(&message.kind) else {
            return true;
        };
        if !kind.is_payer_reply() {
            return true;
        }
        let label = body_label(message);
        let side = self.reply_side(kind, label.as_ref(), kept);
        targets_app(kind, side, label.as_ref(), recipient.cert().scopes())
    }
}

/// Check 10 of a message that passed checks 1 to 9 (Paykit wire 4.1), with no effect: a kind
/// this SDK handles, and a body that parses for it, within the size of a recorded kind. The
/// one place these rules run: the client runs it before the receive step, and a push peek
/// runs it alone. `Err` names the rule that drops the message.
pub(crate) fn check_message(message: &ReceivedMessage) -> Result<(), String> {
    let handled =
        Kind::from_kind(&message.message.kind).is_some_and(|kind| HANDLED_KINDS.contains(&kind));
    if !handled {
        return Err("Paykit wire 4.1 check 10: a kind this app doesn't handle".into());
    }
    check_body(&message.message, message.plaintext.json())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The stored copy of a message whose body passed check 10, as its records hold it
/// (Paykit wire 5.2, Switch spec 8.2): nothing is checked again.
pub(crate) fn stored_copy(message: &ReceivedMessage) -> Result<StoredMessage, String> {
    StoredMessage::from_received(
        message.sender,
        message.cert.clone(),
        message.plaintext.clone(),
        message.message.clone(),
        Some(message.accepted_at_ms),
    )
    .map_err(|error| error.to_string())
}

/// Whether a certificate's scopes hold one of `roles` (Paykit spec 3.3).
pub(crate) fn holds_role(scopes: &[String], roles: &[KindRole]) -> bool {
    roles.iter().any(|role| match role {
        KindRole::Pay => scopes.iter().any(|s| s == "pay"),
        KindRole::Request => acts_as_payee(scopes),
        KindRole::Allowance => scopes.iter().any(|s| s == "allowance"),
        KindRole::AllowanceExecute => scopes.iter().any(|s| s == "allowance_execute"),
        // Any Paykit App: another protocol's scopes alone take no Paykit kind (Switch wire
        // 3.1 "Several protocols").
        KindRole::Any => paykit_lib::scopes::holds_paykit_scope(scopes),
    })
}

/// The label in a message body, if it is a valid one.
pub(crate) fn body_label(message: &Message) -> Option<Label> {
    message
        .body
        .get("label")
        .and_then(serde_json::Value::as_str)
        .and_then(|l| Label::parse(l).ok())
}

/// A local write of Paykit's own state, for a protocol call that commits it; one that can't
/// be encoded is the store failing for the entry.
fn local_write<T: serde::Serialize>(key: &str, value: &T) -> Result<LocalWrite, StoreFailure> {
    state::local_put(key, value).map_err(|_| StoreFailure::of_entry(StoreError::Corrupt))
}

/// A failure of the client's store as a protocol call's failure.
fn store_failure(error: pubky_switch_client::app::AppError) -> StoreFailure {
    match error {
        pubky_switch_client::app::AppError::Store(error) => StoreFailure::from(error),
        _ => StoreFailure::unavailable(StoreError::Unavailable),
    }
}

/// The name of Paykit's layer in app backups (Switch spec 12.4).
pub const BACKUP_NAME: &str = "paykit";

/// The writes that install Paykit's backup layer (Switch spec 12.4): every key of its local
/// state, as the default layer holds it, with the payment journal checked whole first and
/// its entries marked restored (Paykit spec 7.2). The journal's checks are
/// [`JournalEntry::check_restored`]'s, as every runtime read checks an entry, its claim
/// keys blinded under the backup's naming key, each entry at its own attempt's key, and at
/// most one open entry
/// for one obligation, since a payment in progress blocks any other. `Err` names the rule,
/// without content: nothing is written. Pure: it is called at backup time too.
///
/// The layer's schema is closed (`state::StateKey`): a key no builder of the SDK writes is
/// refused, every key is parsed whole, and every value is read as its family's and bound
/// to what its key names.
///
/// Every message another party signed that the layer keeps is verified again for `own`, as
/// its later use verifies it, and bound to the record key it is kept under, from `blind`,
/// the backup's naming key: a journal entry's request and carrier
/// ([`JournalEntry::check_restored`]), a kept cancellation (`kc/`), a kept payer's reply
/// (`pr/`), a held list (`lh/`),
/// a queued `resend_request` (`ans/`: the requester's certificate, a `resend_request`'s
/// body, and the `seq` and `msg_id` its key names), and a payee app's records seen (`ar/`,
/// bound to the payee and app its key names). The rest is the app's own data, read as its
/// family's and bound to its key (`ask/`, times and counts, `side/`, notices `nt/`): a
/// backup carries the app's signing keys, so whoever can write one can sign anything the
/// app can, and a check of what the app signed guards against corruption only.
pub(crate) fn restore_paykit_layer(
    layer: Option<&ProtocolLayer>,
    own: [u8; 32],
    own_cert: &AppCert,
    blind: impl Fn(&[u8]) -> [u8; 32],
) -> Result<Vec<LocalWrite>, String> {
    // Every value is bound to its key by the function every runtime read binds it with
    // (`state::Stored`), with the backup's naming key.
    let blinding = |input: &[u8]| blind(input);
    let binder = state::Binder::new(own, own_cert, &blinding);
    let Some(layer) = layer else {
        return Err("a Paykit App's backup carries Paykit's layer, with its journal".into());
    };
    let state = decode_local_state(&layer.data)
        .ok_or_else(|| String::from("Paykit's layer doesn't read as local state"))?;
    let mut open = std::collections::BTreeSet::new();
    let mut journaled = std::collections::BTreeSet::new();
    let mut acknowledged = Vec::new();
    let mut writes = Vec::with_capacity(state.len());
    let mut cursor_time_ms = 0;
    for (key, value) in state {
        // The schema is closed: a key no builder of the SDK writes is refused, and every
        // value is read as its family's, bound to what its key names (`state::StateKey`).
        let Some(family) = StateKey::parse(&key) else {
            return Err(
                "Paykit's local state in the backup holds a key the SDK doesn't write".into(),
            );
        };
        let unread = || String::from("Paykit's local state in the backup doesn't read");
        let attempt = match family {
            // A hold, or a user's confirmation of one, belongs to a restore before: this one
            // sets its own (`hold.rs`).
            // So do the marks of records not yet listed and of the payer pass owed: this
            // restore sets its own.
            // The certificate last listed under is the backup's own: this restore sets it to
            // the restored one, which its listing covers.
            // A stored entry's note is the client's entry's: the client tells of the entries it
            // restores again, and the note is kept anew then (`stored.rs`). A taken event's
            // mark isn't restored: the client tells the protocol again of the events a restore
            // carries, under the same IDs in place or new ones elsewhere, and taking an event
            // again changes nothing: a dropped entry's time is already in its sender's mark,
            // which asks on its own cadence (`marks.rs`).
            StateKey::RestoreHold
            | StateKey::MissedHold
            | StateKey::HoldConfirmed(_)
            | StateKey::RecordsUnlisted
            | StateKey::MissedUnlisted
            | StateKey::PayerPassOwed
            | StateKey::OwnResendOwed
            | StateKey::ListedCert
            | StateKey::Flagged
            | StateKey::StoredNote { .. }
            | StateKey::Taken(_) => continue,
            // The time of the last read of the lists isn't restored: the copy of the deny
            // list the backup holds may be stale by any time, so the list is read again before
            // any payment (Paykit spec 8.1).
            StateKey::ListsRead => continue,
            // This app's blocks not yet shown by the list are: the client's kept batches
            // belong to the device that kept them, so the restored app keeps counting each
            // and writes it again (`Paykit::send_own_blocks_again`). Its unblocks aren't: an unblock counts
            // only while the client keeps its batch, and the restored app keeps none, so the
            // list decides.
            StateKey::OwnBlock(_) => {
                let mark = state::restored::<state::OwnBlockMark>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                if mark.blocked() {
                    writes.push(LocalWrite::Put { key, value });
                }
                continue;
            }
            StateKey::Journal(attempt) => attempt,
            StateKey::CursorTime => {
                cursor_time_ms =
                    state::restored::<u64>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::Missed { .. }
            | StateKey::Peer(_)
            | StateKey::Spam(_)
            | StateKey::Resent { .. }
            | StateKey::ResentDay { .. }
            | StateKey::BlockFloor => {
                state::restored::<u64>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::Ask { .. } => {
                state::restored::<QueuedAsk>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::Answer { .. } => {
                state::restored::<QueuedAnswer>(&key, &value, &binder).map_err(|_| {
                    String::from(
                        "Paykit spec 9: a queued resend_request in the backup doesn't verify",
                    )
                })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::MissedItem { .. } => {
                state::restored::<MissedItemMark>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::DroppedReplies { .. }
            | StateKey::DroppedUnverified { .. }
            | StateKey::DroppedBlocked { .. } => {
                // A mark is restored whole, its times and when it was last asked, so a restore
                // neither loses nor resets it (Paykit spec 8.1), bound to its key's sender.
                state::restored::<DropMark>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::KeyWaits { .. } => {
                // So is a mark of key waits, with the epochs it waits for.
                state::restored::<KeyWaitMark>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::Denied => {
                state::restored::<Vec<[u8; 32]>>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::Side(_) => {
                state::restored::<KeptSide>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            // A held answer is restored: the record the backup's device wrote holds it, and the
            // restored app queues it from there, or tells the host it was given up, once sends
            // to its payee aren't held (`Paykit::queue_own_again`).
            StateKey::HeldAnswer { .. } => {
                state::restored::<crate::hold::HeldAnswer>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::KeptCancellation(_) => {
                state::restored::<KeptMessage>(&key, &value, &binder).map_err(|_| {
                    String::from(
                        "Paykit spec 8.1: a kept cancellation in the backup doesn't verify",
                    )
                })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::HeldList(_) => {
                state::restored::<KeptMessage>(&key, &value, &binder).map_err(|_| {
                    String::from("Paykit wire 5.4: a held list in the backup doesn't verify")
                })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::ListAsk { .. } => {
                state::restored::<ListAsk>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::SeenRecords { .. } => {
                // They are the payee's, not the app's own: one that doesn't verify would fail
                // every later public-endpoint read of that payee.
                state::restored::<crate::app_records::SeenRecords>(&key, &value, &binder)
                    .map_err(|_| {
                        String::from(
                            "Switch spec 4.2: a payee's seen app records in the backup don't verify",
                        )
                    })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::KeptReply { .. } => {
                state::restored::<KeptMessage>(&key, &value, &binder).map_err(|_| {
                    String::from(
                        "Switch spec 8.2: a kept payer's reply in the backup doesn't verify",
                    )
                })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::BlockedWidening { .. } => {
                state::restored::<crate::marks::BlockedWindow>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::MessageNotice { .. }
            | StateKey::Notice { .. }
            | StateKey::BlockedLost { .. } => {
                let notice =
                    state::restored::<KeptNotice>(&key, &value, &binder).map_err(|_| unread())?;
                // An acknowledged notice of an attempt is kept only while the journal holds
                // the attempt (`Paykit::acknowledge_notice`): one without its entry is a
                // state no run leaves.
                if let (true, StateKey::Notice { attempt, .. }) = (notice.acknowledged, family) {
                    acknowledged.push(attempt);
                }
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
        };
        let bad = || String::from("Paykit spec 7.2: a journal entry in the backup doesn't read");
        // Bound to its key as every runtime read binds it (`state::Stored`): of the attempt
        // its key names, each request payment's claim key the one its payee and request
        // blind to under the backup's naming key, which the restore context gives (Paykit
        // spec 7.1, Paykit wire 1.5), its record the one its request or attempt names, and
        // every message it keeps verified for this app
        // ([`JournalEntry::check_restored`]): checked here, before anything is written.
        let mut entry =
            state::restored::<JournalEntry>(&key, &value, &binder).map_err(|_| bad())?;
        if entry.open() && !open.insert(entry.obligation) {
            return Err("Paykit spec 7.2: two open journal entries for one obligation".into());
        }
        journaled.insert(attempt);
        entry.restored = true;
        let value = state::encode(&entry).map_err(|_| bad())?;
        writes.push(LocalWrite::Put { key, value });
    }
    if acknowledged
        .iter()
        .any(|attempt| !journaled.contains(attempt))
    {
        return Err("Paykit's local state in the backup doesn't read".into());
    }
    // Every open request is held after a restore, set in the restore's own commit (Paykit
    // spec 7.2, "What a restore loses"; `hold.rs`).
    let hold = state::local_put(key::RESTORE_HOLD, &RestoreHold::of(cursor_time_ms))
        .map_err(|_| String::from("the restore's hold can't be written"))?;
    writes.push(hold);
    // The client's copy of the shared records is empty until they are listed again: until
    // then a record's absence means nothing (`Paykit::records_known`).
    let unlisted = state::local_put(key::RECORDS_UNLISTED, &true)
        .map_err(|_| String::from("the restore's records mark can't be written"))?;
    writes.push(unlisted);
    // No certificate is kept as listed under, a backup's included: only a listing Paykit
    // makes keeps one (`Paykit::list_records`), so the restored copy counts as unlisted
    // until then, whichever certificate the client lists under.
    // No new attempt until a reconcile has run the payer pass, under `pay`: the backup
    // doesn't hold the attempts made after it (`Paykit::payer_pass_owed`).
    let owed = state::local_put(key::PAYER_PASS_OWED, &true)
        .map_err(|_| String::from("the restore's payer pass mark can't be written"))?;
    writes.push(owed);
    // The messages this app signed that a batch the backup kept unanswered queued are in no
    // outbox: they are queued again from the records once they are listed
    // (`Paykit::queue_own_again`).
    let resend = state::local_put(key::OWN_RESEND_OWED, &crate::hold::OwnResendOwed::default())
        .map_err(|_| String::from("the restore's re-send mark can't be written"))?;
    writes.push(resend);
    // What a blocked counterparty's inbox accepted after the backup is gone with the
    // restore: every counterparty the deny list holds at the next read of the lists is
    // marked from the backup's cursor time, to ask once it is unblocked (`marks.rs`).
    let from = cursor_time_ms.saturating_sub(pubky_switch_core::records::CLOCK_TOLERANCE_MS);
    let widening = state::local_put(
        &key::blocked_widening(from),
        &crate::marks::BlockedWindow {
            from_ms: from,
            until_ms: crate::marks::BlockedWindow::TO_NOW,
        },
    )
    .map_err(|_| String::from("the restore's block widening can't be written"))?;
    writes.push(widening);
    Ok(writes)
}

impl Protocol for PaykitProtocol {
    fn backup_name(&self) -> Option<String> {
        Some(BACKUP_NAME.to_owned())
    }

    /// Paykit's layer is its whole local state, as the client's default writes it; a
    /// restore checks the certificate it installs, and checks and marks the journal in the
    /// layer, before anything is written.
    fn restore_layer(
        &self,
        layer: Option<&ProtocolLayer>,
        restoring: &RestoreContext<'_>,
    ) -> Result<Vec<LocalWrite>, String> {
        // The certificate the restore installs passes Paykit's checks as every other does
        // (Paykit wire 3.1). Checked here, before the restore's one commit, a backup whose
        // certificate the Switch takes and Paykit refuses writes nothing.
        check_certificate(restoring.cert())
            .map_err(|error| format!("the backup's certificate: {error}"))?;
        restore_paykit_layer(layer, restoring.identity(), restoring.cert(), |input| {
            restoring.blind(input)
        })
    }

    fn opaque_members(&self) -> Vec<OpaqueRule> {
        OPAQUE_MEMBERS
            .iter()
            .map(|m| OpaqueRule {
                kind: m.kind.map(str::to_owned),
                member: m.member.to_owned(),
            })
            .collect()
    }

    /// Paykit's scopes of Paykit spec 3.1 and their labels have keys; extension scopes and
    /// any other sub-scope of a Paykit scope don't (Paykit spec 3.5, Paykit wire 3.1).
    fn has_keys(&self, scope: &str) -> bool {
        match PaykitScope::classify(scope) {
            ScopeName::Paykit(paykit) => paykit.has_keys(),
            _ => false,
        }
    }

    fn sent_between_own_apps(&self, kind: &str) -> bool {
        Kind::from_kind(kind).is_some_and(Kind::sent_between_own_apps)
    }

    /// Check 8 with Paykit's certificate checks and labels (Paykit wire 3.1, 4.1).
    fn check_scopes(
        &self,
        message: &Message,
        sender: &AppCert,
        own: &AppCert,
    ) -> Result<(), String> {
        // Under an own certificate Paykit refuses, such as a renewal's the Switch took, the
        // message isn't judged by it: the receive step holds the entry until a certificate
        // that passes replaces it, and this check runs again then (Paykit wire 3.1).
        if check_certificate(own).is_err() {
            return Ok(());
        }
        PaykitKindRules::for_certificate(own)
            .check_scopes(message, sender)
            .map_err(|error| error.to_string())
    }

    /// The kinds this SDK handles that the certificate's scopes receive (Switch spec 3.4):
    /// none for a certificate Paykit's verifiers refuse (Paykit wire 3.1), such as a
    /// renewal's that the Switch took.
    fn accepts(&self, cert: &AppCert) -> Vec<String> {
        if check_certificate(cert).is_err() {
            return Vec::new();
        }
        HANDLED_KINDS
            .iter()
            .filter(|kind| holds_role(cert.scopes(), kind.recipients()))
            .map(|kind| kind.as_str().to_owned())
            .collect()
    }

    /// Whether another app's scopes receive `kind` by Paykit's table (Paykit spec 3.3), which
    /// a sender checks before it wraps the kind to that app, and before
    /// [`Protocol::sends_to`] narrows by its scopes. An app whose certificate Paykit's
    /// verifiers refuse (Paykit wire 3.1) receives none: the Switch takes such a
    /// certificate, Paykit doesn't.
    fn receives(&self, kind: &str, cert: &AppCert) -> bool {
        check_certificate(cert).is_ok()
            && Kind::from_kind(kind).is_some_and(|k| holds_role(cert.scopes(), k.recipients()))
    }

    /// The label narrowing of Paykit spec 3.4: a payer's acceptance, rejection,
    /// cancellation and proof about a labelled request go only to apps holding `request` or
    /// that label, and about an unlabelled request only to apps holding `request`; a payee's
    /// cancellation, without a label, to apps holding `pay` or `request`.
    fn sends_to(&self, message: &Message, recipient: &AppRecord) -> bool {
        self.narrows(message, recipient, None)
    }

    /// [`Protocol::sends_to`], with the side a cancellation was sent from as the batch that
    /// queued it kept it.
    fn sends_to_with(
        &self,
        message: &Message,
        recipient: &AppRecord,
        view: &LocalView<'_>,
    ) -> bool {
        // A kept side that can't be read sends to no one rather than guess (Paykit spec 3.4):
        // the message isn't sealed to this recipient. So does one that names another message
        // than its key, which doesn't bind (`state::Stored`).
        match state::view_get::<KeptSide>(view, &key::side(&message.msg_id)) {
            Ok(kept) => self.narrows(message, recipient, kept.map(|kept| kept.side)),
            Err(_) => false,
        }
    }

    /// Check 10 (Paykit wire 4.1), with no effect: a kind this SDK handles ([`HANDLED_KINDS`])
    /// and a body that parses for it, within the size of a recorded kind.
    fn check_body(&self, message: &ReceivedMessage) -> Result<(), String> {
        check_message(message)
    }

    /// The effect of a message that passed check 10, added to the batch that moves the
    /// cursor (Switch spec 8.2; `crate::receive`). Called again after `409 conflict`, with
    /// the records read again: the effect is built anew from them.
    fn receive_with(&self, message: &ReceivedMessage, cx: &mut ReceiveContext<'_>) -> Outcome {
        // A released entry whose effect the app wrote since, by a write of its own, is
        // declined: its effect isn't applied twice (Switch spec 8.2; `stored.rs`).
        if message.released {
            let note_key = key::stored_note(&message.sender, &message.message.msg_id);
            match state::cx_get::<crate::stored::StoredNote>(cx, &note_key) {
                Ok(note) if crate::stored::declines(note.as_ref()) => {
                    return Outcome::Drop {
                        reason: "Switch spec 8.2: a released entry whose effect the app wrote \
                                 since"
                            .into(),
                    }
                }
                Ok(_) => {}
                Err(Fail::Store(failure)) => return Outcome::Retry(failure),
                Err(_) => return Outcome::Retry(StoreFailure::of_entry(StoreError::Corrupt)),
            }
        }
        // A message given up stays processed (Paykit spec 8.1): a copy the sender sends again,
        // or the parked entry the host releases, has no effect while its notice is kept. One
        // the client dropped while it waited for a key was never processed: a copy sent again
        // is a new message.
        let notice = key::message_notice(&message.sender, &message.message.msg_id);
        match state::cx_get::<KeptNotice>(cx, &notice) {
            Ok(Some(notice)) if notice.keeps_processed() => {
                return Outcome::Drop {
                    reason: "Paykit spec 8.1: a message given up stays processed".into(),
                }
            }
            Ok(_) => {}
            Err(Fail::Store(failure)) => return Outcome::Retry(failure),
            Err(_) => return Outcome::Retry(StoreFailure::of_entry(StoreError::Corrupt)),
        }
        let stored = match stored_copy(message) {
            Ok(stored) => stored,
            Err(reason) => return Outcome::Drop { reason },
        };
        match receive::effect(cx, &stored, message.seq, &|proof| self.settlement_id(proof)) {
            // A write the context couldn't build for want of a key: the context holds the
            // entry itself, whatever is returned (Switch spec 3.5).
            Ok(()) | Err(Fail::NotNow) => Outcome::Apply(Effect::default()),
            Err(Fail::Need(needs)) => Outcome::Need(needs),
            Err(Fail::Drop(reason)) => Outcome::Drop { reason },
            Err(Fail::Store(failure)) => Outcome::Retry(failure),
            // A record value waits for its scope key: the client stores the entry and passes
            // it at once, and runs the step again once the key comes (Switch spec 8.2). One
            // it drops past its bounds first was never processed: the SDK tells the host and
            // marks its sender to ask again once the key is held (`stored.rs`, `marks.rs`).
            Err(Fail::AwaitsKey { data_epoch }) => Outcome::AwaitKey { data_epoch },
        }
    }

    /// Entries the app can't read: ask the sender to send again (Paykit spec 9).
    fn resend_needed(&self, needed: &ResendNeeded) -> Result<Vec<LocalWrite>, StoreFailure> {
        let ask = QueuedAsk {
            sender: needed.sender,
            from_ms: needed.accepted_from_ms,
            until_ms: Some(needed.accepted_until_ms),
        };
        Ok(vec![local_write(
            &key::ask(&needed.sender, needed.accepted_from_ms),
            &ask,
        )?])
    }

    /// Entries missed while lagging: ask the counterparties of open requests and unfinished
    /// activity to send again (Paykit spec 9; `resend.rs`), and hold the open requests
    /// (Paykit spec 7.2; `hold.rs`): a payee's cancellation may be among
    /// the entries the service deleted. Every request the app holds is held, and one first
    /// seen later only when created before the item's last acceptance time plus the clock
    /// tolerance, the time the item carries (`MissedEntries::accepted_until_ms`). The
    /// confirmations of an earlier hold go in the same commit, so none releases the new
    /// one. The copy of the shared records counts as not listed from the item on (`ml`,
    /// Paykit spec 8.1, "Records not listed yet"), until Paykit's own listing at the end of
    /// the receive (`Paykit::list_records`): the client lists them right after the item,
    /// inside its read, but Paykit judges the copy listed only from a listing of its own,
    /// so what the app decides between the item and that listing decides nothing from an
    /// absence. A mark of its own, not `nl`, which the same read's end would clear.
    fn missed(&self, missed: &MissedEntries) -> Result<Vec<LocalWrite>, StoreFailure> {
        Ok(vec![
            local_write(
                &key::missed_item(missed.accepted_from_ms),
                &MissedItemMark {
                    from_ms: missed.accepted_from_ms,
                    asked: Vec::new(),
                },
            )?,
            local_write(
                key::MISSED_HOLD,
                &crate::hold::MissedHold::of_item(missed.accepted_until_ms),
            )?,
            local_write(key::MISSED_UNLISTED, &true)?,
            // What a blocked counterparty's inbox accepted among the missed entries is asked
            // for once it is unblocked: every counterparty the deny list holds at the next
            // read of the lists is marked for the item's times (`marks.rs`). An item that
            // gives no end (`u64::MAX`, kept owed by an older build) runs to that read, as
            // the missed hold reads it unbounded: an unknown end never narrows the window.
            local_write(
                &key::blocked_widening(missed.accepted_from_ms),
                &crate::marks::BlockedWindow {
                    from_ms: missed.accepted_from_ms,
                    until_ms: if missed.accepted_until_ms == u64::MAX {
                        crate::marks::BlockedWindow::TO_NOW
                    } else {
                        missed.accepted_until_ms
                    },
                },
            )?,
            LocalWrite::DeletePrefix {
                prefix: key::HOLD_CONFIRMED.as_bytes().to_vec(),
            },
        ])
    }

    /// A provider loss, without the protocol's state at hand: only the re-sends are queued.
    fn provider_lost(&self, loss: &ProviderLoss) -> Result<LossActions, StoreFailure> {
        Ok(LossActions {
            conversations: Vec::new(),
            local: vec![local_write(
                &key::missed(loss.accepted_from_ms),
                &loss.accepted_from_ms,
            )?],
        })
    }

    /// A provider loss (Switch spec 12.6 steps 2 and 4): ask every counterparty with Paykit
    /// activity to send again from the old cursor's time, and add their conversation entries
    /// back.
    fn provider_lost_with(
        &self,
        loss: &ProviderLoss,
        view: &LocalView<'_>,
    ) -> Result<LossActions, StoreFailure> {
        // A store that fails, or a peer key that doesn't read, is the step's failure, never
        // an empty list of counterparties: the recovery asks again.
        let peers = state::view_scan::<u64>(view, key::PEERS).map_err(|error| match error {
            crate::PaykitError::Client(error)
                if error != pubky_switch_client::app::AppError::Store(StoreError::Corrupt) =>
            {
                store_failure(error)
            }
            _ => StoreFailure::of_entry(StoreError::Corrupt),
        })?;
        let mut conversations = Vec::with_capacity(peers.len());
        for (rest, _) in peers {
            let peer = unb64_array::<32>(&rest)
                .map_err(|_| StoreFailure::of_entry(StoreError::Corrupt))?;
            conversations.push(peer);
        }
        let mut actions = self.provider_lost(loss)?;
        actions.conversations = conversations;
        Ok(actions)
    }

    fn adds_conversation(&self, kind: &str) -> bool {
        // Paykit wire 4.12: a resend_request adds no conversation entry.
        kind != Kind::ResendRequest.as_str()
    }

    /// Paykit's collections with their fixed policies (Paykit spec 6, Paykit wire 5.1):
    /// `payee_activity` with one policy per label, so an app holding `request` lists and
    /// takes every label's records, and one holding `request:<label>` its label's.
    fn collections(&self) -> Vec<SwitchCollection> {
        crate::hold::RecordSide::All.collections()
    }

    /// The mark of what the app has seen in a record: the newest value it took that reads
    /// (Paykit wire 5.3 "Seen roles", 5.4). A delete or a value that doesn't read keeps the
    /// mark held.
    fn record_seen(
        &self,
        collection: &str,
        seen: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        let own = self.own()?;
        let reads = match (Collection::from_name(collection)?, value) {
            (Collection::PayerActivity, Some(v)) => {
                activity::read(Some(v), &own.identity, ActivitySide::Payer).is_some()
            }
            (Collection::PayeeActivity, Some(v)) => {
                activity::read(Some(v), &own.identity, ActivitySide::Payee).is_some()
            }
            (Collection::PaymentLists, Some(v)) => {
                PaymentListRecord::read(v, &own.identity).is_ok()
            }
            _ => false,
        };
        match (reads, value) {
            (true, Some(v)) => Some(v.to_vec()),
            _ => seen.map(<[u8]>::to_vec),
        }
    }

    /// Whether a newer write goes back on what the app saw (Paykit wire 5.3, 5.4): drops or
    /// replaces a message in a role seen, or changes the Allowance seen; for a
    /// `payment_lists` record, holds an older list or drops a single-use endpoint marked
    /// used. A value that doesn't read is refused too. A delete isn't such a write. Judged
    /// as a reader that doesn't hold the version the write builds on
    /// ([`Protocol::record_regression_on`] is told).
    fn record_regression(
        &self,
        collection: &str,
        seen: &[u8],
        value: Option<&[u8]>,
    ) -> Option<String> {
        self.regression(collection, seen, value, false)
    }

    /// [`Protocol::record_regression`], told what the write builds on: a write built on the
    /// version this app holds is judged as by a reader holding it, which also refuses a
    /// reassociation at or before a period it attempted (Paykit wire 5.3 "Seen Allowance").
    /// The version is judged by the mark of what the app saw, the value it took last that
    /// reads. The client counts a create over a record it holds deleted as built on what it
    /// holds, so such a write is judged against the mark from before the delete: stricter
    /// than the wire format's reader holding the version built on, which a delete leaves with
    /// no attempted period.
    fn record_regression_on(
        &self,
        collection: &str,
        seen: &[u8],
        value: Option<&[u8]>,
        base: WriteBase,
    ) -> Option<String> {
        self.regression(collection, seen, value, base.held)
    }
}

impl PaykitProtocol {
    /// [`Protocol::record_regression`]'s rules; `built_on` when the write builds on the
    /// version this app holds.
    fn regression(
        &self,
        collection: &str,
        seen: &[u8],
        value: Option<&[u8]>,
        built_on: bool,
    ) -> Option<String> {
        let own = self.own()?;
        let value = value?;
        let collection = Collection::from_name(collection)?;
        match collection {
            Collection::PayerActivity | Collection::PayeeActivity => {
                let side = match collection {
                    Collection::PayerActivity => ActivitySide::Payer,
                    _ => ActivitySide::Payee,
                };
                let held = activity::read(Some(seen), &own.identity, side)?;
                let Some(later) = activity::read(Some(value), &own.identity, side) else {
                    return Some("Paykit wire 5.3: an activity record that doesn't read".into());
                };
                // The mark is the value the app saw. A write built on the version the app
                // holds is checked as by a reader holding it, the reassociation rule of that
                // version included; one built on a version it never took, such as one it
                // skipped by listing only the latest values after a `missed` item, is checked
                // only by the seen rules that hold whatever the base (Paykit wire 5.3 "Seen
                // Allowance").
                let checked = if built_on {
                    held.check_later_write_built_on(&later)
                } else {
                    held.check_later_write(&later)
                };
                checked.err().map(|e| e.to_string())
            }
            Collection::PaymentLists => {
                let held = PaymentListRecord::read(seen, &own.identity).ok()?;
                let Ok(later) = PaymentListRecord::read(value, &own.identity) else {
                    return Some(
                        "Paykit wire 5.4: a payment_lists record that doesn't read".into(),
                    );
                };
                // Wire 5.4 "Seen lists": a later list of a greater version, or the same list,
                // and never a single-use mark dropped, at any version.
                held.check_later_write(&later).err().map(|e| e.to_string())
            }
            _ => None,
        }
    }
}

/// Whether `scopes` holds the write scope of a payee record under `label`'s policy.
pub(crate) fn writes_payee_label(scopes: &[String], label: Option<&Label>) -> bool {
    match label {
        Some(label) => switch_scopes::holds(scopes, &label.scope()),
        None => scopes.iter().any(|s| s == "request"),
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    fn protocol() -> PaykitProtocol {
        PaykitProtocol::new()
    }

    #[test]
    fn test_protocol_keys_paykit_scopes_and_labels_only() {
        let p = protocol();
        assert!(p.has_keys("pay"));
        assert!(p.has_keys("request:shop1"));
        assert!(!p.has_keys("ext:paykit-cap"));
        assert!(!p.has_keys("pay:x"));
        assert!(!p.has_keys("other"));
    }

    #[test]
    fn test_protocol_resend_request_adds_no_conversation() {
        let p = protocol();
        assert!(!p.adds_conversation("paykit.resend_request"));
        assert!(p.adds_conversation("paykit.payment_request"));
    }

    #[test]
    fn test_protocol_roles_follow_the_scope_table() {
        let pay = vec!["pay".to_owned()];
        let shop = vec!["request:shop1".to_owned()];
        assert!(holds_role(&pay, Kind::PaymentRequest.recipients()));
        assert!(!holds_role(&shop, Kind::PaymentRequest.recipients()));
        assert!(holds_role(&shop, Kind::PaymentProof.recipients()));
        assert!(!holds_role(&pay, Kind::PaymentProof.recipients()));
    }

    #[test]
    fn test_protocol_payee_label_write_scope() {
        let label = Label::parse("shop1").unwrap();
        assert!(writes_payee_label(&["request".into()], Some(&label)));
        assert!(writes_payee_label(&["request:shop1".into()], Some(&label)));
        assert!(!writes_payee_label(&["request:shop2".into()], Some(&label)));
        assert!(!writes_payee_label(&["request:shop1".into()], None));
    }

    #[test]
    fn test_protocol_without_own_app_keeps_no_marks() {
        let p = protocol();
        assert_eq!(p.record_seen("payer_activity", None, Some(b"{}")), None);
        assert_eq!(
            p.record_regression("payer_activity", b"{}", Some(b"{}")),
            None
        );
    }
}

#[cfg(test)]
mod receive_tests {
    use paykit_lib::derive::natural_key;
    use paykit_lib::records::ActivityRecord;
    use pubky_switch_client::app::RecordTarget;
    use pubky_switch_core::ident::record_key_from_blinded;
    use pubky_switch_core::records::Standing;
    use serde_json::json;

    use super::*;
    use crate::activity::ActivityTarget;
    use crate::testkit::{object, Party, NOW};

    fn received(stored: &StoredMessage) -> ReceivedMessage {
        ReceivedMessage {
            seq: 3,
            sender: stored.sender(),
            accepted_at_ms: NOW,
            message: stored.message().clone(),
            plaintext: stored.plaintext().clone(),
            cert: stored.cert().clone(),
            standing: Standing::Listed,
            context: None,
            parked: false,
            released: false,
            conflict: None,
        }
    }

    /// The block widening a `missed` item owes, from `missed()`'s writes.
    fn widening(missed: &MissedEntries) -> serde_json::Value {
        let writes = PaykitProtocol::new().missed(missed).unwrap();
        let value = writes
            .iter()
            .find_map(|write| match write {
                LocalWrite::Put { key, value } if key.starts_with(b"bw/") => Some(value.clone()),
                _ => None,
            })
            .unwrap();
        serde_json::from_slice(&value).unwrap()
    }

    #[test]
    fn test_missed_widening_runs_to_the_read_when_the_item_gives_no_end() {
        let bounded = MissedEntries {
            seq: 1,
            entries: 1,
            accepted_from_ms: 10,
            accepted_until_ms: 20,
        };
        assert_eq!(widening(&bounded)["d"]["until_ms"], 20);
        // No end known: it runs to the read that applies it, never to its start alone.
        let unbounded = MissedEntries {
            accepted_until_ms: u64::MAX,
            ..bounded
        };
        assert_eq!(
            widening(&unbounded)["d"]["until_ms"],
            crate::marks::BlockedWindow::TO_NOW
        );
    }

    fn protocol_of(own: &Party) -> PaykitProtocol {
        let p = PaykitProtocol::new();
        p.set_own(own.id(), own.cert.clone());
        p
    }

    fn request_body() -> serde_json::Map<String, serde_json::Value> {
        object(json!({
            "amount": { "asset": "btc", "value": "1" },
            "payment_reference": "order-1",
            "proposal_expires_at": null
        }))
    }

    #[test]
    fn test_protocol_check_body_passes_a_valid_message_kept_as_stored() {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let stored = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let message = received(&stored);
        assert_eq!(protocol_of(&alice).check_body(&message), Ok(()));
        let kept = stored_copy(&message).unwrap();
        assert_eq!(kept.hash(), stored.hash());
        assert_eq!(kept.accepted_at(), Some(NOW));
    }

    #[test]
    fn test_protocol_check_body_drops_an_unhandled_kind_and_a_bad_body() {
        let alice = Party::new("alice", "wallet", &["pay", "allowance", "contacts"]);
        let bob = Party::new("bob", "shop", &["request", "allowance"]);
        let p = protocol_of(&alice);
        let stored = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let mut message = received(&stored);
        message.message.kind = "paykit.allowance_end".into();
        assert!(p.check_body(&message).is_err());
        let mut message = received(&stored);
        message.message.body.remove("amount");
        assert!(p.check_body(&message).is_err());
        // Refs the kind doesn't allow.
        let mut message = received(&stored);
        message.message.refs.push([1; 32]);
        assert!(p.check_body(&message).is_err());
    }

    #[test]
    fn test_protocol_sends_payer_replies_by_label() {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let shop1 = Party::new("bob", "shop1", &["request:shop1"]);
        let shop2 = Party::new("bob", "shop2", &["request:shop2"]);
        let accepts = ["paykit.payment_request_acceptance"];
        let p = protocol_of(&alice);
        let labelled = alice.message(
            &bob,
            "paykit.payment_request_acceptance",
            vec![[1; 32]],
            object(json!({ "label": "shop1" })),
        );
        assert!(p.sends_to(labelled.message(), &shop1.record(&accepts)));
        assert!(p.sends_to(labelled.message(), &bob.record(&accepts)));
        assert!(!p.sends_to(labelled.message(), &shop2.record(&accepts)));
        let plain = alice.message(
            &bob,
            "paykit.payment_request_acceptance",
            vec![[1; 32]],
            object(json!({})),
        );
        assert!(p.sends_to(plain.message(), &bob.record(&accepts)));
        assert!(!p.sends_to(plain.message(), &shop1.record(&accepts)));
    }

    #[test]
    fn test_protocol_accepts_only_handled_kinds_its_scopes_receive() {
        let p = protocol_of(&Party::new("alice", "wallet", &["pay", "contacts"]));
        let wallet = Party::new("alice", "wallet", &["pay", "allowance", "contacts"]);
        let kinds = p.accepts(&wallet.cert);
        assert!(kinds.contains(&"paykit.payment_request".to_owned()));
        assert!(!kinds.contains(&"paykit.payment_proof".to_owned()));
        assert!(!kinds.contains(&"paykit.allowance_proposal".to_owned()));
        // Another app's scopes receive by Paykit's table, handled here or not.
        assert!(p.receives("paykit.allowance_proposal", &wallet.cert));
        assert!(!p.receives("paykit.payment_proof", &wallet.cert));
    }

    #[test]
    fn test_protocol_cert_paykit_refuses_receives_and_accepts_nothing() {
        use pubky_switch_core::scopes::Permission;
        let p = protocol_of(&Party::new("alice", "wallet", &["pay", "contacts"]));
        // `request` needs exactly `conversation.add` (Paykit wire 3.1): one without it, or
        // with `lists.read` beside it, is a certificate the Switch takes and Paykit refuses.
        let missing = Party::with_permissions("bob", "shop", &["request"], &[]);
        let extra = Party::with_permissions(
            "bob",
            "shop",
            &["request"],
            &[Permission::ListsRead, Permission::ConversationAdd],
        );
        for app in [&missing, &extra] {
            assert!(!p.receives("paykit.payment_proof", &app.cert));
            assert!(p.accepts(&app.cert).is_empty());
        }
        let valid = Party::new("bob", "shop", &["request"]);
        assert!(p.receives("paykit.payment_proof", &valid.cert));
        assert!(!p.accepts(&valid.cert).is_empty());
    }

    #[test]
    fn test_protocol_pay_certificate_without_contacts_refused() {
        // A paying app reads the deny list (Paykit spec 3.1): a certificate holding `pay`
        // without `contacts`, its permissions exactly what its scopes need, is one the
        // Switch takes and Paykit's verifiers refuse, as a sender, a recipient and an own
        // certificate.
        let wallet = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let request = bob.message(&wallet, "paykit.payment_request", vec![], request_body());
        let pay_only = Party::new("alice", "phone", &["pay"]);
        let p = protocol_of(&wallet);
        assert!(!p.receives("paykit.payment_request", &pay_only.cert));
        assert!(p.accepts(&pay_only.cert).is_empty());
        let proof = Message::new(
            "paykit.payment_proof",
            pubky_switch_core::message::Sender::of_app(pay_only.id(), &pay_only.keys),
            bob.id(),
            NOW,
            vec![[1; 32]],
            object(json!({})),
        )
        .unwrap();
        let error = protocol_of(&bob)
            .check_scopes(&proof, &pay_only.cert, &bob.cert)
            .unwrap_err();
        assert!(error.contains("pay requires contacts"), "{error}");
        assert!(pay_only
            .try_message(
                &bob,
                "paykit.payment_proof",
                vec![[1; 32]],
                object(json!({}))
            )
            .is_err());
        // Under a pay-only own certificate no message is judged, and nothing is accepted.
        assert_eq!(
            protocol_of(&pay_only).check_scopes(request.message(), &bob.cert, &pay_only.cert),
            Ok(())
        );
    }

    #[test]
    fn test_protocol_pay_certificate_with_contacts_accepted() {
        let wallet = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let p = protocol_of(&bob);
        assert!(p.receives("paykit.payment_request", &wallet.cert));
        assert!(p
            .accepts(&wallet.cert)
            .contains(&"paykit.payment_request".to_owned()));
        let request = bob.message(&wallet, "paykit.payment_request", vec![], request_body());
        assert_eq!(
            protocol_of(&wallet).check_scopes(request.message(), &bob.cert, &wallet.cert),
            Ok(())
        );
    }

    #[test]
    fn test_protocol_record_regression_judges_a_reassociation_by_the_version_built_on() {
        use paykit_lib::identifiers::AllowanceId;
        use paykit_lib::records::Reassociation;
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let p = protocol_of(&alice);
        let mut body = request_body();
        body.insert(
            "recurrence".into(),
            json!({ "anchor": "2026-01-31T00:00:00.000Z", "every": 1, "unit": "month",
                "starts_at": "2026-01-31T00:00:00.000Z", "ends_at": null }),
        );
        body.insert(
            "payment_deadline".into(),
            json!({ "type": "period_start", "seconds": 86_400 }),
        );
        let request = bob.message(&alice, "paykit.payment_request", vec![], body);
        let mut record = activity::empty(ActivitySide::Payer, bob.id());
        activity::place(&mut record, activity::Role::Request, request);
        record.allowance_id =
            Some(AllowanceId::parse("d0efffb7-191b-4709-b6c7-e343af5500b4").unwrap());
        let period = 1_772_236_800;
        // The app saw the period attempted early; the version that dropped it, once its
        // attempt ended, it missed; the write it gets moves the request from that period on.
        let seen = ActivityRecord {
            attempted_periods: vec![period],
            ..record.clone()
        };
        let moved = ActivityRecord {
            reassociations: vec![Reassociation {
                allowance_id: AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap(),
                from_period: period,
            }],
            ..record
        };
        let (seen, moved) = (seen.to_json().unwrap(), moved.to_json().unwrap());
        assert_eq!(
            p.record_regression("payer_activity", &seen, Some(&moved)),
            None
        );
        // Built on the version the app holds, the one that attempted the period, the same
        // write moves an attempted period: a regression.
        assert!(p
            .regression("payer_activity", &seen, Some(&moved), true)
            .is_some());
        assert_eq!(
            p.regression("payer_activity", &seen, Some(&moved), false),
            None
        );
        // Dropping the entry again is a regression.
        assert!(p
            .record_regression("payer_activity", &moved, Some(&seen))
            .is_some());
    }

    /// A stand-in for the naming key's blinding a restore is given.
    fn fake_blind(input: &[u8]) -> [u8; 32] {
        pubky_switch_core::enc::hash(input)
    }

    /// The record key of `target` under [`fake_blind`], as the client derives it.
    fn fake_key(target: &RecordTarget) -> [u8; 32] {
        let policy = target.collection.policy.hash().unwrap();
        record_key_from_blinded(
            &target.collection.name,
            &fake_blind(&target.natural_key),
            &policy,
        )
    }

    /// Whether `value` at `key` restores at `party`'s app, bound to its key as a runtime read
    /// binds it ([`state::restored`]).
    fn restores<T: state::Stored>(party: &Party, key: &[u8], value: &[u8]) -> bool {
        let blind = |input: &[u8]| fake_blind(input);
        state::restored::<T>(
            key,
            value,
            &state::Binder::new(party.id(), &party.cert, &blind),
        )
        .is_ok()
    }

    /// Alice's acceptance of bob's request `[4; 32]`, kept at bob's shop, and the record key
    /// of bob's payee record it names.
    fn kept_reply(alice: &Party, bob: &Party) -> (StoredMessage, Vec<u8>, [u8; 32]) {
        let reply = alice.message(
            bob,
            "paykit.payment_request_acceptance",
            vec![[4; 32]],
            object(json!({})),
        );
        let value = state::encode(&KeptMessage::of(&reply)).unwrap();
        let target = ActivityTarget::payee(natural_key::request_record(&bob.id(), &[4; 32]), None);
        (reply, value, fake_key(&target.record_target()))
    }

    #[test]
    fn test_protocol_restore_takes_a_kept_reply_at_its_record() {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let (reply, value, record) = kept_reply(&alice, &bob);
        let at = key::kept_reply(&alice.id(), &record, &reply.message().msg_id);
        assert!(restores::<KeptMessage>(&bob, at.as_bytes(), &value));
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_reply_at_another_key() {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let (reply, value, record) = kept_reply(&alice, &bob);
        let msg_id = reply.message().msg_id.clone();
        let other = "123e4567-e89b-42d3-a456-426614174000";
        for at in [
            key::kept_reply(&alice.id(), &[9; 32], &msg_id),
            key::kept_reply(&alice.id(), &record, other),
            key::kept_reply(&[8; 32], &record, &msg_id),
        ] {
            assert!(
                !restores::<KeptMessage>(&bob, at.as_bytes(), &value),
                "{at}"
            );
        }
        // A kind that isn't a payer's reply, naming the same request.
        let carol = Party::new("carol", "shop", &["request"]);
        let receipt = carol.message(
            &bob,
            "paykit.payment_receipt",
            vec![[4; 32], [5; 32]],
            object(json!({ "attempt_id": "DgCVYQKL1eyDacZvG6YhZg" })),
        );
        let value = state::encode(&KeptMessage::of(&receipt)).unwrap();
        let at = key::kept_reply(&carol.id(), &record, &receipt.message().msg_id);
        assert!(!restores::<KeptMessage>(&bob, at.as_bytes(), &value));
    }

    /// `value`, a kept message's encoded bytes, with its acceptance time removed.
    fn without_acceptance(value: &[u8]) -> Vec<u8> {
        let mut json: serde_json::Value = serde_json::from_slice(value).unwrap();
        json["d"]["accepted_at"] = serde_json::Value::Null;
        serde_json::to_vec(&json).unwrap()
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_reply_without_its_acceptance_time() {
        // A reply is kept only as its inbox accepted it: one without the time can't come
        // from a valid run.
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let (reply, value, record) = kept_reply(&alice, &bob);
        let at = key::kept_reply(&alice.id(), &record, &reply.message().msg_id);
        assert!(!restores::<KeptMessage>(
            &bob,
            at.as_bytes(),
            &without_acceptance(&value)
        ));
    }

    /// Bob's cancellation of his request `[4; 32]` to alice, kept, and the stop key of the
    /// payer's record it names.
    fn kept_cancellation(alice: &Party, bob: &Party) -> (Vec<u8>, Vec<u8>) {
        let cancellation = bob.message(
            alice,
            "paykit.payment_request_cancellation",
            vec![[4; 32]],
            object(json!({})),
        );
        let value = state::encode(&KeptMessage::of(&cancellation)).unwrap();
        let target = ActivityTarget::payer(natural_key::request_record(&bob.id(), &[4; 32]));
        let record = fake_key(&target.record_target());
        (key::kept_cancellation(&record).into_bytes(), value)
    }

    #[test]
    fn test_protocol_restore_takes_a_kept_cancellation_at_its_record() {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let (key, value) = kept_cancellation(&alice, &bob);
        assert!(restores::<KeptMessage>(&alice, &key, &value));
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_cancellation_at_another_key_or_without_its_time() {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let (key, value) = kept_cancellation(&alice, &bob);
        let other = key::kept_cancellation(&[9; 32]).into_bytes();
        assert!(!restores::<KeptMessage>(&alice, &other, &value));
        assert!(!restores::<KeptMessage>(
            &alice,
            &key,
            &without_acceptance(&value)
        ));
    }

    #[test]
    fn test_protocol_missed_marks_bound_to_their_time_and_asks() {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let time = state::encode(&5u64).unwrap();
        assert!(restores::<u64>(&alice, key::missed(5).as_bytes(), &time));
        assert!(!restores::<u64>(&alice, key::missed(6).as_bytes(), &time));
        let mark =
            |asked: Vec<[u8; 32]>| state::encode(&MissedItemMark { from_ms: 5, asked }).unwrap();
        let item = |from: u64| key::missed_item(from).into_bytes();
        assert!(restores::<MissedItemMark>(
            &alice,
            &item(5),
            &mark(vec![[1; 32], [2; 32]])
        ));
        assert!(!restores::<MissedItemMark>(
            &alice,
            &item(6),
            &mark(Vec::new())
        ));
        assert!(!restores::<MissedItemMark>(
            &alice,
            &item(5),
            &mark(vec![[2; 32], [1; 32]])
        ));
        assert!(!restores::<MissedItemMark>(
            &alice,
            &item(5),
            &mark(vec![[1; 32], [1; 32]])
        ));
        assert!(!restores::<MissedItemMark>(&alice, &item(5), &time));
    }

    #[test]
    fn test_protocol_stored_note_bound_to_its_key() {
        // A note read back names the sender and `msg_id` its key does, and so does the
        // message it keeps: one under another key is a corrupt store.
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let msg_id = request.message().msg_id.clone();
        let note = crate::stored::StoredNote {
            stored_id: 1,
            hash: request.hash(),
            sender: bob.id(),
            msg_id: msg_id.clone(),
            message: KeptMessage::of(&request),
            placed: false,
        };
        let value = state::encode(&note).unwrap();
        let at = key::stored_note(&bob.id(), &msg_id);
        assert!(restores::<crate::stored::StoredNote>(
            &alice,
            at.as_bytes(),
            &value
        ));
        assert!(crate::stored::writes_request(&note, &alice.id(), &request.hash()).unwrap());
        let other = key::stored_note(&bob.id(), "123e4567-e89b-42d3-a456-426614174000");
        let by_other = key::stored_note(&[8; 32], &msg_id);
        for at in [other, by_other] {
            assert!(!restores::<crate::stored::StoredNote>(
                &alice,
                at.as_bytes(),
                &value
            ));
        }
        // At its message's key, a note that names another `msg_id` or sender itself.
        for note in [
            crate::stored::StoredNote {
                msg_id: "123e4567-e89b-42d3-a456-426614174000".into(),
                ..note.clone()
            },
            crate::stored::StoredNote {
                sender: [8; 32],
                ..note.clone()
            },
        ] {
            let value = state::encode(&note).unwrap();
            assert!(!restores::<crate::stored::StoredNote>(
                &alice,
                at.as_bytes(),
                &value
            ));
        }
    }

    #[test]
    fn test_protocol_own_receipt_sent_again_after_a_restore() {
        // A restore queues again a payee's own receipt and cancellation from its record: this
        // app stores its own receipt only in the batch that queues it, so the record holds
        // one it sent (Paykit spec 7.2); never a message another app signed.
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let till = Party::new("bob", "till", &["request"]);
        let mut record = ActivityRecord::new(paykit_lib::records::ActivitySide::Payee, alice.id());
        let receipt = |by: &Party| {
            by.message(
                &alice,
                "paykit.payment_receipt",
                vec![[4; 32], [5; 32]],
                object(json!({ "attempt_id": "DgCVYQKL1eyDacZvG6YhZg" })),
            )
        };
        record.receipt = Some(receipt(&bob));
        let own = |record: &ActivityRecord| {
            crate::hold::own_to_send_again(record, &bob.id(), &bob.cert.app_id())
                .into_iter()
                .map(|stored| stored.message().kind.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(own(&record), ["paykit.payment_receipt"]);
        record.cancellation = Some(bob.message(
            &alice,
            "paykit.payment_request_cancellation",
            vec![[4; 32]],
            object(json!({ "reason": null })),
        ));
        assert_eq!(
            own(&record),
            [
                "paykit.payment_request_cancellation",
                "paykit.payment_receipt"
            ]
        );
        record.cancellation = Some(till.message(
            &alice,
            "paykit.payment_request_cancellation",
            vec![[4; 32]],
            object(json!({ "reason": null })),
        ));
        record.receipt = Some(receipt(&till));
        assert!(own(&record).is_empty());
    }

    #[test]
    fn test_protocol_record_regression_on_a_dropped_role() {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let p = protocol_of(&alice);
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let mut seen = activity::empty(ActivitySide::Payer, bob.id());
        activity::place(&mut seen, activity::Role::Request, request.clone());
        let seen = seen.to_json().unwrap();
        // The same record and one with more are fine.
        assert_eq!(
            p.record_regression("payer_activity", &seen, Some(&seen)),
            None
        );
        let cancel = bob.message(
            &alice,
            "paykit.payment_request_cancellation",
            vec![request.hash()],
            object(json!({})),
        );
        let mut more = ActivityRecord::read(&seen, &alice.id(), ActivitySide::Payer).unwrap();
        activity::place(&mut more, activity::Role::Cancellation, cancel);
        let more = more.to_json().unwrap();
        assert_eq!(
            p.record_regression("payer_activity", &seen, Some(&more)),
            None
        );
        // Dropping the request is a regression; so is a value that doesn't read.
        let dropped = activity::empty(ActivitySide::Payer, bob.id())
            .to_json()
            .unwrap();
        assert!(p
            .record_regression("payer_activity", &more, Some(&dropped))
            .is_some());
        assert!(p
            .record_regression("payer_activity", &seen, Some(b"{}"))
            .is_some());
        // A delete isn't one.
        assert_eq!(p.record_regression("payer_activity", &seen, None), None);
        // The mark follows the newest value that reads.
        assert_eq!(
            p.record_seen("payer_activity", Some(&seen), Some(&more)),
            Some(more.clone())
        );
        assert_eq!(
            p.record_seen("payer_activity", Some(&seen), Some(b"{}")),
            Some(seen)
        );
    }
}
