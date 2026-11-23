//! Payments without a request (Paykit spec 8.4): to a Private Payment List's endpoints, with
//! single-use endpoints reserved in the attempt's batch, and credited once by settlement.

use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, Paykit, Refusal, UnrequestedPayment};
use pubky_switch_client::app::AppEvent;
use pubky_switch_client::clock::Clock;

use crate::common::{btc, endpoint, list_version, single_use_payload, World, HOUR_MS};

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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    assert_eq!(wallet.receive(None).await.unwrap().applied.len(), 1);

    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
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
        .outcome
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
        list_version(1, 1),
    )
    .await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();

    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");

    // The phone still holds the list as it was: its attempt's batch fails the list record's
    // version, and nothing is paid.
    let raced = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
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
        .outcome
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList { .. })),
        "{used:?}"
    );
    let used = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList { .. })),
        "{used:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);

    // A newer list carries a fresh single-use endpoint.
    w.clock.advance_ms(1000);
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    wallet.receive(None).await.unwrap();
    let fresh = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
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
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    let public = wallet
        .pay_without_request(to_bob(&w, "1", "btc-lightning-bolt12").with_options(
            PayOptions::default().with_mode(paykit_sdk::PaymentMode::Public(
                pubky_switch_client::app::UserConfirmed::new(),
            )),
        ))
        .await
        .outcome
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(31 * 24 * HOUR_MS);
    let refused = wallet
        .pay_without_request(to_bob(&w, "1", "btc-lightning-bolt12"))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
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
        .outcome
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
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert_eq!(list_requests(&mut shop).await, 1);
    // Asked again while that request is outstanding, nothing more is sent.
    let refused = pay_lightning(&w, &mut wallet).await;
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    shop.receive(None).await.unwrap();
    // The single-use endpoint is used: the list is used up for it, and the payee is asked.
    let used = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList { .. })),
        "{used:?}"
    );
    assert_eq!(list_requests(&mut shop).await, 1);
    wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(list_requests(&mut shop).await, 0);
    // A newer list, used up in turn, is a new version held: asked at once, within the day.
    w.clock.advance_ms(1000);
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let used = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList { .. })),
        "{used:?}"
    );
    assert_eq!(list_requests(&mut shop).await, 1);
}

#[tokio::test]
async fn test_unrequested_after_an_idle_spell_not_refused_for_its_deadline() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    // The wallet hears nothing from its switch for 11 minutes: the highest service time it
    // verified is older than the deadline's 10 minutes. The deadline is set from the latest
    // service time, read first, never from that old one (Paykit spec 8.4, Switch spec 10.1).
    w.clock.advance_ms(11 * 60_000);
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_unrequested_proof_and_receipt_carry_the_attempt_and_no_request_members() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
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

#[tokio::test]
async fn test_unrequested_next_generation_list_replaces_the_last_counter() {
    // Versions compare generation first, then counter: the shop's list at the next
    // generation's first counter is newer than one at the last counter of the generation
    // before, replaces it, and is the one paid (Paykit wire 4.13).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let last = list_version(1, pubky_switch_core::json::MAX_INT);
    let next = list_version(2, 1);
    w.list_to_alice(&mut shop, &[wallet.app_id()], last).await;
    wallet.receive(None).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], next).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.held_lists().await.unwrap().is_empty());
    // A list is paid from only once bob's verified identity has reached its generation
    // (Paykit wire 4.13): his identity moves to generation 2 first.
    let old = w.homeserver.identity_doc(&w.bob_id()).unwrap();
    w.bob
        .sign_next(|body| {
            body.service_keys
                .push(pubky_switch_core::records::ServiceKeyEntry {
                    generation: old.generation(),
                    key: old.body().inbox.service_key,
                });
            body.generation = old.generation() + 1;
        })
        .unwrap();
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let held = wallet
        .app()
        .records_of("payer_activity")
        .await
        .unwrap()
        .into_iter()
        .find_map(|r| r.content.value().map(<[u8]>::to_vec))
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&held).unwrap();
    assert_eq!(
        record["unrequested"]["list_version"],
        serde_json::to_value(next).unwrap()
    );
    assert_eq!(record["unrequested"]["payload"], single_use_payload(next));
}

