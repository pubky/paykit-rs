//! A crash at every commit point (Paykit spec 7.2, Switch spec 8.2): the app stops before or
//! after each commit, is reloaded from what was committed, and resumes.
//!
//! - **Paying:** the rail sees each idempotency key once, the request is paid once, and its
//!   claim is settled once.
//! - **Receiving:** a received message's record write and the cursor move go in one batch,
//!   so the effect is applied once, with no second record write; a receipt settles its
//!   claim once, in that batch.
//! - **Sending:** a request and its record, and a rejection and its record, are written in
//!   one batch: the payee never holds a record of a request the payer never gets, and the
//!   payer never sends a rejection it didn't record, or records one it didn't send.

use std::sync::Arc;

use paykit_lib::bodies::PaymentReceipt;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, Rail, RequestStatus,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{Fault, FaultStore, StateStore};
use pubky_switch_client::testing::TestSigner;
use pubky_switch_core::claims::{Basis, ClaimStatus, EntryKind, Outcome};
use pubky_switch_core::ops::OpKind;

use crate::common::{draft, request_from, store, World};

/// Load app `app_id` of `signer` from `store`.
async fn load(
    w: &World,
    signer: &TestSigner,
    store: Arc<dyn StateStore>,
    app_id: [u8; 16],
) -> Paykit {
    Paykit::load(
        w.env(signer, store),
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

/// Run `fault` at every commit point, stopping after and before each, until the call under
/// test finishes without reaching it. `run` returns whether the fault was hit.
async fn every_commit<F, Fut>(run: F) -> u64
where
    F: Fn(Fault) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut points = 0;
    for stop in [Fault::StopAfter as fn(u64) -> Fault, Fault::StopBefore] {
        let mut n = 1;
        while run(stop(n)).await {
            n += 1;
            assert!(n < 64, "the call never finished");
        }
        points = points.max(n);
    }
    points
}

/// Pay with the wallet's store stopping at `fault`, reload and resume. Returns whether the
/// fault was hit.
async fn crash_and_resume(fault: Fault) -> bool {
    let w = World::new();
    let base = store();
    let wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let wallet_id = wallet.app_id();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    drop(wallet);
    let mut wallet = load(&w, &w.alice, Arc::clone(&base), wallet_id).await;
    wallet.receive(None).await.unwrap();
    drop(wallet);

    let faulty = Arc::new(FaultStore::new(Arc::clone(&base), fault));
    let mut crashing = load(
        &w,
        &w.alice,
        Arc::clone(&faulty) as Arc<dyn StateStore>,
        wallet_id,
    )
    .await;
    let first = crashing.pay_request(&request, PayOptions::default()).await;
    let hit = faulty.stopped();
    drop(crashing);
    if !hit {
        assert!(
            matches!(first, Ok(PayOutcome::Paid { .. })),
            "{fault:?}: {first:?}"
        );
    }

    // Reopen from what was committed, resume, and pay if nothing did.
    let mut wallet = load(&w, &w.alice, Arc::clone(&base), wallet_id).await;
    wallet.resume_payments().await.unwrap();
    wallet.receive(None).await.unwrap();
    if wallet.request(&request).await.unwrap().status != RequestStatus::Paid {
        let again = wallet.pay_request(&request, PayOptions::default()).await;
        assert!(
            matches!(again, Ok(PayOutcome::Paid { .. })),
            "{fault:?}: {again:?}"
        );
    }

    // Paid once: one key with money moved, once; one attempt done on a SETTLED claim.
    let executions = w.rail.all_executions();
    assert_eq!(executions.len(), 1, "{fault:?}: {executions:?}");
    assert!(
        executions.values().all(|n| *n == 1),
        "{fault:?}: {executions:?}"
    );
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Settled, "{fault:?}");
    let done = claim
        .view
        .attempts
        .iter()
        .filter(|a| a.outcome == Outcome::Done)
        .count();
    assert_eq!(done, 1, "{fault:?}: {:?}", claim.view.attempts);
    assert!(
        wallet.payments().await.unwrap().iter().all(|p| !matches!(
            p.stage,
            paykit_sdk::Stage::Paying | paykit_sdk::Stage::RailPaid
        )),
        "{fault:?}"
    );
    // The proof went with `done`: the payee has it.
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived,
        "{fault:?}"
    );
    hit
}

