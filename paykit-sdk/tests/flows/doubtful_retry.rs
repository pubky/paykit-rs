//! A failed attempt its payment method still reports pending makes its claim doubtful: no
//! new attempt is made automatically, and a user makes one only over the doubt shown
//! (Paykit spec 7.1, No automatic payment of doubtful claims; 8.3, Pending holds a failure).

use paykit_lib::AttemptId;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaymentDoubt, Refusal, RequestRef, Resolution};
use pubky_dispatch_client::app::UserConfirmed;
use pubky_dispatch_client::clock::Clock;

use crate::common::{draft, request_from, World};

/// A request paid by hand whose method leaves the payment pending, and the user's
/// confirmation that it failed: the claim shows `failed` while the method still reports
/// the payment pending.
async fn failed_while_pending(w: &World) -> (Paykit, RequestRef, AttemptId) {
    let mut shop = w.shop().await;
    let mut wallet = w.wallet("wallet").await;
    request_from(&mut shop, w.alice_id(), draft("3", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let request = wallet.requests().await.unwrap()[0].request;
    w.rail.next(Behaviour::Hang);
    let first = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(first, PayOutcome::Pending { .. }), "{first:?}");
    let attempt = first.attempt_id();
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    wallet
        .resolve_attempt(
            &obligation,
            attempt,
            Resolution::Failed,
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    (wallet, request, attempt)
}

/// The doubt a refusal shows, which must name `attempt` as pending at the method.
fn pending_doubt(outcome: paykit_sdk::Result<PayOutcome>, attempt: AttemptId) -> PaymentDoubt {
    let error = outcome.unwrap_err();
    let Some(Refusal::MayBePaid {
        doubt: Some(doubt), ..
    }) = error.refusal()
    else {
        panic!("{error:?}");
    };
    assert_eq!(doubt.pending_at_method, vec![attempt], "{doubt:?}");
    (**doubt).clone()
}

#[tokio::test]
async fn test_doubtful_retry_no_automatic_attempt_while_the_failed_one_is_pending() {
    let w = World::new();
    let (mut wallet, request, attempt) = failed_while_pending(&w).await;
    let held = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome;
    pending_doubt(held, attempt);
    assert_eq!(
        w.rail.all_pay_calls().len(),
        1,
        "no second attempt reaches the method"
    );
    // Once the method says the payment failed, the obligation is paid again automatically.
    w.rail.settle(&attempt, false);
    let paid = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert_ne!(paid.attempt_id(), attempt);
}

#[tokio::test]
async fn test_doubtful_retry_by_hand_only_over_the_doubt_shown() {
    let w = World::new();
    let (mut wallet, request, attempt) = failed_while_pending(&w).await;
    let held = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    let doubt = pending_doubt(held, attempt);
    assert_eq!(w.rail.all_pay_calls().len(), 1);
    // The user, warned that the payment may already have gone through, pays it again.
    let paid = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(doubt),
        )
        .await
        .outcome
        .unwrap();
    assert_ne!(paid.attempt_id(), attempt);
    assert_eq!(w.rail.all_pay_calls().len(), 2);
}

#[tokio::test]
async fn test_doubtful_retry_a_history_that_cant_be_read_makes_no_attempt() {
    let w = World::new();
    let (mut wallet, request, attempt) = failed_while_pending(&w).await;
    w.rail.set_lookup_unavailable(&attempt, true);
    let error = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(error, paykit_sdk::PaykitError::Rail(_)),
        "{error:?}"
    );
    assert_eq!(w.rail.all_pay_calls().len(), 1);
}
