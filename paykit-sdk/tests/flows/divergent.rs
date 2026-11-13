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
use paykit_lib::records::ActivitySide;
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
pub fn set_divergent(store: &Arc<dyn StateStore>, app: &Paykit, record: &[u8; 32], on: bool) {
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

/// Confirm `record` as the app shows it now ([`Paykit::divergent_records`]), as a host does
/// once its user has seen it.
pub async fn confirm(app: &mut Paykit, record: &[u8; 32]) -> paykit_sdk::Result<()> {
    let shown = app
        .divergent_records()
        .await?
        .into_iter()
        .find(|shown| shown.record == *record)
        .expect("the record is shown divergent");
    app.confirm_divergent(&shown, UserConfirmed::new()).await
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

/// Confirm every record `refused`, a divergent refusal, names, as it shows them.
async fn confirm_refused(app: &mut Paykit, refused: &paykit_sdk::PaykitError) {
    let Some(Refusal::Divergent { records }) = refused.refusal() else {
        panic!("not a divergent refusal: {refused:?}");
    };
    for shown in records.clone() {
        app.confirm_divergent(&shown, UserConfirmed::new())
            .await
            .unwrap();
    }
}

/// Whether every record `refused`, a divergent refusal, names is shown for a credit.
fn shown_for_a_credit(refused: &paykit_sdk::PaykitError) -> bool {
    matches!(refused.refusal(), Some(Refusal::Divergent { records })
        if records.iter().all(|r| r.may_hold_credit))
}

/// Push every pause mark of `app` past the record it names: each then names a later write
/// than the one held, so it doesn't point to its record.
async fn mark_past_its_record(app: &mut Paykit) {
    let mut writes = Vec::new();
    for (key, value) in app.app().protocol_scan(b"dv/").await.unwrap() {
        let mut mark: serde_json::Value = serde_json::from_slice(&value).unwrap();
        mark["d"]["seen"]["generation"] = serde_json::json!(u64::MAX);
        writes.push(pubky_switch_client::app::LocalWrite::Put {
            key,
            value: serde_json::to_vec(&mark).unwrap(),
        });
    }
    assert!(!writes.is_empty());
    app.app_mut().protocol_commit(&writes).await.unwrap();
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
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_divergent_confirmation_of_another_write_than_the_one_shown_refused() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Nothing divergent: nothing to confirm.
    assert!(wallet.divergent_records().await.unwrap().is_empty());
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    let shown = wallet.divergent_records().await.unwrap().remove(0);
    assert!(shown.version.is_some());
    // A newer write comes, a stop, and the record is divergent again: what the user saw is
    // not what the app holds.
    wallet.reject_request(&request, None).await.unwrap();
    set_divergent(&base, &wallet, &record, true);
    let refused = wallet
        .confirm_divergent(&shown, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused, paykit_sdk::PaykitError::Conflict(_)),
        "{refused:?}"
    );
    let now = wallet.divergent_records().await.unwrap().remove(0);
    assert_ne!(now.version, shown.version);
    wallet
        .confirm_divergent(&now, UserConfirmed::new())
        .await
        .unwrap();
    // Once its view clears, the confirmed mark goes; the old view then confirms nothing.
    set_divergent(&base, &wallet, &record, false);
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .confirm_divergent(&now, UserConfirmed::new())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::NotFound));
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
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    // Confirmed with the view clear, the mark goes at the next receive.
    assert_eq!(wallet.app().protocol_scan(b"dv/").await.unwrap().len(), 1);
    wallet.receive(None).await.unwrap();
    assert!(wallet.divergent_records().await.unwrap().is_empty());
    assert!(wallet.app().protocol_scan(b"dv/").await.unwrap().is_empty());
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
    crate::divergent::confirm(&mut wallet, &record)
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
    crate::divergent::confirm(&mut phone, &record)
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
    crate::divergent::confirm(&mut wallet, &record)
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

    crate::divergent::confirm(&mut shop, &record).await.unwrap();
    shop.receive(None).await.unwrap();
    let held = sale(&shop, &request).await;
    assert_eq!(held.status, PayeeStatus::ProofReceived);
    // A plain confirmation lets the writes go, not a credit: the upload may hold a receipt
    // of the same settlement. The refusal shows it so, and a confirmation of that does.
    let refused = shop.confirm_payment(&record).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, false, false)]);
    assert!(shown_for_a_credit(&refused));
    confirm_refused(&mut shop, &refused).await;
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
    crate::divergent::confirm(&mut shop, &record).await.unwrap();
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
    assert!(shown_for_a_credit(&refused));
    // A plain confirmation, as the listing shows the record, doesn't count for a credit.
    let listed = shop.divergent_records().await.unwrap();
    assert!(listed.iter().all(|r| !r.may_hold_credit));
    crate::divergent::confirm(&mut shop, &other).await.unwrap();
    let refused = shop.confirm_payment(&paid).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(other, false, true)]);
    confirm_refused(&mut shop, &refused).await;

    // The record itself divergent: the credit waits for it too.
    set_divergent(&base, &shop, &paid, true);
    let refused = shop.confirm_payment(&paid).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(paid, true, true)]);
    confirm_refused(&mut shop, &refused).await;
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
    crate::divergent::confirm(&mut wallet, &record)
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
    crate::divergent::confirm(&mut wallet, &record)
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
    crate::divergent::confirm(&mut wallet, &record)
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
    crate::divergent::confirm(&mut wallet, &record)
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
    crate::divergent::confirm(&mut wallet, &record)
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

