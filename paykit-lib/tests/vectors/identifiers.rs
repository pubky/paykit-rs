//! `identifiers.json`: request hashes, obligation IDs with their blinded inputs, and the
//! terms hash (Paykit wire 1.5); and the record keys of Paykit's natural keys and the
//! policies of its collections (Switch wire 1.5).

use paykit_lib::collections::Collection;
use paykit_lib::derive::{
    mark_settlement_tag, natural_key, occurrence_tag, payment_obligation,
    payment_obligation_material, request_hash, request_obligation, request_obligation_material,
    settlement_bucket, settlement_tag, settlement_tag_blinded, settlement_tag_material, terms_hash,
    terms_hash_material,
};
use paykit_lib::identifiers::{AllowanceId, AttemptId, PaymentEndpointIdentifier};
use paykit_lib::records::SettlementBucket;
use paykit_lib::scopes::Label;
use pubky_switch_core::enc::{b64, hash, lp, pubky_id, u64be};
use pubky_switch_core::ident::{record_key_from_blinded, Policy};
use pubky_switch_core::labels;
use serde_json::{json, Value};

use crate::common::{
    arr, b64n, generated, hex, message_json, paykit, pubky, st, writing, Keys, Place, ALICE,
    ALLOWANCE_ID, BOB, SHOP1, UNREQUESTED_TXID,
};

#[test]
fn test_vectors_identifiers() {
    if !writing() {
        check();
    }
}

/// `identifiers.json` against paykit-lib: its generated cases, then every check of its cases. With
/// [`WRITE_VECTORS`](crate::common::WRITE_VECTORS) set, the regeneration driver runs it once it
/// has written the set.
pub(crate) fn check() {
    let keys = Keys::from_seeds();
    generate(&keys);
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

    check_policies(&v);
}

/// The cases paykit-lib generates in `identifiers.json`, compared with the file, and written
/// into the set by the regeneration driver (`regenerate.rs`).
pub(crate) fn generate(keys: &Keys) {
    let mut cases = vec![
        (Place::Member("/policies"), policies()),
        (Place::Member("/record_keys"), record_keys(keys)),
    ];
    cases.extend(derived(keys));
    generated("identifiers.json", &cases);
}

/// The request hashes, the obligation IDs with their blinded inputs, and the terms hash, from
/// the authored members beside them: the messages they name, the identity, the payee, the
/// period start and the attempt ID.
fn derived(keys: &Keys) -> Vec<(Place<'static>, Value)> {
    let v = paykit("identifiers.json");
    let mut out = Vec::new();
    for (i, case) in arr(&v, "request_hash").iter().enumerate() {
        out.push((
            Place::At(format!("/request_hash/{i}/hash")),
            json!(b64(&request_hash(&message_json(st(case, "message"))))),
        ));
    }
    for (i, case) in arr(&v, "obligations").iter().enumerate() {
        let at = |member: &str| Place::At(format!("/obligations/{i}/{member}"));
        let nk = keys.identity(&pubky(st(case, "identity"))).naming_key();
        let (material, obligation) = match case.get("attempt_id") {
            Some(attempt) => {
                let attempt = AttemptId::parse(attempt.as_str().unwrap()).unwrap();
                (
                    payment_obligation_material(&attempt),
                    payment_obligation(&attempt),
                )
            }
            None => {
                let message = match st(case, "name") {
                    "one-time request" => "payment_request",
                    "recurring request, February period" => "payment_request_recurring",
                    other => panic!("no request for the obligation {other}"),
                };
                let rh = request_hash(&message_json(message));
                let payee = pubky(st(case, "payee"));
                let start = case["period_start_secs"].as_u64().unwrap();
                out.push((at("request_hash"), json!(b64(&rh))));
                out.push((at("blinded_payee"), json!(b64(&nk.blind(&payee)))));
                out.push((at("blinded_request_hash"), json!(b64(&nk.blind(&rh)))));
                (
                    request_obligation_material(&nk, &payee, &rh, start),
                    request_obligation(&nk, &payee, &rh, start),
                )
            }
        };
        out.push((at("key_material_hex"), json!(hex(&material))));
        out.push((at("obligation"), json!(b64(&obligation))));
    }
    let t = &v["terms_hash"];
    let (proposal, acceptance) = (
        message_json(st(t, "proposal")),
        message_json(st(t, "acceptance")),
    );
    out.push((
        Place::At("/terms_hash/key_material_hex".into()),
        json!(hex(&terms_hash_material(&proposal, &acceptance))),
    ));
    out.push((
        Place::At("/terms_hash/terms_hash".into()),
        json!(b64(&terms_hash(&proposal, &acceptance))),
    ));
    out
}

const POLICIES_DESCRIPTION: &str = "Wire 1.5, Switch wire 1.5: the policies of Paykit's \
collections, each with its read scope, its write scopes and its policy hash, \
hash(lp(\"pubky-switch/v1/policy\", read_scope, u64(n), write_scope_1, ..., write_scope_n)), \
hashed_bytes_hex being the bytes hashed: the fixed policy of each collection, by its name, \
settlements' the one policy of every payee app's bucket writes, labelled or not, and, last, \
payee_activity's policy for a request labelled shop1.";

