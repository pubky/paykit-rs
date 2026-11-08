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
    let PayOutcome::Pending { attempt_id } = wallet
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