#[tokio::test]
async fn test_divergent_receipt_settles_its_claim_only_once_confirmed() {
    // A payment the method reported failed completed after all: its claim shows the attempt
    // `failed`, and only the payee's receipt settles it. The receipt comes while the payer's
    // record is divergent: it is kept whole, its settlement with it, until the user confirms.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Fail);
    let PayOutcome::Failed { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("failed");
    };
    w.rail.complete_after_failure(&attempt_id);
    wallet.resume_payments().await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    shop.receive(None).await.unwrap();
    let sold = sale(&shop, &request).await.record;
    assert!(shop.confirm_payment(&sold).await.unwrap().is_some());
    wallet.receive(None).await.unwrap();
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let status = |view: Option<pubky_switch_client::app::ClaimRead>| view.unwrap().view.status;
    let claim = wallet.app_mut().read_claim(&obligation).await.unwrap();
    assert_ne!(status(claim), ClaimStatus::Settled);
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let claim = wallet.app_mut().read_claim(&obligation).await.unwrap();
    assert_eq!(status(claim), ClaimStatus::Settled);
    assert!(wallet.request(&request).await.unwrap().receipt.is_some());
}

#[tokio::test]
async fn test_divergent_kept_value_that_doesnt_read_holds_the_new_message() {
    // A value at the kept proof's key that doesn't read: the receive step holds the proof
    // rather than drop it on the key's presence, and once the value is mended and the user
    // confirms, the proof is written.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    let ns = pubky_switch_client::store::client_namespace(&shop.app_id());
    let kept = format!(
        "p/dw/{}/proof/123e4567-e89b-42d3-a456-426614174000",
        pubky_switch_core::enc::b64(&record)
    );
    let mut batch = WriteBatch::new();
    batch
        .put(&ns, kept.clone().into_bytes(), b"not a kept write".to_vec())
        .unwrap();
    base.commit(batch).unwrap();
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    // The state doesn't read: the receive fails rather than act on it.
    assert!(shop.receive(None).await.is_err());
    let mut batch = WriteBatch::new();
    batch.delete(&ns, kept.into_bytes()).unwrap();
    base.commit(batch).unwrap();
    crate::divergent::confirm(&mut shop, &record).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        sale(&shop, &request).await.status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_divergent_kept_write_waits_while_its_record_may_be_missing_from_the_copy() {
    // The shop keeps the payer's replies over its divergent record; another of bob's apps
    // then deletes the record. Past 30 days, a `missed` item comes and Paykit's listing
    // fails: the copy may lack the record, so its absence decides nothing, and the kept
    // messages wait. Once the records are listed, still deleted and unconfirmed, they go.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().protocol_scan(b"dw/").await.unwrap().len(), 2);
    till.receive(None).await.unwrap();
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.policy(None),
        ),
        &natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    till.app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    let now = w.clock.now_ms();
    w.bob.switch().push_missed(1, now - 1_000, now);
    // The client lists the shop's collection right after the item; Paykit's own listing at
    // the end of the receive fails.
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "records?collection".into(),
            after: 1,
        });
    let got = shop.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(shop.app().protocol_scan(b"dw/").await.unwrap().len(), 2);
    assert!(shop.notices().await.unwrap().is_empty());
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_scan(b"dw/").await.unwrap().is_empty());
    assert_eq!(shop.notices().await.unwrap().len(), 2);
}

/// The payee's kept messages over its divergent record: the shop's record of a request
/// alice pays, with the client's flag set before her acceptance and proof come. Returns the
/// shop's store, the shop, alice's wallet, the request and the record.
async fn kept_at_the_shop(
    w: &World,
) -> (Arc<dyn StateStore>, Paykit, Paykit, RequestRef, [u8; 32]) {
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().protocol_scan(b"dw/").await.unwrap().len(), 2);
    (base, shop, wallet, request, record)
}

