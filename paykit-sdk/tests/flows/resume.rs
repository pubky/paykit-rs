//! A resume visits every journal entry (Paykit spec 7.2, 7.3): a payment the method reports
//! made is journaled with its proof, without the Dispatch, whatever another entry's step met,
//! and nothing is journaled under a wallet-wide stop.

use std::sync::Arc;

use paykit_lib::AttemptId;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, Rail, Stage};
use pubky_dispatch_client::clock::Clock;
use pubky_dispatch_client::error::TransportError;
use pubky_dispatch_client::testing::Fault;

use crate::common::{draft, request_from, store, World};

/// Two payments left in flight, in the order a resume visits them.
async fn two_in_flight(w: &World, wallet: &mut Paykit) -> [AttemptId; 2] {
    let mut shop = w.shop().await;
    two_in_flight_from(w, wallet, &mut shop).await
}

/// [`two_in_flight`], to requests `shop` sends.
async fn two_in_flight_from(w: &World, wallet: &mut Paykit, shop: &mut Paykit) -> [AttemptId; 2] {
    for amount in ["1", "2"] {
        request_from(shop, w.alice_id(), draft(amount, w.clock.now_ms())).await;
    }
    wallet.receive(None).await.unwrap();
    for request in wallet.requests().await.unwrap() {
        w.rail.next(Behaviour::Hang);
        let paid = wallet
            .pay_request(&request.request, PayOptions::default())
            .await
            .outcome
            .unwrap();
        assert!(matches!(paid, PayOutcome::Pending { .. }), "{paid:?}");
    }
    let order: Vec<AttemptId> = wallet
        .payments()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.attempt_id)
        .collect();
    [order[0], order[1]]
}

/// The journal stage of `attempt`.
async fn stage(wallet: &Paykit, attempt: &AttemptId) -> Stage {
    wallet
        .payments()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.attempt_id == *attempt)
        .unwrap()
        .stage
}

/// The Dispatch answers nothing for the next `n` requests.
fn dispatch_down(w: &World, n: usize) {
    for _ in 0..n {
        w.alice.switch().fail_next(Fault::Unreachable {
            path: String::new(),
            after: 0,
        });
    }
}

#[tokio::test]
async fn test_resume_dispatch_unreachable_still_journals_a_later_settled_payment() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    // The first failed at the method, so its outcome needs the Dispatch; the second paid.
    w.rail.settle(&first, false);
    w.rail.settle(&second, true);
    dispatch_down(&w, 200);
    // The first is left for the next resume; the caller learns the second was paid and its
    // outcome is owed.
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        resumed
            .iter()
            .any(|o| matches!(o, PayOutcome::Pending { attempt_id, .. } if *attempt_id == first)),
        "{resumed:?}"
    );
    assert!(
        resumed.iter().any(|o| matches!(
            o,
            PayOutcome::PaidOutcomeUnwritten { attempt_id, .. } if *attempt_id == second
        )),
        "{resumed:?}"
    );
    assert_eq!(stage(&wallet, &first).await, Stage::RailFailed);
    assert_eq!(stage(&wallet, &second).await, Stage::RailPaid);
    assert_eq!(w.rail.executions(&second), 1);
    assert_eq!(w.rail.executions(&first), 1);
}

#[tokio::test]
async fn test_resume_restored_entry_reads_its_claim_before_its_history() {
    // A restored entry may have been resolved by the user in another app since the backup:
    // its own step reads its claim before the method's history (Paykit spec 7.2), so with the
    // Dispatch down nothing of it is journaled, even with the method showing it paid.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    w.rail.settle(&second, true);
    dispatch_down(&w, 200);
    assert!(restored.resume_payments().await.is_err());
    assert_eq!(stage(&restored, &first).await, Stage::Paying);
    assert_eq!(stage(&restored, &second).await, Stage::Paying);
    assert_eq!(w.rail.executions(&second), 1);
}

