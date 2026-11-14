//! `documents.json`: the executor grant and its grant history entry (Paykit wire 3.3, 5.4),
//! Paykit's checks of certificates (Paykit wire 3.1): `certificate_rejections`,
//! `labelled_scope_rejections` and `multi_protocol_certificates`, and of app records' Paykit
//! member (Paykit wire 3.2): `app_records`.

use paykit_lib::app_record::{PaykitMember, PROTOCOL_NAME};
use paykit_lib::grant::ExecutorGrant;
use paykit_lib::identifiers::AllowanceId;
use paykit_lib::records::GrantEntry;
use paykit_lib::scopes::{
    check_certificate, paykit_scopes, permissions_for, verify_certificate, PaykitScope,
};
use pubky_switch_core::labels;
use pubky_switch_core::ops::RecordVersion;
use pubky_switch_core::records::{AppCert, AppRecord};
use pubky_switch_core::scopes::{permission_names, Permission};
use pubky_switch_core::sig::SignedDoc;
use serde_json::{json, Value};

use crate::common::{
    arr, b64n, base_cert, body_text, cap_app_record, cert, cert_doc, executor_cert, generated,
    paykit, phone_cert, pubky, quoted, recertified, reissued, shop_app_record, signed, signed_json,
    st, Keys, Place, ALICE, ALLOWANCE_ID, BOB,
};

#[test]
fn test_vectors_documents() {
    let keys = Keys::from_seeds();
    generate(&keys);
    let d = paykit("documents.json");
    // Every app certificate of this set, the base certificates but the executor's and the
    // phone's, which are Paykit's own, is a Paykit App's and passes Paykit's checks: its
    // `permissions` are the list its scopes need.
    for app in &keys.apps {
        let c = cert(app);
        check_certificate(&c).unwrap_or_else(|e| panic!("{}: {e}", app.name));
        assert_eq!(c.permissions(), permissions_for(c.scopes()).as_slice());
        assert_eq!(c.scopes(), crate::common::scopes_of(app).as_slice());
    }
    check_executor_grant(&keys, &d);
    check_executor_certificates(&keys, &d);
    check_payer_certificates(&keys, &d);
    check_certificate_rejections(&keys, &d);
    check_labelled_scope_rejections(&keys, &d);
    check_multi_protocol_certificates(&d);
    check_app_records(&keys, &d);
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
        origin: Default::default(),
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
    assert!(
        error.to_string().contains("paykit_allowance_execute"),
        "{error}"
    );
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
            "plain paykit_request beside a labelled scope" => "a name beside one of its sub-scopes",
            other => panic!("unknown labelled scope rejection {other}"),
        };
        assert!(error.contains(rule), "{name}: {error}");
        if let Ok(c) = at_switch {
            // The derived permissions are right: only the label bound refuses it.
            assert_eq!(c.permissions(), permissions_for(c.scopes()).as_slice());
        }
    }
}

/// The top-level description, which names every member.
const DESCRIPTION: &str = "Paykit's signed documents and Paykit's checks of the Switch's \
certificates and app records (wire 3): the executor grant, alone and as the Allowance \
record's grant history holds it, with Paykit's own executor certificate, and the executor's \
certificate without paykit_contacts, which Paykit's verifiers refuse; Paykit's own phone certificate, and certificates \
holding paykit_pay without paykit_contacts, which they refuse; certificates that Paykit's verifiers refuse for their \
scopes or their permissions, certificates that hold another protocol's scopes beside \
Paykit's or instead of them, and app records as a Paykit reader reads their Paykit member. \
The certificates are this file's certificates with scopes or permissions changed, and the \
app records the shop's app record with its Paykit member changed.";

