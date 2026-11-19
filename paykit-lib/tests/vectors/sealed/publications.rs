//! The record reserving the shop's signed list for Alice (Paykit wire 5.8).

use paykit_lib::derive::natural_key;
use paykit_lib::records::{ListPublicationRecord, StoredMessage};
use paykit_lib::Body;
use pubky_switch_core::enc::{b64, hash, lp, u64be};
use pubky_switch_core::ident::{collection_id, record_key_from_blinded, Policy};
use pubky_switch_core::labels;
use pubky_switch_core::ops::{BatchRef, OpBody, OpKind, RecordPut, RecordVersion, SignedOp};
use pubky_switch_core::sealed::{aad, SealedValue};
use serde_json::{json, Value};

use crate::common::{
    arr, b64n, cert, hex, paykit, pubky, seed, signed, signed_json, st, Keys, ALICE, BOB,
};

const COLLECTION: &str = "private_list_publications";
const MESSAGE: &str = "private_payment_list";
const SCOPE: &str = "paykit_request";

fn publication(message: &StoredMessage) -> ListPublicationRecord {
    let Body::PrivatePaymentList(list) = message.body() else {
        panic!("expected the shop's signed list");
    };
    ListPublicationRecord {
        publisher: message.cert().app_id(),
        recipient: message.message().to,
        generation: list.version.generation(),
        counter: list.version.counter(),
        msg_id: message.message().msg_id.clone(),
        message_hash: message.hash(),
        unknown: Default::default(),
    }
}

/// The fixed-generation policy and private address for the signed list in messages.json.
pub(crate) fn address(keys: &Keys) -> Value {
    let record = publication(&super::stored_message(keys, MESSAGE));
    let naming = keys.identity_named("bob").naming_key();
    let natural =
        natural_key::list_publication(&record.publisher, &record.recipient, record.generation);
    let generation = u64be(record.generation);
    assert_eq!(
        natural,
        lp(&[&record.publisher, &record.recipient, &generation])
    );
    let policy = Policy::new(SCOPE, &[SCOPE]).permanent(Some(record.generation));
    let material = lp(&[
        labels::derive::PERMANENT_POLICY.as_bytes(),
        SCOPE.as_bytes(),
        &u64be(1),
        SCOPE.as_bytes(),
        &generation,
    ]);
    let digest = policy.hash().unwrap();
    assert_eq!(hash(&material), digest);
    let blinded = naming.blind(&natural);
    let key = naming.record_key(COLLECTION, &natural, &digest);
    assert_eq!(key, record_key_from_blinded(COLLECTION, &blinded, &digest));
    json!({
        "name": "shop publication for Alice", "collection": COLLECTION, "identity": BOB,
        "publisher": keys.app("shop").app_id, "recipient": ALICE, "generation": record.generation,
        "natural_key": b64(&natural), "natural_key_hex": hex(&natural),
        "blinded_natural_key": b64(&blinded), "policy": policy, "policy_hash": b64(&digest),
        "policy_material_hex": hex(&material),
        "key_material_hex": hex(&lp(&[COLLECTION.as_bytes(), &blinded, &digest])),
        "record_key": b64(&key),
    })
}

pub(super) fn cases(keys: &Keys) -> Value {
    let list = super::stored_message(keys, MESSAGE);
    let record = publication(&list);
    let identifiers = paykit("identifiers.json");
    let address = arr(&identifiers["record_keys"], "cases")
        .iter()
        .find(|case| case["collection"] == COLLECTION)
        .unwrap();
    let policy: Policy = serde_json::from_value(address["policy"].clone()).unwrap();
    let key = b64n::<32>(st(address, "record_key"));
    let other_natural =
        natural_key::list_publication(&record.publisher, &pubky(BOB), record.generation);
    let owner = keys.identity_named("bob");
    let other_key =
        owner
            .naming_key()
            .record_key(COLLECTION, &other_natural, &policy.hash().unwrap());
    let plaintext = record.to_json().unwrap();
    let mut writes = Vec::new();
    for (name, app, at, accepted) in [
        ("publisher", "shop", key, true),
        ("another authorized app", "cap", key, false),
        ("another recipient's key", "shop", other_key, false),
    ] {
        let writer = keys.app(app);
        let fixture = format!("sealed/private_list_publications/{name}");
        let nonce = seed(&format!("{fixture}/nonce"))[..24].try_into().unwrap();
        let value_sealed = SealedValue::seal_with_nonce(
            &owner.scope_key(1, SCOPE),
            &owner.public(),
            1,
            SCOPE,
            &at,
            &plaintext,
            &nonce,
        )
        .unwrap();
        let op = SignedOp::sign(
            &writer.keys,
            OpBody {
                identity: owner.public(),
                app_id: Some(writer.keys.app_id_bytes()),
                generation: record.generation,
                batch: BatchRef {
                    id: seed(&format!("{fixture}/batch"))[..16].try_into().unwrap(),
                    count: 1,
                },
                conditions: vec![],
                kind: OpKind::RecordPut(RecordPut {
                    key: at,
                    collection: collection_id(COLLECTION, &owner.public()),
                    policy: policy.clone(),
                    expected_version: RecordVersion::new(record.generation, 11),
                    value_sealed,
                    id_inputs: None,
                }),
            },
        )
        .unwrap();
        writes.push(json!({
            "name": name,
            "writer": app,
            "accepted": accepted,
            "record_key": b64(&at),
            "aad_hex": hex(&aad(&owner.public(), SCOPE, &at)),
            "nonce_hex": hex(&nonce),
            "operation": signed_json(op.signed()),
        }));
    }
    json!({
        "message": MESSAGE,
        "plaintext": String::from_utf8(plaintext).unwrap(),
        "writes": writes,
        "later_writes": later_writes(keys, &record),
    })
}

