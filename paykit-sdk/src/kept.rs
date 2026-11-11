//! The batches of this app the client keeps to send again (`p/kb/<batch_id>`, Switch wire
//! 6.4 step 5), and what each carries for Paykit: a request's message, or a payment's
//! attempt.
//!
//! A call whose batch the client kept says so (`AppError::Kept`); the client then sends it,
//! the same bytes, at the next write or read, or drops it, refused for good
//! (`AppEvent::BatchRefused`) or past what its answer can tell (`AppEvent::KeptBatchDropped`,
//! `AppEvent::AttemptDropped`). While it is kept, a request isn't in the outbox yet and an
//! attempt isn't on its claim, so neither absence means anything: the note says why. The
//! client's duty events end a note: read from each receive's report and, at a resume and a
//! flush, from the events the host hasn't acknowledged. The pinned client gives no read of
//! the batches it keeps, so a host that acknowledges those events through the client's own
//! API before Paykit has seen them leaves the note; it then ends with its age, past the
//! window in which a kept batch's answer still tells whether it was applied
//! ([`KEPT_NOTE_MS`]).

use paykit_lib::AttemptId;
use pubky_switch_client::app::{AppEvent, SENT_RETENTION_MS};
use pubky_switch_core::enc::unb64_array;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Result};

/// How long a note says its batch is kept: the time a switch keeps a batch's proofs, past
/// which the client drops a batch sent and unanswered (Switch spec 12.5 "Kept batches"). A
/// batch never sent may stay kept longer; its note ends anyway, and a resume then reads the
/// claim, which shows no attempt until the batch goes, as without a note.
pub(crate) const KEPT_NOTE_MS: u64 = pubky_switch_core::ops::BATCH_PROOFS_MS;
use crate::journal::Stage;
use crate::paykit::Paykit;
use crate::state::{key, Tx};

/// A kept batch's note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptBatch {
    /// The request the batch records and queues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) msg_id: Option<String>,
    /// The attempt the batch writes, as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attempt_id: Option<String>,
    /// When the client kept it, device clock.
    pub(crate) kept_at_ms: u64,
    /// Why the client dropped it, unsent or refused for good: the switch's code, without
    /// content. `None` while it is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dropped: Option<String>,
}

impl Paykit {
    /// Note that the client keeps `batch_id`, carrying the request `msg_id` or the attempt
    /// `attempt`.
    pub(crate) async fn note_kept(
        &mut self,
        batch_id: [u8; 16],
        msg_id: Option<&str>,
        attempt: Option<&AttemptId>,
    ) -> Result<()> {
        let note = KeptBatch {
            msg_id: msg_id.map(str::to_owned),
            attempt_id: attempt.map(|attempt| attempt.to_text()),
            kept_at_ms: self.now(),
            dropped: None,
        };
        self.local_put(&key::kept_batch(&batch_id), &note).await
    }

    /// Every kept batch noted, by batch ID.
    async fn kept_batches(&self) -> Result<Vec<([u8; 16], KeptBatch)>> {
        let mut out = Vec::new();
        for (rest, note) in self.local_scan::<KeptBatch>(key::KEPT_BATCHES).await? {
            let id = unb64_array::<16>(&rest).map_err(|_| PaykitError::corrupt())?;
            out.push((id, note));
        }
        Ok(out)
    }

    /// The note of the kept batch carrying the request `msg_id`.
    pub(crate) async fn kept_request(&self, msg_id: &str) -> Result<Option<([u8; 16], KeptBatch)>> {
        Ok(self
            .kept_batches()
            .await?
            .into_iter()
            .find(|(_, note)| note.msg_id.as_deref() == Some(msg_id)))
    }

    /// Whether the batch carrying `attempt` is still kept: noted, and not dropped. A note
    /// of a dropped batch goes, so the attempt is resolved from its claim, which never shows
    /// it.
    pub(crate) async fn attempt_kept(&mut self, attempt: &AttemptId) -> Result<bool> {
        let text = attempt.to_text();
        for (id, note) in self.kept_batches().await? {
            if note.attempt_id.as_deref() != Some(text.as_str()) {
                continue;
            }
            if note.dropped.is_none() && !self.aged(&note) {
                return Ok(true);
            }
            self.local_delete(&key::kept_batch(&id)).await?;
        }
        Ok(false)
    }

