# Paykit v0.2 conformance

Run the applicable cases against the [protocol](spec.md) and [wire format](wire-format.md). A reference to a numbered section below refers to the Paykit specification unless it says “wire” or names the Dispatch. The [vector guide](vectors/README.md) gives inputs for byte-level checks. Cases involving time, storage, authorization or delivery require the corresponding wallet and service observations. See the [implementation table](implementation.md) for the published evidence.

## App records, authority and current identities

### Local app names

App display names stay in the host's local state and are absent from the signed outer app-record body (Section 4 and wire 3.2). The wallet checks compare the writer's signed record with the record read from another wallet's directory and the Homeserver bytes, at join and after an update. Each labelled seller retains its own request authority, and the other seller's label cannot authorize a reply. The generated app-record envelopes are described in `vectors/README.md`.

### Original recovery report

The original app’s signed recovery report and its independent publication under the keyed `paykit_recovery` scope (Sections 3.1 and 7.3, wire 5.10). Read the original App IDs from the identity-signed opening and report explicit removals independently of verified reports. For each report verify the original signer and the authorized publisher at the authenticated write time. A verified report and a later removal can both appear for the same app. The generated vectors cover the collection policy and the original’s signature.

### Multiple protocol owners

Exercise the Dispatch contracts in Sections 2, 4, 6, 7.3, 8.1 and 9. Required integration cases include an own certificate containing Paykit and another protocol through public join, load and restore, owner-isolated local state and duties, shared deny barriers, failed persistence and unchanged current authority. An unknown recovery ask must remain possibly delivered through restart and restore, without requeueing its signed bytes or automatically issuing another ask. A restored store lacking a give-up mark must not reconstruct a message from a shared record or flush a stale backed-up copy without current evidence that the original delivery remains pending. Existing peer-certificate vectors do not exercise these own-app lifecycle paths.

### Proposal generation

Require a positive `allower_generation` in the proposal. Take it from the Allower’s verified identity and check the current generation and provider-loss boundary before opening the ledger (Section 8.3.1 and wire 4.15).

### Both original acceptance roles

The first release requires both proposer roles with the same Allower wallet approving and executing. Verify the applied permanent P/Q, original Q-bound zero and exact pending deletion before its first grant (wire 5.9). Cover an unanswered batch, reopen, a complete backup for the same app, the original deletion retained or collected, an ordinary successor and a repeated Q. Another app, a stale or incomplete backup, an earlier rejection, withdrawal or discard, and an unrelated or unproved third operation must leave the grant and spending held. An app lacking executor scope cannot create or update the permanent ledger. The generated vectors cover the permanent ledger policy and keys, the original zero for each proposer role and the acceptance hash in populated ledger values. Repeat with the unchanged original ledger, after reopening the same app, and after reconciling a complete backup.

### Generation and counter pair

Private Payment Lists carry the required positive generation/counter pair, compared generation first, then counter, across the list, request hint, proof, consumed endpoint and unrequested destination (wire 4.10, 4.13, 4.14, 5.3 and 5.4; vectors: `messages.json` `private_payment_list`, `private_payment_list_request` and `payment_proof_unrequested`, `sealed.json` `binding_accepted` and `activity_records`; `messages.json` `list_versions` refuses a version with no counter, a zero counter and an integer); a later `payment_lists` write holding another list of the same pair never replaces the held one, and its used entries are kept (wire 4.13, 5.4). When paying, the payer checks a list's generation against the payee's verified `identity.json` before any reservation (wire 4.13, Section 7.2).

### Per-recipient publication reservation

A publisher reserves the next pair by a conditional write to its permanent per-recipient counter. The signed message enters the outbox only after the batch applies. A proven nonapplying batch queues nothing. An unknown outcome keeps the original duty and payloads, including when the deciding window expires. A restored publisher continues from the current counter and its retained floor. Verify plain selling authority (Section 3.3), conditional reservation, a lost batch answer, withdrawal after restore and reusable answers within both rate limits. Retry a withdrawal after definite first non-delivery while an older list remains payable. Check single-use renewals under Section 8.4.1.

### Publication after provider loss

After a provider loss the publisher waits for its own recovery upload before reserving a list or withdrawal in the new generation. It can then use that generation's counter without waiting for other apps' uploads (Section 8.4.1).

### Extension prefix

An extension scope prefix starts with `paykit-`. Reject a prefix that lacks the hyphen (wire 3.1, 4.3 and A14).

### Mixed protocol certificate

A certificate holding another protocol's scopes beside Paykit's (Section 3.2, wire 3.1 "Several protocols"): its `permissions` hold every permission Paykit's scopes need and may hold more, Paykit's rules and bounds count only Paykit's scopes, and those scopes alone authorize Paykit's kinds (`documents.json` `multi_protocol_certificates`, `messages.json` `multi_protocol_senders`).

### Recovery ask sender

A `resend_request` from a Paykit App holding `paykit_contacts`, with any Paykit App permitted to receive it (Section 3.3, wire 4.1, 4.12). Exercise a sender with and without the scope.

### App record members

The checks of wire 1.4 on the identifiers and payloads of an app record's `paykit` member (wire 3.2; `documents.json` `app_records`, the last 4 refused).

### Homeserver republication

An app republishes its record on the Homeserver at a renewal only once the service accepts it, and while a publication fails, or the Homeserver copy differs from the newest record the service accepted, it publishes that newest record again, never an older one and never over a Homeserver copy with a later `updated_at`, until it deletes that record to leave, or leaves (wire 3.2, Renewal).

## Requests, pricing and payment evidence

### Installed wallet payment method

Pay one request through a generated binding and check the original attempt ID, endpoint, amount and reference at the host method. Interrupt the response after submission. Resume through a lookup of that same attempt and observe one transfer, the Payment Proof containing the method's original evidence and the receipt for that proof after the payee's own check. A lookup with an unknown outcome leaves the attempt open. Endpoint selection alone, a refusal before submission and an unverified proof must produce no credit. Exercise the same contract through each supported platform binding (Section 7.2, Installed wallet bindings).

### Pruning a listed period

A reader that holds the version a write was built on ignores and flags a write that drops a listed period without a `claim_step` condition on that period's claim among the conditions its operation signed (wire 5.3).

### Cancellation routing

A cancellation goes to the other role than its sender's, on receipt, on sending and in a resend answer (Sections 3.3, 3.4; wire 4.1, 4.3, 4.12; `messages.json` `cancellation_roles`).

### Conversion terms

With conversion terms, a request's asset and its rates' assets follow the conversion convention, and each rate matches an endpoint the request names (wire 4.4; `messages.json` `conversion_terms`).

### Per-period rate selectors

Period details carry rates only for a request with `per_period` conversion terms, and their rates are cross-asset only, bare or rail-qualified, follow the same convention and match an endpoint the details or the request name (wire 4.9; `messages.json` `conversion_terms`).

### Transfer amount and rounding

The amount to an endpoint is the request's amount times the asset-rail rate, else the asset-wide one, else 1 for an endpoint of the requested asset, rounded up once to the endpoint's smallest unit, and none for an endpoint of another asset that no rate prices (Section 7.2): `paykit-lib` computes it (`amount_for_endpoint`) and `messages.json` `conversion_amounts` checks it; that the payer pays that amount, and treats such an endpoint as unusable under conversion terms, is a rule of an app's run.

### Fees are the payment method's

The payer configures, reports and checks no fee: a payment by hand, an automatic one and an Allowance executor's are each paid whatever fee the payment method would charge, with no confirmation asked for it and no fee journaled (Section 7.2 step 1; wire 5.5, The amount transferred). The transfer's amount check stays: a destination that transfers another amount or asset, or an amount the method can't tell, is unusable for automatic payment and needs the user's choice by hand. Required cases include an automatic payment, an Allowance executor's included, that proceeds while the payment method charges a non-zero fee it bounds internally, with no fee reported to Paykit, and a destination that transfers another amount or asset refused for automatic payment.

### Details checked against the request

A reader of a period record reads the period details it holds against the request record's request, ignores any invalid against it and pays by none (wire 4.9, 5.3).

### Partial public endpoint discovery

A payer's read of a payee's public endpoints reports one app's failure as that app's and keeps the other apps' usable endpoints, within 256 endpoints and 4 MiB of payloads, and reports an app that doesn't fit as over the bound (Section 4).

### Quote authorization

A rate-priced payment outside an Allowance pays the exact quote authorized for this request and, when there is one, its Billing Period, by user confirmation or an expressly authorized bounded host policy. A changed quote requires a fresh decision. Exercise a manual confirmation and a host policy for an automatic Billing Period (Sections 7.2 and 8.2).

### Recurrence end

A recurrence's `ends_at` is present, a time or `null` (wire 4.4).

## Allowance terms and record bindings

### Period limit spelling

Period limits are duplicates only when spelled the same, and carry both their limits (wire 4.15; `allowance_usage.json` `period_limit_spellings`, `period_limit_members`).

### Stored sender authority

A stored message passes the sender's side of check 8 when it is read (wire 5.2; `sealed.json` `activity_records`).

### Activity record roles

An activity record carries `request_hash`, `billing_period`, `attempt_id`, `unrequested` and `allowance_id` only where its binding and side allow (wire 5.3; `sealed.json` `activity_records`).

### Distinct message identifiers

A record's `msg_ids` names each ID once, the 16 newest distinct others, and readers read it in that form (wire 5.3, 5.4; `sealed.json` `msg_ids_bounds`).

### Proof members

A proof's members follow whether it proves a request: `amount` only without one, its Billing Period with the period details it used, and no request context without a request (wire 4.10; `messages.json` `proof_members` has refusal cases for all three rules).

### Receipt occurrence

A receipt is of its proof's attempt and Billing Period, and names no period without a request (wire 4.11). `sealed.json` `activity_records` checks a receipt of another attempt, and `messages.json` `receipt_members` checks a receipt naming a Billing Period without a request. No vector refuses a receipt whose Billing Period differs from its proof's.

### Allowance end references

An Allowance end names the stored acceptance, or is the proposal's withdrawal by its sender before any response; a rejected Allowance has no end; `executor_app_id` only in an acceptance from the allower (wire 4.16; `sealed.json` `allowance_records`).

### Unrequested list version

An unrequested destination's `list_version` is present exactly for a list, and is the proof's (wire 5.3).

### Ledger compaction

A newer Allowance ledger version holds an attempt that left into its totals in neither its entries nor its `totals.failed` (wire 5.5).

### Equal-time details

Period details as late as those held but with another hash replace nothing (wire 4.9, 5.3), and a later write of an `allowances` record keeps the proposal, response and end seen (wire 5.4).

### Preserving consumed endpoints

A later write of a `payment_lists` record holds no older list, no other list of the same `version`, and every `used` entry seen, whatever its list's `version` (wire 4.13, 5.4).

### Conflicting Allowances

