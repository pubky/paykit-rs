//! A blocked payee's requests are paused at a payer app holding `contacts`, and paid only
//! once the user confirms what it was shown (Paykit spec 8.1, "Blocked counterparties").

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PaykitConfig, RailDeclaration, Refusal, RequestRef,
    RequestStatus, UnrequestedPayment,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::Fault;
use pubky_switch_core::http::ErrorCode;

use crate::common::{btc, day_window, draft, endpoint, request_from, store, World};

/// Alice's wallet holding `pay` and `manage_contacts`, with bob's request received, then
/// bob blocked. Returns the wallet and the request.
async fn blocked(w: &World) -> (Paykit, RequestRef) {
    let (mut wallet, _, request) = received(w, PaykitConfig::default()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    (wallet, request)
}

/// Alice's wallet holding `pay` and `manage_contacts`, with `config`, and bob's request
/// received. Returns the wallet, bob's shop and the request.
async fn received(w: &World, config: PaykitConfig) -> (Paykit, Paykit, RequestRef) {
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            config,
            store(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    (wallet, shop, request)
}

/// The deny list's next read fails.
fn fail_list_read(w: &World) {
    w.alice.switch().fail_next(Fault::Error {
        path: "/allow".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
}

/// A wallet on an idempotent rail without history pays bob's request, the dispatch left
/// in flight; then `between` runs, and the wallet resumes a minute later. Returns how many
/// times the rail was called.
async fn resumed_repeat(w: &World, between: impl AsyncFnOnce(&mut Paykit)) -> u64 {
    w.rail.set_declaration(
        RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS).with_idempotent(true),
    );
    let (mut wallet, _, request) = received(w, day_window()).await;
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    between(&mut wallet).await;
    w.clock.advance_ms(60_000);
    wallet.resume_payments().await.unwrap();
    w.rail.pay_calls(&attempt_id)
}

/// The payment the refusal shows.
fn shown(error: paykit_sdk::PaykitError) -> paykit_sdk::BlockedPayment {
    match error.refusal() {
        Some(Refusal::Blocked { payment }) => (**payment).clone(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn test_blocked_request_paid_only_once_the_user_confirms_it() {
    let w = World::new();
    let (mut wallet, request) = blocked(&w).await;
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    let payment = shown(error);
    assert_eq!(payment.payee, w.bob_id());
    assert_eq!(payment.request, Some(request));
    assert_eq!(payment.amount, btc("1"));
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Blocked
    );
    // A confirmation of something else passes nothing.
    let mut other = payment.clone();
    other.amount = btc("2");
    let again = wallet
        .pay_request(
            &request,
            PayOptions::default().with_blocked_confirmed(other),
        )
        .await
        .unwrap_err();
    assert_eq!(shown(again), payment);
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_blocked_confirmed(payment),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_blocked_pause_ends_when_the_payee_is_unblocked() {
    let w = World::new();
    let (mut wallet, request) = blocked(&w).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_blocked_resume_repeats_a_dispatch_to_a_payee_not_blocked() {
    let w = World::new();
    assert_eq!(resumed_repeat(&w, async |_| {}).await, 2);
}

#[tokio::test]
async fn test_blocked_resume_makes_no_repeat_to_a_blocked_payee() {
    // Blocked through the client directly, which the copy doesn't see: the repeat reads the
    // list fresh.
    let w = World::new();
    let calls = resumed_repeat(&w, async |wallet| {
        wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    })
    .await;
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn test_blocked_resume_makes_no_repeat_while_the_deny_list_fails() {
    let w = World::new();
    let calls = resumed_repeat(&w, async |_| fail_list_read(&w)).await;
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn test_blocked_payment_without_a_request_made_only_once_confirmed() {
    let w = World::new();
    let (mut wallet, mut shop, _) = received(&w, PaykitConfig::default()).await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    let payment = || {
        UnrequestedPayment::new(w.bob_id(), btc("0.001"))
            .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")))
    };
    let shown = shown(wallet.pay_without_request(payment()).await.unwrap_err());
    assert_eq!(shown.request, None);
    assert_eq!(shown.amount, btc("0.001"));
    let confirmed = UnrequestedPayment::new(w.bob_id(), btc("0.001")).with_options(
        PayOptions::default()
            .with_endpoint(endpoint("btc-lightning-bolt12"))
            .with_blocked_confirmed(shown),
    );
    let outcome = wallet.pay_without_request(confirmed).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

/// Bob's request received at a wallet whose copy of the lists is read now; bob is then
/// blocked through the client directly, which the copy doesn't see, and the clock moves
/// `ms` on.
async fn blocked_unseen(w: &World, ms: u64) -> (Paykit, RequestRef) {
    let (mut wallet, _, request) = received(w, PaykitConfig::default()).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(ms);
    (wallet, request)
}

#[tokio::test]
async fn test_blocked_copy_read_within_10_minutes_makes_no_call() {
    let w = World::new();
    let (mut wallet, request) = blocked_unseen(&w, 9 * 60_000).await;
    // A read now would fail: none is made, and the copy shows bob not blocked.
    fail_list_read(&w);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_blocked_copy_older_than_10_minutes_read_first() {
    let w = World::new();
    let (mut wallet, request) = blocked_unseen(&w, 11 * 60_000).await;
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(shown(error).payee, w.bob_id());
}

#[tokio::test]
async fn test_blocked_copy_older_than_10_minutes_fails_closed_on_a_failed_read() {
    let w = World::new();
    let (mut wallet, _, request) = received(&w, PaykitConfig::default()).await;
    w.clock.advance_ms(11 * 60_000);
    fail_list_read(&w);
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_blocked_local_block_seen_with_no_read() {
    let w = World::new();
    let (mut wallet, request) = blocked(&w).await;
    // A read would fail; the block this app wrote is in its copy already.
    fail_list_read(&w);
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(shown(error).payee, w.bob_id());
}

/// The next `n` calls to alice's switch's `/ops` don't connect: the batch is kept unanswered.
fn keep_ops(w: &World, n: usize) {
    for _ in 0..n {
        w.alice.switch().fail_next(Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    }
}

#[tokio::test]
async fn test_blocked_own_block_kept_unanswered_counts_at_once() {
    // The wallet blocks bob while its switch doesn't answer: the `deny.put` is kept, the list
    // at the switch doesn't hold bob yet. The resume's fresh read of the list counts the
    // wallet's own block all the same, and makes no repeat (Paykit spec 8.1).
    let w = World::new();
    let calls = resumed_repeat(&w, async |wallet| {
        keep_ops(&w, 8);
        wallet.block(&w.bob_id()).await.unwrap();
        let lists = wallet.app_mut().allow_list().await.unwrap();
        assert!(!lists.deny.contains(&w.bob_id()));
    })
    .await;
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn test_blocked_own_unblock_kept_unanswered_counts_at_once() {
    // Bob is blocked; the wallet's unblock is kept unanswered: the list at the switch still
    // holds him, but the resume's fresh read counts the wallet's own unblock at once, and
    // makes the repeat (Paykit spec 8.1).
    let w = World::new();
    let calls = resumed_repeat(&w, async |wallet| {
        wallet.block(&w.bob_id()).await.unwrap();
        keep_ops(&w, 8);
        wallet.unblock(&w.bob_id()).await.unwrap();
        let lists = wallet.app_mut().allow_list().await.unwrap();
        assert!(lists.deny.contains(&w.bob_id()));
    })
    .await;
    assert_eq!(calls, 2);
}

#[tokio::test]
async fn test_blocked_own_block_mark_goes_once_the_list_shows_it() {
    // The kept `deny.put` lands at the next flush; the next read of the list shows bob, and
    // the mark of the block kept goes.
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    keep_ops(&w, 1);
    wallet.block(&w.bob_id()).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"bk/").await.unwrap().len(), 1);
    wallet.flush().await.unwrap();
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(lists.deny.contains(&w.bob_id()));
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"bk/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_blocked_restore_reads_the_deny_list_again_before_a_payment() {
    // The backup holds the deny list read a moment before it, without bob; bob is blocked
    // after it. A restore within 10 minutes reads the list again before the user's payment,
    // which is refused, not judged by the backup's copy (Paykit spec 8.1).
    let w = World::new();
    let (mut wallet, _, request) = received(&w, PaykitConfig::default()).await;
    let backup = wallet.backup().await.unwrap();
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.reconcile().await.unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    restored
        .confirm_possibly_cancelled(&request, pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    let refused = restored
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Blocked { .. })),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_blocked_own_unblock_kept_unanswered_lets_the_users_payment_go() {
    // The copy the user's payment is judged by drops bob at once while the unblock is kept:
    // the payment goes, with no confirmation.
    let w = World::new();
    let (mut wallet, request) = blocked(&w).await;
    keep_ops(&w, 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

/// A `resend_request` from bob's shop for `request`, kept by the wallet to answer, as a
/// receive that stopped before answering it leaves it.
async fn kept_resend_request(w: &World, wallet: &mut Paykit, shop: &Paykit, request: &RequestRef) {
    let body = paykit_lib::bodies::ResendRequest {
        msg_ids: Vec::new(),
        accepted_from: None,
        accepted_until: None,
        request_hashes: vec![request.request_hash],
        billing_period: None,
    }
    .to_body()
    .unwrap();
    let msg_id = "6f0c2b8e-4a1d-4c3e-9f7a-2b5d8e1c0a93";
    let value = serde_json::json!({
        "v": 1,
        "d": {
            "seq": 1,
            "msg_id": msg_id,
            "requester": pubky_switch_core::enc::b64(&w.bob_id()),
            "cert": shop.app().cert().signed(),
            "body": body,
        }
    });
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: format!("ans/{:020}/{msg_id}", 1).into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
}

#[tokio::test]
async fn test_blocked_kept_resend_request_answered_while_its_sender_isnt_blocked() {
    let w = World::new();
    let (mut wallet, shop, request) = received(&w, PaykitConfig::default()).await;
    kept_resend_request(&w, &mut wallet, &shop, &request).await;
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    assert!(wallet
        .app()
        .protocol_scan(b"ans/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_blocked_kept_resend_request_from_a_sender_blocked_since_not_answered() {
    // Blocking stops every send to the peer (Paykit spec 8.1): a request to re-send kept
    // before the block is dropped unanswered, a block kept unanswered counting at once.
    let w = World::new();
    let (mut wallet, shop, request) = received(&w, PaykitConfig::default()).await;
    kept_resend_request(&w, &mut wallet, &shop, &request).await;
    keep_ops(&w, 1);
    wallet.block(&w.bob_id()).await.unwrap();
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    assert!(wallet
        .app()
        .protocol_scan(b"ans/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_blocked_peer_asked_nothing_after_a_missed_item() {
    // Bob's request is open, but he is blocked: a `missed` item asks him nothing.
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    assert!(crate::common::asked(&wallet, &w.bob_id()).await.is_empty());
    assert!(wallet.app().protocol_scan(b"mi/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_blocked_peer_not_blocked_asked_after_a_missed_item() {
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    assert_eq!(crate::common::asked(&wallet, &w.bob_id()).await.len(), 1);
}

/// A store whose next commit writing the copy of the deny list (`p/dl`) fails, once armed.
#[derive(Default)]
struct FailingDenyCopy {
    inner: pubky_switch_client::store::MemoryStore,
    armed: std::sync::atomic::AtomicBool,
}

impl pubky_switch_client::store::StateStore for FailingDenyCopy {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, pubky_switch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        let copy = batch.writes.iter().any(|write| {
            matches!(write, pubky_switch_client::store::Write::Put { key, .. } if key == b"p/dl")
        });
        if copy && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        self.inner.commit(batch)
    }
}

#[tokio::test]
async fn test_blocked_own_block_kept_mark_and_copy_written_together() {
    // The wallet's block is kept unanswered and the store fails the commit of the copy: the
    // mark of the kept block isn't left without the copy, so no payment the user starts
    // within 10 minutes judges a copy that lacks a block the app counts.
    let w = World::new();
    let failing = std::sync::Arc::new(FailingDenyCopy::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            std::sync::Arc::clone(&failing)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    wallet.receive(None).await.unwrap();
    let before = wallet.app().protocol_get(b"dl").await.unwrap();
    keep_ops(&w, 1);
    failing.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wallet.block(&w.bob_id()).await.is_err());
    assert!(wallet.app().protocol_scan(b"bk/").await.unwrap().is_empty());
    assert_eq!(wallet.app().protocol_get(b"dl").await.unwrap(), before);
}
