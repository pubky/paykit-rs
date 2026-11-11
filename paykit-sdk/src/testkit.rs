//! Fixtures for this crate's unit tests: identities, apps and signed messages from fixed
//! seeds. Tests only.

use paykit_lib::records::StoredMessage;
use paykit_lib::scopes::permissions_for;
use pubky_switch_core::keys::{AppKeys, IdentityKey};
use pubky_switch_core::message::{Message, Sender};
use pubky_switch_core::records::{
    AppCert, AppCertBody, AppRecord, AppRecordBody, IdentityBody, IdentityDoc, IdentityPolicy,
    Inbox, ListedApp, DEFAULT_CERT_LIFETIME_MS,
};
use pubky_switch_core::scopes::Permission;
use serde_json::{Map, Value};

/// 2026-01-01T00:00:00Z.
pub(crate) const NOW: u64 = 1_767_225_600_000;

fn seed(label: &str) -> [u8; 32] {
    pubky_switch_core::enc::hash(format!("paykit-sdk/test/{label}").as_bytes())
}

/// An identity and one of its apps.
pub(crate) struct Party {
    pub(crate) key: IdentityKey,
    pub(crate) keys: AppKeys,
    pub(crate) cert: AppCert,
}

impl Party {
    pub(crate) fn new(name: &str, app: &str, scopes: &[&str]) -> Self {
        let owned: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        Self::with_permissions(name, app, scopes, &permissions_for(&owned))
    }

    /// As [`Party::new`], its certificate granting `permissions`, which the Switch takes
    /// even when they aren't what its Paykit scopes need.
    pub(crate) fn with_permissions(
        name: &str,
        app: &str,
        scopes: &[&str],
        permissions: &[Permission],
    ) -> Self {
        let key = IdentityKey::from_secret(seed(&format!("{name}/identity")));
        let keys = AppKeys::from_secrets(
            seed(&format!("{name}/{app}/signing")),
            seed(&format!("{name}/{app}/bundle")),
        );
        let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        let cert = AppCert::issue(
            &key,
            AppCertBody::new(
                key.public(),
                keys.signing_public(),
                keys.bundle_public(),
                scopes.clone(),
                permissions,
                NOW,
                NOW + DEFAULT_CERT_LIFETIME_MS,
            ),
        )
        .unwrap();
        Self { key, keys, cert }
    }

    pub(crate) fn id(&self) -> [u8; 32] {
        self.key.public()
    }

    pub(crate) fn app_id(&self) -> [u8; 16] {
        self.cert.app_id()
    }

    /// A message of `kind` to `to`, stored as `to` would store it.
    pub(crate) fn message(
        &self,
        to: &Party,
        kind: &str,
        refs: Vec<[u8; 32]>,
        body: Map<String, Value>,
    ) -> StoredMessage {
        self.try_message(to, kind, refs, body).unwrap()
    }

    /// As [`Party::message`], with the refusal of a message its reader wouldn't store.
    pub(crate) fn try_message(
        &self,
        to: &Party,
        kind: &str,
        refs: Vec<[u8; 32]>,
        body: Map<String, Value>,
    ) -> Result<StoredMessage, paykit_lib::error::Error> {
        let message = Message::new(
            kind,
            Sender::of_app(self.id(), &self.keys),
            to.id(),
            NOW,
            refs,
            body,
        )
        .unwrap();
        let plaintext = message.sign(&self.keys).unwrap();
        StoredMessage::new(
            self.id(),
            self.cert.signed(),
            plaintext,
            Some(NOW),
            &to.id(),
        )
    }

    /// The app's record, accepting `accepts`.
    pub(crate) fn record(&self, accepts: &[&str]) -> AppRecord {
        AppRecord::sign(
            &self.keys,
            AppRecordBody::new(
                &self.cert,
                accepts.iter().map(|k| (*k).to_owned()).collect(),
                NOW,
            ),
        )
        .unwrap()
    }

    /// The identity's `identity.json`, listing `apps`.
    pub(crate) fn doc(&self, apps: &[&Party]) -> IdentityDoc {
        let mut body = IdentityBody::first(
            self.id(),
            [1; 16],
            Inbox::new("https://switch.example", [2; 32]),
            IdentityPolicy::default(),
        );
        for app in apps {
            body.apps.push(ListedApp {
                app_id: app.app_id(),
                cert: app.cert.hash(),
            });
        }
        IdentityDoc::sign(&self.key, body).unwrap()
    }
}

/// A JSON object.
pub(crate) fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
