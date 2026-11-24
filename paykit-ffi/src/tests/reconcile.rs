//! Wallet reconciliation and provider loss (Paykit spec 7.3).

use super::{
    method::Account,
    refusal::categorized,
    signer,
    wallet::{enrolled_payer, enrolled_wallet, run},
};
use crate::{
    ConfirmedResolution, DoubtfulAttempt, NativeWallet, RuntimeTimer, WalletError,
    WalletReconciliation, WalletScopes,
};
use paykit_lib::{
    collections::Collection,
    derive::request_obligation,
    recovery::{RecoveryCommitments, RecoveryWitness, RECOVERY_UPLOAD_LABEL},
    Asset, AttemptId, Decimal, PaymentAmount, PaymentReference,
};
use paykit_sdk::{AppScopes, Paykit, RequestDestination, RequestDraft, RequestRef};
use pubky_switch_client::{
    app::{ClaimOp, Collection as SwitchCollection, ObligationDeadline, OpBatch, RecordTarget},
    store::MemoryStore,
};
use pubky_switch_core::{
    claims::Outcome,
    enc::{b64, lp},
    records::{IdentityDoc, DAY_MS},
    shared::ApplicationRef,
};
use pubky_switch_harness::{parties::Asking, Backend, World};
use std::{error::Error, sync::Arc};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn refused<T>(result: Result<T, WalletError>, case: &str, code: &str) -> TestResult {
    let (actual, failure) = result
        .err()
        .and_then(categorized)
        .ok_or("expected a categorized wallet refusal")?;
    assert_eq!((actual, failure.code().as_str()), (case, code));
    Ok(())
}

async fn receive(wallet: &NativeWallet, generation: u64) -> TestResult {
    for _ in 0..12 {
        let page = wallet.receive().await?;
        if page.generation >= generation
            && !page.stopped
            && !page.more
            && page.records_unlisted.is_none()
        {
            return Ok(());
        }
    }
    Err("wallet did not finish its record listing".into())
}

async fn reconcile(wallet: &NativeWallet) -> TestResult<WalletReconciliation> {
    for _ in 0..12 {
        match wallet.reconcile().await {
            Err(WalletError::NeedsUser { failure }) if failure.code() == "not_now" => continue,
            result => return Ok(result?),
        }
    }
    Err("reconciliation did not finish its record listing".into())
}

async fn publish_report(
    wallet: &mut Paykit,
    opening: &IdentityDoc,
    commitments: &RecoveryCommitments,
) -> TestResult<ApplicationRef> {
    let app = wallet.app_mut();
    let original = app.app_id();
    let field = commitments.signing_field(opening, &original)?;
    let signature = app.sign_protocol(RECOVERY_UPLOAD_LABEL, &[&field])?;
    let value = serde_json::to_vec(&serde_json::json!({
        "v": 1,
        "cutoff": b64(&commitments.cutoff),
        "inventory": b64(&commitments.inventory),
        "dispositions": b64(&commitments.dispositions),
        "signature": b64(&signature),
        "author_certificate": app.cert().signed(),
    }))?;
    let family = Collection::RecoveryUploads;
    let target = RecordTarget::new(
        SwitchCollection::new(family.name(), family.fixed_policy()),
        &lp(&[&original, &opening.hash()]),
    );
    app.put_record(&target, &value, vec![]).await?;
    let applied = app
        .read_record_application(&target)
        .await?
        .ok_or("missing report write")?;
    Ok(applied.application)
}

