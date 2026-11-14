//! Activity records as the SDK writes them (Paykit spec 6, Paykit wire 5.3): the record a
//! message goes into, its role there, and the record's next value. The value's format and
//! its binding rules are `paykit-lib`'s ([`ActivityRecord`]); this module only places
//! messages and names the record.
//!
//! **One message per role** (Paykit spec 6): a record stores one message per role. A later
//! message for a role already held is still processed, but not stored: its `msg_id` joins
//! the bounded list of others the record reflects (Paykit wire 5.3).

use paykit_lib::bodies::period_details_order;
use paykit_lib::collections::Collection;
use paykit_lib::records::{
    bounded_msg_ids, ActivityRecord, ActivitySide, StoredMessage, MAX_PERIOD_DETAILS,
};
use paykit_lib::{Body, Label};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
use serde::{Deserialize, Serialize};

/// A message's role in an activity record (Paykit wire 5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Role {
    /// The `payment_request`.
    Request,
    /// The acceptance or rejection.
    Response,
    /// A cancellation from either side.
    Cancellation,
    /// The proof.
    Proof,
    /// The receipt.
    Receipt,
    /// Period details, in a period record, which keeps every details of its period (Paykit
    /// wire 4.9, 5.3).
    PeriodDetails,
}

impl Role {
    /// The role of a body's kind; `None` for kinds activity records don't hold.
    pub fn of(body: &Body) -> Option<Self> {
        Some(match body {
            Body::PaymentRequest(_) => Self::Request,
            Body::PaymentRequestAcceptance(_) | Body::PaymentRequestRejection(_) => Self::Response,
            Body::PaymentRequestCancellation(_) => Self::Cancellation,
            Body::PaymentProof(_) => Self::Proof,
            Body::PaymentReceipt(_) => Self::Receipt,
            Body::PaymentPeriodDetails(_) => Self::PeriodDetails,
            _ => return None,
        })
    }
}

/// Which activity record a message goes into: the side, the natural key and, for a payee's
/// record of a labelled request, its label (Paykit spec 6, Paykit wire 5.1).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ActivityTarget {
    pub(crate) side: SideTag,
    pub(crate) natural_key: Vec<u8>,
    pub(crate) label: Option<Label>,
}

impl std::fmt::Debug for ActivityTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ActivityTarget({:?})", self.side)
    }
}

/// The side, as kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SideTag {
    Payer,
    Payee,
}

/// The side a cancellation was sent from, kept under `side/<msg_id>` with that `msg_id`, so a
/// restore binds the value to its key and a reader takes it only for the message it names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptSide {
    pub(crate) msg_id: String,
    pub(crate) side: SideTag,
}

impl SideTag {
    pub(crate) fn side(self) -> ActivitySide {
        match self {
            Self::Payer => ActivitySide::Payer,
            Self::Payee => ActivitySide::Payee,
        }
    }
}

impl ActivityTarget {
    /// A payer's record (`payer_activity`, fixed policy whatever the label).
    pub(crate) fn payer(natural_key: Vec<u8>) -> Self {
        Self {
            side: SideTag::Payer,
            natural_key,
            label: None,
        }
    }

    /// A payee's record, under the policy of `label`.
    pub(crate) fn payee(natural_key: Vec<u8>, label: Option<&Label>) -> Self {
        Self {
            side: SideTag::Payee,
            natural_key,
            label: label.cloned(),
        }
    }

    pub(crate) fn label(&self) -> Option<Label> {
        self.label.clone()
    }

    pub(crate) fn collection(&self) -> Collection {
        self.side.side().collection()
    }

    /// The client's record target: the collection with its policy, and the natural key.
    pub(crate) fn record_target(&self) -> RecordTarget {
        let collection = self.collection();
        let policy = collection.policy(self.label().as_ref());
        RecordTarget::new(
            SwitchCollection::new(collection.name(), policy),
            &self.natural_key,
        )
    }
}

/// The client's collection of one of Paykit's collections, under the policy of `label`.
pub(crate) fn switch_collection(collection: Collection, label: Option<&Label>) -> SwitchCollection {
    SwitchCollection::new(collection.name(), collection.policy(label))
}