fn later_writes(keys: &Keys, held: &ListPublicationRecord) -> Vec<Value> {
    let other_id = "9871f930-8d33-4451-8649-d56a24e1f007";
    let withdrawal = |counter, id: &str| {
        let list = super::resign_with(keys, MESSAGE, |m| {
            m.msg_id = id.to_owned();
            let version = paykit_lib::ListVersion::new(held.generation, counter).unwrap();
            m.body.insert("version".into(), version.to_value());
            m.body.insert("payment_endpoints".into(), json!({}));
        });
        publication(&list)
    };
    let other_id_only = ListPublicationRecord {
        msg_id: other_id.into(),
        ..held.clone()
    };
    [
        ("exact repeat", held.clone(), true),
        (
            "lower counter",
            withdrawal(held.counter - 1, other_id),
            false,
        ),
        (
            "equal counter with another hash",
            withdrawal(held.counter, &held.msg_id),
            false,
        ),
        (
            "equal counter with another message ID",
            other_id_only,
            false,
        ),
        (
            "newer counter",
            withdrawal(held.counter + 1, other_id),
            true,
        ),
    ]
    .into_iter()
    .map(|(name, next, accepted)| {
        json!({
            "name": name,
            "accepted": accepted,
            "plaintext": String::from_utf8(next.to_json().unwrap()).unwrap(),
        })
    })
    .collect()
}

pub(super) fn check(keys: &Keys, cases: &Value) {
    use pubky_switch_core::sealed::RecordValue;

    let owner = keys.identity_named("bob");
    let held = ListPublicationRecord::read(st(cases, "plaintext").as_bytes()).unwrap();
    assert_eq!(held, publication(&super::stored_message(keys, MESSAGE)));
    let policy = Policy::new(SCOPE, &[SCOPE]).permanent(Some(held.generation));
    let natural = natural_key::list_publication(&held.publisher, &held.recipient, held.generation);
    let expected_key = owner
        .naming_key()
        .record_key(COLLECTION, &natural, &policy.hash().unwrap());
    for case in arr(cases, "writes") {
        let certificate = cert(keys.app(st(case, "writer")));
        assert!(certificate.holds_scope(SCOPE));
        let op =
            SignedOp::verify_with_cert(&signed(&case["operation"]), &certificate, &owner.public())
                .unwrap();
        let OpKind::RecordPut(put) = &op.body().kind else {
            panic!("expected record.put")
        };
        assert_eq!(put.collection, collection_id(COLLECTION, &owner.public()));
        assert_eq!(put.policy, policy);
        assert_eq!(put.key, b64n::<32>(st(case, "record_key")));
        let opened = put
            .value_sealed
            .open(&owner.scope_key(1, SCOPE), &owner.public(), &put.key)
            .unwrap();
        assert_eq!(opened, RecordValue::Json(held.to_json().unwrap()));
        let bound = op.body().app_id == Some(held.publisher) && put.key == expected_key;
        assert_eq!(case["accepted"], bound, "{}", st(case, "name"));
    }
    for case in arr(cases, "later_writes") {
        let later = ListPublicationRecord::read(st(case, "plaintext").as_bytes()).unwrap();
        assert_eq!(
            case["accepted"],
            held.check_later_write(&later).is_ok(),
            "{}",
            st(case, "name")
        );
    }
}