const MULTI_PROTOCOL_DESCRIPTION: &str = "Wire 3.1, spec 3.2, Switch wire 3.1 \"Several \
protocols\" and \"Scope roots\": certificates that hold another protocol's scopes beside \
Paykit's or instead of them, and one whose extension prefix doesn't start with paykit-, as \
Paykit's \
verifiers check them. Each is the shop's certificate of this file's certificates (the \
last, the cap's) with scopes and permissions changed, signed again by bob's \
identity key; the switch takes every one (switch_accepts). A scope whose root, the part \
before any :, isn't Paykit's (paykit, or paykit followed by _ or - and more characters) is \
another protocol's, such as chat. \
paykit_scopes are the scopes Paykit reads, and paykit_permissions the list they need. A \
Paykit verifier requires every permission of paykit_permissions, and, when the certificate \
holds no other protocol's scope, permissions exactly that list. accepted are certificates \
Paykit's verifiers take: paykit_request beside chat, with lists.read, which chat needs, and \
conversation.add, which paykit_request needs; and chat alone with lists.read, a valid certificate \
that holds no Paykit scope, whose Paykit messages recipients drop at check 8 (messages.json \
multi_protocol_senders). refused are certificates they refuse: paykit_request beside chat \
without conversation.add; paykit_request alone with lists.read beside conversation.add; and \
paykit_request beside paykit_ext:cap, an extension scope whose prefix doesn't start with \
paykit-. reason is paykit-lib's wording at a \
Paykit verifier, of the first check that refuses it.";

const APP_RECORDS_DESCRIPTION: &str = "Wire 3.2: Paykit's member of an app record, \
protocols.paykit, as a Paykit reader reads it. Each record is the shop's app record, which \
carries the shop's certificate of this file's certificates, with its paykit member changed or removed (and, for public \
endpoints with expires_at null, expires_at null too), signed again with the shop's signing \
key; the signatures verify. The switch takes every one, since it checks only the form of \
protocols (Switch wire 3.2). accepted are records a Paykit reader takes, the first the \
shop's record as it is; reads is what the reader takes from each: the Payment Endpoint \
Identifiers of capabilities.endpoint_identifiers, and the public endpoints. A missing or null \
paykit member, capabilities, endpoint_identifiers or public_endpoints reads as empty, and \
unknown members are ignored. refused are records Paykit's readers refuse, though the switch \
takes them (switch_accepts): a capabilities or public_endpoints that isn't an object, an \
endpoint_identifiers that isn't an array of strings, a payload that isn't a string, public \
endpoints with expires_at null, and, last, four that break wire 1.4: an endpoint identifier \
in capabilities that isn't a Payment Endpoint Identifier (btc-lightning/bolt11), a public \
endpoint key that isn't one (..), the reserved key private, and an empty payload. protocols \
and expires_at are the record's as signed. reason is the wording at a Paykit reader: the \
prototype's, and paykit-lib's for the last four.";

/// One certificate case of `multi_protocol_certificates`: its name, the app whose
/// certificate it changes, its scopes and permissions.
type CertCase = (
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [Permission],
);

const LISTS_READ: Permission = Permission::ListsRead;
const ADD: Permission = Permission::ConversationAdd;

const MULTI_PROTOCOL_ACCEPTED: [CertCase; 2] = [
    (
        "paykit_request and chat, with lists.read and conversation.add",
        "shop",
        &["paykit_request", "chat"],
        &[LISTS_READ, ADD],
    ),
    (
        "chat only, with lists.read",
        "shop",
        &["chat"],
        &[LISTS_READ],
    ),
];

const MULTI_PROTOCOL_REFUSED: [CertCase; 3] = [
    (
        "paykit_request and chat without conversation.add",
        "shop",
        &["paykit_request", "chat"],
        &[LISTS_READ],
    ),
    (
        "paykit_request only, with lists.read beside conversation.add",
        "shop",
        &["paykit_request"],
        &[LISTS_READ, ADD],
    ),
    (
        "an extension prefix without paykit-: paykit_ext:cap",
        "cap",
        &["paykit_request", "paykit_ext:cap"],
        &[ADD],
    ),
];

/// The app record cases that break wire 1.4: their names and Paykit members.
fn app_record_wire_1_4_cases() -> Vec<(&'static str, Value)> {
    let offer = "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzr";
    let ids = json!(["btc-lightning-bolt11", "btc-bitcoin-p2wpkh"]);
    vec![
        (
            "an endpoint identifier in capabilities that isn't a Payment Endpoint Identifier",
            json!({
                "capabilities": { "endpoint_identifiers": ["btc-lightning/bolt11", "btc-bitcoin-p2wpkh"] },
                "public_endpoints": { "btc-lightning-bolt12": offer },
            }),
        ),
        (
            "a public endpoint key that isn't a Payment Endpoint Identifier",
            json!({
                "capabilities": { "endpoint_identifiers": ids },
                "public_endpoints": { "..": offer },
            }),
        ),
        (
            "the reserved public endpoint key private",
            json!({
                "capabilities": { "endpoint_identifiers": ids },
                "public_endpoints": { "private": offer },
            }),
        ),
        (
            "an empty payload",
            json!({
                "capabilities": { "endpoint_identifiers": ids },
                "public_endpoints": { "btc-lightning-bolt12": "" },
            }),
        ),
    ]
}