/// The client's collection of one of Paykit's collections with every policy it allows (Paykit spec
/// 6, Paykit wire 5.1): `payee_activity` is a sub-scope family, the fixed `paykit_request` policy
/// and one per label, so an app holding `paykit_request` lists and takes every label's records, and
/// one holding only `paykit_request:<label>` its label's; the others have their fixed policy alone.
pub(crate) fn family(collection: Collection) -> SwitchCollection {
    match collection {
        Collection::PayeeActivity => {
            SwitchCollection::with_sub_scopes(collection.name(), collection.fixed_policy())
        }
        _ => SwitchCollection::new(collection.name(), collection.fixed_policy()),
    }
}

/// An empty activity record of `side` with `counterparty`.
pub(crate) fn empty(side: ActivitySide, counterparty: [u8; 32]) -> ActivityRecord {
    ActivityRecord::new(side, counterparty)
}

fn stored_ids(record: &ActivityRecord) -> Vec<String> {
    record
        .stored()
        .iter()
        .map(|(_, s)| s.message().msg_id.clone())
        .collect()
}

/// Whether `message`, a payer's reply to a request (the response role), would take the
/// empty response of `record` while a different reply from its sender to the same request,
/// which its inbox accepted before it, at `order` ([`crate::records::KeptOrder`]), is still
/// kept to write: one of `kept` the record doesn't reflect yet. The earlier of two
/// different replies is the record's response (Paykit spec 6, 8.1), so the later one is
/// then only reflected ([`reflect`]).
pub(crate) fn replied_before(
    record: &ActivityRecord,
    message: &StoredMessage,
    order: crate::records::KeptOrder,
    kept: &[(StoredMessage, crate::records::KeptOrder)],
) -> bool {
    let reply_to = |m: &StoredMessage| {
        Role::of(m.body()) == Some(Role::Response)
            && m.sender() == message.sender()
            && m.message().refs.first() == message.message().refs.first()
    };
    reply_to(message)
        && record.response.is_none()
        && !record.reflects(&message.message().msg_id)
        && kept.iter().any(|(other, at)| {
            *at < order
                && other.hash() != message.hash()
                && reply_to(other)
                && !record.reflects(&other.message().msg_id)
        })
}

/// Reflect `message` in `record` by its `msg_id` only, leaving every role as it is: a
/// reply that an earlier, different one is still kept to take the response of
/// ([`replied_before`]). Returns `false` when the record already reflects it.
pub(crate) fn reflect(record: &mut ActivityRecord, message: &StoredMessage) -> bool {
    let msg_id = message.message().msg_id.clone();
    if record.reflects(&msg_id) {
        return false;
    }
    let stored = stored_ids(record);
    let mut others: Vec<String> = record
        .msg_ids
        .iter()
        .filter(|id| !stored.contains(id))
        .cloned()
        .collect();
    others.push(msg_id);
    record.msg_ids = bounded_msg_ids(&stored, &others);
    true
}

/// Place `message` in its role of `record` (Paykit spec 6 "One message per role"): stored
/// when the role is empty, otherwise reflected by its `msg_id` only. Returns `false` when
/// the record already reflects the message, so nothing changes.
///
/// So a re-sent message, or one placed again by a later write of the app's own, is never
/// applied twice (Paykit spec 8.1, "Stored entries"): one held in its role is reflected for
/// as long as the record holds it, whatever came since, so its record isn't written again.
/// One only reflected may fall past the 16 newest other `msg_id`s; placing it again
/// reflects it again and changes no role or status, since the role was already held.
pub(crate) fn place(record: &mut ActivityRecord, role: Role, message: StoredMessage) -> bool {
    let msg_id = message.message().msg_id.clone();
    if record.reflects(&msg_id) {
        return false;
    }
    let mut others: Vec<String> = {
        let stored = stored_ids(record);
        record
            .msg_ids
            .iter()
            .filter(|id| !stored.contains(id))
            .cloned()
            .collect()
    };
    let slot = match role {
        Role::Request => &mut record.request,
        Role::Response => &mut record.response,
        Role::Cancellation => &mut record.cancellation,
        Role::Proof => &mut record.proof,
        Role::Receipt => &mut record.receipt,
        Role::PeriodDetails => {
            if !place_details(record, message) {
                others.push(msg_id);
            }
            record.msg_ids = bounded_msg_ids(&stored_ids(record), &others);
            record.status = Some(status_text(record).to_owned());
            return true;
        }
    };
    if slot.is_none() {
        *slot = Some(message);
        // The details a period's proof names are the period's current ones from then on,
        // whichever the record held as latest: the payer may pay by any valid details
        // (Paykit spec 8.2, Paykit wire 5.3).
        if role == Role::Proof {
            order_details(record);
        }
    } else {
        others.push(msg_id);
    }
    record.msg_ids = bounded_msg_ids(&stored_ids(record), &others);
    record.status = Some(status_text(record).to_owned());
    true
}

