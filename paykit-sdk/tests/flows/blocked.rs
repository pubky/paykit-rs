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
async fn test_blocked_peer_asked_after_a_missed_item_only_once_unblocked() {
    // Bob's request is open, but he is blocked: a `missed` item asks him nothing, and keeps
    // the ask; once he is unblocked, the next receive asks him, once (Paykit spec 8.1).
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(crate::common::asked(&wallet, &w.bob_id()).await.is_empty());
    assert!(wallet.app().protocol_scan(b"mi/").await.unwrap().is_empty());
    assert_eq!(wallet.app().protocol_scan(b"ask/").await.unwrap().len(), 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(crate::common::asked(&wallet, &w.bob_id()).await.len(), 1);
    assert!(wallet
        .app()
        .protocol_scan(b"ask/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_blocked_peer_asked_after_a_missed_item_only_once_the_deny_list_reads() {
    // The reads of the lists at the receive that takes the `missed` item fail: the copy of
    // the deny list isn't known, so bob is asked nothing yet, and the ask is kept; the next
    // receive reads the list, and asks him.
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    // The read at the receive's start and the one before the asks both fail.
    fail_list_read(&w);
    fail_list_read(&w);
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    assert!(crate::common::asked(&wallet, &w.bob_id()).await.is_empty());
    assert_eq!(wallet.app().protocol_scan(b"ask/").await.unwrap().len(), 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(crate::common::asked(&wallet, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_kept_resend_request_answered_only_once_the_deny_list_reads() {
    // The read of the lists fails before the wallet answers bob's kept request to re-send:
    // the copy of the deny list isn't known, so it is neither answered nor dropped; the
    // next receive reads the list, and answers it.
    let w = World::new();
    let (mut wallet, shop, request) = received(&w, PaykitConfig::default()).await;
    kept_resend_request(&w, &mut wallet, &shop, &request).await;
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    fail_list_read(&w);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    assert_eq!(wallet.app().protocol_scan(b"ans/").await.unwrap().len(), 1);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    assert!(wallet
        .app()
        .protocol_scan(b"ans/")
        .await
        .unwrap()
        .is_empty());
}

/// Bob's shop holding `request` and `manage_contacts`, with a due mark of alice's replies
/// dropped for the bounds, as the shop keeps one once it holds her request's record, and
/// alice's wallet to ask. Returns the shop.
async fn shop_with_a_due_mark(w: &World) -> Paykit {
    let _wallet = w.wallet("wallet").await;
    let mut shop = w
        .join(
            &w.bob,
            "till",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    shop.receive(None).await.unwrap();
    let alice = pubky_switch_core::enc::b64(&w.alice_id());
    let at = w.clock.now_ms();
    let value = serde_json::json!({
        "v": 1,
        "d": {
            "sender": alice,
            "earliest_ms": at,
            "latest_ms": at,
            "due": true,
            "asked_at_ms": null,
            "queued": null,
        }
    });
    shop.app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: format!("pd/{alice}").into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        }])
        .await
        .unwrap();
    shop
}

#[tokio::test]
async fn test_blocked_payer_asked_for_dropped_replies_only_once_unblocked() {
    // The payee side keeps what it owes a blocked payer too: alice's mark asks nothing while
    // she is blocked, and asks once she is unblocked (Paykit spec 8.1).
    let w = World::new();
    let mut shop = shop_with_a_due_mark(&w).await;
    shop.block(&w.alice_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(crate::common::asked(&shop, &w.alice_id()).await.is_empty());
    assert_eq!(shop.app().protocol_scan(b"pd/").await.unwrap().len(), 1);
    shop.unblock(&w.alice_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(crate::common::asked(&shop, &w.alice_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_payer_asked_for_dropped_replies_only_once_the_deny_list_reads() {
    // The shop's read of the lists fails: the copy isn't known, so alice's mark asks
    // nothing; the next receive reads the list, and asks her.
    let w = World::new();
    let mut shop = shop_with_a_due_mark(&w).await;
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    w.bob.switch().fail_next(Fault::Error {
        path: "/allow".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    shop.receive(None).await.unwrap();
    assert!(crate::common::asked(&shop, &w.alice_id()).await.is_empty());
    shop.receive(None).await.unwrap();
    assert_eq!(crate::common::asked(&shop, &w.alice_id()).await.len(), 1);
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
    failing
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wallet.block(&w.bob_id()).await.is_err());
    assert!(wallet.app().protocol_scan(b"bk/").await.unwrap().is_empty());
    assert_eq!(wallet.app().protocol_get(b"dl").await.unwrap(), before);
}

#[tokio::test]
async fn test_blocked_kept_resend_request_at_another_key_refused_as_corrupt() {
    // A request to re-send kept under a key naming another `seq` than its own is a corrupt
    // store: the receive fails rather than answer or drop it.
    let w = World::new();
    let (mut wallet, shop, request) = received(&w, PaykitConfig::default()).await;
    kept_resend_request(&w, &mut wallet, &shop, &request).await;
    let (key, value) = wallet.app().protocol_scan(b"ans/").await.unwrap().remove(0);
    let moved = String::from_utf8(key.clone())
        .unwrap()
        .replace(&format!("ans/{:020}/", 1), &format!("ans/{:020}/", 2));
    wallet
        .app_mut()
        .protocol_commit(&[
            pubky_switch_client::app::LocalWrite::Delete { key },
            pubky_switch_client::app::LocalWrite::Put {
                key: moved.into_bytes(),
                value,
            },
        ])
        .await
        .unwrap();
    let failed = wallet.receive(None).await.unwrap_err();
    assert!(crate::common::is_corrupt(&failed), "{failed:?}");
    assert_eq!(wallet.app().protocol_scan(b"ans/").await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_blocked_mark_queued_request_naming_another_message_refused_as_corrupt() {
    // A mark's queued request names the `msg_id` of another message the outbox delivered to
    // alice, the shop's request, with a hash that isn't that message's: it doesn't point to
    // the request it asked with, so the receive fails as corrupt rather than count alice
    // asked (the mark's proof of its pointer).
    let w = World::new();
    let mut shop = shop_with_a_due_mark(&w).await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let sent = shop
        .app()
        .sent_to(&w.alice_id(), 0, u64::MAX)
        .await
        .unwrap();
    let request = sent
        .iter()
        .find(|m| m.kind == "paykit.payment_request")
        .unwrap();
    let (key, value) = shop.app().protocol_scan(b"pd/").await.unwrap().remove(0);
    let mut mark: serde_json::Value = serde_json::from_slice(&value).unwrap();
    mark["d"]["queued"] = serde_json::json!({
        "msg_id": request.msg_id,
        "hash": pubky_switch_core::enc::b64(&[0; 32]),
    });
    shop.app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key,
            value: serde_json::to_vec(&mark).unwrap(),
        }])
        .await
        .unwrap();
    let failed = shop.receive(None).await.unwrap_err();
    assert!(crate::common::is_corrupt(&failed), "{failed:?}");
    assert!(crate::common::asked(&shop, &w.alice_id()).await.is_empty());
}

#[tokio::test]
async fn test_blocked_peer_ask_held_goes_30_days_after_its_time() {
    // Bob is blocked when a `missed` item asks him: the ask is held, and stays a day short of
    // 30 days after the item's time; past them bob keeps nothing it asks for, so it goes
    // unsent, and an unblock then asks him nothing.
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"ask/").await.unwrap().len(), 1);
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS - pubky_switch_core::records::DAY_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"ask/").await.unwrap().len(), 1);
    w.clock.advance_ms(2 * pubky_switch_core::records::DAY_MS);
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_scan(b"ask/")
        .await
        .unwrap()
        .is_empty());
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(crate::common::asked(&wallet, &w.bob_id()).await.is_empty());
}

#[tokio::test]
async fn test_blocked_own_block_kept_unanswered_survives_a_restore_and_is_written_again() {
    // The wallet blocks bob while its switch doesn't answer, and is backed up with the block
    // kept unanswered. The kept batch stays with the device that kept it: the restored app
    // still counts the block, refusing the user's payment, and writes it again, so the list
    // at the switch holds bob (Paykit spec 8.1).
    let w = World::new();
    let (mut wallet, _, request) = received(&w, PaykitConfig::default()).await;
    keep_ops(&w, 1);
    wallet.block(&w.bob_id()).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(lists.deny.contains(&w.bob_id()));
    // Past the restore's hold, which the user confirms, the block refuses the payment.
    let held = PayOptions::default()
        .with_possibly_cancelled_confirmed(pubky_switch_client::app::UserConfirmed::new());
    let refused = wallet.pay_request(&request, held).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Blocked { .. })),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_blocked_own_block_restored_written_again_after_a_try_that_could_not_go() {
    // The wallet's block is kept unanswered when the backup is taken. After the restore the
    // first write of it again can't go: a batch the restored app kept meanwhile still can't
    // be sent, so the block is neither written nor kept. The next receive writes it again,
    // and the list at the switch holds bob (Paykit spec 8.1).
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    keep_ops(&w, 1);
    wallet.block(&w.bob_id()).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    keep_ops(&w, 2);
    let error = wallet.app_mut().deny(&[7; 32]).await.unwrap_err();
    assert!(error.kept_batch().is_some(), "{error:?}");
    wallet.receive(None).await.unwrap();
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(!lists.deny.contains(&w.bob_id()));
    wallet.receive(None).await.unwrap();
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(lists.deny.contains(&w.bob_id()));
}

/// A store that fails, once only, the first commit of Paykit's own state (`p/`) that
/// follows the client's first commit of a block the switch applied (`lc/deny/<pubky>`).
#[derive(Default)]
struct FailAfterBlock {
    inner: pubky_switch_client::store::MemoryStore,
    armed: std::sync::atomic::AtomicBool,
    used: std::sync::atomic::AtomicBool,
}

impl pubky_switch_client::store::StateStore for FailAfterBlock {
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
        use pubky_switch_client::store::Write;
        use std::sync::atomic::Ordering;
        let ours = batch.writes.iter().any(|write| {
            matches!(write, Write::Put { key, .. } | Write::Delete { key, .. } if key.starts_with(b"p/"))
        });
        if ours && self.armed.swap(false, Ordering::SeqCst) {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        let blocked = batch
            .writes
            .iter()
            .any(|write| matches!(write, Write::Put { key, .. } if key.starts_with(b"lc/deny/")));
        self.inner.commit(batch)?;
        if blocked && !self.used.swap(true, Ordering::SeqCst) {
            self.armed.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[tokio::test]
async fn test_blocked_block_the_switch_took_counts_though_the_store_then_fails() {
    // The switch applies the wallet's block, and the next commit of Paykit's own state
    // fails. The user's payment to bob, judged by the copy read a moment before, still needs
    // the user's confirmation: the block counted before the switch was asked (Paykit spec
    // 8.1).
    let w = World::new();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            std::sync::Arc::new(FailAfterBlock::default()),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let _ = wallet.block(&w.bob_id()).await;
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(lists.deny.contains(&w.bob_id()));
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(shown(error).payee, w.bob_id());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_blocked_resend_request_to_a_blocked_counterparty_refused_with_nothing_sent() {
    // The host asks bob to re-send while the wallet's copy of the deny list holds him:
    // refused, nothing queued or sent (Paykit spec 8.1, "Blocked counterparties"). Once he
    // is unblocked the same call goes.
    let w = World::new();
    let (mut wallet, _request) = blocked(&w).await;
    let selection = paykit_sdk::ResendSelection::default().with_accepted_from_ms(0);
    let before = w.bob.switch().entries_from(&w.alice_id());
    let refused = wallet
        .request_resend(w.bob_id(), selection.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    wallet.flush().await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), before);
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.request_resend(w.bob_id(), selection).await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), before + 1);
}

#[tokio::test]
async fn test_blocked_resend_request_while_the_deny_list_is_unknown_refused_with_nothing_sent() {
    // The wallet's copy of the lists isn't known: its due read failed, and the read the call
    // tries first fails too. Asking bob to re-send is refused, nothing queued or sent; once
    // the lists read again the same call goes (Paykit spec 8.1, wire 4.1).
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    fail_list_read(&w);
    wallet.receive(None).await.unwrap();
    let selection = paykit_sdk::ResendSelection::default().with_accepted_from_ms(0);
    let before = w.bob.switch().entries_from(&w.alice_id());
    fail_list_read(&w);
    let refused = wallet
        .request_resend(w.bob_id(), selection.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    wallet.flush().await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), before);
    wallet.request_resend(w.bob_id(), selection).await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), before + 1);
}

/// Bob is blocked; the wallet's unblock is then kept unanswered, and its batch dropped: sent
/// again at the next read, the switch refuses it for good, so the list still holds bob.
/// Then the phone, another own app, blocks bob too.
async fn unblock_dropped_then_blocked_by_the_phone(w: &World, wallet: &mut Paykit) {
    wallet.block(&w.bob_id()).await.unwrap();
    keep_ops(w, 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.alice.switch().fail_next(Fault::Error {
        path: "/ops".into(),
        code: ErrorCode::Conflict,
        current: serde_json::Value::Null,
    });
    let _ = wallet.receive(None).await;
    let mut phone = w
        .join_with(
            &w.alice,
            "phone",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    phone.block(&w.bob_id()).await.unwrap();
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(lists.deny.contains(&w.bob_id()));
}

#[tokio::test]
async fn test_blocked_own_unblock_whose_batch_is_dropped_counts_no_more_for_the_user() {
    // The wallet's unblock never landed and its batch is gone: it counts no more, so the
    // user's payment to bob, whom the list holds, needs the confirmation (Paykit spec 8.1).
    let w = World::new();
    let (mut wallet, _, request) = received(&w, PaykitConfig::default()).await;
    unblock_dropped_then_blocked_by_the_phone(&w, &mut wallet).await;
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(shown(error).payee, w.bob_id());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_blocked_own_unblock_whose_batch_is_dropped_counts_no_more_for_a_repeat() {
    // As above, for the resume's repeat of a payment in flight, an automatic payment: none
    // is made to bob.
    let w = World::new();
    let calls = resumed_repeat(&w, async |wallet| {
        unblock_dropped_then_blocked_by_the_phone(&w, wallet).await;
    })
    .await;
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn test_blocked_own_unblock_kept_unanswered_not_restored() {
    // Bob is blocked, and the wallet's unblock is kept unanswered when the backup is taken.
    // The restored app keeps no batch the unblock could count by: it isn't restored, nor
    // written again, so the list still holds bob and the user's payment to him needs the
    // confirmation (Paykit spec 8.1).
    let w = World::new();
    let (mut wallet, _, request) = received(&w, PaykitConfig::default()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    keep_ops(&w, 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    wallet
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"bk/").await.unwrap().is_empty());
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(lists.deny.contains(&w.bob_id()));
    let held = PayOptions::default()
        .with_possibly_cancelled_confirmed(pubky_switch_client::app::UserConfirmed::new());
    let error = wallet.pay_request(&request, held).await.unwrap_err();
    assert_eq!(shown(error).payee, w.bob_id());
}

/// A store whose commit of the client's applied block (`lc/deny/<pubky>`) lands but answers
/// `Unknown`, once.
#[derive(Default)]
struct UnknownBlockCommit {
    inner: pubky_switch_client::store::MemoryStore,
    used: std::sync::atomic::AtomicBool,
}

impl pubky_switch_client::store::StateStore for UnknownBlockCommit {
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
        use pubky_switch_client::store::Write;
        use std::sync::atomic::Ordering;
        let blocked = batch
            .writes
            .iter()
            .any(|write| matches!(write, Write::Put { key, .. } if key.starts_with(b"lc/deny/")));
        self.inner.commit(batch)?;
        if blocked && !self.used.swap(true, Ordering::SeqCst) {
            return Err(pubky_switch_client::error::StoreError::Unknown);
        }
        Ok(())
    }
}

#[tokio::test]
async fn test_blocked_block_applied_whose_answer_is_of_unknown_fate_still_counts() {
    // The switch applies the wallet's block and the client's commit of it lands but answers
    // `Unknown`: the call fails with the store's error, of unknown fate. The block stays
    // counted, so the user's payment to bob needs the confirmation (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            std::sync::Arc::new(UnknownBlockCommit::default()),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.block(&w.bob_id()).await.is_err());
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(lists.deny.contains(&w.bob_id()));
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(shown(error).payee, w.bob_id());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_blocked_own_unblock_applied_in_the_read_keeps_the_resend_request_owed() {
    // Bob's request to re-send is kept, and the wallet's unblock of him kept unanswered. The
    // next read sends the unblock and the switch applies it: the unblock counts no more, so
    // the copy of the lists isn't known until it is read again, and the request waits for
    // that, never dropped; the next receive answers it (Paykit spec 8.1, 9).
    let w = World::new();
    let (mut wallet, shop, request) = received(&w, PaykitConfig::default()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    keep_ops(&w, 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    kept_resend_request(&w, &mut wallet, &shop, &request).await;
    let first = wallet.receive(None).await.unwrap();
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(!lists.deny.contains(&w.bob_id()));
    let left = wallet.app().protocol_scan(b"ans/").await.unwrap().len();
    assert_eq!((first.resends, left), (0, 1));
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    assert!(wallet
        .app()
        .protocol_scan(b"ans/")
        .await
        .unwrap()
        .is_empty());
}

/// A store that, once armed, fails the client's next commit that keeps a batch
/// (`opq/<batch_id>`), once, applying nothing.
#[derive(Default)]
struct FailNextKeep {
    inner: pubky_switch_client::store::MemoryStore,
    armed: std::sync::atomic::AtomicBool,
}

impl pubky_switch_client::store::StateStore for FailNextKeep {
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
        use pubky_switch_client::store::Write;
        use std::sync::atomic::Ordering;
        let keeps = batch
            .writes
            .iter()
            .any(|write| matches!(write, Write::Put { key, .. } if key.starts_with(b"opq/")));
        if keeps && self.armed.swap(false, Ordering::SeqCst) {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        self.inner.commit(batch)
    }
}

#[tokio::test]
async fn test_blocked_block_counted_but_never_kept_written_at_the_next_receive() {
    // The client's commit that keeps the wallet's block fails, so the block is neither sent
    // nor kept, though it counts here. The next receive writes it, and the identity's list
    // holds bob, so the other own apps pause for him too (Paykit spec 8.1).
    let w = World::new();
    let target = std::sync::Arc::new(FailNextKeep::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            std::sync::Arc::clone(&target)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    target
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wallet.block(&w.bob_id()).await.is_err());
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(!lists.deny.contains(&w.bob_id()));
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(shown(error).payee, w.bob_id());
    wallet.receive(None).await.unwrap();
    let lists = wallet.app_mut().allow_list().await.unwrap();
    assert!(lists.deny.contains(&w.bob_id()));
}

/// A store that counts the client's writes that queue a message (`out/`) or keep a batch
/// (`opq/`), and those that queue a message alone.
#[derive(Default)]
struct CountsQueued {
    inner: pubky_switch_client::store::MemoryStore,
    queued: std::sync::atomic::AtomicUsize,
    outbox: std::sync::atomic::AtomicUsize,
}

impl pubky_switch_client::store::StateStore for CountsQueued {
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
        use pubky_switch_client::store::Write;
        let queues = batch
            .writes
            .iter()
            .filter(|write| {
                matches!(write, Write::Put { key, .. }
                    if key.starts_with(b"out/") || key.starts_with(b"opq/"))
            })
            .count();
        let outbox = batch
            .writes
            .iter()
            .filter(|write| matches!(write, Write::Put { key, .. } if key.starts_with(b"out/")))
            .count();
        self.inner.commit(batch)?;
        self.queued
            .fetch_add(queues, std::sync::atomic::Ordering::SeqCst);
        self.outbox
            .fetch_add(outbox, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Why the wallet's sends to bob are held.
#[derive(Clone, Copy, Debug)]
enum Held {
    /// The wallet blocked him.
    Blocked,
    /// The wallet's copy of the deny list isn't known, and a read of it fails.
    Unknown,
}

/// The payer's answer to bob's request.
#[derive(Clone, Copy, Debug)]
enum Answer {
    /// A one-time request rejected.
    Reject,
    /// A one-time request cancelled.
    Cancel,
    /// A recurring request rejected before its acceptance.
    RejectRecurring,
}

impl Answer {
    async fn give(
        self,
        wallet: &mut Paykit,
        request: &RequestRef,
    ) -> Result<String, paykit_sdk::PaykitError> {
        match self {
            Self::Reject | Self::RejectRecurring => wallet.reject_request(request, None).await,
            Self::Cancel => wallet.cancel_request(request, None).await,
        }
    }

    fn status(self) -> RequestStatus {
        match self {
            Self::Cancel => RequestStatus::Cancelled,
            Self::Reject | Self::RejectRecurring => RequestStatus::Rejected,
        }
    }

    /// The refusal a second answer, or a payment, of the request gets once it is answered.
    fn refusal(self) -> Refusal {
        match self {
            Self::Cancel => Refusal::Cancelled,
            Self::Reject | Self::RejectRecurring => Refusal::Rejected,
        }
    }

    /// The request record's member that holds the answer.
    fn member(self) -> &'static str {
        match self {
            Self::Cancel => "cancellation",
            Self::Reject | Self::RejectRecurring => "response",
        }
    }
}

/// What happens to alice's wallet between its held answer and the end of the hold.
#[derive(Clone, Copy, Debug)]
enum Between {
    /// Nothing.
    Nothing,
    /// The wallet is loaded again from its store.
    Restart,
    /// The wallet is restored from a backup taken after the answer, onto a new store.
    Restore,
}

/// The wallet's receive and flush while its sends to bob are still `held`: a read of the
/// lists fails first while the copy isn't known.
async fn receive_held(w: &World, wallet: &mut Paykit, held: Held) {
    if let Held::Unknown = held {
        fail_list_read(w);
    }
    wallet.receive(None).await.unwrap();
    wallet.flush().await.unwrap();
}

/// Bob's request is held by alice's wallet, and its sends to bob are then `held`. The
/// wallet's `answer` is recorded at once, on the request record (Paykit spec 8.1, "Blocked
/// counterparties"): the wallet and alice's phone show it, the phone refuses to pay the
/// request, and a second answer is refused. Nothing is queued, the outbox never holds the
/// answer and nothing reaches bob, by any receive or flush, while the sends stay held, also
/// after `between`. Once bob is
/// unblocked, or the lists read again, the next receive delivers the answer, once.
async fn answer_held(held: Held, answer: Answer, between: Between) {
    let w = World::new();
    let counts = std::sync::Arc::new(CountsQueued::default());
    let base = std::sync::Arc::clone(&counts)
        as std::sync::Arc<dyn pubky_switch_client::store::StateStore>;
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            std::sync::Arc::clone(&base),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = if let Answer::RejectRecurring = answer {
        // Entered at alice's switch directly: bob's admits her answer by an invite.
        shop.app_mut().invite(&w.alice_id()).await.unwrap();
        let expires_at = w.clock.now_ms() + 24 * crate::common::HOUR_MS;
        crate::recurring::monthly(&w, &[wallet.app_id()], expires_at, "plan-1");
        wallet.receive(None).await.unwrap();
        wallet.requests().await.unwrap()[0].request
    } else {
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        request
    };
    match held {
        Held::Blocked => wallet.block(&w.bob_id()).await.unwrap(),
        Held::Unknown => {
            // The receive's due read fails, and so does the read the answer tries first.
            w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
            fail_list_read(&w);
            wallet.receive(None).await.unwrap();
            fail_list_read(&w);
        }
    }
    let record = wallet.request(&request).await.unwrap().record;
    let version = wallet
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .version;
    let queued = counts.outbox.load(std::sync::atomic::Ordering::SeqCst);
    let entries = w.bob.switch().entries_from(&w.alice_id());

    let msg_id = answer.give(&mut wallet, &request).await.unwrap();
    wallet.flush().await.unwrap();
    assert_eq!(
        counts.outbox.load(std::sync::atomic::Ordering::SeqCst),
        queued,
        "{held:?} {answer:?}"
    );
    assert!(wallet.app().delivery(&msg_id).await.unwrap().is_none());
    let after = wallet.app().shared_record(&record).await.unwrap().unwrap();
    assert!(after.version > version, "{held:?} {answer:?}");
    let value: serde_json::Value = serde_json::from_slice(after.content.value().unwrap()).unwrap();
    assert!(
        !value[answer.member()].is_null(),
        "{held:?} {answer:?}: {value}"
    );
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        answer.status()
    );
    // The second answer tries a read of the lists too, while the copy isn't known.
    if let Held::Unknown = held {
        fail_list_read(&w);
    }
    let again = answer.give(&mut wallet, &request).await.unwrap_err();
    assert_eq!(
        again.refusal(),
        Some(&answer.refusal()),
        "{held:?} {answer:?}"
    );
    phone.receive(None).await.unwrap();
    assert_eq!(
        phone.request(&request).await.unwrap().status,
        answer.status(),
        "{held:?} {answer:?}"
    );
    if let Answer::Reject | Answer::Cancel = answer {
        let refused = phone
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            refused.refusal(),
            Some(&answer.refusal()),
            "{held:?} {answer:?}"
        );
    }
    for _ in 0..2 {
        receive_held(&w, &mut wallet, held).await;
    }
    assert_eq!(
        counts.outbox.load(std::sync::atomic::Ordering::SeqCst),
        queued,
        "{held:?} {answer:?}"
    );
    assert!(wallet.app().delivery(&msg_id).await.unwrap().is_none());
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);

    let mut wallet = match between {
        Between::Nothing => wallet,
        Between::Restart => {
            let app_id = wallet.app_id();
            drop(wallet);
            w.load_on_clock(&w.alice, base, app_id, std::sync::Arc::clone(&w.clock))
                .await
        }
        Between::Restore => {
            let backup = wallet.backup().await.unwrap();
            drop(wallet);
            Paykit::restore(
                w.env(&w.alice, store()),
                PaykitConfig::default(),
                &backup,
                std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
                w.timer(),
            )
            .await
            .unwrap()
        }
    };
    for _ in 0..2 {
        receive_held(&w, &mut wallet, held).await;
    }
    assert!(wallet.app().delivery(&msg_id).await.unwrap().is_none());
    assert_eq!(
        w.bob.switch().entries_from(&w.alice_id()),
        entries,
        "{held:?} {answer:?} {between:?}"
    );
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        answer.status()
    );

    if let Held::Blocked = held {
        wallet.unblock(&w.bob_id()).await.unwrap();
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(
        w.bob.switch().entries_from(&w.alice_id()),
        entries + 1,
        "{held:?} {answer:?} {between:?}"
    );
    wallet.receive(None).await.unwrap();
    wallet.flush().await.unwrap();
    assert_eq!(
        w.bob.switch().entries_from(&w.alice_id()),
        entries + 1,
        "{held:?} {answer:?} {between:?}"
    );
    assert!(wallet.notices().await.unwrap().is_empty());
    if let Answer::Reject | Answer::Cancel = answer {
        shop.receive(None).await.unwrap();
        let expected = match answer {
            Answer::Cancel => paykit_sdk::PayeeStatus::Cancelled,
            _ => paykit_sdk::PayeeStatus::Rejected,
        };
        assert_eq!(shop.payee_activity().await.unwrap()[0].status, expected);
    }
}

#[tokio::test]
async fn test_blocked_rejection_to_a_blocked_payee_recorded_and_sent_once_unblocked() {
    answer_held(Held::Blocked, Answer::Reject, Between::Nothing).await;
}

#[tokio::test]
async fn test_blocked_rejection_while_the_deny_list_is_unknown_recorded_and_sent_once_it_reads() {
    answer_held(Held::Unknown, Answer::Reject, Between::Nothing).await;
}

#[tokio::test]
async fn test_blocked_cancellation_to_a_blocked_payee_recorded_and_sent_once_unblocked() {
    answer_held(Held::Blocked, Answer::Cancel, Between::Nothing).await;
}

#[tokio::test]
async fn test_blocked_cancellation_while_the_deny_list_is_unknown_recorded_and_sent_once_it_reads()
{
    answer_held(Held::Unknown, Answer::Cancel, Between::Nothing).await;
}

#[tokio::test]
async fn test_blocked_recurring_rejection_to_a_blocked_payee_recorded_and_sent_once_unblocked() {
    answer_held(Held::Blocked, Answer::RejectRecurring, Between::Nothing).await;
}

#[tokio::test]
async fn test_blocked_recurring_rejection_while_the_deny_list_is_unknown_recorded_and_sent_once_it_reads(
) {
    answer_held(Held::Unknown, Answer::RejectRecurring, Between::Nothing).await;
}

#[tokio::test]
async fn test_blocked_held_rejection_kept_unanswered_resumed_with_nothing_sent() {
    // The held rejection's batch is applied with its answer lost: the client keeps it. The
    // receives that settle it while bob is still blocked send him nothing, as the batch
    // queues nothing; once he is unblocked the rejection goes, once (Paykit spec 8.1,
    // "Blocked counterparties").
    let w = World::new();
    let (mut wallet, mut shop, request) = received(&w, PaykitConfig::default()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    let entries = w.bob.switch().entries_from(&w.alice_id());
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let kept = wallet.reject_request(&request, None).await;
    assert!(kept.is_err(), "{kept:?}");
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
        wallet.flush().await.unwrap();
    }
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Rejected
    );
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries + 1);
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        paykit_sdk::PayeeStatus::Rejected
    );
}

#[tokio::test]
async fn test_blocked_held_rejection_kept_while_its_record_doesnt_read() {
    // The rejection is held while bob is blocked. The phone then writes a value that doesn't
    // read over the request record. Once bob is unblocked the record can't show whether it
    // still holds the rejection, so the held answer stays, nothing is sent and nothing is
    // given up. Once the record reads again the rejection goes, once (Paykit spec 8.1,
    // "Blocked counterparties").
    let w = World::new();
    let store = std::sync::Arc::new(crate::outcome_record::NoSeenMarks::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            std::sync::Arc::clone(&store)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    let entries = w.bob.switch().entries_from(&w.alice_id());
    wallet.reject_request(&request, None).await.unwrap();
    phone.receive(None).await.unwrap();
    let target = crate::outcome_record::payer_record(&request);
    let key = phone.app().record_key(&target).unwrap();
    let good = phone
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    store.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    crate::outcome_record::phone_writes(&mut phone, &request, Some(crate::outcome_record::INVALID))
        .await;
    wallet.receive(None).await.unwrap();
    wallet.unblock(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
        wallet.flush().await.unwrap();
    }
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(held.content.value(), Some(crate::outcome_record::INVALID));
    assert_eq!(wallet.app().protocol_scan(b"ha/").await.unwrap().len(), 1);
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
    assert!(wallet.notices().await.unwrap().is_empty());
    phone.receive(None).await.unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&good)).await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries + 1);
    assert!(wallet.app().protocol_scan(b"ha/").await.unwrap().is_empty());
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        paykit_sdk::PayeeStatus::Rejected
    );
}

