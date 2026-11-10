//! `sealed.json`: record values a reader binds to their keys, the rejections, the labelled
//! payee record and the activity records a reader takes or refuses (Paykit wire 5, 5.1, 5.3,
//! 5.4, 5.5).

use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, request_hash};
use paykit_lib::records::{
    bind_ledger, ActivityRecord, ActivitySide, AllowanceRecord, PaymentListRecord,
};
use paykit_lib::scopes::{Label, PaykitScope, ScopeName};
use pubky_switch_core::enc::b64;
use pubky_switch_core::ident::key_of_scope;
use pubky_switch_core::sealed::{aad, pad, RecordValue, SealedValue};
use serde_json::Value;

use crate::common::{
    arr, b64n, generated, hex, message_json, paykit, pubky, quoted, st, switch, unhex32, Keys,
    Place, ALICE, ALLOWANCE_ID, BOB,
};

#[test]
fn test_vectors_sealed() {
    let keys = Keys::load();
    generate(&keys);
    let v = paykit("sealed.json");
    check_bindings(&keys, &v);
    check_labelled(&keys, &v["labelled"]);
    check_activity_rules(&keys);
    check_activity_records(&keys, &v);
    check_msg_ids_bounds(&keys, &v);
}

/// Own cases on activity records (Paykit wire 5.3), built from the vectors' messages: the
/// seen roles, the seen Allowance and attempted periods.
fn check_activity_rules(keys: &Keys) {
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::StoredMessage;
    use pubky_switch_core::message::MessagePlaintext;
    use pubky_switch_core::ops::Condition;

    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let nk = keys.identity(&alice).naming_key();
    let stored = |name: &str| {
        let m = crate::common::message(name);
        let sender = keys.app_by_id(st(&m, "sender_app"));
        let plaintext = MessagePlaintext::from_parts(
            sender.keys.app_id_bytes(),
            b64n::<64>(st(&m, "signature")),
            st(&m, "json").as_bytes().to_vec(),
        )
        .unwrap();
        StoredMessage::new(
            sender.identity,
            &crate::common::cert_doc(&sender.name),
            plaintext,
            None,
            &pubky(st(&m, "recipient_identity")),
        )
        .unwrap()
    };
    // Written without the writer's rules, so the reader's refusals show; the writer refuses
    // every record the reader does.
    let read = |record: &ActivityRecord| {
        let read = ActivityRecord::read(
            &record.to_json_unchecked().unwrap(),
            &alice,
            ActivitySide::Payer,
        );
        assert_eq!(
            read.is_ok(),
            record.to_json().is_ok(),
            "writer and reader agree"
        );
        read
    };
    let base = ActivityRecord {
        side: ActivitySide::Payer,
        counterparty: bob,
        request: Some(stored("payment_request_recurring")),
        response: None,
        cancellation: None,
        proof: None,
        receipt: None,
        period_details: None,
        request_hash: None,
        billing_period: None,
        attempt_id: None,
        unrequested: None,
        msg_ids: Vec::new(),
        status: None,
        allowance_id: None,
        attempted_periods: vec![1_769_817_600, 1_772_236_800],
    };
    let held = read(&base).unwrap();
    assert_eq!(held.attempted_periods, base.attempted_periods);
    // Dropping a listed period needs a claim_step condition on its claim.
    let later = ActivityRecord {
        attempted_periods: vec![1_772_236_800],
        ..held.clone()
    };
    let claim = paykit_lib::derive::request_obligation(
        &nk,
        &bob,
        &request_hash(&message_json("payment_request_recurring")),
        1_769_817_600,
    );
    let with = [Condition::ClaimStep {
        claim_key: claim,
        step: 3,
    }];
    assert_eq!(
        held.check_dropped_periods(&later, &with, &alice, &nk)
            .unwrap(),
        [1_769_817_600]
    );
    assert!(held
        .check_dropped_periods(&later, &[], &alice, &nk)
        .is_err());
    // Out of order, in a one-time request's record, or in a payee's record: invalid.
    let unsorted = ActivityRecord {
        attempted_periods: vec![1_772_236_800, 1_769_817_600],
        ..held.clone()
    };
    assert!(read(&unsorted).is_err());
    let one_time = ActivityRecord {
        request: Some(stored("payment_request")),
        ..held.clone()
    };
    assert!(read(&one_time).is_err());
    let payee = ActivityRecord {
        side: ActivitySide::Payee,
        ..held.clone()
    };
    assert!(payee.to_json().is_err());
    assert!(ActivityRecord::read(
        &payee.to_json_unchecked().unwrap(),
        &bob,
        ActivitySide::Payee
    )
    .is_err());
    // Seen roles: a later write that drops the request is ignored.
    let dropped = ActivityRecord {
        request: None,
        attempted_periods: Vec::new(),
        ..held.clone()
    };
    assert!(held.check_later_write(&dropped).is_err());
    held.check_later_write(&held).unwrap();
    // Seen Allowance: a later write that changes or drops it is ignored.
    let named = ActivityRecord {
        allowance_id: Some(AllowanceId::parse(crate::common::ALLOWANCE_ID).unwrap()),
        ..held.clone()
    };
    let named = read(&named).unwrap();
    held.check_later_write(&named).unwrap();
    assert!(named.check_later_write(&held).is_err());
    // A message of another request makes the record invalid: binding.
    let other = ActivityRecord {
        response: Some(stored("payment_request_acceptance")),
        attempted_periods: Vec::new(),
        ..held
    };
    let error = read(&other).unwrap_err().to_string();
    assert!(
        error.contains("doesn't name the record's request"),
        "{error}"
    );
}

fn key(case: &Value) -> [u8; 32] {
    b64n::<32>(st(case, "record_key"))
}

fn value(case: &Value) -> &[u8] {
    st(case, "value").as_bytes()
}

