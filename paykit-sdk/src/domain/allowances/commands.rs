use chrono::{DateTime, Utc};
use paykit_lib::{
    serialize_allowance_event, AllowanceAcceptance, AllowanceEnd, AllowanceEvent, AllowanceId,
    AllowanceProposal, AllowanceRejection, AllowanceTerms, EventId, PaykitAppId,
};

use crate::{
    domain::linked_peers::require_private_automation_ready,
    storage::{NewOutboundPrivateMessage, StorageAdapter, StorageTransaction},
    PaykitSdkError, PubkyPublicKey, Result,
};

use super::{
    derivation::{derive_allowance_record, derive_records, AllowanceLinkHistory},
    AllowanceHistoryStatus, AllowanceLifecycleState, AllowanceLocalRole, AllowanceRecord,
    PendingAllowanceAcceptance, MAX_PENDING_ALLOWANCE_LINK_RECORDS,
};

/// Local response to a received Allowance proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AllowanceResponse {
    /// Accept the proposal the caller read, named by the inbound stream item
    /// that carried it.
    Acceptance {
        expected_proposal_stream_item_id: u64,
    },
    Rejection,
}

pub(crate) async fn enqueue_allowance_proposal<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: PaykitAppId,
    local_role: AllowanceLocalRole,
    terms: AllowanceTerms,
    now: DateTime<Utc>,
) -> Result<AllowanceRecord>
where
    S: StorageAdapter,
{
    let event_id = EventId::new_v4();
    let allowance_id = AllowanceId::new_v4();
    storage
        .transaction(move |tx| {
            require_link_ready(tx, &counterparty)?;
            let event = AllowanceEvent::Proposal(AllowanceProposal::new(
                event_id,
                allowance_id.clone(),
                local_role.into(),
                terms,
            ));
            append_and_derive(tx, &counterparty, &app_id, event, &allowance_id, now)
        })
        .await
}

pub(crate) async fn enqueue_allowance_response<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: PaykitAppId,
    allowance_id: AllowanceId,
    response: AllowanceResponse,
    now: DateTime<Utc>,
) -> Result<AllowanceRecord>
where
    S: StorageAdapter,
{
    let event_id = EventId::new_v4();
    let action = response.action();
    storage
        .transaction(move |tx| {
            let record = require_actionable_record(
                tx,
                &counterparty,
                &allowance_id,
                action,
                HistoryGate::Consistent,
            )?;
            let event = response_event(&record, response, event_id)?;
            append_and_derive(tx, &counterparty, &app_id, event, &allowance_id, now)
        })
        .await
}

/// Accept the oldest pending proposals on one exact Encrypted Link.
///
/// Pending proposals are received proposals that make the local identity the
/// Allowee and are `Proposed` with `Consistent` history, in ascending
/// [`AllowanceRecord::proposal_stream_item_id`] order. Each Acceptance passes
/// the same checks as [`enqueue_allowance_response`], and all of them are
/// queued in one transaction. The link history is derived once before and
/// once after the appends, and not at all when the link holds more than
/// [`MAX_PENDING_ALLOWANCE_LINK_RECORDS`] records.
pub(crate) async fn accept_pending_allowance_proposals<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: PaykitAppId,
    limit: usize,
    now: DateTime<Utc>,
) -> Result<PendingAllowanceAcceptance>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            require_link_ready(tx, &counterparty)?;
            let history = match AllowanceLinkHistory::load_at_most(
                tx,
                &counterparty,
                MAX_PENDING_ALLOWANCE_LINK_RECORDS,
            ) {
                Some(history) => history,
                None => {
                    return Ok(PendingAllowanceAcceptance::LinkHistoryTooLarge {
                        record_cap: MAX_PENDING_ALLOWANCE_LINK_RECORDS,
                    })
                }
            };
            let mut pending = derive_records(&history, None)
                .into_iter()
                .filter(is_pending_allowee_proposal)
                .collect::<Vec<_>>();
            pending.sort_by_key(|record| record.proposal_stream_item_id);
            let more_pending = pending.len() > limit;
            pending.truncate(limit);
            if pending.is_empty() {
                return Ok(PendingAllowanceAcceptance::Accepted {
                    records: Vec::new(),
                    more_pending,
                });
            }
            let mut accepted_ids = Vec::with_capacity(pending.len());
            for record in &pending {
                // SECURITY: the expected proposal is the one derived in this
                // transaction, so no replacement can slip in between the read
                // and the Acceptance.
                let response = AllowanceResponse::Acceptance {
                    expected_proposal_stream_item_id: record.proposal_stream_item_id.ok_or_else(
                        || PaykitSdkError::Protocol {
                            context: "received Allowance proposal lacks its stream item".into(),
                            source: None,
                        },
                    )?,
                };
                let event = response_event(record, response, EventId::new_v4())?;
                append_event(tx, &counterparty, &app_id, event, now)?;
                accepted_ids.push(record.allowance_id.clone());
            }
            let history = AllowanceLinkHistory::load(tx, &counterparty);
            let mut derived = derive_records(&history, None)
                .into_iter()
                .map(|record| (record.allowance_id.clone(), record))
                .collect::<std::collections::HashMap<_, _>>();
            let records = accepted_ids
                .iter()
                .map(|allowance_id| {
                    derived
                        .remove(allowance_id)
                        .ok_or_else(|| PaykitSdkError::Storage {
                            context: "queued Allowance event was not present in derived state"
                                .into(),
                            source: None,
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(PendingAllowanceAcceptance::Accepted {
                records,
                more_pending,
            })
        })
        .await
}

/// Whether a record is a received proposal the local identity could accept as
/// Allowee without further review of its history.
fn is_pending_allowee_proposal(record: &AllowanceRecord) -> bool {
    record.local_role == Some(AllowanceLocalRole::Allowee)
        && record.state == AllowanceLifecycleState::Proposed
        && record.history_status == AllowanceHistoryStatus::Consistent
        && !local_sent_proposal(record)
}

impl AllowanceResponse {
    fn action(self) -> &'static str {
        match self {
            Self::Acceptance { .. } => "accept Allowance",
            Self::Rejection => "reject Allowance",
        }
    }
}

