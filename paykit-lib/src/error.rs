use thiserror::Error;

/// Domain-specific error type.
///
/// Variants that wrap an underlying cause carry a `source` field backed by
/// [`anyhow::Error`] so that the standard [`std::error::Error::source`] chain
/// is preserved while keeping the public API decoupled from upstream error
/// types. Callers can downcast the source via [`anyhow::Error::downcast_ref`]
/// when they need the original typed error.
#[derive(Debug, Error)]
pub enum PaykitError {
    /// Wrapper for transport layer failures.
    ///
    /// Most user-facing failures bubble up through this variant, encapsulating
    /// lower-level SDK/network errors (timeouts, connection refused, permission
    /// denied, etc.).
    #[error("transport error: {context}")]
    Transport {
        /// Human-readable description of what went wrong.
        context: String,
        /// The underlying error that caused this failure.
        #[source]
        source: anyhow::Error,
    },

    /// The requested resource does not exist.
    ///
    /// Returned when a payment endpoint or other resource is not found (404/GONE).
    #[error("not found: {0}")]
    NotFound(String),

    /// Retrieved data is corrupt or structurally invalid.
    ///
    /// Returned when a resource was successfully fetched from the network but its
    /// content cannot be interpreted — for example invalid UTF-8 bytes or an
    /// unparseable resource path. This is distinct from
    /// [`PaykitError::Transport`] (the network call itself failed).
    #[error("invalid data: {context}")]
    InvalidData {
        /// Human-readable description of the data problem.
        context: String,
        /// The underlying error, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Input failed validation.
    ///
    /// Returned when a caller-supplied value (such as a [`crate::PaymentEndpointIdentifier`]) violates
    /// structural invariants — for example containing path-traversal sequences,
    /// null bytes, or characters outside the allowed set.
    #[error("validation error: {0}")]
    Validation(String),
}

#[derive(Debug, Error)]
#[error("pubky-noise send_message failed with non-retryable error: {0:?}")]
pub(crate) struct NonRetryablePrivateSendError(pub(crate) pubky_noise::PubkyNoiseError);

impl PaykitError {
    /// Returns true when this error came from a deterministic Encrypted Link
    /// private-message send failure that should trigger link recovery instead
    /// of ordinary transport retry.
    pub fn is_non_retryable_private_send_error(&self) -> bool {
        matches!(
            self,
            Self::Transport { source, .. }
                if source
                    .downcast_ref::<NonRetryablePrivateSendError>()
                    .is_some()
        )
    }
}

pub(crate) fn map_error(label: &'static str, err: PaykitError) -> PaykitError {
    match err {
        PaykitError::Transport { context, source } => PaykitError::Transport {
            context: format!("{label}: {context}"),
            source,
        },
        PaykitError::NotFound(msg) => PaykitError::NotFound(format!("{label}: {msg}")),
        PaykitError::InvalidData { context, source } => PaykitError::InvalidData {
            context: format!("{label}: {context}"),
            source,
        },
        PaykitError::Validation(msg) => PaykitError::Validation(format!("{label}: {msg}")),
    }
}
