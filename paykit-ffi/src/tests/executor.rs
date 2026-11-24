//! Payments, consent and reservations under an Allowance (Paykit spec 8.3).

use super::{
    method::Account,
    refusal::categorized,
    signer,
    wallet::{enrolled_payer, enrolled_wallet, host, run},
};
use crate::{
    AllowanceRole, AllowanceStatus, BackupDevice, HostRail, NativeWallet, PaymentDecision,
    PaymentOutcome, PaymentResult, RefusalKind, RequestId, RuntimeTimer, SwitchAlarm, WalletError,
    WalletFailure, WalletHost, WalletScopes,
};
use paykit_lib::{
    allowances::AllowanceTerms, Asset, AttemptId, Decimal, PaymentAmount,
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_sdk::{AppScopes, Paykit, RequestDestination, RequestDraft, UserConfirmed};
use pubky_switch_client::{
    app::AppError,
    store::{Fault, FaultStore, MemoryStore, StoreHandle},
    StoreError,
};
use pubky_switch_harness::{parties::Asking, Backend, World};
use pubky_switch_signer::{CertChange, Signer};
use std::{collections::BTreeMap, error::Error, sync::atomic::Ordering, sync::Arc};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn refusal<T>(
    result: Result<T, WalletError>,
    case: &str,
    code: &str,
) -> TestResult<Arc<WalletFailure>> {
    let (actual, failure) = result
        .err()
        .and_then(categorized)
        .ok_or("expected a wallet refusal")?;
    assert_eq!(actual, case);
    assert_eq!(failure.code(), code);
    Ok(failure)
}

fn invalid<T>(result: Result<T, WalletError>, expected: &str) {
    assert!(matches!(result, Err(WalletError::InvalidInput { field }) if field == expected));
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

async fn receive_app(app: &mut Paykit) -> TestResult {
    for _ in 0..8 {
        let summary = Box::pin(app.receive(None)).await?;
        if !summary.report.more && summary.records_unlisted.is_none() {
            return Ok(());
        }
    }
    Err("app did not finish listing its records".into())
}

async fn join(
    world: &World,
    owner: &str,
    name: &str,
    signer: &mut Signer,
    scopes: AppScopes,
    rail: Arc<HostRail>,
) -> TestResult<Paykit> {
    let identity = world.identity(owner)?;
    let scopes = scopes.build()?;
    let names: Vec<_> = scopes.scopes.iter().map(String::as_str).collect();
    let asking = Asking::for_scopes(world.app_keys(&identity, name)?, &names)?;
    let reviewed = signer.review(asking.request.signed(), &asking.auth)?;
    let approval = Box::pin(signer.approve(reviewed)).await?;
    let mut app = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        asking.join_request(identity.public(), &approval)?,
        rail,
        Arc::new(RuntimeTimer),
    ))
    .await?;
    signer.claim_delivered(&approval.request).await?;
    Box::pin(receive_app(&mut app)).await?;
    Ok(app)
}

struct Parties {
    payer: Arc<NativeWallet>,
    shop: Paykit,
    account: Arc<Account>,
    host: Arc<WalletHost>,
    signer: Signer,
    buyer: [u8; 32],
    _directory: tempfile::TempDir,
}

