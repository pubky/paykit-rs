//! App records: Paykit's members of a Switch app record (Paykit spec 4, Paykit wire 3.2).
//!
//! Paykit's `capabilities` and `public_endpoints`, and the rules of a record carrying public
//! endpoints (its signed `expires_at`, the identity's record age limit, renewal at half its
//! lifetime), are moving under the Switch app record's `protocols.paykit` through a Paykit
//! spec change. Until that change lands, this SDK neither publishes nor reads them:
//!
//! - **Reading** a payee's public endpoints is [`public_endpoints`]: a payment in
//!   [`crate::PaymentMode::Public`] is refused with
//!   [`crate::Refusal::PublicEndpointsUnavailable`] there. Once the members are specified, it
//!   reads the payee's current apps' records from the Homeserver (`AppRecord::protocol` gives
//!   `protocols.paykit`), refuses expired records and those past the record age limit, and
//!   returns their endpoints for the destination check before paying.
//! - **Publishing** this app's members is [`record_members`]: none yet, so apps keep
//!   `AppConfig::publishes_record` off.

use pubky_switch_core::records::AppRecord;
use serde_json::{Map, Value};

use crate::error::{Refusal, Result};

/// A public endpoint of a payee app, from its app record (Paykit spec 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicEndpoint {
    /// The payee app that published it.
    pub app_id: [u8; 16],
    /// The Payment Endpoint Identifier.
    pub endpoint_id: paykit_lib::PaymentEndpointIdentifier,
    /// The payload, which only the rail interprets.
    pub payload: paykit_lib::PaymentEndpointPayload,
}

/// The public endpoints of `payee`'s current apps (Paykit spec 4). Not read yet: the Paykit
/// members of app records are being respecified under `protocols.paykit`.
///
/// # Errors
///
/// [`Refusal::PublicEndpointsUnavailable`] always, for now.
pub fn public_endpoints(payee: &[u8; 32]) -> Result<Vec<PublicEndpoint>> {
    let _ = payee;
    Err(Refusal::PublicEndpointsUnavailable.into())
}

/// The Paykit members this app publishes in its app record's `protocols.paykit` (Paykit spec
/// 4): none yet.
pub fn record_members(record: &AppRecord) -> Option<Map<String, Value>> {
    let _ = record;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_records_public_endpoints_are_refused_for_now() {
        let error = public_endpoints(&[1; 32]).unwrap_err();
        assert_eq!(error.refusal(), Some(&Refusal::PublicEndpointsUnavailable));
    }
}
