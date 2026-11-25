//! Public Payment Endpoints on the app record (Paykit spec 4) and outbox delivery (Dispatch spec 11).

use crate::{NativeWallet, WalletError};
use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_dispatch_client::{app::Publication, transport::TransportError};
use pubky_dispatch_core::http::ErrorCode;
use std::{collections::BTreeMap, fmt};

/// A public Payment Endpoint in the app's signed record (Paykit spec 4).
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct PublicEndpoint {
    /// The Payment Endpoint Identifier (Paykit wire 1.4).
    pub identifier: String,
    /// The opaque Payment Endpoint Payload (Paykit wire 1.4).
    pub payload: String,
}

impl fmt::Debug for PublicEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicEndpoint")
            .field("identifier", &self.identifier)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

fn endpoint_identifier(identifier: &str) -> Result<PaymentEndpointIdentifier, WalletError> {
    PaymentEndpointIdentifier::new(identifier).map_err(|_| WalletError::invalid("identifier"))
}

fn endpoint_payload(payload: &str) -> Result<PaymentEndpointPayload, WalletError> {
    PaymentEndpointPayload::new(payload).map_err(|_| WalletError::invalid("payload"))
}

/// Where the app's signed record stands on its Homeserver (Dispatch spec 4.2).
#[derive(Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PublicationState {
    /// The app publishes its record only to the Dispatch (Dispatch spec 4.2).
    NotPublishing,
    /// The app knows of no record accepted by the Dispatch (Dispatch spec 4.2).
    NothingAccepted,
    /// The Homeserver holds the latest accepted record (Dispatch spec 4.2).
    Published,
    /// A newer local record choice must settle before publication (Dispatch spec 4.2).
    AwaitingRecord,
    /// The Homeserver already holds a newer record (Dispatch spec 4.2).
    Superseded,
    /// A failed Homeserver read or write leaves publication owed (Dispatch spec 4.2).
    Owed {
        /// The transport failure code (Dispatch spec 4.2).
        error: String,
    },
    /// The read to check the Homeserver copy failed (Dispatch spec 4.2).
    Unchecked {
        /// The transport failure code (Dispatch spec 4.2).
        error: String,
    },
    /// The missing Homeserver copy awaits a read confirming the app is current (Dispatch spec 4.2).
    AwaitingRead,
    /// Any publication state added by the Dispatch client maps here without content (Dispatch spec 4.2).
    Other,
}

impl From<Publication> for PublicationState {
    fn from(state: Publication) -> Self {
        match state {
            Publication::NotPublishing => Self::NotPublishing,
            Publication::NothingAccepted => Self::NothingAccepted,
            Publication::Published => Self::Published,
            Publication::AwaitingRecord => Self::AwaitingRecord,
            Publication::Superseded => Self::Superseded,
            Publication::Owed { error } => Self::Owed {
                error: transport_code(error).into(),
            },
            Publication::Unchecked { error } => Self::Unchecked {
                error: transport_code(error).into(),
            },
            Publication::AwaitingRead => Self::AwaitingRead,
            _ => Self::Other,
        }
    }
}

fn transport_code(error: TransportError) -> &'static str {
    match error {
        TransportError::FirstSendUnavailable => "first_send_unavailable",
        TransportError::FirstSendExpired => "first_send_expired",
        TransportError::Refused(_) => "refused",
        TransportError::Connect(_) => "connect",
        TransportError::Timeout => "timeout",
        TransportError::TooLarge(_) => "too_large",
        TransportError::InvalidRequest(_) => "invalid_request",
        TransportError::UnreadableSignedHeader { .. } => "unreadable_signed_header",
        _ => "other",
    }
}

/// Where a message sent by this app stands (Dispatch spec 11).
#[derive(Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MessageDelivery {
    /// The client keeps the signed batch that queues the message (Dispatch wire 6.4).
    Kept {
        /// The 16-byte ID of the kept batch (Dispatch wire 6.4).
        batch_id: Vec<u8>,
    },
    /// The message remains in the outbox (Dispatch spec 11).
    Queued {
        /// How many times delivery has been tried (Dispatch spec 11).
        tries: u64,
        /// The last failure code, or `unknown` for an unrecognized failure (Dispatch spec 11).
        last_error: Option<String>,
    },
    /// The recipient's Dispatch is waiting for admission (Dispatch spec 7).
    WaitingForAdmission {
        /// The first refusal, in Unix milliseconds on the app clock (Dispatch spec 11).
        since_ms: u64,
        /// How many times delivery has been tried (Dispatch spec 11).
        tries: u64,
    },
    /// The sender's own certificate must become current (Dispatch wire 3.8).
    CertificateNotCurrent {
        /// The first refusal, in Unix milliseconds on the app clock (Dispatch spec 11).
        since_ms: u64,
        /// How many times delivery has been tried (Dispatch spec 11).
        tries: u64,
    },
    /// The recipient's inbox accepted the message (Dispatch spec 11).
    Accepted {
        /// The acceptance time in Unix milliseconds (Dispatch spec 11).
        accepted_at_ms: u64,
    },
    /// The delivery ended or its kept batch was dropped (Dispatch spec 11, Dispatch wire 6.4).
    /// A retry may be refused after an accepted try lost its answer. Check the request or
    /// record before sending a replacement (Dispatch spec 11).
    Failed {
        /// The reason code, or `unknown` for an unrecognized reason (Dispatch spec 11).
        reason: String,
        /// The failure time in Unix milliseconds on the app clock (Dispatch spec 11).
        failed_at_ms: u64,
    },
    /// A delivery state the binding does not recognize, without content (Dispatch spec 11).
    Other,
}