impl Parties {
    async fn open(world: &World) -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let account = Arc::new(Account::open(directory.path(), world.clock().clone()));
        account.park.store(false, Ordering::SeqCst);
        let (host, payer, _, signer) = Box::pin(enrolled_wallet(
            world,
            directory.path(),
            account.clone(),
            WalletScopes {
                pay: true,
                allowance: true,
                allowance_execute: true,
                manage_contacts: true,
                ..Default::default()
            },
            None,
        ))
        .await?;
        receive(&payer).await?;
        let mut issuer = Box::pin(signer::serving(world, "shop")).await?;
        let mut shop = Box::pin(join(
            world,
            "shop",
            "wallet",
            &mut issuer,
            AppScopes::new().request().allowance(),
            payer.rail(),
        ))
        .await?;
        let buyer = world.identity("buyer")?.public();
        shop.allow_connect(&buyer).await?;
        payer.allow_connect(shop.identity().into()).await?;
        Ok(Self {
            payer,
            shop,
            account,
            host,
            signer,
            buyer,
            _directory: directory,
        })
    }

    async fn accept(&mut self) -> TestResult<Vec<u8>> {
        let terms = AllowanceTerms::from_json(&serde_json::json!({
            "asset": "btc", "per_payment_amount": {"minimum": "0.01", "maximum": "2.00"},
            "period_limits": [], "lifetime_amount_limit": "10.00", "active_from": null,
            "expires_at": null, "allowed_payment_endpoint_identifiers": null
        }))?;
        let proposal = Box::pin(self.shop.propose_allowance(
            self.buyer,
            paykit_sdk::AllowanceRole::Allowee,
            terms,
        ))
        .await?;
        receive(&self.payer).await?;
        let view = self
            .payer
            .allowances()
            .await?
            .into_iter()
            .find(|v| v.allowance_id == proposal.allowance_id.as_str())
            .ok_or("missing proposal")?;
        assert_eq!(view.role, Some(AllowanceRole::Allower));
        let record = view.record;
        self.payer
            .accept_executable_allowance(record.clone(), true)
            .await?;
        self.payer.resume_allowance_executor(record.clone()).await?;
        Box::pin(receive_app(&mut self.shop)).await?;
        let view = self.payer.allowance(record.clone()).await?;
        assert_eq!(view.status, AllowanceStatus::Accepted);
        assert!(view.consented && view.executor_app_id == Some(self.payer.app_id().await?));
        Ok(record)
    }

    async fn checkout(&mut self, value: &str) -> TestResult<RequestId> {
        let draft = RequestDraft::new(
            PaymentAmount {
                asset: Asset::new("btc")?,
                value: Decimal::new(value)?,
            },
            PaymentReference::new("checkout")?,
            RequestDestination::Endpoints(BTreeMap::from([(
                PaymentEndpointIdentifier::new("btc-lightning-bolt11")?,
                PaymentEndpointPayload::new("lnbc1checkout")?,
            )])),
        );
        let sent = Box::pin(self.shop.send_request(self.buyer, draft)).await?;
        assert!(matches!(
            sent.delivery,
            paykit_sdk::MessageDelivery::Accepted { .. }
        ));
        receive(&self.payer).await?;
        Ok(sent.request.ok_or("missing signed request")?.into())
    }

    async fn other_payer(&mut self, world: &World) -> TestResult<Paykit> {
        Box::pin(join(
            world,
            "buyer",
            "second",
            &mut self.signer,
            AppScopes::new().pay().allowance(),
            self.payer.rail(),
        ))
        .await
    }
}

async fn payments(world: &World) -> TestResult {
    let mut p = Box::pin(Parties::open(world)).await?;
    let record = Box::pin(p.accept()).await?;
    let hold = p.payer.allowance(record.clone()).await?.hold;
    invalid(
        p.payer
            .confirm_allowance_executor(hold.clone(), false)
            .await,
        "user_confirmed",
    );
    p.payer
        .confirm_allowance_executor(hold.clone(), true)
        .await?;
    assert!(p.payer.allowance(record.clone()).await?.consented);

    let inside = Box::pin(p.checkout("2.00")).await?;
    let result = p.payer.pay_under_allowance(inside, record.clone()).await?;
    let PaymentOutcome::Paid { attempt_id, .. } = decision(result)? else {
        return Err("expected the payment at the limit to settle".into());
    };
    AttemptId::parse(&attempt_id)?;
    assert_eq!(p.account.count(), 1);
    let over = Box::pin(p.checkout("2.01")).await?;
    let result = p
        .payer
        .pay_under_allowance(over.clone(), record.clone())
        .await?;
    refusal(decision(result), "permanent", "not_automatic")?;
    assert_eq!(p.account.count(), 1);
    assert_eq!(p.account.calls.load(Ordering::SeqCst), 1);
    let entries = p.payer.payments().await?;
    let [paid] = entries.as_slice() else {
        return Err("expected only the paid attempt".into());
    };
    assert!(paid.attempt_id == attempt_id);

    p.payer.end_allowance(record.clone()).await?;
    refusal(
        p.payer.resume_allowance_executor(record.clone()).await,
        "permanent",
        "allowance_ended",
    )?;
    refusal(
        p.payer.confirm_allowance_executor(hold, true).await,
        "permanent",
        "allowance_ended",
    )?;
    invalid(
        p.payer.pay_under_allowance(over.clone(), vec![0; 31]).await,
        "allowance",
    );
    let app_id = p.payer.app_id().await?;
    p.payer.shutdown().await?;
    let mut host = (*p.host).clone();
    host.rail = Arc::new(p.account.rail_without_history());
    let reopened = host.load(app_id).await?;
    assert!(matches!(
        reopened.pay_under_allowance(over, record).await,
        Err(WalletError::PaymentMethodWithoutLookup)
    ));
    assert_eq!(p.account.count(), 1);
    reopened.shutdown().await?;
    Ok(())
}