/// One `multi_protocol_certificates` case, built from an app's certificate and
/// checked as it is built: the switch takes it, and Paykit's verifier gives `accepted`.
fn cert_case(keys: &Keys, (name, app, scopes, permissions): CertCase, accepted: bool) -> Value {
    certificate_case(&reissued(keys, app, scopes, permissions), name, accepted)
}

/// One certificate case, checked as it is built: the switch takes it, and Paykit's verifier
/// gives `accepted`.
fn certificate_case(c: &AppCert, name: &str, accepted: bool) -> Value {
    assert!(
        AppCert::verify(c.signed(), &c.identity()).is_ok(),
        "{name}: the switch takes it"
    );
    let mut case = json!({
        "body": body_text(c.signed()),
        "name": name,
        "paykit_permissions": permission_names(&permissions_for(c.scopes())),
        "paykit_scopes": paykit_scopes(c.scopes()).iter().map(PaykitScope::name).collect::<Vec<_>>(),
        "permissions": permission_names(c.permissions()),
        "scopes": c.scopes(),
        "signed": signed_json(c.signed()),
        "switch_accepts": true,
    });
    match check_certificate(c) {
        Ok(()) => assert!(accepted, "{name}: Paykit takes it"),
        Err(error) => {
            assert!(!accepted, "{name}: {error}");
            case["reason"] = json!(quoted(&error.to_string()));
        }
    }
    case
}

/// One refused `app_records` case: the shop's app record with its Paykit member
/// replaced, signed again with the shop's signing key.
fn app_record_case(keys: &Keys, name: &str, member: &Value) -> Value {
    let (shop, bob) = (keys.app("shop"), pubky(BOB));
    let base = shop_app_record(keys);
    let mut body = base.body().clone();
    body.protocols.clear();
    body.protocols.insert(
        PROTOCOL_NAME.to_owned(),
        member.as_object().unwrap().clone(),
    );
    let record = AppRecord::sign(&shop.keys, body).unwrap();
    let text = body_text(record.signed());
    let as_signed: Value = serde_json::from_str(&text).unwrap();
    let error = PaykitMember::read(&record).unwrap_err();
    json!({
        "body": text,
        "expires_at": as_signed["expires_at"],
        "name": name,
        "protocols": as_signed["protocols"],
        "reason": quoted(&error.to_string()),
        "signed": signed_json(record.signed()),
        "switch_accepts": AppRecord::verify(record.signed(), &bob).is_ok(),
    })
}

const EXECUTOR_CERTIFICATES_DESCRIPTION: &str = "Spec 3.1, wire 3.1 and 3.3: the executor's \
certificate. paykit_allowance_execute requires paykit_allowance, paykit_pay and \
paykit_contacts, so that every executor reads the deny list. accepted is Paykit's own executor certificate, which the executor grant \
of this file carries: the executor's certificate of this file's certificates, which holds \
paykit_contacts. refused is that certificate with paykit_contacts removed from its scopes, \
so that it holds paykit_allowance_execute without paykit_contacts, and the permissions its \
scopes need, signed again by alice's identity key: the switch takes it (switch_accepts), and Paykit's \
verifiers refuse it. reason is paykit-lib's wording at a Paykit verifier.";

/// `executor_certificates` (Paykit spec 3.1): Paykit's own executor certificate, which the grant
/// carries, and the base one, which holds `paykit_allowance_execute` without `paykit_contacts`.
fn executor_certificates(keys: &Keys) -> Value {
    let base = base_cert(keys, "executor");
    json!({
        "accepted": [certificate_case(&executor_cert(keys), "Paykit's executor certificate, with paykit_contacts", true)],
        "description": EXECUTOR_CERTIFICATES_DESCRIPTION,
        "refused": [certificate_case(&base, "paykit_allowance_execute without paykit_contacts", false)],
    })
}