// ---------------------------------------------------------------------------------------
// A later `payment_lists` write another own app makes (Paykit wire 5.4, "Seen lists")

/// A `private_payment_list` from bob's `shop` to alice at `version`, with one single-use
/// on-chain endpoint paying `payload`, as a record stores it: signed, not delivered.
fn stored_list(w: &World, version: paykit_lib::ListVersion, payload: &str) -> serde_json::Value {
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

/// The version of the list [`written_by_the_phone`] starts from.
fn phone_list_version() -> paykit_lib::ListVersion {
    list_version(1, 1)
}

/// The phone, another own app, writes the record `change` makes of the one it holds, a list of
/// [`phone_list_version`] paid to its single-use endpoint; the wallet then reads its feed. Returns what the wallet holds after, and its flagged writes.
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], phone_list_version())
        .await;
    wallet.receive(None).await.unwrap();
    // The wallet pays its single-use endpoint: the record marks it used.
    let paid = wallet
        .pay_without_request(to_bob(w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
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
        .submit_batch(pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(entry))
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
    let (before, flagged, after) = written_by_the_phone(&w, move |w, value| {
        // Another list at the version held: an equivocation, never taken.
        let other = stored_list(w, phone_list_version(), "bc1qother");
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
        let newer = phone_list_version().next().unwrap();
        value["list"] = stored_list(w, newer, "bc1qnewer");
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
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), phone.app_id()],
        list_version(1, 1),
    )
    .await;
    // The wallet pays the single-use endpoint: it is marked used in the record, in the
    // attempt's batch.
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();

    // The phone reads the list only now. It never saw the record read, so it takes the
    // value that doesn't read; it doesn't write the list over it, which would drop the
    // single-use mark, and keeps it until the record reads.
    phone.receive(None).await.unwrap();
    let held = phone.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].payee, w.bob_id());
    assert_eq!(held[0].version, list_version(1, 1));
    let refused = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_unrequested_list_after_a_delete_stays_held_and_its_single_use_endpoint_unpaid() {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, OpBatch, RecordTarget};

    let w = World::new();
    let storage = crate::common::store();
    let mut wallet = w.wallet_on("wallet", std::sync::Arc::clone(&storage)).await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
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
    let (_, marked) = list_record(&phone, &w).await;
    assert_eq!(marked["used"].as_array().unwrap().len(), 1);
    let op = phone
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let writes = w.record_writes(&w.alice, &key);

    // Bob sends a list of the same version again, the single-use endpoint the wallet paid
    // in it: written afresh, it would be payable again. It is held instead.
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let held = wallet.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    let waiting = |refused: Result<PayOutcome, paykit_sdk::PaykitError>| {
        assert!(
            matches!(
                refused.as_ref().err().and_then(|e| e.refusal()),
                Some(Refusal::WaitingForList { .. })
            ),
            "{refused:?}"
        );
    };
    waiting(
        wallet
            .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
            .await
            .outcome,
    );
    assert_eq!(w.rail.all_executions().len(), 1);

    // No receive, newer list or reload makes the single-use endpoint payable again: no SDK
    // app writes the record over the delete, so the list stays held.
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    wallet.receive(None).await.unwrap();
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, storage, app_id, std::sync::Arc::clone(&w.clock))
        .await;
    wallet.receive(None).await.unwrap();
    let held = wallet.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].version, list_version(1, 2));
    waiting(
        wallet
            .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
            .await
            .outcome,
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.record_writes(&w.alice, &key), writes);

    // A write over the delete that drops the mark the wallet saw is ignored: still held.
    let put = |value: &serde_json::Value| serde_json::to_vec(value).unwrap();
    let mut unmarked = marked.clone();
    unmarked["used"] = serde_json::json!([]);
    for value in [&unmarked, &marked] {
        let op = phone
            .app_mut()
            .record_op(&target, Some(&put(value)), Vec::new())
            .await
            .unwrap();
        phone
            .app_mut()
            .submit_batch(OpBatch::for_protocol("paykit").entry(op))
            .await
            .unwrap();
        wallet.receive(None).await.unwrap();
        wallet.receive(None).await.unwrap();
        if value == &unmarked {
            assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
        }
    }
    // One that keeps it places the held list with the mark.
    assert!(wallet.held_lists().await.unwrap().is_empty());
    let (_, placed) = list_record(&wallet, &w).await;
    assert_eq!(placed["used"].as_array().unwrap().len(), 1, "{placed}");
    assert_eq!(w.rail.all_executions().len(), 1);

    // The delete stays known after the placement: restored from a backup taken now, after
    // the service forgets the record, the list sent again over the absent key is held, never
    // written afresh without the mark.
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.alice.switch().drop_record(&key).unwrap();
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
    restored.receive(None).await.unwrap();
    assert_list_stays_held_and_unpaid(&w, &mut restored, &mut shop, &key).await;
}

