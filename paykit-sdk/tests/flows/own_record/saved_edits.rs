//! An own-record edit the client saves before it reads the service time to sign it (Dispatch
//! spec 4.2): when that read fails, the edit stays saved and unsigned for a later read of the
//! same run. A later run or a restore holds it aside, and Paykit never signs it: it tells the
//! host which endpoints it named (Paykit spec 4, "Changes saved and not yet signed").

use std::sync::{Arc, Mutex};

use paykit_lib::app_record::PaykitMember;
use paykit_sdk::{AppScopes, Notice, NoticeKind, Paykit, PaykitConfig, Rail};
use pubky_dispatch_client::app::AppEvent;
use pubky_dispatch_client::store::StateStore;
use pubky_dispatch_client::testing::Fault;
use pubky_dispatch_client::transport::{BoxFuture, Request, Response, Transport};
use pubky_dispatch_client::TransportError;
use serde_json::Value;

use super::{payload, publishing};
use crate::common::{endpoint, store, World};

/// Every request body sent.
struct Recorded {
    inner: Arc<dyn Transport>,
    bodies: Mutex<Vec<Vec<u8>>>,
}

impl Recorded {
    fn over(inner: Arc<dyn Transport>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            bodies: Mutex::new(Vec::new()),
        })
    }

    /// How many requests were sent so far.
    fn requests(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    /// Whether a request body sent so far holds `needle`, in itself or in a base64 member.
    fn sent(&self, needle: &str) -> bool {
        self.bodies.lock().unwrap().iter().any(|body| {
            serde_json::from_slice::<Value>(body).is_ok_and(|value| mentions(&value, needle))
        })
    }
}

impl Transport for Recorded {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        self.bodies.lock().unwrap().push(request.body.clone());
        self.inner.send(request)
    }
}

/// Whether `value`, or a base64 member of it read as JSON or text, holds `needle`.
fn mentions(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => {
            text.contains(needle)
                || pubky_dispatch_core::enc::unb64(text).is_ok_and(|bytes| {
                    String::from_utf8_lossy(&bytes).contains(needle)
                        || serde_json::from_slice::<Value>(&bytes)
                            .is_ok_and(|inner| mentions(&inner, needle))
                })
        }
        Value::Array(items) => items.iter().any(|item| mentions(item, needle)),
        Value::Object(members) => members.values().any(|member| mentions(member, needle)),
        _ => false,
    }
}

/// The next time read of `signer`'s Dispatch fails, once a fresh sample is too old.
fn time_read_fails(w: &World, signer: &pubky_dispatch_client::testing::TestSigner) {
    w.clock.advance_ms(11 * 60 * 1000);
    signer.dispatch().fail_next(Fault::Unreachable {
        path: "/feed".into(),
        after: 0,
    });
}

/// `shop` offers its endpoint while the service time can't be read: the offer is saved,
/// unsigned.
async fn save_offer_unsigned(w: &World, shop: &mut Paykit) {
    time_read_fails(w, &w.bob);
    assert!(shop
        .set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .is_err());
    assert!(
        shop.app_mut()
            .protocol_record("paykit")
            .await
            .unwrap()
            .pending
    );
    assert!(shop.own_public_endpoints().unwrap().is_empty());
}

/// Bob's shop holding `paykit_request`, and `paykit_pay` with `pay`, publishing its
/// record, on `store`.
async fn shop_on(w: &World, pay: bool, store: Arc<dyn StateStore>) -> Paykit {
    let scopes = if pay {
        AppScopes::new().request().pay().build().unwrap()
    } else {
        AppScopes::new().request().build().unwrap()
    };
    w.join_with(&w.bob, "shop", scopes, publishing(), store)
        .await
}

/// Load `app_id` of bob's from `store` with `config`, sending through `recorded`.
async fn load_recorded(
    w: &World,
    store: Arc<dyn StateStore>,
    app_id: [u8; 16],
    config: PaykitConfig,
    recorded: &Arc<Recorded>,
) -> Paykit {
    let mut env = w.env(&w.bob, store);
    env.transport = Arc::clone(recorded) as Arc<dyn Transport>;
    Paykit::load(env, config, app_id, w.rail.clone(), w.timer())
        .await
        .unwrap()
}

#[tokio::test]
async fn test_own_record_saved_offer_never_signed_once_publishing_stops() {
    // An offer saved while the service time can't be read stays unsigned. The app is loaded
    // again without publishing its record: the saved offer never reaches the Dispatch, and the
    // app holds no public endpoints (Paykit spec 4).
    let w = World::new();
    let kept = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            publishing(),
            Arc::clone(&kept),
        )
        .await;
    save_offer_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut env = w.env(&w.bob, kept);
    env.transport = Arc::clone(&recorded) as Arc<dyn Transport>;
    let mut shop = Paykit::load(
        env,
        PaykitConfig::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    assert!(
        !shop
            .app_mut()
            .protocol_record("paykit")
            .await
            .unwrap()
            .pending
    );
    assert!(!recorded.sent("lno1tips"));
}

