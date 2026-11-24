//! Recurring requests, period pricing and Allowance funding (Paykit spec 8.2, 8.3).

use super::{
    method::Account,
    refusal::categorized,
    signer,
    wallet::{enrolled_payer, enrolled_wallet, run},
};
use crate::{
    AcceptOptions, BillingPeriod, ConversionRate, ListEndpoint, MessageDelivery, NativeWallet,
    PaymentDecision, PaymentOptions, PaymentOutcome, PaymentResult, PeriodDetails,
    PeriodPriceChoice, RecurrenceUnit, RecurringRequest, RecurringRequestStatus, RefusalKind,
    RequestId, RuntimeTimer, WalletError, WalletFailure, WalletScopes,
};
use paykit_lib::{
    allowances::AllowanceTerms,
    bodies::Conversion,
    periods::{AnchoredRule, PeriodRef, Recurrence, Unit},
    Asset, AttemptId, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::{
    AllowanceRole, AppScopes, Paykit, RequestDestination, RequestDraft, UserConfirmed,
};
use pubky_switch_client::store::MemoryStore;
use pubky_switch_core::json::check_uuid_v4;
use pubky_switch_harness::{parties::Asking, Backend, World};
use std::{
    collections::BTreeMap,
    error::Error,
    sync::{atomic::Ordering, Arc},
};

type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;
const ENDPOINT: &str = "btc-lightning-bolt11";

fn refusal<T>(
    result: Result<T, WalletError>,
    case: &str,
    code: &str,
) -> TestResult<Arc<WalletFailure>> {
    let (actual, failure) = result
        .err()
        .and_then(categorized)
        .ok_or("expected a categorized wallet error")?;
    assert_eq!(actual, case);
    assert_eq!(failure.code(), code);
    Ok(failure)
}

fn decision(result: PaymentResult) -> Result<PaymentOutcome, WalletError> {
    assert!(result.public_failures.is_empty());
    match result.decision {
        PaymentDecision::Outcome { outcome } => Ok(outcome),
        PaymentDecision::Error { error } => Err(error),
    }
}

async fn receive(wallet: &NativeWallet) -> TestResult {
    for _ in 0..8 {
        let report = wallet.receive().await?;
        if !report.more && report.records_unlisted.is_none() {
            return Ok(());
        }
    }
    Err("wallet did not finish listing its records".into())
}

fn paid(result: PaymentResult) -> TestResult<String> {
    let PaymentOutcome::Paid {
        attempt_id,
        proof_message_id,
    } = decision(result)?
    else {
        return Err("expected a paid period".into());
    };
    AttemptId::parse(&attempt_id)?;
    check_uuid_v4(&proof_message_id.ok_or("missing signed proof")?)?;
    Ok(attempt_id)
}

struct Wallets {
    _directory: tempfile::TempDir,
    payer: Arc<NativeWallet>,
    seller: Arc<NativeWallet>,
    account: Arc<Account>,
    buyer: [u8; 32],
    schedule: Recurrence,
}

impl Wallets {
    async fn open(world: &World, allowance: bool) -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let account = Arc::new(Account::open(directory.path(), world.clock().clone()));
        account.park.store(false, Ordering::SeqCst);
        let (_, payer, _, _) = Box::pin(enrolled_wallet(
            world,
            directory.path(),
            account.clone(),
            WalletScopes {
                pay: true,
                allowance,
                allowance_execute: allowance,
                ..Default::default()
            },
            None,
        ))
        .await?;
        // A newly joined app needs its first listing before it can show requests (Paykit spec 8.1).
        refusal(payer.recurring_requests().await, "needs_user", "not_now")?;
        receive(&payer).await?;
        let shop = world.identity("shop")?;
        let mut issuer = Box::pin(signer::serving(world, "shop")).await?;
        let mut scopes = AppScopes::new().request().manage_contacts();
        if allowance {
            scopes = scopes.allowance();
        }
        let scopes = scopes.build()?;
        let names = scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>();
        let asking = Asking::for_scopes(world.app_keys(&shop, "wallet")?, &names)?;
        let review = issuer.review(asking.request.signed(), &asking.auth)?;
        let approval = Box::pin(issuer.approve(review)).await?;
        let shop_app = Box::pin(Paykit::join(
            world.app_env(Arc::new(MemoryStore::new())),
            Default::default(),
            asking.join_request(shop.public(), &approval)?,
            payer.rail(),
            Arc::new(RuntimeTimer),
        ))
        .await?;
        issuer.claim_delivered(&approval.request).await?;
        let seller = NativeWallet::opened(shop_app, payer.rail(), None)?;
        receive(&seller).await?;
        let buyer = world.identity("buyer")?.public();
        seller.allow_connect(buyer.into()).await?;
        let start = world.now_ms();
        let schedule = Recurrence::new(AnchoredRule::new(start, 1, Unit::Hour)?, start, None)?;
        Ok(Self {
            _directory: directory,
            payer,
            seller,
            account,
            buyer,
            schedule,
        })
    }

    fn first(&self) -> TestResult<BillingPeriod> {
        Ok(PeriodRef::from(self.schedule.first()?).into())
    }

    async fn send_request(&self, asset: &str, recurring: bool) -> TestResult<RequestId> {
        let expiry = self.schedule.first()?.ends_at;
        let invite = self
            .payer
            .lend(async |app| app.app_mut().register_invite_code(expiry, 1).await)
            .await??;
        let mut draft = RequestDraft::new(
            PaymentAmount {
                asset: Asset::new(asset)?,
                value: Decimal::new("7")?,
            },
            PaymentReference::new("hourly service")?,
            RequestDestination::Endpoints(BTreeMap::from([(
                PaymentEndpointIdentifier::new(ENDPOINT)?,
                PaymentEndpointPayload::new("lnbc1request")?,
            )])),
        )
        .with_invite_code(invite.code)
        .with_proposal_expires_at_ms(expiry);
        if recurring {
            draft = draft.with_recurrence(self.schedule);
        }
        if asset != "btc" {
            draft = draft.with_conversion(Conversion::PerPeriod);
        }
        let buyer = self.buyer;
        let sent = self
            .seller
            .with_wallet(move |app| {
                Box::pin(async move { Ok(app.send_request(buyer, draft).await?) })
            })
            .await?;
        receive(&self.payer).await?;
        Ok(sent.request.ok_or("missing signed request")?.into())
    }

    async fn sale(&self, id: &RequestId) -> TestResult<paykit_sdk::PayeeActivity> {
        receive(&self.seller).await?;
        self.seller
            .lend(async |app| app.payee_activity().await)
            .await??
            .into_iter()
            .find(|sale| {
                sale.request_hash
                    .as_ref()
                    .is_some_and(|hash| hash.as_slice() == id.request_hash)
            })
            .ok_or_else(|| "missing payee activity".into())
    }

    async fn request(&self, id: &RequestId) -> TestResult<RecurringRequest> {
        let mut requests = self.payer.recurring_requests().await?;
        assert_eq!(requests.len(), 1);
        let view = requests.pop().ok_or("missing payer request")?;
        assert_eq!(view.id.payee, id.payee);
        assert_eq!(view.id.request_hash, id.request_hash);
        Ok(view)
    }

    async fn close(self) -> TestResult {
        self.seller.shutdown().await?;
        self.payer.shutdown().await?;
        Ok(())
    }
}

