use std::{future::Future, time::Duration};

use pubky::{PubkySession, StorageLock};

use crate::{PaykitError, Result};

use super::{content_revision, is_not_found, read_bounded_body};

const WRITE_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
#[error("resource changed since it was read")]
struct ResourceChanged;

#[derive(Debug, thiserror::Error)]
#[error("write lock renewal failed; the operation may have committed")]
struct LockRenewalError(#[source] pubky::Error);

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
/// [`content_revision`] of the exact previously read bytes, not HTTP metadata.
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

async fn resource_revision(
    session: &PubkySession,
    path: &str,
    max_bytes: usize,
) -> Result<Option<String>> {
    let response = match session.storage().get(path).await {
        Ok(response) => response,
        Err(error) if is_not_found(&error) => return Ok(None),
        Err(error) => return Err(lock_error("read locked Pubky resource", error)),
    };
    let bytes = read_bounded_body(response, max_bytes, "read locked Pubky resource").await?;
    Ok(Some(content_revision(&bytes)))
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
/// afterward. Cancellation leaves it to expire. A renewal failure stops the
/// operation with an uncertain outcome, not a retryable write conflict: a PUT
/// may already have committed. Callers must reconcile before retrying.
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
    let lock = storage
        .lock(path, WRITE_LOCK_TIMEOUT)
        .await
        .map_err(|source| E::from(lock_error("acquire Pubky write lock", source)))?;
    let mut renewed = lock.clone();
    let renewal = async {
        loop {
            tokio::time::sleep(renewed.timeout() / 3).await;
            if let Err(error) = storage.refresh_lock(&mut renewed, WRITE_LOCK_TIMEOUT).await {
                break error;
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

fn lock_error(context: &str, source: pubky::Error) -> PaykitError {
    PaykitError::Transport {
        context: context.into(),
        source: source.into(),
    }
}
