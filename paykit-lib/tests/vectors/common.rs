//! Helpers shared by the vector tests: loading the files, the fixed keys derived from their
//! seed labels, the certificates and app records built from them, and writing the cases
//! paykit-lib generates.

#![allow(dead_code)]

use std::path::PathBuf;

use paykit_lib::kinds::Kind;
use pubky_switch_core::enc::{hash, pubky_key, unb64_array};
use pubky_switch_core::keys::{AppKeys, IdentityKey};
use pubky_switch_core::records::{AppCert, AppCertBody, AppRecord, AppRecordBody};
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

/// The prefix of every seed label: each secret of the set is
/// `BLAKE3("paykit/v1/test-vectors/" + seed label)`, an Ed25519 seed as it is and an X25519
/// secret clamped as RFC 7748 requires (Switch wire 1.3).
pub const SEED_PREFIX: &str = "paykit/v1/test-vectors/";

/// The 32 secret bytes of a seed label, `BLAKE3(SEED_PREFIX + label)`.
pub fn seed(label: &str) -> [u8; 32] {
    hash(format!("{SEED_PREFIX}{label}").as_bytes())
}

/// The identities of the set, by name: alice, the payer, and bob, the payee.
pub const IDENTITIES: [&str; 2] = ["alice", "bob"];

/// The seed label of an identity's key.
pub fn identity_label(name: &str) -> String {
    format!("{name}/identity")
}

/// When every app certificate of the set is issued.
pub const CERT_ISSUED_AT: &str = "2025-12-01T00:00:00.000Z";

/// One app of the set as its seeds and its base certificate define it.
pub struct AppSeed {
    /// The app's name.
    pub name: &'static str,
    /// The name of the identity it belongs to.
    pub owner: &'static str,
    /// The scopes of its base certificate ([`base_cert`]).
    pub scopes: &'static [&'static str],
    /// When its certificates expire.
    pub expires_at: &'static str,
}

/// The apps of the set: alice's wallet, phone and executor, and bob's shop, his extension
/// app `cap` and his `hosted` app, which sells under the label `shop1`.
pub const APPS: [AppSeed; 6] = [
    AppSeed {
        name: "wallet",
        owner: "alice",
        scopes: &["pay", "contacts", "manage_contacts"],
        expires_at: "2026-12-01T00:00:00.000Z",
    },
    AppSeed {
        name: "phone",
        owner: "alice",
        scopes: &["pay", "allowance"],
        expires_at: "2027-01-01T00:00:00.000Z",
    },
    AppSeed {
        name: "executor",
        owner: "alice",
        scopes: &["pay", "allowance", "allowance_execute"],
        expires_at: "2026-12-01T00:00:00.000Z",
    },
    AppSeed {
        name: "shop",
        owner: "bob",
        scopes: &["request", "allowance"],
        expires_at: "2026-12-01T00:00:00.000Z",
    },
    AppSeed {
        name: "cap",
        owner: "bob",
        scopes: &["request", "ext:paykit-cap"],
        expires_at: "2026-12-01T00:00:00.000Z",
    },
    AppSeed {
        name: "hosted",
        owner: "bob",
        scopes: &["request:shop1"],
        expires_at: "2026-12-01T00:00:00.000Z",
    },
];

/// The extension kind bob's `cap` app accepts, under its scope `ext:paykit-cap`.
pub const CAP_EXTENSION_KIND: &str = "paykit-cap.offer";

/// The label of bob's labelled selling scope, which his `hosted` app holds.
pub const SHOP1: &str = "shop1";

/// One app of the set, with its keys derived from its seed labels.
pub struct App {
    pub name: String,
    pub owner: String,
    pub identity: [u8; 32],
    pub app_id: String,
    pub keys: AppKeys,
    /// The scopes of its base certificate ([`base_cert`]).
    pub scopes: Vec<String>,
    pub expires_at: u64,
}

impl App {
    /// The seed label of its signing key.
    pub fn signing_label(&self) -> String {
        format!("{}/{}/signing", self.owner, self.name)
    }

    /// The seed label of its bundle key.
    pub fn bundle_label(&self) -> String {
        format!("{}/{}/bundle", self.owner, self.name)
    }
}

/// The fixed keys of the set, derived from their seed labels.
pub struct Keys {
    pub identities: Vec<(String, IdentityKey)>,
    pub apps: Vec<App>,
}

