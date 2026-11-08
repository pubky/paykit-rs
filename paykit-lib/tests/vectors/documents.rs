//! `documents.json`: the executor grant and its grant history entry (Paykit wire 3.3, 5.4),
//! and Paykit's checks of certificates (Paykit wire 3.1): `certificate_rejections` and
//! `labelled_scope_rejections`.

use paykit_lib::grant::ExecutorGrant;
use paykit_lib::identifiers::AllowanceId;
use paykit_lib::records::GrantEntry;
use paykit_lib::scopes::{check_certificate, permissions_for, verify_certificate};
use pubky_switch_core::labels;
use pubky_switch_core::ops::RecordVersion;
use pubky_switch_core::records::AppCert;
use pubky_switch_core::scopes::permission_names;
use serde_json::Value;

use crate::common::{
    arr, b64n, cert, cert_doc, paykit, pubky, signed, st, Keys, ALICE, ALLOWANCE_ID, BOB,
};

#[test]
fn test_vectors_documents() {
    let keys = Keys::load();
    let d = paykit("documents.json");
    // Every app certificate of the Switch set is a Paykit App's and passes Paykit's checks:
    // its `permissions` are the list its scopes need.
    for app in &keys.apps {
        let c = cert(app);
        check_certificate(&c).unwrap_or_else(|e| panic!("{}: {e}", app.name));
        assert_eq!(c.permissions(), permissions_for(c.scopes()).as_slice());
        assert_eq!(c.scopes(), app.scopes.as_slice());
    }
    check_executor_grant(&keys, &d);
    check_certificate_rejections(&keys, &d);
    check_labelled_scope_rejections(&keys, &d);
}

/// The executor grant (Paykit wire 3.3), verified and signed again at the version its grant
/// history entry names; moved, or for another counterparty, it doesn't verify.
fn check_executor_grant(keys: &Keys, d: &Value) {
    let (alice, bob) = (pubky(ALICE), pubky(BOB));
    let allowance = AllowanceId::parse(ALLOWANCE_ID).unwrap();
    let terms = b64n::<32>(st(&paykit("identifiers.json")["terms_hash"], "terms_hash"));
    let entry = &d["executor_grant_history_entry"];
    assert_eq!(entry["grant"], d["executor_grant"]);
    let grant = ExecutorGrant::from_json(&d["executor_grant"]).unwrap();
    assert_eq!(
        grant.to_json(),
        d["executor_grant"],
        "the grant writes back"
    );
    let built_on = RecordVersion {
        generation: entry["generation"].as_u64().unwrap(),
        version: entry["version"].as_u64().unwrap(),
    };
    let history = GrantEntry {
        built_on,
        grant: grant.clone(),
    };
    let executor = keys.app("executor");
    let c = grant
        .verify(&alice, &bob, &allowance, &terms, built_on)
        .unwrap();
    assert_eq!(c, cert(executor));
    let again = ExecutorGrant::sign(
        &executor.keys,
        cert_doc("executor"),
        &alice,
        &bob,
        &allowance,
        terms,
        grant.confirmed_generation,
        history.built_on,
    );
    assert_eq!(again, grant, "the grant signs again byte for byte");
    let moved = RecordVersion {
        version: built_on.version + 1,
        ..built_on
    };
    assert!(grant
        .verify(&alice, &bob, &allowance, &terms, moved)
        .is_err());
    assert!(grant
        .verify(&alice, &alice, &allowance, &terms, built_on)
        .is_err());
    assert!(grant
        .verify(&alice, &bob, &allowance, &[0; 32], built_on)
        .is_err());
    let other = AllowanceId::parse("55de1f16-1a9e-4504-bc9c-57efc3a2b38c").unwrap();
    assert!(grant
        .verify(&alice, &bob, &other, &terms, built_on)
        .is_err());
    // A certificate without allowance_execute can't hold a grant: the phone's.
    let phone = ExecutorGrant {
        app_id: keys.app("phone").keys.app_id_bytes(),
        cert: cert_doc("phone"),
        ..grant
    };
    let error = phone
        .verify(&alice, &bob, &allowance, &terms, built_on)
        .unwrap_err();
    assert!(error.to_string().contains("allowance_execute"), "{error}");
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

/// `certificate_rejections` (Paykit wire 3.1): the wallet's certificate with `scopes` and
/// `permissions` changed and signed again; the switch takes each, Paykit refuses each for its
/// permissions.
fn check_certificate_rejections(keys: &Keys, d: &Value) {
    let alice = pubky(ALICE);
    let identity = keys.identity(&alice);
    let cases = arr(&d["certificate_rejections"], "cases");
    assert_eq!(cases.len(), 2);
    for case in cases {
        let name = st(case, "name");
        let doc = signed(&case["signed"]);
        assert_eq!(
            doc.body_bytes().unwrap(),
            st(case, "body").as_bytes(),
            "{name}"
        );
        // Signed again by alice's identity key, byte for byte.
        let again = identity.sign_doc(labels::sig::APP_CERT, &[], doc.body_bytes().unwrap());
        assert_eq!(again, doc, "{name}");
        let at_switch = AppCert::verify(&doc, &alice);
        assert_eq!(at_switch.is_ok(), case["switch_accepts"] == true, "{name}");
        let c = at_switch.unwrap();
        assert_eq!(c.scopes(), strings(&case["scopes"]).as_slice());
        assert_eq!(
            permission_names(c.permissions()),
            strings(&case["permissions"])
        );
        assert_eq!(
            permission_names(&permissions_for(c.scopes())),
            strings(&case["paykit_permissions"]),
            "{name}"
        );
        let error = verify_certificate(&doc, &alice).unwrap_err().to_string();
        assert!(
            error.contains("Paykit spec 3.2") && error.contains("permissions"),
            "{name}: {error}"
        );
    }
}

/// `labelled_scope_rejections` (Paykit wire 3.1, Switch wire 3.1): the hosted certificate
/// with its scopes changed; every verifier refuses them, the switch all but the nine labels.
fn check_labelled_scope_rejections(keys: &Keys, d: &Value) {
    let bob = pubky(BOB);
    let identity = keys.identity(&bob);
    let cases = arr(&d["labelled_scope_rejections"], "cases");
    assert_eq!(cases.len(), 5);
    for case in cases {
        let name = st(case, "name");
        let doc = signed(&case["signed"]);
        assert_eq!(
            doc.body_bytes().unwrap(),
            st(case, "body").as_bytes(),
            "{name}"
        );
        let again = identity.sign_doc(labels::sig::APP_CERT, &[], doc.body_bytes().unwrap());
        assert_eq!(again, doc, "{name}");
        let at_switch = AppCert::verify(&doc, &bob);
        assert_eq!(at_switch.is_ok(), case["switch_accepts"] == true, "{name}");
        let error = verify_certificate(&doc, &bob).unwrap_err().to_string();
        let rule = match name {
            "a label of 33 characters"
            | "a label that isn't valid (upper case)"
            | "an empty label" => "a scope that isn't a name or name:<sub>",
            "nine labelled scopes" => "Paykit wire 3.1: more than 8 labelled selling scopes",
            "plain request beside a labelled scope" => "a name beside one of its sub-scopes",
            other => panic!("unknown labelled scope rejection {other}"),
        };
        assert!(error.contains(rule), "{name}: {error}");
        if let Ok(c) = at_switch {
            // The derived permissions are right: only the label bound refuses it.
            assert_eq!(c.permissions(), permissions_for(c.scopes()).as_slice());
        }
    }
}
