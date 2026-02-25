//! Transport policy for timeout and retry with exponential backoff.
//!
//! The [`TransportPolicy`] struct configures how transport operations handle
//! slow or failing network calls. The Pubky adapters embed a default policy
//! automatically — callers only need to interact with this module when they
//! want non-default settings.
//!
//! # Policy enforcement
//!
//! The retry/timeout execution machinery is **internal to the Pubky adapters**
//! (gated behind the `pubky` feature). Custom transport implementations that
//! satisfy [`AuthenticatedTransport`](crate::AuthenticatedTransport) or
//! [`UnauthenticatedTransportRead`](crate::UnauthenticatedTransportRead) are
//! responsible for their own timeout and retry logic. They may use
//! `TransportPolicy` as a configuration type if desired, but the crate does
//! not expose a generic execution helper.
//!
//! # Examples
//! ```
//! use std::time::Duration;
//! use paykit_lib::TransportPolicy;
//!
//! // Defaults: 30 s timeout, 3 retries, exponential backoff.
//! let default = TransportPolicy::default();
//!
//! // Custom via builder:
//! let custom = TransportPolicy::builder()
//!     .timeout(Duration::from_secs(10))
//!     .max_retries(1)
//!     .build();
//!
//! // Disable all protection (raw, unbounded calls):
//! let none = TransportPolicy::none();
//! ```

use std::time::Duration;

/// Policy controlling timeout and retry behaviour for transport operations.
///
/// Every Pubky adapter applies `TransportPolicy::default()` unless the caller
/// overrides it via `.with_policy()`. The default protects against indefinitely
/// hanging calls by enforcing a per-attempt timeout and retrying transient
/// failures with exponential backoff and full jitter.
#[derive(Clone, Debug)]
pub struct TransportPolicy {
    /// Maximum duration for a single attempt. `None` disables the timeout.
    pub timeout: Option<Duration>,
    /// Number of retry attempts *after* the initial call (0 = no retries).
    pub max_retries: u32,
    /// Base delay for exponential backoff (doubled each retry).
    pub base_delay: Duration,
    /// Upper bound on the backoff delay.
    pub max_delay: Duration,
}

impl Default for TransportPolicy {
    /// Sensible defaults: 30 s timeout, 3 retries, 200 ms base / 10 s cap.
    fn default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(30)),
            max_retries: 3,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
        }
    }
}

impl TransportPolicy {
    /// Start building a custom policy.
    pub fn builder() -> TransportPolicyBuilder {
        TransportPolicyBuilder::default()
    }

    /// A policy that applies no timeout and no retries.
    ///
    /// Equivalent to the pre-policy behaviour where every call was unbounded.
    pub fn none() -> Self {
        Self {
            timeout: None,
            max_retries: 0,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }
}

// ── Builder ─────────────────────────────────────────────────────────────

/// Builder for [`TransportPolicy`].
///
/// Unset fields inherit from [`TransportPolicy::default()`].
#[derive(Clone, Debug, Default)]
pub struct TransportPolicyBuilder {
    timeout: Option<Option<Duration>>,
    max_retries: Option<u32>,
    base_delay: Option<Duration>,
    max_delay: Option<Duration>,
}

impl TransportPolicyBuilder {
    /// Set the per-attempt timeout.
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(Some(duration));
        self
    }

    /// Disable the per-attempt timeout entirely.
    pub fn no_timeout(mut self) -> Self {
        self.timeout = Some(None);
        self
    }

    /// Set the maximum number of retry attempts (0 = no retries).
    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = Some(n);
        self
    }

    /// Set the base delay for exponential backoff.
    pub fn base_delay(mut self, duration: Duration) -> Self {
        self.base_delay = Some(duration);
        self
    }

    /// Set the upper bound on backoff delay.
    pub fn max_delay(mut self, duration: Duration) -> Self {
        self.max_delay = Some(duration);
        self
    }

    /// Build the policy, falling back to defaults for unset fields.
    pub fn build(self) -> TransportPolicy {
        let defaults = TransportPolicy::default();
        TransportPolicy {
            timeout: self.timeout.unwrap_or(defaults.timeout),
            max_retries: self.max_retries.unwrap_or(defaults.max_retries),
            base_delay: self.base_delay.unwrap_or(defaults.base_delay),
            max_delay: self.max_delay.unwrap_or(defaults.max_delay),
        }
    }
}

// ── Execution helper ────────────────────────────────────────────────────