#[tokio::test]
async fn test_divergent_kept_writes_wait_after_the_view_clears_until_confirmed() {
    let w = World::new();
    let (base, mut shop, _wallet, request, record) = kept_at_the_shop(&w).await;
    // A newer write clears the view; the user hasn't confirmed: nothing is written.
    set_divergent(&base, &shop, &record, false);
    shop.receive(None).await.unwrap();
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Requested);
    assert_eq!(shop.app().protocol_scan(b"dw/").await.unwrap().len(), 2);
    confirm(&mut shop, &record).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        sale(&shop, &request).await.status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_divergent_kept_write_pointing_past_its_record_never_written() {
    // A kept message whose recorded write is later than the record held isn't that record's:
    // it isn't written, nothing waits for it, and it is given up at its 30 days with the
    // host's notice, as any kept message (Paykit spec 8.1, "Stored entries").
    let w = World::new();
    let (_base, mut shop, _wallet, request, record) = kept_at_the_shop(&w).await;
    let mut writes = Vec::new();
    for (key, value) in shop.app().protocol_scan(b"dw/").await.unwrap() {
        let mut kept: serde_json::Value = serde_json::from_slice(&value).unwrap();
        kept["d"]["seen"]["generation"] = serde_json::json!(u64::MAX);
        writes.push(pubky_switch_client::app::LocalWrite::Put {
            key,
            value: serde_json::to_vec(&kept).unwrap(),
        });
    }
    shop.app_mut().protocol_commit(&writes).await.unwrap();
    confirm(&mut shop, &record).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Requested);
    assert_eq!(shop.app().protocol_scan(b"dw/").await.unwrap().len(), 2);
    assert!(shop.notices().await.unwrap().is_empty());
    // Nothing waits for them: the record shows no writes waiting.
    assert!(shop
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .all(|r| r.record != record || !r.writes_waiting));
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_scan(b"dw/").await.unwrap().is_empty());
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices.iter().all(|n| n.kind == NoticeKind::MessageGivenUp));
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Requested);
}

#[tokio::test]
async fn test_divergent_users_done_waits_for_the_confirmation_once_the_view_clears() {
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
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let record = wallet.request(&request).await.unwrap().record;
    let done = async |wallet: &mut Paykit| {
        wallet
            .resolve_attempt(
                &obligation,
                attempt_id,
                paykit_sdk::Resolution::Done,
                UserConfirmed::new(),
            )
            .await
    };
    set_divergent(&base, &wallet, &record, true);
    let refused = done(&mut wallet).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, true, true)]);
    // The view clears: the pause stays until the user confirms.
    set_divergent(&base, &wallet, &record, false);
    let refused = done(&mut wallet).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, true, false)]);
    confirm(&mut wallet, &record).await.unwrap();
    done(&mut wallet).await.unwrap();
}

#[tokio::test]
async fn test_divergent_older_kept_list_leaves_a_newer_held_list() {
    // A list v3 is held over a payment_lists record that doesn't read; the record goes
    // divergent and a list v2 is kept over it. Once confirmed, the kept v2 leaves v3 held.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    wallet.receive(None).await.unwrap();
    let record = list_record(&w, &wallet, &shop);
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PaymentLists.name(),
            Collection::PaymentLists.fixed_policy(),
        ),
        &natural_key::payment_list(&w.bob_id(), &shop.app_id()),
    );
    wallet
        .app_mut()
        .put_record(&target, b"{}", Vec::new())
        .await
        .unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], 3).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap()[0].version, 3);
    set_divergent(&base, &wallet, &record, true);
    w.list_to_alice(&mut shop, &[wallet.app_id()], 2).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"dw/").await.unwrap().len(), 1);
    confirm(&mut wallet, &record).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"dw/").await.unwrap().is_empty());
    let held = wallet.held_lists().await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].version, 3);
}

#[tokio::test]
async fn test_divergent_stop_written_by_hand_over_a_divergent_view_leaves_it_paused() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    wallet.cancel_request(&request, None).await.unwrap();
    // The app's own write cleared the view; it saw the record divergent first.
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Cancelled);
    assert!(view.divergent);
    let listed = wallet.divergent_records().await.unwrap();
    assert!(listed[0].paused && !listed[0].divergent);
}

#[tokio::test]
async fn test_divergent_held_request_shows_held_and_divergent() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let restored_store = store();
    let mut restored = Paykit::restore(
        w.env(&w.alice, Arc::clone(&restored_store)),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.reconcile().await.unwrap();
    restored.end_reconcile(UserConfirmed::new()).await.unwrap();
    restored.receive(None).await.unwrap();
    let record = restored.request(&request).await.unwrap().record;
    set_divergent(&restored_store, &restored, &record, true);
    // Held after the restore, and divergent: both show, the hold as the status.
    let view = restored.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::Held);
    assert!(view.divergent);
    restored
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        restored.request(&request).await.unwrap().status,
        RequestStatus::Divergent
    );
}