async fn members(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let scopes = WalletScopes {
        pay: true,
        recovery: true,
        ..Default::default()
    };
    let (_, wallet, id, mut signer) =
        Box::pin(enrolled_wallet(world, root.path(), account, scopes, None)).await?;
    receive(&wallet, 0).await?;
    assert!(wallet.recovery_members().await?.is_none());
    let commitments = wallet
        .lend(async |app| {
            RecoveryCommitments::from_evidence(
                &[RecoveryWitness::Identity(app.app().own_identity())],
                &[],
                &[],
            )
        })
        .await??;

    let buyer = world.identity("buyer")?;
    let successor = "https://recovered-buyer.payments.test";
    world.advance_ms(11 * 60_000);
    world
        .set_account_missing("https://buyer.payments.test", &buyer, true)
        .await?;
    world.add_switch(successor, &[&buyer]).await?;
    let opening = Box::pin(signer.declare_provider_loss(
        successor,
        pubky_switch_signer::UserConfirmed::shown_and_confirmed(),
    ))
    .await?;
    Box::pin(signer.end_staged_admission()).await?;
    receive(&wallet, opening.generation()).await?;
    reconcile(&wallet).await?;
    wallet.end_confirmed_reconciliation().await?;
    world.advance_ms(3_000);
    let write = wallet
        .lend(async |app| Box::pin(publish_report(app, &opening, &commitments)).await)
        .await??;
    receive(&wallet, opening.generation()).await?;
    let members = wallet
        .recovery_members()
        .await?
        .ok_or("missing recovery opening")?;
    assert_eq!(members.opening, opening.hash());
    assert_eq!(members.loss.generation, opening.generation());
    assert!(members.loss.uncertain_until_ms >= members.loss.declared_at_ms);
    assert!(members.loss.payment.is_none());
    assert_eq!(members.originals.as_slice(), std::slice::from_ref(&id));
    assert!(members.removed.is_empty());
    let [report] = members.reports.as_slice() else {
        return Err("expected one verified report".into());
    };
    assert_eq!(report.original, id);
    assert_eq!(report.publisher, id);
    assert_eq!(report.application.generation, write.generation);
    assert_eq!(report.application.seq, write.seq);
    assert_eq!(report.application.item_hash, write.item_hash);
    assert_eq!(report.cutoff, commitments.cutoff);
    assert_eq!(report.inventory, commitments.inventory);
    assert_eq!(report.dispositions, commitments.dispositions);
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_recovery_members_follow_the_signed_opening() {
    run(Backend::Scripted, members);
    run(Backend::Service, members);
}

async fn no_recovery_grant(world: &World) -> TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, wallet, _, _) = Box::pin(enrolled_payer(world, root.path(), account)).await?;
    receive(&wallet, 0).await?;
    refused(wallet.recovery_members().await, "needs_user", "not_now")?;
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_recovery_members_require_the_recovery_grant() {
    run(Backend::Scripted, no_recovery_grant);
    run(Backend::Service, no_recovery_grant);
}

async fn shop(world: &World, wallet: &NativeWallet) -> TestResult<Paykit> {
    let owner = world.identity("shop")?;
    let mut signer = Box::pin(signer::serving(world, "shop")).await?;
    let grant = AppScopes::new().request().build()?;
    let names = grant.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let asking = Asking::for_scopes(world.app_keys(&owner, "wallet")?, &names)?;
    let review = signer.review(asking.request.signed(), &asking.auth)?;
    let approval = Box::pin(signer.approve(review)).await?;
    let mut shop = Box::pin(Paykit::join(
        world.app_env(Arc::new(MemoryStore::new())),
        Default::default(),
        asking.join_request(owner.public(), &approval)?,
        wallet.rail(),
        Arc::new(RuntimeTimer),
    ))
    .await?;
    signer.claim_delivered(&approval.request).await?;
    for _ in 0..8 {
        let page = Box::pin(shop.receive(None)).await?;
        if !page.report.more && page.records_unlisted.is_none() {
            wallet.allow_connect(owner.public().into()).await?;
            shop.app_mut()
                .allow_connect(&world.identity("buyer")?.public())
                .await?;
            return Ok(shop);
        }
    }
    Err("shop did not finish its record listing".into())
}

struct OpenAttempt {
    request: RequestRef,
    obligation: [u8; 32],
    id: AttemptId,
}