#[tokio::test]
async fn test_resume_reports_a_refused_outcome_write_as_the_journal_holds_it() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    w.rail.settle(&first, true);
    w.rail.settle(&second, true);
    // The first outcome's write is refused for now; the second's goes.
    w.alice.switch().fail_next(Fault::Error {
        path: "/ops".into(),
        code: pubky_dispatch_core::http::ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    let resumed = wallet.resume_payments().await.unwrap();
    // Its outcome's batch is kept, and may still go at the end of the run: the resume
    // reports the payment as the journal holds it then, unwritten or paid.
    let first_outcome = resumed.iter().find(|o| o.attempt_id() == first);
    let first_stage = stage(&wallet, &first).await;
    match first_stage {
        Stage::RailPaid => assert!(
            matches!(first_outcome, Some(PayOutcome::PaidOutcomeUnwritten { .. })),
            "{resumed:?}"
        ),
        Stage::Finished | Stage::AcceptanceOwed => assert!(
            matches!(first_outcome, Some(PayOutcome::Paid { .. })),
            "{resumed:?}"
        ),
        other => panic!("{other:?}: {resumed:?}"),
    }
    let second_stage = stage(&wallet, &second).await;
    assert!(
        matches!(second_stage, Stage::Finished | Stage::AcceptanceOwed),
        "{second_stage:?}"
    );
    assert_eq!(w.rail.executions(&first), 1);
    assert_eq!(w.rail.executions(&second), 1);
}

#[tokio::test]
async fn test_resume_under_an_alarm_journals_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    wallet
        .app_mut()
        .raise_rollback("paykit", "test rollback")
        .await
        .unwrap();
    assert!(wallet.app().alarm().is_some());
    w.rail.settle(&second, true);
    assert!(wallet.resume_payments().await.is_err());
    assert_eq!(stage(&wallet, &first).await, Stage::Paying);
    assert_eq!(stage(&wallet, &second).await, Stage::Paying);
}

#[tokio::test]
async fn test_resume_by_an_app_that_left_journals_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let [_, second] = two_in_flight(&w, &mut wallet).await;
    wallet
        .app_mut()
        .leave(pubky_dispatch_client::app::LeaveMode::Immediate(
            pubky_dispatch_client::app::UserConfirmed::new(),
        ))
        .await
        .unwrap();
    w.rail.settle(&second, true);
    let journal = wallet.app().protocol_scan("paykit", b"j/").await.unwrap();
    assert!(wallet.resume_payments().await.is_err());
    assert_eq!(
        wallet.app().protocol_scan("paykit", b"j/").await.unwrap(),
        journal
    );
}

