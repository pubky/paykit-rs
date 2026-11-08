//! `sealed.json`: record values a reader binds to their keys, the rejections, and the
//! labelled payee record (Paykit wire 5, 5.1, 5.3, 5.4, 5.5).

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
    arr, b64n, hex, message_json, paykit, pubky, st, switch, unhex32, Keys, ALICE, BOB,
};

#[test]
fn test_vectors_sealed() {
    let keys = Keys::load();
    let v = paykit("sealed.json");
    check_bindings(&keys, &v);
    check_labelled(&keys, &v["labelled"]);
    check_activity_rules(&keys);
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
    let read = |record: &ActivityRecord| {
        ActivityRecord::read(&record.to_json().unwrap(), &alice, ActivitySide::Payer)
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
    assert!(ActivityRecord::read(&payee.to_json().unwrap(), &bob, ActivitySide::Payee).is_err());
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
