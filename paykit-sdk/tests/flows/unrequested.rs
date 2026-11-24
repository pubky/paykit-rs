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

/// A store whose reads of Paykit's kept list deletes (`lgone/`) fail once `lost_reads` is
/// armed, and whose commits keeping a batch the client signed fail once `batch_keeps` is.
#[derive(Default)]
struct FailingListStore {
    inner: pubky_switch_client::store::MemoryStore,
    lost_reads: std::sync::atomic::AtomicBool,
    batch_keeps: std::sync::atomic::AtomicBool,
}

impl FailingListStore {
    /// Whether `batch` is to fail: armed, it keeps a batch, as its next creation number shows.
    fn fails(&self, batch: &pubky_switch_client::store::WriteBatch) -> bool {
        use pubky_switch_client::store::Write;
        self.batch_keeps.load(std::sync::atomic::Ordering::SeqCst)
            && batch.writes().iter().any(|write| match write {
                Write::Put { key, .. } | Write::Delete { key, .. } => key.ends_with(b"keptn"),
                Write::DeletePrefix { .. } => false,
            })
    }
}

impl pubky_switch_client::store::StateStore for FailingListStore {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        if self.lost_reads.load(std::sync::atomic::Ordering::SeqCst)
            && key.windows(6).any(|part| part == b"lgone/")
        {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
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
        if self.fails(&batch) {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        self.inner.commit(batch)
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, pubky_switch_client::error::StoreError>
    {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        if self.fails(&batch) {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        self.inner.commit_if(ns, owner, batch)
    }
}

#[tokio::test]
async fn test_unrequested_held_list_placement_reports_a_store_failure_keeping_its_hold() {
    let w = World::new();
    let failing = std::sync::Arc::new(FailingListStore::default());
    let mut wallet = w
        .wallet_on(
            "wallet",
            std::sync::Arc::clone(&failing)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    wallet.receive(None).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    // The store fails reading the kept delete while the held list is placed: the receive
    // reports it, rather than end as if the hold were checked (Paykit spec 6).
    failing
        .lost_reads
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(wallet.receive(None).await.is_err());
    failing
        .lost_reads
        .store(false, std::sync::atomic::Ordering::SeqCst);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_unrequested_held_list_placement_reports_a_failed_batch_keep_keeping_its_hold() {
    let w = World::new();
    let failing = std::sync::Arc::new(FailingListStore::default());
    let mut wallet = w
        .wallet_on(
            "wallet",
            std::sync::Arc::clone(&failing)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    // A newer list whose record write is refused is held, the record still reading.
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    crate::common::refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    // The store fails keeping the placement's batch: the receive reports it, rather than
    // end as if the write were sent or refused by the switch (Paykit spec 6).
    failing
        .batch_keeps
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = wallet.receive(None).await.unwrap_err();
    assert!(
        matches!(
            error,
            paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Store(_))
        ),
        "{error:?}"
    );
    failing
        .batch_keeps
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let held = wallet.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].version, list_version(1, 2));
    // Healed, the next receives place it.
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.held_lists().await.unwrap().is_empty());
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

    // A write over the delete, one dropping the mark the wallet saw or one keeping it,
    // places nothing: a later authorization over a delete is withheld (Paykit spec 6; wire
    // 5, "Incomplete protected-record contract").
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
        let held = wallet.held_lists().await.unwrap();
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].version, list_version(1, 2));
        waiting(
            wallet
                .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
                .await
                .outcome,
        );
    }
    assert_eq!(w.rail.all_executions().len(), 1);

    // The delete stays known: restored from a backup taken now, after the service forgets
    // the record, the list sent again over the absent key is held, never written afresh
    // without the mark.
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
            &paykit_sdk::rail::fake_settlement_id(&proof.proof).unwrap(),
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
    payment_list_target(&w.bob_id(), &shop.app_id())
}

