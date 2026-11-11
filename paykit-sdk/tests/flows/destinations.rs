//! Destination checks before paying (Paykit spec 4, 7.2): only from the payee's current app.

use paykit_sdk::{PayOptions, Refusal, RequestStatus};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use pubky_switch_core::records::RemovedApp;

use crate::common::{draft, request_from, World};

#[tokio::test]
async fn test_destinations_from_a_revoked_app_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    // Bob's signer revokes the shop app; its request is still held.
    let shop_id = shop.app_id();
    let now = w.clock.now_ms();
    w.bob
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != shop_id);
            body.removed.push(RemovedApp {
                app_id: shop_id,
                distrust_from: now,
            });
        })
        .unwrap();
    w.clock.advance_ms(1000);

    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Destination(_))),
        "{refused:?}"
    );
    // Refused before any write: nothing journaled, no attempt, nothing moved.
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_destinations_from_a_current_app_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Another app of bob's added meanwhile doesn't change the shop's standing.
    let _other = w.labelled_shop("other", "other").await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn test_destinations_payee_identity_unreadable_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // The payee's identity.json can be read neither from its Homeserver nor from its
    // switch, and the copy held is more than 10 minutes old: the request's destination isn't
    // acted on, not even on the user's confirmation, and nothing is written (Paykit spec 7.2).
    w.homeserver
        .remove(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH);
    w.router.remove(crate::common::BOB_ORIGIN);
    w.clock.advance_ms(11 * 60_000);
    let confirmed = PayOptions::default().with_unconfirmed_identity_confirmed(UserConfirmed::new());
    let stale = wallet.pay_request(&request, confirmed).await.unwrap_err();
    assert!(
        matches!(stale.refusal(), Some(Refusal::IdentityUnavailable(_))),
        "{stale:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_destinations_request_endpoint_paid_without_the_switch_confirmation() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // The payee's switch can't be reached, but its Homeserver gives a fresh identity.json
    // listing the shop: the request's own endpoint is paid without the switch's
    // confirmation, which only public endpoints need (Switch spec 4.3, Paykit spec 7.2).
    w.router.remove(crate::common::BOB_ORIGIN);
    w.clock.advance_ms(11 * 60_000);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_own_identity_missing_for_over_60s_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // The wallet's own identity.json was confirmed, then goes missing from its Homeserver
    // for more than 60 s: no read confirms the copy held any more (Paykit spec 4, Switch
    // spec 12.6), so the payment is refused before any write and the rail is never called.
    assert!(wallet
        .app_mut()
        .refresh_identity()
        .await
        .unwrap()
        .confirmed_at()
        .is_some());
    w.homeserver
        .remove(w.alice_id(), pubky_switch_core::records::IDENTITY_PATH);
    w.clock.advance_ms(61_000);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::IdentityUnavailable(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    let obligation = wallet.request(&request).await.unwrap();
    assert_eq!(obligation.status, RequestStatus::Open);
}

#[tokio::test]
async fn test_destinations_own_identity_confirmed_under_60s_ago_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Missing now, but confirmed 30 s ago: within the 60 s the payment goes on.
    wallet.app_mut().refresh_identity().await.unwrap();
    w.homeserver
        .remove(w.alice_id(), pubky_switch_core::records::IDENTITY_PATH);
    w.clock.advance_ms(30_000);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
}

/// A request to `Any` of bob's apps, restricted to bitcoin on-chain.
fn on_chain_only(now: u64) -> paykit_sdk::RequestDraft {
    let mut draft = draft("1", now);
    draft.destination = paykit_lib::bodies::RequestDestination::Any;
    draft.accepted_endpoint_identifiers = Some(vec![crate::common::endpoint("btc-bitcoin-p2wpkh")]);
    draft
}

