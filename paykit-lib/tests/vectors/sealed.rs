//! `sealed.json`: record values a reader binds to their keys, the rejections, the labelled
//! payee record, activity and settlement marks, and signed list publication counters
//! (Paykit wire 5, 5.1, 5.3, 5.4, 5.5, 5.7, 5.8).

pub(crate) mod publications;

use paykit_lib::collections::Collection;
use paykit_lib::derive::{
    mark_settlement_tag, natural_key, occurrence_tag, request_hash, settlement_bucket,
    settlement_tag,
};
use paykit_lib::identifiers::PaymentEndpointIdentifier;
use paykit_lib::records::{
    bind_ledger, ActivityRecord, ActivitySide, AllowanceRecord, OccurrenceKind, PaymentListRecord,
    PendingAllowanceRecord, SeenMerge, SettlementBucket, SettlementMark, MAX_SETTLEMENT_MARKS,
};
use paykit_lib::scopes::{Label, PaykitScope, ScopeName, SETTLEMENTS};
use pubky_switch_core::enc::b64;
use pubky_switch_core::ident::{key_of_scope, NamingKey, Policy};
use pubky_switch_core::records::AppCert;
use pubky_switch_core::sealed::{aad, pad, RecordValue, SealedValue};
use serde_json::Value;

use crate::common::{
    arr, b64n, generated, hex, message_json, paykit, pubky, quoted, recertified, st, unhex32,
    writing, Keys, Place, ALICE, ALLOWANCE_ID, BOB, UNREQUESTED_TXID,
};

#[test]
fn test_vectors_sealed() {
    if !writing() {
        check();
    }
}

