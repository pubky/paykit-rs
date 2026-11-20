use paykit_sdk::{PaykitError, Refusal};
use pubky_switch_client::{app::AppError, error::StoreError};
use std::sync::Arc;

/// A failure returned to the host without losing the original refusal or storage outcome.
#[derive(Debug, Clone, thiserror::Error, uniffi::Error)]
pub enum WalletError {
    /// An input supplied by the host failed validation.
    #[error("invalid {field}")]
    InvalidInput {
        /// Name of the invalid field, without its contents.
        field: String,
    },
    /// The SDK has no installed wallet under this App ID.
    #[error("no wallet with this app id")]
    NotJoined,
    /// The restored app awaits the host's answer about use after this backup.
    #[error("the restore question is open")]
    RestoreQuestionOpen,
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
    /// The wallet handle was shut down or its in-flight mutation was cancelled.
    #[error("the wallet is closed")]
    Closed,
    /// The payment rules prevent this action on the evidence currently available.
    #[error("payment refused")]
    Refused {
        /// The original refusal with any evidence required for a subsequent confirmation.
        refusal: Arc<PaymentRefusal>,
    },
    /// The payment method could not complete its operation.
    #[error("payment method: {failure}")]
    Method {
        /// The payment method's classified failure.
        failure: crate::MethodError,
    },
    /// The native payment method returned a failure this binding cannot classify.
    #[error("unknown payment method failure")]
    UnknownMethod,
    /// A Paykit or Switch operation failed.
    #[error("wallet operation failed: {detail}")]
    Operation {
        /// The subsystem or rule that reported the failure.
        kind: WalletFailureKind,
        /// The native explanation suitable for reporting the failure.
        detail: String,
        /// The service error code, when returned.
        service_code: Option<String>,
        /// The retained Switch batch ID, when available for later reconciliation.
        kept_batch: Option<Vec<u8>>,
    },
}
impl WalletError {
    pub(crate) fn invalid(s: &str) -> Self {
        Self::InvalidInput { field: s.into() }
    }
}
/// Source of a wallet operation failure.
#[derive(Clone, Debug, uniffi::Enum)]
pub enum WalletFailureKind {
    /// The Switch client could not complete the operation.
    Client,
    /// A change to the wallet's public payment endpoints still awaits publication.
    PublicEndpointChangePending,
    /// The Pubky Homeserver operation failed.
    Homeserver,
    /// The message, record or operation failed a protocol rule.
    InvalidProtocolData,
    /// The state used for the operation changed and must be read again.
    Conflict,
    /// A native case that this binding does not recognize.
    Other,
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
impl From<AppError> for WalletError {
    fn from(e: AppError) -> Self {
        PaykitError::Client(e).into()
    }
}
impl From<PaykitError> for WalletError {
    fn from(e: PaykitError) -> Self {
        let kind = match &e {
            PaykitError::Refused(r) => {
                return Self::Refused {
                    refusal: Arc::new(PaymentRefusal(r.clone())),
                }
            }
            PaykitError::Client(AppError::Refused(
                pubky_switch_client::app::Refusal::NotJoined,
            )) => return Self::NotJoined,
            PaykitError::Client(AppError::Refused(
                pubky_switch_client::app::Refusal::RestoreQuestionOpen,
            )) => return Self::RestoreQuestionOpen,
            PaykitError::Client(AppError::Store(e)) => return e.clone().into(),
            PaykitError::Client(AppError::Protocol(failure)) => {
                return failure.error().clone().into()
            }
            PaykitError::Client(_) => WalletFailureKind::Client,
            PaykitError::PublicEndpointChangePending(_) => {
                WalletFailureKind::PublicEndpointChangePending
            }
            PaykitError::Rail(error) => {
                return Self::Method {
                    failure: match error {
                        paykit_sdk::RailError::Unavailable(r) => {
                            crate::MethodError::Unavailable { reason: r.clone() }
                        }
                        paykit_sdk::RailError::Refused(r) => {
                            crate::MethodError::Refused { reason: r.clone() }
                        }
                        paykit_sdk::RailError::InvalidSelection(r) => {
                            crate::MethodError::InvalidSelection { reason: r.clone() }
                        }
                        _ => return Self::UnknownMethod,
                    },
                }
            }
            PaykitError::Lib(_) => WalletFailureKind::InvalidProtocolData,
            PaykitError::Conflict(_) => WalletFailureKind::Conflict,
            PaykitError::Homeserver(_) => WalletFailureKind::Homeserver,
            _ => WalletFailureKind::Other,
        };
        Self::Operation {
            kind,
            detail: e.to_string(),
            service_code: e.code().map(|code| code.as_str().into()),
            kept_batch: e.kept_batch().map(Vec::from),
        }
    }
}

/// The exact payment refusal and its confirmation evidence retained by the SDK.
#[derive(Debug, uniffi::Object)]
pub struct PaymentRefusal(pub(crate) Refusal);

#[uniffi::export]
impl PaymentRefusal {
    /// Return the native explanation for display.
    pub fn reason(&self) -> String {
        self.0.to_string()
    }
    /// For a failed earlier attempt, return the underlying reason when available.
    pub fn attempted_failure(&self) -> Option<Arc<Self>> {
        match &self.0 {
            Refusal::AttemptFailed(r) => Some(Arc::new(Self(*r.clone()))),
            _ => None,
        }
    }
}
