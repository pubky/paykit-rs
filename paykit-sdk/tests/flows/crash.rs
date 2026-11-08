//! A crash at every commit point of paying (Paykit spec 7.2): the app stops before or after
//! each commit, is reloaded from what was committed, and resumes. The rail sees each
//! idempotency key once, the request is paid once, and its claim is settled once.

use std::sync::Arc;

use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaykitConfig, Rail, RequestStatus};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{Fault, FaultStore, StateStore};
use pubky_switch_core::claims::{ClaimStatus, Outcome};

use crate::common::{draft, request_from, store, World};

/// Pay with the wallet's store stopping at `fault`, reload and resume. Returns whether the
/// fault was hit.
async fn crash_and_resume(fault: Fault) -> bool {
    let w = World::new();
    let base = store();
    let wallet = w
        .join_with(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&base),
        )
        .await;
    let wallet_id = wallet.app_id();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    drop(wallet);
    let rail = Arc::clone(&w.rail) as Arc<dyn Rail>;
    let mut wallet = Paykit::load(
        w.env(&w.alice, Arc::clone(&base)),
        PaykitConfig::default(),
        wallet_id,
        Arc::clone(&rail),
    )
    .await
    .unwrap();
    wallet.receive(None).await.unwrap();
    drop(wallet);

    let faulty = Arc::new(FaultStore::new(Arc::clone(&base), fault));
    let mut crashing = Paykit::load(
        w.env(&w.alice, Arc::clone(&faulty) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        wallet_id,
        Arc::clone(&rail),
    )
    .await
    .unwrap();
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
    let mut wallet = Paykit::load(
        w.env(&w.alice, Arc::clone(&base)),
        PaykitConfig::default(),
        wallet_id,
        rail,
    )
    .await
    .unwrap();
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
