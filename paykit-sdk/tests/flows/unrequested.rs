//! Payments without a request (Paykit spec 8.4): to a Private Payment List's endpoints, with
//! single-use endpoints reserved in the attempt's batch, and credited once by settlement.

use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, Refusal, UnrequestedPayment};
use pubky_switch_client::clock::Clock;

use crate::common::{btc, endpoint, World};

fn to_bob(w: &World, value: &str, id: &str) -> UnrequestedPayment {
    UnrequestedPayment {
        payee: w.bob_id(),
        amount: btc(value),
        options: PayOptions {
            endpoint: Some(endpoint(id)),
            ..PayOptions::default()
        },
    }
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
        matches!(used.refusal(), Some(Refusal::NoDestination(_))),
        "{used:?}"
    );
    let used = wallet
        .pay_without_request(to_bob(&w, "0.0015", "btc-bitcoin-p2wpkh"))
        .await
        .unwrap_err();
    assert!(
        matches!(used.refusal(), Some(Refusal::NoDestination(_))),
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
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    let public = wallet
        .pay_without_request(UnrequestedPayment {
            options: PayOptions {
                mode: paykit_sdk::PaymentMode::Public(
                    pubky_switch_client::app::UserConfirmed::new(),
                ),
                ..PayOptions::default()
            },
            ..to_bob(&w, "1", "btc-lightning-bolt12")
        })
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
    w.clock.advance_ms(31 * 24 * crate::common::HOUR_MS);
    let refused = wallet
        .pay_without_request(to_bob(&w, "1", "btc-lightning-bolt12"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
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