#[tokio::test]
async fn test_crash_after_every_commit_pays_once() {
    let mut n = 1;
    while crash_and_resume(Fault::StopAfter(n)).await {
        n += 1;
        assert!(n < 64, "the payment never finished");
    }
    assert!(n > 3, "the payment commits at several points: {n}");
}

#[tokio::test]
async fn test_crash_before_every_commit_pays_once() {
    let mut n = 1;
    while crash_and_resume(Fault::StopBefore(n)).await {
        n += 1;
        assert!(n < 64, "the payment never finished");
    }
    assert!(n > 3, "the payment commits at several points: {n}");
}

/// The wallet receives bob's request with its store stopping at `fault`, is reloaded and
/// reads again: the request record is written once, with the cursor move past it.
async fn crash_receiving_a_request(fault: Fault) -> bool {
    let w = World::new();
    let base = store();
    let wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let wallet_id = wallet.app_id();
    drop(wallet);
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;

    let faulty = Arc::new(FaultStore::new(Arc::clone(&base), fault));
    let mut crashing = load(
        &w,
        &w.alice,
        Arc::clone(&faulty) as Arc<dyn StateStore>,
        wallet_id,
    )
    .await;
    let first = crashing.receive(None).await;
    let hit = faulty.stopped();
    drop(crashing);
    if !hit {
        assert_eq!(first.unwrap().applied.len(), 1, "{fault:?}");
    }

    let mut wallet = load(&w, &w.alice, Arc::clone(&base), wallet_id).await;
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Open, "{fault:?}");
    assert_eq!(w.record_writes(&w.alice, &view.record), 1, "{fault:?}");
    // The cursor is past the request: reading again applies nothing and writes nothing.
    let again = wallet.receive(None).await.unwrap();
    assert!(again.applied.is_empty(), "{fault:?}: {again:?}");
    assert_eq!(w.record_writes(&w.alice, &view.record), 1, "{fault:?}");
    hit
}

#[tokio::test]
async fn test_crash_receiving_applies_the_effect_with_the_cursor_once() {
    let points = every_commit(crash_receiving_a_request).await;
    assert!(points > 1, "receiving commits at several points: {points}");
}

/// The wallet's payment is in flight when bob's receipt for it arrives; the wallet receives
/// the receipt with its store stopping at `fault`, is reloaded and reads again. The receipt
/// settles the claim by evidence once, in the batch that moves the cursor past it, and the
/// payment, once the rail reports it, is paid once.
async fn crash_receiving_a_receipt(fault: Fault) -> bool {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let wallet_id = wallet.app_id();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    drop(wallet);
    // Only the payee's yes counts (Paykit spec 7.3): its receipt names the attempt.
    let body = PaymentReceipt {
        attempt_id,
        billing_period: None,
    }
    .to_body(2)
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet_id],
        "paykit.payment_receipt",
        vec![[7; 32], request.request_hash],
        body,
    );

    let faulty = Arc::new(FaultStore::new(Arc::clone(&base), fault));
    let mut crashing = load(
        &w,
        &w.alice,
        Arc::clone(&faulty) as Arc<dyn StateStore>,
        wallet_id,
    )
    .await;
    let first = crashing.receive(None).await;
    let hit = faulty.stopped();
    drop(crashing);
    if !hit {
        first.unwrap();
    }

    let mut wallet = load(&w, &w.alice, Arc::clone(&base), wallet_id).await;
    wallet.receive(None).await.unwrap();
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.view.status, ClaimStatus::Settled, "{fault:?}");
    // One `done` by evidence reached the switch, whatever the crash.
    let evidence = w
        .alice
        .switch()
        .applied()
        .iter()
        .filter_map(|op| match &op.body().kind {
            OpKind::Claim(fields) => fields.check().ok(),
            _ => None,
        })
        .filter(|entry| {
            matches!(
                entry.kind,
                EntryKind::Done {
                    basis: Basis::Evidence,
                    ..
                }
            )
        })
        .count();
    assert_eq!(evidence, 1, "{fault:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid,
        "{fault:?}"
    );

    // The rail reports the payment: it is reported once, and nothing is paid again.
    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{fault:?}: {resumed:?}"
    );
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    let done = claim
        .view
        .attempts
        .iter()
        .filter(|a| a.outcome == Outcome::Done)
        .count();
    assert_eq!(done, 1, "{fault:?}");
    assert_eq!(w.rail.executions(&attempt_id), 1, "{fault:?}");
    assert_eq!(w.rail.all_executions().len(), 1, "{fault:?}");
    hit
}

