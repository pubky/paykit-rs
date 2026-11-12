//! Payments without a request (Paykit spec 8.4): to a Private Payment List's endpoints, with
//! single-use endpoints reserved in the attempt's batch, and credited once by settlement.

use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, Paykit, Refusal, UnrequestedPayment};
use pubky_switch_client::app::AppEvent;
use pubky_switch_client::clock::Clock;

use crate::common::{btc, endpoint, World, HOUR_MS};

/// How many `private_payment_list_request`s `shop` takes at its next receive. It publishes no
/// list, so each one is taken with no effect (Paykit spec 8.4).
async fn list_requests(shop: &mut Paykit) -> usize {
    let got = shop.receive(None).await.unwrap();
    got.report
        .events
        .iter()
        .filter(|e| {
            matches!(e, AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.private_payment_list_request")
        })
        .count()
}

fn to_bob(w: &World, value: &str, id: &str) -> UnrequestedPayment {
    UnrequestedPayment::new(w.bob_id(), btc(value))
        .with_options(PayOptions::default().with_endpoint(endpoint(id)))
}

#[tokio::test]
async fn test_unrequested_payment_paid_proven_and_receipted() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    assert_eq!(wallet.receive(None).await.unwrap().applied.len(), 1);

    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);

    // The payee credits it by its settlement, verifies it and sends a receipt.
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].request_hash, None);
    assert_eq!(sales[0].amount, Some(btc("0.0015")));
    assert_eq!(sales[0].payer, w.alice_id());
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    let applied = wallet.receive(None).await.unwrap().applied;
    assert_eq!(applied.len(), 1);

    // A reusable endpoint consumes nothing: it is paid again at once.
    let again = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert!(matches!(again, PayOutcome::Paid { .. }), "{again:?}");
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 2);
}

#[tokio::test]
async fn test_unrequested_single_use_endpoint_used_once() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), phone.app_id()],
        w.clock.now_ms(),
    )
    .await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();

    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");

    // The phone still holds the list as it was: its attempt's batch fails the list record's
    // version, and nothing is paid.
    let raced = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap_err();
    assert!(
        matches!(raced, paykit_sdk::PaykitError::Conflict(_)),
        "{raced:?}"
    );

    // Once it reads the mark, the endpoint is used; so it is for the wallet.
    phone.receive(None).await.unwrap();
    let used = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList)),
        "{used:?}"
    );
    let used = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList)),
        "{used:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);

    // A newer list carries a fresh single-use endpoint.
    w.clock.advance_ms(1000);
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let fresh = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(fresh, PayOutcome::Paid { .. }), "{fresh:?}");
}

#[tokio::test]
async fn test_unrequested_without_a_list_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let refused = wallet
        .pay_without_request(to_bob(&w, "1", "btc-lightning-bolt12"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList)),
        "{refused:?}"
    );
    let public = wallet
        .pay_without_request(to_bob(&w, "1", "btc-lightning-bolt12").with_options(
            PayOptions::default().with_mode(paykit_sdk::PaymentMode::Public(
                pubky_switch_client::app::UserConfirmed::new(),
            )),
        ))
        .await
        .unwrap_err();
    // Nor can bob be paid publicly: he has no identity.json, so no current app (Paykit spec
    // 4).
    assert!(
        matches!(public.refusal(), Some(Refusal::IdentityUnavailable(_))),
        "{public:?}"
    );
}

#[tokio::test]
async fn test_unrequested_expired_list_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    let refused = wallet
        .pay_without_request(to_bob(&w, "1", "btc-lightning-bolt12"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList)),
        "{refused:?}"
    );
    // An expired list is treated as missing: the payee is asked for its current one
    // (Paykit wire 4.13).
    assert_eq!(list_requests(&mut shop).await, 1);
}

/// The refusal of paying bob's Lightning offer, which `wallet` holds no usable list for.
async fn pay_lightning(w: &World, wallet: &mut Paykit) -> paykit_sdk::PaykitError {
    wallet
        .pay_without_request(to_bob(w, "1", "btc-lightning-bolt12"))
        .await
        .unwrap_err()
}

