//! Notices: facts about this app's payments and messages that the user must be told and that
//! no call's result carries, kept in Paykit's local state (`p/nt/`) until the host
//! acknowledges them, and carried in the app's backups (Paykit spec 7.2).
//!
//! A notice is raised once per attempt and kind, or per message given up: raising it again,
//! as a re-sent receipt or a
//! later resume would, changes nothing, and an acknowledged one stays acknowledged. A notice
//! that messages a block dropped can no longer come back is kept per window: per
//! counterparty and the window's earliest acceptance time, which its key names; it is
//! deleted at its acknowledgement. An outgoing message with an uncertain disposition is
//! identified by its recipient, ID and signed hash. Its acknowledgment survives later
//! imports into the same wallet store.

use paykit_lib::AttemptId;
use pubky_switch_core::enc::{b64, serde_b64, unb64_array};
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::journal::JournalEntry;
use crate::paykit::Paykit;
use crate::state::{key, StateKey};

/// What a notice tells the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoticeKind {
    /// "Marked failed, was paid": the payee's receipt confirms a payment whose attempt was
    /// written `failed`, and it arrived once the claim was gone, 30 days after its deadline,
    /// so it can't be recorded on the claim (Paykit spec 7.2, "What a payment method
    /// declares"). An app that gets the receipt and holds the failure raises it. An app
    /// holds the failure when it has it from a stable source: the attempt's owner from its
    /// own journal, or any own app, the owner included, from a view of the claim, kept or
    /// read, that shows the attempt `failed`. An owner restored from a backup that predates
    /// the failure has no journal entry for it, and uses such a view as any other own app
    /// does. The record's `status` is informational and never decides it. An own app that
    /// never saw the failure, such as one that joined after the service deleted the claim,
    /// can't tell and raises nothing. Either way the app records the receipt in its
    /// activity record.
    PaidAfterFailed,
    /// The payment is held: the app's certificate no longer holds `paykit_pay`, so the app reads
    /// and writes no claim and no payer record, and the attempt waits, open, for an app
    /// holding `paykit_pay` (Paykit spec 3.1, 7.3). The payment reports only
    /// [`crate::PayOutcome::Pending`] or [`crate::PayOutcome::PaidOutcomeUnwritten`]
    /// meanwhile.
    HeldWithoutPay,
    /// A message whose effect the switch refused for good, kept to write into its record
    /// later, wasn't written within [`crate::KEPT_MESSAGE_MS`] of its acceptance: the SDK gave
    /// it up (Switch spec 8.2). Its entry stays stored, for the host
    /// ([`pubky_switch_client::app::App::parked_entries`]), and a request it was no longer
    /// shows as [`crate::RequestStatus::Unrecorded`]. Acknowledging the notice
    /// ([`Paykit::acknowledge_notice`]) forgets the entry first
    /// ([`pubky_switch_client::app::App::forget_parked`]); if that fails, the notice stays
    /// and the error is returned, so an entry is never left stored once its notice is gone.
    ///
    /// Also a rejection or payer's cancellation this app recorded while sends to the payee
    /// were held ([`Paykit::reject_request`], [`Paykit::cancel_request`]) whose replay window
    /// of 30 days from its signing passed before they weren't, or one queued and never
    /// tried that was still in the outbox past that window: it was never sent, and never
    /// will be. Its request record still holds it, so no own app pays or accepts the
    /// request; the payee wasn't told. `counterparty` is then the payee, and `msg_id` the
    /// answer's. The same notice tells of such an answer whose request record, read and not
    /// paused as divergent, no longer holds it nor any stop, as after a provider loss whose
    /// confirmed write lacks it: it was never sent, the request may be paid or accepted
    /// again, and the user may answer it again.
    MessageGivenUp,
    /// Ordinary placement expired, but the wallet retained a hold on this Allowance
    /// because the signed message could still establish a conflict.
    AllowancePlacementExpired,
    /// The named acceptance matches this payer's proposal from before its declared
    /// provider loss. It cannot open a budget. Withdrawal requires a verified live original.
    AllowanceAcceptanceBeforeLoss,
    /// The seller confirmed the end of this wallet's withdrawn pre-loss proposal.
    AllowanceWithdrawalConfirmed,
    /// An original outgoing message found after restore may have reached its recipient. So
    /// may a rejection or payer's cancellation tried before it was held, whose outbox copy
    /// was ended past its 30 days (Paykit spec 8.1). Reconcile its outcome with the
    /// counterparty. Acknowledging it records that the host
    /// has seen it, without authorizing delivery or a replacement (Paykit spec 7.2).
    PossiblyDelivered,
    /// The client gave up a counterparty's current Private Payment List, and its delivery
    /// is unknown. `counterparty` is the recipient, `msg_id` and `message_hash` the list.
    /// Nothing more is sent on its own, but an answer to the counterparty's list request
    /// still goes. Acknowledging the notice authorizes nothing. A call that would otherwise
    /// send nothing for that counterparty reports the list as
    /// [`crate::Refusal::ListDeliveryUnknown`], whose value the user confirms another
    /// version with: [`Paykit::publish_list_to`] for a chosen counterparty and
    /// [`Paykit::withdraw_list_from`] for a withdrawn one or for an emptied list. A call
    /// that changes what the counterparty gets, such as withdrawing a nonempty list, sends
    /// the new version instead, and the notice is removed (Paykit spec 8.4, 9).
    ListDeliveryUnknown,
    /// Messages a blocked counterparty's inbox accepted from
    /// [`Notice::lost_from_ms`] to [`Notice::lost_until_ms`], which the app dropped for the
    /// user's block to ask for again once the counterparty is unblocked, can no longer come
    /// back: its sender keeps them no longer, or the bound on marks dropped this one (Paykit
    /// spec 8.1, "Blocked counterparties"). The app never kept them, so the notice holds no
    /// content: a payment among them shows only in the payment method.
    BlockedMessagesLost,
    /// The settlement bucket cannot take a new mark (Paykit wire 5.7).
    SettlementBucketFull,
    /// This settlement can only be credited with the user's confirmation.
    CreditNeedsUser,
    /// An authenticated bucket omitted or raised a lower mark the wallet retained.
    SettlementMarksChanged,
    /// One settlement was credited to different occurrences.
    SettlementCreditedTwice,
    /// A bounded recovery family discarded a window before a request covering it was
    /// delivered. Recovery of those messages is no longer tracked (Paykit spec 9).
    DroppedMessagesLost,
    /// The identity's block floor was seen divergent after a provider loss and is held for
    /// good (Paykit wire 5.6, Divergent floor): no automatic one-time payment goes, every
    /// one-time request is paid only by hand, and an unblock or a widening that needs the
    /// floor raised waits ([`crate::Refusal::FloorHeld`]), unless the shared post-loss hold
    /// stands too, when it goes without the raise. Raised when the hold is first kept
    /// and again at each load while it stands; it names no attempt, message or counterparty.
    BlockFloorHeld,
    /// A block floor raise this wallet owes was tried and isn't written, with no divergence
    /// hold: the floor doesn't read, or the switch refuses the write for now (Paykit wire 5.6,
    /// Raising). Until it is, an unblocked sender's one-time requests wait and it is asked
    /// nothing, and while a widening's raise waits no one is asked to re-send; it is tried again
    /// at each receive. Raised once per load while it stands, and again after a reopen when a
    /// receive leaves it unwritten; removed when the raise is written or the divergence hold
    /// stands, which its own notice tells. It names no attempt, message or counterparty.
    FloorRaiseWaiting,
    /// Automatic one-time payments for the identity are held for good after a provider
    /// loss, and each request can still be paid by hand (Paykit spec 8.1, Paykit wire 5.6,
    /// Shared post-loss hold). Raised once, and again after each reopen, while the hold
    /// stands; it names no attempt, message or counterparty.
    AutomaticPaymentsHeld,
}

