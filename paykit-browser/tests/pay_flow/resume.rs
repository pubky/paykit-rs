//! Resuming a payment left in progress (Paykit spec 7.2, "What a payment method declares",
//! "No second execution"): one the rail settles late, and one whose app is loaded again with
//! no answer from the rail, which an idempotent rail is asked again.

use std::sync::Arc;

use paykit_sdk::rail::{Behaviour, FAKE_LONGEST_COMPLETION_MS};
use paykit_sdk::{PayOptions, PayOutcome, PaykitConfig, RailDeclaration, RequestStatus};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{MemoryStore, StateStore};
use wasm_bindgen_test::wasm_bindgen_test;

use crate::world::{draft, send, World, DAY_MS, HOUR_MS};

#[wasm_bindgen_test]
async fn test_pay_flow_payment_settled_late_resumed_once() {
    let w = World::new();
    let mut wallet = w.wallet().await;
    let mut shop = w.shop().await;
    let request = send(
        &mut shop,
        w.payer_id(),
        draft("1", w.clock.now_ms() + DAY_MS),
    )
    .await;
    wallet.receive(None).await.unwrap();

    // The rail leaves the payment in flight.
    w.rail.next(Behaviour::Hang);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::BeingPaidHere
    );

    // It settles late; resuming reports it, once.
    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
}

#[wasm_bindgen_test]
async fn test_pay_flow_payment_in_flight_asked_again_after_a_reload() {
    let w = World::new();
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new());
    // An orphan window of a day, so a repeat within the hour can still complete in it.
    let config = PaykitConfig::default().with_orphan_after_s(DAY_MS / 1000);
    let (attempt_id, app_id) = {
        let mut wallet = w.wallet_with(Arc::clone(&store), config.clone()).await;
        let mut shop = w.shop().await;
        let request = send(
            &mut shop,
            w.payer_id(),
            draft("1", w.clock.now_ms() + DAY_MS),
        )
        .await;
        wallet.receive(None).await.unwrap();
        w.rail.next(Behaviour::Hang);
        let outcome = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap();
        let PayOutcome::Pending { attempt_id, .. } = outcome else {
            panic!("{outcome:?}");
        };
        (attempt_id, wallet.app_id())
    };

    // The rail is idempotent for a day and keeps no history, so it can't say whether the
    // payment went through. Within that day, the wallet, loaded again from its store, reads
    // the claim, finds its attempt still open, and asks the rail again under the same
    // idempotency key, which moves no money a second time.
    w.rail.set_declaration(
        RailDeclaration::new(FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true)
            .with_idempotency_retention_ms(DAY_MS),
    );
    w.clock.advance_ms(HOUR_MS);
    let mut wallet = w.reload_wallet(store, app_id, config).await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Pending { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 2);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}
