//! Billing Period payments and receipts across independently enrolled wallets.
#![recursion_limit = "256"]

#[path = "shared_settlement/allowance_end.rs"]
mod allowance_end;
#[path = "shared_settlement/cancellation_admission.rs"]
mod cancellation_admission;
#[path = "shared_settlement/foreign_receipt.rs"]
mod foreign_receipt;
#[path = "shared_settlement/invitations.rs"]
mod invitations;
#[path = "shared_settlement/kept_confirmation.rs"]
mod kept_confirmation;
#[path = "shared_settlement/period_policy.rs"]
mod period_policy;
#[path = "shared_settlement/selling_scope.rs"]
mod selling_scope;

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use paykit_lib::bodies::RequestDestination;
use paykit_lib::derive::{
    natural_key, occurrence_tag, settlement_bucket, settlement_id, settlement_tag,
};
use paykit_lib::identifiers::{
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{
    ActivityRecord, ActivitySide, OccurrenceKind, SettlementBucket, SettlementMark,
};
use paykit_lib::{Asset, Body, Decimal, PaymentAmount};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PeriodPrice, ReceiptsOf, RequestDraft, RequestRef,
    RequestedScopes, Timer,
};
use pubky_switch_client::app::{Delivery, SharedRecord};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_harness::{parties::Asking, Backend, World};
use pubky_switch_signer::{Protocols, Signer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct TestTimer;
impl Timer for TestTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

async fn setup_identity(world: &World, name: &str, origin: &str) -> TestResult<Signer> {
    let identity = world.identity(name)?;
    world.add_switch(origin, &[&identity]).await?;
    signer_on(world, name, origin).await
}

async fn signer_on(world: &World, name: &str, origin: &str) -> TestResult<Signer> {
    let identity = world.identity(name)?;
    let mut env = world.signer_env(&identity, Arc::new(MemoryStore::new()))?;
    env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
    let mut signer = Signer::create(env, world.keyring(&identity)?, origin).await?;
    signer.resume().await?;
    Ok(signer)
}

async fn join(
    world: &World,
    signer: &mut Signer,
    identity: &str,
    name: &str,
    scopes: RequestedScopes,
    store: Arc<MemoryStore>,
    rail: &Arc<FakeRail>,
) -> TestResult<Paykit> {
    let owner = world.identity(identity)?;
    let keys = world.app_keys(&owner, name)?;
    let auth = pubky_switch_core::enc::hash(name.as_bytes());
    let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
    let request = Asking::for_scopes_with_paths(keys, &names, &[], auth)?;
    let approved = signer
        .approve(signer.review(request.request.signed(), &request.auth)?)
        .await?;
    signer.claim_delivered(&approved.request).await?;
    let join = request.join_request(owner.public(), &approved)?;
    world.advance_ms(1000);
    let mut wallet = Paykit::join(
        world.app_env(store),
        Default::default(),
        join,
        rail.clone(),
        Arc::new(TestTimer),
    )
    .await?;
    wallet.receive(None).await?;
    Ok(wallet)
}

#[tokio::test]
async fn test_two_payee_wallets_reopen_the_same_manual_period_receipt() -> TestResult {
    settle_on_both(PeriodPayment::Manual, ProofInbox::Available).await
}

#[tokio::test]
async fn test_two_wallets_settle_a_period_at_its_signed_rate() -> TestResult {
    settle_on_both(PeriodPayment::ConfirmedRate, ProofInbox::Available).await
}

#[tokio::test]
async fn test_two_wallets_settle_a_period_with_an_authorized_rate() -> TestResult {
    settle_on_both(PeriodPayment::PolicyRate, ProofInbox::Available).await?;
    settle_on_both(PeriodPayment::PolicyRequestedAmount, ProofInbox::Available).await
}

#[tokio::test]
async fn test_two_wallets_settle_a_period_after_a_queued_proof() -> TestResult {
    settle_on_both(PeriodPayment::PolicyRate, ProofInbox::UnavailableOnce).await
}

async fn settle_on_both(how: PeriodPayment, inbox: ProofInbox) -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(settle_period(&world, how, inbox)).await;
        world.close().await?;
        result.map_err(|error| format!("{backend:?} {how:?}: {error}"))?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProofInbox {
    Available,
    UnavailableOnce,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeriodPayment {
    Manual,
    ConfirmedRate,
    PolicyRate,
    PolicyRequestedAmount,
}

async fn period_wallets(
    world: &World,
    buyer_store: Arc<MemoryStore>,
    rail: &Arc<FakeRail>,
) -> TestResult<(Paykit, Paykit, Signer)> {
    let mut buyer_signer = Box::pin(setup_identity(
        world,
        "subscriber",
        "https://subscriber.payment.test",
    ))
    .await?;
    let mut shop_signer =
        Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let mut buyer = Box::pin(join(
        world,
        &mut buyer_signer,
        "subscriber",
        "subscriber-app",
        AppScopes::new().pay().manage_contacts().build()?,
        buyer_store,
        rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        world,
        &mut shop_signer,
        "shop",
        "period-wallet",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        rail,
    ))
    .await?;
    buyer.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&buyer.identity()).await?;
    Ok((buyer, shop, shop_signer))
}

async fn settle_period(world: &World, how: PeriodPayment, inbox: ProofInbox) -> TestResult {
    let priced = how != PeriodPayment::Manual;
    let mixed = matches!(
        how,
        PeriodPayment::PolicyRate | PeriodPayment::PolicyRequestedAmount
    );
    use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};

    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let buyer_store = Arc::new(MemoryStore::new());
    let (mut buyer, mut shop, mut shop_signer) =
        Box::pin(period_wallets(world, buyer_store.clone(), &rail)).await?;
    let start = world.now_ms() / 1000 * 1000;
    let recurrence = Recurrence::new(AnchoredRule::new(start, 1, Unit::Month)?, start, None)?;
    let period = PeriodRef::from(recurrence.first()?);
    if mixed {
        rail.set_endpoint_identifiers(vec![
            PaymentEndpointIdentifier::new("usd-ach-account")?,
            PaymentEndpointIdentifier::new("btc-lightning-bolt12")?,
            PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
        ]);
    }
    let request = Box::pin(subscribe(world, &mut buyer, &mut shop, recurrence, how)).await?;
    let rate_source = if priced {
        let id = Box::pin(issue_period_rate(
            &mut shop,
            &request,
            period,
            start + 600_000,
        ))
        .await?;
        buyer.receive(None).await?;
        Some((
            id,
            PaymentAmount {
                asset: Asset::new("btc")?,
                value: Decimal::new("0.00012346")?,
            },
        ))
    } else {
        None
    };
    if inbox == ProofInbox::UnavailableOnce {
        world.unreachable("POST", "/inbox");
    }
    let payer_rail = Arc::clone(&rail);
    let shop_app = shop.app_id();
    let (mut buyer, payment, rate_source) = tokio::spawn(Box::pin(async move {
        let mut options = PayOptions::default();
        if let Some((id, expected)) = &rate_source {
            let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
            if mixed {
                let plain = Box::pin(buyer.pay_period_automatically(
                    &request,
                    &period,
                    PeriodPrice::QuoteFor(
                        PaymentEndpointIdentifier::new("usd-ach-account").unwrap(),
                    ),
                ))
                .await;
                assert!(matches!(
                    plain.outcome.unwrap_err().refusal(),
                    Some(paykit_sdk::Refusal::UnsupportedEndpoint(_))
                ));
                // This other priced endpoint also works and the method ranks it first.
                let other = PaymentEndpointIdentifier::new("btc-lightning-bolt12").unwrap();
                let quote = Box::pin(buyer.pay_period_automatically(
                    &request,
                    &period,
                    PeriodPrice::QuoteFor(other.clone()),
                ))
                .await
                .outcome
                .unwrap_err();
                let Some(paykit_sdk::Refusal::PricedByRate { payment }) = quote.refusal() else {
                    panic!("expected a quote for the other endpoint: {quote:?}");
                };
                assert_eq!(payment.endpoint_id, other);
                assert_eq!(&payment.amount, expected);
            }
            let automatic = Box::pin(buyer.pay_period_automatically(
                &request,
                &period,
                PeriodPrice::QuoteFor(endpoint.clone()),
            ))
            .await;
            assert!(automatic.public_failures.is_empty());
            let automatic_refusal = automatic.outcome.unwrap_err();
            let quote = buyer
                .pay_period(
                    &request,
                    &period,
                    PayOptions::default().with_endpoint(endpoint),
                )
                .await;
            assert!(quote.public_failures.is_empty());
            let refusal = quote.outcome.unwrap_err();
            assert_eq!(automatic_refusal.refusal(), refusal.refusal());
            assert!(buyer.payments().await.unwrap().is_empty());
            let Some(paykit_sdk::Refusal::PricedByRate { payment }) = refusal.refusal() else {
                panic!("expected confirmation of the signed period price: {refusal:?}");
            };
            assert_eq!(
                payment.source,
                paykit_sdk::RateSource::PeriodDetails { msg_id: id.clone() }
            );
            assert_eq!(&payment.amount, expected);
            assert!(payer_rail.all_executions().is_empty());
            assert_eq!(payment.billing_period, Some(period));
            if mixed {
                let within_limit = |quote: &paykit_sdk::PricedPayment, ceiling: &Decimal| {
                    quote.request == request
                        && quote.billing_period == Some(period)
                        && quote.app_id == shop_app
                        && quote.endpoint_id.as_str() == "btc-lightning-bolt11"
                        && quote.requested.asset.as_str() == "usd"
                        && quote.amount.asset.as_str() == "btc"
                        && &quote.amount.value <= ceiling
                };
                // A declined quote leaves the host with no payment call to make.
                let denied = if within_limit(payment, &Decimal::new("0.0001").unwrap()) {
                    Some(
                        Box::pin(buyer.pay_period_automatically(
                            &request,
                            &period,
                            PeriodPrice::AtRate(payment),
                        ))
                        .await,
                    )
                } else {
                    None
                };
                assert!(denied.is_none());
                assert!(buyer.payments().await.unwrap().is_empty());
                assert!(payer_rail.all_executions().is_empty());
                if how == PeriodPayment::PolicyRequestedAmount {
                    // This host independently authorizes the signed request's USD amount.
                    let result = Box::pin(buyer.pay_period_automatically(
                        &request,
                        &period,
                        PeriodPrice::RequestedAmount,
                    ))
                    .await;
                    return (buyer, result, None);
                }
                assert!(within_limit(payment, &Decimal::new("0.0002").unwrap()));
                let result = Box::pin(buyer.pay_period_automatically(
                    &request,
                    &period,
                    PeriodPrice::AtRate(payment),
                ))
                .await;
                return (buyer, result, rate_source);
            }
            options = options.with_rate_authorized((**payment).clone());
        }
        let payment = buyer.pay_period(&request, &period, options).await;
        (buyer, payment, rate_source)
    }))
    .await?;
    assert!(payment.public_failures.is_empty());
    let PayOutcome::Paid {
        attempt_id,
        proof_msg_id: Some(proof_id),
        ..
    } = payment.outcome?
    else {
        return Err("expected a paid period with its proof message ID".into());
    };
    if mixed {
        let id = buyer.app_id();
        drop(buyer);
        buyer = Box::pin(Paykit::load(
            world.app_env(buyer_store),
            Default::default(),
            id,
            rail.clone(),
            Arc::new(TestTimer),
        ))
        .await?;
        assert_eq!(buyer.payments().await?.as_slice()[0].attempt_id, attempt_id);
    }
    let queued_hash = if inbox == ProofInbox::UnavailableOnce {
        assert_eq!(world.faults_left(), 0);
        assert!(matches!(
            buyer.app().delivery(&proof_id).await?,
            Some(Delivery::Queued { .. })
        ));
        let queued = buyer.app().queued_messages("paykit").await?;
        let [proof] = queued.as_slice() else {
            return Err("expected one queued period proof".into());
        };
        assert_eq!(proof.msg_id, proof_id);
        assert_eq!(proof.kind, "paykit.payment_proof");
        assert_eq!(proof.to, shop.identity());
        let read = shop.receive(None).await?;
        assert!(!read.applied.contains(&proof_id));
        let before = sale_period(&shop, &request, period).await?;
        assert!(!before.paid && !before.credited);
        Some(proof.hash)
    } else {
        None
    };
    Box::pin(receive_settlement_message(&mut buyer, &mut shop, &proof_id)).await?;
    let period_record = sale_period(&shop, &request, period).await?;
    assert!(
        period_record.paid,
        "{:?} {how:?}: proof {proof_id}",
        world.backend()
    );
    assert!(!period_record.credited);
    let receipt = Box::pin(shop.confirm_payment(&period_record.record))
        .await?
        .ok_or("missing period receipt")?;
    let issued = shop
        .app()
        .shared_record(&period_record.record)
        .await?
        .ok_or("missing shared credit")?;
    let record = ActivityRecord::read(
        issued
            .content
            .value()
            .ok_or("missing period record value")?,
        &shop.identity(),
        ActivitySide::Payee,
    )?;
    let proof = record.proof.as_ref().ok_or("missing signed proof")?;
    assert_eq!(proof.message().msg_id, proof_id);
    if let Some(hash) = queued_hash {
        assert_eq!(proof.hash(), hash);
    }
    if let Some((id, expected)) = rate_source {
        let details = record
            .period_details
            .as_ref()
            .ok_or("missing signed rate")?;
        let credit = record.receipt.as_ref().ok_or("missing signed receipt")?;
        assert_eq!(details.message().msg_id, id);
        assert_eq!(proof.message().refs, [request.request_hash, details.hash()]);
        let Body::PaymentProof(paid) = proof.body() else {
            return Err("expected the signed period payment proof".into());
        };
        assert_eq!(
            paid.payment_endpoint_identifier.as_str(),
            "btc-lightning-bolt11"
        );
        assert_eq!(credit.message().refs, [proof.hash(), request.request_hash]);
        assert_eq!(rail.paid_amount(&attempt_id), Some(expected));
    } else if how == PeriodPayment::PolicyRequestedAmount {
        assert_eq!(
            rail.paid_amount(&attempt_id),
            Some(PaymentAmount {
                asset: Asset::new("usd")?,
                value: Decimal::new("10")?
            })
        );
        assert_eq!(proof.message().refs, [request.request_hash]);
    }

    Box::pin(receive_settlement_message(&mut shop, &mut buyer, &receipt)).await?;
    let received = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].attempt_id, attempt_id);
    assert_eq!(received[0].msg_id, receipt);
    let purchase = buyer.request(&request).await?;
    let payer_period = purchase
        .periods
        .iter()
        .find(|item| item.billing_period == period)
        .ok_or("payer's settled period missing")?;
    assert!(payer_period.paid && payer_period.credited);
    let payer_record = buyer
        .app()
        .shared_record(&payer_period.record)
        .await?
        .ok_or("missing payer record")?;
    let activity = ActivityRecord::read(
        payer_record
            .content
            .value()
            .ok_or("missing payer record value")?,
        &buyer.identity(),
        ActivitySide::Payer,
    )?;
    assert_eq!(activity.manual_only, !mixed);
    assert_eq!(
        payer_period.receipt.as_ref().map(|r| &r.msg_id),
        Some(&receipt)
    );
    let payments = buyer.payments().await?;
    let [payment] = payments.as_slice() else {
        return Err("expected one payer attempt".into());
    };
    assert_eq!(payment.attempt_id, attempt_id);
    assert_eq!(payment.billing_period, Some(period));
    assert_eq!(payment.stage, paykit_sdk::Stage::Finished);
    assert_eq!(payment.receipt.as_ref().map(|r| &r.msg_id), Some(&receipt));

    let store = Arc::new(MemoryStore::new());
    let mut second = Box::pin(join(
        world,
        &mut shop_signer,
        "shop",
        "second-period-wallet",
        AppScopes::new().request().build()?,
        store.clone(),
        &rail,
    ))
    .await?;
    assert_eq!(
        Box::pin(second.confirm_payment(&period_record.record)).await?,
        Some(receipt.clone())
    );
    let app_id = second.app_id();
    drop(second);
    let mut reopened = Box::pin(Paykit::load(
        world.app_env(store),
        Default::default(),
        app_id,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    reopened.receive(None).await?;
    assert_eq!(
        Box::pin(reopened.confirm_payment(&period_record.record)).await?,
        Some(receipt.clone())
    );
    let current = reopened
        .app()
        .shared_record(&period_record.record)
        .await?
        .ok_or("reopened period credit missing")?;
    assert_eq!(current.version, issued.version);
    assert_eq!(current.writer, issued.writer);
    Box::pin(check_period_mark(
        &reopened,
        &current,
        &request.request_hash,
        period,
    ))
    .await?;
    let credited = reopened
        .payee_activity()
        .await?
        .into_iter()
        .flat_map(|activity| activity.periods)
        .filter(|item| item.credited)
        .count();
    assert_eq!(credited, 1);
    buyer.receive(None).await?;
    let received = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].msg_id, receipt);
    assert_eq!(rail.all_executions().values().copied().sum::<u64>(), 1);
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    Ok(())
}

