//! A foreground payer keeps a signed own-account recovery notice across reopening.

use std::{collections::BTreeMap, error::Error, sync::Arc};

use paykit_sdk::{
    rail::FakeRail, AppScopes, PayOptions, PayOutcome, Paykit, RequestRef, RequestStatus,
};
use pubky_dispatch_client::app::{AppError, AppEvent, Refusal as ClientRefusal};
use pubky_dispatch_client::error::StoreError;
use pubky_dispatch_client::store::{Fault, FaultStore};
use pubky_dispatch_core::http::NoticeReason;
use pubky_dispatch_harness::{Backend, World};

use crate::common::{draft, enrolled_wallet, request_from, store, TestTimer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// How many times the fixture pays a request a check held before its handoff.
const MAX_HELD_PAYMENTS: usize = 3;

#[tokio::test]
async fn test_payer_keeps_its_own_recovery_notice_and_payment_hold_after_reopen() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(payer_notice(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn payer_notice(world: &World) -> TestResult {
    let storage = Arc::new(FaultStore::new(store(), Fault::None));
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut payer = Box::pin(enrolled_wallet(
        world,
        "buyer",
        AppScopes::new().pay().build()?,
        Default::default(),
        &rail,
        storage.clone(),
    ))
    .await?;
    let mut seller = Box::pin(enrolled_wallet(
        world,
        "seller",
        AppScopes::new().request().build()?,
        Default::default(),
        &rail,
        store(),
    ))
    .await?;
    payer.app_mut().allow_connect(&seller.identity()).await?;
    seller.app_mut().allow_connect(&payer.identity()).await?;
    let request = Box::pin(request_from(
        &mut seller,
        payer.identity(),
        draft("1", world.now_ms()),
    ))
    .await;
    // Two payments left in flight: a resume under the notice must reach both, whichever
    // the journal holds first.
    rail.set_declaration(
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true)
        .with_history(true),
    );
    let mut in_flight = Vec::new();
    for value in ["2", "3"] {
        in_flight.push(
            Box::pin(request_from(
                &mut seller,
                payer.identity(),
                draft(value, world.now_ms()),
            ))
            .await,
        );
    }
    Box::pin(payer.receive(None)).await?;
    assert_eq!(payer.request(&request).await?.status, RequestStatus::Open);
    let mut attempts = Vec::new();
    for unpaid in &in_flight {
        // The last check before money moves can hold a payment, as a host would see it, when
        // a read during that check fails: `Pending`, with the method not called. A host
        // follows it as the wallet says: a resume closes it unpaid, and the request is paid
        // again. Each is tried at most `MAX_HELD_PAYMENTS` times, and must end in flight.
        // The method's call hangs; a payment held before its handoff doesn't call it, so
        // the one `Hang` waits for the call the retry makes.
        let mut started = None;
        rail.next(paykit_sdk::rail::Behaviour::Hang);
        for _ in 0..MAX_HELD_PAYMENTS {
            let PayOutcome::Pending { attempt_id, .. } =
                Box::pin(payer.pay_request(unpaid, PayOptions::default()))
                    .await
                    .outcome?
            else {
                return Err("a payment did not stay in flight".into());
            };
            if rail.pay_calls(&attempt_id) == 1 {
                started = Some(attempt_id);
                break;
            }
            Box::pin(payer.resume_payments()).await?;
        }
        attempts.push(started.ok_or("a payment was held before its handoff every time")?);
    }
    // Both method calls started: each entry is `Paying`, so the resume below meets two
    // payments in flight.
    let payments = payer.payments().await?;
    for attempt in &attempts {
        assert_eq!(rail.pay_calls(attempt), 1);
        assert_eq!(
            payments
                .iter()
                .find(|view| view.attempt_id == *attempt)
                .map(|view| view.stage),
            Some(paykit_sdk::Stage::Paying)
        );
    }
    // The journal holds its entries by attempt ID: the first fails before the notice, so
    // writing its outcome is what the notice holds.
    attempts.sort_by_key(|attempt| attempt.to_text());
    let [first, later] = attempts[..] else {
        return Err("two payments in flight".into());
    };
    rail.settle(&first, false);
    let held = [first, later];

    let owner = world.identity("buyer")?;
    let origin = "https://buyer.public.test";
    let generation = payer.app().own_identity().generation();
    Box::pin(world.set_account_missing(origin, &owner, true)).await?;
    let reopened = Box::pin(reopen_held_payer(
        world, payer, &request, storage, &rail, held,
    ))
    .await;
    Box::pin(world.set_account_missing(origin, &owner, false)).await?;
    let mut payer = reopened?;
    assert!(payer.app().recovery_required().is_some());

    let read = Box::pin(payer.receive(None)).await?;
    assert!(!read.report.stopped);
    assert_eq!(payer.app().own_identity().generation(), generation);
    assert!(payer.app().recovery_required().is_none());
    assert!(read.report.events.iter().any(|event| matches!(
        event, AppEvent::RecoveryNoticeCleared { generation: cleared, .. } if *cleared == generation
    )));
    assert_eq!(
        rail.all_pay_calls(),
        BTreeMap::from([(first, 1), (later, 1)])
    );

    let PayOutcome::Paid { attempt_id, .. } =
        Box::pin(payer.pay_request(&request, PayOptions::default()))
            .await
            .outcome?
    else {
        return Err("the request did not pay after the recovery notice cleared".into());
    };
    Box::pin(payer.resume_payments()).await?;
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    assert_eq!(rail.all_executions().get(&attempt_id), Some(&1));
    Ok(())
}

async fn reopen_held_payer(
    world: &World,
    mut payer: Paykit,
    request: &RequestRef,
    storage: Arc<FaultStore>,
    rail: &Arc<FakeRail>,
    held: [paykit_lib::AttemptId; 2],
) -> TestResult<Paykit> {
    // A stopped read must reach the host even when resend upkeep cannot read its queue.
    storage.fail_scan(b"p/paykit/ask/", 0);
    let read = Box::pin(payer.receive(None)).await?;
    assert!(read.report.stopped);
    assert!(read.applied.is_empty());
    assert_eq!(read.resends, 0);
    assert!(matches!(
        payer.app().protocol_scan("paykit", b"ask/").await,
        Err(AppError::Store(StoreError::Unavailable))
    ));
    let notice = payer
        .app()
        .recovery_required()
        .ok_or("missing own recovery notice")?;
    assert_eq!(notice.generation, payer.app().own_identity().generation());
    assert_eq!(notice.reason, Some(NoticeReason::Behind));
    assert!(!notice.permanent);
    let duty = read
        .report
        .events
        .iter()
        .find_map(|event| match event {
            AppEvent::RecoveryRequired { id, notice: seen } if seen == &notice => Some(*id),
            _ => None,
        })
        .ok_or("the foreground read did not report the notice")?;

    let app = payer.app_id();
    drop(payer);
    let mut payer = Box::pin(Paykit::load(
        world.app_env(storage),
        Default::default(),
        app,
        rail.clone(),
        Arc::new(TestTimer {
            clock: world.clock().clone(),
        }),
    ))
    .await?;
    assert_eq!(payer.app().recovery_required(), Some(notice));
    let event = AppEvent::RecoveryRequired { id: duty, notice };
    assert!(payer
        .app_mut()
        .pending_duty_events()
        .await?
        .iter()
        .any(|pending| pending.event == event));
    assert!(Box::pin(payer.receive(None)).await?.report.stopped);
    let blocked = Box::pin(payer.pay_request(request, PayOptions::default()))
        .await
        .outcome
        .unwrap_err();
    let reason = ClientRefusal::RecoveryRequired {
        generation: notice.generation,
    };
    assert!(
        matches!(&blocked, paykit_sdk::PaykitError::Client(AppError::Refused(refused)) if refused == &reason),
        "{blocked}"
    );
    // A recovery notice the client holds stops the wallet as a whole: a resume under it
    // returns the recovery at the first entry whose step the notice refuses, and nothing
    // after it is written or looked up, so the payment the method reports made later waits
    // for the resume after the notice clears.
    let [first, later] = held;
    let refused = |resumed: &paykit_sdk::Result<Vec<PayOutcome>>| {
        matches!(resumed, Err(paykit_sdk::PaykitError::Client(AppError::Refused(refused))) if refused == &reason)
            || matches!(resumed, Err(error) if error.code() == Some(pubky_dispatch_core::http::ErrorCode::RecoveryRequired))
    };
    let resumed = Box::pin(payer.resume_payments()).await;
    assert!(refused(&resumed), "{resumed:?}");
    // The method reports the later payment made only now; under the notice it still waits.
    rail.settle(&later, true);
    let resumed = Box::pin(payer.resume_payments()).await;
    assert!(refused(&resumed), "{resumed:?}");
    let stage = |attempt: paykit_lib::AttemptId, payments: &[paykit_sdk::PaymentView]| {
        payments
            .iter()
            .find(|view| view.attempt_id == attempt)
            .map(|view| view.stage)
    };
    let payments = payer.payments().await?;
    assert_eq!(stage(first, &payments), Some(paykit_sdk::Stage::RailFailed));
    assert_eq!(stage(later, &payments), Some(paykit_sdk::Stage::Paying));
    assert_eq!(
        rail.all_pay_calls(),
        BTreeMap::from([(first, 1), (later, 1)])
    );
    Ok(payer)
}
