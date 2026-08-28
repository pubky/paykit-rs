use chrono::{DateTime, Utc};
use paykit_lib::{
    serialize_allowance_event, AllowanceAcceptance, AllowanceEnd, AllowanceEvent, AllowanceId,
    AllowanceProposal, AllowanceRejection, AllowanceTerms, EventId,
};
use serde_json::Value as JsonValue;

use crate::{
    domain::linked_peers::LinkedPeerState,
    storage::{NewOutboundPrivateMessage, StorageAdapter, StorageState, StorageTransaction},
    PaykitReceiverPath, PaykitSdkError, PubkyPublicKey, Result,
};

use super::{
    allowance_record_from_state, AllowanceHistoryStatus, AllowanceLifecycleState,
    AllowanceLocalRole, AllowanceRecord,
};

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
            let mut state = tx.export_storage_state();
            require_link_ready(&state, &counterparty, &counterparty_receiver_path)?;
            require_unused_event_id(
                &state,
                &counterparty,
                &counterparty_receiver_path,
                &event_id,
            )?;
            require_unused_allowance_id(
                &state,
                &counterparty,
                &counterparty_receiver_path,
                &allowance_id,
            )?;
            let event = AllowanceEvent::Proposal(AllowanceProposal::new(
                event_id,
                allowance_id.clone(),
                local_role.into(),
                terms,
            ));
            append_and_derive(
                tx,
                &mut state,
                counterparty,
                counterparty_receiver_path,
                event,
                &allowance_id,
                now,
            )
        })
        .await
}

pub(crate) async fn enqueue_allowance_acceptance<S>(
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
            let mut state = tx.export_storage_state();
            let record = require_record(
                &state,
                &counterparty,
                &counterparty_receiver_path,
                &allowance_id,
            )?;
            require_link_ready(&state, &counterparty, &counterparty_receiver_path)?;
            require_consistent_history(&record, "accept Allowance")?;
            require_local_proposal_recipient(&record, "accept Allowance")?;
            require_lifecycle(
                &record,
                AllowanceLifecycleState::Proposed,
                "accept Allowance",
            )?;
            require_unused_event_id(
                &state,
                &counterparty,
                &counterparty_receiver_path,
                &event_id,
            )?;
            let proposal_event_id = proposal_event_id(&record)?;
            let event = AllowanceEvent::Acceptance(AllowanceAcceptance::new(
                event_id,
                allowance_id.clone(),
                proposal_event_id,
            ));
            append_and_derive(
                tx,
                &mut state,
                counterparty,
                counterparty_receiver_path,
                event,
                &allowance_id,
                now,
            )
        })
        .await
}

pub(crate) async fn enqueue_allowance_rejection<S>(
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
            let mut state = tx.export_storage_state();
            let record = require_record(
                &state,
                &counterparty,
                &counterparty_receiver_path,
                &allowance_id,
            )?;
            require_link_ready(&state, &counterparty, &counterparty_receiver_path)?;
            require_consistent_history(&record, "reject Allowance")?;
            require_local_proposal_recipient(&record, "reject Allowance")?;
            require_lifecycle(
                &record,
                AllowanceLifecycleState::Proposed,
                "reject Allowance",
            )?;
            require_unused_event_id(
                &state,
                &counterparty,
                &counterparty_receiver_path,
                &event_id,
            )?;
            let proposal_event_id = proposal_event_id(&record)?;
            let event = AllowanceEvent::Rejection(AllowanceRejection::new(
                event_id,
                allowance_id.clone(),
                proposal_event_id,
            ));
            append_and_derive(
                tx,
                &mut state,
                counterparty,
                counterparty_receiver_path,
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
            let mut state = tx.export_storage_state();
            let record = require_record(
                &state,
                &counterparty,
                &counterparty_receiver_path,
                &allowance_id,
            )?;
            require_link_ready(&state, &counterparty, &counterparty_receiver_path)?;
            require_consistent_history(&record, "end Allowance")?;
            require_unused_event_id(
                &state,
                &counterparty,
                &counterparty_receiver_path,
                &event_id,
            )?;
            let proposal_event_id = proposal_event_id(&record)?;
            let event = match record.state {
                AllowanceLifecycleState::Proposed => {
                    require_local_proposal_sender(&record, "withdraw Allowance proposal")?;
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
                &mut state,
                counterparty,
                counterparty_receiver_path,
                event,
                &allowance_id,
                now,
            )
        })
        .await
}

