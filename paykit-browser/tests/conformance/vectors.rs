//! Cases of Paykit's normative vectors, `specs/v0.2/vectors/`, rebuilt through the public APIs
//! of paykit-lib and pubky-switch-core and compared byte for byte, with the rejections each
//! file states.
//!
//! Covered here: `keys.json` (identities, app keys and permissions, scope keys),
//! `signatures.json` (the executor grant and the ledger handoff), `identifiers.json` (request
//! hashes, the terms hash, obligation IDs and collection policies) and `encoding.json`
//! (decimal amounts); `documents.json` and `messages.json` are read for the inputs those cases
//! name. This is a subset that shows the same code gives the same bytes in a browser. The
//! complete set, every file and every case, stays in paykit-lib's native vector tests
//! (`paykit-lib/tests/vectors/`).

use paykit_lib::amount::Decimal;
use paykit_lib::collections::Collection;
use paykit_lib::derive::{
    payment_obligation, payment_obligation_material, request_hash, request_obligation,
    request_obligation_material, terms_hash, terms_hash_material,
};
use paykit_lib::grant::{grant_fields, handoff_fields, EXECUTOR_GRANT_LABEL, LEDGER_HANDOFF_LABEL};
use paykit_lib::identifiers::{AllowanceId, AttemptId};
use paykit_lib::scopes::{permissions_for, Label};
use pubky_switch_core::enc::{b64, hash, lp, u64be};
use pubky_switch_core::keys::{AppKeys, IdentityKey};
use pubky_switch_core::ops::RecordVersion;
use pubky_switch_core::scopes::permission_names;
use pubky_switch_core::sig::{signed_bytes, verify};
use serde_json::{json, Value};

use crate::support::{arr, b64n, hex, message_json, num, pubky, seed, st, unhex, vector_file};

/// The Allowance every Allowance case of the set is about.
const ALLOWANCE_ID: &str = "d0efffb7-191b-4709-b6c7-e343af5500b4";

/// The label of bob's labelled selling scope.
const SHOP1: &str = "shop1";

/// An identity of the set by name, alice (the payer) or bob (the payee), from its seed label.
fn identity_named(name: &str) -> IdentityKey {
    IdentityKey::from_secret(seed(&format!("{name}/identity")))
}

/// The identity of the set with this Pubky ID.
fn identity_with_id(id: &str) -> IdentityKey {
    ["alice", "bob"]
        .into_iter()
        .map(identity_named)
        .find(|key| key.id() == id)
        .unwrap_or_else(|| panic!("no identity {id}"))
}

/// The executor app's keys, from its seed labels: it signs the grant and the handoff.
fn executor_keys() -> AppKeys {
    AppKeys::from_secrets(
        seed("alice/executor/signing"),
        seed("alice/executor/bundle"),
    )
}

/// A signature case's fields, as bytes.
fn case_fields(case: &Value) -> Vec<Vec<u8>> {
    arr(case, "fields")
        .iter()
        .map(|f| unhex(st(f, "hex")))
        .collect()
}

/// Signature fields as the slices `sign` and `verify` take.
fn as_slices(fields: &[Vec<u8>]) -> Vec<&[u8]> {
    fields.iter().map(Vec::as_slice).collect()
}

/// A `{generation, version}` value.
fn version_of(v: &Value) -> RecordVersion {
    RecordVersion {
        generation: num(v, "generation"),
        version: num(v, "version"),
    }
}