#[tokio::test]
async fn test_resume_on_a_failing_store_journals_nothing() {
    use pubky_dispatch_client::store::{Fault as StoreFault, FaultStore, StateStore};
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    drop(wallet);
    w.rail.settle(&second, true);
    // The store stops at the first commit after the load: the resume's first write.
    let mut stopped = false;
    for at in 1..64 {
        let failing = Arc::new(FaultStore::new(
            Arc::clone(&base),
            StoreFault::StopBefore(at),
        ));
        let Ok(mut wallet) = Paykit::load(
            w.env(&w.alice, Arc::clone(&failing) as Arc<dyn StateStore>),
            PaykitConfig::default(),
            app_id,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        else {
            continue;
        };
        if failing.stopped() {
            continue;
        }
        assert!(wallet.resume_payments().await.is_err());
        assert!(failing.stopped());
        stopped = true;
        break;
    }
    assert!(stopped);
    let wallet = Paykit::load(
        w.env(&w.alice, Arc::clone(&base)),
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(stage(&wallet, &first).await, Stage::Paying);
    assert_eq!(stage(&wallet, &second).await, Stage::Paying);
}

/// The world's switches, where the next operation batch (`/ops`), once armed, fails at the
/// host's transport with the error armed, nothing sent; later ones go through.
struct RefusingOps {
    inner: Arc<pubky_dispatch_client::testing::Router>,
    next: std::sync::Mutex<Option<pubky_dispatch_client::error::TransportError>>,
}

impl pubky_dispatch_client::transport::Transport for RefusingOps {
    fn send(
        &self,
        request: pubky_dispatch_client::transport::Request,
    ) -> pubky_dispatch_client::transport::BoxFuture<
        '_,
        Result<
            pubky_dispatch_client::transport::Response,
            pubky_dispatch_client::error::TransportError,
        >,
    > {
        let refused = (request.method == "POST" && request.path_and_query.ends_with("/ops"))
            .then(|| self.next.lock().unwrap().take())
            .flatten();
        Box::pin(async move {
            match refused {
                Some(error) => Err(error),
                None => self.inner.send(request).await,
            }
        })
    }
}

fn host_transport_errors() -> [TransportError; 4] {
    [
        TransportError::FirstSendUnavailable,
        TransportError::Refused("origin".into()),
        TransportError::TooLarge(1),
        TransportError::InvalidRequest("header".into()),
    ]
}

async fn settled_with_transport_error(
    w: &World,
    paid: [bool; 2],
    error: TransportError,
) -> (Paykit, [AttemptId; 2]) {
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let attempts = two_in_flight(w, &mut wallet).await;
    drop(wallet);
    for (attempt, paid) in attempts.iter().zip(paid) {
        w.rail.settle(attempt, paid);
    }
    let transport = Arc::new(RefusingOps {
        inner: Arc::clone(&w.router),
        next: std::sync::Mutex::new(None),
    });
    let wallet = w
        .load_with_transport(
            &w.alice,
            base,
            app_id,
            Arc::clone(&transport) as Arc<dyn pubky_dispatch_client::transport::Transport>,
        )
        .await;
    *transport.next.lock().unwrap() = Some(error);
    (wallet, attempts)
}

#[tokio::test]
async fn test_resume_returns_a_host_transport_failure_behind_a_kept_outcome_write() {
    // Both payments were made. The first outcome write meets a failure of the host's own
    // transport, and the batch the client kept is refused when it is sent again: the host
    // must act, so the resume returns the failure, after the other payment's outcome is
    // written, rather than report the first pending at every resume.
    for error in host_transport_errors() {
        let w = World::new();
        let (mut wallet, [first, second]) =
            settled_with_transport_error(&w, [true, true], error.clone()).await;
        // The first submit fails at the transport. The Dispatch refuses the same batch on
        // retry, leaving the first outcome unwritten (Paykit spec 7.2 step 3).
        w.alice.switch().fail_next(Fault::Error {
            path: "/ops".into(),
            code: pubky_dispatch_core::http::ErrorCode::Conflict,
            current: serde_json::Value::Null,
        });
        let resumed = wallet.resume_payments().await;
        let Err(paykit_sdk::PaykitError::Client(failure)) = &resumed else {
            panic!("{error:?}: {resumed:?}");
        };
        assert_eq!(
            failure.unkept(),
            &pubky_dispatch_client::app::AppError::Transport(error.clone()),
            "{resumed:?}"
        );
        // The first payment keeps its proof and its outcome is still to write.
        assert_eq!(stage(&wallet, &first).await, Stage::RailPaid, "{error:?}");
        let second_stage = stage(&wallet, &second).await;
        assert!(
            matches!(second_stage, Stage::Finished | Stage::AcceptanceOwed),
            "{error:?}: {second_stage:?}"
        );
        assert_eq!(w.rail.executions(&first), 1);
        assert_eq!(w.rail.executions(&second), 1);
    }
}

#[tokio::test]
async fn test_resume_returns_paid_outcomes_when_the_kept_batch_applied() {
    // Both payments were made. The first outcome write meets the same failure once: the
    // client keeps the batch and sends it again ahead of the second payment's write, and it
    // applies. By the end of the call both outcomes are written, so the resume reports both
    // paid, with nothing left to return (Paykit spec 7.2 steps 3 and 4).
    for error in host_transport_errors() {
        let w = World::new();
        let (mut wallet, attempts) =
            settled_with_transport_error(&w, [true, true], error.clone()).await;
        let resumed = wallet.resume_payments().await;
        let Ok(outcomes) = &resumed else {
            panic!("{error:?}: {resumed:?}");
        };
        assert_eq!(outcomes.len(), attempts.len());
        for (outcome, attempt) in outcomes.iter().zip(attempts) {
            let PayOutcome::Paid {
                attempt_id,
                proof_msg_id: Some(_),
                ..
            } = outcome
            else {
                panic!("{error:?}: {outcome:?}");
            };
            assert_eq!(*attempt_id, attempt);
            assert_eq!(w.rail.executions(&attempt), 1);
            assert_eq!(stage(&wallet, &attempt).await, Stage::Finished);
        }
    }
}

#[tokio::test]
async fn test_resume_returns_a_failed_outcome_when_its_kept_batch_applied() {
    // The method failed the first payment and made the second. The first's failed outcome
    // meets the transport failure once and its kept batch applies ahead of the second's
    // write: the resume reports the failure written and the payment made, and returns no
    // error for a write that went through (Paykit spec 7.2 steps 3 and 4).
    for error in host_transport_errors() {
        let w = World::new();
        let (mut wallet, [first, second]) =
            settled_with_transport_error(&w, [false, true], error.clone()).await;
        let resumed = wallet.resume_payments().await;
        let Ok(outcomes) = &resumed else {
            panic!("{error:?}: {resumed:?}");
        };
        assert!(
            matches!(
                outcomes.as_slice(),
                [
                    PayOutcome::Failed { attempt_id: failed, .. },
                    PayOutcome::Paid { attempt_id: paid, proof_msg_id: Some(_), .. },
                ] if *failed == first && *paid == second
            ),
            "{error:?}: {outcomes:?}"
        );
        assert_eq!(stage(&wallet, &first).await, Stage::Finished, "{error:?}");
        assert_eq!(stage(&wallet, &second).await, Stage::Finished, "{error:?}");
        assert_eq!(w.rail.executions(&first), 1);
        assert_eq!(w.rail.executions(&second), 1);
    }
}

#[tokio::test]
async fn test_resume_returns_paid_outcomes_when_a_timed_out_write_applied() {
    // A timeout is a wait, so it is no error of the resume; the outcome it left unwritten
    // is still reported as the journal holds it once the kept batch has applied.
    let w = World::new();
    let (mut wallet, attempts) =
        settled_with_transport_error(&w, [true, true], TransportError::Timeout).await;
    let outcomes = wallet.resume_payments().await.unwrap();
    assert_eq!(outcomes.len(), attempts.len());
    for (outcome, attempt) in outcomes.iter().zip(attempts) {
        assert!(
            matches!(
                outcome,
                PayOutcome::Paid { attempt_id, proof_msg_id: Some(_), .. } if *attempt_id == attempt
            ),
            "{outcome:?}"
        );
        assert_eq!(w.rail.executions(&attempt), 1);
        assert_eq!(stage(&wallet, &attempt).await, Stage::Finished);
    }
}

/// Two payments in flight on a wallet whose transport refuses its next `/ops` write with
/// the error put in `next`.
async fn in_flight_behind(w: &World) -> (Paykit, [AttemptId; 2], Arc<RefusingOps>) {
    let mut shop = w.shop().await;
    in_flight_behind_from(w, &mut shop).await
}

/// [`in_flight_behind`], to requests `shop` sends.
async fn in_flight_behind_from(
    w: &World,
    shop: &mut Paykit,
) -> (Paykit, [AttemptId; 2], Arc<RefusingOps>) {
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let attempts = two_in_flight_from(w, &mut wallet, shop).await;
    drop(wallet);
    let transport = Arc::new(RefusingOps {
        inner: Arc::clone(&w.router),
        next: std::sync::Mutex::new(None),
    });
    let wallet = w
        .load_with_transport(
            &w.alice,
            base,
            app_id,
            Arc::clone(&transport) as Arc<dyn pubky_dispatch_client::transport::Transport>,
        )
        .await;
    (wallet, attempts, transport)
}

#[tokio::test]
async fn test_resume_reports_a_payment_found_made_after_its_failure_once() {
    // The method reported the first payment failed, and its failure is written. Its history
    // then shows the payment made after all: the recheck's write times out and goes through
    // on the last flush. The attempt is reported once, as the journal holds it: paid.
    let w = World::new();
    let (mut wallet, [first, second], transport) = in_flight_behind(&w).await;
    w.rail.settle(&first, false);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [PayOutcome::Failed { attempt_id: failed, .. }, PayOutcome::Pending { attempt_id: open, .. }]
                if *failed == first && *open == second
        ),
        "{outcomes:?}"
    );
    w.rail.complete_after_failure(&first);
    *transport.next.lock().unwrap() = Some(TransportError::Timeout);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [PayOutcome::Paid { attempt_id: paid, proof_msg_id: Some(_), .. }, PayOutcome::Pending { attempt_id: open, .. }]
                if *paid == first && *open == second
        ),
        "{outcomes:?}"
    );
    assert_eq!(stage(&wallet, &first).await, Stage::Finished);
    assert_eq!(w.rail.pay_calls(&first), 1);
}

