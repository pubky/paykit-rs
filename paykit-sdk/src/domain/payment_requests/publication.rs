use super::*;
use crate::{storage::NewOutboundPrivateMessage, PaykitSdkError};

/// Durable local outcome of publishing a Payment Request proposal.
///
/// These outcomes do not establish ownership of any public image in the terms.
#[derive(Debug)]
#[non_exhaustive]
pub enum PaymentRequestPublication {
    /// The exact proposal is in the durable queue, possibly already sent.
    Queued {
        /// Existing or newly assigned outbound message ID.
        outbound_message_id: u64,
    },
    /// No matching proposal was queued when the transaction completed.
    NotQueued {
        /// Readiness or validation failure that prevented insertion.
        error: PaykitSdkError,
    },
    /// Storage could not establish the outcome, or the IDs conflict.
    /// Retry with the same IDs and terms after resolving the error.
    Uncertain {
        /// Failure that prevented a conclusive result.
        error: PaykitSdkError,
    },
}

pub(crate) async fn publish_payment_request<S: StorageAdapter>(
    storage: &S,
    counterparty: PubkyPublicKey,
    receiver_path: PaykitReceiverPath,
    event: &PaymentRequest,
    readiness: Result<()>,
    now: DateTime<Utc>,
) -> PaymentRequestPublication {
    let serialized = serialize_payment_request_event(&PaymentRequestEvent::Request(event.clone()))
        .map_err(PaykitSdkError::from)
        .and_then(|raw_json| {
            crate::domain::outbound_private::validate_outbound_private_message(&raw_json)?;
            Ok(raw_json)
        });
    let result = storage
        .transaction(|tx| {
            // Reconcile and insert atomically. Keep sent records as dedupe evidence.
            // A storage callback may commit and then return an error, so every
            // transaction error remains uncertain until a later successful lookup.
            for record in tx.outbound_private_messages(&counterparty, &receiver_path) {
                let value: JsonValue = serde_json::from_str(&record.raw_json).map_err(|_| {
                    PaykitSdkError::Protocol {
                        context: "cannot reconcile malformed outbound message".into(),
                        source: None,
                    }
                })?;
                let same_event = value["event_id"].as_str() == Some(event.event_id.as_str());
                let same_request = record.kind == "paykit.payment_request"
                    && value["payment_request_id"].as_str()
                        == Some(event.payment_request_id.as_str());
                if same_event || same_request {
                    if serialized
                        .as_ref()
                        .is_ok_and(|json| json == &record.raw_json)
                    {
                        return Ok(PaymentRequestPublication::Queued {
                            outbound_message_id: record.outbound_message_id,
                        });
                    }
                    return Err(PaykitSdkError::Protocol {
                        context: "proposal IDs already belong to different outbound content".into(),
                        source: None,
                    });
                }
            }
            let raw_json = match serialized {
                Ok(json) => json,
                Err(error) => return Ok(PaymentRequestPublication::NotQueued { error }),
            };
            if let Err(error) = readiness {
                return Ok(PaymentRequestPublication::NotQueued { error });
            }
            let record = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty,
                receiver_path,
                "paykit.payment_request".into(),
                raw_json,
                now,
            ));
            Ok(PaymentRequestPublication::Queued {
                outbound_message_id: record.outbound_message_id,
            })
        })
        .await;
    result.unwrap_or_else(|error| PaymentRequestPublication::Uncertain { error })
}