/// A signature case against its fields: the signed bytes, the executor's key, its
/// deterministic signature, which verifies, and which a changed version field and the other
/// signing label over the same fields refuse.
fn check_signature_case(case: &Value, label: &str, other_label: &str, fields: &[Vec<u8>]) {
    assert_eq!(st(case, "label"), label);
    let refs = as_slices(fields);
    assert_eq!(
        hex(&signed_bytes(label, &refs)),
        st(case, "signed_bytes_hex")
    );
    let executor = executor_keys();
    let public = b64n::<32>(st(case, "public_key"));
    assert_eq!(public, executor.signing_public());
    let signature = b64n::<64>(st(case, "signature"));
    verify(&public, label, &refs, &signature).unwrap();
    assert_eq!(executor.sign(label, &refs), signature);
    assert!(verify(&public, other_label, &refs, &signature).is_err());

    let mut other = fields.to_vec();
    // Field 6 is a u64 version in both cases.
    other[6] = 99u64.to_be_bytes().to_vec();
    assert!(verify(&public, label, &as_slices(&other), &signature).is_err());
}

conformance_test! {
    /// `keys.json` identities: each identity key from its seed label, with its Pubky ID, its
    /// Switch root and its naming key (Switch wire 1.3, 1.5).
    fn test_vectors_keys_identities() {
        let k = vector_file("keys.json");
        let identities = arr(&k, "identities");
        assert_eq!(identities.len(), 2);
        for case in identities {
            let name = st(case, "name");
            let key = IdentityKey::from_secret(seed(st(case, "seed_label")));
            assert_eq!(hex(&key.secret_bytes()), st(case, "secret_hex"), "{name}");
            assert_eq!(hex(&key.public()), st(case, "public_hex"), "{name}");
            assert_eq!(key.id(), st(case, "pubky_id"), "{name}");
            assert_eq!(hex(&key.switch_root()), st(case, "switch_root_hex"), "{name}");
            assert_eq!(b64(&key.naming_key().to_bytes()), st(case, "naming_key"), "{name}");
        }
    }
}

conformance_test! {
    /// `keys.json` apps: each app's signing and bundle keys from their seed labels, the bundle
    /// secret clamped, its App ID, and the switch permissions Paykit derives from its scopes
    /// (Switch wire 1.3, 3.1; Paykit wire 3.1).
    fn test_vectors_keys_apps() {
        let k = vector_file("keys.json");
        let apps = arr(&k, "apps");
        assert_eq!(apps.len(), 6);
        for case in apps {
            let name = st(case, "name");
            let signing = seed(st(case, "signing_seed_label"));
            let keys = AppKeys::from_secrets(signing, seed(st(case, "bundle_seed_label")));
            assert_eq!(hex(&signing), st(case, "signing_secret_hex"), "{name}");
            assert_eq!(b64(&keys.signing_public()), st(case, "signing_public"), "{name}");
            assert_eq!(hex(&keys.bundle_secret()), st(case, "bundle_secret_hex"), "{name}");
            assert_eq!(b64(&keys.bundle_public()), st(case, "bundle_public"), "{name}");
            assert_eq!(keys.app_id(), st(case, "app_id"), "{name}");
            assert_eq!(hex(&keys.app_id_bytes()), st(case, "app_id_bytes_hex"), "{name}");

            let owner = st(case, "signing_seed_label").split('/').next().unwrap();
            assert_eq!(identity_named(owner).id(), st(case, "identity"), "{name}");

            let scopes: Vec<String> = arr(case, "scopes")
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect();
            let permissions = permission_names(&permissions_for(&scopes));
            assert_eq!(json!(permissions), case["permissions"], "{name}");
        }
    }
}

conformance_test! {
    /// `keys.json` scope keys: `derive("pubky-switch/v1/scope-key", root, u64(data_epoch),
    /// scope)`, which another data epoch changes (Switch wire 1.5).
    fn test_vectors_keys_scope_keys() {
        let k = vector_file("keys.json");
        let cases = arr(&k, "scope_keys");
        assert_eq!(cases.len(), 4);
        for case in cases {
            let identity = identity_named(st(case, "identity"));
            let (epoch, scope) = (num(case, "data_epoch"), st(case, "scope"));
            let material = lp(&[&identity.switch_root(), &u64be(epoch), scope.as_bytes()]);
            assert_eq!(hex(&material), st(case, "key_material_hex"), "{scope}");
            assert_eq!(hex(&identity.scope_key(epoch, scope)), st(case, "key_hex"), "{scope}");
            assert_ne!(hex(&identity.scope_key(epoch + 1, scope)), st(case, "key_hex"));
        }
    }
}