async fn receive_settlement_message(
    sender: &mut Paykit,
    recipient: &mut Paykit,
    id: &str,
) -> TestResult {
    let mut delivery = sender.app().delivery(id).await?;
    if matches!(delivery, Some(Delivery::Queued { .. })) {
        sender.flush().await?;
        delivery = sender.app().delivery(id).await?;
    }
    if !matches!(delivery, Some(Delivery::Accepted { .. })) {
        return Err(format!("message {id} delivery: {delivery:?}").into());
    }
    let read = recipient.receive(None).await?;
    if !read.applied.iter().any(|message| message == id) {
        return Err(format!("expected accepted message {id} in {read:?}").into());
    }
    Ok(())
}

async fn sale_period(
    shop: &Paykit,
    request: &RequestRef,
    period: PeriodRef,
) -> TestResult<paykit_sdk::PeriodView> {
    shop.payee_activity()
        .await?
        .into_iter()
        .find(|activity| activity.request_hash == Some(request.request_hash))
        .and_then(|activity| {
            activity
                .periods
                .into_iter()
                .find(|item| item.billing_period == period)
        })
        .ok_or_else(|| "missing merchant period".into())
}

async fn issue_period_rate(
    shop: &mut Paykit,
    request: &RequestRef,
    period: PeriodRef,
    expires_at: u64,
) -> TestResult<String> {
    let sale = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|sale| sale.request_hash == Some(request.request_hash))
        .ok_or("missing subscription")?;
    let rate = paykit_lib::Rate::new(Asset::new("btc")?, Decimal::new("0.0000123456")?)?;
    let details = paykit_lib::bodies::PeriodDetails {
        billing_period: period,
        payment_endpoints: None,
        rates: Some(vec![rate]),
        expires_at,
    };
    Ok(Box::pin(shop.send_period_details(&sale.record, details))
        .await?
        .msg_id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuoteIssuer {
    Current,
    RemovedBeforeSelection,
    RemovedAfterConfirmation,
    RemovedBeforeResume,
}

#[tokio::test]
async fn test_period_rate_from_a_second_payee_app_requires_its_current_authority() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        for issuer in [
            QuoteIssuer::Current,
            QuoteIssuer::RemovedBeforeSelection,
            QuoteIssuer::RemovedAfterConfirmation,
            QuoteIssuer::RemovedBeforeResume,
        ] {
            let Some(world) = World::start(backend)? else {
                continue;
            };
            let result = Box::pin(second_app_rate(&world, issuer)).await;
            world.close().await?;
            result?;
        }
    }
    Ok(())
}

