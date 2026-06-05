//! Payment Endpoint Reservation records.

use std::{collections::HashMap, fmt};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::{
    adapters::{PaymentEndpointReservation, PaymentEndpointReservationRequest, ReceivingDetail},
    endpoints::normalize_receiving_details,
    outbound_private::validate_outbound_private_message,
    storage::{
        NewOutboundPrivateMessage, OutboundPrivateMessageRecord, PaymentEndpointReservationRecord,
        StorageAdapter,
    },
    PaykitSdkError, PubkyPublicKey, Result,
};
use paykit_lib::{serialize_private_payment_list_json, PrivatePaymentList};

/// Load Payment Endpoint Reservation records for one counterparty.
pub(crate) async fn payment_endpoint_reservations<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<PaymentEndpointReservationRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.payment_endpoint_reservations(counterparty)))
        .await
}

/// Queue a Private Payment List and persist linked reservation records atomically.
pub(crate) async fn queue_private_payment_list_with_reservations<S>(
    storage: &S,
    request: &PaymentEndpointReservationRequest,
    reservations: Vec<PaymentEndpointReservation>,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let (receiving_details, drafts) = build_reservation_records(request, reservations, now)?;
    let payment_endpoints = normalize_receiving_details(receiving_details)?;
    let list = PrivatePaymentList::new(payment_endpoints);
    let raw_json = serialize_private_payment_list_json(&list)?;
    let kind = validate_outbound_private_message(&raw_json)?;
    let counterparty = request.counterparty.clone();

    storage
        .transaction(move |tx| {
            let mut checked_drafts = Vec::with_capacity(drafts.len());
            for draft in drafts {
                let existing =
                    tx.payment_endpoint_reservation(&draft.counterparty, &draft.reservation_id);
                if let Some(existing) = existing.as_ref() {
                    if !draft.matches_existing(existing) {
                        return Err(PaykitSdkError::Protocol(format!(
                            "Payment Endpoint Reservation id '{}' already exists with different details",
                            draft.reservation_id
                        )));
                    }
                }
                checked_drafts.push((draft, existing));
            }

            let outbound = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty,
                kind,
                raw_json,
                now,
            ));
            for (draft, existing) in checked_drafts {
                let record = draft.into_record(outbound.outbound_message_id, existing);
                tx.save_payment_endpoint_reservation(record);
            }
            Ok(outbound)
        })
        .await
}

#[derive(Clone)]
struct PaymentEndpointReservationRecordDraft {
    reservation_id: String,
    counterparty: PubkyPublicKey,
    identifier: String,
    payload_hash: String,
    attribution: HashMap<String, String>,
    expires_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl PaymentEndpointReservationRecordDraft {
    fn matches_existing(&self, existing: &PaymentEndpointReservationRecord) -> bool {
        existing.counterparty == self.counterparty
            && existing.identifier == self.identifier
            && existing.payload_hash == self.payload_hash
    }

    fn into_record(
        self,
        outbound_message_id: u64,
        existing: Option<PaymentEndpointReservationRecord>,
    ) -> PaymentEndpointReservationRecord {
        let (attribution, expires_at, created_at) = existing
            .map(|record| (record.attribution, record.expires_at, record.created_at))
            .unwrap_or((self.attribution, self.expires_at, self.created_at));
        PaymentEndpointReservationRecord {
            reservation_id: self.reservation_id,
            counterparty: self.counterparty,
            identifier: self.identifier,
            payload_hash: self.payload_hash,
            outbound_message_id,
            attribution,
            expires_at,
            created_at,
        }
    }
}

fn build_reservation_records(
    request: &PaymentEndpointReservationRequest,
    reservations: Vec<PaymentEndpointReservation>,
    now: DateTime<Utc>,
) -> Result<(
    Vec<ReceivingDetail>,
    Vec<PaymentEndpointReservationRecordDraft>,
)> {
    let mut reservation_ids = HashMap::new();
    let mut records = Vec::with_capacity(reservations.len());
    let mut receiving_details = Vec::with_capacity(reservations.len());

    for reservation in reservations {
        validate_reservation_id(&reservation.reservation_id)?;
        if reservation_ids
            .insert(reservation.reservation_id.clone(), ())
            .is_some()
        {
            return Err(PaykitSdkError::Protocol(format!(
                "duplicate Payment Endpoint Reservation id '{}'",
                reservation.reservation_id
            )));
        }
        receiving_details.push(reservation.receiving_detail.clone());
        records.push(record_from_reservation(request, reservation, now));
    }

    normalize_receiving_details(receiving_details.clone())?;
    Ok((receiving_details, records))
}

fn validate_reservation_id(reservation_id: &str) -> Result<()> {
    if reservation_id.trim().is_empty() {
        return Err(PaykitSdkError::Protocol(
            "Payment Endpoint Reservation id must not be empty".into(),
        ));
    }
    Ok(())
}

fn record_from_reservation(
    request: &PaymentEndpointReservationRequest,
    reservation: PaymentEndpointReservation,
    now: DateTime<Utc>,
) -> PaymentEndpointReservationRecordDraft {
    let payload_hash = reservation_payload_hash(&reservation.receiving_detail.payload);
    PaymentEndpointReservationRecordDraft {
        reservation_id: reservation.reservation_id,
        counterparty: request.counterparty.clone(),
        identifier: reservation.receiving_detail.identifier,
        payload_hash,
        attribution: reservation.attribution,
        expires_at: reservation.expires_at,
        created_at: now,
    }
}

pub(crate) fn reservation_payload_hash(payload: &str) -> String {
    let digest = Sha256::digest(payload.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl fmt::Debug for PaymentEndpointReservationRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentEndpointReservationRecord")
            .field("reservation_id", &self.reservation_id)
            .field("counterparty", &self.counterparty)
            .field("identifier", &self.identifier)
            .field("payload_hash", &self.payload_hash)
            .field("outbound_message_id", &self.outbound_message_id)
            .field(
                "attribution",
                &format_args!("<redacted:{} fields>", self.attribution.len()),
            )
            .field("expires_at", &self.expires_at)
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::storage::InMemoryStorage;
    use paykit_lib::PaymentEndpointIdentifier;

    fn timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
    }

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    fn request(counterparty: PubkyPublicKey) -> PaymentEndpointReservationRequest {
        PaymentEndpointReservationRequest { counterparty }
    }

    fn reservation(id: &str, payload: &str) -> PaymentEndpointReservation {
        PaymentEndpointReservation {
            reservation_id: id.into(),
            receiving_detail: ReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: payload.into(),
            },
            expires_at: None,
            attribution: HashMap::from([("contact".into(), "alice".into())]),
        }
    }