#[tokio::test]
async fn test_blocked_held_rejection_past_its_window_given_up_with_a_notice() {
    // Bob stays blocked for more than the rejection's replay window of 30 days: once he is
    // unblocked the client refuses to queue it, so it never goes, and a notice tells the
    // host. The request record still holds it, so no own app pays the request (Paykit spec
    // 8.1, "Blocked counterparties"; Switch spec 11).
    let w = World::new();
    let (mut wallet, _shop, request) = received(&w, PaykitConfig::default()).await;
    let mut phone = w.wallet("phone").await;
    wallet.block(&w.bob_id()).await.unwrap();
    let entries = w.bob.switch().entries_from(&w.alice_id());
    let msg_id = wallet.reject_request(&request, None).await.unwrap();
    w.clock.advance_ms(31 * 24 * crate::common::HOUR_MS);
    wallet.receive(None).await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.flush().await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, w.bob_id());
    assert_eq!(notices[0].msg_id.as_deref(), Some(msg_id.as_str()));
    assert_eq!(notices[0].request_hash, Some(request.request_hash));
    // Raised once: a later receive neither raises it again nor sends anything.
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.notices().await.unwrap().len(), 1);
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
    let record = wallet.request(&request).await.unwrap().record;
    let held = wallet.app().shared_record(&record).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(!value["response"].is_null(), "{value}");
    phone.receive(None).await.unwrap();
    assert!(phone
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_blocked_held_rejection_sent_once_after_a_restart() {
    answer_held(Held::Blocked, Answer::Reject, Between::Restart).await;
}

#[tokio::test]
async fn test_blocked_held_cancellation_sent_once_after_a_restart_while_the_deny_list_is_unknown() {
    answer_held(Held::Unknown, Answer::Cancel, Between::Restart).await;
}

#[tokio::test]
async fn test_blocked_held_rejection_sent_once_after_a_restore() {
    answer_held(Held::Blocked, Answer::Reject, Between::Restore).await;
}

#[tokio::test]
async fn test_blocked_held_cancellation_sent_once_after_a_restore_while_the_deny_list_is_unknown() {
    answer_held(Held::Unknown, Answer::Cancel, Between::Restore).await;
}

/// Bob's request received at alice's wallet, and his list, then bob blocked: the user's
/// payment of the request and a payment without a request are each refused as blocked,
/// showing what the user confirms. The wallet's copy of the lists then becomes unknown: the
/// due read at a receive fails. Returns the wallet and the two payments shown.
async fn blocked_then_unknown(
    w: &World,
) -> (
    Paykit,
    RequestRef,
    paykit_sdk::BlockedPayment,
    paykit_sdk::BlockedPayment,
) {
    let (mut wallet, mut shop, request) = received(w, PaykitConfig::default()).await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    let paid = shown(
        wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err(),
    );
    let unrequested = shown(
        wallet
            .pay_without_request(unrequested_payment(w, PayOptions::default()))
            .await
            .unwrap_err(),
    );
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    fail_list_read(w);
    wallet.receive(None).await.unwrap();
    (wallet, request, paid, unrequested)
}

/// A payment without a request to bob's list endpoint, with `options`.
fn unrequested_payment(w: &World, options: PayOptions) -> UnrequestedPayment {
    UnrequestedPayment::new(w.bob_id(), btc("0.001"))
        .with_options(options.with_endpoint(endpoint("btc-lightning-bolt12")))
}

#[tokio::test]
async fn test_blocked_confirmed_payment_while_the_deny_list_is_unknown_refused() {
    // The user confirms paying bob's request, blocked, but the wallet's copy of the lists
    // isn't known and the read the payment makes first fails: the confirmation doesn't stand
    // for a read, so nothing is paid and nothing, acceptance or proof, reaches bob (Paykit
    // spec 8.1, "How fresh the deny list is").
    let w = World::new();
    let (mut wallet, request, paid, _) = blocked_then_unknown(&w).await;
    let entries = w.bob.switch().entries_from(&w.alice_id());
    fail_list_read(&w);
    let refused = wallet
        .pay_request(&request, PayOptions::default().with_blocked_confirmed(paid))
        .await;
    assert!(refused.is_err(), "{refused:?}");
    wallet.flush().await.unwrap();
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
}

#[tokio::test]
async fn test_blocked_confirmed_payment_without_a_request_while_the_deny_list_is_unknown_refused() {
    // As above, for a payment without a request: nothing is paid, and no proof or list
    // request reaches bob.
    let w = World::new();
    let (mut wallet, _, _, unrequested) = blocked_then_unknown(&w).await;
    let entries = w.bob.switch().entries_from(&w.alice_id());
    fail_list_read(&w);
    let refused = wallet
        .pay_without_request(unrequested_payment(
            &w,
            PayOptions::default().with_blocked_confirmed(unrequested),
        ))
        .await;
    assert!(refused.is_err(), "{refused:?}");
    wallet.flush().await.unwrap();
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
}

/// Alice's wallet holds bob's request but no list of his, and its sends to bob are then
/// `held`: a payment without a request to him is refused before a list is asked for, and
/// nothing reaches bob. Once bob is unblocked, or the lists read, the same payment asks
/// him for his list.
async fn no_list_asked_while_held(held: Held) {
    let w = World::new();
    let (mut wallet, _, _) = received(&w, PaykitConfig::default()).await;
    match held {
        Held::Blocked => wallet.block(&w.bob_id()).await.unwrap(),
        Held::Unknown => {
            w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
            fail_list_read(&w);
            wallet.receive(None).await.unwrap();
            fail_list_read(&w);
        }
    }
    let entries = w.bob.switch().entries_from(&w.alice_id());
    let refused = wallet
        .pay_without_request(unrequested_payment(&w, PayOptions::default()))
        .await
        .unwrap_err();
    match held {
        Held::Blocked => assert!(
            matches!(refused.refusal(), Some(Refusal::Blocked { .. })),
            "{refused:?}"
        ),
        Held::Unknown => assert!(
            !matches!(refused.refusal(), Some(Refusal::WaitingForList)),
            "{refused:?}"
        ),
    }
    wallet.flush().await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
    if let Held::Blocked = held {
        wallet.unblock(&w.bob_id()).await.unwrap();
    }
    let waits = wallet
        .pay_without_request(unrequested_payment(&w, PayOptions::default()))
        .await
        .unwrap_err();
    assert!(
        matches!(waits.refusal(), Some(Refusal::WaitingForList)),
        "{waits:?}"
    );
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries + 1);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_blocked_list_request_to_a_blocked_payee_not_sent_unconfirmed() {
    no_list_asked_while_held(Held::Blocked).await;
}

#[tokio::test]
async fn test_blocked_list_request_while_the_deny_list_is_unknown_not_sent() {
    no_list_asked_while_held(Held::Unknown).await;
}

/// Bob's till, holding `request` and `manage_contacts`, on `store`, with its copy of the
/// lists read.
async fn contacts_till(
    w: &World,
    store: std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
) -> Paykit {
    let mut till = w
        .join_with(
            &w.bob,
            "till",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            store,
        )
        .await;
    till.receive(None).await.unwrap();
    till
}

/// The next read of bob's lists fails.
fn fail_bobs_list_read(w: &World) {
    w.bob.switch().fail_next(Fault::Error {
        path: "/allow".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
}

/// Bob's till holds `contacts`, and its sends to alice are then `held`: a new request to
/// her is refused, nothing recorded, queued or reaching her (Paykit spec 8.1, "Blocked
/// counterparties"). Once alice is unblocked, or the lists read, the same request goes.
async fn request_held(held: Held) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let counts = std::sync::Arc::new(CountsQueued::default());
    let mut till = contacts_till(
        &w,
        std::sync::Arc::clone(&counts)
            as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
    )
    .await;
    match held {
        Held::Blocked => till.block(&w.alice_id()).await.unwrap(),
        Held::Unknown => {
            // The receive's due read fails, and so does the read the request tries first.
            w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
            fail_bobs_list_read(&w);
            till.receive(None).await.unwrap();
            fail_bobs_list_read(&w);
        }
    }
    let queued = counts.queued.load(std::sync::atomic::Ordering::SeqCst);
    let entries = w.alice.switch().entries_from(&w.bob_id());

    let refused = till
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{held:?}: {refused:?}"
    );
    till.flush().await.unwrap();
    assert_eq!(
        counts.queued.load(std::sync::atomic::Ordering::SeqCst),
        queued,
        "{held:?}"
    );
    assert!(till.payee_activity().await.unwrap().is_empty(), "{held:?}");
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), entries);

    if let Held::Blocked = held {
        till.unblock(&w.alice_id()).await.unwrap();
    }
    let request = request_from(&mut till, w.alice_id(), draft("1", w.clock.now_ms())).await;
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), entries + 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_blocked_request_to_a_blocked_payer_refused_with_nothing_sent() {
    request_held(Held::Blocked).await;
}

