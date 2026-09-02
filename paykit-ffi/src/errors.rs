use paykit_sdk::PaykitSdkError;

/// Error type exposed through generated bindings.
#[derive(uniffi::Error, Clone, Debug, thiserror::Error)]
pub enum PaykitFfiError {
    /// Another authorized client committed a newer shared-state revision.
    #[error("concurrent_update/{code}: {context}")]
    ConcurrentUpdate {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
    /// Durable storage failed.
    #[error("storage/{code}: {context}")]
    Storage {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
    /// Pubky identity, session, or key capability failed.
    #[error("identity/{code}: {context}")]
    Identity {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
    /// Pubky or Encrypted Link transport failed.
    #[error("transport/{code}: {context}")]
    Transport {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
    /// Requested Paykit or Pubky resource was not found.
    #[error("not_found/{code}: {context}")]
    NotFound {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
    /// Paykit protocol data is invalid, conflicting, or unsupported.
    #[error("protocol/{code}: {context}")]
    Protocol {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
    /// Operation is blocked by configured SDK policy.
    #[error("policy/{code}: {context}")]
    Policy {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
    /// Payment adapter failed.
    #[error("payment_adapter/{code}: {context}")]
    PaymentAdapter {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
    /// Local state needs explicit recovery before automation can continue.
    #[error("recovery_required/{code}: {context}")]
    RecoveryRequired {
        /// Stable machine-readable error code.
        code: String,
        /// Redacted human-readable error context.
        context: String,
    },
}

impl From<PaykitSdkError> for PaykitFfiError {
    fn from(err: PaykitSdkError) -> Self {
        match err {
            PaykitSdkError::ConcurrentUpdate { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| {
                    Self::ConcurrentUpdate {
                        code: "concurrent_update".into(),
                        context,
                    }
                })
            }
            PaykitSdkError::Storage { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| Self::Storage {
                    code: "storage_error".into(),
                    context,
                })
            }
            PaykitSdkError::Identity { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| Self::Identity {
                    code: "identity_error".into(),
                    context,
                })
            }
            PaykitSdkError::Transport { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| Self::Transport {
                    code: "transport_error".into(),
                    context,
                })
            }
            PaykitSdkError::NotFound { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| Self::NotFound {
                    code: "not_found".into(),
                    context,
                })
            }
            PaykitSdkError::Protocol { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| Self::Protocol {
                    code: "protocol_error".into(),
                    context,
                })
            }
            PaykitSdkError::Policy { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| Self::Policy {
                    code: "policy_error".into(),
                    context,
                })
            }
            PaykitSdkError::PaymentAdapter { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| {
                    Self::PaymentAdapter {
                        code: "payment_adapter_error".into(),
                        context,
                    }
                })
            }
            PaykitSdkError::RecoveryRequired { context, source } => {
                callback_ffi_error(source.as_ref(), &context).unwrap_or_else(|| {
                    Self::RecoveryRequired {
                        code: "recovery_required".into(),
                        context,
                    }
                })
            }
            other => Self::Protocol {
                code: "unsupported_sdk_error".into(),
                context: other.to_string(),
            },
        }
    }
}