#[tokio::test]
async fn test_unrequested_without_a_list_asks_the_payee_once_while_outstanding() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Bob shares nothing yet, but takes alice's messages.
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let refused = pay_lightning(&w, &mut wallet).await;
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList)),
        "{refused:?}"
    );
    assert_eq!(list_requests(&mut shop).await, 1);
    // Asked again while that request is outstanding, nothing more is sent.
    let refused = pay_lightning(&w, &mut wallet).await;
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList)),
        "{refused:?}"
    );
    assert_eq!(list_requests(&mut shop).await, 0);
    // A day on, unanswered, it is asked once more.
    w.clock.advance_ms(24 * HOUR_MS + 1);
    pay_lightning(&w, &mut wallet).await;
    assert_eq!(list_requests(&mut shop).await, 1);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_unrequested_used_up_list_asks_again_for_each_newer_version() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = w.clock.now_ms();
    w.list_to_alice(&mut shop, &[wallet.app_id()], first).await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    shop.receive(None).await.unwrap();
    // The single-use endpoint is used: the list is used up for it, and the payee is asked.
    let used = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList)),
        "{used:?}"
    );
    assert_eq!(list_requests(&mut shop).await, 1);
    wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap_err();
    assert_eq!(list_requests(&mut shop).await, 0);
    // A newer list, used up in turn, is a new version held: asked at once, within the day.
    w.clock.advance_ms(1000);
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let used = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList)),
        "{used:?}"
    );
    assert_eq!(list_requests(&mut shop).await, 1);
}

#[tokio::test]
async fn test_unrequested_after_an_idle_spell_not_refused_for_its_deadline() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    // The wallet hears nothing from its switch for 11 minutes: the highest service time it
    // verified is older than the deadline's 10 minutes. The deadline is set from the latest
    // service time, read first, never from that old one (Paykit spec 8.4, Switch spec 10.1).
    w.clock.advance_ms(11 * 60_000);
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_unrequested_proof_and_receipt_carry_the_attempt_and_no_request_members() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    // The payee takes the proof: one without refs carries no label, Billing Period, payment
    // reference or Allowance, and the list version of its list destination (Paykit wire
    // 4.10, 5.3).
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "the proof wasn't taken");
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    wallet.receive(None).await.unwrap();
    // The payer's record holds both: the receipt names the proof's attempt, which is the
    // record's (Paykit wire 4.11, 5.3).
    let held = wallet
        .app()
        .records_of("payer_activity")
        .await
        .unwrap()
        .into_iter()
        .find_map(|r| r.content.value().map(<[u8]>::to_vec))
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&held).unwrap();
    let body = |member: &str| {
        let json = record[member]["plaintext"]["json"].as_str().unwrap();
        let bytes = pubky_switch_core::enc::unb64(json).unwrap();
        let message: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        message["body"].clone()
    };
    let proof = body("proof");
    for member in [
        "label",
        "billing_period",
        "payment_reference",
        "allowance_id",
    ] {
        assert!(proof[member].is_null(), "{member} in a proof without refs");
    }
    assert_eq!(
        proof["list_version"], record["unrequested"]["list_version"],
        "the proof's list version isn't the destination's"
    );
    assert!(!record["unrequested"]["list_version"].is_null());
    let receipt = body("receipt");
    assert_eq!(receipt["attempt_id"], proof["attempt_id"]);
    assert_eq!(receipt["attempt_id"], record["attempt_id"]);
    assert!(receipt["billing_period"].is_null());
}

// ---------------------------------------------------------------------------------------
// A later `payment_lists` write another own app makes (Paykit wire 5.4, "Seen lists")

/// A `private_payment_list` from bob's `shop` to alice at `version`, with one single-use
/// on-chain endpoint paying `payload`, as a record stores it: signed, not delivered.
fn stored_list(w: &World, version: u64, payload: &str) -> serde_json::Value {
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new(payload).unwrap(),
            single_use: true,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version,
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + 30 * 24 * crate::common::HOUR_MS,
    }
    .to_body()
    .unwrap();
    let plaintext = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.private_payment_list",
        vec![],
        body,
    );
    let keys = w.bob.identity().app_keys("shop");
    let cert = w.bob.switch().app(&keys.app_id_bytes()).cert.unwrap();
    serde_json::json!({
        "sender": pubky_switch_core::enc::pubky_id(&w.bob_id()),
        "cert": { "body": cert.signed().body, "sig": cert.signed().sig },
        "plaintext": {
            "sender_app": pubky_switch_core::enc::b64(&plaintext.sender_app_id()),
            "signature": pubky_switch_core::enc::b64(&plaintext.signature()),
            "json": pubky_switch_core::enc::b64(plaintext.json()),
        },
        "accepted_at": null,
    })
}

/// The `payment_lists` record of bob's shop, as `app` holds it, and its target.
async fn list_record(
    app: &paykit_sdk::Paykit,
    w: &World,
) -> (pubky_switch_client::app::RecordTarget, serde_json::Value) {
    use paykit_lib::collections::Collection;
    let shop = w.bob.identity().app_keys("shop").app_id_bytes();
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            Collection::PaymentLists.name(),
            Collection::PaymentLists.policy(None),
        ),
        &paykit_lib::derive::natural_key::payment_list(&w.bob_id(), &shop),
    );
    let key = app.app().record_key(&target).unwrap();
    let held = app.app().shared_record(&key).await.unwrap().unwrap();
    let value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    (target, value)
}

