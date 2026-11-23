//! A foreground payer keeps a signed own-account recovery notice across reopening.

use std::{collections::BTreeMap, error::Error, sync::Arc};

use paykit_sdk::{
    rail::FakeRail, AppScopes, PayOptions, PayOutcome, Paykit, RequestRef, RequestStatus,
};
use pubky_switch_client::app::{AppError, AppEvent, Refusal as ClientRefusal};
use pubky_switch_client::error::StoreError;
use pubky_switch_client::store::{Fault, FaultStore};
use pubky_switch_core::http::NoticeReason;
use pubky_switch_harness::{Backend, World};

use crate::common::{draft, enrolled_wallet, request_from, store, TestTimer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

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
    // A payment left in flight: its repeat meets the notice too.
    rail.set_declaration(
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
    );
    let in_flight = Box::pin(request_from(
        &mut seller,
        payer.identity(),
        draft("2", world.now_ms()),
    ))
    .await;
    Box::pin(payer.receive(None)).await?;
    assert_eq!(payer.request(&request).await?.status, RequestStatus::Open);
    rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending {
        attempt_id: held, ..
    } = Box::pin(payer.pay_request(&in_flight, PayOptions::default()))
        .await
        .outcome?
    else {
        return Err("the first payment did not stay in flight".into());
    };

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
    assert_eq!(rail.all_pay_calls(), BTreeMap::from([(held, 1)]));

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
    held: paykit_lib::AttemptId,
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
    // The repeat isn't reported as one that may pass later: the client's refusal or the
    // switch's own answer says the account awaits a recovery.
    let resumed = Box::pin(payer.resume_payments()).await;
    assert!(
        matches!(&resumed, Err(paykit_sdk::PaykitError::Client(AppError::Refused(refused))) if refused == &reason)
            || matches!(&resumed, Err(error) if error.code() == Some(pubky_switch_core::http::ErrorCode::RecoveryRequired)),
        "{resumed:?}"
    );
    assert_eq!(rail.all_pay_calls(), BTreeMap::from([(held, 1)]));
    Ok(payer)
}