/// `executor_certificates`: each verifies at the switch, signed by alice's identity key;
/// Paykit's verifiers take Paykit's, the grant's certificate, and refuse the base one for
/// the scope rule of Paykit spec 3.1.
fn check_executor_certificates(keys: &Keys, d: &Value) {
    let alice = pubky(ALICE);
    let identity = keys.identity(&alice);
    let e = &d["executor_certificates"];
    for (list, accepted) in [("accepted", true), ("refused", false)] {
        let cases = arr(e, list);
        assert_eq!(cases.len(), 1, "{list}");
        for case in cases {
            let name = st(case, "name");
            let doc = signed(&case["signed"]);
            assert_eq!(body_text(&doc), st(case, "body"), "{name}");
            let again = identity.sign_doc(labels::sig::APP_CERT, &[], doc.body_bytes().unwrap());
            assert_eq!(again, doc, "{name}");
            let c = AppCert::verify(&doc, &alice).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(c.scopes(), strings(&case["scopes"]).as_slice(), "{name}");
            match verify_certificate(&doc, &alice) {
                Ok(_) => {
                    assert!(accepted, "{name}: Paykit takes it");
                    assert_eq!(case["signed"], d["executor_grant"]["cert"], "{name}");
                }
                Err(error) => {
                    assert!(!accepted, "{name}: {error}");
                    assert!(
                        error
                            .to_string()
                            .contains("allowance_execute requires paykit_allowance, paykit_pay and paykit_contacts"),
                        "{name}: {error}"
                    );
                }
            }
        }
    }
    // The refused one is the executor's base certificate, as it is.
    assert_eq!(
        &signed(&arr(e, "refused")[0]["signed"]),
        base_cert(keys, "executor").signed()
    );
}

const PAYER_CERTIFICATES_DESCRIPTION: &str = "Spec 3.1, wire 3.1: a paying app's \
certificate. paykit_pay requires paykit_contacts, so that every paying app reads the deny list and pays \
no counterparty the user blocked. accepted is Paykit's own phone certificate, which the \
phone's messages stored in sealed.json carry: the phone's certificate of this file's \
certificates, which holds paykit_contacts. refused are that certificate with paykit_contacts \
removed from its scopes, so that it holds paykit_pay and paykit_allowance without \
paykit_contacts, and the permissions its scopes need, signed again by alice's identity key, \
and the wallet's certificate of this file's certificates with paykit_pay alone and the permissions paykit_pay needs, signed again by alice's identity key: the \
switch takes each (switch_accepts), and Paykit's verifiers refuse each. reason is \
paykit-lib's wording at a Paykit verifier.";

/// `payer_certificates` (Paykit spec 3.1): Paykit's own phone certificate, which the phone's stored
/// messages carry, and certificates holding `paykit_pay` without `paykit_contacts`: the phone's
/// base certificate, and the wallet's with `paykit_pay` alone.
fn payer_certificates(keys: &Keys) -> Value {
    let base = base_cert(keys, "phone");
    let pay = ["paykit_pay".to_owned()];
    let pay_alone = reissued(keys, "wallet", &["paykit_pay"], &permissions_for(&pay));
    json!({
        "accepted": [certificate_case(&phone_cert(keys), "Paykit's phone certificate, with paykit_contacts", true)],
        "description": PAYER_CERTIFICATES_DESCRIPTION,
        "refused": [
            certificate_case(&base, "paykit_pay and paykit_allowance without paykit_contacts", false),
            certificate_case(&pay_alone, "paykit_pay alone", false),
        ],
    })
}