// Terminal decisions through a real backup and restore.

/// Bob's shop restored from `backup` on a store of its own.
async fn restored_shop(
    w: &World,
    backup: &paykit_sdk::PaykitBackup,
) -> (Arc<dyn StateStore>, Paykit) {
    let base = store();
    let shop = Paykit::restore(
        w.env(&w.bob, Arc::clone(&base)),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    (base, shop)
}

/// Bob's shop asks alice to send again what names `request`, and alice's wallet answers.
async fn sent_again(shop: &mut Paykit, wallet: &mut Paykit, alice: [u8; 32], request: &RequestRef) {
    let selection =
        paykit_sdk::ResendSelection::default().with_request_hashes(vec![request.request_hash]);
    shop.request_resend(alice, selection).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.flush().await.unwrap();
}

#[tokio::test]
async fn test_divergent_restore_carries_a_kept_proof_and_writes_it_once_confirmed() {
    let w = World::new();
    let (_base, mut shop, mut wallet, request, record) = kept_at_the_shop(&w).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let (_restored_store, mut restored) = restored_shop(&w, &backup).await;
    assert_eq!(restored.app().protocol_scan(b"dw/").await.unwrap().len(), 2);
    restored.receive(None).await.unwrap();
    confirm(&mut restored, &record).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(
        sale(&restored, &request).await.status,
        PayeeStatus::ProofReceived
    );
    // The same messages sent again change nothing: they are in the record.
    sent_again(&mut restored, &mut wallet, w.alice_id(), &request).await;
    restored.receive(None).await.unwrap();
    assert!(restored
        .app()
        .protocol_scan(b"dw/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        sale(&restored, &request).await.status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_divergent_placed_write_stays_placed_through_a_restore_and_a_resend() {
    let w = World::new();
    let (_base, mut shop, mut wallet, request, record) = kept_at_the_shop(&w).await;
    confirm(&mut shop, &record).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        sale(&shop, &request).await.status,
        PayeeStatus::ProofReceived
    );
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let (_restored_store, mut restored) = restored_shop(&w, &backup).await;
    restored.receive(None).await.unwrap();
    sent_again(&mut restored, &mut wallet, w.alice_id(), &request).await;
    restored.receive(None).await.unwrap();
    assert!(restored
        .app()
        .protocol_scan(b"dw/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        sale(&restored, &request).await.status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_divergent_given_up_write_stays_given_up_through_a_restore_and_a_resend() {
    let w = World::new();
    let (_base, mut shop, mut wallet, request, _record) = kept_at_the_shop(&w).await;
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.notices().await.unwrap().len(), 2);
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let (_restored_store, mut restored) = restored_shop(&w, &backup).await;
    restored.receive(None).await.unwrap();
    sent_again(&mut restored, &mut wallet, w.alice_id(), &request).await;
    restored.receive(None).await.unwrap();
    // The messages stay processed: nothing is kept again, and no notice is raised twice.
    assert!(restored
        .app()
        .protocol_scan(b"dw/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(restored.notices().await.unwrap().len(), 2);
    assert_eq!(
        sale(&restored, &request).await.status,
        PayeeStatus::Requested
    );
}

#[tokio::test]
async fn test_divergent_ended_mark_stays_ended_through_a_restore_and_a_listing() {
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = wallet.request(&request).await.unwrap().record;
    set_divergent(&base, &wallet, &record, true);
    confirm(&mut wallet, &record).await.unwrap();
    set_divergent(&base, &wallet, &record, false);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"dv/").await.unwrap().is_empty());
    let backup = wallet.backup().await.unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.reconcile().await.unwrap();
    restored.end_reconcile(UserConfirmed::new()).await.unwrap();
    restored.receive(None).await.unwrap();
    assert!(restored.divergent_records().await.unwrap().is_empty());
    assert!(restored
        .app()
        .protocol_scan(b"dv/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_divergent_resume_records_a_payment_made_once_confirmed_over_a_divergent_view() {
    // The wallet stops right after the method's payment is journaled, before its acceptance
    // is released. The request record is then divergent: the resume holds the outcome, and
    // the acceptance with it, until the user confirms; then both go, the view still
    // divergent, so the payee gets the proof of the payment made.
    use pubky_switch_client::store::{Fault, FaultStore};
    for n in 1..96 {
        let w = World::new();
        let base = store();
        let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
        let app_id = wallet.app_id();
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        let record = wallet.request(&request).await.unwrap().record;
        drop(wallet);
        let faulty: Arc<dyn StateStore> =
            Arc::new(FaultStore::new(Arc::clone(&base), Fault::StopAfter(n)));
        let mut crashing = w
            .load_on_clock(&w.alice, faulty, app_id, Arc::clone(&w.clock))
            .await;
        let _ = crashing.pay_request(&request, PayOptions::default()).await;
        drop(crashing);
        let mut wallet = w
            .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
            .await;
        let stage = wallet.payments().await.unwrap().first().map(|p| p.stage);
        if stage != Some(paykit_sdk::Stage::RailPaid) {
            continue;
        }
        shop.receive(None).await.unwrap();
        if sale(&shop, &request).await.status != PayeeStatus::Requested {
            continue;
        }
        set_divergent(&base, &wallet, &record, true);
        let resumed = wallet.resume_payments().await.unwrap();
        assert!(
            matches!(resumed.as_slice(), [PayOutcome::RecordDivergent { .. }]),
            "{resumed:?}"
        );
        shop.receive(None).await.unwrap();
        assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Requested);
        confirm(&mut wallet, &record).await.unwrap();
        let listed = wallet.divergent_records().await.unwrap();
        assert!(!listed[0].paused && listed[0].divergent, "{listed:?}");
        let resumed = wallet.resume_payments().await.unwrap();
        assert!(
            matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
            "{resumed:?}"
        );
        shop.receive(None).await.unwrap();
        assert_eq!(
            sale(&shop, &request).await.status,
            PayeeStatus::ProofReceived
        );
        return;
    }
    panic!("no commit leaves the payment journaled before its acceptance");
}

#[tokio::test]
async fn test_divergent_mark_not_pointing_to_its_record_fails_no_receive() {
    // A mark whose recorded write is later than the record held isn't that record's: it
    // reads as paused and names nothing. The receives go on, a payment by hand is refused
    // as for any paused record, and the user's confirmation replaces the mark.
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
    mark_past_its_record(&mut wallet).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().divergent);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, true, true)]);
    confirm(&mut wallet, &record).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_divergent_mismatched_mark_lets_the_payers_cancel_go() {
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
    mark_past_its_record(&mut wallet).await;
    // A stop goes over it, no mark written, and the payee gets it.
    wallet.cancel_request(&request, None).await.unwrap();
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Cancelled);
    assert!(wallet.divergent_records().await.unwrap()[0].paused);
}

