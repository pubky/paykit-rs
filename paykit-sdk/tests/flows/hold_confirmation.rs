//! A confirmation applies only to the request and uncertainty shown to the user
//! (Paykit spec 7.2, "What a restore loses", and 9).

use std::sync::Arc;

use paykit_sdk::{
    AcceptOptions, PayOptions, PayOutcome, Paykit, PaykitBackup, PaykitConfig, Rail, Refusal,
    RequestRef, RequestStatus,
};
use pubky_dispatch_client::app::UserConfirmed;
use pubky_dispatch_client::clock::Clock;

use crate::common::{draft, request_from, store, World};

async fn shown(app: &Paykit, request: &RequestRef) -> paykit_sdk::HeldRequest {
    assert_eq!(
        app.request(request).await.unwrap().status,
        RequestStatus::Held
    );
    crate::common::held(app, request).await
}

async fn missed(w: &World, app: &mut Paykit) {
    let now = w.clock.now_ms();
    w.alice.dispatch().push_missed(1, now - 1_000, now);
    app.receive(None).await.unwrap();
}

async fn restored(w: &World, backup: &PaykitBackup) -> Paykit {
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut app).await;
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    app
}

#[tokio::test]
async fn test_hold_confirmation_payment_rejects_a_second_missed_item_at_the_same_time() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    let at = w.clock.now_ms();
    missed(&w, &mut wallet).await;
    assert_eq!(w.clock.now_ms(), at);
    wallet
        .pay_request(
            &request,
            PayOptions::default().with_possibly_cancelled_confirmed(confirmed),
        )
        .await
        .outcome
        .expect_err("an answer to the earlier hold must not pay");
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"hc/")
        .await
        .unwrap()
        .is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_explicit_rejects_a_second_missed_item_at_the_same_time() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    let at = w.clock.now_ms();
    missed(&w, &mut wallet).await;
    assert_eq!(w.clock.now_ms(), at);
    wallet
        .confirm_possibly_cancelled(&request, &confirmed, UserConfirmed::new())
        .await
        .expect_err("an answer to the earlier hold must not clear the current hold");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"hc/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_subscription_rejects_a_second_missed_item_at_the_same_time() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        crate::periods::monthly(w.clock.now_ms(), None),
    )
    .await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    let at = w.clock.now_ms();
    missed(&w, &mut wallet).await;
    assert_eq!(w.clock.now_ms(), at);
    wallet
        .accept_subscription_with(
            &request,
            AcceptOptions::default().with_possibly_cancelled_confirmed(confirmed),
        )
        .await
        .expect_err("an answer to the earlier hold must not accept a subscription");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"hc/")
        .await
        .unwrap()
        .is_empty());
    shop.receive(None).await.unwrap();
    let activity = shop.payee_activity().await.unwrap();
    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].status, paykit_sdk::PayeeStatus::Requested);
}

