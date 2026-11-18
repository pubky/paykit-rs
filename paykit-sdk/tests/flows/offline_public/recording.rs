use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use paykit_lib::{derive::payment_obligation, AttemptId};
use pubky_switch_client::app::{server_time_in, ClaimOp, ObligationDeadline, OpBatch};
use pubky_switch_client::clock::{Clock, TestClock};
use pubky_switch_client::error::StoreError;
use pubky_switch_client::store::{
    AcquireOutcome, CommitOutcome, KeyValue, MemoryStore, StateStore, StoreOwner, Write, WriteBatch,
};

use crate::common::World;

struct CommitClock {
    inner: MemoryStore,
    clock: Arc<TestClock>,
    advance: AtomicU64,
}

impl CommitClock {
    fn queues_batch(writes: &WriteBatch) -> bool {
        writes
            .writes()
            .iter()
            .any(|write| matches!(write, Write::Put { key, .. } if key.starts_with(b"opq/")))
    }

    fn after_queue(&self) {
        self.clock
            .advance_ms(self.advance.swap(0, Ordering::SeqCst));
    }
}

impl StateStore for CommitClock {
    fn get(&self, namespace: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get(namespace, key)
    }

    fn scan(
        &self,
        namespace: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        self.inner.scan(namespace, prefix, after, limit)
    }

    fn commit(&self, writes: WriteBatch) -> Result<(), StoreError> {
        let queued = Self::queues_batch(&writes);
        self.inner.commit(writes)?;
        if queued {
            self.after_queue();
        }
        Ok(())
    }

    fn acquire(&self, namespace: &str, owner: StoreOwner) -> Result<AcquireOutcome, StoreError> {
        self.inner.acquire(namespace, owner)
    }

    fn commit_if(
        &self,
        namespace: &str,
        owner: StoreOwner,
        writes: WriteBatch,
    ) -> Result<CommitOutcome, StoreError> {
        let queued = Self::queues_batch(&writes);
        let outcome = self.inner.commit_if(namespace, owner, writes)?;
        if queued && outcome == CommitOutcome::Applied {
            self.after_queue();
        }
        Ok(outcome)
    }
}

