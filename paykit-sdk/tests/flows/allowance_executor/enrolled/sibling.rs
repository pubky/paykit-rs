//! An approval-only app shares the payer identity with the enrolled executor.

use super::*;
use crate::common::{btc, draft, request_from};
use paykit_sdk::{rail::FakeRail, PayOutcome, PayeeStatus, RequestRef};
use pubky_dispatch_client::app::{
    AppError, AppEvent, Collection as DispatchCollection, RecordRead, RecordTarget, Refusal,
};

struct Wallets {
    payer: Paykit,
    seller: Paykit,
    sibling: Paykit,
    rail: Arc<FakeRail>,
    grant: RecordTarget,
    ledger: RecordTarget,
}

async fn wallets(world: &World, proposer: AllowanceRole) -> TestResult<Wallets> {
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let signer = Box::pin(start_signer(world, "payer")).await?;
    let (signer, mut payer) = Box::pin(join_app(
        world,
        "payer",
        "payer",
        signer,
        AppScopes::new().allowance_execute().build()?,
        Arc::new(MemoryStore::new()),
        rail.clone(),
    ))
    .await?;
    let (_, sibling) = Box::pin(join_app(
        world,
        "payer",
        "approver",
        signer,
        AppScopes::new().allowance().build()?,
        Arc::new(MemoryStore::new()),
        rail.clone(),
    ))
    .await?;
    let mut seller = Box::pin(join(
        world,
        "seller",
        AppScopes::new().allowance().request().build()?,
        Arc::new(MemoryStore::new()),
        rail.clone(),
    ))
    .await?;
    assert_eq!(payer.identity(), sibling.identity());
    assert_ne!(payer.app_id(), sibling.app_id());
    assert!(payer.app().cert().holds_scope("paykit_allowance_execute"));
    assert!(sibling.app().cert().holds_scope("paykit_allowance"));
    assert!(!sibling.app().cert().holds_scope("paykit_allowance_execute"));
    let (key, id) = Box::pin(accept(&mut payer, &mut seller, proposer)).await?;
    Box::pin(payer.resume_allowance_executor(&key)).await?;
    let view = payer.allowance(&key).await?;
    assert_eq!(view.executor_app_id, Some(payer.app_id()));
    assert!(view.consented);
    let natural = paykit_lib::derive::natural_key::allowance(&seller.identity(), &id);
    let [grant, ledger] = [Collection::Allowances, Collection::AllowanceLedgers].map(|c| {
        RecordTarget::new(
            DispatchCollection::new(c.name(), c.fixed_policy()),
            &natural,
        )
    });
    assert_eq!(payer.app().record_key(&grant)?, key);
    rail.set_history_payer(payer.identity());
    Ok(Wallets {
        payer,
        seller,
        sibling,
        rail,
        grant,
        ledger,
    })
}

// The host drives each payment on the wallet task, after the request was received.
async fn pay(
    world: &World,
    seller: &mut Paykit,
    mut payer: Paykit,
    key: [u8; 32],
    amount: &str,
) -> TestResult<(Paykit, RequestRef, paykit_sdk::Result<PayOutcome>)> {
    let request = Box::pin(request_from(
        seller,
        payer.identity(),
        draft(amount, world.now_ms()),
    ))
    .await;
    Box::pin(finish_listing(&mut payer)).await;
    let (payer, outcome) = tokio::task::spawn_local(async move {
        let outcome = Box::pin(payer.pay_under_allowance(&request, &key))
            .await
            .outcome;
        (payer, outcome)
    })
    .await?;
    Ok((payer, request, outcome))
}

async fn received_proofs(seller: &mut Paykit) -> TestResult<Vec<String>> {
    let mut proofs = Vec::new();
    for _ in 0..8 {
        let received = Box::pin(seller.receive(None)).await?;
        assert!(!received.report.stopped);
        proofs.extend(
            received
                .report
                .events
                .into_iter()
                .filter_map(|event| match event {
                    AppEvent::MessageReceived { kind, msg_id, .. }
                        if kind == paykit_lib::kinds::Kind::PaymentProof.as_str() =>
                    {
                        Some(msg_id)
                    }
                    _ => None,
                }),
        );
        if !received.report.more && received.records_unlisted.is_none() {
            assert!(!seller.app().listing_owed().await?);
            return Ok(proofs);
        }
    }
    panic!("seller did not finish receiving the payment proofs");
}