#[tokio::test]
async fn test_hold_confirmation_rejects_another_restore_of_the_same_backup() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let first = restored(&w, &backup).await;
    let confirmed = shown(&first, &request).await;
    let at = w.clock.now_ms();
    drop(first);
    let mut second = restored(&w, &backup).await;
    assert_eq!(w.clock.now_ms(), at);
    second
        .confirm_possibly_cancelled(&request, &confirmed, UserConfirmed::new())
        .await
        .expect_err("another restore requires another confirmation");
    assert_eq!(
        second.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(second
        .app()
        .protocol_scan("paykit", b"hc/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_current_payment_pays() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    let result = wallet
        .pay_request(
            &request,
            PayOptions::default().with_possibly_cancelled_confirmed(confirmed),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(result, PayOutcome::Paid { .. }));
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_hold_confirmation_current_explicit_confirmation_clears_hold() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    wallet
        .confirm_possibly_cancelled(&request, &confirmed, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_current_subscription_accepts() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        crate::periods::monthly(w.clock.now_ms(), None),
    )
    .await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    wallet
        .accept_subscription_with(
            &request,
            AcceptOptions::default().with_possibly_cancelled_confirmed(confirmed),
        )
        .await
        .unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Subscribed
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_cannot_confirm_another_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &first).await;
    let other = shown(&wallet, &second).await;
    assert_ne!(confirmed.record(), other.record());
    wallet
        .confirm_possibly_cancelled(&second, &confirmed, UserConfirmed::new())
        .await
        .expect_err("a confirmation must name this request");
    assert_eq!(shown(&wallet, &second).await, other);
    wallet
        .confirm_possibly_cancelled(&second, &other, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        wallet.request(&second).await.unwrap().status,
        RequestStatus::Open
    );
    assert_eq!(shown(&wallet, &first).await, confirmed);
}

async fn reload(
    w: &World,
    app: Paykit,
    storage: Arc<dyn pubky_dispatch_client::store::StateStore>,
) -> Paykit {
    let id = app.app_id();
    drop(app);
    Paykit::load(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn test_hold_confirmation_reload_preserves_the_current_token() {
    let w = World::new();
    let storage = store();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&storage),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_get("paykit", b"he")
        .await
        .unwrap()
        .is_none());
    assert!(wallet.request(&request).await.unwrap().held.is_none());
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    let mut wallet = reload(&w, wallet, storage).await;
    assert_eq!(shown(&wallet, &request).await, confirmed);
    wallet
        .confirm_possibly_cancelled(&request, &confirmed, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

async fn unread_epoch(missing: bool) {
    use pubky_dispatch_client::app::LocalWrite;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    let original = wallet
        .app()
        .protocol_get("paykit", b"he")
        .await
        .unwrap()
        .unwrap();
    let write = if missing {
        LocalWrite::Delete {
            key: b"he".to_vec(),
        }
    } else {
        LocalWrite::Put {
            key: b"he".to_vec(),
            value: br#"{"v":1,"d":false}"#.to_vec(),
        }
    };
    wallet
        .app_mut()
        .protocol_commit("paykit", &[write])
        .await
        .unwrap();
    wallet
        .request(&request)
        .await
        .expect_err("an unread hold identity cannot be shown as confirmable");
    wallet
        .pay_request(
            &request,
            PayOptions::default().with_possibly_cancelled_confirmed(confirmed.clone()),
        )
        .await
        .outcome
        .expect_err("an unread hold identity cannot authorize payment");
    wallet
        .confirm_possibly_cancelled(&request, &confirmed, UserConfirmed::new())
        .await
        .expect_err("an unread hold identity cannot be confirmed");
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"hc/")
        .await
        .unwrap()
        .is_empty());
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: b"he".to_vec(),
                value: original,
            }],
        )
        .await
        .unwrap();
    assert_eq!(shown(&wallet, &request).await, confirmed);
}

#[tokio::test]
async fn test_hold_confirmation_missing_epoch_refuses() {
    Box::pin(unread_epoch(true)).await;
}

#[tokio::test]
async fn test_hold_confirmation_malformed_epoch_refuses() {
    Box::pin(unread_epoch(false)).await;
}

#[tokio::test]
async fn test_hold_confirmation_cancellation_overrides_a_current_token() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    let record = shop.payee_activity().await.unwrap()[0].record;
    shop.cancel_sent_request(&record, None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let error = wallet
        .pay_request(
            &request,
            PayOptions::default().with_possibly_cancelled_confirmed(confirmed),
        )
        .await
        .outcome
        .unwrap_err();
    assert_eq!(error.refusal(), Some(&paykit_sdk::Refusal::Cancelled));
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    assert!(w.rail.all_executions().is_empty());
}

async fn request_without_expiry(w: &World, shop: &mut Paykit) -> RequestRef {
    let mut order = draft("1", w.clock.now_ms());
    order.proposal_expires_at_ms = None;
    assert!(order.payment_deadline_ms.is_none());
    request_from(shop, w.alice_id(), order).await
}

