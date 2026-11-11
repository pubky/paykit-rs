//! An own account that may have been erased (Switch spec 12.7): the switch answers as for an
//! account it doesn't host, unsigned, and the identity publishes no `identity.json`. The
//! client waits 24 hours before it takes the account as gone; meanwhile the switch counts as
//! not answering, and once gone the user is told, nothing is done by itself and the journal
//! stays for the recovery.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use paykit_sdk::{PayOptions, PayOutcome, Refusal};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::Router;
use pubky_switch_client::transport::{BoxFuture, Request, Response, Transport};

use crate::common::{draft, request_from, store, World, ALICE_ORIGIN, HOUR_MS};

/// A transport whose requests to alice's switch are answered, once armed, as an erased
/// account answers: `404`, signed with no key the app pins. Every other request goes through
/// the world's router.
struct ErasedSwitch {
    inner: Arc<Router>,
    erased: AtomicBool,
}

impl Transport for ErasedSwitch {
    fn send(
        &self,
        request: Request,
    ) -> BoxFuture<'_, Result<Response, pubky_switch_client::error::TransportError>> {
        if request.origin == ALICE_ORIGIN && self.erased.load(Ordering::SeqCst) {
            return Box::pin(async {
                Ok(Response {
                    status: 404,
                    headers: Vec::new(),
                    body: br#"{"error":{"code":"not_found","message":"no such account"}}"#.to_vec(),
                })
            });
        }
        self.inner.send(request)
    }
}

#[tokio::test]
async fn test_erasure_wait_is_the_switch_not_answering_then_the_account_gone() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let wallet_id = wallet.app_id();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    drop(wallet);
    let transport = Arc::new(ErasedSwitch {
        inner: Arc::clone(&w.router),
        erased: AtomicBool::new(false),
    });
    let mut wallet = w
        .load_with_transport(
            &w.alice,
            Arc::clone(&base),
            wallet_id,
            Arc::clone(&transport) as Arc<dyn Transport>,
        )
        .await;
    // The signer erased the account and deleted its identity.json from the Homeserver.
    transport.erased.store(true, Ordering::SeqCst);
    w.homeserver
        .remove(w.alice_id(), pubky_switch_core::records::IDENTITY_PATH);
    // Until the wait ends, every read is the switch not answering.
    for _ in 0..4 {
        let error = wallet.receive(None).await.unwrap_err();
        assert!(
            matches!(error.refusal(), Some(Refusal::SwitchUnavailable(_))),
            "{error:?}"
        );
        w.clock.advance_ms(6 * HOUR_MS);
    }
    // The last try, 24 hours on: the account is gone at this switch, and the user is told.
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.account_gone, "{summary:?}");
    assert!(summary.report.stopped);
    // Told again until the host acknowledges it; the journal stays for the recovery.
    let again = wallet.receive(None).await.unwrap();
    assert!(again.account_gone, "{again:?}");
    assert_eq!(wallet.payments().await.unwrap().len(), 1);
    assert_eq!(w.rail.all_executions().len(), 1);
}