impl OpenAttempt {
    // An authenticated request with an open claim, absent from the wallet's journal
    // and the method's history, as can happen after a restore (Paykit spec 7.3).
    async fn on_request(
        world: &World,
        wallet: &NativeWallet,
        shop: &mut Paykit,
        reference: &str,
    ) -> TestResult<Self> {
        let deadline = world
            .now_ms()
            .checked_add(3_600_000)
            .ok_or("deadline overflow")?;
        let draft = RequestDraft::new(
            PaymentAmount {
                asset: Asset::new("btc")?,
                value: Decimal::new("1")?,
            },
            PaymentReference::new(reference)?,
            RequestDestination::Any,
        )
        .with_payment_deadline_ms(deadline);
        let sent = Box::pin(shop.send_request(world.identity("buyer")?.public(), draft)).await?;
        assert!(matches!(
            sent.delivery,
            paykit_sdk::MessageDelivery::Accepted { .. }
        ));
        let request = sent.request.ok_or("missing signed request")?;
        receive(wallet, 0).await?;
        let id = AttemptId::generate()?;
        let pin = deadline.checked_add(1).ok_or("claim deadline overflow")?;
        let obligation = wallet
            .lend(async |app| -> TestResult<[u8; 32]> {
                let key = request_obligation(
                    app.app().naming_key(),
                    &request.payee,
                    &request.request_hash,
                    0,
                );
                let claim = ClaimOp::attempt(key, 0, id.to_bytes(), 300, Some(pin));
                app.app_mut()
                    .submit_batch(
                        OpBatch::for_protocol("paykit")
                            .op(claim.kind())
                            .obligation_deadline(key, ObligationDeadline::At(pin)),
                    )
                    .await?;
                Ok(key)
            })
            .await??;
        Ok(Self {
            request,
            obligation,
            id,
        })
    }

    fn handle(&self, report: &WalletReconciliation) -> TestResult<Arc<DoubtfulAttempt>> {
        let row = report
            .doubtful
            .iter()
            .find(|row| row.obligation == self.obligation)
            .ok_or("missing doubtful obligation")?;
        assert_eq!(
            row.request_hash.as_deref(),
            Some(self.request.request_hash.as_slice())
        );
        let [attempt] = row.attempts.as_slice() else {
            return Err("expected one doubtful attempt".into());
        };
        assert_eq!(attempt.attempt_id(), self.id.to_text());
        Ok(Arc::clone(attempt))
    }

    async fn outcome(&self, wallet: &NativeWallet) -> TestResult<Option<Outcome>> {
        let read = wallet
            .lend(async |app| app.app_mut().read_claim(&self.obligation).await)
            .await??;
        Ok(read.and_then(|claim| {
            claim
                .view
                .attempt(&self.id.to_bytes())
                .map(|attempt| attempt.outcome)
        }))
    }
}

async fn resolve(world: &World) -> TestResult {
    use ConfirmedResolution::{Failed, Paid, Unknown};
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, wallet, _, _) = Box::pin(enrolled_payer(world, root.path(), account.clone())).await?;
    receive(&wallet, 0).await?;
    let mut shop = Box::pin(shop(world, &wallet)).await?;
    let first = Box::pin(OpenAttempt::on_request(world, &wallet, &mut shop, "first")).await?;
    let second = Box::pin(OpenAttempt::on_request(world, &wallet, &mut shop, "second")).await?;
    world.advance_ms(301_000);
    receive(&wallet, 0).await?;
    let report = reconcile(&wallet).await?;
    assert!(!report.payer_skipped);
    let paid = first.handle(&report)?;
    let uncertain = second.handle(&report)?;

    assert_eq!(first.outcome(&wallet).await?, Some(Outcome::Open));
    wallet.resolve_confirmed_attempt(paid, Paid).await?;
    assert_eq!(first.outcome(&wallet).await?, Some(Outcome::Done));
    assert_eq!(second.outcome(&wallet).await?, Some(Outcome::Open));
    wallet.resolve_confirmed_attempt(uncertain, Unknown).await?;
    assert_eq!(second.outcome(&wallet).await?, Some(Outcome::Unknown));
    let report = reconcile(&wallet).await?;
    assert!(report
        .doubtful
        .iter()
        .all(|row| row.obligation != first.obligation));
    let remaining = second.handle(&report)?;
    wallet
        .resolve_confirmed_attempt(remaining.clone(), Failed)
        .await?;
    assert_eq!(second.outcome(&wallet).await?, Some(Outcome::Failed));
    assert!(reconcile(&wallet).await?.doubtful.is_empty());

    // The failed claim expires after its pinned deadline and reconciliation window (Switch wire 6.8).
    world.advance_ms(31 * DAY_MS);
    receive(&wallet, 0).await?;
    assert_eq!(second.outcome(&wallet).await?, None);
    refused(
        wallet.resolve_confirmed_attempt(remaining, Paid).await,
        "permanent",
        "not_found",
    )?;
    assert_eq!(account.count(), 0);
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_doubtful_attempt_resolves_its_original_claim_until_expiry() {
    run(Backend::Scripted, resolve);
    run(Backend::Service, resolve);
}