/// Store period details `message` in the period record `record` (Paykit wire 4.9, 5.3):
/// each details stays, none replacing another, at most [`MAX_PERIOD_DETAILS`] for the
/// period. `period_details` holds the latest, by `created_at` then `msg_id`, and
/// `earlier_details` the others in that order; once the record holds a proof that names
/// details, those stay in `period_details`. Returns `false` when it stores nothing, the
/// record holding 8 already; the caller reflects the `msg_id` then. Details as late as held
/// ones, the same `created_at` and `msg_id`, never reach here: [`place`] takes a message
/// whose `msg_id` the record reflects as placed already, so other details as late, an
/// equivocation, leave the held ones as they are (Paykit wire 4.9).
fn place_details(record: &mut ActivityRecord, message: StoredMessage) -> bool {
    if record.details().len() >= MAX_PERIOD_DETAILS {
        return false;
    }
    // A period record names its request and period, from the details' own signed fields.
    if record.request_hash.is_none() {
        record.request_hash = message.message().refs.first().copied();
    }
    if record.billing_period.is_none() {
        record.billing_period = message.body().billing_period().copied();
    }
    record.earlier_details.push(message);
    order_details(record);
    true
}

/// Order the period details `record` holds (Paykit wire 5.3): the ones its proof names in
/// `period_details`, or else the latest, by `created_at` then `msg_id`, and the others in
/// `earlier_details` in that order.
fn order_details(record: &mut ActivityRecord) {
    let named = record
        .proof
        .as_ref()
        .and_then(|proof| proof.message().refs.get(1).copied());
    let mut all: Vec<StoredMessage> = record
        .period_details
        .take()
        .into_iter()
        .chain(std::mem::take(&mut record.earlier_details))
        .collect();
    if all.is_empty() {
        return;
    }
    all.sort_by(|a, b| period_details_order(a.message(), b.message()));
    let current = named
        .and_then(|hash| all.iter().position(|d| d.hash() == hash))
        .unwrap_or(all.len() - 1);
    record.period_details = Some(all.remove(current));
    record.earlier_details = all;
}

/// Whether the proof `stored` names period details `record` doesn't hold: placed now, the
/// record wouldn't bind (Paykit wire 5.3), so it waits for them as a reply before its record.
pub(crate) fn names_details_not_held(record: &ActivityRecord, stored: &StoredMessage) -> bool {
    matches!(stored.body(), Body::PaymentProof(_))
        && stored
            .message()
            .refs
            .get(1)
            .is_some_and(|named| record.details().iter().all(|d| d.hash() != *named))
}

/// The informational `status` a writer sets (Paykit wire 5.3): written by the last writer,
/// never trusted.
pub(crate) fn status_text(record: &ActivityRecord) -> &'static str {
    let rejected = matches!(
        record.response.as_ref().map(StoredMessage::body),
        Some(Body::PaymentRequestRejection(_))
    );
    if record.receipt.is_some() {
        "receipted"
    } else if record.proof.is_some() {
        "paid"
    } else if record.cancellation.is_some() {
        "cancelled"
    } else if rejected {
        "rejected"
    } else if record.response.is_some() {
        "accepted"
    } else {
        "requested"
    }
}