fn check_bindings(keys: &Keys, v: &Value) {
    let alice = pubky(ALICE);
    let bob = pubky(BOB);
    let nk = keys.identity(&alice).naming_key();
    let accepted = arr(v, "binding_accepted");
    let collections: Vec<&str> = accepted.iter().map(|c| st(c, "collection")).collect();
    assert_eq!(
        collections,
        ["payment_lists", "allowances", "allowance_ledgers"]
    );

    let list =
        PaymentListRecord::bind(value(&accepted[0]), &alice, &nk, &key(&accepted[0])).unwrap();
    assert_eq!(
        (list.payee, list.app_id),
        (bob, keys.app("shop").keys.app_id_bytes())
    );
    assert_eq!(
        list.to_json().unwrap(),
        value(&accepted[0]),
        "the list record writes back"
    );
    // The same value as the Switch set's payment_lists case.
    let switch_case = arr(&switch("sealed.json"), "cases")
        .iter()
        .find(|c| st(c, "name") == "payment_lists record")
        .unwrap()
        .clone();
    assert_eq!(
        st(&switch_case, "plaintext").as_bytes(),
        value(&accepted[0])
    );

    let allowance =
        AllowanceRecord::bind(value(&accepted[1]), &alice, &nk, &key(&accepted[1])).unwrap();
    assert_eq!(allowance.counterparty, bob);
    assert_eq!(allowance.proposal.sender(), bob);
    assert_eq!(allowance.response.as_ref().unwrap().sender(), alice);
    assert_eq!(
        allowance.to_json().unwrap(),
        value(&accepted[1]),
        "the Allowance record writes back"
    );
    let terms = b64n::<32>(st(&paykit("identifiers.json")["terms_hash"], "terms_hash"));
    assert_eq!(allowance.terms_hash(), Some(terms));
    assert!(allowance.current_grant(&alice).is_none(), "no grant yet");
    assert_eq!(allowance.ledger_key(&nk).unwrap(), key(&accepted[2]));
    let ledger = bind_ledger(value(&accepted[2]), &nk, &key(&accepted[2]), &allowance).unwrap();
    assert!(ledger.entries.is_empty());
    assert_eq!(
        ledger.to_json().unwrap(),
        value(&accepted[2]),
        "the ledger writes back"
    );

    // The record with the grant history of documents.json binds, and its grant is current.
    let entry = &paykit("documents.json")["executor_grant_history_entry"];
    let mut with_grant: Value = serde_json::from_slice(value(&accepted[1])).unwrap();
    with_grant["grants"] = serde_json::json!([entry]);
    with_grant["grant"] = entry["grant"].clone();
    let granted = AllowanceRecord::read(&serde_json::to_vec(&with_grant).unwrap(), &alice).unwrap();
    let (_, executor) = granted.current_grant(&alice).unwrap();
    assert_eq!(executor.app_id(), keys.app("executor").keys.app_id_bytes());
    let after = pubky_switch_core::ops::RecordVersion {
        generation: 1,
        version: 13,
    };
    assert_eq!(
        granted.executor_at(&alice, after).unwrap(),
        Some(executor.app_id())
    );

    let rejections = arr(v, "binding_rejections");
    assert_eq!(rejections.len(), 8);
    for case in rejections {
        let name = st(case, "name");
        let error = match st(case, "collection") {
            "payment_lists" => PaymentListRecord::bind(value(case), &alice, &nk, &key(case)).err(),
            "allowances" => AllowanceRecord::bind(value(case), &alice, &nk, &key(case)).err(),
            "allowance_ledgers" => bind_ledger(value(case), &nk, &key(case), &allowance).err(),
            other => panic!("{name}: collection {other}"),
        }
        .unwrap_or_else(|| panic!("{name}: accepted"))
        .to_string();
        let rule = match name {
            "payment_lists: the shop's list at the key of bob's other app" => {
                let other = Collection::PaymentLists
                    .record_key(&nk, &natural_key::payment_list(&bob, &keys.app("cap").keys.app_id_bytes()), None)
                    .unwrap();
                assert_eq!(key(case), other);
                "isn't at the key derived from payee and app_id"
            }
            "payment_lists: app_id names bob's other app, at that app's key" => "sender app isn't app_id",
            "payment_lists: payee names alice, at the key of alice and the shop's App ID, holding the list bob sent" => {
                "sender identity isn't payee"
            }
            "allowances: the record at another Allowance's key" => {
                "isn't at the key derived from counterparty and allowance_id"
            }
            "allowances: allowance_id names another Allowance, at its key"
            | "allowances: an acceptance naming another Allowance" => "names another Allowance",
            "allowance_ledgers: the ledger at another Allowance's ledger key"
            | "allowance_ledgers: counterparty names alice herself, at the Allowance's ledger key" => {
                "isn't at the key derived from its counterparty and allowance_id"
            }
            other => panic!("unknown binding rejection {other}"),
        };
        assert!(error.contains(rule), "{name}: {error}");
    }
}

fn label_of(read_scope: &str) -> Option<Label> {
    match PaykitScope::classify(read_scope) {
        ScopeName::Paykit(PaykitScope::RequestLabel(label)) => Some(label),
        _ => None,
    }
}