#[tokio::test]
async fn test_hold_confirmation_paid_request_stays_unpayable_after_deletion_and_claim_expiry() {
    let w = World::new();
    let mut payer = w.wallet("payer").await;
    let mut shop = w.shop().await;
    let request = request_without_expiry(&w, &mut shop).await;
    payer.receive(None).await.unwrap();
    missed(&w, &mut payer).await;
    let consent = shown(&payer, &request).await;
    let options = PayOptions::default().with_possibly_cancelled_confirmed(consent);
    let payment = payer
        .pay_request(&request, options.clone())
        .await
        .outcome
        .unwrap();
    assert!(matches!(payment, PayOutcome::Paid { .. }));
    let obligation = payer.payments().await.unwrap()[0].obligation;
    let calls = w.rail.all_pay_calls();
    assert_eq!(calls.get(&payment.attempt_id()), Some(&1));
    let target = crate::outcome_record::payer_record(&request);
    payer
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    w.clock
        .advance_ms(401 * pubky_dispatch_core::records::DAY_MS);
    let mut later = w.wallet("later").await;
    assert!(later
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    w.rail.next(paykit_sdk::rail::Behaviour::Refuse);
    let error = later
        .pay_request(&request, options)
        .await
        .outcome
        .unwrap_err();
    assert_eq!(error.refusal(), Some(&paykit_sdk::Refusal::NotFound));
    let held = later
        .app()
        .shared_record(&later.app().record_key(&target).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        held.content,
        pubky_dispatch_client::app::RecordContent::Deleted
    );
    assert!(later.payments().await.unwrap().is_empty());
    assert_eq!(w.rail.all_pay_calls(), calls);
}

#[tokio::test]
async fn test_hold_confirmation_signed_payer_loss_outlives_consent_to_a_missed_item() {
    use paykit_lib::records::{ActivityRecord, ActivitySide};

    let w = World::new();
    let mut payer = w.wallet("payer").await;
    let mut shop = w.shop().await;
    let request = request_without_expiry(&w, &mut shop).await;
    payer.receive(None).await.unwrap();
    missed(&w, &mut payer).await;
    let consent = shown(&payer, &request).await;
    payer
        .confirm_possibly_cancelled(&request, &consent, UserConfirmed::new())
        .await
        .unwrap();
    missed(&w, &mut payer).await;
    let current = shown(&payer, &request).await;
    assert_ne!(consent, current);
    let target = crate::outcome_record::payer_record(&request);
    let key = payer.app().record_key(&target).unwrap();
    let original = payer.app().shared_record(&key).await.unwrap().unwrap();
    let mut record = ActivityRecord::read(
        original.content.value().unwrap(),
        &w.alice_id(),
        ActivitySide::Payer,
    )
    .unwrap();
    // Check the reader of a signed marked value independently of how it was published.
    record.lost_to_delete = Some([73; 32]);
    payer
        .app_mut()
        .put_record(&target, &record.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    let view = payer.request(&request).await.unwrap();
    assert_eq!(view.status, RequestStatus::MayBePaid);
    assert!(view.held.is_none());
    let prior_consent = payer.app().protocol_scan("paykit", b"hc/").await.unwrap();
    for token in [&consent, &current] {
        let refused = payer
            .confirm_possibly_cancelled(&request, token, UserConfirmed::new())
            .await
            .unwrap_err();
        assert!(matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::RecordHeld(_))
        ));
    }
    assert_eq!(
        payer.app().protocol_scan("paykit", b"hc/").await.unwrap(),
        prior_consent
    );
    w.rail.next(paykit_sdk::rail::Behaviour::Refuse);
    let error = payer
        .pay_request(
            &request,
            PayOptions::default().with_possibly_cancelled_confirmed(consent),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(matches!(
        error.refusal(),
        Some(paykit_sdk::Refusal::RecordHeld(_))
    ));
    assert!(payer.payments().await.unwrap().is_empty());
    assert!(w.rail.all_pay_calls().is_empty());

    let mut later = record.clone();
    later.lost_to_delete = Some([74; 32]);
    record.check_later_write(&later).unwrap();
    later.lost_to_delete = None;
    assert!(record.check_later_write(&later).is_err());
}

#[derive(Default)]
struct HoldCommitStore {
    inner: pubky_dispatch_client::store::MemoryStore,
    fault: std::sync::atomic::AtomicU8,
    hit: std::sync::atomic::AtomicBool,
}

impl pubky_dispatch_client::store::StateStore for HoldCommitStore {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_dispatch_client::error::StoreError> {
        self.inner.get(ns, key)
    }
    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_dispatch_client::store::KeyValue>, pubky_dispatch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }
    fn commit(
        &self,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> Result<(), pubky_dispatch_client::error::StoreError> {
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_dispatch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_dispatch_client::store::StoreOwner,
    ) -> Result<
        pubky_dispatch_client::store::AcquireOutcome,
        pubky_dispatch_client::error::StoreError,
    > {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_dispatch_client::store::StoreOwner,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> Result<pubky_dispatch_client::store::CommitOutcome, pubky_dispatch_client::error::StoreError>
    {
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl HoldCommitStore {
    fn commit_with(
        &self,
        batch: pubky_dispatch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_dispatch_client::store::WriteBatch,
        ) -> Result<
            pubky_dispatch_client::store::CommitOutcome,
            pubky_dispatch_client::error::StoreError,
        >,
    ) -> Result<pubky_dispatch_client::store::CommitOutcome, pubky_dispatch_client::error::StoreError>
    {
        use pubky_dispatch_client::{error::StoreError, store::Write};
        use std::sync::atomic::Ordering;
        let hold = batch.writes().iter().any(|write| matches!(write, Write::Put { key, .. } if key == b"p/paykit/rh" || key == b"p/paykit/mh" || key == b"p/paykit/mu"));
        let fault = if hold {
            self.fault.swap(0, Ordering::SeqCst)
        } else {
            0
        };
        if fault != 0 {
            self.hit.store(true, Ordering::SeqCst);
        }
        if fault == 1 {
            return Err(StoreError::Unavailable);
        }
        let applied = apply(batch)?;
        if applied != pubky_dispatch_client::store::CommitOutcome::Applied {
            return Ok(applied);
        }
        if fault == 2 {
            return Err(StoreError::Unknown);
        }
        Ok(pubky_dispatch_client::store::CommitOutcome::Applied)
    }
}

async fn missed_commit(after: bool) {
    use pubky_dispatch_client::store::{client_namespace, StateStore};
    use std::sync::atomic::Ordering;
    let w = World::new();
    let storage = Arc::new(HoldCommitStore::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&storage) as Arc<dyn StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    wallet
        .confirm_possibly_cancelled(&request, &confirmed, UserConfirmed::new())
        .await
        .unwrap();
    let ns = client_namespace(&wallet.app_id());
    let epoch = storage.get(&ns, b"p/paykit/he").unwrap().unwrap();
    assert_eq!(
        storage.scan(&ns, b"p/paykit/hc/", None, 10).unwrap().len(),
        1
    );
    storage
        .fault
        .store(if after { 2 } else { 1 }, Ordering::SeqCst);
    let now = w.clock.now_ms();
    w.alice.dispatch().push_missed(1, now - 1_000, now);
    wallet
        .receive(None)
        .await
        .expect_err("the injected commit failure is reported");
    assert!(storage.hit.load(Ordering::SeqCst));
    let actual = storage.get(&ns, b"p/paykit/he").unwrap().unwrap();
    if after {
        assert_ne!(actual, epoch);
    } else {
        assert_eq!(actual, epoch);
    }
    assert_eq!(
        storage.scan(&ns, b"p/paykit/hc/", None, 10).unwrap().len(),
        usize::from(!after)
    );
    let mut wallet = reload(&w, wallet, Arc::clone(&storage) as Arc<dyn StateStore>).await;
    wallet.receive(None).await.unwrap();
    let current = shown(&wallet, &request).await;
    assert_ne!(current, confirmed);
    wallet
        .confirm_possibly_cancelled(&request, &confirmed, UserConfirmed::new())
        .await
        .expect_err("the retried missed item requires a new confirmation");
    wallet
        .confirm_possibly_cancelled(&request, &current, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_missed_commit_refusal_keeps_the_previous_epoch() {
    Box::pin(missed_commit(false)).await;
}

#[tokio::test]
async fn test_hold_confirmation_missed_lost_commit_answer_keeps_the_new_epoch() {
    Box::pin(missed_commit(true)).await;
}

async fn restore_commit(after: bool) {
    use pubky_dispatch_client::store::{client_namespace, StateStore};
    use std::sync::atomic::Ordering;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    let id = wallet.app_id();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let storage = Arc::new(HoldCommitStore::default());
    storage
        .fault
        .store(if after { 2 } else { 1 }, Ordering::SeqCst);
    let result = Paykit::restore(
        w.env(&w.alice, Arc::clone(&storage) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    assert!(
        result.is_err(),
        "the injected restore commit failure is reported"
    );
    assert!(storage.hit.load(Ordering::SeqCst));
    let ns = client_namespace(&id);
    assert_eq!(storage.get(&ns, b"p/paykit/rh").unwrap().is_some(), after);
    assert_eq!(storage.get(&ns, b"p/paykit/he").unwrap().is_some(), after);
    let mut loaded = if after {
        Paykit::load(
            w.env(&w.alice, Arc::clone(&storage) as Arc<dyn StateStore>),
            PaykitConfig::default(),
            id,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .unwrap()
    } else {
        Paykit::restore(
            w.env(&w.alice, Arc::clone(&storage) as Arc<dyn StateStore>),
            PaykitConfig::default(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .unwrap()
    };
    loaded.reconcile().await.unwrap();
    loaded.end_reconcile(UserConfirmed::new()).await.unwrap();
    let current = shown(&loaded, &request).await;
    assert_ne!(current, confirmed);
    loaded
        .confirm_possibly_cancelled(&request, &confirmed, UserConfirmed::new())
        .await
        .expect_err("the restored hold has its own identity");
    loaded
        .confirm_possibly_cancelled(&request, &current, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        loaded.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_restore_commit_refusal_installs_no_epoch() {
    Box::pin(restore_commit(false)).await;
}

#[tokio::test]
async fn test_hold_confirmation_restore_lost_commit_answer_retains_the_epoch() {
    Box::pin(restore_commit(true)).await;
}

/// The wallet confirms a request held by a `missed` item and pays, but the attempt's batch
/// goes unanswered and is kept. A second `missed` item is read before the kept batch is sent
/// again, so the batch's confirmation is applied under the newer hold.
async fn confirmation_applied_after_a_newer_hold(w: &World) -> (Paykit, RequestRef) {
    Box::pin(kept_confirmation_lands(w, false)).await
}

/// [`confirmation_applied_after_a_newer_hold`], the user confirming the newer hold before the
/// kept batch lands when `confirm_newer`.
async fn kept_confirmation_lands(w: &World, confirm_newer: bool) -> (Paykit, RequestRef) {
    use pubky_dispatch_client::testing::Fault;
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    w.alice.dispatch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    wallet
        .pay_request(
            &request,
            PayOptions::default().with_possibly_cancelled_confirmed(confirmed),
        )
        .await
        .outcome
        .expect_err("the attempt's batch is unanswered");
    assert!(w.rail.all_executions().is_empty());
    w.clock.advance_ms(1_000);
    w.alice.dispatch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    missed(w, &mut wallet).await;
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"hc/")
        .await
        .unwrap()
        .is_empty());
    if confirm_newer {
        let newer = shown(&wallet, &request).await;
        wallet
            .confirm_possibly_cancelled(&request, &newer, UserConfirmed::new())
            .await
            .unwrap();
    }
    // The kept batch is sent again and applied, its confirmation with it.
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"hc/")
            .await
            .unwrap()
            .len(),
        1 + usize::from(confirm_newer),
        "the kept batch's confirmation is applied after the newer hold"
    );
    (wallet, request)
}

#[tokio::test]
async fn test_hold_confirmation_applied_after_a_newer_hold_confirms_nothing() {
    let w = World::new();
    let (mut wallet, request) = confirmation_applied_after_a_newer_hold(&w).await;
    let view = wallet.request(&request).await.unwrap();
    assert_ne!(view.status, RequestStatus::Paid, "{view:?}");
    let held = view.held.expect("the newer hold stands");
    wallet.resume_payments().await.unwrap();
    assert!(w.rail.all_executions().is_empty());
    let refused = wallet
        .pay_request_automatically(&request, PayOptions::default())
        .await
        .outcome
        .expect_err("the newer hold holds automatic payment");
    assert!(
        matches!(refused.refusal(), Some(Refusal::HeldAfterRestore { .. })),
        "{refused:?}"
    );
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .expect_err("the newer hold still needs the user's confirmation");
    assert!(
        matches!(refused.refusal(), Some(Refusal::HeldAfterRestore { .. })),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let result = wallet
        .pay_request(
            &request,
            PayOptions::default().with_possibly_cancelled_confirmed(held),
        )
        .await
        .outcome
        .unwrap();
    assert!(matches!(result, PayOutcome::Paid { .. }), "{result:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_hold_confirmation_applied_after_a_newer_hold_is_not_carried_by_a_restore() {
    let w = World::new();
    let (mut wallet, request) = confirmation_applied_after_a_newer_hold(&w).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let restored = restored(&w, &backup).await;
    assert!(restored
        .app()
        .protocol_scan("paykit", b"hb/")
        .await
        .unwrap()
        .is_empty());
    shown(&restored, &request).await;
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_applied_after_a_newer_hold_stays_held_after_a_clean_restore() {
    // A clean restore lifts its own hold, so the `missed` item's hold the backup carries is
    // the only one: the late confirmation in the backup must not release it.
    let w = World::new();
    let (mut wallet, request) = confirmation_applied_after_a_newer_hold(&w).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(
        restored.app().restore_held_cursor(),
        Some(pubky_dispatch_client::app::HeldCursor::Clean)
    );
    restored.reconcile().await.unwrap();
    restored.end_reconcile(UserConfirmed::new()).await.unwrap();
    assert!(restored.request(&request).await.unwrap().held.is_some());
    for automatic in [false, true] {
        let refused = if automatic {
            restored
                .pay_request_automatically(&request, PayOptions::default())
                .await
        } else {
            restored.pay_request(&request, PayOptions::default()).await
        }
        .outcome
        .unwrap_err();
        assert!(
            matches!(refused.refusal(), Some(Refusal::HeldAfterRestore { .. })),
            "{refused:?}"
        );
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_kept_subscription_acceptance_confirms_no_newer_hold() {
    // The user accepts a held subscription, its batch unanswered and kept; a second
    // `missed` item is read before the batch is applied. The acceptance stands, but its
    // confirmation is of the earlier hold: no Billing Period is paid automatically.
    use pubky_dispatch_client::testing::Fault;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        crate::periods::monthly(w.clock.now_ms(), None),
    )
    .await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    let confirmed = shown(&wallet, &request).await;
    w.alice.dispatch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    let sent = wallet
        .accept_subscription_with(
            &request,
            AcceptOptions::default().with_possibly_cancelled_confirmed(confirmed),
        )
        .await
        .unwrap();
    assert!(
        matches!(sent.delivery, paykit_sdk::MessageDelivery::Kept { .. }),
        "the acceptance's batch is unanswered and kept: {sent:?}"
    );
    w.clock.advance_ms(1_000);
    w.alice.dispatch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    missed(&w, &mut wallet).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet
            .app()
            .protocol_scan("paykit", b"hc/")
            .await
            .unwrap()
            .len(),
        1,
        "the kept batch's confirmation is applied after the newer hold"
    );
    assert!(wallet.request(&request).await.unwrap().held.is_some());
    let refused = wallet
        .pay_period_automatically(
            &request,
            &crate::periods::january(),
            paykit_sdk::PeriodPrice::RequestedAmount,
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::HeldAfterRestore { .. })),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_hold_confirmation_backup_with_a_malformed_hold_identifier_is_refused() {
    use pubky_dispatch_client::app::LocalWrite;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w, &mut wallet).await;
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: b"he".to_vec(),
                value: br#"{"v":1,"d":false}"#.to_vec(),
            }],
        )
        .await
        .unwrap();
    // The backup's own check, the one a restore runs, refuses the layer.
    let refused = wallet.backup().await.unwrap_err();
    assert!(
        format!("{refused:?}").contains("the backup's hold identifier cannot be read"),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_hold_confirmation_late_write_keeps_the_newer_holds_confirmation() {
    // The user confirms the newer hold before the kept batch of the earlier one lands: the
    // late write lands under its own hold and leaves the newer confirmation in place, so the
    // request isn't held and a payment by hand needs no further confirmation. (The user's own
    // attempt marked the request manual-only, so no automatic payment applies to it.)
    let w = World::new();
    let (mut wallet, request) = Box::pin(kept_confirmation_lands(&w, true)).await;
    let view = wallet.request(&request).await.unwrap();
    assert!(view.held.is_none(), "{view:?}");
    if let Err(error) = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
    {
        assert!(
            !matches!(error.refusal(), Some(Refusal::HeldAfterRestore { .. })),
            "{error:?}"
        );
    }
}