#[test]
fn test_allowance_payment_enforces_terms_and_confirmation() {
    run(Backend::Scripted, payments);
    run(Backend::Service, payments);
}

async fn confirmation(world: &World) -> TestResult {
    let mut p = Box::pin(Parties::open(world)).await?;
    let record = Box::pin(p.accept()).await?;
    let original = p.payer.allowance(record.clone()).await?.hold;
    let id = p.payer.app_id().await?;
    let backup = p.payer.backup().await?;

    // The service cursor moves past the backup, so restoring it holds consent (Paykit spec 8.3).
    Box::pin(p.checkout("1")).await?;
    p.payer
        .lend(async |app| app.app_mut().flush_cursor().await)
        .await??;
    p.payer.shutdown().await?;
    let directory = tempfile::tempdir()?;
    let restored = host(world, directory.path(), p.account.clone())
        .restore(id, backup, BackupDevice::InPossession)
        .await?;
    receive(&restored).await?;
    let mut remaining = 8;
    loop {
        match restored.reconcile().await {
            Ok(_) => break,
            // The restored records are still being listed: a wait.
            Err(WalletError::RetryLater { failure })
                if failure.code() == "records_unlisted" && remaining > 1 =>
            {
                remaining -= 1;
            }
            Err(error) => return Err(error.into()),
        }
    }
    restored.end_confirmed_reconciliation().await?;
    let view = restored.allowance(record.clone()).await?;
    assert!(view.confirmation_needed && !view.consented);

    let failure = refusal(
        restored.confirm_allowance_executor(original, true).await,
        "needs_user",
        "allowance_confirmation_needed",
    )?;
    let reason = failure.refusal().ok_or("missing Allowance refusal")?;
    let RefusalKind::AllowanceConfirmationNeeded { record: held } = reason.kind() else {
        return Err("expected an Allowance to confirm".into());
    };
    assert_eq!(held, record);
    let view = restored.allowance(held).await?;
    assert!(view.confirmation_needed && !view.consented);
    restored.confirm_allowance_executor(view.hold, true).await?;
    let view = restored.allowance(record).await?;
    assert!(view.consented && !view.confirmation_needed);
    restored.shutdown().await?;
    Ok(())
}

#[test]
fn test_allowance_confirmation_rejects_a_stale_hold_and_renews_consent() {
    run(Backend::Scripted, confirmation);
    run(Backend::Service, confirmation);
}

async fn scopes(world: &World) -> TestResult {
    let mut p = Box::pin(Parties::open(world)).await?;
    let other = Box::pin(p.other_payer(world)).await?;
    let wallet = NativeWallet::opened(other, p.payer.rail(), None)?;
    let record = Box::pin(p.accept()).await?;
    let request = Box::pin(p.checkout("1")).await?;
    receive(&wallet).await?;
    let hold = wallet.allowance(record.clone()).await?.hold;
    let result = wallet.pay_under_allowance(request, record.clone()).await?;
    let missing = [
        refusal(decision(result), "needs_user", "scope_missing")?,
        refusal(
            wallet.resume_allowance_executor(record.clone()).await,
            "needs_user",
            "scope_missing",
        )?,
        refusal(
            wallet.confirm_allowance_executor(hold, true).await,
            "needs_user",
            "scope_missing",
        )?,
    ];
    for failure in missing {
        let refused = failure.refusal().ok_or("missing executor scope refusal")?;
        assert!(
            matches!(refused.kind(), RefusalKind::ScopeMissing { scope } if scope == "paykit_allowance_execute")
        );
    }
    assert_eq!(p.account.count(), 0);
    wallet.shutdown().await?;
    p.payer.shutdown().await?;
    Ok(())
}

#[test]
fn test_executor_scope_is_required_on_another_payer() {
    run(Backend::Scripted, scopes);
    run(Backend::Service, scopes);
}