/// Two payments in flight, the user's resolution of the second as failed kept after a
/// timeout, and the transport set to time out the next write too.
async fn second_resolved_failed_and_kept(w: &World) -> (Paykit, [AttemptId; 2], Arc<RefusingOps>) {
    second_resolved_and_kept(w, paykit_sdk::Resolution::Failed).await
}

/// [`second_resolved_failed_and_kept`], to requests `shop` sends.
async fn second_resolved_failed_and_kept_from(
    w: &World,
    shop: &mut Paykit,
) -> (Paykit, [AttemptId; 2], Arc<RefusingOps>) {
    second_resolved_and_kept_from(w, shop, paykit_sdk::Resolution::Failed).await
}

/// [`second_resolved_failed_and_kept`], the user resolving it as `resolution`.
async fn second_resolved_and_kept(
    w: &World,
    resolution: paykit_sdk::Resolution,
) -> (Paykit, [AttemptId; 2], Arc<RefusingOps>) {
    let mut shop = w.shop().await;
    second_resolved_and_kept_from(w, &mut shop, resolution).await
}

/// [`second_resolved_and_kept`], to requests `shop` sends.
async fn second_resolved_and_kept_from(
    w: &World,
    shop: &mut Paykit,
    resolution: paykit_sdk::Resolution,
) -> (Paykit, [AttemptId; 2], Arc<RefusingOps>) {
    let (mut wallet, [first, second], transport) = in_flight_behind_from(w, shop).await;
    let obligation = wallet
        .payments()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.attempt_id == second)
        .unwrap()
        .obligation;
    *transport.next.lock().unwrap() = Some(TransportError::Timeout);
    let kept = wallet
        .resolve_attempt(
            &obligation,
            second,
            resolution,
            pubky_dispatch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(kept.kept_batch().is_some(), "{kept:?}");
    assert_eq!(stage(&wallet, &second).await, Stage::Paying);
    (wallet, [first, second], transport)
}

