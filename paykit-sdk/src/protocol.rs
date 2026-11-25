//! [`PaykitProtocol`]: Paykit as the client's [`Protocol`], the rules the app layer can't
//! know (Dispatch spec 1).
//!
//! - **Members and scopes:** the opaque members of Paykit's bodies, which scopes have keys and
//!   check 8 with labels, all from `paykit-lib` (Paykit spec 3, Paykit wire 1.1, 3.1, 4.1).
//! - **Kinds an app takes:** its record's `accepts` lists the kinds its scopes receive that
//!   this SDK handles; a sender wraps a kind to any app whose scopes receive it, by Paykit's
//!   table (Paykit spec 3.3), and narrows a payer's replies by label (Paykit spec 3.4).
//! - **Check 10** ([`Protocol::check_body`]): the kind and its body, with no effect, the one
//!   place these rules run, so a push peek and the receive step agree.
//! - **Receiving** ([`Protocol::receive`]): the effect of a message that passed check 10,
//!   built through the client's receive context so the record write it causes, any claim entry and
//!   local write go in the batch that moves the cursor (Dispatch spec 8.2; `crate::receive`).
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
//! (Dispatch spec 8.2). Destinations never come from shared state alone (Paykit spec 6).

use std::sync::{Arc, PoisonError, RwLock};

use paykit_lib::bodies::check_body;
use paykit_lib::collections::Collection;
use paykit_lib::kinds::{
    check_sender_scopes, holds_role, targets_app, ExtensionKind, Kind, MessageKind, Side,
    OPAQUE_MEMBERS,
};
use paykit_lib::records::{
    ActivitySide, AllowanceRecord, BlockFloorRecord, PaymentListRecord, PendingAllowanceRecord,
    StoredMessage,
};
use paykit_lib::scopes::{acts_as_payee, check_certificate, PaykitScope, ScopeName};
use paykit_lib::{Label, PaykitKindRules};
use pubky_dispatch_client::app::{
    decode_local_state, AppBackup, AppError, Collection as DispatchCollection, Effect, LocalView,
    LocalWrite, LossActions, MissedEntries, OpaqueRule, Outcome, Protocol, ProtocolLayer,
    ProviderLoss, PutBack, PutBackSuccessor, ReceiveContext, ReceivedMessage, RecordDeletion,
    RecordExpiry, RecordMarkContext, RecordRepair, RecordSuccessor, ResendNeeded, RestoreContext,
    StoreFailure, WriteBase,
};
use pubky_dispatch_client::app::{DenyListChange, DenyListGap};
use pubky_dispatch_client::error::StoreError;
use pubky_dispatch_core::enc::{serde_b64, unb64_array};
use pubky_dispatch_core::message::{Message, MessagePlaintext};
use pubky_dispatch_core::receive::KindRules;
use pubky_dispatch_core::records::{AppCert, AppRecord};
use pubky_dispatch_core::scopes as dispatch_scopes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::activity::{self, KeptSide, SideTag};
use crate::allowances::ConsentEntry;
use crate::hold::RestoreHold;
use crate::journal::JournalEntry;
use crate::marks::{DropMark, KeyWaitMark, QueuedRequest};
use crate::notices::KeptNotice;
use crate::pay::ListAsk;
use crate::rail::Rail;
use crate::receive::{self, Fail, QueuedAnswer};
use crate::records::KeptMessage;
use crate::state::{self, key, LostListRecord, StateKey};

/// The kinds this SDK handles (Paykit wire 4.3): the one-time request flow, recurring
/// requests' acceptance and period details, payments without a request, Private Payment Lists,
/// re-sends, and the Allowance messages with `executor_notice`. A publisher answers
/// `private_payment_list_request` from chosen contacts for reusable lists within the hourly
/// and rolling 24-hour limits (Paykit spec 8.4.1).
pub const HANDLED_KINDS: [Kind; 15] = [
    Kind::PaymentRequest,
    Kind::PaymentRequestAcceptance,
    Kind::PaymentRequestRejection,
    Kind::PaymentRequestCancellation,
    Kind::PaymentPeriodDetails,
    Kind::PaymentProof,
    Kind::PaymentReceipt,
    Kind::ResendRequest,
    Kind::PrivatePaymentList,
    Kind::PrivatePaymentListRequest,
    Kind::AllowanceProposal,
    Kind::AllowanceAcceptance,
    Kind::AllowanceRejection,
    Kind::AllowanceEnd,
    Kind::ExecutorNotice,
];

/// A re-send to ask of a sender (`p/ask/`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueuedAsk {
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    pub(crate) from_ms: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) until_ms: Option<u64>,
    /// The `resend_request` this app signed for the ask, kept before it is queued, and the
    /// batch the client keeps with it when the write that queues it wasn't answered: the
    /// next pass reads what became of it before it asks anew (`resend.rs`). `None` while
    /// none was signed for the ask as it stands.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) queued: Option<QueuedRequest>,
}

/// A `missed` item's mark (`p/mi/<from>`): the time its re-sends ask from, and the
/// counterparties asked already for it, kept while a record whose counterparty it may still
/// have to ask is sealed under a scope key the app doesn't hold yet (`resend.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Whether the app publishes its record, which decides whether it holds public
    /// endpoints (`crate::own_record`): `PUBLISHES_UNKNOWN` until the SDK's constructors
    /// set it, as for a protocol a host registers itself, when only the scopes decide.
    publishes_record: std::sync::atomic::AtomicU8,
    /// The app's rail, which names a proof's settlement (Paykit wire 1.5); without one, no
    /// proof names a settlement.
    rail: Option<Arc<dyn Rail>>,
    /// The extension kinds the host's extension handles (Paykit spec 3.3).
    extensions: Vec<ExtensionKind>,
    /// The service time the current Paykit delivery operation observed (Paykit spec 8.1).
    pub(crate) delivery: crate::send_holds::DeliveryClock,
}

