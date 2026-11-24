//! A resume visits every journal entry (Paykit spec 7.2, 7.3): a payment the method reports
//! made is journaled with its proof, without the switch, whatever another entry's step met,
//! and nothing is journaled under a wallet-wide stop.

use std::sync::Arc;

use paykit_lib::AttemptId;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaykitConfig, Rail, Stage};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::TransportError;
use pubky_switch_client::testing::Fault;

use crate::common::{draft, request_from, store, World};

/// Two payments left in flight, in the order a resume visits them.
async fn two_in_flight(w: &World, wallet: &mut Paykit) -> [AttemptId; 2] {
    let mut shop = w.shop().await;
    for amount in ["1", "2"] {
        request_from(&mut shop, w.alice_id(), draft(amount, w.clock.now_ms())).await;
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

/// The switch answers nothing for the next `n` requests.
fn switch_down(w: &World, n: usize) {
    for _ in 0..n {
        w.alice.switch().fail_next(Fault::Unreachable {
            path: String::new(),
            after: 0,
        });
    }
}

#[tokio::test]
async fn test_resume_switch_unreachable_still_journals_a_later_settled_payment() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let [first, second] = two_in_flight(&w, &mut wallet).await;
    // The first failed at the method, so its outcome needs the switch; the second paid.
    w.rail.settle(&first, false);
    w.rail.settle(&second, true);
    switch_down(&w, 200);
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
    // switch down nothing of it is journaled, even with the method showing it paid.
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
    switch_down(&w, 200);
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
        code: pubky_switch_core::http::ErrorCode::Unavailable,
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
        .leave(pubky_switch_client::app::LeaveMode::Immediate(
            pubky_switch_client::app::UserConfirmed::new(),
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
    use pubky_switch_client::store::{Fault as StoreFault, FaultStore, StateStore};
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
    inner: Arc<pubky_switch_client::testing::Router>,
    next: std::sync::Mutex<Option<pubky_switch_client::error::TransportError>>,
}

impl pubky_switch_client::transport::Transport for RefusingOps {
    fn send(
        &self,
        request: pubky_switch_client::transport::Request,
    ) -> pubky_switch_client::transport::BoxFuture<
        '_,
        Result<
            pubky_switch_client::transport::Response,
            pubky_switch_client::error::TransportError,
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
            Arc::clone(&transport) as Arc<dyn pubky_switch_client::transport::Transport>,
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
        // The first submit fails at the transport. The switch refuses the same batch on
        // retry, leaving the first outcome unwritten (Paykit spec 7.2 step 3).
        w.alice.switch().fail_next(Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::Conflict,
            current: serde_json::Value::Null,
        });
        let resumed = wallet.resume_payments().await;
        let Err(paykit_sdk::PaykitError::Client(failure)) = &resumed else {
            panic!("{error:?}: {resumed:?}");
        };
        assert_eq!(
            failure.unkept(),
            &pubky_switch_client::app::AppError::Transport(error.clone()),
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
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let attempts = two_in_flight(w, &mut wallet).await;
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
            Arc::clone(&transport) as Arc<dyn pubky_switch_client::transport::Transport>,
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
            [PayOutcome::Pending { attempt_id: open, .. }, PayOutcome::Paid { attempt_id: paid, proof_msg_id: Some(_), .. }]
                if *open == second && *paid == first
        ),
        "{outcomes:?}"
    );
    assert_eq!(stage(&wallet, &first).await, Stage::Finished);
    assert_eq!(w.rail.pay_calls(&first), 1);
}

/// Two payments in flight, the user's resolution of the second as failed kept after a
/// timeout, and the transport set to time out the next write too.
async fn second_resolved_failed_and_kept(w: &World) -> (Paykit, [AttemptId; 2], Arc<RefusingOps>) {
    let (mut wallet, [first, second], transport) = in_flight_behind(w).await;
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
            paykit_sdk::Resolution::Failed,
            pubky_switch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(kept.kept_batch().is_some(), "{kept:?}");
    assert_eq!(stage(&wallet, &second).await, Stage::Paying);
    (wallet, [first, second], transport)
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
