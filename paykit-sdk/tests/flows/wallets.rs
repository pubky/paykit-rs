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
async fn test_wallets_payment_past_the_deadline_shows_the_others_open_attempt() {
    // The phone's payment is in flight when the request's deadline passes. The wallet, which
    // has heard from its switch since, pays: the claim it reads shows the phone's open
    // attempt, and that is what it answers, not that the deadline passed.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let outcome = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    w.clock.advance_ms(25 * crate::common::HOUR_MS);
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        refused.refusal(),
        Some(&Refusal::BeingHandledByOtherWallet),
        "{refused:?}"
    );
}

/// What the switch's `409` to the wallet's attempt carries, and what follows it.
enum Crossing {
    /// The claim, in `state`, with another app's attempt.
    Claim(pubky_switch_core::claims::ClaimStatus),
    /// No claim; then the request record can't be read again.
    RecordUnreadable,
}

/// Alice's wallet holds bob's request; its copy of bob's `identity.json` is old, so the
/// check before the attempt reads it again, and that read takes 25 hours: past the request's
/// deadline. The switch then answers the attempt's batch with `409`, as `crossing` says, at
/// its own time. Returns the wallet's error.
async fn attempt_answered_past_the_deadline(crossing: Crossing) -> paykit_sdk::PaykitError {
    use pubky_switch_client::testing::Fault;
    use pubky_switch_core::claims::{AttemptView, ClaimView, Outcome};
    use std::sync::Arc;
    let w = World::new();
    let base = crate::common::store();
    let wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    drop(wallet);
    let mut shop = w.shop().await;
    let now = w.clock.now_ms();
    let request = request_from(&mut shop, w.alice_id(), draft("1", now)).await;
    let reads = Arc::new(crate::common::ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut wallet = w
        .load_with_homeserver(
            &w.alice,
            Arc::clone(&base),
            app_id,
            Arc::clone(&reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>,
        )
        .await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(11 * 60_000);
    reads.then(w.alice_id(), 0, false);
    reads.then(w.bob_id(), 25 * crate::common::HOUR_MS, false);
    let current = match crossing {
        Crossing::Claim(status) => {
            let attempt_id = [9; 16];
            let other = [7; 16];
            let view = ClaimView {
                claim_key: [0; 32],
                status,
                owner: (status == pubky_switch_core::claims::ClaimStatus::Attempting)
                    .then_some(other),
                attempt_id: (status == pubky_switch_core::claims::ClaimStatus::Attempting)
                    .then_some(attempt_id),
                step: 2,
                attempts: vec![AttemptView {
                    attempt_id,
                    owner: other,
                    outcome: if status == pubky_switch_core::claims::ClaimStatus::Settled {
                        Outcome::Done
                    } else {
                        Outcome::Open
                    },
                    opened_at: now,
                    orphan_after: 86_400,
                    orphaned: false,
                    possibly_duplicate: false,
                    duplicate: false,
                    resolved_by: (status == pubky_switch_core::claims::ClaimStatus::Settled)
                        .then_some(other),
                }],
                tombstone: false,
                deadline: None,
                divergent: false,
                next_possibly_duplicate: false,
            };
            serde_json::json!({ "claim": view.to_value() })
        }
        Crossing::RecordUnreadable => serde_json::Value::Null,
    };
    w.alice.switch().fail_next(Fault::Error {
        path: "/ops".into(),
        code: pubky_switch_core::http::ErrorCode::Conflict,
        current,
    });
    if let Crossing::RecordUnreadable = crossing {
        w.alice.switch().fail_next(Fault::Unreachable {
            path: "/records/".into(),
            after: 0,
        });
    }
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(w.rail.all_executions().is_empty());
    error
}

#[tokio::test]
async fn test_wallets_attempt_conflict_past_the_deadline_shows_the_claim() {
    // The `409` carries the claim: another wallet's open attempt, or the payment made. That
    // is the answer, though the switch answered past the deadline.
    use pubky_switch_core::claims::ClaimStatus;
    for (status, expected) in [
        (ClaimStatus::Attempting, Refusal::BeingHandledByOtherWallet),
        (ClaimStatus::Settled, Refusal::AlreadyPaid),
    ] {
        let error = attempt_answered_past_the_deadline(Crossing::Claim(status)).await;
        assert_eq!(error.refusal(), Some(&expected), "{status:?}: {error:?}");
    }
}

#[tokio::test]
async fn test_wallets_attempt_conflict_past_the_deadline_with_the_record_unread_is_an_error() {
    // The `409` carries no claim, and the request record can't be read again: whether
    // another own app rejected the request meanwhile isn't known, so the read's failure is
    // the answer, never that the deadline passed.
    let error = attempt_answered_past_the_deadline(Crossing::RecordUnreadable).await;
    assert!(error.refusal().is_none(), "{error:?}");
    assert!(
        matches!(error, paykit_sdk::PaykitError::Client(_)),
        "{error:?}"
    );
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