#[tokio::test]
async fn test_resume_keeps_the_users_done_applied_over_the_methods_failure() {
    // The user's resolution of the second payment as done timed out and is kept. The
    // method's history shows it failed, which the history pass journals; the first payment's
    // write then sends the kept resolution, which finishes the entry as the user said. The
    // method's failure doesn't undo the user's word: the journal keeps the resolution, and
    // the payment is reported resolved, never failed.
    let w = World::new();
    let (mut wallet, [first, second], transport) =
        second_resolved_and_kept(&w, paykit_sdk::Resolution::Done).await;
    w.rail.settle(&first, true);
    w.rail.settle(&second, false);
    *transport.next.lock().unwrap() = Some(TransportError::Timeout);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [
                PayOutcome::Paid { attempt_id: paid, proof_msg_id: Some(_), .. },
                PayOutcome::Resolved { attempt_id: resolved, .. },
            ] if *paid == first && *resolved == second
        ),
        "{outcomes:?}"
    );
    let journal = wallet.payments().await.unwrap();
    let entry = journal.iter().find(|p| p.attempt_id == second).unwrap();
    assert_eq!(entry.stage, Stage::Finished, "{journal:?}");
    assert!(!entry.paid_by_method, "{journal:?}");
    // Nothing reopens it.
    assert!(wallet.resume_payments().await.unwrap().is_empty());
    assert_eq!(stage(&wallet, &second).await, Stage::Finished);
    assert_eq!(w.rail.pay_calls(&second), 1);
}

#[tokio::test]
async fn test_resume_reports_a_method_refusal_whose_write_was_lost_as_failed() {
    // The method refused the first call, so nothing was submitted, and the failed outcome's
    // write wasn't answered. The next resume writes it and reports it failed, no payment
    // started, not as resolved without a word; nor does it ask the method's history about
    // it, since nothing was submitted.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Refuse);
    // The attempt's write goes through; the outcome's is never answered.
    w.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 1,
    });
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    assert!(
        !matches!(paid, Ok(PayOutcome::Failed { .. })),
        "the write went through: {paid:?}"
    );
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [PayOutcome::Failed { reason, .. }] if reason == "Paykit spec 7.2: no payment started"
        ),
        "{paid:?} then {outcomes:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.resume_payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_resume_keeps_the_methods_proof_over_a_replayed_resolution() {
    // The user's resolution of the second payment as failed timed out and is kept, and the
    // resume's first flush can't send it either. The method's history then shows both
    // payments made, and the next write sends the kept resolution ahead of it, whose journal
    // write replaces the second entry. The proof the method gave outlasts it: the payment
    // is reported paid with its proof queued, never paid without one.
    let w = World::new();
    let (mut wallet, [first, second], transport) = second_resolved_failed_and_kept(&w).await;
    for attempt in [first, second] {
        w.rail.settle(&attempt, true);
    }
    *transport.next.lock().unwrap() = Some(TransportError::Timeout);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    for (outcome, attempt) in outcomes.iter().zip([first, second]) {
        assert!(
            matches!(
                outcome,
                PayOutcome::Paid { attempt_id, proof_msg_id: Some(_), .. } if *attempt_id == attempt
            ),
            "{outcome:?}"
        );
        assert_eq!(w.rail.executions(&attempt), 1);
        assert_eq!(stage(&wallet, &attempt).await, Stage::Finished);
    }
    let paid = wallet.payments().await.unwrap();
    assert!(paid.iter().all(|p| p.paid_by_method), "{paid:?}");
}