#[tokio::test]
async fn test_own_record_restored_saved_offer_never_signed_over_a_later_withdrawal() {
    // The backup carries the offer saved during the outage. The user then withdraws the
    // endpoint, and later restores that backup onto a new store: the restore never signs the
    // saved offer over the withdrawal, so payers never see the withdrawn destination again
    // (Paykit spec 4).
    let w = World::new();
    let mut shop = shop_on(&w, false, store()).await;
    save_offer_unsigned(&w, &mut shop).await;
    let backup = shop.backup().await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(!shop.own_public_endpoints().unwrap().is_empty());
    shop.remove_public_endpoint(&endpoint("btc-lightning-bolt12"))
        .await
        .unwrap();
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    drop(shop);
    // Everything the restored app sends, through the restore and its first read.
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut env = w.env(&w.bob, store());
    env.transport = Arc::clone(&recorded) as Arc<dyn Transport>;
    let mut restored = Paykit::restore(env, publishing(), &backup, w.rail.clone(), w.timer())
        .await
        .unwrap();
    restored.receive(None).await.unwrap();
    let published = PaykitMember::read(&super::on_homeserver(&w, &restored).unwrap()).unwrap();
    assert!(published.public_endpoints.is_empty());
    assert!(!recorded.sent("lno1tips"));
    assert!(told_not_published(&restored).await);
}

#[tokio::test]
async fn test_own_record_saved_offer_never_signed_when_the_record_cant_be_tended() {
    // Loaded again without publishing and with capabilities the record can't carry: the
    // upkeep can't replace the saved offer, and the read still never signs it.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, true, Arc::clone(&kept)).await;
    save_offer_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    w.rail
        .set_endpoint_identifiers(super::oversized_capabilities());
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, PaykitConfig::default(), &recorded).await;
    shop.receive(None).await.unwrap();
    assert!(!recorded.sent("lno1tips"));
}

#[tokio::test]
async fn test_own_record_saved_capabilities_never_signed_once_the_method_drops_them() {
    // The app's upkeep saves capabilities while the service time can't be read. Loaded again
    // after the payment method stopped paying one of them, the app never signs it.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, true, Arc::clone(&kept)).await;
    let mut identifiers = w.rail.endpoint_identifiers();
    identifiers.push(endpoint("btc-dropped-method"));
    w.rail.set_endpoint_identifiers(identifiers.clone());
    time_read_fails(&w, &w.bob);
    assert!(shop.update_own_record().await.is_err());
    assert!(
        shop.app_mut()
            .protocol_record("paykit")
            .await
            .unwrap()
            .pending
    );
    let app_id = shop.app_id();
    drop(shop);
    identifiers.retain(|id| *id != endpoint("btc-dropped-method"));
    w.rail.set_endpoint_identifiers(identifiers);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    shop.receive(None).await.unwrap();
    assert!(!recorded.sent("btc-dropped-method"));
    // A change of capabilities no call of the user's asked is told to no one.
    assert!(told(&shop).await.is_none());
}

/// Whether `shop` tells its host that an endpoint change saved earlier wasn't published.
async fn told_not_published(shop: &Paykit) -> bool {
    shop.notices()
        .await
        .unwrap()
        .iter()
        .any(|notice| notice.kind == NoticeKind::PublicEndpointChangeNotPublished)
}

/// Whether `shop` still holds a change of Paykit's member aside, or owes Paykit the duty
/// that tells of one.
async fn still_held(shop: &mut Paykit) -> bool {
    shop.app_mut()
        .held_record_edit("paykit")
        .await
        .unwrap()
        .is_some()
        || shop
            .app_mut()
            .pending_duty_events()
            .await
            .unwrap()
            .iter()
            .any(|duty| {
                matches!(duty.event, AppEvent::RecordEditsHeld { .. })
                    && duty.owners.iter().any(|owner| owner == "paykit")
            })
}

/// `shop` withdraws its offered endpoint while the service time can't be read: the
/// withdrawal is saved, unsigned, and the offer is still the signed one.
async fn save_withdrawal_unsigned(w: &World, shop: &mut Paykit) {
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    time_read_fails(w, &w.bob);
    assert!(shop
        .remove_public_endpoint(&endpoint("btc-lightning-bolt12"))
        .await
        .is_err());
    assert!(
        shop.app_mut()
            .protocol_record("paykit")
            .await
            .unwrap()
            .pending
    );
    assert!(!shop.own_public_endpoints().unwrap().is_empty());
}

fn published_endpoints(w: &World, shop: &Paykit) -> usize {
    PaykitMember::read(&super::on_homeserver(w, shop).unwrap())
        .unwrap()
        .public_endpoints
        .len()
}

/// The endpoint change notice `shop` shows, if any.
async fn told(shop: &Paykit) -> Option<Notice> {
    shop.notices()
        .await
        .unwrap()
        .into_iter()
        .find(|notice| notice.kind == NoticeKind::PublicEndpointChangeNotPublished)
}

fn ids(list: &[paykit_lib::PaymentEndpointIdentifier]) -> Vec<String> {
    list.iter().map(|id| id.as_str().to_owned()).collect()
}

/// What a host does on the notice: withdraw again the endpoints it names to withdraw, after a
/// read, and acknowledge it.
async fn withdraw_again(shop: &mut Paykit) {
    let notice = told(shop).await.expect("an endpoint change notice");
    for id in &notice.endpoints_withdrawn {
        shop.remove_public_endpoint(id).await.unwrap();
    }
    shop.acknowledge_notice(&notice.id).await.unwrap();
}

