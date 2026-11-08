//! The world the flow tests run in: two identities, alice (the payer) and bob (the payee),
//! each with its own scripted switch and signer, Paykit apps joined from approvals, a fake
//! rail, and a bridge between the switches.
//!
//! The client's scripted switch serves an account's own apps; the bridge adds what a
//! counterparty's switch serves a sender (Switch wire 7.4.3, 7.4.5, 7.4.10): the directory
//! with `known_version`, the peer copy of the sender's `identity.json`, and the inbox, whose
//! appends it enters into the recipient's scripted switch for the apps whose receive keys
//! the envelope is sealed to. Answers are signed with the recipient's service key, as its
//! switch would.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use paykit_lib::bodies::RequestDestination;
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
};
use paykit_lib::{Label, PaymentReference};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{AppScopes, Paykit, PaykitConfig, RequestDraft, RequestRef, RequestedScopes};
use pubky_switch_client::clock::{Clock, TestClock};
use pubky_switch_client::error::TransportError;
use pubky_switch_client::store::{MemoryStore, StateStore};
use pubky_switch_client::testing::{
    AppSpec, ProtocolSpec, ScriptedSwitch, Seeds, TestHomeserver, TestIdentity, TestSigner,
    START_MS,
};
use pubky_switch_client::transport::{BoxFuture, Header, Request, Response, Transport};
use pubky_switch_core::enc::{b64, kid, pubky_key, unb64_array};
use pubky_switch_core::envelope::parse_header;
use pubky_switch_core::http::{header, request_signature_of, sign_response};
use pubky_switch_core::keys::IdentityKey;
use pubky_switch_core::time::rfc3339;
use serde_json::{json, Value};