#[tokio::test]
async fn test_resume_delivers_the_proof_kept_over_a_replayed_resolution_to_the_payee() {
    // As above, with the payee reading: the proof the method gave, which the replayed
    // resolution's journal write doesn't take back, reaches the payee for both payments.
    let w = World::new();
    let mut shop = w.shop().await;
    let (mut wallet, [first, second], transport) =
        second_resolved_failed_and_kept_from(&w, &mut shop).await;
    for attempt in [first, second] {
        w.rail.settle(&attempt, true);
    }
    *transport.next.lock().unwrap() = Some(TransportError::Timeout);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        outcomes.iter().all(|outcome| matches!(
            outcome,
            PayOutcome::Paid {
                proof_msg_id: Some(_),
                ..
            }
        )),
        "{outcomes:?}"
    );
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    let activity = shop.payee_activity().await.unwrap();
    assert_eq!(activity.len(), 2, "{activity:?}");
    for record in &activity {
        assert!(
            matches!(
                record.status,
                PayeeStatus::ProofReceived | PayeeStatus::Receipted
            ),
            "{activity:?}"
        );
    }
}

#[tokio::test]
async fn test_resume_rechecks_every_failed_payment_past_a_lookup_it_cant_read() {
    // Both payments were reported failed by the method, then made after all. The method's
    // history can't be read for the first now: a wait, so the recheck leaves it and still
    // finds the second made. The next recheck finds the first.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    for attempt in [first, second] {
        w.rail.settle(&attempt, false);
    }
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, PayOutcome::Failed { .. })),
        "{outcomes:?}"
    );
    for attempt in [first, second] {
        w.rail.complete_after_failure(&attempt);
    }
    w.rail.set_lookup_unavailable(&first, true);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [PayOutcome::Paid { attempt_id, .. }] if *attempt_id == second
        ),
        "{outcomes:?}"
    );
    w.rail.set_lookup_unavailable(&first, false);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [PayOutcome::Paid { attempt_id, .. }] if *attempt_id == first
        ),
        "{outcomes:?}"
    );
    for attempt in [first, second] {
        assert_eq!(w.rail.pay_calls(&attempt), 1);
    }
}

#[tokio::test]
async fn test_resume_keeps_the_methods_failure_over_a_replayed_resolution() {
    // As above, but the method's history shows the second payment failed. The kept
    // resolution's journal write replaces the entry without the method's word; that word
    // goes back, so the failure is reported as the method's and its history is asked again
    // later, where it shows the payment made after all.
    let w = World::new();
    let (mut wallet, [first, second], transport) = second_resolved_failed_and_kept(&w).await;
    w.rail.settle(&first, true);
    w.rail.settle(&second, false);
    *transport.next.lock().unwrap() = Some(TransportError::Timeout);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [
                PayOutcome::Paid { attempt_id: paid, proof_msg_id: Some(_), .. },
                PayOutcome::Failed { attempt_id: failed, reason, .. },
            ] if *paid == first && *failed == second && reason == "the payment method reported a failure"
        ),
        "{outcomes:?}"
    );
    assert_eq!(stage(&wallet, &second).await, Stage::Finished);
    w.rail.complete_after_failure(&second);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [PayOutcome::Paid { attempt_id: paid, .. }] if *paid == second
        ),
        "{outcomes:?}"
    );
    assert_eq!(w.rail.pay_calls(&second), 1);
}