#[tokio::test]
async fn test_unrequested_with_the_own_switch_down_refused_and_nothing_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    // Alice's switch stops answering: the SDK makes no payment without it (Paykit spec 8.4
    // allows one to a reusable public endpoint; this SDK pays attempt-first only).
    w.router.remove(w.alice.origin());
    let refused = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
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
        .outcome
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
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
        .outcome
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
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
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
    store_bundle_at(w, w.alice.switch(), app, cert, first);
}

pub(crate) fn store_bundle_at(
    w: &World,
    switch: &pubky_switch_client::testing::ScriptedSwitch,
    app: [u8; 16],
    cert: &pubky_switch_core::records::AppCert,
    first: u64,
) {
    use pubky_switch_client::app::Protocol as _;
    let epoch = switch.identity_doc().unwrap().data_epoch();
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
        switch
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
    // epoch, which the wallet holds unopened for now. A newer list is held. Once the
    // wallet's key comes, late, the record opens and the list is placed with the mark.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), phone.app_id()],
        list_version(1, 1),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
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
        .submit_batch(pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(op))
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.held_lists().await.unwrap().is_empty());
    // The key comes late: the record opens with its mark, and the list is placed on it.
    store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let (_, value) = list_record(&wallet, &w).await;
    assert_eq!(value["used"].as_array().unwrap().len(), 1, "{value}");
    assert_eq!(value["msg_ids"].as_array().unwrap().len(), 2, "{value}");
    assert!(wallet.held_lists().await.unwrap().is_empty());
}

/// The `paykit.payment_receipt` body of `attempt_id`, for a payment without a request: its
/// `refs` name only the proof.
fn receipt_body(attempt_id: paykit_lib::AttemptId) -> serde_json::Map<String, serde_json::Value> {
    paykit_lib::bodies::PaymentReceipt {
        attempt_id,
        billing_period: None,
        metadata: None,
        payment_reference: None,
    }
    .to_body(1)
    .unwrap()
}

#[tokio::test]
async fn test_unrequested_receipt_from_another_identity_refused() {
    // Carol sends alice a receipt for her payment to bob, naming its attempt and its proof.
    // She isn't the payment's payee, so the record takes nothing of it; bob's own receipt is
    // placed (Paykit wire 4.11, 5.3).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let _carol = w
        .join(
            &w.carol,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let PayOutcome::Paid { attempt_id, .. } = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
        .unwrap()
    else {
        panic!("paid");
    };
    let proof = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|sent| sent.kind == "paykit.payment_proof")
        .unwrap()
        .plaintext
        .hash();
    let from_carol = w.inject(
        &w.carol,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_receipt",
        vec![proof],
        receipt_body(attempt_id),
    );
    // Taken with no effect: nothing is written, and nothing is kept to write later.
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.contains(&from_carol), "{got:?}");
    assert!(wallet.payments().await.unwrap()[0].receipt.is_none());
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"se/")
        .await
        .unwrap()
        .is_empty());
    // Bob's receipt of the same payment goes beside the proof.
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_receipt",
        vec![proof],
        receipt_body(attempt_id),
    );
    wallet.receive(None).await.unwrap();
    assert!(wallet.payments().await.unwrap()[0].receipt.is_some());
}

