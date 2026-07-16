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
    #[error("not found: {context}")]
    NotFound {
        /// Human-readable failure context.
        context: String,
        /// Underlying cause, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Paykit protocol data is invalid, conflicting, or unsupported.
    #[error("protocol error: {context}")]
    Protocol {
        /// Human-readable failure context.
        context: String,
        /// Underlying cause, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Operation is blocked by configured SDK policy.
    #[error("policy error: {context}")]
    Policy {
        /// Human-readable failure context.
        context: String,
        /// Underlying cause, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

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
    #[error("recovery required: {context}")]
    RecoveryRequired {
        /// Human-readable failure context.
        context: String,
        /// Underlying cause, when available.
        #[source]
        source: Option<anyhow::Error>,
    },
}

impl From<paykit_lib::PaykitError> for PaykitSdkError {
    fn from(err: paykit_lib::PaykitError) -> Self {
        match err {
            paykit_lib::PaykitError::Transport { context, source } => Self::Transport {
                context,
                source: Some(source),
            },
            paykit_lib::PaykitError::NotFound(msg) => Self::NotFound {
                context: msg,
                source: None,
            },
            // SECURITY / REDACTION: drop lib `InvalidData` sources entirely.
            // They carry raw parse/decode causes derived from network data or
            // decrypted plaintext. A retained cause would stay out of FFI
            // exception text (the conversion drops every `source` that is not
            // an app-authored `PaykitFfiError` recovered by downcast), but
            // `PaykitSdkError` derives field-wise `Debug`, so it would still
            // surface in `format!("{err:?}")` and structured Rust logs.
            // Dropping it keeps that local logging channel structurally
            // closed; only the lib-supplied `context` string survives.
            paykit_lib::PaykitError::InvalidData { context, source: _ } => Self::Protocol {
                context,
                source: None,
            },
            paykit_lib::PaykitError::Validation(msg) => Self::Protocol {
                context: msg,
                source: None,
            },
        }
    }
}

#[cfg(test)]
mod tests;
