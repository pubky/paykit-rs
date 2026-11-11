//! Two payer apps of one identity (Paykit spec 7.1, 8.5): both receive the request, exactly
//! one pays, and the other shows "being handled by your other wallet", then "Paid" only after
//! SETTLED.

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Refusal, RequestStatus};
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, World};

#[tokio::test]
async fn test_wallets_race_and_exactly_one_pays() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    assert_eq!(
        phone.request(&request).await.unwrap().status,
        RequestStatus::Open
    );

    // The wallet's payment is in flight.
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };

    // The phone sees the claim and makes no attempt.
    let refused = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::BeingHandledByOtherWallet));
    phone.receive(None).await.unwrap();
    assert_eq!(
        phone.request(&request).await.unwrap().status,
        RequestStatus::BeingHandledByOtherWallet
    );

    // "Paid" only after SETTLED.
    w.rail.settle(&attempt_id, true);
    wallet.resume_payments().await.unwrap();
    phone.receive(None).await.unwrap();
    assert_eq!(
        phone.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
    let refused = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::AlreadyPaid));
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_wallets_one_rejects_the_other_cannot_pay() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();

    // The phone still holds the request record as it was: the wallet's rejection changed
    // it, so the phone's attempt, conditioned on the version it read, fails.
    wallet.reject_request(&request, None).await.unwrap();
    let raced = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(raced, paykit_sdk::PaykitError::Conflict(_)),
        "{raced:?}"
    );
    phone.receive(None).await.unwrap();
    let refused = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Rejected));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_wallets_payment_crossing_the_others_rejection_past_the_deadline_shows_it() {
    // The phone rejects the request; past the request's deadline, the wallet, which still
    // holds the record as it was and last heard from its switch before the deadline, pays
    // it. Its attempt is refused with `409`, answered past the deadline: the record the
    // switch holds says the request was rejected, as its status does, and that comes first.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let now = w.clock.now_ms();
    let request = request_from(&mut shop, w.alice_id(), draft("1", now)).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    phone.reject_request(&request, None).await.unwrap();
    w.clock.advance_ms(25 * crate::common::HOUR_MS);
    let raced = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(raced.refusal(), Some(&Refusal::Rejected), "{raced:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Rejected
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_wallets_receipt_settles_a_claim_the_app_never_read() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
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

    // A wallet joined since holds the request record but no view of its claim. The payee's
    // receipt makes its receive step read the claim from the switch, and settle the open
    // attempt by evidence in the cursor's batch (Switch spec 10.2 row 3).
    let mut tablet = w.wallet("tablet").await;
    assert!(tablet
        .app()
        .claim_evidence(&obligation)
        .await
        .unwrap()
        .is_none());
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[tablet.app_id()],
        "paykit.payment_receipt",
        vec![[9; 32], request.request_hash],
        paykit_lib::bodies::PaymentReceipt {
            attempt_id,
            billing_period: None,
        }
        .to_body(2)
        .unwrap(),
    );
    tablet.receive(None).await.unwrap();
    let claim = tablet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.view.status,
        pubky_switch_core::claims::ClaimStatus::Settled
    );
    assert_eq!(
        claim.view.attempts[0].outcome,
        pubky_switch_core::claims::Outcome::Done
    );
}
