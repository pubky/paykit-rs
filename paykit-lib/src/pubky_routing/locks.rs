use std::{future::Future, sync::Mutex, task::Poll, time::Duration};

use pubky::{PubkySession, StorageLock};
use tokio::sync::Notify;

use crate::{PaykitError, Result};

use super::{is_not_found, read_response_revision};

const WRITE_LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_ACQUISITION_MAX_ATTEMPTS: u64 = 8;

#[derive(Debug, thiserror::Error)]
#[error("resource changed since it was read")]
struct ResourceChanged;

#[derive(Debug, thiserror::Error)]
#[error("write lock renewal failed; the operation may have committed")]
struct LockRenewalError(#[source] anyhow::Error);

/// Whether an operation must reread its resource before retrying a write.
pub fn is_write_conflict(error: &PaykitError) -> bool {
    matches!(error, PaykitError::Transport { source, .. }
    if source.is::<ResourceChanged>() || matches!(
        source.downcast_ref::<pubky::Error>(),
        Some(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
            if *status == pubky::StatusCode::LOCKED
                || *status == pubky::StatusCode::PRECONDITION_FAILED
                || *status == pubky::StatusCode::CONFLICT
    ))
}

/// Replace a resource after checking its content revision under a write lock.
///
/// `None` requires an absent resource. A revision comes from
/// [`crate::content_revision`] of the exact previously read bytes, not HTTP metadata.
/// Reads are bounded by `max_bytes`. Session capabilities, key rotation, and
/// request timeouts remain the caller's responsibility.
pub async fn put_resource_if_revision(
    session: &PubkySession,
    path: &str,
    bytes: Vec<u8>,
    expected_revision: Option<&str>,
    max_bytes: usize,
) -> Result<()> {
    with_write_lock(session, path, |lock| async move {
        let current = resource_revision(session, path, max_bytes).await?;
        if current.as_deref() != expected_revision {
            return Err(resource_changed());
        }
        session
            .storage()
            .put_locked(&lock, bytes)
            .await
            .map_err(|error| lock_error("replace Pubky resource", error))?;
        Ok(())
    })
    .await
}

/// Remove a resource only if its content revision still matches under a lock.
///
/// An absent resource is already removed. Session capabilities, key rotation,
/// and request timeouts remain the caller's responsibility.
pub async fn delete_resource_if_revision(
    session: &PubkySession,
    path: &str,
    expected_revision: &str,
    max_bytes: usize,
) -> Result<()> {
    with_write_lock(session, path, |lock| async move {
        let Some(current) = resource_revision(session, path, max_bytes).await? else {
            return Ok(());
        };
        if current != expected_revision {
            return Err(resource_changed());
        }
        session
            .storage()
            .delete_locked(&lock)
            .await
            .map_err(|error| lock_error("delete Pubky resource", error))?;
        Ok(())
    })
    .await
}

pub(super) async fn resource_revision(
    session: &PubkySession,
    path: &str,
    max_bytes: usize,
) -> Result<Option<String>> {
    let response = match session.storage().get(path).await {
        Ok(response) => response,
        Err(error) if is_not_found(&error) => return Ok(None),
        Err(error) => return Err(lock_error("read Pubky resource revision", error)),
    };
    read_response_revision(response, max_bytes, "read Pubky resource revision")
        .await
        .map(Some)
}

fn resource_changed() -> PaykitError {
    PaykitError::Transport {
        context: "Pubky resource changed; reload it before writing".into(),
        source: ResourceChanged.into(),
    }
}

/// Run a Pubky read-modify-write operation under an exclusive file lock.
///
/// Read the current contents inside `operation` and use the supplied lock for
/// every mutation. The lock is renewed while the operation runs and released
/// afterward. Transient renewal failures are retried within the last confirmed
/// lease lifetime. Cancellation leaves it to expire. Loss of the lock stops the
/// operation with an uncertain outcome, not a retryable write conflict: a PUT
/// may already have committed. Callers must reconcile before retrying.
/// Contended acquisition is retried with bounded backoff before `operation`
/// runs. The operation itself is never replayed.
///
/// Homeserver 0.16.2 with GCS fences stale publications by object generation.
/// A delayed write may still land before this holder's first publication; a
/// conflict requires a fresh read, and uncertain shared state needs a confirmed
/// replacement before use. Session creation, capability scope, key rotation,
/// and request timeouts remain the caller's responsibility.
pub async fn with_write_lock<T, E, F, Fut>(
    session: &PubkySession,
    path: &str,
    operation: F,
) -> std::result::Result<T, E>
where
    E: From<PaykitError>,
    F: FnOnce(StorageLock) -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    with_write_lock_timeout(session, path, WRITE_LOCK_TIMEOUT, operation).await
}

pub(crate) async fn with_write_lock_timeout<T, E, F, Fut>(
    session: &PubkySession,
    path: &str,
    timeout: Duration,
    operation: F,
) -> std::result::Result<T, E>
where
    E: From<PaykitError>,
    F: FnOnce(StorageLock) -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    with_write_lock_clock(session, path, timeout, operation, lease_time).await
}

