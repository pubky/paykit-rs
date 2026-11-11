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

/// A request offering Lightning and on-chain, on-chain first by the identifiers' order.
fn two_endpoints(now: u64) -> paykit_sdk::RequestDraft {
    endpoints_draft(
        now,
        &[
            ("btc-bitcoin-p2wpkh", "bc1qaddress"),
            ("btc-lightning-bolt11", "lnbc1invoice"),
        ],
    )
}

/// A request for 1 btc offering `offered`, each an identifier and its payload.
fn endpoints_draft(now: u64, offered: &[(&str, &str)]) -> paykit_sdk::RequestDraft {
    let mut endpoints = std::collections::BTreeMap::new();
    for (id, payload) in offered {
        endpoints.insert(
            crate::common::endpoint(id),
            paykit_lib::PaymentEndpointPayload::new(payload).unwrap(),
        );
    }
    paykit_sdk::RequestDraft::new(
        crate::common::btc("1"),
        paykit_lib::PaymentReference::new("order-1").unwrap(),
        paykit_lib::bodies::RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(now + 24 * crate::common::HOUR_MS)
}

#[tokio::test]
async fn test_destinations_payment_method_order_picks_the_endpoint_and_the_payee_credits_it() {
    // The method prefers Lightning: of the two endpoints the request offers, it is paid,
    // though the payee lists it second, and the payee credits and receipts it.
    let w = World::new();
    w.rail.set_endpoint_identifiers(vec![
        crate::common::endpoint("btc-lightning-bolt11"),
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), two_endpoints(w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    let paid = wallet.payments().await.unwrap();
    assert_eq!(paid[0].endpoint_id.as_str(), "btc-lightning-bolt11");
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].status, paykit_sdk::PayeeStatus::ProofReceived);
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        paykit_sdk::PayeeStatus::Receipted
    );
}