#[tokio::test]
async fn test_resume_reports_a_payment_whose_outcome_another_entrys_write_sent() {
    // Both payments are made, and neither outcome write is answered: both batches are kept.
    // At the next resume the first flush fails too, and the first payment's write sends
    // both kept batches ahead of it, so the second entry is finished before its own turn.
    // It is still reported, once, as the journal holds it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    for attempt in [first, second] {
        w.rail.settle(&attempt, true);
    }
    let unanswered = |n: usize| {
        for _ in 0..n {
            w.alice.switch().fail_next(Fault::Unanswered {
                path: "/ops".into(),
                after: 0,
            });
        }
    };
    // The first's write, its retry ahead of the second's, and the last flush.
    unanswered(3);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [PayOutcome::PaidOutcomeUnwritten { attempt_id: a, .. }, PayOutcome::PaidOutcomeUnwritten { attempt_id: b, .. }]
                if *a == first && *b == second
        ),
        "{outcomes:?}"
    );
    unanswered(1);
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [
                PayOutcome::Paid { attempt_id: a, proof_msg_id: Some(_), .. },
                PayOutcome::Paid { attempt_id: b, proof_msg_id: Some(_), .. },
            ] if *a == first && *b == second
        ),
        "{outcomes:?}"
    );
    for attempt in [first, second] {
        assert_eq!(stage(&wallet, &attempt).await, Stage::Finished);
        assert_eq!(w.rail.executions(&attempt), 1);
    }
    assert!(wallet.resume_payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_resume_reports_a_restored_failure_written_after_the_methods_history_expired() {
    // The method reported the payment failed, and that failure couldn't be written before
    // the backup was taken. The wallet restored from it writes the failure once the method
    // no longer keeps its history: the recheck then drops the method's word from the entry
    // in the same call, and the failure just written is still reported.
    const DAY_MS: u64 = 24 * crate::common::HOUR_MS;
    let w = World::new();
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_history(true)
        .with_history_retention_ms(DAY_MS)
        .with_history_survives_restore(true),
    );
    let mut wallet = w.wallet("wallet").await;
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    w.rail.settle(&first, false);
    // The failure's write and its retry on the last flush.
    for _ in 0..2 {
        w.alice.switch().fail_next(Fault::Unanswered {
            path: "/ops".into(),
            after: 0,
        });
    }
    let outcomes = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            outcomes.as_slice(),
            [PayOutcome::Pending { attempt_id: a, .. }, PayOutcome::Pending { attempt_id: b, .. }]
                if *a == first && *b == second
        ),
        "{outcomes:?}"
    );
    assert_eq!(stage(&wallet, &first).await, Stage::RailFailed);
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    w.clock.advance_ms(DAY_MS + 1);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    let outcomes = restored.resume_payments().await.unwrap();
    assert!(
        outcomes.iter().any(|outcome| matches!(
            outcome,
            PayOutcome::Failed { attempt_id, reason, .. }
                if *attempt_id == first && reason == "the payment method reported a failure"
        )),
        "{outcomes:?}"
    );
    assert_eq!(stage(&restored, &first).await, Stage::Finished);
    // The second, back from the backup at the method's call with no history to tell, is
    // never reported failed: nothing says it wasn't paid.
    assert!(
        !outcomes.iter().any(
            |outcome| matches!(outcome, PayOutcome::Failed { attempt_id, .. } if *attempt_id == second)
        ),
        "{outcomes:?}"
    );
    assert_eq!(w.rail.executions(&first), 1);
    assert_eq!(w.rail.executions(&second), 1);
}

/// How the user's resolution of one of two payments in flight reached the dispatch.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sent {
    /// Its batch never left: kept, nothing applied.
    Unsent,
    /// Applied at the Dispatch, the answer lost: kept, to be sent again.
    AnswerLost,
    /// Applied and answered: the entry is over before the resume.
    Applied,
}

/// What the method's history shows for a payment when the resume runs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shows {
    Paid,
    Failed,
    Pending,
}

