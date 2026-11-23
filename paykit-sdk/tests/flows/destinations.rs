//! Destination checks before paying (Paykit spec 4, 7.2): only from the payee's current app.

use std::sync::Arc;

use paykit_sdk::{PayOptions, Refusal, RequestStatus};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use pubky_switch_core::records::RemovedApp;

use crate::common::{draft, endpoint, list_version, request_from, World, HOUR_MS};

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
        .outcome
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
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn test_destinations_identity_source_expires_at_ten_minutes() {
    for age in [10 * 60_000 - 1, 10 * 60_000] {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        let first = wallet
            .app_mut()
            .identity_of(&w.bob_id(), true)
            .await
            .unwrap()
            .read_at_ms
            .unwrap();
        w.homeserver
            .remove(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH);
        w.router.remove(crate::common::BOB_ORIGIN);
        w.clock.set_ms(first + age);
        let confirmed =
            PayOptions::default().with_unconfirmed_identity_confirmed(UserConfirmed::new());
        let outcome = wallet.pay_request(&request, confirmed).await.outcome;
        if age == 10 * 60_000 {
            let stale = outcome.unwrap_err();
            assert!(
                matches!(stale.refusal(), Some(Refusal::IdentityUnavailable(_))),
                "{stale:?}"
            );
            assert!(wallet.payments().await.unwrap().is_empty());
            assert!(w.rail.all_executions().is_empty());
        } else {
            assert!(
                matches!(outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
                "{outcome:?}"
            );
            assert_eq!(w.rail.all_executions().len(), 1);
        }
    }
}

#[tokio::test]
async fn test_destinations_identity_source_at_zero_is_required_before_payment() {
    let w = World::at(0);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();

    // Only the directory has supplied this higher, honestly signed version.
    let higher = w.bob_directory_version(&mut wallet).await;
    w.router.remove(crate::common::BOB_ORIGIN);
    let directory = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(directory.doc.hash(), higher.hash());
    assert_eq!(directory.read_at_ms, None);
    assert_eq!(w.clock.now_ms(), 0);
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_unconfirmed_identity_confirmed(UserConfirmed::new()),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::IdentityUnavailable(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());

    // The Homeserver actually serves this same version at zero. A private destination
    // may use that observation even while the payee's switch is unreachable.
    w.homeserver.publish(&higher).unwrap();
    let read = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(read.doc.hash(), higher.hash());
    assert_eq!(read.read_at_ms, Some(0));
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
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
        .outcome
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
        .outcome
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
        .outcome
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
    w.list_to_alice_with(&mut shop, &[wallet.app_id()], list_version(1, 1), endpoints)
        .await;
    let request = request_from(&mut shop, w.alice_id(), on_chain_only(w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // The request accepts on-chain only: the list has no endpoint it allows (Paykit wire
    // 4.5), and nothing is paid.
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
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
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    // Without a choice, it is paid on-chain.
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
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
        .outcome
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
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut shop = w.shop().await;
    // The seller wallet's list, then the shop's at a higher version.
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new("lno1wallet").unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: list_version(1, 1),
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + 30 * 24 * crate::common::HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "seller-wallet",
        &w.alice,
        &[wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    wallet.receive(None).await.unwrap();
    // Bob's signer removes the shop: its newer list doesn't hide the seller wallet's.
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
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    seller_wallet.receive(None).await.unwrap();
    assert_eq!(seller_wallet.payee_activity().await.unwrap().len(), 1);
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
        .outcome
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
        .outcome
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
        .outcome
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
        .outcome
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
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
    // Bob's wallet offered on-chain, which the method pays, but its list has expired; his
    // shop's live list offers only Lightning, which it doesn't. The seller wallet may have a current
    // list that serves, so it is asked for it, and alice waits for it.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2wpkh")]);
    let mut wallet = w.wallet("wallet").await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut shop = w.shop().await;
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    let list = |id: &str, payload: &str, version: paykit_lib::ListVersion, expires_at: u64| {
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
        "bc1qwallet",
        list_version(1, 1),
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
        "seller-wallet",
        &w.alice,
        &[wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    let (endpoints, version, _) = list("btc-lightning-bolt12", "lno1shop", list_version(1, 2), 0);
    w.list_to_alice_with(&mut shop, &[wallet.app_id()], version, endpoints)
        .await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(2 * crate::common::HOUR_MS);
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    let got = seller_wallet.receive(None).await.unwrap();
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
fn list_from(
    w: &World,
    app: &str,
    wallet: [u8; 16],
    version: paykit_lib::ListVersion,
    id: &str,
    payload: &str,
) {
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

/// Alice's wallet holds lists from both of Bob's wallets. His second wallet offers on-chain
/// in the newer list. His shop offers Lightning, which the payment method prefers.
async fn two_lists(w: &World) -> (paykit_sdk::Paykit, paykit_sdk::Paykit) {
    w.rail.set_endpoint_identifiers(vec![
        crate::common::endpoint("btc-lightning-bolt12"),
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    list_from(
        w,
        "shop",
        wallet.app_id(),
        list_version(1, 1),
        "btc-lightning-bolt12",
        "lno1shop",
    );
    list_from(
        w,
        "seller-wallet",
        wallet.app_id(),
        list_version(1, 2),
        "btc-bitcoin-p2wpkh",
        "bc1qwallet",
    );
    wallet.receive(None).await.unwrap();
    (wallet, shop)
}

#[tokio::test]
async fn test_destinations_lists_of_every_app_in_the_payment_methods_order() {
    // The method's first choice is paid from the shop's list, though the seller wallet's list, of the
    // greater pair, offers its second.
    let w = World::new();
    let (mut wallet, _shop) = two_lists(&w).await;
    let outcome = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt12"
    );
    // Both lists' endpoints were handed to the method at once, the greater pair's list first, with
    // the payment's amount.
    assert_eq!(
        handed(&w),
        [vec![
            "0 btc-bitcoin-p2wpkh bc1qwallet",
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
        .outcome
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
async fn test_destinations_same_endpoint_in_two_lists_paid_from_the_greater_pair() {
    // Both apps' lists offer the same identifier: the endpoint of the list of the
    // greater pair is paid. The seller wallet's, of the lower pair, fixes another amount, which the transfer check would refuse.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-lightning-bolt12")]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    list_from(
        &w,
        "shop",
        wallet.app_id(),
        list_version(1, 2),
        "btc-lightning-bolt12",
        "lno1shop",
    );
    list_from(
        &w,
        "seller-wallet",
        wallet.app_id(),
        list_version(1, 1),
        "btc-lightning-bolt12",
        "lno1wallet?amount=2",
    );
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    // The greater pair's endpoint is handed first, so the method, whose order is the choice,
    // tries it first.
    assert_eq!(
        handed(&w),
        [vec![
            "0 btc-lightning-bolt12 lno1shop",
            "1 btc-lightning-bolt12 lno1wallet?amount=2",
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
        .outcome
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
    // method can't read an amount from; the seller wallet's on-chain endpoint is paid instead.
    let w = World::new();
    w.rail.set_endpoint_identifiers(vec![
        crate::common::endpoint("btc-lightning-bolt12"),
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    list_from(
        &w,
        "shop",
        wallet.app_id(),
        list_version(1, 2),
        "btc-lightning-bolt12",
        "lno1shop?amount=unknown",
    );
    list_from(
        &w,
        "seller-wallet",
        wallet.app_id(),
        list_version(1, 1),
        "btc-bitcoin-p2wpkh",
        "bc1qwallet",
    );
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-bitcoin-p2wpkh"
    );
    seller_wallet.receive(None).await.unwrap();
    assert_eq!(seller_wallet.payee_activity().await.unwrap().len(), 1);
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
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Transfer { .. })),
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
            .outcome
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
        .outcome
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
        .outcome
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
        w.list_to_alice_with(&mut shop, &[wallet.app_id()], list_version(1, 1), {
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
            .outcome
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
            .outcome
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
            .outcome
            .unwrap();
        assert!(
            matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
            "{what}: {outcome:?}"
        );
    }
}

/// The `held_version` of each list request `wallet` sent bob, in order.
pub(crate) async fn held_versions(
    w: &World,
    wallet: &paykit_sdk::Paykit,
) -> Vec<Option<paykit_lib::ListVersion>> {
    list_requests_sent(w, wallet)
        .await
        .into_iter()
        .map(|(_, held)| held)
        .collect()
}

/// The app asked about and the `held_version` of each list request `wallet` sent bob.
async fn list_requests_sent(
    w: &World,
    wallet: &paykit_sdk::Paykit,
) -> Vec<(Option<[u8; 16]>, Option<paykit_lib::ListVersion>)> {
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
                paykit_lib::Body::PrivatePaymentListRequest(asked) => {
                    (asked.app_id, asked.held_version)
                }
                other => panic!("a list request: {other:?}"),
            }
        })
        .collect()
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
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
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
        .outcome
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
async fn test_destinations_used_up_list_asked_for_whatever_the_method_pays() {
    // Alice pays bob's single-use on-chain endpoint, which uses up the list's version; her
    // method then pays only Taproot. A newer list may offer it, so the payment waits for
    // bob's list and asks him for one, naming the version used up, whatever the method pays
    // of the old one (Paykit spec 8.4, "No usable list"). The used-up version's endpoints
    // aren't handed to the method.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let version = list_version(1, 1);
    w.list_to_alice(&mut shop, &[wallet.app_id()], version)
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
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
    let before = handed(&w).len();
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert!(asked_for_a_list(&mut shop).await);
    assert_eq!(handed(&w).len(), before);
    assert_eq!(held_versions(&w, &wallet).await, vec![Some(version)]);
}

/// The transfer a refusal by the transfer check shows the user.
fn quote_of(refused: &paykit_sdk::PaykitError) -> paykit_sdk::TransferQuote {
    match refused.refusal() {
        Some(Refusal::Transfer { quote, .. }) => (**quote).clone(),
        _ => panic!("not a transfer refusal: {refused:?}"),
    }
}

#[tokio::test]
async fn test_destinations_confirmed_transfer_of_another_destination_refused_again() {
    // The method prefers on-chain, which fixes 2 btc: the user sees that and pays anyway.
    // Meanwhile the method pays only Lightning, which fixes 3: the confirmation is for
    // another destination, so the payment is refused again with what Lightning would
    // transfer, nothing paid. Confirmed in turn, Lightning is paid, once.
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
        .outcome
        .unwrap_err();
    let on_chain = quote_of(&refused);
    assert_eq!(on_chain.endpoint_id.as_str(), "btc-bitcoin-p2wpkh");
    assert_eq!(on_chain.payee, w.bob_id());
    assert_eq!(on_chain.app_id, shop.app_id());
    assert_eq!(on_chain.amount, crate::common::btc("1"));
    assert_eq!(on_chain.transfer.amount, Some(crate::common::btc("2")));
    assert_eq!(
        on_chain.payload_hash,
        pubky_switch_core::enc::hash(b"bc1qaddress?amount=2")
    );
    assert!(
        !format!("{refused:?}").contains("bc1qaddress"),
        "{refused:?}"
    );
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-lightning-bolt11")]);
    let again = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(on_chain.clone()),
        )
        .await
        .outcome
        .unwrap_err();
    let lightning = quote_of(&again);
    assert_eq!(lightning.endpoint_id.as_str(), "btc-lightning-bolt11");
    assert_eq!(lightning.transfer.amount, Some(crate::common::btc("3")));
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(lightning),
        )
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    let paid = wallet.payments().await.unwrap();
    assert_eq!(paid.len(), 1);
    assert_eq!(paid[0].endpoint_id.as_str(), "btc-lightning-bolt11");
    assert_eq!(w.rail.executions(&paid[0].attempt_id), 1);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_confirmed_transfer_quoted_again_differently_refused_again() {
    // The method charges a fee of 5 above the app's cap of 1: the user sees it and pays
    // anyway. Asked again, the method now charges 6: the payment is refused again with the
    // new fee, nothing paid. Confirmed at 6, it is paid, once.
    let w = World::new();
    w.rail
        .set_max_fee(Some(paykit_lib::Decimal::new("5").unwrap()));
    let mut wallet = w
        .wallet_with(
            "wallet",
            crate::common::store(),
            crate::common::btc_fee_cap(paykit_sdk::PaykitConfig::default(), "1"),
        )
        .await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let five = quote_of(&refused);
    assert_eq!(
        five.transfer.max_fee,
        Some(paykit_lib::Decimal::new("5").unwrap())
    );
    w.rail
        .set_max_fee(Some(paykit_lib::Decimal::new("6").unwrap()));
    let again = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(five.clone()),
        )
        .await
        .outcome
        .unwrap_err();
    let six = quote_of(&again);
    assert_eq!(
        six.transfer.max_fee,
        Some(paykit_lib::Decimal::new("6").unwrap())
    );
    assert_eq!(six.endpoint_id, five.endpoint_id);
    assert_eq!(six.payload_hash, five.payload_hash);
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
    let outcome = wallet
        .pay_request(&request, PayOptions::default().with_transfer_confirmed(six))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    let paid = wallet.payments().await.unwrap();
    assert_eq!(paid.len(), 1);
    assert_eq!(w.rail.executions(&paid[0].attempt_id), 1);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_confirmed_retry_pays_a_destination_that_passes_without_the_quote() {
    // The method pays only on-chain, which fixes 2 btc: the user confirms that. Asked
    // again, the method now prefers Lightning, which transfers the 1 btc asked for: that is
    // the payment the user asked for, so it is paid, once, and the confirmed quote isn't
    // used.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2wpkh")]);
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
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let on_chain = quote_of(&refused);
    w.rail.set_endpoint_identifiers(vec![
        crate::common::endpoint("btc-lightning-bolt11"),
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
    ]);
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(on_chain),
        )
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    let paid = wallet.payments().await.unwrap();
    assert_eq!(paid.len(), 1);
    assert_eq!(paid[0].endpoint_id.as_str(), "btc-lightning-bolt11");
    assert_eq!(w.rail.executions(&paid[0].attempt_id), 1);
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// Alice's wallet under `config`, holding bob's one-time request for 1 btc, the method
/// reporting `fee` as the largest fee of every transfer.
async fn with_fee(
    w: &World,
    fee: &str,
    config: paykit_sdk::PaykitConfig,
) -> (paykit_sdk::Paykit, paykit_sdk::RequestRef) {
    w.rail
        .set_max_fee(Some(paykit_lib::Decimal::new(fee).unwrap()));
    let mut wallet = w
        .wallet_with("wallet", crate::common::store(), config)
        .await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    (wallet, request)
}

#[tokio::test]
async fn test_destinations_fee_with_no_cap_paid_without_asking() {
    // With no fee cap, Paykit makes no check of the fee and asks nothing for it (Paykit spec
    // 7.2 step 1): a fee of 5 is paid by hand and automatically, with no confirmation.
    for automatic in [false, true] {
        let w = World::new();
        let (mut wallet, request) = with_fee(&w, "5", paykit_sdk::PaykitConfig::default()).await;
        let outcome = if automatic {
            wallet
                .pay_request_automatically(&request, PayOptions::default())
                .await
        } else {
            wallet.pay_request(&request, PayOptions::default()).await
        }
        .outcome
        .unwrap();
        assert!(
            matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
            "{automatic}: {outcome:?}"
        );
        assert_eq!(w.rail.all_executions().len(), 1, "{automatic}");
    }
}

#[tokio::test]
async fn test_destinations_fee_with_no_cap_grown_before_dispatch_still_paid() {
    // With no fee cap, nothing bounds the fee between planning and dispatch: the fee is
    // what the method reports at the call (Paykit spec 7.2 step 1).
    let w = World::new();
    let (mut wallet, request) = with_fee(&w, "5", paykit_sdk::PaykitConfig::default()).await;
    w.rail.next_max_fees(fees(&[Some("5"), Some("6")]));
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_fee_with_no_cap_not_bound_by_a_confirmation_for_another_amount() {
    // The only destination fixes 2 btc: the user confirms that amount, shown with a fee of
    // 5. Asked again, the method charges 6. With no fee cap the confirmation was given for
    // the amount and binds no fee: it is paid, once (Paykit spec 7.2 step 1).
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2wpkh")]);
    w.rail
        .set_max_fee(Some(paykit_lib::Decimal::new("5").unwrap()));
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let draft = endpoints_draft(
        w.clock.now_ms(),
        &[("btc-bitcoin-p2wpkh", "bc1qaddress?amount=2")],
    );
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let shown = quote_of(&refused);
    assert_eq!(
        shown.transfer.max_fee,
        Some(paykit_lib::Decimal::new("5").unwrap())
    );
    w.rail
        .set_max_fee(Some(paykit_lib::Decimal::new("6").unwrap()));
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(shown),
        )
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_fee_at_the_cap_paid_automatically_above_it_asked_by_hand() {
    // Under a cap of 1: a fee equal to it is paid automatically; one a smallest unit above
    // it is refused automatically and asked by hand (Paykit spec 7.2 step 1).
    let capped = || crate::common::btc_fee_cap(paykit_sdk::PaykitConfig::default(), "1");
    let w = World::new();
    let (mut wallet, request) = with_fee(&w, "1", capped()).await;
    let outcome = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    let w = World::new();
    let (mut wallet, request) = with_fee(&w, "1.00000001", capped()).await;
    for automatic in [true, false] {
        let refused = if automatic {
            wallet
                .pay_request_automatically(&request, PayOptions::default())
                .await
        } else {
            wallet.pay_request(&request, PayOptions::default()).await
        }
        .outcome
        .unwrap_err();
        assert!(
            matches!(refused.refusal(), Some(Refusal::Transfer { .. })),
            "{automatic}: {refused:?}"
        );
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_destinations_fee_cap_of_the_asset_transferred_binds_a_chosen_fee() {
    // The app caps usd fees at 1 and sets no btc cap. The method would pay the 1 btc request
    // with 30 usd and a fee of 5: the user chooses that transfer, its fee above the usd cap.
    // Just before the call the method charges 50: the fee is judged by the cap of the asset
    // transferred, and only the fee the user chose passes (Paykit spec 7.2 step 1).
    let usd = |value: &str| paykit_lib::PaymentAmount {
        value: paykit_lib::Decimal::new(value).unwrap(),
        asset: paykit_lib::Asset::new("usd").unwrap(),
    };
    let w = World::new();
    w.rail.set_transfer_amount(Some(usd("30")));
    let config = paykit_sdk::PaykitConfig::default().with_fee_cap(
        paykit_lib::Asset::new("usd").unwrap(),
        paykit_lib::Decimal::new("1").unwrap(),
    );
    let (mut wallet, request) = with_fee(&w, "5", config).await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let shown = quote_of(&refused);
    w.rail.next_max_fees(fees(&[Some("5"), Some("50")]));
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(shown),
        )
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::AttemptFailed(inner)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert!(matches!(**inner, Refusal::Transfer { .. }), "{inner:?}");
    assert!(w.rail.all_executions().is_empty());
}

/// The fee the method reports for the next transfers, one each, as decimals.
fn fees(values: &[Option<&str>]) -> Vec<Option<paykit_lib::Decimal>> {
    values
        .iter()
        .map(|v| v.map(|v| paykit_lib::Decimal::new(v).unwrap()))
        .collect()
}

#[tokio::test]
async fn test_destinations_fee_grown_since_planning_not_paid() {
    // Under a cap of 1, the method charges no fee when the payment is planned, and 5 just
    // before the call: the final check refuses it, `failed` is written, and nothing is paid.
    let w = World::new();
    let mut wallet = w
        .wallet_with(
            "wallet",
            crate::common::store(),
            crate::common::btc_fee_cap(paykit_sdk::PaykitConfig::default(), "1"),
        )
        .await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next_max_fees(fees(&[None, Some("5")]));
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::AttemptFailed(inner)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    let Refusal::Transfer { quote, .. } = &**inner else {
        panic!("{inner:?}");
    };
    assert_eq!(
        quote.transfer.max_fee,
        Some(paykit_lib::Decimal::new("5").unwrap())
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_destinations_fee_grown_past_the_confirmed_quote_not_paid() {
    // Under a cap of 1, the user confirms a fee of 5; planned at 5, the method charges 6
    // just before the call: the final check refuses it, and nothing is paid.
    let w = World::new();
    w.rail
        .set_max_fee(Some(paykit_lib::Decimal::new("5").unwrap()));
    let mut wallet = w
        .wallet_with(
            "wallet",
            crate::common::store(),
            crate::common::btc_fee_cap(paykit_sdk::PaykitConfig::default(), "1"),
        )
        .await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let five = quote_of(&refused);
    w.rail.next_max_fees(fees(&[Some("5"), Some("6")]));
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(five),
        )
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::AttemptFailed(inner)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert!(matches!(**inner, Refusal::Transfer { .. }), "{inner:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_destinations_fee_of_a_transfer_the_method_cant_tell_binds_the_confirmed_fee() {
    // The method can't tell what it would transfer for the 1 btc request, so the user
    // chooses the transfer, shown with a fee of 0.5. The app caps only usd fees, or sets no
    // cap at all: nothing tells which asset the fee is in. The confirmation binds the fee it
    // showed whatever the caps: a fee grown to 50, at planning or just before the call, is
    // never paid (Paykit spec 7.2 step 1).
    let usd_only = || {
        paykit_sdk::PaykitConfig::default().with_fee_cap(
            paykit_lib::Asset::new("usd").unwrap(),
            paykit_lib::Decimal::new("1").unwrap(),
        )
    };
    for (config, at_dispatch) in [
        (usd_only(), true),
        (usd_only(), false),
        (paykit_sdk::PaykitConfig::default(), true),
        (paykit_sdk::PaykitConfig::default(), false),
    ] {
        let w = World::new();
        w.rail.set_transfer_amount(None);
        let (mut wallet, request) = with_fee(&w, "0.5", config).await;
        let refused = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err();
        let shown = quote_of(&refused);
        assert_eq!(shown.transfer.amount, None);
        w.rail
            .set_max_fee(Some(paykit_lib::Decimal::new("50").unwrap()));
        if at_dispatch {
            w.rail.next_max_fees(fees(&[Some("0.5")]));
        }
        let refused = wallet
            .pay_request(
                &request,
                PayOptions::default().with_transfer_confirmed(shown),
            )
            .await
            .outcome
            .unwrap_err();
        let refusal = match refused.refusal() {
            Some(Refusal::AttemptFailed(inner)) if at_dispatch => &**inner,
            Some(refusal) if !at_dispatch => refusal,
            _ => panic!("{at_dispatch}: {refused:?}"),
        };
        let Refusal::Transfer { quote, .. } = refusal else {
            panic!("{at_dispatch}: {refusal:?}");
        };
        assert_eq!(
            quote.transfer.max_fee,
            Some(paykit_lib::Decimal::new("50").unwrap()),
            "{at_dispatch}"
        );
        assert!(w.rail.all_executions().is_empty(), "{at_dispatch}");
    }
}

/// Alice's wallet under `config`, holding bob's one-time request for 1 btc whose only
/// destination fixes 2 btc, the method reporting `fee` as the largest fee of every
/// transfer, and on `disk`.
async fn with_another_amount(
    w: &World,
    fee: &str,
    disk: std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
    config: paykit_sdk::PaykitConfig,
) -> (paykit_sdk::Paykit, paykit_sdk::RequestRef) {
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2wpkh")]);
    w.rail
        .set_max_fee(Some(paykit_lib::Decimal::new(fee).unwrap()));
    let mut wallet = w.wallet_with("wallet", disk, config).await;
    let mut shop = w.shop().await;
    let draft = endpoints_draft(
        w.clock.now_ms(),
        &[("btc-bitcoin-p2wpkh", "bc1qaddress?amount=2")],
    );
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    (wallet, request)
}

#[tokio::test]
async fn test_destinations_fee_confirmed_under_no_cap_not_paid_above_a_cap_set_later() {
    // Under no btc cap, or one of 10, the user confirms the 2 btc the only destination
    // fixes, shown with a fee of 5 that no cap refused. The app is opened again with a btc
    // cap of 1: that confirmation never chose a fee above the cap, so the user is asked
    // again, and only the fee chosen under the cap now in force is paid (Paykit spec 7.2
    // step 1).
    for earlier in [None, Some("10")] {
        let w = World::new();
        let disk = crate::common::store();
        let config = earlier.map_or_else(paykit_sdk::PaykitConfig::default, |cap| {
            crate::common::btc_fee_cap(paykit_sdk::PaykitConfig::default(), cap)
        });
        let (wallet, request) = with_another_amount(&w, "5", disk.clone(), config).await;
        let mut wallet = wallet;
        let refused = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome
            .unwrap_err();
        let shown = quote_of(&refused);
        let id = wallet.app_id();
        drop(wallet);
        let mut wallet = w
            .load_with(
                &w.alice,
                disk,
                id,
                w.clock.clone(),
                crate::common::btc_fee_cap(paykit_sdk::PaykitConfig::default(), "1"),
            )
            .await;
        let refused = wallet
            .pay_request(
                &request,
                PayOptions::default().with_transfer_confirmed(shown),
            )
            .await
            .outcome
            .unwrap_err();
        let again = quote_of(&refused);
        assert!(w.rail.all_executions().is_empty(), "{earlier:?}");
        let paid = wallet
            .pay_request(
                &request,
                PayOptions::default().with_transfer_confirmed(again),
            )
            .await
            .outcome
            .unwrap();
        assert!(
            matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
            "{earlier:?}: {paid:?}"
        );
        assert_eq!(w.rail.all_executions().len(), 1, "{earlier:?}");
    }
}

#[tokio::test]
async fn test_destinations_confirmed_fee_and_amount_compared_by_value() {
    // Under a btc cap of 1, the user confirms 2 btc with a fee of 5.00. Planned so, the
    // method spells them 2.0 and 5 just before the call: the same values, so the payment
    // goes, once (Paykit wire 1.2).
    let w = World::new();
    let (mut wallet, request) = with_another_amount(
        &w,
        "5.00",
        crate::common::store(),
        crate::common::btc_fee_cap(paykit_sdk::PaykitConfig::default(), "1"),
    )
    .await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let shown = quote_of(&refused);
    w.rail.next_max_fees(fees(&[Some("5.00")]));
    w.rail
        .set_max_fee(Some(paykit_lib::Decimal::new("5").unwrap()));
    w.rail
        .next_transfer_amounts(vec![Some(crate::common::btc("2"))]);
    w.rail.set_transfer_amount(Some(crate::common::btc("2.0")));
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed(shown),
        )
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_list_expiring_while_the_payee_is_read_asked_for_again() {
    // Bob's list is usable for an hour; reading his identity.json before choosing from it
    // takes two. The list has expired by the time it would be chosen: bob is asked for his
    // current one, and alice waits for it, rather than the list being chosen and then
    // refused by the destination check.
    let w = World::new();
    let base = crate::common::store();
    let wallet = w.wallet_on("wallet", std::sync::Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    drop(wallet);
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let now = w.clock.now_ms();
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new("bc1qshop").unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: list_version(1, 1),
        payment_endpoints: endpoints,
        expires_at: now + crate::common::HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[app_id],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
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
    reads.then(w.bob_id(), 2 * crate::common::HOUR_MS, false);
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(Refusal::WaitingForList {
                device_clock_behind: false
            })
        ),
        "{refused:?}"
    );
    assert!(asked_for_a_list(&mut shop).await);
}

/// Alice's wallet pays the single-use endpoint of bob's shop's list at (1, 1), using up that
/// version; the shop has taken the requests sent so far.
async fn used_up_list(w: &World) -> (paykit_sdk::Paykit, paykit_sdk::Paykit) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")).with_options(
                PayOptions::default().with_endpoint(crate::common::endpoint("btc-bitcoin-p2wpkh")),
            ),
        )
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    asked_for_a_list(&mut shop).await;
    (wallet, shop)
}

#[tokio::test]
async fn test_destinations_used_up_list_asked_for_an_endpoint_it_lacks() {
    // The caller names Taproot, which the used-up list doesn't offer: a newer list may, so
    // the payment waits for it and bob is asked (Paykit spec 8.4, "No usable list").
    let w = World::new();
    let (mut wallet, mut shop) = used_up_list(&w).await;
    let refused = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")).with_options(
                PayOptions::default().with_endpoint(crate::common::endpoint("btc-bitcoin-p2tr")),
            ),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert!(asked_for_a_list(&mut shop).await);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_used_up_list_asked_for_a_request_accepting_what_it_lacks() {
    // Bob's request accepts only Taproot, which the used-up list doesn't offer: a newer list
    // may, so the payment waits for it and bob is asked (Paykit spec 8.4, wire 4.5).
    let w = World::new();
    let (mut wallet, mut shop) = used_up_list(&w).await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        paykit_sdk::RequestDraft::new(
            crate::common::btc("1"),
            paykit_lib::PaymentReference::new("order-tr").unwrap(),
            paykit_lib::bodies::RequestDestination::Any,
        )
        .with_proposal_expires_at_ms(w.clock.now_ms() + 24 * crate::common::HOUR_MS)
        .with_accepted_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2tr")]),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert!(asked_for_a_list(&mut shop).await);
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// Bob's wallet joins and sends alice's `wallet` a live list of one reusable Lightning
/// endpoint. Returns the seller wallet.
async fn seller_wallet_lightning_list(
    w: &World,
    wallet: &paykit_sdk::Paykit,
) -> paykit_sdk::Paykit {
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new("lno1wallet").unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version: list_version(1, 1),
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + 30 * 24 * crate::common::HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "seller-wallet",
        &w.alice,
        &[wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    seller_wallet
}

#[tokio::test]
async fn test_destinations_used_up_list_beside_a_list_without_the_endpoint_asked_for() {
    // The shop's list is used up; the seller wallet's live list lacks the Taproot endpoint the caller
    // names. The used-up list decides: a newer one may offer Taproot, so the payment waits
    // for a list and bob is asked, rather than refused for want of a destination.
    let w = World::new();
    let (mut wallet, mut shop) = used_up_list(&w).await;
    let _seller_wallet = seller_wallet_lightning_list(&w, &wallet).await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")).with_options(
                PayOptions::default().with_endpoint(crate::common::endpoint("btc-bitcoin-p2tr")),
            ),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert!(asked_for_a_list(&mut shop).await);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_used_up_and_expired_list_beside_a_list_without_the_endpoint_asked_for() {
    // As above, and the shop's used-up list has also expired: it is still used up, so the
    // payment waits for a list and the shop is asked, naming the pair held.
    let w = World::new();
    let (mut wallet, mut shop) = used_up_list(&w).await;
    w.clock.advance_ms(20 * 24 * crate::common::HOUR_MS);
    let _seller_wallet = seller_wallet_lightning_list(&w, &wallet).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(11 * 24 * crate::common::HOUR_MS);
    let refused = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")).with_options(
                PayOptions::default().with_endpoint(crate::common::endpoint("btc-bitcoin-p2tr")),
            ),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert!(asked_for_a_list(&mut shop).await);
    let asked = list_requests_sent(&w, &wallet).await;
    assert!(
        asked.contains(&(Some(shop.app_id()), Some(list_version(1, 1)))),
        "{asked:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_used_up_list_beside_a_list_the_method_doesnt_pay_asked_for() {
    // The shop's list is used up; the method now pays only Taproot, which the seller wallet's live
    // list doesn't offer. The used-up list decides: the payment waits for a list and bob is
    // asked, rather than refused as unsupported.
    let w = World::new();
    let (mut wallet, mut shop) = used_up_list(&w).await;
    let _seller_wallet = seller_wallet_lightning_list(&w, &wallet).await;
    wallet.receive(None).await.unwrap();
    w.rail
        .set_endpoint_identifiers(vec![crate::common::endpoint("btc-bitcoin-p2tr")]);
    let refused = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::WaitingForList { .. })),
        "{refused:?}"
    );
    assert!(asked_for_a_list(&mut shop).await);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_destinations_list_expired_by_the_service_time_alone_asks_for_the_current_one() {
    // The wallet's device clock runs ten seconds behind the service. Once the service time
    // it verified reaches the list's expiry, 30 days after its inbox accepted the list, the
    // list has expired, also for a payment by hand: it isn't chosen, and the wallet asks bob
    // for his current list and waits for it (Paykit spec 7.2, check the destination; 8.4).
    let w = World::new();
    let wallet_store = crate::common::store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&wallet_store)).await;
    let app_id = wallet.app_id();
    let mut shop = w.shop().await;
    let sent = w.clock.now_ms();
    w.list_to_alice(&mut shop, &[app_id], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    drop(wallet);
    let expires = sent + 30 * HOUR_MS * 24;
    w.clock.set_ms(expires);
    let behind = Arc::new(pubky_switch_client::clock::TestClock::new(expires - 10_000));
    let mut wallet = w
        .load_on_clock(&w.alice, wallet_store, app_id, behind)
        .await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .highest_service_time()
        .is_some_and(|at| at >= expires));
    let refused = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("0.001"))
                .with_options(PayOptions::default().with_endpoint(endpoint("btc-bitcoin-p2wpkh"))),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(Refusal::WaitingForList {
                device_clock_behind: true
            })
        ),
        "{refused:?}"
    );
    assert!(asked_for_a_list(&mut shop).await);
    assert!(w.rail.all_executions().is_empty());
}