pub(crate) async fn with_write_lock_clock<T, E, F, Fut>(
    session: &PubkySession,
    path: &str,
    timeout: Duration,
    operation: F,
    now: impl Fn() -> std::io::Result<Duration> + Sync,
) -> std::result::Result<T, E>
where
    E: From<PaykitError>,
    F: FnOnce(StorageLock) -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    let storage = session.storage();
    let mut attempt = 1;
    let (lock, acquired_at, lease_started_at) = loop {
        let requested_at = tokio::time::Instant::now();
        let lease_started_at = now().map_err(|error| E::from(lost_lock(error.into())))?;
        match storage.lock(path, timeout).await {
            Ok(lock) => break (lock, requested_at, lease_started_at),
            Err(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
                if status == pubky::StatusCode::LOCKED
                    && attempt < LOCK_ACQUISITION_MAX_ATTEMPTS =>
            {
                tokio::time::sleep(Duration::from_millis(50 * attempt)).await;
                attempt += 1;
            }
            Err(source) => return Err(E::from(lock_error("acquire Pubky write lock", source))),
        }
    };
    let renewal_wakeup = Notify::new();
    let prepared = (|| -> Result<_> {
        let valid_until = confirmed_lease_deadline(acquired_at, lock.timeout())?;
        let mut deadline =
            LeaseDeadline::new(lease_started_at, lock.timeout()).map_err(lost_lock)?;
        if deadline
            .check_and_notify(now().map_err(|error| lost_lock(error.into()))?)
            .map_err(lost_lock)?
        {
            renewal_wakeup.notify_one();
        }
        Ok((valid_until, Mutex::new(deadline)))
    })();
    let (valid_until, lease) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            // No operation started. Release only this acquired token without hiding the validation error.
            release_rejected_lock(session, &lock).await;
            return Err(E::from(error));
        }
    };
    let check_lease = || -> anyhow::Result<()> {
        let should_renew = {
            let mut deadline = lease
                .lock()
                .map_err(|_| anyhow::anyhow!("Pubky write lock clock state unavailable"))?;
            deadline.check_and_notify(now()?)?
        };
        if should_renew {
            renewal_wakeup.notify_one();
        }
        Ok(())
    };
    let mut renewed = lock.clone();
    let renewal = async {
        let mut valid_until = valid_until;
        loop {
            if let Err(error) = wait_for_renewal(&lease, valid_until, &now, &renewal_wakeup).await {
                return error;
            }
            loop {
                let requested_at = tokio::time::Instant::now();
                let lease_started_at = match now() {
                    Ok(value) => value,
                    Err(error) => return error.into(),
                };
                let attempt_deadline = match refresh_deadline(&lease, valid_until, lease_started_at)
                {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                match tokio::time::timeout_at(
                    attempt_deadline,
                    storage.refresh_lock(&mut renewed, timeout),
                )
                .await
                {
                    Ok(Ok(())) if tokio::time::Instant::now() < attempt_deadline => {
                        // A response cannot revive a lease that expired while the device slept.
                        if let Err(error) = check_lease() {
                            return error;
                        }
                        let next = match LeaseDeadline::new(lease_started_at, renewed.timeout()) {
                            Ok(value) => value,
                            Err(error) => return error,
                        };
                        let current_time = match now() {
                            Ok(value) => value,
                            Err(error) => return error.into(),
                        };
                        if let Err(error) = next.check(current_time) {
                            return error;
                        }
                        let next_valid_until =
                            match confirmed_lease_deadline(requested_at, renewed.timeout()) {
                                Ok(deadline) => deadline,
                                Err(error) => return anyhow::Error::from(error),
                            };
                        match lease.lock() {
                            Ok(mut deadline) => *deadline = next,
                            Err(_) => {
                                return anyhow::anyhow!("Pubky write lock clock state unavailable")
                            }
                        }
                        valid_until = next_valid_until;
                        break;
                    }
                    Ok(Err(error)) if retryable_lock_refresh(&error) => {
                        let current_time = match now() {
                            Ok(value) => value,
                            Err(error) => return error.into(),
                        };
                        let deadline = match refresh_deadline(&lease, valid_until, current_time) {
                            Ok(value) => value,
                            Err(error) => return error,
                        };
                        let Some(retry_at) =
                            tokio::time::Instant::now().checked_add(Duration::from_secs(1))
                        else {
                            return anyhow::anyhow!("Pubky write lock deadline overflow");
                        };
                        if retry_at >= deadline {
                            return error.into();
                        }
                        tokio::time::sleep_until(retry_at).await;
                    }
                    Ok(Err(error)) => return anyhow::Error::from(error),
                    Ok(Ok(())) | Err(_) => {
                        return anyhow::anyhow!(
                            "Pubky write lock expired before renewal was confirmed"
                        )
                    }
                }
            }
        }
    };
    let mut operation_started = false;
    let work = async {
        tokio::select! {
            biased;
            error = renewal => Err(error),
            result = Box::pin(async {
                operation_started = true;
                operation(lock.clone()).await
            }) => Ok(result),
        }
    };
    // Tokio's scheduling clock can stop during native device sleep. Check the
    // suspend-inclusive lease before every resumed poll, even if renewal's sleep is pending.
    let result = match poll_with_lease(work, check_lease).await {
        Ok(result) => result,
        Err(error) => {
            if !operation_started {
                release_rejected_lock(session, &lock).await;
            }
            return Err(E::from(lost_lock(error)));
        }
    };
    // Releasing the lock does not undo a committed operation. A failed unlock
    // leaves a bounded lease, not a reason to repeat a successful mutation.
    if let Err(error) = storage.unlock(&lock).await {
        tracing::warn!(
            "{}",
            lock_failure_context(
                "Failed to release Pubky write lock; waiting for expiry",
                &error
            )
        );
    }
    result
}