/// `policies`: each collection's fixed policy, and `payee_activity`'s under the label
/// `shop1`.
fn policies() -> Value {
    let shop1 = Label::parse(SHOP1).unwrap();
    let mut named: Vec<(String, Policy)> = Collection::ALL
        .iter()
        .map(|c| (c.name().to_owned(), c.fixed_policy()))
        .collect();
    named.push((
        format!("payee_activity of a request labelled {SHOP1}"),
        Collection::PayeeActivity.policy(Some(&shop1)),
    ));
    let cases: Vec<Value> = named
        .iter()
        .map(|(name, policy)| {
            let count = u64be(policy.write_scopes.len() as u64);
            let mut parts: Vec<&[u8]> = vec![
                labels::derive::POLICY.as_bytes(),
                policy.read_scope.as_bytes(),
                &count,
            ];
            parts.extend(policy.write_scopes.iter().map(String::as_bytes));
            let hashed = lp(&parts);
            let policy_hash = policy.hash().unwrap();
            assert_eq!(hash(&hashed), policy_hash, "{name}");
            json!({
                "hashed_bytes_hex": hex(&hashed),
                "name": name,
                "policy_hash": b64(&policy_hash),
                "read_scope": policy.read_scope,
                "write_scopes": policy.write_scopes,
            })
        })
        .collect();
    json!({ "cases": cases, "description": POLICIES_DESCRIPTION })
}

/// `policies`: each policy is its collection's as paykit-lib names it, and hashes to its
/// hash.
fn check_policies(v: &Value) {
    let shop1 = Label::parse(SHOP1).unwrap();
    let cases = arr(&v["policies"], "cases");
    assert_eq!(cases.len(), Collection::ALL.len() + 1);
    for case in cases {
        let name = st(case, "name");
        let policy = match Collection::from_name(name) {
            Some(c) => c.fixed_policy(),
            None => Collection::PayeeActivity.policy(Some(&shop1)),
        };
        assert_eq!(policy.read_scope, st(case, "read_scope"), "{name}");
        assert_eq!(json!(policy.write_scopes), case["write_scopes"], "{name}");
        assert_eq!(
            b64(&policy.hash().unwrap()),
            st(case, "policy_hash"),
            "{name}"
        );
    }
}

const RECORD_KEYS_DESCRIPTION: &str = "Wire 1.5, Switch wire 1.5: the record keys of \
Paykit's natural keys, each in its collection under its policy, blinded with the naming key \
of the identity whose switch holds it (identity, keys.json): natural_key (and \
natural_key_hex) is the natural key, blinded_natural_key its blind, \
derive(\"pubky-switch/v1/name\", naming_key, natural_key), key_material_hex the input of \
the record key, lp(collection, blinded_natural_key, policy_hash), and record_key \
derive(\"pubky-switch/v1/record\", collection, blinded_natural_key, policy_hash). The \
request hashes are those of messages.json's payment_request, payment_request_recurring \
(its February period, 1772236800) and payment_request_labelled, whose payee record is under \
the policy of its label shop1. The last is the settlements bucket of the bitcoin-txid \
settlement of the payment without a request, its settlement_id, held at bob's switch: \
settlement_tag_input_hex is lp(settlement ID), blinded_settlement_tag_input its blind, \
settlement_tag_hex the settlement's tag, derive(\"paykit/v1/settlement\", \"tag\", \
blinded_settlement_tag_input), bucket u16_be(tag[0..2]) >> 6, mark_settlement_tag_hex bytes 2 \
to 13 of the tag, which its mark stores, and natural_key lp(u64(bucket)); occurrence_tag_hex \
is the first 8 bytes of derive(\"paykit/v1/settlement\", \"occurrence\", \
occurrence_blinded_natural_key), the blind of the natural key of the occurrence it was \
credited to, bob's payee record of that payment.";