#[tokio::test]
async fn test_destinations_list_endpoint_the_request_restricts_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Bob's list offers Lightning only.
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new("lno1reusable").unwrap(),
            single_use: false,
        },
    );
    w.list_to_alice_with(&mut shop, &[wallet.app_id()], w.clock.now_ms(), endpoints)
        .await;
    let request = request_from(&mut shop, w.alice_id(), on_chain_only(w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // The request accepts on-chain only: the list has no endpoint it allows (Paykit wire
    // 4.5), and nothing is paid.
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    // The list is current and simply offers no such endpoint: a newer one needn't differ,
    // so the payee isn't asked for one (Paykit spec 8.4).
    let got = shop.receive(None).await.unwrap();
    assert!(
        !got.report.events.iter().any(|e| matches!(
            e,
            pubky_switch_client::app::AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.private_payment_list_request"
        )),
        "{:?}",
        got.report.events
    );
}

#[tokio::test]
async fn test_destinations_list_paid_only_to_an_endpoint_the_request_allows() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Bob's list offers on-chain and Lightning.
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    let request = request_from(&mut shop, w.alice_id(), on_chain_only(w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // Naming the Lightning endpoint is refused: the request doesn't allow it.
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_endpoint(crate::common::endpoint("btc-lightning-bolt12")),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    // Without a choice, it is paid on-chain.
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    let paid = wallet.payments().await.unwrap();
    assert_eq!(paid.len(), 1);
    assert_eq!(paid[0].endpoint_id.as_str(), "btc-bitcoin-p2wpkh");
}

#[tokio::test]
async fn test_destinations_check_after_the_attempt_writes_failed_and_says_so() {
    let w = World::new();
    let base = crate::common::store();
    let wallet = w.wallet_on("wallet", std::sync::Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    drop(wallet);
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let reads = std::sync::Arc::new(crate::common::ScriptedIdentityReads::new(
        std::sync::Arc::clone(&w.homeserver),
        std::sync::Arc::clone(&w.clock),
    ));
    let mut wallet = w
        .load_with_homeserver(
            &w.alice,
            std::sync::Arc::clone(&base),
            app_id,
            std::sync::Arc::clone(&reads)
                as std::sync::Arc<dyn pubky_switch_client::app::HomeserverReader>,
        )
        .await;
    wallet.receive(None).await.unwrap();
    // The payer's copy of bob's identity.json is old: the check before the attempt reads it
    // again, and that read takes 61 s. The own identity.json, confirmed just before it, is
    // then too old at the check after the attempt, and reading it again fails.
    w.clock.advance_ms(11 * 60_000);
    reads.then(w.alice_id(), 0, false);
    reads.then(w.bob_id(), 61_000, false);
    reads.then(w.alice_id(), 0, true);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    // `failed` was written on the attempt, and the refusal says so (Paykit spec 7.2 step 1).
    let Some(Refusal::AttemptFailed(inner)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert!(
        matches!(**inner, Refusal::IdentityUnavailable(_)),
        "{inner:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Finished);
    let claim = wallet
        .app_mut()
        .read_claim(&payment.obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.view.attempts[0].outcome,
        pubky_switch_core::claims::Outcome::Failed
    );
}

#[tokio::test]
async fn test_destinations_list_of_a_removed_app_shadows_no_current_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut shop = w.shop().await;
    // The till's list, then the shop's at a higher version.
    till.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new("lno1till").unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: w.clock.now_ms(),
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + 30 * 24 * crate::common::HOUR_MS,
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms() + 1000)
        .await;
    wallet.receive(None).await.unwrap();
    // Bob's signer removes the shop: its newer list doesn't hide the till's.
    let shop_id = shop.app_id();
    let now = w.clock.now_ms();
    w.bob
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != shop_id);
            body.removed.push(RemovedApp {
                app_id: shop_id,
                distrust_from: now,
            });
        })
        .unwrap();
    w.clock.advance_ms(11 * 60_000);
    let outcome = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("0.0015"))
                .with_options(
                    PayOptions::default()
                        .with_endpoint(crate::common::endpoint("btc-lightning-bolt12")),
                ),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    till.receive(None).await.unwrap();
    assert_eq!(till.payee_activity().await.unwrap().len(), 1);
}