async fn release_rejected_lock(session: &PubkySession, lock: &StorageLock) {
    if let Err(error) = session.storage().unlock(lock).await {
        tracing::warn!(
            "{}",
            lock_failure_context(
                "Failed to release rejected Pubky write lock; waiting for expiry",
                &error
            )
        );
    }
}

fn poll_with_lease<T>(
    work: impl Future<Output = anyhow::Result<T>>,
    check: impl Fn() -> anyhow::Result<()>,
) -> impl Future<Output = anyhow::Result<T>> {
    // Allocate before returning the future so callers do not embed the lease state machine.
    let mut work = Box::pin(work);
    std::future::poll_fn(move |cx| {
        if let Err(error) = check() {
            return Poll::Ready(Err(error));
        }
        work.as_mut().poll(cx)
    })
}

fn lost_lock(error: anyhow::Error) -> PaykitError {
    PaykitError::Transport {
        context: match error.downcast_ref::<pubky::Error>() {
            Some(source) => lock_failure_context(
                "Pubky write lock lost; operation outcome is uncertain",
                source,
            ),
            None => "Pubky write lock lost; operation outcome is uncertain (lease expired or clock unavailable)".into(),
        },
        source: LockRenewalError(error).into(),
    }
}

// The server rounds timestamps to whole seconds. Reserve that quantization
// budget plus headroom; a local deadline never replaces server-token fencing.
fn safe_lease_lifetime(timeout: Duration) -> anyhow::Result<Duration> {
    if timeout > WRITE_LOCK_TIMEOUT {
        anyhow::bail!("Pubky write lock grant exceeds the supported maximum");
    }
    timeout
        .checked_sub(Duration::from_secs(2).max(timeout / 10))
        .filter(|lifetime| !lifetime.is_zero())
        .ok_or_else(|| anyhow::anyhow!("Pubky write lock grant has no safe lifetime"))
}

