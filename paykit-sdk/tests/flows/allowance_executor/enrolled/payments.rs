//! Debit and receipt across an older backup of the same approving wallet.

use super::*;

/// The service opened the claim and reserved the amount before its reply was lost.
pub(super) async fn unanswered_reservation(
    world: &World,
    mut payer: Paykit,
    mut seller: Paykit,
    storage: Arc<ListsStore>,
    rail: Arc<paykit_sdk::rail::FakeRail>,
    key: [u8; 32],
) -> TestResult {
    use crate::common::{draft, request_from};
    use pubky_switch_core::{claims::Outcome, ops::Condition};

    let identity = payer.identity();
    let app_id = payer.app_id();
    rail.set_history_payer(identity);
    let request = Box::pin(request_from(
        &mut seller,
        identity,
        draft("7", world.now_ms()),
    ))
    .await;
    Box::pin(finish_listing(&mut payer)).await;
    let initial = payer
        .app()
        .records_of("allowance_ledgers")
        .await?
        .pop()
        .ok_or("missing initial ledger")?;
    let (_, predecessor) = applied_put(payer.app(), initial.key, initial.version).await;
    assert!(Ledger::from_json(initial.content.value().unwrap())?
        .entries
        .is_empty());

    world.lose_answer("POST", "/ops");
    let error = Box::pin(payer.pay_under_allowance(&request, &key))
        .await
        .outcome
        .unwrap_err();
    let batch = error
        .kept_batch()
        .ok_or("missing unanswered reservation batch")?;
    assert_eq!(world.faults_left(), 0);
    let markers = payer.app().protocol_scan("paykit", b"lb/").await?;
    assert_eq!(markers.len(), 1);
    assert_eq!(payer.app().protocol_scan("paykit", b"lw/").await?.len(), 1);
    assert_eq!(markers[0].1.as_slice(), batch.as_slice());
    assert!(rail.all_executions().is_empty());
    let backup = Box::pin(payer.backup()).await?;
    drop(payer);

    let timer = || {
        Arc::new(TestTimer {
            clock: world.clock().clone(),
        })
    };
    let mut accepted = None;
    for restore in [false, true] {
        let mut wallet = if restore {
            Box::pin(Paykit::restore(
                app_env(world, Arc::new(MemoryStore::new())),
                Default::default(),
                &backup,
                rail.clone(),
                timer(),
            ))
            .await?
        } else {
            Box::pin(Paykit::load(
                app_env(world, storage.clone()),
                Default::default(),
                app_id,
                rail.clone(),
                timer(),
            ))
            .await?
        };
        Box::pin(finish_listing(&mut wallet)).await;
        let usage = Box::pin(wallet.reconcile_allowance(&key))
            .await?
            .ok_or("missing reserved usage")?;
        assert_eq!(
            (
                usage.lifetime().as_str(),
                usage.reserved(),
                usage.spent(),
                usage.released()
            ),
            ("7", 1, 0, 0)
        );
        let record = wallet
            .app()
            .shared_record(&initial.key)
            .await?
            .ok_or("missing applied reservation")?;
        let (write, revision) = applied_put(wallet.app(), initial.key, record.version).await;
        assert_eq!(write.body().batch.id, batch);
        assert!(write.body().conditions.contains(&Condition::RecordCurrent {
            key: initial.key,
            revision: predecessor,
            deleted: false
        }));
        let ledger = Ledger::from_json(record.content.value().unwrap())?;
        let [entry] = ledger.entries.as_slice() else {
            panic!("expected one reserved payment");
        };
        assert_eq!((entry.value.as_str(), entry.outcome), ("7", None));
        let claim = wallet
            .app_mut()
            .read_claim(&entry.obligation)
            .await?
            .ok_or("missing reservation claim")?;
        let [attempt] = claim.view.attempts.as_slice() else {
            panic!("expected one claim attempt");
        };
        assert_eq!(attempt.attempt_id, entry.attempt_id.to_bytes());
        assert_eq!(attempt.outcome, Outcome::Open);
        if let Some(original) = accepted {
            assert_eq!((revision, entry.attempt_id), original);
        }
        accepted = Some((revision, entry.attempt_id));
        for prefix in [b"lw/", b"lb/"] {
            assert!(wallet
                .app()
                .protocol_scan("paykit", prefix)
                .await?
                .is_empty());
        }
        assert_eq!(
            Box::pin(wallet.reconcile_allowance(&key)).await?,
            Some(usage)
        );
        assert_eq!(
            wallet
                .app()
                .shared_record(&initial.key)
                .await?
                .unwrap()
                .version,
            record.version
        );
        assert_eq!(rail.pay_calls(&entry.attempt_id), 0);
        assert!(rail.all_executions().is_empty());
    }
    Ok(())
}

