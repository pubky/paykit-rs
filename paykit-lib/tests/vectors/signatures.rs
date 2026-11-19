//! `signatures.json`: the executor grant's and the ledger handoff's signatures under
//! Paykit's own labels (Paykit wire 2, 3.3, 5.4).

use paykit_lib::grant::{grant_fields, handoff_fields, EXECUTOR_GRANT_LABEL, LEDGER_HANDOFF_LABEL};
use paykit_lib::identifiers::AllowanceId;
use paykit_lib::recovery::{RecoveryCommitments, RECOVERY_UPLOAD_LABEL};
use pubky_switch_core::ops::RecordVersion;
use pubky_switch_core::records::{IdentityBody, IdentityDoc, Inbox, ListedApp};
use pubky_switch_core::sig::SignedDoc;
use pubky_switch_core::sig::{signed_bytes, verify};

use pubky_switch_core::enc::b64;
use serde_json::json;

use crate::common::{
    arr, b64n, generated, hex, paykit, pubky, st, unhex, writing, Keys, Place, ALICE, ALLOWANCE_ID,
    BOB,
};

#[test]
fn test_vectors_signatures() {
    if !writing() {
        check();
    }
}

/// `signatures.json` against paykit-lib: its generated cases, then every check of its cases. With
/// [`WRITE_VECTORS`](crate::common::WRITE_VECTORS) set, the regeneration driver runs it once it
/// has written the set.
pub(crate) fn check() {
    let keys = Keys::from_seeds();
    generate(&keys);
    let v = paykit("signatures.json");
    let cases = arr(&v, "signatures");
    assert_eq!(cases.len(), 2);
    check_handoff(&keys, &cases[1]);
    check_recovery_report(&keys, &v["recovery_report"]);
    let case = &cases[0];
    assert_eq!(st(case, "label"), EXECUTOR_GRANT_LABEL);
    assert!(!EXECUTOR_GRANT_LABEL.starts_with("pubky-switch/"));
    let fields: Vec<Vec<u8>> = arr(case, "fields")
        .iter()
        .map(|f| unhex(st(f, "hex")))
        .collect();
    let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
    assert_eq!(
        hex(&signed_bytes(EXECUTOR_GRANT_LABEL, &refs)),
        st(case, "signed_bytes_hex")
    );

    // The fields, built from the grant history entry of documents.json.
    let d = paykit("documents.json");
    let entry = &d["executor_grant_history_entry"];
    let grant = &entry["grant"];
    let built = grant_fields(
        &pubky(ALICE),
        &pubky(BOB),
        &AllowanceId::parse(ALLOWANCE_ID).unwrap(),
        &b64n::<32>(st(grant, "terms_hash")),
        grant["confirmed_generation"].as_u64().unwrap(),
        RecordVersion {
            generation: entry["generation"].as_u64().unwrap(),
            version: entry["version"].as_u64().unwrap(),
        },
    );
    assert_eq!(built, fields);

    // The executor app signed it, deterministically.
    let executor = keys.app("executor");
    let public = b64n::<32>(st(case, "public_key"));
    assert_eq!(public, executor.keys.signing_public());
    let signature = b64n::<64>(st(case, "signature"));
    assert_eq!(st(grant, "sig"), st(case, "signature"));
    verify(&public, EXECUTOR_GRANT_LABEL, &refs, &signature).unwrap();
    assert_eq!(executor.keys.sign(EXECUTOR_GRANT_LABEL, &refs), signature);

    // Any other field refuses it.
    let mut other = fields.clone();
    other[6] = 13u64.to_be_bytes().to_vec();
    let other_refs: Vec<&[u8]> = other.iter().map(Vec::as_slice).collect();
    assert!(verify(&public, EXECUTOR_GRANT_LABEL, &other_refs, &signature).is_err());
}

/// A version of a case's member `name`, `{generation, version}`.
fn version_of(case: &serde_json::Value, name: &str) -> RecordVersion {
    RecordVersion {
        generation: case[name]["generation"].as_u64().unwrap(),
        version: case[name]["version"].as_u64().unwrap(),
    }
}

