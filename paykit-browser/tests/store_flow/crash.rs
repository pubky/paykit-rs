//! A wallet that stops after each commit of a payment (Paykit spec 7.2, Switch spec 8.2), on
//! the durable store: the Switch client's `FaultStore` wraps an [`OpfsStore`] and stops the
//! app after its `n`th commit, as a worker that died at once; the store is closed, opened
//! again from disk, and the wallet loaded from it resumes. It mirrors
//! `test_crash_after_every_commit_pays_once` in `paykit-sdk/tests/flows/crash.rs`, with the
//! same outcome: the request is paid once, its claim is settled with one attempt done, and
//! the payee has the Payment Proof.
//!
//! As in `resume.rs`, the store is opened again in the same worker, since the world lives in
//! WebAssembly memory and would not survive a page reload.
//!
//! [`OpfsStore`]: paykit_browser::store::OpfsStore

use std::sync::Arc;

use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, PaykitConfig, RequestStatus, Stage};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{Fault, FaultStore, StateStore};
use pubky_switch_core::claims::{ClaimStatus, Outcome};
use wasm_bindgen_test::wasm_bindgen_test;

use crate::opfs::{fresh_name, remove_store};
use crate::support::open_store;
use crate::world::{draft, send, World, DAY_MS};

/// Pay with the wallet's store stopping at `fault`, open the store again, reload the wallet
/// and resume. Returns whether the fault was hit.
async fn crash_and_resume(fault: Fault) -> bool {
    let w = World::new();
    let name = fresh_name("flow");
    let store = open_store(&name).await;
    let mut shop = w.shop().await;
    let (request, wallet_id) = {
        let mut wallet = w
            .wallet_with(Arc::clone(&store), PaykitConfig::default())
            .await;
        let request = send(
            &mut shop,
            w.payer_id(),
            draft("1", w.clock.now_ms() + DAY_MS),
        )
        .await;
        wallet.receive(None).await.unwrap();
        (request, wallet.app_id())
    };

    let faulty = Arc::new(FaultStore::new(store, fault));
    let (first, hit) = {
        let mut crashing = w
            .reload_wallet(
                Arc::clone(&faulty) as Arc<dyn StateStore>,
                wallet_id,
                PaykitConfig::default(),
            )
            .await;
        let first = crashing
            .pay_request(&request, PayOptions::default())
            .await
            .outcome;
        (first, faulty.stopped())
    };
    // The fault store held the last handle on the store: dropping it closes the store's files,
    // as the worker's end would.
    drop(faulty);
    if !hit {
        assert!(
            matches!(first, Ok(PayOutcome::Paid { .. })),
            "{fault:?}: {first:?}"
        );
    }

    // Open the store again from what was committed, which succeeds only because its files
    // were closed, then resume, and pay if nothing did.
    let store = open_store(&name).await;
    {
        let mut wallet = w
            .reload_wallet(Arc::clone(&store), wallet_id, PaykitConfig::default())
            .await;
        wallet.resume_payments().await.unwrap();
        wallet.receive(None).await.unwrap();
        if wallet.request(&request).await.unwrap().status != RequestStatus::Paid {
            let again = wallet
                .pay_request(&request, PayOptions::default())
                .await
                .outcome;
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
            .expect("the payment's claim");
        assert_eq!(claim.view.status, ClaimStatus::Settled, "{fault:?}");
        let done = claim
            .view
            .attempts
            .iter()
            .filter(|attempt| attempt.outcome == Outcome::Done)
            .count();
        assert_eq!(done, 1, "{fault:?}: {:?}", claim.view.attempts);
        assert!(
            wallet
                .payments()
                .await
                .unwrap()
                .iter()
                .all(|payment| !matches!(payment.stage, Stage::Paying | Stage::RailPaid)),
            "{fault:?}"
        );
    }
    drop(store);

    // The Payment Proof went with `done`: the payee has it.
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived,
        "{fault:?}"
    );
    remove_store(&name).await;
    hit
}

#[wasm_bindgen_test]
async fn test_store_flow_crash_after_every_commit_pays_once() {
    let mut n = 1;
    while crash_and_resume(Fault::StopAfter(n)).await {
        n += 1;
        assert!(n < 64, "the payment never finished");
    }
    assert!(n > 3, "the payment commits at several points: {n}");
}
