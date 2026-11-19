//! Resuming a payment left in flight from the durable store (Paykit spec 7.2, "What a payment
//! method declares", "No second execution"): the test of the same name in
//! `tests/pay_flow/resume.rs`, with the wallet on an [`OpfsStore`] that is closed and opened
//! again between the payment and the resume.
//!
//! The store is opened again in the same worker, not after a real page reload. The scripted
//! switches, the Homeserver and the rail of the world (`tests/world/`) live in WebAssembly
//! memory, so a reload would lose them, and the resumed payment would have no claim to read
//! and no rail to ask. What a reload adds for the store, that a new WebAssembly instance finds
//! the last commit on disk, is checked by `tests/store_reload/`.
//!
//! [`OpfsStore`]: paykit_browser::store::OpfsStore

use std::sync::Arc;

use paykit_sdk::rail::{Behaviour, FAKE_LONGEST_COMPLETION_MS};
use paykit_sdk::{PayOptions, PayOutcome, PaykitConfig, RailDeclaration};
use pubky_switch_client::clock::Clock;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::opfs::{fresh_name, remove_store};
use crate::support::{assert_held_open, open_store};
use crate::world::{draft, send, World, DAY_MS, HOUR_MS};

#[wasm_bindgen_test]
async fn test_store_flow_payment_in_flight_asked_again_after_the_store_reopens() {
    let w = World::new();
    let name = fresh_name("flow");
    let store = open_store(&name).await;
    assert_held_open(&name).await;
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
    // The wallet is gone with the block above; dropping the last handle closes the store's
    // files. What the wallet knows of the payment is now only on disk.
    drop(store);

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
    // This open succeeds only because the first store closed its files.
    let store = open_store(&name).await;
    {
        let mut wallet = w.reload_wallet(Arc::clone(&store), app_id, config).await;
        let resumed = wallet.resume_payments().await.unwrap();
        assert!(
            matches!(resumed.as_slice(), [PayOutcome::Pending { .. }]),
            "{resumed:?}"
        );
        assert_eq!(resumed[0].attempt_id(), attempt_id);
        assert_eq!(w.rail.pay_calls(&attempt_id), 2);
        assert_eq!(w.rail.executions(&attempt_id), 1);
    }
    drop(store);
    remove_store(&name).await;
}