/// The `payment_lists` record of the list of `payee`'s app `app_id`.
fn payment_list_target(
    payee: &[u8; 32],
    app_id: &[u8; 16],
) -> pubky_switch_client::app::RecordTarget {
    use paykit_lib::collections::Collection;
    use pubky_switch_client::app::{Collection as SwitchCollection, RecordTarget};
    let collection = Collection::PaymentLists;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::payment_list(payee, app_id),
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

#[tokio::test]
async fn test_unrequested_unmarked_create_over_a_forgotten_delete_keeps_the_list_held() {
    for fresh in [false, true] {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let mut phone = w.wallet("phone").await;
        let mut shop = w.shop().await;
        let (key, _) =
            pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
        // The wallet sees the delete; the service then forgets it.
        wallet.receive(None).await.unwrap();
        w.alice.switch().drop_record(&key).unwrap();
        let mut wallet = if fresh {
            let backup = wallet.backup().await.unwrap();
            drop(wallet);
            let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
            restored.receive(None).await.unwrap();
            restored
        } else {
            wallet
        };
        // A newly joined own app, which never held the record, writes the list afresh,
        // without the mark.
        let writes = w.record_writes(&w.alice, &key);
        let mut tablet = w.wallet("tablet").await;
        w.list_to_alice(&mut shop, &[tablet.app_id()], list_version(1, 1))
            .await;
        tablet.receive(None).await.unwrap();
        assert_eq!(w.record_writes(&w.alice, &key), writes + 1);
        wallet.receive(None).await.unwrap();
        assert_list_stays_held_and_unpaid(&w, &mut wallet, &mut shop, &key).await;
        // The payee can still be paid meanwhile: a request's own destination is.
        let request = crate::common::request_from(
            &mut shop,
            w.alice_id(),
            crate::common::draft("1", w.clock.now_ms()),
        )
        .await;
        wallet.receive(None).await.unwrap();
        let paid = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap();
        assert!(
            matches!(paid, PayOutcome::Paid { .. }),
            "fresh {fresh}: {paid:?}"
        );
        assert_eq!(w.rail.all_executions().len(), 2, "fresh {fresh}");
    }
}

#[tokio::test]
#[ignore = "the client tells Paykit only of other apps' deletes: a delete by another \
            installation of this App ID, which only a faulty own app writes, is not observed \
            until the client reports it"]
async fn test_unrequested_delete_by_another_installation_of_the_app_keeps_a_later_list_held() {
    use pubky_switch_client::app::OpBatch;

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
    // A second installation of the same app, restored from its backup.
    let backup = wallet.backup().await.unwrap();
    let mut second = restored_onto(&w, crate::common::store(), &backup).await;
    second.receive(None).await.unwrap();
    let target = shop_list_target(&w, &shop);
    let key = second.app().record_key(&target).unwrap();
    assert_eq!(
        list_record(&second, &w).await.1["used"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // The first installation deletes the record; the second sees the delete, and the
    // service then forgets it.
    wallet.app_mut().read_record(&target).await.unwrap();
    let op = wallet
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    second.receive(None).await.unwrap();
    assert!(matches!(
        second
            .app()
            .shared_record(&key)
            .await
            .unwrap()
            .map(|r| r.content),
        Some(pubky_switch_client::app::RecordContent::Deleted)
    ));
    // The second installation is backed up and restored onto a fresh store; the service
    // forgets the delete.
    let backup = second.backup().await.unwrap();
    drop(second);
    w.alice.switch().drop_record(&key).unwrap();
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
    restored.receive(None).await.unwrap();
    assert_list_stays_held_and_unpaid(&w, &mut restored, &mut shop, &key).await;
}

#[tokio::test]
#[ignore = "the client makes a value the protocol holds the record's seen mark, then flags the put-back of the known history as an older list; it needs the client to keep such a value out of the mark"]
async fn test_unrequested_put_back_of_the_known_history_after_a_newer_recreation_places_the_list() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (key, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    wallet.receive(None).await.unwrap();
    // The phone puts the record back with the mark, and pays a newer list's single-use
    // endpoint: the wallet reads both marks.
    put_list_record(&mut phone, &target, Some(&marked)).await;
    wallet.receive(None).await.unwrap();
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), phone.app_id()],
        list_version(1, 2),
    )
    .await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let paid = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    wallet.receive(None).await.unwrap();
    let (_, both) = list_record(&wallet, &w).await;
    assert_eq!(both["used"].as_array().unwrap().len(), 2, "{both}");
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    // The record is deleted again, the service forgets it, and the wallet is restored onto
    // a fresh store: what it read is gone with the store.
    phone.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut phone, &target, None).await;
    w.alice.switch().drop_record(&key).unwrap();
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
    restored.receive(None).await.unwrap();
    // A newly joined app takes a newer list, the third, sent to it and the wallet, and
    // creates the record with it and none of the marks. The restored wallet reads that
    // first and holds the list.
    let mut tablet = w.wallet("tablet").await;
    w.list_to_alice(
        &mut shop,
        &[tablet.app_id(), restored.app_id()],
        list_version(1, 3),
    )
    .await;
    tablet.receive(None).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(restored.held_lists().await.unwrap().len(), 1);
    // Another app puts back the very copy the wallet kept, the second list with both
    // marks: the known history is back, and the wallet takes it (Paykit spec 6).
    let mut laptop = w.wallet("laptop").await;
    laptop.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut laptop, &target, Some(&both)).await;
    // One receive takes the put-back and places the newer list over it; the next reads
    // the placement.
    for _ in 0..2 {
        restored.receive(None).await.unwrap();
    }
    assert_eq!(restored.held_lists().await.unwrap().len(), 0);
    // The record now holds the newer list with both marks.
    let (_, placed) = list_record(&restored, &w).await;
    assert_eq!(placed["used"], both["used"], "{placed}");
    assert_ne!(placed["list"], both["list"], "{placed}");
}

#[tokio::test]
async fn test_unrequested_marks_read_after_a_delete_kept_through_a_fresh_restore() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (key, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    wallet.receive(None).await.unwrap();
    // The phone puts the record back with the mark, and pays a newer list's single-use
    // endpoint: the wallet reads both marks.
    put_list_record(&mut phone, &target, Some(&marked)).await;
    wallet.receive(None).await.unwrap();
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), phone.app_id()],
        list_version(1, 2),
    )
    .await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let paid = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    wallet.receive(None).await.unwrap();
    let (_, both) = list_record(&wallet, &w).await;
    assert_eq!(both["used"].as_array().unwrap().len(), 2, "{both}");
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    // The record is deleted again, the service forgets it, and the wallet is restored onto
    // a fresh store: what it read is gone with the store.
    phone.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut phone, &target, None).await;
    w.alice.switch().drop_record(&key).unwrap();
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
    restored.receive(None).await.unwrap();
    // A newly joined app writes the newer list with the first mark only.
    let mut first_only = both.clone();
    first_only["used"] = marked["used"].clone();
    let mut tablet = w.wallet("tablet").await;
    put_list_record(&mut tablet, &target, Some(&first_only)).await;
    restored.receive(None).await.unwrap();
    let writes = w.record_writes(&w.alice, &key);
    w.list_to_alice(&mut shop, &[restored.app_id()], list_version(1, 2))
        .await;
    restored.receive(None).await.unwrap();
    let again = restored
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(
        !matches!(again, Ok(PayOutcome::Paid { .. })),
        "the paid single-use endpoint was paid again: {again:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 2);
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    assert_eq!(restored.held_lists().await.unwrap().len(), 1);
}

