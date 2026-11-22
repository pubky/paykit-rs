//! A foreground host receives the verified notice and its original-generation clearance.
use super::*;

fn required(events: &[WalletEvent], notice: &WalletRecoveryNotice) -> TestResult<Vec<u8>> {
    let mut matching = events.iter().filter(|event| {
        matches!(&event.data, WalletEventData::RecoveryRequired { notice: seen } if seen == notice)
    });
    let event = matching
        .next()
        .ok_or("the verified notice was not delivered")?;
    assert!(matching.next().is_none());
    assert_eq!(event.owners, ["paykit"]);
    Ok(event.id.clone().ok_or("missing durable notice ID")?)
}

async fn recovery(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (host, payer, app, mut signer) =
        Box::pin(enrolled_payer(world, root.path(), account.clone())).await?;
    let generation = loop {
        let report = Box::pin(payer.receive()).await?;
        if !report.more {
            assert!(!report.stopped);
            break report.generation;
        }
    };
    let owner = world.identity("buyer")?;
    let origin = "https://buyer.payments.test";
    Box::pin(world.set_account_missing(origin, &owner, true)).await?;
    let report = Box::pin(payer.receive()).await?;
    assert!(report.stopped);
    let notice = payer
        .recovery_required()
        .await?
        .ok_or("no recovery notice")?;
    assert_eq!(notice.generation, generation);
    assert_eq!(notice.reason, Some(RecoveryReason::Behind));
    assert!(!notice.permanent);
    assert_eq!(notice.first_seen_ms, world.now_ms());
    let notice_id = required(&report.events, &notice)?;
    payer.shutdown().await?;

    let payer = host.load(app.clone()).await?;
    assert_eq!(payer.recovery_required().await?, Some(notice.clone()));
    assert_eq!(
        required(&payer.unacknowledged_events().await?, &notice)?,
        notice_id
    );
    payer.acknowledge_events(notice_id.clone()).await?;
    assert!(payer
        .unacknowledged_events()
        .await?
        .iter()
        .all(|e| e.id.as_ref() != Some(&notice_id)));
    assert_eq!(payer.recovery_required().await?, Some(notice));

    let successor = "https://new-buyer.payments.test";
    world.add_switch(successor, &[&owner]).await?;
    let recovered = Box::pin(signer.declare_provider_loss(
        successor,
        pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await?;
    assert!(recovered.generation() > generation);
    let mut cleared = None;
    for _ in 0..8 {
        let report = Box::pin(payer.receive()).await?;
        for event in report.events {
            if let WalletEventData::RecoveryNoticeCleared { generation: lost } = event.data {
                assert_eq!(lost, generation);
                assert_eq!(report.generation, recovered.generation());
                assert_eq!(event.owners, ["paykit"]);
                assert!(cleared.is_none());
                cleared = event.id;
            }
        }
        if cleared.is_some() {
            break;
        }
    }
    let cleared = cleared.ok_or("the new generation did not clear the original notice")?;
    assert_ne!(cleared, notice_id);
    assert!(payer.recovery_required().await?.is_none());
    payer.shutdown().await?;

    let payer = host.load(app).await?;
    let pending = payer.unacknowledged_events().await?;
    assert!(pending.iter().any(|e| e.id.as_ref() == Some(&cleared) &&
        matches!(e.data, WalletEventData::RecoveryNoticeCleared { generation: old } if old == generation)));
    payer.acknowledge_events(cleared.clone()).await?;
    assert!(payer
        .unacknowledged_events()
        .await?
        .iter()
        .all(|e| e.id.as_ref() != Some(&cleared)));
    assert_eq!(account.count(), 0);
    Ok(())
}

#[test]
fn test_recovery_notice_and_its_clearance_survive_host_reopen_on_scripted() {
    run(Backend::Scripted, recovery);
}

#[test]
fn test_recovery_notice_and_its_clearance_survive_host_reopen_on_postgresql() {
    run(Backend::Service, recovery);
}
