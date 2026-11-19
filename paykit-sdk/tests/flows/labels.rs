//! Labelled selling scopes (Paykit spec 3.1, 3.4, 6, 8.1): a hosted app holding
//! `paykit_request:<label>` sends every request under its label and gets the payer's replies only
//! about its label's requests; the identity's apps holding `paykit_request` see them too.

use paykit_lib::Label;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, Refusal, RequestRef,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_core::message::MessagePlaintext;

use crate::common::{draft, request_from, store, World};

#[tokio::test]
async fn test_labels_reply_reaches_only_the_labelled_app_and_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop1 = w.labelled_shop("shop1", "shop1").await;
    let mut shop2 = w.labelled_shop("shop2", "shop2").await;
    // The identity's own app holding `paykit_request` reads every label's records, with no list of
    // labels to follow.
    let mut owner = w
        .join(&w.bob, "owner", AppScopes::new().request().build().unwrap())
        .await;
    let request = request_from(&mut shop1, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&request).await.unwrap();
    assert_eq!(view.label.as_deref(), Some("shop1"));
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");

    // The labelled app and the identity's own `paykit_request` app get the replies; another
    // label's app gets none.
    shop1.receive(None).await.unwrap();
    let sales = shop1.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].label.as_deref(), Some("shop1"));
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    let other = shop2.receive(None).await.unwrap();
    assert!(other.applied.is_empty(), "{other:?}");
    assert!(shop2.payee_activity().await.unwrap().is_empty());
    let seen = owner.receive(None).await.unwrap();
    assert_eq!(seen.applied.len(), 2, "{seen:?}");
    assert_eq!(owner.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_labels_labelled_app_needs_its_label() {
    let w = World::new();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop1",
            AppScopes::new()
                .request_label(&Label::parse("shop1").unwrap())
                .build()
                .unwrap(),
            PaykitConfig::default().with_selling_label(Label::parse("shop2").unwrap()),
            store(),
        )
        .await;
    let refused = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Label(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_labels_payer_cannot_send_requests() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let refused = wallet
        .send_request(w.bob_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap_err();
    let Some(refusal @ Refusal::ScopeMissing(scope)) = refused.refusal() else {
        panic!("{refused:?}")
    };
    assert_eq!(scope, "paykit_request or paykit_request:<label>");
    assert_eq!(
        refusal.to_string(),
        "Paykit spec 3.1: the certificate doesn't hold paykit_request or paykit_request:<label>"
    );
}

fn selling(label: &str) -> paykit_sdk::RequestedScopes {
    AppScopes::new()
        .request_label(&Label::parse(label).unwrap())
        .build()
        .unwrap()
}

async fn renew_till(w: &World, till: &mut Paykit, label: &str) {
    let old = till.app().cert().clone();
    let renewed = crate::certificate::renew(w, till.app_id(), &selling(label), 1_000);
    crate::certificate::store_bundle(w, till.app_id(), &renewed);
    crate::certificate::list(w, till.app_id(), &old, &renewed);
    w.clock.advance_ms(1_000);
    till.receive(None).await.unwrap();
    assert_eq!(till.app().cert().app_id(), old.app_id());
    assert_eq!(till.app().cert().hash(), renewed.hash(), "{label}");
}

fn sale(
    w: &World,
    issuer: &pubky_switch_client::testing::TestSigner,
    payer: &pubky_switch_client::testing::TestSigner,
) -> MessagePlaintext {
    w.sign_as(
        issuer,
        "issuer",
        payer,
        "paykit.payment_request",
        vec![],
        serde_json::json!({
            "amount": {"asset":"btc", "value":"1"},
            "payment_reference":"sale",
            "label":"shop1",
            "proposal_expires_at":null,
            "payment_endpoints":{"btc-lightning-bolt11":"lnbc1invoice"}
        })
        .as_object()
        .unwrap()
        .clone(),
    )
}

fn send_stop(w: &World, till: &str, app_id: [u8; 16], request: [u8; 32]) -> String {
    w.inject(
        &w.alice,
        till,
        &w.bob,
        &[app_id],
        "paykit.payment_request_cancellation",
        vec![request],
        serde_json::json!({ "reason":null, "label":null })
            .as_object()
            .unwrap()
            .clone(),
    )
}

async fn local_count(payer: &Paykit, prefix: &[u8]) -> usize {
    payer
        .app()
        .protocol_scan("paykit", prefix)
        .await
        .unwrap()
        .len()
}

/// Finish housekeeping before refusing the request's own writes.
async fn refuse_record_writes(w: &World, payer: &mut Paykit, count: usize) {
    payer.app_mut().rotate().await.unwrap();
    payer.app_mut().flush_cursor().await.unwrap();
    crate::common::refuse_writes(&w.bob, count);
}

async fn recorded_stop(
    w: &World,
    payer: &mut Paykit,
    request: &RequestRef,
) -> paykit_lib::records::StoredMessage {
    assert_eq!(
        payer.request(request).await.unwrap().status,
        paykit_sdk::RequestStatus::Cancelled
    );
    let record = crate::outcome_record::record_value(payer, request)
        .await
        .unwrap();
    let signed =
        paykit_lib::records::StoredMessage::read(&record["cancellation"], &w.bob_id()).unwrap();
    assert_eq!(local_count(payer, b"kc/").await, 0);
    let refused = payer
        .pay_request(request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    assert!(w.rail.all_executions().is_empty());
    signed
}

/// The original certificate decides each stop even if its till is later renewed with a
/// different selling label while the payer still waits for the request (Paykit spec 3.4).
async fn early_stops_after_renewal(restore: bool) {
    let w = World::new();
    let _issuer = w.join(&w.alice, "issuer", selling("shop1")).await;
    let mut till = w.join(&w.alice, "wallet", selling("shop1")).await;
    let storage = store();
    let mut payer = w
        .join_with(
            &w.bob,
            "payer",
            AppScopes::new().pay().build().unwrap(),
            PaykitConfig::default(),
            storage.clone(),
        )
        .await;
    payer.app_mut().invite(&w.alice_id()).await.unwrap();
    let app_id = payer.app_id();
    let request = sale(&w, &w.alice, &w.bob);
    let first = send_stop(&w, "wallet", app_id, request.hash());
    assert!(payer.receive(None).await.unwrap().applied.contains(&first));
    let old = till.app().cert().clone();
    renew_till(&w, &mut till, "shop2").await;
    let second = send_stop(&w, "wallet", app_id, request.hash());
    let got = payer.receive(None).await.unwrap();
    assert!(got.applied.contains(&second), "{got:?}");
    let originals = payer.app().protocol_scan("paykit", b"kc/").await.unwrap();
    assert_eq!(originals.len(), 2);
    let backup = payer.backup().await.unwrap();
    drop(payer);
    let mut payer = if restore {
        Box::pin(Paykit::restore(
            w.env(&w.bob, store()),
            Default::default(),
            &backup,
            w.rail.clone(),
            w.timer(),
        ))
        .await
        .unwrap()
    } else {
        w.load_on_clock(&w.bob, storage, app_id, w.clock.clone())
            .await
    };
    assert_eq!(
        payer.app().protocol_scan("paykit", b"kc/").await.unwrap(),
        originals
    );
    if restore {
        crate::common::finish_listing(&mut payer).await;
        payer.reconcile().await.unwrap();
        payer
            .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
            .await
            .unwrap();
    }
    w.deliver(&w.alice, "issuer", &w.bob, &[app_id], &request);
    let id = pubky_switch_core::message::Message::parse(request.json(), &[])
        .unwrap()
        .msg_id;
    Box::pin(crate::common::receive_message(&mut payer, &id)).await;
    let purchase = RequestRef {
        payee: w.alice_id(),
        request_hash: request.hash(),
    };
    let signed = recorded_stop(&w, &mut payer, &purchase).await;
    assert_eq!(signed.message().msg_id, first);
    assert_eq!(signed.cert().hash(), old.hash());
}

#[tokio::test]
async fn test_labels_cancellation_before_request_survives_till_renewal_and_reload() {
    early_stops_after_renewal(false).await;
}

#[tokio::test]
async fn test_labels_cancellation_before_request_survives_till_renewal_and_restore() {
    early_stops_after_renewal(true).await;
}

/// Receive the signed parent and keep its refused write for a later receive.
async fn receive_request_owed(w: &World, payer: &mut Paykit, request: &MessagePlaintext) {
    w.deliver(&w.alice, "issuer", &w.bob, &[payer.app_id()], request);
    refuse_record_writes(w, payer, 2).await;
    payer.receive(None).await.unwrap();
    assert_eq!(local_count(payer, b"se/").await, 1);
}

/// Every still-possible authority for a protected request survives while its record write
/// is owed, even past the ordinary sender limit and the native admission pool's lifetime.
#[tokio::test]
async fn test_labels_protected_cancellation_outlives_ordinary_bounds() {
    let w = World::new();
    let _issuer = w.join(&w.alice, "issuer", selling("shop1")).await;
    let both = AppScopes::new()
        .pay()
        .request_label(&Label::parse("shop1").unwrap())
        .build()
        .unwrap();
    let _authorized = w.join(&w.alice, "wallet", both).await;
    let mut payer = w
        .join(
            &w.bob,
            "payer",
            AppScopes::new().pay().request().build().unwrap(),
        )
        .await;
    payer.app_mut().invite(&w.alice_id()).await.unwrap();
    let request = sale(&w, &w.alice, &w.bob);
    let app_id = payer.app_id();
    let limit = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    for n in 0..limit {
        let name = format!("other{n}");
        let scopes = AppScopes::new()
            .pay()
            .request_label(&Label::parse(&name).unwrap())
            .build()
            .unwrap();
        let _other = w.join(&w.alice, &name, scopes).await;
        payer
            .app_mut()
            .identity_of(&w.alice_id(), true)
            .await
            .unwrap();
        let stop = send_stop(&w, &name, app_id, request.hash());
        let got = payer.receive(None).await.unwrap();
        assert!(got.applied.contains(&stop), "{got:?}");
    }
    let sale_side = crate::restore::payee_record(&RequestRef {
        payee: w.bob_id(),
        request_hash: request.hash(),
    });
    // A physical row at the opposite key carries no signed outgoing request.
    crate::allowances::write_record(&mut payer, &sale_side, Some(crate::outcome_record::INVALID))
        .await;
    receive_request_owed(&w, &mut payer, &request).await;

    let replies = payer.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert_eq!(replies.len(), limit);
    let applicable = send_stop(&w, "wallet", app_id, request.hash());
    refuse_record_writes(&w, &mut payer, 1).await;
    let got = payer.receive(None).await.unwrap();
    assert!(got.applied.contains(&applicable), "{got:?}");
    assert_eq!(local_count(&payer, b"kc/").await, limit + 1);
    assert_eq!(
        payer.app().protocol_scan("paykit", b"pr/").await.unwrap(),
        replies,
        "custody of the owed payer request leaves the full ordinary reply pool intact"
    );

    w.clock.advance_ms(22 * pubky_switch_core::records::DAY_MS);
    payer.receive(None).await.unwrap();
    let purchase = RequestRef {
        payee: w.alice_id(),
        request_hash: request.hash(),
    };
    assert_eq!(
        recorded_stop(&w, &mut payer, &purchase)
            .await
            .message()
            .msg_id,
        applicable
    );
}

/// A payer may pay normally once the signed parent excludes every early original.
#[tokio::test]
async fn test_labels_payment_proceeds_when_every_early_cancellation_is_excluded() {
    let w = World::new();
    let _issuer = w.join(&w.alice, "issuer", selling("shop1")).await;
    let mut till = w.join(&w.alice, "wallet", selling("shop2")).await;
    let mut payer = w
        .join(
            &w.bob,
            "payer",
            AppScopes::new().pay().request().build().unwrap(),
        )
        .await;
    payer.app_mut().invite(&w.alice_id()).await.unwrap();
    let request = sale(&w, &w.alice, &w.bob);
    let app_id = payer.app_id();
    receive_request_owed(&w, &mut payer, &request).await;
    let first = send_stop(&w, "wallet", app_id, request.hash());
    refuse_record_writes(&w, &mut payer, 1).await;
    assert!(payer.receive(None).await.unwrap().applied.contains(&first));
    renew_till(&w, &mut till, "shop2").await;
    refuse_record_writes(&w, &mut payer, 1).await;
    let equivalent = send_stop(&w, "wallet", app_id, request.hash());
    assert!(payer
        .receive(None)
        .await
        .unwrap()
        .applied
        .contains(&equivalent));
    assert_eq!(local_count(&payer, b"kc/").await, 1);
    renew_till(&w, &mut till, "shop3").await;
    payer
        .app_mut()
        .identity_of(&w.alice_id(), true)
        .await
        .unwrap();
    let other = send_stop(&w, "wallet", app_id, request.hash());
    assert!(payer.receive(None).await.unwrap().applied.contains(&other));
    let purchase = RequestRef {
        payee: w.alice_id(),
        request_hash: request.hash(),
    };
    assert_eq!(local_count(&payer, b"kc/").await, 0);
    let paid = payer
        .pay_request(&purchase, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// Two originals from the same certificate and millisecond belong to different feeds.
/// Use the known loss boundary or retain both if a subsequent loss obscures it.
async fn stops_across_loss(another_loss: bool) {
    use pubky_switch_client::app::FeedPosition;
    use pubky_switch_client::testing::ScriptedSwitch;
    let w = World::new();
    let mut payer = w.wallet("wallet").await;
    let issuer = w.join(&w.bob, "issuer", selling("shop1")).await;
    payer.app_mut().invite(&w.bob_id()).await.unwrap();
    let request = sale(&w, &w.bob, &w.alice);
    let first = crate::provider_loss::declare_loss(&w);
    Box::pin(crate::provider_loss::follow_generation(&mut payer)).await;
    let send = |service: &ScriptedSwitch, target, hash| {
        let stop = w.sign_as(
            &w.bob,
            "issuer",
            &w.alice,
            "paykit.payment_request_cancellation",
            vec![hash],
            Default::default(),
        );
        service
            .send_plaintext(w.bob_id(), issuer.app().cert(), &stop, &[target])
            .unwrap();
        pubky_switch_core::message::Message::parse(stop.json(), &[])
            .unwrap()
            .msg_id
    };
    for _ in 0..24 {
        send(&first, payer.app_id(), [42; 32]);
    }
    let old = send(&first, payer.app_id(), request.hash());
    assert!(payer.receive(None).await.unwrap().applied.contains(&old));
    let second = crate::provider_loss::declare_second_loss(&w);
    Box::pin(crate::provider_loss::follow_generation(&mut payer)).await;
    let new = send(&second, payer.app_id(), request.hash());
    assert!(payer.receive(None).await.unwrap().applied.contains(&new));
    let purchase = RequestRef {
        payee: w.bob_id(),
        request_hash: request.hash(),
    };
    let key = payer
        .app()
        .record_key(&crate::outcome_record::payer_record(&purchase))
        .unwrap();
    let prefix = format!("kc/{}/", pubky_switch_core::enc::b64(&key));
    let originals = payer
        .app()
        .protocol_scan("paykit", prefix.as_bytes())
        .await
        .unwrap();
    assert_eq!(originals.len(), 2);
    let received = |raw: &[u8]| {
        let value: serde_json::Value = serde_json::from_slice(raw).unwrap();
        let data = &value["d"];
        (
            data["accepted_at"].as_u64().unwrap(),
            FeedPosition {
                generation: data["generation"].as_u64().unwrap(),
                seq: data["seq"].as_u64().unwrap(),
            },
        )
    };
    let mut positions: Vec<_> = originals.iter().map(|(_, v)| received(v)).collect();
    positions.sort_by_key(|(_, p)| p.generation);
    let [(at_old, old_position), (at_new, new_position)] = positions[..] else {
        unreachable!()
    };
    assert_eq!(at_old, at_new);
    assert!(
        old_position.generation < new_position.generation && old_position.seq > new_position.seq
    );
    let current = if another_loss {
        let third = crate::provider_loss::declare_third_loss(&w);
        Box::pin(crate::provider_loss::follow_generation(&mut payer)).await;
        third
    } else {
        second
    };
    assert_eq!(
        payer.app().received_order(old_position, new_position),
        (!another_loss).then_some(std::cmp::Ordering::Less)
    );
    current
        .send_plaintext(w.bob_id(), issuer.app().cert(), &request, &[payer.app_id()])
        .unwrap();
    payer.receive(None).await.unwrap();
    let value = crate::outcome_record::record_value(&mut payer, &purchase)
        .await
        .unwrap();
    if another_loss {
        assert!(value["cancellation"].is_null());
        let error = payer.request(&purchase).await.unwrap_err();
        assert!(
            matches!(error.refusal(),Some(Refusal::NotNow(reason)) if reason.contains("order of the request's cancellations")),
            "{error}"
        );

        assert_eq!(
            payer
                .app()
                .protocol_scan("paykit", prefix.as_bytes())
                .await
                .unwrap(),
            originals
        );
        assert!(payer
            .pay_request(&purchase, PayOptions::default())
            .await
            .outcome
            .is_err());
    } else {
        let signed =
            paykit_lib::records::StoredMessage::read(&value["cancellation"], &w.alice_id())
                .unwrap();
        assert_eq!(signed.message().msg_id, old);
        assert_eq!(local_count(&payer, prefix.as_bytes()).await, 0);
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_labels_places_the_earlier_original_across_a_known_feed_loss() {
    stops_across_loss(false).await;
}

#[tokio::test]
async fn test_labels_retains_originals_when_a_later_loss_obscures_their_order() {
    stops_across_loss(true).await;
}

/// A stored acceptance references the request without being its original. The sibling's
/// restored signed sale decides the direction of the later payer cancellation.
#[tokio::test]
async fn test_labels_stored_payer_reply_preserves_the_siblings_sale() {
    let w = World::new();
    let _buyer = w
        .join(
            &w.alice,
            "buyer",
            AppScopes::new().pay().request().build().unwrap(),
        )
        .await;
    let mut till = w.shop().await;
    let mut wallet = w
        .join(
            &w.bob,
            "wallet",
            AppScopes::new().pay().request().build().unwrap(),
        )
        .await;
    wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    let request = request_from(&mut till, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let sale = crate::restore::payee_record(&request);
    let original = wallet.app_mut().read_record(&sale).await.unwrap().unwrap();
    let pending = w.inject(
        &w.alice,
        "buyer",
        &w.bob,
        &[wallet.app_id()],
        "paykit.payment_request_acceptance",
        vec![request.request_hash],
        Default::default(),
    );
    refuse_record_writes(&w, &mut wallet, 2).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(local_count(&wallet, b"se/").await, 1);
    crate::allowances::write_record(&mut till, &sale, None).await;
    wallet.app_mut().read_record(&sale).await.unwrap();
    let stop = send_stop(&w, "buyer", wallet.app_id(), request.request_hash);
    let read = wallet.receive(None).await.unwrap();
    assert!(read.applied.contains(&stop), "{read:?}");
    assert_eq!(local_count(&wallet, b"pr/").await, 1);
    assert_eq!(local_count(&wallet, b"kc/").await, 1);
    crate::allowances::write_record(&mut till, &sale, original.content.value()).await;
    crate::common::finish_listing(&mut wallet).await;
    let held = wallet.app_mut().read_record(&sale).await.unwrap().unwrap();
    let restored = paykit_lib::records::ActivityRecord::read(
        held.content.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap();
    assert_eq!(restored.cancellation.unwrap().message().msg_id, stop);
    assert_eq!(restored.response.unwrap().message().msg_id, pending);
}