async fn second_app_rate(world: &World, issuer: QuoteIssuer) -> TestResult {
    use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};
    use paykit_sdk::rail::Behaviour;
    use pubky_switch_signer::UserConfirmed;

    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    rail.set_declaration(
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
    );
    let buyer_store = Arc::new(MemoryStore::new());
    let (mut buyer, mut shop, mut payee) =
        Box::pin(period_wallets(world, buyer_store.clone(), &rail)).await?;
    let mut quotes = Box::pin(join(
        world,
        &mut payee,
        "shop",
        "quotes",
        AppScopes::new().request().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    assert_ne!(shop.app_id(), quotes.app_id());
    let start = world.now_ms() / 1000 * 1000;
    let recurrence = Recurrence::new(AnchoredRule::new(start, 1, Unit::Month)?, start, None)?;
    let period = PeriodRef::from(recurrence.first()?);
    let request = Box::pin(subscribe(
        world,
        &mut buyer,
        &mut shop,
        recurrence,
        PeriodPayment::ConfirmedRate,
    ))
    .await?;
    quotes.receive(None).await?;
    let id = Box::pin(issue_period_rate(
        &mut quotes,
        &request,
        period,
        start + 3_600_000,
    ))
    .await?;
    buyer.receive(None).await?;
    let mut confirmation = None;
    let mut attempt = None;
    if issuer != QuoteIssuer::RemovedBeforeSelection {
        let error = Box::pin(buyer.pay_period(&request, &period, PayOptions::default()))
            .await
            .outcome
            .unwrap_err();
        let Some(paykit_sdk::Refusal::PricedByRate { payment }) = error.refusal() else {
            return Err(format!("{error:?}").into());
        };
        assert_eq!(
            payment.source,
            paykit_sdk::RateSource::PeriodDetails { msg_id: id }
        );
        assert_eq!(payment.app_id, shop.app_id());
        let options = PayOptions::default().with_rate_authorized((**payment).clone());
        assert!(buyer.payments().await?.is_empty());
        assert!(rail.all_executions().is_empty());
        if issuer == QuoteIssuer::RemovedBeforeResume {
            rail.next(Behaviour::Hang);
            let outcome = Box::pin(buyer.pay_period(&request, &period, options))
                .await
                .outcome?;
            let PayOutcome::Pending { attempt_id, .. } = outcome else {
                return Err(format!("{outcome:?}").into());
            };
            assert_eq!(rail.pay_calls(&attempt_id), 1);
            attempt = Some(attempt_id);
        } else {
            confirmation = Some(options);
        }
    }
    if issuer != QuoteIssuer::Current {
        let doc =
            Box::pin(payee.revoke(quotes.app_id(), None, UserConfirmed::shown_and_confirmed()))
                .await?;
        assert!(doc.lists_removed(&quotes.app_id()));
        assert!(doc.listed_cert(&shop.app_id()).is_some());
        world.advance_ms(11 * 60_000);
    }
    let buyer_id = buyer.app_id();
    if let Some(id) = attempt {
        drop(buyer);
        let mut buyer = Paykit::load(
            world.app_env(buyer_store),
            Default::default(),
            buyer_id,
            rail.clone(),
            Arc::new(TestTimer),
        )
        .await?;
        let outcomes = Box::pin(buyer.resume_payments()).await?;
        assert!(
            matches!(outcomes.as_slice(), [PayOutcome::Uncertain { attempt_id, .. }] if *attempt_id == id),
            "{outcomes:?}"
        );
        assert_eq!(rail.pay_calls(&id), 1);
        assert_eq!(rail.executions(&id), 1);
    } else {
        let outcome =
            Box::pin(buyer.pay_period(&request, &period, confirmation.unwrap_or_default()))
                .await
                .outcome;
        if issuer == QuoteIssuer::Current {
            let PayOutcome::Paid { attempt_id, .. } = outcome? else {
                return Err("expected a paid period".into());
            };
            assert_eq!(rail.pay_calls(&attempt_id), 1);
            assert_eq!(rail.executions(&attempt_id), 1);
        } else {
            let error = outcome.unwrap_err();
            assert!(
                matches!(
                    error.refusal(),
                    Some(paykit_sdk::Refusal::UnsupportedEndpoint(why))
                        if why.contains("no period details price this endpoint")
                ),
                "{issuer:?}: {error:?}"
            );
            assert!(buyer.payments().await?.is_empty());
            assert!(rail.all_executions().is_empty());
        }
    }
    Ok(())
}

async fn subscribe(
    world: &World,
    buyer: &mut Paykit,
    shop: &mut Paykit,
    recurrence: paykit_lib::periods::Recurrence,
    how: PeriodPayment,
) -> TestResult<RequestRef> {
    let priced = how != PeriodPayment::Manual;
    let mut endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
        PaymentEndpointPayload::new("ln-manual-period")?,
    )]);
    if matches!(
        how,
        PeriodPayment::PolicyRate | PeriodPayment::PolicyRequestedAmount
    ) {
        endpoints.insert(
            PaymentEndpointIdentifier::new("usd-ach-account")?,
            PaymentEndpointPayload::new("acct-1")?,
        );
        endpoints.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt12")?,
            PaymentEndpointPayload::new("lno-alternate-period")?,
        );
    }
    let requested = if priced {
        PaymentAmount {
            asset: Asset::new("usd")?,
            value: Decimal::new("10")?,
        }
    } else {
        PaymentAmount {
            asset: Asset::new("btc")?,
            value: Decimal::new("0.00025")?,
        }
    };
    let mut draft = RequestDraft::new(
        requested,
        PaymentReference::new("monthly-plan")?,
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(world.now_ms() / 1000 * 1000 + 86_400_000)
    .with_recurrence(recurrence);
    if priced {
        draft = draft.with_conversion(paykit_lib::bodies::Conversion::PerPeriod);
    }
    let request = Box::pin(shop.send_request(buyer.identity(), draft))
        .await?
        .request
        .ok_or("recurring request was not sent")?;
    buyer.receive(None).await?;
    Box::pin(buyer.accept_subscription(&request)).await?;
    shop.receive(None).await?;
    Ok(request)
}