/// `app` writes `value`, or a delete, at the `payment_lists` record `target`, outside the
/// SDK's own writes.
async fn put_list_record(
    app: &mut Paykit,
    target: &pubky_switch_client::app::RecordTarget,
    value: Option<&serde_json::Value>,
) {
    let bytes = value.map(|v| serde_json::to_vec(v).unwrap());
    let op = app
        .app_mut()
        .record_op(target, bytes.as_deref(), Vec::new())
        .await
        .unwrap();
    app.app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_unrequested_delete_seen_again_keeps_the_list_held_over_a_put_back() {
    for restore in [false, true] {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let mut phone = w.wallet("phone").await;
        let mut shop = w.shop().await;
        let (_, marked) =
            pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
        let target = shop_list_target(&w, &shop);
        // The wallet sees the delete, and holds a newer list over it.
        wallet.receive(None).await.unwrap();
        w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
            .await;
        wallet.receive(None).await.unwrap();
        assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
        // The same delete is reported again: by a read while it is current, or by the
        // listing of a restore onto a fresh store.
        let mut wallet = if restore {
            let backup = wallet.backup().await.unwrap();
            drop(wallet);
            let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
            restored.receive(None).await.unwrap();
            restored
        } else {
            wallet.app_mut().read_record(&target).await.unwrap();
            wallet
        };
        // Another own app puts the record back holding every mark: the list stays held,
        // and nothing is paid from it (Paykit spec 6; wire 5, "Incomplete protected-record
        // contract").
        put_list_record(&mut phone, &target, Some(&marked)).await;
        wallet.receive(None).await.unwrap();
        wallet.receive(None).await.unwrap();
        assert_eq!(
            wallet.held_lists().await.unwrap().len(),
            1,
            "restore {restore}"
        );
        let again = wallet
            .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
            .await
            .outcome;
        assert!(
            !matches!(again, Ok(PayOutcome::Paid { .. })),
            "restore {restore}: {again:?}"
        );
        assert_eq!(w.rail.all_executions().len(), 1, "restore {restore}");
    }
}

#[tokio::test]
async fn test_unrequested_older_list_over_a_forgotten_delete_not_paid() {
    for fresh in [false, true] {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let mut phone = w.wallet("phone").await;
        let mut shop = w.shop().await;
        w.list_to_alice_with(
            &mut shop,
            &[wallet.app_id()],
            list_version(1, 1),
            lightning("lno1withdrawn"),
        )
        .await;
        wallet.receive(None).await.unwrap();
        let target = shop_list_target(&w, &shop);
        phone.app_mut().read_record(&target).await.unwrap();
        let (_, older) = list_record(&phone, &w).await;
        // The payee replaces its endpoint with a newer list; the wallet takes it.
        w.list_to_alice_with(
            &mut shop,
            &[wallet.app_id()],
            list_version(1, 2),
            lightning("lno1newoffer"),
        )
        .await;
        wallet.receive(None).await.unwrap();
        // The record is deleted, the wallet sees it, and the service forgets it.
        phone.app_mut().read_record(&target).await.unwrap();
        put_list_record(&mut phone, &target, None).await;
        wallet.receive(None).await.unwrap();
        let key = wallet.app().record_key(&target).unwrap();
        w.alice.switch().drop_record(&key).unwrap();
        // Restored onto a fresh store, the wallet keeps only what it knew of the delete.
        let mut wallet = if fresh {
            let backup = wallet.backup().await.unwrap();
            drop(wallet);
            let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
            restored.receive(None).await.unwrap();
            restored
        } else {
            wallet
        };
        // A newly joined app writes the older list back, holding every mark.
        let mut tablet = w.wallet("tablet").await;
        put_list_record(&mut tablet, &target, Some(&older)).await;
        wallet.receive(None).await.unwrap();
        let again = wallet
            .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
            .await
            .outcome;
        assert!(
            !matches!(again, Ok(PayOutcome::Paid { .. })),
            "fresh {fresh}: a withdrawn endpoint was paid: {again:?}"
        );
        assert!(w.rail.all_executions().is_empty());
    }
}

#[tokio::test]
async fn test_unrequested_other_list_at_the_same_version_over_a_forgotten_delete_not_paid() {
    for fresh in [false, true] {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let mut phone = w.wallet("phone").await;
        let mut shop = w.shop().await;
        w.list_to_alice_with(
            &mut shop,
            &[wallet.app_id()],
            list_version(1, 2),
            lightning("lno1newoffer"),
        )
        .await;
        wallet.receive(None).await.unwrap();
        let target = shop_list_target(&w, &shop);
        // The record is deleted, the wallet sees it, and the service forgets it.
        phone.app_mut().read_record(&target).await.unwrap();
        put_list_record(&mut phone, &target, None).await;
        wallet.receive(None).await.unwrap();
        let key = wallet.app().record_key(&target).unwrap();
        w.alice.switch().drop_record(&key).unwrap();
        // Restored onto a fresh store, the wallet keeps only what it knew of the delete.
        let mut wallet = if fresh {
            let backup = wallet.backup().await.unwrap();
            drop(wallet);
            let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
            restored.receive(None).await.unwrap();
            restored
        } else {
            wallet
        };
        // A newly joined app writes another list signed at the same version: equivocation, which
        // the version rule refuses (Paykit wire 5.4, "Seen lists").
        let mut laptop = w.wallet("laptop").await;
        w.list_to_alice_with(
            &mut shop,
            &[laptop.app_id(), wallet.app_id()],
            list_version(1, 2),
            lightning("lno1other"),
        )
        .await;
        laptop.receive(None).await.unwrap();
        wallet.receive(None).await.unwrap();
        let again = wallet
            .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
            .await
            .outcome;
        assert!(
            !matches!(again, Ok(PayOutcome::Paid { .. })),
            "fresh {fresh}: another list at the seen version was paid: {again:?}"
        );
        assert!(w.rail.all_executions().is_empty());
    }
}

#[tokio::test]
async fn test_unrequested_delete_over_a_write_this_app_couldnt_read_keeps_no_copy() {
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
    let (target, unmarked) = list_record(&wallet, &w).await;
    assert!(used_entries(&wallet, &w).await.is_empty());
    // The data epoch rotates; the phone gets the new key, the wallet not yet. The phone pays
    // the single-use endpoint: its marking write is sealed at the new epoch.
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    let paid = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    wallet.receive(None).await.unwrap();
    // The phone deletes the record: the wallet's copy is older than what the delete took.
    phone.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut phone, &target, None).await;
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    w.alice.switch().drop_record(&key).unwrap();
    // The wallet's key comes, and a newly joined app writes the copy it held back.
    store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
    wallet.receive(None).await.unwrap();
    let mut tablet = w.wallet("tablet").await;
    put_list_record(&mut tablet, &target, Some(&unmarked)).await;
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
async fn test_unrequested_another_payee_apps_record_at_a_key_never_its_copy() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let second = w
        .join(
            &w.bob,
            "second",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    // The phone holds the shop's list record, a single-use endpoint only, and writes it at
    // the second shop app's key.
    let single_use = std::collections::BTreeMap::from([(
        endpoint("btc-bitcoin-p2wpkh"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new(&single_use_payload(list_version(
                1, 1,
            )))
            .unwrap(),
            single_use: true,
        },
    )]);
    w.list_to_alice_with(&mut shop, &[phone.app_id()], list_version(1, 1), single_use)
        .await;
    phone.receive(None).await.unwrap();
    let (_, foreign) = list_record(&phone, &w).await;
    let target = payment_list_target(&w.bob_id(), &second.app_id());
    put_list_record(&mut phone, &target, Some(&foreign)).await;
    wallet.receive(None).await.unwrap();
    // The phone deletes it and the wallet sees the delete; the service forgets it, and the
    // wallet is restored onto a fresh store.
    phone.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut phone, &target, None).await;
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.alice.switch().drop_record(&key).unwrap();
    let mut wallet = restored_onto(&w, crate::common::store(), &backup).await;
    wallet.receive(None).await.unwrap();
    // A newly joined app writes the second shop's own list there.
    let mut tablet = w.wallet("tablet").await;
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: list_version(1, 2),
        payment_endpoints: lightning("lno1second"),
        expires_at: w.clock.now_ms() + 30 * 24 * HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "second",
        &w.alice,
        &[tablet.app_id(), wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    tablet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), 3);
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-lightning-bolt12"))
        .await
        .outcome;
    assert!(
        !matches!(paid, Ok(PayOutcome::Paid { .. })),
        "a list over a delete seen with no copy of its own was paid: {paid:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_unrequested_put_back_carrying_lost_to_delete_not_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    wallet.receive(None).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    wallet.receive(None).await.unwrap();
    // A put-back from an older copy, holding every mark and naming the delete it answered:
    // never paid automatically (Paykit wire 5).
    let mut put_back = marked.clone();
    put_back["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[5u8; 32]));
    put_list_record(&mut phone, &target, Some(&put_back)).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    let again = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(!matches!(again, Ok(PayOutcome::Paid { .. })), "{again:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_unrequested_write_over_a_delete_dropping_a_seen_mark_flagged() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().flagged_writes().await.unwrap().is_empty());
    // The phone writes the record back over the delete without the mark the wallet saw:
    // the wallet ignores it and shows it with its writer (Paykit wire 5.4 "Seen lists").
    let mut unmarked = marked.clone();
    unmarked["used"] = serde_json::json!([]);
    put_list_record(&mut phone, &target, Some(&unmarked)).await;
    wallet.receive(None).await.unwrap();
    let flagged = wallet.app().flagged_writes().await.unwrap();
    assert_eq!(flagged.len(), 1, "{flagged:?}");
}

#[tokio::test]
async fn test_unrequested_list_over_another_payee_apps_record_at_its_key_held() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let second = w
        .join(
            &w.bob,
            "second",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    // The phone writes the shop's list record at the second shop app's key.
    w.list_to_alice(&mut shop, &[phone.app_id()], list_version(1, 1))
        .await;
    phone.receive(None).await.unwrap();
    let (_, foreign) = list_record(&phone, &w).await;
    let target = payment_list_target(&w.bob_id(), &second.app_id());
    put_list_record(&mut phone, &target, Some(&foreign)).await;
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let writes = w.record_writes(&w.alice, &key);
    // The second shop's own list, at the foreign record's version, is never written over it
    // nor dropped against it: that record doesn't bind at the key.
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: list_version(1, 1),
        payment_endpoints: lightning("lno1second"),
        expires_at: w.clock.now_ms() + 30 * 24 * HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "second",
        &w.alice,
        &[wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_unrequested_delete_over_a_refused_write_keeps_no_copy() {
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
    let target = shop_list_target(&w, &shop);
    phone.app_mut().read_record(&target).await.unwrap();
    let (_, marked) = list_record(&phone, &w).await;
    let mut unmarked = marked.clone();
    unmarked["used"] = serde_json::json!([]);
    // Another own app drops the mark, which the wallet refuses, then deletes that write.
    put_list_record(&mut phone, &target, Some(&unmarked)).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().flagged_writes().await.unwrap().len(), 1);
    put_list_record(&mut phone, &target, None).await;
    wallet.receive(None).await.unwrap();
    // Restored onto a fresh store from a backup taken after the delete, the wallet reads a
    // write without the mark over the delete: it is never paid from.
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
    restored.receive(None).await.unwrap();
    put_list_record(&mut phone, &target, Some(&unmarked)).await;
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
}

/// The `lgone/` entries of `backup`'s Paykit layer, each as stored.
fn lost_entries(backup: &paykit_sdk::PaykitBackup) -> Vec<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(&backup.to_bytes().unwrap()).unwrap();
    let data = value["layers"][0]["data"].as_str().unwrap();
    let bytes = pubky_switch_core::enc::unb64(data).unwrap();
    pubky_switch_client::app::decode_local_state(&bytes)
        .unwrap()
        .into_iter()
        .filter(|(key, _)| key.starts_with(b"lgone/"))
        .map(|(_, value)| {
            let stored: serde_json::Value = serde_json::from_slice(&value).unwrap();
            stored.get("d").cloned().unwrap_or(stored)
        })
        .collect()
}

/// `marked` with its `used` emptied and `lost_to_delete` naming the delete `named`.
fn marked_put_back(marked: &serde_json::Value, named: u8) -> serde_json::Value {
    let mut put_back = marked.clone();
    put_back["used"] = serde_json::json!([]);
    put_back["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[named; 32]));
    put_back
}

#[tokio::test]
async fn test_unrequested_loss_marker_kept_only_for_a_list_bound_to_its_key() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let second = w
        .join(
            &w.bob,
            "second",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    w.list_to_alice(&mut shop, &[phone.app_id()], list_version(1, 1))
        .await;
    phone.receive(None).await.unwrap();
    let (_, foreign) = list_record(&phone, &w).await;
    // The shop's list, marked, written at the second shop app's key, and the shop's own
    // record with its outer payee changed: neither is that key's, so neither keeps a mark of
    // a delete there (Paykit wire 5.4).
    let target = payment_list_target(&w.bob_id(), &second.app_id());
    put_list_record(&mut phone, &target, Some(&marked_put_back(&foreign, 5))).await;
    wallet.receive(None).await.unwrap();
    assert!(lost_entries(&wallet.backup().await.unwrap()).is_empty());
    let own = shop_list_target(&w, &shop);
    let mut payee = marked_put_back(&foreign, 6);
    payee["payee"] = serde_json::json!(pubky_switch_core::enc::b64(&w.alice_id()));
    phone.app_mut().read_record(&own).await.unwrap();
    put_list_record(&mut phone, &own, Some(&payee)).await;
    wallet.receive(None).await.unwrap();
    assert!(lost_entries(&wallet.backup().await.unwrap()).is_empty());
}

#[tokio::test]
async fn test_unrequested_loss_marker_kept_when_the_rest_of_its_value_does_not_read() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    // A put-back naming the delete whose `used` doesn't read: its payee and app still bind
    // the key, so the marker is kept (Paykit spec 6; wire 5).
    let mut put_back = marked_put_back(&marked, 5);
    put_back["used"] = serde_json::json!([{ "endpoint": 7 }]);
    put_list_record(&mut phone, &target, Some(&put_back)).await;
    let mut tablet = w.wallet("tablet").await;
    tablet.receive(None).await.unwrap();
    let kept = lost_entries(&tablet.backup().await.unwrap());
    assert_eq!(kept.len(), 1, "{kept:?}");
}

#[tokio::test]
async fn test_unrequested_two_loss_markers_keep_the_newer_write() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (key, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    let mut tablet = w.wallet("tablet").await;
    put_list_record(&mut phone, &target, Some(&marked_put_back(&marked, 5))).await;
    tablet.receive(None).await.unwrap();
    phone.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut phone, &target, Some(&marked_put_back(&marked, 6))).await;
    tablet.receive(None).await.unwrap();
    let held = tablet.app().shared_record(&key).await.unwrap().unwrap();
    let kept = lost_entries(&tablet.backup().await.unwrap());
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(
        kept[0]["deleted"],
        serde_json::json!([held.version.generation, held.version.version])
    );
}

#[tokio::test]
async fn test_unrequested_loss_marker_written_by_another_installation_of_the_app_kept() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    // A second installation of the tablet's app, restored onto another store, writes the
    // put-back; the tablet reads it.
    let mut tablet = w.wallet("tablet").await;
    let mut other =
        restored_onto(&w, crate::common::store(), &tablet.backup().await.unwrap()).await;
    assert_eq!(other.app_id(), tablet.app_id());
    other.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut other, &target, Some(&marked_put_back(&marked, 5))).await;
    tablet.receive(None).await.unwrap();
    let kept = lost_entries(&tablet.backup().await.unwrap());
    assert_eq!(kept.len(), 1, "{kept:?}");
}