/// The phone, another own app, writes the record `change` makes of the one it holds; the
/// wallet then reads its feed. Returns what the wallet holds after, and its flagged writes.
async fn written_by_the_phone(
    w: &World,
    change: impl FnOnce(&World, &mut serde_json::Value),
) -> (
    serde_json::Value,
    Vec<pubky_switch_client::app::FlaggedWrite>,
    serde_json::Value,
) {
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    // The wallet pays its single-use endpoint: the record marks it used.
    let paid = wallet
        .pay_without_request(to_bob(w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    phone.receive(None).await.unwrap();
    let (target, mut value) = list_record(&phone, w).await;
    let before = list_record(&wallet, w).await.1;
    assert_eq!(before["used"].as_array().unwrap().len(), 1);
    change(w, &mut value);
    let bytes = serde_json::to_vec(&value).unwrap();
    let entry = phone
        .app_mut()
        .record_op(&target, Some(&bytes), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::new().entry(entry))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let after = list_record(&wallet, w).await.1;
    let flagged = wallet.app().flagged_writes().await.unwrap();
    (before, flagged, after)
}

#[tokio::test]
async fn test_unrequested_later_list_record_with_another_list_at_its_version_flagged() {
    let w = World::new();
    let version = w.clock.now_ms();
    let (before, flagged, after) = written_by_the_phone(&w, move |w, value| {
        // Another list at the version held: an equivocation, never taken.
        let other = stored_list(w, version, "bc1qother");
        assert_ne!(value["list"], other);
        value["list"] = other;
    })
    .await;
    assert_eq!(flagged.len(), 1, "{flagged:?}");
    assert_eq!(after, before, "the write was taken");
}

#[tokio::test]
async fn test_unrequested_later_list_record_dropping_a_used_mark_flagged() {
    let w = World::new();
    let (before, flagged, after) = written_by_the_phone(&w, move |w, value| {
        // A newer list, but the single-use mark seen dropped: a newer list doesn't reset
        // `used` (Paykit wire 5.4).
        value["list"] = stored_list(w, w.clock.now_ms() + 1000, "bc1qnewer");
        value["used"] = serde_json::json!([]);
    })
    .await;
    assert_eq!(flagged.len(), 1, "{flagged:?}");
    assert_eq!(after, before, "the write was taken");
}

// ---------------------------------------------------------------------------------------
// A `payment_lists` record that doesn't read is never written over without its marks

#[tokio::test]
async fn test_unrequested_list_over_a_record_that_doesnt_read_keeps_single_use_marks() {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, OpBatch, RecordTarget};

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut tool = w.wallet("tool").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id(), phone.app_id()], 1)
        .await;
    // The wallet pays the single-use endpoint: it is marked used in the record, in the
    // attempt's batch.
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    // A faulty own app writes a value that doesn't read over the record.
    let collection = Collection::PaymentLists;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::payment_list(&w.bob_id(), &shop.app_id()),
    );
    tool.app_mut().read_record(&target).await.unwrap();
    let op = tool
        .app_mut()
        .record_op(&target, Some(b"{\"not\":\"a list record\"}"), Vec::new())
        .await
        .unwrap();
    tool.app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();

    // The phone reads the list only now. It never saw the record read, so it takes the
    // value that doesn't read; it doesn't write the list over it, which would drop the
    // single-use mark, and keeps it until the record reads.
    phone.receive(None).await.unwrap();
    let held = phone.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].payee, w.bob_id());
    assert_eq!(held[0].version, 1);
    let refused = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_unrequested_list_after_a_delete_is_held_until_placed_without_marks() {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, OpBatch, RecordTarget};

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    // Another own app deletes the record, and its single-use mark with it; the wallet sees
    // the delete.
    let collection = Collection::PaymentLists;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::payment_list(&w.bob_id(), &shop.app_id()),
    );
    phone.app_mut().read_record(&target).await.unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();

    // Bob sends a list of the same version again, the single-use endpoint the wallet paid
    // in it: written afresh, it would be payable again. It is held instead.
    w.list_to_alice(&mut shop, &[wallet.app_id()], 1).await;
    wallet.receive(None).await.unwrap();
    let held = wallet.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    let refused = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 1);

    // The host has shown the user the endpoint may be paid twice, and places it anyway.
    wallet
        .place_held_list_without_marks(
            &w.bob_id(),
            &shop.app_id(),
            pubky_switch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert!(wallet.held_lists().await.unwrap().is_empty());
    let again = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(again, PayOutcome::Paid { .. }), "{again:?}");
}

