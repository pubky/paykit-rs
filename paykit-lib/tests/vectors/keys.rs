//! `keys.json`: the fixed keys of the set, each derived from its seed label (Switch wire 1.3,
//! 1.5), with the scopes and permissions of each app's certificate in `documents.json`
//! `certificates` (Paykit wire 3.1).

use pubky_switch_core::enc::{b64, lp, pubky_id, u64be};
use pubky_switch_core::scopes::permission_names;
use serde_json::{json, Value};

use crate::common::{
    cert, generated, hex, identity_label, paykit, pubky, seed, Keys, Place, ALICE, BOB,
    SEED_PREFIX, SHOP1,
};

#[test]
fn test_vectors_keys() {
    let keys = Keys::from_seeds();
    generate(&keys);
    // The vectors' payer and payee are the seeded identities.
    assert_eq!(keys.identity_named("alice").id(), ALICE);
    assert_eq!(keys.identity_named("bob").id(), BOB);
    let k = paykit("keys.json");
    for app in k["apps"].as_array().unwrap() {
        let name = app["name"].as_str().unwrap();
        let held = keys.app(name);
        assert_eq!(pubky(app["identity"].as_str().unwrap()), held.identity);
        assert_eq!(app["app_id"], json!(held.keys.app_id()), "{name}");
    }
}

const DESCRIPTION: &str = "The fixed keys every other file of this set uses (Switch wire \
1.3, 1.5, 3.1). Each secret is BLAKE3(\"paykit/v1/test-vectors/\" + seed label); X25519 \
secrets (bundle keys) are then clamped as RFC 7748 requires, and Ed25519 secrets are 32-byte \
seeds. identities holds alice, the payer, and bob, the payee, with their Pubky IDs, their \
Switch roots, derive(\"pubky-switch/v1/scope-root\", identity secret), and their naming keys, \
derive(\"pubky-switch/v1/naming\", root), which blind every record key and claim input of \
this set. apps holds each app with its signing and bundle keys, its App ID and the scopes and \
switch permissions of its certificate in documents.json certificates: the executor's and \
the phone's hold contacts. scope_keys are derive(\"pubky-switch/v1/scope-key\", root, \
u64(data_epoch), scope), key_material_hex being the input after the label: alice's pay key \
under which sealed.json's values are sealed, and bob's request key and the key of his label \
shop1, which seals sealed.json's labelled record.";

/// The scope keys this set uses, by identity name, data epoch and scope.
fn scope_key_cases() -> [(&'static str, u64, String); 3] {
    [
        ("alice", 2, "pay".to_owned()),
        ("bob", 1, "request".to_owned()),
        ("bob", 1, format!("request:{SHOP1}")),
    ]
}

/// The cases paykit-lib generates in `keys.json`, compared with the file, and written into
/// it with `PAYKIT_WRITE_VECTORS`.
fn generate(keys: &Keys) {
    let identities: Vec<Value> = keys
        .identities
        .iter()
        .map(|(name, key)| {
            let label = identity_label(name);
            assert_eq!(key.secret_bytes(), seed(&label), "{name}");
            json!({
                "name": name,
                "naming_key": b64(&key.naming_key().to_bytes()),
                "pubky_id": key.id(),
                "public_hex": hex(&key.public()),
                "secret_hex": hex(&key.secret_bytes()),
                "seed_label": label,
                "switch_root_hex": hex(&key.switch_root()),
            })
        })
        .collect();
    let apps: Vec<Value> = keys
        .apps
        .iter()
        .map(|app| {
            let c = cert(app);
            json!({
                "app_id": app.app_id,
                "app_id_bytes_hex": hex(&app.keys.app_id_bytes()),
                "bundle_public": b64(&app.keys.bundle_public()),
                "bundle_secret_hex": hex(&app.keys.bundle_secret()),
                "bundle_seed_label": app.bundle_label(),
                "identity": pubky_id(&app.identity),
                "name": app.name,
                "permissions": permission_names(c.permissions()),
                "scopes": c.scopes(),
                "signing_public": b64(&app.keys.signing_public()),
                "signing_secret_hex": hex(&seed(&app.signing_label())),
                "signing_seed_label": app.signing_label(),
            })
        })
        .collect();
    let scope_keys: Vec<Value> = scope_key_cases()
        .iter()
        .map(|(name, epoch, scope)| {
            let identity = keys.identity_named(name);
            let material = lp(&[&identity.switch_root(), &u64be(*epoch), scope.as_bytes()]);
            json!({
                "data_epoch": epoch,
                "identity": name,
                "key_hex": hex(&identity.scope_key(*epoch, scope)),
                "key_material_hex": hex(&material),
                "scope": scope,
            })
        })
        .collect();
    assert!(DESCRIPTION.contains(SEED_PREFIX));
    generated(
        "keys.json",
        &[
            (Place::Member("/apps"), json!(apps)),
            (Place::Member("/description"), json!(DESCRIPTION)),
            (Place::Member("/identities"), json!(identities)),
            (Place::Member("/scope_keys"), json!(scope_keys)),
        ],
    );
}
