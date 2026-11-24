# Paykit host guide

A wallet owns one `Paykit` handle for each approved App ID. Supply the same
durable Switch store, transport, clock and Homeserver access when reopening it.
Use the [Switch app host guide](https://github.com/pubky/pubky-switch/blob/d4f33242e2771a773e004b7fd2be0ebb09505c86/docs/app-host-guide.md)
for store guarantees, approval and client outcomes. Keep app keys and backups in
host-protected storage and serialize operations on the handle. A memory store
cannot preserve unanswered writes or payments across a restart.

The integrator chooses whether to enroll an App ID per installation or share one
App ID and its keys. A separate App ID has its own signer approval and can be
revoked independently. Installations sharing an App ID have the same grants.
Synchronize receive keys, cursor, the public endpoint member and its deadline,
payment journal and backend payment history across those installations and restores. The wallet owns payment-method
idempotency and must prevent duplicate execution of an attempt.
For example, two wallets enrolled independently under the same payer identity
use distinct App IDs and can both retrieve the seller's receipt for the same payment.

## Open a wallet and pay a one-time request

Use `Paykit::join` with a signer-approved `JoinRequest` at enrollment. On an
ordinary startup, load the existing App ID. Use `Paykit::restore` with an
authorized `PaykitBackup` when recovering the app. That path validates the
Paykit layer and installs the payment journal with the client state. The
`*_with_protocols` constructors allow other protocols on the same App ID. Keep
the exact Paykit protocol instance supplied to the registration closure.

The one-time payment path uses these SDK calls. Keep the receive summary so
the host can handle its duty events. List received requests and show the
selected request's payee and amount. Call `pay_request` with
that request's opaque reference only after the user chooses to pay it. Retain
the returned attempt ID and exact outcome through UI refreshes. An unanswered
or uncertain call stays pending while the existing attempt is resolved.

```rust
use std::sync::Arc;

use paykit_sdk::{
    AppEnv, AttemptId, PayOptions, PayResult, Paykit, PaykitConfig, Rail, ReceiveSummary,
    RequestRef, RequestView, Result, Stage, Timer,
};

async fn open_wallet(
    env: AppEnv,
    app_id: [u8; 16],
    rail: Arc<dyn Rail>,
    timer: Arc<dyn Timer>,
) -> Result<Paykit> {
    Paykit::load(env, PaykitConfig::default(), app_id, rail, timer).await
}

async fn refresh_wallet(wallet: &mut Paykit) -> Result<ReceiveSummary> {
    wallet.receive(None).await
}

async fn one_time_requests(wallet: &Paykit) -> Result<Vec<RequestView>> {
    Ok(wallet
        .requests()
        .await?
        .into_iter()
        .filter(|request| !request.recurring)
        .collect())
}

async fn pay_selected(wallet: &mut Paykit, request: &RequestRef) -> PayResult {
    wallet.pay_request(request, PayOptions::default()).await
}

async fn payment_after_restart(wallet: &Paykit, attempt_id: &AttemptId) -> Result<Option<Stage>> {
    Ok(wallet
        .payments()
        .await?
        .into_iter()
        .find(|payment| &payment.attempt_id == attempt_id)
        .map(|payment| payment.stage))
}
```

`RequestView::status` describes the SDK's stored view. Read it together with
`manual_only`, `held` and `divergent`. `PayResult` carries the outcome or error
and any unavailable public endpoint apps. Keep an explicit conservative fallback
for future variants in non-exhaustive enums.

After a restore that isn't clean, the wallet raises the identity's block floor to
the time of its first read of the deny list. A one-time request created before that
time plus the 5-minute clock tolerance opens `manual_only`, whatever its sender,
blocked before or not: it is paid only by hand. One created later opens without it,
unless a higher floor or a hold covers it too.

## Publish a public endpoint

An app with plain `paykit_request` and record publication enabled can use
`set_public_endpoint` to add a reusable destination. A new offer has no Paykit
deadline by default. To choose a fixed cutoff for the whole offer, call
`set_public_endpoints` with the complete endpoint map and `Some(timestamp_ms)`.
Use `None` to offer the full set without a Paykit deadline. A different protocol on the same App ID may
require an earlier deadline for the signed record. Receive renews the record
within the identity age limit. Once the cutoff arrives, upkeep withdraws the
endpoints. A fresh explicit offer may choose a new deadline.

`own_public_endpoints` reads the latest locally signed offer. A successful
`Publication::Published` confirms the Homeserver copy. If a sibling protocol
delays signing, `PaykitError::PublicEndpointChangePending` means that the
client retained the requested edit. Show the withdrawal as pending until the
client signs the updated record and reports its publication.

After restoring a backup, drive the wallet's first receive. When publication
returns `Publication::AwaitingRecord`, the wallet still has a newer record
choice to settle. Keep the change pending and continue normal receives.
A restore carrying only the accepted offer can repair its missing public copy.

## Use a Switch App built by the host

When the host opens the Switch App, use `PaykitConfig::protocol` to build the
protocol with the wallet's configured extension kinds. Register it in the App's
protocol set. Hand the App and that exact
protocol instance to `Paykit::attach` immediately after `App::join` or
`App::load`, before driving its feed or outbox. The wallet uses the same rail
as the protocol and the clock, Homeserver reader and publication setting of
the App. Other registered protocols stay with the App.

```rust
use std::sync::Arc;

use paykit_sdk::{App, AppEnv, Paykit, PaykitConfig, Rail, Result, Timer};

async fn attach_wallet(
    env: AppEnv,
    app_id: [u8; 16],
    config: PaykitConfig,
    rail: Arc<dyn Rail>,
    timer: Arc<dyn Timer>,
) -> Result<Paykit> {
    let protocol = config.protocol(rail);
    let app = App::load(env, Arc::clone(&protocol), config.app.clone(), app_id).await?;
    Paykit::attach(app, config, protocol, timer).await
}
```

Use an SDK join constructor when Paykit should check the approval and the
rail's capabilities before the App is saved. Open backups with
`Paykit::restore` or `Paykit::restore_with_protocols` so Paykit validates the
journal before any restore write. If an SDK-restored store is still in
reconciliation on startup, reopen it with the SDK load constructors.

A backup restored in place, into the store of the installation it came from, keeps
what that installation still holds that stops a payment: a cancellation or an
Allowance end not yet recorded, an Allowance removed from the consent list, an
answer held back, a block not yet in the deny list, the newest payee records seen
and a list held over an unreadable record. The restored app can only do less
because of them, and the user can act on each as before, by unblocking or
consenting to a new Allowance. If the installation's store is damaged so that
`Paykit::restore` refuses it, restore into a new, empty store instead: that
restore reads nothing of the old installation.

Register the extension kinds your app handles with `PaykitConfig::with_extension_kinds`.
After a receive, read `Paykit::extension_messages` and acknowledge each message by
sender and message ID once your extension has durably handled it. Reads leave the
messages available to the host across restarts. The wallet can evict untaken
messages when the per-sender or total bound is reached (Paykit spec 3.3).

## Outgoing payment history and payer access

An app granted `paykit_pay` can make payments and read the identity's past and
future outgoing Paykit history. This includes earlier payments made by other
wallets under the same identity and payments made without a request.

We recommend that the signer or host explain this access in its own words
before the user approves the grant. This applies to direct payer grants,
Allowance executor grants with payer access, and grants that bundle payer
access with another protocol.

## Payment and delivery progress

Use the SDK states below to explain payment progress in the wallet's own words.
The wallet chooses how to present each state. While a payment may already have
gone through, keep that uncertainty clear and follow the reconciliation rules.

After loading, read the durable journal with `Paykit::payments()`. Match the
existing `PaymentView::attempt_id` and use its `stage` to track progress. A durable
`Stage::RailPaid` entry means the rail paid, with outcome reporting still
pending. Use `resume_payments` to progress outstanding work, then read
`payments()` again to refresh the journal view.

When another wallet marks a subscription manual-only, the wallet that started
a payment by hand can still resume that attempt after a restart. The SDK
reuses the same attempt ID if the method still guarantees idempotency. A
cancellation can withhold that repeat and leave the original payment uncertain.

After restoring, process `receive` and `reconcile`, show unresolved attempts
using the report, and complete `end_reconcile` after user confirmation.
A missing receipt or an unavailable provider can leave the outcome unresolved.
Read `Paykit::notices()` after the first receive and on subsequent loads. A
`PossiblyDelivered` notice identifies an outgoing message whose recipient may
already have it. Reconcile it using the message ID, recipient and signed hash.
A receipt can still raise this notice after a provider loss and a later request
cancellation. The wallet verifies the original receipt write with the service
identity from that generation, then matches it to the signed receipt still held
for the same request. An unresolved receipt stays pending when that evidence
is unavailable.
The original wallet can resume an outstanding delivery when reopened from its
current store.

| Case | What to show or do |
|---|---|
| `PayOutcome::Paid` | The payment is recorded and its proof is queued. Show it as paid and keep the attempt ID. |
| `PayOutcome::PaidUnreported` | The method paid, but this app couldn't write the outcome. Show that reporting remains unresolved and use reconciliation. |
| `PayOutcome::Failed` | The method definitively did not pay and the failed outcome was recorded. Show the reason. |
| `PayOutcome::Pending` | Keep showing the same attempt while the method reports or the SDK resumes it. |
| `PayOutcome::Uncertain` | Show that it may already be paid. Resolve with a receipt or the user's reconciliation decision. |
| `PayOutcome::PaidOutcomeUnwritten` | The method paid and the proof is in the journal. Resume the same attempt to record it and queue the proof. |
| `PayOutcome::RecordHeld` | The method paid, but the record update is held. Resume when the record is readable or repaired. |
| `PayOutcome::RecordDivergent` | The method paid and the record awaits confirmation. Show the named record, confirm it with the user and resume. |
| `Stage::Recorded` | The journal exists before the Switch attempt write. |
| `Stage::Attempted` | The attempt is written and checked. Payment has yet to be dispatched. |
| `Stage::Paying` | Dispatch may have happened. Treat the attempt as potentially paid. |
| `Stage::RailPaid` | The method reported payment. The SDK still owns outcome recording. |
| `Stage::RailFailed` | The method reported a definitive failure. The SDK still owns outcome recording. |
| `Stage::AcceptanceOwed` | The payment outcome is recorded. Resume the outstanding acceptance delivery. |
| `Stage::Finished` | The journal operation finished. Read the request or activity for the outcome. |
| `Stage::Abandoned` | The journal entry ended before the attempt was written or money could move. |
| `Resolution::Done` | The user confirms the attempt was paid. |
| `Resolution::Unknown` | The user leaves the attempt unresolved. A later attempt carries possible-duplicate risk. |
| `Resolution::Failed` | The user confirms the attempt definitively did not pay. |
| `MessageDelivery::Kept` | Track the existing batch. Let a read or write retry its signed bytes. |
| `MessageDelivery::Queued` | Keep the same message ID while the outbox retries. |
| `MessageDelivery::WaitingForAdmission` | Tell the user the recipient needs to admit this identity. |
| `MessageDelivery::CertificateNotCurrent` | Obtain renewal from the signer. The message can wait in the outbox. |
| `MessageDelivery::Accepted` | The recipient's inbox accepted the message. Payment and credit have their own outcomes. |
| `MessageDelivery::Failed` | Show the failure and inspect the record and attempt before deciding whether to send something new. |
| `MessageDelivery::Other` | Preserve and display the client state conservatively. |

## Request and Allowance status

The `record` returned by `propose_allowance`, `allowance` and `allowances`
is the account’s permanent Allowance key. The pending proposal lives at its
own ordinary key until answered or withdrawn. Pass the returned key to
`accept_allowance`, `accept_executable_allowance`, `reject_allowance`, `end_allowance` or `discard_pending_allowance`.
The seller uses `propose_allowance` with `AllowanceRole::Allowee` or answers
with `accept_allowance`. The payer uses `propose_executable_allowance` or
`accept_executable_allowance` after the user confirms the counterparty, budget
and automatic payments by this wallet.

Before creating a proposal, the wallet checks the current proposals for that
counterparty. Sent and received proposals have separate counts. A wallet that
sees eight already sent waits for one to close before it creates another.
An incomplete inventory also holds creation. Concurrent wallets may both
create while they each see fewer than eight.

A wallet with `paykit_allowance` can read another app’s pending record on its
account. `PendingAllowanceRecord::read` and `bind` check the signed parties,
proposal and physical key (Paykit wire 5.4). An unreadable or flagged value
keeps the last qualified roles and holds further action. A verified current
deletion ends that proposal’s lifetime.

If the payer restores a backup made before withdrawing the proposal, the wallet
cannot reopen the deleted proposal for spending. Even if neither the restored
wallet nor the service has a record of sending the withdrawal, it may have
been sent. The wallet does not resend it on that basis. A seller who accepted
the proposal may still see `Accepted` if the withdrawal never arrived.

On explicit acceptance, each party records the signed proposal and answer and
removes its current pending proposal together. The payer’s write also creates
the empty spending ledger bound to that answer. The payer wallet needs
`paykit_allowance` to approve the original and `paykit_allowance_execute`
to create the ledger. Every later ledger write needs the same execution scope.
An unanswered write retains the original signed reply for retry. Automatic
payment also requires the payer's explicit choice to use this wallet as executor.

After the original exchange applies, the approving payer calls
`Paykit::resume_allowance_executor` with the permanent Allowance handle to
complete its first executor grant. Repeating that call or reopening the same
wallet preserves the original grant. A sibling wallet, missing original
application or unverified ledger change leaves the grant held. When the grant's
write gets no answer, the Allowance isn't consented yet even if the grant later
applies: retry `Paykit::resume_allowance_executor` until it succeeds. Each retry
sends the kept grant again first, so no `receive` is needed in between, and no
second grant is built. If the client instead gives the grant's batch up with an
unknown outcome (`AppEvent::KeptBatchDropped`), the grant may have applied without
this app's record of it, and the resume stays held. After a
restore that isn't clean or a `missed` item, the user confirms again before the
first grant, as for any Allowance that requires consent again: show the
counterparty, terms and this executor before calling
`Paykit::confirm_allowance_executor`. These calls establish the first grant.

Call `Paykit::reconcile_allowance` to read this executor's verified usage.
The method adapter supplies `Rail::allowance_history` for the payer's entire
wallet account, including other installations and previous certificate lifetimes.
Return `None` when that history is incomplete. When reconciliation returns
`None`, show usage as unknown and keep automatic payment on hold.
An older backup can predate the proof of the original grant. When the
installed wallet still has that proof and its later ledger, reconciliation
counts the outstanding reservations. Show the existing consent prompt if the
restored Allowance requires confirmation.

After a provider loss, an Allowance the user confirmed before the loss pays
nothing automatically. A user who wants automatic payments again ends that
Allowance and creates a new one with the counterparty. It also asks with
`AllowanceConfirmationNeeded`: confirming is recorded and writes its grant again,
but in this version automatic payments under it don't resume, since its usage needs
a repair this version doesn't make. They answer `NotNow` with that reason; don't
offer confirming as the way to resume, and don't ask the user to confirm again.
One-time requests stay held after a provider loss in any case (Paykit spec 8.1,
8.3).

If `Paykit::ledger_alarm` returns an alarm, show its reason when opening the
wallet and after a reconciliation error. The Switch client stops new payment
attempts, including manual payments. Direct the user to the signer to move to
another provider or declare provider loss (Switch spec 10.1).

Call `Paykit::reserved_by_other_apps` with the permanent Allowance handle.
For each returned attempt, show its ID and transfer amount in the Allowance's
asset. If the user selects that attempt and confirms the release, pass its ID
and `UserConfirmed` to `Paykit::release_allowance_attempt`. The wallet
checks the current grant and failure and its own durable evidence that the
attempt ended before dispatch. A restored journal cannot establish that.
If the call returns a kept batch ID, show the release as pending. Receive or
resume and read the verified usage again before showing a change in capacity.
A later signed paid outcome still spends the amount (Paykit spec 8.3).

To pay an eligible one-time request, call `Paykit::pay_under_allowance`
with the same permanent Allowance handle. The wallet reserves the transfer
amount under the grant before calling the payment method. A refused limit or
incomplete wallet history leaves the request unpaid. Use the request and
receipt views to show a completed payment. The user can still choose to pay
a request by hand with `Paykit::pay_request`.

| Case | What to show or do |
|---|---|
| `RequestStatus::Open` | Show the request and the current conditions for acting on it. |
| `RequestStatus::BeingPaidHere` | Show the current payment attempt. |
| `RequestStatus::BeingHandledByOtherWallet` | Show that another own wallet holds the attempt. |
| `RequestStatus::MayBePaid` | Show the doubt and follow the SDK's confirmation or reconciliation path. |
| `RequestStatus::Paid` | Show it as paid based on the settlement or held payment proof. |
| `RequestStatus::Rejected` | Show that the identity rejected the request. |
| `RequestStatus::Cancelled` | Show that the request was cancelled. |
| `RequestStatus::Expired` | Show that the unpaid proposal expired. |
| `RequestStatus::Held` | Show the current restore or missed-item hold and ask for confirmation. |
| `RequestStatus::Unrecorded` | Show the received request while its record is still owed. Keep payment actions disabled. |
| `RequestStatus::Blocked` | Show that the payee is blocked and the request is paused. |
| `RequestStatus::Divergent` | Show the affected record and the recovery confirmation it needs. |
| `RequestStatus::Subscribed` | Show the active recurring request and its Billing Periods. |
| `RequestStatus::Ended` | Show that no new Billing Periods remain. Earlier periods can still have outcomes. |
| `PayeeStatus::Requested` | The request was sent and awaits an answer. |
| `PayeeStatus::Accepted` | The payer is paying or has subscribed. |
| `PayeeStatus::Rejected` | The payer rejected the request. |
| `PayeeStatus::Cancelled` | Either side cancelled it. |
| `PayeeStatus::ProofReceived` | Use the payee's verification and credit path. Show the proof as awaiting credit. |
| `PayeeStatus::Receipted` | Show the payment as credited and retain the receipt. |
| `AllowanceStatus::Proposed` | The proposal awaits an answer. |
| `AllowanceStatus::Accepted` | A stored acceptance is visible. Payments funded by this Allowance require its executor. |
| `AllowanceStatus::Rejected` | The recipient rejected the proposal. |
| `AllowanceStatus::Ended` | Show that the Allowance stopped or was withdrawn. |
| `AllowanceStatus::Conflicted` | Show that the competing proposals or replies prevent further use. |
| `AllowanceStatus::AwaitingConflictCheck` | The wallet retained possible conflict evidence after placement expired. Keep this Allowance on hold while the wallet checks its record. |

## Scheduling a Billing Period

The host schedules payments and records the user's recurring-payment policy. Accepting a
subscription schedules no payment. To fund it from an Allowance, its approving payer wallet
calls `accept_subscription_under_allowance` with the request and its permanent Allowance
handle. For each eligible occurrence, call `pay_period_under_allowance` with that request
and its exact Billing Period. The wallet checks the pinned Allowance, available budget and
payment method before it pays. A rate-priced endpoint requires a different funding decision.
If a later period requires its own destination, the wallet waits for signed details for that
occurrence from the selling app named by the request, if one is named. It selects the
latest valid details the payment method can pay within the request and Allowance. A newer
rate-priced offer leaves an older payable offer available. An unexpired destination already
held for the occurrence remains usable after the window for receiving new details closes.
Missing details return `NoDestination`, and unsupported endpoints leave the period unpaid.
After interruption, call `resume_payments`. Read the seller's receipt with
`ReceiptsOf::Period` for the same request and period.

For a current period outside an Allowance, choose how
the host may use the endpoint before calling `pay_period_automatically`.

- Use `PeriodPrice::RequestedAmount` when the host authorizes an eligible endpoint that
  transfers the requested asset and amount. This call can pay immediately.
- Use `PeriodPrice::QuoteFor(endpoint)` to inspect a signed rate for that endpoint.
  The call returns `PricedByRate` or another refusal and opens no attempt. It still opens
  no attempt when another endpoint can transfer the requested amount.
- Use `PeriodPrice::AtRate(quote)` only after the host authorizes that exact quote. If
  the user or policy declines it, leave the period unpaid and make no payment call.

For example, a user can authorize one subscription's current monthly period at a named
shop, payable to its chosen BTC endpoint for at most 0.0002 BTC at a rate of at most
0.00002 BTC per requested USD. Check the request, period, destination and both assets
before comparing the decimal amounts. If either ceiling is exceeded, keep the payment
waiting or show this quote to the user. The same standing policy can evaluate the next
month's quote. The SDK checks the signed source, amount and payment method again. A fee
above the configured cap still stops automatic payment. With no cap configured, the SDK
makes no fee check and asks nothing for the fee: the payment method's own limit, if it
has one, is the only bound on it. For a payment by hand, pass the
authorized quote through `PayOptions::with_rate_authorized`.

The SDK checks the automatic payment window, the request and period records, and the
usual payment restrictions again before dispatch. A missed period remains available for
`pay_period` when its other payment conditions hold.

## Notices and errors

Read `notices` and `payee_deletion_notices` by their stable identities. Acknowledge
each through its matching SDK API after recording or showing it. Handle Switch
duty events from `ReceiveSummary::report`. Call `receive` on startup and after a
feed wake. It returns one bounded report. Use `more` to decide when to read another
page. `ReceiveSummary::report.more` says the read stopped on one of its per-call
budgets, such as its feed pages or the kept batches it sends again, with more to do
now. Nothing it left is lost or reordered: the next read goes on from there. Handle
each summary's events first; then, while `report.more` is set and `report.stopped`
is not, call `receive(None)` again without waiting. If `stopped` is set, inspect the
reported event: reading again does nothing until it is dealt with. The loop ends at
the first summary with `more` unset or `stopped` set; a host that caps its rounds
reads the rest at its next wake. If
`records_unlisted` says the listing has more to read, call `receive(None)` again. The wallet can still be listing
collections after the Switch has finished its feed read. An explicit `reconcile` or
`end_reconcile` can also run its own listing. Follow the action named in its
`NotNow` result to resume that scan. After a receive error,
read and retain committed duties from `wallet.app_mut().unacknowledged_events()`.
Wait for a later successful `receive` so the SDK can process its duties, then
acknowledge for `"paykit"` with `App::ack_events_for` up to the last duty in
that successful report. Use the app-wide `App::ack_events` only after the
other responsible protocols have handled their duties too. Keep the deletion
notice and any surviving confirmed receipt together when showing a missing
shared record.

For a one-time request, continue calling `receive(None)` after a restart or
backup restore. A wallet that verifies the original deletion and the preceding
record in the current service generation can restore its issued receipt to
shared data. It preserves compatible fields added by the merchant's other
wallets. If one has already published the complete record, the recovering
wallet accepts it without another write. The next scheduled poll can report
the new cursor to the service. Acknowledging the deletion notice leaves the credit restriction in force.

| Case | What to show or do |
|---|---|
| `NoticeKind::PaidAfterFailed` | Show that a verified receipt arrived for an attempt previously shown as failed. |
| `NoticeKind::HeldWithoutPay` | Show that the open attempt needs a wallet with `paykit_pay`. |
| `NoticeKind::MessageGivenUp` | Show the message or request whose effect or delivery was given up. Acknowledgment can also retire the stored entry. |
| `NoticeKind::AllowancePlacementExpired` | Ordinary placement expired. The wallet still holds evidence of a possible Allowance conflict. Acknowledge the notice after showing it. The hold survives acknowledgment and restart. |
| `NoticeKind::AllowanceAcceptanceBeforeLoss` | Show that the identified acceptance cannot open a budget after the payer’s declared provider loss. The proposing wallet withdraws only its verified original pending write. An uncertain read or a later lifetime keeps that record intact. The seller ends its matching accepted Allowance when the withdrawal arrives. |
| `NoticeKind::AllowanceWithdrawalConfirmed` | The seller returned its signed end for the proposal and acceptance this wallet withdrew. The notice identifies that end. The original Allowance stays closed after acknowledgement and reopen. |
| `NoticeKind::PossiblyDelivered` | Reconcile the original message with its recipient using its ID and signed hash. Acknowledging the notice records that the host has seen it. It authorizes no send or replacement. |
| `NoticeKind::ListDeliveryUnknown` | The counterparty may not have this list. For a withdrawal it may keep paying the earlier list; otherwise it may lack the list. Acknowledging the notice authorizes nothing. `publish_list_to` for a chosen counterparty and `withdraw_list_from` for a withdrawn one report `Refusal::ListDeliveryUnknown`; a call that changes the list sends the new version instead. |
| `NoticeKind::BlockedMessagesLost` | Show the affected counterparty and acceptance-time range. Consult the payment method for payments that may have been among the lost messages. |
| `NoticeKind::SettlementBucketFull` | Show that the payee cannot add a mark to this bucket. A warning from a bucket read has no payer. Explain that issuing a receipt without a mark can let the merchant's other wallets credit the same settlement again. |
| `NoticeKind::SettlementMarksChanged` | Show the bucket and the own app that signed its current value. The wallet checks the current write again and restores the retained marks that fit. |
| `NoticeKind::SettlementCreditedTwice` | Show the conflicting occurrence marks. Acknowledging the notice records that the user saw it. Every retained mark continues to prevent another credit. |
| `NoticeKind::CreditNeedsUser` | Show the user which settlement or requested proof needs a credit decision. Acknowledging the warning leaves the credit decision open. |
| `NoticeKind::DroppedMessagesLost` | Show the counterparty and the acceptance-time range for which the wallet stopped requesting dropped replies. Check the payment method for any payment whose outcome is still unknown. Acknowledge the notice after showing it. With 64 distinct loss windows retained, an input that would add another waits for space. |
| `NoticeKind::BlockFloorHeld` | Tell the user that the wallet saw the identity's block floor diverge after a provider loss and holds it for good: no one-time request is paid automatically, each can still be paid by hand, and an unblock or a widening that needs the floor raised waits, with the unblocked sender's one-time requests and, after a restore that isn't clean or a missed item, every re-send request, unless the shared post-loss hold stands too (`NoticeKind::AutomaticPaymentsHeld`), when they go without the raise. Billing Periods and payments without a request go on. Shown once, and again after each reopen while the hold stands; acknowledge it after showing it. |
| `NoticeKind::FloorRaiseWaiting` | Tell the user that the wallet owes a block floor raise it can't write now, because the floor doesn't read or the switch refuses the write for now: until it is written, an unblocked sender's one-time requests wait and it is asked nothing, and while the raise of a restore that isn't clean or of a missed item waits, no one is asked to re-send. Payments by hand, Billing Periods and payments without a request go on. The wallet tries again at the next receive, and from its second unwritten try in a row after a wait of one minute, doubling to ten; the wait ends early when the floor record changes, the floor becomes readable or a new raise is owed. Shown once per load while it stands, and again after a reopen while the raise stays unwritten; the notice goes when the raise is written, or when the divergence hold stands (`NoticeKind::BlockFloorHeld`). Acknowledge it after showing it. |
| `NoticeKind::AutomaticPaymentsHeld` | Tell the user that, after a provider loss, no one-time request of the identity is paid automatically any more, on any of its wallets, and no payment of one is repeated at a resume, one the user started included; each can still be paid by hand. Billing Periods and payments without a request go on. Shown once, and again after each reopen; acknowledge it after showing it. |
| `PaykitError::Client` | Handle the enclosed Switch outcome. Preserve any kept batch and its ID. |
| `PaykitError::Lib` | Use the rule error to distinguish invalid input from invalid network or shared data. |
| `PaykitError::Rail` | Preserve any unresolved attempt unless the method established definitive refusal. |
| `PaykitError::PublicEndpointChangePending` | Keep the previously signed offer visible with a pending change. Resolve the reported signing refusal and check the resulting publication. |
| `PaykitError::Homeserver` | Retry discovery when the Homeserver is available. If the user is waiting, explain that discovery could not finish. |
| `PaykitError::Refused` | Follow the specific refusal below. |
| `PaykitError::Conflict` | Read the current state and decide again. |
| `RailError::Unavailable` | Preserve the open attempt. Submission may have happened. |
| `RailError::Refused` | Use only when the method establishes that this call cannot submit a payment. |
| `RailError::InvalidSelection` | Correct the rail implementation's endpoint selection. |
| `RailOutcome::Paid` | Supply the verified payment proof. |
| `RailOutcome::Failed` | Report only a definitive failure. |
| `RailOutcome::Pending` | Leave the attempt open while its result is unknown. |
| `PublicAppFailureReason::Homeserver` | Retry reading the affected app's Homeserver record. |
| `PublicAppFailureReason::Refused` | Exclude the unusable record. Its rule failure can help diagnose the lookup. |
| `PublicAppFailureReason::Expired` | Wait for a current record from that app. A public record reaches its limit at any finite signed deadline or at the maximum age in the payee's signed identity document (90 days by default). |
| `PublicAppFailureReason::OverBound` | This app's endpoints did not fit within the lookup bounds. Use the reason when explaining an incomplete lookup. |

## The payment method's time tolerance

The method declares `payment_time_tolerance_ms` with its declaration. The payee credits a request only on evidence dated at or after the request's `created_at` less this tolerance. Choose it to cover the method's time precision (at least 1,000 ms for times in whole seconds), the skew of the clock that dates the evidence, and the method's own timestamp rules. Two hours is a conservative early tolerance for a Bitcoin block timestamp. A block timestamp ahead of real time can also put a payment made in time past a deadline, and no tolerance covers that. A Lightning method that reads its own node's settle time needs a few minutes. The tolerance never widens a deadline. The time `verify` returns decides credit, so return the time the payment's evidence dates it, never a placeholder.

## Refusals

| Case | What to show or do |
|---|---|
| `Refusal::NotFound` | Refresh the request or record view. |
| `Refusal::ScopeMissing` | Obtain the required scope from the signer. |
| `Refusal::Label` | Select a selling label the app holds. |
| `Refusal::Rejected` | Show that the request, or the Allowance, has already been rejected. It is final. |
| `Refusal::HeldAfterRestore` | Show the exact hold and possibility of cancellation. Confirm only the hold shown. |
| `Refusal::Cancelled` | Show that the request has already been cancelled. |
| `Refusal::AllowanceEnded` | Show the stop and close actions on this Allowance. It is final: a scheduler stops paying under it and does not retry. |
| `Refusal::AllowanceConflicted` | Show the conflict and close actions on this Allowance. It is final: a scheduler stops paying under it and does not retry. |
| `Refusal::AllowanceConfirmationNeeded` | Automatic payment under the named Allowance waits for the user, after a missed item, or a restore until it is judged clean or the user confirms. The Allowance's `confirmation_needed` shows the same. Show its counterparty, terms and executor; once the user confirms, call `confirm_allowance_executor` with the refusal's `hold` (or the view's `hold`), the Allowance and the hold in force when it was shown. If that call answers `NotNow`, keep the confirmation with that `hold` and call it again later, unless the reason is the repair after a provider loss: the confirmation has landed, and calling again changes nothing. If it answers `AllowanceConfirmationNeeded` again, a newer restore, missed item or provider loss has set another hold: show the Allowance again and ask the user again. After a provider loss it also names an Allowance the user confirmed before the loss: confirming writes its grant again, but automatic payments under it don't resume in this version; the user who wants them ends the Allowance and creates a new one. |
| `Refusal::CreditHistoryLost` | Show that credit is held because the prior credit history is unavailable. |
| `Refusal::AlreadyPaid` | Show the existing paid state. |
| `Refusal::BeingHandledByOtherWallet` | Show that another own wallet holds the attempt. |
| `Refusal::MayBePaid` | Show the exact doubt. Carry it back only after the user confirms it and the SDK supplied a confirmable doubt. |
| `Refusal::LossChanged` | Display the current provider-loss doubt before asking again. |
| `Refusal::DeadlinePassed` | Show the expired deadline and stop the old attempt. |
| `Refusal::Recurring` | Use subscription and Billing Period APIs for this request. |
| `Refusal::MaybeCredited` | Show that the occurrence may already be credited. Wait for evidence or the supported explicit confirmation. |
| `Refusal::PaymentTimeOutside` | The payment method verified the payment, but at a time the request doesn't allow: before the request less the method's declared tolerance, or past its payment deadline or its destination's or rates' validity. Nothing is credited or receipted, and nothing is recorded: the request stays `ProofReceived`, and `verify_payment` or `confirm_payment` refuses the same way again. Show the funds as received and the request as unpaid, for the user to settle with the payer. |
| `Refusal::SettlementBucketFull` | Wait for the payee to decide whether to issue an unmarked receipt. Pass the explicit choice through `ReceiptDetails::with_unmarked_credit_confirmed`. |
| `Refusal::Subscribed` | Show that the recurring request is already accepted. |
| `Refusal::NotSubscribed` | Wait until the payer accepts the recurring request. |
| `Refusal::ManualOnly` | Not this automatic payer's to pay. An occurrence marked to be paid by hand only: no wallet's policy and no Allowance's executor pays it; leave it for the user. From `pay_period_automatically`, a Billing Period funded under an Allowance: leave it for that Allowance's executor (`pay_period_under_allowance`). From `pay_period_under_allowance`, a Billing Period funded under no Allowance: leave it for the wallet's own policy or the user. |
| `Refusal::NotAutomatic` | Show why this payment can't be made automatically: the window or dated subscription, or, under an Allowance, a request outside its terms, a lifetime limit used up with nothing reserved or to release, or an Allowance that expired after the payment was reserved and before its first dispatch. An Allowance set up before a provider loss and never granted here gets its own reason: it can't be granted now, so the user ends it and sets up a new one. After a provider loss, a one-time request is not paid automatically under any Allowance: the shared post-loss hold holds it for good (Paykit spec 8.1). It is final for automatic payment; the user may still pay by hand. |
| `Refusal::PeriodNotStarted` | Show when this Billing Period starts. If the payer chooses to pay early, call `pay_period` for that period with `PayOptions::with_early_payment_confirmed`. After a payer-provider loss, an older subscription may already have paid this period early. Keep it pending while that history is incomplete. |
| `Refusal::AttemptedPeriodsFull` | Wait for an existing period attempt to finish or reconcile its outcome before starting another. |
| `Refusal::Destination` | Show the failed destination check and refresh when the payee has a usable endpoint. When its text says the device clock is behind, the destination expired by the service's time: tell the user the device clock is behind. |
| `Refusal::AttemptFailed` | The attempt was recorded failed before dispatch. Show its enclosed refusal. |
| `Refusal::NoDestination` | Show why no endpoint could be used, including failed public app reads if available. When its text says the device clock is behind, a destination expired by the service's time: tell the user the device clock is behind. |
| `Refusal::UnsupportedEndpoint` | Ask the user to choose a compatible endpoint or payment method. |
| `Refusal::WaitingForList` | Show that a newer Private Payment List was requested. A public destination is a separate user choice. When `device_clock_behind` is set, a list expired by the service's time while the device clock is behind it: tell the user the device clock is behind. |
| `Refusal::ListDeliveryUnknown` | Show the counterparty, whether the list was a withdrawal and how long the earlier or current list may be paid. `withdraw_list_from` has already recorded the counterparty as withdrawn. Pass the same value to `send_list_again` only after the user confirms another version. `Ok` means nothing of unknown outcome is left. `NotNow` keeps the confirmation: a later call or receive sends the list once it can, which for a nonempty list to a blocked counterparty is after the block ends; `lists_waiting_to_send_again` lists the counterparties still waiting. |
| `Refusal::Transfer` | Show the actual transfer or fee quote and return that exact quote only on confirmation. The confirmation binds the fee shown when it is above the cap the quote was judged under, or when the method can't tell what it transfers; a quote judged under another cap is refused again with a new one. |
| `Refusal::PricedByRate` | Obtain authorization for this exact quote from the user or their bounded policy (Scheduling a Billing Period). |
| `Refusal::IdentityUnavailable` | Refresh the required identity before retrying. |
| `Refusal::IdentityUnconfirmed` | Ask for explicit confirmation before using a public destination without Switch confirmation. |
| `Refusal::PaymentInProgress` | Wait for the existing attempt before rejecting or cancelling. |
| `Refusal::NotCurrent` | Refresh authority and reconcile the existing attempt under the current provider and certificate. |
| `Refusal::FloorHeld` | The identity's block floor is held after it was seen divergent: the unblock or widening that needs a raise waits, and retrying changes nothing until the shared post-loss hold stands too, when it goes without the raise. Tell the user that requests can still be paid by hand. |
| `Refusal::NotNow` | Temporary: inspect account, clock and restore state, and try again later. Under an Allowance this includes a blocked counterparty until it is unblocked, a lifetime limit while an entry is reserved, a lifetime limit held by another app's failed attempt the user can release (`reserved_by_other_apps`, `release_allowance_attempt`), and, after a provider loss and the user's confirmation again, an Allowance this version doesn't resume, which waiting doesn't change: the user ends it and creates a new one. |
| `Refusal::Usage` | Correct the supplied API input, such as a payment under an Allowance asked of an app that isn't its executor. |
| `Refusal::RecordHeld` | A payer record with payment history lost to deletion permanently holds that obligation. For a temporarily unreadable record, resume when it can be read or repaired. |
| `Refusal::CertificateRefused` | Seek a suitable certificate from the signer and keep Paykit actions paused meanwhile. |
| `Refusal::SwitchUnavailable` | Show the own Switch as unavailable and keep payment actions waiting. From a payment, directly or as an `AttemptFailed` cause, it means nothing was paid, not a payment in flight: retry the payment once the switch answers. If the retry is refused with `PaymentInProgress`, call `Paykit::resume_payments` and retry once the earlier attempt is resolved; the resume records it failed when the switch takes that write. |
| `Refusal::Blocked` | Show the blocked recipient and the exact payment to the user before any supported override. |
| `Refusal::Divergent` | Show the returned records and the exact confirmation asked for. Run receive while the record still has writes waiting. |
