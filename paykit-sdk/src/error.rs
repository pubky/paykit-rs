use thiserror::Error;

/// Error type for stateful Paykit SDK workflows.
#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// Requested Paykit or Pubky resource was not found.
    #[error("not found: {0}")]
    NotFound(String),

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
            paykit_lib::PaykitError::NotFound(msg) => Self::NotFound(msg),
            // SECURITY / REDACTION: never fold `source` into the Protocol
            // string. `Protocol` crosses the FFI boundary verbatim (rendered
            // into generated Kotlin/Swift exception messages), and lib-level
            // `InvalidData` sources carry raw parse/decode causes derived from
            // network data or decrypted plaintext. Only the curated static
            // `context` label may cross; the cause is deliberately dropped.
            paykit_lib::PaykitError::InvalidData { context, source: _ } => Self::Protocol(context),
            paykit_lib::PaykitError::Validation(msg) => Self::Protocol(msg),
        }
    }
}

#[cfg(test)]
mod tests;
