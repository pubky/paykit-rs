//! An own-record edit the client saves before it reads the service time to sign it (Switch
//! spec 4.2): when that read fails, the edit stays saved and unsigned for a later read.

use std::sync::{Arc, Mutex};

use paykit_sdk::{AppScopes, Paykit, PaykitConfig};
use pubky_switch_client::testing::Fault;
use pubky_switch_client::transport::{BoxFuture, Request, Response, Transport};
use pubky_switch_client::TransportError;
use serde_json::Value;

use super::{payload, publishing};
use crate::common::{endpoint, store, World};

/// Every request body sent to the Switch.
struct Recorded {
    inner: Arc<dyn Transport>,
    bodies: Mutex<Vec<Vec<u8>>>,
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
                || pubky_switch_core::enc::unb64(text).is_ok_and(|bytes| {
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

#[tokio::test]
async fn test_own_record_saved_offer_never_signed_once_publishing_stops() {
    // An offer saved while the service time can't be read stays unsigned. The app is loaded
    // again without publishing its record: the saved offer never reaches the switch, and the
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
    w.clock.advance_ms(11 * 60 * 1000);
    w.bob.switch().fail_next(Fault::Unreachable {
        path: "/feed".into(),
        after: 0,
    });
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
    let app_id = shop.app_id();
    drop(shop);
    let recorded = Arc::new(Recorded {
        inner: Arc::clone(&w.router) as Arc<dyn Transport>,
        bodies: Mutex::new(Vec::new()),
    });
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
    for body in recorded.bodies.lock().unwrap().iter() {
        if let Ok(value) = serde_json::from_slice::<Value>(body) {
            assert!(!mentions(&value, "lno1tips"), "{value}");
        }
    }
}