/// The ledger handoff's signed fields, from the case's grant and ledger versions and item
/// hash, for alice's Allowance with bob.
fn handoff_case_fields(case: &serde_json::Value) -> Vec<Vec<u8>> {
    handoff_fields(
        &pubky(ALICE),
        &pubky(BOB),
        &AllowanceId::parse(ALLOWANCE_ID).unwrap(),
        version_of(case, "grant"),
        version_of(case, "ledger"),
        &b64n::<32>(st(case, "item_hash")),
    )
}

/// The ledger handoff's case: its label, its fields built from the case, its signed bytes,
/// and the executor's signature, which verifies, and which another field refuses.
fn check_handoff(keys: &Keys, case: &serde_json::Value) {
    assert_eq!(st(case, "label"), LEDGER_HANDOFF_LABEL);
    let fields: Vec<Vec<u8>> = arr(case, "fields")
        .iter()
        .map(|f| unhex(st(f, "hex")))
        .collect();
    assert_eq!(handoff_case_fields(case), fields);
    let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
    assert_eq!(
        hex(&signed_bytes(LEDGER_HANDOFF_LABEL, &refs)),
        st(case, "signed_bytes_hex")
    );
    let executor = keys.app("executor");
    let public = b64n::<32>(st(case, "public_key"));
    assert_eq!(public, executor.keys.signing_public());
    let signature = b64n::<64>(st(case, "signature"));
    verify(&public, LEDGER_HANDOFF_LABEL, &refs, &signature).unwrap();
    assert_eq!(executor.keys.sign(LEDGER_HANDOFF_LABEL, &refs), signature);
    // The executor grant's label over the same fields doesn't verify.
    assert!(verify(&public, EXECUTOR_GRANT_LABEL, &refs, &signature).is_err());
    let mut other = fields.clone();
    other[6] = 3u64.to_be_bytes().to_vec();
    let other_refs: Vec<&[u8]> = other.iter().map(Vec::as_slice).collect();
    assert!(verify(&public, LEDGER_HANDOFF_LABEL, &other_refs, &signature).is_err());
}

/// The grant's signed fields, its signed bytes, the executor's public key and its signature,
/// from the grant history entry of `documents.json`, and the same of the ledger handoff from
/// its case, written into `signatures.json` by the regeneration driver (`regenerate.rs`).
pub(crate) fn generate(keys: &Keys) {
    let d = paykit("documents.json");
    let entry = &d["executor_grant_history_entry"];
    let grant = &entry["grant"];
    let fields = grant_fields(
        &pubky(ALICE),
        &pubky(BOB),
        &AllowanceId::parse(ALLOWANCE_ID).unwrap(),
        &b64n::<32>(st(grant, "terms_hash")),
        grant["confirmed_generation"].as_u64().unwrap(),
        RecordVersion {
            generation: entry["generation"].as_u64().unwrap(),
            version: entry["version"].as_u64().unwrap(),
        },
    );
    let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
    let executor = &keys.app("executor").keys;
    let mut cases = vec![
        (
            Place::At("/signatures/0/signed_bytes_hex".into()),
            json!(hex(&signed_bytes(EXECUTOR_GRANT_LABEL, &refs))),
        ),
        (
            Place::At("/signatures/0/public_key".into()),
            json!(b64(&executor.signing_public())),
        ),
        (
            Place::At("/signatures/0/signature".into()),
            json!(b64(&executor.sign(EXECUTOR_GRANT_LABEL, &refs))),
        ),
    ];
    for (i, field) in fields.iter().enumerate() {
        cases.push((
            Place::At(format!("/signatures/0/fields/{i}/hex")),
            json!(hex(field)),
        ));
    }
    // The ledger handoff's, from its case's versions and item hash.
    let v = paykit("signatures.json");
    let handoff = &arr(&v, "signatures")[1];
    let fields = handoff_case_fields(handoff);
    let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
    cases.extend([
        (
            Place::At("/signatures/1/signed_bytes_hex".into()),
            json!(hex(&signed_bytes(LEDGER_HANDOFF_LABEL, &refs))),
        ),
        (
            Place::At("/signatures/1/public_key".into()),
            json!(b64(&executor.signing_public())),
        ),
        (
            Place::At("/signatures/1/signature".into()),
            json!(b64(&executor.sign(LEDGER_HANDOFF_LABEL, &refs))),
        ),
    ]);
    for (i, field) in fields.iter().enumerate() {
        cases.push((
            Place::At(format!("/signatures/1/fields/{i}/hex")),
            json!(hex(field)),
        ));
    }
    cases.push((
        Place::Member("/recovery_report"),
        recovery_report_case(keys),
    ));
    generated("signatures.json", &cases);
}

