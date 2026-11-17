//! Labelled selling scopes (Paykit spec 3.1, 3.4, 6, 8.1): a hosted app holding
//! `paykit_request:<label>` sends every request under its label and gets the payer's replies only
//! about its label's requests; the identity's apps holding `paykit_request` see them too.

use paykit_lib::bodies::{PaymentReceipt, PaymentRequest, RequestReply};
use paykit_lib::records::{ActivityRecord, ActivitySide};
use paykit_lib::{Label, PaymentReference};
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, Refusal, RequestRef,
    RequestStatus,
};
use pubky_switch_client::clock::Clock;
use serde_json::{Map, Value};

use crate::common::{draft, request_from, store, World};
use crate::periods::{january_details, monthly};

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

const DAY_MS: u64 = 24 * crate::common::HOUR_MS;

/// Bob's app `app` sends alice's `wallet` a message of `kind`, as if its inbox had accepted it.
fn from_bob(
    w: &World,
    app: &str,
    wallet: &Paykit,
    kind: &str,
    refs: Vec<[u8; 32]>,
    body: Map<String, Value>,
) {
    w.inject(&w.bob, app, &w.alice, &[wallet.app_id()], kind, refs, body);
}

/// `shop`'s monthly request to alice, from now.
async fn subscription(w: &World, shop: &mut Paykit) -> RequestRef {
    let draft = monthly(w.clock.now_ms(), None);
    shop.send_request(w.alice_id(), draft)
        .await
        .unwrap()
        .request
        .unwrap()
}

/// A payee's cancellation body: no reason, no label (Paykit wire 4.8).
fn cancellation() -> Map<String, Value> {
    RequestReply {
        reason: None,
        label: None,
    }
    .to_body()
    .unwrap()
}

/// Bob's labelled apps `shop1` and `shop2`, and his own app holding `paykit_request`.
async fn bobs_apps(w: &World) -> (Paykit, Paykit, Paykit) {
    (
        w.labelled_shop("shop1", "shop1").await,
        w.labelled_shop("shop2", "shop2").await,
        w.shop().await,
    )
}

#[tokio::test]
async fn test_labels_payee_cancellation_from_another_label_has_no_effect() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let (mut shop1, _shop2, mut own) = bobs_apps(&w).await;
    let now = w.clock.now_ms();
    let labelled = request_from(&mut shop1, w.alice_id(), draft("1", now)).await;
    let unlabelled = request_from(&mut own, w.alice_id(), draft("2", now)).await;
    wallet.receive(None).await.unwrap();
    // shop2 cancels shop1's request, and shop1 the identity's own: neither stops anything.
    from_bob(
        &w,
        "shop2",
        &wallet,
        CANCEL,
        vec![labelled.request_hash],
        cancellation(),
    );
    from_bob(
        &w,
        "shop1",
        &wallet,
        CANCEL,
        vec![unlabelled.request_hash],
        cancellation(),
    );
    wallet.receive(None).await.unwrap();
    for request in [&labelled, &unlabelled] {
        assert_eq!(
            wallet.request(request).await.unwrap().status,
            RequestStatus::Open
        );
        let outcome = wallet
            .pay_request(request, PayOptions::default())
            .await
            .unwrap();
        assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    }
    // shop1's cancellation of its own request, and the plain app's of shop1's, stop them.
    let by_shop1 = request_from(&mut shop1, w.alice_id(), draft("3", now)).await;
    let by_own = request_from(&mut shop1, w.alice_id(), draft("4", now)).await;
    // (`by_own`: shop1's request, cancelled by the identity's own app below.)
    wallet.receive(None).await.unwrap();
    from_bob(
        &w,
        "shop1",
        &wallet,
        CANCEL,
        vec![by_shop1.request_hash],
        cancellation(),
    );
    from_bob(
        &w,
        "shop",
        &wallet,
        CANCEL,
        vec![by_own.request_hash],
        cancellation(),
    );
    wallet.receive(None).await.unwrap();
    for request in [&by_shop1, &by_own] {
        assert_eq!(
            wallet.request(request).await.unwrap().status,
            RequestStatus::Cancelled
        );
        assert!(wallet
            .pay_request(request, PayOptions::default())
            .await
            .is_err());
    }
}

