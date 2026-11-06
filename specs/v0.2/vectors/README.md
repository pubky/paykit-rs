# Paykit v0.2 test vectors

Test vectors for Paykit's own wire format in `specs/v0.2/wire-format.md`: its amounts, derivations, executor
grant, messages and shared record values. Every file is computed by the reference prototype's own code
from fixed keys, shared separately, and fixed randomness, so any implementation can check them byte for byte.

**This set depends on [the Switch's](https://github.com/pubky/pubky-switch/tree/main/spec/vectors)**: its fixed keys (`keys.json`), the certificates and documents of its `documents.json`, its envelopes and its encodings. The seeds, encodings and times follow that set's conventions.

## Files

| File | Holds | Wire section |
|---|---|---|
| `encoding.json` | Decimal amounts (valid, invalid, normalized) | 1.2 |
| `identifiers.json` | Request hashes, obligation IDs (one-time request and Billing Period with their blinded inputs; a payment without a request from its attempt ID alone) and the terms hash. The record keys of Paykit's natural keys and the policies of its collections are the Switch set's `identifiers.json` | 1.5 |
| `documents.json` | The executor grant, and the grant as a grant history entry with the record version (generation, `seq`) its write built on, which its signature covers; `certificate_rejections` (the wallet's certificate with `scopes` and `permissions` changed and signed again, whose well-formed `permissions` aren't the list Paykit's scopes give: a permission extra or missing, which the switch takes, `switch_accepts`, and Paykit's verifiers refuse; those every verifier refuses are the Switch set's `permission_rejections`), and `labelled_scope_rejections` (the hosted certificate with a label of 33 characters, one in upper case, an empty one, nine labelled scopes, and, last, plain `request` beside a labelled scope; `switch_accepts` is true for the nine labels only). The certificates are the Switch set's | 3.1, 3.3, 5.4 |
| `signatures.json` | The signature of Paykit's label, the executor grant's, as the Switch set's `signatures.json` gives those of the Switch's labels | 2 |
| `messages.json` | Every message kind as v0.2 sends it: exact JSON, hash, signature and the signed plaintext before padding; last, a request bob's hosted app sends under its label `shop1` and alice's acceptance, which copies the label; and `label_rejections`, requests a payer drops for their label (an invalid one, another label than the sender's, none from a labelled app), an acceptance whose label isn't valid, and, last, acceptances a payee app drops at check 8 because its certificate (`recipient_cert`) doesn't hold their label (one labelled `shop1` at an app holding only `request:shop2`, one without a label at the hosted app) | Switch wire 4.5, 4 |
| `sealed.json` | Record values a reader binds to their keys (`binding_accepted`: a `payment_lists` record, an `allowances` record and its `allowance_ledgers` record), and the same values at a wrong key or with a field that doesn't match, a list record whose payee isn't the identity that sent its list, and an acceptance naming another Allowance, which a reader refuses (`binding_rejections`); and `labelled`, bob's payee record of the labelled request under its label's policy, sealed with his label key, with the records a reader refuses: under another label's policy, under the fixed policy, and an unlabelled request under a label. They are sealed as the Switch set's `sealed.json` seals | 5, 5.1, 5.3 |
| `periods.json` | Billing Periods: monthly anchor January 31, yearly anchor February 29, daily, weekly, every 2 weeks, every 3 months, hourly and minutes, periods across leap days, with lookups and named-period checks; and `details_rates`, whether period details' rates apply to a payment by its verified payment time: at `expires_at`, a millisecond after, four fraction digits truncated, and whole seconds | spec 8.2, wire 4.9 |
| `allowance_usage.json` | Allowance usage for every limit kind (per payment, anchored period, rolling period, lifetime, payment count), the outcome table (a `failed` claim entry says whether the attempt's owner wrote it; one by another app stays reserved; a ledger `failed` whose claim is now `done` spends), a `totals.failed` item whose claim is now `done`, which counts in the lifetime amount, and a compaction that keeps the failed entry in `totals.failed` | 5.5, spec 8.3 |

## Conventions
- **Seeds, encodings and times** are those of the Switch set's README.
- **Sections.** Descriptions and the table above cite sections of `specs/v0.2/wire-format.md` as "wire N"; "spec N" is the Paykit v0.2 spec, and "Switch wire N" and "Switch spec N" are the Pubky Switch's documents.
- **Allowance usage.** An entry counts by its own `outcome`, else by the state of its attempt's claim listed in `claims` (`open` and `unknown` reserve, `done` spends, `failed` releases, missing spends). `refused_by` is the limit that refuses a candidate.
- **Permissions** (Switch wire 3.1, wire 3.1). The switch checks a certificate's `permissions` for form and enforces them as signed; Paykit's verifiers check them against the scopes.
- **Rejections.** Where a file lists inputs to refuse (`documents.json` `certificate_rejections` and `labelled_scope_rejections`, `sealed.json` `binding_rejections` and the rejections of `labelled`, `messages.json` `label_rejections`), what is normative is that the input is refused. An `error` or `reason` text is the prototype's, quoted without the section number it starts with there, and only informative.

## How the files were generated
The reference prototype, shared separately, generated every file. Its tests recompute every file of both sets and fail on any difference. While computing, the builder also checks each vector with the implementation: every signature verifies, every envelope opens for each recipient, every message passes the recipient checks and decodes by kind, the chain verifies, and the rejection cases fail.

After an intended wire change, the vectors are generated again from the prototype and the diff is reviewed.

The JSON examples in `specs/v0.2/wire-format.md` are taken from these files and the Switch set's; update them with the vectors.
