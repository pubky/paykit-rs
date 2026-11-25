//! Paykit's error type.

/// A failure of one of Paykit's rules, or of a Dispatch rule Paykit relies on.
///
/// Messages name the Paykit section of the rule that failed, such as `Paykit wire 1.2`.
/// They never quote decrypted plaintext, secret keys or the text of a value that failed to
/// parse: they name the member and the rule instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Caller-supplied input that fails a structural check, such as an identifier built
    /// with an invalid value. A bug in the caller's own input, not in a counterparty's.
    #[error("validation: {0}")]
    Validation(String),
    /// Data from the network, from decrypted plaintext or from shared state that breaks one
    /// of Paykit's rules: a misbehaving counterparty, own app or service.
    #[error("invalid: {0}")]
    Invalid(String),
    /// A failure from `pubky-dispatch-core`, carried as the Dispatch reports it: a Dispatch format
    /// that doesn't parse, a signature that doesn't verify, or a crypto failure.
    #[error("dispatch: {0}")]
    Dispatch(#[from] pubky_dispatch_core::Error),
}

impl Error {
    /// The same failure, as a caller's own input that fails a structural check.
    ///
    /// A Dispatch failure stays a Dispatch failure: it is carried as the Dispatch reports it.
    pub fn into_validation(self) -> Self {
        match self {
            Self::Invalid(why) => Self::Validation(why),
            other => other,
        }
    }
}

/// Paykit's result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Shorthand for an [`Error::Invalid`].
pub(crate) fn invalid<T>(why: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(why.into()))
}

/// Fail with an [`Error::Invalid`] unless `condition` holds.
pub(crate) fn ensure(condition: bool, why: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        invalid(why)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_into_validation_moves_invalid_only() {
        assert_eq!(
            Error::Invalid("rule".into()).into_validation(),
            Error::Validation("rule".into())
        );
        let dispatch = Error::Dispatch(pubky_dispatch_core::Error::Invalid("rule".into()));
        assert_eq!(dispatch.clone().into_validation(), dispatch);
    }

    #[test]
    fn test_error_ensure_refuses_a_false_condition() {
        assert!(ensure(true, "rule").is_ok());
        assert_eq!(ensure(false, "rule"), Err(Error::Invalid("rule".into())));
    }
}
