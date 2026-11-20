//! Debit and receipt across an older backup of the same approving wallet.

use super::*;

pub(super) async fn payment_after_restore(
    world: &World,
    wallets: (Paykit, Paykit),
    storage: Arc<ListsStore>,
    rail: Arc<paykit_sdk::rail::FakeRail>,
    key: [u8; 32],
    backup: PaykitBackup,
    amounts: [&str; 3],
) -> TestResult {
    use crate::common::{draft, request_from};
    use pubky_switch_client::clock::Clock;

    let (mut payer, mut seller) = wallets;
    let identity = payer.identity();
    rail.set_history_payer(identity);
    let mut paid = Vec::new();
    for (index, amount) in amounts.into_iter().enumerate() {
        let request = Box::pin(request_from(
            &mut seller,
            identity,
            draft(amount, world.clock().now_ms()),
        ))
        .await;
        Box::pin(finish_listing(&mut payer)).await;
        if index != 0 {
            let held = payer
                .request(&request)
                .await?
                .held
                .expect("request is inside the restored wallet's uncertain interval");
            let refused = Box::pin(payer.pay_under_allowance(&request, &key))
                .await
                .outcome
                .unwrap_err();
            assert!(
                matches!(refused.refusal(), Some(paykit_sdk::Refusal::HeldAfterRestore { held: shown }) if **shown == held),
                "{refused}"
            );
            assert_eq!(rail.all_executions().len(), 1);
            Box::pin(payer.confirm_possibly_cancelled(&request, &held, UserConfirmed::new()))
                .await?;
        }
        if index == 2 {
            rail.set_history_payer(seller.identity());
            let held = Box::pin(payer.pay_under_allowance(&request, &key))
                .await
                .outcome
                .unwrap_err();
            assert!(held.to_string().contains("wallet history"), "{held}");
            assert_eq!(rail.all_executions().len(), 1);
            rail.set_history_payer(identity);
        }
        let result = Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome;
        if index == 1 {
            let refusal = result.unwrap_err();
            assert!(refusal.to_string().contains("lifetime"), "{refusal}");
            assert_eq!(rail.all_executions().len(), 1);
            continue;
        }
        let paykit_sdk::PayOutcome::Paid { attempt_id, .. } =
            result.unwrap_or_else(|error| panic!("pay {amount}: {error}"))
        else {
            panic!("expected a completed transfer");
        };
        paid.push(attempt_id);
        assert_eq!(rail.pay_calls(&attempt_id), 1);
        assert_eq!(
            rail.paid_amount(&attempt_id),
            Some(crate::common::btc(if index == 0 { "70" } else { "30" }))
        );
        Box::pin(finish_listing(&mut seller)).await;
        let sale = seller
            .payee_activity()
            .await?
            .into_iter()
            .find(|sale| sale.attempt_id == Some(attempt_id))
            .expect("seller received the proof");
        let next_request = if amount == "70" {
            Some(Box::pin(seller.send_request(identity, draft("2", world.now_ms()))).await?)
        } else {
            None
        };
        let receipt = Box::pin(seller.confirm_payment(&sale.record))
            .await?
            .expect("seller receipt");
        if let Some(next) = next_request {
            storage.lists_unknown(true);
            let waiting = Box::pin(payer.receive(None)).await?;
            assert!(waiting.report.events.iter().any(|event| matches!(
                event,
                pubky_switch_client::app::AppEvent::ProtocolDeferred {
                    reason: "sender lists unknown",
                    ..
                }
            )));
            assert_eq!(
                waiting.applied.iter().filter(|id| *id == &receipt).count(),
                1
            );
            assert!(!waiting.applied.contains(&next.msg_id));
            storage.lists_unknown(false);
            let resumed = Box::pin(payer.receive(None)).await?;
            assert_eq!(
                resumed
                    .applied
                    .iter()
                    .filter(|id| *id == &next.msg_id)
                    .count(),
                1
            );
            assert!(!resumed.applied.contains(&receipt));
        }
        Box::pin(finish_listing(&mut payer)).await;
        let receipts = payer
            .receipts(paykit_sdk::ReceiptsOf::Request(request))
            .await?;
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].msg_id, receipt);
        assert_eq!(receipts[0].attempt_id, attempt_id);
        assert_eq!(receipts[0].payee, seller.identity());

        if index == 0 {
            let current = Box::pin(payer.reconcile_allowance(&key))
                .await?
                .expect("paid ledger before restoring");
            assert_eq!(
                (
                    current.reserved(),
                    current.spent(),
                    current.lifetime().as_str()
                ),
                (0, 1, "70")
            );
            drop(payer);
            payer = Box::pin(restore_after_first_payment(
                world,
                storage.clone(),
                rail.clone(),
                &key,
                &backup,
            ))
            .await?;
        }
        Box::pin(payer.resume_payments()).await?;
        let usage = Box::pin(payer.reconcile_allowance(&key))
            .await?
            .expect("verified budget");
        assert_eq!(
            usage.lifetime().as_str(),
            if index == 0 { "70" } else { "100" }
        );
        assert_eq!(
            (usage.reserved(), usage.spent(), usage.released()),
            (0, paid.len() as u64, 0)
        );
        for attempt in &paid {
            assert_eq!(rail.pay_calls(attempt), 1);
            assert_eq!(rail.executions(attempt), 1);
        }
    }
    assert_eq!(rail.all_executions().len(), 2);
    Ok(())
}