/// Check a response against its record and build the event that carries it.
fn response_event(
    record: &AllowanceRecord,
    response: AllowanceResponse,
    event_id: EventId,
) -> Result<AllowanceEvent> {
    let action = response.action();
    if local_sent_proposal(record) {
        return Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: local identity sent the proposal"),
            source: None,
        });
    }
    require_lifecycle(record, AllowanceLifecycleState::Proposed, action)?;
    if let AllowanceResponse::Acceptance {
        expected_proposal_stream_item_id,
    } = response
    {
        // SECURITY: an Acceptance names only the Allowance ID and the
        // Proposal Event ID, and the counterparty chose both. After
        // `forget_peer` it can send the same ids again with different
        // terms. The local stream item id is not chosen by the
        // counterparty and forgetting does not reset its counter, so it
        // pins the proposal the caller read. Checking it here, in the
        // transaction that queues the Acceptance, leaves no window
        // between the check and the response.
        require_expected_proposal(record, expected_proposal_stream_item_id, action)?;
    }
    let allowance_id = AllowanceId::new(&record.allowance_id)?;
    let proposal_event_id = bound_proposal_event_id(record)?;
    Ok(match response {
        AllowanceResponse::Acceptance { .. } => AllowanceEvent::Acceptance(
            AllowanceAcceptance::new(event_id, allowance_id, proposal_event_id)?,
        ),
        AllowanceResponse::Rejection => AllowanceEvent::Rejection(AllowanceRejection::new(
            event_id,
            allowance_id,
            proposal_event_id,
        )?),
    })
}

pub(crate) async fn enqueue_allowance_end<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: PaykitAppId,
    allowance_id: AllowanceId,
    now: DateTime<Utc>,
) -> Result<AllowanceRecord>
where
    S: StorageAdapter,
{
    let event_id = EventId::new_v4();
    storage
        .transaction(move |tx| {
            // Invalid or unresolved history alone does not prevent End for an
            // existing record. Proposal references, lifecycle, role, and link
            // readiness still constrain the action.
            let record = require_actionable_record(
                tx,
                &counterparty,
                &allowance_id,
                "end Allowance",
                HistoryGate::RecoverableOnly,
            )?;
            let proposal_event_id = bound_proposal_event_id(&record)?;
            let event = match record.state {
                AllowanceLifecycleState::Proposed | AllowanceLifecycleState::Rejected => {
                    if !local_sent_proposal(&record) {
                        return Err(PaykitSdkError::Policy {
                            context: "cannot withdraw Allowance proposal: local identity did not send the proposal".into(),
                            source: None,
                        });
                    }
                    AllowanceEvent::End(AllowanceEnd::withdrawal(
                        event_id,
                        allowance_id.clone(),
                        proposal_event_id,
                    )?)
                }
                AllowanceLifecycleState::Accepted => {
                    let acceptance_event_id = bound_event_id(
                        record.acceptance_event_id.as_deref(),
                        "accepted Allowance lacks a valid Acceptance Event ID",
                    )?;
                    AllowanceEvent::End(AllowanceEnd::accepted(
                        event_id,
                        allowance_id.clone(),
                        proposal_event_id,
                        acceptance_event_id,
                    )?)
                }
                _ => {
                    return Err(PaykitSdkError::Policy {
                        context: format!(
                            "cannot end Allowance {} in state {:?}",
                            record.allowance_id, record.state
                        ),
                        source: None,
                    });
                }
            };
            append_and_derive(
                tx,
                &counterparty,
                &app_id,
                event,
                &allowance_id,
                now,
            )
        })
        .await
}

