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
        self.try_message_with(to, kind, refs, body, |_| {})
    }

    /// As [`Party::try_message`], the message changed by `edit` before it is signed.
    pub(crate) fn try_message_with(
        &self,
        to: &Party,
        kind: &str,
        refs: Vec<[u8; 32]>,
        body: Map<String, Value>,
        edit: impl FnOnce(&mut Message),
    ) -> Result<StoredMessage, paykit_lib::error::Error> {
        self.stored_as(to, kind, refs, body, edit, Some(NOW))
    }

    /// A message of `kind` to `to` signed as created at `created_at_ms`, stored as accepted
    /// at `accepted_at`.
    pub(crate) fn message_accepted(
        &self,
        to: &Party,
        kind: &str,
        body: Map<String, Value>,
        created_at_ms: u64,
        accepted_at: Option<u64>,
    ) -> StoredMessage {
        let edit = |message: &mut Message| message.created_at_ms = created_at_ms;
        self.stored_as(to, kind, vec![], body, edit, accepted_at)
            .unwrap()
    }

    fn stored_as(
        &self,
        to: &Party,
        kind: &str,
        refs: Vec<[u8; 32]>,
        body: Map<String, Value>,
        edit: impl FnOnce(&mut Message),
        accepted_at: Option<u64>,
    ) -> Result<StoredMessage, paykit_lib::error::Error> {
        let mut message = Message::new(
            kind,
            Sender::of_app(self.id(), &self.keys),
            to.id(),
            NOW,
            refs,
            body,
        )
        .unwrap();
        edit(&mut message);
        let plaintext = message.sign(&self.keys).unwrap();
        StoredMessage::new(
            self.id(),
            self.cert.signed(),
            plaintext,
            accepted_at,
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

/// Two identities' switches, alice's and bob's, on one clock and Homeserver, for the unit
/// tests that drive a whole app: the setup of the flow tests' world, cut to what they use.
pub(crate) struct Switches {
    pub(crate) clock: std::sync::Arc<pubky_switch_client::clock::TestClock>,
    pub(crate) alice: pubky_switch_client::testing::TestSigner,
    pub(crate) bob: pubky_switch_client::testing::TestSigner,
    router: std::sync::Arc<pubky_switch_client::testing::Router>,
}

/// The host's timer, on the switches' clock.
struct ClockTimer(std::sync::Arc<pubky_switch_client::clock::TestClock>);

impl crate::Timer for ClockTimer {
    fn sleep(&self, ms: u64) -> pubky_switch_client::transport::BoxFuture<'static, ()> {
        self.0.advance_ms(ms);
        Box::pin(async {})
    }
}

impl Switches {
    pub(crate) fn new() -> Self {
        use pubky_switch_client::testing::{
            ProtocolSpec, Router, Seeds, TestHomeserver, TestIdentity, TestSigner, START_MS,
        };
        use std::sync::Arc;
        let seeds = Seeds::default();
        let clock = Arc::new(pubky_switch_client::clock::TestClock::new(START_MS));
        let homeserver = Arc::new(TestHomeserver::new());
        let spec = ProtocolSpec {
            keyed_scopes: [
                "paykit_pay",
                "paykit_request",
                "paykit_settlements",
                "paykit_allowance",
                "paykit_allowance_execute",
                "paykit_contacts",
                "paykit_manage_contacts",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
            ..ProtocolSpec::default()
        };
        let signer = |name: &str, origin: &str| {
            TestSigner::at(
                TestIdentity::new(&seeds, name),
                origin,
                Arc::clone(&clock),
                Arc::clone(&homeserver),
                spec.clone(),
            )
        };
        let alice = signer("alice", "https://alice-switch.example");
        let bob = signer("bob", "https://bob-switch.example");
        let router = Arc::new(Router::new());
        router.add_switch(alice.origin(), alice.switch());
        router.add_switch(bob.origin(), bob.switch());
        Self {
            clock,
            alice,
            bob,
            router,
        }
    }

    /// App `name` of `signer`, joined with `scopes` on a store of its own, its first read done.
    pub(crate) async fn join(
        &self,
        signer: &pubky_switch_client::testing::TestSigner,
        name: &str,
        scopes: crate::RequestedScopes,
    ) -> crate::Paykit {
        use std::sync::Arc;
        let keys = signer.identity().app_keys(name);
        let names: Vec<&str> = scopes.scopes.iter().map(String::as_str).collect();
        let spec = pubky_switch_client::testing::AppSpec::new(&names)
            .with_permissions(&scopes.permissions);
        let request = signer.approve(&keys, &spec).unwrap();
        let store = Arc::new(pubky_switch_client::store::MemoryStore::new());
        let env = signer.env_with(
            store,
            Arc::clone(&self.router) as Arc<dyn pubky_switch_client::transport::Transport>,
        );
        let rail = Arc::new(crate::rail::FakeRail::with_clock(
            Arc::clone(&self.clock) as Arc<dyn pubky_switch_client::clock::Clock>
        ));
        let mut app = crate::Paykit::join(
            env,
            crate::PaykitConfig::default(),
            request,
            rail,
            Arc::new(ClockTimer(Arc::clone(&self.clock))),
        )
        .await
        .unwrap();
        app.receive(None).await.unwrap();
        app
    }

    /// Bob's app `name`, holding only `paykit_request:<label>`.
    pub(crate) async fn labelled(&self, name: &str, label: &str) -> crate::Paykit {
        let label = paykit_lib::Label::parse(label).unwrap();
        let scopes = crate::AppScopes::new()
            .request_label(&label)
            .build()
            .unwrap();
        self.join(&self.bob, name, scopes).await
    }
}