impl NoticeKind {
    /// A notice about an own outgoing message that may have reached its recipient. It is
    /// kept at that message's key once acknowledged, so it is raised once.
    pub(crate) fn names_outgoing_message(self) -> bool {
        matches!(self, Self::PossiblyDelivered | Self::ListDeliveryUnknown)
    }

    /// The kind's name in a notice's key.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::PaidAfterFailed => "paid_after_failed",
            Self::HeldWithoutPay => "held_without_pay",
            Self::MessageGivenUp => "given_up",
            Self::AllowancePlacementExpired => "allowance_placement_expired",
            Self::AllowanceAcceptanceBeforeLoss => "allowance_acceptance_before_loss",
            Self::AllowanceWithdrawalConfirmed => "allowance_withdrawal_confirmed",
            Self::PossiblyDelivered => "possibly_delivered",
            Self::ListDeliveryUnknown => "list_delivery_unknown",
            Self::BlockedMessagesLost => "blocked_lost",
            Self::SettlementBucketFull => "settlement_bucket_full",
            Self::CreditNeedsUser => "credit_needs_user",
            Self::SettlementMarksChanged => "settlement_marks_changed",
            Self::SettlementCreditedTwice => "settlement_credited_twice",
            Self::DroppedMessagesLost => "dropped_lost",
            Self::BlockFloorHeld => "floor_held",
            Self::FloorRaiseWaiting => "raise_waiting",
            Self::AutomaticPaymentsHeld => "loss_hold",
        }
    }

    /// The identity-wide kind a key names (`nt/<tag>`): one about the identity as a whole,
    /// at most one kept, naming no attempt, message or counterparty.
    pub(crate) fn identity_from_tag(tag: &str) -> Option<Self> {
        [
            Self::BlockFloorHeld,
            Self::FloorRaiseWaiting,
            Self::AutomaticPaymentsHeld,
        ]
        .into_iter()
        .find(|kind| kind.tag() == tag)
    }

    /// The kind a key names.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "paid_after_failed" => Some(Self::PaidAfterFailed),
            "held_without_pay" => Some(Self::HeldWithoutPay),
            "given_up" => Some(Self::MessageGivenUp),
            "blocked_lost" => Some(Self::BlockedMessagesLost),
            "dropped_lost" => Some(Self::DroppedMessagesLost),
            _ => None,
        }
    }
}

/// What a payee's settlement warning concerns (Paykit wire 5.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SettlementSubject {
    /// The ten-bit bucket number.
    Bucket(u16),
    /// The current bucket write that lacked a retained mark.
    Changed {
        /// The bound ten-bit bucket number.
        bucket: u16,
        /// The own app that signed the current value.
        writer: [u8; 16],
        /// The physical application of the signed write.
        application: pubky_switch_core::shared::ApplicationRef,
    },
    /// Distinct retained occurrence marks for this opaque tag in this bucket.
    Conflicting {
        /// The bound ten-bit bucket number.
        bucket: u16,
        /// The tag carried by the bucket's marks.
        settlement: [u8; paykit_lib::derive::SETTLEMENT_TAG_LEN],
    },
    /// The complete blinded settlement tag for the payee identity.
    Tag([u8; 32]),
    /// The bound requested occurrence and signed proof when the method supplies no ID.
    RequestedProof {
        /// The payee's activity record key.
        record: [u8; 32],
        /// The signed proof's hash.
        proof: [u8; 32],
    },
}

impl SettlementSubject {
    pub(crate) fn path(self) -> String {
        match self {
            Self::Bucket(number) => format!("bucket/{number}"),
            Self::Changed {
                bucket,
                writer,
                application,
            } => format!(
                "changed/{bucket}/{}/{}/{}/{}",
                b64(&writer),
                application.generation,
                application.seq,
                b64(&application.item_hash)
            ),
            Self::Conflicting { bucket, settlement } => {
                format!("twice/{bucket}/{}", b64(&settlement))
            }
            Self::Tag(tag) => format!("tag/{}", b64(&tag)),
            Self::RequestedProof { record, proof } => {
                format!("proof/{}/{}", b64(&record), b64(&proof))
            }
        }
    }

    pub(crate) fn parse(path: &str) -> Option<Self> {
        let (family, data) = path.split_once('/')?;
        let subject = match family {
            "bucket" => {
                let number = data.parse().ok()?;
                if number >= paykit_lib::derive::SETTLEMENT_BUCKETS {
                    return None;
                }
                Self::Bucket(number)
            }
            "changed" | "twice" => {
                let (number, data) = data.split_once('/')?;
                let bucket = number.parse().ok()?;
                if bucket >= paykit_lib::derive::SETTLEMENT_BUCKETS {
                    return None;
                }
                if family == "twice" {
                    Self::Conflicting {
                        bucket,
                        settlement: unb64_array(data).ok()?,
                    }
                } else {
                    let mut parts = data.split('/');
                    let writer = unb64_array(parts.next()?).ok()?;
                    let generation = parts.next()?.parse().ok()?;
                    let seq = parts.next()?.parse().ok()?;
                    let item_hash = unb64_array(parts.next()?).ok()?;
                    if generation == 0 || seq == 0 || parts.next().is_some() {
                        return None;
                    }
                    Self::Changed {
                        bucket,
                        writer,
                        application: pubky_switch_core::shared::ApplicationRef {
                            generation,
                            seq,
                            item_hash,
                        },
                    }
                }
            }
            "tag" => Self::Tag(unb64_array(data).ok()?),
            "proof" => {
                let (record, proof) = data.split_once('/')?;
                Self::RequestedProof {
                    record: unb64_array(record).ok()?,
                    proof: unb64_array(proof).ok()?,
                }
            }
            _ => return None,
        };
        (subject.path() == path).then_some(subject)
    }