/// Execute an async operation with timeout and retry according to `policy`.
///
/// Only [`PaykitError::Transport`] is considered retryable. All other variants
/// — including `Timeout` — are returned immediately. A timed-out operation has
/// already consumed the full timeout budget; retrying it would multiply the
/// total wait time by `max_retries`, defeating the purpose of the timeout.
#[cfg(feature = "pubky")]
pub(crate) async fn execute_with_policy<F, Fut, R>(
    policy: &TransportPolicy,
    op_name: &str,
    f: F,
) -> crate::Result<R>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = crate::Result<R>>,
{
    use crate::PaykitError;

    let mut attempt = 0u32;
    loop {
        let result = match policy.timeout {
            Some(dur) => match tokio::time::timeout(dur, f()).await {
                Ok(inner) => inner,
                Err(_elapsed) => Err(PaykitError::Timeout {
                    context: format!("{op_name}: timed out after {dur:?}"),
                }),
            },
            None => f().await,
        };

        match result {
            Ok(val) => return Ok(val),
            Err(ref e) if is_retryable(e) && attempt < policy.max_retries => {
                attempt += 1;
                let delay = backoff_delay(policy, attempt);
                tracing::warn!(
                    attempt,
                    max = policy.max_retries,
                    delay_ms = delay.as_millis() as u64,
                    op = op_name,
                    "retrying after transient error"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(feature = "pubky")]
fn is_retryable(err: &crate::PaykitError) -> bool {
    matches!(err, crate::PaykitError::Transport { .. })
}

#[cfg(feature = "pubky")]
fn backoff_delay(policy: &TransportPolicy, attempt: u32) -> Duration {
    let base_ms = policy.base_delay.as_millis() as u64;
    // Exponential: base * 2^(attempt-1), clamped to avoid overflow.
    let exp_ms = base_ms.saturating_mul(1u64 << (attempt - 1).min(16));
    let max_ms = policy.max_delay.as_millis() as u64;
    let capped = exp_ms.min(max_ms);
    // Full jitter: uniform random in [0, capped].
    let jittered = if capped > 0 {
        rand::random::<u64>() % (capped + 1)
    } else {
        0
    };
    Duration::from_millis(jittered)
}

// ── Unit tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_policy_values() {
        let p = TransportPolicy::default();
        assert_eq!(p.timeout, Some(Duration::from_secs(30)));
        assert_eq!(p.max_retries, 3);
        assert_eq!(p.base_delay, Duration::from_millis(200));
        assert_eq!(p.max_delay, Duration::from_secs(10));
    }

    #[test]
    fn test_none_policy() {
        let p = TransportPolicy::none();
        assert_eq!(p.timeout, None);
        assert_eq!(p.max_retries, 0);
        assert_eq!(p.base_delay, Duration::ZERO);
        assert_eq!(p.max_delay, Duration::ZERO);
    }

    #[test]
    fn test_builder_defaults_match_default() {
        let built = TransportPolicy::builder().build();
        let default = TransportPolicy::default();
        assert_eq!(built.timeout, default.timeout);
        assert_eq!(built.max_retries, default.max_retries);
        assert_eq!(built.base_delay, default.base_delay);
        assert_eq!(built.max_delay, default.max_delay);
    }

    #[test]
    fn test_builder_custom_values() {
        let p = TransportPolicy::builder()
            .timeout(Duration::from_secs(5))
            .max_retries(1)
            .base_delay(Duration::from_millis(100))
            .max_delay(Duration::from_secs(2))
            .build();
        assert_eq!(p.timeout, Some(Duration::from_secs(5)));
        assert_eq!(p.max_retries, 1);
        assert_eq!(p.base_delay, Duration::from_millis(100));
        assert_eq!(p.max_delay, Duration::from_secs(2));
    }

    #[test]
    fn test_builder_no_timeout() {
        let p = TransportPolicy::builder().no_timeout().build();
        assert_eq!(p.timeout, None);
        // Other fields should still be defaults.
        assert_eq!(p.max_retries, 3);
    }

    #[test]
    fn test_builder_partial_override() {
        let p = TransportPolicy::builder().max_retries(0).build();
        assert_eq!(p.max_retries, 0);
        // Timeout should still be the default.
        assert_eq!(p.timeout, Some(Duration::from_secs(30)));
    }

    #[cfg(feature = "pubky")]
    #[test]
    fn test_backoff_delay_bounded() {
        let policy = TransportPolicy {
            timeout: None,
            max_retries: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
        };
        for attempt in 1..=5 {
            let delay = backoff_delay(&policy, attempt);
            assert!(
                delay <= Duration::from_secs(1),
                "attempt {attempt}: delay {delay:?} exceeds max"
            );
        }
    }

    #[cfg(feature = "pubky")]
    #[test]
    fn test_backoff_delay_zero_base() {
        let policy = TransportPolicy {
            timeout: None,
            max_retries: 3,
            base_delay: Duration::ZERO,
            max_delay: Duration::from_secs(1),
        };
        let delay = backoff_delay(&policy, 1);
        assert_eq!(delay, Duration::ZERO);
    }

    #[cfg(feature = "pubky")]
    #[test]
    fn test_is_retryable_transport() {
        let err = crate::PaykitError::Transport {
            context: "test".into(),
            source: anyhow::anyhow!("boom"),
        };
        assert!(is_retryable(&err));
    }

    #[cfg(feature = "pubky")]
    #[test]
    fn test_not_retryable_timeout() {
        let err = crate::PaykitError::Timeout {
            context: "test".into(),
        };
        assert!(!is_retryable(&err));
    }

    #[cfg(feature = "pubky")]
    #[test]
    fn test_not_retryable_validation() {
        let err = crate::PaykitError::Validation("bad input".into());
        assert!(!is_retryable(&err));
    }

    #[cfg(feature = "pubky")]
    #[test]
    fn test_not_retryable_not_found() {
        let err = crate::PaykitError::NotFound("gone".into());
        assert!(!is_retryable(&err));
    }

    #[cfg(feature = "pubky")]
    #[test]
    fn test_not_retryable_invalid_data() {
        let err = crate::PaykitError::InvalidData {
            context: "bad".into(),
            source: None,
        };
        assert!(!is_retryable(&err));
    }

    #[cfg(feature = "pubky")]
    #[test]
    fn test_not_retryable_profile() {
        let err = crate::PaykitError::Profile("malformed".into());
        assert!(!is_retryable(&err));
    }

    #[cfg(feature = "pubky")]
    #[tokio::test]
    async fn test_execute_success_no_retry() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let policy = TransportPolicy::default();
        let call_count = AtomicU32::new(0);

        let result: crate::Result<&str> = execute_with_policy(&policy, "test_op", || async {
            call_count.fetch_add(1, Ordering::SeqCst);
            Ok("ok")
        })
        .await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "pubky")]
    #[tokio::test]
    async fn test_execute_retries_on_transport_error() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let policy = TransportPolicy::builder()
            .no_timeout()
            .max_retries(2)
            .base_delay(Duration::from_millis(1))
            .max_delay(Duration::from_millis(10))
            .build();

        let call_count = AtomicU32::new(0);

        let result: crate::Result<&str> = execute_with_policy(&policy, "test_op", || async {
            let n = call_count.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(crate::PaykitError::Transport {
                    context: "transient".into(),
                    source: anyhow::anyhow!("connection reset"),
                })
            } else {
                Ok("recovered")
            }
        })
        .await;

        assert_eq!(result.unwrap(), "recovered");
        assert_eq!(call_count.load(Ordering::SeqCst), 3); // 1 initial + 2 retries
    }

    #[cfg(feature = "pubky")]
    #[tokio::test]
    async fn test_execute_no_retry_on_validation() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let policy = TransportPolicy::builder()
            .no_timeout()
            .max_retries(3)
            .build();

        let call_count = AtomicU32::new(0);

        let result: crate::Result<()> = execute_with_policy(&policy, "test_op", || async {
            call_count.fetch_add(1, Ordering::SeqCst);
            Err(crate::PaykitError::Validation("bad".into()))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 1); // No retries
    }

    #[cfg(feature = "pubky")]
    #[tokio::test]
    async fn test_execute_timeout_fires() {
        let policy = TransportPolicy::builder()
            .timeout(Duration::from_millis(10))
            .max_retries(0)
            .build();

        let result: crate::Result<()> = execute_with_policy(&policy, "slow_op", || async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        })
        .await;

        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::PaykitError::Timeout { .. }),
            "expected Timeout, got {err:?}"
        );
    }

    #[cfg(feature = "pubky")]
    #[tokio::test]
    async fn test_execute_no_retry_on_timeout() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let policy = TransportPolicy::builder()
            .timeout(Duration::from_millis(10))
            .max_retries(3)
            .build();

        let call_count = AtomicU32::new(0);

        let result: crate::Result<()> = execute_with_policy(&policy, "slow_op", || async {
            call_count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        })
        .await;

        assert!(matches!(
            result.unwrap_err(),
            crate::PaykitError::Timeout { .. }
        ));
        // Timeout should NOT be retried — only 1 attempt despite max_retries=3.
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "pubky")]
    #[tokio::test]
    async fn test_execute_exhausts_retries() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let policy = TransportPolicy::builder()
            .no_timeout()
            .max_retries(2)
            .base_delay(Duration::from_millis(1))
            .max_delay(Duration::from_millis(5))
            .build();

        let call_count = AtomicU32::new(0);

        let result: crate::Result<()> = execute_with_policy(&policy, "fail_op", || async {
            call_count.fetch_add(1, Ordering::SeqCst);
            Err(crate::PaykitError::Transport {
                context: "always fails".into(),
                source: anyhow::anyhow!("permanent"),
            })
        })
        .await;

        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 3); // 1 initial + 2 retries
    }

    #[cfg(feature = "pubky")]
    #[tokio::test]
    async fn test_execute_zero_retries() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let policy = TransportPolicy::builder()
            .no_timeout()
            .max_retries(0)
            .build();

        let call_count = AtomicU32::new(0);

        let result: crate::Result<()> = execute_with_policy(&policy, "once_op", || async {
            call_count.fetch_add(1, Ordering::SeqCst);
            Err(crate::PaykitError::Transport {
                context: "fail".into(),
                source: anyhow::anyhow!("boom"),
            })
        })
        .await;

        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "pubky")]
    #[tokio::test]
    async fn test_execute_none_policy_no_timeout_no_retry() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let policy = TransportPolicy::none();
        let call_count = AtomicU32::new(0);

        let result: crate::Result<()> = execute_with_policy(&policy, "raw_op", || async {
            call_count.fetch_add(1, Ordering::SeqCst);
            Err(crate::PaykitError::Transport {
                context: "fail".into(),
                source: anyhow::anyhow!("boom"),
            })
        })
        .await;

        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }
}
