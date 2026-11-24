//! Allowance proposals, replies and local views (Paykit spec 8.3).

use super::{
    method::Account,
    refusal::categorized,
    signer,
    wallet::{enrolled_payer, enrolled_wallet, run},
};
use crate::{
    AllowanceRole, AllowanceStatus, AllowanceTerms, AllowanceView, Amount, LimitPeriod,
    MessageDelivery, NativeWallet, PerPaymentRange, PeriodLimit, RuntimeTimer, WalletError,
    WalletScopes,
};
use paykit_lib::{allowances::AllowanceTerms as SignedTerms, PaymentReference};
use paykit_sdk::{self as sdk, AppScopes, Paykit, RequestDestination, RequestDraft, UserConfirmed};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_core::json::check_uuid_v4;
use pubky_switch_harness::{parties::Asking, Backend, World};
use std::{error::Error, path::Path, sync::Arc};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn amount(value: &str) -> Amount {
    Amount {
        asset: "btc".into(),
        value: value.into(),
    }
}

fn terms() -> AllowanceTerms {
    AllowanceTerms {
        asset: "btc".into(),
        per_payment_amount: Some(PerPaymentRange {
            minimum: amount("0.50"),
            maximum: amount("2.0"),
        }),
        period_limits: vec![
            PeriodLimit {
                amount_limit: Some(amount("4.00")),
                payment_count_limit: None,
                period: LimitPeriod::Anchored {
                    every: 1,
                    unit: "month".into(),
                    anchor: "2020-01-31T12:00:00.000000Z".into(),
                },
            },
            PeriodLimit {
                amount_limit: None,
                payment_count_limit: Some(5),
                period: LimitPeriod::Rolling {
                    every: 2,
                    unit: "week".into(),
                },
            },
        ],
        lifetime_amount_limit: Some(amount("8.50")),
        active_from: Some("2020-01-01T00:00:00Z".into()),
        expires_at: Some("2099-01-01T00:00:00.000Z".into()),
        allowed_payment_endpoint_identifiers: Some(vec!["btc-lightning-bolt11".into()]),
    }
}

fn refused<T>(result: Result<T, WalletError>, case: &str, code: &str) -> TestResult {
    let (actual, failure) = result
        .err()
        .and_then(categorized)
        .ok_or("expected a categorized Allowance refusal")?;
    assert_eq!((actual, failure.code().as_str()), (case, code));
    Ok(())
}

fn invalid<T>(result: Result<T, WalletError>, expected: &str) {
    assert!(matches!(result, Err(WalletError::InvalidInput { field }) if field == expected));
}

async fn receive(wallet: &NativeWallet) -> TestResult {
    for _ in 0..8 {
        let report = wallet.receive().await?;
        if !report.more && report.records_unlisted.is_none() {
            return Ok(());
        }
    }
    Err("wallet did not finish its record listing".into())
}

async fn receive_shop(shop: &mut Paykit) -> TestResult {
    for _ in 0..8 {
        let summary = Box::pin(shop.receive(None)).await?;
        if !summary.report.more && summary.records_unlisted.is_none() {
            return Ok(());
        }
    }
    Err("shop did not finish its record listing".into())
}