/// `record_keys` (Paykit wire 1.5): Paykit's natural keys, each blinded into its record key
/// with the naming key of the identity whose switch holds it: bob's for his payee records,
/// alice's for every other.
fn record_keys(keys: &Keys) -> Value {
    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let one_time = request_hash(&message_json("payment_request"));
    let recurring = request_hash(&message_json("payment_request_recurring"));
    let labelled = request_hash(&message_json("payment_request_labelled"));
    let allowance = AllowanceId::parse(ALLOWANCE_ID).unwrap();
    let attempt = AttemptId::parse("mAFdg5_0tVNjmrgiTUZW4g").unwrap();
    let shop = keys.app("shop").keys.app_id_bytes();
    let txid = UNREQUESTED_TXID;
    let shop1 = Label::parse(SHOP1).unwrap();
    let named: [(&str, Collection, Vec<u8>, Option<&Label>); 10] = [
        (
            "payer request record",
            Collection::PayerActivity,
            natural_key::request_record(&bob, &one_time),
            None,
        ),
        (
            "payer period record (February)",
            Collection::PayerActivity,
            natural_key::period_record(&bob, &recurring, 1_772_236_800),
            None,
        ),
        (
            "payer record of a payment without a request",
            Collection::PayerActivity,
            natural_key::payer_payment_record(&alice, &attempt),
            None,
        ),
        (
            "payee request record",
            Collection::PayeeActivity,
            natural_key::request_record(&bob, &one_time),
            None,
        ),
        (
            "payee record of a payment without a request",
            Collection::PayeeActivity,
            natural_key::payee_payment_record(
                &PaymentEndpointIdentifier::parse("btc-bitcoin-p2wpkh").unwrap(),
                txid.as_bytes(),
            ),
            None,
        ),
        (
            "payment_lists record of the shop app",
            Collection::PaymentLists,
            natural_key::payment_list(&bob, &shop),
            None,
        ),
        (
            "pending Allowance proposal",
            Collection::AllowanceProposals,
            natural_key::allowance(&bob, &allowance),
            None,
        ),
        (
            "Allowance record",
            Collection::Allowances,
            natural_key::allowance(&bob, &allowance),
            None,
        ),
        (
            "Allowance ledger",
            Collection::AllowanceLedgers,
            natural_key::allowance(&bob, &allowance),
            None,
        ),
        (
            "payee request record of the request labelled shop1",
            Collection::PayeeActivity,
            natural_key::request_record(&bob, &labelled),
            Some(&shop1),
        ),
    ];
    let cases: Vec<Value> = named
        .iter()
        .map(|(name, collection, natural, label)| {
            let holder = match collection {
                Collection::PayeeActivity => bob,
                _ => alice,
            };
            let nk = keys.identity(&holder).naming_key();
            let policy_hash = collection.policy(*label).hash().unwrap();
            let blinded = nk.blind(natural);
            let material = lp(&[collection.name().as_bytes(), &blinded, &policy_hash]);
            let key = collection.record_key(&nk, natural, *label).unwrap();
            assert_eq!(
                record_key_from_blinded(collection.name(), &blinded, &policy_hash),
                key,
                "{name}"
            );
            json!({
                "blinded_natural_key": b64(&blinded),
                "collection": collection.name(),
                "identity": pubky_id(&holder),
                "key_material_hex": hex(&material),
                "name": name,
                "natural_key": b64(natural),
                "natural_key_hex": hex(natural),
                "policy_hash": b64(&policy_hash),
                "record_key": b64(&key),
            })
        })
        .collect();
    let mut cases = cases;
    cases.push(settlement_bucket_case(keys, txid));
    cases.push(crate::sealed::publications::address(keys));
    json!({ "cases": cases, "description": RECORD_KEYS_DESCRIPTION })
}

/// The `record_keys` case of the `settlements` bucket of the payment without a request's
/// `bitcoin-txid` settlement (Paykit wire 1.5, 5.7): its tag and bucket, the bucket's record
/// key under bob's naming key, and the tag of the occurrence it was credited to, bob's payee
/// record of that payment.
fn settlement_bucket_case(keys: &Keys, txid: &str) -> Value {
    let bob = pubky(BOB);
    let nk = keys.identity(&bob).naming_key();
    let id = txid.as_bytes();
    let material = settlement_tag_material(id);
    let blinded_id = nk.blind(&material);
    let tag = settlement_tag(&nk, id);
    assert_eq!(settlement_tag_blinded(&blinded_id), tag);
    let bucket = settlement_bucket(&tag);
    let natural = natural_key::settlement_bucket(bucket);
    let collection = Collection::Settlements;
    let policy_hash = collection.fixed_policy().hash().unwrap();
    let blinded = nk.blind(&natural);
    let key = SettlementBucket::record_key(bucket, &nk).unwrap();
    assert_eq!(
        record_key_from_blinded(collection.name(), &blinded, &policy_hash),
        key
    );
    let occurrence = natural_key::payee_payment_record(
        &PaymentEndpointIdentifier::parse("btc-bitcoin-p2wpkh").unwrap(),
        id,
    );
    json!({
        "blinded_natural_key": b64(&blinded),
        "blinded_settlement_tag_input": b64(&blinded_id),
        "bucket": bucket,
        "collection": collection.name(),
        "identity": pubky_id(&bob),
        "key_material_hex": hex(&lp(&[collection.name().as_bytes(), &blinded, &policy_hash])),
        "mark_settlement_tag_hex": hex(&mark_settlement_tag(&tag)),
        "name": SETTLEMENT_BUCKET_CASE,
        "natural_key": b64(&natural),
        "natural_key_hex": hex(&natural),
        "occurrence": "payee record of a payment without a request",
        "occurrence_blinded_natural_key": b64(&nk.blind(&occurrence)),
        "occurrence_tag_hex": hex(&occurrence_tag(&nk, &occurrence)),
        "policy_hash": b64(&policy_hash),
        "record_key": b64(&key),
        "settlement_id": txid,
        "settlement_tag_hex": hex(&tag),
        "settlement_tag_input_hex": hex(&material),
    })
}

/// The name of the `settlements` case of `record_keys`.
const SETTLEMENT_BUCKET_CASE: &str =
    "settlements bucket of the bitcoin-txid settlement of the payment without a request";
