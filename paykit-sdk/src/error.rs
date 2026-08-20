use std::fmt;

use thiserror::Error;

/// Error type for stateful Paykit SDK workflows.
#[derive(Error)]
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

impl fmt::Debug for PaykitSdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage { context, source } => debug_error_variant(f, "Storage", context, source),
            Self::Identity { context, source } => {
                debug_error_variant(f, "Identity", context, source)
            }
            Self::Transport { context, source } => {
                debug_error_variant(f, "Transport", context, source)
            }
            Self::NotFound { context, source } => {
                debug_error_variant(f, "NotFound", context, source)
            }
            Self::Protocol { context, source } => {
                debug_error_variant(f, "Protocol", context, source)
            }
            Self::Policy { context, source } => debug_error_variant(f, "Policy", context, source),
            Self::PaymentAdapter { context, source } => {
                debug_error_variant(f, "PaymentAdapter", context, source)
            }
            Self::RecoveryRequired { context, source } => {
                debug_error_variant(f, "RecoveryRequired", context, source)
            }
        }
    }
}

fn debug_error_variant(
    f: &mut fmt::Formatter<'_>,
    name: &'static str,
    context: &str,
    source: &Option<anyhow::Error>,
) -> fmt::Result {
    f.debug_struct(name)
        .field("context", &context)
        .field("source", &source.as_ref().map(|_| "<redacted>"))
        .finish()
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
            // Network parse causes can contain raw private payload fragments.
            // Keep only the curated context when crossing into SDK errors.
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