async fn parties(world: &World, root: &Path) -> TestResult<(Arc<NativeWallet>, Paykit)> {
    let account = Arc::new(Account::open(root, world.clock().clone()));
    let scopes = WalletScopes {
        pay: true,
        allowance: true,
        allowance_execute: true,
        manage_contacts: true,
        ..Default::default()
    };
    let (_, wallet, _, _) = Box::pin(enrolled_wallet(world, root, account, scopes, None)).await?;
    Box::pin(receive(&wallet)).await?;

    let owner = world.identity("shop")?;
    let buyer = world.identity("buyer")?.public();
    let mut issuer = Box::pin(signer::serving(world, "shop")).await?;
    let scopes = AppScopes::new()
        .request()
        .allowance_execute()
        .manage_contacts()
        .build()?;
    let names = scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let asking = Asking::for_scopes(world.app_keys(&owner, "wallet")?, &names)?;
    let review = issuer.review(asking.request.signed(), &asking.auth)?;
    let approval = Box::pin(issuer.approve(review)).await?;
    let mut shop = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        asking.join_request(owner.public(), &approval)?,
        wallet.rail(),
        Arc::new(RuntimeTimer),
    ))
    .await?;
    issuer.claim_delivered(&approval.request).await?;
    Box::pin(receive_shop(&mut shop)).await?;
    shop.app_mut().allow_connect(&buyer).await?;

    // The shop redeems the wallet's invitation with its first request (Switch spec 7).
    let expiry = world
        .now_ms()
        .checked_add(3_600_000)
        .ok_or("invite expiry overflow")?;
    let invite = wallet
        .lend(async |app| app.app_mut().register_invite_code(expiry, 1).await)
        .await??;
    let request = RequestDraft::new(
        amount("1").try_into()?,
        PaymentReference::new("admission")?,
        RequestDestination::Any,
    )
    .with_invite_code(invite.code);
    let sent = Box::pin(shop.send_request(buyer, request)).await?;
    assert!(matches!(
        sent.delivery,
        sdk::MessageDelivery::Accepted { .. }
    ));
    Box::pin(receive(&wallet)).await?;
    Ok((wallet, shop))
}

fn check_terms(view: &AllowanceView) -> TestResult {
    let actual = SignedTerms::try_from(view.terms.clone().ok_or("missing proposal terms")?)?;
    assert_eq!(
        actual.to_json()?,
        SignedTerms::try_from(terms())?.to_json()?
    );
    Ok(())
}