async fn release(world: &World) -> TestResult {
    let mut p = Box::pin(Parties::open(world)).await?;
    let mut other = Box::pin(p.other_payer(world)).await?;
    // Only the original executor expires while the reservation is held (Switch spec 10.2).
    let app_id = crate::fixed(p.payer.app_id().await?, "app_id")?;
    let lifetime = 3_600_000;
    let expiry = world
        .now_ms()
        .checked_add(lifetime)
        .ok_or("certificate expiry overflow")?;
    p.signer
        .renew(
            app_id,
            CertChange {
                expires_at: Some(expiry),
                ..Default::default()
            },
        )
        .await?;
    receive(&p.payer).await?;
    assert_eq!(
        p.payer
            .lend(async |app| app.app().cert().expires_at())
            .await?,
        expiry
    );
    let record = Box::pin(p.accept()).await?;
    let request = Box::pin(p.checkout("1.25")).await?;

    // The service applied the reservation but its reply never reached the executor (Paykit spec 8.3).
    world.lose_answer("POST", "/ops");
    let result = p.payer.pay_under_allowance(request, record.clone()).await?;
    let lost = refusal(decision(result), "outcome_unknown", "transport")?;
    assert!(lost.kept_batch().is_some());
    receive(&p.payer).await?;
    let entries = p.payer.payments().await?;
    let [entry] = entries.as_slice() else {
        return Err("missing reserved attempt".into());
    };
    assert_eq!(p.account.count(), 0);

    world.advance_ms(lifetime + 1);
    Box::pin(receive_app(&mut other)).await?;
    let attempt = AttemptId::parse(&entry.attempt_id)?;
    let obligation = crate::fixed(entry.obligation.clone(), "obligation")?;
    Box::pin(other.resolve_attempt(
        &obligation,
        attempt,
        paykit_sdk::Resolution::Failed,
        UserConfirmed::new(),
    ))
    .await?;
    p.signer.renew(app_id, CertChange::default()).await?;
    // The executor copies the other app's confirmed failure with no one acting, and the
    // reservation comes back (Paykit spec 8.3, `failed` releases).
    receive(&p.payer).await?;
    p.payer.resume_payments().await?;
    let usage = p
        .payer
        .reconcile_allowance(record)
        .await?
        .ok_or("missing verified ledger")?;
    assert_eq!(
        (usage.reserved, usage.released, usage.lifetime.as_str()),
        (0, 1, "0")
    );
    assert_eq!(p.account.calls.load(Ordering::SeqCst), 0);
    p.payer.shutdown().await?;
    Ok(())
}

#[test]
fn test_allowance_reservation_comes_back_on_another_apps_confirmed_failure() {
    run(Backend::Scripted, release);
    run(Backend::Service, release);
}

const MISSING_LEDGER: &str =
    "the current service is missing the executor's accepted permanent ledger";

fn missing_ledger_alarm(alarm: Option<SwitchAlarm>) -> bool {
    matches!(alarm, Some(SwitchAlarm::ProtocolRollback { protocol, reason })
        if protocol == "paykit" && reason == MISSING_LEDGER)
}

async fn ledger(world: &World) -> TestResult {
    let mut p = Box::pin(Parties::open(world)).await?;
    assert!(p.payer.ledger_alarm().await?.is_none());
    Box::pin(p.accept()).await?;
    let id = p.payer.app_id().await?;
    p.payer
        .lend(async |app| app.app_mut().raise_rollback("paykit", MISSING_LEDGER).await)
        .await??;
    for _ in 0..2 {
        assert!(missing_ledger_alarm(p.payer.ledger_alarm().await?));
    }
    p.payer.shutdown().await?;
    assert!(matches!(
        p.payer.ledger_alarm().await,
        Err(WalletError::Closed)
    ));
    let reopened = p.host.load(id).await?;
    assert!(missing_ledger_alarm(reopened.ledger_alarm().await?));
    reopened.shutdown().await?;
    Ok(())
}

#[test]
fn test_ledger_alarm_survives_reopening() {
    run(Backend::Scripted, ledger);
    run(Backend::Service, ledger);
}

async fn unknown_alarm(world: &World) -> TestResult {
    let directory = tempfile::tempdir()?;
    let account = Arc::new(Account::open(directory.path(), world.clock().clone()));
    let (host, payer, id, _) = Box::pin(enrolled_payer(world, directory.path(), account)).await?;
    payer.shutdown().await?;
    let id = crate::fixed(id, "app_id")?;
    let payer = host
        .open_wallet(id, async |mut env| {
            let store = FaultStore::new(env.store.store().clone(), Fault::FailAfter(1));
            env.store = StoreHandle::new(Arc::new(store));
            Ok(host.load_app(env, id).await?)
        })
        .await?;
    assert!(payer.ledger_alarm().await?.is_none());
    let raised = payer
        .lend(async |app| app.app_mut().raise_rollback("paykit", MISSING_LEDGER).await)
        .await?;
    assert!(matches!(raised, Err(AppError::Store(StoreError::Unknown))));
    assert!(missing_ledger_alarm(payer.ledger_alarm().await?));
    payer.shutdown().await?;
    Ok(())
}

#[test]
fn test_ledger_alarm_reloads_an_unknown_commit_on_the_open_wallet() {
    run(Backend::Scripted, unknown_alarm);
    run(Backend::Service, unknown_alarm);
}
