//! The host can distinguish an unknown list from a failed read of that list.

use std::error::Error;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use paykit_sdk::rail::FakeRail;
use paykit_sdk::{AppScopes, PayOptions, PayOutcome, PaykitConfig, RequestStatus};
use pubky_switch_client::app::{AppConfig, AppEvent, RETRY_EVENT_READS};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::StoreError;
use pubky_switch_harness::{Backend, World};

use crate::common::{draft, enrolled_wallet, request_from, store, ListsStore};

type TestResult = Result<(), Box<dyn Error>>;

#[tokio::test]
async fn test_list_wait_and_store_failure_on_scripted() -> TestResult {
    compare(Backend::Scripted).await
}

#[tokio::test]
async fn test_list_wait_and_store_failure_on_postgresql() -> TestResult {
    compare(Backend::Service).await
}

async fn compare(backend: Backend) -> TestResult {
    for missing_time in [true, false] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(receive_request_and_receipt(&world, missing_time)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn receive_request_and_receipt(world: &World, missing_time: bool) -> TestResult {
    let lists = Arc::new(ListsStore::default());
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut buyer = Box::pin(enrolled_wallet(
        world,
        "buyer",
        AppScopes::new().pay().manage_contacts().build()?,
        PaykitConfig::default(),
        &rail,
        lists.clone(),
    ))
    .await?;
    let mut shop = Box::pin(enrolled_wallet(
        world,
        "seller",
        AppScopes::new().request().manage_contacts().build()?,
        PaykitConfig::default().with_app(AppConfig {
            publishes_record: true,
            ..Default::default()
        }),
        &rail,
        store(),
    ))
    .await?;
    shop.app_mut().invite(&buyer.identity()).await?;
    buyer.app_mut().invite(&shop.identity()).await?;

    let paid = request_from(
        &mut shop,
        buyer.identity(),
        draft("1", world.clock().now_ms()),
    )
    .await;
    buyer.receive(None).await?;
    let PayOutcome::Paid { attempt_id, .. } = buyer
        .pay_request(&paid, PayOptions::default())
        .await
        .outcome?
    else {
        return Err("first request did not pay".into());
    };
    shop.receive(None).await?;
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|activity| activity.request_hash == Some(paid.request_hash))
        .ok_or("seller did not receive the proof")?;
    let request = shop
        .send_request(buyer.identity(), draft("2", world.clock().now_ms()))
        .await?;
    let held = request.request.ok_or("second request was not sent")?;
    let receipt = shop
        .confirm_payment(&sale.record)
        .await?
        .ok_or("receipt was not sent")?;

    lists.lists_unknown(missing_time);
    lists.deny_reads_fail(!missing_time);
    let since = world.clock().now_ms();
    let mut held_seq = None;
    let mut receipt_seq = None;
    for attempt in 1..=RETRY_EVENT_READS + 2 {
        let waiting = attempt <= RETRY_EVENT_READS;
        if attempt == RETRY_EVENT_READS + 1 {
            lists.lists_unknown(false);
            lists.deny_reads_fail(false);
        }
        let missing_before = lists.missing_list_reads.load(Ordering::SeqCst);
        let errors_before = lists.failed_deny_reads.load(Ordering::SeqCst);
        let read = Box::pin(buyer.receive(None)).await?;
        let mut deferrals = 0;
        let mut failures = 0;
        for event in &read.report.events {
            match event {
                AppEvent::ProtocolDeferred {
                    seq,
                    stored_id,
                    protocol,
                    reason,
                } => {
                    assert_eq!(protocol, "paykit");
                    assert_eq!(*reason, "sender lists unknown");
                    assert_eq!(*stored_id, None);
                    assert_eq!(*held_seq.get_or_insert(*seq), *seq);
                    deferrals += 1;
                }
                AppEvent::StoreFailing {
                    seq,
                    stored_id,
                    reads,
                    since_ms,
                    failure,
                } => {
                    assert_eq!(*reads, RETRY_EVENT_READS);
                    assert_eq!(*since_ms, since);
                    assert_eq!(*stored_id, None);
                    assert_eq!(failure.error(), &StoreError::Unavailable);
                    assert!(!failure.is_of_entry());
                    assert_eq!(*held_seq.get_or_insert(*seq), *seq);
                    failures += 1;
                }
                AppEvent::MessageReceived { msg_id, seq, .. } if msg_id == &receipt => {
                    assert_eq!(attempt, 1);
                    receipt_seq = Some(*seq);
                }
                AppEvent::MessageReceived { msg_id, seq, .. } if msg_id == &request.msg_id => {
                    assert_eq!(attempt, RETRY_EVENT_READS + 1);
                    assert_eq!(held_seq, Some(*seq));
                }
                AppEvent::MessageDropped { .. } | AppEvent::Parked { .. } => panic!("{event:?}"),
                _ => {}
            }
        }
        assert_eq!(deferrals, usize::from(waiting && missing_time));
        assert_eq!(
            failures,
            usize::from(!missing_time && attempt == RETRY_EVENT_READS)
        );
        assert_eq!(
            lists.missing_list_reads.load(Ordering::SeqCst) > missing_before,
            waiting && missing_time
        );
        assert_eq!(
            lists.failed_deny_reads.load(Ordering::SeqCst) > errors_before,
            waiting && !missing_time
        );
        assert_eq!(
            read.applied.iter().filter(|id| *id == &receipt).count(),
            usize::from(attempt == 1)
        );
        assert_eq!(
            read.applied
                .iter()
                .filter(|id| *id == &request.msg_id)
                .count(),
            usize::from(attempt == RETRY_EVENT_READS + 1)
        );
        assert!(buyer.request(&paid).await?.receipt.is_some());
        assert!(buyer.app_mut().parked_entries().await?.is_empty());
        if waiting {
            assert!(!buyer
                .requests()
                .await?
                .iter()
                .any(|item| item.request == held));
            if let Some(seq) = held_seq {
                assert!(read.report.cursor.seq < seq);
            }
        } else {
            assert_eq!(buyer.request(&held).await?.status, RequestStatus::Open);
            assert!(read.report.cursor.seq >= receipt_seq.ok_or("missing receipt delivery")?);
        }
        world.advance_ms(1_000);
    }
    assert!(
        held_seq.ok_or("missing request delivery")?
            < receipt_seq.ok_or("missing receipt delivery")?
    );
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    assert_eq!(rail.executions(&attempt_id), 1);
    Ok(())
}