async fn funded_ledger(world: &World, proposer: AllowanceRole) -> TestResult {
    let Wallets {
        payer,
        mut seller,
        mut sibling,
        rail,
        grant,
        ledger,
    } = Box::pin(wallets(world, proposer)).await?;
    let original = payer.app_id();
    let key = payer.app().record_key(&grant)?;
    let (mut payer, first, result) = Box::pin(pay(world, &mut seller, payer, key, "70")).await?;
    let PayOutcome::Paid {
        attempt_id: first_attempt,
        proof_msg_id: Some(first_proof),
        ..
    } = result?
    else {
        panic!("expected the first payment");
    };
    assert_eq!(
        Box::pin(received_proofs(&mut seller)).await?,
        [first_proof.as_str()]
    );
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await?
        .expect("funded ledger");
    assert_eq!((usage.lifetime().as_str(), usage.spent()), ("70", 1));
    let RecordRead::Fresh(placed) = payer.app_mut().read_record_fresh(&grant).await? else {
        panic!("expected the original grant");
    };
    let RecordRead::Fresh(funded) = payer.app_mut().read_record_fresh(&ledger).await? else {
        panic!("expected the funded ledger");
    };
    Box::pin(finish_listing(&mut sibling)).await;
    let RecordRead::Fresh(seen) = sibling.app_mut().read_record_fresh(&ledger).await? else {
        panic!("expected the sibling to read the funded ledger");
    };
    assert!(seen.authenticated);
    assert_eq!(
        (&seen.key, &seen.version, &seen.content),
        (&funded.key, &funded.version, &funded.content)
    );

    let value = seen.content.value().expect("live ledger");
    let before = world.trace().lines.len();
    let update = sibling
        .app_mut()
        .put_record(&ledger, value, vec![])
        .await
        .unwrap_err();
    let delete = sibling
        .app_mut()
        .delete_record(&ledger, vec![])
        .await
        .unwrap_err();
    // The client refuses each write before it is sent, a usage refusal carrying only its
    // text: the update for its missing scope (Dispatch spec 10.3), the delete already for the
    // permanent policy's rule, which is checked before scopes (Dispatch wire 6.2).
    for (error, why_refused) in [
        (
            update,
            "the certificate lacks the scope required for the record write",
        ),
        (
            delete,
            "a delete at a permanent policy, in a generation it doesn't pass",
        ),
    ] {
        assert!(
            matches!(&error, AppError::Refused(Refusal::Usage(why)) if why.contains(why_refused)),
            "{error}"
        );
    }
    assert!(
        world.trace().lines[before..]
            .iter()
            .all(|line| line.call != Some("ops")),
        "sibling submitted a record operation"
    );
    for (target, old) in [(&grant, &placed), (&ledger, &funded)] {
        let RecordRead::Fresh(now) = payer.app_mut().read_record_fresh(target).await? else {
            panic!("expected the original wallet to read its live record");
        };
        assert!(now.authenticated);
        assert_eq!(
            (&now.key, &now.version, &now.content),
            (&old.key, &old.version, &old.content)
        );
    }

    let (payer, _, excess) = Box::pin(pay(world, &mut seller, payer, key, "31")).await?;
    let refusal = excess.unwrap_err();
    assert!(
        matches!(refusal.refusal(), Some(paykit_sdk::Refusal::NotAutomatic(why)) if why == "the lifetime limit has no room"),
        "{refusal}"
    );
    assert_eq!(rail.all_pay_calls().values().sum::<u64>(), 1);
    assert!(Box::pin(received_proofs(&mut seller)).await?.is_empty());
    let (mut payer, last, result) = Box::pin(pay(world, &mut seller, payer, key, "30")).await?;
    let PayOutcome::Paid {
        attempt_id: last_attempt,
        proof_msg_id: Some(last_proof),
        ..
    } = result?
    else {
        panic!("expected the remaining budget to pay");
    };
    assert_eq!(payer.app_id(), original);
    assert_ne!(first_attempt, last_attempt);
    assert_ne!(first_proof, last_proof);
    assert_eq!(rail.all_executions().len(), 2);
    let usage = Box::pin(payer.reconcile_allowance(&key))
        .await?
        .expect("final usage");
    assert_eq!(
        (
            usage.lifetime().as_str(),
            usage.spent(),
            usage.reserved(),
            usage.released()
        ),
        ("100", 2, 0, 0)
    );
    assert_eq!(
        Box::pin(received_proofs(&mut seller)).await?,
        [last_proof.as_str()]
    );
    let sales = seller.payee_activity().await?;
    for (request, attempt, amount) in [(first, first_attempt, "70"), (last, last_attempt, "30")] {
        assert_eq!(rail.pay_calls(&attempt), 1);
        assert_eq!(rail.executions(&attempt), 1);
        assert_eq!(rail.paid_amount(&attempt), Some(btc(amount)));
        assert!(sales
            .iter()
            .any(|sale| sale.request_hash == Some(request.request_hash)
                && sale.attempt_id == Some(attempt)
                && sale.status == PayeeStatus::ProofReceived));
    }
    Ok(())
}

async fn check(backend: Backend) -> TestResult {
    tokio::task::LocalSet::new()
        .run_until(async {
            for proposer in [AllowanceRole::Allower, AllowanceRole::Allowee] {
                let Some(world) = World::start(backend)? else {
                    continue;
                };
                let result = Box::pin(funded_ledger(&world, proposer)).await;
                world.close().await?;
                result?;
            }
            Ok(())
        })
        .await
}

#[tokio::test]
async fn test_sibling_cannot_change_funded_ledger_on_scripted_dispatch() -> TestResult {
    check(Backend::Scripted).await
}

#[tokio::test]
async fn test_sibling_cannot_change_funded_ledger_on_dispatch_service() -> TestResult {
    check(Backend::Service).await
}
