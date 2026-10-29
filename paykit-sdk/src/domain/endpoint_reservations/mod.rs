//! Payment Endpoint Reservation records.

use std::{collections::HashMap, fmt};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sha2::{Digest, Sha256};

use crate::{
    domain::adapters::{
        PrivatePaymentEndpointReservation, PrivatePaymentEndpointReservationCancellation,
        PrivateReceivingDetail,
    },
    domain::outbound_private::{validate_outbound_private_message, OutboundPrivateMessageStatus},
    domain::private_lists::normalize_private_receiving_details,
    storage::{
        require_paykit_app_capability, require_peer_link_operation_lease,
        NewOutboundPrivateMessage, OutboundPrivateMessageRecord, PaymentEndpointReservationRecord,
        PeerLinkOperationLease, StorageAdapter, StorageTransaction,
    },
    LinkedPeerState, PaykitSdkError, PubkyPublicKey, Result,
};
use paykit_lib::{
    parse_private_payment_list_json, serialize_private_payment_list_json, PaykitAppId,
    PrivateMessageKind, PrivatePaymentList,
};

const MAX_RESERVATION_ID_LEN: usize = 128;

pub(crate) enum PrivatePaymentListQueuePolicy {
    Always,
    Sync { sent_message_id: Option<u64> },
}

/// Clock difference tolerated between Paykit Apps that share one identity's
/// state. Their clocks stamp `sent_at` and `last_sync_at` independently.
const PRIVATE_PAYMENT_LIST_REUSE_CLOCK_MARGIN: ChronoDuration = ChronoDuration::minutes(5);

fn latest_private_payment_list(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: &PaykitAppId,
) -> Option<OutboundPrivateMessageRecord> {
    tx.outbound_private_messages(counterparty)
        .into_iter()
        .filter(|message| {
            message.app_id == *app_id
                && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
        })
        .max_by_key(|message| message.outbound_message_id)
}

/// Return the latest sent Private Payment List from `app_id` when durable state
/// shows it was published after the last Encrypted Link change.
///
/// This lets a restarted runtime reuse an unchanged list without a stored
/// delivery witness. `last_sync_at` advances whenever the peer's link state
/// changes (establishment, recovery, block, unblock) and on private receives,
/// so a list sent later than that, by more than the clock margin, went out on
/// the current Encrypted Link. Receives make the check conservative: a list
/// sent before one is republished once instead of reused. The caller must
/// confirm that the current link snapshot still matches the stored peer
/// record, because reuse skips the outbound restore that would check it.
pub(crate) fn private_payment_list_sent_after_last_link_change(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: &PaykitAppId,
) -> Option<u64> {
    let peer = tx.linked_peer(counterparty)?;
    if peer.state != LinkedPeerState::Linked {
        return None;
    }
    let last_link_change = peer.last_sync_at?;
    let latest = latest_private_payment_list(tx, counterparty, app_id)?;
    if latest.status != OutboundPrivateMessageStatus::Sent {
        return None;
    }
    // Recovery requeues clear `sent_at`, so it always names a publication made
    // after the message last returned to the retry path.
    let sent_at = latest.sent_at?;
    (sent_at > last_link_change + PRIVATE_PAYMENT_LIST_REUSE_CLOCK_MARGIN)
        .then_some(latest.outbound_message_id)
}

/// SDK cleanup record for a reserved endpoint tied to a queued private list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentEndpointReservationCancellationRecord {
    pub(crate) outbound_message_id: u64,
    pub(crate) app_id: PaykitAppId,
    pub(crate) cancellation: PrivatePaymentEndpointReservationCancellation,
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

/// Load reservation cancellations for invalid or unattempted superseded Private Payment Lists.
///
/// Attempted superseded lists are left for adapter expiry or explicit cleanup because
/// the SDK cannot prove the counterparty did not receive their reserved details.
pub(crate) async fn terminal_private_list_reservation_cancellations<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<PaymentEndpointReservationCancellationRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(terminal_private_list_reservation_cancellations_in_transaction(tx, counterparty))
        })
        .await
}

pub(crate) fn terminal_private_list_reservation_cancellations_in_transaction(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
) -> Vec<PaymentEndpointReservationCancellationRecord> {
    let reservations = tx.payment_endpoint_reservations(counterparty);
    if reservations.is_empty() {
        return Vec::new();
    }
    let outbound = tx
        .outbound_private_messages(counterparty)
        .into_iter()
        .map(|message| (message.outbound_message_id, message))
        .collect::<HashMap<_, _>>();
    let mut superseded = Vec::new();
    let mut invalid = Vec::new();
    for record in reservations {
        let Some(message) = outbound.get(&record.outbound_message_id) else {
            continue;
        };
        if message.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
            continue;
        }
        match message.status {
            OutboundPrivateMessageStatus::Superseded if message.last_attempt_at.is_none() => {
                superseded.push(cancellation_record_from_reservation_record(record));
            }
            OutboundPrivateMessageStatus::Invalid => {
                invalid.push(cancellation_record_from_reservation_record(record));
            }
            _ => {}
        }
    }
    superseded.extend(invalid);
    superseded
}