/// Queue one Allowance event and return the derived record it produced.
///
/// The insert happens inside the caller's transaction so precondition checks
/// and the append are atomic.
fn append_and_derive(
    tx: &mut dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: &PaykitAppId,
    event: AllowanceEvent,
    allowance_id: &AllowanceId,
    now: DateTime<Utc>,
) -> Result<AllowanceRecord> {
    append_event(tx, counterparty, app_id, event, now)?;
    let history = AllowanceLinkHistory::load(tx, counterparty);
    derive_allowance_record(&history, allowance_id).ok_or_else(|| PaykitSdkError::Storage {
        context: "queued Allowance event was not present in derived state".into(),
        source: None,
    })
}

/// Queue one Allowance event inside the caller's transaction.
fn append_event(
    tx: &mut dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: &PaykitAppId,
    event: AllowanceEvent,
    now: DateTime<Utc>,
) -> Result<()> {
    crate::storage::require_paykit_app_capability(tx, app_id, event.kind())?;
    let raw_json = serialize_allowance_event(app_id, &event)?;
    let kind = event.kind().as_str().to_owned();
    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
        counterparty.clone(),
        app_id.clone(),
        kind,
        raw_json,
        now,
    ))?;
    Ok(())
}

/// How strict a command is about the derived history it acts on.
#[derive(Clone, Copy)]
enum HistoryGate {
    /// Require complete and valid history.
    Consistent,
    /// Tolerate invalid or unresolved history; block only on recovery.
    RecoverableOnly,
}

/// Load a record that a local command may act on: known on this exact link,
/// with a ready link and history acceptable under `gate`.
fn require_actionable_record(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    allowance_id: &AllowanceId,
    action: &str,
    gate: HistoryGate,
) -> Result<AllowanceRecord> {
    let history = AllowanceLinkHistory::load(tx, counterparty);
    let record = derive_allowance_record(&history, allowance_id).ok_or_else(|| {
        PaykitSdkError::NotFound {
            context: format!(
                "Allowance {allowance_id} is not known for counterparty {counterparty}"
            ),
            source: None,
        }
    })?;
    require_link_ready(tx, counterparty)?;
    require_history(&record, action, gate)?;
    Ok(record)
}

fn require_link_ready(tx: &dyn StorageTransaction, counterparty: &PubkyPublicKey) -> Result<()> {
    let peer_state = tx.linked_peer(counterparty).map(|peer| peer.state);
    let has_active_link = tx
        .encrypted_link_state(counterparty)
        .and_then(|state| state.link_snapshot)
        .is_some();
    require_private_automation_ready(peer_state, has_active_link, counterparty)
}

fn require_history(record: &AllowanceRecord, action: &str, gate: HistoryGate) -> Result<()> {
    match (record.history_status, gate) {
        (AllowanceHistoryStatus::Consistent, _) => Ok(()),
        (AllowanceHistoryStatus::RecoveryRequired, _) => Err(PaykitSdkError::RecoveryRequired {
            context: format!("cannot {action}: exact Encrypted Link history needs recovery"),
            source: None,
        }),
        (
            AllowanceHistoryStatus::UnresolvedReferences | AllowanceHistoryStatus::Invalid,
            HistoryGate::Consistent,
        ) => Err(PaykitSdkError::Protocol {
            context: format!("cannot {action}: Allowance history is not complete and valid"),
            source: None,
        }),
        (
            AllowanceHistoryStatus::UnresolvedReferences | AllowanceHistoryStatus::Invalid,
            HistoryGate::RecoverableOnly,
        ) => Ok(()),
    }
}

/// Whether the local identity authored the proposal on this record.
///
/// A record always derives from exactly one proposal carrier, so an outbound
/// proposal message means the counterparty is the recipient.
fn local_sent_proposal(record: &AllowanceRecord) -> bool {
    record.proposal_outbound_message_id.is_some()
}

fn require_lifecycle(
    record: &AllowanceRecord,
    expected: AllowanceLifecycleState,
    action: &str,
) -> Result<()> {
    if record.state == expected {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!(
                "cannot {action}: Allowance {} is in state {:?}",
                record.allowance_id, record.state
            ),
            source: None,
        })
    }
}

/// Refuse a response to any proposal other than the one the caller read.
fn require_expected_proposal(
    record: &AllowanceRecord,
    expected_proposal_stream_item_id: u64,
    action: &str,
) -> Result<()> {
    if record.proposal_stream_item_id == Some(expected_proposal_stream_item_id) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!(
                "cannot {action}: the proposal for Allowance {} is not the expected \
                 stream item {expected_proposal_stream_item_id}",
                record.allowance_id
            ),
            source: None,
        })
    }
}

fn bound_proposal_event_id(record: &AllowanceRecord) -> Result<EventId> {
    bound_event_id(
        record.proposal_event_id.as_deref(),
        "Allowance proposal lacks a valid Proposal Event ID",
    )
}

/// Re-validate an Event ID bound to the derived record before a new event
/// references it.
fn bound_event_id(value: Option<&str>, missing: &'static str) -> Result<EventId> {
    value
        .and_then(|id| EventId::new(id).ok())
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: missing.into(),
            source: None,
        })
}