#[tokio::test]
async fn test_destinations_no_endpoint_the_payment_method_pays_refused() {
    // The method pays only Taproot: the request's endpoints are refused as unsupported,
    // nothing is written, and the request stays payable by a method that pays one.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2tr")]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), two_endpoints(w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    // Naming an endpoint the method doesn't pay is refused the same way.
    let named = wallet
        .pay_request(
            &request,
            PayOptions::default().with_endpoint(crate::common::endpoint("btc-lightning-bolt11")),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(named.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{named:?}"
    );
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2wpkh")]);
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
async fn test_destinations_list_without_an_endpoint_the_payment_method_pays_refused() {
    // The payee's list offers on-chain and Lightning; the method pays only Taproot. The
    // list is in use, so the payee isn't asked for another: the payment is refused as
    // unsupported, not left waiting for a list.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2tr")]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_destinations_list_pays_the_payment_methods_first_endpoint() {
    // Bob's list offers on-chain and Lightning; the method prefers Lightning, which comes
    // second by identifier.
    let w = World::new();
    w.rail.set_endpoint_identifiers(vec![
        crate::common::endpoint("btc-lightning-bolt12"),
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt12"
    );
}

#[tokio::test]
async fn test_destinations_unsupported_live_list_beside_an_expired_one_asks_for_it() {
    // Bob's till offered on-chain, which the method pays, but its list has expired; his
    // shop's live list offers only Lightning, which it doesn't. The till may have a current
    // list that serves, so it is asked for it, and alice waits for it.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2wpkh")]);
    let mut wallet = w.wallet("wallet").await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut shop = w.shop().await;
    till.app_mut().invite(&w.alice_id()).await.unwrap();
    let list = |id: &str, payload: &str, version: u64, expires_at: u64| {
        let mut endpoints = std::collections::BTreeMap::new();
        endpoints.insert(
            crate::common::endpoint(id),
            paykit_lib::bodies::ListEndpoint {
                payload: paykit_lib::PaymentEndpointPayload::new(payload).unwrap(),
                single_use: false,
            },
        );
        (endpoints, version, expires_at)
    };
    let now = w.clock.now_ms();
    let (endpoints, version, expires_at) = list(
        "btc-bitcoin-p2wpkh",
        "bc1qtill",
        now,
        now + crate::common::HOUR_MS,
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version,
        payment_endpoints: endpoints,
        expires_at,
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
    let (endpoints, _, _) = list("btc-lightning-bolt12", "lno1shop", 0, 0);
    w.list_to_alice_with(&mut shop, &[wallet.app_id()], now + 1, endpoints)
        .await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(2 * crate::common::HOUR_MS);
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList)),
        "{refused:?}"
    );
    let got = till.receive(None).await.unwrap();
    assert!(
        got.report.events.iter().any(|e| matches!(
            e,
            pubky_switch_client::app::AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.private_payment_list_request"
        )),
        "{:?}",
        got.report.events
    );
}

/// Bob's app `app` sends alice's `wallet` its list at `version`, offering the one reusable
/// endpoint `id`, entered at her switch directly.
fn list_from(w: &World, app: &str, wallet: [u8; 16], version: u64, id: &str, payload: &str) {
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint(id),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new(payload).unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version,
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + 30 * 24 * crate::common::HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        app,
        &w.alice,
        &[wallet],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
}

/// Alice's wallet and bob's shop and till, each app's list held by the wallet: the till's,
/// newer, offers on-chain; the shop's offers Lightning, which the method prefers.
async fn two_lists(w: &World) -> (paykit_sdk::Paykit, paykit_sdk::Paykit) {
    w.rail.set_endpoint_identifiers(vec![
        crate::common::endpoint("btc-lightning-bolt12"),
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    till.app_mut().invite(&w.alice_id()).await.unwrap();
    let now = w.clock.now_ms();
    list_from(
        w,
        "shop",
        wallet.app_id(),
        now,
        "btc-lightning-bolt12",
        "lno1shop",
    );
    list_from(
        w,
        "till",
        wallet.app_id(),
        now + 1,
        "btc-bitcoin-p2wpkh",
        "bc1qtill",
    );
    wallet.receive(None).await.unwrap();
    (wallet, shop)
}

#[tokio::test]
async fn test_destinations_lists_of_every_app_in_the_payment_methods_order() {
    // The method's first choice is paid from the shop's list, though the till's newer list
    // offers its second.
    let w = World::new();
    let (mut wallet, _shop) = two_lists(&w).await;
    let outcome = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt12"
    );
    // Both lists' endpoints were handed to the method at once, the newer list's first, with
    // the payment's amount.
    assert_eq!(
        handed(&w),
        [vec![
            "0 btc-bitcoin-p2wpkh bc1qtill",
            "1 btc-lightning-bolt12 lno1shop",
        ]]
    );
    assert_eq!(w.rail.selections()[0].1, Some(crate::common::btc("1")));
}

#[tokio::test]
async fn test_destinations_request_to_any_app_takes_the_payment_methods_first_across_lists() {
    // A request that leaves the destination to the payee's apps is paid by the same rule.
    let w = World::new();
    let (mut wallet, mut shop) = two_lists(&w).await;
    let mut draft = draft("1", w.clock.now_ms());
    draft.destination = paykit_lib::bodies::RequestDestination::Any;
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt12"
    );
}

#[tokio::test]
async fn test_destinations_same_endpoint_in_two_lists_paid_from_the_newer() {
    // Both apps' lists offer the same identifier: the newer list's endpoint is paid. The
    // till's, older, fixes another amount, which the transfer check would refuse.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-lightning-bolt12")]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    till.app_mut().invite(&w.alice_id()).await.unwrap();
    let now = w.clock.now_ms();
    list_from(
        &w,
        "shop",
        wallet.app_id(),
        now + 1,
        "btc-lightning-bolt12",
        "lno1shop",
    );
    list_from(
        &w,
        "till",
        wallet.app_id(),
        now,
        "btc-lightning-bolt12",
        "lno1till?amount=2",
    );
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    // The newer list's endpoint is handed first, so the method, whose order is the choice,
    // tries it first.
    assert_eq!(
        handed(&w),
        [vec![
            "0 btc-lightning-bolt12 lno1shop",
            "1 btc-lightning-bolt12 lno1till?amount=2",
        ]]
    );
}

/// What each call of the method's choice was handed: each candidate's ID, identifier and
/// payload, in the order handed.
fn handed(w: &World) -> Vec<Vec<String>> {
    w.rail
        .selections()
        .iter()
        .map(|(candidates, _)| {
            candidates
                .iter()
                .map(|c| format!("{} {} {}", c.id.get(), c.endpoint_id, c.payload.as_str()))
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn test_destinations_preferred_endpoint_of_another_amount_falls_to_the_next() {
    // The method prefers on-chain, but the request's on-chain payload fixes another amount:
    // the transfer check refuses it, and the method's next choice, Lightning, is paid and
    // credited.
    let w = World::new();
    w.rail.set_endpoint_identifiers(vec![
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
        crate::common::endpoint("btc-lightning-bolt11"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let draft = endpoints_draft(
        w.clock.now_ms(),
        &[
            ("btc-bitcoin-p2wpkh", "bc1qaddress?amount=2"),
            ("btc-lightning-bolt11", "lnbc1invoice"),
        ],
    );
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
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
    assert_eq!(paid[0].endpoint_id.as_str(), "btc-lightning-bolt11");
    // The request's endpoints, by identifier, with the request's amount.
    assert_eq!(
        handed(&w),
        [vec![
            "0 btc-bitcoin-p2wpkh bc1qaddress?amount=2",
            "1 btc-lightning-bolt11 lnbc1invoice",
        ]]
    );
    assert_eq!(w.rail.selections()[0].1, Some(crate::common::btc("1")));
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        paykit_sdk::PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_destinations_malformed_endpoint_of_one_app_falls_to_another_apps() {
    // The method prefers Lightning, which only the shop's list offers, with a payload the
    // method can't read an amount from; the till's on-chain endpoint is paid instead.
    let w = World::new();
    w.rail.set_endpoint_identifiers(vec![
        crate::common::endpoint("btc-lightning-bolt12"),
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    till.app_mut().invite(&w.alice_id()).await.unwrap();
    let now = w.clock.now_ms();
    list_from(
        &w,
        "shop",
        wallet.app_id(),
        now + 1,
        "btc-lightning-bolt12",
        "lno1shop?amount=unknown",
    );
    list_from(
        &w,
        "till",
        wallet.app_id(),
        now,
        "btc-bitcoin-p2wpkh",
        "bc1qtill",
    );
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-bitcoin-p2wpkh"
    );
    till.receive(None).await.unwrap();
    assert_eq!(till.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_destinations_every_payable_endpoint_of_another_amount_refused() {
    // Each endpoint the method pays fixes another amount: the payment is refused by the
    // transfer check, nothing written or paid.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let draft = endpoints_draft(
        w.clock.now_ms(),
        &[
            ("btc-bitcoin-p2wpkh", "bc1qaddress?amount=2"),
            ("btc-lightning-bolt11", "lnbc1invoice?amount=3"),
        ],
    );
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Transfer(_))),
        "{refused:?}"
    );
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
}

/// The method's answer `selection` makes of the candidates handed in.
type Answer = fn(
    &[paykit_sdk::rail::PaymentEndpointCandidate],
) -> Result<paykit_sdk::EndpointSelection, paykit_sdk::RailError>;

fn offline() -> paykit_sdk::RailError {
    paykit_sdk::RailError::Unavailable("the node is offline".into())
}

#[tokio::test]
async fn test_destinations_method_answering_about_a_candidate_it_wasnt_handed_refused() {
    // The method answers with an ID it wasn't handed, one ID twice, or one both payable and
    // in error, as payable or as in error: the call fails as an invalid choice, nothing
    // written or paid, and the request stays payable.
    use paykit_sdk::rail::CandidateId;
    use paykit_sdk::EndpointSelection;
    let answers: [(&str, Answer); 5] = [
        ("an unknown payable ID", |c| {
            Ok(EndpointSelection::new(vec![CandidateId::new(
                u32::try_from(c.len()).unwrap(),
            )]))
        }),
        ("a payable ID twice", |c| {
            Ok(EndpointSelection::new(vec![c[1].id, c[0].id, c[1].id]))
        }),
        ("an error for an unknown ID", |c| {
            Ok(EndpointSelection::new(vec![c[1].id])
                .with_error(CandidateId::new(u32::try_from(c.len()).unwrap()), offline()))
        }),
        ("an error for one ID twice", |c| {
            Ok(EndpointSelection::new(Vec::new())
                .with_error(c[0].id, offline())
                .with_error(c[0].id, offline()))
        }),
        ("one ID payable and in error", |c| {
            Ok(EndpointSelection::new(vec![c[1].id]).with_error(c[1].id, offline()))
        }),
    ];
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), two_endpoints(w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    for (what, answer) in answers {
        w.rail.set_selection(move |c, _| answer(c));
        let refused = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(
                refused,
                paykit_sdk::PaykitError::Rail(paykit_sdk::RailError::InvalidSelection(_))
            ),
            "{what}: {refused:?}"
        );
    }
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    // The method's own order, Lightning only, is then paid.
    w.rail
        .set_selection(|candidates, _| Ok(EndpointSelection::new(vec![candidates[1].id])));
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
}

#[tokio::test]
async fn test_destinations_candidate_in_error_then_a_payable_one_paid() {
    // The method couldn't judge on-chain, its first candidate, now, and pays Lightning:
    // Lightning is paid.
    let w = World::new();
    w.rail.set_selection(|candidates, _| {
        Ok(paykit_sdk::EndpointSelection::new(vec![candidates[1].id])
            .with_error(candidates[0].id, offline()))
    });
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), two_endpoints(w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
}

#[tokio::test]
async fn test_destinations_method_failing_fails_the_payment_not_as_unsupported() {
    // The method can't judge any candidate now, or the whole call fails, or the one it
    // pays fixes another amount beside one it couldn't judge: the payment fails with the
    // method's error, which the caller may try again, never refused as unsupported or by
    // the transfer check; for a live list, bob isn't asked for another. Nothing is written
    // or paid, and once the method answers, the request is paid.
    use paykit_sdk::EndpointSelection;
    let answers: [(&str, Answer); 3] = [
        // Each with its own error, listed last first: the first handed in is the one
        // returned.
        ("every candidate in error", |c| {
            Ok(c.iter()
                .rev()
                .fold(EndpointSelection::new(Vec::new()), |s, c| {
                    let error = if c.id.get() == 0 {
                        offline()
                    } else {
                        paykit_sdk::RailError::Unavailable(format!("candidate {}", c.id.get()))
                    };
                    s.with_error(c.id, error)
                }))
        }),
        ("the call in error", |_| Err(offline())),
        ("the payable one of another amount", |c| {
            let payable: Vec<_> = c
                .iter()
                .filter(|c| c.payload.as_str().contains("amount="))
                .map(|c| c.id)
                .collect();
            let failed = c.iter().find(|c| !payable.contains(&c.id)).unwrap().id;
            Ok(EndpointSelection::new(payable).with_error(failed, offline()))
        }),
    ];
    for (what, answer) in answers {
        let w = World::new();
        w.rail.set_selection(move |c, _| answer(c));
        let mut wallet = w.wallet("wallet").await;
        let mut shop = w.shop().await;
        let draft = endpoints_draft(
            w.clock.now_ms(),
            &[
                ("btc-bitcoin-p2wpkh", "bc1qaddress"),
                ("btc-lightning-bolt11", "lnbc1invoice?amount=2"),
            ],
        );
        let request = request_from(&mut shop, w.alice_id(), draft).await;
        w.list_to_alice_with(&mut shop, &[wallet.app_id()], w.clock.now_ms(), {
            let mut endpoints = std::collections::BTreeMap::new();
            for (id, payload) in [
                ("btc-bitcoin-p2wpkh", "bc1qlist"),
                ("btc-lightning-bolt12", "lno1list?amount=2"),
            ] {
                endpoints.insert(
                    crate::common::endpoint(id),
                    paykit_lib::bodies::ListEndpoint {
                        payload: paykit_lib::PaymentEndpointPayload::new(payload).unwrap(),
                        single_use: false,
                    },
                );
            }
            endpoints
        })
        .await;
        wallet.receive(None).await.unwrap();
        let failed = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            failed,
            paykit_sdk::PaykitError::Rail(offline()),
            "{what}: the request's endpoints"
        );
        let failed = wallet
            .pay_without_request(paykit_sdk::UnrequestedPayment::new(
                w.bob_id(),
                crate::common::btc("1"),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            failed,
            paykit_sdk::PaykitError::Rail(offline()),
            "{what}: the list's endpoints"
        );
        assert!(!asked_for_a_list(&mut shop).await, "{what}");
        assert!(wallet.payments().await.unwrap().is_empty(), "{what}");
        assert!(w.rail.all_executions().is_empty(), "{what}");
        w.rail
            .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2wpkh")]);
        w.rail.set_selection(|c, _| {
            Ok(EndpointSelection::new(
                c.iter()
                    .filter(|c| c.endpoint_id.as_str() == "btc-bitcoin-p2wpkh")
                    .map(|c| c.id)
                    .collect(),
            ))
        });
        let outcome = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap();
        assert!(
            matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
            "{what}: {outcome:?}"
        );
    }
}

/// Whether `shop` receives a `paykit.private_payment_list_request` at its next read.
async fn asked_for_a_list(shop: &mut paykit_sdk::Paykit) -> bool {
    let got = shop.receive(None).await.unwrap();
    got.report.events.iter().any(|e| {
        matches!(
            e,
            pubky_switch_client::app::AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.private_payment_list_request"
        )
    })
}

#[tokio::test]
async fn test_destinations_method_choosing_none_refused_as_unsupported() {
    // The method pays none of the candidates: the request's endpoints, and a live list's,
    // are refused as unsupported, the list's without asking bob for another.
    let w = World::new();
    w.rail
        .set_selection(|_, _| Ok(paykit_sdk::EndpointSelection::new(Vec::new())));
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), two_endpoints(w.clock.now_ms())).await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    assert!(!asked_for_a_list(&mut shop).await);
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_destinations_used_single_use_endpoint_the_method_doesnt_pay_not_asked_for() {
    // Alice pays bob's single-use on-chain endpoint; her method then pays only Taproot. The
    // list's other endpoint, Lightning, isn't one it pays, nor is the used one, which a newer
    // list would offer again: refused as unsupported, without asking bob for another list.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let version = w.clock.now_ms();
    w.list_to_alice(&mut shop, &[wallet.app_id()], version)
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-bitcoin-p2wpkh"
    );
    asked_for_a_list(&mut shop).await;
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2tr")]);
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::UnsupportedEndpoint(_))),
        "{refused:?}"
    );
    assert!(!asked_for_a_list(&mut shop).await);
    // The used endpoint was handed in apart, after the method chose none of the others.
    let calls = handed(&w);
    assert_eq!(
        calls[calls.len() - 2..],
        [
            vec!["0 btc-lightning-bolt12 lno1reusable".to_owned()],
            vec![format!("0 btc-bitcoin-p2wpkh bc1qsingle{version}")],
        ]
    );
}

#[tokio::test]
async fn test_destinations_used_single_use_endpoint_the_method_cant_judge_fails_the_payment() {
    // As above, but the method can't judge the used endpoint now: whether a newer list would
    // serve isn't known, so the payment fails with the method's error, neither refused as
    // unsupported nor waiting for a list bob isn't asked for.
    use paykit_sdk::EndpointSelection;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    asked_for_a_list(&mut shop).await;
    w.rail.set_selection(|c, _| {
        Ok(c.iter()
            .filter(|c| c.endpoint_id.as_str() == "btc-bitcoin-p2wpkh")
            .fold(EndpointSelection::new(Vec::new()), |s, c| {
                s.with_error(c.id, offline())
            }))
    });
    let failed = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .unwrap_err();
    assert_eq!(failed, paykit_sdk::PaykitError::Rail(offline()));
    assert!(!asked_for_a_list(&mut shop).await);
}