#[tokio::test]
async fn test_divergent_mismatched_mark_takes_a_payees_cancellation() {
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
    mark_past_its_record(&mut wallet).await;
    let sold = shop.payee_activity().await.unwrap()[0].record;
    shop.cancel_sent_request(&sold, None).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
}

#[tokio::test]
async fn test_divergent_mismatched_mark_lets_the_payees_cancel_go() {
    let w = World::new();
    let (_base, mut shop, mut wallet, request, record) = kept_at_the_shop(&w).await;
    mark_past_its_record(&mut shop).await;
    shop.cancel_sent_request(&record, None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Cancelled);
    wallet.receive(None).await.unwrap();
}

#[tokio::test]
async fn test_divergent_mismatched_mark_writes_a_payers_rejection_in_the_receive_step() {
    // The shop's record is seen divergent at a load, and its mark then names a later write
    // than the one held: the payer's rejection, a stop, is written over it at the step.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let shop = shop_on(&w, Arc::clone(&base)).await;
    let app_id = shop.app_id();
    let mut shop = shop;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    drop(shop);
    let mut shop = w
        .load_on_clock(&w.bob, Arc::clone(&base), app_id, Arc::clone(&w.clock))
        .await;
    mark_past_its_record(&mut shop).await;
    wallet.receive(None).await.unwrap();
    wallet.reject_request(&request, None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Rejected);
}

#[tokio::test]
async fn test_divergent_mismatched_mark_beside_a_held_list_fails_no_receive() {
    // A list is held over a `payment_lists` record that doesn't read, and kept over it once
    // divergent; then the record's mark names a later write than the one held. Every receive
    // goes on, the list held.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    wallet.receive(None).await.unwrap();
    let record = list_record(&w, &wallet, &shop);
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PaymentLists.name(),
            Collection::PaymentLists.fixed_policy(),
        ),
        &natural_key::payment_list(&w.bob_id(), &shop.app_id()),
    );
    wallet
        .app_mut()
        .put_record(&target, b"{}", Vec::new())
        .await
        .unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], 3).await;
    wallet.receive(None).await.unwrap();
    set_divergent(&base, &wallet, &record, true);
    w.list_to_alice(&mut shop, &[wallet.app_id()], 2).await;
    wallet.receive(None).await.unwrap();
    mark_past_its_record(&mut wallet).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap()[0].version, 3);
}