impl From<paykit_sdk::MessageDelivery> for MessageDelivery {
    fn from(delivery: paykit_sdk::MessageDelivery) -> Self {
        use paykit_sdk::MessageDelivery as D;
        match delivery {
            D::Kept { batch_id } => Self::Kept {
                batch_id: batch_id.into(),
            },
            D::Queued { tries, last_error } => Self::Queued {
                tries,
                last_error: last_error.map(|error| delivery_code(&error).into()),
            },
            D::WaitingForAdmission { since_ms, tries } => {
                Self::WaitingForAdmission { since_ms, tries }
            }
            D::CertificateNotCurrent { since_ms, tries } => {
                Self::CertificateNotCurrent { since_ms, tries }
            }
            D::Accepted { accepted_at_ms } => Self::Accepted { accepted_at_ms },
            D::Failed {
                reason,
                failed_at_ms,
            } => Self::Failed {
                reason: delivery_code(&reason).into(),
                failed_at_ms,
            },
            _ => Self::Other,
        }
    }
}

// The outbox can store a formatted Dispatch answer, including the service's message.
// Only a recognized code crosses the binding (Dispatch wire 7.6).
fn delivery_code(error: &str) -> &'static str {
    match error {
        "the batch carrying it was dropped" => "batch_dropped",
        "not sealed" => "not_sealed",
        "timed out" => "timeout",
        _ => {
            let error = error
                .strip_prefix("the batch carrying it was refused: ")
                .unwrap_or(error);
            let header = error.split_once(':').map_or(error, |(header, _)| header);
            let checked = |name: &str, status: &str| {
                ErrorCode::parse(name)
                    .filter(|code| Some(code.status()) == status.parse::<u16>().ok())
            };
            if let Some((name, status)) = header
                .strip_suffix(')')
                .and_then(|s| s.split_once(" before the body was read (status "))
            {
                return checked(name, status).map_or("unknown", ErrorCode::as_str);
            }
            let code = match header.split_once(' ') {
                Some((status, name)) => checked(name, status),
                None => ErrorCode::parse(header),
            };
            code.map_or("unknown", ErrorCode::as_str)
        }
    }
}

#[uniffi::export]
impl NativeWallet {
    /// List the latest signed public Payment Endpoints in identifier order (Paykit spec 4).
    pub async fn own_public_endpoints(&self) -> Result<Vec<PublicEndpoint>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .own_public_endpoints()?
                    .into_iter()
                    .map(|(id, payload)| PublicEndpoint {
                        identifier: id.to_string(),
                        payload: payload.into_inner(),
                    })
                    .collect())
            })
        })
        .await
    }

    /// Publish or replace one public Payment Endpoint (Paykit spec 4).
    pub async fn set_public_endpoint(
        &self,
        identifier: String,
        payload: String,
    ) -> Result<PublicationState, WalletError> {
        let identifier = endpoint_identifier(&identifier)?;
        let payload = endpoint_payload(&payload)?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.set_public_endpoint(identifier, payload).await?.into()) })
        })
        .await
    }

    /// Withdraw a public Payment Endpoint by its identifier (Paykit spec 4).
    pub async fn remove_public_endpoint(
        &self,
        identifier: String,
    ) -> Result<PublicationState, WalletError> {
        let identifier = endpoint_identifier(&identifier)?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.remove_public_endpoint(&identifier).await?.into()) })
        })
        .await
    }

    /// Replace the public Payment Endpoints with an optional Unix-millisecond cutoff (Paykit spec 4).
    pub async fn set_public_endpoints(
        &self,
        endpoints: Vec<PublicEndpoint>,
        expires_at_ms: Option<u64>,
    ) -> Result<PublicationState, WalletError> {
        let mut offer = BTreeMap::new();
        for endpoint in endpoints {
            let id = endpoint_identifier(&endpoint.identifier)?;
            let payload = endpoint_payload(&endpoint.payload)?;
            if offer.insert(id, payload).is_some() {
                return Err(WalletError::invalid("identifier"));
            }
        }
        self.with_wallet(move |app| {
            Box::pin(
                async move { Ok(app.set_public_endpoints(offer, expires_at_ms).await?.into()) },
            )
        })
        .await
    }

    /// Refresh the app's own record and retry any publication owed (Dispatch spec 4.2).
    pub async fn update_own_record(&self) -> Result<PublicationState, WalletError> {
        self.with_wallet(|app| Box::pin(async move { Ok(app.update_own_record().await?.into()) }))
            .await
    }

    /// Try queued outbox messages again (Dispatch spec 11).
    pub async fn flush(&self) -> Result<(), WalletError> {
        self.with_wallet(|app| Box::pin(async move { Ok(app.flush().await?) }))
            .await
    }

    /// Read the delivery retained for a message ID, or none if the client keeps no entry, including
    /// after retention expires or after a restore (Dispatch spec 11, 12.4).
    pub async fn message_delivery(
        &self,
        msg_id: String,
    ) -> Result<Option<MessageDelivery>, WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.message_delivery(&msg_id).await?.map(Into::into)) })
        })
        .await
    }
}
