//! Reports from the Switch client. Duty IDs retain their original store epoch.
use crate::Version;
use pubky_switch_client::app;
/// A key bundle part's epoch range.
#[derive(uniffi::Record)]
pub struct EpochRange {
    /// The first data epoch.
    pub first: u64,
    /// The last data epoch.
    pub last: u64,
}
impl From<(u64, u64)> for EpochRange {
    fn from(v: (u64, u64)) -> Self {
        Self {
            first: v.0,
            last: v.1,
        }
    }
}
/// A protected-store failure encountered while processing a Switch event.
#[derive(uniffi::Record)]
pub struct EventStoreFailure {
    /// The typed error that prevented storage.
    pub error: crate::WalletError,
    /// Whether the failure concerns storage of the incoming entry.
    pub of_entry: bool,
}
impl From<app::StoreFailure> for EventStoreFailure {
    fn from(v: app::StoreFailure) -> Self {
        Self {
            error: v.error().clone().into(),
            of_entry: v.is_of_entry(),
        }
    }
}
/// An `attempt` on a claim (Switch wire 6.8), named by its claim key and attempt ID.
#[derive(uniffi::Record)]
pub struct ClaimAttempt {
    /// The claim key.
    pub claim_key: Vec<u8>,
    /// The attempt.
    pub attempt_id: Vec<u8>,
}
impl From<app::ClaimAttempt> for ClaimAttempt {
    fn from(v: app::ClaimAttempt) -> Self {
        Self {
            claim_key: v.claim_key.into(),
            attempt_id: v.attempt_id.into(),
        }
    }
}
/// The original observation of an inbox input in a verified feed response (Switch spec 8.2).
#[derive(uniffi::Record)]
pub struct EntryReceipt {
    /// The inbox entry's ID.
    pub entry_id: Vec<u8>,
    /// Hash of the invitation redeemed for this delivery, when one was used.
    pub invite_code_hash: Option<Vec<u8>>,
    /// The pinned service key that verified the feed response.
    pub service_key: Vec<u8>,
    /// The time stamped on that response, in Unix milliseconds.
    pub read_at_ms: u64,
}
impl From<app::EntryReceipt> for EntryReceipt {
    fn from(v: app::EntryReceipt) -> Self {
        Self {
            entry_id: v.entry_id.into(),
            invite_code_hash: v.invite_code_hash.map(Vec::from),
            service_key: v.service_key.into(),
            read_at_ms: v.read_at_ms,
        }
    }
}
/// The established identity of a signed input whose payload was dropped.
#[derive(uniffi::Record)]
pub struct StoredMessageIdentity {
    /// The signing app's ID.
    pub app_id: Vec<u8>,
    /// The signed message ID.
    pub msg_id: String,
    /// The signed message kind.
    pub kind: String,
    /// The signed plaintext's hash.
    pub hash: Vec<u8>,
    /// The verified certificate's hash.
    pub certificate_hash: Vec<u8>,
}
impl From<app::StoredMessageIdentity> for StoredMessageIdentity {
    fn from(v: app::StoredMessageIdentity) -> Self {
        Self {
            app_id: v.app_id.into(),
            msg_id: v.msg_id,
            kind: v.kind,
            hash: v.hash.into(),
            certificate_hash: v.certificate_hash.into(),
        }
    }
}
/// A write the switch showed that the app didn't take for being older than the one it holds
/// (`KeptReason::Older`, `KeptReason::Overtaken`).
#[derive(uniffi::Record)]
pub struct ShownWrite {
    /// Its version: from a transition proof or a verified stub when `authenticated`, else the
    /// record view's.
    pub version: Version,
    /// Whether a transition proof naming the write, or its verified chain stub, gives `version`
    /// (Switch spec 10.3).
    pub authenticated: bool,
    /// Its item hash, which its writer signed.
    pub item_hash: Vec<u8>,
    /// The app that signed it.
    pub writer: Vec<u8>,
}
impl From<app::ShownWrite> for ShownWrite {
    fn from(v: app::ShownWrite) -> Self {
        Self {
            version: v.version.into(),
            authenticated: v.authenticated,
            item_hash: v.item_hash.into(),
            writer: v.writer.into(),
        }
    }
}
/// Entries the service deleted while the app was lagging (Switch spec 8.2): the protocol asks
/// the counterparties of its unfinished activity to re-send what their inboxes accepted from
/// `accepted_from_ms`, through `accepted_until_ms` if it bounds its re-sends, and lists its
/// collections again.
#[derive(uniffi::Record)]
pub struct MissedEntries {
    /// The authenticated service time of the feed page that first incurred this call.
    pub read_at_ms: u64,
    /// The item's `seq`, in the sequence of the generation the app read it at: a call made again
    /// after a provider change keeps it, as information only.
    pub seq: u64,
    /// How many entries were missed.
    pub entries: u64,
    /// The time to ask from: the later of the item's first acceptance time and the cursor's time,
    /// less the clock tolerance.
    pub accepted_from_ms: u64,
    /// The item's last acceptance time, `accepted_until` (Switch wire 7.5), raised to
    /// `accepted_from_ms` when it is earlier.
    pub accepted_until_ms: u64,
}
impl From<app::MissedEntries> for MissedEntries {
    fn from(v: app::MissedEntries) -> Self {
        Self {
            read_at_ms: v.read_at_ms,
            seq: v.seq,
            entries: v.entries,
            accepted_from_ms: v.accepted_from_ms,
            accepted_until_ms: v.accepted_until_ms,
        }
    }
}
/// A change to the identity's deny list, as the feed shows it to an app holding `lists.read`
/// (Switch spec 8.3): a `deny.put` or a `deny.delete` whose signature the app verified, made
/// by an app of the identity whose certificate holds `lists.manage`, this one included.
#[derive(uniffi::Record)]
pub struct DenyListChange {
    /// The generation of the feed item.
    pub generation: u64,
    /// The item's `seq` in that generation.
    pub seq: u64,
    /// The pubky the change is about.
    pub pubky: Vec<u8>,
    /// `true` when the pubky was added to the deny list, `false` when it was removed.
    pub added: bool,
    /// The item's service time, in Unix milliseconds.
    pub at_ms: u64,
}
impl From<app::DenyListChange> for DenyListChange {
    fn from(v: app::DenyListChange) -> Self {
        Self {
            generation: v.generation,
            seq: v.seq,
            pubky: v.pubky.into(),
            added: v.added,
            at_ms: v.at_ms,
        }
    }
}
/// A provider loss as the protocol sees it (Switch spec 12.6).
#[derive(uniffi::Record)]
pub struct ProviderLoss {
    /// The generation lost.
    pub lost_generation: u64,
    /// The generation recovered at.
    pub generation: u64,
    /// When the loss was declared: the recovery record's service time, in Unix milliseconds.
    pub declared_at_ms: u64,
    /// The acceptance time to ask re-sends from: the time of the cursor at the old service
    /// (Switch spec 12.6 step 4).
    pub accepted_from_ms: u64,
    /// Until when an attempt could have been authorized at the old service: the declaration plus
    /// the residual window and the offset between the services' clocks.
    pub uncertain_until_ms: u64,
}
impl From<app::ProviderLoss> for ProviderLoss {
    fn from(v: app::ProviderLoss) -> Self {
        Self {
            lost_generation: v.lost_generation,
            generation: v.generation,
            declared_at_ms: v.declared_at_ms,
            accepted_from_ms: v.accepted_from_ms,
            uncertain_until_ms: v.uncertain_until_ms,
        }
    }
}
/// Entries the app can't read and whose senders it asks to send again (Switch spec 8.2): the
/// acceptance times to ask for.
#[derive(uniffi::Record)]
pub struct ResendWindow {
    /// The sender to ask.
    pub sender: Vec<u8>,
    /// The first acceptance time to ask for, service time in Unix milliseconds.
    pub accepted_from_ms: u64,
    /// The last acceptance time to ask for.
    pub accepted_until_ms: u64,
    /// Why: an entry wrapped to a key the app doesn't hold, or one a departed own app left
    /// unread.
    pub reason: ResendReason,
}
impl From<app::ResendNeeded> for ResendWindow {
    fn from(v: app::ResendNeeded) -> Self {
        Self {
            sender: v.sender.into(),
            accepted_from_ms: v.accepted_from_ms,
            accepted_until_ms: v.accepted_until_ms,
            reason: v.reason.into(),
        }
    }
}
// Each field is declared once, beside the conversion that preserves its native value.
// The surrounding WalletEvent carries the duty ID and its remaining protocol owners.
macro_rules! project_events {
    (
        $(#[$docs:meta])* $name:ident from $native:path {
            $( $(#[$variant_doc:meta])* $variant:ident
                $( { $( $(#[$field_doc:meta])* $field:ident: $ty:ty $(=> $map:expr)? ),* $(,)? } )?
                $(as ($pattern:pat))?,
            )*
        }
    ) => {
        $(#[$docs])*
        #[derive(uniffi::Enum)]
        pub enum $name {
            $( $(#[$variant_doc])* $variant $( { $( $(#[$field_doc])* $field: $ty ),* } )?, )*
            /// A case introduced by a newer Switch client.
            Other,
        }
        impl From<$native> for $name {
            fn from(value: $native) -> Self {
                use $native as Source;
                match value {
                    $( project_events!(@pattern $variant $(as ($pattern))? $({ $($field),* })?) =>
                        Self::$variant $( { $( $field: project_events!(@field $field $(=> $map)?) ),* } )?, )*
                    _ => Self::Other,
                }
            }
        }
    };
    (@pattern $variant:ident as ($pattern:pat) $($rest:tt)*) => { $pattern };
    (@pattern $variant:ident { $($field:ident),* }) => { Source::$variant { $($field,)* .. } };
    (@pattern $variant:ident) => { Source::$variant };
    (@field $field:ident => $map:expr) => { $map };
    (@field $field:ident) => { $field };
}

project_events! {
/// Evidence of misbehaviour, or of a compromise, that stops the app (Switch spec 3.6, 4.3,
/// 10.1).
SwitchAlarm from app::Alarm {
    /// The service rolled back or forked its chain, or signed an answer that contradicts itself
    /// (Switch spec 10.1).
    Rollback {
        /// The rule the chain broke.
        reason: String,
    },
    /// A protocol on top found, in what the service signed about its own data, that the service
    /// went back on its state or contradicts itself, and raised the alarm (`App::raise_rollback`,
    /// Switch spec 10.1).
    ProtocolRollback {
        /// The protocol that raised it.
        protocol: String,
        /// The rule the service broke, without content.
        reason: String,
    },
    /// Two `identity.json` bodies of the own identity at one version (Switch spec 4.3).
    Equivocation {
        /// The version.
        version: u64,
    },
    /// An own `identity.json` that goes back, or lists again an App ID an earlier version listed
    /// in `removed` (Switch spec 4.3).
    IdentityRollback {
        /// The rule the document broke.
        reason: String,
    },
    /// A registration of this app that neither it nor one of its devices made: a copy of its
    /// signing key is in someone else's hands (Switch spec 3.6).
    Compromise {
        /// The registration's `n`.
        n: u64,
        /// The `distrust_from` the app asks the signer for: the service time at which the
        /// registration was accepted, from its chain item.
        distrust_from: Option<u64>,
    },
    /// A time signed by the service key that is earlier than one the app verified before (Switch
    /// spec 10.1).
    ServiceTimeBackwards {
        /// The highest time verified before, in Unix milliseconds.
        highest: u64,
        /// The earlier time now signed.
        signed: u64,
    },
}
}

project_events! {
/// Why an entry was stored locally (Switch spec 8.2).
ParkReason from app::ParkReason {
    /// Its sender couldn't be checked within the bounded wait.
    Unverifiable,
    /// Its protocol has not admitted this input.
    AwaitingAdmission,
    /// Its shared record write hit a per-kind limit, the storage quota or the value size.
    Quota,
    /// Its effect's batch was refused for good (Switch spec 6.4): it is kept, not applied, and
    /// not tried again.
    Refused,
    /// Its effect has more operations than one batch takes with the cursor move (Switch wire
    /// 6.4): it is kept, not applied, and not tried again.
    TooManyOperations,
    /// Its effect is too large for the app to keep before it sends it, or to commit with its
    /// outcome: the kept effect over the store's value size (`store::MAX_VALUE_BYTES`), or its
    /// local writes over what one commit takes (`store::MAX_BATCH_BYTES`) (Switch spec 8.2).
    EffectTooLarge,
    /// Its receive step asked for more reads from the switch than one entry gets
    /// (`MAX_CLAIM_READS`), or again for what it was given: it is kept, not applied, and not
    /// tried again.
    TooManyReads,
    /// Its effect carries an operation the app layer builds itself (`app.put`, `receive_key.put`,
    /// `key_bundle.put`, `cert.put`), which a protocol may not write: it is kept, not applied,
    /// and not tried again (`AppEvent::EffectRefused`).
    LifecycleOperation,
    /// Its receive step was retried for want of the store (`Outcome::Retry`,
    /// `StoreFailure::unavailable`) for longer than the bounded wait: it is tried again at every
    /// read.
    StoreUnavailable,
    /// The store failed for its receive step in a way that retrying can't mend
    /// (`StoreFailure::of_entry`), such as a corrupt value: it is tried again at every read, in
    /// case the host repaired it.
    StoreRejected,
    /// The protocol needs data before it can apply the input.
    ProtocolDeferred,
    /// Its effect was kept unanswered longer than a resend's answer can tell whether it was
    /// applied (Switch spec 12.5 "Kept batches"): it is kept, and not tried again
    /// (`AppEvent::EffectUncertain`).
    Uncertain,
    /// It was stored as `ParkReason::Uncertain`, and the host released it
    /// (`App::release_parked`): its processed pair is gone, and a later read tries it again as a
    /// fresh receive, with `ReceivedMessage::released` set.
    Released,
    /// Its receive step needs a scope key of `data_epoch` that the app doesn't hold yet (Switch
    /// spec 3.5, 8.2): the step answered `Outcome::AwaitKey`, or applied an effect with a write
    /// the context couldn't seal for want of the current data epoch's key, after the key bundle
    /// was fetched once.
    AwaitingKey {
        /// The data epoch whose key the step waits for.
        data_epoch: u64,
    },
}
}

project_events! {
/// Why an entry waiting for sender standing, a key or admission left custody without being
/// processed (Switch spec 8.2, `AppEvent::StoredEntryDropped`): one of the shared bounds it
/// was past, or a recipient check it failed when tried again.
DropReason from app::DropReason {
    /// It was kept longer than `MAX_UNVERIFIED_AGE_MS` from its acceptance, judged on the highest
    /// service time the app verified.
    Age,
    /// Its sender identity held more than `MAX_UNVERIFIED_PER_SENDER` such entries, and it was
    /// that sender's accepted earliest.
    PerSender,
    /// More than `MAX_UNVERIFIED_TOTAL` such entries, of both reasons, were stored, and it was
    /// the one accepted earliest of the sender identity that held the most.
    Total,
    /// Tried again, it failed recipient check `check` (Switch wire 5.1) under the rules current
    /// then, such as a body its protocol's opaque members no longer allow.
    Rules {
        /// The check that failed, 2 to 8.
        check: u8,
    },
}
}

/// What a locally retained input waits for (Switch spec 8.2).
#[derive(uniffi::Enum)]
pub enum WaitingReason {
    /// Its sender's standing could not be checked.
    Sender,
    /// Its receive step needs a scope key.
    Key {
        /// The epoch whose key is needed.
        data_epoch: u64,
    },
    /// Its protocol has not admitted this input under its ordinary retained-state rule.
    Admission,
}
impl From<app::WaitingReason> for WaitingReason {
    fn from(value: app::WaitingReason) -> Self {
        match value {
            app::WaitingReason::Sender => Self::Sender,
            app::WaitingReason::Key { data_epoch, .. } => Self::Key { data_epoch },
            app::WaitingReason::Admission => Self::Admission,
        }
    }
}
project_events! {
/// Why a renewed certificate wasn't taken (`AppEvent::CertificateRefused`).
CertificateRefusal from app::CertificateRefusal {
    /// It holds a scope of this root, which no registered protocol owns: an app with it would
    /// never load with its protocols (Switch wire 3.1 "Scope roots").
    UnownedRoot {
        /// The permission root that no registered protocol owns.
        value: String,
    } as (Source::UnownedRoot(value)),
    /// The registered protocol `protocol` refused it (`Protocol::check_own_certificate`).
    Protocol {
        /// The protocol's name.
        protocol: String,
        /// Its reason, without content.
        reason: String,
    },
}
}

project_events! {
/// A protocol call whose local writes were over the bound (`AppEvent::LocalWritesRefused`).
RefusedCall from app::RefusedCall {
    /// `Protocol::resend_needed`, about the feed item at `seq`.
    ResendNeeded {
        /// The feed item's `seq`.
        seq: u64,
        /// What the call asked.
        needed: ResendWindow => needed.into(),
    },
    /// `Protocol::missed`.
    Missed {
        /// The verified missed-entry interval.
        value: MissedEntries => value.into(),
    } as (Source::Missed(value)),
    /// `Protocol::deny_list_changed`.
    DenyListChanged {
        /// The change in peer eligibility recorded by the service.
        value: DenyListChange => value.into(),
    } as (Source::DenyListChanged(value)),
    /// `Protocol::deny_list_gap`, about the gap after this position; the copy of the lists was
    /// taken without its writes.
    DenyListGap {
        /// The generation of the last position the history is known complete through.
        generation: u64,
        /// That position's `seq`.
        seq: u64,
    },
    /// `Protocol::provider_lost`, its `LossActions::local`, at this loss; the loss went on
    /// without them.
    ProviderLost {
        /// The service generation and uncertainty interval declared by the identity.
        value: ProviderLoss => value.into(),
    } as (Source::ProviderLost(value)),
}
}

project_events! {
/// Why the app asks for a re-send.
ResendReason from app::ResendReason {
    /// The entry is wrapped to a key the app doesn't hold (after a restore).
    Unopenable,
    /// A departed own app left the entry unread.
    Unreadable,
}
}

project_events! {
/// Why a backup was refused (`AppEvent::BackupRefused`).
BackupRefusal from app::BackupRefusal {
    /// The restore's one commit wouldn't fit the store's value or batch size: the layer, or the
    /// entries stored locally and the processed pairs it carries, are too large for now.
    TooLarge,
    /// The restore's one commit wouldn't fit beside the duty events the protocols haven't
    /// acknowledged, which the backup carries: acknowledge the duty events acted on, then
    /// take the backup again.
    DutiesTooLarge {
        /// The duty events the backup would carry.
        duty_events: u64,
    },
}
}

/// What a restored backup left out of its lists because they didn't fit beside the rest
/// (Switch spec 7): its kept peers and reply grants first, and its deny entries only if
/// those don't fit either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ListsLeftOut {
    /// The backup carried its lists whole; also at a provider loss.
    Nothing,
    /// The backup carried its deny entries alone.
    KeptAndReplies,
    /// The backup carried no lists.
    Everything,
}
impl From<app::ListsLeftOut> for ListsLeftOut {
    fn from(value: app::ListsLeftOut) -> Self {
        match value {
            app::ListsLeftOut::Nothing => Self::Nothing,
            app::ListsLeftOut::KeptAndReplies => Self::KeptAndReplies,
            app::ListsLeftOut::Everything => Self::Everything,
        }
    }
}

/// Why a verified Switch notice holds the installation (Switch spec 12.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RecoveryReason {
    /// Account state is behind its signed service head or generation.
    Behind,
    /// The signed chain head and the account's chain hash disagree.
    Fork,
    /// The account's service mark is missing or unreadable.
    MarkLost,
}

/// The current installation's verified recovery notice, retained by Switch through reopen.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WalletRecoveryNotice {
    /// Generation of the affected installation.
    pub generation: u64,
    /// The understood reason, if any.
    pub reason: Option<RecoveryReason>,
    /// Whether the service retired this generation.
    pub permanent: bool,
    /// Device time when this installation first observed the notice, in Unix milliseconds.
    pub first_seen_ms: u64,
    /// Signed service time in Unix milliseconds.
    pub service_time_ms: u64,
}
impl From<app::RecoveryNotice> for WalletRecoveryNotice {
    fn from(value: app::RecoveryNotice) -> Self {
        use pubky_switch_core::http::NoticeReason;
        Self {
            generation: value.generation,
            reason: value.reason.map(|r| match r {
                NoticeReason::Behind => RecoveryReason::Behind,
                NoticeReason::Fork => RecoveryReason::Fork,
                NoticeReason::MarkLost => RecoveryReason::MarkLost,
            }),
            permanent: value.permanent,
            first_seen_ms: value.first_seen_ms,
            service_time_ms: value.service_time_ms,
        }
    }
}

project_events! {
/// One thing that happened, for the host to show or act on.
WalletEventData from app::AppEvent {
    /// Switch has verified that the installation needs recovery.
    RecoveryRequired {
        /// The complete notice that produced this duty.
        notice: WalletRecoveryNotice => notice.into(),
    },
    /// Switch verified that the notice cleared, including when a later generation was followed.
    RecoveryNoticeCleared {
        /// Generation of the notice that cleared.
        generation: u64,
    },
    /// A verified alarm stopped the app.
    Alarm {
        /// The original alarm that stopped the app.
        value: SwitchAlarm => value.into(),
    } as (Source::Alarm(value)),
    /// The message and its effects were committed.
    MessageReceived {
        /// The original verified feed observation, including any redeemed invitation.
        receipt: EntryReceipt => receipt.into(),
        /// The entry's `seq`.
        seq: u64,
        /// The message kind.
        kind: String,
        /// The message ID.
        msg_id: String,
        /// The sender identity.
        sender: Vec<u8> => sender.into(),
    },
    /// A message failed an incoming check.
    MessageDropped {
        /// The entry's `seq`.
        seq: u64,
        /// The check of Switch wire 5.1 that failed, 1 to 10.
        check: u8,
        /// The rule, without content.
        reason: String,
    },
    /// The sender repeated an existing message ID.
    Duplicate {
        /// The entry's `seq`.
        seq: u64,
        /// Whether the bytes differ from the message processed before.
        conflicting: bool,
    },
    /// The sender could not yet be verified.
    Held {
        /// The entry's `seq`.
        seq: u64,
    },
    /// The owning protocol needs more data to process the input.
    ProtocolDeferred {
        /// The entry's original feed position.
        seq: u64,
        /// Its local ID if this attempt was made from stored custody.
        stored_id: Option<u64>,
        /// The registered protocol that owns the message kind.
        protocol: String,
        /// The protocol's content-free reason for waiting on this attempt.
        reason: String => reason.into(),
    },
    /// Repeated storage failures have held this entry.
    StoreFailing {
        /// The entry's `seq`, the one it was read at for an entry stored locally.
        seq: u64,
        /// The entry's local ID when it is stored locally.
        stored_id: Option<u64>,
        /// The reads in a row that ended in a retry at the entry, this one included.
        reads: u32,
        /// When the first of them was, device time in Unix milliseconds.
        since_ms: u64,
        /// The store's failure on this read.
        failure: EventStoreFailure => failure.into(),
    },
    /// The app cannot produce a backup.
    BackupRefused {
        /// Why.
        reason: BackupRefusal => reason.into(),
    },
    /// An entry remains in local custody.
    Parked {
        /// The `seq` the entry was read at.
        seq: u64,
        /// The entry's local ID while it is stored (`App` keys stored entries by it): it never
        /// changes, across provider changes too.
        stored_id: u64,
        /// Why.
        reason: ParkReason => reason.into(),
    },
    /// The service refused the entry's proposed effect.
    EffectRefused {
        /// The `seq` the entry was read at.
        seq: u64,
        /// The entry's local ID while it is stored (`App` keys stored entries by it): it never
        /// changes, across provider changes too.
        stored_id: u64,
        /// The sender identity.
        sender: Vec<u8> => sender.into(),
        /// The message ID.
        msg_id: String,
        /// The message kind.
        kind: String,
        /// The error code of the refusal; `None` for an effect the app refused itself, over one batch
        /// or carrying an operation the app layer builds (`ParkReason` says which).
        code: Option<String>,
    },
    /// The outcome of the entry's effect is uncertain.
    EffectUncertain {
        /// The `seq` the entry was read at.
        seq: u64,
        /// The entry's local ID while it is stored (`App` keys stored entries by it): it never
        /// changes, across provider changes too.
        stored_id: u64,
        /// The sender identity.
        sender: Vec<u8> => sender.into(),
        /// The message ID.
        msg_id: String,
        /// The message kind.
        kind: String,
        /// The effect's own `attempt`s: whether each was applied can't be known.
        attempts: Vec<ClaimAttempt> => attempts.into_iter().map(Into::into).collect(),
    },
    /// A duplicate was released from local custody.
    ReleasedDuplicate {
        /// The `seq` the entry was read at.
        seq: u64,
        /// The entry's local ID while it was stored.
        stored_id: u64,
        /// The sender identity.
        sender: Vec<u8> => sender.into(),
        /// The message ID.
        msg_id: String,
        /// Whether the bytes differ from the message processed before.
        conflicting: bool,
    },
    /// A stored entry was removed before processing.
    StoredEntryDropped {
        /// The entry's local ID while it was stored.
        stored_id: u64,
        /// The `seq` the entry was read at, in `generation`.
        seq: u64,
        /// The generation it was read at.
        generation: u64,
        /// The sender identity, the one its certificate names.
        sender: Vec<u8> => sender.into(),
        /// When its inbox accepted it, service time in Unix milliseconds: the time a re-send request
        /// asks from.
        accepted_at_ms: u64,
        /// Which bound it was past, or which recipient check it failed.
        reason: DropReason => reason.into(),
        /// What the input waited for when it was selected.
        waiting: WaitingReason => waiting.into(),
        /// The identity established for this signed input.
        message: StoredMessageIdentity => message.into(),
        /// Whether the sender's standing was checked before this input was stored.
        sender_verified: bool,
        /// The original verified response observation, not a portable signed proof.
        receipt: EntryReceipt => receipt.into(),
        /// The immutable identity of this notice across restore. `id` names its current delivery;
        /// this identity names the same obligation when delivery IDs change.
        notice_id: Vec<u8> => notice_id.to_bytes().to_vec(),
    },
    /// A locally held entry became eligible for processing.
    Unparked {
        /// The `seq` the entry was read at.
        seq: u64,
        /// The entry's local ID while it is stored (`App` keys stored entries by it): it never
        /// changes, across provider changes too.
        stored_id: u64,
    },
    /// An envelope could not be opened.
    Unopenable {
        /// The entry's `seq`.
        seq: u64,
        /// Whether a re-send was asked; never for a time before the cursor's, less the clock
        /// tolerance, and not when the protocol's call failed: it is owed then, and asked by a later
        /// read (`App::owed_protocol_calls`).
        resend_asked: bool,
    },
    /// An opened message could not be read.
    Unreadable {
        /// The item's `seq`.
        seq: u64,
        /// Whether a re-send was asked: only for an entry accepted at or before a departure the app
        /// verified in its chain, and not when the protocol's call failed: it is owed then, and asked
        /// by a later read.
        resend_asked: bool,
    },
    /// A protocol callback still awaits completion.
    OwedCallHeld {
        /// The reads in a row that stopped there, this one included.
        reads: u32,
    },
    /// A protocol could not apply a deny-list change.
    DenyListChangeRefused {
        /// The generation of the feed item.
        generation: u64,
        /// Its `seq`.
        seq: u64,
        /// The rule it broke.
        reason: String,
    },
    /// The service's cursor is ahead of the installed checkpoint.
    HeldCursorAhead {
        /// The `seq` the service reports.
        held: u64,
        /// The `seq` of the furthest position the app committed or sent.
        position: u64,
    },
    /// The app verified that feed entries were missed.
    Missed {
        /// The item's `seq`.
        seq: u64,
        /// How many.
        entries: u64,
    },
    /// A new receive key was registered.
    Rotated {
        /// Its `n`.
        n: u64,
    },
    /// A receive-key rotation did not survive.
    RotationLost {
        /// The other device's key's `n`.
        n: u64,
    },
    /// Retired keys were deleted.
    KeysDeleted {
        /// How many.
        count: u64,
    },
    /// The app awaits protected key synchronization.
    AwaitingDeviceSync {
        /// The registration's `n`.
        n: u64,
    },
    /// The restore needs a host decision.
    RestoreQuestion {
        /// The registrations' numbers.
        n: Vec<u64>,
    },
    /// Shared data keys for the named epochs are missing.
    AwaitingKeys {
        /// The data epochs waited for, each once, lowest first.
        epochs: Vec<u64>,
        /// How many entries wait.
        entries: u64,
    },
    /// Protected records still need to be reopened.
    ReopeningOwed {
        /// Why, without content.
        error: String,
    },
    /// Shared data keys are available through this epoch.
    KeysThrough {
        /// The epoch.
        epoch: u64,
    },
    /// Part of a key bundle could not be used.
    BundlePartSkipped {
        /// The range it names.
        range: EpochRange => range.into(),
        /// Why, without content.
        reason: String,
        /// Whether the user is alerted: another naming key than the identity's.
        alert: bool,
    },
    /// The app requested a complete key bundle.
    FullBundleRequested,
    /// The app observed a signed identity update.
    IdentityUpdated {
        /// Its version.
        version: u64,
    },
    /// The identity selected another provider.
    ProviderChanged {
        /// The new generation.
        generation: u64,
    },
    /// The app learned of a certificate renewal.
    RenewalSeen,
    /// The app installed its renewed certificate.
    CertificateTaken {
        /// The new certificate's `issued_at`.
        issued_at: u64,
    },
    /// An app-record protocol member was removed.
    MemberDropped {
        /// The renewed certificate's `issued_at`.
        issued_at: u64,
        /// The protocol's name.
        protocol: String,
    },
    /// An app-record protocol member changed.
    MemberChanged {
        /// The protocol's name.
        protocol: String,
    },
    /// The app could not accept a certificate.
    CertificateRefused {
        /// The refused certificate's `issued_at`.
        issued_at: u64,
        /// Why it was refused.
        refusal: CertificateRefusal => refusal.into(),
    },
    /// The app could not publish its signed record.
    RecordPublishFailed {
        /// Why the write failed.
        error: String,
    },
    /// Listing awaits a required shared data key.
    ListingWaitsForKey,
    /// This app was removed from the identity.
    Removed,
    /// The app lacks verified continuity for the reported interval.
    ContinuityUnverified {
        /// The last `seq` the app had verified.
        verified_seq: u64,
        /// The checkpoint's `seq`.
        checkpoint_seq: u64,
    },
    /// The app observed the provider move.
    Moved,
    /// The device and service clocks differ.
    ClockSkew {
        /// The difference or known forward lower bound, in milliseconds.
        skew_ms: u64,
    },
    /// Two verified peer identity records conflict.
    PeerEquivocation {
        /// The identity.
        identity: Vec<u8> => identity.into(),
        /// The version.
        version: u64,
    },
    /// The app left its account.
    Left,
    /// The app destroyed its local keys.
    KeysDestroyed,
    /// Retained evidence was uploaded.
    EvidenceUploaded {
        /// The heads the service counted.
        heads: u64,
        /// The proofs the service kept or checked.
        proofs: u64,
    },
    /// The provider refused uploaded evidence.
    EvidenceRefused {
        /// The error code of the refusal.
        code: String,
    },
    /// The provider move was cancelled.
    MoveCancelled,
    /// The app followed the identity's provider change.
    ProviderFollowed {
        /// The generation followed.
        generation: u64,
    },
    /// The service shows a prior receive key.
    ReceiveKeyRolledBack {
        /// The `n` the service shows.
        shown: u64,
        /// The `n` of the app's current key.
        current: u64,
    },
    /// A restore, or a provider loss whose upload contributed them, left kept peers or reply
    /// grants unverified, or the restored backup left part of its lists out (Switch spec 7,
    /// 12.4, 12.6): they don't admit until a list read or a change to that peer confirms
    /// them. Show the user, who may look over the kept peers, then acknowledge it.
    ListsUnverified {
        /// The kept peers not yet verified.
        kept: u64,
        /// The reply grants not yet verified.
        replies: u64,
        /// Whether a provider loss's upload contributed the `kept` peers.
        contributed: bool,
        /// What the restored backup left out of its lists.
        left_out: ListsLeftOut => left_out.into(),
    },
    /// The verified service and identity inventories differ.
    ListsDiffer {
        /// Kept peers and deny list entries added back.
        added: u64,
        /// Kept peers the service holds that the app never saw added; shown, not removed.
        unverified_kept_peers: u64,
        /// Invite codes withdrawn.
        withdrawn_codes: u64,
    },
    /// The identity declared a provider loss.
    ProviderLost {
        /// The generation lost.
        lost_generation: u64,
        /// The generation recovered at.
        generation: u64,
    },
    /// The app followed the declared recovery.
    ProviderRecovered {
        /// The generation recovered at.
        generation: u64,
    },
    /// After a provider loss, `over` of the app's deny entries don't fit the replacement's
    /// deny list and nothing of the list was merged: the user unblocks that many peers, and
    /// the recovery is followed again (Switch docs/app-host-guide.md).
    RecoveryDenyListFull {
        /// The generation being recovered.
        generation: u64,
        /// How many of the app's new deny entries don't fit.
        over: u64,
    },
    /// A newer provider change superseded this one.
    ProviderChangeSuperseded {
        /// The generation the change being followed went to.
        generation: u64,
        /// The generation the newest own `identity.json` names.
        newer: u64,
    },
    /// A retained operation batch was applied.
    KeptBatchApplied {
        /// The batch ID it was kept under.
        batch_id: Vec<u8> => batch_id.into(),
    },
    /// A retained operation batch was signed again.
    KeptBatchSignedAgain {
        /// The batch ID it is kept under, which doesn't change.
        batch_id: Vec<u8> => batch_id.into(),
        /// The batch ID its signed bytes carry now, on the wire (Switch wire 6.4).
        wire_batch_id: Vec<u8> => wire_batch_id.into(),
    },
    /// Retained batches still await an outcome.
    KeptBatchesWaiting {
        /// How many batches are kept now.
        kept: u64,
        /// The switch's error code; `None` for the network or an answer that didn't verify.
        code: Option<String>,
    },
    /// A retained batch was dropped.
    KeptBatchDropped {
        /// The batch ID it was kept under, whatever ID its bytes carried once signed again.
        batch_id: Vec<u8> => batch_id.into(),
    },
    /// A retained claim attempt was dropped.
    AttemptDropped {
        /// The claim key.
        claim_key: Vec<u8> => claim_key.into(),
        /// The attempt.
        attempt_id: Vec<u8> => attempt_id.into(),
    },
    /// A restored installation cannot use some retained work.
    RestoreLeftBehind {
        /// The duty events left behind.
        duty_events: u64,
        /// The owed protocol calls left behind.
        owed_calls: u64,
        /// The record changes held aside that the restore couldn't keep (Switch spec 4.2):
        /// each protocol checks that the app record says what it wants.
        record_edits: u64,
    },
    /// Changes of the app record that a later run or a restore held aside for their
    /// protocols, which read, submit again or clear them (Switch spec 4.2). Paykit decides
    /// its own at the load or the next receive and acknowledges this for itself.
    RecordEditsHeld {
        /// The protocols whose member change or renewal is held aside.
        protocols: Vec<String>,
        /// Whether a change of the accepted kinds is held aside.
        accepts_changed: bool,
    },
    /// The authenticated account is unavailable.
    AccountGone as (Source::AccountGone { .. }),
    /// The account became available again.
    AccountBack,
    /// The service registered the push handle.
    PushRegistered {
        /// The handles the service keeps.
        handles: u64,
    },
    /// The service removed the push handle.
    PushRemoved,
    /// The receiver's service accepted the message.
    MessageAccepted {
        /// The message ID.
        msg_id: String,
        /// The acceptance time, the recipient's service time.
        accepted_at_ms: u64,
    },
    /// The message did not reach the receiver.
    MessageUndelivered {
        /// The message ID.
        msg_id: String,
        /// Whether this app had the message accepted before, and only a re-send of it was given up.
        resend: bool,
    },
    /// The app cannot determine whether the message arrived.
    DeliveryUnknown {
        /// The message ID.
        msg_id: String,
    },
    /// A pending record operation was dropped.
    RecordJobDropped {
        /// The message ID.
        msg_id: String,
        /// The batch the job's latest try left kept, if one did.
        batch_id: Option<Vec<u8>> => batch_id.map(Vec::from),
    },
    /// The app stopped retrying a record operation.
    RecordJobStopped {
        /// The message ID.
        msg_id: String,
        /// The record key.
        key: Vec<u8> => key.into(),
        /// The delete's generation.
        generation: u64,
        /// The delete's version.
        version: u64,
        /// The item hash of the signed delete operation (Switch wire 6.3).
        item_hash: Vec<u8> => item_hash.into(),
        /// The batch an earlier try left kept that may have applied, if one did.
        batch_id: Option<Vec<u8>> => batch_id.map(Vec::from),
    },
    /// Record recovery awaits a verified condition.
    PutBackWaits {
        /// The record key.
        key: Vec<u8> => key.into(),
        /// The generation of the signed delete.
        generation: u64,
        /// The version of the signed delete.
        version: u64,
        /// The item hash of the signed delete.
        item_hash: Vec<u8> => item_hash.into(),
    },
    /// The service refused the proposed recovery write.
    PutBackRefused {
        /// The record key.
        key: Vec<u8> => key.into(),
        /// The generation of the signed delete.
        generation: u64,
        /// The version of the signed delete.
        version: u64,
        /// The item hash of the signed delete.
        item_hash: Vec<u8> => item_hash.into(),
        /// Why the replacement could not be written.
        reason: String,
        /// The replacement already sent, if it was, whose outcome may not be known yet: the
        /// kept batch tells what became of it.
        batch_id: Option<Vec<u8>> => batch_id.map(Vec::from),
    },
    /// The app could not apply locally retained writes.
    LocalWritesRefused {
        /// The call whose writes were refused.
        call: RefusedCall => call.into(),
    },
    /// The receiver's app could not be reached.
    UnreachableApp {
        /// The counterparty.
        identity: Vec<u8> => identity.into(),
        /// The app.
        app_id: Vec<u8> => app_id.into(),
    },
    /// A message may need resending after a provider loss.
    ResendAfterLoss {
        /// The counterparty.
        identity: Vec<u8> => identity.into(),
        /// How many messages.
        messages: u64,
    },
    /// The app observed a shared record change.
    RecordChanged {
        /// The record key.
        key: Vec<u8> => key.into(),
    },
    /// The service did not take a submitted write.
    WriteNotTaken {
        /// The record key.
        key: Vec<u8> => key.into(),
    },
    /// The service showed an older record revision.
    RecordShownOlder {
        /// The record key.
        key: Vec<u8> => key.into(),
        /// The write shown.
        write: ShownWrite => write.into(),
    },
    /// A record write needs host attention.
    WriteFlagged {
        /// The record key.
        key: Vec<u8> => key.into(),
        /// The app the write names.
        writer: Vec<u8> => writer.into(),
        /// The rule, without content.
        reason: String,
    },
    /// The app cannot verify the shared record.
    RecordUncheckable {
        /// The record key.
        key: Vec<u8> => key.into(),
        /// The app the write names.
        writer: Vec<u8> => writer.into(),
        /// The write's generation and `seq`.
        version: Version => version.into(),
        /// The size of the mark that wasn't kept.
        mark_bytes: u64,
    },
    /// A restore's seen marks were incomplete (Switch spec 12.4 "Seen marks"): from then on
    /// a record without a mark is checked as new. Raised once, at that restore, for each
    /// registered protocol; not kept, and nothing to acknowledge. A notice: show it, or log it.
    SeenMarksIncomplete {
        /// The protocol told.
        protocol: String,
    },
    /// The app observed divergent shared record histories.
    RecordDivergent {
        /// The record key.
        key: Vec<u8> => key.into(),
    },
    /// The app observed a different claim revision.
    ClaimChanged {
        /// The claim key.
        claim_key: Vec<u8> => claim_key.into(),
    },
    /// The claim differs from its retained pin.
    ClaimPinDiffers {
        /// The claim key.
        claim_key: Vec<u8> => claim_key.into(),
    },
    /// The service refused the operation batch.
    BatchRefused {
        /// The batch ID it was kept under, whatever ID its bytes carried once signed again.
        batch_id: Vec<u8> => batch_id.into(),
        /// The error code of the refusal.
        code: String,
    },
    /// The service does not hold the claim attempt.
    AttemptNotHeld {
        /// The claim key.
        claim_key: Vec<u8> => claim_key.into(),
        /// The attempt.
        attempt_id: Vec<u8> => attempt_id.into(),
    },
}
}
