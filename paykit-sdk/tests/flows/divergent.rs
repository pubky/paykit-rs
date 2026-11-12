//! Divergent records (Paykit spec 6, "Divergent records"; 8.1): after a provider loss, a
//! record whose upload conflicts with the current write shows `divergent` until a newer write
//! clears it. The app pauses it once it has seen it so, until its user confirms.
//!
//! The view's flag is the client's durable mark of the latest view it read (Switch wire
//! 7.4.9, `rcdiv/<key>` in its namespace, `SharedRecord::divergent`): these flows set it
//! there, as a divergent view leaves it, and clear it as a view that isn't divergent does. The
//! client itself clears it on a newer write it takes, its own or one from the feed.

use std::sync::Arc;

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    AppScopes, NoticeKind, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, Rail,
    RailDeclaration, Refusal, RequestRef, RequestStatus, UnrequestedPayment,
};
use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget, UserConfirmed};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{StateStore, WriteBatch};
use pubky_switch_core::claims::ClaimStatus;

use crate::common::{btc, draft, endpoint, request_from, store, World, HOUR_MS};

/// Set (`on`) or clear the client's flag that the latest view of `record` shows it divergent,
/// in the namespace of `app` on `store`.
fn set_divergent(store: &Arc<dyn StateStore>, app: &Paykit, record: &[u8; 32], on: bool) {
    let ns = pubky_switch_client::store::client_namespace(&app.app_id());
    let key = format!("rcdiv/{}", pubky_switch_core::enc::b64(record));
    let mut batch = WriteBatch::new();
    if on {
        batch
            .put(&ns, key.into_bytes(), br#"{"v":3,"d":null}"#.to_vec())
            .unwrap();
    } else {
        batch.delete(&ns, key.into_bytes()).unwrap();
    }
    store.commit(batch).unwrap();
}

/// The divergent records a refusal names, as `(record, paused, divergent)`.
fn refused_records(error: &paykit_sdk::PaykitError) -> Vec<([u8; 32], bool, bool)> {
    match error.refusal() {
        Some(Refusal::Divergent { records }) => records
            .iter()
            .map(|r| (r.record, r.paused, r.divergent))
            .collect(),
        other => panic!("not a divergent refusal: {other:?} ({error:?})"),
    }
}

/// Bob's shop on `store`.
async fn shop_on(w: &World, store: Arc<dyn StateStore>) -> Paykit {
    w.join_with(
        &w.bob,
        "shop",
        AppScopes::new().request().build().unwrap(),
        PaykitConfig::default(),
        store,
    )
    .await
}

/// The payee's record of `request` at `shop`.
async fn sale(shop: &Paykit, request: &RequestRef) -> paykit_sdk::PayeeActivity {
    shop.payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .unwrap()
}

fn idempotent_without_history() -> RailDeclaration {
    RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS).with_idempotent(true)
}

#[tokio::test]
async fn test_divergent_request_paid_by_hand_only_after_confirmation() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);

    // Shown divergent: the payee may have cancelled it.
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Divergent);
    assert!(view.divergent);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, true, true)]);
    assert!(w.rail.all_executions().is_empty());
    let listed = wallet.divergent_records().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].record, record);
    assert_eq!(listed[0].collection, Collection::PayerActivity);

    // The user has seen it: the payment goes.
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_divergent_confirmation_refused_for_a_record_never_seen_divergent() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    let refused = wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::NotFound));
    assert!(wallet.divergent_records().await.unwrap().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_divergent_pause_kept_after_a_newer_write_clears_the_view() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    // A newer write clears the view: the pause stays until the user confirms.
    set_divergent(&base, &wallet, &record, false);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, true, false)]);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Divergent
    );
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    // Confirmed with the view clear, the mark goes at the next receive.
    wallet.receive(None).await.unwrap();
    assert!(wallet.divergent_records().await.unwrap().is_empty());
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_divergent_flag_seen_at_a_load_pauses_once_the_view_clears() {
    // A crash before a receive took its events: the load looks at every record held.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    let app_id = wallet.app_id();
    set_divergent(&base, &wallet, &record, true);
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
        .await;
    set_divergent(&base, &wallet, &record, false);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, true, false)]);
}

#[tokio::test]
async fn test_divergent_two_own_apps_each_pause_until_their_own_user_confirms() {
    let w = World::new();
    let (base_a, base_b) = (store(), store());
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base_a)).await;
    let mut phone = w.wallet_on("phone", Arc::clone(&base_b)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    assert_eq!(phone.request(&request).await.unwrap().record, record);
    set_divergent(&base_a, &wallet, &record, true);
    set_divergent(&base_b, &phone, &record, true);
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    assert!(phone
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());

    // The wallet's user confirms: the phone still pauses.
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    let refused = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, true, true)]);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");

    // The wallet's write reaches the phone and clears its view: the phone's pause stays,
    // shown, until its own user confirms.
    phone.receive(None).await.unwrap();
    let held = phone.request(&request).await.unwrap();
    assert_eq!(held.status, RequestStatus::Paid);
    assert!(held.divergent);
    let listed = phone.divergent_records().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].paused && !listed[0].divergent);
    phone
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    phone.receive(None).await.unwrap();
    assert!(phone.divergent_records().await.unwrap().is_empty());
    assert!(!phone.request(&request).await.unwrap().divergent);
}

