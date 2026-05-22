//! Pubky Routing paths and public storage helpers.

pub(crate) mod paths;
pub(crate) mod public_storage;

#[cfg(test)]
mod tests {
    use crate::{PaykitError, PaymentEndpointIdentifier, PaymentReference};

    #[test]
    fn payment_endpoint_path_is_canonical() {
        let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let path = super::paths::PublicPaymentEndpointPath::local(&identifier);

        assert_eq!(
            path.as_path().as_str(),
            "/pub/paykit/v0/btc-lightning-bolt11"
        );
        assert_eq!(path.identifier(), &identifier);
    }

    #[test]
    fn payment_list_path_is_canonical() {
        let path = super::paths::PublicPaymentListPath::local();

        assert_eq!(path.as_path().as_str(), "/pub/paykit/v0/");
    }

    #[test]
    fn receipt_payload_path_is_canonical() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let path = super::paths::ReceiptPayloadPath::local(&reference);

        assert_eq!(
            path.as_path().as_str(),
            "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(path.reference(), &reference);
    }

    #[test]
    fn identifier_from_resource_path_rejects_directory_path() {
        let err = super::paths::PublicPaymentEndpointPath::identifier_from_resource_path(
            "/pub/paykit/v0/",
        )
        .unwrap_err();

        assert!(matches!(err, PaykitError::InvalidData { .. }));
    }

    #[test]
    fn identifier_from_resource_path_rejects_invalid_identifier() {
        let err = super::paths::PublicPaymentEndpointPath::identifier_from_resource_path(
            "/pub/paykit/v0/foo/bar",
        )
        .unwrap_err();

        assert!(matches!(err, PaykitError::InvalidData { .. }));
    }
}
