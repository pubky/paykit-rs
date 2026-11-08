//! Helpers shared by the vector tests: loading the two sets, the fixed keys and the Switch
//! set's certificates.

#![allow(dead_code)]

use std::path::PathBuf;

use pubky_switch_core::enc::{pubky_key, unb64_array};
use pubky_switch_core::keys::{AppKeys, IdentityKey};
use pubky_switch_core::records::AppCert;
use pubky_switch_core::sig::SignedDoc;
use pubky_switch_core::time::parse_rfc3339;
use serde_json::Value;

/// alice's Pubky ID, the payer of the vectors.
pub const ALICE: &str = "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy";
/// bob's Pubky ID, the payee of the vectors.
pub const BOB: &str = "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo";
/// The Allowance of the vectors.
pub const ALLOWANCE_ID: &str = "d0efffb7-191b-4709-b6c7-e343af5500b4";

/// A file of Paykit's set, `specs/v0.2/vectors/`.
pub fn paykit(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../specs/v0.2/vectors")
        .join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap()
}

/// A file of the Switch's set at the pinned revision.
pub fn switch(name: &str) -> Value {
    let path = pubky_switch_core::spec_vectors_dir().join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap()
}

/// A string member.
pub fn st<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} isn't a string"))
}

/// An array member.
pub fn arr<'a>(v: &'a Value, key: &str) -> &'a Vec<Value> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} isn't an array"))
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes from lower-case hex.
pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// 32 bytes from hex.
pub fn unhex32(text: &str) -> [u8; 32] {
    unhex(text).try_into().unwrap()
}

/// A Pubky ID's key.
pub fn pubky(id: &str) -> [u8; 32] {
    pubky_key(id).unwrap()
}

/// A time of the grammar, in Unix milliseconds.
pub fn time(text: &str) -> u64 {
    parse_rfc3339(text).unwrap()
}

/// Base64url of `N` bytes.
pub fn b64n<const N: usize>(text: &str) -> [u8; N] {
    unb64_array::<N>(text).unwrap()
}

/// One app of the Switch set's `keys.json`.
pub struct App {
    pub name: String,
    pub identity: [u8; 32],
    pub app_id: String,
    pub keys: AppKeys,
    pub scopes: Vec<String>,
}

/// The fixed keys of the Switch set's `keys.json`.
pub struct Keys {
    pub identities: Vec<(String, IdentityKey)>,
    pub apps: Vec<App>,
}

impl Keys {
    pub fn load() -> Self {
        let k = switch("keys.json");
        let identities = arr(&k, "identities")
            .iter()
            .map(|i| {
                let key = IdentityKey::from_secret(unhex32(st(i, "secret_hex")));
                assert_eq!(key.id(), st(i, "pubky_id"));
                (st(i, "name").to_owned(), key)
            })
            .collect();
        let apps = arr(&k, "apps")
            .iter()
            .map(|a| {
                let keys = AppKeys::from_secrets(
                    unhex32(st(a, "signing_secret_hex")),
                    unhex32(st(a, "bundle_secret_hex")),
                );
                assert_eq!(keys.app_id(), st(a, "app_id"));
                App {
                    name: st(a, "name").to_owned(),
                    identity: pubky(st(a, "identity")),
                    app_id: st(a, "app_id").to_owned(),
                    keys,
                    scopes: arr(a, "scopes")
                        .iter()
                        .map(|s| s.as_str().unwrap().to_owned())
                        .collect(),
                }
            })
            .collect();
        Self { identities, apps }
    }

    pub fn identity(&self, public: &[u8; 32]) -> &IdentityKey {
        &self
            .identities
            .iter()
            .find(|(_, k)| &k.public() == public)
            .unwrap()
            .1
    }

    pub fn app(&self, name: &str) -> &App {
        self.apps.iter().find(|a| a.name == name).unwrap()
    }

    pub fn app_by_id(&self, app_id: &str) -> &App {
        self.apps
            .iter()
            .find(|a| a.app_id == app_id)
            .unwrap_or_else(|| panic!("no app {app_id}"))
    }
}

/// The certificate of an app of the Switch set's `documents.json`, by app name.
pub fn cert_doc(app: &str) -> SignedDoc {
    let name = match app {
        "cap" => "cap certificate, extension scope ext:paykit-cap",
        "hosted" => "hosted certificate, labelled selling scope request:shop1",
        other => return document(&format!("{other} certificate")),
    };
    document(name)
}

/// A signed document of the Switch set's `documents.json`, by name.
pub fn document(name: &str) -> SignedDoc {
    let d = switch("documents.json");
    let doc = arr(&d, "documents")
        .iter()
        .find(|x| st(x, "name") == name)
        .unwrap_or_else(|| panic!("no document {name}"));
    signed(&doc["signed"])
}

/// A carried signed document.
pub fn signed(v: &Value) -> SignedDoc {
    SignedDoc {
        body: st(v, "body").to_owned(),
        sig: st(v, "sig").to_owned(),
    }
}

/// An app's certificate, verified as the Switch verifies it.
pub fn cert(app: &App) -> AppCert {
    AppCert::verify(&cert_doc(&app.name), &app.identity).unwrap()
}

/// A message of Paykit's `messages.json`, by name.
pub fn message(name: &str) -> Value {
    let m = paykit("messages.json");
    arr(&m, "messages")
        .iter()
        .find(|x| st(x, "name") == name)
        .unwrap_or_else(|| panic!("no message {name}"))
        .clone()
}

/// A message's exact JSON bytes.
pub fn message_json(name: &str) -> Vec<u8> {
    st(&message(name), "json").as_bytes().to_vec()
}
