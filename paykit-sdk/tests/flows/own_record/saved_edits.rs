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