async fn proposals(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let (wallet, mut shop) = Box::pin(parties(world, root.path())).await?;
    let peer = shop.identity();
    for role in [AllowanceRole::Allowee, AllowanceRole::Allower] {
        let sent = match role {
            AllowanceRole::Allowee => wallet.propose_allowance(peer.into(), role, terms()).await?,
            AllowanceRole::Allower => {
                wallet
                    .propose_executable_allowance(peer.into(), terms(), true)
                    .await?
            }
            AllowanceRole::Other => return Err("unexpected proposal role".into()),
        };
        check_uuid_v4(&sent.allowance_id)?;
        check_uuid_v4(&sent.msg_id)?;
        assert_eq!(sent.record.len(), 32);
        assert!(matches!(sent.delivery, MessageDelivery::Accepted { .. }));
        assert_eq!(
            wallet.message_delivery(sent.msg_id).await?,
            Some(sent.delivery)
        );
        let view = wallet.allowance(sent.record.clone()).await?;
        assert_eq!(view.record, sent.record);
        assert_eq!(view.counterparty, peer);
        assert_eq!(view.allowance_id, sent.allowance_id);
        assert_eq!(view.role, Some(role));
        assert!(view.proposed_here);
        assert_eq!(view.status, AllowanceStatus::Proposed);
        assert_eq!(view.hold.record(), sent.record);
        assert_eq!(view.blocked, Some(false));
        assert!(view.executor_app_id.is_none() && !view.consented && !view.confirmation_needed);
        check_terms(&view)?;
        let listed = wallet.allowances().await?;
        let [only] = listed.as_slice() else {
            return Err("expected one pending proposal".into());
        };
        assert_eq!(only.record, sent.record);
        check_terms(only)?;
        Box::pin(receive_shop(&mut shop)).await?;
        let other = shop
            .allowances()
            .await?
            .into_iter()
            .find(|v| v.allowance_id.as_str() == sent.allowance_id)
            .ok_or("shop did not receive proposal")?;
        assert!(!other.proposed_here);
        assert_eq!(other.counterparty, world.identity("buyer")?.public());
        assert_eq!(
            other.role.map(AllowanceRole::from),
            Some(if role == AllowanceRole::Allower {
                AllowanceRole::Allowee
            } else {
                AllowanceRole::Allower
            })
        );
        check_uuid_v4(&wallet.end_allowance(sent.record.clone()).await?)?;
        refused(
            wallet.allowance(sent.record).await,
            "permanent",
            "not_found",
        )?;
        Box::pin(receive_shop(&mut shop)).await?;
    }
    assert!(wallet.allowances().await?.is_empty());
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_allowance_proposals_preserve_terms_roles_and_withdrawals() {
    run(Backend::Scripted, proposals);
    run(Backend::Service, proposals);
}

async fn incoming(
    wallet: &NativeWallet,
    shop: &mut Paykit,
    role: AllowanceRole,
) -> TestResult<AllowanceView> {
    let buyer = crate::fixed(wallet.identity().await?, "identity")?;
    let sent = match role {
        AllowanceRole::Allower => {
            Box::pin(shop.propose_allowance(
                buyer,
                sdk::AllowanceRole::Allowee,
                terms().try_into()?,
            ))
            .await?
        }
        AllowanceRole::Allowee => {
            Box::pin(shop.propose_executable_allowance(
                buyer,
                terms().try_into()?,
                UserConfirmed::new(),
            ))
            .await?
        }
        AllowanceRole::Other => return Err("unexpected recipient role".into()),
    };
    Box::pin(receive(wallet)).await?;
    let view = wallet
        .allowances()
        .await?
        .into_iter()
        .find(|answer| answer.allowance_id == sent.allowance_id.as_str())
        .ok_or("missing received proposal")?;
    assert_eq!(view.role, Some(role));
    assert!(!view.proposed_here);
    assert_eq!(view.status, AllowanceStatus::Proposed);
    check_terms(&view)?;
    Ok(view)
}

async fn accept_and_end(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let (wallet, mut shop) = Box::pin(parties(world, root.path())).await?;
    for role in [AllowanceRole::Allowee, AllowanceRole::Allower] {
        let view = Box::pin(incoming(&wallet, &mut shop, role)).await?;
        let record = view.record;
        let msg_id = match role {
            AllowanceRole::Allower => {
                invalid(
                    wallet
                        .accept_executable_allowance(record.clone(), false)
                        .await,
                    "user_confirmed",
                );
                refused(
                    wallet.accept_allowance(record.clone()).await,
                    "not_sent",
                    "usage",
                )?;
                wallet
                    .accept_executable_allowance(record.clone(), true)
                    .await?
            }
            AllowanceRole::Allowee => {
                refused(
                    wallet
                        .accept_executable_allowance(record.clone(), true)
                        .await,
                    "not_sent",
                    "usage",
                )?;
                wallet.accept_allowance(record.clone()).await?
            }
            AllowanceRole::Other => return Err("unexpected recipient role".into()),
        };
        check_uuid_v4(&msg_id)?;
        assert_eq!(
            wallet.allowance(record.clone()).await?.status,
            AllowanceStatus::Accepted
        );
        Box::pin(receive_shop(&mut shop)).await?;
        let peer = shop
            .allowances()
            .await?
            .into_iter()
            .find(|v| v.allowance_id.as_str() == view.allowance_id)
            .ok_or("missing shop Allowance")?;
        assert_eq!(peer.status, sdk::AllowanceStatus::Accepted);
        let end = wallet.end_allowance(record.clone()).await?;
        check_uuid_v4(&end)?;
        assert_ne!(end, msg_id);
        assert_eq!(
            wallet.allowance(record).await?.status,
            AllowanceStatus::Ended
        );
        Box::pin(receive_shop(&mut shop)).await?;
        assert_eq!(
            shop.allowance(&peer.record).await?.status,
            sdk::AllowanceStatus::Ended
        );
    }
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_allowance_acceptance_and_end_reach_both_parties() {
    run(Backend::Scripted, accept_and_end);
    run(Backend::Service, accept_and_end);
}

async fn reject_and_discard(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let (wallet, mut shop) = Box::pin(parties(world, root.path())).await?;
    let proposal = Box::pin(incoming(&wallet, &mut shop, AllowanceRole::Allower)).await?;
    let record = proposal.record;
    check_uuid_v4(&wallet.reject_allowance(record.clone()).await?)?;
    refused(
        wallet.reject_allowance(record).await,
        "permanent",
        "not_found",
    )?;
    Box::pin(receive_shop(&mut shop)).await?;
    assert!(shop.allowances().await?.is_empty());

    let proposal = Box::pin(incoming(&wallet, &mut shop, AllowanceRole::Allower)).await?;
    let record = proposal.record;
    refused(
        wallet.discard_pending_allowance(record.clone()).await,
        "not_sent",
        "usage",
    )?;
    let peer = shop.identity();
    wallet
        .lend(async |app| Box::pin(app.block(&peer)).await)
        .await??;
    assert_eq!(wallet.allowance(record.clone()).await?.blocked, Some(true));
    wallet.discard_pending_allowance(record.clone()).await?;
    refused(wallet.allowance(record).await, "permanent", "not_found")?;
    Box::pin(receive_shop(&mut shop)).await?;
    let offers = shop.allowances().await?;
    let [pending] = offers.as_slice() else {
        return Err("expected one unanswered proposal".into());
    };
    assert_eq!(pending.allowance_id.as_str(), proposal.allowance_id);
    assert_eq!(pending.status, sdk::AllowanceStatus::Proposed);
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_allowance_rejection_replies_and_blocked_discard_is_local() {
    run(Backend::Scripted, reject_and_discard);
    run(Backend::Service, reject_and_discard);
}

async fn refusals(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, wallet, _, _) = Box::pin(enrolled_payer(world, root.path(), account)).await?;
    refused(wallet.allowances().await, "needs_user", "not_now")?;
    Box::pin(receive(&wallet)).await?;
    let to = world.identity("shop")?.public().to_vec();
    let record = vec![5; 32];
    refused(
        wallet
            .propose_allowance(to.clone(), AllowanceRole::Allowee, terms())
            .await,
        "needs_user",
        "scope_missing",
    )?;
    invalid(
        wallet
            .propose_executable_allowance(to.clone(), terms(), false)
            .await,
        "user_confirmed",
    );
    refused(
        wallet.propose_executable_allowance(to, terms(), true).await,
        "needs_user",
        "scope_missing",
    )?;
    for result in [
        wallet.accept_allowance(record.clone()).await,
        wallet
            .accept_executable_allowance(record.clone(), true)
            .await,
        wallet.reject_allowance(record.clone()).await,
        wallet.end_allowance(record.clone()).await,
    ] {
        refused(result, "needs_user", "scope_missing")?;
    }
    refused(
        wallet.discard_pending_allowance(record.clone()).await,
        "needs_user",
        "scope_missing",
    )?;
    refused(wallet.allowance(record).await, "permanent", "not_found")?;
    invalid(wallet.allowance(vec![0; 31]).await, "record");
    invalid(wallet.end_allowance(vec![0; 33]).await, "record");
    invalid(
        wallet
            .propose_allowance(vec![], AllowanceRole::Allowee, terms())
            .await,
        "to",
    );
    assert!(wallet.allowances().await?.is_empty());
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_allowance_calls_distinguish_scope_missing_unlisted_and_absent() {
    run(Backend::Scripted, refusals);
    run(Backend::Service, refusals);
}

#[test]
fn test_allowance_terms_validate_fields_and_preserve_wire_spellings() -> TestResult {
    let mut distinct = terms();
    let mut anchored = distinct
        .period_limits
        .first()
        .ok_or("missing limit")?
        .clone();
    if let LimitPeriod::Anchored { anchor, .. } = &mut anchored.period {
        *anchor = "2020-01-31T12:00:00Z".into();
    }
    distinct.period_limits.push(anchored.clone());
    let wire = SignedTerms::try_from(distinct.clone())?.to_json()?;
    assert_eq!(
        wire,
        serde_json::json!({
            "asset": "btc",
            "per_payment_amount": {"minimum": "0.50", "maximum": "2.0"},
            "period_limits": [
                {"amount_limit": "4.00", "payment_count_limit": null,
                 "period": {"kind": "anchored", "every": 1, "unit": "month", "anchor": "2020-01-31T12:00:00.000000Z"}},
                {"amount_limit": null, "payment_count_limit": 5,
                 "period": {"kind": "rolling", "every": 2, "unit": "week"}},
                {"amount_limit": "4.00", "payment_count_limit": null,
                 "period": {"kind": "anchored", "every": 1, "unit": "month", "anchor": "2020-01-31T12:00:00Z"}},
            ],
            "lifetime_amount_limit": "8.50",
            "active_from": "2020-01-01T00:00:00Z",
            "expires_at": "2099-01-01T00:00:00.000Z",
            "allowed_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
        })
    );
    let displayed = AllowanceTerms::from(SignedTerms::from_json(&wire)?);
    assert_eq!(SignedTerms::try_from(displayed)?.to_json()?, wire);
    // The terms reader refuses a repeated limit; the binding names the terms (Paykit wire 4.15).
    distinct.period_limits.push(anchored);
    invalid(SignedTerms::try_from(distinct), "terms");

    for (period, field) in [
        (
            LimitPeriod::Rolling {
                every: 0,
                unit: "day".into(),
            },
            "terms",
        ),
        (
            LimitPeriod::Rolling {
                every: 1,
                unit: "month".into(),
            },
            "terms",
        ),
        (
            LimitPeriod::Rolling {
                every: 1,
                unit: "fortnight".into(),
            },
            "period_limits.period.unit",
        ),
        (
            LimitPeriod::Anchored {
                every: 1,
                unit: "day".into(),
                anchor: "invalid".into(),
            },
            "period_limits.period.anchor",
        ),
    ] {
        let mut bad = terms();
        bad.period_limits = vec![PeriodLimit {
            amount_limit: None,
            payment_count_limit: Some(1),
            period,
        }];
        invalid(SignedTerms::try_from(bad), field);
    }

    let mut wrong_asset = amount("1");
    wrong_asset.asset = "BTC".into();
    for (bad, field) in [
        (
            AllowanceTerms {
                asset: String::new(),
                ..terms()
            },
            "asset",
        ),
        (
            AllowanceTerms {
                per_payment_amount: Some(PerPaymentRange {
                    minimum: amount("3"),
                    maximum: amount("2"),
                }),
                ..terms()
            },
            "terms",
        ),
        (
            AllowanceTerms {
                lifetime_amount_limit: Some(wrong_asset),
                ..terms()
            },
            "lifetime_amount_limit",
        ),
        (
            AllowanceTerms {
                lifetime_amount_limit: Some(amount("private input")),
                ..terms()
            },
            "lifetime_amount_limit",
        ),
        (
            AllowanceTerms {
                active_from: Some("private input".into()),
                ..terms()
            },
            "active_from",
        ),
        (
            AllowanceTerms {
                expires_at: terms().active_from,
                ..terms()
            },
            "terms",
        ),
        (
            AllowanceTerms {
                expires_at: Some("private input".into()),
                ..terms()
            },
            "expires_at",
        ),
        (
            AllowanceTerms {
                allowed_payment_endpoint_identifiers: Some(vec!["private".into()]),
                ..terms()
            },
            "allowed_payment_endpoint_identifiers",
        ),
    ] {
        invalid(SignedTerms::try_from(bad), field);
    }
    Ok(())
}