/// The `payment_lists` record of bob's shop at `wallet`.
fn list_record(w: &World, wallet: &Paykit, shop: &Paykit) -> [u8; 32] {
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PaymentLists.name(),
            Collection::PaymentLists.fixed_policy(),
        ),
        &natural_key::payment_list(&w.bob_id(), &shop.app_id()),
    );
    wallet.app().record_key(&target).unwrap()
}

#[tokio::test]
async fn test_divergent_list_endpoints_paused_single_use_and_reusable() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    wallet.receive(None).await.unwrap();
    let record = list_record(&w, &wallet, &shop);
    set_divergent(&base, &wallet, &record, true);
    for id in ["btc-bitcoin-p2wpkh", "btc-lightning-bolt12"] {
        let payment = UnrequestedPayment::new(w.bob_id(), btc("1"))
            .with_options(PayOptions::default().with_endpoint(endpoint(id)));
        let refused = wallet.pay_without_request(payment).await.unwrap_err();
        assert_eq!(
            refused_records(&refused),
            vec![(record, true, true)],
            "{id}"
        );
    }
    assert!(w.rail.all_executions().is_empty());
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    let payment = UnrequestedPayment::new(w.bob_id(), btc("1"))
        .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")));
    let outcome = wallet.pay_without_request(payment).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_divergent_payee_keeps_a_proof_and_writes_it_after_confirmation() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");

    // The acceptance and the proof are kept, not written over the divergent record.
    let summary = shop.receive(None).await.unwrap();
    assert_eq!(summary.divergent.len(), 1, "{summary:?}");
    assert_eq!(summary.divergent[0].record, record);
    let held = sale(&shop, &request).await;
    assert_eq!(held.status, PayeeStatus::Requested);
    assert!(held.divergent);

    shop.confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let held = sale(&shop, &request).await;
    assert_eq!(held.status, PayeeStatus::ProofReceived);
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Receipted);
}

#[tokio::test]
async fn test_divergent_payee_writes_a_rejection_over_a_divergent_record() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    wallet.receive(None).await.unwrap();
    wallet.reject_request(&request, None).await.unwrap();
    shop.receive(None).await.unwrap();
    // A stop is written at once; the record stays paused until the user confirms.
    let held = sale(&shop, &request).await;
    assert_eq!(held.status, PayeeStatus::Rejected);
    assert!(held.divergent);
}

#[tokio::test]
async fn test_divergent_kept_proof_given_up_past_its_window_with_a_notice() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.notices().await.unwrap().is_empty());
    // The user never confirms: 30 days after their acceptance the kept messages go, the
    // host told of each.
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    shop.receive(None).await.unwrap();
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices.iter().all(|n| n.kind == NoticeKind::MessageGivenUp));
    // A confirmation now writes nothing: they were given up.
    shop.confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Requested);
}

#[tokio::test]
async fn test_divergent_credit_waits_for_the_record_and_another_of_the_payer() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&first, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let paid = sale(&shop, &first).await.record;
    let other = sale(&shop, &second).await.record;

    // Another record of the same payer is divergent: the credit can't tell it isn't one.
    set_divergent(&base, &shop, &other, true);
    let refused = shop.confirm_payment(&paid).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(other, true, true)]);
    shop.confirm_divergent(&other, UserConfirmed::new())
        .await
        .unwrap();

    // The record itself divergent: the credit waits for it too.
    set_divergent(&base, &shop, &paid, true);
    let refused = shop.confirm_payment(&paid).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(paid, true, true)]);
    shop.confirm_divergent(&paid, UserConfirmed::new())
        .await
        .unwrap();
    assert!(shop.confirm_payment(&paid).await.unwrap().is_some());
}

#[tokio::test]
async fn test_divergent_receipt_kept_over_the_payer_record_until_confirmed() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    shop.receive(None).await.unwrap();
    set_divergent(&base, &wallet, &record, true);
    let sold = sale(&shop, &request).await.record;
    assert!(shop.confirm_payment(&sold).await.unwrap().is_some());
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().receipt.is_none());
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().receipt.is_some());
}

#[tokio::test]
async fn test_divergent_repeat_withheld_while_divergent_and_made_once_clear_and_confirmed() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
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
    w.rail.set_declaration(idempotent_without_history());
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    // Confirmed while the view still shows it divergent: still no automatic repeat.
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    // Clear and confirmed: the repeat goes.
    set_divergent(&base, &wallet, &record, false);
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 2);
}

#[tokio::test]
async fn test_divergent_repeat_withheld_after_the_view_clears_until_confirmed() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
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
    w.rail.set_declaration(idempotent_without_history());
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    wallet.resume_payments().await.unwrap();
    set_divergent(&base, &wallet, &record, false);
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 2);
}

#[tokio::test]
async fn test_divergent_outcome_waits_for_the_confirmation() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
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
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    w.rail.settle(&attempt_id, true);
    w.rail.set_declaration(
        RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true)
            .with_history(true),
    );
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::RecordDivergent { record: r, .. }] if *r == record),
        "{resumed:?}"
    );
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(claim.status, ClaimStatus::Attempting);
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_divergent_restore_pauses_again_a_record_confirmed_before_the_backup() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    wallet
        .confirm_divergent(&record, UserConfirmed::new())
        .await
        .unwrap();
    let before = wallet.divergent_records().await.unwrap();
    assert!(!before[0].paused && before[0].divergent);
    let backup = wallet.backup().await.unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let listed = restored.divergent_records().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].record, record);
    assert!(listed[0].paused);
}
