# Paykit Allowances

This document describes the product model and ownership boundaries for Allowances. It does not define wire formats or public APIs.

## TL;DR

1. Payment Requests define what a payer is being asked to pay.
2. An Allowance defines payer-controlled rules for automatically paying qualifying Payment Requests.
3. Subscriptions are Allowances with schedule-related rules, created by accepting a Recurring Payment Request.

## The idea

An Allowance is a policy approved by a payer. It lets the payer's wallet handle qualifying payments without asking the payer to approve every payment.

All payments are still push payments initiated by the payer's wallet. A payee can propose terms or send a Payment Request, but cannot extract funds, hold payment credentials, or bypass the wallet.

An Allowance is not a balance, a reservation of funds, or a guarantee of payment. It also does not replace wallet security, transaction signing, Payment Proofs, or Receipts.

## Two kinds of Allowance

Each Allowance is either scheduled or request-driven. It cannot be both, which prevents the same payment from being triggered twice.

### Scheduled

A Subscription remains an accepted Recurring Payment Request. Accepting it also lets the payer's SDK derive a scheduled Allowance locally. There is no separate Allowance proposal or acceptance.

The payee proposes the recurring terms once. The payer's wallet then:

1. derives each Billing Period from the accepted Recurrence;
2. pushes the exact accepted Payment Amount during the allowed window; and
3. includes the Payment Request ID, Payment Reference, and Billing Period in a Payment Proof, when one is sent.

For each Billing Period, the payee sends a new Payment Request linked to the accepted Recurring Payment Request. The payer's wallet may automatically accept and pay it only when it satisfies the scheduled Allowance. A push notification may wake the wallet, but it carries no payment authority. Changing the amount or another accepted term requires canceling the old Recurring Payment Request and proposing a new one. Proposal Expiry is only the acceptance deadline; it is not the lifetime of the Subscription.

This is new SDK behavior. Today's Payment Request Acceptance message does not grant automatic-payment authority. No existing users so no migration to worry about for now.

### Request-driven

A request-driven Allowance covers separate one-time Payment Requests from an identified payee. Each request has its own Payment Request ID, an exact Payment Amount, and no Recurrence. The payer's wallet evaluates each request and may accept and pay it automatically.

This supports rules such as a maximum amount or number of requests within a period. Its Payment Proof has no Billing Period.

Payment Proof remains optional and payment-method-specific in both modes. It is not proof of settlement or Allowance compliance.

## Shared and local Allowances

Scheduled Allowances use the terms already shared in the accepted Recurring Payment Request. Their automation state stays local to the payer.

A request-driven Allowance can be shared or local.

### Shared

A shared Allowance is exchanged between the payer and payee. Either may propose exact terms. The other may accept or reject them; there are no counteroffers. Either may cancel an active Allowance. Changed terms require a new proposal and cancellation of the old Allowance.

A shared Allowance has a stable Allowance ID and is bound to the payee's exact Paykit Receiver Reference and one asset. A Payment Request must reference that ID to ask for automatic handling.

If a request passes, the SDK queues the standard Payment Request Acceptance before preparing the payment. If it fails, the SDK sends a standard rejection containing only the failed rule category, such as amount, timing, count, endpoint, validity, or capacity. It does not reveal usage, remaining capacity, or wallet state.

An active shared Allowance only says that a qualifying request is eligible for automatic handling. Payment can still fail because of funds, fees, connectivity, routing, or settlement.

### Local

A local Allowance exists only in the payer's wallet. The payee does not need to know about it, and a Payment Request does not reference it.

If a request passes, the wallet may follow the normal acceptance and payment flow. If it fails, the wallet sends no automatic response and leaves the request for normal app handling.

Only one local Allowance may be active for a given payee's Paykit Receiver Reference and asset. The wallet may update it atomically while keeping its identity and audit history.

## Policy rules

An Allowance contains a set of rules. Every configured rule must pass. The first version does not need OR groups, ordered allow or deny rules, FX, or cross-asset evaluation.

Rules may cover:

- an inclusive amount range for each payment;
- total amount or request count within a period;
- a lifetime amount limit;
- a schedule or payment window;
- activation and expiry times;
- allowed Payment Endpoint Identifiers; and
- the exact payer and payee relationship.

Each Allowance uses one asset. Amounts reuse PaymentAmount and use exact decimal arithmetic. Asset values must match exactly; Paykit does not convert currencies or define asset precision.

Period limits may use anchored periods or rolling windows. Months and years are UTC calendar periods with deterministic end-of-month handling. Rolling windows use fixed minutes, hours, days, or weeks.

An accounting period is not a Billing Period. A Billing Period identifies one occurrence of a Recurring Payment Request.

Only the requested Payment Amount consumes capacity. Fees do not count, and refunds do not restore capacity. An Allowance without an expiry remains active until canceled.

## Evaluation and execution

Paykit Library should provide pure, stateless evaluation. Given validated terms, a trusted time, and caller-supplied history, it returns:

- whether the payment is eligible;
- a safe failure category when it is not; and
- the usage that must be reserved when it is.

That result does not reserve anything. Before signing or sending, the stateful SDK must recheck current state and atomically store the reservation. A pending or unknown payment keeps the reservation. A definitive failure releases it, and a verified payment commits it. Payment-method code determines the outcome.