fn recovery_report_case(keys: &Keys) -> serde_json::Value {
    let cert = crate::common::phone_cert(keys);
    let phone = keys.app("phone");
    let mut body = IdentityBody::first(
        phone.identity,
        [4; 16],
        Inbox::new(
            "https://switch.example",
            keys.app("wallet").keys.signing_public(),
        ),
        Default::default(),
    );
    body.version = 3;
    body.generation = 2;
    body.recovery_from = Some(1);
    body.apps.push(ListedApp {
        app_id: cert.app_id(),
        cert: cert.hash(),
    });
    let opening = IdentityDoc::sign(keys.identity(&phone.identity), body).unwrap();
    let commitments = RecoveryCommitments {
        cutoff: [1; 32],
        inventory: [2; 32],
        dispositions: [3; 32],
    };
    let field = commitments.signing_field(&opening, &cert.app_id()).unwrap();
    json!({
        "opening":opening.signed(), "original":b64(&cert.app_id()), "written_at_ms":cert.issued_at(),
        "label": RECOVERY_UPLOAD_LABEL, "signed_bytes_hex":hex(&signed_bytes(RECOVERY_UPLOAD_LABEL,&[&field])),
        "report":{"author_certificate":cert.signed(), "cutoff":b64(&commitments.cutoff), "inventory":b64(&commitments.inventory),
            "dispositions":b64(&commitments.dispositions), "signature":b64(&phone.keys.sign(RECOVERY_UPLOAD_LABEL,&[&field]))}
    })
}

fn check_recovery_report(keys: &Keys, case: &serde_json::Value) {
    let signed: SignedDoc = serde_json::from_value(case["opening"].clone()).unwrap();
    let opening = IdentityDoc::verify(&signed, &keys.app("phone").identity).unwrap();
    let id = b64n::<16>(st(case, "original"));
    let written = case["written_at_ms"].as_u64().unwrap();
    let report = &case["report"];
    let value = serde_json::to_vec(report).unwrap();
    let commitments =
        RecoveryCommitments::verify(&value, &opening, &opening, &id, written).unwrap();
    let mut extended = report.clone();
    extended["new_member"] = serde_json::json!({"value": 1});
    assert_eq!(
        RecoveryCommitments::verify(
            &serde_json::to_vec(&extended).unwrap(),
            &opening,
            &opening,
            &id,
            written,
        )
        .unwrap(),
        commitments
    );
    for name in [
        "cutoff",
        "inventory",
        "dispositions",
        "signature",
        "author_certificate",
    ] {
        let mut incomplete = extended.clone();
        incomplete.as_object_mut().unwrap().remove(name);
        assert!(RecoveryCommitments::verify(
            &serde_json::to_vec(&incomplete).unwrap(),
            &opening,
            &opening,
            &id,
            written,
        )
        .is_err());
    }
    let field = commitments.signing_field(&opening, &id).unwrap();
    assert_eq!(
        hex(&signed_bytes(RECOVERY_UPLOAD_LABEL, &[&field])),
        st(case, "signed_bytes_hex")
    );
    for name in ["cutoff", "inventory", "dispositions"] {
        let mut changed = report.clone();
        changed[name] = b64(&[33; 32]).into();
        assert!(RecoveryCommitments::verify(
            &serde_json::to_vec(&changed).unwrap(),
            &opening,
            &opening,
            &id,
            written
        )
        .is_err());
    }
    assert!(RecoveryCommitments::verify(&value, &opening, &opening, &[0; 16], written).is_err());
    assert!(RecoveryCommitments::verify(&value, &opening, &opening, &id, written - 1).is_err());
}