// SECURITY / REDACTION: the returned `context` is rendered verbatim into the
// generated Kotlin/Swift exception message (see the `#[error("...: {context}")]`
// attributes above), so it crosses the FFI boundary into user-facing app code.
// We therefore MUST NOT fold the raw `source` cause chain into it: anyhow's
// `{:#}` alternate form expands the full chain, which for Pubky transport/storage
// failures can carry the request URL (recovery-marker URLs embed a DH-derived
// PRIVATE storage path) and a non-2xx HTTP response body. Those are sensitive and
// must stay out of shipped exception text.
//
// The raw `source` cause is therefore dropped entirely: it is neither folded into
// `context` nor logged anywhere (no `tracing`/telemetry sink), so an enabled debug
// subscriber cannot capture the URL / DH path / response body. Only the redacted
// outer label survives in `context`. The caller retains the original
// `PaykitSdkError` (with its `source`) at the point of failure if it needs the
// cause for local, non-shipped handling.
//
// Callback errors preserve their variant and machine-readable code, but their
// app-provided context is replaced by the SDK operation label. Platform
// adapters can otherwise accidentally place file paths, payment metadata, or
// provider responses into generated exception text.
fn callback_ffi_error(source: Option<&anyhow::Error>, context: &str) -> Option<PaykitFfiError> {
    let error = source?.downcast_ref::<PaykitFfiError>()?;
    let context = context.to_owned();
    Some(match error {
        PaykitFfiError::ConcurrentUpdate { code, .. } => PaykitFfiError::ConcurrentUpdate {
            code: code.clone(),
            context,
        },
        PaykitFfiError::Storage { code, .. } => PaykitFfiError::Storage {
            code: code.clone(),
            context,
        },
        PaykitFfiError::Identity { code, .. } => PaykitFfiError::Identity {
            code: code.clone(),
            context,
        },
        PaykitFfiError::Transport { code, .. } => PaykitFfiError::Transport {
            code: code.clone(),
            context,
        },
        PaykitFfiError::NotFound { code, .. } => PaykitFfiError::NotFound {
            code: code.clone(),
            context,
        },
        PaykitFfiError::Protocol { code, .. } => PaykitFfiError::Protocol {
            code: code.clone(),
            context,
        },
        PaykitFfiError::Policy { code, .. } => PaykitFfiError::Policy {
            code: code.clone(),
            context,
        },
        PaykitFfiError::PaymentAdapter { code, .. } => PaykitFfiError::PaymentAdapter {
            code: code.clone(),
            context,
        },
        PaykitFfiError::RecoveryRequired { code, .. } => PaykitFfiError::RecoveryRequired {
            code: code.clone(),
            context,
        },
    })
}

pub(crate) fn ffi_error_to_sdk(err: PaykitFfiError, context: &'static str) -> PaykitSdkError {
    let source = Some(anyhow::Error::new(err.clone()));
    match err {
        PaykitFfiError::ConcurrentUpdate {
            code,
            context: _reason,
        } => PaykitSdkError::ConcurrentUpdate {
            context: format!("{context}: {code}"),
            source,
        },
        PaykitFfiError::Storage {
            code,
            context: _reason,
        } => PaykitSdkError::Storage {
            context: format!("{context}: {code}"),
            source,
        },
        PaykitFfiError::Identity {
            code,
            context: _reason,
        } => PaykitSdkError::Identity {
            context: format!("{context}: {code}"),
            source,
        },
        PaykitFfiError::Transport {
            code,
            context: _reason,
        } => PaykitSdkError::Transport {
            context: format!("{context}: {code}"),
            source,
        },
        // Every arm stashes the original callback error in `source`, so the
        // reverse conversion preserves the exact variant and custom code.
        // App-provided context is intentionally replaced by this operation
        // label before it crosses the generated binding boundary.
        PaykitFfiError::NotFound {
            code,
            context: _reason,
        } => PaykitSdkError::NotFound {
            context: format!("{context}: {code}"),
            source,
        },
        PaykitFfiError::Protocol {
            code,
            context: _reason,
        } => PaykitSdkError::Protocol {
            context: format!("{context}: {code}"),
            source,
        },
        PaykitFfiError::Policy {
            code,
            context: _reason,
        } => PaykitSdkError::Policy {
            context: format!("{context}: {code}"),
            source,
        },
        PaykitFfiError::PaymentAdapter {
            code,
            context: _reason,
        } => PaykitSdkError::PaymentAdapter {
            context: format!("{context}: {code}"),
            source,
        },
        PaykitFfiError::RecoveryRequired {
            code,
            context: _reason,
        } => PaykitSdkError::RecoveryRequired {
            context: format!("{context}: {code}"),
            source,
        },
    }
}

pub(crate) fn storage_error(code: impl Into<String>, context: impl Into<String>) -> PaykitFfiError {
    PaykitFfiError::Storage {
        code: code.into(),
        context: context.into(),
    }
}

pub(crate) fn identity_error(
    code: impl Into<String>,
    context: impl Into<String>,
) -> PaykitFfiError {
    PaykitFfiError::Identity {
        code: code.into(),
        context: context.into(),
    }
}

pub(crate) fn validation_error(reason: impl Into<String>) -> PaykitFfiError {
    PaykitFfiError::Protocol {
        code: "validation".into(),
        context: reason.into(),
    }
}

#[cfg(test)]
mod tests;
