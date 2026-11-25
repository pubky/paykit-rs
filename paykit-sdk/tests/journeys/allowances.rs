//! Allowances (Paykit spec 8.3): proposed by either side and accepted by the other, Billing
//! Periods paid under one by the payer's executing app, what its limits hold, its end, and
//! a restore in the middle.

use std::sync::Arc;

use paykit_lib::allowances::AllowanceTerms;
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AllowanceRole, AllowanceStatus, AppScopes, PayOptions, PayOutcome, Paykit, PaykitConfig,
    ReceiptsOf, Refusal, RequestDraft, RequestRef, RequestStatus,
};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::store::MemoryStore;
use pubky_switch_harness::World;
use serde_json::json;

use super::periods::{at_start, hourly, hourly_periods, period_paid_and_receipted, Due, How};
use super::{connect, finish_listing, on_both_backends, Owner, TestResult};

const DAY_MS: u64 = 86_400_000;

#[tokio::test]
async fn test_allowance_proposed_by_the_payer_pays_periods_until_it_ends() -> TestResult {
    on_both_backends(async |world: &World| lifecycle(world, AllowanceRole::Allower).await).await
}

#[tokio::test]
async fn test_allowance_proposed_by_the_seller_pays_periods_until_it_ends() -> TestResult {
    on_both_backends(async |world: &World| lifecycle(world, AllowanceRole::Allowee).await).await
}

#[tokio::test]
async fn test_allowance_limits_hold_what_doesnt_fit() -> TestResult {
    on_both_backends(limits).await
}

#[tokio::test]
async fn test_allowance_restored_behind_its_last_payment_holds_execution_and_leaves_ways_forward(
) -> TestResult {
    on_both_backends(restored_behind).await
}

/// A payer whose wallet executes Allowances and a seller whose shop takes them, each of an
/// identity of its own, admitting each other, on one rail.
struct Parties {
    wallet: Paykit,
    shop: Paykit,
    rail: Arc<FakeRail>,
}

async fn parties(world: &World) -> TestResult<Parties> {
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut payer = Box::pin(Owner::enroll(world, "payer")).await?;
    let mut seller = Box::pin(Owner::enroll(world, "seller")).await?;
    let mut wallet = Box::pin(
        payer.join(
            world,
            "wallet",
            AppScopes::new()
                .pay()
                .allowance_execute()
                .manage_contacts()
                .build()?,
            PaykitConfig::default(),
            Arc::new(MemoryStore::new()),
            &rail,
        ),
    )
    .await?;
    let mut shop = Box::pin(
        seller.join(
            world,
            "shop",
            AppScopes::new()
                .request()
                .allowance()
                .manage_contacts()
                .build()?,
            PaykitConfig::default(),
            Arc::new(MemoryStore::new()),
            &rail,
        ),
    )
    .await?;
    Box::pin(connect(&mut wallet, &mut shop)).await?;
    // The payment method's history answers for the payer's account, which an executor
    // reads before it pays under an Allowance (Paykit spec 8.3).
    rail.set_history_payer(wallet.identity());
    Ok(Parties { wallet, shop, rail })
}

/// Every call the payment method took, repeats included.
fn method_calls(rail: &FakeRail) -> u64 {
    rail.all_pay_calls().values().sum()
}

