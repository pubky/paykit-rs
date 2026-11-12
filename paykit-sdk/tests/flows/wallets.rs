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
    /// No claim; another own app deleted the request record before.
    RecordDeleted,
    /// No claim; then the switch holds no request record (`404`), though the wallet keeps
    /// its copy.
    RecordNotFound,
    /// No claim; another own app wrote the request record over with a value the wallet
    /// refuses, keeping its copy.
    RecordRefused,
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
    if let Crossing::RecordDeleted | Crossing::RecordRefused = crossing {
        let mut phone = w.wallet("phone").await;
        phone.receive(None).await.unwrap();
        let value = match crossing {
            Crossing::RecordRefused => Some(crate::outcome_record::INVALID),
            _ => None,
        };
        crate::outcome_record::phone_writes(&mut phone, &request, value).await;
    }
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
                rebuilt: false,
            };
            serde_json::json!({ "claim": view.to_value() })
        }
        Crossing::RecordUnreadable
        | Crossing::RecordDeleted
        | Crossing::RecordNotFound
        | Crossing::RecordRefused => serde_json::Value::Null,
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
    if let Crossing::RecordNotFound = crossing {
        w.alice.switch().fail_next(Fault::Error {
            path: "/records/".into(),
            code: pubky_switch_core::http::ErrorCode::NotFound,
            current: serde_json::Value::Null,
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
    assert!(
        matches!(error.refusal(), Some(Refusal::SwitchUnavailable(_))),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_wallets_attempt_conflict_past_the_deadline_with_the_record_not_found_is_the_conflict()
{
    // The `409` carries no claim, and the switch answers the record's read `404`: the copy
    // the wallet keeps shows nothing of the request now, so the conflict is the answer, not
    // that the deadline passed.
    let error = attempt_answered_past_the_deadline(Crossing::RecordNotFound).await;
    assert!(
        matches!(error, paykit_sdk::PaykitError::Conflict(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_wallets_attempt_conflict_past_the_deadline_with_a_newer_write_refused_is_the_conflict(
) {
    // Another own app wrote the request record over with a value the wallet refuses: the
    // copy it keeps is no evidence the request is still open, so the conflict is the
    // answer, not that the deadline passed.
    let error = attempt_answered_past_the_deadline(Crossing::RecordRefused).await;
    assert!(
        matches!(error, paykit_sdk::PaykitError::Conflict(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_wallets_attempt_conflict_past_the_deadline_with_the_record_gone_is_the_conflict() {
    // Another own app deleted the request record; the `409` carries no claim and is
    // answered past the deadline. The record read again isn't there, so nothing shows the
    // request open: the conflict is the answer, not that the deadline passed.
    let error = attempt_answered_past_the_deadline(Crossing::RecordDeleted).await;
    assert!(
        matches!(error, paykit_sdk::PaykitError::Conflict(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn test_wallets_payment_past_the_deadline_shows_the_payees_cancellation_placed_elsewhere() {
    // Bob cancelled the request before its deadline, and the phone placed it in the record;
    // the wallet still holds the record as it was. Past the deadline, the record read again
    // says the request was cancelled.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[phone.app_id()],
        "paykit.payment_request_cancellation",
        vec![request.request_hash],
        serde_json::Map::new(),
    );
    phone.receive(None).await.unwrap();
    assert_eq!(
        phone.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    w.clock.advance_ms(25 * crate::common::HOUR_MS);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_wallets_payment_past_the_deadline_over_a_record_gone_or_unreadable() {
    // The phone deletes the request record, writes a value that doesn't read, or writes the
    // record of another request there; the wallet keeps no mark of what it saw and holds the
    // record as it was. Past the deadline, the record read again shows no open request: one
    // gone is not found, and one that doesn't read as this request a conflict, never that
    // the deadline passed.
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    for write in ["delete", "invalid", "another"] {
        let w = World::new();
        let store = Arc::new(crate::outcome_record::NoSeenMarks::default());
        let mut wallet = w
            .wallet_on(
                "wallet",
                Arc::clone(&store) as Arc<dyn pubky_switch_client::store::StateStore>,
            )
            .await;
        let mut phone = w.wallet("phone").await;
        let mut shop = w.shop().await;
        let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        w.clock.advance_ms(1);
        let other = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        phone.receive(None).await.unwrap();
        store.armed.store(true, Ordering::SeqCst);
        let value = match write {
            "delete" => None,
            "invalid" => Some(crate::outcome_record::INVALID.to_vec()),
            _ => {
                let target = crate::outcome_record::payer_record(&other);
                let held = phone.app_mut().read_record(&target).await.unwrap().unwrap();
                Some(held.content.value().unwrap().to_vec())
            }
        };
        crate::outcome_record::phone_writes(&mut phone, &request, value.as_deref()).await;
        w.clock.advance_ms(25 * crate::common::HOUR_MS);
        let error = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err();
        if write == "delete" {
            assert_eq!(
                error.refusal(),
                Some(&Refusal::NotFound),
                "{write}: {error:?}"
            );
        } else {
            assert!(
                matches!(error, paykit_sdk::PaykitError::Conflict(_)),
                "{write}: {error:?}"
            );
        }
        assert!(w.rail.all_executions().is_empty());
    }
}

/// The doubt a refusal "may already be paid" shows the user.
fn doubt_of(refused: &paykit_sdk::PaykitError) -> paykit_sdk::PaymentDoubt {
    match refused.refusal() {
        Some(Refusal::MayBePaid {
            doubt: Some(doubt), ..
        }) => (**doubt).clone(),
        _ => panic!("not a doubt the user may pay over: {refused:?}"),
    }
}

/// The time after which an attempt opened now is orphaned, with a second to spare.
const PAST_ORPHAN_MS: u64 = 4201 * 1000;

#[tokio::test]
async fn test_wallets_confirmed_doubt_changed_since_refused_again() {
    // The phone's attempt turned `unknown`: the wallet's payment may already be paid, and
    // the user confirms that doubt. Before the wallet pays, the tablet makes an attempt
    // over the same doubt, which turns `unknown` too: the confirmed doubt isn't this one,
    // so the wallet is refused again with the new one, nothing paid. Confirmed in turn, it
    // pays.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut tablet = w.wallet("tablet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    for app in [&mut wallet, &mut phone, &mut tablet] {
        app.receive(None).await.unwrap();
    }
    w.rail.next(Behaviour::Hang);
    let first = phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
        .attempt_id();
    let obligation = phone.payments().await.unwrap()[0].obligation;
    w.clock.advance_ms(PAST_ORPHAN_MS);
    let confirmed = pubky_switch_client::app::UserConfirmed::new();
    wallet
        .resolve_attempt(
            &obligation,
            first,
            paykit_sdk::Resolution::Unknown,
            confirmed,
        )
        .await
        .unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    let seen = doubt_of(&refused);
    assert_eq!(seen.obligation, obligation);
    assert_eq!(seen.unknown, [first]);
    // The tablet pays over the same doubt; its attempt turns `unknown` too.
    tablet.receive(None).await.unwrap();
    let tablet_doubt = doubt_of(
        &tablet
            .pay_request(&request, PayOptions::default())
            .await
            .unwrap_err(),
    );
    w.rail.next(Behaviour::Hang);
    let second = tablet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(tablet_doubt),
        )
        .await
        .unwrap()
        .attempt_id();
    w.clock.advance_ms(PAST_ORPHAN_MS);
    wallet.receive(None).await.unwrap();
    wallet
        .resolve_attempt(
            &obligation,
            second,
            paykit_sdk::Resolution::Unknown,
            confirmed,
        )
        .await
        .unwrap();
    let again = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(seen.clone()),
        )
        .await
        .unwrap_err();
    let now = doubt_of(&again);
    assert_ne!(now, seen);
    let mut both = vec![first, second];
    both.sort();
    assert_eq!(now.unknown, both);
    assert!(wallet.payments().await.unwrap().is_empty());
    let outcome = wallet
        .pay_request(
            &request,
            PayOptions::default().with_may_be_paid_confirmed(now),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_wallets_status_shows_the_records_cancellation_over_a_kept_open_attempt() {
    // The wallet keeps the claim with the phone's attempt open; bob then cancels the
    // request, and the wallet reads his cancellation. The request shows cancelled, as its
    // record says, not as being handled by the other wallet.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    phone
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::BeingHandledByOtherWallet
    );
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    shop.cancel_sent_request(&record, None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
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
            metadata: None,
            payment_reference: None,
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
