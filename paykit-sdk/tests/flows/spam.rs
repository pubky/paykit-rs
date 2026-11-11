//! The spam rule (Paykit wire 4.1, Switch spec 7): at an app holding `contacts`, a message
//! that would create state from a sender the user blocked or the service flagged is only
//! counted, and one from a sender whose standing the app can't tell waits until the lists
//! are read; a message that updates a record the app holds is applied whoever sent it. An
//! app without `contacts` gates nothing.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PaykitConfig, RequestStatus, ResendSelection,
    UnrequestedPayment, LISTS_REFRESH_MS,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;
use pubky_switch_client::testing::Router;
use pubky_switch_client::transport::Transport;

use crate::common::{btc, draft, endpoint, request_from, store, ListsStore, World};

/// What alice's wallet knows of bob.
#[derive(Clone, Copy, Debug)]
enum Standing {
    /// Alice blocked him.
    Blocked,
    /// Her switch flagged him.
    Flagged,
    /// Her wallet's copy of the lists isn't known to be complete.
    Unknown,
}

/// Alice's wallet holding `pay` and `manage_contacts`, on `store`, inviting bob as
/// [`World::wallet`] does.
async fn contacts_wallet(w: &World, store: Arc<dyn StateStore>) -> Paykit {
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    wallet
}

/// Put bob in `standing` at alice's `wallet`, on `lists`.
async fn mark(w: &World, wallet: &mut Paykit, lists: &ListsStore, standing: Standing) {
    match standing {
        Standing::Blocked => wallet.app_mut().deny(&w.bob_id()).await.unwrap(),
        Standing::Flagged => lists.flag(&w.bob_id()),
        Standing::Unknown => lists.lists_unknown(true),
    }
}

/// Bob's request reaches alice's wallet once bob is `standing` there: only counted.
async fn request_only_counted(standing: Standing) {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    mark(&w, &mut wallet, &lists, standing).await;

    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty(), "{standing:?}");
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 1);
}

#[tokio::test]
async fn test_spam_blocked_sender_request_only_counted() {
    request_only_counted(Standing::Blocked).await;
}

#[tokio::test]
async fn test_spam_flagged_sender_request_only_counted() {
    request_only_counted(Standing::Flagged).await;
}

#[tokio::test]
async fn test_spam_unknown_lists_defer_a_request_until_read() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    mark(&w, &mut wallet, &lists, Standing::Unknown).await;

    // The wallet can't tell bob's standing: the request waits on the feed, neither opened
    // nor counted, never read as from a clean sender.
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.is_empty(), "{got:?}");
    assert!(wallet.requests().await.unwrap().is_empty());
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 0);

    // The lists are read: the request opens.
    lists.lists_unknown(false);
    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.applied.len(), 1, "{got:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_spam_failed_list_read_defers_a_request() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;

    // The wallet can't read bob's deny-list entry: the request waits on the feed, never
    // read as from a clean sender.
    lists.deny_reads_fail(true);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.is_empty(), "{got:?}");
    assert!(wallet.requests().await.unwrap().is_empty());

    lists.deny_reads_fail(false);
    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.applied.len(), 1, "{got:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

/// Bob's receipt for a payment alice's wallet made reaches it once bob is `standing` there:
/// it updates the record the wallet holds, and is applied.
async fn receipt_still_applies(standing: Standing) {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    mark(&w, &mut wallet, &lists, standing).await;

    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.applied.len(), 1, "{standing:?}: {got:?}");
    let key = wallet.request(&request).await.unwrap().record;
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(!value["receipt"].is_null(), "{standing:?}");
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 0);
}

#[tokio::test]
async fn test_spam_blocked_sender_receipt_still_applies() {
    receipt_still_applies(Standing::Blocked).await;
}

#[tokio::test]
async fn test_spam_flagged_sender_receipt_still_applies() {
    receipt_still_applies(Standing::Flagged).await;
}

#[tokio::test]
async fn test_spam_unknown_lists_receipt_still_applies() {
    receipt_still_applies(Standing::Unknown).await;
}