/// Two payments in flight, one of them (`target`) resolved by the user as `resolution` with
/// its batch `sent` so, the method showing `shows` for the two, and the Dispatch failing as
/// `timeouts` and `claim_unavailable` say: resumed twice. `resume_payments` checks in test
/// builds that each result agrees with the journal it left, every entry that ended in the
/// call reported exactly once; this adds that no payment is made twice.
async fn resumed_under(
    target: usize,
    resolution: Option<(paykit_sdk::Resolution, Sent)>,
    shows: [Shows; 2],
    timeouts: usize,
    claim_unavailable: bool,
) {
    use pubky_dispatch_client::app::UserConfirmed;
    let case = format!(
        "target {target}, {resolution:?}, {shows:?}, {timeouts} timeouts, claims \
         unavailable {claim_unavailable}"
    );
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let attempts = two_in_flight(&w, &mut wallet).await;
    // The user's `failed`, kept behind a timeout, which no call has reported yet.
    let mut user_failed_kept = false;
    let mut told = Vec::new();
    let ops_time_out = |n: usize| {
        for _ in 0..n {
            w.alice.switch().fail_next(Fault::Unanswered {
                path: "/ops".into(),
                after: 0,
            });
        }
    };
    if let Some((resolution, sent)) = resolution {
        let obligation = wallet
            .payments()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.attempt_id == attempts[target])
            .unwrap()
            .obligation;
        match sent {
            Sent::Unsent => ops_time_out(1),
            Sent::AnswerLost => w.alice.switch().fail_next(Fault::LoseAnswer {
                path: "/ops".into(),
            }),
            Sent::Applied => {}
        }
        // Refused, kept or applied: each is a state a resume may meet.
        user_failed_kept =
            matches!(resolution, paykit_sdk::Resolution::Failed) && sent != Sent::Applied;
        let _ = wallet
            .resolve_attempt(
                &obligation,
                attempts[target],
                resolution,
                UserConfirmed::new(),
            )
            .await;
    }
    for (attempt, shows) in attempts.iter().zip(shows) {
        match shows {
            Shows::Paid => w.rail.settle(attempt, true),
            Shows::Failed => w.rail.settle(attempt, false),
            Shows::Pending => {}
        }
    }
    ops_time_out(timeouts);
    if claim_unavailable {
        w.alice.switch().fail_next(Fault::Error {
            path: "claims".into(),
            code: pubky_dispatch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    }
    for _ in 0..2 {
        // An error carries no outcome and is no disagreement. A result is checked against
        // the journal inside the call; here, that it names only these two payments.
        if let Ok(outcomes) = wallet.resume_payments().await {
            assert!(
                outcomes
                    .iter()
                    .all(|outcome| attempts.contains(&outcome.attempt_id())),
                "{case}: {outcomes:?}"
            );
            told.extend(outcomes);
        }
    }
    // No payment is made twice, under its attempt or any other.
    let executions = w.rail.all_executions();
    assert!(
        executions.keys().all(|attempt| attempts.contains(attempt)),
        "{case}: {executions:?}"
    );
    assert!(
        executions.values().all(|n| *n <= 1),
        "{case}: {executions:?}"
    );
    // With the Dispatch answering again, the journal settles: nothing is left told as open
    // that the journal holds as over, which the check inside judges once more.
    let settled = wallet.resume_payments().await;
    assert!(settled.is_ok(), "{case}: {settled:?}");
    told.extend(settled.unwrap_or_default());
    // The user's `failed`, applied by its kept batch, is reported as the user's, once, where
    // the method neither paid nor failed it.
    if user_failed_kept && shows[target] == Shows::Pending {
        let over: Vec<_> = told
            .iter()
            .filter(|outcome| outcome.attempt_id() == attempts[target])
            .filter(|outcome| {
                matches!(
                    outcome,
                    PayOutcome::Failed { .. }
                        | PayOutcome::Resolved { .. }
                        | PayOutcome::Paid { .. }
                )
            })
            .collect();
        assert!(
            matches!(
                over.as_slice(),
                [PayOutcome::Failed { reason, .. }] if reason == "the user resolved it as failed"
            ),
            "{case}: {told:?}"
        );
    }
}

/// Every combination for a resolution of the payment at `target`.
async fn every_combination(target: usize) {
    use paykit_sdk::Resolution;
    let mut resolutions = vec![None];
    for resolution in [Resolution::Failed, Resolution::Unknown, Resolution::Done] {
        for sent in [Sent::Unsent, Sent::AnswerLost, Sent::Applied] {
            resolutions.push(Some((resolution, sent)));
        }
    }
    for resolution in resolutions {
        for resolved in [Shows::Paid, Shows::Failed, Shows::Pending] {
            // The other payment either writes, which sends a kept batch ahead of it, or
            // doesn't.
            for other in [Shows::Paid, Shows::Pending] {
                let mut shows = [other; 2];
                shows[target] = resolved;
                for (timeouts, claim_unavailable) in [(0, false), (1, false), (1, true), (2, false)]
                {
                    Box::pin(resumed_under(
                        target,
                        resolution,
                        shows,
                        timeouts,
                        claim_unavailable,
                    ))
                    .await;
                }
            }
        }
    }
}

#[tokio::test]
async fn test_resume_result_agrees_with_the_journal_whatever_happens_to_the_first_payment() {
    every_combination(0).await;
}

#[tokio::test]
async fn test_resume_result_agrees_with_the_journal_whatever_happens_to_the_second_payment() {
    every_combination(1).await;
}
