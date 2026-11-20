//! The buyer's first payment when the seller's real switch answer reaches the final check.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use pubky_switch_client::error::TransportError;
use pubky_switch_client::store::{client_namespace, StateStore};
use pubky_switch_client::transport::{FirstSend, Request, Response, Transport};

const SELLER: &str = "https://seller.payment.test";

/// Deliver the seller's actual answer to this buyer's original request at its
/// journaled first dispatch. Every other request, including its deadline, goes through.
struct LateSeller {
    transport: Arc<dyn Transport>,
    store: Arc<MemoryStore>,
    namespace: String,
    waited_before_paying: AtomicBool,
    delivered: AtomicBool,
}

impl Transport for LateSeller {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        if request.origin != SELLER
            || request.method != "GET"
            || !request.path_and_query.ends_with("/identity")
            || self.delivered.load(Ordering::SeqCst)
        {
            return self.transport.send(request);
        }
        Box::pin(async move {
            // Finish the real service read on the second runtime worker before exposing
            // its response. Both backends then exercise precisely the same delivery order.
            let response = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.transport.send(request))
            })?;
            std::future::poll_fn(|cx| {
                let entries = self
                    .store
                    .scan(&self.namespace, b"p/paykit/j/", None, 1)
                    .unwrap();
                let paying = entries.first().is_some_and(|(_, value)| {
                    serde_json::from_slice::<serde_json::Value>(value).unwrap()["d"]["stage"]
                        == "paying"
                });
                if paying {
                    self.delivered.store(true, Ordering::SeqCst);
                    Poll::Ready(())
                } else {
                    self.waited_before_paying.store(true, Ordering::SeqCst);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            Ok(response)
        })
    }

    fn send_before<'a>(
        &'a self,
        request: Request,
        first: &'a FirstSend,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        self.transport.send_before(request, first)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_first_payment_uses_the_late_seller_confirmation_and_delivers_its_proof() -> TestResult
{
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = tokio::time::timeout(Duration::from_secs(45), Box::pin(pay(&world))).await;
        world.close().await?;
        result??;
    }
    Ok(())
}

async fn pay(world: &World) -> TestResult {
    let payer = setup_identity(world, "buyer", "https://buyer.payment.test").await?;
    let seller = setup_identity(world, "seller", SELLER).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let store = Arc::new(MemoryStore::new());
    let (mut buyer, _) = Box::pin(join(
        world,
        payer,
        "buyer",
        "wallet",
        AppScopes::new().pay().manage_contacts().build()?,
        store.clone(),
        &rail,
    ))
    .await?;
    let (mut shop, _) = Box::pin(join(
        world,
        seller,
        "seller",
        "shop",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    buyer.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&buyer.identity()).await?;
    let request = shop
        .send_request(
            buyer.identity(),
            invoice("btc-lightning-bolt11", "ln-first-payment")?,
        )
        .await?
        .request
        .ok_or("request missing")?;
    buyer.receive(None).await?;
    let app_id = buyer.app_id();
    drop(buyer);

    world.advance_ms(11 * 60_000);
    let delivery = Arc::new(LateSeller {
        transport: world.transport(),
        store: store.clone(),
        namespace: client_namespace(&app_id),
        waited_before_paying: AtomicBool::new(false),
        delivered: AtomicBool::new(false),
    });
    let mut env = world.app_env(store);
    env.transport = delivery.clone();
    let mut buyer = Paykit::load(
        env,
        Default::default(),
        app_id,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    let (mut buyer, proof_id, attempt_id) = tokio::spawn(Box::pin(async move {
        let outcome = buyer
            .pay_request(&request, PayOptions::default())
            .await
            .outcome?;
        let PayOutcome::Paid {
            attempt_id,
            proof_msg_id: Some(id),
            ..
        } = outcome
        else {
            panic!("{outcome:?}")
        };
        Ok::<_, paykit_sdk::PaykitError>((buyer, id, attempt_id))
    }))
    .await??;
    assert!(delivery.waited_before_paying.load(Ordering::SeqCst));
    assert!(delivery.delivered.load(Ordering::SeqCst));
    assert_eq!(rail.all_pay_calls(), [(attempt_id, 1)].into());
    assert_eq!(rail.all_executions(), [(attempt_id, 1)].into());
    assert!(buyer.app().delivery(&proof_id).await?.is_some());
    shop.receive(None).await?;
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .ok_or("sale missing")?;
    assert_eq!(sale.status, PayeeStatus::ProofReceived);
    let receipt = shop
        .confirm_payment(&sale.record)
        .await?
        .ok_or("receipt missing")?;
    buyer.receive(None).await?;
    let receipts = buyer.receipts(ReceiptsOf::Request(request)).await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].msg_id, receipt);
    Ok(())
}