#[tokio::test]
async fn test_own_record_withdrawal_saved_during_an_outage_is_told_after_a_restart_and_withdrawn_again(
) {
    // A withdrawal saved while the service time can't be read, then a restart: the load holds
    // it aside, and Paykit never signs it. The host is told the endpoint to withdraw, and
    // withdraws it again.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_withdrawal_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    shop.receive(None).await.unwrap();
    assert_eq!(published_endpoints(&w, &shop), 1);
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    assert!(notice.endpoints_offered.is_empty() && notice.endpoints_changed.is_empty());
    assert!(!still_held(&mut shop).await);
    withdraw_again(&mut shop).await;
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    assert_eq!(published_endpoints(&w, &shop), 0);
}

#[tokio::test]
async fn test_own_record_withdrawal_saved_during_an_outage_is_told_after_a_restore() {
    // The backup carries a withdrawal saved during the outage: the restore holds it aside,
    // and once its first read is complete the host is told the endpoint to withdraw again.
    let w = World::new();
    let mut shop = shop_on(&w, false, store()).await;
    save_withdrawal_unsigned(&w, &mut shop).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    let notice = told(&restored).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    assert!(notice.endpoints_offered.is_empty() && notice.endpoints_changed.is_empty());
    assert!(!still_held(&mut restored).await);
    withdraw_again(&mut restored).await;
    assert_eq!(published_endpoints(&w, &restored), 0);
}

#[tokio::test]
async fn test_own_record_offer_saved_during_an_outage_is_not_published_after_a_restart_and_is_told()
{
    // An offer saved during the outage, then a restart: the load holds it aside and Paykit
    // never signs it, and tells the host, which keeps knowing through a restore until it
    // acknowledges it.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_offer_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    shop.receive(None).await.unwrap();
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    assert!(!recorded.sent("lno1tips"));
    assert!(told_not_published(&shop).await);
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_offered),
        vec![a().as_str().to_owned()]
    );
    assert!(notice.endpoints_withdrawn.is_empty() && notice.endpoints_changed.is_empty());
    assert!(!still_held(&mut shop).await);

    let backup = shop.backup().await.unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert!(told_not_published(&restored).await);
    let notice = restored
        .notices()
        .await
        .unwrap()
        .into_iter()
        .find(|notice| notice.kind == NoticeKind::PublicEndpointChangeNotPublished)
        .unwrap();
    restored.acknowledge_notice(&notice.id).await.unwrap();
    restored.receive(None).await.unwrap();
    assert!(!told_not_published(&restored).await);
}

#[tokio::test]
async fn test_own_record_endpoint_change_notice_survives_an_in_place_restore_of_an_older_backup() {
    // The installation tells the host an offer saved during an outage wasn't published. A
    // restore in place of a backup taken before it keeps the installation's notice.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    let older = shop.backup().await.unwrap();
    save_offer_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, Arc::clone(&kept), app_id, publishing(), &recorded).await;
    shop.receive(None).await.unwrap();
    assert!(told_not_published(&shop).await);
    drop(shop);
    let restored = Paykit::restore(
        w.env(&w.bob, kept),
        publishing(),
        &older,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert!(told_not_published(&restored).await);
}

/// Bob's publishing shop on `kept`, and a second device of it restored from its backup,
/// each having read once.
async fn shop_on_two_devices(w: &World, kept: Arc<dyn StateStore>) -> (Paykit, Paykit) {
    let mut first = shop_on(w, false, kept).await;
    first.receive(None).await.unwrap();
    let backup = first.backup().await.unwrap();
    let mut second = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    second.receive(None).await.unwrap();
    (first, second)
}

fn a() -> paykit_lib::PaymentEndpointIdentifier {
    endpoint("btc-lightning-bolt12")
}

fn b() -> paykit_lib::PaymentEndpointIdentifier {
    endpoint("btc-onchain-address")
}

/// The endpoints `shop` signs, with their payloads.
fn signed_endpoints(shop: &Paykit) -> Vec<(String, String)> {
    shop.own_public_endpoints()
        .unwrap()
        .into_iter()
        .map(|(id, payload)| (id.as_str().to_owned(), payload.as_str().to_owned()))
        .collect()
}

/// `first` offers A and B, which `second` reads, then withdraws A while the service time
/// can't be read: the withdrawal is saved, unsigned, and both devices still sign A.
async fn offer_both_then_withdraw_a_unsigned(w: &World, first: &mut Paykit, second: &mut Paykit) {
    first
        .set_public_endpoints(
            std::collections::BTreeMap::from([
                (a(), payload("lno1tips")),
                (b(), payload("bc1qold")),
            ]),
            None,
        )
        .await
        .unwrap();
    second.receive(None).await.unwrap();
    assert_eq!(second.own_public_endpoints().unwrap().len(), 2);
    time_read_fails(w, &w.bob);
    assert!(first.remove_public_endpoint(&a()).await.is_err());
    assert!(first.own_public_endpoints().unwrap().contains_key(&a()));
}

