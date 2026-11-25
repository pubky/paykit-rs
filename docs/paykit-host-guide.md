# Paykit host guide

A wallet owns one `Paykit` handle for each approved App ID. Supply the same
durable Switch store, transport, clock and Homeserver access when reopening it.
Use the [Switch app host guide](https://github.com/pubky/pubky-switch/blob/d4f33242e2771a773e004b7fd2be0ebb09505c86/docs/app-host-guide.md)
for store guarantees, approval and client outcomes. Keep app keys and backups in
host-protected storage and serialize operations on the handle. A memory store
cannot preserve unanswered writes or payments across a restart.

Stored state and backups written by a build before the first release of v0.2 are
not carried across builds: a later build may refuse them, and nothing is upgraded.
From the first release on, every change to a stored format comes with a migration.

The integrator chooses whether to enroll an App ID per installation or share one
App ID and its keys. A separate App ID has its own signer approval and can be
revoked independently. Installations sharing an App ID have the same grants.
Synchronize receive keys, cursor, the public endpoint member and its deadline,
payment journal and backend payment history across those installations and restores. The wallet owns payment-method
idempotency and must prevent duplicate execution of an attempt.
For example, two wallets enrolled independently under the same payer identity
use distinct App IDs and can both retrieve the seller's receipt for the same payment.

> **Your payment method must limit fees.** Paykit configures, reports and checks no fee:
> the amount it pays is checked, but whatever fee your payment method adds on top is
> yours to bound. Set that limit in your payment method, most of all for payments made
> without the user present, such as automatic payments and an Allowance's: fees don't
> count against an Allowance's limits, and a payee can influence them through the
> destination it chooses.

## Open a wallet and pay a one-time request

Use `Paykit::join` with a signer-approved `JoinRequest` at enrollment. On an
ordinary startup, load the existing App ID. Use `Paykit::restore` with an
authorized `PaykitBackup` when recovering the app. That path validates the
Paykit layer and installs the payment journal with the client state. The
`*_with_protocols` constructors allow other protocols on the same App ID. Keep
the exact Paykit protocol instance supplied to the registration closure.

In Kotlin and Swift, pass `WalletScopes` to `prepareEnrollment`. Choose the
roles this app needs. A request scope covers every sale, while `requestLabels`
limits the app to the named labels. The SDK derives required scopes and Switch
permissions. Keep the same scope set and authentication request when resuming
a pending enrollment (Paykit spec 3.1, 3.2).

Pass an optional `WalletConfig` to `WalletHost` for join, load and restore.
Each omitted setting keeps the SDK default. Use `sellingLabel` for the label on
outgoing requests, `orphanAfterS` for the attempt's orphan age in seconds, and
`resendBudget` for the maximum number of messages re-sent to each requester per day.
Register handled message kinds in `extensionKinds` and ask for their prefixes
at enrollment. Public Payment Endpoints require the unlabelled request scope
and `publishesRecord` enabled (Paykit spec 3.3, 4, 7.2, 9, Switch spec 4.2).

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

Each receive also brings the app record up to date after its read. A receive
that tries that upkeep still does its other work, and sets
`ReceiveSummary::app_record_owed`, when the record can't be brought up to date
for one of these reasons:

- the Switch client refuses the update as one it can't sign, for example
  because the rail now pays a list of endpoint identifiers the record can't
  carry;
- the own switch doesn't answer;
- the record's copy on the Homeserver couldn't be written;
- the record's copy on the Homeserver is missing while the app runs on devices
  that share its keys: another device may have removed it to leave, so it is
  written back, and the reason clears, at the first receive that reads the
  feed to its end and so shows the app current;
- the switch has accepted no record of the app yet, as after a provider loss
  or a restore from a backup that carries none: the copy is written once it
  does;
- a newer change of the record is still settling, and the copy waits for it;
- a renewal is due and the record has an expiry policy this version doesn't
  know, which a newer version brings up to date.

A receive whose read stopped, or that returned early, skips the upkeep and
says nothing of these. Any other upkeep failure fails the receive. The record
is tried again at the next receive that tries the upkeep, and
`update_own_record` tries it at once and returns its outcome.

While the store couldn't tell whether the last write of the record was applied,
every receive sets `app_record_owed`: neither a receive's upkeep nor
`update_own_record` changes or renews the record until the app is loaded again
with `Paykit::load`. A delivery the client kept goes again at the start of its
next read, so one that keeps failing there fails that receive.

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

Every restore, in place or into a new store, installs the seen marks the backup
carries in place of the store's own (Switch spec 12.4 "Seen marks"). When some
were left out, the restore reports `AppEvent::SeenMarksIncomplete` once for each
registered protocol (`WalletEventData::SeenMarksIncomplete` in the bindings). It
is a notice, with nothing to acknowledge: from then on a record without a mark
is checked as new. A backup made before backups carried seen marks is refused.

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

A Kotlin or Swift wallet can return no entry from `messageDelivery` after
delivery retention expires or after a restore. A retry can be refused after
an earlier try was accepted and its answer was lost. When delivery is
`Failed`, check the request or record before sending a replacement (Switch
spec 11, 12.4).

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
a repair this version doesn't make. They answer `HeldForUser` with
`AllowanceNotResumed` (code `allowance_not_resumed`); don't
offer confirming as the way to resume, and don't ask the user to confirm again.
When the regrant's reply is lost, calling `confirm_allowance_executor` again lands
the kept regrant, and `resume_allowance_executor` then answers `AllowanceNotResumed`
too: stop retrying, since no wait clears it.
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
The protocol lets the user move a subscription's future Billing Periods to another accepted
Allowance (Paykit spec 8.3); the move never changes past usage, attempts already made,
reservations or obligations in flight. This version offers no call to make the move yet:
accepting the subscription again under the new Allowance answers `Refusal::Subscribed` and
moves nothing. Until then, after the Allowance a subscription was accepted under ends, its
later periods answer `Refusal::AllowanceEnded` and the host pays them by hand
(`pay_period`); a new Allowance pays the subscriptions accepted under it.
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
month's quote. The SDK checks the signed source, amount and payment method again. The
SDK makes no fee check: your payment method bounds the fee. For a payment by hand, pass the
authorized quote through `PayOptions::with_rate_authorized`.

The SDK checks the automatic payment window, the request and period records, and the
usual payment restrictions again before dispatch. A missed period remains available for
`pay_period` when its other payment conditions hold.

## Blocked counterparties

Blocking pauses what is open with a counterparty. It ends nothing (Paykit spec 8.1).
When the user blocks a counterparty with `block`, tell them that their open
subscriptions and Allowances with it are paused, not ended: while the block holds, no
Billing Period is paid automatically and no payment is made under an Allowance. To end
one, call `cancel_request` for a subscription the user pays, `cancel_sent_request` for a
request the user's shop sent, or `end_allowance` for an Allowance. List them with
`requests`, where a request from a blocked payee shows `RequestStatus::Blocked`, with
`payee_activity` for the requests the shop sent, and with `allowances`.

When the user unblocks a counterparty with `unblock`, show the subscriptions and
Allowances with it that resume paying automatically, from the same calls, so the user
can end any they no longer want before its next period is due. An unblocked subscription
pays its later periods automatically; cancelling or ending it is the explicit stop.
Messages held while the counterparty was blocked go once it is unblocked, unless they
are more than 30 days old. Those are given up and never sent from this installation, and
`NoticeKind::MessageGivenUp` names each one an activity record holds. The notice says
only that this installation never saw the message accepted: after a restore, the device
the backup came from may have delivered it.

## What the host drives

### Sending queued messages

The outbox is the host's to drive. `Paykit::flush` (`NativeWallet::flush` in the wallet bindings) makes one bounded delivery try: it reaches a few queued messages within a time budget, and can return `Ok` with messages still queued. `receive` retries some messages, such as the batches it keeps and a queued Private Payment List, but it doesn't drive the outbox. Call `flush` on a schedule of the host's choice, for example after each `receive`, when the app comes to the foreground, when it reconnects, and on a timer.

A request the recipient's switch refused because the sender isn't admitted yet stays queued, and is sent again when a delivery try reaches it after the recipient admits the sender. `Paykit::message_delivery` (`NativeWallet::message_delivery`) tells where a message stands: kept in a batch, queued with its tries, waiting for admission, waiting for a current certificate, accepted by the recipient's inbox, or failed. `None` means the client keeps no delivery status for that message ID; the request's activity may still hold evidence, so look at it before sending anything in the message's place.

### Crediting a payment exactly once

Which payment-method output a proof names is the integrator's to check: Paykit names no payment method and checks no method's outputs. The wallet's `Rail::verify` checks the evidence and returns the time it dates the payment. `Rail::settlement_id` returns the identifier of the settlement the proof pays. For example, a Bitcoin method could check the transaction and output a proof names, and return them as the settlement ID.

`Paykit::confirm_payment` credits a payment by storing its signed receipt and the settlement's mark in one batch. The mark is shared by the payee's own apps: once a settlement's mark is visible, no other credit of it is made, whether for another request, a Billing Period or a payment without a request. A repeat for the same occurrence returns its existing receipt. A provider loss can hide an earlier mark from the app deciding. When the SDK can't tell whether the settlement was already credited, it refuses with `Refusal::MaybeCredited`; when the settlement's bucket is full, or the Switch refuses the bucket for its quota or size, with `Refusal::SettlementBucketFull`. Show the risk, and retry with `Paykit::confirm_payment_with` only on the user's explicit confirmation: `with_maybe_credited_confirmed` for the first, `with_unmarked_credit_confirmed` for the second. The first is still refused when no fresh bucket version was read, or when a bucket seen before is gone with no provider loss to explain it. A credit without a mark can duplicate a credit made elsewhere.

### Sharing a Private Payment List from several selling apps

A selling app that publishes a Private Payment List keeps a counter for each recipient and generation of its own identity. Another app holding the unlabelled selling scope (`paykit_request`) can write that counter. When `publish_list_to` or `withdraw_list_from` has to reserve a new list version and the counter holds another app's write, the call is refused with `HeldForUser`, a withdrawal included: with `PublicationConflict` (code `publication_conflict`) when that write verifies, and with `Unresolved` (code `held_unresolved`) while the counter can't be verified, as when the Switch client flagged a write to it. In Rust the conflict's text names the app that wrote the counter; a binding host gets the code only. A flagged write is reported first as `AppEvent::WriteFlagged` (`WalletEventData::WriteFlagged` in the bindings), which names the app that wrote it: look at that event before revoking anything. With another app's verified write on the counter, retries give the same refusal throughout the same generation; an unverified counter may clear on a later read. The refusal holds publication to that one recipient; other recipients are not affected. To stop that recipient paying this app's list now, revoke this selling app in the signer and enroll it again. That ends this app's lists for every recipient, not only this one, once they read the payee's current identity document, at most 10 minutes later. Otherwise the list lapses at its signed expiry or 30 days after the recipient accepted it. Revoke the app that made the foreign write too.

### Receive events the host acts on

`Paykit::receive` returns the events of its read in `ReceiveSummary::report.events`; `NativeWallet::receive` returns them in the report's `events`. Handle them in order. Among them, three need the host:

- `AppEvent::ProtocolDeferred`: a prerequisite was missing for this input, such as an Allowance record that doesn't read yet. The input stays unprocessed. Make the prerequisite available, then call `receive` again.
- `AppEvent::StoreFailing`: the host's store keeps failing for an input. The client keeps the input; repair or free storage, then call `receive` again.
- `AppEvent::SeenMarksIncomplete`: after a restore, the marks of records this app had seen were missing, unreadable or too large to keep, so a record it saw before is now checked as new. Show or log it.

These three are not kept after the call returns. A crash before the host handles them, or a Paykit error after the read, can lose them; `SeenMarksIncomplete` is held in memory from the restore until the first `receive`, so a crash before that first `receive` loses it too. Duty events are kept until acknowledged and can be read again with `unacknowledged_events`.

### Withdrawing public endpoints and paying offline

To stop offering public endpoints, call `set_public_endpoints` with an empty set while publication is on, and check the result. `Published` means the Homeserver holds the record without them. For any other result, check that the withdrawal is visible before going further. Turn `publishes_record` off only if the app should stop publishing its record on the Homeserver altogether; it still gives its app record to the Switch. Until the withdrawal is visible, the old endpoints expire, or the app is revoked, a payer can still pay the endpoints it read before.

With the own Switch unreachable, `pay_without_request` can still pay a reusable public endpoint the user picked explicitly, with `PaymentMode::Public` and the endpoint named in the payment's options. It needs the app's own identity document confirmed within the last 60 seconds, a copy of the deny list read within the last 10 minutes, and the payee's identity checked. Once the switch is back, call `resume_payments`: it records the payment the method made offline, and never sends that payment again.

### Reply grants and push

The host manages the identity's reply grants through the Switch App (`Paykit::app_mut`). `App::allow_list` lists the peers, each grant's origin, when it was given and when the peer last sent through it, so check the app's own outgoing activity separately; `App::remove_reply` removes a grant and keeps a peer the user keeps for another reason. Pruning is the host's policy: for example, check once a day and remove a grant only after it has been idle in both directions for 180 days, never for a peer the user keeps or an exchange that is unfinished or uncertain.

Push is registered through the Switch App: `App::register_push` with the relay URL and device token, also to change relays, and `App::reregister_push` to retry the saved settings after an interruption or a provider change. A push only wakes the app: it can arrive twice or not at all. Read the feed with `receive` for the payment's state; never take it from the push.

## After a restore or a provider loss

### Restoring a backup

Before restoring, ask who has the device the backup came from (`WalletHost::restore` takes the answer). If it may be lost or stolen, revoke its app and enroll a new one instead.

After `Paykit::restore`, call `receive` before using the Switch App directly; Paykit's own outgoing calls make that first read themselves. `App::restore_held_cursor` gives the first read's verdict once a page settles it: `Clean` releases this restore's hold, though the hold of an earlier restore that wasn't clean, or of a missed feed item, can remain, and a later page found `Ahead` ends the verdict; `Ahead` and `Unknown` keep open requests on hold until the user confirms each, or evidence shows it ended. Run `reconcile` and show its report: each outcome it resolved, the obligations in doubt, and whether payer obligations were skipped because the certificate lacks `paykit_pay`. After the user has seen it, call `end_reconcile` (`endConfirmedReconciliation` in the wallet bindings); an open restore question must be answered first. When payer obligations were skipped, payments still wait for a reconcile by an app holding `paykit_pay`.

If the switch saw receive-key registrations the backup can't explain, `App::restore_question` lists them. Show them, and pass the exact list to `confirm_restored_registrations` when the user recognizes them, or to `refuse_restored_registrations`, which raises the compromise alarm.

A payment refused with `Refusal::HeldAfterRestore` is for a request the payee may have cancelled. Show the hold; `confirm_possibly_cancelled` with that exact hold releases it on the user's confirmation.

### Possible payment before a provider loss

After the identity's provider is lost, a one-time request that was payable before the loss may have been paid by another own app at the old provider. An empty claim at the new provider doesn't show it wasn't.

- A payment by hand on such a request is refused `Refusal::MayBePaid`, its doubt showing the loss (`before_loss`). Show the user that the request may already have been paid. Only on the user's consent, pass that same doubt back (`PayOptions::with_may_be_paid_confirmed`) for one attempt. That consent records nothing and lifts nothing else; an attempt that fails leaves the doubt as it was.
- `confirm_not_paid_before_loss` records the user's statement, through this wallet, that the request was not paid before that loss. Before calling it, account for what Paykit can't see: every original app of the identity, through a verified report or a signed removal (`recovery_members`, which needs the recovery scope, lists the original and removed apps and the reports it could verify; an original can have neither), and every funding account and submission that could have paid it. A report from one installation, an empty claim at the new provider, or a reminder from the payee is not that accounting.
- Nothing the user states releases an automatic payment, or a repeat of an earlier payment on resume. A Billing Period of a subscription accepted before the loss stays held while its payment history is incomplete.

### Records shown divergent

A shared record the switch shows divergent pauses the payments, credits and ordinary writes that depend on it; a cancellation or a rejection can still be written. `divergent_records` lists them: show each, and pass each observation the user confirms to `confirm_divergent`. While `writes_waiting` is set, keep calling `receive`. For a credit, confirm the observation the credit's own refusal returns. Automatic payment waits until the record is no longer divergent. A divergent block floor holds for good: `confirm_divergent` can't release it.

### Replacing the signer

The wallet that holds the identity key can restore the signer on a new installation with `Signer::restore_new`, on the user's confirmation, from the identity key in the new installation's keyring and the signer's encrypted backup on the Homeserver; exporting the key and importing it there are the wallet's. Once the old installation sees its successor, `Signer::status` reports `ManagedBy` with the new installation: show which installation signs now, and stop offering signer actions on the old one.

## Notices and errors

Read `notices` and `payee_deletion_notices` by their stable identities. Acknowledge
each through its matching SDK API after recording or showing it.

Notices through a restore. Identity notices (`BlockFloorHeld`, `FloorRaiseWaiting`,
`AutomaticPaymentsHeld`) aren't restored: the restored wallet tells them again while
they hold, at its load or a later receive. A restore onto a fresh store brings back the
backup's notices. A restore in place, over the installation that kept running after the
backup, keeps, kind by kind:
- `PaidAfterFailed`, `HeldWithoutPay` and the Allowance notices
  (`AllowancePlacementExpired`, `AllowanceAcceptanceBeforeLoss`,
  `AllowanceWithdrawalConfirmed`): the installation's where it holds one, else the
  backup's, so one raised after the backup isn't lost;
- the settlement notices, `PossiblyDelivered`, `ListDeliveryUnknown` and
  `DroppedMessagesLost`: both sides', merged;
- `MessageGivenUp` and `BlockedMessagesLost`: the backup's, except that a held stop the
  installation told keeps its notice. One raised after the backup is lost.

A notice the host acknowledged after the backup, which acknowledging deleted, can be
listed again after the restore: the same occurrence comes back with the same `id` and
`raised_at_ms`, and the host may skip one it recorded acknowledging with both. A notice
raised again is a new occurrence with the same `id` and a later `raised_at_ms`, such as a
`HeldWithoutPay` whose hold started again, or an identity notice told again while its
hold stands: show it as the table below says. Handle Switch
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
that successful report. The SDK acknowledges each duty it takes by itself, so a duty left to the host
never holds back one the SDK took. Show the host's duties, such as
`AppEvent::ListsUnverified` after a restore or a provider loss (kept peers or
reply grants not verified yet, or lists the backup left out), and acknowledge
each once the user has seen it. Use the app-wide `App::ack_events` only after the
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
| `PaykitError::SettlementNoticeUnavailable` | Show the bucket warning from this error if the host has yet to show it. The store could not save or check the notice. Track the original marked receipt through `kept_batch()`. The service code is available through `code()`. The existing batch may still apply and queue its original receipt. |
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

## The payment method's proof

A method reports a payment made with its proof, a JSON object the payer's app signs into the `paykit.payment_proof` message the payee receives (Paykit wire 4.10). The wallet accepts any proof that reads as an object, because the money has already moved: refusing it would leave a paid payment open for good. Keep a proof inside the limits that message has:
- Its whole message, proof included, is at most 16 KiB, and a proof that takes it over leaves the payment paid with its outcome unwritten.
- Keep its nesting shallow, well under 100 levels. The wallet's journal holds the proof a few levels down: a proof nested close to the decoder's limit of 128 is written, then can't be read back, and the wallet's payment list fails until the entry is gone.
- Paykit decodes the proof before it signs it, so a number can lose its spelling, and a large one its value: `1.50` is signed as `1.5`, and an integer above 2^64 changes value, for example 18446744073709551617 is signed as `1.8446744073709552e19`, which is 2^64. Write any number that must survive exactly as a string, for example `{"fee":"1.50"}`.
- Give each member once: a repeated member keeps only its last value.

## The payment method's settlement ID

`settlement_id` names the settlement a proof pays, for the payee's duplicate checks. Return `None` only for a proof that names no settlement: the SDK can't tell `None` from "can't tell now". A proof of a payment that names no request is then dropped for good: the feed moves past it and a resend of it is dropped as already processed, so its credit is lost. A requested proof skips the duplicate checks and is credited only on the user's confirmation (`Refusal::MaybeCredited`). The wallet bindings read a thrown `settlementId` as `None`, with the same effects, so a host method never throws it for a fault that may pass.

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
| `Refusal::AllowanceConfirmationNeeded` | Automatic payment under the named Allowance waits for the user, after a missed item, or a restore until it is judged clean or the user confirms. The Allowance's `confirmation_needed` shows the same. Show the counterparty, terms and executor. In Rust, confirm with the refusal's `hold` or the displayed view's `hold`. A binding host gets only the `record` in the refusal. Read `allowance(record)` for the view to show, then pass that view's `hold` to `confirm_allowance_executor(hold, true)` once the user confirms. If that call answers `NotNow`, keep the confirmation with that `hold` and call it again later. If it answers `AllowanceConfirmationNeeded` again, a newer restore, missed item or provider loss has set another hold: show the Allowance again and ask the user again. After a provider loss it also names an Allowance the user confirmed before the loss: confirming writes its grant again, but automatic payments under it don't resume in this version. A Billing Period paid under it and `resume_allowance_executor` then answer `HeldForUser` with `AllowanceNotResumed` (code `allowance_not_resumed`), as confirming again can, and no retry clears it: offer the user to end the Allowance and set up a new one; it can still be paid by hand. |
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
| `Refusal::WaitingForList` | Show that a newer Private Payment List was requested. A public destination is a separate user choice. When `device_clock_behind` is set, a list expired by the service's time while the device clock is behind it: tell the user the device clock is behind. A list in `held_lists()` waits because its record was deleted, doesn't read, or is absent: the SDK never creates a held list's record over an absent key, since an absence can be a delete the service forgot. After a delete, or over an absent key, it can wait indefinitely; the payee's public endpoints or a request's own destination remain the way to pay. |
| `Refusal::ListDeliveryUnknown` | Show the counterparty, whether the list was a withdrawal and how long the earlier or current list may be paid. `withdraw_list_from` has already recorded the counterparty as withdrawn. Pass the same value to `send_list_again` only after the user confirms another version. `Ok` means nothing of unknown outcome is left. A refusal for now or `HeldForUser` with `CounterpartyBlocked` keeps the confirmation: a later call or receive sends the list once it can, which for a nonempty list to a blocked counterparty is after the block ends; `lists_waiting_to_send_again` lists the counterparties still waiting. |
| `Refusal::Transfer` | Show the actual transfer quote and return that exact quote only on confirmation; a quote that changed since is refused again with a new one, unless the transfer now passes the check on its own. |
| `Refusal::PricedByRate` | Obtain authorization for this exact quote from the user or their bounded policy (Scheduling a Billing Period). |
| `Refusal::IdentityUnavailable` | Refresh the required identity before retrying. |
| `Refusal::IdentityUnconfirmed` | Ask for explicit confirmation before using a public destination without Switch confirmation. |
| `Refusal::PaymentInProgress` | Wait for the existing attempt before rejecting or cancelling. |
| `Refusal::NotCurrent` | Refresh authority and reconcile the existing attempt under the current provider and certificate. |
| `Refusal::FloorHeld` | The identity's block floor is held after it was seen divergent: the unblock or widening that needs a raise waits, and retrying changes nothing until the shared post-loss hold stands too, when it goes without the raise. Tell the user that requests can still be paid by hand. |
| `Refusal::NotNow` | Wait as its `NotNowReason` names, then call again: a later call, a read or the signer coming online clears it. |
| `Refusal::HeldForUser` | Show the counterparty, request or record its `HeldForUserReason` names: the user unblocks it, confirms the paused record or pays by hand, or it waits for the record's repair or the loss steps. It holds that one item, not the wallet. |
| `Refusal::RecoveryNeeded` | Run the wallet's recovery step its `RecoveryReason` names: `reconcile` for the payer pass owed since a restore. A missing `paykit_recovery` scope comes as `ScopeMissing`, and its key not stored yet as `NotNow` with `ScopeKeysAwaited`. |
| `Refusal::Usage` | Correct the supplied API input, such as a payment under an Allowance asked of an app that isn't its executor. |
| `Refusal::RecordHeld` | A payer record with payment history lost to deletion permanently holds that obligation. For a temporarily unreadable record, resume when it can be read or repaired. |
| `Refusal::CertificateRefused` | Seek a suitable certificate from the signer and keep Paykit actions paused meanwhile. |
| `Refusal::SwitchUnavailable` | Show the own Switch as unavailable and keep payment actions waiting. From a payment, directly or as an `AttemptFailed` cause, it means nothing was paid, not a payment in flight: retry the payment once the switch answers. If the retry is refused with `PaymentInProgress`, call `Paykit::resume_payments` and retry once the earlier attempt is resolved; the resume records it failed when the switch takes that write. |
| `Refusal::Blocked` | Show the blocked recipient and the exact payment to the user before any supported override. |
| `Refusal::Divergent` | Show the returned records and the exact confirmation asked for. Run receive while the record still has writes waiting. A credit refused this way is released only by confirming, with `confirm_divergent`, the records this refusal returns (`may_hold_credit`): a record confirmed as `divergent_records()` lists it doesn't count for a credit, and one still owed that confirmation may be missing from that list. Then ask for the credit again. |

## Wallet errors

The Kotlin and Swift wallets report a failed call as one of six cases, each carrying a `WalletFailure` with a stable `code`. The enrollment, restore and store failures keep their own cases (`InvalidInput`, `NotJoined`, `RestoreQuestionOpen`, `Storage` and the enrollment cases) and are not in this table. A `Storage` failure stops the wallet as a whole. A payment the call started may already have been made by the method, so don't pay again: fix the store, then call `resumePayments`, which reports it.

| Case | What it means |
|---|---|
| `NeedsUser` | The user must see it and decide. Only `held_after_restore`, `blocked`, `may_be_paid`, `transfer` and `priced_by_rate` are confirmed by passing the same `failure.refusal` back in `confirmedRefusals`, and an `attempt_failed` with one of them as its cause by passing `attemptedFailure()`; every other code names its own call, or says that no call of these bindings answers it yet. |
| `RetryLater` | Waiting, or a later call such as `receive`, changes it with no user action. Wait `retryAfterMs` when it is set. |
| `OutcomeUnknown` | The call may have taken effect. Reconcile before trying again. |
| `NotSent` | No payment was made and no message went out. Fix the cause, then make the call again. An attempt's own writes may have happened: a failed attempt is stored closed, and a single-use endpoint it used is left out of a retry. |
| `Permanent` | Final: trying again gives the same answer. Never confirmed. |
| `Recovery` | The wallet as a whole is stopped: an alarm, a recovery notice, its certificate or its authority. Stop Paykit actions and follow the recovery steps. A hold on one record is never `Recovery`. |

Three codes are provisional: `usage`, `not_automatic` and `client_not_now`. The SDK and the Switch client give these refusals a text reason only, and one variant covers reasons of different cases, from a wait to an alarm. Until each carries a typed reason, they take the case that never retries by itself and never pays twice. `client_not_now` comes as `NeedsUser`, `usage` as `NotSent` and `not_automatic` as `Permanent`. Every reason of a refusal kind comes as the same case, so a host may branch on the kind alone. Their codes stay, and the typed reasons will add codes.

A refusal of the Switch client for one entry, or the wait for the wallet's own `identity.json`, leaves that entry for the next resume and comes in `resumePayments`' result as its outcome: `PaidOutcomeUnwritten` when the method reported it paid, `Pending` otherwise. Any other failure of an entry, such as the network or a method that can't tell now, ends the call with its error, and the entries after it wait for the next resume. The other entries go on, and every outcome they reached is in the result; `payments()` shows the entries the wallet still keeps, as a finished one may be pruned in the same resume. An owed acceptance that can't be placed now stays owed for the next resume; its outcome is written already, so it reports none. A wallet-wide stop ends the call at once, wherever a step meets it, with nothing written or pruned after it: an alarm, a store failure, an app that stopped, left, is gone, is no longer current or whose certificate expired, a recovery notice the app holds or the switch's answer that the account awaits a recovery, or an open restore question. The work before the entries (the first read) and after them (rechecking failed attempts, the Allowance ledger, the outbox, pruning) can fail the call with its own error too; handle it by its code. A call that returns a stop carries no outcome, so a payment it made or finished is not in its answer. `payments()` lists every payment the wallet keeps with its stage and `paidByMethod`, which tells a payment the method made, its outcome written or not, from a failed one. After a store failure, a payment may already have been made by the method, even when a pay call returned the failure: don't pay again. Fix the store, read `payments()`, then call `resumePayments`, which finishes every payment still open and reports its outcome. After the switch's answer that the app is no longer current, a payment may likewise have been made already: `payments()` lists it at `RailPaid`, paid by the method with its outcome unwritten. The app writes nothing more (Paykit spec 7.3), and it must be approved again before it can report that payment or pay.

Every reason of `NotNow` is `RetryLater`, of `HeldForUser` `NeedsUser`, and of `RecoveryNeeded` `Recovery`. `attempt_failed` takes its cause's case, whatever it is: read `attemptedFailure()` for the cause. `waiting_for_list` is `NeedsUser` when the device clock is behind.

Two rules override a code's case. A failure with a `keptBatch` comes as `OutcomeUnknown`, unless its case is `Recovery`: the wallet sends that batch again by itself, and the outcome shows once it is answered. The one exception is `settlement_warning_unsaved`, which comes as `NeedsUser` with its `keptBatch`: the user is shown the bucket's warning while the kept batch may still apply. A pay that may have been sent never comes as an error: it comes as a pending or uncertain outcome. The one exception is a wallet-wide stop met after the call, such as the store failing or an alarm. It comes as that stop, and the journal keeps the payment, which the resume after the recovery settles from the method's history.

A failure that comes once the request may have left the device comes as `OutcomeUnknown`, also without a `keptBatch`: a timeout, an answer whose signature doesn't verify, an answer too large to take, one the service gave before reading the request, and one that doesn't read or verify. The failure doesn't say which request it was, and some requests the Switch client sends outside a kept batch are safe to repeat only while the first is outstanding: a message the service no longer holds is appended again, and a late connect request can reopen one that was declined. Only a failure that sent nothing comes as a case that calls again.

| Code | From | Case | What the host does |
|---|---|---|---|
| `held_after_restore` | `Refusal::HeldAfterRestore` | `NeedsUser` | Show the hold and that the request may be cancelled; on the user's yes pass the refusal back. |
| `blocked` | `Refusal::Blocked` | `NeedsUser` | Show the blocked payee and the exact payment; on the user's yes pass the refusal back. |
| `may_be_paid` | `Refusal::MayBePaid` with a doubt | `NeedsUser` | Show the doubt; on the user's yes pass the refusal back. |
| `may_be_paid_reconcile` | `Refusal::MayBePaid` without a doubt | `NeedsUser` | Show that an earlier payment may be paid; `reconcile` and answer its doubtful payments with `resolveConfirmedAttempt`, passing each attempt's handle, then pay again. |
| `transfer` | `Refusal::Transfer` | `NeedsUser` | Show the amount asked beside the amount and asset the payment method would transfer, or that it can't tell; on the user's yes pass the refusal back. |
| `priced_by_rate` | `Refusal::PricedByRate` | `NeedsUser` | Get the user's authorization for this quote; then pass the refusal back. |
| `loss_changed` | `Refusal::LossChanged` | `NeedsUser` | Show the current provider-loss boundary and ask again with it. |
| `list_delivery_unknown` | `Refusal::ListDeliveryUnknown` | `NeedsUser` | Show the counterparty and list; call `sendListAgain` only after the user confirms. |
| `divergent` | `Refusal::Divergent` | `NeedsUser` | Show the records; `confirmDivergent` with the observation shown, then `receive`. |
| `maybe_credited` | `Refusal::MaybeCredited` | `NeedsUser` | Show that the occurrence may already be credited; credit only on the user's explicit confirmation. |
| `settlement_bucket_full` | `Refusal::SettlementBucketFull` | `NeedsUser` | Ask the payee whether to issue a receipt without a mark. |
| `period_not_started` | `Refusal::PeriodNotStarted` | `NeedsUser` | Show when the period starts; pay early only on the user's choice. |
| `identity_unconfirmed` | `Refusal::IdentityUnconfirmed` | `NeedsUser` | Ask before paying a payee whose switch couldn't confirm its apps; then set `unconfirmedIdentityConfirmed`. |
| `scope_missing` | `Refusal::ScopeMissing` | `NeedsUser` | Ask the user to approve the missing scope in their signer. |
| `not_subscribed` | `Refusal::NotSubscribed` | `NeedsUser` | The recurring request isn't accepted: the payer accepts it, and the payee waits for that. |
| `floor_held` | `Refusal::FloorHeld` | `NeedsUser` | The identity's block floor is held after it was seen divergent: the unblock or widening that needs a raise waits, and retrying changes nothing until the shared post-loss hold stands too. Tell the user that each request can still be paid by hand. |
| `allowance_confirmation_needed` | `Refusal::AllowanceConfirmationNeeded` | `NeedsUser` | Show the view from `allowance(record)`. Once the user confirms, pass its `hold` to `confirm_allowance_executor(hold, true)`. |
| `waiting_for_list` | `Refusal::WaitingForList` | `RetryLater` | A newer Private Payment List was asked for: pay once it arrives. Paying a public destination is a separate choice. |
| `waiting_for_list_clock_behind` | `Refusal::WaitingForList` with `device_clock_behind` | `NeedsUser` | The payee's list expired by the service's time while the device clock is behind it: tell the user to correct the clock; a newer list from the payee also clears it. Paying a public destination is a separate choice. |
| `payment_in_progress` | `Refusal::PaymentInProgress` | `RetryLater` | Wait for the open attempt, or `resumePayments`, then call again. |
| `being_handled_by_other_wallet` | `Refusal::BeingHandledByOtherWallet` | `RetryLater` | Another own wallet holds the attempt; read again later. |
| `identity_unavailable` | `Refusal::IdentityUnavailable` | `RetryLater` | `receive` refreshes the identity; call again after it. |
| `attempted_periods_full` | `Refusal::AttemptedPeriodsFull` | `RetryLater` | Wait for an open period attempt to finish. |
| `no_destination` | `Refusal::NoDestination` | `NotSent` | No endpoint could be used; the payment result's public failures name the payee apps that couldn't be read. Pay again when the payee has one, or choose another destination. |
| `unsupported_endpoint` | `Refusal::UnsupportedEndpoint` | `NotSent` | Choose an endpoint or method that pays it. |
| `destination` | `Refusal::Destination` | `NotSent` | The destination failed its check before anything was written. Pay again when the payee has a usable endpoint for this request; a request whose destination names an app that is gone needs a new request from the payee. |
| `label` | `Refusal::Label` | `NotSent` | Send under a selling label the app holds. |
| `recurring` | `Refusal::Recurring` | `NotSent` | Pay a recurring request by its Billing Periods. |
| `attempt_failed` | `Refusal::AttemptFailed` | as its cause | The attempt was written and failed before money moved, and nothing was paid. It comes as the case of `attemptedFailure()`, and follows that refusal's row; a confirmation passes `attemptedFailure()` back. |
| `not_found` | `Refusal::NotFound` | `Permanent` | Refresh the view. |
| `rejected` | `Refusal::Rejected` | `Permanent` | Show that the request is rejected. |
| `cancelled` | `Refusal::Cancelled` | `Permanent` | Show that the request is cancelled. |
| `allowance_ended` | `Refusal::AllowanceEnded` | `Permanent` | Show the Allowance's stop and close actions. |
| `allowance_conflicted` | `Refusal::AllowanceConflicted` | `Permanent` | Show the conflict and the Allowance's close actions. |
| `credit_history_lost` | `Refusal::CreditHistoryLost` | `Permanent` | Show that credit is held because the earlier credit history is lost. |
| `already_paid` | `Refusal::AlreadyPaid` | `Permanent` | Show the paid state. |
| `deadline_passed` | `Refusal::DeadlinePassed` | `Permanent` | Show the expired deadline. |
| `payment_time_outside` | `Refusal::PaymentTimeOutside` | `Permanent` | Show the funds as received and the request as unpaid, to settle with the payer. |
| `subscribed` | `Refusal::Subscribed` | `Permanent` | The recurring request is already accepted; only cancelling is left. |
| `manual_only` | `Refusal::ManualOnly` | `Permanent` | Final for this automatic payer: the period is marked manual-only, or is funded under an Allowance its executor pays. The user can still pay it by hand. |
| `unknown` | a refusal the bindings don't recognize | `Permanent` | Show it; never retry or confirm it. |
| `not_current` | `Refusal::NotCurrent` | `Recovery` | Refresh authority and reconcile the attempt under the current provider and certificate. |
| `record_held` | `Refusal::RecordHeld` | `NeedsUser` | One request's payer record is held: it doesn't read, or can't be rebuilt. Show it for that request; other requests go on, and nothing retries it. It goes on once the record reads again; having it written again needs a call these bindings don't offer yet. A record whose payment history was lost to a delete holds that obligation for good, and the refusal doesn't yet say which of the two it is. |
| `certificate_refused` | `Refusal::CertificateRefused`, `AppRefusal::Certificate` | `Recovery` | Get a suitable certificate from the signer; keep Paykit actions paused. |
| `records_unlisted` | `NotNowReason::RecordsUnlisted` | `RetryLater` | Make the call `retry` names again; it continues the listing. |
| `records_not_listed` | `NotNowReason::RecordsNotListed` | `RetryLater` | `receive`, which lists the shared records, then call again. |
| `scope_keys_awaited` | `NotNowReason::ScopeKeysAwaited` | `RetryLater` | The signer stores the certificate's keys when it is next online; tell the user if it stays. |
| `value_awaits_key` | `NotNowReason::ValueAwaitsKey` | `RetryLater` | The signer stores the record's keys when it is next online; call again then. |
| `floor_unreadable` | `NotNowReason::FloorUnreadable` | `RetryLater` | The block floor can't be read now: the payer's records not listed yet, a key awaited, a kept floor write, or the switch not answering. A later `receive` clears it with no one acting. |
| `floor_record_unreadable` | `HeldForUserReason::FloorRecordUnreadable` | `NeedsUser` | The block floor record doesn't read, and no later read clears it. This version has no call that repairs it: it reads again only once another of the user's wallets that holds a readable floor writes it. Until then an unblock is refused and the counterparty stays blocked, a block still takes effect, and one-time requests are paid by hand. Show that. |
| `floor_raise_waiting` | `NotNowReason::FloorRaiseWaiting` | `RetryLater` | A block floor raise left unwritten waits before it is tried again: call again later. |
| `no_service_time` | `NotNowReason::NoServiceTime` | `RetryLater` | `receive`, which verifies a service time, then call again. |
| `read_again` | `NotNowReason::ReadAgain` | `RetryLater` | What the call relies on isn't verified or changed: `receive`, then call again. |
| `earlier_write_unresolved` | `NotNowReason::EarlierWriteUnresolved` | `RetryLater` | Call again once the earlier write is answered, after `receive`. |
| `pending_proposals_full` | `HeldForUserReason::PendingProposalsFull` | `NeedsUser` | Eight of this wallet's Allowance proposals to the counterparty are pending: show them; one answered makes room, and a conflicted one counts until the user discards or withdraws it. |
| `allowance_ledger_full` | `HeldForUserReason::AllowanceLedgerFull` | `NeedsUser` | The Allowance ledger has no room for this payment: show it; earlier payments' outcomes resolving may make room, or not. The user may pay by hand. |
| `call_again` | `NotNowReason::CallAgain` | `RetryLater` | Make the same call again. |
| `allowance_limit` | `NotNowReason::AllowanceLimit` | `RetryLater` | The Allowance limit has no room now; pay when its window rolls. |
| `answer_outcome_unknown` | `HeldForUserReason::AnswerOutcomeUnknown` | `NeedsUser` | Whether the earlier Allowance answer was delivered isn't known: show it; a `receive` may learn it, or its delivery may have failed for good. |
| `not_now` | a `NotNowReason` the bindings don't know | `RetryLater` | Call again later. |
| `counterparty_blocked` | `HeldForUserReason::CounterpartyBlocked` | `NeedsUser` | The deny list holds the counterparty: the user unblocks it, then call again. |
| `held_from_automatic` | `HeldForUserReason::HeldFromAutomatic` | `NeedsUser` | A block or a pause holds the request from automatic payment: the user unblocks or confirms the record, or pays it by hand. |
| `held_for_user` | a `HeldForUserReason` the bindings don't know | `NeedsUser` | Show `reason()` to the user. |
| `attempt_check` | `HeldForUserReason::AttemptCheck` | `NeedsUser` | Inside `attempt_failed`: a check before money moved failed and nothing was paid. A new payment is the user's decision once the check can pass, for a certificate expiring soon after the signer renews it. |
| `reload_required` | `HeldForUserReason::ReloadRequired` | `NeedsUser` | Load the wallet again, then edit its record. |
| `switch_client_hold` | `HeldForUserReason::SwitchClient` | `NeedsUser` | The Switch client holds the attempt. The bindings carry no subtype: tell the user the wallet holds payments for now, finish a reconciliation or a display read the host started, and check the device clock; then make the call again. |
| `held_unresolved` | `HeldForUserReason::Unresolved` | `NeedsUser` | A hold on one request, publication or delivery that no wait clears as far as the wallet can tell: show `reason()`. |
| `payer_pass_owed` | `RecoveryReason::PayerPassOwed` | `Recovery` | `reconcile` with pay runs the payer pass owed since the restore. |
| `activity_base_changed` | `HeldForUserReason::ActivityBaseChanged` | `NeedsUser` | After a provider loss another write sits at this record's version: show it for that request; it goes on once the loss steps run or a write the app takes replaces it. |
| `cancellation_order_unknown` | `HeldForUserReason::CancellationOrderUnknown` | `NeedsUser` | After a provider loss the order of this request's cancellations is unknown: show it for that request; the loss steps decide it. |
| `publication_conflict` | `HeldForUserReason::PublicationConflict` | `NeedsUser` | Writes conflict at one publication counter for this recipient: publication to it is held while the other app's write stands, until a write of the bound publisher replaces it. Show it for that recipient. |
| `record_unreadable` | `HeldForUserReason::RecordUnreadable` | `NeedsUser` | One held record doesn't read: show it for that request; it goes on once the record is rewritten or recovered. |
| `allowance_not_resumed` | `HeldForUserReason::AllowanceNotResumed` | `NeedsUser` | After a provider loss this version doesn't resume automatic payments under the Allowance: offer the user to end it and set up a new one; it can still be paid by hand. |
| `allowance_not_resumed_after_restore` | `HeldForUserReason::AllowanceNotResumedAfterRestore` | `NeedsUser` | After a restore this installation can't establish the Allowance's ledger history, which moved on since the backup: no later read or confirmation resumes it. Offer the user to end it and set up a new one, or to pay by hand; a newer backup of this app also resumes it. Don't ask the user to confirm the Allowance. |
| `allowance_execution_held` | `HeldForUserReason::AllowanceExecutionHeld` | `NeedsUser` | This app's execution of the Allowance is held, as the message names: take it up with `resumeAllowanceExecutor` where this app should execute it, show the Allowance record for the user to confirm, or offer payment by hand. |
| `recovery_needed` | a `RecoveryReason` the bindings don't know | `Recovery` | Show `reason()` and follow the recovery steps. |
| `usage` | `Refusal::Usage` | `NotSent` | Provisional. A value given breaks a Paykit rule: fix the call. |
| `not_automatic` | `Refusal::NotAutomatic` | `Permanent` | Provisional. Not paid automatically: the user can still pay it by hand. |
| `stopped` | `AppRefusal::Stopped`, from a payment too | `Recovery` | The wallet stopped on an alarm: show `restoreState().alarm` and recover. |
| `left` | `AppRefusal::Left`, from a payment too | `Recovery` | The app left its identity: enroll again. |
| `gone` | `AppRefusal::Gone` | `Recovery` | The app isn't current or its account is gone: enroll again. |
| `recovery_required` | `AppRefusal::RecoveryRequired`, from a payment too | `Recovery` | A recovery notice holds writes. `receive` clears a notice that isn't permanent; one that stays needs the recovery to the next generation. |
| `alarm` | `AppError::Alarm` | `Recovery` | Show the alarm and recover. |
| `chain` | `ClientError::Chain` | `Recovery` | The service signed a head that doesn't verify: handled as a rollback alarm. |
| `own_recovery_required` | `ClientError::OwnRecoveryRequired` | `Recovery` | A recovery notice holds writes to this service. `receive` clears one that isn't permanent; one that stays needs the recovery. |
| `approval_refused` | `AppRefusal::Approval` | `Permanent` | The approval doesn't pass its checks: ask for a new one. |
| `client_not_now` | `AppRefusal::NotNow`: reconcile mode, the clocks apart, a read still owed | `NeedsUser` | Provisional. The Switch client gives a text reason only, which the bindings don't pass on: finish the reconciliation, check the device clock, or `receive`; then call again. |
| `kept_batches_ahead` | `AppRefusal::KeptBatchesAhead` | `OutcomeUnknown` | The call's own batch was kept unsent behind earlier kept batches, which the wallet sends in order over its next reads: follow `more` on `receive`, then reconcile. Don't make the call again. A restore doesn't carry kept batches, so after one, reconcile decides. |
| `client_usage` | `AppRefusal::Usage`, `ClientError::Usage` | `NotSent` | A value given breaks a Switch rule: fix the call. |
| `carried_too_large` | `AppRefusal::CarriedTooLarge` | `NotSent` | Acknowledge the duty events acted on, then restore again. |
| `duties_too_large` | `AppRefusal::DutiesTooLarge` | `NotSent` | The backup wouldn't fit beside the duty events it carries: acknowledge the duty events acted on, then back up again. |
| `deny_list_full` | `AppRefusal::DenyListFull` | `NeedsUser` | After a provider loss the new service's deny list has no room for `over` of the app's deny entries: ask the user which peers to unblock, then follow the recovery again. |
| `wont_deliver` | `AppRefusal::WontDeliver` | `Permanent` | The message can no longer be delivered safely. |
| `expired_unsent` | `AppError::ExpiredUnsent` | `NotSent` | The batch was retired before it was sent: make the call again. |
| `store_after_kept` | `AppError::Store` behind a kept batch | `OutcomeUnknown` | The commit failed after the call's batch was kept: it may have applied. Reconcile before trying again. |
| `protocol_store` | `AppError::Protocol` behind a kept batch | `OutcomeUnknown` | Paykit's store failed after the call's batch was kept: it may have applied. Reconcile before trying again. |
| `transport` | `TransportError::Connect` | `RetryLater` | No connection could be made, so nothing was sent: call again later. |
| `transport_no_answer` | `TransportError::Timeout` or another transport failure | `OutcomeUnknown` | The request may have reached the service and taken effect, and no answer came: reconcile before trying again. |
| `transport_first_send_unavailable` | `TransportError::FirstSendUnavailable` | `NotSent` | A guarded first send couldn't be timed: the device clock is too far from the service's, or went back. Nothing was sent: correct the device clock, then call again. |
| `transport_first_send_expired` | `TransportError::FirstSendExpired` | `NotSent` | A guarded first send passed its deadline and was never released: make the call again. |
| `transport_refused` | `TransportError::Refused` | `NotSent` | The host's transport may not connect to the service's origin: allow it, then call again. |
| `transport_answer_too_large` | `TransportError::TooLarge` | `OutcomeUnknown` | The answer is larger than the endpoint's cap or the wallet's transport takes, neither of which the host sets. The request went out, so it may have taken effect: reconcile before trying again. The same call gets the same answer; report it. |
| `transport_invalid_request` | `TransportError::InvalidRequest` | `NotSent` | The host's transport couldn't build the request: fix the transport. |
| `account_not_answering` | `AppError::NotAnsweringAsAccount` | `RetryLater` | The service doesn't answer for the account yet: call again later. |
| `invalid_data` | `AppError::InvalidData`, `ClientError::Core` | `OutcomeUnknown` | Data from the store or from the call's answer doesn't verify or doesn't read. The request may have gone out and taken effect: reconcile before trying again. The same data gives the same answer until what serves it changes; show it and report it. |
| `display_read_pending` | `ClientError::DisplayReadPending` | `RetryLater` | Wait for the signed display read in progress. |
| `bad_service_signature` | `ClientError::BadServiceSignature` or an unreadable signed header, at any status | `OutcomeUnknown` | An answer the pinned service key doesn't verify, from an intermediary such as a proxy, or after the service changed its key: nothing of it was used, its status included. The request went out, so it may have taken effect: reconcile before trying again. `receive` follows the newest `identity.json` and its key; if the answer stays, report it. |
| `stale_answer` | `ClientError::StaleAnswer` | `RetryLater` | A public read outside its window: call again. |
| `body_unread` | `ClientError::BodyUnread`: `too_large`, `rate_limited` or `not_found` before the service read the request | `OutcomeUnknown` | Such an answer binds the request without its body, so it counts as no answer (Switch wire 7.1): reconcile before trying again, after `retryAfterMs` when it is set. |
| `invalid_response` | `ClientError::InvalidResponse` | `OutcomeUnknown` | The call's answer doesn't read. The request went out, so it may have taken effect: reconcile before trying again. |
| `body_over_cap` | `ClientError::BodyOverCap` | `NotSent` | The request is over its cap: make a smaller one. |
| `unpinned` | `ClientError::Unpinned` | `NotSent` | No pinned service key: enroll first. |
| `switch_bad_request` | Switch answer `bad_request` | `Permanent` | The service refused the request as malformed. |
| `switch_bad_signature` | Switch answer `bad_signature` | `Permanent` | The service refused the request signature. |
| `switch_stale_timestamp` | Switch answer `stale_timestamp` | `RetryLater` | Call again: it is signed with a new timestamp. |
| `switch_not_admitted` | Switch answer `not_admitted` | `NotSent` | The recipient doesn't admit this sender now. Nothing applied; send again once the recipient admits it, for example after it adds the contact. |
| `switch_permission_missing` | Switch answer `permission_missing` | `NeedsUser` | The certificate lacks the permission: ask the user to approve a wider one in their signer. |
| `switch_not_current` | Switch answer `not_current` | `Recovery` | The app isn't current any more: recover. |
| `switch_cert_expired` | Switch answer `cert_expired` | `Recovery` | The certificate expired: get a new one from the signer. |
| `switch_not_found` | Switch answer `not_found` | `Permanent` | The service holds no such object. |
| `switch_conflict` | Switch answer `conflict` | `NotSent` | The state changed: read again, decide again, then call again. |
| `switch_stale_epoch` | Switch answer `stale_epoch` | `RetryLater` | Call again after `receive`. |
| `switch_stale_generation` | Switch answer `stale_generation` | `RetryLater` | Call again after `receive`. |
| `switch_stale_key` | Switch answer `stale_key` | `RetryLater` | Call again after `receive`. |
| `switch_stale_identity` | Switch answer `stale_identity` | `RetryLater` | The `identity.json` delivered isn't the newest the service holds: call again after `receive`, which reads the current one. |
| `switch_identity_needed` | Switch answer `identity_needed` | `RetryLater` | Call again after `receive`. |
| `switch_moved` | Switch answer `moved` | `RetryLater` | The account moved: call again after `receive`. |
| `switch_too_large` | Switch answer `too_large` | `NotSent` | The request is over a size cap of the service. Nothing applied; make a smaller one. |
| `switch_rate_limited` | Switch answer `rate_limited` | `RetryLater` | Wait `retryAfterMs`, then call again. |
| `switch_quota_wait` | Switch answer `quota_exceeded` naming `inbox`, `sender_share`, `claims` or `claim_share` | `RetryLater` | The limit frees with time: as the recipient's apps read their feed, or as the service deletes old claims, 30 days after their deadline or 400 days after their last entry. A claim with an open or unknown attempt is never deleted by age, so this app's doubtful attempts may need the user's answer first. Call again later. |
| `switch_quota_open_attempts` | Switch answer `quota_exceeded` naming `open_attempts` | `NeedsUser` | This app holds the most open attempts it may. The new attempt's batch stays kept at the front of the app's queue and is sent again before each later write, so the outcome writes that would free a slot, from `resumePayments` or the user resolving a doubtful attempt, wait behind it until the switch takes that batch. Don't pay again: show that this app's payments wait, and call `resumePayments` later. |
| `switch_quota_full` | Switch answer `quota_exceeded` naming `storage`, `record_keys`, `contacts`, `conversations`, `deny` or `invite_codes` | `NeedsUser` | The limit frees after the user removes something, such as contacts or records, through another app or the signer (these bindings have no call for it); invite codes also free as they expire or are used up. `quotaLimit()` names the limit. Then call again. |
| `switch_quota_connect_requests` | Switch answer `quota_exceeded` naming `connect_requests` | `NotSent` | The recipient has its pending connect requests full. Nothing applied; ask again much later, as the recipient may never answer them. |
| `switch_quota_final` | Switch answer `quota_exceeded` naming `attempts` | `Permanent` | Final: no more attempts fit this obligation's claim. |
| `switch_quota_exceeded` | Switch answer `quota_exceeded` naming a limit the bindings don't know, or none | `NeedsUser` | Show it to the user; it is never retried by itself. |
| `switch_staged` | Switch answer `staged` | `RetryLater` | Call again later. |
| `switch_unavailable` | `Refusal::SwitchUnavailable`, Switch answer `unavailable` | `RetryLater` | The own Switch is unavailable: nothing was paid. Call again once it answers. |
| `switch_recovery_required` | Switch answer `recovery_required` | `Recovery` | The account awaits a recovery. `receive` clears a notice that isn't permanent; one that stays needs the recovery to the next generation. |
| `public_endpoint_change_pending` | `PaykitError::PublicEndpointChangePending`, a refusal of the Switch client | as its cause | The edit is saved, and is published once what stopped its signing clears. It comes as `Recovery` when the client stopped for a recovery. Otherwise it comes as `NeedsUser`: the user or another own app acts first, such as renewing a sibling's record whose policy expired. |
| `method_unknown` | `RailError::Unavailable` from a lookup or endpoint selection | `RetryLater` | The method couldn't answer now: call again later. |
| `method_not_submitted` | `RailError::Refused` | `NotSent` | The method refused the call. |
| `method_invalid_selection` | `RailError::InvalidSelection` | `Permanent` | Fix the method's endpoint selection. |
| `invalid_input` | `paykit_lib::Error::Validation` | `NotSent` | A value given breaks a Paykit rule: fix the call. |
| `invalid_protocol_data` | other `paykit_lib::Error` | `Permanent` | Network or stored data breaks a Paykit rule. |
| `conflict` | `PaykitError::Conflict` | `NotSent` | Read the current state, decide again, then call again. |
| `homeserver` | `PaykitError::Homeserver` | `RetryLater` | The Homeserver couldn't be read now: call again later. |
| `settlement_warning_unsaved` | `PaykitError::SettlementNoticeUnavailable` | `NeedsUser` | The Switch refused a new settlement bucket for lack of a record slot, and the wallet couldn't save or check that bucket's warning. Show the warning for `settlement_bucket()` if the host has yet to show it; it grants no consent to issue another receipt. `settlement_warning_outcome()` says what this call did to the warning: `Failed` when it wrote none, and when its lookup of an earlier warning failed, whether one is saved already is unverified; `OutcomeUnknown` when the store can't tell the outcome of its lookup or its write: it may be saved, by this call or an earlier one. The marked receipt and its delivery are still kept under `kept_batch()`, and that batch may still apply. `service_code()` and `quota_limit()` give the Switch's answer. |
| `unclassified` | a Paykit failure the bindings don't know | `OutcomeUnknown` | Reconcile before trying again. |
| `client_unclassified` | a Switch client failure the bindings don't know | `OutcomeUnknown` | Reconcile before trying again. |
| `method_unclassified` | a method failure the bindings don't know | `OutcomeUnknown` | Reconcile before trying again. |
