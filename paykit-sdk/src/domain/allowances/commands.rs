use chrono::{DateTime, Utc};
use paykit_lib::{
    serialize_allowance_event, AllowanceAcceptance, AllowanceEnd, AllowanceEvent, AllowanceId,
    AllowanceProposal, AllowanceRejection, AllowanceTerms, EventId,
};

use crate::{
    domain::{linked_peers::require_private_automation_ready, private_stream::canonical_event_id},
    storage::{NewOutboundPrivateMessage, StorageAdapter, StorageTransaction},
    PaykitReceiverPath, PaykitSdkError, PubkyPublicKey, Result,
};

use super::{
    derivation::{canonical_allowance_id, derive_allowance_record, AllowanceLinkHistory},
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
            let history =
                AllowanceLinkHistory::load(tx, &counterparty, &counterparty_receiver_path);
            require_unused_event_id(tx, &history, &event_id)?;
            require_unused_allowance_id(&history, &allowance_id)?;
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
                &event_id,
                action,
            )?;
            if local_sent_proposal(&record) {
                return Err(PaykitSdkError::Policy {
                    context: format!("cannot {action}: local identity sent the proposal"),
                    source: None,
                });
            }
            require_lifecycle(&record, AllowanceLifecycleState::Proposed, action)?;
            let proposal_event_id = proposal_event_id(&record)?;
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
                &event_id,
                "end Allowance",
            )?;
            let proposal_event_id = proposal_event_id(&record)?;
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
                    let acceptance_event_id = record
                        .acceptance_event_id
                        .as_deref()
                        .and_then(|id| EventId::new(id).ok())
                        .ok_or_else(|| PaykitSdkError::Protocol {
                            context: "accepted Allowance lacks a valid Acceptance Event ID".into(),
                            source: None,
                        })?;
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
/// with a ready link, consistent history, and a fresh Event ID.
fn require_actionable_record(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    allowance_id: &AllowanceId,
    event_id: &EventId,
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
    require_unused_event_id(tx, &history, event_id)?;
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

fn proposal_event_id(record: &AllowanceRecord) -> Result<EventId> {
    record
        .proposal_event_id
        .as_deref()
        .and_then(|id| EventId::new(id).ok())
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: "Allowance proposal lacks a valid Proposal Event ID".into(),
            source: None,
        })
}

fn require_unused_event_id(
    tx: &dyn StorageTransaction,
    history: &AllowanceLinkHistory,
    event_id: &EventId,
) -> Result<()> {
    let inbound_used = tx
        .event_dedup_record(
            &history.counterparty,
            &history.counterparty_receiver_path,
            event_id.as_str(),
        )
        .is_some();
    let outbound_used = history
        .outbound
        .iter()
        .any(|message| canonical_event_id(&message.raw_json).as_deref() == Some(event_id.as_str()));
    if inbound_used || outbound_used {
        Err(PaykitSdkError::Protocol {
            context: "new Allowance Event ID already exists on the exact Encrypted Link".into(),
            source: None,
        })
    } else {
        Ok(())
    }
}

fn require_unused_allowance_id(
    history: &AllowanceLinkHistory,
    allowance_id: &AllowanceId,
) -> Result<()> {
    let used = history
        .items
        .iter()
        .map(|item| item.raw_json.as_str())
        .chain(
            history
                .outbound
                .iter()
                .map(|message| message.raw_json.as_str()),
        )
        .any(|raw_json| canonical_allowance_id(raw_json).as_deref() == Some(allowance_id.as_str()));
    if used {
        Err(PaykitSdkError::Protocol {
            context: "new Allowance ID already exists on the exact Encrypted Link".into(),
            source: None,
        })
    } else {
        Ok(())
    }
}
