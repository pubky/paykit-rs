//! [`PaykitProtocol`]: Paykit as the client's [`Protocol`], the rules the app layer can't
//! know (Switch spec 1).
//!
//! - **Members and scopes:** the opaque members of Paykit's bodies, which scopes have keys and
//!   check 8 with labels, all from `paykit-lib` (Paykit spec 3, Paykit wire 1.1, 3.1, 4.1).
//! - **Kinds an app takes:** its record's `accepts` lists the kinds its scopes receive that
//!   this SDK handles; a sender wraps a kind to any app whose scopes receive it, by Paykit's
//!   table (Paykit spec 3.3), and narrows a payer's replies by label (Paykit spec 3.4).
//! - **Receiving:** check 10, then the message's effect. The client's protocol callback
//!   can't build a shared record write (it has no access to the app's keys or the record it
//!   holds), so the effect is queued in the protocol's local state, committed with the
//!   processed pair and the cursor, and written by [`crate::Paykit::receive`] right after,
//!   in the batch that carries the deferred cursor move (Switch spec 8.2).
//! - **Collections** with their fixed policies, and the marks that stop a record write from
//!   dropping a message in a role an app has seen (Paykit spec 6, Paykit wire 5.3, 5.4).
//! - **Sent messages' records** ([`Protocol::sent_record`]): a reply's stored copy goes into
//!   the request's record (Paykit wire 5.3).
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
use paykit_lib::scopes::{acts_as_payee, PaykitScope, ScopeName};
use paykit_lib::{Label, PaykitKindRules};
use pubky_switch_client::app::{
    Collection as SwitchCollection, Effect, LocalWrite, LossActions, MissedEntries, OpaqueRule,
    Outcome, Protocol, ProviderLoss, ReceivedMessage, RecordJob, ResendNeeded, SentMessage,
};
use pubky_switch_client::store::StateStore;
use pubky_switch_core::enc::{serde_b64, unb64_array};
use pubky_switch_core::message::Message;
use pubky_switch_core::receive::KindRules;
use pubky_switch_core::records::{AppCert, AppRecord};
use pubky_switch_core::scopes as switch_scopes;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};

use crate::activity::{self, JobTag, SideTag};
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

/// A received message whose effect is still to be written (`p/fx/`). Its `Debug` shows no
/// content.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct QueuedMessage {
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    pub(crate) cert: SignedDoc,
    pub(crate) plaintext: String,
    pub(crate) accepted_at_ms: u64,
    pub(crate) seq: u64,
}

impl std::fmt::Debug for QueuedMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "QueuedMessage(seq {})", self.seq)
    }
}

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
/// One instance per app: it reads the protocol's own local state in the app's namespace,
/// and learns the app's identity and certificate once the app has joined or loaded
/// ([`PaykitProtocol::set_own`]); [`crate::Paykit`] does both.
pub struct PaykitProtocol {
    store: Arc<dyn StateStore>,
    ns: String,
    own: RwLock<Option<Own>>,
    known_labels: Vec<Label>,
}

impl std::fmt::Debug for PaykitProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PaykitProtocol({})", self.ns)
    }
}