Pending conflicts follow Section 8.3.1. After acceptance a second, different proposal or response under the same `allowance_id` makes the Allowance conflicted. The permanent record holds it in `conflicted`, a proposal from either side with valid terms and another hash than `proposal`, or, beside the record's response, an acceptance or rejection from the proposal's recipient that refers to the proposal, with another hash than `response`; a conflicted record has no current grant, and a later write never drops or replaces it (Section 8.3, wire 5.4; `sealed.json` `allowance_records` checks the record's form, and no vector has a later write that drops it, which `paykit-lib` checks); keeping the second message as the stop, sending no response, end or grant for a conflicted Allowance, and an executor's attempts already open under it (Section 8.3, Conflicted) are rules of an app's run.

### Original proposal delivery

An app verifies that its own signed proposal was written to its pending record before sending it. Its first rejection or pre-acceptance withdrawal can be sent only after its original delete of the exact pending write applies. Section 8.3.1 specifies the current record and original batch checks before an acceptance can be delivered. These are SDK flow checks; the wire vectors cover the form of the records.

## Billing Periods and funded attempts

### Long Allowance periods

Allowance terms take any positive `every`, and a period too long to evaluate makes the evaluation ineligible, not the terms invalid (wire 4.15).

### Receipt metadata and reference

A receipt may carry the payee's `metadata`, and, for a payment without a request, a `payment_reference`; a receipt carrying a reference and a request hash is refused (wire 4.11, 1.1; `messages.json` `receipt_members`).

### Verified rate time

Period details' rates apply from their `created_at`, cut to whole seconds, through their `expires_at`, and each period detail stays valid until its own `expires_at` (wire 4.2, 4.9; `periods.json` `details_rates`).

### Earlier period details

A period record keeps every period detail: `period_details` is the latest or the one the stored proof names, `earlier_details` the others, at most 7, and only the write that stores the proof may point `period_details` back at older details (wire 5.3; `sealed.json` `activity_records` checks the record's form, and its `later_writes` a write that drops details seen; no vector has a later write that points `period_details` back).

### Reassociation order

A payer's request record of a recurring request holds `reassociations`, at most 16, strictly ascending, each `from_period` a Billing Period start of the stored request, by its second, and a reassociation is added only from a period later than every period in `attempted_periods` of the version its write builds on, which a reader holding that version checks (wire 5.3; `sealed.json` `activity_records`, with its `later_writes` and their `built_on_held`).

### Confirmed reassociation

A reassociation is added only after the user confirms it, to an accepted Allowance of the request's counterparty, neither ended nor conflicted, whose terms match the request, possibly not active yet, by a `record.put` carrying `server_time_in(null, from_period)` (Section 8.3, wire 5.3).

### Subscription funding

An executor accepts a subscription under an Allowance only while the executor's conditions hold and the terms are valid and in force, capacity being checked at each payment, and the request matches the terms; and its acceptance, conditioned on the Allowance record's version, pins the Allowance in `allowance_id` (Sections 8.2, 8.3; wire 5.3); `paykit-lib` checks where `allowance_id` may appear (`sealed.json` `activity_records`) and that a later write never changes or drops one seen, which no vector does.

### Eligibility for a period payment

A payer pays a Billing Period only of a subscription its request record shows accepted, and an executor pays one only under the Allowance the request record names, which for a recurring request only an executor's acceptance writes, never a period's payment, so a subscription accepted without an Allowance is never paid under one (Sections 8.2, 8.3; wire 5.3, 5.5).

### Automatic payment authority

An executor pays automatically only while every condition of Section 8.3, Executor, holds, the pauses of Sections 6, 7.1, 7.2 and 8.3 included; an ended or expired Allowance pays nothing more of a request, whose occurrences are then paid only by hand, a period or lifetime limit without room blocks only the period it affects, and a handoff keeps the Allowance; the executor never spends its Allowance at a conversion rate; a ledger that can't take an entry after compaction, counted with an outcome in every entry, pays nothing automatically, and compaction is judged on the latest verified service time (Section 8.3).

### Automatic payment window

An executor, or an app paying a period by its own policy without its user's confirmation, pays automatically only from the request's first Billing Period when the recorded time of the payer's acceptance precedes that period, else from the period containing that time, and each period only until the earlier of its attempt deadline, for a request with a payment deadline, and the next Billing Period's start, for every request, on the highest service time it has verified and judged again at the first handoff, other periods being paid only by hand, each within its own payment deadline (Section 8.2, Paying periods).

### Doubtful claim

An executor never attempts an obligation whose claim holds an attempt that its Allowance's ledger names neither in an entry nor in `totals.failed` (Section 8.3, Doubtful obligations).

### Fresh executor authorization

An executor reads the Allowance record again immediately before an attempt's first handoff and pays nothing unless it still holds every condition of Section 8.3, Executor, at the version the attempt's batch was conditioned on, a read that fails or a record that doesn't read paying nothing; a resume's repeat under an Allowance that a read made immediately before it shows ended or rejected, or whose end the executor keeps as a stop, or whose record that read fails or doesn't read, is held, never made, its ledger entry staying reserved and the attempt open (Section 8.3, Executor; Attempts already open).

### Allowances count transferred amounts

An Allowance counts the amount the payment transfers in the requested asset, never less than the requested amount, for its reservation, its spending and every limit, and a ledger entry's `value` is that amount (Sections 8.2 and 8.3; wire 5.5).

### Manual payment record

An attempt its user started by hand writes `manual_only` on the occurrence's record in its own batch, the request record of a one-time request or the period record of a Billing Period, created if the period has none, at the version it read, so no executor, nor an app paying a period by its own policy, pays that occurrence automatically again (Section 8.3, Manual only; wire 5.3).

### Ledger outcomes

The current executor writes each attempt's outcome into its ledger entry, in the batch that records the outcome when it records it itself and otherwise from the claim read again, `done` from a claim, `failed` only when the attempt's owner wrote it or the user released the amount with the never-handed-off evidence of Section 8.3, and a later `done` over a `failed`, a compacted failed attempt whose claim shows `done` moving into the totals once; a previous executor records its own attempts' outcomes on their claims alone (Section 8.3, Outcomes in the ledger, Handoff).

### Failed attempts

A `failed` attempt counts as spent immediately, while its claim remains and after it is gone, whoever wrote the failure, whether the entry holds `failed` or reads it from the claim, and after compaction in `totals.failed`; it releases only on the executor's durable journal evidence that it ended before handoff and can never hand off, never on a user's confirmation alone or a restored journal, and a claim, ledger entry or proof showing it paid overrides that evidence; the executor writes `done` from its copy of the request's or Billing Period's payer record holding a proof of this attempt naming this Allowance, whoever made it, while no readable copy, no proof and a proof of another attempt or another or no Allowance release nothing (Section 8.3, `failed` isn't final; wire 5.5, Counting): the pure counting rules and generated `allowance_usage.json` cases are implemented by `paykit-lib` on `v0.2`, using already validated attempt evidence; the executor's durable journal, proof collection and ledger updates remain runtime work.

## Original Allowances, deleted records and handoff

### Pending reply before spending

An executor pays nothing under an Allowance while an answer to its proposal is kept or stored and not yet written into the record, or while its counterparty owes a re-send of replies dropped for the bounds (Section 8.3, Executor).

### Durable end before sending

An end of an accepted Allowance is stored before it is sent. An app already holding a matched permanent proposal and acceptance records an end naming both when a withdrawal crosses the acceptance, unless the Allowance is conflicted. A proposer whose pending record was deleted checks the retirement of its own exact pending write before sending a withdrawal under Section 8.3.1. A crossing rejection creates no permanent record. A proposal or answer whose record fails to open waits for its scope key or is kept as a stored message. The app immediately keeps a counterparty's end as a stop. It drops that stop when the permanent record holds the end or the matching withdrawal and rejection settle the pending lifetime. For a record not held, it is bounded with the cancellations kept as stops and droppable after 30 days, never while a stored message of the Allowance waits to be written, when it counts outside the bounds (Section 8.3, Ends reach the executor, Ending).

Exercise both the payer and the seller as proposer, after an in-place restore and on receipt of a late acceptance. Keep the original pending create, the matching signed rejection, the proved conditional delete of that create and an uninterrupted service history. With both current keys absent, require one withdrawal referencing only P. A reload or repeated Q must produce no second withdrawal. Repeat across an orderly generation change with the original service keys. Withhold the withdrawal when the create, delete proof or required historical key is missing, even if the app still has P and both keys read absent. If the missing key becomes available while the original retirement is still current, require one withdrawal. After a provider loss that breaks continuity, leave the peer’s later lifetime of the same signed P intact even when the replacement service shows both keys absent. Separately recreate the same signed P on the proposer’s account before delivery and require the later pending record to survive. If that lifetime is then accepted or deleted, require its outcome to stand and the old withdrawal to stay suppressed.

Exercise an Allower that proposed P before a declared provider loss and an Allowee that records its matching signed acceptance Q. Reinstall the original signed pending write at the replacement service. On ordinary receive, require the Allower to report that Q cannot open a budget, conditionally delete that exact live write with the permanent key absent, and deliver one withdrawal naming only P. Interrupt after preparation but before proof of application. W must stay held until its exact applied delete is verified. Interrupt again after application and reopen with no permanent payer record. Verify that the retained P, Q, W and exact delete with its service proof recover the same obligation. Every eligible retry uses the original W and obeys the generated-end deny rules. Require the Allowee’s matching mirror to end with both references. An unrelated end or an end naming a different Q must leave the Allower’s settlement outstanding. Deliver the matching signed end, including after reopening before it arrives. Require the Allower to recognize it and durably retire only that settlement. Reopen after retirement and repeat the end. The original stays retired, no further W is sent, a later lifetime is unchanged and neither wallet gains payer spending authority. Separately restore an older same-app backup that sees the live signed P and matching Q but lacks the original create. Require the host’s permanent-refusal notice, an unchanged pending record and no withdrawal. Repeat with an outage and no signed loss, an unknown or future proposal generation, an absent or unreadable pending key, an unproved original, a recreated lifetime, an opposing reply and an ineligible sender. Keep uncertain work held and preserve any later lifetime.

### Permanent record absence

The service refuses deletion of a permanent Allowance and its ledger. An app that loses a previously observed permanent record in the same account history raises the rollback alarm and withholds payment, grants and acceptance (Section 8.3.1).

### Malformed loss marker

A `lost_to_delete` member that is present but isn't a valid value, `null` and a value of another type included, never clears a restriction, in any collection that carries it: the record reads at its least permissive state (wire 5, Deletion data that doesn't read); only an absent member means no restriction.

### Activity loss marker

In activity records accept a canonical 32-byte `lost_to_delete` hash. Reject `null`, a boolean, a short hash and a padded hash (wire 5.3, `sealed.json` activity records).

### List loss markers

For `payment_lists`, the `lost_to_delete` format checks, runtime behavior and positive and rejection vectors remain outstanding. These rules cover the allowed families, validation of the delete item hash, preservation in later writes and the family's least permissive state (Section 6 and wire Section 5). The SDK on `v0.2` implements wire Section 5's publication and service-cursor hold for a receipt already issued for a one-time payee request in the deletion's original service generation, and shared replacement and the cursor hold remain outstanding for other protected records. The origin and lineage evidence specified at Dispatch revision `f0bbddd1` is absent from the current pin. The Paykit family mapping and integration are required before release.

### Publishing a previously issued receipt

For a deletion of `payee_activity`, an exact predecessor containing an issued receipt, an older signed receipt and a missing copy each have the result in wire Section 5. Check writer attribution, a warning acknowledged before replay, a later write at the key and a restart. Both Billing Period boundaries must match the signed request. The retained-receipt test rejects a signed proof and receipt with a different end time. Restoring a backup that predates the receipt on an installation that still holds it must retain the signed receipt and its credit restriction. Restoring the same backup on an empty installation cannot recover missing receipt bytes. A retained receipt for a settlement prevents crediting that settlement to another occurrence even if it names another payer. An unrelated settlement is judged on its own evidence. These cases check the displayed receipt and the effects on local state, shared records and credit. For the one-time receipt publication rule, conformance also requires a complete successor with no further write, a conditional repair preserving both records’ opaque members at every retained object, a hold on conflicting opaque values, ordinary readers with and without proof of the same-delete marker exception, quota refusal holding the service cursor, and the original applied and unapplied writes across restart, restore and an exclusive handoff between devices sharing an App ID. The remaining families’ replacement and service-cursor behavior is outstanding.

### Older payer record

The permanent payer hold of an older-copy put-back (Section 6, Payment history lost): runtime checks and generated cases must reject payment through the marked record with an absent claim after 400-day retention and a request clock four minutes ahead, a future-dated or replayed request, an early pinned deadline, a present claim showing only failed attempts, a purged then recreated claim, partial recovery after provider loss despite user confirmation, and erasure followed by reenrollment; every Billing Period of a marked recurring request stays held, including one paid early by hand. Positive cases must preserve the proved exact-predecessor put-back path without adding a new loss marker.

### Protecting deleted records

A record whose content withholds or bounds a payment or a credit, of the families Section 6 lists, deleted by another app, is put back over the delete, naming the delete's version, before or atomically with the service cursor acknowledgment of that delete, a durable publication obligation holding acknowledgment while quota, the value limit or another refusal blocks the write: as the app's copy when that copy is the write the delete was built on, naming the deleting app, and otherwise as the family's least permissive state with `lost_to_delete` naming the delete by its item hash, naming no app, under which a payer's occurrence is paid never, by hand neither, once put back from an older copy, with no claim state or user confirmation lifting that hold, and a payee's occurrence is credited nothing; every older copy of a `payee_activity` or `payment_lists` record enters its protected family even without a receipt or `used` entry; a user's confirmation binds the write's item hash and advances only over authenticated exact predecessors, ending at the first delete, missed write, unproven predecessor or other value the app reads, and at a restore; a record held as deleted reads as that state; a write-back that loses to another takes it or writes the two merged over it; an app without a write scope only names the deleting app and tells its user; a deleting app is named once and the user told once per deleting app and once per delete an older copy answers; and a payer deletes a record holding `manual_only` before its request's deadline, or a request record listing a period, never (Section 6, Putting back a deleted record and Deleting finished activity; wire 5, Put back over a delete).

### One Allowance per request

When more than one accepted Allowance covers a request whose record names none, the executor chooses the one it accepts or pays it under, pays under that one alone, and its batch names it in the request record (Section 8.3, One Allowance per request); an executor that held a ledger version before it became the executor keeps it as its last ledger version (Section 8.3, Last ledger version); and no reader takes a missing ledger as empty (Section 8.3, A grant implies a ledger).

### Handoff authority

A later write of an Allowance record that drops a `ledger_handoff` its reader has seen, or replaces it by one for the same or an earlier grant, is ignored and flagged; after an earlier grant, a new executor pays nothing automatically until a valid handoff carrying the complete matching value or successful complete-authority repair establishes its accepted anchor under the current grant; a version the service shows alone or an earlier-term copy supplies none of those paths. The previous executor writes `ledger_handoff` only while the current ledger exactly matches its complete value and every ledger write it sent has a verified answer incorporated into that state; it writes none for a version a restore alone installed, for one it did not write or take by a `ledger_handoff` since its own grant, or for a grant its own app signs again (Section 8.3, Last ledger version and Handoff; wire 2 and 5.4; generated vectors of the `ledger_handoff` binding and signature are in `sealed.json` and `signatures.json`).

### Handoff preservation

Handoff preservation and ordering (wire 5.4): required checks cover omission of parsed and unread handoffs, an earlier-grant replay, a changed handoff for the same grant, an unread handoff with a readable grant position and one that cannot be placed, a valid later handoff observed after skipped grant versions, and a later write dropping or replacing the current last grant. Exact preservation and a verified handoff for a strictly later grant are positive cases. A handoff naming no ledger write is signed over its own invalid ledger tuple so its signature verifies and only the version rule refuses it. See `sealed.json` and `signatures.json` for the binding and signature.

### Confirmed ledger repair

An executor repairs a ledger it didn't accept only with its user's confirmation of the figures shown, writes exactly the ledger whose figures it showed, and refuses if anything it was built from changed; complete usage authority is required even for an empty inventory, and no complete version kept refuses instead of starting from zero; the merge keeps the version kept as it was, may count an attempt twice, never less, and refuses for a request with more Billing Periods whose claim may still be held than the repair reads (Section 8.3, After a confirmation; wire 5.5).

### Complete usage for repair

Complete usage authority and its bounded durable state (Section 8.3; wire 5.5) are required before repair and handoff. Required cases cover a lifetime-limited payment compacted into totals, its activity and claim evidence expiring, and provider loss removing the accepted ledger and journal: repair of an empty inventory refuses and a new obligation cannot reuse the lifetime capacity, even after confirmation or a new grant. Also required are partial surviving inventories, lost compacted counts and failed items, an older accepted ledger missing a later applied batch, unanswered first-grant, executor, terminal-outcome and compaction writes across failed local persistence and restart, a backup missing a later write, a hash-only or incomplete-source handoff, an older-copy put-back, another instance using the same executor identity, and provider loss showing an older matching copy after that instance's later write was lost. Positive cases retain cumulative usage through compaction, repair a current complete value after definitive non-application or incorporation of an applied pending write, and transfer the complete current value by a valid handoff. A concurrent writer must fail the repair's CAS. Neither deadline expiry nor elapsed usage windows repairs lost completeness. Exercise these cases with durable evidence for the complete inventory.

### Repair after handoff

Successful repair as an alternative to handoff (Section 8.3; wire 5.4 and 5.5): A hands a complete ledger to B, B writes nothing and goes offline, then A becomes executor again with its complete checkpoint still exactly current. Confirming and applying the conditioned repair durably establishes its result anchor and permits automatic payment when every other check passes. Unknown, refused and locally uncommitted repair results do not; a changed term requires its own authority. A late handoff is checked against the retained first repair base, preserving rejection of an earlier named write, another item hash at the same version, a later named write and a repair built on absence. The authenticated repair result and the base have separate purposes and survive reload. Required runtime and generated cases come with the implementation.

### Reconstructing a transfer amount

Repair amount reconstruction requires durable transfer-amount evidence, including rounding and excluding fees, for every included attempt; a nominal request or Billing Period amount, a claim, a request proof without an amount, current endpoint units or rates, and confirmation alone cannot supply it. Required rejection cases cover claim-only and proof-only reconstruction after a rounded payment, journals retaining only the request amount, missing evidence after claim expiry, restore or provider loss, and an unaccepted shown ledger that understates the transfer. The concrete request `0.000000001` BTC, transfer `0.00000001` BTC and lifetime limit `0.000000011` BTC must not allow a second `0.00000001` BTC transfer after repair. Positive cases use a retained accepted amount or a journaled exact transfer bound, preserve a larger accepted value, include a recurring period and exclude fees; confirmed failure without never-handed-off evidence stays spent. No current repair implementation or generated case verifies this complete rule; implementation and cases are required before release (Section 8.3, Amounts recovered by a repair; wire 5.5).

### Checking a newer ledger

The executor checks a newer ledger version against the last version it accepted and the terms of the `allowances` record it holds, at each service time from the version it accepted to the newer write's, before it builds on it; an app that only reads the ledger flags a newer version that counts less once the reference client hands that check the write's service time (Section 8.3, From one version to the next; wire 5.5): `paykit-lib` on `v0.2` implements the pure check of a newer version against a held one over that range of service times (`check_successor`), and `allowance_usage.json` `compaction` `successors` holds its generated cases; obtaining and authenticating the two service times, handing them to the check and acting on its verdict are rules of an app's run.

## Manual payments, settlement and recovery

### Manual-only occurrence

`manual_only` is set only on a payer's record, the request record of a one-time or a recurring request or the period record of a Billing Period, as `true` or absent (wire 5.3; `sealed.json` `activity_records`), and a later write never drops one seen.

### Manual-only first write

A period with no period record yet is marked by the write creating it, built on no version, and a payer's period record may hold the mark and no message (Section 8.3, Manual only; wire 5.3): `paykit-lib` takes such a record, which no vector shows.

### Block floor

`block_floor` is one record per identity under `paykit_pay` using the permanent record policy (`"permanent": {"generation": null}`) in every generation (Section 6; Section 8.1, Blocked counterparties; `specs/v0.2/wire-format.md` Sections 5.1 and 5.6). Required library checks cover its value, its binding to the own identity under the permanent policy, ignoring a later write of a lower floor, and ignoring any unknown `lost_to_delete` member while preserving unknown members on rewrite. A wallet refuses to delete `block_floor`, and the Dispatch service enforces deletion refusal for permanent `generation: null` records in every generation (Pubky Dispatch spec, Section 10.3.1).

Unblocking a peer does not authorize a payment: a one-time request from before the unblock stays manual, and a later request still has to satisfy the ordinary payment authorization rules (Sections 7.2, 8.2 and 8.3). Required wallet outcomes cover: an old one-time request created earlier than the block floor plus the clock tolerance staying `manual_only` with zero automatic handoff after unblock and across reopen or restore; a valid later one-time request created at or after the block floor plus the clock tolerance that satisfies the ordinary payment authorization rules paying once; a previously seen live `block_floor` disappearing during continuous account history (signed `404` to an authorized reader, covering listing omitting the key, or authenticated `{0, 0}` create), or any authenticated delete of `block_floor` in any generation, raising the rollback alarm, keeping the highest floor seen and holding automatic one-time payments and repeats across reopen (while an unauthenticated delete is ignored without treating it as absence, and with none seen counts as an unreadable floor); and absence of `block_floor` covered by a declared provider loss (when `recovery_from` covers the last generation in which the app saw the floor live) preserving the separate provider-loss rules (Section 7.3) without lowering a previously seen floor. Also cover divergent recovery after a declared provider loss: a lower current `block_floor` at the replacement service alongside a hidden higher uploaded floor (`divergent: true`), followed by another writer's successor from the lower floor that clears `divergent` to `false`, reopen, and backup/restore. Once the wallet observes `divergent: true` or `RecordDivergent` for `block_floor` (including when `block_floor` is not currently held locally or the live view already shows `divergent: false`), it must keep a durable divergence hold, refuse to write a `block_floor` create or successor itself (applying unblocks and widenings that must raise the floor without the raise only while the shared post-loss hold also stands at it, wire 5.6, Unblocking under the divergence hold), never clear or drop the hold on the cleared successor, reopen, restore (including in-place restore from an earlier backup), a later provider loss, scope changes, a `missed` item, a refused certificate or user confirmation, keep marking incoming one-time requests `manual_only`, and hold every automatic one-time handoff and every resume repeat of a one-time payment (including when the user approved the original handoff manually) indefinitely under current Dispatch APIs (while ordinary manual payments, recurring Billing Periods, payments without a request, stops and receipts continue under their own rules, and a non-divergent `block_floor` on a loss-free identity still permits ordinary authorized one-time payments). Divergence arises only in a generation after a provider loss, where the shared post-loss hold also stands, so these cases observe the divergence hold by its own effects: `manual_only` marks, refused `block_floor` writes and unblocks and widenings applied without a raise. Under both holds, an unblock the wallet writes, an unblock another own app wrote that its read of the lists shows, and the widening of a restore that isn't clean and of a `missed` item each apply at once with no `block_floor` write by this wallet: its own unblock is written; for each unblock the sender's entries are taken and its asks queued; a widening's marks are applied, and their asks go once the counterparty is unblocked; a one-time request from the blocked window that comes back opens `manual_only` at this wallet and pays only by hand. A post-loss wallet that holds no divergence hold still raises `block_floor` at its unblocks and widenings while the record is not divergent. Under a divergent `block_floor` view, a `RecordDivergent` for its key or a retained divergence hold while the shared post-loss hold doesn't stand (`recovery_from: null` and no recorded loss), as only a service not following Dispatch spec 12.6 shows, the wallet applies nothing without a raise: an unblock it writes waits as for a floor that can't be read, for an unblock it didn't write it records it in its copy of the lists together with a pending raise for that sender that survives restart and restore, holds that sender's one-time requests and its asks until the raise is applied, and takes its other entries under their own rules, the widening stays unapplied, no `block_floor` is written, and a one-time request from the blocked window gets zero payment-method hands off at every own wallet. For the joining wallet: the `block_floor` view at the replacement service has been cleared to `divergent: false` by an earlier write before the unblock; this wallet is the only own app holding `paykit_pay` at the unblock; and the request's signed `created_at` is at or after the current `block_floor` plus the clock tolerance. An own wallet that joins after the unblock then holds no divergence hold, no deny list entry and no mark of that sender, and the floor is not raised: that request, read first by the joining wallet, opens there without `manual_only` and, although otherwise eligible for automatic payment (as under an Allowance whose executor the joining wallet is), gets zero payment-method handoffs, by the shared post-loss hold alone; each later one-time request is also held from automatic payment at both wallets (wire 5.6, Unblocking under the divergence hold). Also cover the shared post-loss hold on automatic one-time payments using the `recovery_from` of the verified current own `identity.json` (Sections 7.3, 8.1 and 8.3; wire 5.6): (a) a loss-free cooperative move or key change in place (`generation > 1` with `recovery_from: null` and no recorded loss) causes no hold and lets an eligible later one-time request at or after `floor` plus the clock tolerance pay automatically once; (b) after any declared provider loss, even when all available apps agree on the same low `block_floor` with no divergence reported (and with a late recovery upload), and after an empty upload, an elapsed wait or a signed app removal, every own app holding `paykit_pay`, including one that joins, is restored or takes a new certificate after the loss, judges by the newest own `identity.json` it has verified and a `block_floor` answer signed for the service and generation it names at final handoff authorization and holds every automatic one-time payment with zero payment-method handoffs, tells its host of the hold once and again after each reopen, and keeps a recorded loss across restart, scope changes and in-place restore; (c) a loss, then a move, then a loss (a declared provider loss, then a cooperative move or key change in place where `recovery_from` stays non-null below `generation - 1`, then a second declared provider loss) keeps the hold across every stage; (d) a later one-time request created and accepted after the loss and its residual window remains held from automatic payment (including after Allowance reconfirmation); (e) a resume's repeat of a one-time payment its user started, after recording a durable attempt and restarting or reopening, is withheld with zero payment-method handoffs while reconciliation under Section 7.3 proceeds by history lookup; (f) a fresh-device restore from a pre-loss backup judges by the newest own `identity.json` it has verified at final authorization and holds automatic one-time payments with no recorded loss; and (g) manual initiation of an otherwise eligible post-loss one-time request still follows ordinary uncertainty and no-double-pay rules and pays once without clearing the hold, while reconciliation, recurring Billing Periods, payments without a request, stops, receipts and separate observed-divergence protections continue under their own rules. A fresh app that can read only a pre-loss `identity.json` served by the lost provider during the residual window is outside these cases (wire 5.6, Shared post-loss hold). `identifiers.json` holds the permanent `block_floor` policy and its hash, `paykit-lib` checks identity binding under that policy, and the SDK's runtime tests cover the wallet's deletion refusal, continuous-history disappearance and the divergent-floor hold across a cleared view, reopen and restore in place and on a fresh device. The SDK's runtime tests also cover the shared post-loss hold across a later key change and a second loss, and unblocks applied without the raise under both holds.

### Permanent settlement bucket

`settlements` uses the permanent record policy in every generation (Section 6; wire 5.1 and 5.7). The Dispatch supports this policy and compares the current write of a recovered record. Paykit derives the permanent bucket key and reproduces the policy hash, bucket keys and sealed values from its vectors. Its SDK applies a marked credit with the signed receipt in one conditional batch. Two payee apps that name different occurrences for one settlement can apply only one such credit.

### Seller certificate

`paykit_request` and every labelled selling scope require `paykit_settlements` and `paykit_contacts`, so a payee app's certificate holds all three (Section 3.1, wire 3.1): `paykit-lib` on `v0.2` checks these requirements. The `documents.json` `certificates` entries for payee apps, including `hosted`, hold both required scopes; `documents.json` `payee_certificates` includes accepted plain and labelled selling certificates and rejection cases missing a required scope.

### Requests below the block floor

A one-time request whose signed `created_at` is earlier than its sender's block mark's latest time, or the block floor, plus the clock tolerance, opens its payer record with `manual_only`, also when that record is written later, and once a read that opened it without the mark is done, the block floor being the identity's `block_floor` record, raised only as a blocked window closes, by each app once, or as a widening applies, never per message and never at the ask, and, except under both holds (wire 5.6, Unblocking under the divergence hold), as follows: by a Paykit App holding `paykit_pay` that removes a sender from the deny list, to the service time of its latest verified read plus the clock tolerance before it writes the unblock, which it doesn't write if the raise fails; by each app holding `paykit_pay`, for an unblock it didn't write, as its read of the lists shows it, to the same time, the app recording it in its copy of the lists together with a pending raise for that sender that survives restart and restore, holding that sender's one-time requests and its asks until the raise is applied, and taking its other entries, such as period details, request acceptances, proofs, receipts, stops, Private Payment Lists and recurring requests, under their own rules, so a copy that comes back unasked after the unblock opens marked in every own app that saw the block, by a restore that isn't clean to the time of its first read of the lists and by a `missed` item to the earlier of its last acceptance time and the service time of the read that passes it, each in the commit that applies its widening, every one-time request judged against the time of each raise owed and not applied as well, and a widening applied only with its raise, except under both holds, under which the unblock and the widening apply without the raise (wire 5.6, Unblocking under the divergence hold); an app without `paykit_pay` raising nothing and not holding unblocks, a sender's one-time requests or widenings waiting for a floor raise (while keeping any recorded loss, `block_floor` divergence hold or seen floor across scope changes); every unblock moving every unpaid one-time request created before it to payment by hand; an own app that joins after an unblock no app holding `paykit_pay` raised the floor before writing, before any own app holding `paykit_pay` that saw the block raised it, isn't covered (for one applied without the raise under both holds, the shared post-loss hold covers it); a recurring one opens with no mark; no app pays a one-time request so dated automatically, whatever its record holds, judged at final handoff authorization by the newest own `identity.json` the app has verified and a read of the `block_floor` record immediately before each automatic handoff, accepted only when signed for the service and generation that document names, the shared post-loss hold, an own `identity.json` that can't be read or equivocates, continuity unverified, an answer not so signed, a read that fails, doesn't read, shows an unauthenticated delete with none seen, shows a divergent `block_floor` view or raises the rollback alarm, or a retained `block_floor` divergence hold, withholding the handoff; and a floor an app can't read holds every one-time request (Section 8.1, Blocked counterparties). While the raise for an unblock it didn't write isn't applied, its `block_floor` write meeting `409 conflict` at each try, an app holding `paykit_pay` takes that sender's `payment_period_details` for a subscription it accepted under their own rules, holds a one-time request from that sender created before the unblock unprocessed, and keeps an already-open one-time request of that sender created before the block paid only by hand; at its next receive, the write succeeding, the raise is applied and the held one-time request opens `manual_only` (wire 5.6, Raising). The retry of an unwritten raise, such as over a floor value that doesn't read with none seen, follows wire 5.6, Raising. After a try that leaves the raise unwritten for a cause other than the divergence hold, the app tries it again at its next receive; after a second such try in a row, it waits before the next: one minute, doubling after each further such try, up to 10 minutes, the age the deny list may have before a payment the user starts (Section 8.1, How fresh the deny list is). Whatever the wait, it tries again at its next receive once the floor record it holds changes or the floor becomes readable, its payer records listed or the scope key it waits for held. A `409 conflict` neither starts nor ends a wait: the app has read the newer record and builds anew at its next receive. While the raise waits, the app reads neither the lists nor the floor for it, and tells its host. A raise the divergence hold keeps from being written is not tried again while that hold stands alone; it goes without the raise once the shared post-loss hold stands too (wire 5.6, Unblocking under the divergence hold).

### Executor contacts

`paykit_allowance_execute` requires `paykit_contacts`, so an executor's certificate holds both (Section 3.1, wire 3.1; `documents.json` `executor_certificates`).

### Payer contacts

`paykit_pay` requires `paykit_contacts`, so a paying app's certificate holds both (Section 3.1, wire 3.1; `documents.json` `payer_certificates`, and the certificates of `messages.json` `cancellation_roles` and `sealed.json` `allowance_records`).

### Unrequested amount

The record of a payment without a request keeps its amount in `unrequested.amount`, the stored proof's (wire 4.10, 5.3; `sealed.json` `activity_records`).

### Preserving unknown members

A writer that writes a shared record again keeps every member it doesn't know, each object's as it was read (wire 5; `sealed.json` `activity_records`, the record of a payment without a request with members its reader doesn't know).

### Payment time lower bound

The payee's lower bound on the verified payment time, which an own app that didn't record the request takes from the request's signed `created_at` (Section 7.1).

### Missed entry

After a `missed` item, an app holds every open request and Allowance as after a restore, makes no automatic attempt or acceptance and repeats no handoff made before the item, counts no confirmation given before it, holds a request first seen after it only if its signed `created_at` is earlier than the item's last acceptance time plus the clock tolerance, with nothing releasing the hold automatically, and asks each counterparty of an open request or Allowance, and of its unfinished activity records, to re-send, never from before its cursor's time less the clock tolerance (Section 7.2, What a `missed` item loses).

### Missed block window

A `missed` item marks every counterparty its deny list holds at its next read of the lists as blocked from the item's earliest to its last acceptance time, and asks it once it is unblocked, raising the block floor to the earlier of that window's last acceptance time and the service time of the read that passes the item in the commit that applies those marks, except under both holds, where it applies the marks without the raise (Section 7.2, What a `missed` item loses; wire 5.6, Unblocking under the divergence hold).

### Missing debit history

After the payer's own provider loss, incomplete debit history holds payment of the exact request or Billing Period (Sections 7.3 and 8.2). A required wallet case pays a period early by hand, recovers the payer while its start is still in the future and keeps the original debit count when the earlier outcome cannot be established. A matching settled claim or verified receipt retains its effect. A payer backup restore with continuous claim history, a payee's provider loss with the payer's history intact and a genuinely later request follow their ordinary rules. A one-time request payable before the loss is refused for payment by hand with the doubt shown; a required case consents to one attempt, which is made once, records no statement and leaves the request in doubt until its claim is SETTLED or a receipt verifies it. Another required case confirms everything the user can for the same request, and no automatic payment or repeat follows (Section 7.3).

### Different activity at a held version

Under Section 6, Different writes at the same activity version, hold activity that depends on a different signed write at the held version. The Dispatch revision adopted here reports when its selected write differs from the signed write the app holds at the same version. Paykit conformance requires a payer and a payee to hold the payment, credit or record update that depends on a disputed request or Billing Period, including a period that depends on its disputed parent, while unrelated activity continues. A reload must preserve the hold, local stops and the outcome of a payment already made. A backup restore must read the service before acting and cannot reconstruct a missing comparison from the journal. When the original signed write is selected again or a successor is accepted, the app must recheck the current record and retry an eligible retained message once with its original ID. These are SDK runtime cases. The shared wire vectors and the Dispatch client's tests do not exercise the Paykit effects.

### Divergent record

While the latest view of a request, period, Allowance or ledger record shows it divergent after a provider loss, nothing is paid or accepted automatically on it or under it, a request record's pause holding every Billing Period of its request, a payment by hand of the occurrence waits for the user, and a payee app credits no proof against it until its user confirms; while a `payment_lists` record's view does, no endpoint of its list, single-use or reusable, is paid automatically, and one by hand only after the user has seen that the list may be stale, the upload possibly holding a newer list; an app that has seen a record divergent keeps the pause until its user confirms; and while a view is divergent, only a stop or a write the user confirmed goes over it, a receive-step write that adds no stop being kept until then and an app's own write, such as period details or a receipt, waiting for the user; for `block_floor`, seeing a divergent view or divergence event sets a durable hold across cleared views, restart, restore, a later provider loss and scope changes (separately from the shared post-loss hold of Section 8.1 and wire 5.6) that no compliant writer creates or writes a successor over and no user confirmation lifts or drops, holding every automatic one-time handoff and every resume repeat of a one-time payment (including when the original handoff was approved manually) indefinitely under current Dispatch APIs (Sections 6, 8.1; wire 5.6).

### Retaining divergence

A record is seen divergent whenever a view read showed it so, also one a later write read in the same pass of the feed cleared; for records other than `block_floor`, the app keeps a mark of each, with its collection, key, the write shown, whether its user confirmed it and whether that confirmation was shown for a credit, a payee app keeping a counterparty record's mark past a cleared view until it was; a confirmation is of the write shown, confirms nothing for another, and holds for that write alone; a confirmation of such a record no longer held ends its pause and drops its mark; a mark that matches no write the app holds keeps the record at its key paused until the user confirms, lifts no other pause and stops no other record; a backup carries the marks and a restore installs each one unconfirmed, while a `missed` item sets, lifts and undoes none, and an app under a certificate Paykit refuses still keeps them (whereas `block_floor` keeps its non-confirmable durable divergence hold across cleared views, unheld records, restart, reopen, restore, a later provider loss, scope changes, `missed` items and refused certificates, with no user confirmation lifting or dropping it: Block floor, Divergent record, wire 5.6); a receipt over a divergent payer record, or over one whose kept messages wait, is kept with the claim `done` it carries evidence for, whether or not the app can read the record, a receipt of a payment without a request under the spam rule when it can't, and both are written only once the user has confirmed the record and the record reads, a receipt kept as a reply before its record counting, once the record reads paused, as a message kept over it; and what records a payment already handed off, a one-time request's acceptance not yet sent, the outcome and the proof, waits while the payer record is paused, until the user confirms, and then goes once the messages kept over the record are written, also while the view still shows the record divergent, the claim's `done` waiting with the outcome and a `failed` going at once, alone; a kept message is given up after its 30 days also while its record's divergence is unconfirmed; messages kept over a divergent record are, per role, the earliest accepted and, of each sender, the 16 latest after it, at most 34 in all, the first receipt from the record's counterparty for each attempt the app's journal or the claim shows aside, which is never dropped, a message dropped counting in its sender's mark and asked for again (Section 9, Messages dropped for a bound), a counterparty's never going for another sender's over a record that reads, and the list of the highest version, written in inbox order and placed as the receive step places them after the confirmation, every later write that adds no stop waiting behind those accepted before it, one that no longer points to its record not written and given up at its 30 days, nothing waiting for it, before anything is paid, accepted, credited or marked done on the record; a confirmation of a divergent record counts for the credit check only if it showed the user that the record may hold a credit its current write lacks, every payee record checked, one it can't read counting as the counterparty's while it is paused, divergent or not confirmed for a credit; and when every list of a payee that would otherwise be usable is paused, a payment by hand waits for the user or a choice of public endpoints, and no publishing app whose list is paused is asked for a newer one, a list of an app `identity.json` doesn't list in `apps`, or of a higher generation than that copy, never one of them and a listed app's paused list pausing whatever certificate it was signed under, and a request that no confirmation could let be paid is refused for that rather than paused (Sections 6, 7.2, 8.1, 8.4).

### Divergence notice

An app sets its mark from the client's divergence event by the record's key, never by the event's ID, acknowledges the event once the mark is kept, or with no mark for a record it no longer holds (except for the identity's derived `block_floor` key, for which it always sets or retains its durable divergence hold before acknowledgment even when `block_floor` is not currently held locally or the current view already shows `divergent: false`), and judges other records' state now from their current flag, and a write over a record held as deleted, or over a delete the app refused, names the delete's version (Section 6, Divergent records, Writing over a delete; wire 5.6).

## Credit, destination choice and admission

### Concurrent settlement credit

Independent payee apps that verify one settlement for different occurrences, including requests from different payers, can both find it unmarked. If both credits write a mark, the batch that first applies stores the receipt and the settlement mark. A conflicting batch stores and queues nothing. Two hand-confirmed receipt-only credits of different occurrences can both apply while a full bucket stays current. Its app reads the current bucket and the occurrence again and repeats the credit decision. An app that retains a valid prior receipt or a seen settlement mark still refuses another credit of that settlement after restart or restore (Section 8.1 step 4; wire 5.7). Exercise the conditional mark and receipt with two independently enrolled payee apps.

### Protected payment history

A payer never deletes its record of a paid obligation without a deadline and a payee app never deletes a record that holds a settlement it credited; a visible delete of a potentially protected activity or payment-list key remains a hold at an app with no seen state, and a replay cannot create unmarked authority over it. The service refuses deletion of permanent `block_floor`, `settlements`, accepted Allowance and ledger records. The disappearance of any of them after the app observed it in the same account history raises the rollback alarm. Deletion of an ordinary pending proposal follows Section 8.3.1. A new app and an old-backup reader must reject before a claim attempt, payment or credit while a listed witness's protective publication is blocked, and a concurrent recreation cannot clear the hold. Required rejection cases include an absence read followed by another app's create and delete before publication, a put-back for one delete overtaken by a different delete before it applies, a no-seen-state reader of an unmarked recreation and a reader of its later unmarked rewrite; each must stop before authority or a claim attempt, also after an old-backup restore and while an honest listed witness's repair is blocked. The positive paths remain withheld pending the complete generic wire contract and its pin; future positive cases require publication-time authenticated absence for genuine creation or a proved protective put-back, followed by authenticated exact-predecessor continuity preserving every restriction. An app with seen state writes only the protective put-back Section 6 requires (Sections 6, Deleting finished activity, Creation and later writes, and 8.1, Records not listed yet).

### Blocked sender recovery

A denied counterparty's payments wait for the user, and an app sends a blocked counterparty no re-send request, asking it again for what it still owes to ask once it is unblocked, and no rejection or payer's cancellation, which it records at once and sends only once its copy of the deny list is known and doesn't hold the payee, deciding nothing about the held message while the request record is paused as divergent and unconfirmed, and giving it up, the host told, once the record reads unpaused holding neither it nor any stop, and, holding `paykit_contacts`, no new request or period details, which it refuses also while its copy of the deny list isn't known, while a payee's cancellation, a receipt and the acceptance and proof of a payment already handed off still go when first sent; nor does it send such a counterparty an Allowance proposal, acceptance or rejection, refused also while its copy of the deny list isn't known, while a user's eligible Allowance end goes when first sent and on queued retries, and the end it makes itself for a withdrawal crossing its acceptance is recorded at once with its notice to its own apps, only its message to the counterparty waiting on every delivery route, including kept batches and restore, until sends aren't held, and going only until 30 days after its `created_at`, less the clock tolerance, the end in the record standing either way (Section 8.3, Blocked counterparties); a Billing Period paid without an Allowance carries no `allowance_id`, and one paid without the user's confirmation follows every rule for an automatic payment (Sections 8.1, 8.2).

### Dropped blocked message

A message counted because the user blocked its sender is dropped as one never processed, its pair forgotten once the read is done, and counted in one mark per sender identity of the earliest and latest acceptance times, at most 128 but for those of senders the deny list holds, which its own limit bounds, one still naming pairs to forget never going, one only a restore that isn't clean or a `missed` item made going first and any other only once an ask covering its latest time was delivered, or, with none such, the earliest of those whose ask was refused or can't be sent yet, a message whose mark names 64 pairs to forget held until they are forgotten; the mark asks the sender to re-send from its earliest time to its latest only once its pairs are forgotten, no own block or unblock of the sender is still to be shown, and a read of the lists made for the ask shows the sender neither blocked nor flagged, again at most once in any 24 hours until 30 days after its latest time, a delivered ask ending its window; a message counted only because its sender is flagged is not marked; and the host is told, once per sender and window and with no content, of a window that expires, or that the bound drops, before an ask for it was delivered, and of the part of one past its sender's 30 days at its first ask (`specs/v0.2/wire-format.md` Section 4.1; Section 8.1, Blocked counterparties).

### Period details intake

The payee sends period details only after the payer's acceptance, from an app that writes them into its period record first and sends none past 8 for a period; the payer stores them only for a request it has accepted and a period not shown paid that is the next to start, or has started and, with a deadline, whose deadline hasn't passed, or, without one, is one of the 16 latest to have started, and the payee sends them only for such a period; it takes those of a rejected or cancelled request with no effect, and pays by none its period record doesn't hold, by default the latest that carries both the destination and the rates the payment needs, never one's destination with another's rates; a period paid without the user's confirmation is never one whose period record holds `manual_only`, and its batch writes that record at the version read; a period whose claim holds an `unknown` attempt stays listed and refuses a cancellation, as a one-time request with one does; accepting a subscription takes the restore and `missed` holds and the deny list as paying does, and a period record of an accepted request is an update, not new activity, under the spam rule (wire 4.1); a payee app stores a receipt at every credit, sending it or not, credits a proof only if no other occurrence it can verify across counterparties holds a proof of the same settlement with a receipt, and credits none while it can't open such a record, one shows divergent or its copy may be incomplete, unless its user confirms; two independently creditable payments that the payment method names as two settlements are both credited, and a replay the method names as the same settlement is credited once, however many transactions carry them; under `per_period_endpoints` it credits a later period only for a payment to the destination of period details its proof names; a payee app credits a payment at their rates only once it holds them; a rate-priced payment under the payer's own policy requires authorization of that occurrence's exact quote under Section 7.2; and a recurring request has ended once its last Billing Period has, on the highest verified service time, and may still be accepted then (Sections 8.1, 8.2).

### Single-use list consumption

Paying a single-use endpoint consumes its list's version, and a payee answers a list request naming a used version, an expired one, or none (Section 8.4).

### Acceptance after resolving a payment

The acceptance is also sent once the user marks as paid a payment that may have been handed off, never after a failure the method reported (Section 7.2 step 2), and the first recording batch after paying without the Dispatch uses a fresh service time only once every possible earlier batch is accounted for as never applied and unable still to apply, preserving an existing attempt's step and pinned deadline instead (Section 8.4, Late recording; wire 5.3).

### Paying without the payer service

Paying without the own Dispatch is only to a public endpoint, never to an endpoint of a Private Payment List, and only on a copy of the deny list read at most 10 minutes before (Section 8.4, Without the Dispatch).

### Identity freshness before handoff

Destination checks in Sections 4 and 7.2 use the source and time of the observed identity version. For the payer's current Homeserver version, an age of zero or 59,999 ms passes the time check at handoff. An age of exactly 60,000 ms fails. A backward clock change observed during the read or before handoff invalidates that observation until a fresh read succeeds. A signed message destination needs a current observation of the exact payee version, including a genuine observation at time zero. A missing observation or a read of an earlier version after a provider move cannot qualify the newer version. For a successful public lookup, an app-record read that begins or finishes outside the shared interval must be repeated. An app-record read entirely inside the interval still cannot authorize handoff at its end. If the Dispatch observation expires first while the exact document observation remains fresh, the payer repeats the lookup before using the endpoint. For a failed lookup or one still pending after grace, a fresh Homeserver observation alone permits a public endpoint only with explicit user confirmation before any write and with the app-record read wholly within its interval. An automatic payment waits while the lookup is pending and stops on failure. A newer answer before handoff requires new app-record reads under that document.

### Destination expiry clock

A destination has expired once its expiry is at or before the later of the payer's device clock and the highest service time it has verified (Section 7.2, check the destination; wire 3.2, 4.9 and 4.13). Run each case for a public record with a finite `expires_at`, one with a null `expires_at` at `updated_at` plus `record_max_age`, a Private Payment List at its `expires_at` and at 30 days after `accepted_at`, and period details at their `expires_at`, by hand and automatically, the two clocks within the clock tolerance for an automatic payment. Required cases: an expiry between a device clock behind and the verified service time refuses the destination with no payment-method call, `failed` written on any attempt already written, and the user told that the device clock is behind the service's time; an expiry reached only by the service time in the attempt's answer writes `failed` before handoff with no payment-method call; an expiry between a verified service time behind and the device clock is refused as well; an expiry equal to the later clock is refused and one a millisecond after it pays once; a payer with no verified service time judges by its device clock alone; and a payment without the Dispatch to a public endpoint refuses a destination that the highest service time verified before the outage shows expired, with no service read.

### Published record expiry and extension custody

The record with public endpoints is published only by an app holding `paykit_request`, at most 256 endpoints, with an optional Paykit deadline; the payer enforces any finite record deadline and the signer's record age ceiling. The publisher renews past half the effective lifetime and keeps an unanswered renewal pending. An app that no longer holds `paykit_request` or no longer publishes its record signs its next record without public endpoints; an app lists identifiers in `capabilities` only while it holds `paykit_pay`, and a reader takes what a record lists (cover finite-lifetime publication and capability listing, a null deadline before and exactly at the identity ceiling, a shorter deadline including one set by another protocol, renewal, withdrawal, certificate removal and method validity before handoff, including payment without the payer's own Dispatch); extension messages are kept until taken, at an app holding `paykit_contacts` only from senders that may create state, bounded by one atomic local transaction for intake and eviction, with the same bounds validated at backup and restore, and their drops counted as Section 9 requires; implementation and tests for extension eviction follow Sections 3.3 and 8.1; an extension sender must hold `paykit_contacts` explicitly and sends no extension message to a counterparty its copy of the deny list holds, or while that copy isn't known: the send is refused, nothing signed or queued, unlike the rejection and the payer's cancellation that Section 8.1 records at once and whose messages it holds and sends once that copy is known and doesn't hold the counterparty; a payment made without the own Dispatch is journaled and checked again on what the app holds before it pays, writes nothing when the payment method reports it failed, whose history is still asked about it, waits in reconcile mode with every other new attempt, and, restored, is decided by the attempt the claim shows, with its step and `deadline`, or else by the payment method's history (Sections 3.3, 4, 8.1, 8.4, 9; `specs/v0.2/wire-format.md` Section 3.2).

### Outbound permissions

Extension-only and Allowance-only certificates remain valid for receiving; extension sends, Allowance proposals and answers, and recovery asks require explicit `paykit_contacts`, with recipients enforcing the sender table. Required implementation tests must cover missing scope (`MissingScope`, no signing, recording or queueing), scope plus a complete permitting deny copy (send succeeds), unknown or restored copies and known blocks (send refused), loss of scope or a later block before queued delivery (nothing escapes), and unrelated protocol permissions (no Paykit authority). The user's initial terminal stops and own-app notices retain only their specified exceptions; an end reflecting a crossing response is recorded but its message waits for contacts and a permitting deny copy. Drop marks awaiting contacts stay in the ordinary bounded pool, with expiry, eviction, host notice and pair atomicity preserved; saturation must not create an unbounded authorization wait or stop intake (Sections 3.1 to 3.4, 8.1, 8.3 and 9). Exercise receiving with the Allowance scope alone, and retry the same proposal, rejection or end after changing the app’s scopes. A backup must preserve the original end’s origin even if the service applies the write and the reply is lost.

### Bounded recovery obligations

Bounded message eviction guarantees the atomic recording of the recovery obligation with the required payload and processed-pair changes, not eventual payload recovery. Implementation tests must cover an extension-only receiver that cannot send its ask, expiry and capacity retirement of its undelivered mark with the required host notice, continued intake without a separate Paykit payload queue, and a delivered ask whose answer is unavailable under sender retention or re-send limits; neither an ask nor a notice establishes recovery. A permitted re-send within retention remains eligible under the pair-forgetting rule. The ordinary mark lifetime and retirement policy remain unchanged, so an evicted payload may be lost permanently (Sections 3.3, 8.1 and 9): required runtime cases, not existing library or vector checks.

### Ordinary admission

Strict ordinary admission keeps each existing family within its stated count bounds and preserves its required victim: a protected earlier victim sends the unadmitted input through plain waiting or, when the same cancellation also imposes an immediate payer stop, waiting with designated local custody under the adopted Dispatch Section 8.2. The dual-role wallet must retain the signed stop through reopen, replay and restore, admit the seller-side reply once when room returns, and still place the original stop after the waiting input expires or is selected for immediate overflow. The overflow must durably record its payload-free notice with the stop, with no processed pair for that input. Required implementation tests must cover both 64-per-sender and 128-total saturation with a genuinely uncertain prior effect, unchanged prior payload and pair, an eligible prior victim and an incoming victim, all three client waiting reasons sharing atomic custody bounds, fair later retries without resetting identity or age, current authorization and deny checks, preserved mandatory stops and protected first receipts, actual custody or notice persistence failure, expiry and overflow notices counted once in the existing dropped-reply mark, crash and restore between notice creation and acknowledgment, backup age preservation, successful stored-duplicate retirement and independent uncertainty remaining protected. The existing ask limits, sender retention and undelivered-mark host notices still apply, and no test may infer effect resolution or payload recovery from a custody drop (Sections 3.3, 8.1, 8.3 and 9; Pubky Dispatch spec, Sections 8.2 and 11). The SDK tests on Scripted and PostgreSQL cover the dual-role stop at the per-sender bound, admission after reopening, expiry after restore and eviction of an already waiting reply. Dispatch tests separately cover immediate selection of the incoming reply. The Paykit transaction that records the dropped-reply mark and acknowledges the notice, and its crash and restore tests, remain outstanding. These are runtime checks.

### Claim for a payment without a request

Library and vector checks: for wire 1.5, require the blinded input, `key_material_hex` and obligation ID for a fixed payer naming key and 16-byte attempt ID (vectors: `identifiers.json` `obligations`). Two wallets of the same payer derive the same claim key for the same attempt. Changing the payer's key or the attempt changes the result.

Required runtime tests: for both public and private online payments, verify that a lost submission reply, restoration and a late verified receipt use the original claim and attempt with no second payment call. A second authorized wallet uses the payment's `payer_activity` record to find that claim. Check the live owner and the orphan resolution allowed by Section 7.3, including a receipt with the wrong sender or attempt that leaves the claim unresolved. For an eligible offline public payment, verify late recording against the same derived claim.

Hold a claim carrying the attempt's `attempt_id` only at `derive("paykit/v1/obligation", "payment", attempt ID)`, with the attempt ID not blinded, and no other evidence of settlement. In each payment mode the recovering app neither reads that key nor takes that claim as the payment's: it finds no claim at the obligation of wire 1.5, leaves the attempt unresolved and makes no further payment call (wire 1.5).

### Recording an offline payment

Late recording of a payment already handed off without the Dispatch must be tested with authoritative success more than ten minutes after the first returning answer, reconciliation lasting longer than that, quota refusal and an expired first submission; each established uncreated claim can still be recorded under the original attempt ID without another payment call. Tests must also cover an applied batch with a lost answer, an existing open attempt completed after its pinned deadline, a failed or forgotten claim corrected through its record and proof, and a restored journal predating a submitted batch. Missing answers, an absent claim after provider loss or forgetting, expired batch evidence and non-authoritative or expired method history must not authorize a fresh attempt, a refreshed pin or a repeat handoff; unknown outcomes remain accounted for (Section 8.4, Late recording; wire 5.3): required runtime tests, not library or vector checks.

## Receipts, held messages and retry

### Payee settlement checks

The payee expects the amount the payer's rule gives for the endpoint its proof names, with its own payment method's smallest unit for that identifier, and a payment to an endpoint the terms leave unusable pays no occurrence and gets no receipt; a payer's payment method names the unit of every endpoint it pays, an endpoint whose unit it can't name being unusable for a payment of a request, as is, under conversion terms, one of another asset no rate prices or one whose identifier isn't `<asset>-<rail>-<format>`; a payee app whose payment method can't name the endpoint's unit credits no proof to it; and an endpoint of the requested asset no rate prices needs no confirmation of rates and is paid automatically only where it otherwise may be (Section 7.2 step 1).

### Receipt after claim expiry

A receipt that comes once its claim is gone is still recorded in the payer's activity record, and an app that holds the failure, from its own journal or a view of the claim that showed it, tells the user that a payment marked failed was paid, while one that never saw the failure shows nothing (Sections 7.2, What a payment method declares, and 8.3).

### Blocked handoff repeat

A resume's repeat of a handed off payment is withheld while the deny list holds the payee (Section 8.1, Blocked counterparties); a payment the user starts judges the deny list as read at most 10 minutes before, an automatic payment reads it again before each handoff and an automatic acceptance before it is made, a block the app wrote counts at once, an unblock it wrote counts at once for its payments and sends and in the spam rule only once its copy of the lists shows it, a restored copy counts as never read, and a failed read never counts as one that doesn't hold a counterparty (Section 8.1, How fresh the deny list is).

### Manual-only hand off repeat

A resume makes no repeat of an automatic attempt's payment of a request whose record holds `manual_only`, the repeat of an attempt its user started being judged as any repeat of a payment the user confirmed (Section 7.2) except under the shared post-loss hold, and no repeat of a payment of a one-time request while the shared post-loss hold stands, as the newest own `identity.json` the app has verified shows it, or dated before the block floor as a read of the `block_floor` record made immediately before the repeat shows it, whatever its record holds, an own `identity.json` that can't be read or equivocates, continuity unverified, a `block_floor` answer not signed for the service and generation that document names, a read that fails, doesn't read, shows an unauthenticated delete with none seen, shows a divergent `block_floor` view or raises the rollback alarm, or a retained `block_floor` divergence hold, withholding the repeat, the attempt staying open until the payment method's history or a receipt settles it (Section 8.1, Blocked counterparties; Section 8.3, Manual only).

### Early cancellation

A payee's cancellation is kept as the stop at once, whatever the request record's value, never waiting for a scope key and never lifted by giving its entry up, and dropped once the record holds a cancellation; a stop for a request whose record the app doesn't hold, or holds as deleted, is gated by the spam rule, kept at most 64 per sender identity and 128 in all, counted apart from kept replies, and may be dropped 30 days after its acceptance, but never while the copy may lack records or a stored message of the request waits to be written, when it counts outside the bounds and is never evicted (Section 8.1, Messages that cross).

### One response per request

A payer app sends at most one response, an acceptance or a rejection, to a request, and only while the request record holds neither a response nor a cancellation: after an acceptance it may cancel, after a rejection it neither accepts nor cancels, and after a cancellation it sends no response; it judges this from the request record, so a response another own app recorded counts, and sends none while its copy may lack that record or can't open it (Section 8.1, Rejecting and cancelling).

### Replies before a record

A payee app keeps a payer's reply that comes before the request's record, and a payer app a payee's period details before their request's record, and an app an Allowance acceptance or rejection before it can match the proposal, with disposition under Section 8.3.1, and one from the counterparty of a record it holds but can't place it in yet, outside the spam rule, holding the entry while a payee request record that doesn't open for good can't show the counterparty, counted together, for at most 30 days, 64 per sender identity and 128 in all, drops past a sender's 64 that sender's reply accepted earliest and past 128 that of the sender holding the most, the lower identity on a tie, without processing it, keeps one mark per sender identity of the earliest and latest acceptance times among the replies it dropped, each until 30 days after its latest time and at most 128, the one whose latest time is earliest among those an ask covering whose latest time was delivered going first, then among those whose ask was refused or can't be sent yet, the host told, carried by a backup, and asks that sender to re-send from the earliest time at once, whatever records it holds, again at most once in any 24 hours while the mark lives; no received message is written over a held record's value the app can't read: a reply over such a payee request record is kept as a reply before its record under the spam rule and placed once the record reads naming its sender, a request over such a payer record is held until it reads and taken with no effect at 30 days, the host told, and a proof of a payment without a request over such a payee record is kept the same way under the spam rule, going with no notice once the record names another sender and, dropped for the bounds, asked for again as any reply (Section 8.1, Replies before their record; Section 9).

### Inbox order

Kept messages are ordered by their acceptance time and, within one millisecond, by the order the inbox accepted them in, never by `msg_id`: the app processes kept replies to one record in that order, including both orders of an Allowance acceptance and rejection under Section 8.3.1, and an Allowance's kept ends, every distinct one kept while it can't be checked against the record, are taken in that order once the record reads, the first that names what the record holds ending it (Section 8.1, Replies before their record; Section 8.3, Ends reach the executor).

### Stored uncertain effects

A message of any recorded kind (wire 4.3) whose effect the service refused for good, whose record write met a quota or the value limit, whose record's view shows it divergent, or whose effect's fate couldn't be known, has its effect retried later under the rules for its kind, including the pending and permanent record rules for Allowance messages (Section 8.3.1), without writing over a record value the app can't read or open; a kept message may be given up 30 days after its acceptance, judged on the service time the app verified, the host told, but never while the app's copy may lack its record, unless the client drops it while it waits for a scope key (below), and what it then loses is stated by its kind; giving up never lifts a stop; a released copy is declined once its effect has been applied; and entries dropped unverified are asked for again by a mark of their own, at the same cadence until it expires (Section 8.1, Stored entries).

### Unknown first delivery

When the client gives up a message this app sent before its store saw it accepted, and an activity record of this app holds it in one of its roles, the app tells its host once, naming the record's counterparty, the message and its request, and keeps the record; a terminal unknown outcome follows Deliveries of unknown outcome even for a message with no activity record; a notice of it never keeps a counterparty's message under the same `msg_id` processed (Section 8.1, Undelivered messages).

### Waiting for scope keys

A message waiting for a scope key, but a payee's cancellation and an `allowance_end`, waits in the client's store under the bounds of Pubky Dispatch spec, Section 8.2, is processed once the key comes, and, if the client drops it, is given up unprocessed with one notice to the host, whether or not the copy may lack its record, and asked for again by a mark of its own once the key is held (Section 8.1, Stored entries).

### Eviction and recovery marks

Every bound on the received messages an app keeps, on replies and stops kept before their record, on extension messages not yet taken, on messages kept over a divergent record, on entries dropped unverified or while they waited for a scope key, and on messages dropped for the user's block, drops a message as one never processed, its pair forgotten in the commit that drops it, and counts it in its sender's mark of its family; the client's admission-custody drop instead creates a durable notice whose acknowledgment and ordinary dropped-reply mark changes commit together without forgetting an independently protected pair or effect. The mark asks that sender to re-send as soon as sends to it aren't held, whatever records the app holds, a mark of key waits once the key is held, and again at most once in any 24 hours while it lives; a mark goes 30 days after its latest time, or for a bound on marks only once an ask covering its latest time was delivered, a block mark only a restore that isn't clean or a `missed` item made going first, and with none such the earliest whose ask was refused or can't be sent yet, the host told, so a bound never holds an entry; a mark waiting only on held sends counts toward no bound, the deny list's limit bounding those; an ask known to be undelivered permits another ask at most once in any 24 hours, a terminal unknown ask is reconciled under Section 9, none queued while one to that sender is in the outbox; a required victim whose drop cannot be committed is retained without admitting an incoming counted payload above the family's bound; a protected prior victim defers only the incoming admission, and an actual storage failure remains a failure; an undelivered obligation retired at expiry or for capacity is reported to the host, with no payload, and neither that notice nor an ask guarantees recovery, which remains best effort under the bounded mark lifecycle and sender retention (Section 9, Messages dropped for a bound).

### Incomplete collection inventory

A message of any recorded kind (wire 4.3), Allowance messages and period details included, whose effect is decided from whether the app holds a record of a collection the app's copy hasn't listed yet, from its join, after a restore, after a `missed` item until the app has listed again, or after a certificate that reads collections, or members of one, that the certificate it last listed under, or any it held since, didn't, or period details it would take with no effect from such a record, is held unprocessed and read again once it is listed; one about a record the copy holds builds its write on the version held, or over a refused write under Section 6 and open question 36, and is built again on a newer one; one that needs a scope key the app doesn't hold yet waits for it as Section 8.1, Stored entries says (Section 8.1, Records not listed yet). When a request waits for the sender lists, the host's report identifies Paykit, the waiting delivery and the missing prerequisite (Section 17, question 27). A later eligible receipt for an existing payment can apply while the request waits. Once the lists are available, the app retries the original request and applies it once if eligible. A failed local read or custody write follows the Dispatch storage failure rule. At day 30 an unreadable permanent Allowance record or a pending proposal of uncertain identity can still require a conflict hold. The host can distinguish expired placement with retained conflict evidence. Acknowledgment, restart and a later live write preserve the hold. An exact deletion can retire the affected pending lifetime and a lawful recreation starts a new one.

### Invitation delivery reference

Invitation delivery follows the Dispatch contract in the specification’s introduction. The receiver still verifies the sender’s selling scope and signed Paykit request. The invitation grants no payment authority.

Accept a request using one invitation and interrupt delivery of the append answer. Retry the same sealed envelope and signed Paykit request in a newly signed HTTP request carrying a different valid invite code. Require the original retained entry and the same four append result fields, `entry_id`, `accepted_at`, `directory_version` and `known_version`. Verify the fresh service signature on the HTTP response against the exact retry request, including its changed `Switch-Invite-Code` and request signature (Dispatch wire 7.1 and 7.4.3). A sibling receiver and a wallet restored from a backup taken before registration must both observe the first invitation’s authenticated hash for that entry. Verify that the second code still has zero uses after the retry and can admit a distinct request.

Also exercise ordinary admission without an invitation and refusal when the sender lacks the required scope. These are wallet conformance requirements. The invitation reference changes no signed Paykit body or stored vector.

### Queued sends

Every queued delivery and re-send is judged by the current per-message send rule before transmission, including retries, generic flush, payment resume, kept-batch resolution, reload and restore; blocked or unknown recipients retain the held kinds, including automatic Allowance ends for crossing withdrawals, while eligible user-initiated ends, other permitted stopping and payment-outcome messages and list withdrawals remain deliverable; a confirmed blocked subscription acceptance requires its exact durable confirmation and a fresh known deny decision for its original delivery only; durable origin evidence, role and effect, durable holds, failed hold updates, replay expiry and release after an unblock follow Section 8.1, Messages already queued.

## Private lists, restore and handoff

### List request limits

A list request naming the newest version sent gets a newer version, one naming an older version that hasn't expired gets the newest re-sent, answers are bounded per version named and per 24 hours, a version already answered gets the newest re-sent at most once an hour, a payer without a usable list asks again at most once in any 24 hours, and a blocked counterparty gets no list, renewal or answer (Section 8.4, Answering a list request, Blocked counterparties).

### Plain selling scope

Only plain `paykit_request` permits list publication, request intake and withdrawal, including after restore. A publisher holding its required `paykit_contacts` sends no list, renewal or answer while its copy of the deny list holds the counterparty or isn't known, keeps what is owed one it already sends its lists to until that ends and nothing else holds it, refuses a list the host chooses to send, and holds an answer or a proof's newer version past the daily bound until the bound allows, answered once; a payer sends no list request while its copy holds the payee or isn't known, and asks again once that ends within 24 hours of the request, otherwise at its next payment; a withdrawal goes at once whether sends are held or not, is sent again an hour after it at the earliest, then hourly for a day and daily, while a version the counterparty may hold can be paid, at once for a restore's withdrawal never queued, never after the counterparty's Dispatch refused it for good; and after a restore, until the host sets its list, a withdrawal also goes to a counterparty the restored state doesn't know, the request must name a held version and every newly constructed withdrawal reserves a greater pair from the per-recipient counter. Only a counter bound to its authenticated publisher supplies a recipient or allocation authority. An empty list never adds a reply grant at the publisher's Dispatch, through any delivery path (Sections 8.1, 8.4.1).

### Old private offers

The publishing app keeps what each version of its Private Payment Lists offered until 30 days after its `expires_at`, credits a proof naming such a version within that time only for a payload it offered that payer at that version, sends no receipt for one it didn't, and after that time checks such a proof by its payment method alone (Section 8.4, Only what it offered).

### Returning single-use payloads

A publishing app gives a single-use payload back to its payment method only when no payer can pay it, never one of a version a restore brought back before it was seen delivered short of the app's removal, and owes a change of its list to every counterparty that gets it from the commit that makes it until its message is queued, a version that fails to go for another reason tried again hourly for a day, then daily (Section 8.4, Giving payloads back, Publishing).

### Clean restore

After a clean restore, one the cursor the service holds for the app shows to have passed no entry since the backup (Pubky Dispatch spec, Section 12.4), an app holds nothing for the restore and asks nothing; after any other restore from a backup, an app holds every open request and Allowance, making no automatic attempt or acceptance on them, until the record shows a cancellation or end or the user confirms, and asks each counterparty to re-send from the backup's position (Section 7.2, What a restore loses): a rule of an app's run, which neither `paykit-lib`, a stateless library, nor a vector checks; the conformance case `restore_stale_backup_cancellation_stop`, below, does.

### Older backup restored in place

An older backup restored in place over the installation that kept running after it keeps the protective state the installation still holds in its own store, such as a kept cancellation or end not yet in its record, an Allowance removed from the consent list, a held answer, kept as restored, and a block not yet in the deny list, written again; the user can act on each as before the restore, such as unblocking or consenting to a new Allowance. Required cases include a request its payee cancelled after the backup, the cancellation kept but not yet in the request record, refused after the restore with the user's confirmation of the hold and then shown cancelled; an Allowance ended after the backup that stays off the consent list, the user's confirmation refused; and a block made after the backup, unanswered, written again and refusing the payment, while the user can unblock again.

### Payee record history

A payer that drops a record from its bounded history of a payee app's records, or can't keep a group at one `updated_at`, uses no record of that app at or before that time afterwards, also while that time is ahead of its clock (Section 4). Required cases: a conflict dropped from the history while the clock ran ahead is not used after the clock steps back; a member of a group too large to keep is never the fallback of a later conflict; an in-place restore keeps both; a record dated beyond the clock tolerance never moves that time; and a Private Payment List and a request's destination from the same payee still pay meanwhile.

### Publisher restore

A restored publishing app whose backup holds its list or a list it sent sends no list but a withdrawal until its host sets its list again, tells its host so once, and removes every carried list delivery, empty or nonempty, before the host can send it, durably retaining its obligation to withdraw and suppressing any later materialization from a kept batch; it withdraws with a newly signed empty version reserved under Section 8.4.1, even after a clean restore, and sends the list again only to recipients its host chooses when it sets the list (Sections 7.2, What a restore loses, and 8.4, Withdrawing a list).

### Blocked peers after restore

A restore that isn't clean marks every counterparty its deny list holds at its next read of the lists as blocked, from the backup's cursor time less the clock tolerance, or the restore's bound on the times it restores where that is earlier, to that read, asks it once it is unblocked, and raises the block floor to that read's time in the commit that applies those marks, except under both holds, where it applies the marks without the raise (wire 5.6, Unblocking under the divergence hold); a sender blocked and unblocked again between the backup and that read is asked only by the restore's other re-sends, and a one-time request of it that comes back opens with `manual_only`, but where that unblock applied without the raise under both holds, at an app that holds no divergence hold it may open without `manual_only` and gets zero payment-method handoffs by the shared post-loss hold (Section 7.2, What a restore loses).

### Restored delivery origin

After a restore, delivery from a carried outbox or stored signed plaintext requires current evidence that the exact original delivery remains pending. A backup taken before a give-up may contain the outbox entry and no unknown-delivery duty. Another backup may hold only the record's signed bytes after the terminal duty was acknowledged. Neither case permits automatic delivery or a replacement. The host reconciles the possible effect. A delivery proved still pending is subject to the original deadline, current send authority and the record checks of Section 7.2, What a restore loses; these runtime cases have no Paykit vector.

### Time remaining for handoff

The handoff window (Section 7.2 step 1): `orphan_after` at least the payment method's longest time plus the allowance before the handoff, a handoff only within that allowance, and a repeat only while the method's longest time fits before the orphan age. Relevant wallet tests include `test_fence_attempt_past_its_pre_handoff_budget_not_paid`, `test_orphan_after_below_the_floor_refused`, `test_default_config_takes_a_one_day_rail` and `test_repeat_too_close_to_the_orphan_age_not_made` cover it, and the conformance case `handoff_window`, below, states it.

### Restored certificate

The restored certificate (Section 7.2): a certificate Paykit's rules refuse is never installed by a restore or a join, and nothing is done under one taken later. Relevant wallet tests include `test_restore_certificate_paykit_refuses_writes_nothing`, `test_certificate_join_paykit_refuses_writes_and_sends_nothing`, `test_certificate_renewal_paykit_refuses_holds_entries_and_refuses_calls` and `test_certificate_restore_then_renewal_paykit_refuses_takes_no_action` cover it, and the conformance case `restored_certificate`, below, states it.


## Wallet and service scenarios

Use the scenario name when reporting the result, together with the wallet and service versions and the observed outcome.

### `private_list_publication_requires_plain_selling_scope`

Applies to Sections 3.1, 3.3, 3.4 and 8.4.1; wire 4.13, 4.14 and 5.8.

- *Setup:* one payee has a plain selling app and a labelled selling app. A payer has chosen private payment. Give the labelled app a separate payer scope for the payer-role checks.
- *Steps:* choose the same recipient at both selling apps. Issue a first list, request one with and without a target App ID, and withdraw. Queue a publication under the plain certificate, narrow it to a labelled scope, and resolve the retained batch, flush, retry and restore with a withdrawal owed. Restore publishing authority and run discovery again. Check the payer's intake of both a nonempty and an empty list signed under a labelled-only selling certificate.
- *Required:* only the plain selling app advertises list requests, receives the eligible requests and reserves, sends or withdraws a list. A named request for the labelled app gets no answer. The labelled app's first offer creates no counter or queued message. Narrowing or restoring without the plain scope keeps the original obligation without transmitting, even if a retained batch later proves applied. Once authorized and otherwise eligible, the restored app reserves its withdrawal at the current generation's key. The payer rejects either labelled-signed list before changing its stored list or version or using a destination. The labelled app's sales and their replies still work under its label. Its independent payer scope still lets it request and receive a plain publisher's list.

### `private_list_reservations_are_recipient_local`

Applies to Section 8.4.1; wire 4.13 and 5.8.

- *Setup:* one app publishes to A and B in one generation; a second instance of the same app holds the same counters.
- *Steps:* reserve for B between two reservations for A; let both instances reserve for A from the same counter; have a requester name a pair above A's counter.
- *Required:* A's counter moves only for A's lists. Exactly one of the two batches applies; the other queues nothing, its message is never sent, and a fresh reservation for that recipient must exceed the winning counter. No requester's hint moves a counter. Proof, used-endpoint and journal paths keep both members of the pair and the recipient.

### `private_list_withdrawal_creates_no_reply_grant`

Applies to Section 8.4.1; wire 4.13.

- *Setup:* the publisher can add reply grants. A counter identifies a recipient whose original list never reached it and whose identity has no reply grant on the publisher's dispatch.
- *Steps:* restore and issue the owed empty withdrawal. Exercise direct send, release of a kept counter batch, reconstruction, reload, restore, flush, retry and an eligible re-send. Observe the publisher's reply grants before recipient acceptance, after a failed delivery attempt and after successful delivery. Separately send an eligible nonempty first list to another new recipient under the same certificate.
- *Required:* no withdrawal creates a reply grant or grants the recipient admission through one. Each eligible nonempty first list retains ordinary first-message admission.

### `private_list_batches_survive_unknown_outcomes`

Applies to Section 8.4.1.

- *Setup:* a nonempty list with single-use payloads is owed to a recipient.
- *Steps:* crash before the batch, after the Dispatch applied it but before its answer, and after the message was queued; lose the answer and let the client send the kept batch again; have the Dispatch refuse the batch with a conflict, then sign again.
- *Required:* a list is queued only by the batch that applied its counter write. A definitively unapplied batch sends nothing and its payloads stay unissued. A retry that proves prior application preserves the original reservation. Expiry without a deciding result keeps the payloads reserved. A batch sent again keeps its pair and bytes. While an outcome isn't known, nothing else is prepared for that recipient.

### `private_list_counters_continue_after_restore`

Applies to Section 8.4.1; wire 5.8.

- *Setup:* an app has a backup whose counters are below those at its service, for recipients it published to before and after the backup.
- *Steps:* restore; restore the same backup again; run a second instance from it; withdraw and publish after each. Present a counter under the correct key whose authenticated writer is a different own app, with a lower, plausible higher or maximal value. Exercise publication, withdrawal and recipient discovery from a running publisher and from an older backup without the corresponding floor. Send list requests from a counterparty the restored state holds nothing of, during the restored mode and after the host sets its list.
- *Required:* under compliant writes, each new reservation after a restore exceeds the earlier pairs for that recipient. The restored app withdraws to every possible prior recipient found in its backup or its own service records, including a still-payable list issued only after the backup in an earlier generation. It reserves each new withdrawal in the current generation. Each wrong-writer input is reported with its authenticated writer. It produces no allocation, withdrawal, exhaustion or new recipient, even when the backup has no higher floor. Earlier authenticated evidence remains intact. A current publisher-authenticated write consistent with that evidence permits normal allocation. During restored mode, an eligible request naming a held version can cause that withdrawal. A request naming no version from an unchosen counterparty creates no counter. After the host sets its list, a request from an unchosen counterparty creates none. Repeating the backup and restore after an empty withdrawal for that counterparty must never cause a nonempty list to go there when the host sets the list for its chosen recipients.

### `private_list_withdrawal_after_provider_loss`

Applies to Section 8.4.1.

- *Setup:* a provider loss moves the identity to a new generation while the app's own recovery upload and other apps' uploads are pending.
- *Steps:* withdraw a list, then finish the app's own upload while another app's upload never comes.
- *Required:* the withdrawal waits for the app's own upload only, then takes counter 1 at the new generation's key, and the recipient takes it over every list of the earlier generation.

### `private_list_pair_receiver_and_bounds`

Applies to Section 8.4.1; wire 4.13 and 5.8.

- *Steps:* install `(g+1,1)` after `(g,9007199254740991)`; deliver lower and equal-pair/different-hash messages, an empty list, expired lists, and a list of a generation above the payee's verified `identity.json`, then let the verified document catch up without delivering that list again; on both sides of the catch-up, attempt a payment to an otherwise eligible, unexpired single-use endpoint of that list. Exercise proof/consumed lookup across recipients and generations; maximal values and retained unknown members; the binding to the permanent policy, exact operands and ordinary rate, record and local storage limits.
- *Required:* only a greater authenticated pair replaces; equivocation is rejected and reported; empty/expired ordering and consumed marks remain. A list of a generation above the verified document is installed by its pair before any payment, is refused while the document is behind, with no `used` entry written and no version consumed, and becomes payable unchanged, without redelivery, once the document reaches its generation; a lower pair of that app stays ignored throughout. Unavailable history holds. A malformed, misbound or oversized counter authorizes no publication. A failed store or condition queues no new message. Every generic cap is enforced, not satisfied by truncation. A refusal leaves duties and uncertain resources retained. Positive and rejection encoding vectors for the pair and publication record are generated with their implementing code.

### `queued_sends_follow_current_deny_state`

Applies to Sections 8.1 and 8.4.

- *Setup:* an admitted counterparty has received an initial Private Payment List. A transient delivery failure leaves one changed nonempty list queued. Before the next attempted delivery, the app either blocks the recipient or fails to refresh an expired deny copy, which is now unknown. No changed list has reached the recipient at this point.
- *Steps:* independently exercise direct retry, generic flush, payment resume, re-send, resolving a previously unanswered batch, reload followed by flush, and restore followed by flush. Repeat for each held kind in Section 8.1, including both roles of a cancellation, both reasons for an acceptance and both origins of an Allowance end. Include a failure to persist a hold and a crash after queueing but before a delivery attempt.
- *Required:* no message required to wait reaches the counterparty through any route. Neither a clean restore nor a batch that later applies makes it eligible. A known, permitted recipient can receive an otherwise eligible queued message after an unblock; one still unknown cannot. Expired messages are given up, never delivered after the hold ends. A carried list removed on restore never reappears, whether it was nonempty or an empty withdrawal; the fresh restore withdrawal follows the case below, and only a newly permitted nonempty list after the host sets its endpoints may follow it.

### `queued_stops_and_outcomes_still_go`

Applies to Sections 7.2, 8.1, 8.3 and 8.4.

- *Setup:* independently queue an empty newer list withdrawal, an owed receipt, a handed off payment's acceptance and proof, a payee cancellation and an eligible user-initiated `allowance_end` under a transient delivery failure. Keep an unrelated nonempty list and list request to the same recipient queued beside each. Establish either a block or an unknown deny copy before delivery.
- *Steps:* exercise the same delivery routes above, with the required restore first-read guard completed where applicable. For a positive restore case, retain current evidence that each exact original delivery is still pending. Also restore only an older outbox copy after the original store gave up the delivery with an unknown outcome. Separately try a payer cancellation, a subscription acceptance that reports no handed off payment and an automatic Allowance end for a crossing withdrawal over a seeded accepted Allowance record (Section 8.3.1); also attempt to reconstruct a receipt from a shared record after restore while sends remain held.
- *Required:* a stopping or payment-outcome message proved still owed reaches the counterparty while the unrelated held messages do not. Retries preserve the original message, except that restore selectively supersedes the carried list delivery with the newly signed empty version required by Sections 7.2 and 8.4. This replacement authorizes no duplicate payment, receipt, proof, acceptance, cancellation or Allowance end. The withdrawal leaves no usable list, including in the post-backup publication case below. The payer cancellation, subscription acceptance and automatic crossing end stay held. The automatic end remains recorded and its own-app notice still goes. The exception does not reconstruct a receipt forbidden by the restore rule, erase a recorded stop, or release all sends to the recipient. The stale outbox copy stays held for reconciliation. Missing evidence of a permitted role or effect keeps that message held rather than treating its kind as sufficient.

### `queued_allowance_ends_follow_their_origin`

Applies to Sections 8.1 and 8.3.

- *Setup:* an app holding `paykit_allowance` and `paykit_contacts` independently queues, for the same counterparty, an eligible user-initiated end of one Allowance and an end generated automatically for a proposer's withdrawal crossing an acceptance of another, previously accepted under Section 8.3.1. Both ends are recorded; a transient delivery failure or an unanswered batch leaves their counterparty messages owed. Establish either a block or an unknown deny copy before the next delivery.
- *Steps:* exercise every delivery route in `queued_sends_follow_current_deny_state`, including later application of the kept batch, reload, and clean and uncertain restore with their ordinary guards completed and current evidence that each original delivery is still owed. Separately make the durable evidence of an end's origin unavailable. Also exercise the crossing withdrawal arriving while sends are already held, as in `crossing_end_recorded_while_held`.
- *Required:* the user's eligible end reaches the counterparty without a new logical end, while the automatic crossing end stays held on every route. Both durable ends stand and their own-app notices remain eligible; the executor makes no new payment under either ended Allowance. An end with unknown origin stays held rather than gaining the exception from its kind, reference form, signature, recorded end or queued status. Once sends are permitted and all other restrictions allow it, the automatic end may go with its original bytes before its replay deadline. An expired message is given up under the existing rules, while the end remains in the record.

### `restored_withdrawal_supersedes_unseen_list`

Applies to Sections 7.2, 8.1 and 8.4; wire 4.13.

- *Setup:* a transient failure leaves an empty withdrawal W queued at version v. Take backup K, then have the host resume sharing and deliver a nonempty list L at a greater version to the same counterparty. Restore K before the host re-applies those changes. Exercise both a clean and an uncertain restore verdict and both a block and an unknown deny copy. Keep an owed receipt and dispatched-payment proof beside W in K.
- *Steps:* complete the ordinary certificate, current-identity and restore first-read guards. Attempt every delivery route in `queued_sends_follow_current_deny_state`, including a carried kept batch that later applies. Include a failed local update and a crash between suppressing W and delivering its replacement. Deliver replacement W1, explicitly re-enable and deliver L2, then restore the same K and deliver W2, while every verified time is equal. Repeat when the recipient first became known after K, and after a genuine generation change followed by two restores within that generation. In a separate control, retry W without restoring and without any later publication.
- *Required:* the app never treats W's old signed version as satisfying the restored withdrawal. It suppresses that carried delivery and retains the obligation durably, then sends a newly signed empty version whose reserved pair is greater than L, without relying on a clock or burst bound. W2 exceeds L2 through the current recipient counter; at a greater generation counter 1 exceeds every earlier-generation counter, and the next restore continues that new namespace. The counterparty holds no usable list afterward. If the valid replacement cannot yet be published, the obligation waits and W is not released. The unrelated owed receipt and proof remain deliverable under their own rules, while nonempty lists and list requests remain held. The control without restore retries the original eligible W normally. Later retries of the fresh withdrawal do not extend any signed message's replay deadline.

### `restore_stale_backup_cancellation_stop`

Applies to Sections 7.2, What a restore loses, and 8.1 to 8.3.

- *Setup:* payer identity A with one app P holding `paykit_pay` and `paykit_contacts` (a variant has two apps, both of whose record writes fail); payee identity B with an app S holding `paykit_request`, `paykit_settlements` and `paykit_contacts`. S sends R1, a one-time request without a deadline, R2, a recurring request that P accepts (a variant pays R2 under an Allowance with P as its executor, using an accepted Allowance record holding P's grant and grant history with P's accepted complete ledger and P's consent list holding it), and R3, another one-time request without a deadline. P records all three. P takes a layered backup K0 at cursor c0.
- *Steps:* S sends a `payment_request_cancellation` C1 for R1 and C2 for R2. P verifies both, keeps the stops and passes the entries in that commit, at cursor c1 after c0. Then either (F) P's request record writes are refused for good, such as with an injected `403 permission_missing`, or (P) P stops before it sends them. Every target app's cursor has passed the entries, so the service deletes them. P is restored from K0.
- *Expected:* P was restored, so it holds every open request and Allowance. Nothing in the chain shows that P passed anything after c0, since the entries were passed with cursor moves alone, so an implementation that relies on the chain to decide whether to hold fails this case. P makes no automatic attempt on R1, R3 or any Billing Period of R2, in the Allowance variant even after the user confirms the Allowance again (Section 8.3); a manual attempt on R1 or R3 needs the user's confirmation that the payee may have cancelled it; and P sends S a `resend_request` with `accepted_from` at the `accepted_at` of the entry at c0, less 5 minutes.
- *Branches:* (i) S re-sends C1 and C2: P keeps both stops, makes no attempt on R1 or R2 even with the user's confirmation, and writes the request records; R3 stays held until the user confirms it. (ii) S never answers: the hold stays; once the user confirms a request, an attempt on it is allowed. (iii) P is restored instead from a backup K1 taken at c1: the kept stops come back and stop R1 and R2 for good, and no confirmation overrides them; the hold still covers every other open request and Allowance, so R3 is held until the user confirms it. (iv) Another own app's record write succeeded: the record shows the cancellations, which stop R1 and R2 for good.

### `handoff_window`

Applies to Section 7.2 step 1.

- *Setup:* payer identity A with one app P holding `paykit_pay` and `paykit_contacts`, paying through a method that declares its longest time L and isn't idempotent; payee identity B with an app S holding `paykit_request`, `paykit_settlements` and `paykit_contacts`. S sends R, a one-time request.
- *Steps:* P writes the `attempt` for R. (a) The handoff is delayed, after the `attempt`'s answer, until P's allowance before the handoff has passed. (b) P is configured with an `orphan_after` below L plus that allowance.
- *Expected:* (a) P submits nothing and writes `failed`, and S sees no acceptance. (b) P refuses the configuration before it writes any `attempt`.

### `multi_protocol_own_lifecycle`

Applies to Sections 2 and 3.2.

- *Setup:* an own certificate with valid Paykit scopes and another protocol's scope, and a host protocol set registering both owners with disjoint collections and local state.
- *Steps:* use the public Paykit lifecycle to join, persist and load, then take and restore a layered backup. At each path receive a valid kind for each owner, inspect and acknowledge their duties, and apply a shared deny-list change while one owner cannot persist its callback. Repeat with a missing owner and a certificate missing a permission Paykit requires.
- *Required:* the valid mixed certificate works through all three public paths without removing the other owner or bypassing Paykit validation; each owner handles only its kinds and local writes, and Paykit never acknowledges another owner's duty. The deny barrier holds until every owner durably takes it, including after reload. Missing owners and invalid Paykit authority refuse without partial lifecycle installation. These are required SDK cases, not existing vector coverage.

### `restored_certificate`

Applies to Section 7.2, The restored certificate.

- *Setup:* identity A with app P holding `paykit_pay` and `paykit_contacts`; its certificate passes Paykit's rules. P takes a layered backup K0. A's signer then renews P's certificate with one Dispatch permission that P's Paykit scopes don't derive, which the Dispatch takes and Paykit refuses, and lists it in `identity.json`.
- *Steps:* (a) A backup K1 carrying the refused certificate, made by hand since P takes no backup under a certificate it refuses, is restored. (b) P is restored from K0, then reads its feed, and a counterparty sends it a `payment_request`.
- *Expected:* (a) The restore is refused and writes nothing. (b) The restore succeeds; the first read takes the renewed certificate; P then sends nothing, pays nothing and holds the request unprocessed. Once A renews the certificate again with one that passes, P processes the request.

### `crossing_end_recorded_while_held`

Applies to Section 8.3, Ending and Blocked counterparties.

- *Setup:* allower identity A with two apps holding `paykit_contacts`: W, which can record an end, and X, which approves and executes Allowances; allowee identity B with an app S. Start with A's accepted Allowance record holding S's proposal, X's acceptance, X's grant and grant history, with X's accepted complete ledger and X's consent list, kept durably, holding the Allowance, while S holds only its proposal; S withdraws the proposal before it has the acceptance, so the withdrawal crosses the acceptance. A's user blocks B before W reads the withdrawal.
- *Steps:* (a) W reads the withdrawal while sends to B are held. (b) A's user unblocks B, and X runs before W runs again; S has an open request the Allowance would cover, which X would otherwise pay automatically.
- *Expected:* (a) the Allowance record holds the end naming the proposal and the acceptance, X is sent the `executor_notice`, and nothing reaches S. (b) X pays nothing under the Allowance: it finds the end in the record, and a batch it built on the record's version before the end fails. At W's next run the end's message reaches S once.

### `settlement_credited_once`

Applies to Sections 6, Settlement marks, and 8.1 step 4.

- *Setup:* payee identity B with two apps, W holding `paykit_request`, `paykit_settlements` and `paykit_contacts`, and H holding `paykit_request:shop1`, `paykit_settlements` and `paykit_contacts`; payer identity A. Repeat with two payer identities presenting the same method settlement on different requests. W has sent A a one-time request R1, and H a one-time request R2 under `shop1`, for the same amount, both payable to one reusable endpoint of B. A pays R1 by one settlement S, and sends a proof naming S for R1 and another for R2, as a payer reusing one settlement would. Run both with an initially absent bucket and with another settlement T already marked in it.
- *Steps:* (a) W and H verify their proofs and submit their credit batches at once. (b) A new app J of B holding `paykit_request`, `paykit_settlements` and `paykit_contacts` joins, and A sends a proof naming S for a new request R3 that J sent. (c) W and H reopen and read the shared bucket and their respective activity records.
- *Expected:* (a) one batch applies; the other app's record keeps its proof and no receipt, it sends A no receipt, and its user is shown that S was credited to another occurrence; H learns of R1 only what Section 12 says. (b) J reads S's bucket, finds its mark, and credits the proof for R3 to nothing. (c) The wallet whose credit applied reads its original receipt. The other wallet reads its own proof without a receipt and the first wallet's settlement mark in the shared bucket. If the other wallet holds only its label, that mark reveals just the information described in Section 12. The bucket also keeps T's mark when present.

### `settlement_bucket_permanent`

Applies to Section 6, Fixed policies and Settlement marks; Section 8.1 step 4; wire 5.7. The wallet tests exercise concurrent marked credits, independent settlements, ordinary restart, retained marks after restore and stored-only receipts. The cases below also require recovery across provider loss.

- *Setup:* payee identity B with apps W and J holding `paykit_request`, `paykit_settlements` and `paykit_contacts`. W has credited settlement S, its mark in bucket K.
- *Steps:* (a) W's host requests a delete of K. (b) J reads K after a declared provider loss. (c) W holds a seen mark that a bucket of 9,000 marks lacks.
- *Expected:* (a) Permanent policy refuses deletion and K keeps its marks. (b) J checks all prior receipts and marks it holds and applies Section 8.1's provider-loss rule before any credit. (c) When W's bounded rewrite would equal the current value, W sends no bucket write, keeps its seen mark and tells its host (`SettlementBucketFull`).
- *Receipt-only comparison:* a payee confirms an unmarked credit by hand after its bucket put was refused for quota. If another wallet of the same payee marks that settlement before the receipt-only batch applies, its bucket condition fails. The first wallet reads the current bucket even when the failure carries no record view, finds the mark, and stores and sends no receipt for the second occurrence.
