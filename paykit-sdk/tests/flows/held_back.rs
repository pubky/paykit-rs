//! A payment the Switch client holds back (Switch spec 3.6, 10.1, 12.3, 12.4): an alarm, an
//! app that left, a recovery notice. The client's own refusal reaches the caller, so the
//! host can tell a recovery from a wait, on a first payment and on a repeat (Paykit spec
//! 7.2).

use paykit_sdk::rail::{Behaviour, RailDeclaration};
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaykitError, Refusal};
use pubky_switch_client::app::{AppError, LeaveMode, Refusal as ClientRefusal, UserConfirmed};
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, World};

/// An idempotent rail, so a resume repeats the dispatch through the pay gate.
fn repeatable(w: &World) {
    w.rail.set_declaration(
        RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
    );
}

/// Alice's wallet with one payment of bob's request left in flight, and a second request
/// not paid yet.
async fn with_a_payment_in_flight(
    w: &World,
) -> (Paykit, paykit_lib::AttemptId, paykit_sdk::RequestRef) {
    repeatable(w);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let paid = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let later = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&paid, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("pending");
    };
    (wallet, attempt_id, later)
}

/// The client's refusal, not a "not now" that reads as a wait.
fn held_by(error: &PaykitError, expected: &ClientRefusal) -> bool {
    !matches!(error.refusal(), Some(Refusal::NotNow(_)))
        && matches!(error, PaykitError::Client(AppError::Refused(refusal)) if refusal == expected)
}

#[tokio::test]
async fn test_held_back_alarm_reaches_the_payment_and_the_repeat() {
    let w = World::new();
    let (mut wallet, attempt_id, later) = with_a_payment_in_flight(&w).await;
    wallet
        .app_mut()
        .raise_rollback("paykit", "test rollback")
        .await
        .unwrap();
    let refused = wallet
        .pay_request(&later, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(held_by(&refused, &ClientRefusal::Stopped), "{refused:?}");
    // The repeat isn't reported as one that may pass later.
    let resumed = wallet.resume_payments().await;
    assert!(
        matches!(&resumed, Err(error) if held_by(error, &ClientRefusal::Stopped)),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_held_back_app_that_left_reaches_the_payment_and_the_repeat() {
    let w = World::new();
    let (mut wallet, attempt_id, later) = with_a_payment_in_flight(&w).await;
    wallet
        .app_mut()
        .leave(LeaveMode::Immediate(UserConfirmed::new()))
        .await
        .unwrap();
    let refused = wallet
        .pay_request(&later, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(held_by(&refused, &ClientRefusal::Left), "{refused:?}");
    let resumed = wallet.resume_payments().await;
    assert!(
        matches!(&resumed, Err(error) if held_by(error, &ClientRefusal::Left)),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}
