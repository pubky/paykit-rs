use pubky_switch_client::app::{Refusal, WontDeliver};

/// A Switch rule that refused the operation, including a refusal beneath a retained batch.
#[derive(Clone, Debug, uniffi::Enum)]
pub enum SwitchRefusal {
    /// The installation stopped on an alarm.
    Stopped,
    /// The installation left its identity or was removed.
    Left,
    /// The installation is no longer current, or its account is gone.
    Gone,
    /// The service requires recovery of the followed generation.
    RecoveryRequired {
        /// The affected provider generation.
        generation: u64,
    },
    /// The signer approval did not pass the app's checks.
    Approval,
    /// No wallet is installed under the supplied App ID.
    NotJoined,
    /// The host must resolve an outstanding restore question.
    RestoreQuestionOpen,
    /// The current state prevents the operation.
    NotNow,
    /// The caller's input violates a rule.
    Usage,
    /// A protocol refused the installed certificate.
    Certificate {
        /// The protocol that refused it.
        protocol: String,
    },
    /// The store cannot carry all outstanding work into the restore.
    CarriedTooLarge {
        /// Number of outstanding duties.
        duty_events: u64,
        /// Number of outstanding protocol calls.
        owed_calls: u64,
    },
    /// The service can no longer safely receive this message.
    PastReplayWindow {
        /// The original replay deadline in Unix milliseconds.
        until_ms: u64,
    },
    /// The app already gave up on delivery.
    DeliveryGivenUp,
    /// The message does not fit in the outbox.
    DeliveryTooLarge,
    /// Earlier stored batches must settle first.
    KeptBatchesAhead {
        /// ID of the first outstanding batch.
        batch_id: Vec<u8>,
        /// First send time, when the batch was sent.
        kept_since_ms: Option<u64>,
        /// End of its original answer window, when sent.
        answer_until_ms: Option<u64>,
        /// Number of earlier stored batches.
        kept: u64,
    },
    /// An unrecognized native refusal. Keep the operation held.
    Unknown,
}

impl From<&Refusal> for SwitchRefusal {
    fn from(value: &Refusal) -> Self {
        match value {
            Refusal::Stopped => Self::Stopped,
            Refusal::Left => Self::Left,
            Refusal::Gone => Self::Gone,
            Refusal::RecoveryRequired { generation } => Self::RecoveryRequired {
                generation: *generation,
            },
            Refusal::Approval(_) => Self::Approval,
            Refusal::NotJoined => Self::NotJoined,
            Refusal::RestoreQuestionOpen => Self::RestoreQuestionOpen,
            Refusal::NotNow(_) => Self::NotNow,
            Refusal::Usage(_) => Self::Usage,
            Refusal::Certificate { protocol, .. } => Self::Certificate {
                protocol: protocol.clone(),
            },
            Refusal::CarriedTooLarge {
                duty_events,
                owed_calls,
            } => Self::CarriedTooLarge {
                duty_events: *duty_events,
                owed_calls: *owed_calls,
            },
            Refusal::WontDeliver(WontDeliver::PastReplayWindow { until_ms }) => {
                Self::PastReplayWindow {
                    until_ms: *until_ms,
                }
            }
            Refusal::WontDeliver(WontDeliver::GivenUp) => Self::DeliveryGivenUp,
            Refusal::WontDeliver(WontDeliver::TooLarge) => Self::DeliveryTooLarge,
            Refusal::KeptBatchesAhead(ahead) => Self::KeptBatchesAhead {
                batch_id: ahead.batch_id.into(),
                kept_since_ms: ahead.kept_since_ms,
                answer_until_ms: ahead.answer_until_ms,
                kept: ahead.kept,
            },
            _ => Self::Unknown,
        }
    }
}
