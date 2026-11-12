//! Helpers shared by the vector tests: loading the two sets, the fixed keys and the Switch
//! set's certificates, and writing the cases paykit-lib generates.

#![allow(dead_code)]

use std::path::PathBuf;

use pubky_switch_core::enc::{pubky_key, unb64_array};
use pubky_switch_core::keys::{AppKeys, IdentityKey};
use pubky_switch_core::records::AppCert;
use pubky_switch_core::scopes::Permission;
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

/// The scopes of Paykit's own executor certificate (Paykit spec 3.1): the Switch set's, with
/// `contacts`, which `allowance_execute` requires.
pub const EXECUTOR_SCOPES: [&str; 4] = ["pay", "allowance", "allowance_execute", "contacts"];

/// The scopes of Paykit's own phone certificate (Paykit spec 3.1): the Switch set's, with
/// `contacts`, which `pay` requires.
pub const PHONE_SCOPES: [&str; 3] = ["pay", "allowance", "contacts"];

/// The scopes of the certificates this set signs itself, by app name: the apps whose Switch
/// set certificate lacks `contacts`, which Paykit requires beside `pay` and
/// `allowance_execute` (Paykit spec 3.1).
pub fn paykit_signed_scopes(app: &str) -> Option<&'static [&'static str]> {
    match app {
        "executor" => Some(&EXECUTOR_SCOPES),
        "phone" => Some(&PHONE_SCOPES),
        _ => None,
    }
}

/// The certificate of an app, by app name: the Switch set's `documents.json` certificate, but
/// the executor's and the phone's, which this set signs itself ([`paykit_cert`]).
pub fn cert_doc(app: &str) -> SignedDoc {
    if paykit_signed_scopes(app).is_some() {
        return paykit_cert(&Keys::load(), app).signed().clone();
    }
    let name = match app {
        "cap" => "cap certificate, extension scope ext:paykit-cap",
        "hosted" => "hosted certificate, labelled selling scope request:shop1",
        other => return document(&format!("{other} certificate")),
    };
    document(name)
}

/// Paykit's own certificate of `app`, the executor or the phone: the Switch set's certificate
/// with [`paykit_signed_scopes`] and the permissions they need, signed again by alice's
/// identity key. The Switch set's hold no `contacts`, so Paykit's verifiers refuse them
/// (Paykit spec 3.1).
pub fn paykit_cert(keys: &Keys, app: &str) -> AppCert {
    let scopes = paykit_signed_scopes(app).unwrap_or_else(|| panic!("{app} isn't Paykit's"));
    let app = keys.app(app);
    let switch_cert = document(&format!("{} certificate", app.name));
    let mut body = AppCert::verify(&switch_cert, &app.identity)
        .unwrap()
        .body()
        .clone();
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
    body.permissions =
        pubky_switch_core::scopes::permission_names(&paykit_lib::scopes::permissions_for(&scopes));
    body.scopes = scopes;
    AppCert::issue(keys.identity(&app.identity), body).unwrap()
}

/// Paykit's own executor certificate ([`paykit_cert`]).
pub fn executor_cert(keys: &Keys) -> AppCert {
    paykit_cert(keys, "executor")
}

/// Paykit's own phone certificate ([`paykit_cert`]).
pub fn phone_cert(keys: &Keys) -> AppCert {
    paykit_cert(keys, "phone")
}

/// The scopes of an app's certificate in this set: the Switch set's `keys.json` scopes, and
/// [`paykit_signed_scopes`] for the certificates this set signs itself.
pub fn scopes_of(app: &App) -> Vec<String> {
    match paykit_signed_scopes(&app.name) {
        Some(scopes) => scopes.iter().map(|s| (*s).to_owned()).collect(),
        None => app.scopes.clone(),
    }
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

/// A carried signed document as the files write it.
pub fn signed_json(doc: &SignedDoc) -> Value {
    serde_json::json!({ "body": doc.body, "sig": doc.sig })
}

/// A signed document's body text.
pub fn body_text(doc: &SignedDoc) -> String {
    String::from_utf8(doc.body_bytes().unwrap()).unwrap()
}

/// The Switch set's certificate of `app` with `scopes` and `permissions` changed, signed again
/// by its identity key: the switch's checks take it (Switch wire 3.1).
pub fn reissued(keys: &Keys, app: &str, scopes: &[&str], permissions: &[Permission]) -> AppCert {
    let app = keys.app(app);
    let mut body = cert(app).body().clone();
    body.scopes = scopes.iter().map(|s| (*s).to_owned()).collect();
    body.permissions = pubky_switch_core::scopes::permission_names(permissions);
    AppCert::issue(keys.identity(&app.identity), body).unwrap()
}

/// A refusal's reason as the files quote it: without the section it starts with (README
/// "Rejections").
pub fn quoted(error: &str) -> String {
    let (kind, rest) = error.split_once(": ").unwrap_or(("", error));
    match rest.split_once(": ") {
        Some((section, tail)) if section.starts_with("Paykit ") => format!("{kind}: {tail}"),
        _ => error.to_owned(),
    }
}

/// Where a case that paykit-lib generates lies in its vector file (README "How the files were
/// generated"), as a JSON pointer.
#[derive(Clone, Copy, Debug)]
pub enum Place<'a> {
    /// A member paykit-lib writes whole.
    Member(&'a str),
    /// A case of an array, found by its `name`.
    Case(&'a str),
}

/// The environment variable that makes the vector tests write the cases paykit-lib generates
/// into `specs/v0.2/vectors/` before comparing them, after an intended change. Test-only: the
/// generator runs only in these tests.
pub const WRITE_VECTORS: &str = "PAYKIT_WRITE_VECTORS";

/// Compare the cases paykit-lib generates for the file `name` with the file, value for value;
/// with [`WRITE_VECTORS`] set, write them into the file first. Every file is JSON written with
/// sorted members, indented by two spaces, and a final newline, so writing one changes no
/// other byte of it.
pub fn generated(name: &str, cases: &[(Place<'_>, Value)]) {
    if std::env::var_os(WRITE_VECTORS).is_some() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../specs/v0.2/vectors")
            .join(name);
        let mut file = paykit(name);
        for (place, value) in cases {
            match place {
                Place::Member(pointer) => {
                    let (parent, member) = pointer.rsplit_once('/').expect("a member pointer");
                    file.pointer_mut(parent)
                        .and_then(Value::as_object_mut)
                        .unwrap_or_else(|| panic!("{name}: no object at {parent}"))
                        .insert(member.to_owned(), value.clone());
                }
                Place::Case(pointer) => {
                    let list = file
                        .pointer_mut(pointer)
                        .and_then(Value::as_array_mut)
                        .unwrap_or_else(|| panic!("{name}: no array at {pointer}"));
                    match list.iter_mut().find(|c| c["name"] == value["name"]) {
                        Some(held) => *held = value.clone(),
                        None => list.push(value.clone()),
                    }
                }
            }
        }
        let mut text = serde_json::to_string_pretty(&file).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    let file = paykit(name);
    for (place, value) in cases {
        let held = match place {
            Place::Member(pointer) => file.pointer(pointer),
            Place::Case(pointer) => file
                .pointer(pointer)
                .and_then(Value::as_array)
                .and_then(|list| list.iter().find(|c| c["name"] == value["name"])),
        };
        assert_eq!(
            held,
            Some(value),
            "{name}: {place:?} isn't what paykit-lib generates; set {WRITE_VECTORS} after an intended change"
        );
    }
}
