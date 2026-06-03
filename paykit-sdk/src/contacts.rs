//! Contact records and contact payment resolution types.

/// Result category for contact payment resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContactPaymentResolutionStatus {
    /// A payable endpoint was found.
    Payable,
    /// No endpoint was found.
    NoEndpoint,
    /// Endpoints exist but are unsupported.
    UnsupportedEndpoint,
    /// Private recovery is still in progress.
    PrivateRecoveryPending,
    /// The local identity cannot establish private links.
    PublicOnlySession,
}