fn details(period: BillingPeriod) -> PeriodDetails {
    PeriodDetails {
        expires_at_ms: period.ends_at,
        billing_period: period,
        payment_endpoints: Some(vec![ListEndpoint {
            identifier: ENDPOINT.into(),
            payload: "lnbc1period".into(),
        }]),
        rates: None,
    }
}

async fn manual_period(world: &World) -> TestResult {
    let w = Box::pin(Wallets::open(world, false)).await?;
    let payer = &w.payer;
    let one_time = w.send_request("btc", false).await?;
    let id = w.send_request("btc", true).await?;
    let one_time_views = payer.requests().await?;
    assert_eq!(one_time_views.len(), 1);
    assert!(one_time_views
        .iter()
        .any(|r| r.id.request_hash == one_time.request_hash));

    let period = w.first()?;
    let open = w.request(&id).await?;
    assert_eq!(open.record.len(), 32);
    assert_eq!(open.payment_reference, "hourly service");
    assert_eq!(
        (open.amount.asset.as_str(), open.amount.value.as_str()),
        ("btc", "7")
    );
    assert!(matches!(open.status, RecurringRequestStatus::Open));
    assert!(open.label.is_none() && open.held.is_none() && open.receipt.is_none());
    assert!(!open.divergent && !open.manual_only && open.periods.is_empty());
    assert_eq!(open.proposal_expires_at_ms, Some(period.ends_at));
    assert_eq!(open.recurrence.anchor, period.starts_at);
    assert_eq!(open.recurrence.starts_at, period.starts_at);
    assert_eq!(open.recurrence.ends_at, None);
    assert_eq!(open.recurrence.every, 1);
    assert!(matches!(open.recurrence.unit, RecurrenceUnit::Hour));

    let sale = w.sale(&id).await?.record;
    refusal(
        w.seller
            .send_period_details(sale.into(), details(period.clone()))
            .await,
        "needs_user",
        "not_subscribed",
    )?;
    let not_subscribed = refusal(
        decision(
            payer
                .pay_period(id.clone(), period.clone(), PaymentOptions::default())
                .await?,
        ),
        "needs_user",
        "not_subscribed",
    )?;
    let unrelated = not_subscribed
        .refusal()
        .ok_or("missing subscription refusal")?;
    for (field, options) in [
        (
            "possibly_cancelled_confirmed",
            AcceptOptions {
                possibly_cancelled_confirmed: Some(unrelated.clone()),
                ..Default::default()
            },
        ),
        (
            "blocked_confirmed",
            AcceptOptions {
                blocked_confirmed: Some(unrelated.clone()),
                ..Default::default()
            },
        ),
    ] {
        let result = payer.accept_subscription(id.clone(), Some(options)).await;
        assert!(
            matches!(result, Err(WalletError::InvalidInput { field: actual }) if actual == field)
        );
    }
    let result = payer
        .pay_period_automatically(
            id.clone(),
            period.clone(),
            PeriodPriceChoice::AtRate { quote: unrelated },
        )
        .await;
    assert!(matches!(result, Err(WalletError::InvalidInput { field }) if field == "quote"));
    let acceptance = payer.accept_subscription(id.clone(), None).await?;
    check_uuid_v4(&acceptance.msg_id)?;
    assert!(matches!(
        acceptance.delivery,
        MessageDelivery::Accepted { .. }
    ));
    refusal(
        payer
            .accept_subscription(id.clone(), Some(AcceptOptions::default()))
            .await,
        "permanent",
        "subscribed",
    )?;
    assert_eq!(w.account.count(), 0);

    receive(&w.seller).await?;
    let message = w
        .seller
        .send_period_details(sale.into(), details(period.clone()))
        .await?;
    check_uuid_v4(&message.msg_id)?;
    assert!(matches!(message.delivery, MessageDelivery::Accepted { .. }));
    receive(payer).await?;
    let accepted = w.request(&id).await?;
    assert!(matches!(
        accepted.status,
        RecurringRequestStatus::Subscribed
    ));
    let [view] = accepted.periods.as_slice() else {
        return Err("expected one period".into());
    };
    assert_eq!(view.record.len(), 32);
    assert_ne!(view.record, accepted.record);
    assert_eq!(view.billing_period.starts_at, period.starts_at);
    assert_eq!(view.billing_period.ends_at, period.ends_at);
    assert!(!view.paid && !view.credited && view.receipt.is_none());
    let [sent] = view.details.as_slice() else {
        return Err("expected one set of details".into());
    };
    assert_eq!(sent.msg_id, message.msg_id);
    assert_eq!(sent.app_id, w.seller.app_id().await?);
    assert_eq!(sent.created_at_ms, world.now_ms());
    assert_eq!(sent.expires_at_ms, period.ends_at);
    assert_eq!(sent.endpoint_ids, [ENDPOINT]);
    assert!(sent.rates.is_empty());

    let attempt = paid(
        payer
            .pay_period(id.clone(), period.clone(), PaymentOptions::default())
            .await?,
    )?;
    let sale = w.sale(&id).await?;
    let [occurrence] = sale.periods.as_slice() else {
        return Err("expected one payee period".into());
    };
    let receipt = w
        .seller
        .lend(async |app| Box::pin(app.confirm_payment(&occurrence.record)).await)
        .await??
        .ok_or("missing period receipt")?;
    receive(payer).await?;
    let confirmed = w.request(&id).await?;
    let [view] = confirmed.periods.as_slice() else {
        return Err("missing paid period".into());
    };
    assert!(view.paid && view.credited);
    let held = view.receipt.as_ref().ok_or("missing payer receipt")?;
    assert_eq!(held.message_id, receipt);
    assert_eq!(held.attempt_id, attempt);
    assert_eq!(held.request_hash.as_ref(), Some(&id.request_hash));
    assert_eq!(
        held.period.as_ref().map(|p| (p.starts_at, p.ends_at)),
        Some((period.starts_at, period.ends_at))
    );
    assert_eq!(w.account.count(), 1);
    w.close().await
}