#[derive(Clone, Copy)]
struct LeaseDeadline {
    started_at: Duration,
    renew_at: Duration,
    expires_at: Duration,
    renewal_notified: bool,
}

impl LeaseDeadline {
    fn new(started_at: Duration, timeout: Duration) -> anyhow::Result<Self> {
        let lifetime = safe_lease_lifetime(timeout)?;
        Ok(Self {
            started_at,
            renew_at: started_at
                .checked_add(lifetime / 3)
                .ok_or_else(|| anyhow::anyhow!("Pubky write lock deadline overflow"))?,
            expires_at: started_at
                .checked_add(lifetime)
                .ok_or_else(|| anyhow::anyhow!("Pubky write lock deadline overflow"))?,
            renewal_notified: false,
        })
    }

    fn check(self, now: Duration) -> anyhow::Result<()> {
        if now < self.started_at || now >= self.expires_at {
            anyhow::bail!("Pubky write lock expired or its clock moved backwards");
        }
        Ok(())
    }

    fn check_and_notify(&mut self, now: Duration) -> anyhow::Result<bool> {
        self.check(now)?;
        if now < self.renew_at || self.renewal_notified {
            return Ok(false);
        }
        self.renewal_notified = true;
        Ok(true)
    }
}

async fn wait_for_renewal(
    lease: &Mutex<LeaseDeadline>,
    valid_until: tokio::time::Instant,
    now: &impl Fn() -> std::io::Result<Duration>,
    wakeup: &Notify,
) -> anyhow::Result<()> {
    let remaining_after_renewal = {
        let deadline = lease
            .lock()
            .map_err(|_| anyhow::anyhow!("Pubky write lock clock state unavailable"))?;
        deadline.expires_at - deadline.renew_at
    };
    let renew_at = valid_until
        .checked_sub(remaining_after_renewal)
        .ok_or_else(|| anyhow::anyhow!("Pubky write lock deadline overflow"))?;
    // A suspended runtime cannot execute renewal. Once it runs again, silent
    // operations still need a bounded native-clock check without an operation wake.
    let interval = Duration::from_secs(1);
    let first_check = tokio::time::Instant::now()
        .checked_add(interval)
        .ok_or_else(|| anyhow::anyhow!("Pubky write lock deadline overflow"))?;
    let mut clock_checks = tokio::time::interval_at(first_check, interval);
    clock_checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            // Either monotonic clock may make renewal due. A ready normal timer
            // is sufficient; it must not spin waiting for a later native reading.
            _ = tokio::time::sleep_until(renew_at) => return Ok(()),
            _ = clock_checks.tick() => {},
            _ = wakeup.notified() => {},
        }
        let current_time = now()?;
        let deadline = lease
            .lock()
            .map_err(|_| anyhow::anyhow!("Pubky write lock clock state unavailable"))?;
        deadline.check(current_time)?;
        if current_time >= deadline.renew_at {
            return Ok(());
        }
        // A prior generation's stored permit cannot trigger another refresh.
    }
}