fn append_and_derive(
    tx: &mut dyn StorageTransaction,
    state: &mut StorageState,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    event: AllowanceEvent,
    allowance_id: &AllowanceId,
    now: DateTime<Utc>,
) -> Result<AllowanceRecord> {
    let raw_json = serialize_allowance_event(&event)?;
    let kind = event.kind().as_str().to_owned();
    let outbound = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
        counterparty.clone(),
        counterparty_receiver_path.clone(),
        kind,
        raw_json,
        now,
    ));
    state.outbound_private_messages.push(outbound);
    allowance_record_from_state(
        state,
        &counterparty,
        &counterparty_receiver_path,
        allowance_id,
    )
    .ok_or_else(|| PaykitSdkError::Storage {
        context: "queued Allowance event was not present in derived state".into(),
        source: None,
    })
}

fn require_record(
    state: &StorageState,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    allowance_id: &AllowanceId,
) -> Result<AllowanceRecord> {
    allowance_record_from_state(
        state,
        counterparty,
        counterparty_receiver_path,
        allowance_id,
    )
    .ok_or_else(|| PaykitSdkError::NotFound {
        context: format!(
            "Allowance {allowance_id} is not known for counterparty {counterparty} on receiver {counterparty_receiver_path}"
        ),
        source: None,
    })
}

fn require_link_ready(
    state: &StorageState,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) -> Result<()> {
    let peer_state = state
        .linked_peers
        .get(&(counterparty.clone(), counterparty_receiver_path.clone()))
        .map(|peer| peer.state.clone());
    if peer_state == Some(LinkedPeerState::Blocked) {
        return Err(PaykitSdkError::Policy {
            context: format!("counterparty {counterparty} is blocked"),
            source: None,
        });
    }
    let has_active_link = state
        .encrypted_link_states
        .get(&(counterparty.clone(), counterparty_receiver_path.clone()))
        .and_then(|link| link.link_snapshot.as_ref())
        .is_some();
    if peer_state == Some(LinkedPeerState::Linked) && has_active_link {
        return Ok(());
    }
    Err(PaykitSdkError::RecoveryRequired {
        context: format!(
            "no complete active Encrypted Link state for counterparty {counterparty} on receiver {counterparty_receiver_path}"
        ),
        source: None,
    })
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

fn require_local_proposal_recipient(record: &AllowanceRecord, action: &str) -> Result<()> {
    if record.proposal_stream_item_id.is_some() && record.proposal_outbound_message_id.is_none() {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: local identity sent the proposal"),
            source: None,
        })
    }
}

fn require_local_proposal_sender(record: &AllowanceRecord, action: &str) -> Result<()> {
    if record.proposal_outbound_message_id.is_some() && record.proposal_stream_item_id.is_none() {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: local identity did not send the proposal"),
            source: None,
        })
    }
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
    state: &StorageState,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    event_id: &EventId,
) -> Result<()> {
    let inbound_used = state.event_dedup_records.contains_key(&(
        counterparty.clone(),
        counterparty_receiver_path.clone(),
        event_id.as_str().to_owned(),
    ));
    let outbound_used = state.outbound_private_messages.iter().any(|message| {
        &message.counterparty == counterparty
            && &message.counterparty_receiver_path == counterparty_receiver_path
            && canonical_id(&message.raw_json, "event_id").as_deref() == Some(event_id.as_str())
    });
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
    state: &StorageState,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    allowance_id: &AllowanceId,
) -> Result<()> {
    let inbound_used = state.private_stream_items.iter().any(|item| {
        &item.counterparty == counterparty
            && &item.counterparty_receiver_path == counterparty_receiver_path
            && canonical_id(&item.raw_json, "allowance_id").as_deref()
                == Some(allowance_id.as_str())
    });
    let outbound_used = state.outbound_private_messages.iter().any(|message| {
        &message.counterparty == counterparty
            && &message.counterparty_receiver_path == counterparty_receiver_path
            && canonical_id(&message.raw_json, "allowance_id").as_deref()
                == Some(allowance_id.as_str())
    });
    if inbound_used || outbound_used {
        Err(PaykitSdkError::Protocol {
            context: "new Allowance ID already exists on the exact Encrypted Link".into(),
            source: None,
        })
    } else {
        Ok(())
    }
}

fn canonical_id(raw_json: &str, field: &str) -> Option<String> {
    let value: JsonValue = serde_json::from_str(raw_json).ok()?;
    let id = value.get(field)?.as_str()?;
    let is_canonical = match field {
        "event_id" => EventId::new(id).is_ok(),
        "allowance_id" => AllowanceId::new(id).is_ok(),
        _ => false,
    };
    if !is_canonical {
        return None;
    }
    Some(id.to_owned())
}