#[tokio::test]
async fn test_labels_stop_kept_before_its_request_dropped_for_another_label() {
    // shop2's cancellation reaches the wallet before shop1's request: kept as a stop, then
    // dropped once the request reads, so the request stays payable.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let (_shop1, _shop2, _own) = bobs_apps(&w).await;
    let body = PaymentRequest {
        amount: crate::common::btc("1"),
        payment_reference: PaymentReference::new("sale-1").unwrap(),
        proposal_expires_at: Some(w.clock.now_ms() + DAY_MS),
        payment_deadline: None,
        recurrence: None,
        destination: draft("1", w.clock.now_ms()).destination,
        per_period_endpoints: false,
        label: Some(Label::parse("shop1").unwrap()),
        accepted_payment_endpoint_identifiers: None,
        conversion: None,
        metadata: None,
    }
    .to_body()
    .unwrap();
    let request = w.sign_as(
        &w.bob,
        "shop1",
        &w.alice,
        "paykit.payment_request",
        Vec::new(),
        body,
    );
    let hash = paykit_lib::derive::message_hash(request.json());
    from_bob(&w, "shop2", &wallet, CANCEL, vec![hash], cancellation());
    wallet.receive(None).await.unwrap();
    w.deliver(&w.bob, "shop1", &w.alice, &[wallet.app_id()], &request);
    wallet.receive(None).await.unwrap();
    let held = RequestRef {
        payee: w.bob_id(),
        request_hash: hash,
    };
    assert_eq!(
        wallet.request(&held).await.unwrap().status,
        RequestStatus::Open
    );
    let outcome = wallet
        .pay_request(&held, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_labels_period_details_from_another_label_never_stored() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let (mut shop1, _shop2, mut own) = bobs_apps(&w).await;
    let now = w.clock.now_ms();
    let labelled = subscription(&w, &mut shop1).await;
    let unlabelled = subscription(&w, &mut own).await;
    wallet.receive(None).await.unwrap();
    for request in [&labelled, &unlabelled] {
        wallet.accept_subscription(request).await.unwrap();
    }
    let details = january_details(now + DAY_MS).to_body().unwrap();
    // shop2's details for shop1's subscription, and shop1's for the identity's own, each with
    // its own destination: the wallet stores neither, so it pays to neither.
    from_bob(
        &w,
        "shop2",
        &wallet,
        DETAILS,
        vec![labelled.request_hash],
        details.clone(),
    );
    from_bob(
        &w,
        "shop1",
        &wallet,
        DETAILS,
        vec![unlabelled.request_hash],
        details.clone(),
    );
    wallet.receive(None).await.unwrap();
    for request in [&labelled, &unlabelled] {
        let view = wallet.request(request).await.unwrap();
        assert!(
            view.periods.iter().all(|p| p.details.is_empty()),
            "{view:?}"
        );
    }
    // shop1's for its own, and the plain app's for shop1's, are stored.
    from_bob(
        &w,
        "shop1",
        &wallet,
        DETAILS,
        vec![labelled.request_hash],
        details.clone(),
    );
    from_bob(
        &w,
        "shop",
        &wallet,
        DETAILS,
        vec![unlabelled.request_hash],
        details,
    );
    wallet.receive(None).await.unwrap();
    for request in [&labelled, &unlabelled] {
        let view = wallet.request(request).await.unwrap();
        assert_eq!(view.periods.len(), 1, "{view:?}");
        assert_eq!(view.periods[0].details.len(), 1);
    }
}

#[tokio::test]
async fn test_labels_receipt_from_another_label_settles_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let (mut shop1, _shop2, _own) = bobs_apps(&w).await;
    let request = request_from(&mut shop1, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}")
    };
    let view = wallet.request(&request).await.unwrap();
    let held = wallet
        .app()
        .shared_record(&view.record)
        .await
        .unwrap()
        .unwrap();
    let record = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
        ActivitySide::Payer,
    )
    .unwrap();
    let proof = record.proof.unwrap().hash();
    let receipt = PaymentReceipt {
        attempt_id,
        billing_period: None,
        metadata: None,
        payment_reference: None,
    }
    .to_body(2)
    .unwrap();
    // shop2's receipt of shop1's sale is stored nowhere; shop1's own is.
    from_bob(
        &w,
        "shop2",
        &wallet,
        RECEIPT,
        vec![proof, request.request_hash],
        receipt.clone(),
    );
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().receipt.is_none());
    from_bob(
        &w,
        "shop1",
        &wallet,
        RECEIPT,
        vec![proof, request.request_hash],
        receipt,
    );
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.unwrap().receipt.is_some());
}

#[tokio::test]
async fn test_labels_labelled_app_sends_nothing_about_another_labels_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let (mut shop1, mut shop2, _own) = bobs_apps(&w).await;
    let request = request_from(&mut shop1, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    shop1.receive(None).await.unwrap();
    let sale = shop1.payee_activity().await.unwrap()[0].record;
    // shop2 holds no record of shop1's sale: it cancels nothing, and the wallet gets nothing.
    assert!(shop2.cancel_sent_request(&sale, None).await.is_err());
    assert!(shop2.payee_activity().await.unwrap().is_empty());
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.is_empty(), "{got:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

const CANCEL: &str = "paykit.payment_request_cancellation";
const DETAILS: &str = "paykit.payment_period_details";
const RECEIPT: &str = "paykit.payment_receipt";