fn refresh_deadline(
    lease: &Mutex<LeaseDeadline>,
    valid_until: tokio::time::Instant,
    now: Duration,
) -> anyhow::Result<tokio::time::Instant> {
    let remaining = {
        let deadline = lease
            .lock()
            .map_err(|_| anyhow::anyhow!("Pubky write lock clock state unavailable"))?;
        deadline.check(now)?;
        deadline.expires_at - now
    };
    let scheduler_now = tokio::time::Instant::now();
    if scheduler_now >= valid_until {
        anyhow::bail!("Pubky write lock expired before renewal was confirmed");
    }
    scheduler_now
        .checked_add(remaining)
        .map(|deadline| deadline.min(valid_until))
        .ok_or_else(|| anyhow::anyhow!("Pubky write lock deadline overflow"))
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn lease_time() -> std::io::Result<Duration> {
    #[cfg(target_vendor = "apple")]
    let clock = libc::CLOCK_MONOTONIC_RAW;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let clock = libc::CLOCK_BOOTTIME;
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Both clocks advance during system sleep and are independent of wall-clock adjustments.
    // SAFETY: value points to a valid writable timespec; clock is a platform-defined clock ID.
    if unsafe { libc::clock_gettime(clock, &mut value) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(std::io::Error::other("invalid lease clock reading"));
    }
    Ok(Duration::new(value.tv_sec as u64, value.tv_nsec as u32))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn lease_time() -> std::io::Result<Duration> {
    // Preserve the existing monotonic behavior on other targets. The explicit
    // suspend-inclusive guarantee applies to the native platforms mapped above.
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    Ok(START.get_or_init(std::time::Instant::now).elapsed())
}

fn confirmed_lease_deadline(
    requested_at: tokio::time::Instant,
    timeout: Duration,
) -> Result<tokio::time::Instant> {
    let lifetime = safe_lease_lifetime(timeout).map_err(lost_lock)?;
    let deadline = requested_at
        .checked_add(lifetime)
        .ok_or_else(|| lost_lock(anyhow::anyhow!("Pubky write lock deadline overflow")))?;
    if tokio::time::Instant::now() >= deadline {
        return Err(PaykitError::Transport {
            context: "Pubky write lock response arrived after its safe lease expired".into(),
            source: anyhow::anyhow!("expired Pubky write lock response"),
        });
    }
    Ok(deadline)
}

fn retryable_lock_refresh(error: &pubky::Error) -> bool {
    match error {
        pubky::Error::Request(pubky::errors::RequestError::Transport(_)) => true,
        pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }) => {
            status.is_server_error() || *status == pubky::StatusCode::TOO_MANY_REQUESTS
        }
        _ => false,
    }
}

fn lock_error(context: &str, source: pubky::Error) -> PaykitError {
    PaykitError::Transport {
        context: lock_failure_context(context, &source),
        source: source.into(),
    }
}

