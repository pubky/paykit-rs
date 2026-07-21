use paykit_lib::{PaymentEndpointIdentifier, PaymentRequestId};

use crate::{errors::validation_error, PaykitFfiError};

pub(crate) fn parse_payment_request_id(value: String) -> Result<PaymentRequestId, PaykitFfiError> {
    PaymentRequestId::new(value).map_err(|err| validation_error(err.to_string()))
}

pub(crate) fn parse_endpoint_identifier(
    value: String,
) -> Result<PaymentEndpointIdentifier, PaykitFfiError> {
    PaymentEndpointIdentifier::new(value).map_err(|err| validation_error(err.to_string()))
}