/// Allowance terms in btc: the lifetime limit `lifetime`, at most `per_payment` a payment
/// and at most `per_day` payments a day.
fn terms(lifetime: &str, per_payment: &str, per_day: Option<u64>) -> TestResult<AllowanceTerms> {
    let period_limits = match per_day {
        Some(count) => json!([{
            "amount_limit": lifetime,
            "payment_count_limit": count,
            "period": {
                "anchor": "2026-01-01T00:00:00.000Z",
                "every": 1,
                "kind": "anchored",
                "unit": "day"
            }
        }]),
        None => json!([]),
    };
    Ok(AllowanceTerms::from_json(&json!({
        "asset": "btc",
        "lifetime_amount_limit": lifetime,
        "per_payment_amount": { "minimum": "0.0001", "maximum": per_payment },
        "period_limits": period_limits,
        "active_from": null,
        "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    }))?)
}

/// `proposer` proposes an Allowance of `terms`, the other side accepts it, and the payer's
/// wallet takes the grant to execute it. Returns the payer's and the seller's record keys.
async fn accepted(
    parties: &mut Parties,
    proposer: AllowanceRole,
    terms: AllowanceTerms,
) -> TestResult<([u8; 32], [u8; 32])> {
    let Parties { wallet, shop, .. } = parties;
    Box::pin(accepted_between(wallet, shop, proposer, terms)).await
}

/// [`accepted`], between the payer's `wallet` and the seller's `shop`.
async fn accepted_between(
    wallet: &mut Paykit,
    shop: &mut Paykit,
    proposer: AllowanceRole,
    terms: AllowanceTerms,
) -> TestResult<([u8; 32], [u8; 32])> {
    let sent = match proposer {
        AllowanceRole::Allower => {
            Box::pin(wallet.propose_executable_allowance(
                shop.identity(),
                terms,
                UserConfirmed::new(),
            ))
            .await?
        }
        _ => {
            Box::pin(shop.propose_allowance(wallet.identity(), AllowanceRole::Allowee, terms))
                .await?
        }
    };
    let (receiver, proposed_by) = match proposer {
        AllowanceRole::Allower => (&mut *shop, wallet.identity()),
        _ => (&mut *wallet, shop.identity()),
    };
    Box::pin(finish_listing(receiver)).await?;
    let offer = Box::pin(receiver.allowances())
        .await?
        .into_iter()
        .find(|view| view.allowance_id == sent.allowance_id && view.counterparty == proposed_by)
        .ok_or("the proposal never reached the other side")?;
    assert_eq!(offer.status, AllowanceStatus::Proposed, "{offer:?}");
    assert!(!offer.proposed_here);
    let (key, seller_key) = match proposer {
        AllowanceRole::Allower => {
            Box::pin(shop.accept_allowance(&offer.record)).await?;
            Box::pin(finish_listing(wallet)).await?;
            (sent.record, offer.record)
        }
        _ => {
            Box::pin(wallet.accept_executable_allowance(&offer.record, UserConfirmed::new()))
                .await?;
            Box::pin(finish_listing(shop)).await?;
            (offer.record, sent.record)
        }
    };
    // The executing wallet takes the grant; neither side has paid anything yet.
    Box::pin(wallet.resume_allowance_executor(&key)).await?;
    let view = Box::pin(wallet.allowance(&key)).await?;
    assert_eq!(view.status, AllowanceStatus::Accepted, "{view:?}");
    assert!(view.consented && !view.confirmation_needed, "{view:?}");
    assert_eq!(view.executor_app_id, Some(wallet.app_id()));
    let seller = Box::pin(shop.allowance(&seller_key)).await?;
    assert_eq!(seller.status, AllowanceStatus::Accepted, "{seller:?}");
    Ok((key, seller_key))
}

/// The seller sends `draft`; the payer accepts the subscription under the Allowance `key`
/// and the seller reads the acceptance. Returns the request and the seller's record of it.
async fn subscribed_under(
    wallet: &mut Paykit,
    shop: &mut Paykit,
    draft: RequestDraft,
    key: &[u8; 32],
) -> TestResult<(RequestRef, [u8; 32])> {
    let sent = Box::pin(shop.send_request(wallet.identity(), draft)).await?;
    let request = sent.request.ok_or("the request was not sent")?;
    Box::pin(wallet.receive(None)).await?;
    Box::pin(wallet.accept_subscription_under_allowance(&request, key)).await?;
    assert_eq!(
        Box::pin(wallet.request(&request)).await?.status,
        RequestStatus::Subscribed
    );
    Box::pin(shop.receive(None)).await?;
    let sale = Box::pin(shop.payee_activity())
        .await?
        .into_iter()
        .find(|activity| activity.request_hash == Some(request.request_hash))
        .ok_or("the seller has no record of the subscription")?;
    Ok((request, sale.record))
}

/// Proposed by `proposer` and accepted by the other side, an Allowance pays a Billing Period
/// of a subscription accepted under it, proven, credited and receipted, and counts it in its
/// usage. Once the payer ends it, the next period isn't paid under it.
async fn lifecycle(world: &World, proposer: AllowanceRole) -> TestResult {
    let mut parties = Box::pin(parties(world)).await?;
    let (key, seller_key) = Box::pin(accepted(
        &mut parties,
        proposer,
        terms("0.001", "0.0005", None)?,
    ))
    .await?;
    let Parties {
        wallet, shop, rail, ..
    } = &mut parties;
    let (draft, periods) = hourly(world, "lnbc-allowance", false)?;
    let (request, sale) = Box::pin(subscribed_under(wallet, shop, draft, &key)).await?;

    let (first, next) = (periods[0], periods[1]);
    {
        at_start(world, first);
        let due = Due {
            request,
            sale,
            period: first,
            published: "lnbc-allowance",
        };
        Box::pin(period_paid_and_receipted(
            wallet,
            shop,
            rail,
            due,
            How::UnderAllowance,
        ))
        .await?;
        let usage = Box::pin(wallet.reconcile_allowance(&key))
            .await?
            .ok_or("the Allowance shows no usage")?;
        assert_eq!(usage.spent(), 1, "{usage:?}");
        assert_eq!(usage.reserved(), 0, "{usage:?}");
    }
    let spent = method_calls(rail);

    // The payer ends the Allowance; the seller sees it ended, and nothing more is paid.
    Box::pin(wallet.end_allowance(&key)).await?;
    assert_eq!(
        Box::pin(wallet.allowance(&key)).await?.status,
        AllowanceStatus::Ended
    );
    Box::pin(shop.receive(None)).await?;
    assert_eq!(
        Box::pin(shop.allowance(&seller_key)).await?.status,
        AllowanceStatus::Ended
    );
    at_start(world, next);
    let refused = Box::pin(wallet.pay_period_under_allowance(&request, &next))
        .await
        .outcome;
    assert!(
        matches!(
            refused.as_ref().err().and_then(|e| e.refusal()),
            Some(Refusal::AllowanceEnded)
        ),
        "{refused:?}"
    );
    assert_eq!(method_calls(rail), spent);
    Ok(())
}

/// The restored app reconciles, its listing done over as many calls as it takes, and ends
/// reconciling.
async fn reconciled(app: &mut Paykit) -> TestResult {
    for _ in 0..8 {
        match Box::pin(app.reconcile()).await {
            Ok(report) => {
                assert!(report.doubtful.is_empty(), "{report:?}");
                Box::pin(app.end_reconcile(UserConfirmed::new())).await?;
                return Ok(());
            }
            Err(error) if matches!(error.refusal(), Some(Refusal::NotNow(_))) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("the restored app's listing never finished".into())
}

/// The seller asks `wallet` for `amount` btc once, with a week to pay; the wallet reads it.
async fn one_time(
    world: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    amount: &str,
    reference: &str,
) -> TestResult<RequestRef> {
    let draft = super::invoice(amount, reference, &format!("lnbc-{reference}"))?
        .with_proposal_expires_at_ms(world.now_ms() + 7 * DAY_MS);
    let sent = Box::pin(shop.send_request(wallet.identity(), draft)).await?;
    let request = sent.request.ok_or("the request was not sent")?;
    Box::pin(wallet.receive(None)).await?;
    Ok(request)
}

/// The seller verifies and credits the payment of `request`, and the payer then holds its
/// receipt: exactly one, on both sides.
async fn credited_once(wallet: &mut Paykit, shop: &mut Paykit, request: &RequestRef) -> TestResult {
    Box::pin(shop.receive(None)).await?;
    let sale = Box::pin(shop.payee_activity())
        .await?
        .into_iter()
        .find(|activity| activity.request_hash == Some(request.request_hash))
        .ok_or("the seller has no record of the payment")?;
    assert!(Box::pin(shop.verify_payment(&sale.record)).await?.is_some());
    let receipt = Box::pin(shop.confirm_payment(&sale.record))
        .await?
        .ok_or("the seller could not confirm the payment")?;
    Box::pin(wallet.receive(None)).await?;
    let receipts = Box::pin(wallet.receipts(ReceiptsOf::Request(*request))).await?;
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(receipts[0].msg_id, receipt);
    Ok(())
}

/// Whether `outcome` is refused as never fitting the Allowance, for the limit `limit` names.
fn never_fits(outcome: &paykit_sdk::Result<PayOutcome>, limit: &str) -> bool {
    matches!(
        outcome.as_ref().err().and_then(|e| e.refusal()),
        Some(Refusal::NotAutomatic(reason)) if reason.contains(limit)
    )
}

/// What an Allowance's limits hold (Paykit spec 8.3), each alone: a payment over the
/// per-payment maximum never fits; a payment over the day's count waits for the next day and
/// is then paid; a payment the lifetime limit has no room left for never fits. Each payment
/// is made once, credited and receipted, and nothing that doesn't fit is paid.
async fn limits(world: &World) -> TestResult {
    let mut parties = Box::pin(parties(world)).await?;
    let (key, _) = Box::pin(accepted(
        &mut parties,
        AllowanceRole::Allower,
        terms("0.0006", "0.0004", Some(1))?,
    ))
    .await?;
    let Parties {
        wallet, shop, rail, ..
    } = &mut parties;
    let paid =
        |outcome: &paykit_sdk::Result<PayOutcome>| matches!(outcome, Ok(PayOutcome::Paid { .. }));

    // Never fits: over the per-payment maximum, with the lifetime and the day untouched.
    let over = Box::pin(one_time(world, wallet, shop, "0.0005", "over")).await?;
    let outcome = Box::pin(wallet.pay_under_allowance(&over, &key))
        .await
        .outcome;
    assert!(never_fits(&outcome, "per_payment"), "{outcome:?}");
    assert_eq!(method_calls(rail), 0);

    let first = Box::pin(one_time(world, wallet, shop, "0.00025", "first")).await?;
    let outcome = Box::pin(wallet.pay_under_allowance(&first, &key))
        .await
        .outcome;
    assert!(paid(&outcome), "{outcome:?}");
    assert_eq!(method_calls(rail), 1);
    Box::pin(credited_once(wallet, shop, &first)).await?;

    // Fits later: the day's one payment is spent. The payment waits, and goes the next day.
    let second = Box::pin(one_time(world, wallet, shop, "0.00025", "second")).await?;
    let outcome = Box::pin(wallet.pay_under_allowance(&second, &key))
        .await
        .outcome;
    assert!(
        matches!(
            outcome.as_ref().err().and_then(|e| e.refusal()),
            Some(Refusal::NotNow(_))
        ),
        "{outcome:?}"
    );
    assert_eq!(method_calls(rail), 1);
    world.advance_ms(DAY_MS);
    Box::pin(wallet.receive(None)).await?;
    let outcome = Box::pin(wallet.pay_under_allowance(&second, &key))
        .await
        .outcome;
    assert!(paid(&outcome), "{outcome:?}");
    assert_eq!(method_calls(rail), 2);
    Box::pin(credited_once(wallet, shop, &second)).await?;
    let usage = Box::pin(wallet.reconcile_allowance(&key))
        .await?
        .ok_or("the Allowance shows no usage")?;
    assert_eq!((usage.spent(), usage.reserved()), (2, 0), "{usage:?}");

    // Never fits: 0.0005 of the 0.0006 lifetime is spent and nothing is reserved to release,
    // so 0.0002 more never fits, though it is within the per-payment range and the day's count.
    world.advance_ms(DAY_MS);
    Box::pin(wallet.receive(None)).await?;
    let rest = Box::pin(one_time(world, wallet, shop, "0.0002", "rest")).await?;
    let outcome = Box::pin(wallet.pay_under_allowance(&rest, &key))
        .await
        .outcome;
    assert!(never_fits(&outcome, "lifetime"), "{outcome:?}");
    assert_eq!(method_calls(rail), 2);
    Ok(())
}

/// The payer's wallet is backed up after the Allowance is accepted and a subscription is
/// accepted under it; the first Billing Period is then paid under the Allowance. Restored
/// from that backup on a new device, the wallet is behind the Allowance's ledger: the paid
/// period is never paid again, and execution under the Allowance is held, with nothing
/// executed (Paykit spec 8.3, "Resuming the same executor"). The user's ways forward: pay
/// the period by hand, or end the Allowance and create a new one, which pays a new
/// subscription. Nothing is paid twice.
async fn restored_behind(world: &World) -> TestResult {
    let mut parties = Box::pin(parties(world)).await?;
    let (key, seller_key) = Box::pin(accepted(
        &mut parties,
        AllowanceRole::Allowee,
        terms("0.001", "0.0005", None)?,
    ))
    .await?;
    let Parties {
        wallet, shop, rail, ..
    } = &mut parties;
    let (draft, periods) = hourly_periods(world, "lnbc-allowance", false, 3)?;
    let (request, sale) = Box::pin(subscribed_under(wallet, shop, draft, &key)).await?;
    let backup = Box::pin(wallet.backup()).await?;
    at_start(world, periods[0]);
    let due = Due {
        request,
        sale,
        period: periods[0],
        published: "lnbc-allowance",
    };
    Box::pin(period_paid_and_receipted(
        wallet,
        shop,
        rail,
        due,
        How::UnderAllowance,
    ))
    .await?;

    let mut restored = Box::pin(Paykit::restore(
        super::app_env(world, Arc::new(MemoryStore::new())),
        PaykitConfig::default(),
        &backup,
        rail.clone(),
        Arc::new(super::HostTimer),
    ))
    .await?;
    Box::pin(restored.receive(None)).await?;
    Box::pin(finish_listing(&mut restored)).await?;
    reconciled(&mut restored).await?;
    let already_paid = |outcome: &paykit_sdk::Result<PayOutcome>| {
        matches!(
            outcome.as_ref().err().and_then(|error| error.refusal()),
            Some(Refusal::AlreadyPaid)
        )
    };

    // Restored behind, the wallet still finds the first period paid: the period record it
    // reads again shows it paid, and it is never paid again (Paykit spec 7.2).
    let again = Box::pin(restored.pay_period_under_allowance(&request, &periods[0]))
        .await
        .outcome;
    assert!(already_paid(&again), "{again:?}");
    assert_eq!(method_calls(rail), 1);

    // The seller answers the restored wallet's request to send again what it may have
    // missed, and the user confirms the request held since the restore (Paykit spec 7.2,
    // "What a restore loses").
    at_start(world, periods[1]);
    Box::pin(shop.receive(None)).await?;
    Box::pin(restored.receive(None)).await?;
    let held = Box::pin(restored.request(&request))
        .await?
        .held
        .ok_or("the restored subscription isn't held")?;
    Box::pin(restored.confirm_possibly_cancelled(&request, &held, UserConfirmed::new())).await?;

    // Under the Allowance, execution is refused for good and nothing is executed: the old
    // device advanced the Allowance's ledger after the backup, which this installation can't
    // establish (Paykit spec 8.3, "Resuming the same executor"). No later read brings it
    // back and no confirmation lifts it, so the user isn't asked to confirm: the user ends
    // the Allowance or pays by hand, and reconciling it says the same.
    let not_resumed =
        Refusal::HeldForUser(paykit_sdk::HeldForUserReason::AllowanceNotResumedAfterRestore);
    for attempt in ["first", "again"] {
        let refused = Box::pin(restored.pay_period_under_allowance(&request, &periods[1]))
            .await
            .outcome
            .expect_err("executed under the Allowance");
        if refused.refusal() != Some(&not_resumed) {
            return Err(format!("{attempt}: not refused as not resumed: {refused:?}").into());
        }
    }
    let reconciling = Box::pin(restored.reconcile_allowance(&key))
        .await
        .expect_err("the restored executor reconciled the Allowance");
    if reconciling.refusal() != Some(&not_resumed) {
        return Err(format!("reconciling: {reconciling:?}").into());
    }
    assert_eq!(method_calls(rail), 1);

    // The user's ways forward. By hand, the period is paid once, credited and receipted.
    let due = Due {
        request,
        sale,
        period: periods[1],
        published: "lnbc-allowance",
    };
    Box::pin(period_paid_and_receipted(
        &mut restored,
        shop,
        rail,
        due,
        How::ByHand,
    ))
    .await?;
    assert_eq!(method_calls(rail), 2);
    let again = Box::pin(restored.pay_period(&request, &periods[1], PayOptions::default()))
        .await
        .outcome;
    assert!(already_paid(&again), "{again:?}");

    // Or the user ends the Allowance, and a new one is proposed, accepted and granted.
    Box::pin(restored.end_allowance(&key)).await?;
    Box::pin(shop.receive(None)).await?;
    assert_eq!(
        Box::pin(shop.allowance(&seller_key)).await?.status,
        AllowanceStatus::Ended
    );
    let (renewed, _) = Box::pin(accepted_between(
        &mut restored,
        shop,
        AllowanceRole::Allower,
        terms("0.001", "0.0005", None)?,
    ))
    .await?;
    // The protocol lets the user move the subscription's future periods to the new Allowance
    // (Paykit spec 8.3), but this version offers no call to do it: accepting it again under
    // the new Allowance moves nothing, and its next period, under the ended Allowance, is
    // paid by hand, credited and receipted.
    let moved = Box::pin(restored.accept_subscription_under_allowance(&request, &renewed)).await;
    assert!(
        matches!(
            moved.as_ref().err().and_then(|e| e.refusal()),
            Some(Refusal::Subscribed)
        ),
        "{moved:?}"
    );
    at_start(world, periods[2]);
    Box::pin(restored.receive(None)).await?;
    let ended = Box::pin(restored.pay_period_under_allowance(&request, &periods[2]))
        .await
        .outcome;
    assert!(
        matches!(
            ended.as_ref().err().and_then(|e| e.refusal()),
            Some(Refusal::AllowanceEnded)
        ),
        "{ended:?}"
    );
    assert_eq!(method_calls(rail), 2);
    let due = Due {
        request,
        sale,
        period: periods[2],
        published: "lnbc-allowance",
    };
    Box::pin(period_paid_and_receipted(
        &mut restored,
        shop,
        rail,
        due,
        How::ByHand,
    ))
    .await?;
    assert_eq!(method_calls(rail), 3);

    // The new Allowance pays a new subscription accepted under it, once, credited and
    // receipted.
    let (draft, renewed_periods) = hourly_periods(world, "lnbc-renewed", false, 1)?;
    let sent = Box::pin(shop.send_request(restored.identity(), draft)).await?;
    let renewed_request = sent.request.ok_or("the request was not sent")?;
    Box::pin(restored.receive(None)).await?;
    // A held request is confirmed by the user, as the restore's hold asks.
    if let Some(held) = Box::pin(restored.request(&renewed_request)).await?.held {
        Box::pin(restored.confirm_possibly_cancelled(
            &renewed_request,
            &held,
            UserConfirmed::new(),
        ))
        .await?;
    }
    Box::pin(restored.accept_subscription_under_allowance(&renewed_request, &renewed)).await?;
    Box::pin(shop.receive(None)).await?;
    let renewed_sale = Box::pin(shop.payee_activity())
        .await?
        .into_iter()
        .find(|activity| activity.request_hash == Some(renewed_request.request_hash))
        .ok_or("the seller has no record of the new subscription")?
        .record;
    at_start(world, renewed_periods[0]);
    let due_renewed = Due {
        request: renewed_request,
        sale: renewed_sale,
        period: renewed_periods[0],
        published: "lnbc-renewed",
    };
    Box::pin(period_paid_and_receipted(
        &mut restored,
        shop,
        rail,
        due_renewed,
        How::UnderAllowance,
    ))
    .await?;
    assert_eq!(method_calls(rail), 4);
    let usage = Box::pin(restored.reconcile_allowance(&renewed))
        .await?
        .ok_or("the new Allowance shows no usage")?;
    assert_eq!((usage.spent(), usage.reserved()), (1, 0), "{usage:?}");

    // Nothing is paid twice: each period of the first subscription stays paid.
    for period in &periods {
        let again = Box::pin(restored.pay_period(&request, period, PayOptions::default()))
            .await
            .outcome;
        assert!(already_paid(&again), "{again:?}");
    }
    assert_eq!(method_calls(rail), 4);
    Ok(())
}