#[test]
fn test_recurring_requests_manual_period_and_receipt() {
    run(Backend::Scripted, manual_period);
    run(Backend::Service, manual_period);
}

async fn automatic_price(world: &World) -> TestResult {
    let w = Box::pin(Wallets::open(world, false)).await?;
    let payer = &w.payer;
    let id = w.send_request("usd", true).await?;
    let period = w.first()?;
    check_uuid_v4(
        &payer
            .accept_subscription(id.clone(), Some(AcceptOptions::default()))
            .await?
            .msg_id,
    )?;
    let record = w.sale(&id).await?.record;
    let mut price = details(period.clone());
    price.rates = Some(vec![ConversionRate {
        asset: "btc".into(),
        value: "0.0001".into(),
    }]);
    let message = w.seller.send_period_details(record.into(), price).await?;
    receive(payer).await?;
    let request = w.request(&id).await?;
    let [occurrence] = request.periods.as_slice() else {
        return Err("missing priced period".into());
    };
    let [details] = occurrence.details.as_slice() else {
        return Err("missing signed rate".into());
    };
    let [rate] = details.rates.as_slice() else {
        return Err("expected one rate".into());
    };
    assert_eq!(
        (rate.asset.as_str(), rate.value.as_str()),
        ("btc", "0.0001")
    );
    let shown = payer
        .pay_period_automatically(
            id.clone(),
            period.clone(),
            PeriodPriceChoice::QuoteFor {
                endpoint: ENDPOINT.into(),
            },
        )
        .await?;
    let failure = refusal(decision(shown), "needs_user", "priced_by_rate")?;
    let quote = failure.refusal().ok_or("missing priced payment")?;
    assert!(
        matches!(quote.kind(), RefusalKind::PricedByRate { request, period: Some(p), detail_message_id: Some(source), amount, .. }
        if request.request_hash == id.request_hash && (p.starts_at, p.ends_at) == (period.starts_at, period.ends_at)
        && source == message.msg_id && amount.asset == "btc" && amount.value == "0.0007")
    );
    assert_eq!(w.account.count(), 0);
    paid(
        payer
            .pay_period_automatically(id, period, PeriodPriceChoice::AtRate { quote })
            .await?,
    )?;
    assert_eq!(w.account.count(), 1);
    w.close().await
}