#[tokio::test]
async fn test_unrequested_proof_of_a_settlement_seen_not_credited_again_over_its_delete() {
    // The shop credited alice's payment without a request; Bob's wallet then deletes the
    // record, and a proof of the same settlement comes again. The shop saw the record's
    // messages, so the proof is one for the record it saw: nothing is written over the
    // delete, and the payment isn't credited again (Paykit spec 8.1, "Records not listed
    // yet").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 1);
    seller_wallet.receive(None).await.unwrap();
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
    let paykit_lib::Body::PaymentProof(proof) = paykit_lib::Body::parse(&message).unwrap() else {
        panic!("a proof");
    };
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayeeActivity.name(),
            paykit_lib::collections::Collection::PayeeActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::payee_payment_record(
            &proof.payment_endpoint_identifier,
            &proof.settlement_id().unwrap(),
        ),
    );
    seller_wallet
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let key = shop.app().record_key(&target).unwrap();
    let written = w.record_writes(&w.bob, &key);
    let again = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        Vec::new(),
        message.body.clone(),
    );
    let got = shop.receive(None).await.unwrap();
    assert!(got.applied.contains(&again), "{got:?}");
    assert_eq!(w.record_writes(&w.bob, &key), written);
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(
        held.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
    assert!(shop.payee_activity().await.unwrap().is_empty());
}

/// One reusable Lightning endpoint with `payload`.
fn lightning(
    payload: &str,
) -> std::collections::BTreeMap<
    paykit_lib::PaymentEndpointIdentifier,
    paykit_lib::bodies::ListEndpoint,
> {
    std::collections::BTreeMap::from([(
        endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new(payload).unwrap(),
            single_use: false,
        },
    )])
}

/// The destination of `app`'s one payment without a request, as its activity record holds it.
async fn paid_destination(app: &Paykit) -> serde_json::Value {
    let held = app
        .app()
        .records_of("payer_activity")
        .await
        .unwrap()
        .into_iter()
        .find_map(|r| r.content.value().map(<[u8]>::to_vec))
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&held).unwrap();
    record["unrequested"].clone()
}

/// The `used` entries of `app`'s record of bob's shop's list.
async fn used_entries(app: &Paykit, w: &World) -> Vec<serde_json::Value> {
    let (_, held) = list_record(app, w).await;
    held.get("used")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[tokio::test]
async fn test_unrequested_list_above_the_payees_generation_not_paid() {
    // A list bob's shop signed at a generation his identity hasn't reached is no evidence of
    // it: it plays no part in a payment, and the payer waits for a list. Once bob's verified
    // identity reaches that generation, the same list is paid, not sent again (Paykit wire
    // 4.13).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let generation = w.homeserver.identity_doc(&w.bob_id()).unwrap().generation();
    let ahead = list_version(generation + 1, 1);
    w.list_to_alice(&mut shop, &[wallet.app_id()], ahead).await;
    assert_eq!(wallet.receive(None).await.unwrap().applied.len(), 1);
    // Installed by its pair before any payment.
    let (_, held) = list_record(&wallet, &w).await;
    assert!(held["list"].is_object(), "{held}");
    for id in ["btc-bitcoin-p2wpkh", "btc-lightning-bolt12"] {
        let refused = wallet
            .pay_without_request(to_bob(&w, "0.0015", id))
            .await
            .outcome
            .unwrap_err();
        assert!(
            matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
            "{refused:?}"
        );
    }
    assert!(w.rail.all_executions().is_empty());
    // The generation is checked before the reserving batch: nothing is reserved, and the
    // list's version is not consumed (Paykit spec 7.2).
    assert!(used_entries(&wallet, &w).await.is_empty());
    assert_eq!(list_requests(&mut shop).await, 1);
    // The request names the pair held, though the list isn't paid (Paykit wire 4.14).
    assert_eq!(
        crate::destinations::held_versions(&w, &wallet).await,
        vec![Some(ahead)]
    );
    // Bob's identity reaches the list's generation: once the payer's copy is read again,
    // past the 10 minutes a copy stays fresh (Paykit spec 7.2), the list held is paid.
    let old = w.homeserver.identity_doc(&w.bob_id()).unwrap();
    w.bob
        .sign_next(|body| {
            body.service_keys
                .push(pubky_switch_core::records::ServiceKeyEntry {
                    generation: old.generation(),
                    key: old.body().inbox.service_key,
                });
            body.generation = ahead.generation();
        })
        .unwrap();
    w.clock.advance_ms(HOUR_MS);
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let paid = paid_destination(&wallet).await;
    assert_eq!(paid["list_version"], serde_json::to_value(ahead).unwrap());
    assert_eq!(paid["payload"], single_use_payload(ahead));
    assert_eq!(used_entries(&wallet, &w).await.len(), 1);
}

#[tokio::test]
async fn test_unrequested_single_use_endpoint_used_consumes_the_whole_version() {
    // Paying one single-use endpoint of a list version consumes that version: neither its
    // other single-use endpoint nor its reusable one is paid, and the payee is asked for a
    // newer list. A mark at an older version consumes nothing of the newer one (Paykit wire
    // 5.4, spec 8.4).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let endpoints = |version: paykit_lib::ListVersion| {
        let mut endpoints = lightning("lno1reusable");
        for id in ["btc-bitcoin-p2tr", "btc-bitcoin-p2wpkh"] {
            endpoints.insert(
                endpoint(id),
                paykit_lib::bodies::ListEndpoint {
                    payload: paykit_lib::PaymentEndpointPayload::new(&format!(
                        "{}{id}",
                        single_use_payload(version)
                    ))
                    .unwrap(),
                    single_use: true,
                },
            );
        }
        endpoints
    };
    let first = list_version(1, 1);
    w.list_to_alice_with(&mut shop, &[wallet.app_id()], first, endpoints(first))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    shop.receive(None).await.unwrap();
    let refused = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), btc("0.0015")))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert_eq!(list_requests(&mut shop).await, 1);
    let refused = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    // A newer list: the endpoint used at the older version is paid again at this one.
    w.clock.advance_ms(1000);
    let second = list_version(1, 2);
    w.list_to_alice_with(&mut shop, &[wallet.app_id()], second, endpoints(second))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
    let used = used_entries(&wallet, &w).await;
    assert_eq!(used.len(), 2, "{used:?}");
    assert_eq!(used[1]["version"], serde_json::to_value(second).unwrap());
}