This keeps the library reusable while leaving concurrency and cross-device state with the wallet.

Canonical schedule expansion is not defined by Payment Request v0.2. Before schedule evaluation becomes public API, Paykit must define time-boundary and missed-period behavior. Evaluation must receive a trusted time rather than read a clock.

## Who owns what

Paykit Protocol and Paykit Library own:

- shared Allowance terms, IDs, lifecycle messages, parsing, serialization, and structural validation;
- the relationship between Allowances and Payment Requests;
- pure schedule and policy evaluation; and
- the existing Recurring Payment Request, Recurrence, Acceptance, Payment Proof, and Billing Period shapes.

Paykit SDK owns:

- one app-facing Allowance model with scheduled and request-driven variants;
- shared and local lifecycle state, usage, reservations, and payment correlation;
- due and active-state views built with library evaluation;
- deduplication by Payment Request ID, plus Billing Period for scheduled payments; and
- lifecycle message queues and crash recovery.

The wallet and payment-method integrations own:

- consent, authentication, notifications, and other UX;
- durable storage, a trusted clock, timers, and background work;
- endpoint and funding selection, fees, balances, routing, and credentials;
- building, signing, broadcasting, monitoring, and reconciling payments; and
- method-specific Payment Proof validation and execution outcomes.

The library remains stateless and payment-method-neutral. It does not move funds. Canceling a Recurring Payment Request stops new scheduled executions. Disabling local automation alone does not send a Payment Request Cancellation.

## Decision log

This is the audit trail for the product discussion, not required reading for the main concept. Later decisions take precedence where noted.

1. **How do Payment Requests, Allowances, and Subscriptions relate?** Initial answer: automatic charges use one-time Payment Requests, and a scheduled Allowance is a Subscription. Decisions 29 and 30 later kept Recurring Payment Requests for scheduled payments.
2. **Who may propose a shared Allowance?** Either payer or payee.
3. **Can either party counteroffer?** No. Proposals contain exact terms and may only be accepted or rejected.
4. **Must a payee accept a payer's proposal?** Yes, before it becomes an active shared Allowance.
5. **How are policies represented?** As composable rule types.
6. **How are rules combined?** Every configured rule must pass. V1 has no OR or ordered allow or deny rules.
7. **Which controls are in scope?** Per-payment and period amounts, schedules and windows, validity, request count, lifetime total, and endpoint restrictions.
8. **Which period styles are supported?** Anchored periods and rolling windows.
9. **Can one Allowance use multiple assets or FX?** No. It uses one asset and performs no FX.
10. **How does a request select a shared Allowance?** By exact Allowance ID. Local Allowances are matched without an ID, as decided in 19.
11. **Who may cancel a shared Allowance?** Either party, unilaterally.
12. **Can shared terms be edited?** No. Changed terms require a new proposal and cancellation of the old Allowance.
13. **When is capacity consumed?** It is reserved atomically before signing or another irreversible action.
14. **Who determines payment success?** Payment-method code reports a generic outcome. Unknown stays reserved, failure releases, and verified payment commits.
15. **What happens when a shared request passes?** The SDK queues Payment Request Acceptance before preparing payment work.
16. **When can a scheduled payment execute?** Within explicit early and late bounds, not anywhere in its accounting period.
17. **What happens when a shared request fails?** The SDK rejects it automatically.
18. **What does that rejection reveal?** Only the failed rule category, never usage or remaining capacity.
19. **Must an Allowance be shared?** No. A local Allowance stays wallet-only and sends no automatic response when a request fails.
20. **How are overlapping local policies avoided?** Only one may be active for the same payee Pubky key, Paykit Receiver Path, and asset.
21. **How are local terms changed?** Atomically in place, retaining identity and audit history.
22. **How is a per-payment amount constrained?** With an inclusive range. Equal bounds mean an exact amount.
23. **Do refunds restore capacity?** No.
24. **Do fees consume capacity?** No. Only the requested Payment Amount counts.
25. **How are calendar periods anchored?** In UTC, with deterministic month and year behavior.
26. **Which units can roll?** Minutes, hours, days, and weeks. Months and years are anchored.
27. **Is expiry required?** No. An Allowance may remain active until canceled.
28. **How are assets and amounts represented?** With PaymentAmount, exact asset matching, and exact decimal arithmetic.
29. **Who triggers a scheduled Subscription payment?** The payee sends a new Payment Request for each Billing Period. The payer's wallet validates it under the scheduled Allowance and pushes the payment.
30. **What does accepting a Recurring Payment Request create?** A Subscription and a locally derived scheduled Allowance, with no second protocol flow.
31. **Can one Allowance have both trigger modes?** No.
32. **What amount is paid on schedule?** The exact accepted amount. A change requires a new proposal.
33. **How does the SDK expose the modes?** As one Allowance model with mutually exclusive variants.
34. **What scheduling belongs in the library?** Pure evaluation from terms, time, and supplied history. The wallet owns state and execution.
35. **What does request evaluation return?** Eligibility and the exact reservation intent.
36. **What does a shared Allowance promise?** Eligibility for automatic handling, not guaranteed payment.
37. **How is a scheduled occurrence identified?** By the existing Billing Period on its Payment Proof.