#[tokio::test]
async fn test_unrequested_loss_marker_read_first_kept_through_a_fresh_restore() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut tablet = w.wallet("tablet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    // One list to the wallet and the tablet. The wallet pays its single-use endpoint; the
    // tablet holds the list but doesn't read the record yet.
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), tablet.app_id()],
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
    let target = shop_list_target(&w, &shop);
    let key = wallet.app().record_key(&target).unwrap();
    phone.app_mut().read_record(&target).await.unwrap();
    let (_, marked) = list_record(&phone, &w).await;
    // The record is deleted, the service forgets the delete, and the phone puts it back
    // from an older copy without the mark, naming the delete. The tablet first learns of
    // the delete from that put-back, then is backed up.
    put_list_record(&mut phone, &target, None).await;
    w.alice.switch().drop_record(&key).unwrap();
    let mut put_back = marked.clone();
    put_back["used"] = serde_json::json!([]);
    put_back["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[5u8; 32]));
    let mut laptop = w.wallet("laptop").await;
    put_list_record(&mut laptop, &target, Some(&put_back)).await;
    tablet.receive(None).await.unwrap();
    let backup = tablet.backup().await.unwrap();
    drop(tablet);
    // A later write drops the marker. Restored onto a fresh store, which holds none of what
    // the client saw at the record, the tablet reads only that write: it is still no list
    // to pay from, as the marker read is kept in Paykit's own state, which the backup
    // carries (Paykit spec 6, wire 5).
    let mut dropped = put_back.clone();
    dropped.as_object_mut().unwrap().remove("lost_to_delete");
    put_list_record(&mut laptop, &target, Some(&dropped)).await;
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
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
    assert_eq!(restored.held_lists().await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "the client forgets what it saw of a record at a restore, so after a fresh restore it can't show the writer of a write that drops a marker it saw before; it needs the client to keep that through a backup and restore"]
async fn test_unrequested_write_dropping_a_loss_marker_after_a_fresh_restore_shown_with_its_writer()
{
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut tablet = w.wallet("tablet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    // One list to the wallet and the tablet. The wallet pays its single-use endpoint; the
    // tablet holds the list but doesn't read the record yet.
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), tablet.app_id()],
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
    let target = shop_list_target(&w, &shop);
    let key = wallet.app().record_key(&target).unwrap();
    phone.app_mut().read_record(&target).await.unwrap();
    let (_, marked) = list_record(&phone, &w).await;
    // The record is deleted, the service forgets the delete, and the phone puts it back
    // from an older copy without the mark, naming the delete. The tablet first learns of
    // the delete from that put-back, then is backed up.
    put_list_record(&mut phone, &target, None).await;
    w.alice.switch().drop_record(&key).unwrap();
    let mut put_back = marked.clone();
    put_back["used"] = serde_json::json!([]);
    put_back["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[5u8; 32]));
    let mut laptop = w.wallet("laptop").await;
    put_list_record(&mut laptop, &target, Some(&put_back)).await;
    tablet.receive(None).await.unwrap();
    let backup = tablet.backup().await.unwrap();
    drop(tablet);
    // A later write drops the marker. Restored onto a fresh store, which holds none of what
    // the client saw at the record, the tablet reads only that write: it is still no list
    // to pay from, as the marker read is kept in Paykit's own state, which the backup
    // carries (Paykit spec 6, wire 5).
    let mut dropped = put_back.clone();
    dropped.as_object_mut().unwrap().remove("lost_to_delete");
    put_list_record(&mut laptop, &target, Some(&dropped)).await;
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
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
    assert_eq!(restored.held_lists().await.unwrap().len(), 1);
    // The write that dropped the marker is shown with its writer (Paykit wire 5).
    let flagged = restored.app().flagged_writes().await.unwrap();
    assert_eq!(flagged.len(), 1, "{flagged:?}");
    assert_eq!(flagged[0].key, key);
    assert_eq!(flagged[0].writer, laptop.app_id());
}

#[tokio::test]
#[ignore = "the client never shows the app a write it flags, and forgets what it saw of the record at a restore, so the paid single-use endpoint is paid again; it needs the client to keep that through a backup and restore"]
async fn test_unrequested_flagged_put_back_then_a_dropped_marker_not_paid_after_a_fresh_restore() {
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
    let target = shop_list_target(&w, &shop);
    let key = wallet.app().record_key(&target).unwrap();
    phone.app_mut().read_record(&target).await.unwrap();
    let (_, marked) = list_record(&phone, &w).await;
    // The record is deleted and the service forgets the delete. A put-back from an older
    // copy, without the mark and naming the delete, is flagged by the wallet for dropping
    // the mark it saw, so Paykit never sees its marker. The wallet is then backed up.
    put_list_record(&mut phone, &target, None).await;
    w.alice.switch().drop_record(&key).unwrap();
    let mut put_back = marked.clone();
    put_back["used"] = serde_json::json!([]);
    put_back["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[5u8; 32]));
    let mut laptop = w.wallet("laptop").await;
    put_list_record(&mut laptop, &target, Some(&put_back)).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().flagged_writes().await.unwrap().len(), 1);
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    // A later write drops the marker. Restored onto a fresh store, the wallet reads only
    // that write: the single-use endpoint it paid is never paid again (Paykit spec 6).
    let mut dropped = put_back.clone();
    dropped.as_object_mut().unwrap().remove("lost_to_delete");
    put_list_record(&mut laptop, &target, Some(&dropped)).await;
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
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
}

#[tokio::test]
async fn test_unrequested_later_write_dropping_lost_to_delete_flagged_and_not_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    // The phone puts the record back from an older copy, without the mark, naming the
    // delete: an app that joins now never saw the delete, and reads the put-back first.
    let mut put_back = marked.clone();
    put_back["used"] = serde_json::json!([]);
    put_back["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[5u8; 32]));
    put_list_record(&mut phone, &target, Some(&put_back)).await;
    let mut tablet = w.wallet("tablet").await;
    tablet.receive(None).await.unwrap();
    // A later write that drops the marker is ignored and shown with its writer (Paykit wire
    // 5), and nothing is paid from it.
    let mut dropped = put_back.clone();
    dropped.as_object_mut().unwrap().remove("lost_to_delete");
    put_list_record(&mut phone, &target, Some(&dropped)).await;
    tablet.receive(None).await.unwrap();
    let again = tablet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(
        !matches!(again, Ok(PayOutcome::Paid { .. })),
        "the paid single-use endpoint was paid again: {again:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(tablet.app().flagged_writes().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_unrequested_delete_over_a_dropped_loss_marker_keeps_no_copy() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    // The phone puts the record back from an older copy, without the mark, naming the
    // delete; an app that joins now reads that put-back first.
    let mut put_back = marked.clone();
    put_back["used"] = serde_json::json!([]);
    put_back["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[5u8; 32]));
    put_list_record(&mut phone, &target, Some(&put_back)).await;
    let mut tablet = w.wallet("tablet").await;
    tablet.receive(None).await.unwrap();
    // The phone drops the marker, which the tablet refuses, then deletes that write.
    let mut dropped = put_back.clone();
    dropped.as_object_mut().unwrap().remove("lost_to_delete");
    put_list_record(&mut phone, &target, Some(&dropped)).await;
    tablet.receive(None).await.unwrap();
    assert_eq!(tablet.app().flagged_writes().await.unwrap().len(), 1);
    put_list_record(&mut phone, &target, None).await;
    tablet.receive(None).await.unwrap();
    // Restored onto a fresh store from a backup taken after the delete, the tablet reads the
    // same write put back over the delete: it is never paid from.
    let backup = tablet.backup().await.unwrap();
    drop(tablet);
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
    restored.receive(None).await.unwrap();
    put_list_record(&mut phone, &target, Some(&dropped)).await;
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
}

#[tokio::test]
async fn test_unrequested_later_write_changing_lost_to_delete_taken() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (_, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    let mut put_back = marked.clone();
    put_back["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[5u8; 32]));
    put_list_record(&mut phone, &target, Some(&put_back)).await;
    let mut tablet = w.wallet("tablet").await;
    tablet.receive(None).await.unwrap();
    // A later put-back naming another delete changes the marker: it is taken, not flagged
    // (Paykit wire 5, "Repeated deletes").
    let mut renamed = put_back.clone();
    renamed["lost_to_delete"] = serde_json::json!(pubky_switch_core::enc::b64(&[6u8; 32]));
    put_list_record(&mut phone, &target, Some(&renamed)).await;
    tablet.receive(None).await.unwrap();
    assert!(tablet.app().flagged_writes().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_unrequested_loss_marker_of_a_recovery_upload_kept_through_an_in_place_restore() {
    use pubky_switch_client::client::answers::{RecoveryUpload, UploadedOp};
    use pubky_switch_client::client::{Credentials, SwitchClient};
    use pubky_switch_client::testing::{Fault, SWITCH_ORIGIN};
    use pubky_switch_core::ops::{Entry, OpsRequest};
    let w = World::new();
    let storage = crate::common::store();
    let mut tablet = w.wallet_on("tablet", std::sync::Arc::clone(&storage)).await;
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    // The tablet holds the list and is backed up before anything else happens.
    w.list_to_alice(
        &mut shop,
        &[wallet.app_id(), tablet.app_id()],
        list_version(1, 1),
    )
    .await;
    tablet.receive(None).await.unwrap();
    let backup = tablet.backup().await.unwrap();
    // The wallet pays the single-use endpoint; the phone deletes the record and the service
    // forgets the delete.
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let target = shop_list_target(&w, &shop);
    let key = wallet.app().record_key(&target).unwrap();
    phone.app_mut().read_record(&target).await.unwrap();
    let (_, marked) = list_record(&phone, &w).await;
    put_list_record(&mut phone, &target, None).await;
    w.alice.switch().drop_record(&key).unwrap();
    // The phone's put-back naming the delete, without the mark, never reaches the old
    // service; after a provider loss it is a recovery upload, signed and readable with no
    // proof of its version.
    let put_back = marked_put_back(&marked, 5);
    let op = phone
        .app_mut()
        .record_op(
            &target,
            Some(&serde_json::to_vec(&put_back).unwrap()),
            vec![],
        )
        .await
        .unwrap();
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    let batch = pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(op);
    assert!(phone.app_mut().submit_batch(batch).await.is_err());
    let missed = w.alice.switch().unreached_bodies().pop().unwrap();
    let records = OpsRequest::parse(&missed)
        .unwrap()
        .entries
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Signed(op) => Some(UploadedOp {
                op,
                cert: phone.app().cert().signed().clone(),
                proof: None,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1);
    let lost = phone.app().own_identity().signed().clone();
    let next = crate::provider_loss::declare_loss(&w);
    phone.app_mut().refresh_identity().await.unwrap();
    phone.app_mut().follow_provider().await.unwrap();
    SwitchClient::new(
        SWITCH_ORIGIN,
        w.alice_id(),
        Credentials::App {
            identity: w.alice_id(),
            keys: w.alice.identity().app_keys("phone"),
            cert: phone.app().cert().clone(),
        },
        Some(next.service_key()),
        w.router.clone(),
        w.clock.clone(),
    )
    .unwrap()
    .recovery_upload(
        &RecoveryUpload {
            lost_identity: Some(lost),
            records,
            ..Default::default()
        },
        true,
    )
    .await
    .unwrap();
    // The tablet reads the put-back: its client holds it, unproved, and no receive or
    // backup follows before the tablet is dropped.
    tablet.app_mut().refresh_identity().await.unwrap();
    tablet.app_mut().follow_provider().await.unwrap();
    let held = tablet
        .app_mut()
        .read_record(&target)
        .await
        .unwrap()
        .unwrap();
    assert!(!held.authenticated);
    assert!(held.content.value().is_some());
    drop(tablet);
    // A later write drops the marker and the mark.
    let mut dropped = put_back.clone();
    dropped.as_object_mut().unwrap().remove("lost_to_delete");
    phone.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut phone, &target, Some(&dropped)).await;
    // The older backup, taken before the marker, is restored in place: the installation's
    // held put-back still names the delete, so the list is never paid from again (Paykit
    // spec 6; wire 5).
    let mut restored = Paykit::restore(
        w.env(&w.alice, storage),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    crate::provider_loss::follow_generation(&mut restored).await;
    // The receive key the tablet registered at the new service after the backup is its own.
    restored
        .app_mut()
        .confirm_restored_registrations(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    restored.reconcile().await.unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(lost_entries(&restored.backup().await.unwrap()).len(), 1);
    let again = restored
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
async fn test_unrequested_list_keeping_every_known_mark_after_a_delete_stays_held_and_unpaid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let (key, marked) =
        pay_single_use_then_delete_elsewhere(&w, &mut wallet, &mut phone, &mut shop).await;
    let target = shop_list_target(&w, &shop);
    wallet.receive(None).await.unwrap();
    // Another app puts back the very copy the wallet read before the delete, the list with
    // every mark it knew, and the shop sends a newer list over it. Neither is taken: a later
    // authorization over a delete needs a lineage proof this revision lacks (Paykit spec 6;
    // wire 5, "Incomplete protected-record contract").
    put_list_record(&mut phone, &target, Some(&marked)).await;
    wallet.receive(None).await.unwrap();
    let writes = w.record_writes(&w.alice, &key);
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    let again = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(
        !matches!(again, Ok(PayOutcome::Paid { .. })),
        "a list over the delete was paid from: {again:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    let held = wallet.held_lists().await.unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].version, list_version(1, 2));
    // The payee is still paid through a request's own destination.
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("0.00025", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 2);
}

#[tokio::test]
async fn test_unrequested_list_held_over_an_unreadable_record_stays_held_over_a_forgotten_delete() {
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
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    // A faulty own app writes a value that doesn't read over the marked record; the phone
    // reads the list only now and holds it, and is backed up.
    let target = shop_list_target(&w, &shop);
    let key = wallet.app().record_key(&target).unwrap();
    tool.app_mut().read_record(&target).await.unwrap();
    put_list_record(
        &mut tool,
        &target,
        Some(&serde_json::json!({ "not": "a list record" })),
    )
    .await;
    phone.receive(None).await.unwrap();
    assert_eq!(phone.held_lists().await.unwrap().len(), 1);
    let backup = phone.backup().await.unwrap();
    drop(phone);
    // The record is deleted, which the phone never sees, and the service forgets it.
    put_list_record(&mut tool, &target, None).await;
    w.alice.switch().drop_record(&key).unwrap();
    // Restored, the phone reads the key absent: what the unreadable value held is unknown,
    // so the list stays held, never written afresh without the mark (Paykit spec 6).
    let mut restored = restored_onto(&w, crate::common::store(), &backup).await;
    for _ in 0..2 {
        restored.receive(None).await.unwrap();
    }
    let again = restored
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome;
    assert!(
        !matches!(again, Ok(PayOutcome::Paid { .. })),
        "the paid single-use endpoint was paid again: {again:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(restored.held_lists().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_unrequested_held_list_over_an_absent_key_never_created_over_a_forgotten_delete() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut tool = w.wallet("tool").await;
    let mut shop = w.shop().await;
    // The wallet holds the list over the absent key, its first write refused, and its next
    // pass sends nothing that reaches the switch, or a create built then would be kept.
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    crate::common::refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let _ = wallet.receive(None).await;
    let target = shop_list_target(&w, &shop);
    let key = wallet.app().record_key(&target).unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), 0);
    for _ in 0..3 {
        if !w.alice.switch().unreached_bodies().is_empty() {
            break;
        }
        // The wallet sent nothing: another app's writes, never sent again, meet the faults
        // left.
        let op = tool
            .app_mut()
            .record_op(
                &payment_list_target(&w.bob_id(), &[7; 16]),
                None,
                Vec::new(),
            )
            .await
            .unwrap();
        let batch = pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(op);
        assert!(tool.app_mut().submit_batch(batch).await.is_err());
    }
    assert!(!w.alice.switch().unreached_bodies().is_empty());
    // The phone writes the list, pays its single-use endpoint and deletes the record; the
    // service forgets the delete, all inside five minutes.
    w.list_to_alice(&mut shop, &[phone.app_id()], list_version(1, 1))
        .await;
    phone.receive(None).await.unwrap();
    let paid = phone
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    phone.app_mut().read_record(&target).await.unwrap();
    put_list_record(&mut phone, &target, None).await;
    w.alice.switch().drop_record(&key).unwrap();
    let writes = w.record_writes(&w.alice, &key);
    // The wallet's next passes create nothing at the key: a held list's create over an
    // apparently absent key is withheld (Paykit wire 5, "Incomplete protected-record
    // contract"), so no list without the mark is written there for another app to take.
    w.clock.advance_ms(60 * 1000);
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(w.record_writes(&w.alice, &key), writes);
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
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