async fn restore_after_first_payment(
    world: &World,
    storage: Arc<dyn StateStore>,
    rail: Arc<paykit_sdk::rail::FakeRail>,
    key: &[u8; 32],
    backup: &PaykitBackup,
) -> TestResult<Paykit> {
    let mut payer = Box::pin(Paykit::restore(
        app_env(world, storage),
        Default::default(),
        backup,
        rail,
        Arc::new(TestTimer {
            clock: world.clock().clone(),
        }),
    ))
    .await?;
    Box::pin(finish_listing(&mut payer)).await;
    let mut finished = false;
    for _ in 0..8 {
        match Box::pin(payer.reconcile()).await {
            Ok(report) => {
                assert!(!report.payer_skipped);
                finished = true;
                break;
            }
            Err(paykit_sdk::PaykitError::Refused(paykit_sdk::Refusal::NotNow(reason)))
                if reason.contains("Call reconcile again") => {}
            Err(error) => return Err(error.into()),
        }
    }
    assert!(finished);
    Box::pin(payer.end_reconcile(UserConfirmed::new())).await?;
    let before = payer.allowance(key).await?;
    assert_eq!(before.status, AllowanceStatus::Accepted);
    assert_eq!(before.blocked, Some(false));
    assert!(!before.consented);
    let usage = Box::pin(payer.reconcile_allowance(key))
        .await?
        .expect("restored complete paid ledger");
    assert_eq!(
        (usage.reserved(), usage.spent(), usage.lifetime().as_str()),
        (0, 1, "70")
    );
    assert!(Box::pin(payer.resume_allowance_executor(key))
        .await
        .is_err());
    Box::pin(payer.confirm_allowance_executor(key, UserConfirmed::new()))
        .await
        .expect("reconfirm the grant with its reconciled paid ledger");
    world.advance_ms(paykit_sdk::LISTS_REFRESH_MS + 1);
    Box::pin(finish_listing(&mut payer)).await;
    Box::pin(finish_listing(&mut payer)).await;
    let floors = payer.app().records_of("block_floor").await?;
    assert_eq!(floors.len(), 1);
    let floor = paykit_lib::records::BlockFloorRecord::read(floors[0].content.value().unwrap())?;
    let after = floor.floor_ms + pubky_switch_core::records::CLOCK_TOLERANCE_MS + 1;
    world.advance_ms(after.saturating_sub(world.now_ms()));
    Ok(payer)
}
