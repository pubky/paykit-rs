# Paykit v0.2 wire format (Path A)

**Status.** Complete; a review candidate, with the spec. The wire format of `specs/v0.2/spec.md` ("the spec"). The reference prototype implements it but the rules this revision adds, which spec Section 18 lists ("Where the reference prototype differs") and Appendix A marks; `paykit-lib`, with `pubky-switch-core` for the Switch's rules, implements all of it; an independent implementation, shared separately, written from this document alone, hasn't run this revision's vectors. `paykit-lib`'s vector tests generate and check every vector of this set, and `pubky-switch-core`'s check every vector of the Switch's, in public code anyone can run. The spec decides behaviour; this document decides bytes. It builds on the Switch wire format ([`spec/wire-format.md`](https://github.com/pubky/pubky-switch/blob/28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3/spec/wire-format.md) of the pubky-switch repository, at commit `28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3`, the commit the spec builds on), which defines every format Paykit uses but its own: identities, certificates, app records, envelopes and the message frame, operations, the HTTP API and the rest. It settles every wire detail of Paykit's own that the spec leaves open, taking the phase 2 prototype, the reference prototype, as the source of truth for what it implements, so that a second team can build an interoperable implementation without reading the prototype. Where this document had to choose something the prototype doesn't fix, Appendix A says so.

**Test vectors.** `specs/v0.2/vectors/` holds Paykit's test vectors for its encodings, signatures, derivations, messages and record values; its `README.md` lists the files. They use the Switch's algorithms, not its vector set: every key, certificate, app record, policy, record key and sealed value in them is Paykit's own, from the fixed seeds of its `README.md`: identities alice and bob, alice's apps `wallet`, `phone` and `executor`, bob's apps `shop` and `cap`. Every example in this document comes from those files. `paykit-lib` generates every vector of this set (`specs/v0.2/vectors/README.md`, How the files were generated).

**Choices.** Where the spec is silent, Appendix A records the choice this document made.

**Conventions.**

- "Section N" means this document; "spec N" means the Paykit v0.2 specification; "Switch spec N" and "Switch wire N" mean the Pubky Switch's.
- Field tables have the columns Field, Type, Req (required), Constraints and Notes.
- Types:

  | Type | Meaning |
  |---|---|
  | `amount` | decimal amount string (1.2) |
  | `uuid` | UUID v4 string (Switch wire 1.7) |

  The other types are the Switch's (Switch wire, Conventions).
- **Absent and null.** "Req: no" means the member may be missing. Writers omit absent optional members, except where a table says the member is always written, and except in the shared record values of Section 5, whose writers may write every member with `null` for absent (the prototype does). Readers treat `null` like a missing member, unless the table says `null` has its own meaning.
- Examples of signed documents show the decoded body. Long base64url values are cut short with `...` in a few examples; the vector files have them in full.
- **Rejection vectors.** Where a vector file lists inputs to refuse, what is normative is that the input is refused, and the HTTP status and `code` where this document fixes one. An `error` or `message` text in a vector, such as "incomplete batch: <id>" in the Switch set's [`chain.json`](https://github.com/pubky/pubky-switch/blob/28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3/spec/vectors/chain.json), is that of the code that generated the case, which the vectors READMEs name, and only informative.

## 1. Encodings

### 1.1 JSON in Paykit's documents

Paykit's documents follow the JSON rules of the Switch (Switch wire 1.1). What Paykit adds:

- **Integers** follow the Switch's rule (Switch wire 1.1), outside the opaque members below. Paykit documents carry no other numbers. Times are never integers on the wire, except the Private Payment List `version` (Unix milliseconds) and period starts inside derivations.
- **Opaque members** of Paykit's message bodies, besides the Switch's:
  - a message body's `metadata` (Switch wire 5.1, Sections 4.5 and 4.11);
  - the `proof` of a `paykit.payment_proof` body (4.10);

  Every other member is checked, whatever its name: a `metadata` elsewhere in a message, or a `proof` holding a transition proof or a Stored message. An extension kind's body (4.3) is no exception: only its `metadata` is opaque.

### 1.2 Amounts

Amounts are decimal strings: `amount.value` of requests and proofs, every amount in Allowance terms, and the values and totals of Allowance ledgers.

- **Grammar** (v0.1 `PaymentAmount.value`): `[0-9]+(\.[0-9]*)?` or `\.[0-9]+`. ASCII digits, at most one `.`, at least one digit. No sign, exponent, grouping separator or whitespace. `.5`, `10.` and leading or trailing zeros are valid.
- **Bounds:** at most 18 digits after the `.`, and the value times 10^18 below 2^128. The largest valid value is `340282366920938463463.374607431768211455`. Writers should keep the integer part to 20 digits.
- **Arithmetic** is exact: implementations compare and add amounts as integers scaled by 10^18 (the prototype uses `u128`). An addition that overflows makes an Allowance evaluation fail closed (spec 8.3). The original spelling is kept and signed; two spellings of one value are equal when compared.
- **Computed amounts** (an Allowance ledger's `totals.spent`, 5.5) are written in the normalized spelling of `encoding.json` `amounts`: no leading zeros before a non-zero integer digit, no trailing fraction zeros, no `.` without a fraction, and `0` for zero. A ledger that was never compacted carries `totals.spent` as `""`, which reads as zero; compaction writes the normalized sum, `0` when nothing was paid.
- **Asset** (`asset`): non-empty, no control characters, compared exactly and case-sensitively. By convention it is the lower-case asset segment of the Payment Endpoint Identifiers, such as `btc`, `usd` or `usdt`.
- **Scale:** an amount is in whole units of its asset (`0.00025` `btc`, not satoshis). Paykit fixes no per-asset scale. A payment method rounds up to its endpoint's smallest unit, as `specs/payment-conversion.md` says; 18 fraction digits cover every common asset.
- **Rates** (`conversion.rates`, period details): the same grammar, and the value must be above zero.
- Vectors: `encoding.json` `amounts`, with the normalized value of each valid amount.

### 1.3 Identifiers

Paykit's identifiers, beside the Switch's (Switch wire 1.7):

| Value | Form | Length | Notes |
|---|---|---|---|
| `allowance_id` | UUID v4 | 36 | the form of the Switch's `msg_id`: lower-case hex, `8-4-4-4-12`, version nibble 4, variant bits `10`; a MUST: readers reject upper case and other versions |
| `attempt_id` | `b64(16)` | 22 | random; the claim's attempt ID (Switch wire 6.2) is the payment's |

### 1.4 Payment Endpoint Identifiers and payloads

- A **Payment Endpoint Identifier** is a string as `specs/payment-endpoint-identifier.md` defines it: ASCII letters and digits, `-`, `_` and `.`, at most 64 characters, not `.`, `..` or `private`. The recommended form is three lower-case segments, `<asset>-<rail>-<format>`, such as `btc-lightning-bolt11`. Requests with conversion terms must use that form.
- A **Payment Endpoint Payload** is a non-empty UTF-8 string. Paykit never interprets it.

### 1.5 Derivations

- Paykit's derivations use the Switch's `hash()` and `derive()` (Switch wire 1.5). Paykit's own, the obligation IDs, use `paykit/v1/`.

| Value | Computation |
|---|---|
| Message hash (`refs`, request hash) | `hash(message JSON bytes)` |
| Terms hash | `hash(lp(proposal message JSON bytes, acceptance message JSON bytes))` |
| Obligation of a request | `derive("paykit/v1/obligation", "request", blind(payee identity), blind(request hash), u64(period start in Unix seconds, or 0 for a one-time request))` |
| Obligation of a payment without a request | `derive("paykit/v1/obligation", "payment", attempt ID (16 bytes))`; it names no payee, so a former holder of the naming key can't test guesses of the payee against it (Switch spec 14, 15) |

**Natural keys** (Switch spec 13.3), each an `lp()`:

| Collection | Record | Natural key |
|---|---|---|
| `payer_activity`, `payee_activity` | request record | `lp(payee identity, request hash)` |
| `payer_activity`, `payee_activity` | period record | `lp(payee identity, request hash, u64(period start in Unix seconds))` |
| `payer_activity` | payment without a request | `lp(own identity, attempt ID)` |
| `payee_activity` | payment without a request | `lp(Payment Endpoint Identifier text, settlement ID)`, below |
| `payment_lists` | one per payee app | `lp(payee identity, publishing app ID (16 bytes))` |
| `allowances`, `allowance_ledgers` | one per Allowance | `lp(counterparty identity, allowance_id text)` |
| `block_floor` | the one record | `lp(own identity)` |

- **Whose naming key** is the Switch's rule (Switch wire 1.5), for obligations too. The obligation of a payment without a request is the exception: it isn't blinded, and is derived from the attempt ID alone (its row in the first table).
- **Period start** in a derivation is the Billing Period's `starts_at` in Unix seconds, rounded down; periods start on whole seconds whenever the recurrence's `anchor` does, and an anchor with milliseconds is rounded down the same way.
- **Settlement ID** (spec 8.4): the bytes the payment method defines to name one settlement, the same in every proof of it, such as a Lightning payment hash or an on-chain transaction ID. The payee works it out from the proof's `proof` object and the `payment_endpoint_identifier` (4.10). Every proof of one payment then names one payee record, so a second one is a replay, credited once. A proof whose method gives no settlement ID isn't credited. Each proof `type` has one spelling of its settlement ID, so the same settlement written another way (an upper-case `txid`, say) can't name a second record. A proof whose members aren't in that spelling gives no settlement ID, so it isn't credited:

  | Proof `type` | Settlement ID |
  |---|---|
  | `bitcoin-txid` | the `txid` member, 64 lower-case hex characters, as its UTF-8 text (64 bytes). It has no output index: one transaction is one settlement to a Payment Endpoint, so a payer pays two payments without a request to one endpoint in two transactions. Other members, such as a `vout`, aren't part of it |
  | the prototype's fake rail (`{ "rail": "fake", "tx": ... }`, no `type`) | its `tx` text, not empty |
  | any other | defined by its payment method; a payee that doesn't know the method credits no payment without a request by it |

  The vectors' payment without a request uses `bitcoin-txid` (`identifiers.json`).

Vectors: `identifiers.json` `request_hash`, `obligations` with the blinded inputs of each, and `terms_hash`. The record keys of these natural keys are `identifiers.json` `record_keys`.

## 2. Signatures

Paykit's own format signs under a `paykit/v1/` label, with the Switch's algorithm and signature inputs (Switch wire 2.1, 2.3):

| Object | Label | Signed by | Fields after the label | Where the signature travels |
|---|---|---|---|---|
| Executor grant | `paykit/v1/executor-grant` | executor app signing key | identity, counterparty identity, `allowance_id`, terms hash, `u64(confirmed_generation)`, `u64(generation)` and `u64(seq)` of the Allowance record version its write builds on (0 and 0 for a new record) | `sig` of the grant (3.3) |

- Vectors: `signatures.json`, the executor grant's case, as the Switch set's [`signatures.json`](https://github.com/pubky/pubky-switch/blob/28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3/spec/vectors/signatures.json) gives those of the Switch's labels.

## 3. Paykit's records

### 3.1 App certificates

A Paykit App's certificate is the Switch's (Switch wire 3.1). Paykit fixes its scopes and checks them:

- **`scopes`:** for Paykit, scope names of spec 3.1, each with the scopes it requires; extension scopes `paykit_ext:<prefix>`, at most 8; labelled selling scopes `paykit_request:<label>`, at most 8. A scope's root is its `name`, the part before any `:`. Paykit's roots are `paykit` and every root that is `paykit_` or `paykit-` followed by at least one more character (Switch wire 3.1, "Scope roots"), and a scope with any other root, such as `chat`, `pay` or `request`, is another protocol's (Several protocols, below). Paykit's readers ignore the other well-formed names of its roots they don't know: names that aren't a scope of spec 3.1 and start with neither `paykit_ext:` nor `paykit_request:`. That includes a sub-scope of another Paykit scope, such as `paykit_pay:x` or `paykit_contacts:x`, a plain `paykit_ext`, a plain `paykit`, and a root such as `paykit_future`: Paykit gives them no meaning, so they grant no permissions and have no keys, and the signer refuses an app request naming one. An extension scope's `<prefix>` is `paykit-` followed by 1 to 25 characters of `a-z`, `0-9` and `-`, 32 characters at most, such as `paykit-cap`: a kind's prefix is its protocol's name, or that name followed by `-` and more characters (Switch wire 3.1, "Scope roots"), so every extension kind is Paykit's, and none is taken for `paykit.<name>`, whose prefix is the name alone. `paykit_ext:paykit` and `paykit_ext:cap` are malformed. It sends and receives the extension kinds `<prefix>.<name>` (4.3). A label follows the syntax of a sub-scope (Switch wire 3.1, "Scope names"), without the `paykit-` a prefix needs, and `paykit` is a label like any other: `paykit_request:<label>` acts as `paykit_request` for that label only (Labelled selling scopes, below)
- **Paykit's verifiers.** After the Switch's checks, Paykit's verifiers, at apps and at payers, also check Paykit's scope rules on Paykit's scopes: no name that starts with `paykit_ext:` but isn't a valid extension scope (such as `paykit_ext:paykit` or `paykit_ext:cap`), at most 8 extension scopes, a label of the sub-scope syntax, at most 8 labelled selling scopes, and every scope's required scopes (below), while they ignore the other names they don't know; and that `permissions` holds every permission the table below derives from Paykit's scopes, and, when the certificate holds no other protocol's scope, is exactly that list, with none missing and no other. A certificate that fails these is invalid at Paykit's verifiers, though the switch takes it.

**Switch permissions from Paykit's scopes** (spec 3.1). The table's second column is Paykit's: the signer sets `permissions` from Paykit's scopes by it, and from another protocol's scopes by that protocol's table, and Paykit's verifiers derive the list again from the scopes they know; names they don't know, and other protocols' scopes, grant nothing in it. A scope may appear in several rows. The list holds each permission that one of the scopes grants, once, in the order below.

A well-formed list with a permission missing, or, in a certificate that holds no other protocol's scope, with one extra, is refused by Paykit's verifiers, while the switch takes it and enforces exactly that list: the switch no longer checks the signer's derivation, so a signer that grants too much is honoured by the switch and refused by Paykit's apps, and the signer is trusted with everything anyway. A change to this table needs a new certificate version (`v`), since verifiers of this version would refuse a certificate carrying the new list. Vectors: `documents.json` `certificate_rejections`, which the switch takes (`switch_accepts`); those every reader refuses are the Switch set's `permission_rejections`.

| Switch permission | Granted by any of |
|---|---|
| `claims.read` | `paykit_pay`, `paykit_allowance` |
| `claims.write` | `paykit_pay` |
| `lists.read` | `paykit_contacts` |
| `lists.manage` | `paykit_manage_contacts` |
| `conversation.add` | `paykit_pay`, `paykit_request`, any labelled selling scope `paykit_request:<label>`, `paykit_allowance`, any extension scope `paykit_ext:<prefix>` |

Only valid names count in this table: a name that starts with `paykit_request:` or `paykit_ext:` but isn't a valid labelled selling scope or extension scope (above), such as `paykit_ext:paykit`, derives no permission. A certificate holding one is invalid at Paykit's verifiers anyway.

`paykit_pay` requires `paykit_contacts`, `paykit_allowance_execute` requires `paykit_allowance`, `paykit_pay` and `paykit_contacts`, and `paykit_manage_contacts` requires `paykit_contacts` (spec 3.1); a certificate that breaks this is invalid. Vectors: `documents.json` `executor_certificates` and `payer_certificates`.

**Several protocols** (spec 3.2, Switch wire 3.1 "Several protocols"). A certificate may hold other protocols' scopes beside Paykit's, or only theirs. Paykit's verifiers apply the Switch's rule with Paykit as the protocol they know:

- `permissions` holds every permission Paykit's scopes need, by the table above. When the certificate holds another protocol's scope, the list may hold more, that protocol's; when it holds none, it is exactly Paykit's list. A `paykit_pay:x`, a plain `paykit_ext` or a plain `paykit` is Paykit's, not another protocol's, so it doesn't let the list hold more.
- Paykit's scope rules apply in full to Paykit's scopes, and the bounds of 8 extension scopes and 8 labelled selling scopes count only them.
- Another protocol's scopes, and the permissions they bring, authorize nothing in Paykit: check 8 (4.1) still needs the Paykit scope of each kind, and a certificate that holds no Paykit scope sends and receives no Paykit kind, though Paykit's verifiers take it.

Vectors: `documents.json` `multi_protocol_certificates` (`paykit_request` beside `chat` with `lists.read` and `conversation.add`, taken; without `conversation.add`, refused; `paykit_request` alone with `lists.read` beside `conversation.add`, refused; `chat` alone, taken; `paykit_request` beside `paykit_ext:cap`, refused), and `messages.json` `multi_protocol_senders`.

**Labelled selling scopes** (spec 3.1). `paykit_request:<label>` has the switch permissions of `paykit_request` (above) and sends and receives the kinds of `paykit_request` (4.3), for the requests sent under its label. It is one use of the scope rule (Switch wire 3.1, "Scope names"): plain `paykit_request` covers every `paykit_request:<label>`, a certificate holding `paykit_request` holds every label, and one holding `paykit_request:<label>` holds only that label. So a certificate holds plain `paykit_request` or labelled scopes, never both: every verifier refuses such a certificate, the switch included, the signer never issues one, and it refuses an app request naming both (Switch wire 3.9). Vectors: `documents.json` `certificates`, among them the certificate of bob's hosted app `hosted`, holding `paykit_request:shop1`, and `labelled_scope_rejections` (a label of 33 characters, one in upper case, an empty one, nine labels, `paykit_request` beside a label; only the nine labels pass the switch's rule).

### 3.2 App records

A Paykit App's record is the Switch's (Switch wire 3.2). Its `accepts` holds Paykit's kinds (`paykit.payment_request`) or extension kinds (`paykit-cap.offer`, 4.3) under an extension scope the certificate holds, and its Paykit member is `protocols.paykit` (Switch wire 3.2, "Protocols' members"), an object:

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `capabilities` | object | no | `{ "endpoint_identifiers": array<string> }` | default empty: the Payment Endpoint Identifiers (1.4) the app can pay to |
| `public_endpoints` | object | no | Payment Endpoint Identifier to payload (1.4), each payload a string | default empty |

The example of Switch wire 3.2, `shop`'s record, carries both. These rules are Paykit's, and the switch checks none of them:

- **No member:** a record without a `paykit` member, or with `null` there, has no capabilities and no public endpoints. A missing or `null` `capabilities`, `endpoint_identifiers` or `public_endpoints` is empty. Unknown members in it are ignored (Switch wire 1.1).
- **Malformed:** a Paykit reader refuses a record whose `paykit` member isn't of this form, as it refuses any invalid record: a `capabilities` or `public_endpoints` that isn't an object, an `endpoint_identifiers` that isn't an array of strings, or a payload that isn't a string. It refuses one whose identifiers or payloads break 1.4 too: an identifier in `endpoint_identifiers`, or a key of `public_endpoints`, that isn't a Payment Endpoint Identifier, such as `private`, `..` or one with a `/`, and an empty payload.
- **Expiry required** (spec 4): a record with public endpoints carries `expires_at`, not `null`, chosen by the app, at most 30 days after `updated_at` recommended (the prototype: exactly 30 days). A Paykit reader refuses one without it. A record without public endpoints may write `null`.
- Vectors: `documents.json` `app_records`, `shop`'s record with its `paykit` member changed or removed and signed again: records a Paykit reader takes, with the endpoint identifiers and public endpoints it reads from each (`reads`), and records it refuses by the rules above, among them the rules of 1.4, which the switch takes (`switch_accepts`). The form of `protocols` is the Switch set's [`documents.json`](https://github.com/pubky/pubky-switch/blob/28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3/spec/vectors/documents.json) `app_record_protocols`.
- **Expiry** (spec 4): payers refuse public endpoints from a record whose `expires_at` is at or before their clock. The service doesn't check `expires_at`; the directory serves the record as delivered.
- **Record age limit** (spec 4): a payer uses a record's public endpoints only until the earlier of its `expires_at` and its `updated_at` plus the `record_max_age` of the identity's `identity.json` (Switch wire 3.3; 90 days when absent), and not at all when its `updated_at` is more than 5 minutes ahead of the payer's clock. So a stolen app key can't sign a record that stays payable for long: the signer, which signs `identity.json`, sets the limit. An app renews its record within that limit too (Renewal, below).
- **Path** (Switch spec 4.2): a payer uses a record read from `/pub/pubky-switch/v1/apps/<app_id>.json` only if its `app_id` is that `<app_id>`.
- **Renewal** (spec 4): once less than half a record's lifetime (`updated_at` to the earlier of `expires_at` and `updated_at` plus `record_max_age`) remains, the app renews it at its next run: it signs the record again with a new `updated_at` and `expires_at`, delivers it with `app.put` and, once the service accepts it, republishes it on the Homeserver; while a publication fails, or the Homeserver copy differs from the newest record the service accepted, it publishes that newest record again, never an older one and never over a Homeserver copy with a later `updated_at`, until it deletes that record to leave, or leaves (Switch spec 4.2). A record with more than half its lifetime left is not rewritten. The prototype checks after every feed read; an app offline at the half-way point renews when it next runs, and one that doesn't run before `expires_at` leaves payers refusing the record until it does.
- **Withdrawing a destination:** a stale copy can't be recalled before it expires, so a payee whose destination is compromised removes it and revokes the app, or replaces it with a new one (Switch spec 4.2).

### 3.3 Executor grant

An executor app's signed grant, stored in the Allowance record value (5.4). Other own apps check it without asking the executor.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `app_id` | app-id | yes | the executor app | |
| `terms_hash` | `b64(32)` | yes | the Allowance's terms hash (Switch wire 1.5) | |
| `confirmed_generation` | int | yes | the generation in which the executor's user last confirmed the Allowance | signed, so no other app can raise it (spec 8.3) |
| `cert` | `signed<AppCert>` | yes | the executor's certificate, holding `paykit_allowance_execute` | |
| `sig` | `b64(64)` | yes | label `paykit/v1/executor-grant` over `lp(label, identity, counterparty identity, allowance_id text, terms hash, u64(confirmed_generation), u64(generation), u64(seq))` | `generation` and `seq`: the record version the grant's write builds on, below |

A grant counts only if `cert` verifies for the own identity, names `app_id` and holds `paykit_allowance_execute`; the signature verifies with the certificate's signing key for this identity, counterparty and `allowance_id`, at the record version its grant history entry names (5.4); and `terms_hash` equals the terms hash of the record's proposal and acceptance.

**The signed version.** The last two signed fields are the Allowance record version the grant's write builds on: the `expected_version` (generation, `seq`) of the `record.put` that adds the grant to the record's grant history (5.4), which is the version the executor read, or (0, 0) for a new record. Since a record's version is the `seq` of the write that produced it (Switch wire 6.2), the writer can't know its own new version before the service gives it; the version it read is fixed when it signs. The grant doesn't carry the numbers; a reader takes them from the grant's history entry. A grant moved to another place in the history therefore no longer verifies, so an own app with `paykit_allowance` can't make a ledger write look like the work of another executor by moving versions. A grant outside the history, such as a `grant` that isn't the history's last entry, counts for nothing.

Example (`documents.json` `executor_grant`, whose write built on version (1, 12); certificate cut short):

```json
{
  "app_id": "cw5ahtdkixcafna9zh1y5y5o1w",
  "cert": {
    "body": "eyJ2IjoxLCJraW5kIjoicHVia3ktc3dpdGNoLmFw...",
    "sig": "wX8NrG-Xn5tfiN4Nhe9iON15nc3rgBO60ysBJnuvWGmT0WgAUaOXHKRiaMvqT0FArsXOJXWFeiVxni5Jq6ynAw"
  },
  "confirmed_generation": 1,
  "sig": "TwAuCSVx5eKgCkbbyAxeqddsr0xSL5QebVA794GsLihQO5VxA0O_qUCAk30KFH2TNuVgcWqpORJMNWNEaaLGDw",
  "terms_hash": "PEj43Oz6GuVKcLXW3n5vIbBdSORajscVVsogpqd7Y9M"
}
```

## 4. Messages

### 4.1 Paykit's kinds in the common fields

Paykit's messages use the Switch's common fields and recipient checks (Switch wire 5.1). Paykit fixes:

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `kind` | string | yes | `paykit.` followed by a name of 4.3, or an extension kind `<prefix>.<name>` (4.3); each kind fixes the order of `refs` (4.3) | recipients drop kinds they don't know |

- **Sizes:** A kind that activity records store (4.3, "Recorded") is at most 16384 bytes of JSON (spec 5).
- **Check 8** for Paykit's kinds (Switch wire 5.1): the table is that of spec 3.3. Only Paykit's scopes count: another protocol's scopes in either certificate send and receive no Paykit kind (3.1, Several protocols). A `resend_request` names no kinds, so any current Paykit App of the counterparty, one whose certificate holds at least one Paykit scope, may send it, and any Paykit App takes it; the answer is filtered instead (4.12). An extension kind `<prefix>.<name>` needs `paykit_ext:<prefix>` in both certificates. A labelled selling scope `paykit_request:<label>` counts as `paykit_request` for the kind (3.1), and labels then narrow it. A `payment_request` with `label` needs `paykit_request` or `paykit_request:<label>` for that label in the sender's certificate, and one without needs `paykit_request` (4.5). A payer's acceptance, rejection, cancellation or proof with `label` needs `paykit_request` or `paykit_request:<label>` for that label in the own certificate, and one without `label` needs `paykit_request`; a cancellation without `label` may also be the payee's (4.3, 4.8). A cancellation's recipient holds the other role than its sender: one from an app holding `paykit_pay` passes only at an app the label rule above lets take it, and one without `label` from an app holding `paykit_request` or a labelled selling scope only at an app holding `paykit_pay`, so `paykit_pay` to `paykit_pay` and `paykit_request` to `paykit_request` are dropped (vectors: `messages.json` `cancellation_roles`).
- **Check 10** for Paykit's kinds (Switch wire 5.1): the body is that of the kind's section (4.5 to 4.17). The members the recipient reads must have their types (an `attempt_id` of 16 bytes, payment endpoints mapping identifiers to non-empty payloads, times that parse), and the rules the kind's section states must hold: a request carries at most one of `payment_endpoints` and `required_app_id` (4.5), period details carry endpoints or rates (4.9), a `resend_request` selects something (4.12), Allowance terms are valid (4.15), an `executor_notice` names a known reason (4.17), and a `label`, in a request or a payer's reply (4.5 to 4.10), is a valid label (3.1). A kind the recipient doesn't handle fails this check, an extension kind included. An app that handles an extension kind passes its body to the extension as it is: it carries no Paykit payment semantics, and no shared record holds it.
- **Spam rule** (Switch spec 7): an app doesn't create an activity record for a message from a counterparty the service has flagged (`GET /allow`, Switch wire 7.4.6) or the user has blocked, and keeps only a count. A message counted because the user blocked its sender is dropped as one never processed: the commit that counts it also keeps a mark of that sender, with the earliest and latest times its inbox accepted such messages, and once the read is done the app forgets the message's pair (Pubky Switch spec, Section 11), so a re-sent copy is a new entry. At most one mark is kept per sender and a bounded number in all (the Paykit SDK: 128). A mark that still names pairs to forget never goes for that bound, and one that only a restore or a `missed` item made (spec 7.2) goes first; a mark that goes, for that bound or its own expiry, keeps its latest time in the app's block floor (spec 8.1, Blocked counterparties); a message for which no mark can be kept, or whose mark already names as many pairs to forget as it may (the Paykit SDK: 64), is held instead, so none is lost. The app asks the sender to re-send what its inbox accepted from the mark's earliest time to its latest (spec 9), in a batch that first raises the identity's block floor to that latest time (5.6; spec 8.1, Blocked counterparties), only once every pair the mark names is forgotten, no block or unblock of the sender that the app wrote is still to be shown by the lists, and a read of the lists it makes for the ask, after the messages it has taken by then, shows the sender neither blocked nor flagged. While the flagged senders aren't known, including past a bounded copy of them (the Paykit SDK: 4,096), or when that read fails, it waits. Once an ask for a mark is delivered, its window is done, and the next message counted, or a restore's or a `missed` item's widening applied, starts a new one; it asks again at most once in any 24 hours while the mark lives, until 30 days after its latest time, as for replies dropped for the bounds (spec 8.1, Replies before their record). What comes back is taken as new. A message counted only because the service flagged its sender is not marked, and its pair is kept. A message that updates an existing record (a receipt for a payment in progress) is applied, and so is one that opens a period record of a request the app holds and the payer has accepted, which updates that request's activity (spec 8.2). What binds an app depends on what it can know:
  - **An app holding `paykit_contacts`,** and so the switch permission `lists.read` (spec 3.2), keeps a copy of the lists: the deny list and flagged senders `GET /allow` answers, and the feed's `allow.*` and `deny.*` items since (Switch wire 7.5). A sender its copy shows blocked or flagged gets only a count. A sender the app blocked itself gets only a count at once, before its copy shows the block; one it unblocked itself is judged by its copy alone. A sender whose state it doesn't know isn't clean: while its copy isn't known to be complete, because it never read the lists, was restored from a backup, followed a provider loss or failed to refresh them, the app creates no new activity record for that sender's message. It defers the message until it has read the lists, then handles it as the lists say, and whenever it holds no complete copy it reads the lists before it handles new activity. So such an app creates no new activity record until it has read the lists, whatever the reason it can't read them. Updates to records it already holds still apply. The prototype reads the flagged senders at most every 10 minutes.
  - **An app without `paykit_contacts`,** which holds no `paykit_pay` (spec 3.1), so an app that doesn't pay, such as a payee app, isn't bound by the rule: it holds no lists and can't read them, so it has no state of a sender to consult. In such an app a message from a sender the service has flagged, or one accepted before a block, can create an activity record. That is accepted because the service refuses a denied sender's new appends (Switch spec 7), and the only rule that fails closed, creating no new activity record at all, would keep a payee-only app from receiving a payment it didn't request. Protecting such apps is open (spec 17, question 27).

### 4.2 Changes from the v0.1 bodies

Bodies use the v0.1 definitions of `specs/payment-requests.md`, `specs/payment-conversion.md`, `specs/allowances.md` and `specs/paykit-sdk.md`, with these changes (spec 5):

| v0.1 | v0.2 |
|---|---|
| top-level `version`, `kind`, `app_id` | the common fields `v`, `kind`, `from.app_id` (Switch wire 5.1) |
| `event_id` | `msg_id` |
| `payment_request_id` | the request hash in `refs` |
| the `request` object of `paykit.payment_request` | the request terms are the `body` itself |
| `conversion_quote_id` of a proof | the period details hash in the proof's `refs` |
| `paykit.payment_conversion_quote` | `paykit.payment_period_details` (4.9) |
| `valid_from` of a quote | the period details' `created_at`, cut to whole seconds (4.9) |
| `proposal_event_id`, `acceptance_event_id` of Allowance messages | the proposal and acceptance hashes in `refs` |
| Receipt Access, Receipt Location, Delivery Confirmations | `paykit.payment_receipt` (4.11); no confirmations |
| the 1000-byte message limit | the envelope sizes of Switch wire 5.1 |
| closed-world objects, unknown members invalid | unknown members ignored (Switch wire 1.1) |
| `required_app_id` required, nullable | optional, only without `payment_endpoints` |
| Private Payment List endpoint values are payload strings | objects with `payload` and `single_use` (4.13) |

New in v0.2: `per_period_endpoints`, `payment_period_details`, `resend_request`, `private_payment_list_request` and `executor_notice`.

### 4.3 Kinds

| Kind | Sender needs | Recipient needs | Body | `refs`, in order | Recorded |
|---|---|---|---|---|---|
| `payment_request` | `paykit_request` | `paykit_pay` | 4.5 | none | yes |
| `payment_request_acceptance` | `paykit_pay` | `paykit_request` | 4.6 | request hash | yes |
| `payment_request_rejection` | `paykit_pay` | `paykit_request` | 4.7 | request hash | yes |
| `payment_request_cancellation` | `paykit_request` or `paykit_pay` | the other one | 4.8 | request hash | yes |
| `payment_period_details` | `paykit_request` | `paykit_pay` | 4.9 | request hash | yes |
| `payment_proof` | `paykit_pay` | `paykit_request` | 4.10 | request hash, then the period details hash it used; none without a request | yes |
| `payment_receipt` | `paykit_request` | `paykit_pay` | 4.11 | proof hash, then the request hash when there is a request | yes |
| `resend_request` | any Paykit scope | any Paykit scope; the answer re-sends only kinds the requester's certificate receives | 4.12 | none | no |
| `private_payment_list` | `paykit_request` | `paykit_pay` | 4.13 | none | in `payment_lists` |
| `private_payment_list_request` | `paykit_pay` | `paykit_request` | 4.14 | none | no |
| `allowance_proposal` | `paykit_allowance` | `paykit_allowance` | 4.15 | none | in `allowances` |
| `allowance_acceptance`, `allowance_rejection` | `paykit_allowance` | `paykit_allowance` | 4.16 | proposal hash | in `allowances` |
| `allowance_end` | `paykit_allowance` | `paykit_allowance` | 4.16 | proposal hash, then the acceptance hash (absent for a withdrawn proposal) | in `allowances` |
| `executor_notice` | `paykit_allowance` | `paykit_allowance_execute` | 4.17 | none | no |
| an extension kind `<prefix>.<name>` | `paykit_ext:<prefix>` | `paykit_ext:<prefix>` | defined by the extension | the extension's | no |

**Extension kinds** (spec 3.3). An extension kind is `<prefix>.<name>`: a prefix as for an extension scope (3.1), which starts with `paykit-`, then `.` and a name of 1 to 64 characters of `a-z`, `0-9` and `_`. Under the Switch's rule a kind whose prefix is `paykit` followed by `-` and more characters is Paykit's (Switch wire 3.1, "Scope roots"), so every extension kind is Paykit's, and the `-` keeps it apart from Paykit's own kinds, whose prefix is `paykit` alone: `paykit-cap.offer` is an extension kind, while `paykit.payment_request` is one of Paykit's own and `chat.message` another protocol's, which a Paykit app drops as a kind it doesn't know. One certificate scope `paykit_ext:<prefix>` covers every kind with that prefix, and an app's record lists the kinds it handles in `accepts`, so a new kind needs no new certificate and nothing from the signer. Senders wrap one only to apps whose certificate holds the scope and whose record accepts the kind (Switch spec 3.4). Extension kinds carry no Paykit payment semantics: a payment never relies on one, no Paykit collection records one, and they name no destination. A recipient app that doesn't handle the kind drops it at check 10, keeping it as processed (Switch wire 5.1). The common fields of Switch wire 5.1 apply; the body and `refs` are the extension's. The body is parsed like every message (Switch wire 1.1, check 5), so a number outside its `metadata` must be an integer of Switch wire 1.1: an extension writes a decimal, such as a price, as a string, as Paykit writes amounts (1.2). A message whose body holds `2.5` outside `metadata` is dropped at check 5, before check 10 could pass it on. Vectors: `documents.json` `extension_app`, a certificate with `paykit_ext:paykit-cap` and a record accepting `paykit-cap.offer`.

**Labelled selling scopes** (spec 3.1, 3.4). A labelled selling scope `paykit_request:<label>` sends and receives the kinds that `paykit_request` does in the table above, and its record's `accepts` lists them. A request sent under it carries `label` (4.5), and the payer's acceptance, rejection, cancellation and proof copy the request's label (4.6 to 4.10), so the payee's apps find the record under that label's policy (5.1). **Targets:** the payer wraps those four kinds, about a labelled request, only to the payee's apps whose certificate holds `paykit_request` or `paykit_request:<label>` for that label, and about an unlabelled request, or a payment without one, only to those holding `paykit_request`. A cancellation from the payee carries no label and goes only to the payer's apps holding `paykit_pay`, since its recipient holds the other role (spec 3.3); a payer's cancellation follows the payer's targets above, so an app holding only `paykit_pay` gets no payer's cancellation. Recipients apply the same rule at check 8 (Switch wire 5.1) and drop a reply that breaks it: an app holding only `paykit_request:shop2` drops an acceptance labelled `shop1`, and one holding only labelled scopes drops an acceptance without a label. A resend answer (4.12) follows the same targets. Vectors: `messages.json` `payment_request_labelled`, `payment_request_acceptance_labelled` and `label_rejections`.

`refs` name messages by hash. A reader that needs a referenced message (the request of an acceptance, the proof of a receipt) finds it by hash in its records. `executor_notice` is sent only between apps of one identity; every other kind between identities (Switch spec 7).

### 4.4 Shared body types

- **Amount:** `{ "value": amount, "asset": string }` (1.2).
- **Billing Period:** `{ "starts_at": time, "ends_at": time }`. It must equal a period the request's recurrence computes, compared as instants (spec 8.2; vectors in `periods.json`). The rules the vectors follow: boundary k is `anchor + k * every` units, in fixed seconds for `minute`, `hour`, `day` and `week`, and as a calendar step from the original anchor for `month` and `year`, clamped to the target month's last day; period k is `[boundary(k), boundary(k+1))`. The first period is the one containing `starts_at`; with `ends_at`, the last is the one containing the instant just before `ends_at`, whole (it isn't cut at `ends_at`). The period at a time t is the period k with `boundary(k) <= t < boundary(k+1)`, if k lies between the first and the last; else there is none. A named period is valid only if it equals one of those periods.
- **Payment endpoints:** an object mapping Payment Endpoint Identifiers to non-empty payload strings, with at least one member. The sending app owns them (`from.app_id`).
- **Rates:** a non-empty array of `{ "asset": string, "value": amount }`, with unique `asset` selectors (`specs/payment-conversion.md`).
- **Recurrence:** `{ "every": int (at least 1), "unit": "minute" | "hour" | "day" | "week" | "month" | "year", "starts_at": time, "anchor": time, "ends_at": time | null }`, `ends_at` after `starts_at` when set. Every member is present, `ends_at` as `null` for none; a recurrence without it is invalid.
- **Payment deadline:** `{ "type": "at", "timestamp": time }` on a one-time request, or `{ "type": "period_start", "seconds": int }` on a recurring one.
- **Conversion:** `{ "type": "fixed", "rates": rates }`, or `{ "type": "per_period" }` on a recurring request only. A request with conversion terms is in an asset of lower-case letters and digits, and each fixed rate's `asset` is `<asset>` or `<asset>-<rail>` of such segments, matching the asset, and the rail when it names one, of at least one identifier the request names in `accepted_payment_endpoint_identifiers` or `payment_endpoints`, when it names any; recipients drop a request that breaks this (vectors: `messages.json` `conversion_terms`).

### 4.5 `payment_request`

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `amount` | Amount | yes | | |
| `payment_reference` | string | yes | 1 to 256 characters, no control characters | copied into the payment where the method allows it |
| `proposal_expires_at` | `time \| null` | yes, always written | `null`: no expiry | bounds the acceptance of a recurring request, through a condition on its batch (spec 8.2); a one-time request is answered by paying it, so there it bounds the attempt, through its deadline (below) |
| `payment_deadline` | Payment deadline | no | the type matching the request | |
| `recurrence` | Recurrence | no | absent (or `null`) for a one-time request | |
| `payment_endpoints` | Payment endpoints | no | | the payer pays only to these |
| `required_app_id` | app-id | no | only without `payment_endpoints` | pay only to that payee app's endpoints (spec 8.1) |
| `per_period_endpoints` | bool | no | only with `recurrence` | default `false`; `payment_endpoints` then serve only the first period |
| `label` | string | no | a label (3.1); from a sender holding `paykit_request:<label>` or `paykit_request` | the selling scope it is sent under (spec 3.1); absent otherwise. A sender holding only labelled scopes always writes it |
| `accepted_payment_endpoint_identifiers` | `array<string>` | no | non-empty, unique | narrows the identifiers the payer may use; with `payment_endpoints`, every key of that map must be in it |
| `conversion` | Conversion | no | | as `specs/payment-conversion.md` |
| `metadata` | object | no | | opaque |

A request carrying both `payment_endpoints` and `required_app_id` is invalid: recipients drop it. Its request hash identifies it everywhere: obligations, activity records and `refs`.

**The attempt's deadline** (spec 7.1; the Switch's deadline check is Switch spec 10.2). An `attempt` on a one-time request carries `deadline` = `d + 1 ms` (Switch wire 6.2), where `d` is the earlier of `proposal_expires_at` and the `payment_deadline`'s `timestamp`; with only one of them, `d` is that one; with neither, the attempt carries no `deadline`. An `attempt` on a Billing Period of a recurring request carries the period's `starts_at` plus the payment deadline's `seconds`, plus 1 ms, or no `deadline` without a payment deadline; a recurring request's proposal expiry bounds only its acceptance (spec 8.2). `d` is taken at millisecond precision, further digits truncated (Switch wire 1.6), and `deadline` is written with three fraction digits: `.100` and `.1009` give `.101`, and `.999` gives `.000` of the next second. The service refuses an `attempt` from the `deadline` on (Switch wire 6.8), so the last instant it takes one is `d`. The acceptance of a recurring request is bounded the same way, by `server_time_in(null, proposal_expires_at + 1 ms)` (Switch wire 6.3, spec 8.2), and not at all when `proposal_expires_at` is `null`. Vectors: the Switch set's [`operations.json`](https://github.com/pubky/pubky-switch/blob/28562dc9095d09e1d9b28d1fe7faa9c0f4d676e3/spec/vectors/operations.json) `attempt_deadlines`, which give `d + 1 ms` for a `d` with fractions of three and four digits, and none without a `d`; how Paykit computes `d` has no vector.

**The attempt's not-before** (spec 8.2). An `attempt` on a Billing Period carries the condition `server_time_in(s, null)` (Switch wire 6.3), where `s` is the period's `starts_at`, so the service refuses it with `409 conflict` and its `server_time` before the period starts on its own clock. No automatic payment carries anything else. A payment by hand before the start, after the user confirms paying that period early, carries as `s` the latest service time the app had verified when the user confirmed. An attempt on a one-time request carries no not-before. An attempt on a Billing Period the request record doesn't list in `attempted_periods` also writes the request record, adding it (5.3).

Example (one-time, with a destination):

```json
{
  "v": 1,
  "kind": "paykit.payment_request",
  "msg_id": "04660a75-8d0e-46a9-a230-3d9e3d5e7d75",
  "created_at": "2026-01-01T00:00:00.000Z",
  "from": {
    "identity": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
    "app_id": "brrz3m6g5rci6iqnzke44q4djc"
  },
  "to": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "refs": [],
  "body": {
    "amount": {
      "asset": "btc",
      "value": "0.00025"
    },
    "payment_deadline": {
      "timestamp": "2026-01-03T00:00:00.000Z",
      "type": "at"
    },
    "payment_endpoints": {
      "btc-lightning-bolt11": "lnbc250u1pjexample0request0invoice"
    },
    "payment_reference": "order-1042",
    "proposal_expires_at": "2026-01-02T00:00:00.000Z"
  }
}
```

A recurring request with per-period destinations:

```json
{
  "amount": {
    "asset": "usd",
    "value": "9.99"
  },
  "conversion": {
    "type": "per_period"
  },
  "payment_deadline": {
    "seconds": 86400,
    "type": "period_start"
  },
  "payment_endpoints": {
    "btc-lightning-bolt11": "lnbc1pjexample0first0period"
  },
  "payment_reference": "subscription-5",
  "per_period_endpoints": true,
  "proposal_expires_at": "2026-01-08T00:00:00.000Z",
  "recurrence": {
    "anchor": "2026-01-31T00:00:00.000Z",
    "ends_at": null,
    "every": 1,
    "starts_at": "2026-01-31T00:00:00.000Z",
    "unit": "month"
  }
}
```

### 4.6 `payment_request_acceptance`

Body: `{}`, or `{ "label": string }` for a labelled request, its `label` (4.5). `refs`: the request hash. v0.1 defined no other field. The example is in Switch wire 5.1.

### 4.7 `payment_request_rejection`

| Field | Type | Req | Notes |
|---|---|---|---|
| `reason` | string | no | omitted when absent; `null` is treated as absent |
| `label` | string | no | a labelled request's `label` (4.5); absent otherwise |

`refs`: the request hash.

```json
{
  "reason": "user_rejected"
}
```

### 4.8 `payment_request_cancellation`

Body as the rejection (`reason`, optional; a payer's cancellation of a labelled request carries its `label`, a payee's none). `refs`: the request hash. Either side may send it. A payer records its cancellation of a recurring request in a batch conditioned on the claim of every Billing Period its request record lists in `attempted_periods`, and sends it after that batch succeeds (5.3, spec 8.2); a listed period with an open or `unknown` attempt refuses it.

### 4.9 `payment_period_details`

The payee's details for one Billing Period of a recurring request (spec 8.2).

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `billing_period` | Billing Period | yes | a period of the request | |
| `payment_endpoints` | Payment endpoints | no | | the period's destination |
| `rates` | Rates | no | cross-asset only: no rate for the requested asset | as v0.1 quotes |
| `expires_at` | time | yes | | the payer uses only unexpired details; their rates apply to a payment verified from their start through it (below) |

- At least one of `payment_endpoints` and `rates` is present; recipients drop details with neither.
- Cross-asset only means no rate whose `asset`, or its asset segment, is the requested asset: a `usd` request's details with a `usd` or a `usd-polygon` rate are invalid against it. Each rate's `asset` is `<asset>` or `<asset>-<rail>` of lower-case letters and digits, as in conversion terms (4.4), or recipients drop the details; and it matches the asset, and the rail when it names one, of an endpoint the details' `payment_endpoints` or the request names, when they name any, or the details are invalid against the request (vectors: `messages.json` `conversion_terms`).
- Each details stays valid until its own `expires_at`: a later message for the same period replaces none (spec 8.2). Later means a later `created_at` instant, then, at the same instant, the greater `msg_id` compared as text; the latest that carries what the payment needs is the payer's default (spec 8.2). Details as late as those held, the same `created_at` and `msg_id`, are the same message only if their hash is; other details as late are an equivocation: the reader keeps the details it holds and stores no other. A period has at most 8 details: the payee sends no more, a reader that holds 8 stores no other (5.3), and the payer pays by none its period record doesn't hold.
- `refs`: the request hash.
- **Rates and payment time** (spec 8.2). The rates apply to a payment whose independently verified payment time, from the payment method's evidence, is at or after their start and at or before `expires_at`, all read at millisecond precision, further digits truncated (Switch wire 1.6). Their start is the details' `created_at` cut to whole seconds, v0.1's `valid_from`: `2026-02-28T00:00:00.999Z` starts at `2026-02-28T00:00:00.000Z`. The payee sets `created_at` to the time it signs the details and never backdates them, as in v0.1. The payment method defines what counts as verified payment time, as in v0.1 (`specs/payment-conversion.md`): for an ERC-20 transfer, the canonical block timestamp of its successful execution; a Bitcoin app documents its own rule, such as its zero-confirmation policy. An evidence time in whole seconds, such as a block timestamp, is read as `.000`. The payee credits a payment at the rates only then; one verified outside that interval doesn't pay the period at those rates, and gets no receipt. The payer pays by the rates only while its device clock and its highest verified service time are both at or after their start and before `expires_at`, at planning and again after the attempt's answer, before money moves. Rates price a payment to an endpoint of another asset than the requested one, by the asset-rail rate, else the asset-wide one, as `specs/payment-conversion.md` says; a payment to an endpoint of the requested asset is paid 1:1 and isn't bound by them. Vectors: `periods.json` `details_rates`.

```json
{
  "billing_period": {
    "ends_at": "2026-03-31T00:00:00.000Z",
    "starts_at": "2026-02-28T00:00:00.000Z"
  },
  "expires_at": "2026-03-01T00:00:00.000Z",
  "payment_endpoints": {
    "btc-lightning-bolt11": "lnbc1pjexample0february0period"
  },
  "rates": [
    {
      "asset": "btc",
      "value": "0.00001234"
    }
  ]
}
```

### 4.10 `payment_proof`

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `attempt_id` | `b64(16)` | yes | the claim's attempt | |
| `billing_period` | Billing Period | for a recurring request | | |
| `payment_endpoint_identifier` | string | yes | the identifier paid | |
| `payment_app_id` | app-id | yes | the payee app whose endpoint was paid | |
| `list_version` | int | when paid to a Private Payment List | that list's `version` | |
| `amount` | Amount | for a payment without a request | | a request's amount comes from the request |
| `allowance_id` | uuid | for a payment under an Allowance | | omitted for every other payment, whether or not the user confirmed it (spec 8.2) |
| `label` | string | for a labelled request | the request's `label` (4.5) | absent for an unlabelled request and a payment without one |
| `payment_reference` | string | no | equals the request's | v0.1 required it; the request hash in `refs` now identifies the request |
| `proof` | object | yes | opaque, method-specific: an object whose members the payment method defines (`specs/payment-requests.md`); by convention `{ "type": string, ... }`, such as `{ "type": "bitcoin-bolt11-preimage", "data": "<hex>" }` in the vectors | |

`refs`: the request hash, then the hash of the period details the payment used, if any. A proof of a payment without a request has no `refs`; the payee records it by its settlement (Switch wire 1.5), so the same evidence sent again under a new `msg_id` is a replay. A proof with `refs` carries no `amount`; one without carries no `billing_period`, `label`, `payment_reference` or `allowance_id`, which name a request's context (an automatic payment under an Allowance pays a request, spec 8.3); and one that names the period details it used carries their `billing_period`. Recipients drop a proof that breaks this (vectors: `messages.json` `proof_members`).

The `amount` of a proof without a request is the asset and amount paid, which the payment method transferred exactly (spec 7.2 step 1). The payer's record of the payment and the payee's keep it as the stored proof gives it (5.3); a receipt carries none and confirms the proof whose hash it names (4.11); and the payee checks the payment by its own method (spec 8.1). Before the payer writes `done` with the proof, its record of the payment holds the destination and this amount (`unrequested`, 5.3).

```json
{
  "v": 1,
  "kind": "paykit.payment_proof",
  "msg_id": "5aabefde-df1d-43dd-99d1-086bac038944",
  "created_at": "2026-02-28T00:00:00.000Z",
  "from": {
    "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
    "app_id": "wejf37ngr7qizej3eoijn3hq4c"
  },
  "to": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
  "refs": [
    "c_ys4n4gUNKJa3D13sszDTpkj8rqdB206n-gIMVuzXY",
    "kM3kzLA74HW6gW84eMivgv3gCSowYHwWWlBefx5WXnU"
  ],
  "body": {
    "attempt_id": "4TyjQUac-NiiIf-haaHWbA",
    "billing_period": {
      "ends_at": "2026-03-31T00:00:00.000Z",
      "starts_at": "2026-02-28T00:00:00.000Z"
    },
    "payment_app_id": "brrz3m6g5rci6iqnzke44q4djc",
    "payment_endpoint_identifier": "btc-lightning-bolt11",
    "proof": {
      "data": "0d1c2b3a",
      "type": "bitcoin-bolt11-preimage"
    }
  }
}
```

Without a request:

```json
{
  "amount": {
    "asset": "btc",
    "value": "0.0015"
  },
  "attempt_id": "mAFdg5_0tVNjmrgiTUZW4g",
  "list_version": 1767225960000,
  "payment_app_id": "brrz3m6g5rci6iqnzke44q4djc",
  "payment_endpoint_identifier": "btc-bitcoin-p2wpkh",
  "proof": {
    "txid": "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
    "type": "bitcoin-txid"
  }
}
```

### 4.11 `payment_receipt`

| Field | Type | Req | Notes |
|---|---|---|---|
| `attempt_id` | `b64(16)` | yes | the attempt of the proof it confirms |
| `billing_period` | Billing Period | for a recurring request | the proof's |
| `metadata` | object | no | the payee's, opaque (1.1), as v0.1 `Receipt.metadata` |
| `payment_reference` | string | no | a payment without a request: the payee's reference for it, 1 to 256 characters, no control characters (4.5); recipients drop a receipt that carries it and a request hash, whose request's reference applies |

`refs`: the proof hash, then the request hash when there is a request. The payer's apps find the obligation and the record from the request hash, and may hold the receipt without the proof (Switch spec 10.2 row 3). A receipt with the proof hash alone names no Billing Period. A record that holds a receipt and its proof is invalid when their `attempt_id` or `billing_period` differ, and the record of a payment without a request when its receipt's `attempt_id` isn't the record's (5.3; vectors: `sealed.json` `activity_records`).

```json
{
  "v": 1,
  "kind": "paykit.payment_receipt",
  "msg_id": "f447883c-ff13-4876-81d6-86f54a79abd8",
  "created_at": "2026-01-01T00:08:00.000Z",
  "from": {
    "identity": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
    "app_id": "brrz3m6g5rci6iqnzke44q4djc"
  },
  "to": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "refs": [
    "BDGN__ffJTDNNNyF2lkTCrRNUox5u3NGNuwKgxAhcJc",
    "pukrdI5C7SfN7mlLheLGWRk2qzt8RTKXlXv4E1rRGKg"
  ],
  "body": {
    "attempt_id": "DgCVYQKL1eyDacZvG6YhZg"
  }
}
```

### 4.12 `resend_request`

Asks the counterparty to re-send messages it sent (spec 5, Switch spec 8.2, 12.6). Best effort.

| Field | Type | Req | Notes |
|---|---|---|---|
| `msg_ids` | `array<uuid>` | no | default `[]`; messages by ID |
| `accepted_from` | time | no | messages the recipient's inbox accepted at or after this time |
| `accepted_until` | time | no | ... and at or before this time; absent means no upper bound |
| `request_hashes` | `array<b64(32)>` | no | the requests named, and every message whose `refs` name one of them |
| `billing_period` | Billing Period | no | with `request_hashes`: only that period's messages |

- At least one of `msg_ids`, `accepted_from` and `request_hashes` is present, and the times parse; recipients drop any other (Switch wire 5.1 check 10).
- Times select by the acceptance times the sender kept (spec 9), at millisecond precision: the first one of each message or any later re-send's. To ask for an entry it can't open, an app sends `accepted_from` and `accepted_until` both equal to the entry's `accepted_at`, which returns every message that sender's inbox accepted in that millisecond; duplicates are dropped by `msg_id`.
- The sender re-seals each selected message it first accepted in the last 30 days to the current receive keys, with its original signed plaintext, and only messages of kinds that the requesting app's certificate allows it to receive (spec 9): a wallet with `paykit_pay` and `paykit_contacts` but no `paykit_allowance` gets back no Allowance messages. Of a payer's acceptances, rejections, cancellations and proofs, and a payee's cancellations, it re-sends only those check 8 lets the requesting app take under their label and role (Switch wire 5.1, Section 4.3): an app holding only `paykit_request:shop2` gets back none labelled `shop1` and none without a label, and one holding only `paykit_request` no payee's cancellation. A sender that keeps messages longer doesn't re-send older ones, since recipients keep processed IDs for 30 days. Retention runs from the first acceptance: the sender keeps one entry per `msg_id` with its first acceptance time and the later ones, and a re-send doesn't restart it.
- **Limits** (spec 9): the sender re-sends one message to the same requester identity at most once an hour (`RESEND_COOLDOWN_MS`), and at most a daily number of messages per requester identity, which it chooses (the prototype's `DEFAULT_RESEND_BUDGET`, 500); it leaves the rest out. Shifting a range by a millisecond selects the same messages, so it gets nothing new.
- **What an app asks for** (spec 9): the unopenable, unreadable and `missed` items that lead an app to ask come from the service, and the app can't check them. For an entry it can't open, an unreadable item and a `missed` item, it never asks for a time before its cursor's time less the clock tolerance (5 minutes): for the entry and the unreadable item, only if its `accepted_at` is no earlier than that, and for the `missed` item, from the later of its `accepted_from` and that time (Switch spec 8.2). For an unreadable item it also asks only after it has verified a departure of another own app in its chain (a `leave` or `revoke` transition, Switch wire 6.5) at or after the entry's `accepted_at`. It ignores an item whose times don't parse.
- A `resend_request` adds no conversation entry (Switch spec 7): a forged entry from a stranger must not admit that stranger.
- `billing_period` is read only with `request_hashes`; without them it selects nothing, and senders don't send it alone. An empty array counts as absent.
- `request_hashes` selects each named request and every message whose `refs` name one of them. With `billing_period`, only the messages whose body names that Billing Period are selected; the request itself, which names none, is left out. Selectors combine as a union, and each message is re-sent once, whatever its selectors (duplicates are dropped by `msg_id`).
- Any own Paykit App may ask for entries it can't open, whatever its Paykit scopes.

```json
{
  "accepted_from": "2026-01-01T00:00:30.123Z",
  "accepted_until": "2026-01-01T00:00:30.123Z"
}
```

By request and Billing Period (`messages.json` `resend_request_period`):

```json
{
  "billing_period": {
    "ends_at": "2026-03-31T00:00:00.000Z",
    "starts_at": "2026-02-28T00:00:00.000Z"
  },
  "request_hashes": [
    "c_ys4n4gUNKJa3D13sszDTpkj8rqdB206n-gIMVuzXY"
  ]
}
```

### 4.13 `private_payment_list`

The latest list of one publishing app (spec 8.4).

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `version` | int | yes | Unix milliseconds of publishing, higher than every earlier version from this app, even after a restore; while the app's lists wait for its host after a restore, above the later of the publishing time and the highest service time it verified by more than twice the clock tolerance (spec 8.4, Withdrawing a list) | |
| `payment_endpoints` | object | yes | Payment Endpoint Identifier to endpoint | |
| `expires_at` | time | yes | chosen by the publishing app from its current time, not from `version`; the prototype sets 30 days after publishing | a list without it, or with one that doesn't parse, is dropped (Switch wire 5.1 check 10). Payers use a list only until the earlier of `expires_at` and 30 days after its inbox accepted it (`accepted_at`), on their clock; past that it has expired, and they ask for a newer one (4.14) |

An endpoint:

| Field | Type | Req | Notes |
|---|---|---|---|
| `payload` | string | yes | non-empty |
| `single_use` | bool | no | default `false`, omitted when `false`; paying it consumes this list version |

A list with a lower or equal `version` than one held from the same app is ignored (Switch spec 8.2). An expired list is treated as missing (spec 8.4): the payer sends `private_payment_list_request`.

**Renewal** (Switch spec 4.2): the same rule as for records (3.2). Once less than half a list's lifetime (publishing to `expires_at`) remains, the publishing app sends the counterparty a newer list at its next run, with a higher `version` and a new `expires_at`; like any newer list it carries fresh single-use endpoints (spec 8.4). A list with more than half its lifetime left is not sent again.

```json
{
  "payment_endpoints": {
    "btc-bitcoin-p2wpkh": {
      "payload": "bc1qexample0single0use0address0000000000",
      "single_use": true
    },
    "btc-lightning-bolt12": {
      "payload": "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzr"
    }
  },
  "expires_at": "2026-01-31T00:06:00.000Z",
  "version": 1767225960000
}
```

### 4.14 `private_payment_list_request`

| Field | Type | Req | Notes |
|---|---|---|---|
| `held_version` | `int \| null` | no, always written | the `version` the sender holds, `null` when none |
| `app_id` | app-id | no | the publishing app asked about |

```json
{
  "app_id": "brrz3m6g5rci6iqnzke44q4djc",
  "held_version": 1767225960000
}
```

### 4.15 `allowance_proposal`

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `allowance_id` | uuid | yes | fresh | |
| `proposer_role` | string | yes | `allower` or `allowee` | the sender's role |
| `terms` | object | yes | the Allowance Terms of `specs/allowances.md`, every member present | |

Terms, as v0.1, all members required:

| Field | Type | Constraints |
|---|---|---|
| `asset` | string | non-empty, no control characters |
| `per_payment_amount` | `{ "minimum": amount, "maximum": amount } \| null` | minimum not above maximum |
| `period_limits` | `array<object>` | each `{ "amount_limit": amount \| null, "payment_count_limit": int \| null, "period": period }`, not both limits `null`, entries unique |
| `lifetime_amount_limit` | `amount \| null` | |
| `active_from` | `time \| null` | first eligible instant |
| `expires_at` | `time \| null` | first ineligible instant, after `active_from` |
| `allowed_payment_endpoint_identifiers` | `array<string> \| null` | non-empty, unique |

A period is `{ "kind": "anchored", "every": int, "unit": unit, "anchor": time }` or `{ "kind": "rolling", "every": int, "unit": "minute" | "hour" | "day" | "week" }`, with `every` at least 1. Any such `every` is valid: a period whose length or boundary can't be represented makes every evaluation that needs it ineligible, never the terms invalid (`specs/allowances.md`). Terms configure at least one monetary ceiling or an expiry (`specs/allowances.md`). Two period limits are duplicates only when every member is equal as spelled: `amount_limit` and `anchor` compare as strings, so `1` and `1.0`, or one instant spelled two ways, are distinct entries; `payment_count_limit` and `every` compare as integers, and `kind` and `unit` exactly (vectors: `allowance_usage.json` `period_limit_spellings`). A period limit carries both `amount_limit` and `payment_count_limit`, each possibly `null`; one that leaves either out is invalid (vectors: `allowance_usage.json` `period_limit_members`).

**Checking the terms:** apps check the whole table, amounts by 1.2 and times by Switch wire 1.6, when a proposal arrives, dropping one whose terms fail; when they verify an Allowance record, which is invalid with such terms (Switch spec 8.2); and before each automatic payment. A time that doesn't parse makes the terms invalid: it is never read as no limit. Senders don't propose invalid terms.

**Meaning of the terms** (v0.1 `specs/allowances.md`, restated here):

- `per_payment_amount` is an inclusive range: a payment is allowed when `minimum <= value <= maximum`.
- Limits are inclusive ceilings: with a candidate payment, a period's counted amount plus its value must be at most `amount_limit`, and its counted payments plus one at most `payment_count_limit`; the lifetime amount plus its value at most `lifetime_amount_limit`.
- A **monetary ceiling** is `per_payment_amount`, an `amount_limit` or `lifetime_amount_limit`; terms with none of them and `expires_at: null` are invalid.
- `active_from` is the first eligible instant and `expires_at` the first ineligible one, on the service's time of the executor batch.
- `allowed_payment_endpoint_identifiers`, when set, lists the only identifiers an executor may pay to.
- A request in another asset than `asset` is never paid under the Allowance (5.5).

**Unknown members in `terms`.** v0.1 made terms closed; v0.2 readers ignore unknown members (Switch wire 1.1), and the prototype does. An executor that ignores a restriction it doesn't know pays more than the proposer meant, so a later version that adds a restriction must also change `v` or the kind.

```json
{
  "allowance_id": "d0efffb7-191b-4709-b6c7-e343af5500b4",
  "proposer_role": "allowee",
  "terms": {
    "active_from": "2026-01-01T00:00:00.000Z",
    "allowed_payment_endpoint_identifiers": [
      "btc-lightning-bolt11"
    ],
    "asset": "btc",
    "expires_at": null,
    "lifetime_amount_limit": "0.10",
    "per_payment_amount": {
      "maximum": "0.01",
      "minimum": "0.0001"
    },
    "period_limits": [
      {
        "amount_limit": "0.03",
        "payment_count_limit": 5,
        "period": {
          "anchor": "2026-01-31T00:00:00.000Z",
          "every": 1,
          "kind": "anchored",
          "unit": "month"
        }
      }
    ]
  }
}
```

### 4.16 `allowance_acceptance`, `allowance_rejection`, `allowance_end`

| Field | Type | Req | Notes |
|---|---|---|---|
| `allowance_id` | uuid | yes | the proposal's |
| `executor_app_id` | app-id | no | in an acceptance from the allower: the executor app at acceptance. Informational; the grant in the Allowance record decides (spec 8.3) |

- Acceptance and rejection: `refs` is the proposal hash. Only the proposal's recipient answers.
- End: `refs` is the proposal hash and the acceptance hash. A proposal withdrawal by its sender has only the proposal hash.
- The terms hash is `hash(lp(proposal JSON bytes, acceptance JSON bytes))` (Switch wire 1.5).
- In an Allowance record (5.4), an end after the acceptance names exactly the proposal hash and that acceptance's hash; an end naming the proposal hash alone comes from the proposal's sender before any response; a withdrawal that crosses the acceptance, naming the proposal hash alone once the record holds the acceptance, isn't stored: the record reflects it by its `msg_id`, and the app holding the acceptance ends the Allowance with an end naming both (spec 8.3); at the proposer, whose record holds its withdrawal, a crossing acceptance or rejection, and the end naming both, aren't stored either: the record reflects them by their `msg_id`s; a withdrawal that crosses a rejection isn't stored, and the record stays rejected; a rejected Allowance has no end; and `executor_app_id` appears only in an acceptance, and only when the proposal's recipient is the allower (its proposer the allowee). Recipients drop a rejection or an end that carries `executor_app_id`, and a record that breaks the rest is invalid (vectors: `sealed.json` `allowance_records`).

```json
{
  "v": 1,
  "kind": "paykit.allowance_acceptance",
  "msg_id": "250b9ef2-c357-4856-b8c0-863368e627b5",
  "created_at": "2026-01-01T00:13:00.000Z",
  "from": {
    "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
    "app_id": "cw5ahtdkixcafna9zh1y5y5o1w"
  },
  "to": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
  "refs": [
    "d_lSlfB4ApJgnnzeHaXDXEPnY75IIb8lXrnihxE_vZE"
  ],
  "body": {
    "allowance_id": "d0efffb7-191b-4709-b6c7-e343af5500b4",
    "executor_app_id": "cw5ahtdkixcafna9zh1y5y5o1w"
  }
}
```

### 4.17 `executor_notice`

Between apps of one identity only, through the own inbox, wrapped only to the executor app (spec 8.3).

| Field | Type | Req | Constraints |
|---|---|---|---|
| `reason` | string | yes | `ended` or `handed_off` |
| `allowance_record` | `b64(32)` | yes | the record key of the Allowance record |

```json
{
  "allowance_record": "ErVk3byZhtqK-MMclhlnci5g1SeR79osx7Hxw5BlifA",
  "reason": "ended"
}
```

## 5. Shared record values

Own apps of one identity share state through sealed shared records (spec 6, Switch spec 10.3). Apps of different vendors on one identity read each other's records, so the plaintext of each Paykit collection is part of the wire format. A value is JSON (Switch wire 1.1), sealed as in Switch wire 6.2 under the collection's read scope. Readers verify every stored message on use (Switch spec 8.2) and work out status from the verified messages and the claim, never from a field alone. A writer that writes a record again keeps every member it doesn't know as it read it, in the value and in each object of it that it keeps, so a member a later version adds survives an older writer.

### 5.1 Fixed policies

| Collection | `read_scope` | `write_scopes` | Policy hash (vectors) |
|---|---|---|---|
| `payer_activity` | `paykit_pay` | `["paykit_pay"]` | `identifiers.json` `policies` |
| `payee_activity` | `paykit_request` | `["paykit_request"]` | |
| `payee_activity`, a request sent under a label | `paykit_request:<label>` | `["paykit_request:<label>"]` | `identifiers.json` `policies` (`shop1`) |
| `payment_lists` | `paykit_pay` | `["paykit_pay"]` | |
| `allowances` | `paykit_allowance` | `["paykit_allowance"]` | |
| `allowance_ledgers` | `paykit_allowance` | `["paykit_allowance_execute"]` | |
| `block_floor` | `paykit_pay` | `["paykit_pay"]` | |

Apps ignore a record in these collections whose policy differs. A labelled request's records, its request record and its period records, have its label's policy, are sealed with the label key (Switch wire 1.5) under the AAD of Switch wire 6.2 with `scope` `paykit_request:<label>`, and bind that label (5.3).

### 5.2 Stored message

A verifiable copy of one message (Switch spec 8.2):

| Field | Type | Req | Notes |
|---|---|---|---|
| `sender` | pubky | yes | the sender identity |
| `cert` | `signed<AppCert>` | yes | the sender's certificate, as the inbox entry carried it |
| `plaintext` | object | yes | `{ "sender_app": b64(16), "signature": b64(64), "json": b64 }`: the signed plaintext of Switch wire 4.5 without padding, the JSON as base64url of its exact bytes |
| `accepted_at` | `time \| null` | no | the acceptance time at the recipient's inbox, when known |

A reader checks a stored message as the inbox checked it, so far as the record allows: its certificate, its signature and its body, and the sender's side of check 8 (4.1): the sender's certificate holds a scope that sends the kind and, for a `payment_request`, its label. A record holding a stored message that fails is invalid (vectors: `sealed.json` `activity_records`).

### 5.3 Activity records (`payer_activity`, `payee_activity`)

One message per role (spec 6).

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | the other identity |
| `request` | Stored message | no | the `payment_request` |
| `response` | Stored message | no | acceptance or rejection |
| `cancellation` | Stored message | no | from either side |
| `proof` | Stored message | no | |
| `receipt` | Stored message | no | |
| `period_details` | Stored message | no | period records: the latest details held (4.9), or, once the record holds a proof that names details, those |
| `earlier_details` | `array<Stored message>` | no | period records: every other details held for the period, in 4.9 order, at most 7 |
| `request_hash` | `b64(32)` | no | period records: the request's hash |
| `billing_period` | Billing Period | no | period records |
| `attempt_id` | `b64(16)` | no | a payment without a request |
| `unrequested` | object | no | a payment without a request: its destination, below |
| `msg_ids` | `array<uuid>` | no | the `msg_id`s of the messages the record stores, and of at most 16 others it reflects, the newest (Switch spec 11) |
| `status` | string | no | informational, written by the last writer, never trusted |
| `allowance_id` | uuid | no | a payer's request record: the Allowance an executor accepted the request under, written in the acceptance's batch, conditioned on the Allowance record's version, or else the one its first automatic payment was made under, written in that attempt's batch; later automatic payments of the request and its periods use only that one (spec 8.3) |
| `attempted_periods` | `array<int>` | no | a payer's request record of a recurring request: the start, in Unix seconds as in the obligation ID (Switch wire 1.5), of every Billing Period that may have an open or `unknown` attempt; at most 16, ascending, each once (spec 8.2). Anywhere else, or with more, a repeat or another order, the record is invalid |
| `reassociations` | `array<object>` | no | a payer's request record of a recurring request, beside `allowance_id`: `{ "allowance_id": uuid, "from_period": int }`, the Allowance the user moved the Billing Periods from `from_period` on to (spec 8.3); at most 16, `from_period` a period start as in `attempted_periods`, strictly ascending; the first names another Allowance than `allowance_id`, and each another than the entry before it |
| `manual_only` | bool | no | `true` or absent: a payer's request record, or a payer's period record; no app pays the occurrence automatically, an executor or an app paying a Billing Period without its user's confirmation, and while the request record of a recurring request holds it no executor accepts that request and no app pays one of its Billing Periods automatically (spec 8.2, 8.3) |

`unrequested`: `{ "app_id": app-id, "endpoint_id": string, "payload": string, "source": "request" | "list" | "public", "list_version": int (for a list), "single_use": bool, "amount": Amount }`. `list_version` is present for a list and absent otherwise, and the stored proof's `list_version` is the same (4.10). `amount` is the asset and amount being paid: every app writes it with the `attempt`, and the stored proof's `amount` is the same.

`request_hash`, `billing_period` and `earlier_details` appear only in a period record, `attempt_id` and `unrequested` only in that of a payment without a request, and `allowance_id` and `reassociations` only in a payer's request record, each by the record's binding (below), and `manual_only` only where the table says; a record that carries one anywhere else is invalid (vectors: `sealed.json` `activity_records`).

A payment without a request to a public endpoint made while the payer's own switch didn't answer (spec 8.4) has the same record and claim, written once the switch answers: first the batch of its `attempt` (with a `deadline` 10 minutes after the service time of the switch's first answer once it answers again, such as the read of the claim below; spec 8.4) and this record, then `done` and the record with its proof. The payer reads the payment's claim first: a lost answer may have had the first batch applied, and then only the record is read back.

A request record sits at the key derived from `blind(lp(payee, request hash))`, a period record at `blind(lp(payee, request hash, u64(period start)))` (Switch wire 1.5). Readers check that the stored request's hash matches the record key, recomputing it with their own naming key.

**Binding** (Switch spec 8.2): every stored message names the record's obligation by its own signed fields, never by the record's unsigned members:

- the request hash at its place in `refs`: first in an acceptance, rejection, cancellation, period details and proof, second in a receipt, after the proof hash it confirms (4.6 to 4.11);
- for a Billing Period, the signed `billing_period` of the period details, proof and receipt.

In a request record these must name its request, and none may name a Billing Period. **Labels** (spec 6): in a `payee_activity` record under a labelled policy, the stored request and every message from the payer (acceptance, rejection, the payer's cancellation, proof) carry that `label`; under the fixed policy none does. The payee's own messages, period details, receipts and the payee's cancellation, carry no `label`, and readers don't check them for one. A period record holds no request, so under a labelled policy the payer's messages in it carry the label. A record that breaks this is invalid. The record key derives from the policy hash, so a labelled request's record sits at another key than an unlabelled one's. A `payer_activity` record keeps the fixed policy whatever the request's label. Vectors: `sealed.json` `labelled`, with its rejections (another label's policy, the fixed policy, an unlabelled request under a label). **Seen Allowance:** an app that has seen a request record's `allowance_id`, or an entry of its `reassociations`, ignores and flags a write that changes or drops it, or that adds an entry not after the last it saw (Switch spec 8.2). The `record.put` that adds an entry carries `server_time_in(null, s)`, `s` the entry's `from_period`, so the period hasn't started on the service's clock. A writer adds an entry only with a `from_period` later than every period in the `attempted_periods` of the record version its write builds on, and a reader that holds that version ignores and flags a write whose new entry's `from_period` is at or before one of them: a period attempted early stays under the Allowance it was attempted under (spec 8.2, 8.3). An attempt on a later period, early or not, counts under the Allowance of the last entry starting at or before it. **Seen marks:** an app ignores and flags a write that drops a `manual_only` it saw. A record without a `request` is a period record if any message in it names a request or a period, or `request_hash` or `billing_period` is set; then every message in it must name both, all the same ones, the key is recomputed from them, and `request_hash` and `billing_period`, if set, must equal them. A message missing a binding field makes the record invalid. A record whose messages name no request is that of a payment without a request.

**Attempted periods** (spec 8.2). The batches that read or write `attempted_periods`:

- **An attempt on a Billing Period not listed** carries, besides the `attempt` with its conditions (`record_version` of the request record, `server_time_in(s, null)`, 4.5), a `record.put` of the request record at its next version: the list with the period added and, for each listed period it drops, a `claim_step` condition on that period's claim (Switch wire 1.5), at the step the writer read, on the `record.put` itself. It drops only periods whose claim it read with no open or `unknown` attempt. With 16 listed and none to drop, the app makes no such attempt. An executor's batch writes the request record once, with `allowance_id` too when it names the Allowance (5.5).
- **An attempt on a listed period** doesn't write the request record; its `record_version` condition is enough.
- **Any other write** of the request record may drop listed periods the same way, each with its `claim_step` condition; one that doesn't drop keeps the list as it read it.
- **A payer's cancellation** (4.8) is a `record.put` of the request record at its next version, with a `claim_step` condition for the claim of every listed period, at the step read, none of which had an open or `unknown` attempt.
- **Conditions per operation:** these `record.put`s are the Paykit operations with the most conditions: at most 18, one `claim_step` for each of the at most 16 listed periods and, where one is carried, a `record_version` and the `server_time_in` of a write that adds a reassociation (Seen Allowance, above); every other Paykit operation carries at most two (4.5, 5.5). The pinned Switch sets no limit on the number of conditions per operation.

A reader that holds the version a write was built on (its `expected_version`) checks every listed period the write drops: the `record.put` must carry a `claim_step` condition on that period's claim. One that doesn't is ignored and flagged, like a dropped role (Seen roles, below), and the reader keeps the period listed. The reader checks only that the condition is there, among the conditions the write's operation signed, which come with the write (Switch spec 8.2): the service enforced it against the claim as it stood when it applied the write, and it enforced a cancellation's conditions the same way, so readers don't check those. Readers check the list's shape: ascending, each start once, at most 16. A start that names no Billing Period of the request fences nothing. An empty list is the same as none, and writers leave it out. `status` never comes from the list (Switch spec 8.2).

**`msg_ids` is bounded** (Switch spec 11): a writer keeps the IDs of the messages the record stores and the 16 newest of the others it adds (`RECENT_MSG_IDS`), in order, and drops older ones; each app's processed pairs, kept 30 days, cover those. Each ID appears once, and a repeat is dropped at its later place before the 16 are taken, so the 16 are distinct; a reader reads a list that repeats an ID, or names more, as the list a writer would have written (vectors: `sealed.json` `msg_ids_bounds`). A reader treats a message the record stores as reflected whether or not the list names it. So a record stays bounded however many messages repeat a role it holds.

**Seen roles** (Switch spec 8.2): a record holds one message per role, so a reader that has seen a message in a role ignores, and shows with its writer, a later write that drops or replaces it. Period details it saw, in `period_details` or `earlier_details`, stay in one of them: a later write that drops one, or holds other details as late with another hash, is ignored the same way. A write may move `period_details` only to later details, except the write that stores the proof, which sets it to the details the proof names. A delete isn't such a write (spec 6), but the reader keeps what it saw, so a later write that brings the record back without it is one.

The prototype writes every member of an activity record, with `null` for an absent one, as the example shows, except `allowance_id`, which it leaves out when absent, `attempted_periods`, `earlier_details` and `reassociations`, which it leaves out when empty, and `manual_only`, which it leaves out when false; readers treat `null` like a missing member (Conventions).

Example (alice's `payer_activity` record of bob's request, certificate and plaintext cut short):

```json
{
  "v": 1,
  "counterparty": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
  "request": {
    "sender": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
    "cert": {
      "body": "eyJ2IjoxLCJraW5kIjoicHVia3ktc3dpdGNoLmFw...",
      "sig": "DcZij9RBJFkc9zKg4JRc9zdz2sgDvUR0ZAWP2_VBgKqCUIJFKQSy2x2uKApzYCfiFD6DFvKL161n6kj-J_FGBw"
    },
    "plaintext": {
      "sender_app": "CQl8r8bZGV9VwrqRrTtDSw",
      "signature": "9AI7IHiwT4Ze9m4FJmNLSlcb9gR0gjXtW9WQP--XzeWUaaVjIuTXkuBMzJNAzOIq-I_WsvGpOPsggy4Och3JCg",
      "json": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LnBheW1lbnRf..."
    },
    "accepted_at": "2026-01-01T00:00:00.412Z"
  },
  "response": null,
  "cancellation": null,
  "proof": null,
  "receipt": null,
  "unrequested": null,
  "attempt_id": null,
  "msg_ids": [
    "04660a75-8d0e-46a9-a230-3d9e3d5e7d75"
  ],
  "status": null,
  "period_details": null,
  "request_hash": null,
  "billing_period": null
}
```

### 5.4 `payment_lists` and `allowances`

**`payment_lists`**, one per payee app:

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `payee` | pubky | yes | |
| `app_id` | app-id | yes | the publishing app |
| `list` | Stored message | yes | the latest `private_payment_list` |
| `used` | `array<object>` | no | single-use endpoints used: `{ "version": int, "endpoint": string, "attempt_id": b64(16) }`, each written in the batch of the `attempt` that pays it, whatever it pays: a request, a Billing Period, an automatic payment or none (spec 7.2) |
| `msg_ids` | `array<uuid>` | no | bounded as in 5.3: the list's own and the 16 newest others |

**Binding** (Switch spec 8.2), as for activity records: the reader recomputes the record key with its own naming key (Switch wire 1.5) and acts on the record only if

- the key is the one derived from `payee` and `app_id`;
- the stored list verifies, addressed to this identity, its sender identity is `payee` and its sender app is `app_id`.

**Seen lists** (Switch spec 8.2; Section 4.13): a reader that holds a `payment_lists` record ignores, and shows with its writer, a later write whose list has a lower `version`, or the same `version` and another hash, and one that drops a `used` entry it saw, whatever the newer list's `version`: a newer list doesn't reset `used`.

**`allowances`**, one per Allowance, on either side:

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | |
| `allowance_id` | uuid | yes | |
| `proposal` | Stored message | no | |
| `response` | Stored message | no | acceptance or rejection |
| `end` | Stored message | no | |
| `conflicted` | Stored message | no | a second `allowance_proposal` under the record's `allowance_id`, from either side, with another hash than `proposal`, or a second `allowance_acceptance` or `allowance_rejection`, beside `response`, with another hash than it: the Allowance is conflicted (spec 8.3). Writers leave it out when absent |
| `grant` | Executor grant | no | the current grant (3.3): the last entry of `grants` |
| `grants` | `array<object>` | no | the grant history, below; default `[]` |
| `msg_ids` | `array<uuid>` | no | bounded as in 5.3: those of the messages it stores and the 16 newest others |

**Binding** (Switch spec 8.2): the reader recomputes the record key with its own naming key (Switch wire 1.5) and acts on the record only if

- the key is the one derived from `counterparty` and `allowance_id`;
- there is a `proposal`; a record with a `response` or an `end` but no proposal is invalid;
- each member holds its kind: `proposal` an `allowance_proposal` with a `proposer_role` of `allower` or `allowee` and valid terms (4.15), `response` an `allowance_acceptance` or `allowance_rejection`, `end` an `allowance_end`, and `conflicted` either an `allowance_proposal` as `proposal` is, with another hash than `proposal`, or, beside a `response`, an `allowance_acceptance` or `allowance_rejection` as `response` is, with another hash than `response` (vectors: `sealed.json` `allowance_records`);
- every stored message names that `allowance_id` (the proposal's body, and the body of the acceptance, rejection or end, 4.15 and 4.16) and is between this identity and `counterparty`, with the sides its kind requires: the proposal, and `conflicted` holding a proposal, from either side, the response, and `conflicted` holding a response, from the proposal's recipient, the end from either side, each addressed to the other; the response, a conflicting response and the end refer to the proposal in `refs`.

A grant doesn't affect whether the record is valid. Grants are checked against the terms hash of the record's own proposal and acceptance (Switch wire 1.5); without an acceptance no grant is valid. `grant` is the current grant only if it is the last entry of a usable history and verifies for this identity, `counterparty` and the Allowance at that entry's version (3.3), and the record holds no `conflicted`; otherwise the record has no current grant. A conflicted record still names the executor at each version (Grant history, below), by which its ledger's later writes are checked. A history entry that doesn't verify means "no valid grant from this version on" (Valid entries, below), and a history that isn't usable makes every ledger check against it fail closed.

**Seen roles** (Switch spec 8.2, spec 6): an `allowances` record holds one proposal, one response, one end and one `conflicted`, so a reader that has seen one of them ignores, and shows with its writer, a later write that drops or replaces it, by hash. So no write clears a conflict: once a reader has seen `conflicted`, it ignores every later write without it.

**Grant history** (spec 8.3 "Who wrote it"). `grants` holds every executor grant the record has held, oldest first, each with the record version the write adding it built on:

| Field | Type | Req | Notes |
|---|---|---|---|
| `generation` | int | yes | the generation of the version the adding `record.put` read (its `expected_version`), 0 for a new record |
| `version` | int | yes | that version's `seq`, 0 for a new record; with `generation`, signed by the grant (3.3) |
| `grant` | Executor grant | yes | 3.3 |

- **Writing.** An executor that writes its grant, at acceptance, at a handoff, or when a confirmation signs it again (spec 8.3), appends an entry naming the version its write builds on, the one it read, and sets `grant` to the same grant. A write that fails with `409` is rebuilt from a fresh read, so an entry's version is always the one the adding write built on. Writers only append: every other write keeps `grants` as read.
- **A usable history** has versions that strictly increase, ordered by (generation, `seq`), and ends with an entry whose grant equals `grant`. Otherwise every ledger check against it fails closed.
- **Valid entries.** In a usable history an entry names an executor only if its grant verifies at the entry's version for this identity, counterparty and Allowance, and names the current terms hash (3.3). Any other entry means "no valid grant from this version on".
- **The executor at a version** v is the one named by the last entry whose version is older than v: the adding write built on that version, so the grant holds from the write's own version, which is newer. A ledger write (5.5) counts only if it carries `record_version` on the Allowance record at some v no newer than the Allowance record the app holds, and its writer is the executor at v. This holds for versions the app never saw: a version that is now a stub, an app added later, or a new executor checking its predecessor's writes.
- **Rewrites.** An app that has seen a history ignores and flags a write whose `grants` doesn't start with it (Switch spec 8.2). An app that never saw it relies on the signed versions: a moved grant doesn't verify (3.3). Dropping an entry is still possible.
- **Records without a history** have no current grant, until the executor writes its grant again.
- **Size.** An entry with its certificate takes about 1 KiB, so a record whose stored messages are small holds about 200 grants within the 256 KiB cap, and fewer beside large ones: unlike the kinds activity records store (4.1), Allowance messages are bounded only by the envelope limit of 65536 bytes (Switch wire 4.4).

Example entry (`documents.json` `executor_grant_history_entry`: the record was created at (1, 12) and the executor's acceptance write built on it; certificate cut short):

```json
{
  "generation": 1,
  "grant": {
    "app_id": "cw5ahtdkixcafna9zh1y5y5o1w",
    "cert": {
      "body": "eyJ2IjoxLCJraW5kIjoicHVia3ktc3dpdGNoLmFw...",
      "sig": "wX8NrG-Xn5tfiN4Nhe9iON15nc3rgBO60ysBJnuvWGmT0WgAUaOXHKRiaMvqT0FArsXOJXWFeiVxni5Jq6ynAw"
    },
    "confirmed_generation": 1,
    "sig": "TwAuCSVx5eKgCkbbyAxeqddsr0xSL5QebVA794GsLihQO5VxA0O_qUCAk30KFH2TNuVgcWqpORJMNWNEaaLGDw",
    "terms_hash": "PEj43Oz6GuVKcLXW3n5vIbBdSORajscVVsogpqd7Y9M"
  },
  "version": 12
}
```

### 5.5 `allowance_ledgers`

**Binding** (Switch spec 8.2): the reader recomputes the record key with its own naming key (Switch wire 1.5) and acts on the ledger only if the key is the one derived from its `counterparty` and `allowance_id`, and both fields equal those of the reader's `allowances` record for that Allowance; the ledger's checks below run against that record. A ledger, Allowance record or `payment_lists` record that fails its binding is invalid: the reader doesn't act on it, flags it as Switch spec 8.2 says, and keeps the content it trusts. A reader that holds no `allowances` record for the Allowance yet, or one without an acceptance, doesn't act on the ledger either, but flags nothing: it keeps the ledger version it holds until it holds such a record with an acceptance (spec 8.3). Vectors: `sealed.json` `binding_accepted` and `binding_rejections`.

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | the allowee |
| `allowance_id` | uuid | yes | |
| `entries` | `array<object>` | no | one per attempt under the Allowance, below |
| `totals` | object | no | `{ "spent": amount, "count": int, "failed": array<object> }`: paid amounts and payments of compacted entries, and the compacted `failed` entries, each `{ "obligation": b64(32), "attempt_id": b64(16), "value": amount }`, so a later `done` still counts once; `spent` may be `""` for none, and `failed` is left out when empty. |

An entry:

| Field | Type | Req | Notes |
|---|---|---|---|
| `obligation` | `b64(32)` | yes | the request's obligation ID, the claim key of its claim (Switch wire 6.2) |
| `attempt_id` | `b64(16)` | yes | |
| `value` | amount | yes | the amount paid; the payment method transfers exactly this (spec 7.2 step 1), and fees aren't counted |
| `asset` | string | yes | |
| `at` | time | yes | the time the executor stated, vouched for by `server_time_in(at, at + 60 s)` |
| `outcome` | `"done" \| "failed"` | no | the attempt's outcome, once known: `done` is final; `failed`, written only when the attempt's owner failed it or the user released the amount, releases it until a later `done` replaces it |

- **Executor batch** (spec 8.3): the ledger write carries `record_version(Allowance record)` and `server_time_in(at, end)`, where `end` is `at + 60 s` cut at the next boundary of any anchored limit; the `attempt` carries the request's `deadline` and `record_version(request record)` (spec 7.2); a third operation writes the activity record showing the payment as being handled (spec 8.3 step 3); when the request record doesn't name the Allowance yet, that record, or for a Billing Period a further write of the request record, carries `allowance_id` (5.3), and for a Billing Period not yet in `attempted_periods` that one write of the request record also adds it (5.3); and one more writes the `payment_lists` record when a single-use endpoint is paid (5.4).
- **The amount transferred** (spec 7.2 step 1): before the batch, the executor has its payment method report what it will transfer to the destination; another amount or asset, an amount it can't tell, or a fee above the app's cap makes the destination unusable for automatic payment.
- **Last ledger version** (spec 8.3): the executor keeps the version and item hash (Switch wire 6.6) of the last ledger write it wrote or read with an authenticated version (Switch wire 6.2 Accepting a write). Before an automatic payment, the ledger write `GET /records` shows must be that one, by item hash, or one whose signed `expected_version` is that version or later. Another write, `404`, or a write that fails the checks of Switch wire 6.2 is a rollback (Switch spec 10.1), except a `404` after a delete the executor holds at or after the kept version, and a write whose operation verifies with its writer's certificate (the record view's `cert`, the certificate the service checked the write against, Switch wire 6.2 step 1), that is the kept write or states `expected_version` at or after the kept version, but that fails a check of Switch wire 6.2 by policy (its writer's standing): those are refused as untrusted ("Building on a version"), which pauses the Allowance. An operation that doesn't verify with its writer's certificate, or doesn't match its stub (Switch wire 6.6), is a rollback even without a kept version. An executor that held a ledger version with an authenticated version before it became the executor keeps it as its kept version; one that never held one, such as a new one after a handoff that never read the ledger, builds on the write `GET /records` shows once it passes the other checks of "Building on a version", below, and keeps it from then on (spec 8.3; spec 17, question 34).
- **Counting:** an entry's own `done` spends, and its own `failed` releases unless its attempt's claim now shows `done`, which spends. An entry without an `outcome` counts by its attempt's claim: `open` and `unknown` reserve, `done` spends, `failed` releases only when `resolved_by` is the attempt's owner (Switch wire 6.8) and otherwise reserves until the user releases it, and an entry whose claim is gone, or whose claim doesn't hold the attempt, spends. A `totals.failed` item counts as an entry's own `failed` does, in the lifetime amount only, since its `at` is past every period: it releases unless its attempt's claim now shows `done`, which adds its value to the lifetime amount until the executor moves it into `spent` and `count` (Outcomes written, below), so a later `done` counts once and never as less. An anchored limit counts entries whose `at` is in the period containing the evaluation time; a rolling limit of length L counts an entry while `t < at + 60 s + L`.
- **Outcomes written** (spec 8.3): the executor writes `done` from a claim, `failed` from a claim only when its `resolved_by` is the owner, `failed` for an amount the user released, and `done` over `failed` when a claim shows it later. For that it reads again the claims of `failed` entries and of compacted failed attempts; a compacted one whose claim shows `done` moves from `totals.failed` into `spent` and `count`, once. The service keeps those claims to their deadline plus 30 days (Switch wire 6.8), and a `failed` entry or `totals.failed` item stays at least as long, so a `done` a late receipt records in that window is seen. A receipt after the window can't be recorded on the claim, which is gone; an executor that gets one treats the attempt as `done` all the same and writes `done` over the entry's `failed` (or moves the compacted item into the totals).
- **Compaction:** entries with a final outcome and `at + longest period + 60 s < now` are dropped, `now` being the latest service time the executor has verified: a `done` one's value and count are added to `totals`, and a `failed` one is kept in `totals.failed`. The longest period of an anchored limit is the length of its period containing `now`. A ledger that can't take a new entry within the 256 KiB limit even after compaction pays nothing automatically (spec 8.3).
- **A valid ledger** (spec 8.3 Compaction): every entry's `asset` is the terms' `asset`, each `attempt_id` appears once among the entries and `totals.failed`, and every `value`, `at` and `totals.spent` parses (Switch wire 1.6, Section 1.2). Usage and compaction refuse a ledger that isn't valid; they never read it as smaller.
- **From one version to the next** (spec 8.3): a reader that holds a ledger version accepts a newer one only if it counts no less: every held entry is still there with the same `obligation`, `attempt_id`, `value`, `asset` and `at`, its `outcome` unchanged or gone from none to `done` or `failed`, or from `failed` to `done`; or it was compacted, its `at + longest period + 60 s` before the newer write's service time (Switch wire 6.6 stub or Switch wire 6.5 proof, else the reader's latest service time): a held `done` entry into the totals, and a held entry without an outcome or `failed` into `totals.failed` with the same `obligation` and `value`, or into the totals, since it may have become `done` between the two versions, the longest period being that of the terms of the reader's `allowances` record (Binding, above) (what decides is the outcome it had when it was compacted, which the reader may not have seen); every held `totals.failed` item is still there unchanged or left into the totals; and `spent` and `count` grew by at least the values and number of what left into them. An attempt that left into the totals is in neither the newer `entries` nor its `totals.failed`, so it counts once. New entries are allowed. A newer version that breaks a rule is ignored and flagged (Switch spec 8.2).
- **Building on a version** (spec 8.3): the executor writes a ledger version, in any batch, only on a write that passes the kept-version check above, the `distrust_from` check (Switch spec 12.2 step 3) on the Allowance record and the ledger, "Who wrote it" (5.4) and the rules above against the last version it accepted; its `record.put` then carries that write's version as `expected_version`. On a refusal it writes no ledger version and the Allowance needs the user's confirmation again; an outcome batch carries the claim outcome alone.
- **A grant implies a ledger** (spec 8.3): the executor that writes an Allowance record's first grant then writes the empty ledger (`entries` and `totals` empty), conditioned on the new Allowance record version; where a ledger record is already at the key, it writes no empty ledger over it and takes that ledger as one it didn't accept ("After a confirmation", below). Before that write lands, the executor that wrote the grant takes a `404` as the empty ledger only while its own journal, not one restored from a backup, shows the empty ledger not yet sent; otherwise a `404` is "usage history lost". Afterwards a `404`, or a write whose sealed value is empty, is "usage history lost" for the executor: untrusted, not paid on.
- **After a confirmation** (spec 8.3): the executor computes the rewrite and shows its figures (usage per period limit, lifetime amount, entries reserved, spent and released, amounts held reserved) before the user confirms; the confirmation computes it again and writes it only if the ledger and the version it builds on are the same as shown, else it refuses. The rewrite starts from the last ledger version it accepted, which it keeps with its content; adds from the ledger shown the entries and `totals.failed` items it lacks (an entry's `outcome` only if `done`; entries of another asset, or that don't parse, are left out), a `done` for an entry it holds, and the higher `spent` and `count`; adds its journal's attempts under the Allowance; and adds every attempt on a claim of the counterparty's obligations whose `owner` is an app the grant history names, unless that attempt's owner failed it or its verified proof names no `allowance_id` or another one, with the request's amount and the attempt's `opened_at` as `at`.
- **Rules the vectors settle**, as the prototype applies them:
  - **Order of refusal:** the per-payment range first, then each period limit in the order of `period_limits`, its amount before its count, then the lifetime limit; `refused_by` names the first that refuses.
  - **Rolling window:** an entry counts while `t < at + 60 s + L`, with no lower bound, so an entry dated after t (within the 60 s window) counts too.
  - **Lifetime and counts** include reserved entries as well as spent ones; compaction adds only `done` entries to `spent` and `count`, and keeps `failed` ones in `totals.failed`.
  - **Final** for compaction means the entry's own `outcome` is `done` or `failed`; an entry without one stays, whatever its claim shows.
  - **No period limits:** the longest period is 0, so final entries older than 60 s are compacted.
  - **Assets:** an executor refuses a request whose amount's `asset` differs from the terms' `asset` before evaluating limits, so a ledger holds entries of one asset; an entry of another asset makes the ledger invalid (above), and usage and compaction refuse it.
- Vectors: `allowance_usage.json` has every limit kind, the outcome table (a `failed` claim entry carries `resolved_by_owner`), a case for a `failed` by another app and for a ledger `failed` whose claim is now `done`, and a compaction that keeps a `failed` entry in `totals.failed`.

### 5.6 `block_floor`

One record per identity, its block floor (spec 8.1, Blocked counterparties):

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `floor` | time | yes | a one-time request whose signed `created_at` is earlier than it plus the clock tolerance is paid only by hand |

**Binding** (Switch spec 8.2): the reader recomputes the record key with its own naming key (Switch wire 1.5) and acts on the record only if it is the key derived from its own identity.

**Seen floor** (Switch spec 8.2): a reader that holds a `block_floor` record ignores, and shows with its writer, a later write of a lower `floor`, so the floor it applies is the highest it has seen. A delete, or a value that doesn't read, keeps the floor seen; with none seen, the reader can't read the floor, and every one-time request counts as dated before it (spec 8.1). Before each automatic dispatch of a one-time request, and each resume's repeat, the app reads the record again from its switch and applies the higher of the floor that read shows and the one it holds; a read that fails, or a value that doesn't read with none seen, withholds the dispatch (spec 8.1, 8.3).

**Raising:** a writer builds on the version it holds and writes the higher of the floor held and the time it raises to, in the batch of the ask that needs the raise; after `409 conflict` it reads the record again and builds anew.

Vectors: none. Its policy is that of `payer_activity`, whose hash `identifiers.json` `policies` has; `paykit-lib` checks the value and the seen floor (spec 18).

## Appendix A. Choices this document made

The prototype fixed every other detail above when this document was written; the rules this revision adds since, which spec Section 18 lists, it predates. These details it left open, so this document chose; a row whose prototype cell says it differs names a divergence:

| # | Detail | Choice | Prototype |
|---|---|---|---|
| A1 | Time comparisons | as instants at millisecond precision (Switch wire 1.6); period details ordered by `created_at` instant, then `msg_id` text (4.9) | compares milliseconds; orders period details by the `created_at` text, the same order for the fixed form |
| A2 | Amount bounds | at most 18 fraction digits and value times 10^18 below 2^128, integer part of at most 20 digits advised; rates above zero (1.2) | the same bounds through its `u128` decimal; differs: checks that a rate is above zero only when it prices a payment, not when it reads the body |
| A3 | `accepted_payment_endpoint_identifiers` | optional, since the destination already limits a request to the payee's own endpoints (spec 8.1), and an app that receives no payments can't know which identifiers the identity's other apps publish; narrows the identifiers, and holds every key of `payment_endpoints` when both are present (4.5) | differs: ignores it, so it neither narrows nor checks the identifiers, nor matches rate selectors against them (4.4) |
| A4 | `payment_reference` in a proof | optional; when present it equals the request's (4.10) | doesn't write it |
| A5 | Period details | at least one of `payment_endpoints` and `rates`; v0.1's `valid_from` is `created_at` cut to whole seconds; each valid until its own `expires_at`, at most 8 per period (4.9) | differs: doesn't check that details carry one, and predates the selector rules, the start and the bound of 4.9 |
| A6 | `resend_request` selectors | `request_hashes` and `billing_period` added for the spec's "by request or Billing Period"; at least one selector; both range ends inclusive, an absent end open (4.12) | implements every selector |
| A7 | `reason` on rejection and cancellation | kept from v0.1, optional (4.7, 4.8) | sends `{}` |
| A8 | Allowance terms and unknown members | ignored, as everywhere in v0.2; a later restriction needs a new `v` or kind (4.15) | ignores them |
| A9 | Record expiry | at most 30 days after `updated_at`; required with public endpoints, `null` without (3.2); Private Payment Lists 30 days (4.13); renewed at the next run once less than half the lifetime remains | 30 days; renews after each feed read once past half the lifetime (`renew_destinations`) |
| A10 | How many `msg_id`s a record keeps | those of the messages it stores and the 16 newest distinct others, each once; readers read a list in that form (5.3, 5.4) | keeps the 16 newest others (`RECENT_MSG_IDS`); differs: predates the rules that repeats are dropped before the 16 are taken and that readers bound the list |
| A11 | Re-send limits | one message to one requester identity at most once an hour; a daily number of messages per requester identity the sender chooses (4.12) | 500 a day (`DEFAULT_RESEND_BUDGET`, `AppConfig.resend_budget`) |
| A12 | What a compacted `failed` entry keeps | its `obligation`, `attempt_id` and `value` in `totals.failed`, so the executor can read its claim again and add a later `done` to the totals once (5.5) | the same (`CompactedFailed`) |
| A13 | Record age limit | `record_max_age` in whole seconds, at least 1, 90 days when absent; an `updated_at` more than 5 minutes ahead is refused (3.2, Switch wire 3.3) | `DEFAULT_RECORD_MAX_AGE_S`, `AppRecord::payable_until` |
| A14 | Extension scopes and kinds | `paykit_ext:<prefix>`, prefix `paykit-` and 1 to 25 more of `a-z`, `0-9`, `-` (so its kinds are Paykit's, Switch wire 3.1 "Scope roots"); kind `<prefix>.<name>`, name 1 to 64 of `a-z`, `0-9`, `_`; at most 8 extension scopes per certificate; they give `conversation.add` | differs: accepts a prefix without `-`, but `paykit` (`records::extension_scope`, `check_scopes`); it predates this rule. Received extension messages are handed over in `ReceiveReport::extensions` |
| A15 | The settlement ID of a `bitcoin-txid` proof | its `txid`, 64 lower-case hex characters, as text, without an output index; a proof in another spelling isn't credited (Switch wire 1.5) | `wire::settlement_id`, which the rail uses for proofs with a `type` |

The vector files follow these choices.
