use std::{future::Future, sync::Mutex, task::Poll, time::Duration};

use pubky::{PubkySession, StorageLock};

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
    let valid_until = confirmed_lease_deadline(acquired_at, lock.timeout()).map_err(E::from)?;
    let lease = Mutex::new(
        LeaseDeadline::new(lease_started_at, lock.timeout())
            .map_err(|error| E::from(lost_lock(error)))?,
    );
    let check_lease = || -> anyhow::Result<()> {
        lease
            .lock()
            .map_err(|_| anyhow::anyhow!("Pubky write lock clock state unavailable"))?
            .check(now()?)
    };
    check_lease().map_err(|error| E::from(lost_lock(error)))?;
    let mut renewed = lock.clone();
    let renewal = async {
        let mut valid_until = valid_until;
        loop {
            tokio::time::sleep_until(valid_until - renewed.timeout() * 2 / 3).await;
            loop {
                let requested_at = tokio::time::Instant::now();
                let lease_started_at = match now() {
                    Ok(value) => value,
                    Err(error) => return error.into(),
                };
                if let Err(error) = check_lease() {
                    return error;
                }
                if requested_at >= valid_until {
                    return anyhow::anyhow!(
                        "Pubky write lock expired before renewal was confirmed"
                    );
                }
                match tokio::time::timeout_at(
                    valid_until,
                    storage.refresh_lock(&mut renewed, timeout),
                )
                .await
                {
                    Ok(Ok(())) if tokio::time::Instant::now() < valid_until => {
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
                        match lease.lock() {
                            Ok(mut deadline) => *deadline = next,
                            Err(_) => {
                                return anyhow::anyhow!("Pubky write lock clock state unavailable")
                            }
                        }
                        valid_until =
                            match confirmed_lease_deadline(requested_at, renewed.timeout()) {
                                Ok(deadline) => deadline,
                                Err(error) => return anyhow::Error::from(error),
                            };
                        break;
                    }
                    Ok(Err(error))
                        if retryable_lock_refresh(&error)
                            && requested_at + Duration::from_secs(1) < valid_until =>
                    {
                        tokio::time::sleep_until(
                            (tokio::time::Instant::now() + Duration::from_secs(1)).min(valid_until),
                        )
                        .await;
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
    let work = async {
        tokio::select! {
            biased;
            error = renewal => Err(error),
            result = Box::pin(async { operation(lock.clone()).await }) => Ok(result),
        }
    };
    // Tokio's scheduling clock can stop during native device sleep. Check the
    // suspend-inclusive lease before every resumed poll, even if renewal's sleep is pending.
    let result = poll_with_lease(work, check_lease)
        .await
        .map_err(|error| E::from(lost_lock(error)))?;
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

#[derive(Clone, Copy)]
struct LeaseDeadline {
    started_at: Duration,
    expires_at: Duration,
}

impl LeaseDeadline {
    fn new(started_at: Duration, timeout: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            started_at,
            expires_at: started_at
                .checked_add(timeout)
                .ok_or_else(|| anyhow::anyhow!("Pubky write lock deadline overflow"))?,
        })
    }

    fn check(self, now: Duration) -> anyhow::Result<()> {
        if now < self.started_at || now >= self.expires_at {
            anyhow::bail!("Pubky write lock expired or its clock moved backwards");
        }
        Ok(())
    }
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
    let deadline = requested_at + timeout;
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
        assert!(deadline.check(Duration::from_secs(69)).is_ok());
        assert!(deadline.check(Duration::from_secs(70)).is_err());
        assert!(deadline.check(Duration::from_secs(9)).is_err());
        assert!(LeaseDeadline::new(Duration::MAX, Duration::from_secs(1)).is_err());
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
            requested_at + timeout
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