fn check_labelled(keys: &Keys, l: &Value) {
    let bob = pubky(BOB);
    assert_eq!(unhex32(st(l, "identity_hex")), bob);
    let identity = keys.identity(&bob);
    let nk = identity.naming_key();
    let epoch = l["data_epoch"].as_u64().unwrap();
    let shop1 = Label::parse("shop1").unwrap();
    let label_key = identity.scope_key(epoch, &shop1.scope());
    assert_eq!(hex(&label_key), st(l, "label_key_hex"));
    let policy = Collection::PayeeActivity.policy(Some(&shop1));
    assert_eq!(policy.read_scope, st(l, "read_scope"));
    assert_eq!(
        serde_json::to_value(&policy.write_scopes).unwrap(),
        l["write_scopes"]
    );
    assert_eq!(b64(&policy.hash().unwrap()), st(l, "policy_hash"));
    let natural = natural_key::request_record(
        &bob,
        &request_hash(&message_json("payment_request_labelled")),
    );
    let record_key = Collection::PayeeActivity
        .record_key(&nk, &natural, Some(&shop1))
        .unwrap();
    assert_eq!(b64(&record_key), st(l, "record_key"));

    // Sealed under the label key, with request:shop1 in the AAD, byte for byte.
    let plaintext = st(l, "plaintext").as_bytes();
    assert_eq!(
        hex(&aad(&bob, "request:shop1", &record_key)),
        st(l, "aad_hex")
    );
    let nonce: [u8; 24] = crate::common::unhex(st(l, "nonce_hex")).try_into().unwrap();
    let sealed = SealedValue::seal_with_nonce(
        &label_key,
        &bob,
        epoch,
        "request:shop1",
        &record_key,
        plaintext,
        &nonce,
    )
    .unwrap();
    let want: SealedValue = serde_json::from_value(l["sealed"].clone()).unwrap();
    assert_eq!(sealed, want);
    assert_eq!(
        pad(plaintext).unwrap().len() as u64,
        l["padded_len"].as_u64().unwrap()
    );
    assert_eq!(
        sealed.open(&label_key, &bob, &record_key).unwrap(),
        RecordValue::Json(plaintext.to_vec())
    );
    // The request key and another label's key don't open it.
    let root = identity.switch_root();
    for other in [
        key_of_scope(&root, epoch, "request"),
        key_of_scope(&root, epoch, "request:shop2"),
    ] {
        assert!(sealed.open(&other, &bob, &record_key).is_err());
    }

    // Read as bob holds it: the record binds shop1 and sits at its key.
    let record = ActivityRecord::bind(
        plaintext,
        &bob,
        ActivitySide::Payee,
        &nk,
        &record_key,
        Some(&shop1),
    )
    .unwrap();
    assert_eq!(
        record.to_json().unwrap(),
        plaintext,
        "the record writes back"
    );
    assert_eq!(
        record.request.as_ref().unwrap().message().from.app_id,
        keys.app("hosted").keys.app_id_bytes()
    );
    assert_eq!(
        record.response.as_ref().unwrap().hash(),
        request_hash(&message_json("payment_request_acceptance_labelled"))
    );
    assert_ne!(
        record.record_key(&bob, &nk, None).unwrap(),
        record_key,
        "the fixed policy's key is another"
    );

    let rejections = arr(l, "rejections");
    assert_eq!(rejections.len(), 3);
    for case in rejections {
        let name = st(case, "name");
        let label = label_of(st(case, "read_scope"));
        let policy = Collection::PayeeActivity.policy(label.as_ref());
        assert_eq!(policy.read_scope, st(case, "read_scope"), "{name}");
        assert_eq!(
            b64(&policy.hash().unwrap()),
            st(case, "policy_hash"),
            "{name}"
        );
        let read = ActivityRecord::read(value(case), &bob, ActivitySide::Payee).unwrap();
        assert_eq!(
            b64(&read.record_key(&bob, &nk, label.as_ref()).unwrap()),
            st(case, "record_key"),
            "{name}"
        );
        let error = ActivityRecord::bind(
            value(case),
            &bob,
            ActivitySide::Payee,
            &nk,
            &key(case),
            label.as_ref(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("another label than the record's policy"),
            "{name}: {error}"
        );
    }

    // The Switch set's payer request record binds under the fixed policy, whatever the label.
    let alice = pubky(ALICE);
    let case = arr(&switch("sealed.json"), "cases")
        .iter()
        .find(|c| st(c, "name") == "payer_activity request record")
        .unwrap()
        .clone();
    let anks = keys.identity(&alice).naming_key();
    let payer = ActivityRecord::bind(
        st(&case, "plaintext").as_bytes(),
        &alice,
        ActivitySide::Payer,
        &anks,
        &b64n::<32>(st(&case, "record_key")),
        None,
    )
    .unwrap();
    assert_eq!(payer.to_json().unwrap(), st(&case, "plaintext").as_bytes());
    assert_eq!(
        payer.record_key(&alice, &anks, Some(&shop1)).unwrap(),
        payer.record_key(&alice, &anks, None).unwrap()
    );
}

/// A stored message of `messages.json`, by name, as its recipient stores it.
fn stored_message(keys: &Keys, name: &str) -> paykit_lib::records::StoredMessage {
    let m = crate::common::message(name);
    stored_json(keys, &m)
}

/// A stored message from a message case: its `json`, `signature` and `sender_app`, with the
/// sender app's certificate of the Switch set.
fn stored_json(keys: &Keys, m: &Value) -> paykit_lib::records::StoredMessage {
    use pubky_switch_core::message::MessagePlaintext;
    let sender = keys.app_by_id(st(m, "sender_app"));
    let plaintext = MessagePlaintext::from_parts(
        sender.keys.app_id_bytes(),
        b64n::<64>(st(m, "signature")),
        st(m, "json").as_bytes().to_vec(),
    )
    .unwrap();
    let to = pubky_switch_core::message::Message::parse(
        st(m, "json").as_bytes(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap()
    .to;
    paykit_lib::records::StoredMessage::new(
        sender.identity,
        &crate::common::cert_doc(&sender.name),
        plaintext,
        None,
        &to,
    )
    .unwrap()
}

/// An empty activity record.
fn activity(side: ActivitySide, counterparty: [u8; 32]) -> ActivityRecord {
    ActivityRecord {
        side,
        counterparty,
        request: None,
        response: None,
        cancellation: None,
        proof: None,
        receipt: None,
        period_details: None,
        request_hash: None,
        billing_period: None,
        attempt_id: None,
        unrequested: None,
        msg_ids: Vec::new(),
        status: None,
        allowance_id: None,
        attempted_periods: Vec::new(),
    }
}

/// The three records the cases change, as their readers hold them: alice's payer request
/// record of the recurring request, naming the Allowance; her period record of its February
/// period, with its details and proof; her record of the payment without a request; and
/// bob's payee request record of the recurring request.
struct Bases {
    payer_request: ActivityRecord,
    period: ActivityRecord,
    unrequested: ActivityRecord,
    payee_request: ActivityRecord,
}

fn bases(keys: &Keys) -> Bases {
    use paykit_lib::bodies::Body;
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::{DestinationSource, UnrequestedDestination};
    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let request = stored_message(keys, "payment_request_recurring");
    let payer_request = ActivityRecord {
        request: Some(request.clone()),
        allowance_id: Some(AllowanceId::parse(ALLOWANCE_ID).unwrap()),
        ..activity(ActivitySide::Payer, bob)
    };
    let details = stored_message(keys, "payment_period_details");
    let period_proof = stored_message(keys, "payment_proof_period");
    let period = ActivityRecord {
        request_hash: Some(request.hash()),
        billing_period: details.body().billing_period().copied(),
        period_details: Some(details),
        proof: Some(period_proof),
        ..activity(ActivitySide::Payer, bob)
    };
    let proof = stored_message(keys, "payment_proof_unrequested");
    let list = stored_message(keys, "private_payment_list");
    let (Body::PaymentProof(p), Body::PrivatePaymentList(l)) = (proof.body(), list.body()) else {
        panic!("a proof and a list")
    };
    let endpoint = &l.payment_endpoints[&p.payment_endpoint_identifier];
    let unrequested = ActivityRecord {
        attempt_id: Some(p.attempt_id),
        unrequested: Some(UnrequestedDestination {
            app_id: list.message().from.app_id,
            endpoint_id: p.payment_endpoint_identifier.clone(),
            payload: endpoint.payload.clone(),
            source: DestinationSource::List,
            list_version: Some(l.version),
            single_use: endpoint.single_use,
        }),
        proof: Some(proof.clone()),
        ..activity(ActivitySide::Payer, bob)
    };
    let payee_request = ActivityRecord {
        request: Some(request),
        ..activity(ActivitySide::Payee, alice)
    };
    Bases {
        payer_request,
        period,
        unrequested,
        payee_request,
    }
}

/// The reader of a record: its identity, by side.
fn reader(record: &ActivityRecord) -> [u8; 32] {
    match record.side {
        ActivitySide::Payer => pubky(ALICE),
        ActivitySide::Payee => pubky(BOB),
    }
}

/// The record key of `base` with its reader's naming key, under the fixed policy.
fn base_key(keys: &Keys, base: &ActivityRecord) -> [u8; 32] {
    let own = reader(base);
    base.record_key(&own, &keys.identity(&own).naming_key(), None)
        .unwrap()
}

/// Bind `value`, a record of `side`, at `key` under the fixed policy, as its reader.
fn bind_activity(
    keys: &Keys,
    value: &[u8],
    side: ActivitySide,
    key: &[u8; 32],
) -> paykit_lib::Result<ActivityRecord> {
    let own = match side {
        ActivitySide::Payer => pubky(ALICE),
        ActivitySide::Payee => pubky(BOB),
    };
    ActivityRecord::bind(
        value,
        &own,
        side,
        &keys.identity(&own).naming_key(),
        key,
        None,
    )
}

/// A case of `activity_records`: its name, the record and the key it sits at.
type ActivityCase = (&'static str, ActivityRecord, [u8; 32]);

/// The cases of `activity_records`, accepted then refused.
fn activity_cases(keys: &Keys) -> (Vec<ActivityCase>, Vec<ActivityCase>) {
    use paykit_lib::identifiers::AllowanceId;
    let b = bases(keys);
    let keyed =
        |name, record: ActivityRecord, base: &ActivityRecord| (name, record, base_key(keys, base));
    let allowance = Some(AllowanceId::parse(ALLOWANCE_ID).unwrap());
    let request_hash = b.period.request_hash;
    let billing_period = b.period.billing_period;
    let accepted = vec![
        keyed(
            "a payer's request record naming its Allowance",
            b.payer_request.clone(),
            &b.payer_request,
        ),
        keyed(
            "a period record with request_hash and billing_period",
            b.period.clone(),
            &b.period,
        ),
        keyed(
            "the record of a payment without a request with attempt_id and unrequested",
            b.unrequested.clone(),
            &b.unrequested,
        ),
        keyed(
            "a payee's request record",
            b.payee_request.clone(),
            &b.payee_request,
        ),
    ];
    let refused = vec![
        keyed(
            "request_hash in a request record",
            ActivityRecord {
                request_hash,
                ..b.payer_request.clone()
            },
            &b.payer_request,
        ),
        keyed(
            "billing_period in a request record",
            ActivityRecord {
                billing_period,
                ..b.payer_request.clone()
            },
            &b.payer_request,
        ),
        keyed(
            "attempt_id in a request record",
            ActivityRecord {
                attempt_id: b.unrequested.attempt_id,
                ..b.payer_request.clone()
            },
            &b.payer_request,
        ),
        keyed(
            "unrequested in a request record",
            ActivityRecord {
                unrequested: b.unrequested.unrequested.clone(),
                ..b.payer_request.clone()
            },
            &b.payer_request,
        ),
        keyed(
            "attempt_id in a period record",
            ActivityRecord {
                attempt_id: b.unrequested.attempt_id,
                ..b.period.clone()
            },
            &b.period,
        ),
        keyed(
            "unrequested in a period record",
            ActivityRecord {
                unrequested: b.unrequested.unrequested.clone(),
                ..b.period.clone()
            },
            &b.period,
        ),
        keyed(
            "allowance_id in a period record",
            ActivityRecord {
                allowance_id: allowance.clone(),
                ..b.period.clone()
            },
            &b.period,
        ),
        keyed(
            "allowance_id in the record of a payment without a request",
            ActivityRecord {
                allowance_id: allowance.clone(),
                ..b.unrequested.clone()
            },
            &b.unrequested,
        ),
        keyed(
            "allowance_id in a payee's request record",
            ActivityRecord {
                allowance_id: allowance,
                ..b.payee_request.clone()
            },
            &b.payee_request,
        ),
    ];
    (accepted, refused)
}

fn side_collection(side: ActivitySide) -> &'static str {
    match side {
        ActivitySide::Payer => "payer_activity",
        ActivitySide::Payee => "payee_activity",
    }
}

/// The top-level description, which names every member.
const DESCRIPTION: &str =
    "Paykit's sealed record values (wire 5), sealed as the Switch's sealed values are (the \
Switch set's sealed.json) under alice's scope_key(2, \"pay\") from the Switch set's \
keys.json, with record keys blinded with alice's naming key. binding_accepted holds record \
values, as they are sealed, that a reader binds to their record keys, recomputed with \
alice's naming key (wire 5.4, 5.5): a payment_lists record, an allowances record and its \
allowance_ledgers record, checked against that allowances record. binding_rejections holds \
the same values at a wrong key or with a field that doesn't match, a list record whose \
payee isn't the identity that sent its list (bob's list, filed under payee alice at the \
key of alice and the shop's App ID) and an acceptance naming another Allowance, each of \
which a reader refuses. labelled is a payee record of a labelled request. activity_records \
holds activity records a reader binds, and the same records with a member their binding \
and side don't allow, which it refuses.";

const ACTIVITY_DESCRIPTION: &str =
    "Wire 5.3: activity record values, built from messages.json's messages, and \
the record keys they sit at under the fixed policy, blinded with their reader's naming key: \
alice's for payer_activity, bob's for payee_activity. Some members only some records carry: \
request_hash and billing_period only a period record, attempt_id and unrequested only the \
record of a payment without a request, and allowance_id only a payer's request record. \
accepted are records their reader binds at their keys; refused are the same records with a \
member where their binding and side don't allow it, which their reader refuses, and, last, \
alice's request records storing a request whose sender's certificate doesn't allow it \
(wire 5.2, the sender's side of check 8): messages.json label_rejections' request labelled \
shop2 from bob's hosted app, which holds only request:shop1, and messages.json's \
payment_request presented with documents.json's chat-only certificate of the shop's keys, at \
the keys their requests name. Appended after \
them, on payment evidence (wire 4.11): accepted, records whose receipt is of the stored \
proof's attempt, or of the record's attempt_id; refused, the same with a receipt of another \
attempt. reason is paykit-lib's wording, of the first check that \
refuses it.";

/// The cases paykit-lib generates in `sealed.json` (README "How the files were generated"),
/// compared with the file, and written into it with `PAYKIT_WRITE_VECTORS`.
fn generate(keys: &Keys) {
    let (accepted, refused) = activity_cases(keys);
    let write = |(name, record, key): &ActivityCase, taken: bool| -> Value {
        // A refused record is written without the writer's rules, which refuse it too.
        assert_eq!(record.to_json().is_ok(), taken, "{name}: the writer");
        let value = record.to_json_unchecked().unwrap();
        let mut out = serde_json::json!({
            "collection": side_collection(record.side),
            "name": name,
            "record_key": b64(key),
            "value": String::from_utf8(value.clone()).unwrap(),
        });
        match bind_activity(keys, &value, record.side, key) {
            Ok(_) => assert!(taken, "{name}: taken"),
            Err(error) => {
                assert!(!taken, "{name}: {error}");
                out["reason"] = serde_json::json!(quoted(&error.to_string()));
            }
        }
        out
    };
    let (evidence_accepted, evidence_refused) = evidence_cases(keys);
    let accepted: Vec<ActivityCase> = accepted.into_iter().chain(evidence_accepted).collect();
    let mut refused: Vec<Value> = refused.iter().map(|c| write(c, false)).collect();
    for (name, m, cert) in unauthorized_stored(keys) {
        let (value, key) = record_with_request(keys, stored_value(&m, &cert), st(&m, "json"));
        let error = bind_activity(keys, &value, ActivitySide::Payer, &key)
            .expect_err(name)
            .to_string();
        refused.push(serde_json::json!({
            "collection": "payer_activity",
            "name": name,
            "reason": quoted(&error),
            "record_key": b64(&key),
            "value": String::from_utf8(value).unwrap(),
        }));
    }
    refused.extend(evidence_refused.iter().map(|c| write(c, false)));
    let member = serde_json::json!({
        "accepted": accepted.iter().map(|c| write(c, true)).collect::<Vec<_>>(),
        "description": ACTIVITY_DESCRIPTION,
        "refused": refused,
    });
    generated(
        "sealed.json",
        &[
            (
                Place::Member("/description"),
                serde_json::json!(DESCRIPTION),
            ),
            (Place::Member("/activity_records"), member),
            (Place::Member("/msg_ids_bounds"), msg_ids_bounds(keys)),
        ],
    );
}

/// `activity_records` (Paykit wire 5.3): each record binds at its key, or is refused, as its
/// list says; the accepted ones write back byte for byte.
fn check_activity_records(keys: &Keys, v: &Value) {
    let m = &v["activity_records"];
    let (accepted, refused) = activity_cases(keys);
    assert_eq!(
        arr(m, "accepted").len(),
        accepted.len() + evidence_cases(keys).0.len()
    );
    assert_eq!(
        arr(m, "refused").len(),
        refused.len() + unauthorized_stored(keys).len() + evidence_cases(keys).1.len()
    );
    for (list, taken) in [("accepted", true), ("refused", false)] {
        for case in arr(m, list) {
            let name = st(case, "name");
            let side = match st(case, "collection") {
                "payer_activity" => ActivitySide::Payer,
                "payee_activity" => ActivitySide::Payee,
                other => panic!("{name}: collection {other}"),
            };
            match bind_activity(keys, value(case), side, &key(case)) {
                Ok(record) => {
                    assert!(taken, "{name}: taken");
                    assert_eq!(
                        record.to_json().unwrap(),
                        value(case),
                        "{name}: writes back"
                    );
                }
                Err(error) => {
                    assert!(!taken, "{name}: {error}");
                    assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
                }
            }
        }
    }
}

/// A base record with one member placed where its binding or side doesn't allow it is
/// refused, naming that member (Paykit wire 5.3).
fn refuses_member(record: &ActivityRecord, member: &str) {
    let json = record.to_json_unchecked().unwrap();
    let error = ActivityRecord::read(&json, &reader(record), record.side)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!("{member} only in")),
        "{member}: {error}"
    );
    // The writer refuses to write it, for the same rule.
    let error = record.to_json().unwrap_err();
    assert!(
        matches!(&error, paykit_lib::Error::Validation(why) if why.contains(&format!("{member} only in"))),
        "{member}: {error}"
    );
}

#[test]
fn test_activity_record_refuses_request_hash_outside_a_period_record() {
    let keys = Keys::load();
    let b = bases(&keys);
    let record = ActivityRecord {
        request_hash: b.period.request_hash,
        ..b.payer_request
    };
    refuses_member(&record, "request_hash");
}

#[test]
fn test_activity_record_refuses_billing_period_outside_a_period_record() {
    let keys = Keys::load();
    let b = bases(&keys);
    let record = ActivityRecord {
        billing_period: b.period.billing_period,
        ..b.payer_request
    };
    refuses_member(&record, "billing_period");
}

#[test]
fn test_activity_record_refuses_attempt_id_outside_an_unrequested_record() {
    let keys = Keys::load();
    let b = bases(&keys);
    for base in [b.payer_request, b.period] {
        let record = ActivityRecord {
            attempt_id: b.unrequested.attempt_id,
            ..base
        };
        refuses_member(&record, "attempt_id");
    }
}

#[test]
fn test_activity_record_refuses_unrequested_outside_an_unrequested_record() {
    let keys = Keys::load();
    let b = bases(&keys);
    for base in [b.payer_request, b.period] {
        let record = ActivityRecord {
            unrequested: b.unrequested.unrequested.clone(),
            ..base
        };
        refuses_member(&record, "unrequested");
    }
}

#[test]
fn test_activity_record_refuses_allowance_id_outside_a_payer_request_record() {
    let keys = Keys::load();
    let b = bases(&keys);
    let allowance = b.payer_request.allowance_id.clone();
    for base in [b.period, b.unrequested, b.payee_request] {
        let record = ActivityRecord {
            allowance_id: allowance.clone(),
            ..base
        };
        refuses_member(&record, "allowance_id");
    }
}

const MSG_IDS_DESCRIPTION: &str = "Wire 5.3, 5.4: binding_accepted's payment_lists record with \
msg_ids replaced, and the msg_ids its reader keeps (kept), which a writer writes: the ID of \
each message the record stores, then the 16 newest distinct IDs of the others it names, \
oldest first, each once. Repeats are dropped at their later places before the 16 are taken. \
The record binds at binding_accepted's record_key.";

/// The `msg_ids` cases of `msg_ids_bounds`: names and the list a record names.
fn msg_ids_cases(list_id: &str) -> Vec<(&'static str, Vec<String>)> {
    let uuid = |i: u64| format!("6d7d1f0e-6c4e-4c47-9d2b-{i:012x}");
    let many: Vec<String> = (0..20).map(uuid).collect();
    let mut repeats: Vec<String> = (0..16).map(uuid).collect();
    repeats.extend(std::iter::repeat_n(uuid(15), 8));
    repeats.extend(std::iter::repeat_n(uuid(99), 8));
    let mut stored_late: Vec<String> = (0..3).map(uuid).collect();
    stored_late.push(list_id.to_owned());
    vec![
        ("20 other IDs: the 16 newest are kept", many),
        (
            "16 other IDs, then two IDs repeated: each counts once",
            repeats,
        ),
        (
            "the stored list's ID named after others: it comes first",
            stored_late,
        ),
    ]
}

/// `msg_ids_bounds`, as paykit-lib generates it.
fn msg_ids_bounds(keys: &Keys) -> Value {
    let v = paykit("sealed.json");
    let accepted = &arr(&v, "binding_accepted")[0];
    let alice = pubky(ALICE);
    let base = PaymentListRecord::read(value(accepted), &alice).unwrap();
    let list_id = base.list.message().msg_id.clone();
    let cases: Vec<Value> = msg_ids_cases(&list_id)
        .into_iter()
        .map(|(name, ids)| {
            let mut record: Value = serde_json::from_slice(value(accepted)).unwrap();
            record["msg_ids"] = serde_json::json!(ids);
            let record = serde_json::to_vec(&record).unwrap();
            let read = PaymentListRecord::bind(
                &record,
                &alice,
                &keys.identity(&alice).naming_key(),
                &key(accepted),
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(read.msg_ids.len() <= 1 + paykit_lib::records::RECENT_MSG_IDS);
            serde_json::json!({
                "kept": read.msg_ids,
                "name": name,
                "value": String::from_utf8(record).unwrap(),
            })
        })
        .collect();
    serde_json::json!({ "cases": cases, "description": MSG_IDS_DESCRIPTION })
}

/// `msg_ids_bounds` (Paykit wire 5.3, 5.4): each record binds, its reader keeps `kept`, and
/// writes it back with that list.
fn check_msg_ids_bounds(keys: &Keys, v: &Value) {
    let alice = pubky(ALICE);
    let accepted = &arr(v, "binding_accepted")[0];
    let cases = arr(&v["msg_ids_bounds"], "cases");
    assert_eq!(cases.len(), 3);
    for case in cases {
        let name = st(case, "name");
        let read = PaymentListRecord::bind(
            value(case),
            &alice,
            &keys.identity(&alice).naming_key(),
            &key(accepted),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let kept: Vec<String> = serde_json::from_value(case["kept"].clone()).unwrap();
        assert_eq!(read.msg_ids, kept, "{name}");
        let back: Value = serde_json::from_slice(&read.to_json().unwrap()).unwrap();
        assert_eq!(back["msg_ids"], case["kept"], "{name}: written");
    }
}

#[test]
fn test_activity_record_writes_canonical_msg_ids() {
    let keys = Keys::load();
    let b = bases(&keys);
    let request_id = b
        .payer_request
        .request
        .as_ref()
        .unwrap()
        .message()
        .msg_id
        .clone();
    let uuid = |i: u64| format!("6d7d1f0e-6c4e-4c47-9d2b-{i:012x}");
    let mut msg_ids: Vec<String> = (0..20).map(uuid).collect();
    msg_ids.extend(std::iter::repeat_n(uuid(19), 5));
    let record = ActivityRecord {
        msg_ids,
        ..b.payer_request
    };
    let written = record.to_json().unwrap();
    let back: Value = serde_json::from_slice(&written).unwrap();
    let mut want = vec![request_id];
    want.extend((4..20).map(uuid));
    assert_eq!(back["msg_ids"], serde_json::json!(want));
    let read = ActivityRecord::read(&written, &pubky(ALICE), ActivitySide::Payer).unwrap();
    assert_eq!(read.msg_ids, want);
    assert_eq!(read.to_json().unwrap(), written);
}

/// A stored message as a record holds it (Paykit wire 5.2), from a message case signed by
/// `sender_app`, with the certificate `cert`; built by hand, since a reader refuses it.
fn stored_value(m: &Value, cert: &pubky_switch_core::sig::SignedDoc) -> Value {
    let json = st(m, "json");
    let message = pubky_switch_core::message::Message::parse(
        json.as_bytes(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    serde_json::json!({
        "accepted_at": null,
        "cert": { "body": cert.body, "sig": cert.sig },
        "plaintext": {
            "json": b64(json.as_bytes()),
            "sender_app": b64(&message.from.app_id),
            "signature": st(m, "signature"),
        },
        "sender": pubky_switch_core::enc::pubky_id(&message.from.identity),
    })
}

/// Messages a reader refuses from shared state for check 8's sender side (Paykit wire 5.2):
/// names, the message case and the sender certificate presented with it.
fn unauthorized_stored(
    keys: &Keys,
) -> Vec<(&'static str, Value, pubky_switch_core::sig::SignedDoc)> {
    let labels = paykit("messages.json");
    let shop2 = arr(&labels["label_rejections"], "cases")
        .iter()
        .find(|c| st(c, "name") == "another label than the sender's certificate holds")
        .unwrap()
        .clone();
    let chat = arr(
        &paykit("documents.json")["multi_protocol_certificates"],
        "accepted",
    )
    .iter()
    .find(|c| st(c, "name") == "chat only, with lists.read")
    .unwrap()["signed"]
        .clone();
    vec![
        (
            "a stored request labelled shop2 from the hosted app, which holds only request:shop1",
            shop2,
            crate::common::cert_doc("hosted"),
        ),
        (
            "a stored payment_request from the shop with a certificate holding only chat",
            crate::common::message("payment_request"),
            crate::common::signed(&chat),
        ),
    ]
    .into_iter()
    .inspect(|(_, m, _)| assert_eq!(keys.app_by_id(st(m, "sender_app")).identity, pubky(BOB)))
    .collect()
}

/// Alice's payer request record holding `request`, as JSON, and the key it would sit at.
fn record_with_request(keys: &Keys, request: Value, json: &str) -> (Vec<u8>, [u8; 32]) {
    let alice = pubky(ALICE);
    let base = activity(ActivitySide::Payer, pubky(BOB));
    let mut record: Value = serde_json::from_slice(&base.to_json().unwrap()).unwrap();
    record["request"] = request;
    let natural = natural_key::request_record(&pubky(BOB), &request_hash(json.as_bytes()));
    let key = Collection::PayerActivity
        .record_key(&keys.identity(&alice).naming_key(), &natural, None)
        .unwrap();
    (serde_json::to_vec(&record).unwrap(), key)
}

#[test]
fn test_stored_message_refuses_a_label_its_sender_does_not_hold() {
    use paykit_lib::records::StoredMessage;
    let keys = Keys::load();
    let alice = pubky(ALICE);
    for (name, m, cert) in unauthorized_stored(&keys) {
        let error = StoredMessage::read(&stored_value(&m, &cert), &alice)
            .unwrap_err()
            .to_string();
        assert!(error.contains("sender's certificate"), "{name}: {error}");
        let (record, key) = record_with_request(&keys, stored_value(&m, &cert), st(&m, "json"));
        assert!(
            bind_activity(&keys, &record, ActivitySide::Payer, &key).is_err(),
            "{name}: in a record"
        );
    }
    // The same request under the label the hosted app holds is stored.
    let labelled = crate::common::message("payment_request_labelled");
    StoredMessage::read(
        &stored_value(&labelled, &crate::common::cert_doc("hosted")),
        &alice,
    )
    .unwrap();
}

#[test]
fn test_activity_record_writer_refuses_what_readers_refuse() {
    use paykit_lib::records::DestinationSource;
    let keys = Keys::load();
    let b = bases(&keys);
    let other_request = stored_message(&keys, "payment_request_acceptance");
    let mut no_version = b.unrequested.clone();
    if let Some(u) = no_version.unrequested.as_mut() {
        u.source = DestinationSource::List;
        u.list_version = None;
    }
    let cases: Vec<(&str, ActivityRecord)> = vec![
        (
            "a response to another request (binding, refs)",
            ActivityRecord {
                response: Some(other_request),
                ..b.payer_request.clone()
            },
        ),
        (
            "a request in the proof's role (kind)",
            ActivityRecord {
                proof: b.payer_request.request.clone(),
                ..b.period.clone()
            },
        ),
        (
            "a period record whose billing_period isn't its messages'",
            ActivityRecord {
                billing_period: Some(paykit_lib::periods::PeriodRef {
                    starts_at: 0,
                    ends_at: 1,
                }),
                ..b.period.clone()
            },
        ),
        (
            "attempted_periods out of order",
            ActivityRecord {
                attempted_periods: vec![1_772_236_800, 1_769_817_600],
                ..b.payer_request.clone()
            },
        ),
        (
            "attempted_periods in a period record",
            ActivityRecord {
                attempted_periods: vec![1_769_817_600],
                ..b.period.clone()
            },
        ),
        ("a list destination without its version", no_version),
        (
            "a payer's record seen as the payee's (side)",
            ActivityRecord {
                side: ActivitySide::Payee,
                allowance_id: None,
                ..b.payer_request.clone()
            },
        ),
    ];
    for (name, record) in cases {
        let error = record.to_json().expect_err(name);
        assert!(
            matches!(error, paykit_lib::Error::Validation(_)),
            "{name}: {error}"
        );
        let json = record.to_json_unchecked().unwrap();
        let own = reader(&record);
        assert!(
            ActivityRecord::read(&json, &own, record.side).is_err(),
            "{name}: the reader refuses it too"
        );
    }
    // The base records write, and read back.
    for record in [b.payer_request, b.period, b.unrequested, b.payee_request] {
        let json = record.to_json().unwrap();
        ActivityRecord::read(&json, &reader(&record), record.side).unwrap();
    }
}

#[test]
fn test_record_writers_refuse_msg_ids_that_are_not_uuids() {
    let keys = Keys::load();
    let alice = pubky(ALICE);
    let v = paykit("sealed.json");
    let accepted = arr(&v, "binding_accepted");
    let bad = vec!["not-a-uuid".to_owned()];
    let upper = vec!["6D7D1F0E-6C4E-4C47-9D2B-000000000001".to_owned()];
    for ids in [bad, upper] {
        let activity = ActivityRecord {
            msg_ids: ids.clone(),
            ..bases(&keys).payer_request
        };
        assert!(activity.to_json().is_err(), "activity record {ids:?}");
        let mut list = PaymentListRecord::read(value(&accepted[0]), &alice).unwrap();
        list.msg_ids = ids.clone();
        assert!(list.to_json().is_err(), "payment_lists record {ids:?}");
        let mut allowance = AllowanceRecord::read(value(&accepted[1]), &alice).unwrap();
        allowance.msg_ids = ids.clone();
        assert!(allowance.to_json().is_err(), "allowances record {ids:?}");
    }
    // A UUID v4 among them writes, after the stored IDs.
    let mut list = PaymentListRecord::read(value(&accepted[0]), &alice).unwrap();
    list.msg_ids = vec!["6d7d1f0e-6c4e-4c47-9d2b-000000000001".to_owned()];
    let back = PaymentListRecord::read(&list.to_json().unwrap(), &alice).unwrap();
    assert_eq!(back.msg_ids.len(), 2);
}

/// A message of `messages.json` changed and signed again by its sender: `base` with its
/// `refs` and body replaced, stored as its recipient stores it.
fn resigned(
    keys: &Keys,
    base: &str,
    refs: Option<Vec<[u8; 32]>>,
    body: impl FnOnce(&mut serde_json::Map<String, Value>),
) -> paykit_lib::records::StoredMessage {
    use pubky_switch_core::message::MessagePlaintext;
    let mut m = pubky_switch_core::message::Message::parse(
        &message_json(base),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    if let Some(refs) = refs {
        m.refs = refs;
    }
    body(&mut m.body);
    let json = m.to_json().unwrap();
    let sender = keys.app_by_id(&m.from.app_id_text());
    let plaintext = MessagePlaintext::sign_json(
        &sender.keys,
        json.clone(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    stored_json(
        keys,
        &serde_json::json!({
            "json": String::from_utf8(json).unwrap(),
            "sender_app": m.from.app_id_text(),
            "signature": b64(&plaintext.signature()),
        }),
    )
}

/// The cases of `activity_records` on payment evidence, appended after the others: a
/// receipt binds its proof's attempt (Paykit wire 4.11).
fn evidence_cases(keys: &Keys) -> (Vec<ActivityCase>, Vec<ActivityCase>) {
    let bob = pubky(BOB);
    let paid = ActivityRecord {
        request: Some(stored_message(keys, "payment_request")),
        proof: Some(stored_message(keys, "payment_proof")),
        receipt: Some(stored_message(keys, "payment_receipt")),
        ..activity(ActivitySide::Payer, bob)
    };
    let other_attempt = resigned(keys, "payment_receipt", None, |b| {
        b.insert(
            "attempt_id".into(),
            serde_json::json!("AAAAAAAAAAAAAAAAAAAAAA"),
        );
    });
    let b = bases(keys);
    let proof_hash = b.unrequested.proof.as_ref().unwrap().hash();
    let unrequested_receipt = |attempt: &str| {
        resigned(keys, "payment_receipt", Some(vec![proof_hash]), |b| {
            b.insert("attempt_id".into(), serde_json::json!(attempt));
        })
    };
    let attempt = b.unrequested.attempt_id.unwrap().to_text();
    let with_receipt = ActivityRecord {
        receipt: Some(unrequested_receipt(&attempt)),
        ..b.unrequested.clone()
    };
    let receipt_only = ActivityRecord {
        proof: None,
        receipt: Some(unrequested_receipt("AAAAAAAAAAAAAAAAAAAAAA")),
        ..b.unrequested.clone()
    };
    let key = |r: &ActivityRecord| base_key(keys, r);
    let accepted = vec![
        (
            "a payer's request record with the proof and the receipt of its attempt",
            paid.clone(),
            key(&paid),
        ),
        (
            "the record of a payment without a request with the receipt of its attempt",
            with_receipt.clone(),
            key(&with_receipt),
        ),
    ];
    let refused = vec![
        (
            "a receipt of another attempt than the stored proof's",
            ActivityRecord {
                receipt: Some(other_attempt),
                ..paid.clone()
            },
            key(&paid),
        ),
        (
            "the record of a payment without a request with a receipt of another attempt than its attempt_id",
            receipt_only,
            key(&b.unrequested),
        ),
    ];
    (accepted, refused)
}
