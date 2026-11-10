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

use std::sync::{PoisonError, RwLock};

use paykit_lib::bodies::check_body;
use paykit_lib::collections::Collection;
use paykit_lib::kinds::{targets_app, Kind, Role as KindRole, Side, OPAQUE_MEMBERS};
use paykit_lib::records::{ActivitySide, PaymentListRecord, StoredMessage};
use paykit_lib::scopes::{acts_as_payee, PaykitScope, ScopeName};
use paykit_lib::{Label, PaykitKindRules};
use pubky_switch_client::app::{
    Collection as SwitchCollection, Effect, LocalView, LocalWrite, LossActions, MissedEntries,
    OpaqueRule, Outcome, Protocol, ProviderLoss, ReceiveContext, ReceivedMessage, ResendNeeded,
};
use pubky_switch_core::enc::{serde_b64, unb64_array};
use pubky_switch_core::message::Message;
use pubky_switch_core::receive::KindRules;
use pubky_switch_core::records::{AppCert, AppRecord};
use pubky_switch_core::scopes as switch_scopes;
use serde::{Deserialize, Serialize};

use crate::activity::{self, SideTag};
use crate::receive::{self, Fail};
use crate::state::{self, key};

/// The kinds this SDK handles (Paykit wire 4.3): the one-time request flow, payments without
/// a request, the payer's side of Private Payment Lists and re-sends. Recurring requests,
/// Allowances and publishing Private Payment Lists come later.
pub const HANDLED_KINDS: [Kind; 8] = [
    Kind::PaymentRequest,
    Kind::PaymentRequestAcceptance,
    Kind::PaymentRequestRejection,
    Kind::PaymentRequestCancellation,
    Kind::PaymentProof,
    Kind::PaymentReceipt,
    Kind::ResendRequest,
    Kind::PrivatePaymentList,
];

/// A re-send to ask of a sender (`p/ask/`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QueuedAsk {
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    pub(crate) from_ms: u64,
    pub(crate) until_ms: Option<u64>,
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

/// A record of Paykit's local state, read through a [`LocalView`]: `Err` when the store
/// can't be read or the record doesn't read, which the caller never takes as an absent one.
fn view_get<T: serde::de::DeserializeOwned>(
    view: &LocalView<'_>,
    key: &str,
) -> std::result::Result<Option<T>, ()> {
    match view.local_get(key.as_bytes()).map_err(|_| ())? {
        None => Ok(None),
        Some(bytes) => state::decode(&bytes).map(Some).map_err(|_| ()),
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

impl Protocol for PaykitProtocol {
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
        PaykitKindRules::for_certificate(own)
            .check_scopes(message, sender)
            .map_err(|error| error.to_string())
    }

    /// The kinds this SDK handles that the certificate's scopes receive (Switch spec 3.4).
    fn accepts(&self, cert: &AppCert) -> Vec<String> {
        HANDLED_KINDS
            .iter()
            .filter(|kind| holds_role(cert.scopes(), kind.recipients()))
            .map(|kind| kind.as_str().to_owned())
            .collect()
    }

    /// Whether another app's scopes receive `kind` by Paykit's table (Paykit spec 3.3), which
    /// a sender checks before it wraps the kind to that app.
    fn receives(&self, kind: &str, cert: &AppCert) -> bool {
        Kind::from_kind(kind).is_some_and(|k| holds_role(cert.scopes(), k.recipients()))
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
        // the message isn't sealed to this recipient.
        match view_get::<SideTag>(view, &key::side(&message.msg_id)) {
            Ok(kept) => self.narrows(message, recipient, kept),
            Err(()) => false,
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
        let stored = match stored_copy(message) {
            Ok(stored) => stored,
            Err(reason) => return Outcome::Drop { reason },
        };
        match receive::effect(cx, &stored, message.seq) {
            // A write the context couldn't build for want of a key: the context holds the
            // entry itself, whatever is returned (Switch spec 3.5).
            Ok(()) | Err(Fail::NotNow) => Outcome::Apply(Effect::default()),
            Err(Fail::Need(needs)) => Outcome::Need(needs),
            Err(Fail::Drop(reason)) => Outcome::Drop { reason },
            Err(Fail::Store(failure)) => Outcome::Retry(failure),
        }
    }

    /// Entries the app can't read: ask the sender to send again (Paykit spec 9).
    fn resend_needed(&self, needed: &ResendNeeded) -> Vec<LocalWrite> {
        let ask = QueuedAsk {
            sender: needed.sender,
            from_ms: needed.accepted_from_ms,
            until_ms: Some(needed.accepted_until_ms),
        };
        state::local_put(&key::ask(&needed.sender, needed.accepted_from_ms), &ask)
            .map(|w| vec![w])
            .unwrap_or_default()
    }

    /// Entries missed while lagging: ask the counterparties of unfinished activity to send
    /// again from the item's time (Paykit spec 9).
    fn missed(&self, missed: &MissedEntries) -> Vec<LocalWrite> {
        state::local_put(
            &key::missed(missed.accepted_from_ms),
            &missed.accepted_from_ms,
        )
        .map(|w| vec![w])
        .unwrap_or_default()
    }

    /// A provider loss, without the protocol's state at hand: only the re-sends are queued.
    fn provider_lost(&self, loss: &ProviderLoss) -> LossActions {
        LossActions {
            conversations: Vec::new(),
            local: state::local_put(&key::missed(loss.accepted_from_ms), &loss.accepted_from_ms)
                .map(|w| vec![w])
                .unwrap_or_default(),
        }
    }

    /// A provider loss (Switch spec 12.6 steps 2 and 4): ask every counterparty with Paykit
    /// activity to send again from the old cursor's time, and add their conversation entries
    /// back.
    fn provider_lost_with(&self, loss: &ProviderLoss, view: &LocalView<'_>) -> LossActions {
        let conversations = view
            .local_scan(key::PEERS.as_bytes())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, _)| {
                let rest = k.get(key::PEERS.len()..)?;
                unb64_array::<32>(std::str::from_utf8(rest).ok()?).ok()
            })
            .collect();
        let mut actions = self.provider_lost(loss);
        actions.conversations = conversations;
        actions
    }

    fn adds_conversation(&self, kind: &str) -> bool {
        // Paykit wire 4.12: a resend_request adds no conversation entry.
        kind != Kind::ResendRequest.as_str()
    }

    /// Paykit's collections with their fixed policies (Paykit spec 6, Paykit wire 5.1):
    /// `payee_activity` with one policy per label, so an app holding `request` lists and
    /// takes every label's records, and one holding `request:<label>` its label's.
    fn collections(&self) -> Vec<SwitchCollection> {
        vec![
            activity::family(Collection::PayerActivity),
            activity::family(Collection::PayeeActivity),
            activity::family(Collection::PaymentLists),
        ]
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
    /// used. A value that doesn't read is refused too. A delete isn't such a write.
    fn record_regression(
        &self,
        collection: &str,
        seen: &[u8],
        value: Option<&[u8]>,
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
                held.check_later_write(&later).err().map(|e| e.to_string())
            }
            Collection::PaymentLists => {
                let held = PaymentListRecord::read(seen, &own.identity).ok()?;
                let Ok(later) = PaymentListRecord::read(value, &own.identity) else {
                    return Some(
                        "Paykit wire 5.4: a payment_lists record that doesn't read".into(),
                    );
                };
                list_regression(&held, &later)
            }
            _ => None,
        }
    }
}