#[tokio::test]
async fn test_unrequested_with_the_own_switch_down_refused_and_nothing_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    // Alice's switch stops answering: the SDK makes no payment without it (Paykit spec 8.4
    // allows one to a reusable public endpoint; this SDK pays attempt-first only).
    w.router.remove(w.alice.origin());
    let refused = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::SwitchUnavailable(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_unrequested_with_the_own_switch_unavailable_refused_and_nothing_paid() {
    // The switch answers `503 unavailable`: the same refusal as one that doesn't answer.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "claims".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    let refused = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::SwitchUnavailable(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_unrequested_paid_with_its_outcome_unwritten_is_no_error() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    // The attempt's batch goes through; the outcome's, after the method paid, can't connect.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 1,
        });
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .unwrap();
    let PayOutcome::PaidOutcomeUnwritten { attempt_id, .. } = outcome else {
        panic!("money moved, so the call reports it paid: {outcome:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    // A resume writes the outcome and sends the proof, paying nothing again.
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_unrequested_list_ask_mark_backs_up_and_restores() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let refused = pay_lightning(&w, &mut wallet).await;
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList)),
        "{refused:?}"
    );
    let backup = wallet.backup().await.expect("a backup with an lq/ mark");
    let backup = paykit_sdk::PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .expect("restore of a backup with an lq/ mark");
    let _ = restored.backup().await.expect("backup after restore");
}

/// The signer stores app `app`'s key bundle for `cert`, every data epoch up to the current.
pub(crate) fn store_bundle(w: &World, app: [u8; 16], cert: &pubky_switch_core::records::AppCert) {
    store_bundle_from(w, app, cert, 1);
}

/// The signer stores app `app`'s key bundle for `cert`, the data epochs from `first` to the
/// current.
pub(crate) fn store_bundle_from(
    w: &World,
    app: [u8; 16],
    cert: &pubky_switch_core::records::AppCert,
    first: u64,
) {
    use pubky_switch_client::app::Protocol as _;
    let epoch = w.alice.switch().identity_doc().unwrap().data_epoch();
    let protocol = paykit_sdk::PaykitProtocol::new();
    let parts = pubky_switch_core::records::seal_bundle_parts(
        w.alice.identity().key(),
        cert,
        first,
        epoch,
        &|scope| protocol.has_keys(scope),
    )
    .unwrap();
    for part in parts {
        w.alice
            .switch()
            .signer_op(pubky_switch_core::ops::OpKind::KeyBundlePut(
                pubky_switch_core::ops::KeyBundlePut {
                    target_app_id: app,
                    bundle: part.bundle,
                    first_epoch: part.first_epoch,
                    last_epoch: part.last_epoch,
                },
            ))
            .unwrap();
    }
}

#[tokio::test]
async fn test_unrequested_list_over_a_value_awaiting_its_epoch_key_keeps_single_use_marks() {
    // The wallet paid the single-use endpoint, marked used in the record. The data epoch
    // rotates; the phone gets the new key and writes the record again, sealed at the new
    // epoch, which the wallet holds unopened for now. A newer list is held, not offered to
    // be placed without the marks: a placement is refused as waiting for the keys. Once the
    // wallet's key comes, late, the record opens and the list is placed with the mark.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id(), phone.app_id()], 1)
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    phone.receive(None).await.unwrap();
    assert_eq!(
        list_record(&phone, &w).await.1["used"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    let (target, value) = list_record(&phone, &w).await;
    let bytes = serde_json::to_vec(&value).unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, Some(&bytes), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::new().entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let record_key = wallet.app().record_key(&target).unwrap();
    let held = wallet
        .app()
        .shared_record(&record_key)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            held.content,
            pubky_switch_client::app::RecordContent::Unreadable(
                pubky_switch_client::app::Unopened::KeyNotHeld { .. }
            )
        ),
        "{:?}",
        held.content
    );
    w.list_to_alice(&mut shop, &[wallet.app_id()], 2).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.held_lists().await.unwrap().is_empty());
    let refused = wallet
        .place_held_list_without_marks(
            &w.bob_id(),
            &shop.app_id(),
            pubky_switch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(why)) if why.contains("scope keys")),
        "{refused:?}"
    );
    // The key comes late: the record opens with its mark, and the list is placed on it.
    store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let (_, value) = list_record(&wallet, &w).await;
    assert_eq!(value["used"].as_array().unwrap().len(), 1, "{value}");
    assert_eq!(value["msg_ids"].as_array().unwrap().len(), 2, "{value}");
    assert!(wallet.held_lists().await.unwrap().is_empty());
}
