//! An own-record edit the client saves before it reads the service time to sign it (Dispatch
//! spec 4.2): when that read fails, the edit stays saved and unsigned for a later read.

use std::sync::{Arc, Mutex};

use paykit_lib::app_record::PaykitMember;
use paykit_sdk::{AppScopes, Paykit, PaykitConfig, Rail};
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
    signer.switch().fail_next(Fault::Unreachable {
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
#[ignore = "needs the Dispatch revision that keeps a saved edit dormant after a reload or a restore"]
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
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    let _ = restored.receive(None).await;
    let published = PaykitMember::read(&super::on_homeserver(&w, &restored).unwrap()).unwrap();
    assert!(published.public_endpoints.is_empty());
}

#[tokio::test]
#[ignore = "needs the Dispatch revision that keeps a saved edit dormant after a reload or a restore"]
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
    let _ = shop.receive(None).await;
    assert!(!recorded.sent("lno1tips"));
}

#[tokio::test]
#[ignore = "needs the Dispatch revision that keeps a saved edit dormant after a reload or a restore"]
async fn test_own_record_saved_offer_never_signed_after_an_unknown_store_result() {
    // Loaded again without publishing, the replacement of the saved offer meets a store that
    // can't tell whether it applied, and didn't. The next read never signs the offer.
    use pubky_dispatch_client::store::{Fault as StoreFault, FaultStore};
    let w = World::new();
    let kept = store();
    let mut shop = shop_on(&w, false, Arc::clone(&kept)).await;
    save_offer_unsigned(&w, &mut shop).await;
    let app_id = shop.app_id();
    drop(shop);
    let faulty = Arc::new(FaultStore::new(Arc::clone(&kept), StoreFault::None));
    faulty.fail_put_once_with(b"rmem", pubky_dispatch_client::error::StoreError::Unknown);
    let recorded = Recorded::over(Arc::clone(&w.router) as Arc<dyn Transport>);
    let mut shop = load_recorded(
        &w,
        Arc::clone(&faulty) as Arc<dyn StateStore>,
        app_id,
        PaykitConfig::default(),
        &recorded,
    )
    .await;
    let _ = shop.receive(None).await;
    let _ = shop.receive(None).await;
    assert!(!recorded.sent("lno1tips"));
}

#[tokio::test]
#[ignore = "needs the Dispatch revision that keeps a saved edit dormant after a reload or a restore"]
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
}