async fn record_after_commit(delay: u64) {
    let world = World::new();
    let store = Arc::new(CommitClock {
        inner: MemoryStore::new(),
        clock: Arc::clone(&world.clock),
        advance: AtomicU64::new(0),
    });
    let mut wallet = world.wallet_on("wallet", store.clone()).await;
    let attempt = AttemptId::generate().unwrap();
    let obligation = payment_obligation(&attempt);
    assert!(wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    let start = wallet.app().highest_service_time().unwrap();
    let deadline = start + 600_000;
    let claim = ClaimOp::attempt(obligation, 0, attempt.to_bytes(), 3600, Some(deadline));
    let batch = OpBatch::for_protocol("paykit")
        .op_if(claim.kind(), vec![server_time_in(None, Some(deadline))])
        .obligation_deadline(obligation, ObligationDeadline::At(deadline))
        .first_send_before(deadline);
    assert_eq!(world.clock.now_ms(), start);
    let before = world.router.log().len();
    store.advance.store(delay, Ordering::SeqCst);
    let result = wallet.app_mut().submit_batch(batch).await;
    assert_eq!(world.clock.now_ms(), start + delay);
    let requests = world.router.log();
    let posts = requests[before..]
        .iter()
        .filter(|request| request.starts_with("POST ") && request.ends_with("/ops"))
        .count();
    let observed = wallet.app_mut().read_claim(&obligation).await.unwrap();
    assert_eq!(world.rail.pay_calls(&attempt), 0);
    if delay == 0 {
        assert_eq!(posts, 1);
        assert_eq!(result.unwrap().proofs.len(), 1);
        let view = observed.unwrap().view;
        assert_eq!(view.attempts.len(), 1);
        assert_eq!(view.deadline, Some(deadline));
        assert!(view.attempt(&attempt.to_bytes()).is_some());
    } else {
        assert!(result.is_err());
        assert!(observed.is_none());
        assert_eq!(posts, 0, "an expired first recording must not be submitted");
    }
}

#[tokio::test]
async fn test_offline_public_original_attempt_has_authenticated_application_evidence() {
    record_after_commit(0).await;
}

#[tokio::test]
async fn test_offline_public_first_recording_waits_if_its_deadline_elapsed() {
    record_after_commit(600_000).await;
}

#[tokio::test]
async fn test_offline_public_renews_after_only_candidate_is_retired_unsent() {
    let world = World::new();
    let store = Arc::new(CommitClock {
        inner: MemoryStore::new(),
        clock: world.clock.clone(),
        advance: AtomicU64::new(0),
    });
    let mut wallet = world.wallet_on("wallet", store.clone()).await;
    let _shop = super::shop_with_endpoint(&world).await;
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    let id = wallet
        .pay_without_request(super::payment(&world))
        .await
        .outcome
        .unwrap()
        .attempt_id();
    let obligation = payment_obligation(&id);
    world
        .router
        .add_switch(world.alice.origin(), world.alice.switch());

    let old_cutoff = world.clock.now_ms() + 600_000;
    store.advance.store(600_000, Ordering::SeqCst);
    let before = world.router.log().len();
    wallet.resume_payments().await.unwrap();
    assert_eq!(world.clock.now_ms(), old_cutoff);
    assert!(wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .is_none());
    assert!(world.router.log()[before..]
        .iter()
        .all(|r| !(r.starts_with("POST ") && r.ends_with("/ops"))));

    wallet.resume_payments().await.unwrap();
    let view = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(view.deadline, Some(old_cutoff + 600_000));
    assert_eq!(view.attempts.len(), 1);
    assert_eq!(
        view.attempt(&id.to_bytes()).unwrap().outcome,
        pubky_switch_core::claims::Outcome::Done
    );
    assert_eq!(world.rail.pay_calls(&id), 1);
}

/// Count device reads until dispatch so the changed sample lands on the last
/// one even when the number of earlier checks changes.
struct DispatchClock {
    now: u64,
    rail: Arc<paykit_sdk::rail::FakeRail>,
    change: Option<(usize, i64)>,
    reads: std::sync::Mutex<Option<usize>>,
}

impl Clock for DispatchClock {
    fn now_ms(&self) -> u64 {
        let mut reads = self.reads.lock().unwrap();
        if let Some(count) = reads.as_mut() {
            if self.rail.all_executions().is_empty() {
                *count += 1;
                if let Some((at, delta)) = self.change {
                    // Return to the ordinary time at dispatch after observing a later
                    // sample. The payee and deny-list reads still cover that time.
                    if delta < 0 && *count + 1 == at {
                        return self.now + delta.unsigned_abs();
                    }
                    if delta >= 0 && *count == at {
                        return self.now.checked_add_signed(delta).unwrap();
                    }
                }
            }
        }
        self.now
    }
}

async fn offline_on_last_read(change: Option<(usize, i64)>) -> usize {
    use paykit_sdk::{PayOutcome, Paykit, PaykitConfig, PaykitError, Refusal};

    let world = World::new();
    let saved = crate::common::store();
    let app_id = world.wallet_on("wallet", saved.clone()).await.app_id();
    let _shop = super::shop_with_endpoint(&world).await;
    let clock = Arc::new(DispatchClock {
        now: world.clock.now_ms() + 10,
        rail: world.rail.clone(),
        change,
        reads: std::sync::Mutex::new(None),
    });
    let mut env = world.env(&world.alice, saved);
    env.clock = clock.clone();
    let mut wallet = Paykit::load(
        env,
        PaykitConfig::default(),
        app_id,
        world.rail.clone(),
        world.timer(),
    )
    .await
    .unwrap();
    wallet.receive(None).await.unwrap();
    world.router.remove(world.alice.origin());
    *clock.reads.lock().unwrap() = Some(0);
    let result = wallet
        .pay_without_request(super::payment(&world))
        .await
        .outcome;
    let last = clock.reads.lock().unwrap().take().unwrap();
    let entries = wallet.payments().await.unwrap();
    assert_eq!(entries.len(), 1);
    let id = entries[0].attempt_id;
    match change.map(|(_, delta)| delta) {
        None | Some(59_999) => {
            assert!(
                matches!(result, Ok(PayOutcome::PaidOutcomeUnwritten { attempt_id, .. }) if attempt_id == id),
                "{result:?}"
            );
            assert_eq!(world.rail.pay_calls(&id), 1);
        }
        Some(delta) => {
            assert!(
                matches!(
                    result,
                    Err(PaykitError::Refused(Refusal::IdentityUnavailable(_)))
                ),
                "{result:?}"
            );
            assert_eq!(world.rail.pay_calls(&id), 0);
            assert!(world.rail.all_executions().is_empty());
            if delta < 0 {
                assert_eq!(wallet.app().identity_confirmation_age_ms(), None);
                wallet.app_mut().refresh_identity().await.unwrap();
                assert_eq!(wallet.app().identity_confirmation_age_ms(), Some(0));
                let retried = wallet
                    .pay_without_request(super::payment(&world))
                    .await
                    .outcome
                    .unwrap()
                    .attempt_id();
                assert_ne!(retried, id);
                assert_eq!(world.rail.pay_calls(&retried), 1);
                assert_eq!(world.rail.pay_calls(&id), 0);
            }
        }
    }
    last
}

#[tokio::test]
async fn test_offline_public_own_identity_at_last_read_before_dispatch() {
    let last = offline_on_last_read(None).await;
    assert!(last > 1);
    for delta in [59_999, 60_000, -1] {
        offline_on_last_read(Some((last, delta))).await;
    }
}