#[test]
fn test_period_automatic_payment_uses_the_authorized_rate() {
    run(Backend::Scripted, automatic_price);
    run(Backend::Service, automatic_price);
}

async fn funded_period(world: &World) -> TestResult {
    let w = Box::pin(Wallets::open(world, true)).await?;
    let id = w.send_request("btc", true).await?;
    let period = w.first()?;
    let terms = AllowanceTerms::from_json(&serde_json::json!({
        "asset": "btc", "lifetime_amount_limit": "7", "period_limits": [],
        "per_payment_amount": null, "active_from": null, "expires_at": null,
        "allowed_payment_endpoint_identifiers": null
    }))?;
    // Admit the shop for its Allowance proposal (Switch spec 7).
    w.payer.allow_connect(w.seller.identity().await?).await?;
    let buyer = w.buyer;
    let proposal = w
        .seller
        .with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .propose_allowance(buyer, AllowanceRole::Allowee, terms)
                    .await?)
            })
        })
        .await?;
    receive(&w.payer).await?;
    let key = w
        .payer
        .lend(async |app| app.allowances().await)
        .await??
        .into_iter()
        .find(|offer| offer.allowance_id == proposal.allowance_id)
        .ok_or("missing Allowance proposal")?
        .record;
    w.payer
        .with_wallet(move |app| {
            Box::pin(async move {
                app.accept_executable_allowance(&key, UserConfirmed::new())
                    .await?;
                Ok(app.resume_allowance_executor(&key).await?)
            })
        })
        .await?;
    refusal(
        w.payer
            .accept_subscription_under_allowance(id.clone(), vec![0; 32])
            .await,
        "permanent",
        "not_found",
    )?;
    let accepted = w
        .payer
        .accept_subscription_under_allowance(id.clone(), key.into())
        .await?;
    check_uuid_v4(&accepted.msg_id)?;
    assert!(matches!(
        accepted.delivery,
        MessageDelivery::Accepted { .. }
    ));
    assert_eq!(w.account.count(), 0);
    refusal(
        decision(
            w.payer
                .pay_period_automatically(
                    id.clone(),
                    period.clone(),
                    PeriodPriceChoice::RequestedAmount,
                )
                .await?,
        ),
        "permanent",
        "manual_only",
    )?;
    paid(
        w.payer
            .pay_period_under_allowance(id.clone(), period.clone())
            .await?,
    )?;
    refusal(
        decision(w.payer.pay_period_under_allowance(id, period).await?),
        "permanent",
        "already_paid",
    )?;
    assert_eq!(w.account.count(), 1);
    w.close().await
}