pub const HOUR_MS: u64 = 3_600_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every scope Paykit gives keys to, for the signers' approvals.
pub fn spec() -> ProtocolSpec {
    ProtocolSpec {
        keyed_scopes: [
            "pay",
            "request",
            "allowance",
            "allowance_execute",
            "contacts",
            "manage_contacts",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect(),
        ..ProtocolSpec::default()
    }
}

/// An account and a sender whose `identity.json` it holds a copy of.
type PeerCopy = ([u8; 32], [u8; 32]);

/// A reachable app: its App ID, its receive key's kid, and its directory entry.
type Reachable = ([u8; 16], [u8; 8], Value);

/// One identity's account at the bridge.
struct Account {
    identity: [u8; 32],
    switch: Arc<ScriptedSwitch>,
    service: IdentityKey,
}

/// The switches of every identity, reached by the account their path names.
pub struct Bridge {
    clock: Arc<TestClock>,
    accounts: Mutex<Vec<Account>>,
    /// The peer copies each account holds: (account, sender) to version.
    known: Mutex<HashMap<PeerCopy, u64>>,
    /// Appends already answered, by body, so a retry gets the original answer.
    answered: Mutex<HashMap<Vec<u8>, Value>>,
    /// The apps each account ever made reachable, for its `apps_version`.
    reachable: Mutex<HashMap<[u8; 32], Vec<[u8; 16]>>>,
}

impl Bridge {
    pub fn new(clock: Arc<TestClock>) -> Self {
        Self {
            clock,
            accounts: Mutex::default(),
            known: Mutex::default(),
            answered: Mutex::default(),
            reachable: Mutex::default(),
        }
    }

    /// The account's reachable apps, each with its receive key's kid, its app record and its
    /// receive key record, and the `apps_version`: how many apps were ever reachable.
    fn reachable(&self, account: [u8; 32], switch: &ScriptedSwitch) -> (Vec<Reachable>, u64) {
        let mut apps = Vec::new();
        if let Some(doc) = switch.identity_doc() {
            for listed in &doc.body().apps {
                let held = switch.app(&listed.app_id);
                let (Some(record), Some(key)) = (held.record.clone(), held.receive_key.clone())
                else {
                    continue;
                };
                if held.departed {
                    continue;
                }
                let Ok(value) = key.peek::<Value>() else {
                    continue;
                };
                let Some(public) = value["x25519"]
                    .as_str()
                    .and_then(|p| unb64_array::<32>(p).ok())
                else {
                    continue;
                };
                apps.push((
                    listed.app_id,
                    kid(&public),
                    json!({ "app_id": pubky_switch_core::enc::zbase32_encode(&listed.app_id),
                            "app_record": record, "record": key }),
                ));
            }
        }
        let mut reachable = lock(&self.reachable);
        let seen = reachable.entry(account).or_default();
        for (app_id, _, _) in &apps {
            if !seen.contains(app_id) {
                seen.push(*app_id);
            }
        }
        (apps, u64::try_from(seen.len()).unwrap_or(0).max(1))
    }

    pub fn add(&self, identity: &TestIdentity, switch: &Arc<ScriptedSwitch>) {
        lock(&self.accounts).push(Account {
            identity: identity.public(),
            switch: Arc::clone(switch),
            service: IdentityKey::from_secret(identity.service_secret()),
        });
    }

    fn handle(&self, request: &Request) -> Result<Response, TransportError> {
        let mut parts = request.path_and_query.splitn(4, '/');
        let (_, _, account, rest) = (parts.next(), parts.next(), parts.next(), parts.next());
        let account = account
            .and_then(|a| pubky_key(a).ok())
            .ok_or_else(|| TransportError::Connect("no account in the path".into()))?;
        let route = rest
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
            .to_owned();
        let (switch, service, sender_switch) = {
            let accounts = lock(&self.accounts);
            let target = accounts
                .iter()
                .find(|a| a.identity == account)
                .ok_or_else(|| TransportError::Connect("no such account".into()))?;
            let sender = request
                .header(header::IDENTITY)
                .and_then(|text| pubky_key(text).ok());
            let sender_switch = sender
                .filter(|s| *s != account)
                .and_then(|s| accounts.iter().find(|a| a.identity == s))
                .map(|a| (a.identity, Arc::clone(&a.switch)));
            (
                Arc::clone(&target.switch),
                IdentityKey::from_secret(target.service.secret_bytes()),
                sender_switch,
            )
        };
        let Some((sender, sender_switch)) = sender_switch else {
            return switch.handle(request);
        };
        let known = lock(&self.known).get(&(account, sender)).copied();
        match (request.method.as_str(), route.as_str()) {
            ("GET", "directory") => {
                let answer = switch.handle(request)?;
                let mut body: Value = serde_json::from_slice(&answer.body)
                    .map_err(|e| TransportError::Other(e.to_string()))?;
                body["known_version"] = json!(known);
                body["apps_version"] = json!(self.reachable(account, &switch).1);
                Ok(respond(
                    &service,
                    request,
                    answer.status,
                    &body,
                    &rfc3339(self.clock.now_ms()),
                ))
            }
            ("PUT", "peers") => {
                let signed: pubky_switch_core::sig::SignedDoc =
                    serde_json::from_slice(&request.body)
                        .map_err(|e| TransportError::Other(e.to_string()))?;
                let doc = pubky_switch_core::records::IdentityDoc::verify(&signed, &sender)
                    .map_err(|e| TransportError::Other(e.to_string()))?;
                let version = doc.version().max(known.unwrap_or(0));
                lock(&self.known).insert((account, sender), version);
                let body = json!({ "version": version, "known_version": version });
                Ok(respond(
                    &service,
                    request,
                    200,
                    &body,
                    &rfc3339(self.clock.now_ms()),
                ))
            }
            ("POST", "inbox") => {
                if let Some(answer) = lock(&self.answered).get(&request.body).cloned() {
                    return Ok(respond(
                        &service,
                        request,
                        200,
                        &answer,
                        &rfc3339(self.clock.now_ms()),
                    ));
                }
                let app = request
                    .header(header::APP)
                    .and_then(|text| pubky_switch_core::enc::app_id_decode(text).ok())
                    .ok_or_else(|| TransportError::Other("no app".into()))?;
                let cert = sender_switch
                    .app(&app)
                    .cert
                    .ok_or_else(|| TransportError::Other("unknown sender app".into()))?;
                let parsed = parse_header(&request.body)
                    .map_err(|e| TransportError::Other(e.to_string()))?;
                let kids: Vec<[u8; 8]> = parsed.kids().collect();
                let sealed_for: u64 = request
                    .path_and_query
                    .split("apps_version=")
                    .nth(1)
                    .and_then(|v| v.split('&').next())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let (apps, apps_version) = self.reachable(account, &switch);
                // An app made reachable since the sender sealed: 409 stale_key, with its
                // records, so the sender seals again (Switch wire 7.4.3).
                let added: Vec<Value> = apps
                    .iter()
                    .filter(|(_, k, _)| !kids.contains(k))
                    .map(|(_, _, entry)| entry.clone())
                    .collect();
                if sealed_for < apps_version && !added.is_empty() {
                    let body = json!({ "code": "stale_key", "message": "bridge", "current": {
                        "receive_keys": [], "not_current": [], "unknown_kids": [],
                        "added_apps": added, "apps_version": apps_version } });
                    return Ok(respond(
                        &service,
                        request,
                        pubky_switch_core::http::ErrorCode::StaleKey.status(),
                        &body,
                        &rfc3339(self.clock.now_ms()),
                    ));
                }
                let targets: Vec<[u8; 16]> = apps
                    .iter()
                    .filter(|(_, k, _)| kids.contains(k))
                    .map(|(id, _, _)| *id)
                    .collect();
                let seq = switch.send_envelope(sender, &cert, request.body.clone(), &targets);
                let answer = json!({
                    "entry_id": b64(&[u8::try_from(seq % 256).unwrap_or(0); 16]),
                    "accepted_at": rfc3339(self.clock.now_ms()),
                    "directory_version": switch.seq(),
                    "known_version": known,
                });
                lock(&self.answered).insert(request.body.clone(), answer.clone());
                Ok(respond(
                    &service,
                    request,
                    200,
                    &answer,
                    &rfc3339(self.clock.now_ms()),
                ))
            }
            _ => switch.handle(request),
        }
    }
}

fn respond(
    key: &IdentityKey,
    request: &Request,
    status: u16,
    body: &Value,
    server_time: &str,
) -> Response {
    let bytes = body.to_string().into_bytes();
    let signature = sign_response(
        key,
        status,
        request_signature_of(request.header(header::SIGNATURE)).as_ref(),
        server_time,
        &bytes,
    );
    Response {
        status,
        headers: vec![
            Header::new(header::SERVICE_SIGNATURE, &b64(&signature)),
            Header::new(header::SERVER_TIME, server_time),
        ],
        body: bytes,
    }
}

impl Transport for Bridge {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        let answer = self.handle(&request);
        Box::pin(async move { answer })
    }
}