/// `sealed.json` against paykit-lib: its generated cases, then every check of its cases. With
/// [`WRITE_VECTORS`](crate::common::WRITE_VECTORS) set, the regeneration driver runs it once it
/// has written the set.
pub(crate) fn check() {
    let keys = Keys::from_seeds();
    generate(&keys);
    let v = paykit("sealed.json");
    check_bindings(&keys, &v);
    check_labelled(&keys, &v["labelled"]);
    check_activity_rules(&keys);
    check_activity_records(&keys, &v);
    check_later_writes(&keys, &v);
    check_msg_ids_bounds(&keys, &v);
    check_allowance_records(&keys, &v);
    check_pending_proposals(&keys, &v["allowance_proposals"]);
    check_settlement_marks(&keys, &v["settlement_marks"]);
    publications::check(&keys, &v["private_list_publications"]);
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
        request: Some(stored("payment_request_recurring")),
        attempted_periods: vec![1_769_817_600, 1_772_236_800],
        ..ActivityRecord::new(ActivitySide::Payer, bob)
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

    // Sealed under the label key, with paykit_request:shop1 in the AAD, byte for byte.
    let plaintext = st(l, "plaintext").as_bytes();
    assert_eq!(
        hex(&aad(&bob, "paykit_request:shop1", &record_key)),
        st(l, "aad_hex")
    );
    let nonce: [u8; 24] = crate::common::unhex(st(l, "nonce_hex")).try_into().unwrap();
    let sealed = SealedValue::seal_with_nonce(
        &label_key,
        &bob,
        epoch,
        "paykit_request:shop1",
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
        key_of_scope(&root, epoch, "paykit_request"),
        key_of_scope(&root, epoch, "paykit_request:shop2"),
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

    // A payer request record binds under the fixed policy, whatever the label.
    let alice = pubky(ALICE);
    let case = arr(&paykit("sealed.json")["activity_records"], "accepted")
        .iter()
        .find(|c| {
            st(c, "name")
                == "a payer's request record with the proof and the receipt of its attempt"
        })
        .unwrap()
        .clone();
    let anks = keys.identity(&alice).naming_key();
    let payer = ActivityRecord::bind(
        value(&case),
        &alice,
        ActivitySide::Payer,
        &anks,
        &key(&case),
        None,
    )
    .unwrap();
    assert_eq!(payer.to_json().unwrap(), value(&case));
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
/// sender app's certificate of this set.
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
    ActivityRecord::new(side, counterparty)
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
            amount: p
                .amount
                .clone()
                .expect("a proof of a payment without a request has its amount"),
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
        keyed(
            "a payee's request record put back from an older copy",
            ActivityRecord {
                lost_to_delete: Some([7; 32]),
                ..b.payee_request.clone()
            },
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
Switch set's sealed.json) under alice's scope_key(2, \"paykit_pay\") from this set's \
keys.json, with record keys blinded with alice's naming key. binding_accepted holds record \
values, as they are sealed, that a reader binds to their record keys, recomputed with \
alice's naming key (wire 5.4, 5.5): a payment_lists record, an allowances record and its \
allowance_ledgers record, checked against that allowances record. binding_rejections holds \
the same values at a wrong key or with a field that doesn't match, a list record whose \
payee isn't the identity that sent its list (bob's list, filed under payee alice at the \
key of alice and the shop's App ID) and an acceptance naming another Allowance, each of \
which a reader refuses. labelled is a payee record of a labelled request. activity_records \
holds activity records a reader binds, and the same records with a member their binding \
and side don't allow, which it refuses. allowance_records holds allowances records a reader \
binds or refuses by their messages' lifecycle.";

const ACTIVITY_DESCRIPTION: &str =
    "Wire 5.3: activity record values, built from messages.json's messages, and \
the record keys they sit at under the fixed policy, blinded with their reader's naming key: \
alice's for payer_activity, bob's for payee_activity. Some members only some records carry: \
request_hash, billing_period and earlier_details only a period record, attempt_id and \
unrequested only the record of a payment without a request, allowance_id and \
reassociations only a payer's request record, and manual_only only a payer's request record \
or a payer's period record. \
accepted are records their reader binds at their keys; refused are the same records with a \
member where their binding and side don't allow it, which their reader refuses, and, last, \
alice's request records storing a request whose sender's certificate doesn't allow it \
(wire 5.2, the sender's side of check 8): messages.json label_rejections' request labelled \
shop2 from bob's hosted app, which holds only paykit_request:shop1, and messages.json's \
payment_request presented with documents.json's chat-only certificate of the shop's keys, at \
the keys their requests name. Appended after \
them, on payment evidence (wire 4.11): accepted, records whose receipt is of the stored \
proof's attempt, or of the record's attempt_id; refused, the same with a receipt of another \
attempt. Appended last, on the members only some records carry (wire 4.9, 5.3): accepted, \
a payer's request record of the recurring request with reassociations; alice's request \
record of the one-time request, manual_only; her request record of the recurring request, \
manual_only; her period record, manual_only; the period record whose proof names messages.json's period \
details, with later details, signed again by the shop a day later under another msg_id, in \
earlier_details; the same record without its proof, the later details in period_details and \
the earlier in earlier_details; the record of the payment without a request with \
unrequested.amount, the proof's; and that record with members its reader doesn't know, in \
the value, in unrequested and in the stored proof, which its writer keeps; and a \
reassociation from the February period of the recurring request signed again with its \
anchor at half a second past midnight, named by the second it starts in; refused, those \
members where the record's binding, side or request doesn't allow them, period details out \
of order or held twice, reassociations without allowance_id, naming it first or not \
strictly ascending, an unrequested.amount other than the stored proof's, and a \
reassociation naming a time that isn't a Billing Period start of the stored request, which \
periods name by their start cut to whole seconds (between two starts, before the first \
period, for the request signed again ending at its March period after its ends_at, and, for \
the request signed again anchored at half a second past midnight, the second before its \
February period, whose own second it accepts); and, last, a period record with manual_only \
false, which is true or absent. later_writes holds pairs of \
versions of one record, the version a reader holds (held) and a later write (value), both of \
which bind at record_key, with built_on_held true when the write builds on the held version \
(wire 5.3 \"Seen roles\", \"Seen Allowance\", \"Seen marks\"). On alice's request record of \
the recurring request with its February period attempted early: accepted, a reassociation \
from a period after every attempted period, and one from that period built on a later \
version that dropped it, which a reader holding the older version takes; refused, built on \
the held version, a reassociation from the attempted period and one from before it, since a \
period attempted early stays under the Allowance it was attempted under. Also refused, \
whatever the write builds on: a write dropping period details seen only in earlier_details. \
reason is paykit-lib's wording, of the first check that refuses it.";

/// Later period details for the February period of the recurring request: messages.json's
/// `payment_period_details` a day later, under another `msg_id`, signed again by the shop.
fn later_details(keys: &Keys) -> paykit_lib::records::StoredMessage {
    resign_with(keys, "payment_period_details", |m| {
        m.msg_id = "6b1f0e2a-3c4d-4e5f-8a6b-7c8d9e0f1a2b".into();
        m.created_at_ms += 86_400_000;
    })
}

/// The record `record` as its writer writes it, with members it doesn't know added at
/// `members` (JSON pointer to an object, member, value), then read back by its reader.
fn with_unknown(
    keys: &Keys,
    record: &ActivityRecord,
    key: &[u8; 32],
    members: &[(&str, &str, Value)],
) -> ActivityRecord {
    let mut value: Value = serde_json::from_slice(&record.to_json().unwrap()).unwrap();
    for (pointer, name, member) in members {
        value
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert((*name).to_owned(), member.clone());
    }
    bind_activity(keys, &serde_json::to_vec(&value).unwrap(), record.side, key).unwrap()
}

/// The cases on the members of wire 5.3 that some records carry, appended after the others:
/// `earlier_details`, `reassociations`, `manual_only` and `unrequested.amount`,
/// and members a reader doesn't know, which it keeps when it writes the record again.
fn member_cases(keys: &Keys) -> (Vec<ActivityCase>, Vec<ActivityCase>) {
    use paykit_lib::bodies::Body;
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::Reassociation;
    let bob = pubky(BOB);
    let b = bases(keys);
    let key = |r: &ActivityRecord| base_key(keys, r);
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    let named = b.payer_request.allowance_id.clone().unwrap();
    let moved = |entries: Vec<(AllowanceId, u64)>| -> Vec<Reassociation> {
        entries
            .into_iter()
            .map(|(allowance_id, from_period)| Reassociation {
                allowance_id,
                from_period,
            })
            .collect()
    };
    let reassociated = ActivityRecord {
        reassociations: moved(vec![(other.clone(), 1_772_236_800)]),
        ..b.payer_request.clone()
    };
    let one_time = ActivityRecord {
        request: Some(stored_message(keys, "payment_request")),
        ..activity(ActivitySide::Payer, bob)
    };
    let one_time_marked = ActivityRecord {
        manual_only: true,
        ..one_time.clone()
    };
    let period_marked = ActivityRecord {
        manual_only: true,
        ..b.period.clone()
    };
    let (held, later) = (
        b.period.period_details.clone().unwrap(),
        later_details(keys),
    );
    // The proof names the earlier details, so period_details holds them and the later ones
    // are earlier_details.
    let named_by_proof = ActivityRecord {
        earlier_details: vec![later.clone()],
        ..b.period.clone()
    };
    let without_proof = ActivityRecord {
        proof: None,
        ..b.period.clone()
    };
    let latest = ActivityRecord {
        period_details: Some(later.clone()),
        earlier_details: vec![held.clone()],
        ..without_proof.clone()
    };
    let Some(Body::PaymentProof(proof)) = b.unrequested.proof.as_ref().map(|p| p.body().clone())
    else {
        panic!("a proof")
    };
    let with_amount = |amount: paykit_lib::PaymentAmount| ActivityRecord {
        unrequested: b
            .unrequested
            .unrequested
            .clone()
            .map(|u| paykit_lib::records::UnrequestedDestination { amount, ..u }),
        ..b.unrequested.clone()
    };
    let paid_amount = with_amount(proof.amount.clone().unwrap());
    let mut other_amount = proof.amount.clone().unwrap();
    other_amount.value = paykit_lib::Decimal::parse("0.0016").unwrap();
    let unknown = with_unknown(
        keys,
        &paid_amount,
        &key(&b.unrequested),
        &[
            ("", "payment_note", serde_json::json!({ "text": "kept" })),
            ("/unrequested", "rail_hint", serde_json::json!("on-chain")),
            ("/proof", "relayed_by", serde_json::json!(null)),
        ],
    );
    let accepted = vec![
        (
            "a payer's request record of a recurring request with reassociations",
            reassociated.clone(),
            key(&reassociated),
        ),
        (
            "a payer's request record of a one-time request, manual_only",
            one_time_marked.clone(),
            key(&one_time),
        ),
        (
            "a payer's request record of a recurring request, manual_only",
            ActivityRecord {
                manual_only: true,
                ..b.payer_request.clone()
            },
            key(&b.payer_request),
        ),
        (
            "a payer's period record, manual_only",
            period_marked.clone(),
            key(&period_marked),
        ),
        (
            "a period record whose proof names earlier details, with later ones in earlier_details",
            named_by_proof.clone(),
            key(&named_by_proof),
        ),
        (
            "a period record without a proof, the latest details in period_details",
            latest.clone(),
            key(&latest),
        ),
        (
            "the record of a payment without a request with unrequested.amount, the proof's",
            paid_amount.clone(),
            key(&paid_amount),
        ),
        (
            "the record of a payment without a request with members its reader doesn't know",
            unknown.clone(),
            key(&unknown),
        ),
    ];
    let refused = vec![
        (
            "earlier_details in a request record",
            ActivityRecord {
                earlier_details: vec![held.clone()],
                ..b.payer_request.clone()
            },
            key(&b.payer_request),
        ),
        (
            "earlier_details later than period_details, which no proof names",
            ActivityRecord {
                earlier_details: vec![later.clone()],
                ..without_proof.clone()
            },
            key(&without_proof),
        ),
        (
            "earlier_details holding period_details again",
            ActivityRecord {
                earlier_details: vec![held.clone()],
                ..without_proof.clone()
            },
            key(&without_proof),
        ),
        (
            "reassociations in a period record",
            ActivityRecord {
                reassociations: moved(vec![(other.clone(), 1_772_236_800)]),
                ..b.period.clone()
            },
            key(&b.period),
        ),
        (
            "reassociations without allowance_id",
            ActivityRecord {
                allowance_id: None,
                ..reassociated.clone()
            },
            key(&reassociated),
        ),
        (
            "reassociations whose first entry names allowance_id",
            ActivityRecord {
                reassociations: moved(vec![(named.clone(), 1_772_236_800)]),
                ..b.payer_request.clone()
            },
            key(&b.payer_request),
        ),
        (
            "reassociations not strictly ascending",
            ActivityRecord {
                reassociations: moved(vec![(other.clone(), 1_772_236_800), (named, 1_772_236_800)]),
                ..b.payer_request.clone()
            },
            key(&b.payer_request),
        ),
        (
            "manual_only in a payee's request record",
            ActivityRecord {
                manual_only: true,
                ..b.payee_request.clone()
            },
            key(&b.payee_request),
        ),
        (
            "unrequested.amount other than the stored proof's",
            with_amount(other_amount),
            key(&b.unrequested),
        ),
    ];
    let (start_accepted, start_refused) = period_start_cases(keys);
    let mut accepted = accepted;
    accepted.extend(start_accepted);
    let mut refused = refused;
    refused.extend(start_refused);
    (accepted, refused)
}

/// The recurring request signed again by the shop with `recurrence` members changed.
fn recurring_with(keys: &Keys, members: &[(&str, &str)]) -> paykit_lib::records::StoredMessage {
    resign_with(keys, "payment_request_recurring", |m| {
        let recurrence = m
            .body
            .get_mut("recurrence")
            .and_then(Value::as_object_mut)
            .unwrap();
        for (name, value) in members {
            recurrence.insert((*name).into(), serde_json::json!(value));
        }
    })
}

/// Cases whose reassociation names a Billing Period of the stored request by its start, in
/// seconds (wire 5.3): accepted, the February period of the request signed again with its
/// anchor at half a second past midnight, which its start's second names; refused, a time
/// between two starts, before the first period, after the request's `ends_at`, and, for the
/// half-second anchor, the second before its February period.
fn period_start_cases(keys: &Keys) -> (Vec<ActivityCase>, Vec<ActivityCase>) {
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::Reassociation;
    let b = bases(keys);
    let key = |r: &ActivityRecord| base_key(keys, r);
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    let from = |base: &ActivityRecord, from_period| ActivityRecord {
        reassociations: vec![Reassociation {
            allowance_id: other.clone(),
            from_period,
        }],
        ..base.clone()
    };
    let on = |request| ActivityRecord {
        request: Some(request),
        ..b.payer_request.clone()
    };
    // The recurring request ending at its March period's start: February is its last.
    let ended = on(recurring_with(
        keys,
        &[("ends_at", "2026-03-31T00:00:00.000Z")],
    ));
    // The recurring request anchored at half a second past midnight.
    let half_second = on(recurring_with(
        keys,
        &[
            ("anchor", "2026-01-31T00:00:00.500Z"),
            ("starts_at", "2026-01-31T00:00:00.500Z"),
        ],
    ));
    (
        vec![(
            "a reassociation from a Billing Period starting half a second past its second",
            from(&half_second, 1_772_236_800),
            key(&half_second),
        )],
        vec![
            (
                "a reassociation from between two Billing Period starts",
                from(&b.payer_request, 1_772_236_801),
                key(&b.payer_request),
            ),
            (
                "a reassociation from before the first Billing Period",
                from(&b.payer_request, 1_767_225_600),
                key(&b.payer_request),
            ),
            (
                "a reassociation from after the request's ends_at",
                from(&ended, 1_774_915_200),
                key(&ended),
            ),
            (
                "a reassociation from the second before a Billing Period starting half a second past its second",
                from(&half_second, 1_772_236_799),
                key(&half_second),
            ),
        ],
    )
}

/// Member values the typed activity record writer cannot encode (wire 5.3).
fn written_member_cases(keys: &Keys) -> Vec<(&'static str, ActivitySide, Vec<u8>, [u8; 32])> {
    let b = bases(keys);
    let case = |name, record: &ActivityRecord, member: &str, field: Value| {
        let mut value: Value = serde_json::from_slice(&record.to_json().unwrap()).unwrap();
        value[member] = field;
        (
            name,
            record.side,
            serde_json::to_vec(&value).unwrap(),
            base_key(keys, record),
        )
    };
    vec![
        case(
            "manual_only false in a payer's period record",
            &b.period,
            "manual_only",
            serde_json::json!(false),
        ),
        case(
            "a null delete item hash",
            &b.payee_request,
            "lost_to_delete",
            Value::Null,
        ),
        case(
            "a boolean delete item hash",
            &b.payee_request,
            "lost_to_delete",
            serde_json::json!(true),
        ),
        case(
            "a short delete item hash",
            &b.payee_request,
            "lost_to_delete",
            serde_json::json!("AA"),
        ),
        case(
            "a padded delete item hash",
            &b.payee_request,
            "lost_to_delete",
            serde_json::json!(format!("{}=", b64(&[7; 32]))),
        ),
    ]
}

/// A later-write case of `activity_records`: its name, whether the write builds on the
/// version the reader holds, that version, the later write, and the key both sit at.
type LaterWriteCase = (&'static str, bool, ActivityRecord, ActivityRecord, [u8; 32]);

/// `activity_records`' later writes (wire 5.3 "Seen roles", "Seen Allowance", "Seen marks"):
/// alice's request record of the recurring request listing its February period as
/// attempted, early, and writes that add a reassociation, built on that version or on a later
/// one that dropped the period; and a later write that drops details seen only in
/// earlier_details.
fn later_write_cases(keys: &Keys) -> (Vec<LaterWriteCase>, Vec<LaterWriteCase>) {
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::Reassociation;
    let b = bases(keys);
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    let held = ActivityRecord {
        attempted_periods: vec![1_772_236_800],
        ..b.payer_request.clone()
    };
    let from = |base: &ActivityRecord, from_period| ActivityRecord {
        reassociations: vec![Reassociation {
            allowance_id: other.clone(),
            from_period,
        }],
        ..base.clone()
    };
    let key = base_key(keys, &held);
    // Built on the version that dropped the period once its attempt ended.
    let dropped = b.payer_request.clone();
    let (period, details) = period_held(keys);
    let later = later_details(keys);
    let both = ActivityRecord {
        period_details: Some(later.clone()),
        earlier_details: vec![details],
        ..period.clone()
    };
    let without_earlier = with_details(&period, later);
    (
        vec![
            (
                "a reassociation from a period after every attempted period",
                true,
                held.clone(),
                from(&held, 1_774_915_200),
                key,
            ),
            (
                "a reassociation from a period the held version lists, built on a later version that dropped it",
                false,
                held.clone(),
                from(&dropped, 1_772_236_800),
                key,
            ),
        ],
        vec![
            (
                "a reassociation from an attempted period",
                true,
                held.clone(),
                from(&held, 1_772_236_800),
                key,
            ),
            (
                "a reassociation from before an attempted period",
                true,
                held.clone(),
                from(&held, 1_769_817_600),
                key,
            ),
            (
                "a write dropping period details seen only in earlier_details",
                false,
                both.clone(),
                without_earlier,
                base_key(keys, &both),
            ),
        ],
    )
}

/// The check of a reader that holds `held`: [`ActivityRecord::check_later_write_built_on`]
/// when the write builds on it, else [`ActivityRecord::check_later_write`].
fn later_write_check(
    held: &ActivityRecord,
    later: &ActivityRecord,
    built_on_held: bool,
) -> paykit_lib::Result<()> {
    if built_on_held {
        held.check_later_write_built_on(later)
    } else {
        held.check_later_write(later)
    }
}

/// The later writes, as paykit-lib generates them: each record as its writer writes it, and
/// the reason its reader ignores a refused write.
fn later_writes(keys: &Keys) -> Value {
    let (accepted, refused) = later_write_cases(keys);
    let case = |(name, built_on_held, held, later, key): &LaterWriteCase, taken: bool| -> Value {
        let mut out = serde_json::json!({
            "built_on_held": built_on_held,
            "collection": side_collection(held.side),
            "held": String::from_utf8(held.to_json().unwrap()).unwrap(),
            "name": name,
            "record_key": b64(key),
            "value": String::from_utf8(later.to_json().unwrap()).unwrap(),
        });
        match later_write_check(held, later, *built_on_held) {
            Ok(()) => assert!(taken, "{name}: taken"),
            Err(error) => {
                assert!(!taken, "{name}: {error}");
                out["reason"] = serde_json::json!(quoted(&error.to_string()));
            }
        }
        out
    };
    serde_json::json!({
        "accepted": accepted.iter().map(|c| case(c, true)).collect::<Vec<_>>(),
        "refused": refused.iter().map(|c| case(c, false)).collect::<Vec<_>>(),
    })
}

/// `activity_records`' later writes: both versions bind at their key, and the reader holding
/// `held` takes the accepted write and ignores the refused one, for its rule, checking as a
/// reader that holds the version the write builds on when `built_on_held` says so.
fn check_later_writes(keys: &Keys, v: &Value) {
    let m = &v["activity_records"]["later_writes"];
    let (accepted, refused) = later_write_cases(keys);
    assert_eq!(arr(m, "accepted").len(), accepted.len());
    assert_eq!(arr(m, "refused").len(), refused.len());
    for (list, taken) in [("accepted", true), ("refused", false)] {
        for case in arr(m, list) {
            let name = st(case, "name");
            let bind = |member: &str| {
                bind_activity(
                    keys,
                    st(case, member).as_bytes(),
                    ActivitySide::Payer,
                    &key(case),
                )
                .unwrap_or_else(|e| panic!("{name}: {member}: {e}"))
            };
            let (held, later) = (bind("held"), bind("value"));
            let built_on_held = case["built_on_held"].as_bool().unwrap();
            match later_write_check(&held, &later, built_on_held) {
                Ok(()) => assert!(taken, "{name}: taken"),
                Err(error) => {
                    assert!(!taken, "{name}: {error}");
                    assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
                }
            }
        }
    }
}

/// The cases paykit-lib generates in `sealed.json` (README "How the files were generated"),
/// compared with the file, and written into the set by the
/// regeneration driver (`regenerate.rs`).
pub(crate) fn generate(keys: &Keys) {
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
    let (member_accepted, member_refused) = member_cases(keys);
    let accepted: Vec<ActivityCase> = accepted
        .into_iter()
        .chain(evidence_accepted)
        .chain(member_accepted)
        .collect();
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
    refused.extend(member_refused.iter().map(|c| write(c, false)));
    for (name, side, value, key) in written_member_cases(keys) {
        let error = bind_activity(keys, &value, side, &key)
            .expect_err(name)
            .to_string();
        refused.push(serde_json::json!({
            "collection": side_collection(side),
            "name": name,
            "reason": quoted(&error),
            "record_key": b64(&key),
            "value": String::from_utf8(value).unwrap(),
        }));
    }
    let member = serde_json::json!({
        "accepted": accepted.iter().map(|c| write(c, true)).collect::<Vec<_>>(),
        "description": ACTIVITY_DESCRIPTION,
        "later_writes": later_writes(keys),
        "refused": refused,
    });
    let mut cases = vec![
        (
            Place::Member("/description"),
            serde_json::json!(DESCRIPTION),
        ),
        (Place::Member("/activity_records"), member),
        (Place::Member("/msg_ids_bounds"), msg_ids_bounds(keys)),
        (Place::Member("/allowance_records"), allowance_records(keys)),
        (
            Place::Member("/allowance_proposals"),
            pending_proposals(keys),
        ),
        (Place::Member("/settlement_marks"), settlement_marks(keys)),
        (
            Place::Member("/private_list_publications"),
            publications::cases(keys),
        ),
    ];
    cases.extend(binding_cases(keys));
    cases.extend(labelled_cases(keys));
    generated("sealed.json", &cases);
}

/// `binding_accepted`'s `allowances` record, carrying the certificates of this set as
/// [`binding_cases`] generates it.
fn allowance_record_value(keys: &Keys) -> Vec<u8> {
    let held = paykit("sealed.json");
    binding_value(keys, &arr(&held, "binding_accepted")[1]).into_bytes()
}

/// Keep each authored binding error while signing the current proposal and response.
fn binding_value(keys: &Keys, case: &Value) -> String {
    let text = recertified(keys, st(case, "value"));
    if st(case, "collection") == "payment_lists" {
        // The stored list is `messages.json`'s, as that file signs it.
        let value: Value = serde_json::from_str(&text).unwrap();
        let list = stored_message(keys, "private_payment_list");
        let held = &value["list"]["plaintext"];
        assert_eq!(
            st(held, "sender_app"),
            b64(&list.plaintext().sender_app_id())
        );
        return text
            .replace(st(held, "signature"), &b64(&list.plaintext().signature()))
            .replace(st(held, "json"), &b64(list.plaintext().json()));
    }
    if st(case, "collection") != "allowances" {
        return text;
    }
    let value: Value = serde_json::from_str(&text).unwrap();
    let proposal = stored_message(keys, "allowance_proposal");
    let mut response = pubky_switch_core::message::Message::parse(
        &pubky_switch_core::enc::unb64(st(&value["response"]["plaintext"], "json")).unwrap(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    response.refs = vec![proposal.hash()];
    let sender = keys.app_by_id(&response.from.app_id_text());
    let response = pubky_switch_core::message::MessagePlaintext::sign_json(
        &sender.keys,
        response.to_json().unwrap(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    let mut text = text;
    for (member, plaintext) in [("proposal", proposal.plaintext()), ("response", &response)] {
        let held = &value[member]["plaintext"];
        assert_eq!(st(held, "sender_app"), b64(&plaintext.sender_app_id()));
        text = text
            .replace(st(held, "signature"), &b64(&plaintext.signature()))
            .replace(st(held, "json"), &b64(plaintext.json()));
    }
    text
}

/// `sealed.json` with its binding cases as [`binding_cases`] generates them: the cases other
/// generators build on.
pub(crate) fn bound_sealed() -> Value {
    crate::common::with_generated("sealed.json", &binding_cases(&Keys::from_seeds()))
}

/// The Allowance the binding cases name beside the vectors' own.
const OTHER_ALLOWANCE_ID: &str = "55de1f16-1a9e-4504-bc9c-57efc3a2b38c";

/// The record key a binding case sits at, by its name: the key of its collection's natural
/// key, under the collection's fixed policy, blinded with alice's naming key.
fn binding_key(keys: &Keys, name: &str) -> [u8; 32] {
    use paykit_lib::identifiers::AllowanceId;
    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let nk = keys.identity(&alice).naming_key();
    let shop = keys.app("shop").keys.app_id_bytes();
    let cap = keys.app("cap").keys.app_id_bytes();
    let ours = AllowanceId::parse(ALLOWANCE_ID).unwrap();
    let other = AllowanceId::parse(OTHER_ALLOWANCE_ID).unwrap();
    let (collection, natural) = match name {
        "payment_lists record of bob's shop list" => {
            (Collection::PaymentLists, natural_key::payment_list(&bob, &shop))
        }
        "payment_lists: the shop's list at the key of bob's other app"
        | "payment_lists: app_id names bob's other app, at that app's key" => {
            (Collection::PaymentLists, natural_key::payment_list(&bob, &cap))
        }
        "payment_lists: payee names alice, at the key of alice and the shop's App ID, holding the list bob sent" => {
            (Collection::PaymentLists, natural_key::payment_list(&alice, &shop))
        }
        "allowances record: bob's proposal, alice's acceptance"
        | "allowances: an acceptance naming another Allowance" => {
            (Collection::Allowances, natural_key::allowance(&bob, &ours))
        }
        "allowances: the record at another Allowance's key"
        | "allowances: allowance_id names another Allowance, at its key" => {
            (Collection::Allowances, natural_key::allowance(&bob, &other))
        }
        "allowance_ledgers record of that Allowance, empty"
        | "allowance_ledgers: counterparty names alice herself, at the Allowance's ledger key" => {
            (Collection::AllowanceLedgers, natural_key::allowance(&bob, &ours))
        }
        "allowance_ledgers: the ledger at another Allowance's ledger key" => {
            (Collection::AllowanceLedgers, natural_key::allowance(&bob, &other))
        }
        other => panic!("no record key for the binding case {other}"),
    };
    collection.record_key(&nk, &natural, None).unwrap()
}

/// `binding_accepted` and `binding_rejections`: each value as the file holds it, carrying
/// the certificates of this set ([`recertified`]): for the executor's stored messages,
/// Paykit's own executor certificate (`documents.json` `executor_certificates`); and each
/// record key ([`binding_key`]).
fn binding_cases(keys: &Keys) -> Vec<(Place<'static>, Value)> {
    let file = paykit("sealed.json");
    let mut out = Vec::new();
    for list in ["binding_accepted", "binding_rejections"] {
        for (i, case) in arr(&file, list).iter().enumerate() {
            let at = format!("/{list}/{i}");
            out.push((
                Place::At(format!("{at}/value")),
                Value::String(binding_value(keys, case)),
            ));
            out.push((
                Place::At(format!("{at}/record_key")),
                Value::String(b64(&binding_key(keys, st(case, "name")))),
            ));
        }
    }
    out
}

/// The seed label of the nonce `labelled` is sealed with.
const LABELLED_NONCE_LABEL: &str = "sealed/labelled/nonce";

/// The label of each `labelled` rejection, by its name: the policy it is read under.
fn labelled_rejection_label(name: &str) -> Option<Label> {
    match name {
        "the labelled request's record under another label's policy (paykit_request:shop2)" => {
            Some(Label::parse("shop2").unwrap())
        }
        "the labelled request's record under the fixed policy" => None,
        "a record under paykit_request:shop1 holding an unlabelled request (messages.json payment_request)" => {
            Some(Label::parse("shop1").unwrap())
        }
        other => panic!("no label for the labelled rejection {other}"),
    }
}

/// `labelled` (Paykit wire 5.1, 5.3): bob's payee record of the labelled request, from its
/// authored plaintext, epoch and nonce: the label key, the policy, the record key, the AAD
/// and the sealed value; and each rejection's policy and key, from its label.
fn labelled_cases(keys: &Keys) -> Vec<(Place<'static>, Value)> {
    use serde_json::json;
    let file = paykit("sealed.json");
    let l = &file["labelled"];
    let bob = pubky(BOB);
    let identity = keys.identity(&bob);
    let nk = identity.naming_key();
    let epoch = l["data_epoch"].as_u64().unwrap();
    let shop1 = Label::parse(crate::common::SHOP1).unwrap();
    let scope = shop1.scope();
    let label_key = identity.scope_key(epoch, &scope);
    let policy = Collection::PayeeActivity.policy(Some(&shop1));
    let natural = natural_key::request_record(
        &bob,
        &request_hash(&message_json("payment_request_labelled")),
    );
    let record_key = Collection::PayeeActivity
        .record_key(&nk, &natural, Some(&shop1))
        .unwrap();
    let plaintext = recertified(keys, st(l, "plaintext"));
    // The nonce is the seed of its label, cut to the nonce's length (README "Seeds").
    let nonce: [u8; 24] = crate::common::seed(LABELLED_NONCE_LABEL)[..24]
        .try_into()
        .unwrap();
    let sealed = SealedValue::seal_with_nonce(
        &label_key,
        &bob,
        epoch,
        &scope,
        &record_key,
        plaintext.as_bytes(),
        &nonce,
    )
    .unwrap();
    let mut out = vec![
        (Place::At("/labelled/nonce_hex".into()), json!(hex(&nonce))),
        (Place::At("/labelled/identity_hex".into()), json!(hex(&bob))),
        (
            Place::At("/labelled/label_key_hex".into()),
            json!(hex(&label_key)),
        ),
        (
            Place::At("/labelled/read_scope".into()),
            json!(policy.read_scope),
        ),
        (
            Place::At("/labelled/write_scopes".into()),
            json!(policy.write_scopes),
        ),
        (
            Place::At("/labelled/policy_hash".into()),
            json!(b64(&policy.hash().unwrap())),
        ),
        (
            Place::At("/labelled/record_key".into()),
            json!(b64(&record_key)),
        ),
        (
            Place::At("/labelled/aad_hex".into()),
            json!(hex(&aad(&bob, &scope, &record_key))),
        ),
        (Place::At("/labelled/plaintext".into()), json!(plaintext)),
        (
            Place::At("/labelled/padded_len".into()),
            json!(pad(plaintext.as_bytes()).unwrap().len()),
        ),
        (
            Place::At("/labelled/sealed".into()),
            serde_json::to_value(&sealed).unwrap(),
        ),
    ];
    for (i, case) in arr(l, "rejections").iter().enumerate() {
        let label = labelled_rejection_label(st(case, "name"));
        let policy = Collection::PayeeActivity.policy(label.as_ref());
        let value = recertified(keys, st(case, "value"));
        let read = ActivityRecord::read(value.as_bytes(), &bob, ActivitySide::Payee).unwrap();
        let at = format!("/labelled/rejections/{i}");
        out.extend([
            (
                Place::At(format!("{at}/read_scope")),
                json!(policy.read_scope),
            ),
            (
                Place::At(format!("{at}/write_scopes")),
                json!(policy.write_scopes),
            ),
            (
                Place::At(format!("{at}/policy_hash")),
                json!(b64(&policy.hash().unwrap())),
            ),
            (
                Place::At(format!("{at}/record_key")),
                json!(b64(&read.record_key(&bob, &nk, label.as_ref()).unwrap())),
            ),
            (Place::At(format!("{at}/value")), json!(value)),
        ]);
    }
    out
}

/// `activity_records` (Paykit wire 5.3): each record binds at its key, or is refused, as its
/// list says; the accepted ones write back byte for byte.
fn check_activity_records(keys: &Keys, v: &Value) {
    let m = &v["activity_records"];
    let (accepted, refused) = activity_cases(keys);
    let (member_accepted, member_refused) = member_cases(keys);
    assert_eq!(
        arr(m, "accepted").len(),
        accepted.len() + evidence_cases(keys).0.len() + member_accepted.len()
    );
    assert_eq!(
        arr(m, "refused").len(),
        refused.len()
            + unauthorized_stored(keys).len()
            + evidence_cases(keys).1.len()
            + member_refused.len()
            + written_member_cases(keys).len()
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
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let record = ActivityRecord {
        request_hash: b.period.request_hash,
        ..b.payer_request
    };
    refuses_member(&record, "request_hash");
}

#[test]
fn test_activity_record_refuses_billing_period_outside_a_period_record() {
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let record = ActivityRecord {
        billing_period: b.period.billing_period,
        ..b.payer_request
    };
    refuses_member(&record, "billing_period");
}

#[test]
fn test_activity_record_refuses_attempt_id_outside_an_unrequested_record() {
    let keys = Keys::from_seeds();
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
    let keys = Keys::from_seeds();
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
    let keys = Keys::from_seeds();
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
    let v = bound_sealed();
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
    let keys = Keys::from_seeds();
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
            "a stored request labelled shop2 from the hosted app, which holds only paykit_request:shop1",
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

/// Alice's payer request record holding `paykit_request`, as JSON, and the key it would sit at.
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
    let keys = Keys::from_seeds();
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
    let keys = Keys::from_seeds();
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
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let v = bound_sealed();
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
    resign_with(keys, base, |m| {
        if let Some(refs) = refs {
            m.refs = refs;
        }
        body(&mut m.body);
    })
}

/// A message of `messages.json` changed by `change` and signed by the sender app it then
/// names, stored as its recipient stores it.
fn resign_with(
    keys: &Keys,
    base: &str,
    change: impl FnOnce(&mut pubky_switch_core::message::Message),
) -> paykit_lib::records::StoredMessage {
    use pubky_switch_core::message::MessagePlaintext;
    let mut m = pubky_switch_core::message::Message::parse(
        &message_json(base),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    change(&mut m);
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

const ALLOWANCE_RECORDS_DESCRIPTION: &str = "Wire 4.16, 5.4: allowances record values of \
alice's, built from binding_accepted's record and messages.json's Allowance messages, some \
signed again by their sender with refs or sides changed, at the record key binding_accepted \
gives. A response refers to the proposal; an end refers to the proposal and the stored \
acceptance; an end referring to the proposal alone is its sender's withdrawal, before any \
response; a rejected Allowance has no end; executor_app_id comes only in an acceptance, from \
the allower; conflicted is a second proposal under the record's allowance_id, from either \
side, with valid terms and another hash than the record's proposal, or, beside the record's \
response, a second acceptance or rejection from the proposal's recipient, referring to the \
proposal, with another hash than the record's response (spec 8.3). accepted are \
records a reader binds; refused are those it refuses, the last two records whose stored \
message check 10 drops: a rejection carrying executor_app_id, and a conflicting proposal \
whose terms aren't valid. Among them, an Allowance handed on from alice's executor to her \
wallet, its grant history with both grants, and the executor's ledger_handoff for the \
wallet's grant, which a reader binds, and the same ledger_handoff naming a ledger version no \
write gives, signed by the executor over that version, so it fails only that rule, which it \
binds too, the ledger_handoff counting for nothing. reason is paykit-lib's wording.";

/// A case of `allowance_records`: its name and the record.
type AllowanceCase = (&'static str, AllowanceRecord);

/// The cases of `allowance_records`, accepted then refused.
fn allowance_cases(keys: &Keys) -> (Vec<AllowanceCase>, Vec<AllowanceCase>) {
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(keys), &alice).unwrap();
    let proposal = base.proposal.hash();
    let acceptance = base.response.as_ref().unwrap().hash();
    let end = stored_message(keys, "allowance_end");
    let end_with = |refs: Vec<[u8; 32]>| resigned(keys, "allowance_end", Some(refs), |_| {});
    let withdrawal = resign_with(keys, "allowance_end", |m| {
        m.from = pubky_switch_core::message::Sender {
            identity: pubky(BOB),
            app_id: keys.app("shop").keys.app_id_bytes(),
        };
        m.to = alice;
        m.refs = vec![proposal];
    });
    // messages.json's rejection answers another proposal: this one answers the record's.
    let rejection = resigned(keys, "allowance_rejection", Some(vec![proposal]), |b| {
        b.insert("allowance_id".into(), serde_json::json!(ALLOWANCE_ID));
    });
    let accepted = vec![
        (
            "the accepted Allowance with its end",
            AllowanceRecord {
                end: Some(end),
                ..base.clone()
            },
        ),
        (
            "a proposal withdrawn by its sender",
            AllowanceRecord {
                response: None,
                end: Some(withdrawal),
                ..base.clone()
            },
        ),
    ];
    let refused = vec![
        (
            "an end referring to the proposal alone after the acceptance",
            AllowanceRecord {
                end: Some(end_with(vec![proposal])),
                ..base.clone()
            },
        ),
        (
            "an end referring to another acceptance",
            AllowanceRecord {
                end: Some(end_with(vec![proposal, [7; 32]])),
                ..base.clone()
            },
        ),
        (
            "an end after a rejection",
            AllowanceRecord {
                response: Some(rejection.clone()),
                end: Some(end_with(vec![proposal, rejection.hash()])),
                ..base.clone()
            },
        ),
        (
            "an end referring to an acceptance the record doesn't hold",
            AllowanceRecord {
                response: None,
                end: Some(end_with(vec![proposal, acceptance])),
                ..base.clone()
            },
        ),
        (
            "a withdrawal by the proposal's recipient",
            AllowanceRecord {
                response: None,
                end: Some(end_with(vec![proposal])),
                ..base.clone()
            },
        ),
    ];
    let mut refused = refused;
    // A second, different proposal under the record's allowance_id makes the Allowance
    // conflicted (Paykit spec 8.3, Paykit wire 5.4): from the counterparty, here with other
    // terms, or from this identity's own app, crossing the counterparty's.
    let second = resign_with(keys, "allowance_proposal", |m| {
        m.msg_id = SECOND_PROPOSAL.into();
        m.body
            .get_mut("terms")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("lifetime_amount_limit".into(), serde_json::json!("0.20"));
    });
    let crossing = resign_with(keys, "allowance_proposal", |m| {
        m.msg_id = CROSSING_PROPOSAL.into();
        m.from = pubky_switch_core::message::Sender {
            identity: alice,
            app_id: keys.app("phone").keys.app_id_bytes(),
        };
        m.to = pubky(BOB);
        m.body
            .insert("proposer_role".into(), serde_json::json!("allower"));
    });
    let mut accepted = accepted;
    accepted.extend([
        (
            "an accepted Allowance conflicted by a second proposal from the counterparty",
            AllowanceRecord {
                conflicted: Some(second),
                ..base.clone()
            },
        ),
        (
            "a proposal conflicted by a crossing proposal from this identity",
            AllowanceRecord {
                response: None,
                conflicted: Some(crossing),
                ..base.clone()
            },
        ),
    ]);
    // executor_app_id only in an acceptance from the allower (Paykit wire 4.16).
    let allower_proposal = resigned(keys, "allowance_proposal", None, |b| {
        b.insert("proposer_role".into(), serde_json::json!("allower"));
    });
    let from_allowee = resigned(
        keys,
        "allowance_acceptance",
        Some(vec![allower_proposal.hash()]),
        |_| {},
    );
    refused.push((
        "an acceptance from the allowee carrying executor_app_id",
        AllowanceRecord {
            proposal: allower_proposal,
            response: Some(from_allowee),
            ..base.clone()
        },
    ));
    let other_allowance = resign_with(keys, "allowance_proposal", |m| {
        m.msg_id = SECOND_PROPOSAL.into();
        m.body.insert(
            "allowance_id".into(),
            serde_json::json!("55de1f16-1a9e-4504-bc9c-57efc3a2b38c"),
        );
    });
    refused.extend([
        (
            "conflicted holding the record's own proposal",
            AllowanceRecord {
                conflicted: Some(base.proposal.clone()),
                ..base.clone()
            },
        ),
        (
            "conflicted holding the record's own acceptance",
            AllowanceRecord {
                conflicted: base.response.clone(),
                ..base.clone()
            },
        ),
        (
            "a conflicting proposal naming another Allowance",
            AllowanceRecord {
                conflicted: Some(other_allowance),
                ..base.clone()
            },
        ),
    ]);
    let (more_accepted, more_refused) = conflicting_response_cases(keys, &base, rejection);
    accepted.extend(more_accepted);
    refused.extend(more_refused);
    let handed = handed_on(keys, &base);
    let no_write = handed_on_naming_no_write(keys, &handed);
    accepted.push((
        "an Allowance handed on from the executor to the wallet, with the executor's ledger_handoff",
        handed,
    ));
    accepted.push((
        "an Allowance handed on with a ledger_handoff naming a ledger version no write gives, which counts for nothing",
        no_write,
    ));
    (accepted, refused)
}

/// The cases of `allowance_records` whose `conflicted` holds a second response (Paykit spec
/// 8.3, Paykit wire 5.4): beside the record's acceptance, its proposal's recipient's
/// rejection, or another acceptance; and the responses a reader refuses there.
fn conflicting_response_cases(
    keys: &Keys,
    base: &AllowanceRecord,
    rejection: paykit_lib::records::StoredMessage,
) -> (Vec<AllowanceCase>, Vec<AllowanceCase>) {
    let proposal = base.proposal.hash();
    let acceptance = base.response.as_ref().unwrap().hash();
    let other_acceptance = resign_with(keys, "allowance_acceptance", |m| {
        m.msg_id = SECOND_RESPONSE.into();
    });
    let accepted = vec![
        (
            "an accepted Allowance conflicted by a rejection after the acceptance",
            AllowanceRecord {
                conflicted: Some(rejection.clone()),
                ..base.clone()
            },
        ),
        (
            "an accepted Allowance conflicted by another acceptance",
            AllowanceRecord {
                conflicted: Some(other_acceptance.clone()),
                ..base.clone()
            },
        ),
    ];
    let other_allowance = resign_with(keys, "allowance_acceptance", |m| {
        m.msg_id = SECOND_RESPONSE.into();
        m.body.insert(
            "allowance_id".into(),
            serde_json::json!("55de1f16-1a9e-4504-bc9c-57efc3a2b38c"),
        );
    });
    let from_proposer = resign_with(keys, "allowance_acceptance", |m| {
        m.msg_id = SECOND_RESPONSE.into();
        m.from = pubky_switch_core::message::Sender {
            identity: pubky(BOB),
            app_id: keys.app("shop").keys.app_id_bytes(),
        };
        m.to = pubky(ALICE);
        m.body.remove("executor_app_id");
    });
    let other_proposal = resign_with(keys, "allowance_acceptance", |m| {
        m.msg_id = SECOND_RESPONSE.into();
        m.refs = vec![[7; 32]];
    });
    // executor_app_id only in an acceptance from the allower (Paykit wire 4.16), in a
    // conflicting acceptance too: here the proposer is the allower.
    let allower_proposal = resigned(keys, "allowance_proposal", None, |b| {
        b.insert("proposer_role".into(), serde_json::json!("allower"));
    });
    let from_allowee = |msg_id: &str, executor: bool| {
        resign_with(keys, "allowance_acceptance", |m| {
            m.msg_id = msg_id.into();
            m.refs = vec![allower_proposal.hash()];
            if !executor {
                m.body.remove("executor_app_id");
            }
        })
    };
    let mut refused = vec![
        (
            "a conflicting response naming another Allowance",
            AllowanceRecord {
                conflicted: Some(other_allowance),
                ..base.clone()
            },
        ),
        (
            "a conflicting response where the record holds no response",
            AllowanceRecord {
                response: None,
                conflicted: Some(other_acceptance),
                ..base.clone()
            },
        ),
        (
            "a conflicting response from the proposal's sender",
            AllowanceRecord {
                conflicted: Some(from_proposer),
                ..base.clone()
            },
        ),
        (
            "a conflicting response referring to another proposal",
            AllowanceRecord {
                conflicted: Some(other_proposal),
                ..base.clone()
            },
        ),
        (
            "a conflicting acceptance from the allowee carrying executor_app_id",
            AllowanceRecord {
                proposal: allower_proposal.clone(),
                response: Some(from_allowee("250b9ef2-c357-4856-b8c0-863368e627b5", false)),
                conflicted: Some(from_allowee(SECOND_RESPONSE, true)),
                ..base.clone()
            },
        ),
    ];
    // An end isn't a response: a second end is processed, not stored (Paykit spec 8.3).
    refused.push((
        "conflicted holding an end",
        AllowanceRecord {
            conflicted: Some(resigned(
                keys,
                "allowance_end",
                Some(vec![proposal, acceptance]),
                |_| {},
            )),
            ..base.clone()
        },
    ));
    (accepted, refused)
}

/// The `msg_id` of the second response of `allowance_records`' conflicted records, from
/// alice's executor.
const SECOND_RESPONSE: &str = "c3e5a7b9-1d2f-4e6a-8b0c-2d4f6a8c0e1b";

/// The `msg_id` of the second proposal of `allowance_records`' conflicted records, from bob.
const SECOND_PROPOSAL: &str = "0d4c2f5e-8a1b-4c3d-9e7f-6a5b4c3d2e1f";

/// The `msg_id` of the proposal alice's phone sends under bob's proposal's `allowance_id`.
const CROSSING_PROPOSAL: &str = "5b1e7c9a-2d4f-4a6b-8c0e-1f3a5c7e9b2d";

/// A record whose rejection carries `executor_app_id`, which check 10 drops (Paykit wire
/// 4.16): the stored message can't be built, so the case is the record's JSON with the
/// rejection signed as its sender signs it.
fn rejection_with_executor(keys: &Keys) -> (Vec<u8>, [u8; 32]) {
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(keys), &alice).unwrap();
    record_with_unstorable(keys, "allowance_rejection", "response", |m| {
        m.refs = vec![base.proposal.hash()];
        m.body
            .insert("allowance_id".into(), serde_json::json!(ALLOWANCE_ID));
        m.body.insert(
            "executor_app_id".into(),
            serde_json::json!(keys.app("executor").app_id),
        );
    })
}

/// A record whose conflicting proposal has terms that aren't valid, a minimum above the
/// maximum, which check 10 drops (Paykit wire 4.15, 5.4), built as
/// [`rejection_with_executor`] builds its case.
fn conflicted_with_invalid_terms(keys: &Keys) -> (Vec<u8>, [u8; 32]) {
    record_with_unstorable(keys, "allowance_proposal", "conflicted", |m| {
        m.msg_id = SECOND_PROPOSAL.into();
        let range = m
            .body
            .get_mut("terms")
            .and_then(|t| t.get_mut("per_payment_amount"))
            .and_then(Value::as_object_mut)
            .unwrap();
        range.insert("minimum".into(), serde_json::json!("0.02"));
    })
}

/// `binding_accepted`'s `allowances` record with `member` holding `base` changed by `change`
/// and signed as its sender signs it: a message no stored message can hold, so the case is
/// the record's JSON. Returns it with the record's key.
fn record_with_unstorable(
    keys: &Keys,
    base: &str,
    member: &str,
    change: impl FnOnce(&mut pubky_switch_core::message::Message),
) -> (Vec<u8>, [u8; 32]) {
    use pubky_switch_core::message::MessagePlaintext;
    let s = bound_sealed();
    let accepted = &arr(&s, "binding_accepted")[1];
    let mut m = pubky_switch_core::message::Message::parse(
        &message_json(base),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    change(&mut m);
    let json = m.to_json().unwrap();
    let sender = keys.app_by_id(&m.from.app_id_text());
    let plaintext = MessagePlaintext::sign_json(
        &sender.keys,
        json.clone(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    let stored = serde_json::json!({
        "accepted_at": null,
        "cert": signed_json_doc(&crate::common::cert_doc(&sender.name)),
        "plaintext": {
            "json": b64(&json),
            "sender_app": b64(&m.from.app_id),
            "signature": b64(&plaintext.signature()),
        },
        "sender": pubky_switch_core::enc::pubky_id(&sender.identity),
    });
    let mut record: Value = serde_json::from_slice(value(accepted)).unwrap();
    record[member] = stored;
    (serde_json::to_vec(&record).unwrap(), key(accepted))
}

fn signed_json_doc(doc: &pubky_switch_core::sig::SignedDoc) -> Value {
    serde_json::json!({ "body": doc.body, "sig": doc.sig })
}

/// The certificate of alice's wallet holding the executor's scopes, signed again by her
/// identity key, so the wallet may take a grant.
fn wallet_executor_cert(keys: &Keys) -> AppCert {
    let scopes = crate::common::paykit_signed_scopes("executor").unwrap();
    let names: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
    crate::common::reissued(
        keys,
        "wallet",
        scopes,
        &paykit_lib::scopes::permissions_for(&names),
    )
}

/// `base` with its grant history: alice's executor's grant on the record at `{1, 1}`, then
/// her wallet's at `{1, 3}`, which hands the Allowance on; and the executor's
/// `ledger_handoff` for the wallet's grant, naming the ledger write at `{1, 2}` (Paykit wire
/// 5.4, "Ledger handoff").
fn handed_on(keys: &Keys, base: &AllowanceRecord) -> AllowanceRecord {
    use paykit_lib::grant::{ExecutorGrant, LedgerHandoff};
    use paykit_lib::records::GrantEntry;
    use pubky_switch_core::ops::RecordVersion;
    let alice = pubky(ALICE);
    let terms = base.terms_hash().unwrap();
    let at = |version| RecordVersion {
        generation: 1,
        version,
    };
    let entry = |app: &str, cert: AppCert, version| {
        let grant = ExecutorGrant::sign(
            &keys.app(app).keys,
            cert.signed().clone(),
            &alice,
            &base.counterparty,
            &base.allowance_id,
            terms,
            1,
            at(version),
        );
        GrantEntry {
            built_on: at(version),
            grant,
            origin: Default::default(),
        }
    };
    let grants = vec![
        entry("executor", crate::common::executor_cert(keys), 1),
        entry("wallet", wallet_executor_cert(keys), 3),
    ];
    let handoff = LedgerHandoff::sign(
        &keys.app("executor").keys,
        crate::common::executor_cert(keys).signed().clone(),
        &alice,
        &base.counterparty,
        &base.allowance_id,
        at(3),
        at(2),
        [9; 32],
    );
    AllowanceRecord {
        grant: grants.last().map(|entry| entry.grant.clone()),
        grants,
        ledger_handoff: Some(handoff),
        ..base.clone()
    }
}

/// `handed`, with the executor's `ledger_handoff` for the wallet's grant naming instead the
/// ledger version `{1, 0}`, which no write gives, signed over that version (Paykit wire 5.4).
/// A ledger_handoff that doesn't read doesn't affect whether the record is valid: the record
/// binds, and it counts for nothing. Everything but that rule holds: the signature verifies
/// over the version named, with the same certificate and grant as `handed`'s, so a reader
/// that misses the rule takes it.
fn handed_on_naming_no_write(keys: &Keys, handed: &AllowanceRecord) -> AllowanceRecord {
    use paykit_lib::grant::{handoff_fields, LedgerHandoff, LEDGER_HANDOFF_LABEL};
    use pubky_switch_core::ops::RecordVersion;
    let alice = pubky(ALICE);
    let valid = handed.ledger_handoff.clone().unwrap();
    let no_write = RecordVersion {
        generation: 1,
        version: 0,
    };
    let signed = LedgerHandoff::sign(
        &keys.app("executor").keys,
        valid.cert.clone(),
        &alice,
        &handed.counterparty,
        &handed.allowance_id,
        valid.grant,
        no_write,
        valid.item_hash,
    );
    let fields = handoff_fields(
        &alice,
        &handed.counterparty,
        &handed.allowance_id,
        valid.grant,
        no_write,
        &valid.item_hash,
    );
    let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
    pubky_switch_core::sig::verify(
        &keys.app("executor").keys.signing_public(),
        LEDGER_HANDOFF_LABEL,
        &refs,
        &signed.sig,
    )
    .unwrap();
    let mut value: Value = serde_json::from_slice(&handed.to_json().unwrap()).unwrap();
    value["ledger_handoff"] = signed.to_json();
    let record = AllowanceRecord::read(&serde_json::to_vec(&value).unwrap(), &alice).unwrap();
    // It counts for nothing for that rule alone.
    let unread = record.ledger_handoff_unread.clone().unwrap();
    assert!(record.ledger_handoff.is_none());
    assert_eq!(
        LedgerHandoff::from_json(&unread).unwrap_err().to_string(),
        "invalid: Paykit wire 5.4: ledger_handoff names a ledger version a write gives"
    );
    assert!(record.ledger_handoff_counts(&alice).is_err());
    record
}

#[test]
fn test_ledger_handoff_binds_and_writes_back() {
    // A record with a ledger_handoff binds at its key and writes it back as read, an unknown
    // member in it included; an unread handoff is retained without invalidating the record.
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let nk = keys.identity(&alice).naming_key();
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let key = handed.record_key(&nk).unwrap();
    let json = handed.to_json().unwrap();
    let read = AllowanceRecord::bind(&json, &alice, &nk, &key).unwrap();
    assert_eq!(read.ledger_handoff, handed.ledger_handoff);
    assert_eq!(read.to_json().unwrap(), json);
    let mut value: Value = serde_json::from_slice(&json).unwrap();
    value["ledger_handoff"]["note"] = serde_json::json!({ "by": "a later version" });
    let with_unknown = serde_json::to_vec(&value).unwrap();
    let read = AllowanceRecord::bind(&with_unknown, &alice, &nk, &key).unwrap();
    let written: Value = serde_json::from_slice(&read.to_json().unwrap()).unwrap();
    assert_eq!(written["ledger_handoff"]["note"]["by"], "a later version");
    // A later handoff's replaces it: the replacement gets none of its members.
    let mut replaced = read.clone();
    if let Some(handoff) = replaced.ledger_handoff.as_mut() {
        *handoff = paykit_lib::grant::LedgerHandoff {
            origin: Default::default(),
            ..handoff.clone()
        };
    }
    let written: Value = serde_json::from_slice(&replaced.to_json().unwrap()).unwrap();
    assert!(written["ledger_handoff"].get("note").is_none());
    // One that doesn't read leaves the record valid: it binds, counts for nothing, and is
    // written back as read; a reader holding the record without it flags the write.
    value["ledger_handoff"]["item_hash"] = serde_json::json!(b64(&[9; 31]));
    let bad = serde_json::to_vec(&value).unwrap();
    let read = AllowanceRecord::bind(&bad, &alice, &nk, &key).unwrap();
    assert!(read.ledger_handoff.is_none());
    assert!(read.ledger_handoff_counts(&alice).is_err());
    let written: Value = serde_json::from_slice(&read.to_json().unwrap()).unwrap();
    assert_eq!(written["ledger_handoff"], value["ledger_handoff"]);
    let held = AllowanceRecord {
        ledger_handoff: None,
        ledger_handoff_unread: None,
        ..handed.clone()
    };
    assert!(held.check_later_handoff(&read, &alice).is_err());
}

fn handoff_with_unknown_versions() -> (Keys, AllowanceRecord, Value) {
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let mut value: Value = serde_json::from_slice(&handed.to_json().unwrap()).unwrap();
    value["ledger_handoff"]["grant"]["note"] = serde_json::json!({"grant": [1, 2, 3]});
    value["ledger_handoff"]["ledger"]["note"] = serde_json::json!({"ledger": [4, 5, 6]});
    value["ledger_handoff"]["cert"]["note"] = serde_json::json!({"certificate": true});
    value["ledger_handoff"]["note"] = serde_json::json!({"handoff": true});
    let nk = keys.identity(&alice).naming_key();
    let key = handed.record_key(&nk).unwrap();
    let read =
        AllowanceRecord::bind(&serde_json::to_vec(&value).unwrap(), &alice, &nk, &key).unwrap();
    assert!(read.ledger_handoff_counts(&alice).unwrap().is_some());
    (keys, read, value)
}

#[test]
fn test_ledger_handoff_preserves_unknown_grant_version_members() {
    let (_, read, value) = handoff_with_unknown_versions();
    let written: Value = serde_json::from_slice(&read.to_json().unwrap()).unwrap();
    assert_eq!(
        written["ledger_handoff"]["grant"],
        value["ledger_handoff"]["grant"]
    );
}

#[test]
fn test_ledger_handoff_preserves_unknown_ledger_version_members() {
    let (_, read, value) = handoff_with_unknown_versions();
    let written: Value = serde_json::from_slice(&read.to_json().unwrap()).unwrap();
    assert_eq!(
        written["ledger_handoff"]["ledger"],
        value["ledger_handoff"]["ledger"]
    );
}

#[test]
fn test_ledger_handoff_cloned_copy_preserves_all_unknown_members() {
    let (_, read, value) = handoff_with_unknown_versions();
    let copied = read.clone();
    let json = copied.to_json().unwrap();
    let written: Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(written["ledger_handoff"], value["ledger_handoff"]);
    let reread = AllowanceRecord::read(&json, &pubky(ALICE)).unwrap();
    assert!(reread
        .ledger_handoff_counts(&pubky(ALICE))
        .unwrap()
        .is_some());
    assert_eq!(reread.to_json().unwrap(), json);
}

#[test]
fn test_ledger_handoff_replacement_does_not_inherit_nested_members() {
    let (keys, read, _) = handoff_with_unknown_versions();
    let (replacement, _) = handed_back(&keys, &read);
    read.check_later_handoff(&replacement, &pubky(ALICE))
        .unwrap();
    let json = replacement.to_json().unwrap();
    let written: Value = serde_json::from_slice(&json).unwrap();
    for pointer in [
        "/ledger_handoff",
        "/ledger_handoff/grant",
        "/ledger_handoff/ledger",
        "/ledger_handoff/cert",
    ] {
        assert!(
            written.pointer(pointer).unwrap().get("note").is_none(),
            "{pointer}"
        );
    }
    let reread = AllowanceRecord::read(&json, &pubky(ALICE)).unwrap();
    assert!(reread
        .ledger_handoff_counts(&pubky(ALICE))
        .unwrap()
        .is_some());
}

#[test]
fn test_ledger_handoff_unread_preserves_nested_version_members() {
    let (keys, read, _) = handoff_with_unknown_versions();
    let unread = handed_on_naming_no_write(&keys, &read);
    let mut value: Value = serde_json::from_slice(&unread.to_json().unwrap()).unwrap();
    value["ledger_handoff"]["grant"]["note"] = serde_json::json!({"grant": [1, 2, 3]});
    value["ledger_handoff"]["ledger"]["note"] = serde_json::json!({"ledger": [4, 5, 6]});
    let nk = keys.identity(&pubky(ALICE)).naming_key();
    let key = read.record_key(&nk).unwrap();
    let read = AllowanceRecord::bind(
        &serde_json::to_vec(&value).unwrap(),
        &pubky(ALICE),
        &nk,
        &key,
    )
    .unwrap();
    assert!(read.ledger_handoff.is_none());
    assert!(read.ledger_handoff_unread.is_some());
    assert!(read.ledger_handoff_counts(&pubky(ALICE)).is_err());
    let written: Value = serde_json::from_slice(&read.to_json().unwrap()).unwrap();
    assert_eq!(written["ledger_handoff"], value["ledger_handoff"]);
}

#[test]
fn test_ledger_handoff_counts_only_signed_by_the_previous_executor() {
    // The executor's handoff to the wallet counts; the same handoff signed by the wallet, or
    // altered after it was signed, or naming the first grant, doesn't verify and is flagged by
    // a reader holding the record without it; once another grant of the executor follows,
    // the handoff to the wallet counts for nothing.
    use paykit_lib::grant::LedgerHandoff;
    use pubky_switch_core::ops::RecordVersion;
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let handoff = handed.ledger_handoff.clone().unwrap();
    assert_eq!(
        handed.ledger_handoff_counts(&alice).unwrap(),
        Some(&handoff)
    );
    let held = AllowanceRecord {
        ledger_handoff: None,
        ledger_handoff_unread: None,
        ..handed.clone()
    };
    held.check_later_handoff(&handed, &alice).unwrap();
    let at = |version| RecordVersion {
        generation: 1,
        version,
    };
    let by_wallet = LedgerHandoff::sign(
        &keys.app("wallet").keys,
        wallet_executor_cert(&keys).signed().clone(),
        &alice,
        &base.counterparty,
        &base.allowance_id,
        at(3),
        at(2),
        [9; 32],
    );
    let altered = LedgerHandoff {
        item_hash: [8; 32],
        ..handoff.clone()
    };
    let first = LedgerHandoff::sign(
        &keys.app("executor").keys,
        crate::common::executor_cert(&keys).signed().clone(),
        &alice,
        &base.counterparty,
        &base.allowance_id,
        at(1),
        at(2),
        [9; 32],
    );
    for (name, wrong) in [
        ("by the wallet", by_wallet),
        ("altered", altered),
        ("the first grant", first),
    ] {
        let later = AllowanceRecord {
            ledger_handoff: Some(wrong),
            ..handed.clone()
        };
        assert!(later.ledger_handoff_counts(&alice).is_err(), "{name}");
        assert!(held.check_later_handoff(&later, &alice).is_err(), "{name}");
        // A write keeping the one it holds isn't flagged.
        later.check_later_handoff(&later, &alice).unwrap();
    }
    // Handed back to the executor: the handoff to the wallet is of an earlier handoff.
    let mut back = handed.clone();
    let grant = paykit_lib::grant::ExecutorGrant::sign(
        &keys.app("executor").keys,
        crate::common::executor_cert(&keys).signed().clone(),
        &alice,
        &base.counterparty,
        &base.allowance_id,
        base.terms_hash().unwrap(),
        1,
        at(5),
    );
    back.grants.push(paykit_lib::records::GrantEntry {
        built_on: at(5),
        grant: grant.clone(),
        origin: Default::default(),
    });
    back.grant = Some(grant);
    assert_eq!(back.ledger_handoff_counts(&alice).unwrap(), None);
}

/// `handed`, handed back to the executor by its grant at `{1, 5}`, with the wallet's
/// `ledger_handoff` for that grant, naming the ledger write at `{1, 4}`; and the executor's
/// handoff to the wallet, which verifies where it sits but no longer counts.
fn handed_back(
    keys: &Keys,
    handed: &AllowanceRecord,
) -> (AllowanceRecord, paykit_lib::grant::LedgerHandoff) {
    use paykit_lib::grant::{ExecutorGrant, LedgerHandoff};
    use pubky_switch_core::ops::RecordVersion;
    let alice = pubky(ALICE);
    let at = |version| RecordVersion {
        generation: 1,
        version,
    };
    let grant = ExecutorGrant::sign(
        &keys.app("executor").keys,
        crate::common::executor_cert(keys).signed().clone(),
        &alice,
        &handed.counterparty,
        &handed.allowance_id,
        handed.terms_hash().unwrap(),
        1,
        at(5),
    );
    let mut back = handed.clone();
    back.grants.push(paykit_lib::records::GrantEntry {
        built_on: at(5),
        grant: grant.clone(),
        origin: Default::default(),
    });
    back.grant = Some(grant);
    let to_wallet = handed.ledger_handoff.clone().unwrap();
    let by_wallet = LedgerHandoff::sign(
        &keys.app("wallet").keys,
        wallet_executor_cert(keys).signed().clone(),
        &alice,
        &handed.counterparty,
        &handed.allowance_id,
        at(5),
        at(4),
        [8; 32],
    );
    back.ledger_handoff = Some(by_wallet);
    assert!(back.ledger_handoff_counts(&alice).unwrap().is_some());
    assert!(back.check_ledger_handoff(&alice, &to_wallet).is_ok());
    (back, to_wallet)
}

#[test]
fn test_ledger_handoff_later_write_never_drops_one_seen() {
    // A later write keeps the ledger_handoff seen, parsed or not read, as read: one that
    // leaves it out is flagged (Paykit wire 5.4).
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let without = AllowanceRecord {
        ledger_handoff: None,
        ledger_handoff_unread: None,
        ..handed.clone()
    };
    let error = handed.check_later_handoff(&without, &alice).unwrap_err();
    assert!(
        error.to_string().contains("drops the ledger_handoff"),
        "{error}"
    );
    let unread = handed_on_naming_no_write(&keys, &handed);
    let error = unread.check_later_handoff(&without, &alice).unwrap_err();
    assert!(
        error.to_string().contains("drops the ledger_handoff"),
        "{error}"
    );
    // Kept as read, it passes.
    handed.check_later_handoff(&handed, &alice).unwrap();
    unread.check_later_handoff(&unread, &alice).unwrap();
    without.check_later_handoff(&without, &alice).unwrap();
}

#[test]
fn test_ledger_handoff_later_write_refuses_an_earlier_grant() {
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let (back, to_wallet) = handed_back(&keys, &handed);
    let replayed = AllowanceRecord {
        ledger_handoff: Some(to_wallet),
        ..back.clone()
    };
    assert!(back.check_later_handoff(&replayed, &alice).is_err());
}

#[test]
fn test_ledger_handoff_later_write_refuses_a_changed_same_grant() {
    // A handoff is written once for its grant. Even an otherwise counting signature
    // cannot replace the seen handoff with another ledger write (Paykit wire 5.4).
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let seen = handed.ledger_handoff.as_ref().unwrap();
    for (version, item_hash) in [(1, [7; 32]), (2, [8; 32]), (3, [9; 32])] {
        let replacement = paykit_lib::grant::LedgerHandoff::sign(
            &keys.app("executor").keys,
            seen.cert.clone(),
            &alice,
            &handed.counterparty,
            &handed.allowance_id,
            seen.grant,
            pubky_switch_core::ops::RecordVersion {
                generation: 1,
                version,
            },
            item_hash,
        );
        let later = AllowanceRecord {
            ledger_handoff: Some(replacement),
            ..handed.clone()
        };
        assert!(later.ledger_handoff_counts(&alice).unwrap().is_some());
        let error = handed.check_later_handoff(&later, &alice).unwrap_err();
        assert!(
            error.to_string().contains("isn't for a later grant"),
            "{error}"
        );
    }
}

#[test]
fn test_ledger_handoff_later_write_takes_a_later_grant() {
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let (back, _) = handed_back(&keys, &handed);
    handed.check_later_handoff(&back, &alice).unwrap();
    // An unread handoff whose grant can be placed has the same ordering boundary.
    let unread = handed_on_naming_no_write(&keys, &handed);
    unread.check_later_handoff(&back, &alice).unwrap();
}

#[test]
fn test_ledger_handoff_later_write_keeps_an_unplaced_unread_handoff() {
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let (back, _) = handed_back(&keys, &handed);
    let mut unread = handed_on_naming_no_write(&keys, &handed);
    unread.ledger_handoff_unread.as_mut().unwrap()["grant"] = serde_json::json!("not a version");
    assert!(unread.check_later_handoff(&back, &alice).is_err());
    unread.check_later_handoff(&unread, &alice).unwrap();
}

#[test]
fn test_ledger_handoff_later_write_seen_after_skipped_versions_is_taken() {
    // A reader holding the executor's handoff to the wallet, or none, skips versions: the
    // wallet hands on to the executor and names its last version for that grant, then the
    // executor hands on to the wallet again. The write it reads holds the wallet's handoff,
    // for a later grant than the one seen, which no longer counts: it takes it.
    use paykit_lib::grant::ExecutorGrant;
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let (mut later, _) = handed_back(&keys, &handed);
    let built_on = pubky_switch_core::ops::RecordVersion {
        generation: 1,
        version: 7,
    };
    let grant = ExecutorGrant::sign(
        &keys.app("wallet").keys,
        wallet_executor_cert(&keys).signed().clone(),
        &alice,
        &handed.counterparty,
        &handed.allowance_id,
        handed.terms_hash().unwrap(),
        1,
        built_on,
    );
    later.grants.push(paykit_lib::records::GrantEntry {
        built_on,
        grant: grant.clone(),
        origin: Default::default(),
    });
    later.grant = Some(grant);
    assert_eq!(later.ledger_handoff_counts(&alice).unwrap(), None);
    handed.check_later_write(&later).unwrap();
    handed.check_later_handoff(&later, &alice).unwrap();
    let none_seen = AllowanceRecord {
        ledger_handoff: None,
        ..handed.clone()
    };
    none_seen.check_later_handoff(&later, &alice).unwrap();
}

#[test]
fn test_allowance_later_write_keeps_the_current_grant_the_last_entry() {
    // The current grant is the last entry of the grant history: a later write that drops it,
    // or names an earlier entry's grant, is flagged, as one appending a grant isn't (Paykit
    // wire 5.4).
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let base = AllowanceRecord::read(&allowance_record_value(&keys), &alice).unwrap();
    let handed = handed_on(&keys, &base);
    let dropped = AllowanceRecord {
        grant: None,
        ..handed.clone()
    };
    let earlier = AllowanceRecord {
        grant: Some(handed.grants[0].grant.clone()),
        ..handed.clone()
    };
    for (name, later) in [("dropped", dropped), ("earlier", earlier)] {
        let error = handed.check_later_write(&later).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("current grant isn't the last entry"),
            "{name}: {error}"
        );
    }
    handed.check_later_write(&handed).unwrap();
    let (back, _) = handed_back(&keys, &handed);
    handed.check_later_write(&back).unwrap();
}

/// Alice's phone proposes an Allowance to Bob while Bob is proposing under the same ID.
fn crossing_proposal(keys: &Keys) -> paykit_lib::records::StoredMessage {
    resign_with(keys, "allowance_proposal", |m| {
        m.msg_id = CROSSING_PROPOSAL.into();
        m.from = pubky_switch_core::message::Sender {
            identity: pubky(ALICE),
            app_id: keys.app("phone").keys.app_id_bytes(),
        };
        m.to = pubky(BOB);
        m.body
            .insert("proposer_role".into(), serde_json::json!("allower"));
    })
}

fn pending_proposals(keys: &Keys) -> Value {
    let own = pubky(ALICE);
    let nk = keys.identity(&own).naming_key();
    let incoming = stored_message(keys, "allowance_proposal");
    let outgoing = crossing_proposal(keys);
    let mut pending = PendingAllowanceRecord::new(&own, incoming).unwrap();
    let sent = PendingAllowanceRecord::new(&own, outgoing.clone()).unwrap();
    let encode = |name: &str, record: &PendingAllowanceRecord| {
        serde_json::json!({
            "name": name,
            "record_key": b64(&record.record_key(&nk).unwrap()),
            "value": String::from_utf8(record.to_json().unwrap()).unwrap(),
        })
    };
    let received = encode("the proposal Alice received from Bob", &pending);
    let allowance_key = Collection::Allowances
        .record_key(
            &nk,
            &natural_key::allowance(pending.counterparty(), pending.allowance_id()),
            None,
        )
        .unwrap();
    let mut wrong_key = received.clone();
    wrong_key["name"] = serde_json::json!("a pending proposal at its allowances key");
    wrong_key["record_key"] = serde_json::json!(b64(&allowance_key));
    let mut repeated = received.clone();
    let mut payload: Value = serde_json::from_slice(value(&received)).unwrap();
    payload["conflicted"] = payload["proposal"].clone();
    repeated["name"] = serde_json::json!("the original proposal also held as its conflict");
    repeated["value"] = serde_json::json!(serde_json::to_string(&payload).unwrap());
    assert!(pending.note_proposal(outgoing).unwrap());
    serde_json::json!({
        "description": "Wire 5.4: pending Allowance proposals held by Alice at the same ordinary record key. The crossing proposal has the same ID and different signed bytes. A reader retains the first proposal and any conflict until the pending record is deleted.",
        "accepted": [received, encode("the proposal Alice sent to Bob", &sent),
            encode("Bob's proposal with Alice's crossing proposal", &pending)],
        "refused": [wrong_key, repeated],
    })
}

fn check_pending_proposals(keys: &Keys, cases: &Value) {
    let own = pubky(ALICE);
    let nk = keys.identity(&own).naming_key();
    for case in arr(cases, "refused") {
        assert!(
            PendingAllowanceRecord::bind(value(case), &own, &nk, &key(case)).is_err(),
            "{}",
            st(case, "name")
        );
    }
    let records: Vec<_> = arr(cases, "accepted")
        .iter()
        .map(|case| {
            let record = PendingAllowanceRecord::bind(value(case), &own, &nk, &key(case)).unwrap();
            assert_eq!(record.to_json().unwrap(), value(case));
            assert_eq!(record.counterparty(), &pubky(BOB));
            assert_eq!(record.allowance_id().as_str(), ALLOWANCE_ID);
            record
        })
        .collect();
    let [received, sent, conflicted] = records.as_slice() else {
        panic!("three pending records");
    };
    assert_ne!(received.proposal().hash(), sent.proposal().hash());
    assert_eq!(
        conflicted.conflicted().unwrap().hash(),
        sent.proposal().hash()
    );
    received.check_later_write(conflicted).unwrap();
    conflicted.check_later_write(conflicted).unwrap();
    assert!(conflicted.check_later_write(received).is_err());
    assert!(received.check_later_write(sent).is_err());
    let mut again = received.clone();
    assert!(!again.note_proposal(received.proposal().clone()).unwrap());
    let third = resign_with(keys, "allowance_proposal", |m| {
        m.msg_id = SECOND_PROPOSAL.into()
    });
    assert!(again.note_proposal(third).unwrap());
    assert!(!again.note_proposal(sent.proposal().clone()).unwrap());
    assert!(conflicted.check_later_write(&again).is_err());
}

#[test]
fn test_pending_proposal_binds_its_signed_messages_and_keeps_unknown_members() {
    let keys = Keys::from_seeds();
    let own = pubky(ALICE);
    let nk = keys.identity(&own).naming_key();
    let proposal = stored_message(&keys, "allowance_proposal");
    let response = stored_message(&keys, "allowance_acceptance");
    assert!(PendingAllowanceRecord::new(&[0; 32], proposal.clone()).is_err());
    assert!(PendingAllowanceRecord::new(&own, response.clone()).is_err());
    let mut pending = PendingAllowanceRecord::new(&own, proposal.clone()).unwrap();
    assert!(pending.note_proposal(response).is_err());
    let key = pending.record_key(&nk).unwrap();
    let other_side = PendingAllowanceRecord::new(&pubky(BOB), proposal).unwrap();
    assert!(pending.check_later_write(&other_side).is_err());
    let mut fields: Value = serde_json::from_slice(&pending.to_json().unwrap()).unwrap();
    for member in ["v", "counterparty", "allowance_id", "proposal"] {
        let mut missing = fields.clone();
        missing.as_object_mut().unwrap().remove(member);
        assert!(
            PendingAllowanceRecord::read(&serde_json::to_vec(&missing).unwrap(), &own).is_err(),
            "{member}"
        );
    }
    let extended = |v: &Value| serde_json::to_vec(v).unwrap();
    fields["future"] = serde_json::json!(["one", 2]);
    fields["proposal"]["future"] = serde_json::json!(3);
    let mut read = PendingAllowanceRecord::bind(&extended(&fields), &own, &nk, &key).unwrap();
    read.note_proposal(crossing_proposal(&keys)).unwrap();
    let wrote: Value = serde_json::from_slice(&read.to_json().unwrap()).unwrap();
    assert_eq!(wrote["future"], fields["future"]);
    assert_eq!(wrote["proposal"], fields["proposal"]);
    fields["allowance_id"] = serde_json::json!("55de1f16-1a9e-4504-bc9c-57efc3a2b38c");
    assert!(PendingAllowanceRecord::read(&extended(&fields), &own).is_err());
}

/// `allowance_records`, as paykit-lib generates it.
fn allowance_records(keys: &Keys) -> Value {
    let alice = pubky(ALICE);
    let nk = keys.identity(&alice).naming_key();
    let (accepted, refused) = allowance_cases(keys);
    let write = |(name, record): &AllowanceCase, taken: bool| -> Value {
        assert_eq!(record.to_json().is_ok(), taken, "{name}: the writer");
        let value = record.to_json_unchecked().unwrap();
        let key = record.record_key(&nk).unwrap();
        let mut out = serde_json::json!({
            "name": name,
            "record_key": b64(&key),
            "value": String::from_utf8(value.clone()).unwrap(),
        });
        match AllowanceRecord::bind(&value, &alice, &nk, &key) {
            Ok(_) => assert!(taken, "{name}: taken"),
            Err(error) => {
                assert!(!taken, "{name}: {error}");
                out["reason"] = serde_json::json!(quoted(&error.to_string()));
            }
        }
        out
    };
    let mut refused: Vec<Value> = refused.iter().map(|c| write(c, false)).collect();
    for (name, (value, key)) in [
        (
            "a rejection carrying executor_app_id",
            rejection_with_executor(keys),
        ),
        (
            "a conflicting proposal whose terms aren't valid",
            conflicted_with_invalid_terms(keys),
        ),
    ] {
        let error = AllowanceRecord::bind(&value, &alice, &nk, &key).expect_err(name);
        refused.push(serde_json::json!({
            "name": name,
            "reason": quoted(&error.to_string()),
            "record_key": b64(&key),
            "value": String::from_utf8(value).unwrap(),
        }));
    }
    serde_json::json!({
        "accepted": accepted.iter().map(|c| write(c, true)).collect::<Vec<_>>(),
        "description": ALLOWANCE_RECORDS_DESCRIPTION,
        "refused": refused,
    })
}

/// `allowance_records` (Paykit wire 4.16, 5.4): each record binds at its key, or is refused,
/// as its list says.
fn check_allowance_records(keys: &Keys, v: &Value) {
    let alice = pubky(ALICE);
    let nk = keys.identity(&alice).naming_key();
    let m = &v["allowance_records"];
    let (accepted, refused) = allowance_cases(keys);
    assert_eq!(arr(m, "accepted").len(), accepted.len());
    assert_eq!(arr(m, "refused").len(), refused.len() + 2);
    for (list, taken) in [("accepted", true), ("refused", false)] {
        for case in arr(m, list) {
            let name = st(case, "name");
            match AllowanceRecord::bind(value(case), &alice, &nk, &key(case)) {
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

#[test]
fn test_activity_record_proof_names_the_list_it_paid() {
    use paykit_lib::records::DestinationSource;
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    // The proof paid a list version; a destination that isn't that list doesn't match it.
    let mut public = b.unrequested.clone();
    if let Some(u) = public.unrequested.as_mut() {
        u.source = DestinationSource::Public;
        u.list_version = None;
    }
    let error = public.to_json().unwrap_err().to_string();
    assert!(error.contains("list_version"), "{error}");
    let json = public.to_json_unchecked().unwrap();
    assert!(ActivityRecord::read(&json, &pubky(ALICE), ActivitySide::Payer).is_err());
    b.unrequested.to_json().unwrap();
}

/// A period record holding `messages.json`'s period details, and a later write of it with
/// `details` instead.
fn period_held(keys: &Keys) -> (ActivityRecord, paykit_lib::records::StoredMessage) {
    let details = stored_message(keys, "payment_period_details");
    let held = ActivityRecord {
        proof: None,
        period_details: Some(details.clone()),
        ..bases(keys).period
    };
    (held, details)
}

fn with_details(held: &ActivityRecord, d: paykit_lib::records::StoredMessage) -> ActivityRecord {
    ActivityRecord {
        period_details: Some(d),
        ..held.clone()
    }
}

#[test]
fn test_activity_record_seen_period_details_again_as_late_is_kept() {
    // The same message again, as late with the same hash: nothing changes. A check that
    // took only later details would refuse it.
    let keys = Keys::from_seeds();
    let (held, details) = period_held(&keys);
    held.check_later_write(&with_details(&held, details))
        .unwrap();
}

#[test]
fn test_activity_record_seen_period_details_as_late_with_another_hash_are_refused() {
    // Another signed message as late (the same created_at and msg_id), other rates: an
    // equivocation, which replaces nothing and is stored beside nothing.
    let keys = Keys::from_seeds();
    let (held, details) = period_held(&keys);
    let equivocation = resigned(&keys, "payment_period_details", None, |body| {
        body.insert(
            "rates".into(),
            serde_json::json!([{ "asset": "btc", "value": "0.00009999" }]),
        );
    });
    assert_ne!(equivocation.hash(), details.hash());
    let error = held
        .check_later_write(&with_details(&held, equivocation.clone()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("drops period details seen"), "{error}");
    let beside = ActivityRecord {
        earlier_details: vec![equivocation],
        ..held.clone()
    };
    let error = held.check_later_write(&beside).unwrap_err().to_string();
    assert!(error.contains("two details as late"), "{error}");
}

#[test]
fn test_activity_record_seen_period_details_kept_beside_later_ones() {
    // Later details replace none: the later write keeps those seen in earlier_details.
    let keys = Keys::from_seeds();
    let (held, details) = period_held(&keys);
    let later = later_details(&keys);
    let kept = ActivityRecord {
        earlier_details: vec![details],
        ..with_details(&held, later.clone())
    };
    held.check_later_write(&kept).unwrap();
    let error = held
        .check_later_write(&with_details(&held, later))
        .unwrap_err()
        .to_string();
    assert!(error.contains("drops period details seen"), "{error}");
    // Back to the earlier details alone: they are no longer the latest held, and the later
    // ones are dropped.
    assert!(kept.check_later_write(&held).is_err());
}

#[test]
fn test_activity_record_seen_period_details_the_proof_write_names_earlier_ones() {
    // The payer paid by the earlier details after later ones came: the write that stores its
    // proof sets period_details to the details the proof names, and keeps the later ones.
    let keys = Keys::from_seeds();
    let (held, details) = period_held(&keys);
    let later = later_details(&keys);
    let latest = ActivityRecord {
        earlier_details: vec![details.clone()],
        ..with_details(&held, later.clone())
    };
    let proof = bases(&keys).period.proof;
    assert_eq!(
        proof.as_ref().unwrap().message().refs.get(1),
        Some(&details.hash())
    );
    let paid = ActivityRecord {
        proof: proof.clone(),
        period_details: Some(details.clone()),
        earlier_details: vec![later],
        ..held.clone()
    };
    latest.check_later_write(&paid).unwrap();
    // The proof's write that drops the later details is refused.
    let dropped = ActivityRecord {
        proof,
        period_details: Some(details),
        ..held
    };
    let error = latest.check_later_write(&dropped).unwrap_err().to_string();
    assert!(error.contains("drops period details seen"), "{error}");
}

#[test]
fn test_activity_record_period_details_at_most_eight() {
    let keys = Keys::from_seeds();
    let (held, _) = period_held(&keys);
    let details = |n: u64| {
        resign_with(&keys, "payment_period_details", |m| {
            m.msg_id = format!("6b1f0e2a-3c4d-4e5f-8a6b-{n:012x}");
            m.created_at_ms -= 1000 * (n + 1);
        })
    };
    let alice = pubky(ALICE);
    let with = |n: u64| {
        let mut earlier: Vec<_> = (0..n).map(details).collect();
        earlier.reverse();
        ActivityRecord {
            earlier_details: earlier,
            ..held.clone()
        }
    };
    let seven = with(7);
    let json = seven.to_json().unwrap();
    assert_eq!(
        ActivityRecord::read(&json, &alice, ActivitySide::Payer)
            .unwrap()
            .details()
            .len(),
        8
    );
    let eight = with(8);
    assert!(eight.to_json().is_err());
    let error = ActivityRecord::read(
        &eight.to_json_unchecked().unwrap(),
        &alice,
        ActivitySide::Payer,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("at most 7"), "{error}");
}

#[test]
fn test_activity_record_seen_marks_are_kept() {
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::Reassociation;
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    let named = b.payer_request.allowance_id.clone().unwrap();
    let entry = |allowance_id: &AllowanceId, from_period| Reassociation {
        allowance_id: allowance_id.clone(),
        from_period,
    };
    // Reassociations: an entry after the last seen is taken; one changed or dropped isn't.
    let one = ActivityRecord {
        reassociations: vec![entry(&other, 1_772_236_800)],
        ..b.payer_request.clone()
    };
    let two = ActivityRecord {
        reassociations: vec![entry(&other, 1_772_236_800), entry(&named, 1_774_915_200)],
        ..b.payer_request.clone()
    };
    b.payer_request.check_later_write(&one).unwrap();
    one.check_later_write(&two).unwrap();
    let error = two.check_later_write(&one).unwrap_err().to_string();
    assert!(error.contains("reassociation seen"), "{error}");
    let changed = ActivityRecord {
        reassociations: vec![entry(&other, 1_774_915_200)],
        ..b.payer_request.clone()
    };
    assert!(one.check_later_write(&changed).is_err());
    // An entry before the last seen can't be added: the list is strictly ascending.
    let before = ActivityRecord {
        reassociations: vec![entry(&other, 1_772_236_800), entry(&named, 1_769_817_600)],
        ..b.payer_request.clone()
    };
    assert!(one.check_later_write(&before).is_err());
    // manual_only stays once seen.
    let marked = ActivityRecord {
        manual_only: true,
        ..b.period.clone()
    };
    b.period.check_later_write(&marked).unwrap();
    let error = marked.check_later_write(&b.period).unwrap_err().to_string();
    assert!(error.contains("manual_only"), "{error}");
}

#[test]
fn test_activity_record_allowance_for_follows_reassociations() {
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::Reassociation;
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let named = b.payer_request.allowance_id.clone().unwrap();
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    let moved = ActivityRecord {
        reassociations: vec![Reassociation {
            allowance_id: other.clone(),
            from_period: 1_772_236_800,
        }],
        ..b.payer_request.clone()
    };
    assert_eq!(moved.allowance_for(1_769_817_600), Some(&named));
    assert_eq!(moved.allowance_for(1_772_236_800), Some(&other));
    assert_eq!(moved.allowance_for(1_774_915_200), Some(&other));
    assert_eq!(b.payer_request.allowance_for(1_774_915_200), Some(&named));
    // A later period attempted early, after the reassociation, counts under the Allowance it
    // was moved to; the write listing it keeps the entry.
    let attempted = ActivityRecord {
        attempted_periods: vec![1_774_915_200],
        ..moved.clone()
    };
    moved.check_later_write(&attempted).unwrap();
    assert_eq!(attempted.allowance_for(1_774_915_200), Some(&other));
}

#[test]
fn test_allowance_record_seen_roles_are_kept() {
    let keys = Keys::from_seeds();
    let (accepted, _) = allowance_cases(&keys);
    let ended = accepted[0].1.clone();
    ended.check_later_write(&ended).unwrap();
    let no_end = AllowanceRecord {
        end: None,
        ..ended.clone()
    };
    // Adding an end is a later write; dropping the one seen isn't.
    no_end.check_later_write(&ended).unwrap();
    assert!(ended.check_later_write(&no_end).is_err());
    // Another acceptance, or another proposal, in place of the one seen.
    let other_acceptance = resigned(&keys, "allowance_acceptance", None, |body| {
        body.remove("executor_app_id");
    });
    let replaced = AllowanceRecord {
        response: Some(other_acceptance),
        end: None,
        ..ended.clone()
    };
    assert!(no_end.check_later_write(&replaced).is_err());
    let other_proposal = resigned(&keys, "allowance_proposal", None, |body| {
        body.insert("proposer_role".into(), serde_json::json!("allower"));
    });
    let reproposed = AllowanceRecord {
        proposal: other_proposal,
        ..no_end.clone()
    };
    assert!(no_end.check_later_write(&reproposed).is_err());
}

#[test]
fn test_allowance_record_conflicted_is_never_dropped() {
    let keys = Keys::from_seeds();
    let (accepted, _) = allowance_cases(&keys);
    let plain = accepted[0].1.clone();
    let conflicted = accepted
        .iter()
        .find(|(name, _)| name.contains("second proposal from the counterparty"))
        .unwrap()
        .1
        .clone();
    let second = conflicted.conflicted.clone().unwrap();
    // A later write may make a record conflicted, and keeps it so.
    let unended = AllowanceRecord {
        end: None,
        ..plain.clone()
    };
    unended.check_later_write(&conflicted).unwrap();
    conflicted.check_later_write(&conflicted).unwrap();
    let ended = AllowanceRecord {
        end: plain.end.clone(),
        ..conflicted.clone()
    };
    conflicted.check_later_write(&ended).unwrap();
    // It never clears it, replaces the conflicting proposal, or swaps the two.
    let cleared = AllowanceRecord {
        conflicted: None,
        ..conflicted.clone()
    };
    let error = conflicted
        .check_later_write(&cleared)
        .unwrap_err()
        .to_string();
    assert!(error.contains("role seen"), "{error}");
    let third = resign_with(&keys, "allowance_proposal", |m| {
        m.msg_id = "9f2a4c6e-8b0d-4e1f-a3b5-c7d9e1f3a5b7".into();
        m.body
            .insert("proposer_role".into(), serde_json::json!("allower"));
    });
    let replaced = AllowanceRecord {
        conflicted: Some(third),
        ..conflicted.clone()
    };
    assert!(conflicted.check_later_write(&replaced).is_err());
    let swapped = AllowanceRecord {
        proposal: second,
        conflicted: Some(conflicted.proposal.clone()),
        response: None,
        ..conflicted.clone()
    };
    swapped.to_json().unwrap();
    let held = AllowanceRecord {
        response: None,
        ..conflicted.clone()
    };
    assert!(held.check_later_write(&swapped).is_err());
}

#[test]
fn test_allowance_record_conflicting_response_is_never_dropped() {
    let keys = Keys::from_seeds();
    let (accepted, _) = allowance_cases(&keys);
    let case = |part: &str| {
        accepted
            .iter()
            .find(|(name, _)| name.contains(part))
            .unwrap()
            .1
            .clone()
    };
    let plain = case("the accepted Allowance with its end");
    let rejected = case("a rejection after the acceptance");
    let reaccepted = case("conflicted by another acceptance");
    let unended = AllowanceRecord { end: None, ..plain };
    // A later write may add the conflicting response, and keeps it as seen.
    unended.check_later_write(&rejected).unwrap();
    rejected.check_later_write(&rejected).unwrap();
    // It never clears it, nor replaces it with another conflicting response.
    let cleared = AllowanceRecord {
        conflicted: None,
        ..rejected.clone()
    };
    assert!(rejected.check_later_write(&cleared).is_err());
    assert!(rejected.check_later_write(&reaccepted).is_err());
    assert!(rejected.current_grant(&pubky(ALICE)).is_none());
}

#[test]
fn test_allowance_record_conflicted_has_no_current_grant() {
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let entry = &paykit("documents.json")["executor_grant_history_entry"];
    let mut with_grant: Value = serde_json::from_slice(&allowance_record_value(&keys)).unwrap();
    with_grant["grants"] = serde_json::json!([entry]);
    with_grant["grant"] = entry["grant"].clone();
    let granted = AllowanceRecord::read(&serde_json::to_vec(&with_grant).unwrap(), &alice).unwrap();
    let (_, executor) = granted.current_grant(&alice).unwrap();
    let (accepted, _) = allowance_cases(&keys);
    let second = accepted
        .iter()
        .find_map(|(name, r)| {
            name.contains("second proposal from the counterparty")
                .then(|| r.conflicted.clone())
                .flatten()
        })
        .unwrap();
    let conflicted = AllowanceRecord {
        conflicted: Some(second),
        ..granted.clone()
    };
    // Nothing is paid under it: no current grant. The grant history still names the
    // executor at each version, so the outcomes of attempts already open are recorded.
    assert!(conflicted.current_grant(&alice).is_none());
    let after = pubky_switch_core::ops::RecordVersion {
        generation: 1,
        version: 13,
    };
    assert_eq!(
        conflicted.executor_at(&alice, after).unwrap(),
        Some(executor.app_id())
    );
    let json = conflicted.to_json().unwrap();
    let read = AllowanceRecord::read(&json, &alice).unwrap();
    assert!(read.conflicted.is_some() && read.current_grant(&alice).is_none());
    assert!(read.msg_ids.iter().any(|id| id == SECOND_PROPOSAL));
}

#[test]
fn test_allowance_record_conflicted_msg_id_is_a_stored_one() {
    // The conflicting proposal is stored, so its msg_id stays however many others follow it
    // (Paykit wire 5.3, 5.4).
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let (accepted, _) = allowance_cases(&keys);
    let conflicted = accepted
        .iter()
        .find(|(name, _)| name.contains("second proposal from the counterparty"))
        .unwrap()
        .1
        .clone();
    let mut json: Value = serde_json::from_slice(&conflicted.to_json().unwrap()).unwrap();
    let mut ids = vec![serde_json::json!(SECOND_PROPOSAL)];
    ids.extend((1..=17u32).map(|i| serde_json::json!(format!("{i:08x}-0000-4000-8000-{i:012x}"))));
    json["msg_ids"] = Value::Array(ids);
    let read = AllowanceRecord::read(&serde_json::to_vec(&json).unwrap(), &alice).unwrap();
    assert!(read.msg_ids.iter().any(|id| id == SECOND_PROPOSAL));
    assert!(!read.msg_ids.iter().any(|id| id.starts_with("00000001-")));
}

/// The `payment_lists` record of `binding_accepted`, a used single-use endpoint of its list,
/// and that list signed again with `version` moved by `delta`, other content otherwise the
/// same.
fn list_case(keys: &Keys, delta: i64) -> (PaymentListRecord, paykit_lib::records::StoredMessage) {
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let mut held = PaymentListRecord::read(value(&arr(&s, "binding_accepted")[0]), &alice).unwrap();
    let paykit_lib::Body::PrivatePaymentList(held_list) = held.list.body() else {
        panic!("a list")
    };
    held.used.push(paykit_lib::records::UsedEndpoint {
        version: held_list.version,
        endpoint: paykit_lib::identifiers::PaymentEndpointIdentifier::parse("btc-bitcoin-p2wpkh")
            .unwrap(),
        attempt_id: paykit_lib::identifiers::AttemptId::from_bytes([3; 16]),
        origin: Default::default(),
    });
    let list = resigned(keys, "private_payment_list", None, |body| {
        let version = paykit_lib::ListVersion::from_value(&body["version"]).unwrap();
        let counter = version.counter().checked_add_signed(delta).unwrap();
        let moved = paykit_lib::ListVersion::new(version.generation(), counter).unwrap();
        body.insert("version".into(), moved.to_value());
        if delta == 0 {
            // The same version, other signed content.
            let expires = body["expires_at"]
                .as_str()
                .unwrap()
                .replace(".000Z", ".001Z");
            body.insert("expires_at".into(), serde_json::json!(expires));
        }
    });
    (held, list)
}

#[test]
fn test_payment_list_record_later_write_follows_the_list_version() {
    let keys = Keys::from_seeds();
    let (held, other_same_version) = list_case(&keys, 0);
    // The same list, its used grown: taken.
    let mut grown = held.clone();
    grown.used.push(paykit_lib::records::UsedEndpoint {
        attempt_id: paykit_lib::identifiers::AttemptId::from_bytes([4; 16]),
        ..held.used[0].clone()
    });
    held.check_later_write(&grown).unwrap();
    held.check_later_write(&held).unwrap();
    // A greater version: taken, with the used entries it held.
    let (_, newer) = list_case(&keys, 1);
    let newer = PaymentListRecord {
        list: newer,
        ..held.clone()
    };
    held.check_later_write(&newer).unwrap();
    // A lower version, or another list of the same version: refused.
    let (_, older) = list_case(&keys, -1);
    let older = PaymentListRecord {
        list: older,
        ..held.clone()
    };
    assert!(held.check_later_write(&older).is_err());
    assert_ne!(other_same_version.hash(), held.list.hash());
    let equivocation = PaymentListRecord {
        list: other_same_version,
        ..held.clone()
    };
    let error = held
        .check_later_write(&equivocation)
        .unwrap_err()
        .to_string();
    assert!(error.contains("same version"), "{error}");
}

#[test]
fn test_payment_list_record_later_write_keeps_used_entries() {
    let keys = Keys::from_seeds();
    let (held, _) = list_case(&keys, 0);
    // At the same version, dropping a used entry is refused.
    let dropped = PaymentListRecord {
        used: Vec::new(),
        ..held.clone()
    };
    let error = held.check_later_write(&dropped).unwrap_err().to_string();
    assert!(error.contains("seen used"), "{error}");
    // At a greater version too: wire 5.4 gives a newer list no reset of used.
    let (_, newer) = list_case(&keys, 1);
    let newer_dropped = PaymentListRecord {
        list: newer,
        used: Vec::new(),
        ..held.clone()
    };
    assert!(held.check_later_write(&newer_dropped).is_err());
}

#[test]
fn test_record_transitions_hold_records_built_in_code() {
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    // A later write no reader takes: request_hash in a request record.
    let later = ActivityRecord {
        request_hash: b.period.request_hash,
        ..b.payer_request.clone()
    };
    b.payer_request.check_later_write(&b.payer_request).unwrap();
    assert!(b.payer_request.check_later_write(&later).is_err());
    assert!(b
        .payer_request
        .check_dropped_periods(
            &later,
            &[],
            &pubky(ALICE),
            &keys.identity(&pubky(ALICE)).naming_key()
        )
        .is_err());
    // A payment_lists record whose list is another identity's.
    let (held, _) = list_case(&keys, 0);
    let other_payee = PaymentListRecord {
        payee: pubky(ALICE),
        ..held.clone()
    };
    assert!(held.check_later_write(&other_payee).is_err());
    // An allowances record that names another Allowance.
    let (accepted, _) = allowance_cases(&keys);
    let ended = accepted[0].1.clone();
    let renamed = AllowanceRecord {
        allowance_id: paykit_lib::identifiers::AllowanceId::parse(
            "55de1f16-1a9e-4504-bc9c-57efc3a2b38c",
        )
        .unwrap(),
        ..ended.clone()
    };
    assert!(ended.check_later_write(&renamed).is_err());
}

#[test]
fn test_stored_message_from_received_is_its_plaintexts() {
    use paykit_lib::records::StoredMessage;
    let keys = Keys::from_seeds();
    let request = stored_message(&keys, "payment_request");
    let other = stored_message(&keys, "payment_request_recurring");
    let built = |s: &StoredMessage, message: &pubky_switch_core::message::Message| {
        StoredMessage::from_received(
            s.sender(),
            s.cert().clone(),
            s.plaintext().clone(),
            message.clone(),
            Some(1),
        )
    };
    // The client's own parts: the same stored copy as a reader rebuilds.
    let same = built(&request, request.message()).unwrap();
    assert_eq!(same.hash(), request.hash());
    // A parsed message beside another message's plaintext: refused, so no record writer gets
    // a copy whose checks and bytes disagree.
    let error = built(&request, other.message()).unwrap_err().to_string();
    assert!(error.contains("isn't the stored plaintext's"), "{error}");
    // Another app's certificate beside the plaintext.
    let cap = AppCert::verify(&crate::common::cert_doc("cap"), &pubky(BOB)).unwrap();
    assert!(StoredMessage::from_received(
        request.sender(),
        cap,
        request.plaintext().clone(),
        request.message().clone(),
        Some(1),
    )
    .is_err());
}

#[test]
fn test_record_transitions_compare_versions_of_one_record() {
    let keys = Keys::from_seeds();
    let alice = pubky(ALICE);
    let b = bases(&keys);
    // An unrequested record holds no message to keep: another attempt's record isn't its
    // next version.
    let bare = ActivityRecord {
        proof: None,
        ..b.unrequested.clone()
    };
    bare.check_later_write(&bare).unwrap();
    let other_attempt = ActivityRecord {
        attempt_id: Some(paykit_lib::identifiers::AttemptId::from_bytes([9; 16])),
        ..bare.clone()
    };
    let error = bare
        .check_later_write(&other_attempt)
        .unwrap_err()
        .to_string();
    assert!(error.contains("another record's"), "{error}");
    // A request record against a period record of the same request.
    let held = ActivityRecord {
        attempted_periods: Vec::new(),
        ..b.payer_request.clone()
    };
    assert!(held
        .check_dropped_periods(&b.period, &[], &alice, &keys.identity(&alice).naming_key())
        .is_err());
    // A valid payment_lists record of another of bob's apps, with a newer version.
    let (list, _) = list_case(&keys, 0);
    let cap = keys.app("cap").keys.app_id_bytes();
    let cap_list = resign_with(&keys, "private_payment_list", |m| {
        m.from.app_id = cap;
        let version = paykit_lib::ListVersion::from_value(&m.body["version"]).unwrap();
        m.body
            .insert("version".into(), version.next().unwrap().to_value());
    });
    let other_app = PaymentListRecord {
        app_id: cap,
        list: cap_list,
        ..list.clone()
    };
    let error = list.check_later_write(&other_app).unwrap_err().to_string();
    assert!(error.contains("another payee app's"), "{error}");
    // An allowances record with another counterparty, its messages unchanged.
    let (accepted, _) = allowance_cases(&keys);
    let ended = accepted[0].1.clone();
    let swapped = AllowanceRecord {
        counterparty: alice,
        ..ended.clone()
    };
    assert!(ended.check_later_history(&swapped).is_err());
    assert!(ended.check_later_write(&swapped).is_err());
}

#[test]
fn test_issued_payee_receipt_preserves_independent_deletion_restrictions() {
    let keys = Keys::from_seeds();
    let paid = ActivityRecord {
        request: Some(stored_message(&keys, "payment_request")),
        proof: Some(stored_message(&keys, "payment_proof")),
        receipt: Some(stored_message(&keys, "payment_receipt")),
        ..activity(ActivitySide::Payee, pubky(ALICE))
    };
    let delete = [1; 32];
    let prior = [2; 32];
    let eager = ActivityRecord {
        lost_to_delete: Some(delete),
        receipt: None,
        ..paid.clone()
    };
    let repaired = paid
        .merge_issued_payee_receipt(&eager, &delete, true)
        .unwrap();
    assert_eq!(repaired.to_json().unwrap(), paid.to_json().unwrap());
    let indirect = paid
        .merge_issued_payee_receipt(&eager, &delete, false)
        .unwrap();
    assert_eq!(indirect.lost_to_delete, Some(delete));
    let restricted = ActivityRecord {
        lost_to_delete: Some(prior),
        ..paid
    };
    let repaired = restricted
        .merge_issued_payee_receipt(&eager, &delete, true)
        .unwrap();
    assert_eq!(repaired.to_json().unwrap(), restricted.to_json().unwrap());
    assert!(restricted
        .merge_issued_payee_receipt(&eager, &delete, false)
        .is_err());
}

#[test]
fn test_issued_payee_receipt_merges_notes_and_refuses_conflicting_values() {
    let keys = Keys::from_seeds();
    let paid = ActivityRecord {
        request: Some(stored_message(&keys, "payment_request")),
        proof: Some(stored_message(&keys, "payment_proof")),
        receipt: Some(stored_message(&keys, "payment_receipt")),
        ..activity(ActivitySide::Payee, pubky(ALICE))
    };
    let note = |field: &str, value: &str, nested_field: &str, nested_value: &str| {
        let mut json = written(&paid);
        json[field] = serde_json::json!(value);
        json["proof"]["cert"][nested_field] = serde_json::json!(nested_value);
        ActivityRecord::read(
            &serde_json::to_vec(&json).unwrap(),
            &pubky(BOB),
            ActivitySide::Payee,
        )
        .unwrap()
    };
    let original = note(
        "merchant_note",
        "collected",
        "merchant_reference",
        "order 5",
    );
    let current = note("shipping_note", "delivered", "shipping_reference", "item 8");
    let merged = original
        .merge_issued_payee_receipt(&current, &[7; 32], false)
        .unwrap();
    let value = written(&merged);
    assert_eq!(value["merchant_note"], "collected");
    assert_eq!(value["shipping_note"], "delivered");
    assert_eq!(value["proof"]["cert"]["merchant_reference"], "order 5");
    assert_eq!(value["proof"]["cert"]["shipping_reference"], "item 8");
    assert_eq!(value["receipt"], written(&paid)["receipt"]);
    assert!(original
        .merge_issued_payee_receipt(
            &note("merchant_note", "refunded", "merchant_reference", "order 5"),
            &[7; 32],
            false
        )
        .is_err());
    assert!(original
        .merge_issued_payee_receipt(
            &note(
                "shipping_note",
                "delivered",
                "merchant_reference",
                "order 6"
            ),
            &[7; 32],
            false
        )
        .is_err());
}

#[test]
fn test_issued_receipt_publication_keeps_members_of_each_record_and_nested_object() {
    let keys = Keys::from_seeds();
    let paid = ActivityRecord {
        request: Some(stored_message(&keys, "payment_request")),
        proof: Some(stored_message(&keys, "payment_proof")),
        receipt: Some(stored_message(&keys, "payment_receipt")),
        ..activity(ActivitySide::Payee, pubky(ALICE))
    };
    let read = |json: &Value| {
        ActivityRecord::read(
            &serde_json::to_vec(json).unwrap(),
            &pubky(BOB),
            ActivitySide::Payee,
        )
        .unwrap()
    };
    let mut prior = written(&paid);
    prior["merchant_reference"] = "order 5".into();
    prior["proof"]["cert"]["merchant_note"] = "collected".into();
    let mut eager = written(&paid);
    eager["shipping_reference"] = "item 8".into();
    eager["proof"]["cert"]["shipping_note"] = "delivered".into();
    let mut union = prior.clone();
    union["shipping_reference"] = eager["shipping_reference"].clone();
    union["proof"]["cert"]["shipping_note"] = eager["proof"]["cert"]["shipping_note"].clone();
    assert!(read(&prior)
        .check_receipt_publication(&read(&union))
        .is_ok());
    assert!(read(&eager)
        .check_receipt_publication(&read(&union))
        .is_ok());
    assert!(read(&prior)
        .check_receipt_publication(&read(&eager))
        .is_err());
    union["proof"]["cert"]["shipping_note"] = "returned".into();
    assert!(read(&prior)
        .check_receipt_publication(&read(&union))
        .is_ok());
    assert!(read(&eager)
        .check_receipt_publication(&read(&union))
        .is_err());
}

#[test]
fn test_activity_record_receipt_is_of_its_proofs_billing_period() {
    // A period record with the February period's proof, and a receipt of that proof's attempt
    // that names another Billing Period: refused for the receipt's period.
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let proof = b.period.proof.clone().unwrap();
    let paykit_lib::bodies::Body::PaymentProof(p) = proof.body() else {
        panic!("a proof")
    };
    let receipt = |period: Value| {
        resign_with(&keys, "payment_receipt", |m| {
            m.refs = vec![proof.hash(), b.period.request_hash.unwrap()];
            m.body.insert(
                "attempt_id".into(),
                serde_json::json!(p.attempt_id.to_text()),
            );
            m.body.insert("billing_period".into(), period);
        })
    };
    let february = serde_json::json!({ "ends_at": "2026-03-31T00:00:00.000Z",
        "starts_at": "2026-02-28T00:00:00.000Z" });
    let march = serde_json::json!({ "ends_at": "2026-04-30T00:00:00.000Z",
        "starts_at": "2026-03-31T00:00:00.000Z" });
    let paid = ActivityRecord {
        receipt: Some(receipt(february)),
        ..b.period.clone()
    };
    paid.to_json().unwrap();
    let other_period = ActivityRecord {
        receipt: Some(receipt(march)),
        ..b.period.clone()
    };
    let error = other_period.to_json().unwrap_err().to_string();
    assert!(
        error.contains("the receipt isn't of the stored proof's attempt and Billing Period"),
        "{error}"
    );
    let json = other_period.to_json_unchecked().unwrap();
    assert!(ActivityRecord::read(&json, &pubky(ALICE), ActivitySide::Payer).is_err());
}

/// `record` as its writer writes it, with members added at `members` (JSON pointer to an
/// object, member, value), read back by alice.
fn read_with_unknown(record: &ActivityRecord, members: &[(&str, &str, Value)]) -> ActivityRecord {
    let mut value: Value = serde_json::from_slice(&record.to_json().unwrap()).unwrap();
    for (pointer, name, member) in members {
        value
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert((*name).to_owned(), member.clone());
    }
    ActivityRecord::read(
        &serde_json::to_vec(&value).unwrap(),
        &pubky(ALICE),
        ActivitySide::Payer,
    )
    .unwrap()
}

fn written(record: &ActivityRecord) -> Value {
    serde_json::from_slice(&record.to_json().unwrap()).unwrap()
}

#[test]
fn test_activity_record_keeps_unknown_members() {
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let note = serde_json::json!({ "text": "kept", "n": 7 });
    let read = read_with_unknown(
        &b.unrequested,
        &[
            ("", "payment_note", note.clone()),
            ("/unrequested", "rail_hint", serde_json::json!("on-chain")),
            ("/proof", "relayed_by", Value::Null),
            ("/proof/cert", "chain", serde_json::json!(["a"])),
        ],
    );
    assert!(!read.unknown.is_empty());
    // Read, changed in a member it knows, and written again: every member it doesn't know
    // is there as read.
    let changed = ActivityRecord {
        status: Some("paid".into()),
        ..read
    };
    let out = written(&changed);
    assert_eq!(out["payment_note"], note);
    assert_eq!(out["unrequested"]["rail_hint"], "on-chain");
    assert!(out["proof"].as_object().unwrap().contains_key("relayed_by"));
    assert_eq!(out["proof"]["cert"]["chain"], serde_json::json!(["a"]));
    assert_eq!(out["status"], "paid");
    // And reading that again keeps them once more.
    let again = ActivityRecord::read(
        &changed.to_json().unwrap(),
        &pubky(ALICE),
        ActivitySide::Payer,
    )
    .unwrap();
    assert_eq!(written(&again), out);
    // A record built in code has no members it doesn't know, and writes none.
    assert!(b.unrequested.unknown.is_empty());
    assert!(written(&b.unrequested).get("payment_note").is_none());
}

#[test]
fn test_activity_record_unknown_members_follow_their_object() {
    let keys = Keys::from_seeds();
    let (held, details) = period_held(&keys);
    let read = read_with_unknown(
        &held,
        &[("/period_details", "relay", serde_json::json!("x"))],
    );
    // The stored details move to earlier_details: their members move with them.
    let later = later_details(&keys);
    let moved = ActivityRecord {
        period_details: Some(later),
        earlier_details: vec![details],
        ..read.clone()
    };
    let out = written(&moved);
    assert_eq!(out["earlier_details"][0]["relay"], "x");
    assert!(out["period_details"].get("relay").is_none());
    // An object the writer no longer holds drops its members with it.
    let b = bases(&keys);
    let with_proof = read_with_unknown(
        &b.unrequested,
        &[("/proof", "relayed_by", serde_json::json!("bob"))],
    );
    let without = ActivityRecord {
        proof: None,
        ..with_proof
    };
    assert!(!String::from_utf8(without.to_json().unwrap())
        .unwrap()
        .contains("relayed_by"));
}

#[test]
fn test_list_and_allowance_records_keep_unknown_members() {
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let accepted = arr(&s, "binding_accepted");
    let add = |case: &Value, pointer: &str| {
        let mut v: Value = serde_json::from_slice(value(case)).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("future".into(), serde_json::json!({ "k": 1 }));
        v.pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("nested".into(), serde_json::json!(true));
        serde_json::to_vec(&v).unwrap()
    };
    let list = PaymentListRecord::read(&add(&accepted[0], "/list"), &alice).unwrap();
    let out: Value = serde_json::from_slice(&list.to_json().unwrap()).unwrap();
    assert_eq!(out["future"]["k"], 1);
    assert_eq!(out["list"]["nested"], true);
    let allowance = AllowanceRecord::read(&add(&accepted[1], "/proposal"), &alice).unwrap();
    let out: Value = serde_json::from_slice(&allowance.to_json().unwrap()).unwrap();
    assert_eq!(out["future"]["k"], 1);
    assert_eq!(out["proposal"]["nested"], true);
    // Without unknown members, the bytes are the file's.
    let plain = AllowanceRecord::read(value(&accepted[1]), &alice).unwrap();
    assert!(plain.unknown.is_empty());
    assert_eq!(plain.to_json().unwrap(), value(&accepted[1]));
}

#[test]
fn test_activity_record_reassociation_on_a_version_the_reader_does_not_hold() {
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::Reassociation;
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    let period = 1_772_236_800;
    // The first version lists the period, attempted early; the second drops it once the
    // attempt ended; the third, built on the second, moves the request from that period on.
    let first = ActivityRecord {
        attempted_periods: vec![period],
        ..b.payer_request.clone()
    };
    let second = b.payer_request.clone();
    let third = ActivityRecord {
        reassociations: vec![Reassociation {
            allowance_id: other,
            from_period: period,
        }],
        ..second.clone()
    };
    // A reader still holding the first version takes the third: it doesn't hold the version
    // the third builds on.
    first.check_later_write(&third).unwrap();
    // The holder of the second takes it too: the second lists no period.
    second.check_later_write_built_on(&third).unwrap();
    // But a write built on the first, which lists the period, may not add it.
    let error = first
        .check_later_write_built_on(&third)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("at or before an attempted period"),
        "{error}"
    );
}

#[test]
fn test_activity_record_seen_rules_hold_for_every_member() {
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    // Period details seen only in earlier_details stay.
    let (held, details) = period_held(&keys);
    let later = later_details(&keys);
    let both = ActivityRecord {
        period_details: Some(later.clone()),
        earlier_details: vec![details],
        ..held.clone()
    };
    let error = both
        .check_later_write(&with_details(&held, later))
        .unwrap_err()
        .to_string();
    assert!(error.contains("drops period details seen"), "{error}");
    // manual_only is true or absent: false doesn't read.
    let mut value: Value = serde_json::from_slice(&b.period.to_json().unwrap()).unwrap();
    value["manual_only"] = serde_json::json!(false);
    let error = ActivityRecord::read(
        &serde_json::to_vec(&value).unwrap(),
        &pubky(ALICE),
        ActivitySide::Payer,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("manual_only"), "{error}");
}

#[test]
fn test_activity_record_period_starts_resolve_through_the_recurrence() {
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::Reassociation;
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let alice = pubky(ALICE);
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    let from = |from_period| ActivityRecord {
        reassociations: vec![Reassociation {
            allowance_id: other.clone(),
            from_period,
        }],
        ..b.payer_request.clone()
    };
    // A start of the request's periods is taken, at write and read.
    let at_start = from(1_772_236_800);
    ActivityRecord::read(&at_start.to_json().unwrap(), &alice, ActivitySide::Payer).unwrap();
    // A second later isn't: the writer and the reader refuse it.
    let between = from(1_772_236_801);
    assert!(between.to_json().is_err());
    let error = ActivityRecord::read(
        &between.to_json_unchecked().unwrap(),
        &alice,
        ActivitySide::Payer,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("isn't a Billing Period start"), "{error}");
    // And a later write that carries it is ignored.
    assert!(b.payer_request.check_later_write(&between).is_err());
    // A request with ends_at: its last period's start is taken, the next refused.
    let ended = ActivityRecord {
        request: Some(resign_with(&keys, "payment_request_recurring", |m| {
            m.body
                .get_mut("recurrence")
                .and_then(Value::as_object_mut)
                .unwrap()
                .insert(
                    "ends_at".into(),
                    serde_json::json!("2026-03-31T00:00:00.000Z"),
                );
        })),
        ..b.payer_request.clone()
    };
    let moved = |from_period| ActivityRecord {
        reassociations: vec![Reassociation {
            allowance_id: other.clone(),
            from_period,
        }],
        ..ended.clone()
    };
    moved(1_772_236_800).to_json().unwrap();
    assert!(moved(1_774_915_200).to_json().is_err());
}

/// `value` with `member` set to `unknown` in the object at each pointer.
fn with_members(value: &[u8], members: &[(&str, Value)]) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(value).unwrap();
    for (pointer, unknown) in members {
        v.pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .unwrap_or_else(|| panic!("{pointer}"))
            .insert("later_member".into(), unknown.clone());
    }
    serde_json::to_vec(&v).unwrap()
}

/// The member each pointer's object holds in `json`.
fn members_at(json: &[u8], pointers: &[&str]) -> Vec<Value> {
    let v: Value = serde_json::from_slice(json).unwrap();
    pointers
        .iter()
        .map(|p| {
            v.pointer(&format!("{p}/later_member"))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect()
}

#[test]
fn test_allowance_record_equal_grants_keep_their_own_unknown_members() {
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let entry = paykit("documents.json")["executor_grant_history_entry"].clone();
    let mut base: Value = serde_json::from_slice(value(&arr(&s, "binding_accepted")[1])).unwrap();
    // The current grant is the last of the history: two equal objects. And the history holds
    // one entry twice, which the record reads (only its use is refused).
    base["grant"] = entry["grant"].clone();
    base["grants"] = serde_json::json!([entry.clone(), entry]);
    let pointers = [
        "/grant",
        "/grant/cert",
        "/grants/0",
        "/grants/0/grant",
        "/grants/1",
        "/grants/1/grant/cert",
    ];
    let members: Vec<(&str, Value)> = pointers
        .iter()
        .enumerate()
        .map(|(i, p)| (*p, serde_json::json!(format!("of object {i}"))))
        .collect();
    let json = with_members(&serde_json::to_vec(&base).unwrap(), &members);
    let mut read = AllowanceRecord::read(&json, &alice).unwrap();
    read.msg_ids
        .push("00000000-0000-4000-8000-000000000001".into());
    let written = read.to_json().unwrap();
    let again = AllowanceRecord::read(&written, &alice)
        .unwrap()
        .to_json()
        .unwrap();
    let want: Vec<Value> = members.iter().map(|(_, v)| v.clone()).collect();
    assert_eq!(members_at(&written, &pointers), want);
    assert_eq!(members_at(&again, &pointers), want);
}

#[test]
fn test_payment_list_record_equal_used_entries_keep_their_own_unknown_members() {
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let mut base: Value = serde_json::from_slice(value(&arr(&s, "binding_accepted")[0])).unwrap();
    let used = base["used"][0].clone();
    base["used"] = serde_json::json!([used.clone(), used]);
    let members = [
        ("/used/0", serde_json::json!("first")),
        ("/used/1", serde_json::json!("second")),
    ];
    let json = with_members(&serde_json::to_vec(&base).unwrap(), &members);
    let mut read = PaymentListRecord::read(&json, &alice).unwrap();
    assert_eq!(read.used.len(), 2);
    read.msg_ids
        .push("00000000-0000-4000-8000-000000000001".into());
    let written = read.to_json().unwrap();
    let again = PaymentListRecord::read(&written, &alice)
        .unwrap()
        .to_json()
        .unwrap();
    let want = vec![serde_json::json!("first"), serde_json::json!("second")];
    assert_eq!(members_at(&written, &["/used/0", "/used/1"]), want);
    assert_eq!(members_at(&again, &["/used/0", "/used/1"]), want);
    // A unique entry that moves keeps its members: reordered, each still has its own.
    let mut other: Value = serde_json::from_slice(&json).unwrap();
    other["used"][1]["attempt_id"] = serde_json::json!("AAAAAAAAAAAAAAAAAAAAAA");
    let mut read = PaymentListRecord::read(&serde_json::to_vec(&other).unwrap(), &alice).unwrap();
    read.used.reverse();
    let written = read.to_json().unwrap();
    assert_eq!(
        members_at(&written, &["/used/0", "/used/1"]),
        vec![serde_json::json!("second"), serde_json::json!("first")]
    );
}

#[test]
fn test_payment_list_record_unknown_members_stay_with_their_used_entry() {
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let mut base: Value = serde_json::from_slice(value(&arr(&s, "binding_accepted")[0])).unwrap();
    let used = base["used"][0].clone();
    base["used"] = serde_json::json!([used.clone(), used]);
    let json = with_members(
        &serde_json::to_vec(&base).unwrap(),
        &[
            ("/used/0", serde_json::json!("first")),
            ("/used/1", serde_json::json!("second")),
        ],
    );
    let held = PaymentListRecord::read(&json, &alice).unwrap();
    // The first of the two equal entries removed: the one kept keeps its own members.
    let mut first_gone = held.clone();
    first_gone.used.remove(0);
    let written = first_gone.to_json().unwrap();
    assert_eq!(
        members_at(&written, &["/used/0"]),
        vec![serde_json::json!("second")]
    );
    // Reordered, each keeps its own.
    let mut reordered = held;
    reordered.used.reverse();
    let written = reordered.to_json().unwrap();
    assert_eq!(
        members_at(&written, &["/used/0", "/used/1"]),
        vec![serde_json::json!("second"), serde_json::json!("first")]
    );
}

#[test]
fn test_allowance_record_unknown_members_stay_with_their_grant() {
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let entry = paykit("documents.json")["executor_grant_history_entry"].clone();
    let mut base: Value = serde_json::from_slice(value(&arr(&s, "binding_accepted")[1])).unwrap();
    base["grant"] = entry["grant"].clone();
    base["grants"] = serde_json::json!([entry]);
    let json = with_members(
        &serde_json::to_vec(&base).unwrap(),
        &[
            ("/grant", serde_json::json!("current")),
            ("/grants/0/grant", serde_json::json!("history")),
        ],
    );
    let held = AllowanceRecord::read(&json, &alice).unwrap();
    // Without the current grant, the history's grant keeps its own members, and no other
    // object takes the current grant's.
    let mut without = held;
    without.grant = None;
    let written = without.to_json().unwrap();
    assert_eq!(
        members_at(&written, &["/grants/0/grant"]),
        vec![serde_json::json!("history")]
    );
    assert!(!String::from_utf8(written).unwrap().contains("current"));
}

#[test]
fn test_allowance_record_renewed_grant_leaves_the_history_grant_its_own_members() {
    use paykit_lib::grant::ExecutorGrant;
    use paykit_lib::records::GrantEntry;
    use pubky_switch_core::ops::RecordVersion;
    let keys = Keys::from_seeds();
    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let s = bound_sealed();
    let entry = paykit("documents.json")["executor_grant_history_entry"].clone();
    // The current grant is the history's last, each with members of its own.
    let mut base: Value = serde_json::from_slice(value(&arr(&s, "binding_accepted")[1])).unwrap();
    base["grant"] = entry["grant"].clone();
    base["grants"] = serde_json::json!([entry]);
    let pointers = [
        "/grant",
        "/grant/cert",
        "/grants/0/grant",
        "/grants/0/grant/cert",
    ];
    let members = [
        ("/grant", serde_json::json!("current")),
        ("/grant/cert", serde_json::json!("current cert")),
        ("/grants/0/grant", serde_json::json!("history")),
        ("/grants/0/grant/cert", serde_json::json!("history cert")),
    ];
    let json = with_members(&serde_json::to_vec(&base).unwrap(), &members);
    let held = AllowanceRecord::read(&json, &alice).unwrap();
    assert!(held.current_grant(&alice).is_some());
    // The executor renews: a new grant, signed at the version its write builds on, becomes
    // current, and the old one stays in the history.
    let old = held.grant.clone().unwrap();
    let built_on = RecordVersion {
        generation: 1,
        version: 13,
    };
    let executor = keys.app("executor");
    let renewed = ExecutorGrant::sign(
        &executor.keys,
        crate::common::cert_doc("executor"),
        &alice,
        &bob,
        &held.allowance_id,
        old.terms_hash,
        old.confirmed_generation + 1,
        built_on,
    );
    let mut later = held.clone();
    later.grants.push(GrantEntry {
        built_on,
        grant: renewed.clone(),
        origin: Default::default(),
    });
    later.grant = Some(renewed);
    held.check_later_write(&later).unwrap();
    assert!(later.current_grant(&alice).is_some());
    // Written and read again: the history's grant keeps its own members, and nothing takes
    // the old current grant's.
    let written = later.to_json().unwrap();
    let again = AllowanceRecord::read(&written, &alice)
        .unwrap()
        .to_json()
        .unwrap();
    for json in [&written, &again] {
        assert_eq!(
            members_at(json, &pointers),
            vec![
                Value::Null,
                Value::Null,
                serde_json::json!("history"),
                serde_json::json!("history cert"),
            ]
        );
        assert_eq!(
            members_at(json, &["/grants/1", "/grants/1/grant"]),
            vec![Value::Null, Value::Null]
        );
        assert!(!String::from_utf8(json.clone()).unwrap().contains("current"));
    }
}

#[test]
fn test_record_list_version_pair_keeps_unknown_members() {
    // A list version inside a record is an object like any other: a writer keeps the
    // members it doesn't know as it read them (wire 5), in a Payment List record's used
    // entries and in an activity record's unrequested destination alike.
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let list = st(&arr(&s, "binding_accepted")[0], "value").to_owned();
    let unrequested = bases(&Keys::from_seeds()).unrequested.to_json().unwrap();
    type Rewrite = fn(&[u8], &[u8; 32]) -> Vec<u8>;
    let cases: [(&str, Vec<u8>, &str, Rewrite); 2] = [
        (
            "Payment List record",
            list.into_bytes(),
            "/used/0/version",
            |json, own| {
                PaymentListRecord::read(json, own)
                    .unwrap()
                    .to_json()
                    .unwrap()
            },
        ),
        (
            "activity record",
            unrequested,
            "/unrequested/list_version",
            |json, own| {
                ActivityRecord::read(json, own, ActivitySide::Payer)
                    .unwrap()
                    .to_json()
                    .unwrap()
            },
        ),
    ];
    for (name, json, pointer, rewrite) in cases {
        let mut base: Value = serde_json::from_slice(&json).unwrap();
        let pair = base
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("{name}"));
        assert!(pair.get("generation").is_some(), "{name}");
        pair["later_member"] = serde_json::json!({ "nested": ["read"] });
        let written = rewrite(&serde_json::to_vec(&base).unwrap(), &alice);
        assert_eq!(
            members_at(&written, &[pointer]),
            vec![serde_json::json!({ "nested": ["read"] })],
            "{name}"
        );
        assert_eq!(rewrite(&written, &alice), written, "{name}");
    }
}

#[test]
fn test_payment_list_record_new_used_entry_takes_no_members() {
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let mut base: Value = serde_json::from_slice(value(&arr(&s, "binding_accepted")[0])).unwrap();
    base["used"][0]["later_member"] = serde_json::json!("read");
    let held = PaymentListRecord::read(&serde_json::to_vec(&base).unwrap(), &alice).unwrap();
    // An equal entry built in code, put before the one read: it has no members, and the one
    // read keeps its own.
    let mut later = held.clone();
    let new = paykit_lib::records::UsedEndpoint {
        origin: Default::default(),
        ..held.used[0].clone()
    };
    later.used.insert(0, new);
    let written = later.to_json().unwrap();
    assert_eq!(
        members_at(&written, &["/used/0", "/used/1"]),
        vec![Value::Null, serde_json::json!("read")]
    );
}

#[test]
fn test_activity_record_half_second_anchor_names_periods_by_their_second() {
    use paykit_lib::bodies::Body;
    use paykit_lib::identifiers::AllowanceId;
    use paykit_lib::records::{reassociation_condition, Reassociation};
    use pubky_switch_core::ops::Condition;
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let nk = keys.identity(&alice).naming_key();
    let request = recurring_with(
        &keys,
        &[
            ("anchor", "2026-01-31T00:00:00.500Z"),
            ("starts_at", "2026-01-31T00:00:00.500Z"),
        ],
    );
    let Body::PaymentRequest(body) = request.body().clone() else {
        panic!("a request")
    };
    let periods = body.recurrence.as_ref().unwrap().periods(3).unwrap();
    let starts: Vec<u64> = periods.iter().map(|p| p.start_secs()).collect();
    assert_eq!(starts, [1_769_817_600, 1_772_236_800, 1_774_915_200]);
    assert!(periods.iter().all(|p| p.starts_at % 1000 == 500));
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    let record = ActivityRecord {
        request: Some(request.clone()),
        attempted_periods: vec![starts[0]],
        reassociations: vec![Reassociation {
            allowance_id: other.clone(),
            from_period: starts[1],
        }],
        ..b.payer_request.clone()
    };
    // A reassociation from a period's second reads and writes.
    let read =
        ActivityRecord::read(&record.to_json().unwrap(), &alice, ActivitySide::Payer).unwrap();
    // The period's Allowance goes by the same seconds.
    assert_eq!(
        read.allowance_for(starts[0]),
        b.payer_request.allowance_id.as_ref()
    );
    assert_eq!(read.allowance_for(starts[1]), Some(&other));
    assert_eq!(read.allowance_for(starts[2]), Some(&other));
    // The condition the write adding it carries ends at that second, before the period's
    // own start, half a second later: the period hasn't started.
    let Condition::ServerTimeIn {
        start: None,
        end: Some(end),
    } = reassociation_condition(starts[1]).unwrap()
    else {
        panic!("server_time_in")
    };
    assert!(end <= periods[1].starts_at && periods[1].starts_at - end < 1000);
    // A listed period dropped needs the condition on the claim its second names.
    let dropped = ActivityRecord {
        attempted_periods: Vec::new(),
        ..read.clone()
    };
    let claim = paykit_lib::derive::request_obligation(&nk, &bob, &request.hash(), starts[0]);
    let with = [Condition::ClaimStep {
        claim_key: claim,
        step: 3,
    }];
    assert_eq!(
        read.check_dropped_periods(&dropped, &with, &alice, &nk)
            .unwrap(),
        [starts[0]]
    );
    // The milliseconds of a period start name no period.
    let millis = ActivityRecord {
        reassociations: vec![Reassociation {
            allowance_id: other,
            from_period: periods[1].starts_at,
        }],
        ..record
    };
    assert!(millis.to_json().is_err());
}

/// The `payment_lists` record of `binding_accepted` with one `used` entry per member, each
/// entry the record's own, holding that member.
fn list_with_used(members: &[&str]) -> Vec<u8> {
    let s = bound_sealed();
    let mut base: Value = serde_json::from_slice(value(&arr(&s, "binding_accepted")[0])).unwrap();
    let used = base["used"][0].clone();
    base["used"] = Value::Array(members.iter().map(|_| used.clone()).collect());
    for (i, member) in members.iter().enumerate() {
        base["used"][i]["later_member"] = serde_json::json!(member);
    }
    serde_json::to_vec(&base).unwrap()
}

#[test]
fn test_payment_list_record_element_of_another_read_takes_no_members() {
    let alice = pubky(ALICE);
    let a = PaymentListRecord::read(&list_with_used(&["of the first"]), &alice).unwrap();
    let mut b = PaymentListRecord::read(&list_with_used(&["of the second"]), &alice).unwrap();
    // The first record's entry put into the second: it takes neither record's members.
    b.used = vec![a.used[0].clone()];
    let written = b.to_json().unwrap();
    assert_eq!(members_at(&written, &["/used/0"]), vec![Value::Null]);
    // The same from a stale read of the same record, read again once it changed.
    let stale = PaymentListRecord::read(&list_with_used(&["kept", "dropped"]), &alice).unwrap();
    let mut fresh =
        PaymentListRecord::read(&list_with_used(&["kept", "dropped", "added"]), &alice).unwrap();
    fresh.used = vec![fresh.used[0].clone(), stale.used[1].clone()];
    let written = fresh.to_json().unwrap();
    assert_eq!(
        members_at(&written, &["/used/0", "/used/1"]),
        vec![serde_json::json!("kept"), Value::Null]
    );
}

#[test]
fn test_allowance_record_history_grant_made_current_keeps_its_members() {
    let alice = pubky(ALICE);
    let s = bound_sealed();
    let entry = paykit("documents.json")["executor_grant_history_entry"].clone();
    let mut base: Value = serde_json::from_slice(value(&arr(&s, "binding_accepted")[1])).unwrap();
    base["grants"] = serde_json::json!([entry]);
    // A record read without a current grant: its history grant, made current, carries its
    // members there too, and keeps them once the record is read again.
    let without = with_members(
        &serde_json::to_vec(&base).unwrap(),
        &[("/grants/0/grant", serde_json::json!("history"))],
    );
    let held = AllowanceRecord::read(&without, &alice).unwrap();
    let mut later = held.clone();
    later.grant = Some(held.grants[0].grant.clone());
    let written = later.to_json().unwrap();
    // The grant made current from the history carries its members there too.
    assert_eq!(
        members_at(&written, &["/grant", "/grants/0/grant"]),
        vec![serde_json::json!("history"), serde_json::json!("history")]
    );
    let again = AllowanceRecord::read(&written, &alice).unwrap();
    let mut dropped = again.clone();
    dropped.grant = None;
    assert_eq!(
        members_at(&dropped.to_json().unwrap(), &["/grants/0/grant"]),
        vec![serde_json::json!("history")]
    );
}

#[test]
fn test_activity_record_attempted_entry_naming_no_period_drops_without_a_claim() {
    use pubky_switch_core::ops::Condition;
    let keys = Keys::from_seeds();
    let b = bases(&keys);
    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let nk = keys.identity(&alice).naming_key();
    // 2026-02-10, inside the February period: no period starts there, so it fences nothing.
    let between = 1_770_681_600;
    let start = 1_772_236_800;
    let held = ActivityRecord {
        attempted_periods: vec![between, start],
        ..b.payer_request.clone()
    };
    let read = ActivityRecord::read(&held.to_json().unwrap(), &alice, ActivitySide::Payer).unwrap();
    // Dropped without a claim_step: taken.
    let later = ActivityRecord {
        attempted_periods: vec![start],
        ..read.clone()
    };
    assert_eq!(
        read.check_dropped_periods(&later, &[], &alice, &nk)
            .unwrap(),
        [between]
    );
    // A real start dropped without its claim_step is still refused, and taken with it.
    let both = ActivityRecord {
        attempted_periods: Vec::new(),
        ..read.clone()
    };
    assert!(read.check_dropped_periods(&both, &[], &alice, &nk).is_err());
    let claim = paykit_lib::derive::request_obligation(
        &nk,
        &bob,
        &read.request.as_ref().unwrap().hash(),
        start,
    );
    let with = [Condition::ClaimStep {
        claim_key: claim,
        step: 2,
    }];
    assert_eq!(
        read.check_dropped_periods(&both, &with, &alice, &nk)
            .unwrap(),
        [between, start]
    );
}

const SETTLEMENT_MARKS_DESCRIPTION: &str = "Wire 1.5, 5.7, spec 6: settlement bucket \
values, their derivations and sealed encodings. accepted holds bob's one-time request, \
Billing Period and payment-without-request marks. Each settlement ID is the lower-case hex \
seed sealed/settlement_marks/<kind>/txid, except kind 3 which uses identifiers.json's \
bitcoin-txid settlement. The occurrence keys refer to the requests in messages.json; kind 2 \
uses the February period starting at 1772236800. Bucket, record key and mark tags follow \
wire 1.5 under bob's naming key. The fixed policy reads and writes paykit_settlements. \
Values are sealed at data epoch 1 with its key from keys.json, the nonce seed \
sealed/settlement_marks/<kind>/nonce cut to 24 bytes, and the Switch sealed AAD. Neither \
paykit_request nor its shop1 key opens them. example reproduces wire 5.7. refused contains \
malformed marks and the wrong collection policy, with the library's exact error. merges \
merges a seen copy into a current write: the lower mark where two name one settlement, the \
union of different marks, and over 9,000 marks the current write's settlements then the \
seen marks in ascending byte order while places remain, with no write when that equals the \
current write. These are format and merge vectors; they decide no credit and test no \
read at the switch, rollback alarm or provider-loss rule.";

/// The data epoch of bob's `paykit_settlements` key the buckets are sealed at.
const SETTLEMENTS_EPOCH: u64 = 1;

/// One credited settlement of `settlement_marks`: its case name, kind, settlement ID and the
/// natural key of the occurrence it was credited to.
struct Credited {
    name: &'static str,
    kind: OccurrenceKind,
    settlement_id: String,
    occurrence: Vec<u8>,
}

/// The seed label of a `settlement_marks` value of `kind`, by its use (`txid` or `nonce`).
fn settlement_seed_label(kind: OccurrenceKind, what: &str) -> String {
    format!("sealed/settlement_marks/{}/{what}", kind.code())
}

/// The settlements `settlement_marks` credits, one of each kind.
fn credited() -> [Credited; 3] {
    let bob = pubky(BOB);
    let txid = |kind| hex(&crate::common::seed(&settlement_seed_label(kind, "txid")));
    [
        Credited {
            name: "a one-time request's mark",
            kind: OccurrenceKind::OneTimeRequest,
            settlement_id: txid(OccurrenceKind::OneTimeRequest),
            occurrence: natural_key::request_record(
                &bob,
                &request_hash(&message_json("payment_request")),
            ),
        },
        Credited {
            name: "a Billing Period's mark",
            kind: OccurrenceKind::BillingPeriod,
            settlement_id: txid(OccurrenceKind::BillingPeriod),
            occurrence: natural_key::period_record(
                &bob,
                &request_hash(&message_json("payment_request_recurring")),
                1_772_236_800,
            ),
        },
        Credited {
            name: "a payment without a request's mark",
            kind: OccurrenceKind::PaymentWithoutRequest,
            settlement_id: UNREQUESTED_TXID.to_owned(),
            occurrence: natural_key::payee_payment_record(
                &PaymentEndpointIdentifier::parse("btc-bitcoin-p2wpkh").unwrap(),
                UNREQUESTED_TXID.as_bytes(),
            ),
        },
    ]
}

/// The bucket and mark of a credited settlement, under bob's naming key.
fn credited_mark(keys: &Keys, c: &Credited) -> (u16, SettlementMark) {
    let nk = keys.identity(&pubky(BOB)).naming_key();
    let tag = settlement_tag(&nk, c.settlement_id.as_bytes());
    let mark = SettlementMark::new(&tag, c.kind, occurrence_tag(&nk, &c.occurrence));
    (settlement_bucket(&tag), mark)
}

/// A mark as `settlement_marks` lists it.
fn mark_json(mark: &SettlementMark) -> Value {
    serde_json::json!({
        "kind": mark.kind.code(),
        "occurrence_tag_hex": hex(&mark.occurrence),
        "settlement_tag_hex": hex(&mark.settlement),
    })
}

/// A bucket value of raw mark bytes, as no reader takes it.
fn raw_bucket(bytes: &[u8]) -> String {
    format!(r#"{{"v":1,"marks":"{}"}}"#, b64(bytes))
}

/// The value of wire 5.7's example.
const SETTLEMENT_MARKS_EXAMPLE: &str =
    r#"{"v":1,"marks":"fMwzZXxKU-r3TUswAdLhLnFb3qh7uLRaxJvmJjPm0NJwAoHqru5D8S_X"}"#;

/// `settlement_marks` (Paykit wire 5.7): bob's buckets of a settlement of each kind, sealed and
/// opened; wire 5.7's example; and the values a reader refuses.
fn settlement_marks(keys: &Keys) -> Value {
    use serde_json::json;
    let bob = pubky(BOB);
    let identity = keys.identity(&bob);
    let nk = identity.naming_key();
    let scope_key = identity.scope_key(SETTLEMENTS_EPOCH, SETTLEMENTS);
    let policy = Collection::Settlements.fixed_policy();
    let mut accepted = Vec::new();
    let mut marks = Vec::new();
    // Each credited settlement's bucket.
    for c in credited() {
        let nonce_label = settlement_seed_label(c.kind, "nonce");
        let (bucket, mark) = credited_mark(keys, &c);
        marks.push(mark);
        let plaintext = raw_bucket(&mark.to_bytes()).into_bytes();
        let record_key = SettlementBucket::record_key(bucket, &nk).unwrap();
        let nonce: [u8; 24] = crate::common::seed(&nonce_label)[..24].try_into().unwrap();
        let sealed = SealedValue::seal_with_nonce(
            &scope_key,
            &bob,
            SETTLEMENTS_EPOCH,
            SETTLEMENTS,
            &record_key,
            &plaintext,
            &nonce,
        )
        .unwrap();
        assert_eq!(
            SettlementBucket::bind(&plaintext, bucket, &nk, &record_key)
                .unwrap()
                .marks(),
            [mark],
            "{}",
            c.name
        );
        accepted.push(json!({
            "aad_hex": hex(&aad(&bob, SETTLEMENTS, &record_key)),
            "bucket": bucket,
            "marks": [mark_json(&mark)],
            "name": c.name,
            "nonce_hex": hex(&nonce),
            "occurrence_natural_key_hex": hex(&c.occurrence),
            "padded_len": pad(&plaintext).unwrap().len(),
            "plaintext": String::from_utf8(plaintext).unwrap(),
            "record_key": b64(&record_key),
            "sealed": serde_json::to_value(&sealed).unwrap(),
            "settlement_id": c.settlement_id,
        }));
    }
    let example = bucket_at(&nk, SETTLEMENT_MARKS_EXAMPLE.as_bytes()).unwrap();
    assert_eq!(
        example.to_json().unwrap(),
        SETTLEMENT_MARKS_EXAMPLE.as_bytes()
    );

    // The refused values, from the accepted marks.
    marks.sort();
    let (low, high) = (marks[0].to_bytes(), marks[1].to_bytes());
    // The lower settlement's mark beside one naming the other settlement's occurrence.
    let mut twice = [marks[0], marks[0]];
    twice[1].kind = marks[1].kind;
    twice[1].occurrence = marks[1].occurrence;
    twice.sort();
    let with_kind = |kind: u8| {
        let mut bytes = low;
        bytes[12] = kind;
        bytes
    };
    let one_time = credited_mark(keys, &credited()[0]).1;
    let over: Vec<u8> = (0..=MAX_SETTLEMENT_MARKS as u128)
        .flat_map(|i| {
            let mut settlement = [0; 12];
            settlement.copy_from_slice(&i.to_be_bytes()[4..]);
            SettlementMark {
                settlement,
                kind: OccurrenceKind::OneTimeRequest,
                occurrence: one_time.occurrence,
            }
            .to_bytes()
        })
        .collect();
    let values: [(&str, String); 7] = [
        ("a bucket with no mark", raw_bucket(&[])),
        ("marks out of order", raw_bucket(&[high, low].concat())),
        (
            "a settlement twice",
            raw_bucket(&[twice[0].to_bytes(), twice[1].to_bytes()].concat()),
        ),
        (
            "a length that isn't a whole number of marks",
            raw_bucket(&[&low[..], &high[..20]].concat()),
        ),
        ("a mark of kind 0", raw_bucket(&with_kind(0))),
        ("a mark of kind 4", raw_bucket(&with_kind(4))),
        ("9,001 marks", raw_bucket(&over)),
    ];
    let mut refused: Vec<Value> = values
        .iter()
        .map(|(name, value)| {
            let error = bucket_at(&nk, value.as_bytes())
                .expect_err(name)
                .to_string();
            json!({ "name": name, "reason": quoted(&error), "value": value })
        })
        .collect();
    let unrequested = credited_mark(keys, &credited()[2]);
    let other = Collection::PayeeActivity.fixed_policy();
    assert_eq!(Collection::Settlements.policy_label(&other), None);
    let other_key = naming_record_key(&nk, unrequested.0, &other);
    let value = raw_bucket(&unrequested.1.to_bytes()).into_bytes();
    let error = SettlementBucket::bind(&value, unrequested.0, &nk, &other_key)
        .unwrap_err()
        .to_string();
    refused.push(json!({
        "bucket": unrequested.0,
        "name": "the payment without a request's bucket under payee_activity's policy",
        "policy_hash": b64(&other.hash().unwrap()),
        "read_scope": other.read_scope,
        "reason": quoted(&error),
        "record_key": b64(&other_key),
        "value": String::from_utf8(value).unwrap(),
        "write_scopes": other.write_scopes,
    }));
    json!({
        "accepted": accepted,
        "merges": settlement_mark_merges(&nk, &marks, &twice, one_time.occurrence),
        "data_epoch": SETTLEMENTS_EPOCH,
        "description": SETTLEMENT_MARKS_DESCRIPTION,
        "example": {
            "marks": example.marks().iter().map(mark_json).collect::<Vec<_>>(),
            "name": "wire 5.7's example: a one-time request's mark and a Billing Period's",
            "value": SETTLEMENT_MARKS_EXAMPLE,
        },
        "identity_hex": hex(&bob),
        "policy_hash": b64(&policy.hash().unwrap()),
        "read_scope": policy.read_scope,
        "refused": refused,
        "scope_key_hex": hex(&scope_key),
        "write_scopes": policy.write_scopes,
    })
}

/// The bucket every settlement value of `settlement_marks` not bound to its own bucket is
/// read as.
const VALUE_BUCKET: u16 = 0;

/// `json` read and bound as bucket [`VALUE_BUCKET`] at its key under `nk`.
fn bucket_at(nk: &NamingKey, json: &[u8]) -> paykit_lib::Result<SettlementBucket> {
    let key = SettlementBucket::record_key(VALUE_BUCKET, nk).unwrap();
    SettlementBucket::bind(json, VALUE_BUCKET, nk, &key)
}

/// A mark merge as `settlement_marks.merges` lists it: the write, if any, whether the bucket
/// is over capacity, the settlements named twice and whether the writer is shown.
fn merge_result(merge: SeenMerge) -> (Option<SettlementBucket>, bool, Vec<[u8; 12]>, bool) {
    match merge {
        SeenMerge::Holds { twice } => (None, false, twice, false),
        SeenMerge::WriteBack {
            bucket,
            twice,
            show_writer,
        } => (Some(bucket), false, twice, show_writer),
        SeenMerge::OverCapacity {
            bucket,
            twice,
            show_writer,
        } => (bucket, true, twice, show_writer),
    }
}

/// `settlement_marks.merges` (Paykit wire 5.7, Seen marks and Over capacity): a seen copy
/// merged into a current write, from the accepted marks, the two marks of one settlement and
/// numbered settlements of the one-time request's occurrence.
fn settlement_mark_merges(
    nk: &NamingKey,
    marks: &[SettlementMark],
    twice: &[SettlementMark; 2],
    occurrence: [u8; 8],
) -> Value {
    use serde_json::json;
    let bucket = |marks: &[SettlementMark]| {
        let mut marks = marks.to_vec();
        marks.sort();
        let bytes: Vec<u8> = marks.iter().flat_map(|m| m.to_bytes()).collect();
        bucket_at(nk, raw_bucket(&bytes).as_bytes()).unwrap()
    };
    let numbered = |i: u128| {
        let mut settlement = [0; 12];
        settlement.copy_from_slice(&i.to_be_bytes()[4..]);
        SettlementMark {
            settlement,
            kind: OccurrenceKind::OneTimeRequest,
            occurrence,
        }
    };
    let filling: Vec<SettlementMark> = (0..MAX_SETTLEMENT_MARKS as u128 - 1)
        .map(numbered)
        .collect();
    let fill = bucket(&filling);
    let candidates = bucket(&[
        numbered(MAX_SETTLEMENT_MARKS as u128),
        numbered(MAX_SETTLEMENT_MARKS as u128 - 1),
    ]);
    let SeenMerge::OverCapacity {
        bucket: Some(filled),
        ..
    } = fill.merged_with(&candidates).unwrap()
    else {
        panic!("one place remains");
    };
    let [low, high] = *twice;
    [
        (
            "a lower seen mark of a settlement replaces the current mark",
            bucket(&[high]),
            bucket(&[low]),
        ),
        (
            "a higher seen mark of a settlement leaves the current write",
            bucket(&[low]),
            bucket(&[high]),
        ),
        (
            "different marks are united",
            bucket(&marks[..1]),
            bucket(&marks[1..2]),
        ),
        (
            "one place takes the lower new mark in byte order",
            fill,
            candidates.clone(),
        ),
        (
            "no place leaves the full write as it is",
            filled,
            candidates,
        ),
    ]
    .into_iter()
    .map(|(name, current, seen)| {
        let (write, over_capacity, twice, show_writer) =
            merge_result(current.merged_with(&seen).unwrap());
        json!({
            "name": name,
            "current": String::from_utf8(current.to_json().unwrap()).unwrap(),
            "seen": String::from_utf8(seen.to_json().unwrap()).unwrap(),
            "write": write.map(|bucket| String::from_utf8(bucket.to_json().unwrap()).unwrap()),
            "over_capacity": over_capacity,
            "twice": twice.iter().map(|tag| hex(tag)).collect::<Vec<_>>(),
            "show_writer": show_writer,
        })
    })
    .collect()
}

/// Merge every `settlement_marks.merges` case's seen copy into its current write again.
fn check_settlement_mark_merges(nk: &NamingKey, cases: &Value) {
    let cases = cases.as_array().unwrap();
    assert_eq!(cases.len(), 5);
    for case in cases {
        let name = st(case, "name");
        let current = bucket_at(nk, st(case, "current").as_bytes()).unwrap();
        let seen = bucket_at(nk, st(case, "seen").as_bytes()).unwrap();
        let (write, over_capacity, twice, show_writer) =
            merge_result(current.merged_with(&seen).unwrap());
        let write = write.map(|bucket| bucket.to_json().unwrap());
        assert_eq!(
            write.as_deref(),
            case["write"].as_str().map(str::as_bytes),
            "{name}"
        );
        assert_eq!(
            Some(over_capacity),
            case["over_capacity"].as_bool(),
            "{name}"
        );
        assert_eq!(Some(show_writer), case["show_writer"].as_bool(), "{name}");
        let tags: Vec<String> = twice.iter().map(|tag| hex(tag)).collect();
        assert_eq!(serde_json::json!(tags), case["twice"], "{name}");
        // A write is never the current write again, and merging the same copy into it
        // writes nothing more.
        if let Some(write) = &write {
            assert_ne!(write.as_slice(), st(case, "current").as_bytes(), "{name}");
            let written = bucket_at(nk, write).unwrap();
            assert!(
                matches!(
                    written.merged_with(&seen).unwrap(),
                    SeenMerge::Holds { .. } | SeenMerge::OverCapacity { bucket: None, .. }
                ),
                "{name}"
            );
        }
    }
}

/// The record key of bucket `bucket` under `policy`, with `naming_key` (Switch wire 1.5).
fn naming_record_key(
    naming_key: &pubky_switch_core::ident::NamingKey,
    bucket: u16,
    policy: &pubky_switch_core::ident::Policy,
) -> [u8; 32] {
    naming_key.record_key(
        Collection::Settlements.name(),
        &natural_key::settlement_bucket(bucket),
        &policy.hash().unwrap(),
    )
}

/// `settlement_marks` (Paykit wire 5.7): each accepted bucket is its settlement's, its mark
/// that settlement's and its occurrence's, sealed byte for byte under bob's
/// `paykit_settlements` key, which alone of bob's keys opens it, and bound at its bucket's key;
/// the example reads with its marks; and every refused value is refused.
fn check_settlement_marks(keys: &Keys, m: &Value) {
    let bob = pubky(BOB);
    assert_eq!(unhex32(st(m, "identity_hex")), bob);
    let identity = keys.identity(&bob);
    let nk = identity.naming_key();
    let epoch = m["data_epoch"].as_u64().unwrap();
    let scope_key = identity.scope_key(epoch, SETTLEMENTS);
    assert_eq!(hex(&scope_key), st(m, "scope_key_hex"));
    let policy = Collection::Settlements.fixed_policy();
    assert_eq!(policy.read_scope, st(m, "read_scope"));
    assert_eq!(b64(&policy.hash().unwrap()), st(m, "policy_hash"));
    let root = identity.switch_root();
    let accepted = arr(m, "accepted");
    let kinds: Vec<u64> = accepted
        .iter()
        .map(|c| arr(c, "marks")[0]["kind"].as_u64().unwrap())
        .collect();
    assert_eq!(kinds, [1, 2, 3]);
    for case in accepted {
        let name = st(case, "name");
        let id = st(case, "settlement_id").as_bytes();
        let tag = settlement_tag(&nk, id);
        let bucket = settlement_bucket(&tag);
        assert_eq!(
            case["bucket"].as_u64().unwrap(),
            u64::from(bucket),
            "{name}"
        );
        let record_key = SettlementBucket::record_key(bucket, &nk).unwrap();
        assert_eq!(b64(&record_key), st(case, "record_key"), "{name}");
        let plaintext = st(case, "plaintext").as_bytes();
        let read = SettlementBucket::bind(plaintext, bucket, &nk, &record_key).unwrap();
        assert_eq!(read.to_json().unwrap(), plaintext, "{name}: writes back");
        let mark = read.find(&mark_settlement_tag(&tag)).copied().unwrap();
        let occurrence = crate::common::unhex(st(case, "occurrence_natural_key_hex"));
        assert_eq!(mark.occurrence, occurrence_tag(&nk, &occurrence), "{name}");
        assert_eq!(json_marks(&read), case["marks"], "{name}");
        let nonce: [u8; 24] = crate::common::unhex(st(case, "nonce_hex"))
            .try_into()
            .unwrap();
        let sealed = SealedValue::seal_with_nonce(
            &scope_key,
            &bob,
            epoch,
            SETTLEMENTS,
            &record_key,
            plaintext,
            &nonce,
        )
        .unwrap();
        let want: SealedValue = serde_json::from_value(case["sealed"].clone()).unwrap();
        assert_eq!(sealed, want, "{name}");
        assert_eq!(
            sealed.open(&scope_key, &bob, &record_key).unwrap(),
            RecordValue::Json(plaintext.to_vec()),
            "{name}"
        );
        for other in [
            key_of_scope(&root, epoch, "paykit_request"),
            key_of_scope(&root, epoch, "paykit_request:shop1"),
        ] {
            assert!(sealed.open(&other, &bob, &record_key).is_err(), "{name}");
        }
    }
    check_settlement_mark_merges(&nk, &m["merges"]);
    let example = &m["example"];
    let read = bucket_at(&nk, st(example, "value").as_bytes()).unwrap();
    assert_eq!(json_marks(&read), example["marks"]);
    let refused = arr(m, "refused");
    assert_eq!(refused.len(), 8);
    for case in refused {
        let name = st(case, "name");
        let value = st(case, "value").as_bytes();
        let error = match case.get("record_key") {
            None => bucket_at(&nk, value).unwrap_err(),
            Some(key) => {
                let policy = Policy::new(st(case, "read_scope"), &strings(&case["write_scopes"]));
                assert_eq!(
                    Collection::Settlements.policy_label(&policy),
                    None,
                    "{name}"
                );
                let bucket = case["bucket"].as_u64().unwrap() as u16;
                let key = b64n::<32>(key.as_str().unwrap());
                assert_eq!(key, naming_record_key(&nk, bucket, &policy), "{name}");
                // The value reads, and binds only at its bucket's key.
                let own = SettlementBucket::record_key(bucket, &nk).unwrap();
                SettlementBucket::bind(value, bucket, &nk, &own).unwrap();
                SettlementBucket::bind(value, bucket, &nk, &key).unwrap_err()
            }
        };
        assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
    }
}

/// A bucket's marks as `settlement_marks` lists them.
fn json_marks(bucket: &SettlementBucket) -> Value {
    Value::Array(bucket.marks().iter().map(mark_json).collect())
}

/// A JSON array of strings.
fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect()
}