/// Load reservation cancellations for one outbound Private Payment List if any linked
/// reservation expired before it was sent.
pub(crate) fn expired_outbound_reservation_cancellations_in_transaction(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    outbound_message_id: u64,
    now: DateTime<Utc>,
) -> Vec<PaymentEndpointReservationCancellationRecord> {
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
    if has_expired {
        records
            .into_iter()
            .map(cancellation_record_from_reservation_record)
            .collect()
    } else {
        Vec::new()
    }
}

fn cancellation_record_from_reservation_record(
    record: PaymentEndpointReservationRecord,
) -> PaymentEndpointReservationCancellationRecord {
    PaymentEndpointReservationCancellationRecord {
        outbound_message_id: record.outbound_message_id,
        app_id: record.app_id,
        cancellation: PrivatePaymentEndpointReservationCancellation {
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
    counterparty: &PubkyPublicKey,
    app_id: PaykitAppId,
    reservations: Vec<PrivatePaymentEndpointReservation>,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    queue_private_payment_list_with_reservations_inner(
        storage,
        counterparty,
        app_id,
        reservations,
        now,
        None,
        PrivatePaymentListQueuePolicy::Always,
    )
    .await
}

/// Queue a Private Payment List with linked reservations while a peer operation
/// lease is active.
pub(crate) async fn queue_private_payment_list_with_reservations_with_link_lease<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    app_id: PaykitAppId,
    reservations: Vec<PrivatePaymentEndpointReservation>,
    now: DateTime<Utc>,
    lease: &PeerLinkOperationLease,
    policy: PrivatePaymentListQueuePolicy,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    queue_private_payment_list_with_reservations_inner(
        storage,
        counterparty,
        app_id,
        reservations,
        now,
        Some(lease.clone()),
        policy,
    )
    .await
}

async fn queue_private_payment_list_with_reservations_inner<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    app_id: PaykitAppId,
    reservations: Vec<PrivatePaymentEndpointReservation>,
    now: DateTime<Utc>,
    lease: Option<PeerLinkOperationLease>,
    policy: PrivatePaymentListQueuePolicy,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            queue_private_payment_list_with_reservations_in_transaction(
                tx,
                counterparty,
                app_id,
                reservations,
                now,
                policy,
            )
        })
        .await
}

/// Queue a complete list and its reservations in the caller's transaction.
pub(crate) fn queue_private_payment_list_with_reservations_in_transaction(
    tx: &mut dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: PaykitAppId,
    reservations: Vec<PrivatePaymentEndpointReservation>,
    now: DateTime<Utc>,
    policy: PrivatePaymentListQueuePolicy,
) -> Result<OutboundPrivateMessageRecord> {
    let (receiving_details, drafts) =
        build_reservation_records(counterparty, &app_id, reservations, now)?;
    let payment_endpoints = normalize_private_receiving_details(receiving_details)?;
    let list = PrivatePaymentList::new(app_id, payment_endpoints);
    let raw_json = serialize_private_payment_list_json(&list)?;
    let (message_app_id, kind) = validate_outbound_private_message(&raw_json)?;
    let counterparty = counterparty.clone();

    require_paykit_app_capability(tx, &message_app_id, PrivateMessageKind::PrivatePaymentList)?;
    let mut checked_drafts = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let existing = tx.payment_endpoint_reservation(
            &draft.counterparty,
            &draft.app_id,
            &draft.reservation_id,
        );
        if let Some(existing) = existing.as_ref() {
            if existing.cancellation_started_at.is_some() {
                return Err(PaykitSdkError::Policy {
                    context: format!(
                        "Payment Endpoint Reservation id '{}' is being canceled",
                        draft.reservation_id
                    ),
                    source: None,
                });
            }
            if !draft.matches_existing(existing) {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Payment Endpoint Reservation id '{}' already exists with different details",
                        draft.reservation_id
                    ),
                    source: None,
                });
            }
        }
        checked_drafts.push((draft, existing));
    }

    if let PrivatePaymentListQueuePolicy::Sync { sent_message_id } = policy {
        // Pending, sending, and failed messages remain on the retry path, so
        // they can be reused. A sent message is reusable only when the caller
        // showed it was delivered on the current Encrypted Link.
        if let Some(message) = latest_private_payment_list(tx, &counterparty, &message_app_id) {
            let reusable = match message.status {
                OutboundPrivateMessageStatus::Pending
                | OutboundPrivateMessageStatus::Sending
                | OutboundPrivateMessageStatus::Failed => true,
                OutboundPrivateMessageStatus::Sent => {
                    sent_message_id == Some(message.outbound_message_id)
                }
                _ => false,
            };
            let same_reservations = checked_drafts.iter().all(|(_, existing)| {
                existing.as_ref().is_some_and(|record| {
                    record.outbound_message_id == message.outbound_message_id
                        && record.expires_at.is_none_or(|expiry| expiry > now)
                })
            }) && tx
                .payment_endpoint_reservations(&counterparty)
                .iter()
                .filter(|record| record.outbound_message_id == message.outbound_message_id)
                .count()
                == checked_drafts.len();
            if reusable
                && same_reservations
                && parse_private_payment_list_json(&message.raw_json)
                    .is_ok_and(|stored| stored == list)
            {
                return Ok(message);
            }
        }
    }

    let outbound = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
        counterparty,
        message_app_id,
        kind,
        raw_json,
        now,
    ))?;
    for (draft, existing) in checked_drafts {
        let record = draft.into_record(outbound.outbound_message_id, existing);
        tx.save_payment_endpoint_reservation(record);
    }
    Ok(outbound)
}