#[tokio::test]
async fn test_divergent_later_message_kept_is_reflected_once_written() {
    // A second acceptance over the divergent record is kept beside the first; once the user
    // confirms, the first goes into its role and the second is reflected, as at a first read.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    wallet.receive(None).await.unwrap();
    let body = |label: Option<&str>| {
        serde_json::json!({ "label": label })
            .as_object()
            .cloned()
            .unwrap()
    };
    let first = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_request_acceptance",
        vec![request.request_hash],
        body(None),
    );
    w.clock.advance_ms(1_000);
    let second = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_request_acceptance",
        vec![request.request_hash],
        body(None),
    );
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().protocol_scan(b"dw/").await.unwrap().len(), 2);
    confirm(&mut shop, &record).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_scan(b"dw/").await.unwrap().is_empty());
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let ids: Vec<String> = value["msg_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect();
    assert!(ids.contains(&first) && ids.contains(&second), "{value}");
    assert_eq!(sale(&shop, &request).await.status, PayeeStatus::Accepted);
}

#[tokio::test]
async fn test_divergent_kept_value_at_a_key_without_its_msg_id_holds_the_new_message() {
    // A value at a kept proof's key that names no `msg_id` doesn't read: the receive step
    // holds the proof, and once the value is removed and the user confirms, it is written.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    let ns = pubky_switch_client::store::client_namespace(&shop.app_id());
    let kept = format!("p/dw/{}/proof", pubky_switch_core::enc::b64(&record));
    let mut batch = WriteBatch::new();
    batch
        .put(&ns, kept.clone().into_bytes(), b"not a kept write".to_vec())
        .unwrap();
    base.commit(batch).unwrap();
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(shop.receive(None).await.is_err());
    let mut batch = WriteBatch::new();
    batch.delete(&ns, kept.into_bytes()).unwrap();
    base.commit(batch).unwrap();
    confirm(&mut shop, &record).await.unwrap();
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        sale(&shop, &request).await.status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_divergent_outcome_waits_after_the_view_clears_until_confirmed() {
    // A payment made over a paused record: its outcome, acceptance and proof wait while the
    // record is paused, also once a newer write has cleared the view, and go once the user
    // confirms.
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
        matches!(resumed.as_slice(), [PayOutcome::RecordDivergent { .. }]),
        "{resumed:?}"
    );
    set_divergent(&base, &wallet, &record, false);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::RecordDivergent { record: r, .. }] if *r == record),
        "{resumed:?}"
    );
    shop.receive(None).await.unwrap();
    assert_ne!(
        sale(&shop, &request).await.status,
        PayeeStatus::ProofReceived
    );
    confirm(&mut wallet, &record).await.unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        sale(&shop, &request).await.status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_divergent_kept_list_written_before_a_payment_to_it() {
    // A newer list is kept over the divergent `payment_lists` record; the user confirms.
    // Until the next receive writes the kept list, no payment goes to the list: the
    // refusal shows its writes waiting. Then the payment goes.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    wallet.receive(None).await.unwrap();
    let record = list_record(&w, &wallet, &shop);
    set_divergent(&base, &wallet, &record, true);
    w.list_to_alice(&mut shop, &[wallet.app_id()], 2).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"dw/").await.unwrap().len(), 1);
    confirm(&mut wallet, &record).await.unwrap();
    let payment = || {
        UnrequestedPayment::new(w.bob_id(), btc("1"))
            .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")))
    };
    let refused = wallet.pay_without_request(payment()).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, false, true)]);
    let Some(Refusal::Divergent { records }) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert!(records[0].writes_waiting);
    assert!(w.rail.all_executions().is_empty());
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_scan(b"dw/").await.unwrap().is_empty());
    let outcome = wallet.pay_without_request(payment()).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

/// The client's durable divergence report: `AppEvent::RecordDivergent { id, key }` is a duty
/// event, delivered until acknowledged, kept through a restart and a cleared flag, and raised
/// again under a new ID after a restore; `ScriptedSwitch::make_divergent` makes a record
/// divergent at the switch, as a conflicting upload does.
mod durable_event {
    use super::*;

    /// The app whose upload conflicts with the record's current write.
    const UPLOADER: [u8; 16] = [0x5a; 16];