conformance_test! {
    /// `signatures.json` executor grant: its fields rebuilt from the grant history entry of
    /// `documents.json`, its signed bytes and the executor's signature, which the ledger
    /// handoff's label over the same fields refuses (Paykit wire 5.4).
    fn test_vectors_signatures_executor_grant() {
        let v = vector_file("signatures.json");
        let case = &arr(&v, "signatures")[0];
        let fields = case_fields(case);

        let d = vector_file("documents.json");
        let entry = &d["executor_grant_history_entry"];
        let grant = &entry["grant"];
        let built = grant_fields(
            &identity_named("alice").public(),
            &identity_named("bob").public(),
            &AllowanceId::parse(ALLOWANCE_ID).unwrap(),
            &b64n::<32>(st(grant, "terms_hash")),
            num(grant, "confirmed_generation"),
            version_of(entry),
        );
        assert_eq!(built, fields);
        assert_eq!(st(grant, "sig"), st(case, "signature"));
        check_signature_case(case, EXECUTOR_GRANT_LABEL, LEDGER_HANDOFF_LABEL, &fields);
    }
}

conformance_test! {
    /// `signatures.json` ledger handoff: its fields from the case's versions and item hash,
    /// its signed bytes and the executor's signature, which the executor grant's label over
    /// the same fields refuses (Paykit wire 5.10).
    fn test_vectors_signatures_ledger_handoff() {
        let v = vector_file("signatures.json");
        let case = &arr(&v, "signatures")[1];
        let fields = case_fields(case);
        let built = handoff_fields(
            &identity_named("alice").public(),
            &identity_named("bob").public(),
            &AllowanceId::parse(ALLOWANCE_ID).unwrap(),
            version_of(&case["grant"]),
            version_of(&case["ledger"]),
            &b64n::<32>(st(case, "item_hash")),
        );
        assert_eq!(built, fields);
        check_signature_case(case, LEDGER_HANDOFF_LABEL, EXECUTOR_GRANT_LABEL, &fields);
    }
}

conformance_test! {
    /// `identifiers.json` request hashes and terms hash, from the exact message bytes of
    /// `messages.json`; the terms hash depends on the order of its two messages (Paykit
    /// wire 1.5).
    fn test_vectors_identifiers_hashes() {
        let v = vector_file("identifiers.json");
        let m = vector_file("messages.json");
        let cases = arr(&v, "request_hash");
        assert!(!cases.is_empty());
        for case in cases {
            let json = message_json(&m, st(case, "message"));
            assert_eq!(b64(&request_hash(&json)), st(case, "hash"));
        }

        let t = &v["terms_hash"];
        let proposal = message_json(&m, st(t, "proposal"));
        let acceptance = message_json(&m, st(t, "acceptance"));
        assert_eq!(
            hex(&terms_hash_material(&proposal, &acceptance)),
            st(t, "key_material_hex")
        );
        assert_eq!(b64(&terms_hash(&proposal, &acceptance)), st(t, "terms_hash"));
        assert_ne!(b64(&terms_hash(&acceptance, &proposal)), st(t, "terms_hash"));
    }
}