#[derive(Clone)]
struct PaymentEndpointReservationRecordDraft {
    reservation_id: String,
    counterparty: PubkyPublicKey,
    app_id: PaykitAppId,
    identifier: String,
    payload_hash: String,
    attribution: HashMap<String, String>,
    expires_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl PaymentEndpointReservationRecordDraft {
    fn matches_existing(&self, existing: &PaymentEndpointReservationRecord) -> bool {
        existing.counterparty == self.counterparty
            && existing.app_id == self.app_id
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
            app_id: self.app_id,
            identifier: self.identifier,
            payload_hash: self.payload_hash,
            outbound_message_id,
            attribution,
            expires_at,
            cancellation_started_at: None,
            created_at,
        }
    }
}

fn build_reservation_records(
    counterparty: &PubkyPublicKey,
    app_id: &PaykitAppId,
    reservations: Vec<PrivatePaymentEndpointReservation>,
    now: DateTime<Utc>,
) -> Result<(
    Vec<PrivateReceivingDetail>,
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
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "duplicate Payment Endpoint Reservation id '{}'",
                    reservation.reservation_id
                ),
                source: None,
            });
        }
        receiving_details.push(reservation.receiving_detail.clone());
        records.push(record_from_reservation(
            counterparty,
            app_id,
            reservation,
            now,
        ));
    }

    normalize_private_receiving_details(receiving_details.clone())?;
    Ok((receiving_details, records))
}

pub(crate) fn validate_reservation_id(reservation_id: &str) -> Result<()> {
    if reservation_id.trim().is_empty() {
        return Err(PaykitSdkError::Protocol {
            context: "Payment Endpoint Reservation id must not be empty".into(),
            source: None,
        });
    }
    if reservation_id.len() > MAX_RESERVATION_ID_LEN {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Payment Endpoint Reservation id must be at most {MAX_RESERVATION_ID_LEN} bytes"
            ),
            source: None,
        });
    }
    if reservation_id.chars().any(char::is_control) {
        return Err(PaykitSdkError::Protocol {
            context: "Payment Endpoint Reservation id must not contain control characters".into(),
            source: None,
        });
    }
    Ok(())
}

fn record_from_reservation(
    counterparty: &PubkyPublicKey,
    app_id: &PaykitAppId,
    reservation: PrivatePaymentEndpointReservation,
    now: DateTime<Utc>,
) -> PaymentEndpointReservationRecordDraft {
    let payload_hash = reservation_payload_hash(&reservation.receiving_detail.payload);
    PaymentEndpointReservationRecordDraft {
        reservation_id: reservation.reservation_id,
        counterparty: counterparty.clone(),
        app_id: app_id.clone(),
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
            .field("reservation_id", &"<redacted>")
            .field("counterparty", &self.counterparty)
            .field("app_id", &self.app_id)
            .field("identifier", &self.identifier)
            .field("payload_hash", &"<redacted>")
            .field("outbound_message_id", &self.outbound_message_id)
            .field(
                "attribution",
                &format_args!("<redacted:{} fields>", self.attribution.len()),
            )
            .field("expires_at", &self.expires_at)
            .field("cancellation_started_at", &self.cancellation_started_at)
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[cfg(test)]
mod tests;
