use thiserror::Error;

/// Error type for stateful Paykit SDK workflows.
#[derive(Debug, Error)]
pub enum PaykitSdkError {
    /// Durable storage failed.
    #[error("storage error: {context}")]
    Storage {
        /// Human-readable failure context.
        context: String,
        /// Underlying cause, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Pubky identity, session, or key capability failed.
    #[error("identity error: {context}")]
    Identity {
        /// Human-readable failure context.
        context: String,
        /// Underlying cause, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Pubky or Encrypted Link transport failed.
    #[error("transport error: {context}")]
    Transport {
        /// Human-readable failure context.
        context: String,
        /// Underlying cause, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Paykit protocol data is invalid, conflicting, or unsupported.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Operation is blocked by configured SDK policy.
    #[error("policy error: {0}")]
    Policy(String),

    /// Payment adapter failed.
    #[error("payment adapter error: {context}")]
    PaymentAdapter {
        /// Human-readable failure context.
        context: String,
        /// Underlying cause, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Local state needs explicit recovery before automation can continue.
    #[error("recovery required: {0}")]
    RecoveryRequired(String),
}

impl From<paykit_lib::PaykitError> for PaykitSdkError {
    fn from(err: paykit_lib::PaykitError) -> Self {
        match err {
            paykit_lib::PaykitError::Transport { context, source } => Self::Transport {
                context,
                source: Some(source),
            },
            paykit_lib::PaykitError::NotFound(msg) => Self::Transport {
                context: msg,
                source: None,
            },
            paykit_lib::PaykitError::InvalidData { context, source } => Self::Protocol(
                source
                    .map(|source| format!("{context}: {source}"))
                    .unwrap_or(context),
            ),
            paykit_lib::PaykitError::Validation(msg) => Self::Protocol(msg),
        }
    }
}