#[tokio::test]
async fn test_own_record_reload_signs_nothing_and_tells_a_withdrawal_over_another_devices_change() {
    // A is withdrawn during an outage; another device of the app then changes B. The reload
    // sends nothing and signs nothing; the first receive keeps the newer B and tells the host
    // to withdraw A again, which it does under the record it read.
    let w = World::new();
    let kept = store();
    let (mut first, mut second) = shop_on_two_devices(&w, Arc::clone(&kept)).await;
    offer_both_then_withdraw_a_unsigned(&w, &mut first, &mut second).await;
    let app_id = first.app_id();
    drop(first);
    second
        .set_public_endpoint(b(), payload("bc1qnew"))
        .await
        .unwrap();
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut first = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    assert_eq!(recorded.requests(), 0);
    first.receive(None).await.unwrap();
    assert!(!recorded.sent("bc1qold"));
    let notice = told(&first).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    assert!(notice.endpoints_offered.is_empty() && notice.endpoints_changed.is_empty());
    withdraw_again(&mut first).await;
    let want = vec![(b().as_str().to_owned(), "bc1qnew".to_owned())];
    assert_eq!(signed_endpoints(&first), want);
    assert_eq!(published_endpoints(&w, &first), 1);
    assert!(!recorded.sent("bc1qold"));
}

#[tokio::test]
async fn test_own_record_restore_tells_a_withdrawal_over_another_devices_change() {
    // The backup carries A's withdrawal saved during an outage; another device then changes
    // B. Restored, the newer B stays and the host is told to withdraw A again.
    let w = World::new();
    let (mut first, mut second) = shop_on_two_devices(&w, store()).await;
    offer_both_then_withdraw_a_unsigned(&w, &mut first, &mut second).await;
    let backup = first.backup().await.unwrap();
    drop(first);
    second
        .set_public_endpoint(b(), payload("bc1qnew"))
        .await
        .unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    let notice = told(&restored).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    assert!(notice.endpoints_offered.is_empty() && notice.endpoints_changed.is_empty());
    withdraw_again(&mut restored).await;
    let want = vec![(b().as_str().to_owned(), "bc1qnew".to_owned())];
    assert_eq!(signed_endpoints(&restored), want);
}

#[tokio::test]
async fn test_own_record_held_change_without_its_record_is_told_without_endpoints() {
    // A held change whose record of what the user asked is gone is still told, naming no
    // endpoint: the host shows the endpoints as they are.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_offer_unsigned(&w, &mut shop).await;
    shop.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_dispatch_client::app::LocalWrite::Delete {
                key: b"ec".to_vec(),
            }],
        )
        .await
        .unwrap();
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    shop.receive(None).await.unwrap();
    let notice = told(&shop).await.unwrap();
    assert!(
        notice.endpoints_withdrawn.is_empty()
            && notice.endpoints_offered.is_empty()
            && notice.endpoints_changed.is_empty()
    );
    // Acknowledging it twice is no error.
    shop.acknowledge_notice(&notice.id).await.unwrap();
    shop.acknowledge_notice(&notice.id).await.unwrap();
}

#[tokio::test]
async fn test_own_record_restored_capability_change_never_erases_a_later_offer() {
    // A change of capabilities saved during an outage, with no endpoints offered, is carried
    // by the backup. The app then offers an endpoint. Restoring that backup in place keeps
    // the offer: a held change is never signed.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, true, Arc::clone(&kept)).await;
    let mut identifiers = w.rail.endpoint_identifiers();
    identifiers.push(endpoint("btc-another-method"));
    w.rail.set_endpoint_identifiers(identifiers);
    time_read_fails(&w, &w.bob);
    assert!(shop.update_own_record().await.is_err());
    let backup = shop.backup().await.unwrap();
    shop.receive(None).await.unwrap();
    shop.set_public_endpoint(a(), payload("lno1tips"))
        .await
        .unwrap();
    drop(shop);
    let mut restored = Paykit::restore(
        w.env(&w.bob, kept),
        publishing(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(
        signed_endpoints(&restored),
        vec![(a().as_str().to_owned(), "lno1tips".to_owned())]
    );
    assert_eq!(published_endpoints(&w, &restored), 1);
    assert!(told(&restored).await.is_none());
}

#[tokio::test]
async fn test_own_record_payload_change_saved_during_an_outage_is_told_as_a_payload_change() {
    // A new payload for an endpoint already signed, saved during an outage, then a restart:
    // never signed, and told as a payload change, which the host makes again only when the
    // user asks. The old payload stays signed meanwhile.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    shop.set_public_endpoint(a(), payload("lno1tips"))
        .await
        .unwrap();
    time_read_fails(&w, &w.bob);
    assert!(shop
        .set_public_endpoint(a(), payload("lno1new"))
        .await
        .is_err());
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    shop.receive(None).await.unwrap();
    assert!(!recorded.sent("lno1new"));
    assert_eq!(
        signed_endpoints(&shop),
        vec![(a().as_str().to_owned(), "lno1tips".to_owned())]
    );
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_changed),
        vec![a().as_str().to_owned()]
    );
    assert!(notice.endpoints_withdrawn.is_empty() && notice.endpoints_offered.is_empty());
}

/// The digest the SDK keeps of a set of public endpoints: each identifier and payload
/// length-prefixed, in identifier order.
fn digest(endpoints: &[(&str, &str)]) -> [u8; 32] {
    let mut sorted = endpoints.to_vec();
    sorted.sort();
    let mut bytes = Vec::new();
    for (id, payload) in sorted {
        for part in [id, payload] {
            bytes.extend_from_slice(&(part.len() as u64).to_be_bytes());
            bytes.extend_from_slice(part.as_bytes());
        }
    }
    pubky_dispatch_core::enc::hash(&bytes)
}