/// `payer_certificates`: each verifies at the switch, signed by alice's identity key;
/// Paykit's verifiers take Paykit's phone certificate and refuse each holding `paykit_pay` without
/// `paykit_contacts` for the scope rule of Paykit spec 3.1.
fn check_payer_certificates(keys: &Keys, d: &Value) {
    let alice = pubky(ALICE);
    let identity = keys.identity(&alice);
    let e = &d["payer_certificates"];
    for (list, accepted, count) in [("accepted", true, 1), ("refused", false, 2)] {
        let cases = arr(e, list);
        assert_eq!(cases.len(), count, "{list}");
        for case in cases {
            let name = st(case, "name");
            let doc = signed(&case["signed"]);
            assert_eq!(body_text(&doc), st(case, "body"), "{name}");
            let again = identity.sign_doc(labels::sig::APP_CERT, &[], doc.body_bytes().unwrap());
            assert_eq!(again, doc, "{name}");
            let c = AppCert::verify(&doc, &alice).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(case["switch_accepts"], true, "{name}");
            assert_eq!(c.scopes(), strings(&case["scopes"]).as_slice(), "{name}");
            assert!(c.scopes().iter().any(|s| s == "paykit_pay"), "{name}");
            // Its permissions are exactly what its scopes need: only the scope rule refuses.
            assert_eq!(
                c.permissions(),
                permissions_for(c.scopes()).as_slice(),
                "{name}"
            );
            match verify_certificate(&doc, &alice) {
                Ok(_) => {
                    assert!(accepted, "{name}: Paykit takes it");
                    assert_eq!(doc, cert_doc("phone"), "{name}");
                }
                Err(error) => {
                    assert!(!accepted, "{name}: {error}");
                    assert!(
                        error
                            .to_string()
                            .contains("paykit_pay requires paykit_contacts"),
                        "{name}: {error}"
                    );
                    assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
                }
            }
        }
    }
    // The first refused is the phone's base certificate, as it is.
    assert_eq!(
        &signed(&arr(e, "refused")[0]["signed"]),
        base_cert(keys, "phone").signed()
    );
}

const CERTIFICATES_DESCRIPTION: &str = "Wire 3.1, Switch wire 3.1: the certificate of \
each app of keys.json, as Paykit's verifiers take it, in its carrying form (signed: body and \
sig). Each holds its app's keys, scopes and permissions as keys.json gives them, issued_at \
2025-12-01T00:00:00.000Z and the app's expires_at, signed by its identity's key under \
pubky-switch/v1/app-cert. The executor's and the phone's are Paykit's own, the accepted \
certificates of executor_certificates and payer_certificates, holding paykit_contacts; the others \
are the certificates every other case of this set signs again with scopes or permissions \
changed. app is the app's name in keys.json.";

/// `certificates` (Paykit wire 3.1): each app's certificate of this set, which Paykit's
/// verifiers take.
fn certificates(keys: &Keys) -> Value {
    let cases: Vec<Value> = keys
        .apps
        .iter()
        .map(|app| {
            let doc = cert_doc(&app.name);
            verify_certificate(&doc, &app.identity).unwrap_or_else(|e| panic!("{}: {e}", app.name));
            json!({
                "app": app.name,
                "name": format!("{} certificate", app.name),
                "signed": signed_json(&doc),
            })
        })
        .collect();
    json!({ "cases": cases, "description": CERTIFICATES_DESCRIPTION })
}

const EXTENSION_APP_DESCRIPTION: &str = "Wire 3.1, 3.2, Switch wire 3.1, 3.2: bob's \
extension app cap. certificate is its certificate of certificates, holding paykit_request \
beside the extension scope paykit_ext:paykit-cap. app_record is its app record, signed by its signing key \
under pubky-switch/v1/app-record: it carries that certificate, accepts the kinds a payee \
app takes from payers and the extension kind paykit-cap.offer, and its Paykit member states \
its endpoint identifiers and no public endpoints; its label is bob cap, its updated_at its \
certificate's issued_at, and its expires_at null. The switch takes the record, and a Paykit \
reader reads its member.";

/// `extension_app` (Paykit wire 3.1, 3.2): the `cap` app's certificate and its app record,
/// which accepts an extension kind.
fn extension_app(keys: &Keys) -> Value {
    let record = cap_app_record(keys);
    let read = AppRecord::verify(record.signed(), &pubky(BOB)).unwrap();
    assert!(read.accepts(crate::common::CAP_EXTENSION_KIND));
    PaykitMember::read(&read).unwrap();
    json!({
        "app_record": {
            "app": "cap",
            "name": "cap app record, accepting the extension kind paykit-cap.offer",
            "signed": signed_json(record.signed()),
        },
        "certificate": {
            "app": "cap",
            "name": "cap certificate",
            "signed": signed_json(&cert_doc("cap")),
        },
        "description": EXTENSION_APP_DESCRIPTION,
    })
}