/// Read an activity record value as `own` holds it, or `None` when it doesn't read: the
/// reader doesn't act on an invalid record (Paykit wire 5.3).
pub(crate) fn read(
    value: Option<&[u8]>,
    own: &[u8; 32],
    side: ActivitySide,
) -> Option<ActivityRecord> {
    ActivityRecord::read(value?, own, side).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use paykit_lib::derive::natural_key;

    #[test]
    fn test_activity_target_payee_label_takes_its_policy() {
        let label = Label::parse("shop1").unwrap();
        let labelled = ActivityTarget::payee(vec![1], Some(&label)).record_target();
        assert_eq!(
            labelled.collection.policy.read_scope,
            "paykit_request:shop1"
        );
        let fixed = ActivityTarget::payee(vec![1], None).record_target();
        assert_eq!(fixed.collection.policy.read_scope, "paykit_request");
        let payer = ActivityTarget::payer(natural_key::request_record(&[1; 32], &[2; 32]));
        assert_eq!(payer.record_target().collection.name, "payer_activity");
    }

    #[test]
    fn test_activity_payee_family_admits_each_label_policy() {
        let family = family(Collection::PayeeActivity);
        let label = Label::parse("shop1").unwrap();
        assert!(family.admits(&Collection::PayeeActivity.policy(Some(&label))));
        assert!(family.admits(&Collection::PayeeActivity.fixed_policy()));
        assert_eq!(
            family.sub_scope_policy("shop1").unwrap(),
            Collection::PayeeActivity.policy(Some(&label))
        );
        // The payer's records have their fixed policy alone.
        let payer = super::family(Collection::PayerActivity);
        assert!(!payer.admits(&Collection::PayeeActivity.policy(Some(&label))));
    }

    #[test]
    fn test_activity_status_follows_the_roles() {
        let record = empty(ActivitySide::Payer, [3; 32]);
        assert_eq!(status_text(&record), "requested");
    }
}

#[cfg(test)]
mod place_tests {
    use serde_json::json;

    use super::*;
    use crate::testkit::{object, Party};

    fn request(bob: &Party, alice: &Party, reference: &str) -> StoredMessage {
        bob.message(
            alice,
            "paykit.payment_request",
            vec![],
            object(json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": reference,
                "proposal_expires_at": null
            })),
        )
    }

    #[test]
    fn test_activity_place_stores_one_message_per_role() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new("bob", "shop", &["paykit_request"]);
        let mut record = empty(ActivitySide::Payer, bob.id());
        let first = request(&bob, &alice, "order-1");
        assert!(place(&mut record, Role::Request, first.clone()));
        assert_eq!(record.status.as_deref(), Some("requested"));
        // The same message again changes nothing.
        assert!(!place(&mut record, Role::Request, first.clone()));
        // Another for the role held is reflected, not stored.
        let second = request(&bob, &alice, "order-2");
        assert!(place(&mut record, Role::Request, second.clone()));
        assert_eq!(
            record.request.as_ref().map(|s| s.hash()),
            Some(first.hash())
        );
        assert!(record.reflects(&second.message().msg_id));
        let json = record.to_json().unwrap();
        assert!(ActivityRecord::read(&json, &alice.id(), ActivitySide::Payer).is_ok());
    }

    #[test]
    fn test_activity_place_keeps_the_newest_sixteen_others() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new("bob", "shop", &["paykit_request"]);
        let mut record = empty(ActivitySide::Payer, bob.id());
        assert!(place(
            &mut record,
            Role::Request,
            request(&bob, &alice, "r0")
        ));
        let mut last = String::new();
        for i in 1..=20 {
            let other = request(&bob, &alice, &format!("r{i}"));
            last = other.message().msg_id.clone();
            assert!(place(&mut record, Role::Request, other));
        }
        assert_eq!(record.msg_ids.len(), 17);
        assert!(record.msg_ids.contains(&last));
    }

    #[test]
    fn test_activity_place_again_changes_no_role_however_long_ago() {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new("bob", "shop", &["paykit_request"]);
        let mut record = empty(ActivitySide::Payer, bob.id());
        let first = request(&bob, &alice, "r0");
        let second = request(&bob, &alice, "r1");
        assert!(place(&mut record, Role::Request, first.clone()));
        assert!(place(&mut record, Role::Request, second.clone()));
        for i in 2..=20 {
            assert!(place(
                &mut record,
                Role::Request,
                request(&bob, &alice, &format!("r{i}"))
            ));
        }
        // Held in its role, a message is reflected however many others came since.
        assert!(!record.msg_ids[1..].contains(&first.message().msg_id));
        assert!(!place(&mut record, Role::Request, first.clone()));
        // One only reflected and past the newest 16 is reflected again, and the record's
        // roles and status stay as they were.
        assert!(!record.reflects(&second.message().msg_id));
        let before = record.clone();
        assert!(place(&mut record, Role::Request, second.clone()));
        assert!(record.reflects(&second.message().msg_id));
        assert_eq!(record.stored().len(), before.stored().len());
        assert_eq!(
            record.request.as_ref().map(|s| s.hash()),
            Some(first.hash())
        );
        assert_eq!(record.status, before.status);
        assert!(!place(&mut record, Role::Request, second));
    }
}
