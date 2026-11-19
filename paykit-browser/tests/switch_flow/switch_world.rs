//! The test worlds the rig enrols on the real service (`POST /rig/world`), whose Paykit Apps
//! join it through `FetchTransport`.

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_browser::clock::JsClock;
use paykit_browser::fetch::FetchTransport;
use paykit_browser::timer::JsTimer;
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{Paykit, PaykitConfig};
use pubky_switch_client::app::{AppEnv, HomeserverReader, HomeserverWriter, JoinRequest};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{MemoryStore, StoreHandle};
use pubky_switch_client::testing::{Seeds, TestHomeserver, TestIdentity};
use pubky_switch_client::transport::{Header, Transport};
use pubky_switch_core::enc::unb64;
use pubky_switch_core::records::{AppRequest, IDENTITY_PATH};
use pubky_switch_core::sig::SignedDoc;
use serde_json::{json, Value};
use wasm_bindgen_test::wasm_bindgen_test;

use crate::switch_rig::{config, origin, request, transport};

/// How long the rig may take to enrol a world, in milliseconds: it runs the real signer for
/// every identity and app, on a service other engines and contexts of the run also use. The
/// rig stops its enrol program after 45 s, so a slow enrolment fails with the rig's answer
/// rather than this timeout.
const WORLD_TIMEOUT_MS: u64 = 60_000;

/// The token the rig requires for `POST /rig/world`, which `rig/test.sh --switch` makes for
/// each run and sets at compile time: the rig refuses a request without it, so a page or
/// program that does not know it cannot make the rig enrol.
///
/// # Panics
///
/// When the target was built without it: a network test never passes without its rig.
fn rig_token() -> &'static str {
    option_env!("PAYKIT_BROWSER_RIG_TOKEN").unwrap_or_else(|| {
        panic!(
            "PAYKIT_BROWSER_RIG_TOKEN is not set: run the network tests with \
             `paykit-browser/rig/test.sh --switch`"
        )
    })
}

/// A world the rig enrolled on the real service for one test (`POST /rig/world`): its
/// identities, enrolled by their signers, and their apps, approved but not joined yet; the
/// in-memory Homeserver the apps read and write, holding each identity's latest
/// `identity.json`; and the SDK's fake rail, on the browser's clock.
///
/// Each world has keys of its own, from a seed prefix the rig picks at random, so no two
/// tests, engines, contexts or runs share an identity or an app: an app joins a service once.
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
        let mut slow = config();
        slow.request_timeout_ms = WORLD_TIMEOUT_MS;
        let response = FetchTransport::allowing_localhost_for_loopback_tests(slow)
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

    /// The public key of identity `name` of this world, enrolled or not.
    pub fn id(&self, name: &str) -> [u8; 32] {
        TestIdentity::new(&self.seeds, name).public()
    }

    /// Joins app `label` (`<identity>:<app>`) to the service with the request and approval
    /// the rig's signer made, and finishes the join with a first receive. The app reaches the
    /// service through [`transport`], keeps its state in memory, reads the browser's clock,
    /// waits on the browser's timers, and pays through the world's rail.
    ///
    /// # Panics
    ///
    /// When the world has no such app, or the join or its first receive fails.
    pub async fn join(&self, label: &str) -> Paykit {
        let (Some(app), Some((identity, name))) = (self.apps.get(label), label.split_once(':'))
        else {
            panic!("the world has no app {label}");
        };
        let identity = TestIdentity::new(&self.seeds, identity);
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
            transport: Arc::new(transport()),
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

/// The rig refuses `POST /rig/world` with `403`, before it enrols anything, without the run's
/// token and with a wrong token of the same length. Every other test here enrols its world
/// with the right token.
#[wasm_bindgen_test]
async fn test_switch_flow_rig_world_refuses_a_missing_or_wrong_token() {
    let token = rig_token();
    let (head, last) = token.split_at(token.len().saturating_sub(1));
    let wrong = format!("{head}{}", if last == "0" { "1" } else { "0" });
    for (case, path) in [
        ("no token", "/rig/world".to_owned()),
        ("a wrong token", format!("/rig/world?token={wrong}")),
    ] {
        let mut post = request("POST", &path);
        post.headers
            .push(Header::new("content-type", "application/json"));
        post.body = br#"{"identities": ["payer"], "apps": []}"#.to_vec();
        let response = transport()
            .send(post)
            .await
            .expect("the rig answers /rig/world");
        assert_eq!(response.status, 403, "{case}");
    }
}