/// Keep in `shop`'s store an ask of the public endpoints with `endpoints` and `base`
/// digests, withdrawing `withdrawn`, as one a crash left behind.
async fn keep_ask(shop: &mut Paykit, endpoints: [u8; 32], base: [u8; 32], withdrawn: &[&str]) {
    // The SDK's local record: its version, then the value.
    let value = serde_json::json!({ "v": 1, "d": {
        "endpoints": pubky_dispatch_core::enc::b64(&endpoints),
        "base": pubky_dispatch_core::enc::b64(&base),
        "classes": { "withdrawn": withdrawn, "offered": [], "changed": [] },
    }});
    shop.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_dispatch_client::app::LocalWrite::Put {
                key: b"ec".to_vec(),
                value: serde_json::to_vec(&value).unwrap(),
            }],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_own_record_load_tells_a_held_change_before_any_call() {
    // The open tells the held withdrawal and clears it, before any read: a call made right
    // after the load, which signs, can't write over what the user asked, and the host still
    // gets the endpoint to withdraw.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_withdrawal_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    assert_eq!(recorded.requests(), 0);
    assert!(shop
        .app_mut()
        .held_record_edit("paykit")
        .await
        .unwrap()
        .is_none());
    shop.set_public_endpoint(b(), payload("bc1qnew"))
        .await
        .unwrap();
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    assert!(notice.endpoints_offered.is_empty() && notice.endpoints_changed.is_empty());
    // The withdrawal was never signed: the call signed the offer over the record held.
    assert!(signed_endpoints(&shop).contains(&(a().as_str().to_owned(), "lno1tips".to_owned())));
    withdraw_again(&mut shop).await;
    assert_eq!(
        signed_endpoints(&shop),
        vec![(b().as_str().to_owned(), "bc1qnew".to_owned())]
    );
}

#[tokio::test]
async fn test_own_record_restore_tells_a_held_change_at_the_open() {
    // A restore holds the backup's saved offer aside, and the open tells it before any read.
    let w = World::new();
    let mut shop = shop_on(&w, false, store()).await;
    save_offer_unsigned(&w, &mut shop).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let restored = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    let notice = told(&restored).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_offered),
        vec!["btc-lightning-bolt12".to_owned()]
    );
    assert!(notice.endpoints_withdrawn.is_empty() && notice.endpoints_changed.is_empty());
}

#[tokio::test]
async fn test_own_record_open_whose_telling_fails_shuts_every_signing_until_it_is_told() {
    // The notice can't be written at the load: the app opens all the same, the change stays
    // held, and every call that would sign is refused, with the store's failure itself,
    // until a later try tells it. A receive whose try fails so ends on that failure.
    use pubky_dispatch_client::store::{Fault, FaultStore};
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_withdrawal_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let faulty = Arc::new(FaultStore::new(kept, Fault::None));
    let notices = b"p/paykit/nt/endpoint_change_not_published/";
    faulty.fail_put_once(notices);
    let mut shop = Paykit::load(
        w.env(&w.bob, Arc::clone(&faulty) as Arc<dyn StateStore>),
        publishing(),
        app_id,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert!(told(&shop).await.is_none());
    assert!(shop
        .app_mut()
        .held_record_edit("paykit")
        .await
        .unwrap()
        .is_some());
    let untold = |error: paykit_sdk::PaykitError| {
        matches!(
            error,
            paykit_sdk::PaykitError::Client(pubky_dispatch_client::app::AppError::Store(
                pubky_dispatch_client::StoreError::Unavailable
            ))
        )
    };
    faulty.fail_put_once(notices);
    assert!(untold(
        shop.set_public_endpoint(b(), payload("bc1qnew"))
            .await
            .unwrap_err()
    ));
    faulty.fail_put_once(notices);
    assert!(untold(shop.update_own_record().await.unwrap_err()));
    assert_eq!(
        signed_endpoints(&shop),
        vec![(a().as_str().to_owned(), "lno1tips".to_owned())]
    );
    faulty.fail_put_once(notices);
    assert!(untold(shop.receive(None).await.unwrap_err()));
    // The next receive tells it, and signing goes on.
    shop.receive(None).await.unwrap();
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    assert!(!still_held(&mut shop).await);
    withdraw_again(&mut shop).await;
    assert!(shop.own_public_endpoints().unwrap().is_empty());
}

#[tokio::test]
async fn test_own_record_held_change_with_another_ask_on_the_same_record_is_told_by_its_endpoints()
{
    // The ask kept is a later one's, made on the record signed now, as a crash between it and
    // its save leaves: the held offer is told by its own endpoints against that record.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_offer_unsigned(&w, &mut shop).await;
    keep_ask(&mut shop, [9; 32], digest(&[]), &["btc-other"]).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_offered),
        vec!["btc-lightning-bolt12".to_owned()]
    );
    assert!(notice.endpoints_withdrawn.is_empty() && notice.endpoints_changed.is_empty());
}

#[tokio::test]
async fn test_own_record_held_change_with_an_ask_on_another_record_names_no_endpoint() {
    // The ask kept was made on a record other than the one signed now: the held change's
    // classes against the record signed now could be wrong, so it is told naming no endpoint.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_offer_unsigned(&w, &mut shop).await;
    keep_ask(&mut shop, [9; 32], [8; 32], &["btc-other"]).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    let notice = told(&shop).await.unwrap();
    assert!(
        notice.endpoints_withdrawn.is_empty()
            && notice.endpoints_offered.is_empty()
            && notice.endpoints_changed.is_empty()
    );
}