#[tokio::test]
async fn test_unrequested_list_of_the_held_pair_never_replaces_the_held_one() {
    // Two lists of one version from one app: the second never replaces the list the record
    // holds, which stays the one paid (Paykit wire 4.13, 5.4).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let version = paykit_lib::ListVersion::new(1, 5).unwrap();
    w.list_to_alice_with(
        &mut shop,
        &[wallet.app_id()],
        version,
        lightning("lno1first"),
    )
    .await;
    assert_eq!(wallet.receive(None).await.unwrap().applied.len(), 1);
    w.list_to_alice_with(
        &mut shop,
        &[wallet.app_id()],
        version,
        lightning("lno1second"),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let handed: Vec<String> = w
        .rail
        .selections()
        .into_iter()
        .flat_map(|(candidates, _)| candidates)
        .map(|c| c.payload.as_str().to_owned())
        .collect();
    assert_eq!(handed, vec!["lno1first".to_owned()]);
}

#[tokio::test]
async fn test_unrequested_reusable_endpoint_over_a_refused_marking_write_not_paid() {
    // The phone writes a record marking the list's version used, but holding another list at
    // that version: the wallet sees the write and refuses it, keeping the unmarked list it
    // selects from. Its attempts on the single-use and the reusable endpoint are conditioned
    // on the version of the write it took, not the refused one, fail, and nothing is paid (Paykit spec 8.4,
    // wire 5.4; Switch spec 8.2).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let version = list_version(1, 1);
    w.list_to_alice(&mut shop, &[wallet.app_id(), phone.app_id()], version)
        .await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let (target, mut value) = list_record(&phone, &w).await;
    value["list"] = stored_list(&w, version, "bc1qother");
    value["used"] = serde_json::json!([{
        "version": version,
        "endpoint": "btc-bitcoin-p2wpkh",
        "attempt_id": paykit_lib::AttemptId::generate().unwrap().to_text(),
    }]);
    let bytes = serde_json::to_vec(&value).unwrap();
    let entry = phone
        .app_mut()
        .record_op(&target, Some(&bytes), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(entry))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().flagged_writes().await.unwrap().len(), 1);
    assert!(used_entries(&wallet, &w).await.is_empty());

    for id in ["btc-bitcoin-p2wpkh", "btc-lightning-bolt12"] {
        let refused = wallet
            .pay_without_request(to_bob(&w, "0.0015", id))
            .await
            .outcome
            .unwrap_err();
        assert!(
            matches!(refused, paykit_sdk::PaykitError::Conflict(_)),
            "{id}: {refused:?}"
        );
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_unrequested_reusable_endpoint_of_a_version_used_elsewhere_not_paid() {
    // The wallet pays the list's single-use endpoint, which consumes the whole version. The
    // phone still holds the record unmarked: its attempt on the reusable endpoint of that
    // version is conditioned on the record's version, fails it, and nothing is paid; once it
    // reads the mark, the list is used up (Paykit spec 8.4, wire 5.4).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), phone.app_id()],
        list_version(1, 1),
    )
    .await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");

    let raced = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(raced, paykit_sdk::PaykitError::Conflict(_)),
        "{raced:?}"
    );
    phone.receive(None).await.unwrap();
    let used = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::WaitingForList { .. })),
        "{used:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_unrequested_list_held_over_a_forgotten_delete_stays_held_after_a_fresh_restore() {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, OpBatch, RecordTarget};

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    // The service forgets the delete, as it does once every own app's cursor passed it:
    // the key reads as absent and a create there starts afresh.
    let key = wallet.app().record_key(&target).unwrap();
    w.alice.switch().drop_record(&key).unwrap();
    let writes = w.record_writes(&w.alice, &key);

    // A fresh store restored from a backup holds the held list but not the delete.
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    crate::common::finish_listing(&mut restored).await;
    restored.reconcile().await.unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    restored.receive(None).await.unwrap();
    let again = restored
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(
        !matches!(again, Ok(PayOutcome::Paid { .. })),
        "the paid single-use endpoint was paid again: {again:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    assert_eq!(restored.held_lists().await.unwrap().len(), 1);
    // A newer list over the absent key is held too: the deleted write's version is unknown.
    w.list_to_alice(&mut shop, &[restored.app_id()], list_version(1, 2))
        .await;
    restored.receive(None).await.unwrap();
    let held = restored.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].version, list_version(1, 2));
    let refused = restored
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    // The user's way to pay this payee meanwhile: its public endpoints, a separate choice,
    // which never touches the held list or its single-use endpoint.
    crate::public::publish(
        &w,
        crate::public::tip_jar(),
        Some(w.clock.now_ms() + 30 * 24 * HOUR_MS),
    );
    let public = restored
        .pay_without_request(crate::public::public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(public, PayOutcome::Paid { .. }), "{public:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
    assert_eq!(restored.held_lists().await.unwrap().len(), 1);
    assert_eq!(w.record_writes(&w.alice, &key), writes);
}