impl Keys {
    /// Every key of the set from its seed label ([`seed`]).
    pub fn from_seeds() -> Self {
        let identities: Vec<(String, IdentityKey)> = IDENTITIES
            .iter()
            .map(|name| {
                let key = IdentityKey::from_secret(seed(&identity_label(name)));
                ((*name).to_owned(), key)
            })
            .collect();
        let apps = APPS
            .iter()
            .map(|a| {
                let mut app = App {
                    name: a.name.to_owned(),
                    owner: a.owner.to_owned(),
                    identity: [0; 32],
                    app_id: String::new(),
                    keys: AppKeys::from_secrets([0; 32], [0; 32]),
                    scopes: a.scopes.iter().map(|s| (*s).to_owned()).collect(),
                    expires_at: time(a.expires_at),
                };
                app.keys =
                    AppKeys::from_secrets(seed(&app.signing_label()), seed(&app.bundle_label()));
                app.app_id = app.keys.app_id();
                app.identity = identities
                    .iter()
                    .find(|(name, _)| name == a.owner)
                    .unwrap()
                    .1
                    .public();
                app
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

    pub fn identity_named(&self, name: &str) -> &IdentityKey {
        &self.identities.iter().find(|(n, _)| n == name).unwrap().1
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

/// The base certificate of an app: its [`APPS`] scopes with the permissions Paykit's scopes
/// need, issued at [`CERT_ISSUED_AT`] by its identity key (Switch wire 3.1).
pub fn base_cert(keys: &Keys, app: &str) -> AppCert {
    let app = keys.app(app);
    let body = AppCertBody::new(
        app.identity,
        app.keys.signing_public(),
        app.keys.bundle_public(),
        app.scopes.clone(),
        &paykit_lib::scopes::permissions_for(&app.scopes),
        time(CERT_ISSUED_AT),
        app.expires_at,
    );
    AppCert::issue(keys.identity(&app.identity), body).unwrap()
}

/// The Paykit kinds a payee app takes from payers: answers to its requests, proofs, and the
/// requests for its list and for a resend.
pub const PAYEE_ACCEPTS: [Kind; 6] = [
    Kind::PaymentRequestAcceptance,
    Kind::PaymentRequestRejection,
    Kind::PaymentRequestCancellation,
    Kind::PaymentProof,
    Kind::PrivatePaymentListRequest,
    Kind::ResendRequest,
];

/// The Allowance kinds the shop takes besides [`PAYEE_ACCEPTS`].
pub const SHOP_ALLOWANCE_ACCEPTS: [Kind; 4] = [
    Kind::AllowanceProposal,
    Kind::AllowanceAcceptance,
    Kind::AllowanceRejection,
    Kind::AllowanceEnd,
];

/// The endpoint identifiers bob's apps state in their Paykit member (Paykit wire 3.2).
pub const BOB_ENDPOINT_IDENTIFIERS: [&str; 2] = ["btc-lightning-bolt11", "btc-bitcoin-p2wpkh"];

/// The shop's public BOLT12 offer, in its Paykit member.
pub const SHOP_BOLT12_OFFER: &str = "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzr";

/// An app record of bob's, signed by the app's signing key (Switch wire 3.2): it carries the
/// app's base certificate, accepts `accepts`, and holds a Paykit member stating
/// [`BOB_ENDPOINT_IDENTIFIERS`] and `public_endpoints`.
fn bob_app_record(
    keys: &Keys,
    app: &str,
    accepts: Vec<String>,
    public_endpoints: Value,
    updated_at: &str,
    expires_at: Option<&str>,
) -> AppRecord {
    let mut body = AppRecordBody::new(&base_cert(keys, app), accepts, time(updated_at));
    body.protocols.insert(
        paykit_lib::app_record::PROTOCOL_NAME.to_owned(),
        serde_json::json!({
            "capabilities": { "endpoint_identifiers": BOB_ENDPOINT_IDENTIFIERS },
            "public_endpoints": public_endpoints,
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    body.label = Some(format!("bob {app}"));
    body.expires_at = expires_at.map(time);
    AppRecord::sign(&keys.app(app).keys, body).unwrap()
}

/// The shop's app record: [`PAYEE_ACCEPTS`] and [`SHOP_ALLOWANCE_ACCEPTS`], with its public
/// BOLT12 offer.
pub fn shop_app_record(keys: &Keys) -> AppRecord {
    let accepts = PAYEE_ACCEPTS
        .iter()
        .chain(&SHOP_ALLOWANCE_ACCEPTS)
        .map(|k| k.as_str().to_owned())
        .collect();
    bob_app_record(
        keys,
        "shop",
        accepts,
        serde_json::json!({ "btc-lightning-bolt12": SHOP_BOLT12_OFFER }),
        "2025-12-31T00:00:00.000Z",
        Some("2026-01-30T00:00:00.000Z"),
    )
}

/// The `cap` app's record: [`PAYEE_ACCEPTS`] and its extension kind [`CAP_EXTENSION_KIND`],
/// without public endpoints, never expiring.
pub fn cap_app_record(keys: &Keys) -> AppRecord {
    let accepts = PAYEE_ACCEPTS
        .iter()
        .map(|k| k.as_str().to_owned())
        .chain([CAP_EXTENSION_KIND.to_owned()])
        .collect();
    bob_app_record(
        keys,
        "cap",
        accepts,
        serde_json::json!({}),
        CERT_ISSUED_AT,
        None,
    )
}

/// The scopes of Paykit's own executor certificate (Paykit spec 3.1): the base certificate's,
/// with `contacts`, which `allowance_execute` requires.
pub const EXECUTOR_SCOPES: [&str; 4] = ["pay", "allowance", "allowance_execute", "contacts"];

/// The scopes of Paykit's own phone certificate (Paykit spec 3.1): the base certificate's,
/// with `contacts`, which `pay` requires.
pub const PHONE_SCOPES: [&str; 3] = ["pay", "allowance", "contacts"];

/// The scopes of the certificates this set holds beside the base ones, by app name: the apps
/// whose base certificate lacks `contacts`, which Paykit requires beside `pay` and
/// `allowance_execute` (Paykit spec 3.1).
pub fn paykit_signed_scopes(app: &str) -> Option<&'static [&'static str]> {
    match app {
        "executor" => Some(&EXECUTOR_SCOPES),
        "phone" => Some(&PHONE_SCOPES),
        _ => None,
    }
}

/// The certificate of an app, by app name: its base certificate ([`base_cert`]), but for the
/// executor and the phone Paykit's own ([`paykit_cert`]).
pub fn cert_doc(app: &str) -> SignedDoc {
    let keys = Keys::from_seeds();
    if paykit_signed_scopes(app).is_some() {
        return paykit_cert(&keys, app).signed().clone();
    }
    base_cert(&keys, app).signed().clone()
}

/// Paykit's own certificate of `app`, the executor or the phone: its base certificate with
/// [`paykit_signed_scopes`] and the permissions they need, signed by alice's identity key.
/// The base certificates hold no `contacts`, so Paykit's verifiers refuse them (Paykit spec
/// 3.1).
pub fn paykit_cert(keys: &Keys, app: &str) -> AppCert {
    let scopes = paykit_signed_scopes(app).unwrap_or_else(|| panic!("{app} isn't Paykit's"));
    let mut body = base_cert(keys, app).body().clone();
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
    body.permissions =
        pubky_switch_core::scopes::permission_names(&paykit_lib::scopes::permissions_for(&scopes));
    body.scopes = scopes;
    AppCert::issue(keys.identity(&keys.app(app).identity), body).unwrap()
}

/// Paykit's own executor certificate ([`paykit_cert`]).
pub fn executor_cert(keys: &Keys) -> AppCert {
    paykit_cert(keys, "executor")
}

/// Paykit's own phone certificate ([`paykit_cert`]).
pub fn phone_cert(keys: &Keys) -> AppCert {
    paykit_cert(keys, "phone")
}

/// The scopes of an app's certificate in this set: its [`APPS`] scopes, and
/// [`paykit_signed_scopes`] for the executor and the phone.
pub fn scopes_of(app: &App) -> Vec<String> {
    match paykit_signed_scopes(&app.name) {
        Some(scopes) => scopes.iter().map(|s| (*s).to_owned()).collect(),
        None => app.scopes.clone(),
    }
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

/// The certificate of `app` with `scopes` and `permissions` changed, signed again
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
/// with [`WRITE_VECTORS`] set, write them into the file first, creating it if the set doesn't
/// hold it. Every file is JSON written with
/// sorted members, indented by two spaces, and a final newline, so writing one changes no
/// other byte of it.
pub fn generated(name: &str, cases: &[(Place<'_>, Value)]) {
    if std::env::var_os(WRITE_VECTORS).is_some() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../specs/v0.2/vectors")
            .join(name);
        // A file the set doesn't hold yet starts as an empty object.
        let mut file = if path.exists() {
            paykit(name)
        } else {
            Value::Object(Default::default())
        };
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
        // The other tests read the file in parallel: write it beside and rename it into
        // place, so none of them reads it half written.
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, text).unwrap_or_else(|e| panic!("{}: {e}", partial.display()));
        std::fs::rename(&partial, &path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
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