/// Alice, bob, their switches, the Homeserver, the clock and the rail.
pub struct World {
    pub clock: Arc<TestClock>,
    pub homeserver: Arc<TestHomeserver>,
    pub alice: TestSigner,
    pub bob: TestSigner,
    pub bridge: Arc<Bridge>,
    pub rail: Arc<FakeRail>,
}

impl World {
    pub fn new() -> Self {
        let seeds = Seeds::default();
        let clock = Arc::new(TestClock::new(START_MS));
        let homeserver = Arc::new(TestHomeserver::new());
        let alice = TestSigner::new(
            TestIdentity::new(&seeds, "alice"),
            Arc::clone(&clock),
            Arc::clone(&homeserver),
            spec(),
        );
        let bob = TestSigner::new(
            TestIdentity::new(&seeds, "bob"),
            Arc::clone(&clock),
            Arc::clone(&homeserver),
            spec(),
        );
        let bridge = Arc::new(Bridge::new(Arc::clone(&clock)));
        bridge.add(alice.identity(), alice.switch());
        bridge.add(bob.identity(), bob.switch());
        let rail = Arc::new(FakeRail::with_clock(Arc::clone(&clock) as Arc<dyn Clock>));
        Self {
            clock,
            homeserver,
            alice,
            bob,
            bridge,
            rail,
        }
    }

    pub fn alice_id(&self) -> [u8; 32] {
        self.alice.identity().public()
    }

    pub fn bob_id(&self) -> [u8; 32] {
        self.bob.identity().public()
    }

    /// The approval of app `name` of `signer` for `scopes`.
    pub fn approve(
        &self,
        signer: &TestSigner,
        name: &str,
        scopes: &RequestedScopes,
    ) -> pubky_switch_client::app::JoinRequest {
        let keys = signer.identity().app_keys(name);
        let app = AppSpec::new(&scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>())
            .with_permissions(&scopes.permissions);
        signer.approve(&keys, &app).unwrap()
    }

    /// The environment of an app of `signer` on `store`.
    pub fn env(
        &self,
        signer: &TestSigner,
        store: Arc<dyn StateStore>,
    ) -> pubky_switch_client::app::AppEnv {
        signer.env_with(store, Arc::clone(&self.bridge) as Arc<dyn Transport>)
    }

    /// Join app `name` of `signer` with `scopes` and `config`, on a store of its own, and
    /// finish the join.
    pub async fn join_with(
        &self,
        signer: &TestSigner,
        name: &str,
        scopes: RequestedScopes,
        config: PaykitConfig,
        store: Arc<dyn StateStore>,
    ) -> Paykit {
        let request = self.approve(signer, name, &scopes);
        let mut app = Paykit::join(
            self.env(signer, store),
            config,
            request,
            Arc::clone(&self.rail) as Arc<dyn paykit_sdk::Rail>,
        )
        .await
        .unwrap();
        app.receive(None).await.unwrap();
        app
    }