pub(super) async fn late_period_proof(
    world: &World,
    payer: &mut Paykit,
    seller: &mut Paykit,
    rail: &paykit_sdk::rail::FakeRail,
    key: &[u8; 32],
) -> TestResult {
    use crate::common::{draft, request_from};
    use paykit_lib::allowances::EntryOutcome;
    use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};

    rail.set_history_payer(payer.identity());
    let start = world.now_ms() / 1_000 * 1_000;
    let schedule = Recurrence::new(AnchoredRule::new(start, 1, Unit::Hour)?, start, None)?;
    let periods = schedule.periods(2)?;
    let period = PeriodRef::from(periods[0]);
    let next = PeriodRef::from(periods[1]);
    let request = Box::pin(request_from(
        seller,
        payer.identity(),
        draft("7", start).with_recurrence(schedule),
    ))
    .await;
    Box::pin(finish_listing(payer)).await;
    Box::pin(payer.accept_subscription_under_allowance(&request, key)).await?;
    rail.next(paykit_sdk::rail::Behaviour::Fail);
    let failed = Box::pin(payer.pay_period_under_allowance(&request, &period))
        .await
        .outcome?;
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = failed else {
        panic!("expected a reported failure, got {failed:?}");
    };
    assert_eq!(
        super::super::only_ledger(payer).await.0.entries[0].outcome,
        Some(EntryOutcome::Failed)
    );
    assert!(rail.hold_history_outcome(&attempt_id));
    rail.complete_after_failure(&attempt_id);
    Box::pin(payer.resume_payments()).await?;
    let (ledger, revision) = super::super::only_ledger(payer).await;
    let entry = &ledger.entries[0];
    assert_eq!(
        (entry.attempt_id, entry.outcome),
        (attempt_id, Some(EntryOutcome::Done))
    );
    let claim = payer
        .app_mut()
        .read_claim(&entry.obligation)
        .await?
        .unwrap();
    assert_eq!(
        claim.view.attempt(&attempt_id.to_bytes()).unwrap().outcome,
        pubky_switch_core::claims::Outcome::Failed
    );
    Box::pin(finish_listing(seller)).await;
    let sales = seller.payee_activity().await?;
    let sale = sales
        .iter()
        .find(|s| s.request_hash == Some(request.request_hash))
        .unwrap();
    let occurrence = sale
        .periods
        .iter()
        .find(|p| p.billing_period == period)
        .unwrap();
    Box::pin(seller.confirm_payment(&occurrence.record))
        .await?
        .expect("seller receipt");
    Box::pin(finish_listing(payer)).await;
    let settled = payer
        .app_mut()
        .read_claim(&entry.obligation)
        .await?
        .unwrap();
    assert_eq!(
        settled
            .view
            .attempt(&attempt_id.to_bytes())
            .unwrap()
            .outcome,
        pubky_switch_core::claims::Outcome::Done
    );
    let receipts = payer
        .receipts(paykit_sdk::ReceiptsOf::Period(request, period))
        .await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].attempt_id, attempt_id);
    assert!(payer
        .receipts(paykit_sdk::ReceiptsOf::Period(request, next))
        .await?
        .is_empty());
    assert_eq!(super::super::only_ledger(payer).await.1, revision);
    let usage = Box::pin(payer.reconcile_allowance(key))
        .await?
        .expect("paid occurrence");
    assert_eq!((usage.lifetime().as_str(), usage.spent()), ("7", 1));
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    let history = rail.allowance_history(payer.identity()).await?.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].outcome, paykit_sdk::rail::RailOutcome::Failed);
    Ok(())
}

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