/// A later `payment_lists` write that goes back on one seen (Paykit wire 5.4, Switch spec
/// 8.2): an older list, or a single-use endpoint seen used and dropped, which would let it
/// be paid twice.
pub(crate) fn list_regression(
    held: &PaymentListRecord,
    later: &PaymentListRecord,
) -> Option<String> {
    let held_version = list_version(held);
    let later_version = list_version(later);
    if later_version < held_version {
        return Some("Paykit wire 5.4: a later write holds an older list".into());
    }
    let dropped = held.used.iter().any(|used| !later.used.contains(used));
    if dropped && later_version == held_version {
        return Some("Paykit wire 5.4: a later write drops a single-use endpoint seen used".into());
    }
    None
}

fn list_version(record: &PaymentListRecord) -> u64 {
    match record.list.body() {
        paykit_lib::Body::PrivatePaymentList(list) => list.version,
        _ => 0,
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
    use paykit_lib::records::ActivityRecord;
    use pubky_switch_core::records::Standing;
    use serde_json::json;

    use super::*;
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
            conflict: None,
        }
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
        let alice = Party::new("alice", "wallet", &["pay"]);
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
        let alice = Party::new("alice", "wallet", &["pay", "allowance"]);
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
        let alice = Party::new("alice", "wallet", &["pay"]);
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
        let p = protocol_of(&Party::new("alice", "wallet", &["pay"]));
        let wallet = Party::new("alice", "wallet", &["pay", "allowance"]);
        let kinds = p.accepts(&wallet.cert);
        assert!(kinds.contains(&"paykit.payment_request".to_owned()));
        assert!(!kinds.contains(&"paykit.payment_proof".to_owned()));
        assert!(!kinds.contains(&"paykit.allowance_proposal".to_owned()));
        // Another app's scopes receive by Paykit's table, handled here or not.
        assert!(p.receives("paykit.allowance_proposal", &wallet.cert));
        assert!(!p.receives("paykit.payment_proof", &wallet.cert));
    }

    #[test]
    fn test_protocol_record_regression_on_a_dropped_role() {
        let alice = Party::new("alice", "wallet", &["pay"]);
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