    #[tokio::test]
    async fn test_divergent_event_pauses_through_a_restart_and_a_cleared_flag() {
        // The client reads the record divergent and keeps the duty event; Paykit stops before
        // taking it. The flag then clears, as a newer write the client takes does, and the
        // app starts again: the load sees no flag, and the duty event alone pauses the record.
        let w = World::new();
        let base = store();
        let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
        let app_id = wallet.app_id();
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        let record = wallet.request(&request).await.unwrap().record;
        w.alice
            .switch()
            .make_divergent(UPLOADER, record, [1; 32])
            .unwrap();
        wallet.app_mut().receive(None).await.unwrap();
        assert!(
            wallet
                .app()
                .shared_record(&record)
                .await
                .unwrap()
                .unwrap()
                .divergent
        );
        set_divergent(&base, &wallet, &record, false);
        drop(wallet);
        let mut wallet = w
            .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
            .await;
        assert!(wallet.app().protocol_scan(b"dv/").await.unwrap().is_empty());
        let summary = wallet.receive(None).await.unwrap();
        assert!(
            summary.divergent.iter().any(|r| r.record == record),
            "{summary:?}"
        );
        let refused = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err();
        let named = refused_records(&refused);
        assert!(
            named.len() == 1 && named[0].0 == record && named[0].1,
            "{named:?}"
        );
    }

    #[tokio::test]
    async fn test_divergent_event_pauses_a_record_cleared_after_the_read() {
        // The receive that reads the record divergent takes the event; the flag clears right
        // after. No view the app takes later shows it divergent: the event's mark alone keeps
        // the pause, until the user confirms.
        let w = World::new();
        let base = store();
        let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        let record = wallet.request(&request).await.unwrap().record;
        w.alice
            .switch()
            .make_divergent(UPLOADER, record, [1; 32])
            .unwrap();
        let summary = wallet.receive(None).await.unwrap();
        assert!(
            summary.divergent.iter().any(|r| r.record == record),
            "{summary:?}"
        );
        set_divergent(&base, &wallet, &record, false);
        let refused = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err();
        assert_eq!(refused_records(&refused), vec![(record, true, false)]);
        confirm(&mut wallet, &record).await.unwrap();
        let outcome = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap();
        assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn test_divergent_repeat_withheld_when_its_fresh_read_shows_the_record_divergent() {
        // The wallet's own copy shows the request record clear; the switch holds it
        // divergent. The repeat's fresh read of the record is the first view that shows it:
        // the repeat is withheld on that view.
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
        w.alice
            .switch()
            .make_divergent(UPLOADER, record, [1; 32])
            .unwrap();
        assert!(
            !wallet
                .app()
                .shared_record(&record)
                .await
                .unwrap()
                .unwrap()
                .divergent
        );
        let resumed = wallet.resume_payments().await.unwrap();
        assert!(
            matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
            "{resumed:?}"
        );
        assert_eq!(w.rail.pay_calls(&attempt_id), 1);
        assert!(wallet.divergent_records().await.unwrap()[0].paused);
    }
}

/// The `msg_id` of the message the shop's record at `record` holds in its role `role`.
async fn in_role(shop: &Paykit, record: &[u8; 32], role: &str) -> Option<String> {
    let held = shop.app().shared_record(record).await.unwrap().unwrap();
    let value = held.content.value().unwrap();
    let read =
        paykit_lib::records::ActivityRecord::read(value, &shop.identity(), ActivitySide::Payee)
            .unwrap();
    let stored = match role {
        "response" => read.response,
        "proof" => read.proof,
        _ => panic!("a role"),
    };
    stored.map(|stored| stored.message().msg_id.clone())
}

#[tokio::test]
async fn test_divergent_acceptance_after_a_confirmation_written_after_the_kept_one() {
    // An acceptance is kept over the divergent record; the user confirms; a second one comes
    // in the next read, whose step runs before the kept messages are written. It waits behind
    // the kept one, which goes into its role first, in the order the inbox accepted them
    // (Paykit spec 8.1, "Stored entries").
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    wallet.receive(None).await.unwrap();
    let body = || {
        serde_json::json!({ "label": null })
            .as_object()
            .cloned()
            .unwrap()
    };
    let shop_app = shop.app_id();
    let acceptance = |w: &World| {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop_app],
            "paykit.payment_request_acceptance",
            vec![request.request_hash],
            body(),
        )
    };
    let first = acceptance(&w);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().protocol_scan(b"dw/").await.unwrap().len(), 1);
    confirm(&mut shop, &record).await.unwrap();
    w.clock.advance_ms(1_000);
    let second = acceptance(&w);
    shop.receive(None).await.unwrap();
    assert_eq!(in_role(&shop, &record, "response").await, Some(first));
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(value["msg_ids"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!(second)));
    assert!(shop.app().protocol_scan(b"dw/").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_divergent_proof_after_a_confirmation_leaves_the_kept_one_credited() {
    // The acceptance and the proof of a payment are kept over the divergent record; the user
    // confirms; a second proof comes in the next read. The kept proof takes the role, and is
    // the one credited.
    let w = World::new();
    let (_base, mut shop, wallet, request, record) = kept_at_the_shop(&w).await;
    let sent = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|sent| sent.kind == "paykit.payment_proof")
        .unwrap();
    let message = pubky_switch_core::message::Message::parse(
        sent.plaintext.json(),
        paykit_lib::kinds::OPAQUE_MEMBERS,
    )
    .unwrap();
    confirm(&mut shop, &record).await.unwrap();
    w.clock.advance_ms(1_000);
    let again = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        vec![request.request_hash],
        message.body.clone(),
    );
    assert_ne!(again, message.msg_id);
    shop.receive(None).await.unwrap();
    assert_eq!(
        in_role(&shop, &record, "proof").await,
        Some(message.msg_id.clone())
    );
    assert_eq!(
        sale(&shop, &request).await.status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_divergent_later_write_waits_behind_a_kept_one_not_written_yet() {
    // The kept acceptance can't be written at the receive after the confirmation: its write
    // meets the quota. A later acceptance in that read waits behind it, kept, and the kept
    // one goes first once the switch takes it.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &record, true);
    wallet.receive(None).await.unwrap();
    let body = || {
        serde_json::json!({ "label": null })
            .as_object()
            .cloned()
            .unwrap()
    };
    let shop_app = shop.app_id();
    let acceptance = |w: &World| {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop_app],
            "paykit.payment_request_acceptance",
            vec![request.request_hash],
            body(),
        )
    };
    let first = acceptance(&w);
    shop.receive(None).await.unwrap();
    confirm(&mut shop, &record).await.unwrap();
    w.clock.advance_ms(1_000);
    let second = acceptance(&w);
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::QuotaExceeded,
            current: serde_json::Value::Null,
        });
    shop.receive(None).await.unwrap();
    assert_eq!(in_role(&shop, &record, "response").await, None);
    assert_eq!(shop.app().protocol_scan(b"dw/").await.unwrap().len(), 2);
    shop.receive(None).await.unwrap();
    assert_eq!(in_role(&shop, &record, "response").await, Some(first));
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(value["msg_ids"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!(second)));
}