    #[tokio::test]
    async fn test_queue_private_payment_list_with_reservations_stores_linked_records() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let outbound = queue_private_payment_list_with_reservations(
            &storage,
            &request(counterparty.clone()),
            vec![reservation("res-1", "ln-secret")],
            timestamp(),
        )
        .await
        .unwrap();

        let list = paykit_lib::parse_private_payment_list_json(&outbound.raw_json).unwrap();
        let records = payment_endpoint_reservations(&storage, &counterparty)
            .await
            .unwrap();

        assert_eq!(
            list.get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap())
                .unwrap()
                .as_str(),
            "ln-secret"
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outbound_message_id, outbound.outbound_message_id);
        assert_ne!(records[0].payload_hash, "ln-secret");
        assert!(!format!("{:?}", records[0]).contains("ln-secret"));
        assert!(!format!("{:?}", records[0]).contains("alice"));
    }

    #[tokio::test]
    async fn test_queue_private_payment_list_with_reservations_rejects_duplicate_identifiers() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let result = queue_private_payment_list_with_reservations(
            &storage,
            &request(counterparty),
            vec![reservation("res-1", "one"), reservation("res-2", "two")],
            timestamp(),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
        assert!(storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .is_empty());
    }

    #[tokio::test]
    async fn test_queue_private_payment_list_with_reservations_reuses_existing_id() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        queue_private_payment_list_with_reservations(
            &storage,
            &request(counterparty.clone()),
            vec![reservation("res-1", "one")],
            timestamp(),
        )
        .await
        .unwrap();

        let outbound = queue_private_payment_list_with_reservations(
            &storage,
            &request(counterparty.clone()),
            vec![PaymentEndpointReservation {
                reservation_id: "res-1".into(),
                receiving_detail: ReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "one".into(),
                },
                expires_at: Some(timestamp()),
                attribution: HashMap::from([("contact".into(), "bob".into())]),
            }],
            timestamp(),
        )
        .await
        .unwrap();
        let snapshot = storage.snapshot().unwrap();

        assert_eq!(snapshot.payment_endpoint_reservations.len(), 1);
        assert_eq!(
            snapshot
                .payment_endpoint_reservations
                .get(&(counterparty.clone(), "res-1".into()))
                .unwrap()
                .outbound_message_id,
            outbound.outbound_message_id
        );
        let record = snapshot
            .payment_endpoint_reservations
            .get(&(counterparty.clone(), "res-1".into()))
            .unwrap();
        assert_eq!(record.attribution.get("contact").unwrap(), "alice");
        assert!(record.expires_at.is_none());
    }

    #[tokio::test]
    async fn test_queue_private_payment_list_with_reservations_rejects_conflicting_existing_id() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        queue_private_payment_list_with_reservations(
            &storage,
            &request(counterparty.clone()),
            vec![reservation("res-1", "one")],
            timestamp(),
        )
        .await
        .unwrap();

        let result = queue_private_payment_list_with_reservations(
            &storage,
            &request(counterparty),
            vec![reservation("res-1", "two")],
            timestamp(),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_queue_private_payment_list_with_reservations_scopes_ids_by_counterparty() {
        let storage = InMemoryStorage::new();
        let first = counterparty();
        let second = counterparty();

        queue_private_payment_list_with_reservations(
            &storage,
            &request(first),
            vec![reservation("res-1", "one")],
            timestamp(),
        )
        .await
        .unwrap();
        queue_private_payment_list_with_reservations(
            &storage,
            &request(second),
            vec![reservation("res-1", "two")],
            timestamp(),
        )
        .await
        .unwrap();

        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .payment_endpoint_reservations
                .len(),
            2
        );
    }
}
