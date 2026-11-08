//! `identifiers.json`: request hashes, obligation IDs with their blinded inputs, and the
//! terms hash (Paykit wire 1.5); and the record keys and policies of Paykit's natural keys
//! and collections, which the Switch set's `identifiers.json` holds.

use paykit_lib::collections::Collection;
use paykit_lib::derive::{
    natural_key, payment_obligation, payment_obligation_material, request_hash, request_obligation,
    request_obligation_material, terms_hash, terms_hash_material,
};
use paykit_lib::identifiers::{AllowanceId, AttemptId, PaymentEndpointIdentifier};
use paykit_lib::scopes::Label;
use pubky_switch_core::enc::b64;

use crate::common::{
    arr, b64n, hex, message_json, paykit, pubky, st, switch, Keys, ALLOWANCE_ID, BOB,
};

#[test]
fn test_vectors_identifiers() {
    let keys = Keys::load();
    let v = paykit("identifiers.json");
    for case in arr(&v, "request_hash") {
        assert_eq!(
            b64(&request_hash(&message_json(st(case, "message")))),
            st(case, "hash")
        );
    }
    let obligations = arr(&v, "obligations");
    assert_eq!(obligations.len(), 3);
    for case in obligations {
        let name = st(case, "name");
        // Claims sit in the payer's switch: blinded with the payer's naming key.
        let nk = keys.identity(&pubky(st(case, "identity"))).naming_key();
        let (material, obligation) = match case.get("attempt_id") {
            Some(attempt) => {
                // From the attempt ID alone: no payee, no naming key.
                assert!(case.get("payee").is_none() && case.get("blinded_payee").is_none());
                let attempt = AttemptId::parse(attempt.as_str().unwrap()).unwrap();
                (
                    payment_obligation_material(&attempt),
                    payment_obligation(&attempt),
                )
            }
            None => {
                let payee = pubky(st(case, "payee"));
                let rh = b64n::<32>(st(case, "request_hash"));
                assert_eq!(b64(&nk.blind(&payee)), st(case, "blinded_payee"), "{name}");
                assert_eq!(
                    b64(&nk.blind(&rh)),
                    st(case, "blinded_request_hash"),
                    "{name}"
                );
                // The payee's own naming key blinds to another value.
                let payee_nk = keys.identity(&payee).naming_key();
                assert_ne!(b64(&payee_nk.blind(&payee)), st(case, "blinded_payee"));
                let start = case["period_start_secs"].as_u64().unwrap();
                (
                    request_obligation_material(&nk, &payee, &rh, start),
                    request_obligation(&nk, &payee, &rh, start),
                )
            }
        };
        assert_eq!(hex(&material), st(case, "key_material_hex"), "{name}");
        assert_eq!(b64(&obligation), st(case, "obligation"), "{name}");
    }
    let t = &v["terms_hash"];
    let (proposal, acceptance) = (
        message_json(st(t, "proposal")),
        message_json(st(t, "acceptance")),
    );
    assert_eq!(
        hex(&terms_hash_material(&proposal, &acceptance)),
        st(t, "key_material_hex")
    );
    assert_eq!(
        b64(&terms_hash(&proposal, &acceptance)),
        st(t, "terms_hash")
    );

    check_switch_policies();
    check_switch_record_keys(&keys);
}

/// The Switch set's `policies`: Paykit's fixed policies, and `payee_activity` under the
/// label `shop1`, hash as the Switch set gives them.
fn check_switch_policies() {
    let s = switch("identifiers.json");
    let shop1 = Label::parse("shop1").unwrap();
    let mut seen = 0;
    for case in arr(&s, "policies") {
        let policy = match st(case, "name") {
            "payee_activity of a request labelled shop1" => {
                Collection::PayeeActivity.policy(Some(&shop1))
            }
            name => match Collection::from_name(name) {
                Some(c) => c.fixed_policy(),
                None => continue,
            },
        };
        assert_eq!(policy.read_scope, st(case, "read_scope"));
        assert_eq!(b64(&policy.hash().unwrap()), st(case, "policy_hash"));
        seen += 1;
    }
    assert_eq!(seen, 6, "five fixed policies and one labelled");
}

/// The Switch set's `record_keys`, whose natural keys are Paykit's (Paykit wire 1.5).
fn check_switch_record_keys(keys: &Keys) {
    let s = switch("identifiers.json");
    let bob = pubky(BOB);
    let alice_id = "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy";
    let alice = pubky(alice_id);
    let one_time = request_hash(&message_json("payment_request"));
    let recurring = request_hash(&message_json("payment_request_recurring"));
    let labelled = request_hash(&message_json("payment_request_labelled"));
    let allowance = AllowanceId::parse(ALLOWANCE_ID).unwrap();
    let attempt = AttemptId::parse("mAFdg5_0tVNjmrgiTUZW4g").unwrap();
    let shop = keys.app("shop").keys.app_id_bytes();
    let txid = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
    let shop1 = Label::parse("shop1").unwrap();
    let cases = arr(&s, "record_keys");
    assert_eq!(cases.len(), 9);
    for case in cases {
        let name = st(case, "name");
        let (collection, natural, label) = match name {
            "payer request record" => (
                Collection::PayerActivity,
                natural_key::request_record(&bob, &one_time),
                None,
            ),
            "payer period record (February)" => (
                Collection::PayerActivity,
                natural_key::period_record(&bob, &recurring, 1_772_236_800),
                None,
            ),
            "payer record of a payment without a request" => (
                Collection::PayerActivity,
                natural_key::payer_payment_record(&alice, &attempt),
                None,
            ),
            "payee request record" => (
                Collection::PayeeActivity,
                natural_key::request_record(&bob, &one_time),
                None,
            ),
            "payee record of a payment without a request" => (
                Collection::PayeeActivity,
                natural_key::payee_payment_record(
                    &PaymentEndpointIdentifier::parse("btc-bitcoin-p2wpkh").unwrap(),
                    txid.as_bytes(),
                ),
                None,
            ),
            "payment_lists record of the shop app" => (
                Collection::PaymentLists,
                natural_key::payment_list(&bob, &shop),
                None,
            ),
            "Allowance record" => (
                Collection::Allowances,
                natural_key::allowance(&bob, &allowance),
                None,
            ),
            "Allowance ledger" => (
                Collection::AllowanceLedgers,
                natural_key::allowance(&bob, &allowance),
                None,
            ),
            "payee request record of the request labelled shop1" => (
                Collection::PayeeActivity,
                natural_key::request_record(&bob, &labelled),
                Some(&shop1),
            ),
            other => panic!("unknown record key case {other}"),
        };
        assert_eq!(st(case, "collection"), collection.name(), "{name}");
        assert_eq!(hex(&natural), st(case, "natural_key_hex"), "{name}");
        let nk = keys.identity(&pubky(st(case, "identity"))).naming_key();
        let key = collection.record_key(&nk, &natural, label).unwrap();
        assert_eq!(b64(&key), st(case, "record_key"), "{name}");
        assert_eq!(
            b64(&collection.policy(label).hash().unwrap()),
            st(case, "policy_hash"),
            "{name}"
        );
    }
}
