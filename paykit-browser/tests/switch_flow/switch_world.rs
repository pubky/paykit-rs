//! The test worlds the rig enrols on the real service (`POST /rig/world`), whose Paykit Apps
//! join it through `FetchTransport`, the rig's or one a test configures. The `switch_flow`,
//! `switch_faults` and `switch_http` targets include this module.

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_browser::clock::JsClock;
use paykit_browser::timer::JsTimer;
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{Paykit, PaykitConfig};
use pubky_switch_client::app::{AppEnv, HomeserverReader, HomeserverWriter, JoinRequest};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{MemoryStore, StoreHandle};
use pubky_switch_client::testing::{Seeds, TestHomeserver, TestIdentity};
use pubky_switch_client::transport::{Header, Transport};
use pubky_switch_core::enc::unb64;
use pubky_switch_core::records::{AppRequest, IdentityDoc, IDENTITY_PATH};
use pubky_switch_core::sig::SignedDoc;
use serde_json::{json, Value};

use crate::switch_rig::{origin, request, rig_token, transport, transport_with};

/// The payer's identity.
pub const PAYER: &str = "payer";
/// The payee's identity.
pub const PAYEE: &str = "payee";

/// How long the rig may take to enrol a world, in milliseconds: it runs the real signer for
/// every identity and app, on a service other engines and contexts of the run also use. The
/// rig stops its enrol program after 45 s, so a slow enrolment fails with the rig's answer
/// rather than this timeout.
const WORLD_TIMEOUT_MS: u64 = 60_000;

/// A world the rig enrolled on the real service for one test (`POST /rig/world`): its
/// identities, enrolled by their signers, and their apps, approved but not joined yet; the
/// in-memory Homeserver the apps read and write, holding each identity's latest
/// `identity.json`; and the SDK's fake rail, on the browser's clock.
///
/// Each world has keys of its own, from a seed prefix the rig picks at random, so no two
/// tests, engines, contexts or runs share an identity, an app or a path at the service: an
/// app joins a service once.
pub struct SwitchWorld {
    seeds: Seeds,
    homeserver: Arc<TestHomeserver>,
    /// The approved apps, by `<identity>:<app>`.
    apps: BTreeMap<String, EnrolledApp>,
    /// The rail every app of the world pays through.
    pub rail: Arc<FakeRail>,
}

/// An app the rig's signer approved, from the world's output (`rig/switch_enroll.rs`).
struct EnrolledApp {
    /// The signed app request's JSON.
    request: Vec<u8>,
    /// The approval's bytes.
    approval: Vec<u8>,
}

impl SwitchWorld {
    /// Asks the rig for a new world with `identities` and their `apps`, given as
    /// `(identity, app)`, each app asking for `paykit_pay` and `paykit_request` and the scopes
    /// they require. It puts each identity's `identity.json` on the world's Homeserver, and
    /// checks that each identity and app the rig enrolled has the keys the seed prefix gives.
    ///
    /// # Panics
    ///
    /// When the target was built without the rig's token, when the rig refuses or fails, or
    /// when its answer is not a world for these names.
    pub async fn enrol(identities: &[&str], apps: &[(&str, &str)]) -> Self {
        let body = json!({
            "identities": identities,
            "apps": apps
                .iter()
                .map(|(identity, app)| json!({ "identity": identity, "app": app }))
                .collect::<Vec<_>>(),
        });
        let mut post = request("POST", &format!("/rig/world?token={}", rig_token()));
        post.headers
            .push(Header::new("content-type", "application/json"));
        post.body = serde_json::to_vec(&body).expect("the world request is JSON");
        let response = transport_with(|config| config.request_timeout_ms = WORLD_TIMEOUT_MS)
            .send(post)
            .await
            .expect("the rig answers /rig/world");
        assert_eq!(
            response.status,
            200,
            "/rig/world: {}",
            String::from_utf8_lossy(&response.body)
        );
        let world: Value = serde_json::from_slice(&response.body).expect("/rig/world answers JSON");
        assert_eq!(text(&world, "origin"), origin(), "the world's origin");
        let seeds = Seeds::new(text(&world, "seed_prefix"));

        let homeserver = Arc::new(TestHomeserver::new());
        let enrolled = world["identities"]
            .as_object()
            .expect("the world lists its identities");
        assert_eq!(enrolled.len(), identities.len(), "the world's identities");
        for name in identities {
            let entry = &enrolled[*name];
            let identity = TestIdentity::new(&seeds, name);
            assert_eq!(
                text(entry, "id"),
                identity.key().id(),
                "the Pubky ID of {name}"
            );
            homeserver.put(
                identity.public(),
                IDENTITY_PATH,
                bytes(entry, "identity_json"),
            );
        }

        let listed = world["apps"].as_object().expect("the world lists its apps");
        assert_eq!(listed.len(), apps.len(), "the world's apps");
        let mut enrolled_apps = BTreeMap::new();
        for (identity, app) in apps {
            let label = format!("{identity}:{app}");
            let entry = &listed[&label];
            assert_eq!(
                (text(entry, "identity"), text(entry, "app")),
                (*identity, *app),
                "{label}"
            );
            assert_eq!(
                text(entry, "app_id"),
                TestIdentity::new(&seeds, identity).app_keys(app).app_id(),
                "the App ID of {label}"
            );
            let enrolled = EnrolledApp {
                request: bytes(entry, "request"),
                approval: bytes(entry, "approval"),
            };
            enrolled_apps.insert(label, enrolled);
        }

        let rail = Arc::new(FakeRail::with_clock(Arc::new(JsClock) as Arc<dyn Clock>));
        Self {
            seeds,
            homeserver,
            apps: enrolled_apps,
            rail,
        }
    }