#[tokio::test]
async fn test_own_record_ask_with_no_held_change_is_told_as_asked() {
    // An ask neither signed nor held, as one whose held change a restore left behind as
    // unreadable, is told as asked at the next open; one the record signed already has is
    // told to no one.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    shop.set_public_endpoint(a(), payload("lno1tips"))
        .await
        .unwrap();
    let signed = digest(&[(a().as_str(), "lno1tips")]);
    keep_ask(&mut shop, signed, digest(&[]), &[b().as_str()]).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, Arc::clone(&kept), app_id, publishing(), &recorded).await;
    assert!(told(&shop).await.is_none());
    keep_ask(&mut shop, digest(&[]), signed, &[a().as_str()]).await;
    drop(shop);
    let shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
}

#[tokio::test]
async fn test_own_record_each_held_change_is_told_and_acknowledged_apart() {
    // Two held changes, told at two loads: each is a notice of its own, and acknowledging the
    // first leaves the second.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_withdrawal_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, Arc::clone(&kept), app_id, publishing(), &recorded).await;
    let first = told(&shop).await.unwrap();
    shop.receive(None).await.unwrap();
    time_read_fails(&w, &w.bob);
    assert!(shop
        .set_public_endpoint(b(), payload("bc1qnew"))
        .await
        .is_err());
    drop(shop);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    let notices: Vec<Notice> = shop
        .notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|notice| notice.kind == NoticeKind::PublicEndpointChangeNotPublished)
        .collect();
    assert_eq!(notices.len(), 2);
    let second = notices.iter().find(|notice| notice.id != first.id).unwrap();
    assert_eq!(
        ids(&second.endpoints_offered),
        vec![b().as_str().to_owned()]
    );
    shop.acknowledge_notice(&first.id).await.unwrap();
    let left: Vec<String> = shop
        .notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|notice| notice.kind == NoticeKind::PublicEndpointChangeNotPublished)
        .map(|notice| notice.id)
        .collect();
    assert_eq!(left, vec![second.id.clone()]);
}

/// Load `app_id` of bob's shop from `faulty`, whose puts of an endpoint change notice are
/// all refused: the open can't tell the held change, so it stays held and untold.
async fn load_untold(
    w: &World,
    faulty: &Arc<pubky_dispatch_client::store::FaultStore>,
    app_id: [u8; 16],
) -> Paykit {
    load_untold_with(w, faulty, app_id, publishing()).await
}

/// [`load_untold`] with `config`.
async fn load_untold_with(
    w: &World,
    faulty: &Arc<pubky_dispatch_client::store::FaultStore>,
    app_id: [u8; 16],
    config: PaykitConfig,
) -> Paykit {
    faulty.refuse_puts_as_too_large(Some(b"p/paykit/nt/endpoint_change_not_published/"));
    let mut shop = Paykit::load(
        w.env(&w.bob, Arc::clone(faulty) as Arc<dyn StateStore>),
        config,
        app_id,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert!(told(&shop).await.is_none());
    assert!(shop
        .app_mut()
        .held_record_edit("paykit")
        .await
        .unwrap()
        .is_some());
    shop
}

/// Bob's signer renews `shop`'s certificate for the scopes it holds; the shop takes the
/// renewal at a receive, then the `identity.json` that lists it at the next.
async fn renew_and_adopt(w: &World, shop: &mut Paykit) -> pubky_dispatch_core::records::AppCert {
    let old = shop.app().cert().clone();
    let scopes = AppScopes::new().request().build().unwrap();
    let renewed = crate::certificate::renew_for(w, &w.bob, "shop", shop.app_id(), &scopes, 1_000);
    // A receive takes the renewal at its read; while the held change can't be told, its
    // upkeep then ends on the store's failure.
    let _ = shop.receive(None).await;
    crate::certificate::list_for(&w.bob, shop.app_id(), &old, &renewed);
    let _ = shop.receive(None).await;
    assert_eq!(shop.app().cert().hash(), renewed.hash());
    renewed
}

#[tokio::test]
async fn test_own_record_renewal_while_a_withdrawal_is_untold_signs_the_signed_member() {
    // The renewal callback is the one signing outside the gate (`PaykitProtocol::member_for`):
    // while a held withdrawal is untold, a renewal and the adoption of the certificate sign
    // the member as signed, the withdrawn endpoint still in it. The held change has no effect,
    // stays held, and is told once it can be.
    use pubky_dispatch_client::store::{Fault, FaultStore};
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_withdrawal_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let faulty = Arc::new(FaultStore::new(kept, Fault::None));
    let mut shop = load_untold(&w, &faulty, app_id).await;
    let signed = signed_endpoints(&shop);
    assert_eq!(
        signed,
        vec![(a().as_str().to_owned(), "lno1tips".to_owned())]
    );
    let renewed = renew_and_adopt(&w, &mut shop).await;
    let record = PaykitMember::read(&super::on_homeserver(&w, &shop).unwrap()).unwrap();
    assert_eq!(signed_endpoints(&shop), signed);
    assert_eq!(record.public_endpoints.len(), 1);
    assert_eq!(
        pubky_dispatch_core::records::AppRecord::verify(
            shop.app().record(),
            &w.bob.identity().public()
        )
        .unwrap()
        .cert()
        .hash(),
        renewed.hash()
    );
    assert!(still_held(&mut shop).await);
    // Told once the notice can be written; the host withdraws again.
    faulty.refuse_puts_as_too_large(None);
    shop.receive(None).await.unwrap();
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    withdraw_again(&mut shop).await;
    assert!(shop.own_public_endpoints().unwrap().is_empty());
}

#[tokio::test]
async fn test_own_record_renewal_while_an_offer_is_untold_signs_the_signed_member() {
    // As above for a held offer: the renewal and the adoption sign the member as signed, the
    // offered endpoint still absent from it.
    use pubky_dispatch_client::store::{Fault, FaultStore};
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    shop.set_public_endpoint(a(), payload("lno1tips"))
        .await
        .unwrap();
    time_read_fails(&w, &w.bob);
    assert!(shop
        .set_public_endpoint(b(), payload("bc1qnew"))
        .await
        .is_err());
    let app_id = shop.app_id();
    drop(shop);
    let faulty = Arc::new(FaultStore::new(kept, Fault::None));
    let mut shop = load_untold(&w, &faulty, app_id).await;
    let signed = signed_endpoints(&shop);
    assert_eq!(
        signed,
        vec![(a().as_str().to_owned(), "lno1tips".to_owned())]
    );
    renew_and_adopt(&w, &mut shop).await;
    assert_eq!(signed_endpoints(&shop), signed);
    assert_eq!(published_endpoints(&w, &shop), 1);
    assert!(still_held(&mut shop).await);
    faulty.refuse_puts_as_too_large(None);
    shop.receive(None).await.unwrap();
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_offered),
        vec![b().as_str().to_owned()]
    );
}