#[tokio::test]
async fn test_blocked_request_while_the_deny_list_is_unknown_refused_with_nothing_sent() {
    request_held(Held::Unknown).await;
}

#[tokio::test]
async fn test_blocked_request_from_an_app_without_contacts_sent_while_the_lists_dont_read() {
    // Bob's shop holds no `contacts`, so it reads no lists and holds nothing: a read of them
    // that would fail decides nothing, and its request goes.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    fail_bobs_list_read(&w);
    let entries = w.alice.switch().entries_from(&w.bob_id());
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), entries + 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_blocked_payees_cancellation_to_a_blocked_payer_delivered() {
    // A payee's cancellation stops a payment: bob's till, holding `contacts`, cancels its
    // request to alice after blocking her, and the cancellation reaches her wallet.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut till = contacts_till(&w, store()).await;
    let request = request_from(&mut till, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    till.block(&w.alice_id()).await.unwrap();
    let record = till.payee_activity().await.unwrap()[0].record;
    till.cancel_sent_request(&record, None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
}

#[tokio::test]
async fn test_blocked_receipt_to_a_blocked_payer_delivered() {
    // A receipt settles a payment already made: bob's till, holding `contacts`, confirms
    // alice's payment after blocking her, and the receipt reaches her wallet's record.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut till = contacts_till(&w, store()).await;
    let request = request_from(&mut till, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    till.receive(None).await.unwrap();
    till.block(&w.alice_id()).await.unwrap();
    let record = till.payee_activity().await.unwrap()[0].record;
    assert!(till.confirm_payment(&record).await.unwrap().is_some());
    wallet.receive(None).await.unwrap();
    let key = wallet.request(&request).await.unwrap().record;
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(!value["receipt"].is_null(), "{value}");
}