#[test]
fn test_period_under_allowance_uses_only_the_selected_grant() {
    run(Backend::Scripted, funded_period);
    run(Backend::Service, funded_period);
}

async fn payment_guards(world: &World) -> TestResult {
    let directory = tempfile::tempdir()?;
    let account = Arc::new(Account::open(directory.path(), world.clock().clone()));
    let (host, wallet, app_id, _) =
        Box::pin(enrolled_payer(world, directory.path(), account.clone())).await?;
    wallet.shutdown().await?;
    let mut host = (*host).clone();
    host.rail = Arc::new(account.rail_without_history());
    let wallet = host.load(app_id).await?;
    let id = RequestId {
        payee: world.identity("shop")?.public().into(),
        request_hash: vec![0; 32],
    };
    let period = BillingPeriod {
        starts_at: 0,
        ends_at: world.now_ms(),
    };
    for result in [
        wallet
            .pay_period(id.clone(), period.clone(), PaymentOptions::default())
            .await,
        wallet
            .pay_period_automatically(
                id.clone(),
                period.clone(),
                PeriodPriceChoice::RequestedAmount,
            )
            .await,
        wallet
            .pay_period_under_allowance(id.clone(), period.clone())
            .await,
    ] {
        assert!(matches!(
            result,
            Err(WalletError::PaymentMethodWithoutLookup)
        ));
    }
    // A bad price choice is the host's input error before the method's lookup is checked.
    let choice = wallet
        .pay_period_automatically(
            id.clone(),
            period,
            PeriodPriceChoice::QuoteFor {
                endpoint: "bad/endpoint".into(),
            },
        )
        .await;
    assert!(
        matches!(choice, Err(WalletError::InvalidInput { field }) if field == "endpoint identifier")
    );
    for period in [
        BillingPeriod {
            starts_at: 1,
            ends_at: 1,
        },
        BillingPeriod {
            starts_at: 1,
            ends_at: 0,
        },
        BillingPeriod {
            starts_at: 0,
            ends_at: u64::MAX,
        },
    ] {
        assert!(
            matches!(wallet.pay_period(id.clone(), period, PaymentOptions::default()).await,
            Err(WalletError::InvalidInput { field }) if field == "billing period")
        );
    }
    assert!(wallet.payments().await?.is_empty());
    assert_eq!(account.count(), 0);
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_period_payments_validate_bounds_and_require_lookup() {
    run(Backend::Scripted, payment_guards);
    run(Backend::Service, payment_guards);
}