#[tokio::test]
async fn test_own_record_renewal_after_publishing_stops_signs_no_endpoint() {
    // The renewal callback builds the member for what the app does now: loaded without
    // publishing its record, a renewal taken while a held change is untold signs no public
    // endpoint, as every other signing would.
    use pubky_dispatch_client::store::{Fault, FaultStore};
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    shop.set_public_endpoint(a(), payload("lno1tips"))
        .await
        .unwrap();
    time_read_fails(&w, &w.bob);
    assert!(shop
        .set_public_endpoint(b(), payload("bc1qnew"))
        .await
        .is_err());
    let app_id = shop.app_id();
    drop(shop);
    let faulty = Arc::new(FaultStore::new(kept, Fault::None));
    let mut shop = load_untold_with(&w, &faulty, app_id, PaykitConfig::default()).await;
    assert!(!signed_endpoints(&shop).is_empty());
    renew_and_adopt(&w, &mut shop).await;
    assert!(signed_endpoints(&shop).is_empty());
    assert!(still_held(&mut shop).await);
}

#[tokio::test]
async fn test_own_record_two_withdrawals_to_the_same_endpoints_are_two_notices() {
    // Withdrawing A to no endpoint is told and left unacknowledged. B is offered alone, and
    // withdrawing it to no endpoint is saved and held in turn: a second notice, naming B,
    // not lost under the first because both leave the same endpoints.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_withdrawal_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, Arc::clone(&kept), app_id, publishing(), &recorded).await;
    let first = told(&shop).await.unwrap();
    assert_eq!(
        ids(&first.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    shop.receive(None).await.unwrap();
    shop.set_public_endpoints(
        std::collections::BTreeMap::from([(b(), payload("bc1q"))]),
        None,
    )
    .await
    .unwrap();
    time_read_fails(&w, &w.bob);
    assert!(shop.remove_public_endpoint(&b()).await.is_err());
    drop(shop);
    let shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    let mut withdrawn: Vec<Vec<String>> = shop
        .notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|notice| notice.kind == NoticeKind::PublicEndpointChangeNotPublished)
        .map(|notice| ids(&notice.endpoints_withdrawn))
        .collect();
    withdrawn.sort();
    assert_eq!(
        withdrawn,
        vec![vec![a().as_str().to_owned()], vec![b().as_str().to_owned()]]
    );
}

/// A store whose next commit deleting a key that starts with `prefix` fails, applying
/// nothing.
struct FailingDelete {
    inner: Arc<dyn StateStore>,
    prefix: Mutex<Option<Vec<u8>>>,
}

impl FailingDelete {
    fn failing(&self, batch: &pubky_dispatch_client::store::WriteBatch) -> bool {
        let mut prefix = self.prefix.lock().unwrap();
        let hit = prefix.as_ref().is_some_and(|prefix| {
            batch.writes().iter().any(|write| {
                matches!(write, pubky_dispatch_client::store::Write::Delete { key, .. }
                    if key.starts_with(prefix))
            })
        });
        if hit {
            *prefix = None;
        }
        hit
    }
}

impl StateStore for FailingDelete {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_dispatch_client::error::StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_dispatch_client::store::KeyValue>, pubky_dispatch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> Result<(), pubky_dispatch_client::error::StoreError> {
        if self.failing(&batch) {
            return Err(pubky_dispatch_client::error::StoreError::Unavailable);
        }
        self.inner.commit(batch)
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_dispatch_client::store::StoreOwner,
    ) -> Result<
        pubky_dispatch_client::store::AcquireOutcome,
        pubky_dispatch_client::error::StoreError,
    > {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_dispatch_client::store::StoreOwner,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> Result<pubky_dispatch_client::store::CommitOutcome, pubky_dispatch_client::error::StoreError>
    {
        if self.failing(&batch) {
            return Err(pubky_dispatch_client::error::StoreError::Unavailable);
        }
        self.inner.commit_if(ns, owner, batch)
    }
}

#[tokio::test]
async fn test_own_record_notice_acknowledged_before_its_clear_is_shown_once() {
    // The notice is written at the load and the clear after it fails. The host acknowledges
    // it while the change is still held; the next receive clears the change, and the notice
    // isn't shown again.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_withdrawal_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let failing = Arc::new(FailingDelete {
        inner: kept,
        prefix: Mutex::new(Some(b"rheld/".to_vec())),
    });
    let mut shop = Paykit::load(
        w.env(&w.bob, Arc::clone(&failing) as Arc<dyn StateStore>),
        publishing(),
        app_id,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert!(failing.prefix.lock().unwrap().is_none());
    assert!(shop
        .app_mut()
        .held_record_edit("paykit")
        .await
        .unwrap()
        .is_some());
    let notice = told(&shop).await.unwrap();
    assert_eq!(
        ids(&notice.endpoints_withdrawn),
        vec![a().as_str().to_owned()]
    );
    shop.acknowledge_notice(&notice.id).await.unwrap();
    assert!(told(&shop).await.is_none());
    shop.receive(None).await.unwrap();
    assert!(!still_held(&mut shop).await);
    assert!(told(&shop).await.is_none());
}

/// Sign `member` as Paykit's, as another writer of the record could, not through Paykit.
async fn sign_paykit_member(shop: &mut Paykit, member: serde_json::Map<String, Value>) {
    shop.app_mut()
        .set_protocol_member("paykit", Some(member), None)
        .await
        .unwrap();
}

fn unreadable_member() -> serde_json::Map<String, Value> {
    serde_json::json!({ "public_endpoints": "not an object" })
        .as_object()
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn test_own_record_unreadable_signed_member_with_nothing_held_opens_the_gate() {
    // The record signed holds a Paykit member that doesn't read, and nothing is held or
    // asked: there is nothing to tell, so nothing waits for a telling.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    sign_paykit_member(&mut shop, unreadable_member()).await;
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    assert!(told(&shop).await.is_none());
    let error = shop.update_own_record().await.unwrap_err();
    assert!(
        !matches!(
            error,
            paykit_sdk::PaykitError::Refused(paykit_sdk::Refusal::NotNow(
                paykit_sdk::NotNowReason::EndpointChangeUntold
            ))
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_own_record_held_change_over_an_unreadable_signed_member_is_told_and_cleared() {
    // The record signed holds a Paykit member that doesn't read, and a change saved after it
    // is held at the load: it is told naming no endpoint, and cleared.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    sign_paykit_member(&mut shop, unreadable_member()).await;
    let offer = PaykitMember {
        endpoint_identifiers: Vec::new(),
        public_endpoints: std::collections::BTreeMap::from([(a(), payload("lno1tips"))]),
    };
    time_read_fails(&w, &w.bob);
    assert!(shop
        .app_mut()
        .set_protocol_member("paykit", Some(offer.to_member()), None)
        .await
        .is_err());
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
    let notice = told(&shop).await.unwrap();
    assert!(
        notice.endpoints_withdrawn.is_empty()
            && notice.endpoints_offered.is_empty()
            && notice.endpoints_changed.is_empty()
    );
    assert!(shop
        .app_mut()
        .held_record_edit("paykit")
        .await
        .unwrap()
        .is_none());
    assert!(!recorded.sent("lno1tips"));
}

#[tokio::test]
async fn test_own_record_unreadable_held_change_is_told_by_an_ask_only_on_the_record_signed() {
    // A held change that doesn't read, beside an ask made on another record than the one
    // signed now: that ask may name an endpoint offered since, so the change is told naming no
    // endpoint. An ask made on the record signed now is told as asked.
    for (base_is_signed, named) in [(false, Vec::new()), (true, vec![a().as_str().to_owned()])] {
        let w = World::new();
        let kept = store();
        let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
        time_read_fails(&w, &w.bob);
        assert!(shop
            .app_mut()
            .set_protocol_member("paykit", Some(unreadable_member()), None)
            .await
            .is_err());
        let base = if base_is_signed { digest(&[]) } else { [8; 32] };
        keep_ask(&mut shop, [9; 32], base, &[a().as_str()]).await;
        let app_id = shop.app_id();
        drop(shop);
        let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
        let shop = load_recorded(&w, kept, app_id, publishing(), &recorded).await;
        let notice = told(&shop).await.unwrap();
        assert_eq!(ids(&notice.endpoints_withdrawn), named, "{base_is_signed}");
        assert!(notice.endpoints_offered.is_empty() && notice.endpoints_changed.is_empty());
    }
}

#[tokio::test]
async fn test_own_record_restore_in_place_over_an_unreadable_ask_completes() {
    // The installation's ask doesn't read: a restore in place leaves it out, as the settling
    // reads it, and completes.
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    let backup = shop.backup().await.unwrap();
    shop.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_dispatch_client::app::LocalWrite::Put {
                key: b"ec".to_vec(),
                value: b"not an ask".to_vec(),
            }],
        )
        .await
        .unwrap();
    drop(shop);
    let restored = Paykit::restore(
        w.env(&w.bob, kept),
        publishing(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await;
    assert!(restored.is_ok(), "{:?}", restored.err());
}