    fn kind(self) -> NoticeKind {
        match self {
            Self::Bucket(_) => NoticeKind::SettlementBucketFull,
            Self::Changed { .. } => NoticeKind::SettlementMarksChanged,
            Self::Conflicting { .. } => NoticeKind::SettlementCreditedTwice,
            Self::Tag(_) | Self::RequestedProof { .. } => NoticeKind::CreditNeedsUser,
        }
    }
}

/// A notice for the host to show, until it acknowledges it ([`Paykit::acknowledge_notice`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Notice {
    /// Its stable ID, which [`Paykit::acknowledge_notice`] takes.
    pub id: String,
    /// What it tells.
    pub kind: NoticeKind,
    /// The bucket or settlement of a payee warning, when applicable.
    pub settlement: Option<SettlementSubject>,
    /// The attempt it is about; `None` for a notice about a message.
    pub attempt_id: Option<AttemptId>,
    /// The message it is about, by its `msg_id`; `None` for a notice about an attempt.
    pub msg_id: Option<String>,
    /// The counterparty: the payee of an attempt, the message's sender, or, for an own
    /// answer given up, its payee. A possibly delivered message names its recipient. A settlement
    /// warning names the payer whose proof first raised it.
    /// A shared bucket observation identifies no payer.
    pub counterparty: Option<[u8; 32]>,
    /// The conflicting marks when one settlement names more than one occurrence.
    pub settlement_marks: Vec<paykit_lib::records::SettlementMark>,
    /// The request paid or named, `None` for none.
    pub request_hash: Option<[u8; 32]>,
    /// The signed JSON hash of a possibly delivered outgoing message.
    pub message_hash: Option<[u8; 32]>,
    /// When it was raised, in Unix milliseconds: the highest service time the app had
    /// verified then, 0 before any.
    pub raised_at_ms: u64,
    /// For [`NoticeKind::BlockedMessagesLost`] or [`NoticeKind::DroppedMessagesLost`]: the earliest acceptance time of what can no
    /// longer come back.
    pub lost_from_ms: Option<u64>,
    /// For [`NoticeKind::BlockedMessagesLost`] or [`NoticeKind::DroppedMessagesLost`]: the latest such time.
    pub lost_until_ms: Option<u64>,
}

/// A notice as kept (`p/nt/<attempt_id>/<kind>`, `p/nt/<sender>/<msg_id>` for a message
/// given up, or `p/nt/<counterparty>/lost-<from_ms>` for a window of messages a block
/// dropped that can no longer come back, `from_ms` its earliest acceptance time, zero-padded
/// to 20 digits, or `p/nt/<tag>` for an identity-wide notice): what its key names repeats in
/// it, so a restore binds the value to its key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptNotice {
    pub(crate) kind: NoticeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settlement: Option<String>,
    pub(crate) attempt_id: Option<String>,
    pub(crate) msg_id: Option<String>,
    pub(crate) counterparty: Option<String>,
    pub(crate) request_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message_hash: Option<String>,
    pub(crate) raised_at_ms: u64,
    pub(crate) acknowledged: bool,
    /// For a message given up: the entry the client keeps stored for it, which the
    /// notice's acknowledgement forgets ([`Paykit::acknowledge_notice`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stored: Option<StoredHandle>,
    /// For a message given up with no entry stored, where it came from; absent for one with
    /// its entry's handle (`stored`), as every notice the stored entries raise is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<GivenUpOrigin>,
    /// For messages a blocked counterparty sent that can no longer come back: their window
    /// of acceptance times, earliest and latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lost: Option<(u64, u64)>,
}

/// Where a message given up with no entry stored came from: no entry, so no handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GivenUpOrigin {
    /// The client dropped its entry, stored while it waited for a scope key, past the
    /// bounds of Switch spec 8.2: it was never processed, so a copy its sender sends again is
    /// taken as a new message.
    KeyWait,
    /// A payer's reply kept for its record, or a receipt kept for a payer record that
    /// didn't read (`pr/`), was given up.
    KeptReply,
    /// A message kept to write over a divergent record (`dw/`) was given up, its record's
    /// user not having confirmed it in time.
    Divergent,
    /// The receive step gave up an Allowance proposal or answer over a record value that
    /// doesn't read, past its window (`allowances.rs`): it was processed.
    ReceiveStep,
    /// Placement expired with a separate durable Allowance conflict hold.
    AllowanceConflict,
    /// The original proposing app cannot open a budget from this pre-loss proposal.
    AllowanceBeforeLoss,
    /// This app's own rejection or payer's cancellation, recorded while sends to the payee
    /// were held (`ha/`) or held in the outbox (`qs/`), passed its replay window unsent, or
    /// was lost with a write of its request record that holds neither it nor any stop.
    HeldAnswer,
    /// A received message whose entry was held unprocessed while the record it goes into
    /// couldn't be read, past its 30 days.
    HeldUnread,
}

impl GivenUpOrigin {
    fn notice_kind(origin: Option<Self>) -> NoticeKind {
        match origin {
            Some(Self::AllowanceConflict) => NoticeKind::AllowancePlacementExpired,
            Some(Self::AllowanceBeforeLoss) => NoticeKind::AllowanceAcceptanceBeforeLoss,
            _ => NoticeKind::MessageGivenUp,
        }
    }
}

/// What a message given up leaves behind ([`KeptNotice::given_up`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GivenUpFrom {
    /// An entry the client keeps stored, which the acknowledgement forgets.
    Stored(StoredHandle),
    /// No entry: the message came from there.
    Origin(GivenUpOrigin),
}

/// The handle of an entry the client keeps stored (Switch spec 8.2): its local ID, and the
/// message hash [`pubky_switch_client::app::App::forget_parked`] checks it by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredHandle {
    pub(crate) stored_id: u64,
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
}

