//! Payments, consent and reservations under an Allowance (Paykit spec 8.3).

use super::{
    method::Account,
    refusal::categorized,
    signer,
    wallet::{enrolled_payer, enrolled_wallet, host, run},
};
use crate::{
    AllowanceRole, AllowanceStatus, BackupDevice, DispatchAlarm, HostRail, NativeWallet,
    PaymentDecision, PaymentOutcome, PaymentResult, RefusalKind, RequestId, RuntimeTimer,
    WalletError, WalletFailure, WalletHost, WalletScopes,
};
use paykit_lib::{
    allowances::AllowanceTerms, Asset, AttemptId, Decimal, PaymentAmount,
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
use paykit_sdk::{AppScopes, Paykit, RequestDestination, RequestDraft, UserConfirmed};
use pubky_dispatch_client::{
    app::AppError,
    store::{Fault, FaultStore, MemoryStore, StoreHandle},
    StoreError,
};
use pubky_dispatch_harness::{parties::Asking, Backend, World};
use pubky_dispatch_signer::{CertChange, Signer};
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

    let attempt = AttemptId::from_bytes([0; 16]).to_text();
    assert!(p
        .payer
        .reserved_by_other_apps(record.clone())
        .await?
        .is_empty());
    invalid(
        p.payer
            .release_allowance_attempt(record.clone(), attempt.clone(), false)
            .await,
        "user_confirmed",
    );
    invalid(
        p.payer
            .release_allowance_attempt(record.clone(), "invalid".into(), true)
            .await,
        "attempt_id",
    );
    refusal(
        p.payer
            .release_allowance_attempt(record.clone(), attempt, true)
            .await,
        "not_sent",
        "usage",
    )?;

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
    let attempt = AttemptId::from_bytes([0; 16]).to_text();
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
        refusal(
            wallet.reserved_by_other_apps(record.clone()).await,
            "needs_user",
            "scope_missing",
        )?,
        refusal(
            wallet
                .release_allowance_attempt(record, attempt, true)
                .await,
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
    // Only the original executor expires while the reservation is held (Dispatch spec 10.2).
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
    let lost = refusal(decision(result), "outcome_unknown", "transport_no_answer")?;
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
    receive(&p.payer).await?;
    let reserved = p.payer.reserved_by_other_apps(record.clone()).await?;
    let [reservation] = reserved.as_slice() else {
        return Err("expected one reservation".into());
    };
    assert!(reservation.attempt_id == entry.attempt_id);
    assert_eq!(reservation.value, "1.25");

    // Record the local pre-handoff stop before releasing the other app's failure (Paykit spec 8.3).
    p.payer.resume_payments().await?;
    invalid(
        p.payer
            .release_allowance_attempt(record.clone(), reservation.attempt_id.clone(), false)
            .await,
        "user_confirmed",
    );
    let remaining = p.payer.reserved_by_other_apps(record.clone()).await?;
    assert!(matches!(remaining.as_slice(), [held] if held.attempt_id == reservation.attempt_id));
    p.payer
        .release_allowance_attempt(record.clone(), reservation.attempt_id.clone(), true)
        .await?;
    assert!(p
        .payer
        .reserved_by_other_apps(record.clone())
        .await?
        .is_empty());
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
fn test_allowance_reservation_releases_a_confirmed_foreign_failure() {
    run(Backend::Scripted, release);
    run(Backend::Service, release);
}

/// The refusal after a provider loss names the repair the user makes: no retry clears it.
fn not_resumed<T>(result: Result<T, WalletError>) -> TestResult {
    let failure = refusal(result, "needs_user", "allowance_not_resumed")?;
    let refused = failure.refusal().ok_or("missing the typed refusal")?;
    assert!(matches!(
        refused.kind(),
        RefusalKind::HeldForUser {
            reason: crate::HeldForUserKind::AllowanceNotResumed
        }
    ));
    Ok(())
}

/// The grants this app holds on the Allowance record, as its store has it.
async fn grant_count(wallet: &NativeWallet, record: &[u8]) -> TestResult<usize> {
    let key = crate::fixed(record.to_vec(), "record")?;
    wallet
        .lend(async |app| {
            let held = app
                .app()
                .shared_record(&key)
                .await?
                .ok_or("missing the Allowance record")?;
            let value = held
                .content
                .value()
                .ok_or("the Allowance record doesn't read")?;
            let allowance = paykit_lib::records::AllowanceRecord::read(value, &app.identity())?;
            Ok::<_, Box<dyn Error>>(allowance.grants.len())
        })
        .await?
}

/// After a provider loss the user confirms the Allowance again and the regrant's reply is
/// lost. The call again lands the kept regrant and builds no second one; the resume then
/// answers that automatic payments under it don't resume in this version, as a typed
/// refusal, the same every time (Paykit spec 8.3, "After provider loss").
async fn regrant_lost(world: &World) -> TestResult {
    let mut p = Box::pin(Parties::open(world)).await?;
    let record = Box::pin(p.accept()).await?;
    let before = grant_count(&p.payer, &record).await?;

    let buyer = world.identity("buyer")?;
    let successor = "https://recovered-buyer.payments.test";
    world.advance_ms(11 * 60_000);
    world
        .set_account_missing("https://buyer.payments.test", &buyer, true)
        .await?;
    world.add_dispatch(successor, &[&buyer]).await?;
    let opening = Box::pin(p.signer.declare_provider_loss(
        successor,
        pubky_dispatch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await?;
    Box::pin(p.signer.end_staged_admission()).await?;
    after_loss(&p.payer, opening.generation()).await?;
    let shop = p.shop.identity();
    p.payer
        .lend(async move |app| app.app_mut().invite(&shop).await)
        .await??;
    world.advance_ms(pubky_dispatch_client::app::DOC_CACHE_MS + 1);
    Box::pin(receive_app(&mut p.shop)).await?;

    let view = p.payer.allowance(record.clone()).await?;
    assert!(
        view.confirmation_needed,
        "the loss asks the user to confirm again"
    );
    world.lose_answer("POST", "/ops");
    let lost = p
        .payer
        .confirm_allowance_executor(view.hold.clone(), true)
        .await;
    let (case, _) = lost
        .err()
        .and_then(categorized)
        .ok_or("the regrant's answer is lost")?;
    assert_eq!(case, "outcome_unknown");
    // The service applied it, and this app doesn't know yet.
    assert_eq!(
        grant_count(&p.payer, &record).await?,
        before,
        "the lost regrant isn't landed here yet"
    );
    // The call again sends the kept regrant first, so it lands here, once.
    match p
        .payer
        .confirm_allowance_executor(view.hold.clone(), true)
        .await
    {
        Ok(()) => {}
        again => not_resumed(again)?,
    }
    assert_eq!(
        grant_count(&p.payer, &record).await?,
        before + 1,
        "the call again lands the regrant"
    );
    // Automatic payments under it don't resume, and no retry changes that.
    not_resumed(p.payer.resume_allowance_executor(record.clone()).await)?;
    receive(&p.payer).await?;
    assert_eq!(
        grant_count(&p.payer, &record).await?,
        before + 1,
        "no retry builds another grant"
    );
    assert_eq!(p.account.count(), 0);
    p.payer.shutdown().await?;
    Ok(())
}

/// The wallet follows the provider loss to `generation`: it lists its records and ends the
/// reconciliation the loss opened.
async fn after_loss(wallet: &NativeWallet, generation: u64) -> TestResult {
    let mut listed = false;
    for _ in 0..12 {
        let page = wallet.receive().await?;
        if page.generation >= generation
            && !page.stopped
            && !page.more
            && page.records_unlisted.is_none()
        {
            listed = true;
            break;
        }
    }
    if !listed {
        return Err("the wallet did not list its records after the loss".into());
    }
    for _ in 0..12 {
        match wallet.reconcile().await {
            // The records are still being listed: a wait.
            Err(WalletError::RetryLater { failure }) if failure.code() == "records_unlisted" => {}
            result => {
                result?;
                wallet.end_confirmed_reconciliation().await?;
                return Ok(());
            }
        }
    }
    Err("the reconciliation after the loss did not finish its listing".into())
}

#[test]
fn test_allowance_regrant_after_a_lost_reply_answers_not_resumed() {
    run(Backend::Scripted, regrant_lost);
    run(Backend::Service, regrant_lost);
}

const MISSING_LEDGER: &str =
    "the current service is missing the executor's accepted permanent ledger";

fn missing_ledger_alarm(alarm: Option<DispatchAlarm>) -> bool {
    matches!(alarm, Some(DispatchAlarm::ProtocolRollback { protocol, reason })
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