#[tokio::test]
async fn test_divergent_credit_flag_set_by_the_host_credits_nothing() {
    // The host takes a record never shown for a credit and sets its flag itself before the
    // confirmation: the app keeps no showing of it, so the credit stays refused. The refusal
    // then shows it so, and a confirmation of that lets the credit go.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_on(&w, Arc::clone(&base)).await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let paid = sale(&shop, &request).await.record;
    set_divergent(&base, &shop, &paid, true);
    let mut shown = shop
        .divergent_records()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.record == paid)
        .unwrap();
    assert!(!shown.may_hold_credit);
    shown.may_hold_credit = true;
    shop.confirm_divergent(&shown, UserConfirmed::new())
        .await
        .unwrap();
    let refused = shop.confirm_payment(&paid).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(paid, false, true)]);
    assert!(shown_for_a_credit(&refused));
    confirm_refused(&mut shop, &refused).await;
    assert!(shop.confirm_payment(&paid).await.unwrap().is_some());
}

/// The publishing apps `wallet` asked bob for a list of, one per list request it sent.
async fn list_asks(w: &World, wallet: &Paykit) -> Vec<Option<[u8; 16]>> {
    let sent = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap();
    sent.iter()
        .filter(|sent| sent.kind == "paykit.private_payment_list_request")
        .map(|sent| {
            let message = pubky_switch_core::message::Message::parse(
                sent.plaintext.json(),
                paykit_lib::kinds::OPAQUE_MEMBERS,
            )
            .unwrap();
            match paykit_lib::Body::parse(&message).unwrap() {
                paykit_lib::Body::PrivatePaymentListRequest(asked) => asked.app_id,
                other => panic!("a list request: {other:?}"),
            }
        })
        .collect()
}

#[tokio::test]
async fn test_divergent_paused_list_asks_the_app_whose_list_expired() {
    // Bob's shop's list is paused as divergent; his till's list has merely expired. The
    // payment waits for the user's confirmation of the shop's list, the till is asked for its
    // current list, and the shop isn't (Paykit spec 8.4).
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let mut till = w
        .join_with(
            &w.bob,
            "till",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    till.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new("lno1till").unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: 1,
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "till",
        &w.alice,
        &[wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(2 * HOUR_MS);
    let record = list_record(&w, &wallet, &shop);
    set_divergent(&base, &wallet, &record, true);
    let payment = UnrequestedPayment::new(w.bob_id(), btc("1"))
        .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")));
    let refused = wallet.pay_without_request(payment).await.unwrap_err();
    assert_eq!(refused_records(&refused), vec![(record, true, true)]);
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(list_asks(&w, &wallet).await, vec![Some(till.app_id())]);
}