impl KeptNotice {
    /// The identity-wide notice of `kind`, raised at `at`.
    pub(crate) fn identity(kind: NoticeKind, at: u64) -> Self {
        Self {
            kind,
            settlement: None,
            counterparty: None,
            raised_at_ms: at,
            acknowledged: false,
            attempt_id: None,
            msg_id: None,
            message_hash: None,
            request_hash: None,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    pub(crate) fn settlement(subject: SettlementSubject, payer: Option<[u8; 32]>, at: u64) -> Self {
        Self {
            kind: subject.kind(),
            settlement: Some(subject.path()),
            counterparty: payer.map(|peer| b64(&peer)),
            raised_at_ms: at,
            acknowledged: false,
            attempt_id: None,
            msg_id: None,
            message_hash: None,
            request_hash: None,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    /// A notice of `kind` about the payment `entry` journals, raised at `at`.
    pub(crate) fn of(kind: NoticeKind, entry: &JournalEntry, at: u64) -> Self {
        Self {
            kind,
            settlement: None,
            attempt_id: Some(entry.attempt_id.clone()),
            msg_id: None,
            counterparty: Some(b64(&entry.payee)),
            request_hash: entry.request_hash.clone(),
            message_hash: None,
            raised_at_ms: at,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    /// The notice "marked failed, was paid" of `attempt`, paid to `payee` for the request
    /// `request_hash`, if any, raised at `at`.
    pub(crate) fn late(
        attempt: AttemptId,
        payee: [u8; 32],
        request_hash: Option<&[u8; 32]>,
        at: u64,
    ) -> Self {
        Self {
            kind: NoticeKind::PaidAfterFailed,
            settlement: None,
            attempt_id: Some(attempt.to_text()),
            msg_id: None,
            counterparty: Some(b64(&payee)),
            request_hash: request_hash.map(|hash| pubky_switch_core::enc::b64(hash)),
            message_hash: None,
            raised_at_ms: at,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    /// The notice that message `msg_id` from `sender` was given up, raised at `at`: with its
    /// entry's handle when the client keeps one stored, or else with where it came from.
    pub(crate) fn given_up(
        sender: [u8; 32],
        msg_id: &str,
        request_hash: Option<[u8; 32]>,
        at: u64,
        from: GivenUpFrom,
    ) -> Self {
        let (stored, origin) = match from {
            GivenUpFrom::Stored(handle) => (Some(handle), None),
            GivenUpFrom::Origin(origin) => (None, Some(origin)),
        };
        Self {
            kind: GivenUpOrigin::notice_kind(origin),
            settlement: None,
            attempt_id: None,
            msg_id: Some(msg_id.to_owned()),
            counterparty: Some(b64(&sender)),
            request_hash: request_hash.map(|hash| pubky_switch_core::enc::b64(&hash)),
            message_hash: None,
            raised_at_ms: at,
            acknowledged: false,
            stored,
            origin,
            lost: None,
        }
    }

    pub(crate) fn withdrawal_confirmed(end: &paykit_lib::records::StoredMessage) -> Result<Self> {
        Ok(Self {
            kind: NoticeKind::AllowanceWithdrawalConfirmed,
            msg_id: Some(end.message().msg_id.clone()),
            counterparty: Some(b64(&end.sender())),
            raised_at_ms: end.accepted_at().ok_or_else(PaykitError::corrupt)?,
            settlement: None,
            attempt_id: None,
            request_hash: None,
            message_hash: None,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        })
    }

    pub(crate) fn list_delivery_unknown(
        to: &[u8; 32],
        msg_id: &str,
        hash: &[u8; 32],
        at: u64,
    ) -> Self {
        Self::outgoing(NoticeKind::ListDeliveryUnknown, to, msg_id, hash, at)
    }

    /// The notice that the own message `msg_id` of hash `hash` to `to`, whose delivery ended,
    /// may have reached it, raised at `at`.
    pub(crate) fn delivery_unknown(to: &[u8; 32], msg_id: &str, hash: &[u8; 32], at: u64) -> Self {
        Self::outgoing(NoticeKind::PossiblyDelivered, to, msg_id, hash, at)
    }

    fn possibly_delivered(stored: &paykit_lib::records::StoredMessage, at: u64) -> Self {
        let message = stored.message();
        Self::outgoing(
            NoticeKind::PossiblyDelivered,
            &message.to,
            &message.msg_id,
            &stored.hash(),
            at,
        )
    }

    /// A notice about an own outgoing message, kept at that message's key.
    fn outgoing(kind: NoticeKind, to: &[u8; 32], msg_id: &str, hash: &[u8; 32], at: u64) -> Self {
        Self {
            kind,
            settlement: None,
            attempt_id: None,
            msg_id: Some(msg_id.to_owned()),
            counterparty: Some(b64(to)),
            request_hash: None,
            message_hash: Some(pubky_switch_core::enc::b64(hash)),
            raised_at_ms: at,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    /// Whether the message given up stays processed, so a copy sent again has no effect while
    /// the notice is kept: every give-up but the client's drop of an entry waiting for a
    /// key, which was never processed ([`GivenUpOrigin::KeyWait`]).
    pub(crate) fn keeps_processed(&self) -> bool {
        self.origin != Some(GivenUpOrigin::KeyWait)
    }

    /// The notice read whole, at the attempt and kind its key names: one that doesn't is a
    /// corrupt store.
    pub(crate) fn check(&self, attempt: &AttemptId, kind: NoticeKind) -> Result<()> {
        if self.kind != kind
            || matches!(
                kind,
                NoticeKind::MessageGivenUp
                    | NoticeKind::AllowancePlacementExpired
                    | NoticeKind::AllowanceAcceptanceBeforeLoss
                    | NoticeKind::AllowanceWithdrawalConfirmed
                    | NoticeKind::PossiblyDelivered
                    | NoticeKind::ListDeliveryUnknown
                    | NoticeKind::BlockedMessagesLost
                    | NoticeKind::DroppedMessagesLost
            )
            || self.attempt_id.as_deref() != Some(attempt.to_text().as_str())
            || self.msg_id.is_some()
            || self.stored.is_some()
            || self.origin.is_some()
            || self.lost.is_some()
        {
            return Err(PaykitError::corrupt());
        }
        self.check_hash()
    }

    /// The notice that what `counterparty`'s inbox accepted from `from` to `until`, dropped
    /// for the user's block, can no longer come back, raised at `at`.
    pub(crate) fn blocked_lost(counterparty: [u8; 32], from: u64, until: u64, at: u64) -> Self {
        Self {
            kind: NoticeKind::BlockedMessagesLost,
            settlement: None,
            attempt_id: None,
            msg_id: None,
            counterparty: Some(b64(&counterparty)),
            request_hash: None,
            message_hash: None,
            raised_at_ms: at,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: Some((from, until.max(from))),
        }
    }

    /// The notice of lost messages read whole, at the counterparty and start its key names:
    /// one that doesn't is a corrupt store. It is deleted at its acknowledgement.
    pub(crate) fn check_lost(&self, counterparty: &[u8; 32], from: u64) -> Result<()> {
        self.check_lost_kind(counterparty, from, NoticeKind::BlockedMessagesLost)
    }

    pub(crate) fn dropped_lost(counterparty: [u8; 32], from: u64, until: u64, at: u64) -> Self {
        let mut notice = Self::blocked_lost(counterparty, from, until, at);
        notice.kind = NoticeKind::DroppedMessagesLost;
        notice
    }

    fn check_lost_kind(&self, counterparty: &[u8; 32], from: u64, kind: NoticeKind) -> Result<()> {
        let window_read = self
            .lost
            .is_some_and(|(start, end)| start == from && end >= start);
        if self.kind != kind
            || self.attempt_id.is_some()
            || self.msg_id.is_some()
            || self.request_hash.is_some()
            || self.counterparty.as_deref() != Some(&b64(counterparty))
            || self.stored.is_some()
            || self.origin.is_some()
            || !window_read
            || self.acknowledged
        {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }

    /// The notice of a message given up read whole, at the sender and `msg_id` its key
    /// names: one that doesn't is a corrupt store.
    pub(crate) fn check_message(&self, sender: &[u8; 32], msg_id: &str) -> Result<()> {
        // A message notice is deleted at its acknowledgement, after its entry is forgotten,
        // so a valid run keeps none acknowledged. It holds its entry's handle exactly when it
        // names no origin: one raised for a stored entry always has it, and one with no
        // entry stored (a key wait's, a kept reply's) names where it came from instead.
        if self.kind == NoticeKind::AllowanceWithdrawalConfirmed {
            if self.msg_id.as_deref() != Some(msg_id)
                || self.counterparty.as_deref() != Some(&b64(sender))
                || self.message_hash.is_some()
                || self.acknowledged
                || self.attempt_id.is_some()
                || self.stored.is_some()
                || self.origin.is_some()
                || self.lost.is_some()
                || self.request_hash.is_some()
                || self.settlement.is_some()
            {
                return Err(PaykitError::corrupt());
            }
            return Ok(());
        }
        let expected = GivenUpOrigin::notice_kind(self.origin);
        if self.kind != expected
            || self.attempt_id.is_some()
            || self.msg_id.as_deref() != Some(msg_id)
            || self.counterparty.as_deref() != Some(&b64(sender))
            || self.stored.is_some() == self.origin.is_some()
            || self.lost.is_some()
            || self.acknowledged
        {
            return Err(PaykitError::corrupt());
        }
        self.check_hash()
    }

    fn check_hash(&self) -> Result<()> {
        self.request_hash
            .as_deref()
            .map(unb64_array::<32>)
            .transpose()
            .map(|_| ())
            .map_err(|_| PaykitError::corrupt())
    }

    fn view(&self, id: String) -> Result<Notice> {
        let request_hash = self
            .request_hash
            .as_deref()
            .map(unb64_array::<32>)
            .transpose()
            .map_err(|_| PaykitError::corrupt())?;
        Ok(Notice {
            id,
            kind: self.kind,
            settlement: self
                .settlement
                .as_deref()
                .and_then(SettlementSubject::parse),
            attempt_id: self
                .attempt_id
                .as_deref()
                .map(AttemptId::parse)
                .transpose()
                .map_err(|_| PaykitError::corrupt())?,
            msg_id: self.msg_id.clone(),
            counterparty: self
                .counterparty
                .as_deref()
                .map(unb64_array)
                .transpose()
                .map_err(|_| PaykitError::corrupt())?,
            settlement_marks: Vec::new(),
            request_hash,
            message_hash: self
                .message_hash
                .as_deref()
                .map(unb64_array::<32>)
                .transpose()
                .map_err(|_| PaykitError::corrupt())?,
            raised_at_ms: self.raised_at_ms,
            lost_from_ms: self.lost.map(|(from, _)| from),
            lost_until_ms: self.lost.map(|(_, until)| until),
        })
    }
}

/// Bind `notice`, read at `key` (`nt/...`), to what the key names, as a restore checks it:
/// a value at another notice's key, or under a key that names no notice, is a corrupt store.
pub(crate) fn bind_notice(key: &str, notice: &KeptNotice) -> Result<()> {
    let family = StateKey::parse(key.as_bytes());
    let peer = notice
        .counterparty
        .as_deref()
        .map(unb64_array::<32>)
        .transpose()
        .map_err(|_| PaykitError::corrupt())?;
    let peer_allowed = match family {
        Some(StateKey::SettlementNotice(
            SettlementSubject::Changed { .. } | SettlementSubject::Conflicting { .. },
        )) => peer.is_none(),
        Some(StateKey::SettlementNotice(SettlementSubject::Bucket(_))) => true,
        Some(StateKey::IdentityNotice(_)) => peer.is_none(),
        _ => peer.is_some(),
    };
    if !peer_allowed {
        return Err(PaykitError::corrupt());
    }
    match family {
        Some(StateKey::SettlementNotice(subject)) => {
            if notice.kind == subject.kind()
                && notice.settlement.as_deref() == Some(&subject.path())
                && notice.attempt_id.is_none()
                && notice.msg_id.is_none()
                && notice.message_hash.is_none()
                && notice.stored.is_none()
                && notice.origin.is_none()
                && notice.lost.is_none()
                && notice.request_hash.is_none()
            {
                Ok(())
            } else {
                Err(PaykitError::corrupt())
            }
        }
        _ if notice.settlement.is_some() => Err(PaykitError::corrupt()),
        Some(StateKey::IdentityNotice(kind)) => {
            if *notice == KeptNotice::identity(kind, notice.raised_at_ms) {
                Ok(())
            } else {
                Err(PaykitError::corrupt())
            }
        }
        Some(StateKey::RestoredDelivery { to, msg_id, hash }) => {
            if !notice.kind.names_outgoing_message()
                || notice.counterparty.as_deref() != Some(&b64(&to))
                || notice.msg_id.as_ref() != Some(&msg_id)
                || notice.message_hash.as_deref() != Some(&pubky_switch_core::enc::b64(&hash))
                || notice.attempt_id.is_some()
                || notice.request_hash.is_some()
                || notice.stored.is_some()
                || notice.origin.is_some()
                || notice.lost.is_some()
            {
                return Err(PaykitError::corrupt());
            }
            Ok(())
        }
        _ if notice.message_hash.is_some() => Err(PaykitError::corrupt()),
        Some(StateKey::Notice { attempt, kind }) => notice.check(&attempt, kind),
        Some(StateKey::MessageNotice { sender, msg_id }) => notice.check_message(&sender, &msg_id),
        Some(StateKey::BlockedLost {
            counterparty,
            from_ms,
        }) => notice.check_lost(&counterparty, from_ms),
        Some(StateKey::DroppedLost {
            counterparty,
            from_ms,
            ..
        }) => notice.check_lost_kind(&counterparty, from_ms, NoticeKind::DroppedMessagesLost),
        _ => Err(PaykitError::corrupt()),
    }
}

/// The ID of the notice of `kind` about `attempt`: its key after the family's prefix.
pub(crate) fn notice_id(attempt: &AttemptId, kind: NoticeKind) -> String {
    format!("{}/{}", attempt.to_text(), kind.tag())
}

impl Paykit {
    pub(crate) async fn raise_settlement_notice(
        &mut self,
        subject: SettlementSubject,
        payer: [u8; 32],
    ) -> Result<()> {
        let path = subject.path();
        let key = format!("{}{path}", key::SETTLEMENT_NOTICES);
        if self.kept_notice(&key).await?.is_none() {
            let notice =
                KeptNotice::settlement(subject, Some(payer), self.service_time().unwrap_or(0));
            self.local_put(&key, &notice).await?;
        }
        Ok(())
    }

    /// An authenticated original from a restored record has no known delivery disposition.
    pub(crate) async fn note_restored_delivery(
        &mut self,
        stored: &paykit_lib::records::StoredMessage,
    ) -> Result<()> {
        if stored.sender() != self.identity() || stored.cert().app_id() != self.app_id() {
            return Err(PaykitError::corrupt());
        }
        let message = stored.message();
        // A live outbox, an accepted delivery or a terminal client result already has a
        // host-visible disposition (Switch spec 11). Preserve it.
        if self.app.delivery(&message.msg_id).await?.is_some() {
            return Ok(());
        }
        // So does a held send this installation gave up and told the host of: an in-place
        // restore keeps the notice it was told with, given up or possibly delivered (Paykit
        // spec 8.1, "Undelivered messages").
        if self
            .local_get::<crate::send_holds::QueuedStop>(&key::queued_stop(&message.msg_id))
            .await?
            .is_some_and(|stop| stop.told)
        {
            return Ok(());
        }
        let key = key::restored_delivery(&message.to, &message.msg_id, &stored.hash());
        if self.kept_notice(&key).await?.is_none() {
            self.local_put(
                &key,
                &KeptNotice::possibly_delivered(stored, self.service_time().unwrap_or(0)),
            )
            .await?;
        }
        Ok(())
    }

    /// The notices the host hasn't acknowledged, oldest first.
    ///
    /// # Errors
    ///
    /// A store failure, or a kept notice that doesn't read.
    pub async fn notices(&self) -> Result<Vec<Notice>> {
        let mut out = Vec::new();
        // Each read whole and bound to its key ([`bind_notice`]).
        for (id, kept) in self.local_scan::<KeptNotice>(key::NOTICES).await? {
            if !kept.acknowledged {
                let mut notice = kept.view(id)?;
                if let Some(SettlementSubject::Conflicting { bucket, settlement }) =
                    notice.settlement
                {
                    let key = self
                        .app
                        .record_key(&crate::settlement_marks::target(bucket))?;
                    notice.settlement_marks = self
                        .seen_settlement_marks(bucket, &key)
                        .await?
                        .marks()
                        .iter()
                        .copied()
                        .filter(|m| m.settlement == settlement)
                        .collect();
                }
                out.push(notice);
            }
        }
        out.sort_by_key(|notice| notice.raised_at_ms);
        Ok(out)
    }

    /// Acknowledge `id` once the user has seen the notice. The wallet stops listing it.
    /// A payment notice keeps its acknowledgment while the journal holds the payment.
    /// For a possibly delivered message, the acknowledgment survives repeated imports
    /// into this store and never triggers delivery. A notice of a list whose delivery ended
    /// unknown goes once that list's outcome is settled; a backup taken before then can
    /// bring it back, unacknowledged, until the next receive.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for an ID no notice has; a store failure.
    pub async fn acknowledge_notice(&mut self, id: &str) -> Result<()> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        let key = format!("{}{id}", key::NOTICES);
        if !matches!(
            StateKey::parse(key.as_bytes()),
            Some(
                StateKey::Notice { .. }
                    | StateKey::MessageNotice { .. }
                    | StateKey::RestoredDelivery { .. }
                    | StateKey::BlockedLost { .. }
                    | StateKey::SettlementNotice(_)
                    | StateKey::DroppedLost { .. }
                    | StateKey::IdentityNotice(_)
            )
        ) {
            return Err(Refusal::NotFound.into());
        }
        let mut kept = self.kept_notice(&key).await?.ok_or(Refusal::NotFound)?;
        if kept.acknowledged {
            return Ok(());
        }
        // Kept acknowledged while the journal holds its payment, which could raise it again;
        // dropped with it, or now when it is gone (`Paykit::prune_journal`). A message given
        // up is never raised again: its notice goes now.
        let attempt = kept
            .attempt_id
            .as_deref()
            .map(AttemptId::parse)
            .transpose()
            .map_err(|_| PaykitError::corrupt())?;
        let held = match attempt {
            Some(attempt) => self.journal_entry(&attempt).await?.is_some(),
            None => false,
        };
        // The entry a message given up left stored is forgotten first: a failure keeps the
        // notice, for the host to acknowledge again.
        if let Some(stored) = &kept.stored {
            self.forget_stored(stored, &kept).await?;
        }
        // An identity-wide notice acknowledged stays told for this load, as one shown is.
        if let Some(StateKey::IdentityNotice(kind)) = StateKey::parse(key.as_bytes()) {
            if !self.identity_told.contains(&kind) {
                self.identity_told.push(kind);
            }
        }
        if !held && kept.settlement.is_none() && !kept.kind.names_outgoing_message() {
            return self.local_delete(&key).await;
        }
        kept.acknowledged = true;
        self.local_put(&key, &kept).await
    }

    /// Tell the host of the identity-wide notice of `kind`, once per load: a notice already
    /// shown and not yet acknowledged is left as it is.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub(crate) async fn tell_identity_notice(&mut self, kind: NoticeKind) -> Result<()> {
        if self.identity_told.contains(&kind) {
            return Ok(());
        }
        let key = key::identity_notice(kind);
        if self.kept_notice(&key).await?.is_none() {
            let at = self.service_time().unwrap_or(0);
            self.local_put(&key, &KeptNotice::identity(kind, at))
                .await?;
        }
        self.identity_told.push(kind);
        Ok(())
    }

    /// Tell the host, at a load, a restore or a receive's end, of every identity-wide hold
    /// that stands: the block floor held after it was seen divergent
    /// ([`NoticeKind::BlockFloorHeld`]) and the shared post-loss hold
    /// ([`NoticeKind::AutomaticPaymentsHeld`], `crate::loss`).
    ///
    /// # Errors
    ///
    /// A store failure, or a kept floor that doesn't read.
    pub(crate) async fn tell_identity_notices(&mut self) -> Result<()> {
        if self.floor_kept().await?.divergent {
            self.tell_identity_notice(NoticeKind::BlockFloorHeld)
                .await?;
        }
        if self.shared_loss_hold().await? {
            self.tell_identity_notice(NoticeKind::AutomaticPaymentsHeld)
                .await?;
        }
        Ok(())
    }

    /// The notice kept at `key`, read whole and bound to it ([`bind_notice`]).
    ///
    /// # Errors
    ///
    /// A store failure, or a value that doesn't read or isn't the notice its key names.
    pub(crate) async fn kept_notice(&self, key: &str) -> Result<Option<KeptNotice>> {
        self.local_get::<KeptNotice>(key).await
    }

    /// Forget the entry `stored` names, if the client still keeps it: by its local ID and
    /// hash, or, after a restore onto another store, which tells of its entries again, by
    /// its hash. One gone already, such as one the host forgot itself, is not looked for.
    /// The entry found is bound to `notice`: one holding another message than the sender's
    /// `msg_id` the notice names is a corrupt store, and nothing is forgotten.
    async fn forget_stored(&mut self, stored: &StoredHandle, notice: &KeptNotice) -> Result<()> {
        let entries = self.app.parked_entries().await?;
        let entry = entries
            .iter()
            .find(|e| e.stored_id == stored.stored_id && e.hash == stored.hash)
            .or_else(|| entries.iter().find(|e| e.hash == stored.hash));
        let Some(entry) = entry.cloned() else {
            return Ok(());
        };
        let message = self.app.parked_entry(entry.stored_id).await?;
        let names = message.as_ref().is_some_and(|parked| {
            notice.counterparty.as_deref() == Some(&b64(&parked.sender))
                && parked.message.as_ref().map(|m| m.msg_id.as_str()) == notice.msg_id.as_deref()
        });
        if !names {
            return Err(PaykitError::corrupt());
        }
        self.app.forget_parked(entry.stored_id, &entry.hash).await?;
        Ok(())
    }

    /// Raise the notice of `kind` about the payment `entry` journals, unless it is kept
    /// already.
    pub(crate) async fn raise_notice(
        &mut self,
        kind: NoticeKind,
        entry: &JournalEntry,
    ) -> Result<()> {
        let key = key::notice(&entry.attempt()?, kind);
        if self.kept_notice(&key).await?.is_some() {
            return Ok(());
        }
        let at = self.service_time().unwrap_or(0);
        self.local_put(&key, &KeptNotice::of(kind, entry, at)).await
    }

    /// Drop the notice of `kind` about `entry`'s payment, acknowledged or not, once what it
    /// says no longer holds: raised again, it is listed again.
    pub(crate) async fn drop_notice(
        &mut self,
        kind: NoticeKind,
        entry: &JournalEntry,
    ) -> Result<()> {
        let key = key::notice(&entry.attempt()?, kind);
        if self.kept_notice(&key).await?.is_some() {
            self.local_delete(&key).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(attempt: &AttemptId) -> KeptNotice {
        KeptNotice {
            kind: NoticeKind::PaidAfterFailed,
            settlement: None,
            attempt_id: Some(attempt.to_text()),
            msg_id: None,
            counterparty: Some(b64(&[1; 32])),
            request_hash: Some(pubky_switch_core::enc::b64(&[2; 32])),
            message_hash: None,
            raised_at_ms: 3,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        }
    }

    #[test]
    fn test_identity_notices_bind_only_at_their_key_and_name_nothing() {
        for kind in [
            NoticeKind::BlockFloorHeld,
            NoticeKind::FloorRaiseWaiting,
            NoticeKind::AutomaticPaymentsHeld,
        ] {
            let key = key::identity_notice(kind);
            let notice = KeptNotice::identity(kind, 5);
            assert!(bind_notice(&key, &notice).is_ok(), "{kind:?}");
            // Never kept acknowledged: acknowledging removes it.
            let mut acknowledged = notice.clone();
            acknowledged.acknowledged = true;
            assert!(bind_notice(&key, &acknowledged).is_err());
            let mut named = notice.clone();
            named.counterparty = Some(b64(&[1; 32]));
            assert!(bind_notice(&key, &named).is_err());
            let mut other = notice.clone();
            other.kind = NoticeKind::MessageGivenUp;
            assert!(bind_notice(&key, &other).is_err());
            // Nor at another notice's key.
            let attempt = AttemptId::from_bytes([5; 16]);
            assert!(
                bind_notice(&key::notice(&attempt, NoticeKind::HeldWithoutPay), &notice).is_err()
            );
        }
    }

    #[test]
    fn test_settlement_notices_bind_the_complete_subject() {
        let subjects = [
            SettlementSubject::Bucket(1023),
            SettlementSubject::Tag([4; 32]),
            SettlementSubject::RequestedProof {
                record: [6; 32],
                proof: [7; 32],
            },
        ];
        for subject in subjects {
            let path = subject.path();
            let key = format!("{}{path}", key::SETTLEMENT_NOTICES);
            assert_eq!(SettlementSubject::parse(&path), Some(subject));
            let mut notice = KeptNotice {
                kind: subject.kind(),
                settlement: Some(path),
                message_hash: None,
                attempt_id: None,
                msg_id: None,
                counterparty: Some(b64(&[9; 32])),
                request_hash: None,
                raised_at_ms: 84,
                acknowledged: false,
                stored: None,
                origin: None,
                lost: None,
            };
            for acknowledged in [false, true] {
                notice.acknowledged = acknowledged;
                assert!(bind_notice(&key, &notice).is_ok());
                assert_eq!(notice.view(key.clone()).unwrap().settlement, Some(subject));
                assert!(bind_notice("nt/settlement/bucket/2", &notice).is_err());
            }
        }
        for (subject, with_peer, without_peer) in [
            (SettlementSubject::Bucket(3), true, true),
            (
                SettlementSubject::Changed {
                    bucket: 3,
                    writer: [2; 16],
                    application: pubky_switch_core::shared::ApplicationRef {
                        generation: 2,
                        seq: 3,
                        item_hash: [7; 32],
                    },
                },
                false,
                true,
            ),
            (
                SettlementSubject::Conflicting {
                    bucket: 3,
                    settlement: [4; paykit_lib::derive::SETTLEMENT_TAG_LEN],
                },
                false,
                true,
            ),
            (SettlementSubject::Tag([4; 32]), true, false),
            (
                SettlementSubject::RequestedProof {
                    record: [3; 32],
                    proof: [7; 32],
                },
                true,
                false,
            ),
        ] {
            let key = format!("{}{}", key::SETTLEMENT_NOTICES, subject.path());
            for (peer, valid) in [(Some([9; 32]), with_peer), (None, without_peer)] {
                let notice = KeptNotice::settlement(subject, peer, 84);
                assert_eq!(
                    bind_notice(&key, &notice).is_ok(),
                    valid,
                    "{subject:?} {peer:?}"
                );
            }
        }
        for bad in ["bucket/1024", "bucket/00", "tag/2", "proof/1", "proof/2/3"] {
            assert!(SettlementSubject::parse(bad).is_none());
        }
    }

    #[test]
    fn test_restored_delivery_notice_binds_the_recipient_id_and_message_hash() {
        let to = [2; 32];
        let hash = [3; 32];
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let mut notice = KeptNotice {
            kind: NoticeKind::PossiblyDelivered,
            settlement: None,
            attempt_id: None,
            msg_id: Some(id.into()),
            counterparty: Some(b64(&to)),
            request_hash: None,
            message_hash: Some(pubky_switch_core::enc::b64(&hash)),
            raised_at_ms: 3,
            acknowledged: false,
            stored: None,
            origin: None,
            lost: None,
        };
        let key = key::restored_delivery(&to, id, &hash);
        assert!(bind_notice(&key, &notice).is_ok());
        notice.acknowledged = true;
        assert!(bind_notice(&key, &notice).is_ok());
        for different in [
            key::restored_delivery(&[4; 32], id, &hash),
            key::restored_delivery(&to, id, &[4; 32]),
            key::restored_delivery(&to, "223e4567-e89b-42d3-a456-426614174000", &hash),
            key::message_notice(&to, id),
        ] {
            assert!(bind_notice(&different, &notice).is_err());
        }
        notice.kind = NoticeKind::MessageGivenUp;
        assert!(bind_notice(&key, &notice).is_err());
        let withdrawal = KeptNotice::list_delivery_unknown(&to, id, &hash, 3);
        assert!(bind_notice(&key, &withdrawal).is_ok());
        assert!(bind_notice(&key::restored_delivery(&to, id, &[4; 32]), &withdrawal).is_err());
        notice.kind = NoticeKind::PossiblyDelivered;
        notice.message_hash = None;
        assert!(bind_notice(&key, &notice).is_err());
    }

    #[test]
    fn test_notices_only_outgoing_message_notices_carry_a_message_hash() {
        let attempt = AttemptId::from_bytes([5; 16]);
        let paid = kept(&attempt);
        let held = KeptNotice {
            kind: NoticeKind::HeldWithoutPay,
            ..paid.clone()
        };
        let peer = [1; 32];
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let given_up = KeptNotice::given_up(
            peer,
            id,
            None,
            3,
            GivenUpFrom::Origin(GivenUpOrigin::KeyWait),
        );
        for (key, mut notice) in [
            (key::notice(&attempt, paid.kind), paid),
            (key::notice(&attempt, held.kind), held),
            (key::message_notice(&peer, id), given_up),
            (
                key::blocked_lost(&peer, 3),
                KeptNotice::blocked_lost(peer, 3, 4, 5),
            ),
        ] {
            assert!(bind_notice(&key, &notice).is_ok());
            notice.message_hash = Some(pubky_switch_core::enc::b64(&[7; 32]));
            assert!(bind_notice(&key, &notice).is_err());
        }
    }

    #[test]
    fn test_notices_kept_notice_bound_to_its_key() {
        let attempt = AttemptId::from_bytes([5; 16]);
        let notice = kept(&attempt);
        assert!(notice.check(&attempt, NoticeKind::PaidAfterFailed).is_ok());
        let view = notice.view(notice_id(&attempt, notice.kind)).unwrap();
        assert_eq!(view.request_hash, Some([2; 32]));
        assert_eq!(
            StateKey::parse(key::notice(&attempt, notice.kind).as_bytes()),
            Some(StateKey::Notice {
                attempt,
                kind: NoticeKind::PaidAfterFailed
            })
        );
    }

    #[test]
    fn test_notices_kept_notice_at_another_key_refused() {
        let attempt = AttemptId::from_bytes([5; 16]);
        let notice = kept(&attempt);
        assert!(notice.check(&attempt, NoticeKind::HeldWithoutPay).is_err());
        assert!(notice
            .check(&AttemptId::from_bytes([6; 16]), NoticeKind::PaidAfterFailed)
            .is_err());
        let bad = KeptNotice {
            request_hash: Some("not-a-hash".into()),
            ..notice
        };
        assert!(bad.check(&attempt, NoticeKind::PaidAfterFailed).is_err());
    }

    #[test]
    fn test_notices_message_notice_bound_to_its_sender_and_message() {
        let id = "123e4567-e89b-42d3-a456-426614174000";
        let handle = StoredHandle {
            stored_id: 4,
            hash: [5; 32],
        };
        let notice =
            KeptNotice::given_up([1; 32], id, Some([2; 32]), 3, GivenUpFrom::Stored(handle));
        assert!(notice.check_message(&[1; 32], id).is_ok());
        assert!(notice.check_message(&[9; 32], id).is_err());
        assert!(notice
            .check_message(&[1; 32], "223e4567-e89b-42d3-a456-426614174000")
            .is_err());
        assert!(notice
            .check(&AttemptId::from_bytes([5; 16]), NoticeKind::MessageGivenUp)
            .is_err());
    }
}