#[tokio::test]
async fn test_crash_receiving_a_receipt_settles_its_claim_once() {
    let points = every_commit(crash_receiving_a_receipt).await;
    assert!(points > 1, "receiving commits at several points: {points}");
}

/// Bob's shop sends a request with its store stopping at `fault`, is reloaded and flushes.
/// The request and its record were written together: either both or neither.
async fn crash_sending_a_request(fault: Fault) -> bool {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let base = store();
    let shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&base),
        )
        .await;
    let shop_id = shop.app_id();
    drop(shop);

    let faulty = Arc::new(FaultStore::new(Arc::clone(&base), fault));
    let mut crashing = load(
        &w,
        &w.bob,
        Arc::clone(&faulty) as Arc<dyn StateStore>,
        shop_id,
    )
    .await;
    let first = crashing
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await;
    let hit = faulty.stopped();
    drop(crashing);
    if !hit {
        assert!(first.is_ok(), "{fault:?}: {first:?}");
    }

    let mut shop = load(&w, &w.bob, Arc::clone(&base), shop_id).await;
    shop.flush().await.unwrap();
    if shop.payee_activity().await.unwrap().is_empty() {
        // Nothing was written: the request never went out, and is sent again.
        wallet.receive(None).await.unwrap();
        assert!(wallet.requests().await.unwrap().is_empty(), "{fault:?}");
        shop.send_request(w.alice_id(), draft("1", w.clock.now_ms()))
            .await
            .unwrap();
    }
    wallet.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{fault:?}");
    assert_eq!(w.record_writes(&w.bob, &sales[0].record), 1, "{fault:?}");
    let requests = wallet.requests().await.unwrap();
    assert_eq!(requests.len(), 1, "{fault:?}");
    assert_eq!(
        Some(requests[0].request.request_hash),
        sales[0].request_hash
    );
    assert_eq!(w.alice.switch().entries_from(&w.bob_id()), 1, "{fault:?}");
    hit
}

#[tokio::test]
async fn test_crash_sending_a_request_keeps_it_and_its_record_together() {
    let points = every_commit(crash_sending_a_request).await;
    assert!(points > 1, "sending commits at several points: {points}");
}

/// The wallet rejects bob's request with its store stopping at `fault`, is reloaded and
/// flushes. The rejection and its record were written together: either both or neither.
async fn crash_rejecting(fault: Fault) -> bool {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let wallet_id = wallet.app_id();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    drop(wallet);

    let faulty = Arc::new(FaultStore::new(Arc::clone(&base), fault));
    let mut crashing = load(
        &w,
        &w.alice,
        Arc::clone(&faulty) as Arc<dyn StateStore>,
        wallet_id,
    )
    .await;
    let first = crashing.reject_request(&request, None).await;
    let hit = faulty.stopped();
    drop(crashing);
    if !hit {
        assert!(first.is_ok(), "{fault:?}: {first:?}");
    }

    let mut wallet = load(&w, &w.alice, Arc::clone(&base), wallet_id).await;
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    if wallet.request(&request).await.unwrap().status != RequestStatus::Rejected {
        // Nothing was written: no rejection went out either, and the user rejects again.
        assert_eq!(w.bob.switch().entries_from(&w.alice_id()), 0, "{fault:?}");
        wallet.reject_request(&request, None).await.unwrap();
        shop.receive(None).await.unwrap();
    }
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Rejected,
        "{fault:?}"
    );
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), 1, "{fault:?}");
    let record = wallet.request(&request).await.unwrap().record;
    // The request record by the receive step, then the rejection: two writes.
    assert_eq!(w.record_writes(&w.alice, &record), 2, "{fault:?}");
    hit
}

#[tokio::test]
async fn test_crash_rejecting_keeps_the_rejection_and_its_record_together() {
    let points = every_commit(crash_rejecting).await;
    assert!(points > 1, "rejecting commits at several points: {points}");
}