conformance_test! {
    /// `identifiers.json` obligation IDs: a request's from the payer's naming key, the payee,
    /// the request hash and the period start, blinded with the payer's naming key and not the
    /// payee's; a payment's without a request from its attempt ID alone (Paykit wire 1.5).
    fn test_vectors_identifiers_obligations() {
        let v = vector_file("identifiers.json");
        let cases = arr(&v, "obligations");
        assert_eq!(cases.len(), 3);
        for case in cases {
            let name = st(case, "name");
            let (material, obligation) = match case.get("attempt_id") {
                Some(attempt) => {
                    let attempt = AttemptId::parse(attempt.as_str().unwrap()).unwrap();
                    (payment_obligation_material(&attempt), payment_obligation(&attempt))
                }
                None => {
                    let nk = identity_with_id(st(case, "identity")).naming_key();
                    let payee = pubky(st(case, "payee"));
                    let rh = b64n::<32>(st(case, "request_hash"));
                    assert_eq!(b64(&nk.blind(&payee)), st(case, "blinded_payee"), "{name}");
                    assert_eq!(
                        b64(&nk.blind(&rh)),
                        st(case, "blinded_request_hash"),
                        "{name}"
                    );
                    let payee_nk = identity_with_id(st(case, "payee")).naming_key();
                    assert_ne!(b64(&payee_nk.blind(&payee)), st(case, "blinded_payee"));
                    let start = num(case, "period_start_secs");
                    (
                        request_obligation_material(&nk, &payee, &rh, start),
                        request_obligation(&nk, &payee, &rh, start),
                    )
                }
            };
            assert_eq!(hex(&material), st(case, "key_material_hex"), "{name}");
            assert_eq!(b64(&obligation), st(case, "obligation"), "{name}");
        }
    }
}

conformance_test! {
    /// `identifiers.json` policies: each collection's fixed policy, and `payee_activity`'s
    /// under the label `shop1`, with its scopes, the bytes it hashes and its policy hash
    /// (Paykit wire 1.5, Switch wire 1.5).
    fn test_vectors_identifiers_policies() {
        let v = vector_file("identifiers.json");
        let cases = arr(&v["policies"], "cases");
        assert_eq!(cases.len(), Collection::ALL.len() + 1);
        let shop1 = Label::parse(SHOP1).unwrap();
        for case in cases {
            let name = st(case, "name");
            let policy = match Collection::from_name(name) {
                Some(collection) => collection.fixed_policy(),
                None => Collection::PayeeActivity.policy(Some(&shop1)),
            };
            assert_eq!(policy.read_scope, st(case, "read_scope"), "{name}");
            assert_eq!(json!(policy.write_scopes), case["write_scopes"], "{name}");
            assert_eq!(
                case.get("permanent").cloned(),
                policy.permanent.map(|p| json!({ "generation": p.generation })),
                "{name}"
            );
            let policy_hash = policy.hash().unwrap();
            assert_eq!(b64(&policy_hash), st(case, "policy_hash"), "{name}");
            assert_eq!(hash(&unhex(st(case, "hashed_bytes_hex"))), policy_hash, "{name}");
        }
        // The label changes the policy.
        assert_ne!(
            Collection::PayeeActivity.policy(Some(&shop1)).hash().unwrap(),
            Collection::PayeeActivity.fixed_policy().hash().unwrap()
        );
    }
}

conformance_test! {
    /// `encoding.json` amounts: each valid amount keeps its spelling and reads back from its
    /// normalized one; each invalid amount is refused under Paykit wire 1.2.
    fn test_vectors_encoding_amounts() {
        let e = vector_file("encoding.json");
        let valid = arr(&e["amounts"], "valid");
        assert!(!valid.is_empty());
        for case in valid {
            let amount = Decimal::parse(st(case, "text")).unwrap();
            assert_eq!(amount.as_str(), st(case, "text"));
            assert_eq!(amount.normalized(), st(case, "normalized"));
            let again = Decimal::parse(st(case, "normalized")).unwrap();
            assert_eq!(again, amount);
        }
        let invalid = arr(&e["amounts"], "invalid");
        assert!(!invalid.is_empty());
        for case in invalid {
            let error = Decimal::parse(st(case, "text")).unwrap_err().to_string();
            assert!(error.contains("Paykit wire 1.2"), "{}: {error}", st(case, "why"));
        }
    }
}