/// [`PaykitProtocol`]'s publishing setting: not told yet.
const PUBLISHES_UNKNOWN: u8 = 0;
/// [`PaykitProtocol`]'s publishing setting: the app publishes its record.
const PUBLISHES_YES: u8 = 1;
/// [`PaykitProtocol`]'s publishing setting: the app doesn't publish its record.
const PUBLISHES_NO: u8 = 2;

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
            publishes_record: std::sync::atomic::AtomicU8::new(PUBLISHES_UNKNOWN),
            rail: None,
            extensions: Vec::new(),
            delivery: crate::send_holds::DeliveryClock::default(),
        }
    }

    /// Set whether the app publishes its record, as its configuration says
    /// (`pubky_dispatch_client::app::AppConfig::publishes_record`).
    pub(crate) fn set_publishes_record(&self, publishes: bool) {
        let value = if publishes {
            PUBLISHES_YES
        } else {
            PUBLISHES_NO
        };
        self.publishes_record
            .store(value, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the app publishes its record: `None` when Paykit wasn't told.
    fn publishes_record(&self) -> Option<bool> {
        match self
            .publishes_record
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            PUBLISHES_YES => Some(true),
            PUBLISHES_NO => Some(false),
            _ => None,
        }
    }

    /// The protocol, handling the extension kinds `kinds` besides Paykit's (Paykit spec 3.3):
    /// they pass check 10 as they arrived and the receive step keeps them for the host
    /// (`crate::extensions`).
    #[must_use]
    pub fn with_extension_kinds(mut self, kinds: Vec<ExtensionKind>) -> Self {
        self.extensions = kinds;
        self
    }

    pub(crate) fn has_extension_kinds(&self, kinds: &[ExtensionKind]) -> bool {
        use std::collections::HashSet;

        self.extensions.iter().collect::<HashSet<_>>() == kinds.iter().collect::<HashSet<_>>()
    }

    /// Whether `kind` is an extension kind the host registered.
    fn handles_extension(&self, kind: &str) -> bool {
        self.extensions.iter().any(|ext| ext.as_string() == kind)
    }

    /// The protocol of one app paying and paid through `rail`, which names the settlement of
    /// a proof of its method (Paykit wire 1.5, spec 8.4).
    pub fn with_rail(rail: Arc<dyn Rail>) -> Self {
        Self {
            rail: Some(rail),
            ..Self::new()
        }
    }

    pub(crate) fn rail(&self) -> Option<&Arc<dyn Rail>> {
        self.rail.as_ref()
    }

    pub(crate) fn bind_for_attachment(&self, identity: [u8; 32], cert: AppCert) -> bool {
        let mut own = self.own.write().unwrap_or_else(PoisonError::into_inner);
        if own.is_some() {
            return false;
        }
        *own = Some(Own { identity, cert });
        true
    }

    /// The settlement ID of a proof's `proof` object, as the rail names it (Paykit wire 1.5).
    /// A protocol without a rail ([`PaykitProtocol::new`]) pays and credits nothing, so no
    /// proof names a settlement for it.
    fn settlement_id(
        &self,
        endpoint_id: &paykit_lib::PaymentEndpointIdentifier,
        proof: &Map<String, Value>,
    ) -> Option<Vec<u8>> {
        self.rail.as_ref()?.settlement_id(endpoint_id, proof)
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
                        && !own.cert.scopes().iter().any(|s| s == "paykit_pay") =>
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
/// (Paykit wire 5.2, Dispatch spec 8.2): nothing is checked again.
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

/// The `lost_to_delete` member of a `payment_lists` value, as written, if present (Paykit
/// wire 5, 5.4).
/// A `payment_lists` value as the client's seen-mark hooks judge it: its list and marks,
/// read without its `lost_to_delete`. A marker that doesn't read is still a marker (Paykit
/// wire 5, "Deletion data that doesn't read"): the value is taken, so Paykit observes it and
/// keeps its hold, and becomes the mark, so a later write that drops it is shown with its
/// writer. The hooks compare only the marker's presence ([`list_loss_marker`]); the list
/// and its marks follow the rules of wire 5.4.
fn list_for_marks(value: &[u8], own: &[u8; 32]) -> Option<PaymentListRecord> {
    let mut members: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(value).ok()?;
    members.remove("lost_to_delete");
    let unmarked = serde_json::to_vec(&members).ok()?;
    PaymentListRecord::read(&unmarked, own).ok()
}

pub(crate) fn list_loss_marker(value: &[u8]) -> Option<serde_json::Value> {
    let members: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(value).ok()?;
    members.get("lost_to_delete").cloned()
}

/// A failure of the client's store as a protocol call's failure.
pub(crate) fn store_failure(error: pubky_dispatch_client::app::AppError) -> StoreFailure {
    match error {
        pubky_dispatch_client::app::AppError::Store(error) => StoreFailure::from(error),
        _ => StoreFailure::unavailable(StoreError::Unavailable),
    }
}

/// A receive effect exposes its storage failure to the record owner.
pub(crate) fn step_failure(failure: Fail) -> StoreFailure {
    match failure {
        Fail::Store(error) => error,
        _ => StoreFailure::unavailable(StoreError::Unavailable),
    }
}

/// A local read failure keeps its real store error; malformed protocol state is corrupt.
pub(crate) fn view_failure(error: crate::PaykitError) -> StoreFailure {
    match error {
        crate::PaykitError::Client(error)
            if error != pubky_dispatch_client::app::AppError::Store(StoreError::Corrupt) =>
        {
            store_failure(error)
        }
        _ => StoreFailure::of_entry(StoreError::Corrupt),
    }
}

/// Recheck this message's sender and the current deny copy before its delivery or re-send.
pub(crate) fn may_deliver_to_contact(
    delivery: &pubky_dispatch_client::app::DeliveryView<'_>,
) -> crate::error::Result<bool> {
    let cert = delivery.cert();
    if check_certificate(cert).is_err()
        || !receive::reads_lists(cert.scopes())
        || check_sender_scopes(delivery.message(), cert.scopes()).is_err()
    {
        return Ok(false);
    }
    Ok(crate::send_holds::deny_decision(delivery)? == crate::send_holds::DenyDecision::Permits)
}

/// How many keys of each family whose size the runtime bounds a restored layer holds
/// ([`restore_paykit_layer`]).
#[derive(Default)]
struct BoundedFamilies {
    extensions: usize,
    extensions_of: std::collections::BTreeMap<[u8; 32], usize>,
    lost_owed: usize,
    dropped_replies: usize,
    dropped_unverified: usize,
    key_waits: usize,
    dropped_blocked: usize,
}

impl BoundedFamilies {
    /// Count a key of `family`: `Err` naming the family once it is past its bound.
    fn count(&mut self, family: &StateKey) -> Result<(), String> {
        let (count, bound, what) = match family {
            StateKey::LostOwed { .. } => (
                &mut self.lost_owed,
                crate::marks::MAX_LOST_OWED,
                "notices of lost messages owed",
            ),
            StateKey::DroppedReplies { .. } => (
                &mut self.dropped_replies,
                crate::marks::MAX_DROPPED_MARKS,
                "marks of dropped replies",
            ),
            StateKey::DroppedUnverified { .. } => (
                &mut self.dropped_unverified,
                crate::marks::MAX_DROPPED_MARKS,
                "marks of entries dropped unverified",
            ),
            StateKey::KeyWaits { .. } => (
                &mut self.key_waits,
                crate::marks::MAX_DROPPED_MARKS,
                "marks of key waits",
            ),
            StateKey::DroppedBlocked { .. } => (
                &mut self.dropped_blocked,
                crate::marks::MAX_DROPPED_MARKS,
                "marks of blocked messages",
            ),
            StateKey::Extension { sender, .. } => {
                // Bounded per sender as well as in all (`crate::extensions`).
                let of_sender = self.extensions_of.entry(*sender).or_default();
                *of_sender += 1;
                if *of_sender > crate::extensions::MAX_EXTENSION_MESSAGES_PER_SENDER {
                    return Err("Paykit's local state in the backup holds more extension \
                                messages of one sender than the SDK keeps"
                        .into());
                }
                (
                    &mut self.extensions,
                    crate::extensions::MAX_EXTENSION_MESSAGES,
                    "extension messages",
                )
            }
            _ => return Ok(()),
        };
        *count += 1;
        if *count > bound {
            return Err(format!(
                "Paykit's local state in the backup holds more {what} than the SDK keeps"
            ));
        }
        Ok(())
    }
}

/// The name of Paykit's layer in app backups (Dispatch spec 12.4).
pub const BACKUP_NAME: &str = "paykit";

/// The newest provider loss the installation an in-place restore replaces has recorded
/// (`rl`, the generation recovered at), if any. A fresh destination has none.
fn previous_loss_recorded(
    restoring: &RestoreContext<'_>,
    binder: &state::Binder<'_>,
) -> Result<Option<u64>, String> {
    let mut recorded = None;
    for (k, value) in restoring
        .previous_local_scan(key::LOSS_RECORDED.as_bytes())
        .map_err(|_| "the installation's recorded loss cannot be read")?
    {
        if k == key::LOSS_RECORDED.as_bytes() {
            recorded = Some(
                state::restored::<u64>(&k, &value, binder)
                    .map_err(|_| "invalid recorded provider loss")?,
            );
        }
    }
    Ok(recorded)
}

/// What the installation an in-place restore replaces keeps of the block floor: its kept
/// floor (`fk`) with its held floor record's floor and divergence, so a backup taken before
/// either never lowers the floor or drops the hold (Paykit wire 5.6, Divergent floor). A fresh
/// destination has none.
fn previous_floor_kept(
    restoring: &RestoreContext<'_>,
    binder: &state::Binder<'_>,
) -> Result<crate::floor::FloorKept, String> {
    let mut kept = crate::floor::FloorKept::default();
    for (k, value) in restoring
        .previous_local_scan(key::FLOOR_KEPT.as_bytes())
        .map_err(|_| "the installation's block floor hold cannot be read")?
    {
        if k == key::FLOOR_KEPT.as_bytes() {
            kept = kept.with(
                state::restored::<crate::floor::FloorKept>(&k, &value, binder)
                    .map_err(|_| "invalid retained block floor hold")?,
            );
        }
    }
    let policy = Collection::BlockFloor.fixed_policy();
    let hash = policy
        .hash()
        .map_err(|_| "the block floor policy cannot be hashed")?;
    let floor_key = pubky_dispatch_core::ident::record_key_from_blinded(
        Collection::BlockFloor.name(),
        &restoring.blind(&paykit_lib::derive::natural_key::block_floor(
            &restoring.identity(),
        )),
        &hash,
    );
    for held in restoring.previous_records_of(Collection::BlockFloor.name()) {
        let held = held.map_err(|_| "the installation's block floor cannot be read")?;
        if held.key == floor_key && held.policy == policy {
            // The installation's copy shows the floor live in its authenticated write's own
            // generation (`Paykit::keep_floor_view`).
            let live = held.authenticated.then_some(held.version.generation);
            kept = kept.with(crate::floor::FloorKept::of_view(&held, live));
        }
    }
    Ok(kept)
}

/// The writes that install Paykit's backup layer (Dispatch spec 12.4): every key of its local
/// state, as the default layer holds it, with the payment journal checked whole first and
/// its entries marked restored (Paykit spec 7.2). The journal's checks are
/// [`JournalEntry::check_restored`]'s, as every runtime read checks an entry, its claim
/// keys blinded under the backup's naming key, each entry at its own attempt's key, and at
/// most one open entry not restored before for one obligation, since a payment in progress
/// blocks any other; an entry an earlier restore left open beside it is that payment's
/// doubt, never handed off again. `Err` names the rule,
/// without content: nothing is written. Pure: it is called at backup time too.
///
/// The layer's schema is closed (`state::StateKey`): a key no builder of the SDK writes is
/// refused, every key occurs once and is parsed whole, and every value is read as its
/// family's and bound to what its key names.
///
/// Every message another party signed that the layer keeps is verified again for `own`, as
/// its later use verifies it, and bound to the record key it is kept under, from `blind`,
/// the backup's naming key: a journal entry's request and carrier
/// ([`JournalEntry::check_restored`]), a kept cancellation (`kc/`), a kept payer's reply or
/// Allowance answer (`pr/`), a kept Allowance end (`ae/`), a held list (`lh/`),
/// a queued `resend_request` (`ans/`: the requester's certificate, a `resend_request`'s
/// body, and the `seq` and `msg_id` its key names), and a payee app's records seen (`ar/`,
/// bound to the payee and app its key names). The rest is the app's own data, read as its
/// family's and bound to its key (`ask/`, times and counts, `side/`, `en/`, notices `nt/`
/// and those owed `bl/`, the consent list `ac/`, each entry marked to be confirmed again): a
/// backup carries the app's signing keys, so whoever can write one can sign anything the
/// app can, and a check of what the app signed guards against corruption only.
///
/// The families whose size the runtime bounds at every write are counted, and a layer
/// past a bound is refused: the notices of lost messages owed (`bl/`, at most
/// [`crate::marks::MAX_LOST_OWED`]) and each family of marks (`pd/`, `ud/`, `kw/`, `bd/`, at
/// most [`crate::marks::MAX_DROPPED_MARKS`] each). Retired reply windows and their
/// unacknowledged loss notices together occupy at most [`crate::marks::MAX_LOST_OWED`]
/// distinct sender and start-time pairs across both histories.
pub(crate) fn restore_paykit_layer(
    layer: Option<&ProtocolLayer>,
    restoring: &RestoreContext<'_>,
    settlement: crate::receive::Settlement<'_>,
) -> Result<Vec<LocalWrite>, String> {
    // Every value is bound to its key by the function every runtime read binds it with
    // (`state::Stored`), with the backup's naming key.
    let own = restoring.identity();
    let own_cert = restoring.cert();
    let blind = |input: &[u8]| restoring.blind(input);
    let binder = state::Binder::new(own, own_cert, &blind);
    let mut publications = crate::publish::RestoredPublications::default();
    for prefix in [
        key::PUBLICATIONS,
        key::LIST_RESERVATIONS,
        key::PUBLICATION_HISTORY,
        key::LIST_RECIPIENTS,
    ] {
        for (key, value) in restoring
            .previous_local_scan(prefix.as_bytes())
            .map_err(|_| "the installation's private list publications cannot be read")?
        {
            match prefix {
                key::PUBLICATIONS => publications.pending(&key, &value, &binder),
                key::LIST_RESERVATIONS => publications.applied(&key, &value, &binder),
                key::LIST_RECIPIENTS => {
                    state::restored::<crate::publish::ListRecipient>(&key, &value, &binder)
                        .and_then(|recipient| publications.recipient(recipient))
                }
                _ => publications.observed(&key, &value, &binder),
            }
            .map_err(|err| format!("invalid retained private list publication: {err}"))?;
        }
    }
    let mut settlement_buckets = std::collections::BTreeMap::new();
    for (key, value) in restoring
        .previous_local_scan(key::SETTLEMENT_BUCKETS.as_bytes())
        .map_err(|_| "the settlement bucket history cannot be read")?
    {
        let history =
            state::restored::<crate::settlement_marks::BucketHistory>(&key, &value, &binder)
                .map_err(|_| "invalid retained settlement bucket history")?;
        settlement_buckets.insert(key, history);
    }
    let mut settlement_marks = std::collections::BTreeMap::new();
    for (key, value) in restoring
        .previous_local_scan(key::SETTLEMENT_MARKS.as_bytes())
        .map_err(|_| "the installation's settlement marks cannot be read")?
    {
        state::restored::<crate::settlement_marks::ObservedMark>(&key, &value, &binder)
            .map_err(|_| "invalid retained settlement mark")?;
        settlement_marks.insert(key, value);
    }
    let mut settlement_notices = std::collections::BTreeMap::new();
    for (key, value) in restoring
        .previous_local_scan(key::SETTLEMENT_NOTICES.as_bytes())
        .map_err(|_| "the installation's settlement notices cannot be read")?
    {
        let notice = state::restored::<KeptNotice>(&key, &value, &binder)
            .map_err(|_| "invalid retained settlement notice")?;
        settlement_notices.insert(key, notice);
    }
    // A repeated import on this installation preserves notices the host has already seen,
    // and unresolved ones first observed since the backup (Paykit spec 7.2).
    let mut delivery_notices = std::collections::BTreeMap::new();
    let mut unresolved = std::collections::BTreeMap::new();
    let mut pending_conflicts = std::collections::BTreeMap::new();
    let mut original_approvals = std::collections::BTreeMap::new();
    let mut prepared_replies = std::collections::BTreeMap::new();
    let mut retirements = std::collections::BTreeMap::new();
    let mut sourced_replies = std::collections::BTreeMap::new();
    for (key, value) in restoring
        .previous_local_scan(key::RESTORED_DELIVERIES.as_bytes())
        .map_err(|_| "the outgoing delivery notices cannot be read")?
    {
        let notice = state::restored::<KeptNotice>(&key, &value, &binder)
            .map_err(|_| "an outgoing delivery notice is invalid")?;
        delivery_notices.insert(key, notice);
    }
    let mut history = crate::deletions::RestoredReceipts::default();
    for prefix in [
        key::PAYEE_DELETIONS,
        key::RETAINED_RECEIPTS,
        key::CREDIT_HOLDS,
    ] {
        let previous = restoring
            .previous_local_scan(prefix.as_bytes())
            .map_err(|_| "the installation's payee history cannot be read")?;
        for (key, value) in previous {
            let family = StateKey::parse(&key).ok_or("invalid retained payee history key")?;
            history
                .local(&key, &value, &family, &binder, settlement, true)
                .map_err(|_| "invalid retained payee history")?;
        }
    }
    history
        .previous_activity(restoring, &binder, settlement)
        .map_err(|err| format!("the installation's payee activity cannot be retained: {err}"))?;
    let Some(layer) = layer else {
        return Err("a Paykit App's backup carries Paykit's layer, with its journal".into());
    };
    let state = decode_local_state(&layer.data)
        .ok_or_else(|| String::from("Paykit's layer doesn't read as local state"))?;
    let keys: std::collections::BTreeSet<_> = state.iter().map(|(key, _)| key).collect();
    if keys.len() != state.len() {
        return Err("Paykit's local state in the backup repeats a key".into());
    }
    let before = state
        .iter()
        .find(|(key, _)| key.as_slice() == key::RESTORE_HOLD.as_bytes())
        .map(|(key, value)| state::restored::<RestoreHold>(key, value, &binder))
        .transpose()
        .map_err(|_| String::from("the backup's restore hold cannot be read"))?;
    // The hold identifier the backup's confirmations and approvals for execution are
    // matched to: one given under an older identifier counted for nothing there, and carries
    // nothing here.
    let backup_epoch = state
        .iter()
        .find(|(key, _)| key.as_slice() == key::HOLD_EPOCH.as_bytes())
        .map(|(key, value)| state::restored::<crate::hold::HoldEpoch>(key, value, &binder))
        .transpose()
        .map_err(|_| String::from("the backup's hold identifier cannot be read"))?;
    let mut approvals = std::collections::BTreeSet::new();
    let mut confirmed_before = std::collections::BTreeSet::new();
    let mut executor_history = std::collections::BTreeMap::new();
    for (key, value) in restoring
        .previous_local_scan(key::EXECUTOR_CHECKPOINTS.as_bytes())
        .map_err(|_| "the installed executor history cannot be read")?
    {
        let row = state::restored::<crate::executor::ExecutorHistory>(&key, &value, &binder)
            .map_err(|_| "the installed executor history does not verify")?;
        executor_history.insert(key, row);
    }
    let mut proposals = std::collections::BTreeMap::new();
    let mut ledger_batches = std::collections::BTreeMap::<_, std::collections::BTreeSet<_>>::new();
    for (name, bytes) in restoring
        .previous_local_scan(key::LEDGER_PROPOSALS.as_bytes())
        .map_err(|_| "the installed ledger preparations cannot be read")?
    {
        state::restored::<crate::executor::LedgerProposal>(&name, &bytes, &binder)
            .map_err(|_| "the installed ledger preparation does not verify")?;
        proposals.insert(name, bytes);
    }
    for (name, bytes) in restoring
        .previous_local_scan(key::LEDGER_BATCHES.as_bytes())
        .map_err(|_| "the installed ledger batch IDs cannot be read")?
    {
        let Some(StateKey::LedgerBatch { record, digest }) = StateKey::parse(&name) else {
            return Err("invalid installed ledger batch key".into());
        };
        ledger_batches.entry((record, digest)).or_default().extend(
            state::restored::<state::LedgerBatchIds>(&name, &bytes, &binder)
                .map_err(|_| "invalid installed ledger batch IDs")?,
        );
    }
    let mut compactions = std::collections::BTreeMap::new();
    for (key, value) in restoring
        .previous_local_scan(key::LEDGER_COMPACTIONS.as_bytes())
        .map_err(|_| "the installed ledger compactions cannot be read")?
    {
        let receipt = state::restored::<crate::executor::LedgerCompaction>(&key, &value, &binder)
            .map_err(|_| "an installed ledger compaction does not verify")?;
        compactions.insert(key, receipt);
    }
    let mut open = std::collections::BTreeSet::new();
    let mut recordings = std::collections::BTreeSet::new();
    let mut receipt_batches = std::collections::BTreeMap::new();
    let mut awaiting_recording = std::collections::BTreeSet::new();
    let mut journaled = std::collections::BTreeSet::new();
    let mut acknowledged = Vec::new();
    let mut writes = crate::reply_asks::restore(restoring, &binder, &state)?;
    // An in-place restore from an earlier backup keeps the installation's divergence hold
    // and highest floor seen, its held floor record's included (Paykit wire 5.6).
    let mut floor_kept = previous_floor_kept(restoring, &binder)?;
    // And its recorded loss: an in-place restore keeps the shared post-loss hold's evidence
    // (Paykit spec 8.1, wire 5.6).
    let mut loss_recorded = previous_loss_recorded(restoring, &binder)?;
    // A backup taken before a `payment_lists` delete was seen cannot discharge the
    // installation's knowledge of it either (Paykit spec 6).
    let mut lists_lost: std::collections::BTreeMap<Vec<u8>, LostListRecord> =
        std::collections::BTreeMap::new();
    let mut keep_lost = |k: Vec<u8>, lost: LostListRecord| match lists_lost.remove(&k) {
        Some(held) => lists_lost.insert(k, held.merge(lost)),
        None => lists_lost.insert(k, lost),
    };
    for (k, value) in restoring
        .previous_local_scan(key::LISTS_LOST.as_bytes())
        .map_err(|_| "the installation's deleted list records cannot be read")?
    {
        if !matches!(StateKey::parse(&k), Some(StateKey::ListRecordLost(_))) {
            return Err("the installation holds an invalid deleted list record".into());
        }
        let lost = state::restored::<LostListRecord>(&k, &value, &binder)
            .map_err(|_| "the installation holds an invalid deleted list record")?;
        keep_lost(k, lost);
    }
    // And its holds over unread `payment_lists` records: a later absence of one holds its
    // list too, which a backup taken before the hold doesn't know (Paykit spec 6).
    let mut lists_unread = std::collections::BTreeSet::new();
    for (k, value) in restoring
        .previous_local_scan(key::LISTS_UNREAD.as_bytes())
        .map_err(|_| "the installation's unread list records cannot be read")?
    {
        if !matches!(StateKey::parse(&k), Some(StateKey::ListRecordUnread(_))) {
            return Err("the installation holds an invalid unread list record".into());
        }
        state::restored::<bool>(&k, &value, &binder)
            .map_err(|_| "the installation holds an invalid unread list record")?;
        lists_unread.insert(k);
    }
    // And the marker of a `payment_lists` value the installation's client holds but never
    // showed Paykit, such as a recovery upload with no proof of its version: an earlier
    // backup lacks it, and a later value dropping it is still never taken (Paykit spec 6;
    // wire 5).
    for record in restoring.previous_records_of(Collection::PaymentLists.name()) {
        let record = record.map_err(|_| "the installation's list records cannot be read")?;
        let Some(value) = record.content.value() else {
            continue;
        };
        if list_loss_marker(value).is_none() {
            continue;
        }
        let marked = receive::marked_entry(value, &record.key, record.version, None, |t| {
            binder.record_key(t).ok()
        });
        if let Some(lost) = marked {
            keep_lost(key::list_record_lost(&record.key).into_bytes(), lost);
        }
    }
    // The installation's stops: a restore in place keeps the store's unacknowledged
    // reports under their IDs, so the decision taken on them stays with them (Paykit spec
    // 8.1, 9).
    let mut previous_stops = std::collections::BTreeMap::new();
    for (k, value) in restoring
        .previous_local_scan(key::QUEUED_STOPS.as_bytes())
        .map_err(|_| "the installation's queued stops cannot be read")?
    {
        let stop = state::restored::<crate::send_holds::QueuedStop>(&k, &value, &binder)
            .map_err(|_| "the installation holds an invalid queued stop")?;
        previous_stops.insert(k, stop);
    }
    // A held send told is told with its notice: the installation's notice of one it gave
    // up stays with its entry, and one the host has acknowledged, which deleted it, stays
    // gone whatever the backup holds (Paykit spec 8.1, "Undelivered messages").
    let mut stop_notices = std::collections::BTreeMap::new();
    for stop in previous_stops.values().filter(|stop| stop.told) {
        let at = key::message_notice(&stop.to, &stop.msg_id);
        let mut held = None;
        for (k, value) in restoring
            .previous_local_scan(at.as_bytes())
            .map_err(|_| "the installation's notices cannot be read")?
        {
            if k == at.as_bytes() {
                state::restored::<KeptNotice>(&k, &value, &binder)
                    .map_err(|_| "the installation holds an invalid notice")?;
                held = Some(value);
            }
        }
        stop_notices.insert(at.into_bytes(), held);
    }
    // And its notices of payments, keyed by their attempt and kind: a restore keeps each one
    // either side holds, the installation's where both do, so a notice the installation
    // raised after the backup isn't lost (Paykit spec 7.2, "What a restore loses"). A
    // `PaidAfterFailed` is raised again at its key only once an acknowledgement dropped it,
    // and a `HeldWithoutPay` once a resume under `paykit_pay` did: either way the
    // installation's copy is the later one. One the host acknowledged after the backup,
    // which acknowledging deleted, can be listed again. A fresh store has none. A message's
    // notice is the backup's: its kind, origin and kept entry can differ from the
    // installation's at one key, and its own restore steps write it.
    let mut notices: std::collections::BTreeMap<Vec<u8>, crate::notices::KeptNotice> =
        std::collections::BTreeMap::new();
    for (k, value) in restoring
        .previous_local_scan(key::NOTICES.as_bytes())
        .map_err(|_| "the installation's notices cannot be read")?
    {
        if matches!(StateKey::parse(&k), Some(StateKey::Notice { .. })) {
            let notice = state::restored::<KeptNotice>(&k, &value, &binder)
                .map_err(|_| "the installation holds an invalid notice")?;
            notices.insert(k, notice);
        }
    }
    let mut cursor_time_ms = 0;
    let mut bounded = BoundedFamilies::default();
    for (key, value) in state {
        // The schema is closed: a key no builder of the SDK writes is refused, and every
        // value is read as its family's, bound to what its key names (`state::StateKey`).
        let Some(family) = StateKey::parse(&key) else {
            return Err(
                "Paykit's local state in the backup holds a key the SDK doesn't write".into(),
            );
        };
        let unread = || String::from("Paykit's local state in the backup doesn't read");
        bounded.count(&family)?;
        let attempt = match family {
            StateKey::ReplyAsks { .. } | StateKey::DroppedLost { .. } => continue, // Merged and bounded together above.
            StateKey::RestoreHold => continue, // Validated above before reading its confirmations.
            StateKey::MissedHold => {
                state::restored::<crate::hold::MissedHold>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::MissedEndUnknown | StateKey::MissedConsents => {
                state::restored::<bool>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            // A confirmation of an earlier hold cannot also confirm a later restore.
            StateKey::HoldConfirmed { record, epoch }
            | StateKey::HoldConfirmedBefore { record, epoch } => {
                state::restored::<bool>(&key, &value, &binder).map_err(|_| unread())?;
                if backup_epoch.as_ref().map(crate::hold::HoldEpoch::as_bytes) == Some(&epoch)
                    && (matches!(family, StateKey::HoldConfirmed { .. })
                        || !before.as_ref().is_some_and(RestoreHold::in_force))
                {
                    confirmed_before.insert(record);
                }
                continue;
            }
            StateKey::RestoreConsent(_) | StateKey::RestoreApproval(_) => {
                state::restored::<bool>(&key, &value, &binder).map_err(|_| unread())?;
                continue;
            }
            StateKey::ExecutionApproval { record, hold } => {
                state::restored::<bool>(&key, &value, &binder).map_err(|_| unread())?;
                if hold.as_ref() == backup_epoch.as_ref().map(crate::hold::HoldEpoch::as_bytes) {
                    approvals.insert(record);
                }
                continue;
            }
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
            StateKey::HoldEpoch
            | StateKey::RecordsUnlisted
            | StateKey::MissedUnlisted
            | StateKey::PayerPassOwed
            | StateKey::OwnResendOwed
            | StateKey::ListedCert
            | StateKey::Flagged
            | StateKey::StoredNote { .. }
            | StateKey::Taken(_) => continue,
            // Only the original installation holds this duty. A restore reconciles its
            // shared end through the outgoing delivery notices.
            StateKey::OwedEnd(_) => continue,
            // The time of the last read of the lists isn't restored: the copy of the deny
            // list the backup holds may be stale by any time, so the list is read again before
            // any payment (Paykit spec 8.1).
            // A deny change told goes with them: a restore reads the lists again anyway.
            StateKey::ListsRead | StateKey::ListsReadUnder | StateKey::ListsTold => continue,
            // How a raise was last tried belongs to the installation that tried it.
            StateKey::RaiseRetry => continue,
            // Nor the service time the delivery hook judges by: the restore's first read
            // writes it again, and until then the device clock alone judges.
            StateKey::ServiceTime => continue,
            // A read's records to check against the block floor belong to the read: the
            // restore holds every open request (`hold.rs`).
            StateKey::FloorOpened(_) => continue,
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
            StateKey::LossBoundary => {
                state::restored::<crate::LossBoundary>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::LossRecorded => {
                let backup = state::restored::<u64>(&key, &value, &binder).map_err(|_| unread())?;
                loss_recorded = loss_recorded.max(Some(backup));
                continue;
            }
            StateKey::NotPaidBeforeLoss(_) => {
                state::restored::<bool>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::OfflineBatch(attempt) => {
                state::restored::<[u8; 16]>(&key, &value, &binder).map_err(|_| unread())?;
                recordings.insert(attempt);
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::ReceiptBatch {
                record,
                hash,
                msg_id,
                ..
            } => {
                state::restored::<crate::receipt_delivery::ReceiptBatch>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                // The original batch may have finished after this snapshot.
                let imported = key::receipt_batch(&record, &hash, &msg_id, true).into_bytes();
                if let Some(previous) = receipt_batches.insert(imported.clone(), value.clone()) {
                    if previous != value {
                        return Err(unread());
                    }
                } else {
                    writes.push(LocalWrite::Put {
                        key: imported,
                        value,
                    });
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
            | StateKey::ResentDay { .. } => {
                state::restored::<u64>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::FloorKept => {
                let backup = state::restored::<crate::floor::FloorKept>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                floor_kept = floor_kept.with(backup);
                continue;
            }
            StateKey::BlockFloor => {
                state::restored::<crate::floor::PendingUnblocks>(&key, &value, &binder)
                    .map_err(|_| unread())?;
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
            // A restored outbox can hold the stop's copy, which the receive step still ends
            // past its window, and the restored client can hold an unacknowledged report of
            // its end, which is taken again as it was decided, told once (Paykit spec 8.1,
            // 9): the entry is restored whole.
            StateKey::QueuedStop(_) => {
                let carried =
                    state::restored::<crate::send_holds::QueuedStop>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                // Restored in place, the installation's decision wins unless the backup's
                // is further on.
                let stop = match previous_stops.remove(&key) {
                    Some(previous) if previous.progress() >= carried.progress() => previous,
                    _ => carried,
                };
                let value = state::encode(&stop).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::SendExemption(_) => {
                state::restored::<crate::send_holds::SendExemption>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            // The previous installation could have released this answer since the backup.
            StateKey::HeldAnswer { .. } => {
                let mut answer = state::restored::<crate::hold::HeldAnswer>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                answer.restored = true;
                let value = state::encode(&answer).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::KeptCancellation { .. } => {
                state::restored::<KeptMessage>(&key, &value, &binder).map_err(|_| {
                    String::from(
                        "Paykit spec 8.1: a kept cancellation in the backup doesn't verify",
                    )
                })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::PendingRetirement(record) => {
                let end =
                    state::restored::<crate::allowances::PendingRetirement>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                retirements.insert(record, end);
                continue;
            }
            StateKey::AllowanceEvidence(record) => {
                let held =
                    state::restored::<crate::allowances::ConflictEvidence>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                unresolved.insert(record, held);
                continue;
            }
            StateKey::PendingConflict(record) => {
                let conflict =
                    state::restored::<crate::allowances::PendingConflict>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                pending_conflicts.insert(record, conflict);
                continue;
            }
            StateKey::AllowanceEnd { .. } | StateKey::Conflict(_) => {
                state::restored::<KeptMessage>(&key, &value, &binder).map_err(|_| {
                    String::from(
                        "Paykit spec 8.3: a kept Allowance stop in the backup doesn't verify",
                    )
                })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::LedgerProposal { .. } => {
                state::restored::<crate::executor::LedgerProposal>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                if proposals
                    .insert(key, value.clone())
                    .is_some_and(|old| old != value)
                {
                    return Err("two different proposals name the same ledger preparation".into());
                }
                continue;
            }
            StateKey::LedgerBatch { record, digest } => {
                ledger_batches.entry((record, digest)).or_default().extend(
                    state::restored::<state::LedgerBatchIds>(&key, &value, &binder)
                        .map_err(|_| unread())?,
                );
                continue;
            }
            StateKey::LedgerCompaction { .. } => {
                let receipt =
                    state::restored::<crate::executor::LedgerCompaction>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                if compactions.get(&key).is_some_and(|held| held != &receipt) {
                    return Err("two different receipts for the same ledger compaction".into());
                }
                compactions.insert(key, receipt);
                continue;
            }
            StateKey::ExecutorCheckpoint(_) => {
                let selected =
                    state::restored::<crate::executor::ExecutorHistory>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                if let Some(held) = executor_history.get_mut(&key) {
                    held.include(selected);
                } else {
                    executor_history.insert(key, selected);
                }
                continue;
            }
            StateKey::Consent(record_key) => {
                // The backup may predate an end: every Allowance on the consent list is
                // confirmed by the user again before the executor pays under it (Paykit spec
                // 8.3, "After a restore"). A removal stays for good.
                let entry =
                    state::restored::<ConsentEntry>(&key, &value, &binder).map_err(|_| unread())?;
                let again = entry.consent.confirm_again();
                if again != entry.consent {
                    writes.push(
                        state::local_put(&key::restore_consent(&record_key), &true)
                            .map_err(|_| unread())?,
                    );
                }
                let value = state::encode(&entry.with(again)).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::PendingReply {
                record,
                reply: hash,
            } => {
                let reply = state::restored::<crate::allowances::PreparedPendingReply>(
                    &key, &value, &binder,
                )
                .map_err(|_| unread())?;
                prepared_replies.insert((record, hash), reply);
                continue;
            }
            StateKey::OriginalAllowance(record) => {
                let original =
                    state::restored::<crate::allowances::OriginalAllowance>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                original_approvals.insert(record, original);
                continue;
            }
            StateKey::AppliedReply(_) => {
                state::restored::<crate::allowances::AppliedReply>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::FloorBatch => {
                // The client omits unresolved batches on restore. The pending window and its
                // original cutoff survive in the protocol layer and are rechecked on read.
                state::restored::<[u8; 16]>(&key, &value, &binder).map_err(|_| unread())?;
                continue;
            }
            StateKey::GrantBatch(_) => {
                // The client omits unresolved batches on restore, so a first grant kept in
                // one is not carried: the restored app reads the grant from the record.
                state::restored::<[u8; 16]>(&key, &value, &binder).map_err(|_| unread())?;
                continue;
            }
            StateKey::EndBatch(_) => {
                // A backup omits the unresolved Dispatch batch. Its signed end's origin
                // can survive independently of the outcome of its counterparty delivery.
                state::restored::<[u8; 16]>(&key, &value, &binder).map_err(|_| unread())?;
                continue;
            }
            StateKey::EndOrigin(_) => {
                state::restored::<crate::allowances::EndOrigin>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::NoticeTarget(_) => {
                state::restored::<crate::allowances::NoticeTarget>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::ListRecordLost(_) => {
                let lost = state::restored::<LostListRecord>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                keep_lost(key, lost);
                continue;
            }
            StateKey::ListRecordUnread(_) => {
                state::restored::<bool>(&key, &value, &binder).map_err(|_| unread())?;
                lists_unread.insert(key);
                continue;
            }
            StateKey::HeldList(_) => {
                state::restored::<KeptMessage>(&key, &value, &binder).map_err(|_| {
                    String::from("Paykit wire 5.4: a held list in the backup doesn't verify")
                })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::ListSettings => {
                state::restored::<crate::publish::ListSettings>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                continue;
            }
            StateKey::ListDiscovery => continue,
            StateKey::ListRecipient(_) => {
                let recipient =
                    state::restored::<crate::publish::ListRecipient>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                publications
                    .recipient(recipient)
                    .map_err(|err| err.to_string())?;
                continue;
            }
            StateKey::Publication(_) => {
                publications
                    .pending(&key, &value, &binder)
                    .map_err(|_| unread())?;
                continue;
            }
            StateKey::PublicationHistory(_) => {
                publications
                    .observed(&key, &value, &binder)
                    .map_err(|err| format!("invalid private list publication in backup: {err}"))?;
                continue;
            }
            StateKey::ListReservation(_) => {
                publications
                    .applied(&key, &value, &binder)
                    .map_err(|err| format!("invalid private list publication in backup: {err}"))?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::ListAsk { .. } => {
                state::restored::<ListAsk>(&key, &value, &binder).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            // Not restored: the restored app tells its host again at its load.
            StateKey::IdentityNotice(_) => {
                state::restored::<KeptNotice>(&key, &value, &binder).map_err(|_| unread())?;
                continue;
            }
            StateKey::SettlementNotice(_) => {
                let restored = state::restored::<KeptNotice>(&key, &value, &binder)
                    .map_err(|_| "invalid restored settlement notice")?;
                settlement_notices
                    .entry(key)
                    .and_modify(|current| {
                        let acknowledged = current.acknowledged || restored.acknowledged;
                        if restored.raised_at_ms < current.raised_at_ms {
                            *current = restored.clone();
                        }
                        current.acknowledged = acknowledged;
                    })
                    .or_insert(restored);
                continue;
            }
            StateKey::SettlementBucket(_) => {
                let history = state::restored::<crate::settlement_marks::BucketHistory>(
                    &key, &value, &binder,
                )
                .map_err(|_| "invalid restored settlement bucket history")?;
                settlement_buckets
                    .entry(key)
                    .and_modify(|previous| previous.include(history))
                    .or_insert(history);
                continue;
            }
            StateKey::SettlementMark { .. } => {
                state::restored::<crate::settlement_marks::ObservedMark>(&key, &value, &binder)
                    .map_err(|_| "invalid restored settlement mark")?;
                settlement_marks.insert(key, value);
                continue;
            }
            family @ (StateKey::PayeeDeletion { .. }
            | StateKey::RetainedReceipt(..)
            | StateKey::CreditHold(_)) => {
                history
                    .local(&key, &value, &family, &binder, settlement, false)
                    .map_err(|err| {
                        format!("the backup's payee history cannot be retained: {err}")
                    })?;
                continue;
            }
            StateKey::SeenRecords { payee, .. } => {
                // They are the payee's, not the app's own: one that doesn't verify would fail
                // every later public-endpoint read of that payee. They are kept as a read
                // keeps them, so a history over the bounds is cut and raises the floor.
                let unread = || {
                    String::from(
                        "Dispatch spec 4.2: a payee's seen app records in the backup don't verify",
                    )
                };
                let seen =
                    state::restored::<crate::app_records::SeenRecords>(&key, &value, &binder)
                        .map_err(|_| unread())?;
                let kept = seen.canonical(&payee).ok_or_else(unread)?;
                let value = state::encode(&kept).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::KeptReply { .. } => {
                let message =
                    state::restored::<KeptMessage>(&key, &value, &binder).map_err(|_| {
                        String::from(
                            "Dispatch spec 8.2: a kept payer's reply in the backup doesn't verify",
                        )
                    })?;
                if message.pending_source.is_some() {
                    sourced_replies.insert(String::from_utf8(key).map_err(|_| unread())?, message);
                } else {
                    writes.push(LocalWrite::Put { key, value });
                }
                continue;
            }
            StateKey::Extension { .. } => {
                state::restored::<crate::extensions::ExtensionMessage>(&key, &value, &binder)
                    .map_err(|_| {
                        "Paykit wire 4.3: a kept extension message in the backup doesn't verify"
                    })?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            // A notice of lost messages owed is carried as the marks and the notices are: the
            // mark it stands for is gone, so a backup without it would lose the notice. The
            // restore writes back the outbox's records the backup carries, under the same
            // `msg_id`s, and no sent messages: the pass after the read settles the notice from
            // the restored outbox's answer on its request (`Paykit::settle_lost_owed`). Its
            // times identify the original window. The number of notices remains bounded.
            StateKey::LostOwed { .. } => {
                state::restored::<crate::marks::LostOwed>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::BlockedWidening(_) => {
                state::restored::<crate::marks::MissedWidening>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            // A divergent record's mark comes back paused, a confirmation given before the
            // restore included; a message kept over a divergent record is bound to its
            // record as every runtime read binds it (`divergent.rs`).
            StateKey::Divergent(_) => {
                let mark =
                    state::restored::<crate::divergent::DivergentMark>(&key, &value, &binder)
                        .map_err(|_| unread())?
                        .restored();
                let value = state::encode(&mark).map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::DivergentWrite { .. } => {
                state::restored::<crate::divergent::DivergentWrite>(&key, &value, &binder)
                    .map_err(|_| unread())?;
                writes.push(LocalWrite::Put { key, value });
                continue;
            }
            StateKey::RestoredDelivery { .. } => {
                let incoming =
                    state::restored::<KeptNotice>(&key, &value, &binder).map_err(|_| unread())?;
                match delivery_notices.entry(key) {
                    std::collections::btree_map::Entry::Occupied(mut held) => {
                        // Both kinds share the key of the message: two kinds for one
                        // message don't read.
                        let kept = held.get_mut();
                        if kept.kind != incoming.kind {
                            return Err(unread());
                        }
                        kept.acknowledged |= incoming.acknowledged;
                        kept.raised_at_ms = kept.raised_at_ms.min(incoming.raised_at_ms);
                    }
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert(incoming);
                    }
                }
                continue;
            }
            StateKey::MessageNotice { .. }
            | StateKey::Notice { .. }
            | StateKey::BlockedLost { .. } => {
                let notice =
                    state::restored::<KeptNotice>(&key, &value, &binder).map_err(|_| unread())?;
                // The installation's notice of a stop it told wins (`stop_notices`).
                if stop_notices.contains_key(&key) {
                    continue;
                }
                // An acknowledged notice of an attempt is kept only while the journal holds
                // the attempt (`Paykit::acknowledge_notice`): one without its entry is a
                // state no run leaves.
                if let (true, StateKey::Notice { attempt, .. }) = (notice.acknowledged, &family) {
                    acknowledged.push(*attempt);
                }
                if !matches!(family, StateKey::Notice { .. }) {
                    writes.push(LocalWrite::Put { key, value });
                    continue;
                }
                // The installation's notice at this key is the newer one, as raised or
                // acknowledged since the backup, and wins: no flag of the backup's is merged
                // into it, so an acknowledgement before the backup never hides a notice raised
                // again after it. The backup's is taken only where the installation has none.
                notices.entry(key).or_insert(notice);
                continue;
            }
        };
        let bad = || String::from("Paykit spec 7.2: a journal entry in the backup doesn't read");
        // Bound to its key as every runtime read binds it (`state::Stored`): of the attempt
        // its key names, its claim key the one its payee and request, or its attempt for a
        // payment without a request, blind to under the backup's naming key, which the
        // restore context gives (Paykit spec 7.1, Paykit wire 1.5), its record the one its
        // request or attempt names, and every message it keeps verified for this app
        // ([`JournalEntry::check_restored`]): checked here, before anything is written.
        let mut entry =
            state::restored::<JournalEntry>(&key, &value, &binder).map_err(|_| bad())?;
        // An app has one payment of an obligation in progress at a time; an entry an earlier
        // restore left open beside it is that payment's doubt, which the user paid over, and
        // is never handed off again (Paykit spec 7.2, 12.4).
        if entry.open() && !entry.restored && !open.insert(entry.obligation) {
            return Err("Paykit spec 7.2: two open journal entries for one obligation".into());
        }
        if entry.offline == Some(crate::journal::OfflineRecording::AwaitingRecording)
            && entry.rail_proof.is_some()
        {
            awaiting_recording.insert(attempt);
        }
        journaled.insert(attempt);
        entry.restored = true;
        entry.never_dispatched = false;
        let value = state::encode(&entry).map_err(|_| bad())?;
        writes.push(LocalWrite::Put { key, value });
    }
    crate::allowances::restore_originals(restoring, &binder, original_approvals, &mut writes)
        .map_err(|_| "the original payer approval cannot be retained")?;
    crate::allowances::restore_conflicts(
        restoring,
        &binder,
        (pending_conflicts, unresolved),
        prepared_replies,
        retirements,
        sourced_replies,
        &mut writes,
    )
    .map_err(|_| "the pending Allowance replies cannot be retained")?;
    if let Some(recorded) = loss_recorded {
        writes.push(
            state::local_put(key::LOSS_RECORDED, &recorded)
                .map_err(|_| "the recorded provider loss cannot be written")?,
        );
    }
    for key in lists_unread {
        writes.push(LocalWrite::Put {
            key,
            value: state::encode(&true).map_err(|_| "an unread list record cannot be written")?,
        });
    }
    for (key, lost) in lists_lost {
        writes.push(LocalWrite::Put {
            key,
            value: state::encode(&lost).map_err(|_| "a deleted list record cannot be written")?,
        });
    }
    if !floor_kept.is_empty() {
        writes.push(
            state::local_put(key::FLOOR_KEPT, &floor_kept)
                .map_err(|_| "the block floor hold cannot be written")?,
        );
    }
    for (key, notice) in delivery_notices {
        let value = state::encode(&notice).map_err(|_| "a delivery notice cannot be written")?;
        writes.push(LocalWrite::Put { key, value });
    }
    for (key, stop) in previous_stops {
        let value = state::encode(&stop).map_err(|_| "a queued stop cannot be written")?;
        writes.push(LocalWrite::Put { key, value });
    }
    for (key, value) in stop_notices {
        if let Some(value) = value {
            writes.push(LocalWrite::Put { key, value });
        }
    }
    for (key, notice) in notices {
        // An acknowledged one of a payment the restored journal doesn't hold has nothing left
        // to keep it (`Paykit::acknowledge_notice`).
        if let (true, Some(StateKey::Notice { attempt, .. })) =
            (notice.acknowledged, StateKey::parse(&key))
        {
            if !journaled.contains(&attempt) {
                continue;
            }
        }
        let value = state::encode(&notice).map_err(|_| "a notice cannot be written")?;
        writes.push(LocalWrite::Put { key, value });
    }
    if !recordings.is_subset(&awaiting_recording) {
        return Err("an offline recording has no matching payment journal entry".into());
    }
    if acknowledged
        .iter()
        .any(|attempt| !journaled.contains(attempt))
    {
        return Err("Paykit's local state in the backup doesn't read".into());
    }
    let restore_hold = RestoreHold::of(cursor_time_ms, before.as_ref());
    let epoch = crate::hold::HoldEpoch::new()
        .map_err(|_| String::from("the restore's hold identifier can't be created"))?;
    // A carried confirmation holds this restore's identifier, so a later hold ends it.
    for record in confirmed_before {
        writes.push(
            state::local_put(&key::hold_confirmed_before(&record, &epoch), &true)
                .map_err(|_| String::from("the restored request confirmation cannot be written"))?,
        );
    }
    writes.push(
        state::local_put(key::HOLD_EPOCH, &epoch)
            .map_err(|_| String::from("the restore's hold identifier can't be written"))?,
    );
    // A carried approval waits for the restore's verdict: a clean one makes it the current
    // hold's approval, and one that isn't clean drops it, so its Allowance is held until the
    // user confirms again, as every first grant needs the current hold's approval.
    for record in approvals {
        writes.push(
            state::local_put(&key::restore_approval(&record), &true)
                .map_err(|_| String::from("the restored approval cannot be written"))?,
        );
    }
    let hold = state::local_put(key::RESTORE_HOLD, &restore_hold)
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
    // No new attempt until a reconcile has run the payer pass, under `paykit_pay`: the backup
    // doesn't hold the attempts made after it (`Paykit::payer_pass_owed`).
    let owed = state::local_put(key::PAYER_PASS_OWED, &true)
        .map_err(|_| String::from("the restore's payer pass mark can't be written"))?;
    writes.push(owed);
    // Check the restored records for original deliveries whose disposition the host must
    // reconcile. The records cannot authorize another delivery (Paykit spec 7.2).
    let resend = state::local_put(key::OWN_RESEND_OWED, &crate::hold::OwnResendOwed::default())
        .map_err(|_| String::from("the restored-record scan mark can't be written"))?;
    writes.push(resend);
    for (key, mut history) in settlement_buckets {
        // The selected native layer may hold an older value or none. Only a fresh,
        // authenticated current bucket can authorize a rewrite.
        history.repair = true;
        writes.push(LocalWrite::Put {
            key,
            value: state::encode(&history).map_err(|_| "invalid settlement bucket history")?,
        });
    }
    for (key, notice) in settlement_notices {
        let value = state::encode(&notice).map_err(|_| "invalid saved settlement notice")?;
        writes.push(LocalWrite::Put { key, value });
    }
    writes.extend(
        settlement_marks
            .into_iter()
            .map(|(key, value)| LocalWrite::Put { key, value }),
    );
    writes.extend(
        history
            .writes()
            .map_err(|_| "the retained payee evidence cannot be stored")?,
    );
    writes.extend(
        publications
            .writes()
            .map_err(|_| "the retained private list publications cannot be stored")?,
    );
    // Only an ID committed with signed native bytes makes a proposal outstanding.
    // Restore both sources' unresolved IDs, including same-proposal reseals.
    for ((record, digest), ids) in ledger_batches {
        let suffix = format!(
            "{}/{}",
            pubky_dispatch_core::enc::b64(&record),
            pubky_dispatch_core::enc::b64(&digest)
        );
        let proposal = format!("{}{suffix}", key::LEDGER_PROPOSALS).into_bytes();
        let value = proposals
            .remove(&proposal)
            .ok_or("the pending ledger batch has no proposed value")?;
        writes.push(LocalWrite::Put {
            key: proposal,
            value,
        });
        writes.push(LocalWrite::Put {
            key: format!("{}{suffix}", key::LEDGER_BATCHES).into_bytes(),
            value: ids.into_iter().flatten().collect(),
        });
    }
    for (key, receipt) in compactions {
        writes.push(LocalWrite::Put {
            key,
            value: state::encode(&receipt).map_err(|_| "cannot encode a ledger compaction")?,
        });
    }
    for (key, mut history) in executor_history {
        for checkpoint in &mut history.0 {
            checkpoint.restored = true;
        }
        let value =
            state::encode(&history).map_err(|_| "the executor history cannot be retained")?;
        writes.push(LocalWrite::Put { key, value });
    }
    // A previous publication can outlive both the backup and a narrowed certificate.
    // Discovery waits until this app holds plain paykit_request (Paykit spec 8.4.1).
    writes.push(state::local_put(key::LIST_DISCOVERY, &true).map_err(|e| e.to_string())?);
    keep_installation_stops(restoring, &binder, &mut writes)?;
    Ok(writes)
}

/// The installation's protective state, which an older backup restored in place can
/// predate, is merged with the backup's as a union, never replaced by it:
///
/// - every stop it keeps: a payee's cancellation not yet placed in its request record
///   (Paykit spec 8.1) and a counterparty's Allowance end not yet placed in its record
///   (Paykit spec 8.3);
/// - every Allowance it removed from its consent list, which stays removed for good (the
///   consent list, Paykit spec 8.3);
/// - every answer it holds back, a rejection or a cancellation, marked restored as the
///   backup's are, so it is reported, never sent on this restore's word (Paykit spec 8.1,
///   "Blocked counterparties");
/// - every block of its own the list doesn't show yet, which the restored app writes
///   again (Paykit spec 8.1);
/// - the payee app records it has seen, merged with the backup's as one set, so a record
///   older than one seen, or one side of a conflict either saw, is never used again (Dispatch
///   spec 4.2);
/// - a list it holds over a record that doesn't read, where it is newer than the backup's,
///   so a later list, a withdrawal included, is never lost under an older one (Paykit wire
///   5.4).
///
/// Each is read bound to its key, as every runtime read binds it; one that doesn't verify
/// refuses the restore, as for the other families merged from the installation: a restore
/// into a fresh store reads none of them. An entry already written, the backup's or one the
/// Allowance merge above kept, stays as written; a removal replaces the backup's entry for
/// its Allowance, and the mark that this restore holds that consent goes with it.
fn keep_installation_stops(
    restoring: &RestoreContext<'_>,
    binder: &state::Binder<'_>,
    writes: &mut Vec<LocalWrite>,
) -> Result<(), String> {
    let written = |writes: &[LocalWrite], at: &[u8]| {
        writes
            .iter()
            .any(|w| matches!(w, LocalWrite::Put { key, .. } if key.as_slice() == at))
    };
    for prefix in [key::KEPT_CANCELLATIONS, key::ALLOWANCE_ENDS] {
        for (at, value) in restoring
            .previous_local_scan(prefix.as_bytes())
            .map_err(|_| "the installation's kept stops cannot be read")?
        {
            state::restored::<KeptMessage>(&at, &value, binder)
                .map_err(|_| "the installation holds a kept stop that doesn't verify")?;
            if !written(writes, &at) {
                writes.push(LocalWrite::Put { key: at, value });
            }
        }
    }
    for (at, value) in restoring
        .previous_local_scan(key::CONSENTS.as_bytes())
        .map_err(|_| "the installation's consent list cannot be read")?
    {
        let entry = state::restored::<ConsentEntry>(&at, &value, binder)
            .map_err(|_| "the installation holds a consent entry that doesn't verify")?;
        if !matches!(entry.consent, crate::allowances::Consent::Removed { .. }) {
            continue;
        }
        let Some(StateKey::Consent(record)) = StateKey::parse(&at) else {
            return Err("the installation holds an invalid consent key".into());
        };
        // The restore's marks for the Allowance go with the backup's entry: its consent held
        // and its approval for execution, which a clean verdict would otherwise make live.
        let held = key::restore_consent(&record);
        let approval = key::restore_approval(&record);
        writes.retain(|w| {
            !matches!(w, LocalWrite::Put { key, .. }
                if key.as_slice() == at.as_slice()
                    || key.as_slice() == held.as_bytes()
                    || key.as_slice() == approval.as_bytes())
        });
        writes.push(LocalWrite::Put { key: at, value });
    }
    for (at, value) in restoring
        .previous_local_scan(key::HELD_ANSWERS.as_bytes())
        .map_err(|_| "the installation's held answers cannot be read")?
    {
        let mut answer = state::restored::<crate::hold::HeldAnswer>(&at, &value, binder)
            .map_err(|_| "the installation holds a held answer that doesn't verify")?;
        if !written(writes, &at) {
            answer.restored = true;
            let value = state::encode(&answer).map_err(|_| "a held answer cannot be retained")?;
            writes.push(LocalWrite::Put { key: at, value });
        }
    }
    for (at, value) in restoring
        .previous_local_scan(key::OWN_BLOCKS.as_bytes())
        .map_err(|_| "the installation's own blocks cannot be read")?
    {
        let mark = state::restored::<state::OwnBlockMark>(&at, &value, binder)
            .map_err(|_| "the installation holds an own block that doesn't verify")?;
        if mark.blocked() && !written(writes, &at) {
            writes.push(LocalWrite::Put { key: at, value });
        }
    }
    let slot = |writes: &[LocalWrite], at: &[u8]| {
        writes
            .iter()
            .position(|w| matches!(w, LocalWrite::Put { key, .. } if key.as_slice() == at))
    };
    for (at, value) in restoring
        .previous_local_scan(b"ar/")
        .map_err(|_| "the installation's seen app records cannot be read")?
    {
        let Some(StateKey::SeenRecords { payee, .. }) = StateKey::parse(&at) else {
            return Err("the installation holds an invalid seen app records key".into());
        };
        let seen = state::restored::<crate::app_records::SeenRecords>(&at, &value, binder)
            .map_err(|_| "the installation holds seen app records that don't verify")?;
        match slot(writes, &at) {
            None => {
                let kept = seen
                    .canonical(&payee)
                    .ok_or("the installation holds seen app records that don't verify")?;
                let value =
                    state::encode(&kept).map_err(|_| "seen app records cannot be retained")?;
                writes.push(LocalWrite::Put { key: at, value });
            }
            Some(i) => {
                let LocalWrite::Put { value: held, .. } = &writes[i] else {
                    continue;
                };
                let backup = state::restored::<crate::app_records::SeenRecords>(&at, held, binder)
                    .map_err(|_| "the backup's seen app records don't verify")?;
                let merged = seen
                    .merged(&backup, &payee)
                    .ok_or("the seen app records don't verify")?;
                let value =
                    state::encode(&merged).map_err(|_| "seen app records cannot be retained")?;
                writes[i] = LocalWrite::Put { key: at, value };
            }
        }
    }
    let own = binder.identity();
    for (at, value) in restoring
        .previous_local_scan(key::HELD_LISTS.as_bytes())
        .map_err(|_| "the installation's held lists cannot be read")?
    {
        let held = state::restored::<KeptMessage>(&at, &value, binder)
            .map_err(|_| "the installation holds a held list that doesn't verify")?;
        let version = |kept: &KeptMessage| {
            kept.stored(own)
                .ok()
                .as_ref()
                .and_then(crate::receive::list_version)
        };
        match slot(writes, &at) {
            None => writes.push(LocalWrite::Put { key: at, value }),
            Some(i) => {
                let LocalWrite::Put { value: kept, .. } = &writes[i] else {
                    continue;
                };
                let backup = state::restored::<KeptMessage>(&at, kept, binder)
                    .map_err(|_| "the backup's held list doesn't verify")?;
                if version(&held) > version(&backup) {
                    writes[i] = LocalWrite::Put { key: at, value };
                }
            }
        }
    }
    Ok(())
}

/// Refuses `backup` when the provider loss its Paykit layer keeps (`pl`) was recovered at a
/// later generation than the one its client follows (Paykit spec 7.3): the protocol's loss
/// step writes the boundary of the loss the client follows, in the commit that follows it,
/// so no run keeps one above it, and one above would leave every later loss's step writing
/// nothing ([`crate::loss::loss_writes`]). Checked before the restore writes anything. A
/// layer with repeated keys or a value that doesn't read is left to the layer's own check
/// ([`restore_paykit_layer`]), which refuses it.
///
/// The client offers no reading of the generation a backup's client follows before the
/// restore's one commit, which writes Paykit's layer too, so this reads it from the
/// backup's bytes as the client writes them (`meta`): a backup in which it can't be read
/// is refused, never restored unchecked.
///
/// # Errors
///
/// [`pubky_dispatch_client::app::AppError::InvalidData`] for such a backup, and for one
/// keeping a loss whose client's generation can't be read; the client's errors for one
/// whose bytes don't read.
pub(crate) fn check_restored_loss(backup: &AppBackup) -> crate::error::Result<()> {
    /// The part of the client's backup this check reads: the identity and the generation
    /// its client follows.
    #[derive(Deserialize)]
    struct Followed {
        meta: FollowedMeta,
    }
    #[derive(Deserialize)]
    struct FollowedMeta {
        #[serde(with = "serde_b64")]
        identity: [u8; 32],
        generation: u64,
    }
    let Some(layer) = backup
        .layers()?
        .into_iter()
        .find(|layer| layer.protocol == BACKUP_NAME)
    else {
        return Ok(());
    };
    let Some(state) = decode_local_state(&layer.data) else {
        return Ok(());
    };
    let Some((at, value)) = state
        .iter()
        .find(|(at, _)| at.as_slice() == key::LOSS_BOUNDARY.as_bytes())
    else {
        return Ok(());
    };
    let followed: Followed = serde_json::from_slice(&backup.to_bytes()?).map_err(|_| {
        AppError::InvalidData(
            "the backup: Paykit spec 7.3: the generation its client follows can't be read, \
             so its provider loss can't be checked"
                .into(),
        )
    })?;
    let binder = state::Binder::unblinded(followed.meta.identity);
    let Ok(kept) = state::restored::<crate::loss::LossBoundary>(at, value, &binder) else {
        return Ok(());
    };
    if kept.generation > followed.meta.generation {
        return Err(AppError::InvalidData(
            "the backup: Paykit spec 7.3: its provider loss is of a later generation than \
             its client follows"
                .into(),
        )
        .into());
    }
    Ok(())
}

impl Protocol for PaykitProtocol {
    fn name(&self) -> String {
        BACKUP_NAME.to_owned()
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
        // certificate the Dispatch takes and Paykit refuses writes nothing.
        check_certificate(restoring.cert())
            .map_err(|error| format!("the backup's certificate: {error}"))?;
        restore_paykit_layer(layer, restoring, &|endpoint, proof| {
            self.settlement_id(endpoint, proof)
        })
    }

    /// Check the applied original before delivering any Allowance reply (Paykit spec
    /// 8.3.1). Proposals, acceptances, rejections,
    /// re-send requests and extensions require current sender scopes and a permitting
    /// deny copy. An original user end has its own delivery rule (Paykit spec 3.1).
    /// A Private Payment List must still match the current sharing choice (Paykit spec 8.4).
    /// Requests, period details, list requests, rejections, payer cancellations and
    /// subscription acceptances go only while the deny decision permits them, and none of
    /// them once it is 30 days past its `created_at`, less the clock tolerance; a payee's
    /// cancellation and a payment's acceptance, proof and receipt still go when first
    /// sent, under a certificate that sends them, but not as a re-send (Paykit spec 3.1,
    /// 8.1, 9).
    fn may_send(
        &self,
        delivery: &pubky_dispatch_client::app::DeliveryView<'_>,
    ) -> Result<bool, StoreFailure> {
        // Nothing goes under a certificate Paykit refuses, or one that no longer sends the
        // kind, before any rule of the kind's own (Paykit spec 3.1 to 3.3, 7.2, 9).
        let cert = delivery.cert();
        if check_certificate(cert).is_err()
            || paykit_lib::kinds::check_sender_scopes(delivery.message(), cert.scopes()).is_err()
        {
            return Ok(false);
        }
        let kind = MessageKind::parse(&delivery.message().kind);
        if crate::allowances::pending_delivery(delivery.message())
            && (crate::allowances::pending_conflict_holds_delivery(delivery)
                .map_err(view_failure)?
                || !crate::allowances::reply_is_applied(delivery)?)
        {
            return Ok(false);
        }
        // No message the counterparty hold governs goes past its window, whichever rules
        // its kind has besides (Paykit spec 8.1, "Messages already queued").
        if crate::send_holds::held_past_window(delivery, self.delivery.observed(delivery.now_ms()))
            .map_err(view_failure)?
        {
            return Ok(false);
        }
        match kind {
            Ok(MessageKind::Paykit(Kind::PrivatePaymentList)) => crate::publish::may_send(delivery),
            Ok(MessageKind::Paykit(Kind::AllowanceEnd)) => {
                crate::allowances::may_deliver_end(delivery).map_err(view_failure)
            }
            Ok(MessageKind::Paykit(Kind::ResendRequest)) => {
                if !may_deliver_to_contact(delivery).map_err(view_failure)?
                    || crate::floor::delivery_raise_holds(delivery).map_err(view_failure)?
                {
                    return Ok(false);
                }
                crate::reply_asks::may_deliver_request(delivery).map_err(view_failure)
            }
            Ok(
                MessageKind::Paykit(
                    Kind::AllowanceProposal | Kind::AllowanceAcceptance | Kind::AllowanceRejection,
                )
                | MessageKind::Extension(_),
            ) => may_deliver_to_contact(delivery).map_err(view_failure),
            Ok(MessageKind::Paykit(
                kind @ (Kind::PaymentRequest
                | Kind::PaymentPeriodDetails
                | Kind::PaymentRequestAcceptance
                | Kind::PaymentRequestRejection
                | Kind::PaymentRequestCancellation
                | Kind::PrivatePaymentListRequest
                | Kind::PaymentProof
                | Kind::PaymentReceipt),
            )) => crate::send_holds::may_deliver(delivery, kind).map_err(view_failure),
            // An `executor_notice` goes to an own app, not a counterparty (Paykit spec 8.1).
            Ok(MessageKind::Paykit(Kind::ExecutorNotice)) => Ok(true),
            // A message whose role can't be established waits (Paykit spec 8.1).
            Err(_) => Ok(false),
        }
    }

    fn append_records(
        &self,
        delivery: &pubky_dispatch_client::app::DeliveryView<'_>,
    ) -> Result<Vec<pubky_dispatch_client::app::RecordTarget>, StoreFailure> {
        crate::allowances::append_records(delivery)
    }

    fn judge_append(
        &self,
        delivery: &pubky_dispatch_client::app::DeliveryView<'_>,
        records: &[(
            pubky_dispatch_client::app::RecordTarget,
            pubky_dispatch_client::app::RecordRead,
        )],
    ) -> Result<pubky_dispatch_client::app::AppendDecision, StoreFailure> {
        crate::allowances::judge_append(delivery, records)
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
        // Under an own certificate Paykit refuses, such as a renewal's the Dispatch took, the
        // message isn't judged by it: the receive step holds the entry until a certificate
        // that passes replaces it, and this check runs again then (Paykit wire 3.1).
        if check_certificate(own).is_err() {
            return Ok(());
        }
        PaykitKindRules::for_certificate(own)
            .check_scopes(message, sender)
            .map_err(|error| error.to_string())
    }

    /// Paykit's member under a renewed certificate, from the record's one
    /// (`crate::own_record::member_under`): signed once with the certificate, so a record
    /// under it never lists a public endpoint or a capability it doesn't cover (Paykit spec,
    /// "What this pin adopts", "Renewing a certificate").
    fn member_for(
        &self,
        member: Option<&Map<String, Value>>,
        ask: RecordExpiry,
        cert: &AppCert,
    ) -> (Option<Map<String, Value>>, RecordExpiry) {
        crate::own_record::member_under(member, ask, cert, self.publishes_record())
    }

    /// The kinds this SDK handles that the certificate's scopes receive (Dispatch spec 3.4):
    /// none for a certificate Paykit's verifiers refuse (Paykit wire 3.1), such as a
    /// renewal's that the Dispatch took.
    fn accepts(&self, cert: &AppCert) -> Vec<String> {
        if check_certificate(cert).is_err() {
            return Vec::new();
        }
        let mut kinds: Vec<String> = HANDLED_KINDS
            .iter()
            .filter(|kind| holds_role(cert.scopes(), kind.recipients()))
            .map(|kind| kind.as_str().to_owned())
            .collect();
        // The registered extension kinds whose `ext:<prefix>` the certificate holds (Paykit
        // wire 4.3): never wider than what its scopes receive (Dispatch spec 3.4).
        for ext in &self.extensions {
            let kind = ext.as_string();
            if cert.scopes().contains(&ext.prefix().scope()) && !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        kinds
    }

    /// Whether another app's scopes receive `kind` by Paykit's table (Paykit spec 3.3), which
    /// a sender checks before it wraps the kind to that app, and before
    /// [`Protocol::sends_to`] narrows by its scopes. An app whose certificate Paykit's
    /// verifiers refuse (Paykit wire 3.1) receives none: the Dispatch takes such a
    /// certificate, Paykit doesn't.
    fn receives(&self, kind: &str, cert: &AppCert) -> bool {
        if check_certificate(cert).is_err() {
            return false;
        }
        // An extension kind goes to an app whose certificate holds its `ext:<prefix>`; the
        // client also asks that its record accept the kind (Paykit wire 4.3).
        if let Ok(ext) = ExtensionKind::parse(kind) {
            return cert.scopes().contains(&ext.prefix().scope());
        }
        Kind::from_kind(kind).is_some_and(|k| holds_role(cert.scopes(), k.recipients()))
    }

    /// The label narrowing of Paykit spec 3.4: a payer's acceptance, rejection,
    /// cancellation and proof about a labelled request go only to apps holding `paykit_request` or
    /// that label, and about an unlabelled request only to apps holding `paykit_request`; a payee's
    /// cancellation, without a label, only to apps holding `paykit_pay`.
    fn sends_to(&self, message: &Message, recipient: &AppRecord, view: &LocalView<'_>) -> bool {
        // A kept side that can't be read sends to no one rather than guess (Paykit spec 3.4):
        // the message isn't sealed to this recipient. So does one that names another message
        // than its key, which doesn't bind (`state::Stored`).
        // An `executor_notice` is wrapped only to the executor app its batch kept (Paykit
        // wire 4.17): to no app without one.
        if message.kind == Kind::ExecutorNotice.as_str() {
            return matches!(
                state::view_get::<crate::allowances::NoticeTarget>(
                    view,
                    &key::notice_target(&message.msg_id),
                ),
                Ok(Some(kept)) if kept.app_id == recipient.cert().app_id()
            );
        }
        match state::view_get::<KeptSide>(view, &key::side(&message.msg_id)) {
            Ok(kept) => self.narrows(message, recipient, kept.map(|kept| kept.side)),
            Err(_) => false,
        }
    }

    /// Check 10 (Paykit wire 4.1), with no effect: a kind this SDK handles ([`HANDLED_KINDS`])
    /// and a body that parses for it, within the size of a recorded kind.
    fn check_body(&self, message: &ReceivedMessage) -> Result<(), String> {
        // An extension kind the host registered passes as it arrived: its body is the
        // extension's (Paykit wire 4.3).
        if self.handles_extension(&message.message.kind) {
            return Ok(());
        }
        check_message(message)
    }

    /// The effect of a message that passed check 10, added to the batch that moves the
    /// cursor (Dispatch spec 8.2; `crate::receive`). Called again after `409 conflict`, with
    /// the records read again: the effect is built anew from them.
    fn receive(&self, message: &ReceivedMessage, cx: &mut ReceiveContext<'_>) -> Outcome {
        // A released entry whose effect the app wrote since, by a write of its own, is
        // declined: its effect isn't applied twice (Dispatch spec 8.2; `stored.rs`).
        if message.released {
            let note_key = key::stored_note(&message.sender, &message.message.msg_id);
            match state::cx_get::<crate::stored::StoredNote>(cx, &note_key) {
                Ok(note) if crate::stored::declines(note.as_ref()) => {
                    return Outcome::Drop {
                        reason: "Dispatch spec 8.2: a released entry whose effect the app wrote \
                                 since"
                            .into(),
                    };
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
                };
            }
            Ok(_) => {}
            Err(Fail::Store(failure)) => return Outcome::Retry(failure),
            Err(_) => return Outcome::Retry(StoreFailure::of_entry(StoreError::Corrupt)),
        }
        let one_time_request =
            stored_copy(message).is_ok_and(|stored| crate::floor::floor_applies(&stored));
        let effect =
            receive::check_before_effect(cx, &message.sender, one_time_request).and_then(|()| {
                if self.handles_extension(&message.message.kind) {
                    return crate::extensions::extension_effect(cx, message);
                }
                let stored = stored_copy(message).map_err(Fail::Drop)?;
                receive::effect(cx, &stored, message.position(), &|endpoint, proof| {
                    self.settlement_id(endpoint, proof)
                })
                .map(|()| Outcome::Apply(Effect::default()))
            });
        match effect {
            Ok(outcome) => outcome,
            // A write the context couldn't build for want of a key: the context holds the
            // entry itself, whatever is returned (Dispatch spec 3.5).
            Err(Fail::NotNow) => Outcome::Apply(Effect::default()),
            Err(Fail::Need(needs)) => Outcome::Need(needs),
            Err(Fail::Consume) => Outcome::Consume,
            Err(Fail::AwaitAdmission) => Outcome::AwaitAdmission,
            Err(Fail::AwaitAdmissionWithCustody(custody)) => {
                Outcome::AwaitAdmissionWithCustody(custody)
            }
            Err(Fail::Defer { reason, custody }) => Outcome::Defer { reason, custody },
            Err(Fail::Drop(reason)) => Outcome::Drop { reason },
            Err(Fail::Store(failure)) => Outcome::Retry(failure),
            // A record value waits for its scope key: the client stores the entry and passes
            // it at once, and runs the step again once the key comes (Dispatch spec 8.2). One
            // it drops past its bounds first was never processed: the SDK tells the host and
            // marks its sender to ask again once the key is held (`stored.rs`, `marks.rs`).
            Err(Fail::AwaitsKey { data_epoch }) => Outcome::AwaitKey { data_epoch },
        }
    }

    /// A deny change the feed shows takes effect before the next delivery decision: none is
    /// taken on the copy of the lists until they are read again, and the copy's deny list
    /// stays, so that read sees an unblock (Paykit spec 8.1, "Messages already queued";
    /// Paykit wire 5.6).
    fn deny_list_changed(&self, change: &DenyListChange) -> Result<Vec<LocalWrite>, StoreFailure> {
        self.delivery.learned(None);
        // Its service time, the latest of those told, as the feed tells them in order.
        Ok(vec![local_write(key::LISTS_TOLD, &change.at_ms)?])
    }

    /// A gap in the deny list's history (Dispatch spec 8.3), taken as a change, with its list
    /// kept for the receive (`Paykit::take_learned_lists`).
    fn deny_list_gap(&self, gap: &DenyListGap) -> Result<Vec<LocalWrite>, StoreFailure> {
        self.delivery.learned(gap.list_read.then_some(&gap.denied));
        Ok(vec![local_write(key::LISTS_TOLD, &u64::MAX)?])
    }

    /// Entries the app can't read: ask the sender to send again (Paykit spec 9).
    fn resend_needed(&self, needed: &ResendNeeded) -> Result<Vec<LocalWrite>, StoreFailure> {
        let ask = QueuedAsk {
            sender: needed.sender,
            from_ms: needed.accepted_from_ms,
            until_ms: Some(needed.accepted_until_ms),
            queued: None,
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
    /// tolerance, the time the item carries (`MissedEntries::accepted_until_ms`). The client
    /// hands `u64::MAX` there when no end is known: for a gap the cursor the service holds
    /// shows ahead (Dispatch spec 8.3 "Ahead"), and for a call it kept owed without one. That
    /// is no time: the item sets the hold's end as not known (`mu`), which holds every open
    /// request, and leaves the bound an earlier item set in place, since this step can't
    /// read it; the receive bounds the hold once its read completes, by the service time it
    /// verified then (`Paykit::bound_missed_hold`). The confirmations of an earlier hold go
    /// in the same commit, so none releases the new one. The copy of the shared records counts as not listed from the item on (`ml`,
    /// Paykit spec 8.1, "Records not listed yet"), until Paykit's own listing at the end of
    /// the receive (`Paykit::list_records`): the client lists them right after the item,
    /// inside its read, but Paykit judges the copy listed only from a listing of its own,
    /// so what the app decides between the item and that listing decides nothing from an
    /// absence. A mark of its own, not `nl`, which the same read's end would clear.
    /// Every Allowance on the consent list is held too, to be confirmed again as after a
    /// restore (Paykit spec 8.3; `mc`, which `Paykit::confirm_consents_again` turns into each
    /// entry's mark after the receive): an end may be among the deleted entries.
    fn missed(&self, missed: &MissedEntries) -> Result<Vec<LocalWrite>, StoreFailure> {
        let epoch = crate::hold::HoldEpoch::new()
            .map_err(|_| StoreFailure::unavailable(StoreError::Unavailable))?;
        let hold = if missed.accepted_until_ms == u64::MAX {
            local_write(key::MISSED_END_UNKNOWN, &true)?
        } else {
            local_write(
                key::MISSED_HOLD,
                &crate::hold::MissedHold::bounded_by(missed.accepted_until_ms),
            )?
        };
        Ok(vec![
            local_write(key::HOLD_EPOCH, &epoch)?,
            local_write(
                &key::missed_item(missed.accepted_from_ms),
                &MissedItemMark {
                    from_ms: missed.accepted_from_ms,
                    asked: Vec::new(),
                },
            )?,
            hold,
            local_write(key::MISSED_UNLISTED, &true)?,
            // What a blocked counterparty's inbox accepted among the missed entries is asked
            // for once it is unblocked: every counterparty the deny list holds at the next
            // read of the lists is marked for the item's times (`marks.rs`).
            local_write(
                &key::blocked_widening(epoch.as_bytes()),
                &crate::marks::MissedWidening {
                    id: epoch,
                    windows: vec![crate::marks::BlockedWindow {
                        from_ms: missed.accepted_from_ms,
                        until_ms: if missed.accepted_until_ms == u64::MAX {
                            missed.read_at_ms.max(missed.accepted_from_ms)
                        } else {
                            missed.accepted_until_ms
                        },
                        floor_ms: missed.accepted_until_ms.min(missed.read_at_ms),
                    }],
                    deny: None,
                },
            )?,
            local_write(key::MISSED_CONSENTS, &true)?,
            LocalWrite::DeletePrefix {
                prefix: key::HOLD_CONFIRMED.as_bytes().to_vec(),
            },
            LocalWrite::DeletePrefix {
                prefix: key::HOLD_CONFIRMED_BEFORE.as_bytes().to_vec(),
            },
            LocalWrite::DeletePrefix {
                prefix: key::RESTORE_CONSENTS.as_bytes().to_vec(),
            },
            // An approval given before the item approves nothing after it (Paykit spec 7.2).
            LocalWrite::DeletePrefix {
                prefix: key::EXECUTION_APPROVALS.as_bytes().to_vec(),
            },
            LocalWrite::DeletePrefix {
                prefix: key::RESTORE_APPROVALS.as_bytes().to_vec(),
            },
        ])
    }

    /// A provider loss (Dispatch spec 12.6 steps 2 and 4): ask every counterparty with Paykit
    /// activity to send again from the old cursor's time, and add their reply grants
    /// back.
    fn provider_lost(
        &self,
        loss: &ProviderLoss,
        view: &LocalView<'_>,
    ) -> Result<LossActions, StoreFailure> {
        // A store that fails, or a peer key that doesn't read, is the step's failure, never
        // an empty list of counterparties: the recovery asks again.
        let peers = state::view_scan::<u64>(view, key::PEERS).map_err(view_failure)?;
        let mut reply_peers = Vec::with_capacity(peers.len());
        for (rest, _) in peers {
            let peer = unb64_array::<32>(&rest)
                .map_err(|_| StoreFailure::of_entry(StoreError::Corrupt))?;
            reply_peers.push(peer);
        }
        let mut actions = LossActions {
            reply_peers,
            local: vec![local_write(
                &key::missed(loss.accepted_from_ms),
                &loss.accepted_from_ms,
            )?],
        };
        actions.local.extend(crate::loss::loss_writes(loss, view)?);
        // No copy of the lists read before the loss is taken as fresh: the first use after it
        // reads the lists again ([`crate::Paykit::loss_lists`]).
        actions.local.push(LocalWrite::Delete {
            key: key::LISTS_READ.as_bytes().to_vec(),
        });
        Ok(actions)
    }

    fn adds_reply(&self, kind: &str) -> bool {
        // Paykit wire 4.12: a resend_request adds no reply grant.
        kind != Kind::ResendRequest.as_str()
    }

    fn message_adds_reply(&self, message: &Message, _: &MessagePlaintext) -> bool {
        // A withdrawal never admits its recipient to the publisher's inbox (Paykit wire 4.13).
        message.kind != Kind::PrivatePaymentList.as_str()
            || paykit_lib::bodies::PrivatePaymentList::from_body(&message.body)
                .is_ok_and(|list| !list.payment_endpoints.is_empty())
    }

    /// Paykit's collections with their fixed policies (Paykit spec 6, Paykit wire 5.1):
    /// `payee_activity` with one policy per label, so an app holding `paykit_request` lists and
    /// takes every label's records, and one holding `paykit_request:<label>` its label's.
    fn collections(&self) -> Vec<DispatchCollection> {
        let mut collections = crate::hold::RecordSide::All.collections();
        collections.push(crate::publish::collection());
        collections
    }

    fn observe_record(
        &self,
        observation: &pubky_dispatch_client::app::RecordObservation<'_>,
        context: &ReceiveContext<'_>,
    ) -> Result<Vec<LocalWrite>, StoreFailure> {
        if observation.record.collection == Collection::AllowanceProposals.name() {
            return crate::allowances::observe_reopened(observation, context);
        }
        let mut writes = crate::settlement_marks::observe(observation, context)?;
        writes.extend(crate::publish::history::observe(observation, context)?);
        writes.extend(crate::receive::observe_list_record(observation, context)?);
        Ok(writes)
    }

    fn mark_ends_at_delete(&self, collection: &str) -> bool {
        collection == Collection::AllowanceProposals.name()
    }

    fn record_lifetime_ended(
        &self,
        record: &RecordMarkContext<'_>,
        version: pubky_dispatch_core::ops::RecordVersion,
        context: &ReceiveContext<'_>,
    ) -> Result<Vec<LocalWrite>, StoreFailure> {
        crate::allowances::lifetime_ended(record, version, context)
    }

    fn qualify_record_mark(
        &self,
        input: &RecordMarkContext<'_>,
        seen: Option<&[u8]>,
        value: &[u8],
    ) -> Option<Result<Vec<u8>, String>> {
        (input.collection.name == Collection::AllowanceProposals.name())
            .then(|| self.pending_mark(input, seen, value))
    }

    /// The roles the app has seen in a record (Paykit wire 5.3 "Seen roles", 5.4).
    fn record_seen(
        &self,
        collection: &str,
        seen: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        let own = self.own()?;
        if collection == crate::publish::COLLECTION {
            return value
                .and_then(|bytes| crate::publish::own_counter_mark(own.cert.app_id(), bytes))
                .or_else(|| seen.map(<[u8]>::to_vec));
        }
        let reads = match (Collection::from_name(collection)?, value) {
            (Collection::PayerActivity, Some(v)) => {
                activity::read(Some(v), &own.identity, ActivitySide::Payer).is_some()
            }
            (Collection::PayeeActivity, Some(v)) => {
                activity::read(Some(v), &own.identity, ActivitySide::Payee).is_some()
            }
            // The limit: the client gives this mark no record key, so a value is not bound
            // here to the key its payee and app derive (Paykit wire 5.4), and another payee
            // app's valid record written at a key becomes the mark there, against which the
            // real payee app's lists are then refused. Binding needs the key from the client.
            (Collection::PaymentLists, Some(v)) => list_for_marks(v, &own.identity).is_some(),
            (Collection::BlockFloor, Some(v)) => BlockFloorRecord::read(v).is_ok(),
            (Collection::Allowances, Some(v)) => AllowanceRecord::read(v, &own.identity).is_ok(),
            _ => false,
        };
        match (reads, value) {
            (true, Some(v)) => Some(v.to_vec()),
            _ => seen.map(<[u8]>::to_vec),
        }
    }

    fn record_put_back(
        &self,
        deletion: &RecordDeletion<'_>,
        cx: &ReceiveContext<'_>,
    ) -> Result<Option<PutBack>, StoreFailure> {
        // `block_floor` is permanent (Paykit wire 5.1, 5.6): the client refuses a delete of it,
        // so none comes here.
        if deletion.collection == paykit_lib::collections::Collection::PaymentLists.name() {
            // The delete is kept as seen in the commit that observes it, from the feed or a
            // read: no later absence or value places a list there (Paykit spec 6; wire 5,
            // "Incomplete protected-record contract").
            //
            // The limit: the client reports only another app's delete here, so a delete by
            // another installation of this App ID, which only a faulty own app writes, is
            // seen as a delete only by a list arriving over it ([`crate::receive`]
            // `hold_list`), until the client reports it.
            return Ok(Some(PutBack::Observe {
                observed: crate::receive::list_record_deleted(
                    cx,
                    deletion.key,
                    deletion.version,
                    Some(deletion.item_hash),
                )?,
            }));
        }
        crate::deletions::observe_payee_deletion(deletion, cx, &|endpoint, proof| {
            self.settlement_id(endpoint, proof)
        })
    }

    fn record_put_back_successor(
        &self,
        deletion: &RecordDeletion<'_>,
        successor: &RecordSuccessor<'_>,
        cx: &ReceiveContext<'_>,
    ) -> Result<PutBackSuccessor, StoreFailure> {
        crate::deletions::receipt_publication(deletion, successor, cx, &|endpoint, proof| {
            self.settlement_id(endpoint, proof)
        })
    }

    /// Whether a newer write goes back on what the app saw (Paykit wire 5.3, 5.4): drops or
    /// replaces a message in a role seen, or changes the Allowance seen; for a
    /// `payment_lists` record, holds an older list or drops a single-use endpoint marked
    /// used; for the `block_floor` record, lowers the floor. A value that doesn't read is
    /// refused too. A delete isn't such a write. A write built on the
    /// version this app holds is judged as by a reader holding it, which also refuses a
    /// reassociation at or before a period it attempted (Paykit wire 5.3 "Seen Allowance").
    /// The version is judged by the mark of what the app saw. For activity records the
    /// client counts a create over a record it holds deleted as built on what it
    /// holds, so such a write is judged against the mark from before the delete: stricter
    /// than the wire format's reader holding the version built on, which a delete leaves with
    /// no attempted period.
    fn record_regression(
        &self,
        collection: &str,
        seen: Option<&[u8]>,
        value: Option<&[u8]>,
        base: WriteBase,
    ) -> Option<String> {
        if collection == crate::publish::COLLECTION {
            return crate::publish::foreign_publication(
                self.own().map(|own| own.cert.app_id()),
                seen,
                value,
            );
        }
        // Nothing seen of the record yet, as at its first read: nothing to go back on.
        self.regression(collection, seen?, value, base.held, base.repair.as_ref())
    }
}

impl PaykitProtocol {
    /// Bind both signed roles to the service key where this wallet qualified them.
    fn pending_mark(
        &self,
        input: &RecordMarkContext<'_>,
        seen: Option<&[u8]>,
        value: &[u8],
    ) -> Result<Vec<u8>, String> {
        let own = self
            .own()
            .ok_or("Paykit spec 3.2: the app identity is unavailable")?;
        let record =
            PendingAllowanceRecord::read(value, &own.identity).map_err(|e| e.to_string())?;
        let target = crate::allowances::allowance_record_target(
            Collection::AllowanceProposals,
            record.counterparty(),
            record.allowance_id(),
        );
        if input.collection.policy != target.collection.policy
            || input.record_key(&target).map_err(|e| e.to_string())? != input.key
        {
            return Err("Paykit wire 5.4: the signed proposal belongs to another record".into());
        }

        // A version, the actual key and the original hash precede the optional
        // first conflict. Neither signed role can be replaced (Paykit wire 5.4).
        let mut mark = vec![1];
        mark.extend_from_slice(&input.key);
        mark.extend_from_slice(&record.proposal().hash());
        let original_size = mark.len();
        if let Some(conflict) = record.conflicted() {
            mark.extend_from_slice(&conflict.hash());
        }
        if let Some(previous) = seen {
            if (previous.len() != original_size && previous.len() != original_size + 32)
                || !mark.starts_with(previous)
            {
                return Err(
                    "Paykit wire 5.4: the proposal does not preserve the roles at this key".into(),
                );
            }
        }
        Ok(mark)
    }

    /// [`Protocol::record_regression`]'s rules; `built_on` when the write builds on the
    /// version this app holds.
    fn regression(
        &self,
        collection: &str,
        seen: &[u8],
        value: Option<&[u8]>,
        built_on: bool,
        repair: Option<&RecordRepair>,
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
                if side == ActivitySide::Payee
                    && held.lost_to_delete.is_some()
                    && later.lost_to_delete != held.lost_to_delete
                    && !repair.is_some_and(|r| {
                        crate::deletions::restores_payee_marker(r, seen, &later, &own.identity)
                    })
                {
                    return Some(
                        "Paykit wire 5: a later write drops the deletion restriction".into(),
                    );
                }
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
                // Wire 5: a reader that has seen `lost_to_delete` ignores, and shows with its
                // writer, a later write that drops it, whether or not the marker it saw reads
                // (wire 5, "Deletion data that doesn't read"). One that changes it is taken: a
                // put-back answering a later delete names that delete ("Repeated deletes").
                if list_loss_marker(seen).is_some() && list_loss_marker(value).is_none() {
                    return Some("Paykit wire 5: a later write drops lost_to_delete".into());
                }
                let held = list_for_marks(seen, &own.identity)?;
                let Some(later) = list_for_marks(value, &own.identity) else {
                    return Some(
                        "Paykit wire 5.4: a payment_lists record that doesn't read".into(),
                    );
                };
                // Wire 5.4 "Seen lists": a later list of a greater version, or the same list,
                // and never a single-use mark dropped, at any version.
                held.check_later_write(&later).err().map(|e| e.to_string())
            }
            Collection::BlockFloor => {
                let held = BlockFloorRecord::read(seen).ok()?;
                let Ok(later) = BlockFloorRecord::read(value) else {
                    return Some("Paykit wire 5.6: a block_floor record that doesn't read".into());
                };
                // Wire 5.6: the floor only rises.
                held.check_later_write(&later).err().map(|e| e.to_string())
            }
            Collection::Allowances => {
                let held = AllowanceRecord::read(seen, &own.identity).ok()?;
                let Ok(later) = AllowanceRecord::read(value, &own.identity) else {
                    return Some("Paykit wire 5.4: an allowances record that doesn't read".into());
                };
                // Wire 5.4 "Seen roles" and "Rewrites": the roles and history seen stay;
                // "Ledger handoff": a replacement verifies and answers a later grant,
                // even when another grant has followed it (Paykit spec 8.3, "Handoff").
                held.check_later_write(&later)
                    .and_then(|()| held.check_later_handoff(&later, &own.identity))
                    .err()
                    .map(|e| e.to_string())
            }
            _ => None,
        }
    }
}

/// Whether `scopes` holds the write scope of a payee record under `label`'s policy.
pub(crate) fn writes_payee_label(scopes: &[String], label: Option<&Label>) -> bool {
    match label {
        Some(label) => dispatch_scopes::holds(scopes, &label.scope()),
        None => scopes.iter().any(|s| s == "paykit_request"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_recovery_state_missed_item_wrong_types_refused() {
        assert!(serde_json::from_value::<super::MissedItemMark>(
            serde_json::json!({"from_ms":1,"asked":null})
        )
        .is_err());
    }

    #[test]
    fn test_recovery_state_missed_item_round_trip() {
        let mark = super::MissedItemMark {
            from_ms: 300_000,
            asked: vec![[1; 32], [2; 32]],
        };
        let bytes = serde_json::to_vec(&mark).unwrap();
        assert_eq!(
            serde_json::from_slice::<super::MissedItemMark>(&bytes).unwrap(),
            mark
        );
    }

    #[test]
    fn test_recovery_state_missed_item_rejects_unknown() {
        let value = serde_json::json!({"from_ms":300_000,"asked":[],"extra":{"nested":[7]}});
        assert!(serde_json::from_value::<super::MissedItemMark>(value).is_err());
    }

    #[test]
    fn test_recovery_state_missed_item_rejects_duplicates() {
        assert!(serde_json::from_str::<super::MissedItemMark>(
            r#"{"from_ms":1,"from_ms":1,"asked":[]}"#
        )
        .is_err());
    }

    use super::*;

    fn queued_ask_value() -> serde_json::Value {
        serde_json::to_value(QueuedAsk {
            sender: [1; 32],
            from_ms: 7,
            until_ms: None,
            queued: None,
        })
        .unwrap()
    }

    #[test]
    fn test_mark_state_ask_keeps_nullable_members() {
        let value = queued_ask_value();
        assert_eq!(value.as_object().unwrap().len(), 4);
        assert!(value.as_object().unwrap().contains_key("until_ms"));
        assert!(value.as_object().unwrap().contains_key("queued"));
        let read: QueuedAsk = serde_json::from_value(value).unwrap();
        assert_eq!((read.until_ms, read.queued), (None, None));
    }

    #[test]
    fn test_mark_state_ask_requires_until() {
        let mut value = queued_ask_value();
        value.as_object_mut().unwrap().remove("until_ms").unwrap();
        assert!(serde_json::from_value::<QueuedAsk>(value).is_err());
    }

    #[test]
    fn test_mark_state_ask_requires_queued() {
        let mut value = queued_ask_value();
        value.as_object_mut().unwrap().remove("queued").unwrap();
        assert!(serde_json::from_value::<QueuedAsk>(value).is_err());
    }

    #[test]
    fn test_mark_state_ask_refuses_unknown_member() {
        let mut value = queued_ask_value();
        value["extra"] = serde_json::json!({"nested": [1]});
        assert!(serde_json::from_value::<QueuedAsk>(value).is_err());
    }

    #[test]
    fn test_mark_state_ask_refuses_duplicate_members() {
        let value = serde_json::to_string(&queued_ask_value()).unwrap();
        let duplicate = value.replacen('{', "{\"until_ms\":null,", 1);
        assert!(serde_json::from_str::<QueuedAsk>(&duplicate).is_err());
    }

    fn protocol() -> PaykitProtocol {
        PaykitProtocol::new()
    }

    #[test]
    fn test_protocol_keys_paykit_scopes_and_labels_only() {
        let p = protocol();
        assert!(p.has_keys("paykit_pay"));
        assert!(p.has_keys("paykit_request:shop1"));
        assert!(!p.has_keys("paykit_ext:paykit-cap"));
        assert!(!p.has_keys("paykit_pay:x"));
        assert!(!p.has_keys("other"));
    }

    #[test]
    fn test_protocol_resend_request_adds_no_reply_grant() {
        let p = protocol();
        assert!(!p.adds_reply("paykit.resend_request"));
        assert!(p.adds_reply("paykit.payment_request"));
    }

    #[test]
    fn test_protocol_roles_follow_the_scope_table() {
        let pay = vec!["paykit_pay".to_owned()];
        let shop = vec!["paykit_request:shop1".to_owned()];
        assert!(holds_role(&pay, Kind::PaymentRequest.recipients()));
        assert!(!holds_role(&shop, Kind::PaymentRequest.recipients()));
        assert!(holds_role(&shop, Kind::PaymentProof.recipients()));
        assert!(!holds_role(&pay, Kind::PaymentProof.recipients()));
    }

    #[test]
    fn test_protocol_payee_label_write_scope() {
        let label = Label::parse("shop1").unwrap();
        assert!(writes_payee_label(&["paykit_request".into()], Some(&label)));
        assert!(writes_payee_label(
            &["paykit_request:shop1".into()],
            Some(&label)
        ));
        assert!(!writes_payee_label(
            &["paykit_request:shop2".into()],
            Some(&label)
        ));
        assert!(!writes_payee_label(&["paykit_request:shop1".into()], None));
    }

    #[test]
    fn test_protocol_without_own_app_keeps_no_marks() {
        let p = protocol();
        assert_eq!(p.record_seen("payer_activity", None, Some(b"{}")), None);
        assert_eq!(
            p.regression("payer_activity", b"{}", Some(b"{}"), false, None),
            None
        );
    }
}

#[cfg(test)]
mod receive_tests {
    use paykit_lib::derive::natural_key;
    use paykit_lib::records::ActivityRecord;
    use paykit_lib::Body;
    use pubky_dispatch_client::app::RecordTarget;
    use pubky_dispatch_core::ident::record_key_from_blinded;
    use pubky_dispatch_core::records::Standing;
    use serde_json::json;

    use super::*;
    use crate::activity::ActivityTarget;
    use crate::allowances::allowance_target;
    use crate::testkit::{object, Party, NOW};

    fn received(stored: &StoredMessage) -> ReceivedMessage {
        ReceivedMessage {
            // Supplied callback metadata, not a fixture proving feed acceptance.
            receipt: pubky_dispatch_client::app::EntryReceipt {
                entry_id: [1; 16],
                service_key: [2; 32],
                read_at_ms: NOW,
                invite_code_hash: None,
            },
            generation: 1,
            seq: 3,
            sender: stored.sender(),
            accepted_at_ms: NOW,
            message: stored.message().clone(),
            plaintext: stored.plaintext().clone(),
            cert: stored.cert().clone(),
            standing: Standing::Listed,
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
    fn test_missed_widening_keeps_the_items_times() {
        let missed = MissedEntries {
            read_at_ms: 0,
            seq: 1,
            entries: 1,
            accepted_from_ms: 10,
            accepted_until_ms: 20,
        };
        assert_eq!(widening(&missed)["d"]["windows"][0]["from_ms"], 10);
        assert_eq!(widening(&missed)["d"]["windows"][0]["until_ms"], 20);
        assert_eq!(widening(&missed)["d"]["windows"][0]["floor_ms"], 0);
    }

    #[test]
    fn test_missed_widening_raises_to_the_read_that_first_passed_the_item() {
        // Paykit wire 5.6, Raising; spec 7.2, What a `missed` item loses: the widening raises
        // the floor to the earlier of the item's last acceptance time and the authenticated
        // time of the feed page that first passed it (`read_at_ms`), which the client keeps
        // across a retried or delayed callback, never the time the callback runs at.
        let at = |read_at_ms, accepted_until_ms| {
            widening(&MissedEntries {
                read_at_ms,
                seq: 1,
                entries: 1,
                accepted_from_ms: 10,
                accepted_until_ms,
            })["d"]["windows"][0]["floor_ms"]
                .clone()
        };
        assert_eq!(at(15, 20), 15);
        assert_eq!(at(30, 20), 20);
        assert_eq!(at(40, u64::MAX), 40);
    }

    /// The keys `missed()` puts, with their values as JSON.
    fn missed_puts(missed: &MissedEntries) -> Vec<(String, serde_json::Value)> {
        PaykitProtocol::new()
            .missed(missed)
            .unwrap()
            .into_iter()
            .filter_map(|write| match write {
                LocalWrite::Put { key, value } => Some((
                    String::from_utf8(key).unwrap(),
                    serde_json::from_slice(&value).unwrap(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_missed_drops_every_approval_for_execution() {
        // An approval given before the item approves nothing after it, and neither does one
        // a restore carries to its verdict (Paykit spec 7.2, "What a `missed` item loses").
        let missed = MissedEntries {
            read_at_ms: 0,
            seq: 1,
            entries: 0,
            accepted_from_ms: 10,
            accepted_until_ms: 20,
        };
        let deleted: Vec<Vec<u8>> = PaykitProtocol::new()
            .missed(&missed)
            .unwrap()
            .into_iter()
            .filter_map(|write| match write {
                LocalWrite::DeletePrefix { prefix } => Some(prefix),
                _ => None,
            })
            .collect();
        for prefix in [key::EXECUTION_APPROVALS, key::RESTORE_APPROVALS] {
            assert!(deleted.contains(&prefix.as_bytes().to_vec()), "{prefix}");
        }
    }

    #[test]
    fn test_missed_with_no_end_sets_the_holds_end_not_known() {
        // `u64::MAX` is the client's "no end known": the hold's end is set as not known,
        // and no bound is written over an earlier item's.
        let missed = MissedEntries {
            read_at_ms: 0,
            seq: 1,
            entries: 0,
            accepted_from_ms: 10,
            accepted_until_ms: u64::MAX,
        };
        let puts = missed_puts(&missed);
        let value = |key: &str| {
            puts.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v["d"].clone())
        };
        assert_eq!(value(key::MISSED_END_UNKNOWN), Some(json!(true)));
        assert_eq!(value(key::MISSED_HOLD), None);
        // An item with an end is bounded by it, and sets no unknown end.
        let missed = MissedEntries {
            accepted_until_ms: 20,
            ..missed
        };
        let puts = missed_puts(&missed);
        let value = |key: &str| {
            puts.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v["d"].clone())
        };
        assert_eq!(value(key::MISSED_HOLD), Some(json!({ "bound_ms": 20 })));
        assert_eq!(value(key::MISSED_END_UNKNOWN), None);
    }

    fn protocol_of(own: &Party) -> PaykitProtocol {
        let p = PaykitProtocol::new();
        p.set_own(own.id(), own.cert.clone());
        p
    }

    #[test]
    fn test_protocol_only_a_readable_nonempty_private_list_can_add_a_reply_grant() {
        let shop = Party::new("shop", "shop", &["paykit_request", "paykit_settlements"]);
        let p = protocol_of(&shop);
        for (endpoints, expected) in [
            (json!({"btc-bitcoin-p2wpkh": {"payload": "bc1q"}}), true),
            (json!({}), false),
            (json!(null), false),
        ] {
            let message = Message::new(
                Kind::PrivatePaymentList.as_str(),
                pubky_dispatch_core::message::Sender::of_app(shop.id(), &shop.keys),
                [4; 32],
                NOW,
                vec![],
                object(json!({
                    "version": {"generation": 1, "counter": 1},
                    "expires_at": pubky_dispatch_core::time::rfc3339(NOW + 3_600_000),
                    "payment_endpoints": endpoints
                })),
            )
            .unwrap();
            let plaintext = message.sign(&shop.keys).unwrap();
            assert_eq!(p.message_adds_reply(&message, &plaintext), expected);
        }
    }

    fn request_body() -> serde_json::Map<String, serde_json::Value> {
        object(json!({
            "amount": { "asset": "btc", "value": "1" },
            "payment_reference": "order-1",
            "proposal_expires_at": null
        }))
    }

    #[test]
    fn test_receipt_publication_binds_the_signed_source_to_its_request_key() {
        let payer = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let payee = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let request = payee.message(&payer, "paykit.payment_request", vec![], request_body());
        let mut next_order = request_body();
        next_order.insert("payment_reference".into(), "order-2".into());
        let other = payee.message(&payer, "paykit.payment_request", vec![], next_order);
        let attempt = "mAFdg5_0tVNjmrgiTUZW4g";
        let proof = payer.message(&payee, "paykit.payment_proof", vec![request.hash()], object(json!({
            "attempt_id": attempt, "payment_app_id": pubky_dispatch_core::enc::app_id(&payee.keys.signing_public()),
            "payment_endpoint_identifier":"btc-bitcoin-p2wpkh", "proof": {"rail":"fake","tx":"t1"}
        })));
        let receipt = payee.message(
            &payer,
            "paykit.payment_receipt",
            vec![proof.hash(), request.hash()],
            object(json!({"attempt_id":attempt})),
        );
        let request_key = |r: &StoredMessage| {
            ActivityTarget::payee(
                paykit_lib::derive::natural_key::request_record(&payee.id(), &r.hash()),
                None,
            )
            .record_target()
        };
        let first = request_key(&request);
        let second = request_key(&other);
        let naming = payee.key.naming_key();
        let key = |target: &RecordTarget| {
            naming.record_key(
                &target.collection.name,
                &target.natural_key,
                &target.collection.policy.hash().unwrap(),
            )
        };
        let mut activity = ActivityRecord::new(ActivitySide::Payee, payer.id());
        activity.request = Some(request);
        activity.proof = Some(proof);
        activity.receipt = Some(receipt);
        let value = activity.to_json().unwrap();
        let read = |target: &RecordTarget| {
            crate::records::bind_activity(
                &value,
                &payee.id(),
                ActivitySide::Payee,
                None,
                &key(target),
                |t| Some(key(t)),
                &|_, _| None,
            )
        };
        assert!(read(&first).unwrap().receipt.is_some());
        assert!(read(&second).is_none());
    }

    #[test]
    fn test_protocol_check_body_passes_a_valid_message_kept_as_stored() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let stored = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let message = received(&stored);
        assert_eq!(protocol_of(&alice).check_body(&message), Ok(()));
        let kept = stored_copy(&message).unwrap();
        assert_eq!(kept.hash(), stored.hash());
        assert_eq!(kept.accepted_at(), Some(NOW));
    }

    #[test]
    fn test_protocol_check_body_drops_an_unhandled_kind_and_a_bad_body() {
        let alice = Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_allowance", "paykit_contacts"],
        );
        let bob = Party::new(
            "bob",
            "shop",
            &[
                "paykit_request",
                "paykit_allowance",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
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
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let shop1 = Party::new(
            "bob",
            "shop1",
            &[
                "paykit_request:shop1",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
        let shop2 = Party::new(
            "bob",
            "shop2",
            &[
                "paykit_request:shop2",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
        let accepts = ["paykit.payment_request_acceptance"];
        let p = protocol_of(&alice);
        let labelled = alice.message(
            &bob,
            "paykit.payment_request_acceptance",
            vec![[1; 32]],
            object(json!({ "label": "shop1" })),
        );
        assert!(p.narrows(labelled.message(), &shop1.record(&accepts), None));
        assert!(p.narrows(labelled.message(), &bob.record(&accepts), None));
        assert!(!p.narrows(labelled.message(), &shop2.record(&accepts), None));
        let plain = alice.message(
            &bob,
            "paykit.payment_request_acceptance",
            vec![[1; 32]],
            object(json!({})),
        );
        assert!(p.narrows(plain.message(), &bob.record(&accepts), None));
        assert!(!p.narrows(plain.message(), &shop1.record(&accepts), None));
    }

    #[test]
    fn test_protocol_accepts_only_handled_kinds_its_scopes_receive() {
        let p = protocol_of(&Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_contacts"],
        ));
        let wallet = Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_allowance", "paykit_contacts"],
        );
        let kinds = p.accepts(&wallet.cert);
        assert!(kinds.contains(&"paykit.payment_request".to_owned()));
        assert!(!kinds.contains(&"paykit.payment_proof".to_owned()));
        // `paykit_allowance` takes the four Allowance messages; only `paykit_allowance_execute`
        // takes `executor_notice` (Paykit spec 3.3).
        for kind in [
            "paykit.allowance_proposal",
            "paykit.allowance_acceptance",
            "paykit.allowance_rejection",
            "paykit.allowance_end",
        ] {
            assert!(kinds.contains(&kind.to_owned()), "{kind}");
        }
        assert!(!kinds.contains(&"paykit.executor_notice".to_owned()));
        let executor = Party::new(
            "alice",
            "executor",
            &[
                "paykit_pay",
                "paykit_allowance",
                "paykit_allowance_execute",
                "paykit_contacts",
            ],
        );
        assert!(p
            .accepts(&executor.cert)
            .contains(&"paykit.executor_notice".to_owned()));
        // Another app's scopes receive by Paykit's table.
        assert!(p.receives("paykit.allowance_proposal", &wallet.cert));
        assert!(!p.receives("paykit.payment_proof", &wallet.cert));
        let plain = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        assert!(!p
            .accepts(&plain.cert)
            .contains(&"paykit.allowance_proposal".to_owned()));
    }

    #[test]
    fn test_protocol_cert_paykit_refuses_receives_and_accepts_nothing() {
        use pubky_dispatch_core::scopes::Permission;
        let p = protocol_of(&Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_contacts"],
        ));
        // A payee app's scopes need exactly `lists.read` and `reply.add` (Paykit wire
        // 3.1): one without them, or with `claims.read` beside them, is a certificate the Dispatch
        // takes and Paykit refuses.
        let missing = Party::with_permissions(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
            &[],
        );
        let extra = Party::with_permissions(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
            &[
                Permission::ClaimsRead,
                Permission::ListsRead,
                Permission::ReplyAdd,
            ],
        );
        for app in [&missing, &extra] {
            assert!(!p.receives("paykit.payment_proof", &app.cert));
            assert!(p.accepts(&app.cert).is_empty());
        }
        let valid = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        assert!(p.receives("paykit.payment_proof", &valid.cert));
        assert!(!p.accepts(&valid.cert).is_empty());
    }

    #[test]
    fn test_protocol_pay_certificate_without_contacts_refused() {
        // A paying app reads the deny list (Paykit spec 3.1): a certificate holding `paykit_pay`
        // without `paykit_contacts`, its permissions exactly what its scopes need, is one the
        // Dispatch takes and Paykit's verifiers refuse, as a sender, a recipient and an own
        // certificate.
        let wallet = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let request = bob.message(&wallet, "paykit.payment_request", vec![], request_body());
        let pay_only = Party::new("alice", "phone", &["paykit_pay"]);
        let p = protocol_of(&wallet);
        assert!(!p.receives("paykit.payment_request", &pay_only.cert));
        assert!(p.accepts(&pay_only.cert).is_empty());
        let proof = Message::new(
            "paykit.payment_proof",
            pubky_dispatch_core::message::Sender::of_app(pay_only.id(), &pay_only.keys),
            bob.id(),
            NOW,
            vec![[1; 32]],
            object(json!({})),
        )
        .unwrap();
        let error = protocol_of(&bob)
            .check_scopes(&proof, &pay_only.cert, &bob.cert)
            .unwrap_err();
        assert!(
            error.contains("paykit_pay requires paykit_contacts"),
            "{error}"
        );
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
        let wallet = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
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
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
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
            p.regression("payer_activity", &seen, Some(&moved), false, None),
            None
        );
        // Built on the version the app holds, the one that attempted the period, the same
        // write moves an attempted period: a regression.
        assert!(p
            .regression("payer_activity", &seen, Some(&moved), true, None)
            .is_some());
        assert_eq!(
            p.regression("payer_activity", &seen, Some(&moved), false, None),
            None
        );
        // Dropping the entry again is a regression.
        assert!(p
            .regression("payer_activity", &moved, Some(&seen), false, None)
            .is_some());
    }

    /// A stand-in for the naming key's blinding a restore is given.
    fn fake_blind(input: &[u8]) -> [u8; 32] {
        pubky_dispatch_core::enc::hash(input)
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
        let value = state::encode(&KeptMessage::taken_at(
            &reply,
            pubky_dispatch_client::app::FeedPosition {
                generation: 1,
                seq: 1,
            },
        ))
        .unwrap();
        let target = ActivityTarget::payee(natural_key::request_record(&bob.id(), &[4; 32]), None);
        (reply, value, fake_key(&target.record_target()))
    }

    #[test]
    fn test_protocol_restore_takes_a_kept_reply_at_its_record() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let (reply, value, record) = kept_reply(&alice, &bob);
        let at = key::kept_reply(&alice.id(), &record, &reply.message().msg_id);
        assert!(restores::<KeptMessage>(&bob, at.as_bytes(), &value));
    }

    /// Bob's Allowance proposal to alice, his end of it, and the parties.
    fn allowance_end() -> (Party, Party, StoredMessage, StoredMessage) {
        let alice = Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_allowance", "paykit_contacts"],
        );
        let bob = Party::new(
            "bob",
            "shop",
            &[
                "paykit_request",
                "paykit_allowance",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
        let id = "d0efffb7-191b-4709-b6c7-e343af5500b4";
        let terms = json!({
            "asset": "btc", "per_payment_amount": null, "period_limits": [],
            "lifetime_amount_limit": "0.10", "active_from": null, "expires_at": null,
            "allowed_payment_endpoint_identifiers": null
        });
        let proposal = bob.message(
            &alice,
            "paykit.allowance_proposal",
            vec![],
            object(json!({ "allowance_id": id, "proposer_role": "allowee", "allower_generation": 1, "terms": terms })),
        );
        let end = bob.message(
            &alice,
            "paykit.allowance_end",
            vec![proposal.hash()],
            object(json!({ "allowance_id": id })),
        );
        (alice, bob, proposal, end)
    }

    #[test]
    fn test_protocol_restore_takes_a_kept_end_at_its_record() {
        let (alice, bob, _, end) = allowance_end();
        let Body::AllowanceEnd(reply) = end.body() else {
            panic!("an end");
        };
        let target = allowance_target(&bob.id(), &reply.allowance_id);
        let key = key::allowance_end(&fake_key(&target), &end.message().msg_id);
        let value = state::encode(&KeptMessage::taken(&end, 0)).unwrap();
        assert!(restores::<KeptMessage>(&alice, key.as_bytes(), &value));
        // Under another `msg_id` of the same record: refused.
        let other = key::allowance_end(&fake_key(&target), "0d6f5b9e-3c1a-4e8b-9f2d-7a4c1e5b8d30");
        assert!(!restores::<KeptMessage>(&alice, other.as_bytes(), &value));
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_end_at_another_key_or_of_another_kind() {
        let (alice, bob, proposal, end) = allowance_end();
        let value = state::encode(&KeptMessage::taken(&end, 0)).unwrap();
        let other = key::allowance_end(&[9; 32], &end.message().msg_id);
        assert!(!restores::<KeptMessage>(&alice, other.as_bytes(), &value));
        // The proposal under the end's key isn't an end.
        let Body::AllowanceProposal(body) = proposal.body() else {
            panic!("a proposal");
        };
        let target = allowance_target(&bob.id(), &body.allowance_id);
        let key = key::allowance_end(&fake_key(&target), &proposal.message().msg_id);
        let proposal = state::encode(&KeptMessage::of(&proposal)).unwrap();
        assert!(!restores::<KeptMessage>(&alice, key.as_bytes(), &proposal));
    }

    #[test]
    fn test_protocol_restore_takes_a_kept_conflict_and_refuses_one_elsewhere() {
        let (alice, bob, proposal, end) = allowance_end();
        let Body::AllowanceProposal(body) = proposal.body() else {
            panic!("a proposal");
        };
        let record = fake_key(&allowance_target(&bob.id(), &body.allowance_id));
        let at = key::conflict(&record);
        let kept = state::encode(&KeptMessage::taken(&proposal, 1)).unwrap();
        assert!(restores::<KeptMessage>(&alice, at.as_bytes(), &kept));
        // Under another record's key, or an end under a conflict's key: refused.
        let other = key::conflict(&[9; 32]);
        assert!(!restores::<KeptMessage>(&alice, other.as_bytes(), &kept));
        let end = state::encode(&KeptMessage::taken(&end, 1)).unwrap();
        assert!(!restores::<KeptMessage>(&alice, at.as_bytes(), &end));
    }

    #[test]
    fn test_protocol_record_regression_on_a_dropped_allowance_end() {
        let (alice, bob, proposal, _) = allowance_end();
        let p = protocol_of(&alice);
        let Body::AllowanceProposal(body) = proposal.body() else {
            panic!("a proposal");
        };
        let reply = object(json!({ "allowance_id": body.allowance_id.as_str() }));
        let acceptance = alice.message(
            &bob,
            "paykit.allowance_acceptance",
            vec![proposal.hash()],
            reply.clone(),
        );
        let end = bob.message(
            &alice,
            "paykit.allowance_end",
            vec![proposal.hash(), acceptance.hash()],
            reply,
        );
        let open = |end: Option<StoredMessage>| {
            AllowanceRecord {
                counterparty: bob.id(),
                allowance_id: body.allowance_id.clone(),
                proposal: proposal.clone(),
                response: Some(acceptance.clone()),
                end,
                conflicted: None,
                grant: None,
                grants: Vec::new(),
                ledger_handoff: None,
                ledger_handoff_unread: None,
                msg_ids: Vec::new(),
                unknown: Default::default(),
            }
            .to_json()
            .unwrap()
        };
        let seen = open(None);
        let ended = open(Some(end));
        assert_eq!(
            p.regression("allowances", &seen, Some(&ended), false, None),
            None
        );
        assert_eq!(
            p.record_seen("allowances", Some(&seen), Some(&ended)),
            Some(ended.clone())
        );
        // A later write that drops the end seen is refused, as is one that doesn't read.
        assert!(p
            .regression("allowances", &ended, Some(&seen), false, None)
            .is_some());
        assert!(p
            .regression("allowances", &seen, Some(b"{}"), false, None)
            .is_some());
    }

    fn handoff_version(version: u64) -> pubky_dispatch_core::ops::RecordVersion {
        pubky_dispatch_core::ops::RecordVersion {
            generation: 1,
            version,
        }
    }

    fn append_handoff_grant(record: &mut AllowanceRecord, app: &Party, version: u64) {
        let built_on = handoff_version(version);
        let grant = paykit_lib::grant::ExecutorGrant::sign(
            &app.keys,
            app.cert.signed().clone(),
            &app.id(),
            &record.counterparty,
            &record.allowance_id,
            record.terms_hash().unwrap(),
            1,
            built_on,
        );
        record.grants.push(paykit_lib::records::GrantEntry {
            built_on,
            grant: grant.clone(),
            origin: Default::default(),
        });
        record.grant = Some(grant);
    }

    fn signed_handoff(
        record: &AllowanceRecord,
        signer: &Party,
        ledger: (u64, [u8; 32]),
    ) -> paykit_lib::grant::LedgerHandoff {
        paykit_lib::grant::LedgerHandoff::sign(
            &signer.keys,
            signer.cert.signed().clone(),
            &signer.id(),
            &record.counterparty,
            &record.allowance_id,
            record.grants.last().unwrap().built_on,
            handoff_version(ledger.0),
            ledger.1,
        )
    }

    /// Two consecutive handoffs, with valid signed messages, grants and ledger tuples.
    fn handoff_records() -> (Party, Party, AllowanceRecord, AllowanceRecord) {
        let scopes = [
            "paykit_pay",
            "paykit_allowance",
            "paykit_allowance_execute",
            "paykit_contacts",
        ];
        let first = Party::new("alice", "first", &scopes);
        let second = Party::new("alice", "second", &scopes);
        let (_, bob, proposal, _) = allowance_end();
        let Body::AllowanceProposal(body) = proposal.body() else {
            panic!("an Allowance proposal");
        };
        let allowance_id = body.allowance_id.clone();
        let response = first.message(
            &bob,
            "paykit.allowance_acceptance",
            vec![proposal.hash()],
            object(json!({ "allowance_id": allowance_id.as_str() })),
        );
        let mut handed = AllowanceRecord {
            counterparty: bob.id(),
            allowance_id,
            proposal,
            response: Some(response),
            end: None,
            conflicted: None,
            grant: None,
            grants: Vec::new(),
            msg_ids: Vec::new(),
            ledger_handoff: None,
            ledger_handoff_unread: None,
            unknown: Default::default(),
        };
        append_handoff_grant(&mut handed, &first, 1);
        append_handoff_grant(&mut handed, &second, 3);
        handed.ledger_handoff = Some(signed_handoff(&handed, &first, (2, [2; 32])));
        let mut back = handed.clone();
        append_handoff_grant(&mut back, &first, 5);
        back.ledger_handoff = Some(signed_handoff(&back, &second, (4, [4; 32])));
        for record in [&handed, &back] {
            let read = AllowanceRecord::read(&record.to_json().unwrap(), &first.id()).unwrap();
            assert!(read.current_grant(&first.id()).is_some());
            assert!(read.ledger_handoff_counts(&first.id()).unwrap().is_some());
        }
        (first, second, handed, back)
    }

    /// A signed handoff whose only unread field is a ledger version no write gives.
    fn unread_handoff(record: &AllowanceRecord, signer: &Party, grant: u64) -> AllowanceRecord {
        use paykit_lib::grant::{handoff_fields, LedgerHandoff, LEDGER_HANDOFF_LABEL};
        let signed = LedgerHandoff::sign(
            &signer.keys,
            signer.cert.signed().clone(),
            &signer.id(),
            &record.counterparty,
            &record.allowance_id,
            handoff_version(grant),
            handoff_version(0),
            [2; 32],
        );
        let fields = handoff_fields(
            &signer.id(),
            &record.counterparty,
            &record.allowance_id,
            signed.grant,
            signed.ledger,
            &signed.item_hash,
        );
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        pubky_dispatch_core::sig::verify(
            &signer.keys.signing_public(),
            LEDGER_HANDOFF_LABEL,
            &refs,
            &signed.sig,
        )
        .unwrap();
        let mut value: Value = serde_json::from_slice(&record.to_json().unwrap()).unwrap();
        value["ledger_handoff"] = signed.to_json();
        let read =
            AllowanceRecord::read(&serde_json::to_vec(&value).unwrap(), &signer.id()).unwrap();
        let unread = read.ledger_handoff_unread.as_ref().unwrap();
        assert!(read.ledger_handoff.is_none());
        assert_eq!(
            LedgerHandoff::from_json(unread).unwrap_err().to_string(),
            "invalid: Paykit wire 5.4: ledger_handoff names a ledger version a write gives"
        );
        read
    }

    fn handoff_regression(
        own: &Party,
        seen: &AllowanceRecord,
        later: &AllowanceRecord,
    ) -> Option<String> {
        // Pass serialized records through the shared regression rule and its parsing.
        protocol_of(own).regression(
            "allowances",
            &seen.to_json().unwrap(),
            Some(&later.to_json().unwrap()),
            false,
            None,
        )
    }

    #[test]
    fn test_protocol_handoff_regression_refuses_a_dropped_handoff() {
        let (own, _, seen, _) = handoff_records();
        let mut later = seen.clone();
        later.ledger_handoff = None;
        let reason = handoff_regression(&own, &seen, &later).unwrap();
        assert!(reason.contains("drops the ledger_handoff"), "{reason}");
        assert_eq!(handoff_regression(&own, &seen, &seen), None);
        assert_eq!(handoff_regression(&own, &later, &later), None);
    }

    #[test]
    fn test_protocol_handoff_regression_refuses_a_dropped_unread_handoff() {
        let (own, _, handed, _) = handoff_records();
        let seen = unread_handoff(&handed, &own, 3);
        let mut later = seen.clone();
        later.ledger_handoff_unread = None;
        let reason = handoff_regression(&own, &seen, &later).unwrap();
        assert!(reason.contains("drops the ledger_handoff"), "{reason}");
        assert_eq!(handoff_regression(&own, &seen, &seen), None);
    }

    #[test]
    fn test_protocol_handoff_regression_refuses_an_earlier_grant() {
        let (own, _, handed, seen) = handoff_records();
        let mut later = seen.clone();
        later.ledger_handoff = handed.ledger_handoff;
        later
            .check_ledger_handoff(&own.id(), later.ledger_handoff.as_ref().unwrap())
            .unwrap();
        let reason = handoff_regression(&own, &seen, &later).unwrap();
        assert!(reason.contains("isn't for a later grant"), "{reason}");
    }

    #[test]
    fn test_protocol_handoff_regression_refuses_a_changed_same_grant() {
        let (own, _, seen, _) = handoff_records();
        for version in [1, 2, 3] {
            let mut later = seen.clone();
            later.ledger_handoff = Some(signed_handoff(&seen, &own, (version, [8; 32])));
            assert!(later.ledger_handoff_counts(&own.id()).unwrap().is_some());
            let reason = handoff_regression(&own, &seen, &later).unwrap();
            assert!(reason.contains("isn't for a later grant"), "{reason}");
        }
    }

    #[test]
    fn test_protocol_handoff_regression_takes_a_later_current_grant() {
        let (own, _, seen, later) = handoff_records();
        assert!(later.ledger_handoff_counts(&own.id()).unwrap().is_some());
        assert_eq!(handoff_regression(&own, &seen, &later), None);
        let unread = unread_handoff(&seen, &own, 3);
        assert_eq!(handoff_regression(&own, &unread, &later), None);
    }

    #[test]
    fn test_protocol_handoff_regression_takes_a_later_historical_grant() {
        let (own, next, seen, mut later) = handoff_records();
        append_handoff_grant(&mut later, &next, 7);
        assert_eq!(later.ledger_handoff_counts(&own.id()).unwrap(), None);
        assert_eq!(handoff_regression(&own, &seen, &later), None);
        let mut none_seen = seen.clone();
        none_seen.ledger_handoff = None;
        assert_eq!(handoff_regression(&own, &none_seen, &later), None);
    }

    #[test]
    fn test_protocol_handoff_regression_keeps_an_unplaced_unread_handoff() {
        let (own, _, handed, later) = handoff_records();
        let seen = unread_handoff(&handed, &own, 9);
        let reason = handoff_regression(&own, &seen, &later).unwrap();
        assert!(reason.contains("isn't for a later grant"), "{reason}");
        assert_eq!(handoff_regression(&own, &seen, &seen), None);
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_reply_at_another_key() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
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
        let carol = Party::new(
            "carol",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let receipt = carol.message(
            &bob,
            "paykit.payment_receipt",
            vec![[4; 32], [5; 32]],
            object(json!({ "attempt_id": "DgCVYQKL1eyDacZvG6YhZg" })),
        );
        let value = state::encode(&KeptMessage::taken_at(
            &receipt,
            pubky_dispatch_client::app::FeedPosition {
                generation: 1,
                seq: 1,
            },
        ))
        .unwrap();
        let at = key::kept_reply(&carol.id(), &record, &receipt.message().msg_id);
        assert!(!restores::<KeptMessage>(&bob, at.as_bytes(), &value));
    }

    /// Alice's acceptance of bob's Allowance proposal, kept at bob's shop before his record,
    /// and the record key of his Allowance record it names.
    fn kept_allowance_answer(alice: &Party, bob: &Party) -> (StoredMessage, Vec<u8>, [u8; 32]) {
        let id = "d0efffb7-191b-4709-b6c7-e343af5500b4";
        let answer = alice.message(
            bob,
            "paykit.allowance_acceptance",
            vec![[4; 32]],
            object(json!({ "allowance_id": id })),
        );
        let Body::AllowanceAcceptance(body) = answer.body() else {
            panic!("an acceptance");
        };
        let record = fake_key(&allowance_target(&alice.id(), &body.allowance_id));
        let at = pubky_dispatch_client::app::FeedPosition {
            generation: 1,
            seq: 1,
        };
        let value = state::encode(&KeptMessage::taken_at(&answer, at)).unwrap();
        (answer, value, record)
    }

    #[test]
    fn test_protocol_restore_takes_a_kept_allowance_answer_and_refuses_one_elsewhere() {
        let alice = Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_allowance", "paykit_contacts"],
        );
        let bob = Party::new(
            "bob",
            "shop",
            &[
                "paykit_request",
                "paykit_allowance",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
        let (answer, value, record) = kept_allowance_answer(&alice, &bob);
        let msg_id = answer.message().msg_id.clone();
        let at = key::kept_reply(&alice.id(), &record, &msg_id);
        assert!(restores::<KeptMessage>(&bob, at.as_bytes(), &value));
        for other in [
            key::kept_reply(&alice.id(), &[9; 32], &msg_id),
            key::kept_reply(&alice.id(), &record, "123e4567-e89b-42d3-a456-426614174000"),
            key::kept_reply(&[8; 32], &record, &msg_id),
        ] {
            assert!(
                !restores::<KeptMessage>(&bob, other.as_bytes(), &value),
                "{other}"
            );
        }
        // Kept only as its inbox accepted it, with its time.
        assert!(!restores::<KeptMessage>(
            &bob,
            at.as_bytes(),
            &without_acceptance(&value)
        ));
    }

    #[test]
    fn test_protocol_restore_refuses_a_pooled_kept_message_it_cannot_order() {
        // The kept replies and the kept stops are bounded pools ordered by when their inbox
        // accepted each member, which decides what the bounds drop, which of an Allowance's
        // ends stops it and which of its answers is the record's response: a member without
        // the feed `seq` its entry was read at can't be ordered, so a restore refuses it, of
        // every family of a pool.
        let (alice, bob, _, end) = allowance_end();
        let Body::AllowanceEnd(reply) = end.body() else {
            panic!("an end");
        };
        let target = allowance_target(&bob.id(), &reply.allowance_id);
        let key = key::allowance_end(&fake_key(&target), &end.message().msg_id);
        let unordered = state::encode(&KeptMessage::of(&end)).unwrap();
        assert!(!restores::<KeptMessage>(&alice, key.as_bytes(), &unordered));
        let alice = Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_allowance", "paykit_contacts"],
        );
        let bob = Party::new(
            "bob",
            "shop",
            &[
                "paykit_request",
                "paykit_allowance",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
        let (answer, _, record) = kept_allowance_answer(&alice, &bob);
        let at = key::kept_reply(&alice.id(), &record, &answer.message().msg_id);
        let unordered = state::encode(&KeptMessage::of(&answer)).unwrap();
        assert!(!restores::<KeptMessage>(&bob, at.as_bytes(), &unordered));
        let missing_generation = state::encode(&KeptMessage::taken(&answer, 1)).unwrap();
        assert!(!restores::<KeptMessage>(
            &bob,
            at.as_bytes(),
            &missing_generation
        ));
        let (reply, _, record) = kept_reply(&alice, &bob);
        let at = key::kept_reply(&alice.id(), &record, &reply.message().msg_id);
        let unordered = state::encode(&KeptMessage::of(&reply)).unwrap();
        assert!(!restores::<KeptMessage>(&bob, at.as_bytes(), &unordered));
        let wallet = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let shop = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let cancellation = shop.message(
            &wallet,
            "paykit.payment_request_cancellation",
            vec![[4; 32]],
            object(json!({})),
        );
        let target = ActivityTarget::payer(natural_key::request_record(&shop.id(), &[4; 32]));
        let key = key::kept_cancellation(&fake_key(&target.record_target()), &shop.cert.hash(), 1);
        let unordered = state::encode(&KeptMessage::of(&cancellation)).unwrap();
        assert!(!restores::<KeptMessage>(
            &wallet,
            key.as_bytes(),
            &unordered
        ));
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_end_without_its_acceptance_time() {
        let (alice, bob, _, end) = allowance_end();
        let Body::AllowanceEnd(reply) = end.body() else {
            panic!("an end");
        };
        let key = key::allowance_end(
            &fake_key(&allowance_target(&bob.id(), &reply.allowance_id)),
            &end.message().msg_id,
        );
        let value = state::encode(&KeptMessage::taken(&end, 0)).unwrap();
        assert!(!restores::<KeptMessage>(
            &alice,
            key.as_bytes(),
            &without_acceptance(&value)
        ));
    }

    #[test]
    fn test_protocol_restore_takes_a_consent_entry_only_at_its_allowances_record() {
        // The entry names its Allowance; under the key of another record it would consent
        // to an Allowance its user never confirmed.
        let (alice, bob, proposal, _) = allowance_end();
        let Body::AllowanceProposal(body) = proposal.body() else {
            panic!("a proposal");
        };
        let record = fake_key(&allowance_target(&bob.id(), &body.allowance_id));
        let entry = crate::allowances::ConsentEntry::new(
            bob.id(),
            &body.allowance_id,
            crate::allowances::Consent::Held {
                confirm_again: false,
            },
        );
        let value = state::encode(&entry).unwrap();
        assert!(restores::<ConsentEntry>(
            &alice,
            key::consent(&record).as_bytes(),
            &value
        ));
        assert!(!restores::<ConsentEntry>(
            &alice,
            key::consent(&[9; 32]).as_bytes(),
            &value
        ));
        let other = crate::allowances::ConsentEntry::new(
            alice.id(),
            &body.allowance_id,
            crate::allowances::Consent::Held {
                confirm_again: false,
            },
        );
        assert!(!restores::<ConsentEntry>(
            &alice,
            key::consent(&record).as_bytes(),
            &state::encode(&other).unwrap()
        ));
    }

    /// `value`, a kept message's encoded bytes, with its acceptance time removed.
    fn without_acceptance(value: &[u8]) -> Vec<u8> {
        let mut json: serde_json::Value = serde_json::from_slice(value).unwrap();
        json["d"]["accepted_at"] = serde_json::Value::Null;
        serde_json::to_vec(&json).unwrap()
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_conflict_without_its_acceptance_time() {
        // Every writer of `cf/` keeps a message its inbox accepted, with the time: a proposal
        // or a response conflict without it is no state a run leaves, and the restore
        // refuses it rather than install a stop for good.
        let (alice, bob, proposal, _) = allowance_end();
        let Body::AllowanceProposal(body) = proposal.body() else {
            panic!("a proposal");
        };
        let record = fake_key(&allowance_target(&bob.id(), &body.allowance_id));
        let at = key::conflict(&record);
        let response = bob.message(
            &alice,
            "paykit.allowance_rejection",
            vec![proposal.hash()],
            object(json!({ "allowance_id": body.allowance_id.as_str() })),
        );
        for conflict in [&proposal, &response] {
            let position = pubky_dispatch_client::app::FeedPosition {
                generation: 1,
                seq: 1,
            };
            let value = state::encode(&KeptMessage::taken_at(conflict, position)).unwrap();
            assert!(restores::<KeptMessage>(&alice, at.as_bytes(), &value));
            assert!(!restores::<KeptMessage>(
                &alice,
                at.as_bytes(),
                &without_acceptance(&value)
            ));
        }
    }

    #[test]
    fn test_protocol_restore_refuses_a_held_list_without_its_acceptance_time() {
        // Every writer of `lh/` keeps a list its inbox accepted, with the time, which orders
        // it behind the writes kept over its record: a restore refuses one without it.
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let list = bob.message(
            &alice,
            "paykit.private_payment_list",
            vec![],
            object(json!({
                "version": { "generation": 1, "counter": 7 },
                "expires_at": pubky_dispatch_core::time::rfc3339(NOW + 3_600_000),
                "payment_endpoints": {
                    "btc-bitcoin-p2wpkh": { "payload": "bc1q", "single_use": true }
                }
            })),
        );
        let target = crate::records::list_target(&bob.id(), &list.message().from.app_id);
        let at = key::held_list(&fake_key(&target));
        let value = state::encode(&KeptMessage::of(&list)).unwrap();
        assert!(restores::<KeptMessage>(&alice, at.as_bytes(), &value));
        assert!(!restores::<KeptMessage>(
            &alice,
            at.as_bytes(),
            &without_acceptance(&value)
        ));
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_conflict_it_cannot_order() {
        // A conflict's stop is ordered among its record's answers by when its inbox accepted
        // it: an answer accepted after it never takes the response. Every writer keeps it
        // with the feed `seq` its entry was read at, so a restore refuses one without it.
        let (alice, bob, proposal, _) = allowance_end();
        let Body::AllowanceProposal(body) = proposal.body() else {
            panic!("a proposal");
        };
        let at = key::conflict(&fake_key(&allowance_target(&bob.id(), &body.allowance_id)));
        let value = state::encode(&KeptMessage::of(&proposal)).unwrap();
        assert!(!restores::<KeptMessage>(&alice, at.as_bytes(), &value));
    }

    /// `value`, a kept message's encoded bytes, with its feed `seq` set to `seq`, or removed
    /// for `None`.
    fn with_seq(value: &[u8], seq: Option<serde_json::Value>) -> Vec<u8> {
        let mut json: serde_json::Value = serde_json::from_slice(value).unwrap();
        let kept = json["d"].as_object_mut().unwrap();
        match seq {
            Some(seq) => kept.insert("seq".into(), seq),
            None => kept.remove("seq"),
        };
        serde_json::to_vec(&json).unwrap()
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_unrequested_proof_it_cannot_order() {
        // A proof of a payment without a request is a kept reply (`pr/`) like any other, and
        // is placed among them in the order its inbox accepted it: one kept without its full
        // feed position can't be ordered, so a restore refuses it, though its
        // record, which only the rail names, isn't bound until it is placed.
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let proof = alice.message(
            &bob,
            "paykit.payment_proof",
            Vec::new(),
            object(json!({
                "attempt_id": "mAFdg5_0tVNjmrgiTUZW4g",
                "payment_app_id": "brrz3m6g5rci6iqnzke44q4djc",
                "payment_endpoint_identifier": "btc-bitcoin-p2wpkh",
                "amount": { "asset": "btc", "value": "0.1" },
                "proof": { "rail": "fake", "tx": "t1" },
            })),
        );
        let at = key::kept_reply(&alice.id(), &[9; 32], &proof.message().msg_id);
        let value = state::encode(&KeptMessage::taken_at(
            &proof,
            pubky_dispatch_client::app::FeedPosition {
                generation: 1,
                seq: 3,
            },
        ))
        .unwrap();
        assert!(restores::<KeptMessage>(&bob, at.as_bytes(), &value));
        assert!(restores::<KeptMessage>(
            &bob,
            at.as_bytes(),
            &with_seq(&value, Some(json!(0)))
        ));
        for unordered in [
            with_seq(&value, None),
            with_seq(&value, Some(serde_json::Value::Null)),
            with_seq(&value, Some(json!("3"))),
            without_acceptance(&value),
        ] {
            assert!(!restores::<KeptMessage>(&bob, at.as_bytes(), &unordered));
        }
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_reply_without_its_acceptance_time() {
        // A reply is kept only as its inbox accepted it: one without the time can't come
        // from a valid run.
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
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
        let value = state::encode(&KeptMessage::taken_at(
            &cancellation,
            pubky_dispatch_client::app::FeedPosition {
                generation: 1,
                seq: 1,
            },
        ))
        .unwrap();
        let target = ActivityTarget::payer(natural_key::request_record(&bob.id(), &[4; 32]));
        let record = fake_key(&target.record_target());
        (
            key::kept_cancellation(&record, &bob.cert.hash(), 1).into_bytes(),
            value,
        )
    }

    #[test]
    fn test_protocol_restore_takes_a_kept_cancellation_at_its_record() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let (key, value) = kept_cancellation(&alice, &bob);
        assert!(restores::<KeptMessage>(&alice, &key, &value));
    }

    #[test]
    fn test_protocol_restore_refuses_a_kept_cancellation_at_another_key_or_without_its_time() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let (key, value) = kept_cancellation(&alice, &bob);
        let other = key::kept_cancellation(&[9; 32], &bob.cert.hash(), 1).into_bytes();
        assert!(!restores::<KeptMessage>(&alice, &other, &value));
        // A renewal of the same app can grant different selling scopes.
        let target = ActivityTarget::payer(natural_key::request_record(&bob.id(), &[4; 32]));
        let record = fake_key(&target.record_target());
        let other_cert = key::kept_cancellation(&record, &[6; 32], 1).into_bytes();
        assert!(!restores::<KeptMessage>(&alice, &other_cert, &value));
        let other_generation = key::kept_cancellation(&record, &bob.cert.hash(), 2);
        assert!(!restores::<KeptMessage>(
            &alice,
            other_generation.as_bytes(),
            &value
        ));
        assert!(!restores::<KeptMessage>(
            &alice,
            &key,
            &without_acceptance(&value)
        ));
    }

    #[test]
    fn test_protocol_missed_marks_bound_to_their_time_and_asks() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
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
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let msg_id = request.message().msg_id.clone();
        let note = crate::stored::StoredNote {
            stored_id: 1,
            hash: request.hash(),
            sender: bob.id(),
            msg_id: msg_id.clone(),
            message: KeptMessage::taken(&request, 1),
            placed: false,
        };
        let value = state::encode(&note).unwrap();
        let at = key::stored_note(&bob.id(), &msg_id);
        assert!(restores::<crate::stored::StoredNote>(
            &alice,
            at.as_bytes(),
            &value
        ));
        let bound: crate::stored::BoundNote =
            state::bound(at.as_bytes(), &value, &state::Binder::unblinded(alice.id())).unwrap();
        assert!(bound.writes_request(&request.hash()));
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
    fn test_protocol_stored_receipt_does_not_imply_delivery_intent() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let seller_wallet = Party::new(
            "bob",
            "seller-wallet",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
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
            crate::hold::own_to_send_again(record, &bob.id(), &bob.cert.app_id(), true)
                .into_iter()
                .map(|stored| stored.message().kind.clone())
                .collect::<Vec<_>>()
        };
        assert!(own(&record).is_empty());
        record.cancellation = Some(bob.message(
            &alice,
            "paykit.payment_request_cancellation",
            vec![[4; 32]],
            object(json!({ "reason": null })),
        ));
        assert_eq!(own(&record), ["paykit.payment_request_cancellation"]);
        record.cancellation = Some(seller_wallet.message(
            &alice,
            "paykit.payment_request_cancellation",
            vec![[4; 32]],
            object(json!({ "reason": null })),
        ));
        record.receipt = Some(receipt(&seller_wallet));
        assert!(own(&record).is_empty());
    }

    #[test]
    fn test_protocol_restore_selects_each_period_detail_signed_by_this_app() {
        // Reconciliation covers current and earlier details in signing order (Paykit
        // spec 7.2). Another app can also have signed details in the same record.
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let seller_wallet = Party::new(
            "bob",
            "seller-wallet",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let details = |by: &Party, expires: &str| {
            by.message(
                &alice,
                "paykit.payment_period_details",
                vec![[7; 32]],
                object(json!({
                    "billing_period": {
                        "starts_at": "2026-02-28T00:00:00.000Z",
                        "ends_at": "2026-03-31T00:00:00.000Z"
                    },
                    "expires_at": expires,
                    "payment_endpoints": { "btc-lightning-bolt11": "lnbc1period" }
                })),
            )
        };
        let mut record = activity::empty(ActivitySide::Payee, alice.id());
        for (by, expires) in [
            (&bob, "2026-03-01T00:00:00.000Z"),
            (&bob, "2026-03-02T00:00:00.000Z"),
            (&seller_wallet, "2026-03-03T00:00:00.000Z"),
        ] {
            assert!(activity::place(
                &mut record,
                activity::Role::PeriodDetails,
                details(by, expires)
            ));
        }
        assert_eq!(record.earlier_details.len(), 2);
        let own = |record: &ActivityRecord| {
            crate::hold::own_to_send_again(record, &bob.id(), &bob.cert.app_id(), true)
                .into_iter()
                .map(|stored| stored.hash())
                .collect::<Vec<_>>()
        };
        let mut expected: Vec<[u8; 32]> = record
            .earlier_details
            .iter()
            .chain(record.period_details.iter())
            .filter(|stored| stored.cert().app_id() == bob.cert.app_id())
            .map(|stored| stored.hash())
            .collect();
        assert_eq!(expected.len(), 2);
        assert_eq!(own(&record), expected);
        // Whichever the record holds as current: a proof naming the earliest makes it the
        // period's current details, and each is still queued once.
        let earliest = record.earlier_details[0].clone();
        let body = paykit_lib::bodies::PaymentProof {
            attempt_id: paykit_lib::AttemptId::from_bytes([9; 16]),
            billing_period: Some(paykit_lib::periods::PeriodRef {
                starts_at: 1_772_236_800_000,
                ends_at: 1_774_915_200_000,
            }),
            payment_endpoint_identifier: paykit_lib::PaymentEndpointIdentifier::new(
                "btc-lightning-bolt11",
            )
            .unwrap(),
            payment_app_id: [0; 16],
            list_version: None,
            amount: None,
            allowance_id: None,
            label: None,
            payment_reference: None,
            proof: object(json!({ "preimage": "00" })),
        }
        .to_body(2)
        .unwrap();
        let proof = alice.message(
            &bob,
            "paykit.payment_proof",
            vec![[7; 32], earliest.hash()],
            body,
        );
        assert!(activity::place(&mut record, activity::Role::Proof, proof));
        assert_eq!(
            record.period_details.as_ref().map(|d| d.hash()),
            Some(earliest.hash())
        );
        let mut sent = own(&record);
        sent.sort();
        expected.sort();
        assert_eq!(sent, expected);
        assert_eq!(
            crate::hold::own_details(&record, &bob.id(), &bob.cert.app_id()).len(),
            2
        );
        // A stop the request record or the period's credit shows covers them: none goes.
        assert!(
            crate::hold::own_to_send_again(&record, &bob.id(), &bob.cert.app_id(), false)
                .is_empty()
        );
        // A later stop covers every one of them: only the stop goes again.
        let cancellation = bob.message(
            &alice,
            "paykit.payment_request_cancellation",
            vec![[7; 32]],
            object(json!({ "reason": null })),
        );
        record.cancellation = Some(cancellation.clone());
        assert_eq!(own(&record), [cancellation.hash()]);
    }

    #[test]
    fn test_protocol_record_regression_on_a_dropped_role() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let p = protocol_of(&alice);
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let mut seen = activity::empty(ActivitySide::Payer, bob.id());
        activity::place(&mut seen, activity::Role::Request, request.clone());
        let seen = seen.to_json().unwrap();
        // The same record and one with more are fine.
        assert_eq!(
            p.regression("payer_activity", &seen, Some(&seen), false, None),
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
            p.regression("payer_activity", &seen, Some(&more), false, None),
            None
        );
        // Dropping the request is a regression; so is a value that doesn't read.
        let dropped = activity::empty(ActivitySide::Payer, bob.id())
            .to_json()
            .unwrap();
        assert!(p
            .regression("payer_activity", &more, Some(&dropped), false, None)
            .is_some());
        assert!(p
            .regression("payer_activity", &seen, Some(b"{}"), false, None)
            .is_some());
        // A delete isn't one.
        assert_eq!(
            p.regression("payer_activity", &seen, None, false, None),
            None
        );
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