#[tokio::test]
async fn test_spam_blocked_sender_list_cancellation_and_resend_only_counted() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    // A list, a cancellation of a request the wallet holds no record of, and a
    // `resend_request`: each would create state.
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    let body = paykit_lib::bodies::RequestReply {
        reason: None,
        label: None,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_request_cancellation",
        vec![[9; 32]],
        body,
    );
    shop.request_resend(
        w.alice_id(),
        ResendSelection {
            request_hashes: vec![[9; 32]],
            ..ResendSelection::default()
        },
    )
    .await
    .unwrap();
    mark(&w, &mut wallet, &lists, Standing::Blocked).await;

    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.resends, 0, "{got:?}");
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 3);
    // No list was kept: a payment to bob's list endpoints finds none.
    let refused = wallet
        .pay_without_request(UnrequestedPayment {
            payee: w.bob_id(),
            amount: btc("0.001"),
            options: PayOptions {
                endpoint: Some(endpoint("btc-lightning-bolt12")),
                ..PayOptions::default()
            },
        })
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_spam_blocked_payer_unrequested_proof_only_counted() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(UnrequestedPayment {
            payee: w.bob_id(),
            amount: btc("0.001"),
            options: PayOptions {
                endpoint: Some(endpoint("btc-lightning-bolt12")),
                ..PayOptions::default()
            },
        })
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // Bob blocks alice before his shop reads her proof, which no record of his matches.
    shop.app_mut().deny(&w.alice_id()).await.unwrap();

    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap().is_empty());
    assert_eq!(shop.spam_count(&w.alice_id()).await.unwrap(), 1);
}

#[tokio::test]
async fn test_spam_without_contacts_unknown_lists_gate_nothing() {
    // A wallet without `contacts` never reads the lists: bob's standing is unknown there,
    // and his request opens.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.applied.len(), 1, "{got:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 0);
}

/// A transport counting the `GET /allow` reads of the lists, and failing them to connect
/// while `fail` is set; every other request goes through the world's router.
struct CountingAllow {
    inner: Arc<Router>,
    reads: AtomicUsize,
    fail: AtomicBool,
}

impl CountingAllow {
    fn new(w: &World) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::clone(&w.router),
            reads: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        })
    }
}

impl Transport for CountingAllow {
    fn send(
        &self,
        request: pubky_switch_client::transport::Request,
    ) -> pubky_switch_client::transport::BoxFuture<
        '_,
        Result<
            pubky_switch_client::transport::Response,
            pubky_switch_client::error::TransportError,
        >,
    > {
        if request.method == "GET" && request.path_and_query.ends_with("/allow") {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Box::pin(async {
                    Err(pubky_switch_client::error::TransportError::Connect(
                        "the lists can't be read now".into(),
                    ))
                });
            }
        }
        self.inner.send(request)
    }
}

/// Alice's wallet holding `contacts`, its lists read at its join, loaded again with its
/// requests through a counting transport.
async fn counted_wallet(w: &World) -> (Paykit, Arc<CountingAllow>) {
    let store = store();
    let joined = contacts_wallet(w, Arc::clone(&store)).await;
    let app_id = joined.app_id();
    drop(joined);
    let transport = CountingAllow::new(w);
    let wallet = w
        .load_with_transport(
            &w.alice,
            store,
            app_id,
            Arc::clone(&transport) as Arc<dyn Transport>,
        )
        .await;
    (wallet, transport)
}

#[tokio::test]
async fn test_spam_lists_read_at_most_every_ten_minutes() {
    let w = World::new();
    let (mut wallet, transport) = counted_wallet(&w).await;

    // The join read the lists; within 10 minutes no receive reads them again.
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(LISTS_REFRESH_MS - 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(transport.reads.load(Ordering::SeqCst), 0);
    // Then the next receive reads them, once.
    w.clock.advance_ms(1);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(transport.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_spam_failed_refresh_of_a_stale_copy_defers_a_request() {
    // The copy of the lists is past its 10 minutes and its read fails to connect: it may
    // miss a sender flagged since, so the request waits on the feed until a read succeeds
    // (Paykit wire 4.1), and the receive itself goes on.
    let w = World::new();
    let (mut wallet, transport) = counted_wallet(&w).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    w.clock.advance_ms(LISTS_REFRESH_MS);
    transport.fail.store(true, Ordering::SeqCst);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.is_empty(), "{got:?}");
    assert!(wallet.requests().await.unwrap().is_empty());
    assert_eq!(transport.reads.load(Ordering::SeqCst), 1);

    // The next receive reads the lists: the request opens.
    transport.fail.store(false, Ordering::SeqCst);
    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.applied.len(), 1, "{got:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}
