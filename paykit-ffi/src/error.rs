use pubky_switch_client::error::StoreError;

/// An enrollment or protected-storage failure returned to the host.
#[derive(Debug, Clone, thiserror::Error, uniffi::Error)]
pub enum WalletError {
    /// Paykit's payer scope rules refused the generated app request.
    #[error("invalid Paykit request: {reason}")]
    Protocol {
        /// The rule that refused the request.
        reason: String,
    },
    /// An input supplied by the host failed validation.
    #[error("invalid {field}")]
    InvalidInput {
        /// Name of the invalid field, without its contents.
        field: String,
    },
    /// This enrollment reference has no pending request for the operation.
    #[error("the enrollment is absent or already complete")]
    EnrollmentUnavailable,
    /// The reference already belongs to another authentication request or approval.
    #[error("the enrollment is bound to another approval or authentication request")]
    EnrollmentConflict,
    /// The approval failed verification against the chosen identity and authentication request.
    #[error("approval verification failed")]
    ApprovalRefused,
    /// The native secure random generator could not produce the required key material.
    #[error("secure randomness unavailable")]
    RandomUnavailable,
    /// The protected store failed or another writer owns it.
    #[error("storage: {failure}")]
    Storage {
        /// The original storage failure, including an unknown commit outcome.
        failure: crate::StorageError,
    },
}
impl WalletError {
    pub(crate) fn invalid(s: &str) -> Self {
        Self::InvalidInput { field: s.into() }
    }
}
impl From<StoreError> for WalletError {
    fn from(e: StoreError) -> Self {
        use crate::StorageError as S;
        let failure = match e {
            StoreError::InUse => S::InUse,
            StoreError::NotOwner => S::NotOwner,
            StoreError::Unavailable => S::Unavailable,
            StoreError::Unknown => S::Unknown,
            StoreError::Full => S::Full,
            StoreError::Corrupt => S::Corrupt,
            StoreError::TooLarge { size, cap, .. } => S::TooLarge { size, cap },
            _ => S::Other,
        };
        Self::Storage { failure }
    }
}