async fn check_period_mark(
    shop: &Paykit,
    record: &SharedRecord,
    request_hash: &[u8; 32],
    period: PeriodRef,
) -> TestResult {
    let naming = shop.app().naming_key();
    let activity = ActivityRecord::bind(
        record.content.value().ok_or("missing paid period")?,
        &shop.identity(),
        ActivitySide::Payee,
        naming,
        &record.key,
        None,
    )?;
    let signed = activity.proof.as_ref().ok_or("missing period proof")?;
    let Body::PaymentProof(proof) = signed.body() else {
        return Err("expected a payment proof".into());
    };
    let id = settlement_id(&proof.proof).ok_or("missing method settlement ID")?;
    let tag = settlement_tag(naming, &id);
    let number = settlement_bucket(&tag);
    let key = SettlementBucket::record_key(number, naming)?;
    let shared = shop
        .app()
        .shared_record(&key)
        .await?
        .ok_or("missing shared settlement bucket")?;
    let bucket = SettlementBucket::bind(
        shared
            .content
            .value()
            .ok_or("unreadable settlement bucket")?,
        number,
        naming,
        &key,
    )?;
    let natural = natural_key::period_record(&shop.identity(), request_hash, period.start_secs());
    let expected = SettlementMark::new(
        &tag,
        OccurrenceKind::BillingPeriod,
        occurrence_tag(naming, &natural),
    );
    assert_eq!(bucket.marks(), &[expected]);
    Ok(())
}