    /// Identity `name` of this world, with the keys its seed gives, enrolled or not.
    pub fn identity(&self, name: &str) -> TestIdentity {
        TestIdentity::new(&self.seeds, name)
    }

    /// The public key of identity `name` of this world, enrolled or not.
    pub fn id(&self, name: &str) -> [u8; 32] {
        self.identity(name).public()
    }

    /// The `identity.json` the rig's signer published for identity `name`, verified.
    ///
    /// # Panics
    ///
    /// When the world enrolled no such identity.
    #[allow(dead_code)] // `switch_http` and `switch_faults` only.
    pub fn identity_doc(&self, name: &str) -> IdentityDoc {
        self.homeserver
            .identity_doc(&self.id(name))
            .unwrap_or_else(|| panic!("the world has no identity.json for {name}"))
    }

    /// Joins app `label` (`<identity>:<app>`) with the default transport of the network
    /// targets ([`transport`]); see [`SwitchWorld::join_with`].
    pub async fn join(&self, label: &str) -> Paykit {
        self.join_with(label, Arc::new(transport())).await
    }

    /// Joins app `label` (`<identity>:<app>`) to the service with the request and approval
    /// the rig's signer made, and finishes the join with a first receive. The app reaches the
    /// service through `transport`, keeps its state in memory, reads the browser's clock,
    /// waits on the browser's timers, and pays through the world's rail.
    ///
    /// # Panics
    ///
    /// When the world has no such app, or the join or its first receive fails.
    pub async fn join_with(&self, label: &str, transport: Arc<dyn Transport>) -> Paykit {
        let (Some(app), Some((identity, name))) = (self.apps.get(label), label.split_once(':'))
        else {
            panic!("the world has no app {label}");
        };
        let identity = self.identity(identity);
        let signed: SignedDoc =
            serde_json::from_slice(&app.request).expect("the app request is a signed document");
        let join = JoinRequest {
            identity: identity.public(),
            keys: identity.app_keys(name),
            request: AppRequest::verify(&signed).expect("the app request verifies"),
            approval: app.approval.clone(),
        };
        let env = AppEnv {
            store: StoreHandle::inline(Arc::new(MemoryStore::new())),
            transport,
            clock: Arc::new(JsClock),
            homeserver: Arc::clone(&self.homeserver) as Arc<dyn HomeserverReader>,
            homeserver_writer: Some(Arc::clone(&self.homeserver) as Arc<dyn HomeserverWriter>),
        };
        let mut paykit = Paykit::join(
            env,
            PaykitConfig::default(),
            join,
            Arc::clone(&self.rail) as Arc<_>,
            Arc::new(JsTimer),
        )
        .await
        .unwrap_or_else(|error| panic!("{label} joins: {error:?}"));
        paykit
            .receive(None)
            .await
            .unwrap_or_else(|error| panic!("{label}'s first receive: {error:?}"));
        paykit
    }
}

/// Reads `paykit`'s feed to its end and sends the cursor move the read deferred (Switch spec
/// 8.2), so a wait on the feed taken next starts quiet: neither woken at once by what the
/// join left on the feed nor cut short when the deferred move falls due.
///
/// # Panics
///
/// When the read or the move fails.
#[allow(dead_code)] // `switch_http` only.
pub async fn settle(paykit: &mut Paykit) {
    paykit
        .receive(None)
        .await
        .unwrap_or_else(|error| panic!("the settling receive: {error:?}"));
    paykit
        .app_mut()
        .flush_cursor()
        .await
        .unwrap_or_else(|error| panic!("the deferred cursor move: {error:?}"));
}

/// The text member `name` of `object`, from a world's output.
fn text<'a>(object: &'a Value, name: &str) -> &'a str {
    object[name]
        .as_str()
        .unwrap_or_else(|| panic!("the world has no text {name}"))
}

/// The bytes member `name` of `object`, from a world's output, base64url without padding
/// (Switch wire 1.2).
fn bytes(object: &Value, name: &str) -> Vec<u8> {
    unb64(text(object, name)).unwrap_or_else(|_| panic!("{name} is not base64url"))
}