/// The cases paykit-lib generates in `documents.json` (README "How the files were
/// generated"), compared with the file, and written into it with `PAYKIT_WRITE_VECTORS`.
fn generate(keys: &Keys) {
    // Signed again with their own scopes and permissions, the certificates are the same byte
    // for byte.
    for app in ["shop", "cap"] {
        let a = keys.app(app);
        let held = cert(a);
        let perms = held.permissions().to_vec();
        let scopes: Vec<&str> = held.scopes().iter().map(String::as_str).collect();
        assert_eq!(
            reissued(keys, app, &scopes, &perms).signed(),
            &cert_doc(app)
        );
    }
    let member = json!({
        "accepted": MULTI_PROTOCOL_ACCEPTED.iter().map(|c| cert_case(keys, *c, true)).collect::<Vec<_>>(),
        "description": MULTI_PROTOCOL_DESCRIPTION,
        "refused": MULTI_PROTOCOL_REFUSED.iter().map(|c| cert_case(keys, *c, false)).collect::<Vec<_>>(),
    });
    // The grant, signed by the executor over its authored members (Paykit wire 3.3), with
    // Paykit's own executor certificate, which the signature doesn't cover.
    let file = paykit("documents.json");
    let entry = &file["executor_grant_history_entry"];
    let held = ExecutorGrant::from_json(&file["executor_grant"]).unwrap();
    let terms = b64n::<32>(st(&paykit("identifiers.json")["terms_hash"], "terms_hash"));
    let built_on = RecordVersion {
        generation: entry["generation"].as_u64().unwrap(),
        version: entry["version"].as_u64().unwrap(),
    };
    let grant = ExecutorGrant::sign(
        &keys.app("executor").keys,
        executor_cert(keys).signed().clone(),
        &pubky(ALICE),
        &pubky(BOB),
        &AllowanceId::parse(ALLOWANCE_ID).unwrap(),
        terms,
        held.confirmed_generation,
        built_on,
    )
    .to_json();
    let mut cases = vec![
        (Place::Member("/description"), json!(DESCRIPTION)),
        (Place::Member("/executor_grant"), grant.clone()),
        (Place::Member("/executor_grant_history_entry/grant"), grant),
        (
            Place::Member("/executor_certificates"),
            executor_certificates(keys),
        ),
        (
            Place::Member("/payer_certificates"),
            payer_certificates(keys),
        ),
        (Place::Member("/multi_protocol_certificates"), member),
        (Place::Member("/certificates"), certificates(keys)),
        (Place::Member("/extension_app"), extension_app(keys)),
        (
            Place::Member("/app_records/description"),
            json!(APP_RECORDS_DESCRIPTION),
        ),
    ];
    for (name, member) in app_record_wire_1_4_cases() {
        cases.push((
            Place::Case("/app_records/refused"),
            app_record_case(keys, name, &member),
        ));
    }
    cases.push((
        Place::Member("/certificate_rejections/cases"),
        Value::Array(certificate_rejections(keys)),
    ));
    cases.push((
        Place::Member("/labelled_scope_rejections/cases"),
        Value::Array(labelled_scope_rejections(keys)),
    ));
    cases.extend(app_record_signatures(keys, &file));
    generated("documents.json", &cases);
}

/// The cases of `certificate_rejections`: their names, scopes and permissions.
const CERTIFICATE_REJECTIONS: [CertCase; 2] = [
    (
        "extra: lists.manage kept after paykit_manage_contacts is gone",
        "wallet",
        &["paykit_pay", "paykit_contacts"],
        &[
            Permission::ClaimsRead,
            Permission::ClaimsWrite,
            LISTS_READ,
            Permission::ListsManage,
            ADD,
        ],
    ),
    (
        "missing: conversation.add, which paykit_pay needs",
        "wallet",
        &["paykit_pay", "paykit_contacts", "paykit_manage_contacts"],
        &[
            Permission::ClaimsRead,
            Permission::ClaimsWrite,
            LISTS_READ,
            Permission::ListsManage,
        ],
    ),
];

