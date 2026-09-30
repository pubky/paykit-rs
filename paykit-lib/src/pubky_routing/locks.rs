use std::{future::Future, time::Duration};

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
/// The homeserver must prevent writes from committing after lock ownership is
/// lost. Session creation, capability scope, key rotation, and request timeouts
/// remain the caller's responsibility.
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
    let storage = session.storage();
    let mut attempt = 1;
    let (lock, acquired_at) = loop {
        let requested_at = tokio::time::Instant::now();
        match storage.lock(path, WRITE_LOCK_TIMEOUT).await {
            Ok(lock) => break (lock, requested_at),
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
    let mut renewed = lock.clone();
    let renewal = async {
        let mut valid_until = acquired_at + renewed.timeout();
        loop {
            tokio::time::sleep_until(valid_until - renewed.timeout() * 2 / 3).await;
            loop {
                let requested_at = tokio::time::Instant::now();
                match tokio::time::timeout_at(
                    valid_until,
                    storage.refresh_lock(&mut renewed, WRITE_LOCK_TIMEOUT),
                )
                .await
                {
                    Ok(Ok(())) => {
                        valid_until = requested_at + renewed.timeout();
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
                    Err(_) => {
                        return anyhow::anyhow!(
                            "Pubky write lock expired before renewal was confirmed"
                        )
                    }
                }
            }
        }
    };
    let result = tokio::select! {
        biased;
        error = renewal => {
            return Err(E::from(PaykitError::Transport {
                context: "Pubky write lock lost; operation outcome is uncertain".into(),
                source: LockRenewalError(error).into(),
            }));
        }
        result = operation(lock.clone()) => result,
    };
    // Releasing the lock does not undo a committed operation. A failed unlock
    // leaves a bounded lease, not a reason to repeat a successful mutation.
    if storage.unlock(&lock).await.is_err() {
        tracing::warn!("Failed to release Pubky write lock; waiting for expiry");
    }
    result
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
        context: context.into(),
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
