use paykit_sdk::PaykitSdkError;

/// Error type exposed through generated bindings.
#[derive(uniffi::Error, Clone, Debug, thiserror::Error)]
pub enum PaykitFfiError {
    /// Durable storage failed.
    #[error("storage/{code}: {context}")]
    Storage { code: String, context: String },
    /// Pubky identity, session, or key capability failed.
    #[error("identity/{code}: {context}")]
    Identity { code: String, context: String },
    /// Pubky or Encrypted Link transport failed.
    #[error("transport/{code}: {context}")]
    Transport { code: String, context: String },
    /// Requested Paykit or Pubky resource was not found.
    #[error("not_found/{code}: {context}")]
    NotFound { code: String, context: String },
    /// Paykit protocol data is invalid, conflicting, or unsupported.
    #[error("protocol/{code}: {context}")]
    Protocol { code: String, context: String },
    /// Operation is blocked by configured SDK policy.
    #[error("policy/{code}: {context}")]
    Policy { code: String, context: String },
    /// Payment adapter failed.
    #[error("payment_adapter/{code}: {context}")]
    PaymentAdapter { code: String, context: String },
    /// Local state needs explicit recovery before automation can continue.
    #[error("recovery_required/{code}: {context}")]
    RecoveryRequired { code: String, context: String },
}

impl From<PaykitSdkError> for PaykitFfiError {
    fn from(err: PaykitSdkError) -> Self {
        match err {
            PaykitSdkError::Storage { context, source } => callback_ffi_error(source.as_ref())
                .unwrap_or_else(|| Self::Storage {
                    code: "storage_error".into(),
                    context: format_context(context, source),
                }),
            PaykitSdkError::Identity { context, source } => callback_ffi_error(source.as_ref())
                .unwrap_or_else(|| Self::Identity {
                    code: "identity_error".into(),
                    context: format_context(context, source),
                }),
            PaykitSdkError::Transport { context, source } => callback_ffi_error(source.as_ref())
                .unwrap_or_else(|| Self::Transport {
                    code: "transport_error".into(),
                    context: format_context(context, source),
                }),
            PaykitSdkError::NotFound(context) => Self::NotFound {
                code: "not_found".into(),
                context,
            },
            PaykitSdkError::Protocol(context) => Self::Protocol {
                code: "protocol_error".into(),
                context,
            },
            PaykitSdkError::Policy(context) => Self::Policy {
                code: "policy_error".into(),
                context,
            },
            PaykitSdkError::PaymentAdapter { context, source } => {
                callback_ffi_error(source.as_ref()).unwrap_or_else(|| Self::PaymentAdapter {
                    code: "payment_adapter_error".into(),
                    context: format_context(context, source),
                })
            }
            PaykitSdkError::RecoveryRequired(context) => Self::RecoveryRequired {
                code: "recovery_required".into(),
                context,
            },
            _ => Self::Protocol {
                code: "unsupported_sdk_error".into(),
                context: "unsupported SDK error".into(),
            },
        }
    }
}

fn format_context(context: String, source: Option<anyhow::Error>) -> String {
    match source {
        Some(source) => format!("{context}: {source:#}"),
        None => context,
    }
}

fn callback_ffi_error(source: Option<&anyhow::Error>) -> Option<PaykitFfiError> {
    source.and_then(|source| source.downcast_ref::<PaykitFfiError>().cloned())
}

pub(crate) fn ffi_error_to_sdk(err: PaykitFfiError, context: &'static str) -> PaykitSdkError {
    let source = Some(anyhow::Error::new(err.clone()));
    match err {
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
        PaykitFfiError::NotFound {
            code,
            context: _reason,
        } => PaykitSdkError::NotFound(format!("{context}: {code}")),
        PaykitFfiError::Protocol {
            code,
            context: _reason,
        } => PaykitSdkError::Protocol(format!("{context}: {code}")),
        PaykitFfiError::Policy {
            code,
            context: _reason,
        } => PaykitSdkError::Policy(format!("{context}: {code}")),
        PaykitFfiError::PaymentAdapter {
            code,
            context: _reason,
        } => PaykitSdkError::PaymentAdapter {
            context: format!("{context}: {code}"),
            source: None,
        },
        PaykitFfiError::RecoveryRequired {
            code,
            context: _reason,
        } => PaykitSdkError::RecoveryRequired(format!("{context}: {code}")),
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