    pub async fn join(&self, signer: &TestSigner, name: &str, scopes: RequestedScopes) -> Paykit {
        self.join_with(signer, name, scopes, PaykitConfig::default(), store())
            .await
    }

    /// Alice's wallet, holding `pay`.
    pub async fn wallet(&self, name: &str) -> Paykit {
        self.join(&self.alice, name, AppScopes::new().pay().build().unwrap())
            .await
    }

    /// Bob's shop, holding `request`.
    pub async fn shop(&self) -> Paykit {
        self.join(
            &self.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await
    }

    /// Bob's labelled selling app, holding `request:<label>`.
    pub async fn labelled_shop(&self, name: &str, label: &str) -> Paykit {
        let label = Label::parse(label).unwrap();
        self.join(
            &self.bob,
            name,
            AppScopes::new().request_label(&label).build().unwrap(),
        )
        .await
    }
}

pub fn store() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new())
}

pub fn btc(value: &str) -> PaymentAmount {
    PaymentAmount {
        value: Decimal::new(value).unwrap(),
        asset: Asset::new("btc").unwrap(),
    }
}

pub fn endpoint(id: &str) -> PaymentEndpointIdentifier {
    PaymentEndpointIdentifier::new(id).unwrap()
}

/// A one-time request for `value` btc, paid to one invoice of the sending app, expiring a day
/// from now.
pub fn draft(value: &str, now: u64) -> RequestDraft {
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt11"),
        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    );
    RequestDraft {
        amount: btc(value),
        payment_reference: PaymentReference::new("order-1").unwrap(),
        proposal_expires_at_ms: Some(now + 24 * HOUR_MS),
        payment_deadline_ms: None,
        destination: RequestDestination::Endpoints(endpoints),
        accepted_endpoint_identifiers: None,
        metadata: None,
    }
}

/// Bob sends alice a request from `shop`; alice's `wallets` receive it. Returns the request.
pub async fn request_from(shop: &mut Paykit, alice: [u8; 32], draft: RequestDraft) -> RequestRef {
    let sent = shop.send_request(alice, draft).await.unwrap();
    sent.request.expect("the request is accepted at once")
}

impl World {
    /// A message of `kind` with `body` and `refs` from `signer`'s app `app`, entered into the
    /// switch of `to` for its apps `targets`, as if its inbox had accepted it. Returns the
    /// `msg_id`.
    // A test fixture naming the sender, the recipient and the message in full.
    #[allow(clippy::too_many_arguments)]
    pub fn inject(
        &self,
        signer: &TestSigner,
        app: &str,
        to: &TestSigner,
        targets: &[[u8; 16]],
        kind: &str,
        refs: Vec<[u8; 32]>,
        body: serde_json::Map<String, Value>,
    ) -> String {
        let keys = signer.identity().app_keys(app);
        let cert = signer.switch().app(&keys.app_id_bytes()).cert.unwrap();
        let message = pubky_switch_core::message::Message::new(
            kind,
            pubky_switch_core::message::Sender::of_app(signer.identity().public(), &keys),
            to.identity().public(),
            self.clock.now_ms(),
            refs,
            body,
        )
        .unwrap();
        let plaintext = message.sign(&keys).unwrap();
        to.switch()
            .send_plaintext(signer.identity().public(), &cert, &plaintext, targets)
            .unwrap();
        message.msg_id
    }

    /// Bob's app `app` sends alice's apps `targets` a Private Payment List of `version`,
    /// with a single-use on-chain endpoint and a reusable Lightning offer, for 30 days.
    pub fn list_to_alice(&self, app: &str, targets: &[[u8; 16]], version: u64) -> String {
        let mut endpoints = std::collections::BTreeMap::new();
        endpoints.insert(
            endpoint("btc-bitcoin-p2wpkh"),
            paykit_lib::bodies::ListEndpoint {
                payload: PaymentEndpointPayload::new(&format!("bc1qsingle{version}")).unwrap(),
                single_use: true,
            },
        );
        endpoints.insert(
            endpoint("btc-lightning-bolt12"),
            paykit_lib::bodies::ListEndpoint {
                payload: PaymentEndpointPayload::new("lno1reusable").unwrap(),
                single_use: false,
            },
        );
        let body = paykit_lib::bodies::PrivatePaymentList {
            version,
            payment_endpoints: endpoints,
            expires_at: self.clock.now_ms() + 30 * 24 * HOUR_MS,
        }
        .to_body();
        self.inject(
            &self.bob,
            app,
            &self.alice,
            targets,
            "paykit.private_payment_list",
            Vec::new(),
            body,
        )
    }
}
