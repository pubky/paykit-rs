use chrono::{DateTime, Utc};
use paykit_lib::{
    serialize_allowance_event, AllowanceAcceptance, AllowanceEnd, AllowanceEvent, AllowanceId,
    AllowanceProposal, AllowanceRejection, AllowanceTerms, EventId,
};

use crate::{
    domain::linked_peers::require_private_automation_ready,
    storage::{NewOutboundPrivateMessage, StorageAdapter, StorageTransaction},
    PaykitReceiverPath, PaykitSdkError, PubkyPublicKey, Result,
};

use super::{
    derivation::{derive_allowance_record, AllowanceLinkHistory},
    AllowanceHistoryStatus, AllowanceLifecycleState, AllowanceLocalRole, AllowanceRecord,
};

/// Local response to a received Allowance proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AllowanceResponse {
    Acceptance,
    Rejection,
}

pub(crate) async fn enqueue_allowance_proposal<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
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
            require_link_ready(tx, &counterparty, &counterparty_receiver_path)?;
            let event = AllowanceEvent::Proposal(AllowanceProposal::new(
                event_id,
                allowance_id.clone(),
                local_role.into(),
                terms,
            ));
            append_and_derive(
                tx,
                &counterparty,
                &counterparty_receiver_path,
                event,
                &allowance_id,
                now,
            )
        })
        .await
}

pub(crate) async fn enqueue_allowance_response<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    allowance_id: AllowanceId,
    response: AllowanceResponse,
    now: DateTime<Utc>,
) -> Result<AllowanceRecord>
where
    S: StorageAdapter,
{
    let event_id = EventId::new_v4();
    let action = match response {
        AllowanceResponse::Acceptance => "accept Allowance",
        AllowanceResponse::Rejection => "reject Allowance",
    };
    storage
        .transaction(move |tx| {
            let record = require_actionable_record(
                tx,
                &counterparty,
                &counterparty_receiver_path,
                &allowance_id,
                action,
            )?;
            if local_sent_proposal(&record) {
                return Err(PaykitSdkError::Policy {
                    context: format!("cannot {action}: local identity sent the proposal"),
                    source: None,
                });
            }
            require_lifecycle(&record, AllowanceLifecycleState::Proposed, action)?;
            let proposal_event_id = bound_proposal_event_id(&record)?;
            let event =
                match response {
                    AllowanceResponse::Acceptance => AllowanceEvent::Acceptance(
                        AllowanceAcceptance::new(event_id, allowance_id.clone(), proposal_event_id),
                    ),
                    AllowanceResponse::Rejection => AllowanceEvent::Rejection(
                        AllowanceRejection::new(event_id, allowance_id.clone(), proposal_event_id),
                    ),
                };
            append_and_derive(
                tx,
                &counterparty,
                &counterparty_receiver_path,
                event,
                &allowance_id,
                now,
            )
        })
        .await
}

pub(crate) async fn enqueue_allowance_end<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    allowance_id: AllowanceId,
    now: DateTime<Utc>,
) -> Result<AllowanceRecord>
where
    S: StorageAdapter,
{
    let event_id = EventId::new_v4();
    storage
        .transaction(move |tx| {
            let record = require_actionable_record(
                tx,
                &counterparty,
                &counterparty_receiver_path,
                &allowance_id,
                "end Allowance",
            )?;
            let proposal_event_id = bound_proposal_event_id(&record)?;
            let event = match record.state {
                AllowanceLifecycleState::Proposed => {
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
                    ))
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
                    ))
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
                &counterparty_receiver_path,
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
    counterparty_receiver_path: &PaykitReceiverPath,
    event: AllowanceEvent,
    allowance_id: &AllowanceId,
    now: DateTime<Utc>,
) -> Result<AllowanceRecord> {
    let raw_json = serialize_allowance_event(&event)?;
    let kind = event.kind().as_str().to_owned();
    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
        counterparty.clone(),
        counterparty_receiver_path.clone(),
        kind,
        raw_json,
        now,
    ));
    let history = AllowanceLinkHistory::load(tx, counterparty, counterparty_receiver_path);
    derive_allowance_record(&history, allowance_id).ok_or_else(|| PaykitSdkError::Storage {
        context: "queued Allowance event was not present in derived state".into(),
        source: None,
    })
}

/// Load a record that a local command may act on: known on this exact link,
/// with a ready link and consistent history.
fn require_actionable_record(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    allowance_id: &AllowanceId,
    action: &str,
) -> Result<AllowanceRecord> {
    let history = AllowanceLinkHistory::load(tx, counterparty, counterparty_receiver_path);
    let record = derive_allowance_record(&history, allowance_id).ok_or_else(|| {
        PaykitSdkError::NotFound {
            context: format!(
                "Allowance {allowance_id} is not known for counterparty {counterparty} on receiver {counterparty_receiver_path}"
            ),
            source: None,
        }
    })?;
    require_link_ready(tx, counterparty, counterparty_receiver_path)?;
    require_consistent_history(&record, action)?;
    Ok(record)
}

fn require_link_ready(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) -> Result<()> {
    let peer_state = tx
        .linked_peer(counterparty, counterparty_receiver_path)
        .map(|peer| peer.state);
    let has_active_link = tx
        .encrypted_link_state(counterparty, counterparty_receiver_path)
        .and_then(|state| state.link_snapshot)
        .is_some();
    require_private_automation_ready(peer_state, has_active_link, counterparty)
}

fn require_consistent_history(record: &AllowanceRecord, action: &str) -> Result<()> {
    match record.history_status {
        AllowanceHistoryStatus::Consistent => Ok(()),
        AllowanceHistoryStatus::RecoveryRequired => Err(PaykitSdkError::RecoveryRequired {
            context: format!("cannot {action}: exact Encrypted Link history needs recovery"),
            source: None,
        }),
        AllowanceHistoryStatus::UnresolvedReferences | AllowanceHistoryStatus::Invalid => {
            Err(PaykitSdkError::Protocol {
                context: format!("cannot {action}: Allowance history is not complete and valid"),
                source: None,
            })
        }
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