/// The cases of `labelled_scope_rejections`: their names, scopes and the reason every
/// verifier gives, in the prototype's wording.
const LABELLED_SCOPE_REJECTIONS: [(&str, &[&str], &str); 5] = [
    (
        "a label of 33 characters",
        &["paykit_request:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        "invalid: \"paykit_request:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\" isn't a scope name or name:<sub>",
    ),
    (
        "a label that isn't valid (upper case)",
        &["paykit_request:Shop1"],
        "invalid: \"paykit_request:Shop1\" isn't a scope name or name:<sub>",
    ),
    (
        "an empty label",
        &["paykit_request:"],
        "invalid: \"paykit_request:\" isn't a scope name or name:<sub>",
    ),
    (
        "nine labelled scopes",
        &[
            "paykit_request:shop1",
            "paykit_request:shop2",
            "paykit_request:shop3",
            "paykit_request:shop4",
            "paykit_request:shop5",
            "paykit_request:shop6",
            "paykit_request:shop7",
            "paykit_request:shop8",
            "paykit_request:shop9",
        ],
        "invalid: more than 8 labelled scopes",
    ),
    (
        "plain paykit_request beside a labelled scope",
        &["paykit_request", "paykit_request:shop1"],
        "invalid: paykit_request and paykit_request:shop1 together: paykit_request covers every paykit_request:<sub>",
    ),
];

/// A certificate of `app` with `scopes` and `permissions`, signed by its identity key without
/// the switch's checks, which some of these certificates fail.
fn signed_with(keys: &Keys, app: &str, scopes: &[&str], permissions: &[Permission]) -> SignedDoc {
    let app = keys.app(app);
    let mut body = base_cert(keys, &app.name).body().clone();
    body.scopes = scopes.iter().map(|s| (*s).to_owned()).collect();
    body.permissions = permission_names(permissions);
    keys.identity(&app.identity)
        .sign_doc(labels::sig::APP_CERT, &[], body.to_json().unwrap())
}

/// `certificate_rejections` (Paykit wire 3.1): the wallet's certificate with scopes and
/// permissions that aren't the list Paykit derives from its scopes.
fn certificate_rejections(keys: &Keys) -> Vec<Value> {
    CERTIFICATE_REJECTIONS
        .iter()
        .map(|&(name, app, scopes, permissions)| {
            let doc = signed_with(keys, app, scopes, permissions);
            let owned: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
            let identity = keys.app(app).identity;
            let error = verify_certificate(&doc, &identity).unwrap_err().to_string();
            json!({
                "body": body_text(&doc),
                "name": name,
                "paykit_permissions": permission_names(&permissions_for(&owned)),
                "permissions": permission_names(permissions),
                "reason": quoted(&error),
                "scopes": scopes,
                "signed": signed_json(&doc),
                "switch_accepts": AppCert::verify(&doc, &identity).is_ok(),
            })
        })
        .collect()
}

/// `labelled_scope_rejections` (Paykit wire 3.1): the hosted app's certificate with its
/// scopes changed and the permissions Paykit derives from them.
fn labelled_scope_rejections(keys: &Keys) -> Vec<Value> {
    LABELLED_SCOPE_REJECTIONS
        .iter()
        .map(|&(name, scopes, reason)| {
            let owned: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
            let doc = signed_with(keys, "hosted", scopes, &permissions_for(&owned));
            json!({
                "body": body_text(&doc),
                "name": name,
                "reason": reason,
                "scopes": scopes,
                "signed": signed_json(&doc),
                "switch_accepts": AppCert::verify(&doc, &pubky(BOB)).is_ok(),
            })
        })
        .collect()
}

/// Every `app_records` case's record: its body as the file holds it, the shop's app record
/// with the case's `protocols` and `expires_at`, carrying the shop's certificate of this set,
/// signed by the shop's signing key (Switch wire 3.2).
fn app_record_signatures(keys: &Keys, file: &Value) -> Vec<(Place<'static>, Value)> {
    let mut out = Vec::new();
    for list in ["accepted", "refused"] {
        for (i, case) in arr(&file["app_records"], list).iter().enumerate() {
            let text = recertified(keys, st(case, "body"));
            let doc = keys.app("shop").keys.sign_doc(
                labels::sig::APP_RECORD,
                &[],
                text.as_bytes().to_vec(),
            );
            let at = format!("/app_records/{list}/{i}");
            out.push((Place::At(format!("{at}/body")), json!(text)));
            out.push((Place::At(format!("{at}/signed")), signed_json(&doc)));
        }
    }
    out
}

/// `multi_protocol_certificates` (Paykit wire 3.1, Switch wire 3.1 "Several protocols"):
/// each certificate verifies at the switch, and Paykit's verifiers take the accepted ones and
/// refuse the others, for their permissions or their scopes.
fn check_multi_protocol_certificates(d: &Value) {
    let bob = pubky(BOB);
    let m = &d["multi_protocol_certificates"];
    for (list, accepted) in [("accepted", true), ("refused", false)] {
        for case in arr(m, list) {
            let name = st(case, "name");
            let doc = signed(&case["signed"]);
            assert_eq!(body_text(&doc), st(case, "body"), "{name}");
            let c = AppCert::verify(&doc, &bob).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(case["switch_accepts"], true, "{name}");
            assert_eq!(c.scopes(), strings(&case["scopes"]).as_slice(), "{name}");
            assert_eq!(
                permission_names(c.permissions()),
                strings(&case["permissions"])
            );
            assert_eq!(
                permission_names(&permissions_for(c.scopes())),
                strings(&case["paykit_permissions"]),
                "{name}"
            );
            let verdict = verify_certificate(&doc, &bob);
            assert_eq!(verdict.is_ok(), accepted, "{name}: {verdict:?}");
            if let Err(error) = verdict {
                assert_eq!(quoted(&error.to_string()), st(case, "reason"), "{name}");
            }
        }
    }
    assert_eq!(arr(m, "accepted").len(), 2);
    assert_eq!(arr(m, "refused").len(), 3);
}

/// `app_records` (Paykit wire 3.2): each record is the shop's with its Paykit member, and
/// for one its `expires_at`, changed, signed again by the shop byte for byte; the switch takes
/// every one; a Paykit reader reads the accepted ones as `reads` says, and refuses the others.
fn check_app_records(keys: &Keys, d: &Value) {
    let (shop, bob) = (keys.app("shop"), pubky(BOB));
    let base: Value = serde_json::from_str(&body_text(shop_app_record(keys).signed())).unwrap();
    let records = &d["app_records"];
    for (list, accepted) in [("accepted", true), ("refused", false)] {
        for case in arr(records, list) {
            let name = st(case, "name");
            let doc = signed(&case["signed"]);
            let text = body_text(&doc);
            assert_eq!(text, st(case, "body"), "{name}");
            let again = shop
                .keys
                .sign_doc(labels::sig::APP_RECORD, &[], text.as_bytes().to_vec());
            assert_eq!(again, doc, "{name}: signed again");
            let mut want = base.clone();
            want["protocols"] = case["protocols"].clone();
            want["expires_at"] = case["expires_at"].clone();
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                want,
                "{name}: the shop's record with its member changed"
            );
            let record = AppRecord::verify(&doc, &bob).unwrap_or_else(|e| panic!("{name}: {e}"));
            if !accepted {
                assert_eq!(case["switch_accepts"], true, "{name}");
            }
            match (PaykitMember::read(&record), accepted) {
                (Ok(read), true) => {
                    let ids: Vec<&str> = read
                        .endpoint_identifiers
                        .iter()
                        .map(|i| i.as_str())
                        .collect();
                    assert_eq!(json!(ids), case["reads"]["endpoint_identifiers"], "{name}");
                    let endpoints: serde_json::Map<String, Value> = read
                        .public_endpoints
                        .iter()
                        .map(|(i, p)| (i.as_str().to_owned(), json!(p.as_str())))
                        .collect();
                    assert_eq!(
                        Value::Object(endpoints),
                        case["reads"]["public_endpoints"],
                        "{name}"
                    );
                }
                (Err(_), false) => {}
                (verdict, _) => panic!("{name}: {verdict:?}"),
            }
        }
    }
    assert_eq!(arr(records, "accepted").len(), 12);
    assert_eq!(arr(records, "refused").len(), 10);
}
