//! Payment Endpoint Reservation records.

use std::{collections::HashMap, fmt};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::{
    adapters::{
        PaymentEndpointReservation, PaymentEndpointReservationRelease,
        PaymentEndpointReservationRequest, ReceivingDetail,
    },
    endpoints::normalize_receiving_details,
    outbound_private::{validate_outbound_private_message, OutboundPrivateMessageStatus},
    storage::{
        require_peer_link_operation_lease, NewOutboundPrivateMessage, OutboundPrivateMessageRecord,
        PaymentEndpointReservationRecord, PeerLinkOperationLease, StorageAdapter,
    },
    PaykitSdkError, PubkyPublicKey, Result,
};
use paykit_lib::{serialize_private_payment_list_json, PrivateMessageKind, PrivatePaymentList};

const MAX_RESERVATION_ID_LEN: usize = 128;

/// SDK cleanup record for a reserved endpoint tied to a queued private list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentEndpointReservationReleaseRecord {
    pub(crate) outbound_message_id: u64,
    pub(crate) release: PaymentEndpointReservationRelease,
}

/// Load Payment Endpoint Reservation records for one counterparty.
#[cfg(test)]
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

/// Load reservation releases for superseded Private Payment Lists that were never attempted.
pub(crate) async fn unattempted_superseded_reservation_releases<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<PaymentEndpointReservationReleaseRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            let outbound = tx.outbound_private_messages(counterparty);
            let superseded_unattempted = outbound
                .iter()
                .filter(|message| {
                    message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                        && message.status == OutboundPrivateMessageStatus::Superseded
                        && message.last_attempt_at.is_none()
                })
                .map(|message| message.outbound_message_id)
                .collect::<std::collections::HashSet<_>>();

            let releases = tx
                .payment_endpoint_reservations(counterparty)
                .into_iter()
                .filter(|record| superseded_unattempted.contains(&record.outbound_message_id))
                .map(release_record_from_reservation_record)
                .collect();
            Ok(releases)
        })
        .await
}

/// Load reservation releases for invalid Private Payment Lists that can no
/// longer use their linked reservations.
pub(crate) async fn invalid_private_list_reservation_releases<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<PaymentEndpointReservationReleaseRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            let outbound = tx.outbound_private_messages(counterparty);
            let invalid_private_lists = outbound
                .iter()
                .filter(|message| {
                    message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                        && message.status == OutboundPrivateMessageStatus::Invalid
                })
                .map(|message| message.outbound_message_id)
                .collect::<std::collections::HashSet<_>>();

            let releases = tx
                .payment_endpoint_reservations(counterparty)
                .into_iter()
                .filter(|record| invalid_private_lists.contains(&record.outbound_message_id))
                .map(release_record_from_reservation_record)
                .collect();
            Ok(releases)
        })
        .await
}

/// Load reservation releases for one outbound Private Payment List if any linked
/// reservation expired before it was sent.
pub(crate) async fn expired_outbound_reservation_releases<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    outbound_message_id: u64,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentEndpointReservationReleaseRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            let records = tx
                .payment_endpoint_reservations(counterparty)
                .into_iter()
                .filter(|record| record.outbound_message_id == outbound_message_id)
                .collect::<Vec<_>>();
            let has_expired = records.iter().any(|record| {
                record
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= now)
            });
            let releases = if has_expired {
                records
                    .into_iter()
                    .map(release_record_from_reservation_record)
                    .collect()
            } else {
                Vec::new()
            };
            Ok(releases)
        })
        .await
}

fn release_record_from_reservation_record(
    record: PaymentEndpointReservationRecord,
) -> PaymentEndpointReservationReleaseRecord {
    PaymentEndpointReservationReleaseRecord {
        outbound_message_id: record.outbound_message_id,
        release: PaymentEndpointReservationRelease {
            reservation_id: record.reservation_id,
            counterparty: record.counterparty,
            identifier: record.identifier,
            payload_hash: record.payload_hash,
            attribution: record.attribution,
        },
    }
}

/// Queue a Private Payment List and persist linked reservation records atomically.
#[cfg(test)]
pub(crate) async fn queue_private_payment_list_with_reservations<S>(
    storage: &S,
    request: &PaymentEndpointReservationRequest,
    reservations: Vec<PaymentEndpointReservation>,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    queue_private_payment_list_with_reservations_inner(storage, request, reservations, now, None)
        .await
}

/// Queue a Private Payment List with linked reservations while a peer operation
/// lease is active.
pub(crate) async fn queue_private_payment_list_with_reservations_with_link_lease<S>(
    storage: &S,
    request: &PaymentEndpointReservationRequest,
    reservations: Vec<PaymentEndpointReservation>,
    now: DateTime<Utc>,
    lease: &PeerLinkOperationLease,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    queue_private_payment_list_with_reservations_inner(
        storage,
        request,
        reservations,
        now,
        Some(lease.clone()),
    )
    .await
}

async fn queue_private_payment_list_with_reservations_inner<S>(
    storage: &S,
    request: &PaymentEndpointReservationRequest,
    reservations: Vec<PaymentEndpointReservation>,
    now: DateTime<Utc>,
    lease: Option<PeerLinkOperationLease>,
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
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let mut checked_drafts = Vec::with_capacity(drafts.len());
            for draft in drafts {
                let existing =
                    tx.payment_endpoint_reservation(&draft.counterparty, &draft.reservation_id);
                if let Some(existing) = existing.as_ref() {
                    if existing.release_started_at.is_some() {
                        return Err(PaykitSdkError::Policy(format!(
                            "Payment Endpoint Reservation id '{}' is being released",
                            draft.reservation_id
                        )));
                    }
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
            release_started_at: None,
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

pub(crate) fn validate_reservation_id(reservation_id: &str) -> Result<()> {
    if reservation_id.trim().is_empty() {
        return Err(PaykitSdkError::Protocol(
            "Payment Endpoint Reservation id must not be empty".into(),
        ));
    }
    if reservation_id.len() > MAX_RESERVATION_ID_LEN {
        return Err(PaykitSdkError::Protocol(format!(
            "Payment Endpoint Reservation id must be at most {MAX_RESERVATION_ID_LEN} bytes"
        )));
    }
    if reservation_id.chars().any(char::is_control) {
        return Err(PaykitSdkError::Protocol(
            "Payment Endpoint Reservation id must not contain control characters".into(),
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
            .field("release_started_at", &self.release_started_at)
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[cfg(test)]
mod tests;