/// The `payment_lists` record of bob's shop list, in alice's switch.
fn shop_list_target(w: &World, shop: &Paykit) -> pubky_switch_client::app::RecordTarget {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
    let collection = Collection::PaymentLists;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::payment_list(&w.bob_id(), &shop.app_id()),
    )
}

/// The wallet pays the single-use endpoint of bob's shop list, then the phone, another own
/// app, deletes the list's record. Returns the record's key and its value before the delete,
/// which marks the endpoint used.
async fn pay_single_use_then_delete_elsewhere(
    w: &World,
    wallet: &mut Paykit,
    phone: &mut Paykit,
    shop: &mut Paykit,
) -> ([u8; 32], serde_json::Value) {
    use pubky_switch_client::app::OpBatch;
    w.list_to_alice(shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let target = shop_list_target(w, shop);
    phone.app_mut().read_record(&target).await.unwrap();
    let (_, marked) = list_record(phone, w).await;
    let op = phone
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    (wallet.app().record_key(&target).unwrap(), marked)
}

/// Restore `backup` onto `storage`, list the restored app's side and end its reconcile.
async fn restored_onto(
    w: &World,
    storage: std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
    backup: &paykit_sdk::PaykitBackup,
) -> Paykit {
    let mut restored = Paykit::restore(
        w.env(&w.alice, storage),
        paykit_sdk::PaykitConfig::default(),
        backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    crate::common::finish_listing(&mut restored).await;
    restored.reconcile().await.unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    restored
}

/// After the forgotten delete, the shop sends the same list again: it stays held and its
/// paid single-use endpoint isn't paid again.
async fn assert_list_stays_held_and_unpaid(
    w: &World,
    restored: &mut Paykit,
    shop: &mut Paykit,
    key: &[u8; 32],
) {
    let writes = w.record_writes(&w.alice, key);
    w.list_to_alice(shop, &[restored.app_id()], list_version(1, 1))
        .await;
    restored.receive(None).await.unwrap();
    let again = restored
        .pay_without_request(to_bob(w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(
        !matches!(again, Ok(PayOutcome::Paid { .. })),
        "the paid single-use endpoint was paid again: {again:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.record_writes(&w.alice, key), writes);
    assert_eq!(restored.held_lists().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_unrequested_delete_seen_before_any_list_survives_a_restore_and_a_forget() {
    for in_place in [false, true] {
        let w = World::new();
        let storage = crate::common::store();
        let mut wallet = w.wallet_on("wallet", std::sync::Arc::clone(&storage)).await;
        let mut phone = w.wallet("phone").await;
        let mut shop = w.shop().await;
        let (key, _) =
            pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
        // The wallet sees the delete; no list follows before the backup.
        wallet.receive(None).await.unwrap();
        assert!(wallet.held_lists().await.unwrap().is_empty());
        let backup = wallet.backup().await.unwrap();
        drop(wallet);
        // The service forgets the delete: the key reads absent.
        w.alice.switch().drop_record(&key).unwrap();
        let onto = if in_place {
            storage
        } else {
            crate::common::store()
        };
        let mut restored = restored_onto(&w, onto, &backup).await;
        restored.receive(None).await.unwrap();
        assert_list_stays_held_and_unpaid(&w, &mut restored, &mut shop, &key).await;
    }
}

#[tokio::test]
async fn test_unrequested_hold_over_a_delete_survives_an_in_place_restore_from_before_it() {
    let w = World::new();
    let storage = crate::common::store();
    let mut wallet = w.wallet_on("wallet", std::sync::Arc::clone(&storage)).await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    // The backup is taken before the delete and before the hold.
    let backup = wallet.backup().await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let target = shop_list_target(&w, &shop);
    phone.app_mut().read_record(&target).await.unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    let key = wallet.app().record_key(&target).unwrap();
    drop(wallet);
    w.alice.switch().drop_record(&key).unwrap();
    // Restored in place: the installation saw the delete, which the older backup didn't.
    let mut restored = restored_onto(&w, storage, &backup).await;
    restored.receive(None).await.unwrap();
    assert_list_stays_held_and_unpaid(&w, &mut restored, &mut shop, &key).await;
}

#[tokio::test]
async fn test_unrequested_placement_settling_late_keeps_a_newer_hold() {
    use pubky_switch_client::app::OpBatch;
    use pubky_switch_client::testing::Fault;

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (key, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    wallet.receive(None).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    // The phone puts the record back with the mark; the wallet places its held list there,
    // and the answer to that write is lost: its batch is kept, to settle later.
    let op = phone
        .app_mut()
        .record_op(
            &target,
            Some(&serde_json::to_vec(&marked).unwrap()),
            Vec::new(),
        )
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    let _ = wallet.receive(None).await;
    // Before it settles, the record is deleted again and a newer list is held over it.
    phone.app_mut().read_record(&target).await.unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 3))
        .await;
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    let _ = wallet.receive(None).await;
    let held = wallet.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].version, list_version(1, 3));
    // The kept batch settles: the newer hold stays, and the paid endpoint stays unpaid
    // after the service forgets the delete.
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let held = wallet.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].version, list_version(1, 3));
    w.alice.switch().drop_record(&key).unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 3))
        .await;
    wallet.receive(None).await.unwrap();
    let again = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(
        !matches!(again, Ok(PayOutcome::Paid { .. })),
        "the paid single-use endpoint was paid again: {again:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_unrequested_list_held_for_a_refused_write_stays_held_over_a_later_delete() {
    use pubky_switch_client::app::OpBatch;

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    // A newer list whose record write is refused is held, the record still reading.
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    crate::common::refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    // Then another own app deletes the record, the wallet sees it, and the service forgets it.
    let target = shop_list_target(&w, &shop);
    phone.app_mut().read_record(&target).await.unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.alice.switch().drop_record(&key).unwrap();
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
    restored.receive(None).await.unwrap();
    let writes = w.record_writes(&w.alice, &key);
    let again = restored
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(
        !matches!(again, Ok(PayOutcome::Paid { .. })),
        "the paid single-use endpoint was paid again: {again:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    let held = restored.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].version, list_version(1, 2));
}