// Only allow-listed categories cross the FFI boundary. Error displays may contain
// private resource paths, session tokens, or untrusted server response bodies.
fn lock_failure_context(context: &str, source: &pubky::Error) -> String {
    use pubky::errors::RequestError;
    let category = match source {
        pubky::Error::Request(RequestError::Server { status, .. }) => {
            return format!("{context} (HTTP {})", status.as_u16());
        }
        pubky::Error::Request(RequestError::Transport(error)) if error.is_timeout() => {
            "request timeout"
        }
        pubky::Error::Request(RequestError::Transport(error)) if error.is_connect() => {
            "connection failure"
        }
        pubky::Error::Request(RequestError::Transport(_)) => "transport failure",
        pubky::Error::Request(RequestError::Validation { .. }) => "invalid request",
        pubky::Error::Request(RequestError::DecodeJson { .. }) => "invalid response",
        _ => "Pubky client failure",
    };
    format!("{context} ({category})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn test_silent_resume_rechecks_native_clock_without_operation_wakeup() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let clock = Arc::new(AtomicU64::new(0));
        let worker_clock = clock.clone();
        let waiting = tokio::spawn(async move {
            let lease =
                Mutex::new(LeaseDeadline::new(Duration::ZERO, Duration::from_secs(60)).unwrap());
            wait_for_renewal(
                &lease,
                tokio::time::Instant::now() + Duration::from_secs(54),
                &|| Ok(Duration::from_secs(worker_clock.load(Ordering::SeqCst))),
                &Notify::new(),
            )
            .await
        });
        tokio::task::yield_now().await;
        clock.store(44, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            waiting.is_finished(),
            "silent work must not wait for the old scheduler renewal point"
        );
        waiting.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn test_silent_expired_resume_fails_before_refresh() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let clock = Arc::new(AtomicU64::new(0));
        let worker_clock = clock.clone();
        let waiting = tokio::spawn(async move {
            let lease =
                Mutex::new(LeaseDeadline::new(Duration::ZERO, Duration::from_secs(60)).unwrap());
            wait_for_renewal(
                &lease,
                tokio::time::Instant::now() + Duration::from_secs(54),
                &|| Ok(Duration::from_secs(worker_clock.load(Ordering::SeqCst))),
                &Notify::new(),
            )
            .await
        });
        tokio::task::yield_now().await;
        clock.store(54, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            waiting.is_finished(),
            "expired silent work must be noticed on the bounded tick"
        );
        assert!(waiting.await.unwrap().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn test_silent_clock_ticks_do_not_renew_an_early_lease() {
        let waiting = tokio::spawn(async move {
            let lease =
                Mutex::new(LeaseDeadline::new(Duration::ZERO, Duration::from_secs(60)).unwrap());
            wait_for_renewal(
                &lease,
                tokio::time::Instant::now() + Duration::from_secs(54),
                &|| Ok(Duration::ZERO),
                &Notify::new(),
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn test_resume_notification_is_once_per_confirmed_lease() {
        let mut deadline = LeaseDeadline::new(Duration::ZERO, Duration::from_secs(60)).unwrap();
        assert!(!deadline.check_and_notify(Duration::from_secs(17)).unwrap());
        assert!(deadline.check_and_notify(Duration::from_secs(18)).unwrap());
        assert!(!deadline.check_and_notify(Duration::from_secs(30)).unwrap());
        assert!(deadline.check_and_notify(Duration::from_secs(54)).is_err());
        deadline = LeaseDeadline::new(Duration::from_secs(30), Duration::from_secs(60)).unwrap();
        assert!(!deadline.check_and_notify(Duration::from_secs(47)).unwrap());
        assert!(deadline.check_and_notify(Duration::from_secs(48)).unwrap());
    }

    #[tokio::test]
    async fn test_stale_renewal_permit_waits_for_current_lease() {
        let clock = std::cell::Cell::new(Duration::ZERO);
        let lease = Mutex::new(LeaseDeadline::new(clock.get(), Duration::from_secs(60)).unwrap());
        let wakeup = Notify::new();
        wakeup.notify_one();
        let now = || Ok(clock.get());
        let mut waiting = Box::pin(wait_for_renewal(
            &lease,
            tokio::time::Instant::now() + Duration::from_secs(54),
            &now,
            &wakeup,
        ));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        clock.set(Duration::from_secs(18));
        assert!(lease.lock().unwrap().check_and_notify(clock.get()).unwrap());
        wakeup.notify_one();
        assert!(matches!(
            waiting.as_mut().poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
    }

    #[tokio::test]
    async fn test_notification_before_waiter_registration_is_retained() {
        let lease =
            Mutex::new(LeaseDeadline::new(Duration::ZERO, Duration::from_secs(60)).unwrap());
        let wakeup = Notify::new();
        assert!(lease
            .lock()
            .unwrap()
            .check_and_notify(Duration::from_secs(18))
            .unwrap());
        wakeup.notify_one();
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_renewal(
                &lease,
                tokio::time::Instant::now() + Duration::from_secs(54),
                &|| Ok(Duration::from_secs(18)),
                &wakeup,
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn test_ready_scheduler_timer_progresses_with_early_native_clock() {
        let lease =
            Mutex::new(LeaseDeadline::new(Duration::ZERO, Duration::from_secs(60)).unwrap());
        let wakeup = Notify::new();
        wakeup.notify_one();
        // Its renewal point is already past, but its expiration is still future.
        tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_renewal(
                &lease,
                tokio::time::Instant::now() + Duration::from_secs(35),
                &|| Ok(Duration::ZERO),
                &wakeup,
            ),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[test]
    fn test_refresh_attempt_is_bounded_by_native_time_remaining() {
        let lease =
            Mutex::new(LeaseDeadline::new(Duration::ZERO, Duration::from_secs(60)).unwrap());
        let start = tokio::time::Instant::now();
        let scheduler_expiry = start + Duration::from_secs(54);
        let bound = refresh_deadline(&lease, scheduler_expiry, Duration::from_secs(50)).unwrap();
        assert!(bound >= start + Duration::from_secs(4));
        assert!(bound <= tokio::time::Instant::now() + Duration::from_secs(4));
        assert!(refresh_deadline(&lease, scheduler_expiry, Duration::from_secs(54)).is_err());
        assert!(refresh_deadline(&lease, start - Duration::from_secs(1), Duration::ZERO).is_err());
    }

    #[test]
    fn test_server_quantization_cannot_admit_a_ready_continuation() {
        let start = Duration::from_millis(1000499);
        let deadline = LeaseDeadline::new(start, Duration::from_secs(60)).unwrap();
        // PostgreSQL rounds the acquire and validity timestamps to seconds.
        // At 1059.5s it considers the server's stored expiry of 1060 expired.
        let server_expiry = Duration::from_millis(1059500);
        let continued = std::cell::Cell::new(false);
        let mut guarded = Box::pin(poll_with_lease(
            async {
                continued.set(true);
                Ok(())
            },
            || deadline.check(server_expiry),
        ));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            guarded.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert!(!continued.get());
    }

    #[test]
    fn test_invalid_granted_lease_returns_error_without_panicking() {
        for timeout in [
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(61),
            Duration::from_secs(u64::MAX),
        ] {
            assert!(LeaseDeadline::new(Duration::ZERO, timeout).is_err());
            let result = std::panic::catch_unwind(|| {
                confirmed_lease_deadline(tokio::time::Instant::now(), timeout)
            });
            assert!(result.is_ok(), "an invalid server grant must not panic");
            assert!(result.unwrap().is_err());
        }
    }

    #[test]
    fn test_sleep_expiry_stops_a_ready_continuation_before_polling_it() {
        use std::{cell::Cell, task::Context};
        let clock = Cell::new(Duration::from_secs(10));
        let deadline = LeaseDeadline::new(clock.get(), Duration::from_secs(60)).unwrap();
        let ready = Cell::new(false);
        let continued = Cell::new(false);
        let work = async {
            std::future::poll_fn(|_| {
                if ready.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            continued.set(true);
            Ok(())
        };
        let mut guarded = Box::pin(poll_with_lease(work, || deadline.check(clock.get())));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(guarded.as_mut().poll(&mut cx).is_pending());
        // Only the lease clock advances: the scheduler's renewal timer is still pending.
        clock.set(Duration::from_secs(71));
        ready.set(true);
        assert!(matches!(
            guarded.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert!(!continued.get());
    }

    #[test]
    fn test_lease_response_and_clock_failures_fail_closed() {
        let deadline =
            LeaseDeadline::new(Duration::from_secs(10), Duration::from_secs(60)).unwrap();
        assert!(deadline.check(Duration::from_secs(63)).is_ok());
        assert!(deadline.check(Duration::from_secs(64)).is_err());
        assert!(deadline.check(Duration::from_secs(9)).is_err());
        assert!(LeaseDeadline::new(Duration::MAX, Duration::from_secs(60)).is_err());
        let continued = std::cell::Cell::new(false);
        let mut guarded = Box::pin(poll_with_lease(
            async {
                continued.set(true);
                Ok(())
            },
            || Err(std::io::Error::other("clock unavailable").into()),
        ));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            guarded.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert!(!continued.get());
    }

    #[test]
    fn test_confirmed_renewal_extends_the_guard_before_a_ready_continuation() {
        let clock = std::cell::Cell::new(Duration::from_secs(10));
        let deadline =
            Mutex::new(LeaseDeadline::new(clock.get(), Duration::from_secs(60)).unwrap());
        clock.set(Duration::from_secs(30));
        deadline.lock().unwrap().check(clock.get()).unwrap();
        *deadline.lock().unwrap() =
            LeaseDeadline::new(clock.get(), Duration::from_secs(60)).unwrap();
        clock.set(Duration::from_secs(71));
        let mut guarded = Box::pin(poll_with_lease(async { Ok(42) }, || {
            deadline.lock().unwrap().check(clock.get())
        }));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            guarded.as_mut().poll(&mut cx),
            Poll::Ready(Ok(42))
        ));
    }

    #[test]
    fn test_native_lease_clock_advances() {
        let before = lease_time().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        assert!(lease_time().unwrap() > before);
    }

    #[test]
    fn test_delayed_lock_response_cannot_extend_lease_from_receipt_time() {
        let requested_at = tokio::time::Instant::now() - Duration::from_secs(30);
        let timeout = Duration::from_secs(60);
        assert_eq!(
            confirmed_lease_deadline(requested_at, timeout).unwrap(),
            requested_at + Duration::from_secs(54)
        );
        let requested_at = tokio::time::Instant::now() - Duration::from_secs(61);
        let error = confirmed_lease_deadline(requested_at, timeout).unwrap_err();
        assert!(!is_write_conflict(&error));
        assert!(matches!(error, PaykitError::Transport { .. }));
        assert!(confirmed_lease_deadline(tokio::time::Instant::now(), Duration::ZERO).is_err());
    }

    #[tokio::test]
    async fn test_lock_timeout_redacts_request_url() {
        // Keep the socket open without answering so the real transport reports a timeout.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/private-path?token=secret",
            listener.local_addr().unwrap()
        );
        let source = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap_err();
        let context = lock_failure_context(
            "acquire Pubky write lock",
            &pubky::Error::Request(pubky::errors::RequestError::Transport(source)),
        );
        assert_eq!(context, "acquire Pubky write lock (request timeout)");
    }

    #[test]
    fn test_lock_failure_preserves_status_without_response_body() {
        for status in [401, 403, 423, 429, 500, 503] {
            let error = lock_error(
                "acquire Pubky write lock",
                pubky::Error::Request(pubky::errors::RequestError::Server {
                    status: pubky::StatusCode::from_u16(status).unwrap(),
                    message: "secret-session-token /pub/paykit/private/path".into(),
                }),
            );
            let PaykitError::Transport { context, .. } = error else {
                panic!("expected transport")
            };
            assert_eq!(context, format!("acquire Pubky write lock (HTTP {status})"));
        }
    }

    #[test]
    fn test_lock_failure_redacts_request_and_decode_details() {
        for request_error in [
            pubky::errors::RequestError::Validation {
                message: "private path".into(),
            },
            pubky::errors::RequestError::DecodeJson {
                message: "private payload".into(),
            },
        ] {
            let context = lock_failure_context(
                "read Pubky resource revision",
                &pubky::Error::Request(request_error),
            );
            assert!(!context.contains("private"));
            assert!(
                context == "read Pubky resource revision (invalid request)"
                    || context == "read Pubky resource revision (invalid response)"
            );
        }
    }

    #[test]
    fn test_generation_conflict_requires_fresh_operation() {
        for (status, conflict) in [
            (pubky::StatusCode::CONFLICT, true),
            (pubky::StatusCode::PRECONDITION_FAILED, true),
            (pubky::StatusCode::LOCKED, true),
            (pubky::StatusCode::INTERNAL_SERVER_ERROR, false),
        ] {
            let source = pubky::Error::Request(pubky::errors::RequestError::Server {
                status,
                message: "test response".into(),
            });
            let error = PaykitError::Transport {
                context: "write resource".into(),
                source: source.into(),
            };
            assert_eq!(is_write_conflict(&error), conflict);
        }
    }

    #[test]
    fn test_lock_refresh_retries_only_transient_server_errors() {
        for (status, retryable) in [
            (pubky::StatusCode::SERVICE_UNAVAILABLE, true),
            (pubky::StatusCode::TOO_MANY_REQUESTS, true),
            (pubky::StatusCode::PRECONDITION_FAILED, false),
            (pubky::StatusCode::LOCKED, false),
            (pubky::StatusCode::UNAUTHORIZED, false),
            (pubky::StatusCode::FORBIDDEN, false),
        ] {
            let error = pubky::Error::Request(pubky::errors::RequestError::Server {
                status,
                message: "test response".into(),
            });
            assert_eq!(retryable_lock_refresh(&error), retryable);
        }
    }
}
