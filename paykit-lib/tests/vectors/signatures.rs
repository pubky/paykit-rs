//! `signatures.json`: the executor grant's signature under Paykit's own label (Paykit wire
//! 2, 3.3).

use paykit_lib::grant::{grant_fields, EXECUTOR_GRANT_LABEL};
use paykit_lib::identifiers::AllowanceId;
use pubky_switch_core::ops::RecordVersion;
use pubky_switch_core::sig::{signed_bytes, verify};

use pubky_switch_core::enc::b64;
use serde_json::json;

use crate::common::{
    arr, b64n, generated, hex, paykit, pubky, st, unhex, Keys, Place, ALICE, ALLOWANCE_ID, BOB,
};

#[test]
fn test_vectors_signatures() {
    let keys = Keys::from_seeds();
    generate(&keys);
    let v = paykit("signatures.json");
    let cases = arr(&v, "signatures");
    assert_eq!(cases.len(), 1);
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

/// The grant's signed fields, its signed bytes, the executor's public key and its signature,
/// from the grant history entry of `documents.json`, written into `signatures.json` with
/// `PAYKIT_WRITE_VECTORS`.
fn generate(keys: &Keys) {
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
    generated("signatures.json", &cases);
}
