//! The typed reasons of the refusals that wait, that the user answers, or that a recovery step
//! clears. Each reason names what clears it, so a caller acts on the reason alone; its text
//! is for display.

/// What a call waits for: a later call, a read or a wait changes it, and the call is made
/// again then.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NotNowReason {
    /// The shared records aren't listed yet; calling `retry` again continues the listing
    /// (Switch spec 8.2, 8.3, 12.4).
    #[error("the shared records aren't listed yet. {} to continue the listing", retry.call())]
    RecordsUnlisted {
        /// The call that continues the listing.
        retry: ListingRetry,
    },
    /// The shared records aren't listed since the restore, the missed item or under the
    /// certificate held now; a receive or a write lists them (Switch spec 8.2, 8.3, 12.4
    /// step 2.1).
    #[error(
        "Switch spec 8.2, 8.3, 12.4 step 2.1: the shared records aren't listed since the \
         restore, the missed item or under the certificate held now"
    )]
    RecordsNotListed,
    /// The scope keys of the certificate taken aren't stored yet; the signer stores them when
    /// it is next online (Switch spec 3.5).
    #[error(
        "Switch spec 3.5: waiting for the scope keys of the certificate taken, which the \
         signer stores when it is next online"
    )]
    ScopeKeysAwaited,
    /// A record's value is sealed under scope keys not stored yet; the signer stores them
    /// when it is next online (Switch spec 3.5).
    #[error(
        "Switch spec 3.5: waiting for the scope keys the record's value is sealed under, which \
         the signer stores when it is next online"
    )]
    ValueAwaitsKey,
    /// The block floor can't be read now: the payer's records not listed yet, a scope key
    /// awaited, a kept floor write, a read or a write the switch didn't answer, or a delete
    /// nothing authenticates. A later receive clears each of them with no one acting
    /// (Paykit wire 5.6). A floor record that doesn't read is
    /// [`HeldForUserReason::FloorRecordUnreadable`], and one deleted or unlawfully absent is
    /// the rollback alarm.
    #[error("Paykit wire 5.6: the block floor can't be read now")]
    FloorUnreadable,
    /// A raise of the block floor left unwritten waits before it is tried again; the same
    /// call once the wait has passed clears this (Paykit wire 5.6).
    #[error("Paykit wire 5.6: a block floor raise left unwritten waits before it is tried again")]
    FloorRaiseWaiting,
    /// No service time is verified yet; a read of the switch that verifies one clears this
    /// (Switch spec 10.1). The text names the call's rule.
    #[error("{0}")]
    NoServiceTime(&'static str),
    /// What the call relies on isn't verified, isn't current or changed during the read; a
    /// later read clears this. The text names what.
    #[error("{0}")]
    ReadAgain(&'static str),
    /// An earlier write of this app is unresolved; its answer clears this. The text names it.
    #[error("{0}")]
    EarlierWriteUnresolved(&'static str),
    /// The same call made again clears this: it picks a new identifier or reads again. The
    /// text names why.
    #[error("{0}")]
    CallAgain(&'static str),
    /// An Allowance limit has no room for the payment now; its window rolling, or an
    /// earlier payment's release, makes room (Paykit spec 8.3).
    #[error("the {limit} limit has no room")]
    AllowanceLimit {
        /// The limit with no room.
        limit: String,
    },
}

/// The call [`NotNowReason::RecordsUnlisted`] names to continue a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ListingRetry {
    /// Call `receive` again.
    Receive,
    /// Retry the original action.
    OriginalAction,
    /// Call `reconcile` again.
    Reconcile,
    /// Call `end_reconcile` again.
    EndReconcile,
}

impl ListingRetry {
    fn call(self) -> &'static str {
        match self {
            Self::Receive => "Call receive again",
            Self::OriginalAction => "Retry the original action",
            Self::Reconcile => "Call reconcile again",
            Self::EndReconcile => "Call end_reconcile again",
        }
    }
}

/// What the user or the host acts on before the call goes on, which no wait clears: an
/// unblock, a paused record's confirmation, a new decision to pay, a reload, the execution of
/// an Allowance taken up or a new one set up, or a hold on one record, publication or
/// delivery that only its repair or the loss steps lift (Paykit spec 7.3, 8.1, 8.3, 8.4). It
/// holds that item, not the wallet.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HeldForUserReason {
    /// The deny list holds the counterparty; the user unblocking it clears this (Paykit spec
    /// 8.1, "Blocked counterparties"). The text names what is held.
    #[error("{0}")]
    CounterpartyBlocked(&'static str),
    /// The request is held from automatic payment: by a block, a block floor, a restore's or
    /// a missed item's hold, or a pause. The user unblocking the payee, confirming the hold or
    /// the paused record, or paying it by hand clears it; a floor holds until it is paid by
    /// hand (Paykit spec 8.1, Paykit wire 5.6).
    #[error("this request is held from automatic payment")]
    HeldFromAutomatic,
    /// After a provider loss the switch shows another write at the held activity record's
    /// version; a write the app takes replaces it (Switch spec 12.6).
    #[error("the switch shows another write at the held activity record's version")]
    ActivityBaseChanged,
    /// The order of the request's cancellations is unknown after a provider loss; the loss
    /// steps decide it (Paykit spec 7.3).
    #[error("the order of the request's cancellations is unknown after provider loss")]
    CancellationOrderUnknown,
    /// Two signed lists, or another publisher's write, conflict at one publication counter:
    /// publication to that recipient is held while the other app's write stands, until a
    /// bound publisher write replaces it (Paykit spec 8.4.1, Paykit wire 5.8). The text names
    /// them.
    #[error("{0}")]
    PublicationConflict(String),
    /// A held record doesn't read; it is rewritten or recovered, or else stays held (Paykit
    /// wire 5.3). The text names the record.
    #[error("{0}")]
    RecordUnreadable(&'static str),
    /// The block floor record doesn't read, and this app has seen no floor it could write
    /// back, so it writes no raise (Paykit wire 5.6). No later read of this app clears it,
    /// and this version has no call that repairs the record: it reads again only once an own
    /// app that holds a readable floor writes it. Until then an unblock, which needs the
    /// raise, is refused and the counterparty stays blocked; a block still takes effect; and
    /// one-time requests are paid by hand, none automatically.
    #[error("Paykit wire 5.6: the block floor record doesn't read")]
    FloorRecordUnreadable,
    /// After a provider loss this version doesn't resume automatic payments under the
    /// Allowance, whose usage needs a repair it doesn't make: the user ends it and sets up a
    /// new one (Paykit spec 8.3, "After provider loss").
    #[error(
        "after a provider loss this version doesn't resume automatic payments under the \
         Allowance, whose usage needs a repair it doesn't make: the user ends it and sets up a \
         new one"
    )]
    AllowanceNotResumed,
    /// After a restore this installation can't establish the Allowance's ledger history: the
    /// ledger moved on by more than one write since the backup, and the write between is no
    /// longer readable, so no later read brings it back, and this version makes no repair.
    /// The user ends the Allowance and sets up a new one, or pays by hand; a newer backup of
    /// this app that holds the ledger as it is now also resumes it (Paykit spec 8.3,
    /// "Resuming the same executor").
    #[error(
        "after a restore this installation can't establish the Allowance's ledger history: \
         the user ends it and sets up a new one, or pays by hand"
    )]
    AllowanceNotResumedAfterRestore,
    /// This app's execution of the Allowance is held until the user or the host acts, as the
    /// text names: this app takes the execution up ([`crate::Paykit::resume_allowance_executor`]),
    /// the user confirms the Allowance record as shown, or pays by hand (Paykit spec 8.3,
    /// "Executor").
    #[error("{0}")]
    AllowanceExecutionHeld(&'static str),
    /// A check before money moved failed, so the attempt was written `failed` and nothing was
    /// paid. It comes only inside [`crate::Refusal::AttemptFailed`]. A new payment is the
    /// user's decision, made once the check can pass: for a certificate expiring within the
    /// budget, after the signer renews it (Paykit spec 7.2 step 1). The text names the check.
    #[error("{0}")]
    AttemptCheck(&'static str),
    /// The store couldn't tell whether the last write of the app's record was applied; the
    /// host loads the app again ([`crate::Paykit::load`]) before editing its record, which
    /// no wait on this handle clears.
    #[error(
        "the store couldn't tell whether the last write of the app's record was applied: load \
         the app again (Paykit::load) before editing its record"
    )]
    ReloadRequired,
    /// The Switch client holds the attempt back, named in its text: reconcile mode, which the
    /// user ends, the clocks apart, which the user may have to correct, or a display read the
    /// host owes (Switch spec 10.1, 12.4). The text alone tells them apart, so it is shown.
    #[error("{0}")]
    SwitchClient(String),
    /// A hold on one request, publication or delivery that no wait clears as far as this
    /// wallet can tell: the text names it, and the host shows it (Paykit spec 8.1, 8.4).
    #[error("{0}")]
    Unresolved(&'static str),
    /// Eight own Allowance proposals to this counterparty are already pending: the
    /// counterparty answering one makes room, though a conflicted one keeps counting until
    /// the user discards or withdraws it, so it is shown (Paykit spec 8.3).
    #[error("eight own proposals to this counterparty are already pending")]
    PendingProposalsFull,
    /// The Allowance ledger has no room for the payment and its outcome: earlier outcomes
    /// resolving may let it compact and make room, or the entries it keeps may never, so it
    /// is shown (Paykit spec 8.3).
    #[error("the Allowance ledger has no room for the payment and its outcome")]
    AllowanceLedgerFull,
    /// The original Allowance answer has no verified outcome: a receive may learn whether it
    /// was delivered, or its delivery may have failed for good, so it is shown (Paykit spec
    /// 8.3).
    #[error("the original Allowance answer has no verified outcome")]
    AnswerOutcomeUnknown,
}

/// The recovery step of the wallet as a whole that clears a refusal: no wait does (Paykit
/// spec 7.3).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RecoveryReason {
    /// No reconcile under `paykit_pay` has read the payer's obligations since the restore;
    /// [`crate::Paykit::reconcile`] runs that payer pass (Paykit spec 7.3).
    #[error(
        "Paykit spec 7.3: no reconcile under pay has read the payer's obligations since the \
         restore"
    )]
    PayerPassOwed,
}