impl PaykitProtocol {
    /// The protocol of the app whose state is in `ns` of `store`. `known_labels` are labels
    /// of the identity's labelled selling scopes whose `payee_activity` records an app
    /// holding plain `request` follows, besides the labels its own certificate holds.
    pub fn new(store: Arc<dyn StateStore>, ns: String, known_labels: Vec<Label>) -> Self {
        Self {
            store,
            ns,
            own: RwLock::new(None),
            known_labels,
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

    /// The labels whose `payee_activity` records this app reads and writes: those its own
    /// certificate holds, and, for an app holding `request`, the known labels (Paykit spec
    /// 3.1, 6).
    pub fn labels(&self) -> Vec<Label> {
        let Some(own) = self.own() else {
            return Vec::new();
        };
        let scopes = own.cert.scopes();
        let mut labels: Vec<Label> = scopes
            .iter()
            .filter_map(|s| match PaykitScope::classify(s) {
                ScopeName::Paykit(PaykitScope::RequestLabel(label)) => Some(label),
                _ => None,
            })
            .collect();
        if scopes.iter().any(|s| s == "request") {
            for label in &self.known_labels {
                if !labels.contains(label) {
                    labels.push(label.clone());
                }
            }
        }
        labels
    }
}

/// Whether a certificate's scopes hold one of `roles` (Paykit spec 3.3).
pub(crate) fn holds_role(scopes: &[String], roles: &[KindRole]) -> bool {
    roles.iter().any(|role| match role {
        KindRole::Pay => scopes.iter().any(|s| s == "pay"),
        KindRole::Request => acts_as_payee(scopes),
        KindRole::Allowance => scopes.iter().any(|s| s == "allowance"),
        KindRole::AllowanceExecute => scopes.iter().any(|s| s == "allowance_execute"),
        KindRole::Any => true,
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
        let Some(kind) = Kind::from_kind(&message.kind) else {
            return true;
        };
        if !kind.is_payer_reply() {
            return true;
        }
        let label = body_label(message);
        let side = if kind == Kind::PaymentRequestCancellation {
            match state::get_sync::<SideTag>(
                self.store.as_ref(),
                &self.ns,
                &key::side(&message.msg_id),
            ) {
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
        } else {
            Side::Payer
        };
        targets_app(kind, side, label.as_ref(), recipient.cert().scopes())
    }

    /// Check 10 (Paykit wire 4.1) and the message's effect, queued for
    /// [`crate::Paykit::receive`] with the cursor move.
    fn receive(&self, message: &ReceivedMessage) -> Outcome {
        let handled = Kind::from_kind(&message.message.kind)
            .is_some_and(|kind| HANDLED_KINDS.contains(&kind));
        if !handled {
            return Outcome::Drop {
                reason: "Paykit wire 4.1 check 10: a kind this app doesn't handle".into(),
            };
        }
        if let Err(error) = check_body(&message.message, message.plaintext.json()) {
            return Outcome::Drop {
                reason: error.to_string(),
            };
        }
        let queued = QueuedMessage {
            sender: message.sender,
            cert: message.cert.signed().clone(),
            plaintext: pubky_switch_core::enc::b64(&message.plaintext.encode()),
            accepted_at_ms: message.accepted_at_ms,
            seq: message.seq,
        };
        match state::local_put(&key::effect(message.seq, &message.message.msg_id), &queued) {
            Ok(write) => Outcome::Apply(Effect {
                ops: Vec::new(),
                local: vec![write],
            }),
            Err(_) => Outcome::Drop {
                reason: "the message's effect can't be kept".into(),
            },
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

    /// A provider loss (Switch spec 12.6 steps 2 and 4): ask every counterparty with Paykit
    /// activity to send again from the old cursor's time, and add their conversation entries
    /// back.
    fn provider_lost(&self, loss: &ProviderLoss) -> LossActions {
        let conversations = state::scan_sync::<u64>(self.store.as_ref(), &self.ns, key::PEERS)
            .into_iter()
            .filter_map(|(rest, _)| unb64_array::<32>(&rest).ok())
            .collect();
        LossActions {
            conversations,
            local: state::local_put(&key::missed(loss.accepted_from_ms), &loss.accepted_from_ms)
                .map(|w| vec![w])
                .unwrap_or_default(),
        }
    }

    fn adds_conversation(&self, kind: &str) -> bool {
        // Paykit wire 4.12: a resend_request adds no conversation entry.
        kind != Kind::ResendRequest.as_str()
    }

    /// Paykit's collections with their fixed policies (Paykit spec 6, Paykit wire 5.1), and
    /// the labelled policies of `payee_activity` for the labels this app follows.
    fn collections(&self) -> Vec<SwitchCollection> {
        let mut out = vec![
            activity::switch_collection(Collection::PayerActivity, None),
            activity::switch_collection(Collection::PayeeActivity, None),
            activity::switch_collection(Collection::PaymentLists, None),
        ];
        for label in self.labels() {
            out.push(activity::switch_collection(
                Collection::PayeeActivity,
                Some(&label),
            ));
        }
        out
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

    /// A sent message's stored copy, in its role of the record its job names (Paykit wire
    /// 5.3); `None` when the record already reflects it.
    fn sent_record(
        &self,
        job: &RecordJob,
        sent: &SentMessage,
        current: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        let own = self.own()?;
        let tag: JobTag = serde_json::from_slice(&job.tag).ok()?;
        let stored = StoredMessage::new(
            own.identity,
            own.cert.signed(),
            sent.plaintext.clone(),
            sent.accepted_at_ms,
            &sent.to,
        )
        .ok()?;
        let side = tag.side.side();
        let mut record = activity::read(current, &own.identity, side)
            .unwrap_or_else(|| activity::empty(side, sent.to));
        if !activity::place(&mut record, tag.role, stored) {
            return None;
        }
        record.to_json().ok()
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
    use pubky_switch_client::store::MemoryStore;

    use super::*;

    fn protocol() -> PaykitProtocol {
        PaykitProtocol::new(Arc::new(MemoryStore::new()), "client/x".into(), Vec::new())
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
        assert!(p.labels().is_empty());
    }
}

#[cfg(test)]
mod receive_tests {
    use paykit_lib::records::ActivityRecord;
    use pubky_switch_client::store::MemoryStore;
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
        let p = PaykitProtocol::new(Arc::new(MemoryStore::new()), "client/x".into(), Vec::new());
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
    fn test_protocol_receive_queues_a_valid_message() {
        let alice = Party::new("alice", "wallet", &["pay"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let stored = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let Outcome::Apply(effect) = protocol_of(&alice).receive(&received(&stored)) else {
            panic!("applied");
        };
        assert!(effect.ops.is_empty());
        assert_eq!(effect.local.len(), 1);
    }

    #[test]
    fn test_protocol_receive_drops_an_unhandled_kind_and_a_bad_body() {
        let alice = Party::new("alice", "wallet", &["pay", "allowance"]);
        let bob = Party::new("bob", "shop", &["request", "allowance"]);
        let mut stored = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let mut message = received(&stored);
        message.message.kind = "paykit.allowance_end".into();
        assert!(matches!(
            protocol_of(&alice).receive(&message),
            Outcome::Drop { .. }
        ));
        stored = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let mut message = received(&stored);
        message.message.body.remove("amount");
        assert!(matches!(
            protocol_of(&alice).receive(&message),
            Outcome::Drop { .. }
        ));
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

    #[test]
    fn test_protocol_sent_record_places_the_reply() {
        let alice = Party::new("alice", "wallet", &["pay"]);
        let bob = Party::new("bob", "shop", &["request"]);
        let p = protocol_of(&alice);
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let mut record = activity::empty(ActivitySide::Payer, bob.id());
        activity::place(&mut record, activity::Role::Request, request.clone());
        let current = record.to_json().unwrap();
        let reply = alice.message(
            &bob,
            "paykit.payment_request_rejection",
            vec![request.hash()],
            object(json!({})),
        );
        let target = activity::ActivityTarget::payer(vec![1]);
        let job = RecordJob {
            target: target.record_target(),
            tag: serde_json::to_vec(&JobTag {
                side: SideTag::Payer,
                role: activity::Role::Response,
            })
            .unwrap(),
        };
        let sent = SentMessage {
            to: bob.id(),
            kind: "paykit.payment_request_rejection".into(),
            msg_id: reply.message().msg_id.clone(),
            plaintext: reply.plaintext().clone(),
            accepted_at_ms: Some(NOW),
        };
        let next = p.sent_record(&job, &sent, Some(&current)).unwrap();
        let read = ActivityRecord::read(&next, &alice.id(), ActivitySide::Payer).unwrap();
        assert!(read.response.is_some());
        assert_eq!(read.status.as_deref(), Some("rejected"));
        // Written once: the record reflects it now.
        assert_eq!(p.sent_record(&job, &sent, Some(&next)), None);
    }
}