#[tokio::test]
async fn test_historical_subscription_keeps_an_early_paid_period_held_after_loss() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let outcome = Box::pin(early_paid_period_after_loss(&world)).await;
        world.close().await?;
        outcome?;
    }
    Ok(())
}

async fn follow_subscriber(wallet: &mut Paykit) -> TestResult<paykit_sdk::ReconcileReport> {
    wallet.app_mut().refresh_identity().await?;
    wallet.app_mut().follow_provider().await?;
    for _ in 0..8 {
        let summary = wallet.receive(None).await?;
        if summary.report.more || summary.records_unlisted.is_some() {
            continue;
        }
        let report = wallet.reconcile().await?;
        wallet
            .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
            .await?;
        return Ok(report);
    }
    Err("subscriber did not finish reading its recovered records".into())
}

#[tokio::test]
async fn test_reconcile_finds_fractional_periods_after_restart_and_provider_loss() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        let Some(world) = World::start(backend)? else {
            continue;
        };
        let result = Box::pin(reconcile_fractional_periods(&world)).await;
        world.close().await?;
        result?;
    }
    Ok(())
}

async fn reconcile_fractional_periods(world: &World) -> TestResult {
    use paykit_lib::collections::Collection;
    use paykit_lib::derive::request_obligation;
    use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};
    use pubky_switch_client::app::{
        ClaimOp, Collection as SwitchCollection, ObligationDeadline, OpBatch, RecordTarget,
    };

    let origin = "https://subscriber.payment.test";
    let mut signer = Box::pin(setup_identity(world, "subscriber", origin)).await?;
    let mut merchant = Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let store = Arc::new(MemoryStore::new());
    let mut buyer = Box::pin(join(
        world,
        &mut signer,
        "subscriber",
        "subscriber-app",
        AppScopes::new().pay().manage_contacts().build()?,
        store.clone(),
        &rail,
    ))
    .await?;
    let mut shop = Box::pin(join(
        world,
        &mut merchant,
        "shop",
        "period-wallet",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    buyer.app_mut().invite(&shop.identity()).await?;
    shop.app_mut().invite(&buyer.identity()).await?;
    let anchor = world.now_ms() / 1000 * 1000 + 500;
    let recurrence = Recurrence::new(AnchoredRule::new(anchor, 1, Unit::Month)?, anchor, None)?;
    let first = PeriodRef::from(recurrence.first()?);
    let next = PeriodRef::from(
        recurrence
            .lookup(first.ends_at)?
            .ok_or("missing next period")?,
    );
    let mut occurrences = Vec::new();
    for (listed, recorded) in [(first, next), (next, first)] {
        let request = Box::pin(subscribe(
            world,
            &mut buyer,
            &mut shop,
            recurrence,
            PeriodPayment::Manual,
        ))
        .await?;
        let target = |natural: Vec<u8>| {
            RecordTarget::new(
                SwitchCollection::new(
                    Collection::PayerActivity.name(),
                    Collection::PayerActivity.policy(None),
                ),
                &natural,
            )
        };
        let parent = target(natural_key::request_record(
            &request.payee,
            &request.request_hash,
        ));
        let key = buyer.app().record_key(&parent)?;
        let held = buyer
            .app()
            .shared_record(&key)
            .await?
            .ok_or("missing subscription")?;
        let mut activity = ActivityRecord::bind(
            held.content.value().ok_or("unreadable subscription")?,
            &buyer.identity(),
            ActivitySide::Payer,
            buyer.app().naming_key(),
            &key,
            None,
        )?;
        // A retained request can name an attempt whose separate period record is absent.
        activity.attempted_periods.push(listed.start_secs());
        let parent_write = buyer
            .app_mut()
            .record_op(&parent, Some(&activity.to_json()?), Vec::new())
            .await?;
        let obligation = |period: PeriodRef| {
            request_obligation(
                buyer.app().naming_key(),
                &request.payee,
                &request.request_hash,
                period.start_secs(),
            )
        };
        let listed_key = obligation(listed);
        let recorded_key = obligation(recorded);
        let id = paykit_lib::AttemptId::generate()?;
        let claim = ClaimOp::attempt(listed_key, 0, id.to_bytes(), 4200, None);
        // Another period has only its current signed record, as after a finished attempt
        // has been dropped from the request's attempted_periods.
        let period_target = target(natural_key::period_record(
            &request.payee,
            &request.request_hash,
            recorded.start_secs(),
        ));
        let mut value = ActivityRecord::new(ActivitySide::Payer, request.payee);
        value.request_hash = Some(request.request_hash);
        value.billing_period = Some(recorded);
        value.manual_only = true;
        let period_write = buyer
            .app_mut()
            .record_op(&period_target, Some(&value.to_json()?), Vec::new())
            .await?;
        buyer
            .app_mut()
            .submit_batch(
                OpBatch::for_protocol("paykit")
                    .entry(parent_write)
                    .entry(period_write)
                    .op(claim.kind())
                    .obligation_deadline(listed_key, ObligationDeadline::NoDeadline),
            )
            .await?;
        occurrences.push((request, listed_key, recorded_key, id));
    }
    let app_id = buyer.app_id();
    drop(buyer);
    let mut reopened = Box::pin(Paykit::load(
        world.app_env(store),
        Default::default(),
        app_id,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    let pending = Box::pin(reopened.reconcile()).await?;
    for (request, listed_key, _, attempt) in &occurrences {
        assert!(
            pending
                .doubtful
                .iter()
                .any(|row| row.obligation == *listed_key
                    && row.request_hash == Some(request.request_hash)
                    && row.attempts == [*attempt]),
            "{pending:?}"
        );
    }
    let subscriber = world.identity("subscriber")?;
    world.lose_switch(origin);
    let recovered = "https://recovered-subscriber.payment.test";
    world.add_switch(recovered, &[&subscriber]).await?;
    Box::pin(signer.declare_provider_loss(
        recovered,
        pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await?;
    world.advance_ms(1000);
    signer.end_staged_admission().await?;
    world.advance_ms(1000);
    let report = Box::pin(follow_subscriber(&mut reopened)).await?;
    for (request, listed, recorded, _) in occurrences {
        for obligation in [listed, recorded] {
            let mut rows = report
                .doubtful
                .iter()
                .filter(|row| row.obligation == obligation);
            let row = rows
                .next()
                .ok_or_else(|| format!("missing period in {report:?}"))?;
            assert_eq!(row.payee, Some(request.payee));
            assert_eq!(row.request_hash, Some(request.request_hash));
            assert!(row.before_loss.is_some());
            assert!(rows.next().is_none());
        }
    }
    assert!(rail.all_executions().is_empty());
    Ok(())
}

async fn early_paid_period_after_loss(world: &World) -> TestResult {
    use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};
    use pubky_switch_client::app::UserConfirmed;

    let mut signer = Box::pin(setup_identity(
        world,
        "subscriber",
        "https://subscriber.payment.test",
    ))
    .await?;
    let mut merchant = Box::pin(setup_identity(world, "shop", "https://shop.payment.test")).await?;
    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let mut buyer = Box::pin(join(
        world,
        &mut signer,
        "subscriber",
        "subscriber-app",
        AppScopes::new().pay().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await
    .map_err(|e| format!("enroll payer: {e}"))?;
    let mut shop = Box::pin(join(
        world,
        &mut merchant,
        "shop",
        "period-wallet",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await
    .map_err(|e| format!("enroll payee: {e}"))?;
    let tablet_store = Arc::new(MemoryStore::new());
    let mut tablet = Box::pin(join(
        world,
        &mut signer,
        "subscriber",
        "subscriber-tablet",
        AppScopes::new().pay().manage_contacts().build()?,
        tablet_store.clone(),
        &rail,
    ))
    .await
    .map_err(|e| format!("enroll tablet: {e}"))?;
    buyer
        .app_mut()
        .invite(&shop.identity())
        .await
        .map_err(|e| format!("payer invite: {e}"))?;
    shop.app_mut()
        .invite(&buyer.identity())
        .await
        .map_err(|e| format!("merchant invite: {e}"))?;
    let start = world.now_ms() / 1000 * 1000;
    let schedule = || Recurrence::new(AnchoredRule::new(start, 1, Unit::Month)?, start, None);
    let recurrence = schedule()?;
    let next = recurrence
        .lookup(recurrence.first()?.ends_at)?
        .ok_or("second period missing")?;
    let period = PeriodRef::from(next);
    let request = Box::pin(subscribe(
        world,
        &mut buyer,
        &mut shop,
        recurrence,
        PeriodPayment::Manual,
    ))
    .await
    .map_err(|e| format!("initial subscription: {e}"))?;
    tablet
        .receive(None)
        .await
        .map_err(|e| format!("tablet learns subscription: {e}"))?;
    assert!(tablet
        .request(&request)
        .await
        .map_err(|e| format!("tablet subscription: {e}"))?
        .held
        .is_none());
    let tablet_id = tablet.app_id();
    drop(tablet);
    let early = || PayOptions::default().with_early_payment_confirmed(UserConfirmed::new());
    let (mut buyer, first) = tokio::spawn(Box::pin(async move {
        let outcome = buyer.pay_period(&request, &period, early()).await.outcome;
        (buyer, outcome)
    }))
    .await?;
    let PayOutcome::Paid { attempt_id, .. } = first.map_err(|e| format!("early payment: {e}"))?
    else {
        return Err("period did not settle early".into());
    };
    shop.receive(None).await?;
    let key = shop
        .payee_activity()
        .await?
        .into_iter()
        .filter(|a| a.request_hash == Some(request.request_hash))
        .flat_map(|a| a.periods)
        .find(|p| p.billing_period == period)
        .ok_or("payee did not receive the period proof")?
        .record;
    let receipt = Box::pin(shop.confirm_payment(&key))
        .await
        .map_err(|e| format!("original receipt: {e}"))?
        .ok_or("payee did not issue the receipt")?;
    buyer
        .receive(None)
        .await
        .map_err(|e| format!("payer receives signed receipt: {e}"))?;
    let held = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].msg_id, receipt);
    assert_eq!(held[0].attempt_id, attempt_id);
    assert_eq!(rail.pay_calls(&attempt_id), 1);

    let subscriber = world.identity("subscriber")?;
    world.lose_switch("https://subscriber.payment.test");
    let recovered_origin = "https://recovered-subscriber.payment.test";
    world.add_switch(recovered_origin, &[&subscriber]).await?;
    Box::pin(signer.declare_provider_loss(
        recovered_origin,
        pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await
    .map_err(|e| format!("declare payer provider loss: {e}"))?;
    world.advance_ms(1000);
    signer
        .end_staged_admission()
        .await
        .map_err(|e| format!("finish recovered admission: {e}"))?;
    world.advance_ms(1000);
    // The offline tablet only saw the accepted subscription before the payment.
    let mut restored = Box::pin(Paykit::load(
        world.app_env(tablet_store),
        Default::default(),
        tablet_id,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await
    .map_err(|e| format!("reopen offline tablet: {e}"))?;
    Box::pin(follow_subscriber(&mut restored))
        .await
        .map_err(|e| format!("recover offline tablet: {e}"))?;
    let loss = restored
        .app()
        .recovery_info()
        .await?
        .ok_or("subscriber did not follow loss")?
        .loss;
    assert!(loss.uncertain_until_ms < period.starts_at);
    assert!(restored
        .receipts(ReceiptsOf::Period(request, period))
        .await?
        .is_empty());
    let mut options = early();
    if let Some(hold) = restored
        .request(&request)
        .await
        .map_err(|e| format!("earlier subscription at recovered service: {e}"))?
        .held
    {
        options = options.with_possibly_cancelled_confirmed(hold);
    }
    let (restored, result) = tokio::spawn(Box::pin(async move {
        let result = restored
            .pay_period(&request, &period, options)
            .await
            .outcome;
        (restored, result)
    }))
    .await?;
    let refused = result.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(paykit_sdk::Refusal::MayBePaid {
        reason, doubt: None,
    }) if reason.contains("provider loss")),
        "{refused}"
    );
    assert_eq!(rail.all_executions().values().sum::<u64>(), 1);
    drop(restored);

    // The paying installation held the signed proof and receipt throughout the loss.
    let mut paid = buyer;
    Box::pin(follow_subscriber(&mut paid))
        .await
        .map_err(|e| format!("recover paid wallet: {e}"))?;
    let original = paid.receipts(ReceiptsOf::Period(request, period)).await?;
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].msg_id, receipt);
    let (mut paid, again) = tokio::spawn(Box::pin(async move {
        let result = paid.pay_period(&request, &period, early()).await.outcome;
        (paid, result)
    }))
    .await?;
    assert_eq!(
        again.unwrap_err().refusal(),
        Some(&paykit_sdk::Refusal::AlreadyPaid)
    );
    assert_eq!(rail.all_executions().values().sum::<u64>(), 1);

    let later = loss.uncertain_until_ms + pubky_switch_core::records::CLOCK_TOLERANCE_MS + 1;
    world.advance_ms(later.saturating_sub(world.now_ms()));
    shop.receive(None).await?;
    let fresh = Box::pin(subscribe(
        world,
        &mut paid,
        &mut shop,
        schedule()?,
        PeriodPayment::Manual,
    ))
    .await
    .map_err(|e| format!("post-window subscription: {e}"))?;
    assert_ne!(fresh, request);
    let result = tokio::spawn(Box::pin(async move {
        paid.pay_period(&fresh, &period, early()).await.outcome
    }))
    .await??;
    assert!(matches!(result, PayOutcome::Paid { .. }));
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    assert_eq!(rail.all_executions().values().sum::<u64>(), 2);
    Ok(())
}

#[tokio::test]
async fn test_later_period_uses_the_required_apps_signed_destination_and_receipt() -> TestResult {
    for backend in [Backend::Scripted, Backend::Service] {
        for priced in [false, true] {
            let Some(world) = World::start(backend)? else {
                continue;
            };
            let result = Box::pin(settle_required_period(&world, priced)).await;
            world.close().await?;
            result?;
        }
    }
    Ok(())
}

async fn settle_required_period(world: &World, priced: bool) -> TestResult {
    use paykit_lib::periods::{AnchoredRule, Recurrence, Unit};

    let rail = Arc::new(FakeRail::with_clock(world.clock().clone()));
    let store = Arc::new(MemoryStore::new());
    let (mut buyer, mut shop, mut signer) =
        Box::pin(period_wallets(world, store.clone(), &rail)).await?;
    let mut seller_wallet = Box::pin(join(
        world,
        &mut signer,
        "shop",
        "destinations",
        AppScopes::new().request().manage_contacts().build()?,
        Arc::new(MemoryStore::new()),
        &rail,
    ))
    .await?;
    seller_wallet.app_mut().invite(&buyer.identity()).await?;
    let start = world.now_ms() / 1000 * 1000;
    let recurrence = Recurrence::new(AnchoredRule::new(start, 1, Unit::Minute)?, start, None)?;
    let period = PeriodRef::from(recurrence.periods(2)?[1]);
    let mut draft = RequestDraft::new(
        PaymentAmount {
            asset: Asset::new(if priced { "usd" } else { "btc" })?,
            value: Decimal::new("1")?,
        },
        PaymentReference::new("period-destination")?,
        RequestDestination::RequiredApp(seller_wallet.app_id()),
    )
    .with_recurrence(recurrence)
    .with_per_period_endpoints()
    .with_proposal_expires_at_ms(start + 600_000);
    if priced {
        draft = draft.with_conversion(paykit_lib::bodies::Conversion::PerPeriod);
    }
    let request = Box::pin(shop.send_request(buyer.identity(), draft))
        .await?
        .request
        .ok_or("missing request")?;
    Box::pin(buyer.receive(None)).await?;
    Box::pin(buyer.accept_subscription(&request)).await?;
    Box::pin(shop.receive(None)).await?;
    Box::pin(seller_wallet.receive(None)).await?;
    world.advance_ms(period.starts_at.saturating_sub(world.now_ms()));
    let sale = seller_wallet
        .payee_activity()
        .await?
        .into_iter()
        .find(|sale| sale.request_hash == Some(request.request_hash))
        .ok_or("seller wallet did not see the subscription")?;
    let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
    let offered = paykit_lib::bodies::PeriodDetails {
        billing_period: period,
        payment_endpoints: Some(BTreeMap::from([(
            endpoint.clone(),
            PaymentEndpointPayload::new("ln-signed-period")?,
        )])),
        rates: if priced {
            Some(vec![paykit_lib::Rate::new(
                Asset::new("btc")?,
                Decimal::new("0.0001")?,
            )?])
        } else {
            None
        },
        expires_at: period.ends_at + 600_000,
    };
    Box::pin(shop.send_period_details(&sale.record, offered.clone())).await?;
    Box::pin(buyer.receive(None)).await?;
    let wrong = Box::pin(buyer.pay_period(&request, &period, PayOptions::default())).await;
    assert!(matches!(
        wrong.outcome.unwrap_err().refusal(),
        Some(paykit_sdk::Refusal::NoDestination(_))
    ));
    assert!(buyer.payments().await?.is_empty());
    assert!(rail.all_executions().is_empty());
    Box::pin(seller_wallet.receive(None)).await?;
    let details = Box::pin(seller_wallet.send_period_details(&sale.record, offered)).await?;
    Box::pin(buyer.receive(None)).await?;
    let mut options = PayOptions::default();
    if priced {
        let quote = Box::pin(buyer.pay_period(&request, &period, options.clone()))
            .await
            .outcome
            .unwrap_err();
        let Some(paykit_sdk::Refusal::PricedByRate { payment }) = quote.refusal() else {
            return Err(format!("expected a signed quote: {quote:?}").into());
        };
        assert_eq!(payment.app_id, seller_wallet.app_id());
        assert_eq!(
            payment.source,
            paykit_sdk::RateSource::PeriodDetails {
                msg_id: details.msg_id.clone()
            }
        );
        options = options.with_rate_authorized((**payment).clone());
    }
    let paid = Box::pin(buyer.pay_period(&request, &period, options))
        .await
        .outcome?;
    let PayOutcome::Paid { attempt_id, .. } = paid else {
        return Err("period payment did not settle".into());
    };
    let buyer_app = buyer.app_id();
    drop(buyer);
    let mut buyer = Box::pin(Paykit::load(
        world.app_env(store),
        Default::default(),
        buyer_app,
        rail.clone(),
        Arc::new(TestTimer),
    ))
    .await?;
    assert_eq!(buyer.payments().await?.as_slice()[0].attempt_id, attempt_id);
    Box::pin(shop.receive(None)).await?;
    let occurrence = shop
        .payee_activity()
        .await?
        .into_iter()
        .find(|s| s.request_hash == Some(request.request_hash))
        .ok_or("missing sale")?
        .periods
        .into_iter()
        .find(|p| p.billing_period == period)
        .ok_or("missing period")?;
    let receipt = Box::pin(shop.confirm_payment(&occurrence.record))
        .await?
        .ok_or("missing receipt")?;
    let held = shop
        .app()
        .shared_record(&occurrence.record)
        .await?
        .ok_or("missing credited period")?;
    let record = ActivityRecord::read(
        held.content.value().ok_or("unreadable credit")?,
        &shop.identity(),
        ActivitySide::Payee,
    )?;
    let chosen = record
        .period_details
        .as_ref()
        .ok_or("missing selected details")?;
    let proof = record.proof.as_ref().ok_or("missing proof")?;
    let Body::PaymentProof(body) = proof.body() else {
        return Err("unexpected proof".into());
    };
    assert_eq!(chosen.message().msg_id, details.msg_id);
    assert_eq!(chosen.cert().app_id(), seller_wallet.app_id());
    assert_eq!(body.payment_app_id, seller_wallet.app_id());
    assert_eq!(body.payment_endpoint_identifier, endpoint);
    assert_eq!(proof.message().refs, [request.request_hash, chosen.hash()]);
    assert_eq!(
        record
            .receipt
            .as_ref()
            .ok_or("missing signed receipt")?
            .message()
            .refs,
        [proof.hash(), request.request_hash]
    );
    Box::pin(buyer.receive(None)).await?;
    let received = buyer.receipts(ReceiptsOf::Period(request, period)).await?;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].msg_id, receipt);
    assert_eq!(rail.pay_calls(&attempt_id), 1);
    Ok(())
}
