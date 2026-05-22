//! Typed Paykit paths on Pubky Routing.

use std::fmt;

use crate::{PaykitError, PaymentEndpointIdentifier, PaymentReference, PublicKey, Result};

/// Conventional prefix for public Paykit data hosted on Pubky storage.
///
/// Public Payment Endpoints are stored at
/// `/pub/paykit/v0/{payment_endpoint_identifier}`.
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit/v0/";

/// Conventional prefix for private encrypted Paykit data.
///
/// Pubky-noise uses this as the base path for encrypted private application
/// messages. Receipt payloads are stored under this private namespace too.
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit/v0/private";

const RECEIPTS_SEGMENT: &str = "receipts";

/// Canonical Paykit path on a Pubky homeserver.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PaykitPath(String);

impl PaykitPath {
    fn new(path: String) -> Self {
        debug_assert!(path.starts_with(PAYKIT_PATH_PREFIX));
        Self(path)
    }

    /// Borrow the path string.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Build an addressed Pubky resource string for a remote owner.
    pub(crate) fn addressed(&self, owner: &PublicKey) -> String {
        format!("{owner}{}", self.0)
    }
}

impl AsRef<str> for PaykitPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for PaykitPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Canonical Payment List directory path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PublicPaymentListPath {
    path: PaykitPath,
}

impl PublicPaymentListPath {
    /// Local homeserver path for the payee-published Payment List directory.
    pub(crate) fn local() -> Self {
        Self {
            path: PaykitPath::new(PAYKIT_PATH_PREFIX.to_string()),
        }
    }

    /// Addressed Pubky resource for the payee-published Payment List directory.
    pub(crate) fn addressed(payee: &PublicKey) -> String {
        Self::local().path.addressed(payee)
    }

    /// Borrow the canonical path.
    #[cfg(test)]
    pub(crate) fn as_path(&self) -> &PaykitPath {
        &self.path
    }
}

/// Canonical public Payment Endpoint path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PublicPaymentEndpointPath {
    identifier: PaymentEndpointIdentifier,
    path: PaykitPath,
}

impl PublicPaymentEndpointPath {
    /// Local homeserver path for a public Payment Endpoint.
    pub(crate) fn local(identifier: &PaymentEndpointIdentifier) -> Self {
        Self {
            identifier: identifier.clone(),
            path: PaykitPath::new(format!("{PAYKIT_PATH_PREFIX}{}", identifier.as_str())),
        }
    }

    /// Addressed Pubky resource for a payee's public Payment Endpoint.
    pub(crate) fn addressed(payee: &PublicKey, identifier: &PaymentEndpointIdentifier) -> String {
        Self::local(identifier).path.addressed(payee)
    }

    /// Extract and validate the Payment Endpoint Identifier from a listed Pubky resource path.
    pub(crate) fn identifier_from_resource_path(path: &str) -> Result<PaymentEndpointIdentifier> {
        let Some(identifier) = path.strip_prefix(PAYKIT_PATH_PREFIX) else {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "resource path '{path}' is outside Paykit Payment Endpoint prefix '{PAYKIT_PATH_PREFIX}'"
                ),
                source: None,
            });
        };

        if identifier.is_empty() || identifier.contains('/') {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "cannot extract Payment Endpoint Identifier from resource path '{path}'"
                ),
                source: None,
            });
        }

        PaymentEndpointIdentifier::new(identifier).map_err(|err| PaykitError::InvalidData {
            context: format!("storage returned invalid Payment Endpoint Identifier '{identifier}'"),
            source: Some(err.into()),
        })
    }

    /// Borrow the Payment Endpoint Identifier used by this path.
    #[cfg(test)]
    pub(crate) fn identifier(&self) -> &PaymentEndpointIdentifier {
        &self.identifier
    }

    /// Borrow the canonical path.
    pub(crate) fn as_path(&self) -> &PaykitPath {
        &self.path
    }
}

/// Canonical encrypted Receipt payload path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ReceiptPayloadPath {
    reference: PaymentReference,
    path: PaykitPath,
}

impl ReceiptPayloadPath {
    /// Local homeserver path for an encrypted Receipt payload.
    pub(crate) fn local(reference: &PaymentReference) -> Self {
        Self {
            reference: reference.clone(),
            path: PaykitPath::new(format!(
                "{PAYKIT_PRIVATE_PATH_PREFIX}/{RECEIPTS_SEGMENT}/{}",
                reference.as_str()
            )),
        }
    }

    /// Borrow the Payment Reference used by this path.
    #[cfg(test)]
    pub(crate) fn reference(&self) -> &PaymentReference {
        &self.reference
    }

    /// Borrow the canonical path.
    pub(crate) fn as_path(&self) -> &PaykitPath {
        &self.path
    }
}