    /// Whether `note` is past the time it says its batch is kept ([`KEPT_NOTE_MS`]).
    pub(crate) fn aged(&self, note: &KeptBatch) -> bool {
        self.now().saturating_sub(note.kept_at_ms) > KEPT_NOTE_MS
    }

    /// Take the client's events about kept batches into the notes: a batch dropped is
    /// marked, one signed again moves to its new ID, one applied at a move goes. Then the
    /// notes that no longer say anything go: a request the outbox holds, an attempt whose
    /// journal entry moved past `Recorded`, and any note past its window
    /// ([`KEPT_NOTE_MS`]) and the outbox's retention of sent messages after it.
    pub(crate) async fn note_kept_events(&mut self, events: &[AppEvent]) -> Result<()> {
        let notes = self.kept_batches().await?;
        if notes.is_empty() {
            return Ok(());
        }
        let mut held: std::collections::BTreeMap<[u8; 16], KeptBatch> = notes.into_iter().collect();
        let mut tx = Tx::default();
        for event in events {
            match event {
                AppEvent::BatchRefused { batch_id, code, .. } => {
                    if let Some(note) = held.get_mut(batch_id) {
                        note.dropped = Some(code.clone());
                        tx.put(&key::kept_batch(batch_id), note)?;
                    }
                }
                AppEvent::KeptBatchDropped { batch_id, .. } => {
                    if let Some(note) = held.get_mut(batch_id) {
                        note.dropped = Some("dropped unanswered".into());
                        tx.put(&key::kept_batch(batch_id), note)?;
                    }
                }
                AppEvent::AttemptDropped { attempt_id, .. } => {
                    let text = AttemptId::from_bytes(*attempt_id).to_text();
                    for (id, note) in held.iter_mut() {
                        if note.attempt_id.as_deref() == Some(text.as_str()) {
                            note.dropped = Some("dropped unanswered".into());
                            tx.put(&key::kept_batch(id), note)?;
                        }
                    }
                }
                AppEvent::KeptBatchSignedAgain {
                    batch_id,
                    new_batch_id,
                } => {
                    if let Some(note) = held.remove(batch_id) {
                        tx.delete(&key::kept_batch(batch_id));
                        tx.put(&key::kept_batch(new_batch_id), &note)?;
                        held.insert(*new_batch_id, note);
                    }
                }
                AppEvent::KeptBatchApplied { batch_id } if held.remove(batch_id).is_some() => {
                    tx.delete(&key::kept_batch(batch_id));
                }
                _ => {}
            }
        }
        let now = self.now();
        for (id, note) in &held {
            // Past its window and the outbox's retention after it, a note says nothing more.
            let old = now.saturating_sub(note.kept_at_ms)
                > KEPT_NOTE_MS.saturating_add(SENT_RETENTION_MS);
            let done = old
                || match (&note.msg_id, &note.attempt_id) {
                    (Some(msg_id), _) if note.dropped.is_none() => {
                        self.app.delivery(msg_id).await?.is_some()
                    }
                    (None, Some(attempt)) if note.dropped.is_none() => {
                        let attempt =
                            AttemptId::parse(attempt).map_err(|_| PaykitError::corrupt())?;
                        self.local_get::<crate::journal::JournalEntry>(&key::journal(&attempt))
                            .await?
                            .is_none_or(|entry| entry.stage != Stage::Recorded)
                    }
                    _ => false,
                };
            if done {
                tx.delete(&key::kept_batch(id));
            }
        }
        self.local_commit(tx).await
    }

    /// [`Paykit::note_kept_events`] over the duty events the host hasn't acknowledged.
    pub(crate) async fn note_unacknowledged(&mut self) -> Result<()> {
        if self.kept_batches().await?.is_empty() {
            return Ok(());
        }
        let events = self.app.unacknowledged_events().await?;
        self.note_kept_events(&events).await
    }
}
