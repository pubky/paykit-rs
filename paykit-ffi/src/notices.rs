use crate::{NativeWallet, WalletError};
use std::sync::Arc;

/// What a notice tells the user.
#[derive(uniffi::Enum)]
pub enum WalletNoticeKind {
    /// "Marked failed, was paid": the payee's receipt confirms a payment whose attempt was
    /// written `failed`, and it arrived once the claim was gone, 30 days after its deadline, so
    /// it can't be recorded on the claim (Paykit spec 7.2, "What a payment method declares").
    PaidAfterFailed,
    /// The payment is held: the app's certificate no longer holds `paykit_pay`, so the app reads
    /// and writes no claim and no payer record, and the attempt waits, open, for an app holding
    /// `paykit_pay` (Paykit spec 3.1, 7.3).
    HeldWithoutPay,
    /// A message whose effect the Dispatch refused for good, kept to write into its record later,
    /// wasn't written within `KEPT_MESSAGE_MS` of its acceptance: the SDK gave it up (Dispatch spec
    /// 8.2).
    MessageGivenUp,
    /// Ordinary placement expired, but the wallet retained a hold on this Allowance because the
    /// signed message could still establish a conflict.
    AllowancePlacementExpired,
    /// The seller accepted this payer's proposal from before provider loss. The acceptance
    /// cannot open a budget. Withdrawal requires the verified live original proposal.
    AllowanceAcceptanceBeforeLoss,
    /// The seller confirmed the end of this wallet's withdrawn pre-loss proposal.
    AllowanceWithdrawalConfirmed,
    /// An original outgoing message found after restore may have reached its recipient.
    PossiblyDelivered,
    /// A counterparty's current Private Payment List was given up with an unknown outcome.
    /// For a withdrawal the counterparty may keep paying the earlier list; otherwise it may
    /// lack the list. Only the user can authorize another version.
    ListDeliveryUnknown,
    /// Messages a blocked counterparty's inbox accepted from `Notice::lost_from_ms` to
    /// `Notice::lost_until_ms`, which the app dropped for the user's block to ask for again once
    /// the counterparty is unblocked, can no longer come back: its sender keeps them no longer,
    /// or the bound on marks dropped this one (Paykit spec 8.1, "Blocked counterparties").
    BlockedMessagesLost,
    /// The settlement bucket cannot take a new mark (Paykit wire 5.7).
    SettlementBucketFull,
    /// This settlement can only be credited with the user's confirmation.
    CreditNeedsUser,
    /// An authenticated bucket omitted or raised a lower mark the wallet retained.
    SettlementMarksChanged,
    /// One settlement was credited to different occurrences.
    SettlementCreditedTwice,
    /// A bounded recovery family discarded a window before a request covering it was delivered.
    DroppedMessagesLost,
    /// The identity's block floor is held after it was seen divergent: no automatic one-time
    /// payment goes and each request can still be paid by hand. Shown once, and again at each
    /// reopen.
    BlockFloorHeld,
    /// A block floor raise the wallet owes isn't written yet, with no divergence hold: until it
    /// is, an unblocked sender's one-time requests wait and it is asked nothing, and while a
    /// widening's raise waits no one is asked to re-send. The wallet tries again at the next
    /// receive, then after a wait of one minute doubling to ten, ended early when the floor
    /// changes or becomes readable or a new raise is owed.
    /// Shown once per load while it stands; it goes when the raise is written, or when the
    /// divergence hold stands, which its own notice tells.
    FloorRaiseWaiting,
    /// Automatic one-time payments for the identity, and resume repeats of one-time payments,
    /// the user's own included, are held for good after a provider loss; each request can
    /// still be paid by hand, and Billing Periods and payments without a request go on.
    /// Shown once, and again after each reopen.
    AutomaticPaymentsHeld,
    /// The record no longer shows this stop, and the request or Allowance is still open
    /// (Paykit spec 7.2).
    StopNotRecorded,
    /// A change of the app's public endpoints saved and not signed wasn't published after a
    /// restart or a restore (Paykit spec 4): withdraw again the endpoints it names to withdraw;
    /// ask the user before making its offers and payload changes again.
    PublicEndpointChangeNotPublished,
    /// A native case that this binding does not recognize.
    Other,
}
impl From<paykit_sdk::NoticeKind> for WalletNoticeKind {
    fn from(k: paykit_sdk::NoticeKind) -> Self {
        use paykit_sdk::NoticeKind as K;
        match k {
            K::PaidAfterFailed => Self::PaidAfterFailed,
            K::HeldWithoutPay => Self::HeldWithoutPay,
            K::MessageGivenUp => Self::MessageGivenUp,
            K::AllowancePlacementExpired => Self::AllowancePlacementExpired,
            K::AllowanceAcceptanceBeforeLoss => Self::AllowanceAcceptanceBeforeLoss,
            K::AllowanceWithdrawalConfirmed => Self::AllowanceWithdrawalConfirmed,
            K::StopNotRecorded => Self::StopNotRecorded,
            K::PossiblyDelivered => Self::PossiblyDelivered,
            K::ListDeliveryUnknown => Self::ListDeliveryUnknown,
            K::BlockedMessagesLost => Self::BlockedMessagesLost,
            K::SettlementBucketFull => Self::SettlementBucketFull,
            K::CreditNeedsUser => Self::CreditNeedsUser,
            K::SettlementMarksChanged => Self::SettlementMarksChanged,
            K::SettlementCreditedTwice => Self::SettlementCreditedTwice,
            K::DroppedMessagesLost => Self::DroppedMessagesLost,
            K::BlockFloorHeld => Self::BlockFloorHeld,
            K::FloorRaiseWaiting => Self::FloorRaiseWaiting,
            K::AutomaticPaymentsHeld => Self::AutomaticPaymentsHeld,
            K::PublicEndpointChangeNotPublished => Self::PublicEndpointChangeNotPublished,
            _ => Self::Other,
        }
    }
}
/// The durable settlement or proof a wallet notice identifies.
#[derive(uniffi::Enum)]
pub enum NoticeSubject {
    /// A settlement bucket needs the host's attention.
    Bucket {
        /// The affected settlement bucket number.
        number: u16,
    },
    /// A particular authenticated write changed a settlement bucket.
    Changed {
        /// The settlement bucket number.
        bucket: u16,
        /// The writing app's App ID.
        writer: Vec<u8>,
        /// The provider generation that dated the change.
        generation: u64,
        /// The sequence of the authenticated change.
        sequence: u64,
        /// The hash of the authenticated item.
        item_hash: Vec<u8>,
    },
    /// More than one occurrence claims this settlement.
    Conflicting {
        /// The settlement bucket number.
        bucket: u16,
        /// The settlement ID with conflicting occurrences.
        settlement: Vec<u8>,
    },
    /// The notice identifies a settlement by its tag.
    Tag {
        /// The settlement tag.
        tag: Vec<u8>,
    },
    /// A payment proof for the named request needs attention.
    RequestedProof {
        /// The protected activity record holding the proof.
        record: Vec<u8>,
        /// The original payment proof's hash.
        proof: Vec<u8>,
    },
    /// A native case that this binding does not recognize.
    Other,
}
impl From<paykit_sdk::SettlementSubject> for NoticeSubject {
    fn from(s: paykit_sdk::SettlementSubject) -> Self {
        use paykit_sdk::SettlementSubject as S;
        match s {
            S::Bucket(n) => Self::Bucket { number: n },
            S::Changed {
                bucket,
                writer,
                application,
                ..
            } => Self::Changed {
                bucket,
                writer: writer.into(),
                generation: application.generation,
                sequence: application.seq,
                item_hash: application.item_hash.into(),
            },
            S::Conflicting {
                bucket, settlement, ..
            } => Self::Conflicting {
                bucket,
                settlement: settlement.into(),
            },
            S::Tag(tag) => Self::Tag { tag: tag.into() },
            S::RequestedProof { record, proof, .. } => Self::RequestedProof {
                record: record.into(),
                proof: proof.into(),
            },
            _ => Self::Other,
        }
    }
}
/// One settlement mark of a `settlements` bucket (Paykit wire 5.7): a settlement the identity
/// has credited, and the occurrence it was credited to.
#[derive(uniffi::Record)]
pub struct SettlementMark {
    /// Bytes 2 to 13 of the settlement's tag (`derive::mark_settlement_tag`).
    pub settlement: Vec<u8>,
    /// The occurrence kind used for the settlement mark.
    pub occurrence_kind: u8,
    /// The occurrence's tag (`derive::occurrence_tag`).
    pub occurrence: Vec<u8>,
}
/// A notice for the host to show, until it acknowledges it (`Paykit::acknowledge_notice`).
#[derive(uniffi::Record)]
pub struct WalletNotice {
    /// Its stable ID, which `Paykit::acknowledge_notice` takes.
    pub id: String,
    /// What it tells.
    pub kind: WalletNoticeKind,
    /// The bucket or settlement of a payee warning, when applicable.
    pub settlement: Option<NoticeSubject>,
    /// The attempt it is about; `None` for a notice about a message.
    pub attempt_id: Option<String>,
    /// The signed message ID associated with the notice, if present.
    pub message_id: Option<String>,
    /// The counterparty: the payee of an attempt, the message's sender, or, for an own answer
    /// given up, its payee.
    pub counterparty: Option<Vec<u8>>,
    /// The conflicting marks when one settlement names more than one occurrence.
    pub settlement_marks: Vec<SettlementMark>,
    /// The request paid or named, `None` for none.
    pub request_hash: Option<Vec<u8>>,
    /// The signed JSON hash of a possibly delivered outgoing message.
    pub message_hash: Option<Vec<u8>>,
    /// When it was raised, in Unix milliseconds: the highest service time the app had verified
    /// then, 0 before any.
    pub raised_at_ms: u64,
    /// For `NoticeKind::BlockedMessagesLost` or `NoticeKind::DroppedMessagesLost`: the earliest
    /// acceptance time of what can no longer come back.
    pub lost_from_ms: Option<u64>,
    /// For `NoticeKind::BlockedMessagesLost` or `NoticeKind::DroppedMessagesLost`: the latest
    /// such time.
    pub lost_until_ms: Option<u64>,
    /// For `NoticeKind::PublicEndpointChangeNotPublished`: the endpoints to withdraw again.
    pub endpoints_withdrawn: Vec<String>,
    /// For `NoticeKind::PublicEndpointChangeNotPublished`: the endpoints to offer again, when
    /// the user asks.
    pub endpoints_offered: Vec<String>,
    /// For `NoticeKind::PublicEndpointChangeNotPublished`: the endpoints whose new payload to
    /// set again, when the user asks.
    pub endpoints_changed: Vec<String>,
}
/// The original retained deletion notice that the host can acknowledge.
#[derive(uniffi::Object)]
pub struct DeletionNotice(paykit_sdk::PayeeDeletionNotice);
/// A deletion to show the user (Paykit spec 6).
#[derive(uniffi::Record)]
pub struct DeletedRecord {
    /// The record the delete named.
    pub record: Vec<u8>,
    /// The generation of the deletion.
    pub generation: u64,
    /// The sequence of the signed deletion in its generation.
    pub version: u64,
    /// The authenticated deletion revision.
    pub deletion: Vec<u8>,
    /// The deleting App ID when the app could prove its predecessor was protected.
    pub writer: Option<Vec<u8>>,
    /// The verified service time of the original feed page, absent on a listing alone.
    pub page_time_ms: Option<u64>,
}
#[uniffi::export]
impl DeletionNotice {
    /// Read the authenticated deletion and the affected record before acknowledging it.
    pub fn details(&self) -> DeletedRecord {
        let n = &self.0;
        DeletedRecord {
            record: n.record.into(),
            generation: n.generation,
            version: n.version,
            deletion: n.delete.into(),
            writer: n.writer.map(Vec::from),
            page_time_ms: n.page_time_ms,
        }
    }
}
#[uniffi::export]
impl NativeWallet {
    /// List the Paykit notices awaiting individual acknowledgment.
    pub async fn notices(&self) -> Result<Vec<WalletNotice>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .notices()
                    .await?
                    .into_iter()
                    .map(|n| WalletNotice {
                        id: n.id,
                        kind: n.kind.into(),
                        settlement: n.settlement.map(Into::into),
                        attempt_id: n.attempt_id.map(|id| id.to_text()),
                        message_id: n.msg_id,
                        counterparty: n.counterparty.map(Vec::from),
                        settlement_marks: n
                            .settlement_marks
                            .into_iter()
                            .map(|mark| SettlementMark {
                                settlement: mark.settlement.into(),
                                occurrence_kind: mark.kind.code(),
                                occurrence: mark.occurrence.into(),
                            })
                            .collect(),
                        request_hash: n.request_hash.map(Vec::from),
                        message_hash: n.message_hash.map(Vec::from),
                        raised_at_ms: n.raised_at_ms,
                        lost_from_ms: n.lost_from_ms,
                        lost_until_ms: n.lost_until_ms,
                        endpoints_withdrawn: n
                            .endpoints_withdrawn
                            .iter()
                            .map(|id| id.as_str().to_owned())
                            .collect(),
                        endpoints_offered: n
                            .endpoints_offered
                            .iter()
                            .map(|id| id.as_str().to_owned())
                            .collect(),
                        endpoints_changed: n
                            .endpoints_changed
                            .iter()
                            .map(|id| id.as_str().to_owned())
                            .collect(),
                    })
                    .collect())
            })
        })
        .await
    }
    /// Acknowledge the notice after handling its effects durably.
    pub async fn acknowledge_notice(&self, id: String) -> Result<(), WalletError> {
        self.with_wallet(move |app| Box::pin(async move { Ok(app.acknowledge_notice(&id).await?) }))
            .await
    }
    /// List pending notices about deleted payee records.
    pub async fn payee_deletion_notices(&self) -> Result<Vec<Arc<DeletionNotice>>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move {
                Ok(app
                    .payee_deletion_notices()
                    .await?
                    .into_iter()
                    .map(|n| Arc::new(DeletionNotice(n)))
                    .collect())
            })
        })
        .await
    }
    /// Acknowledge the exact deletion notice after handling it.
    pub async fn acknowledge_payee_deletion(
        &self,
        notice: Arc<DeletionNotice>,
    ) -> Result<(), WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.acknowledge_payee_deletion(&notice.0).await?) })
        })
        .await
    }
}
