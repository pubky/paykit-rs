# Paykit v0.2 wire format

This draft defines Paykit’s encodings and validation. The [specification](spec.md) identifies the normative Pubky Dispatch revision and each separately adopted contract. Wire Section 5 identifies the protected-record paths whose positive wire contract is incomplete. Appendix A lists encoding choices.

**Invitation delivery.** All invitation-specific rules and citations use [Dispatch revision `6ef258d1ada96db7637394b57307335cb802b461`](https://github.com/pubky/pubky-dispatch/blob/6ef258d1ada96db7637394b57307335cb802b461/spec/wire-format.md), as named in the specification's **Invitation delivery** paragraph. These include registration (6.2), its service-written `reply.add` transition (6.5), the request header and trusted-time rule (7.1, 7.3), append and retry by the same sealed envelope (7.4.3), the allowlist response (7.4.6), readable feed entries (7.5) and export parts (10.3). The admission encodings these sections touch, the permission `reply.add` that registering and withdrawing an invite code require and that gives access to `invite_code.*` feed entries (6.2, 7.5), the service-written `reply.add` transition and which apps may read it in the feed (6.5, 7.4.3, 7.5), the `peers` array of the allowlist response (7.4.6) and the `admission` export part (10.3), are those of the peer admission adoption at Pubky Dispatch revision `964c832895630465812292ded65227104dca22ca` (spec Section 4), not of this invitation revision. Unrelated Dispatch rules retain their earlier references. No invitation member is added to a signed Paykit message body, and the stored Paykit vectors are unchanged. The [implementation table](implementation.md) links the SDK runtime checks.

**Test vectors.** The [vector guide](vectors/README.md) lists the files, fixed inputs and regeneration order. The [implementation table](implementation.md) distinguishes library checks, generated examples and wallet behavior.

**Choices.** Where the spec is silent, Appendix A records the choice this document made.

**Conventions.**

- "Section N" means this document; "spec N" means the Paykit v0.2 specification; "Dispatch spec N" and "Dispatch wire N" mean the Pubky Dispatch's.
- Field tables have the columns Field, Type, Req (required), Constraints and Notes.
- Types:

  | Type | Meaning |
  |---|---|
  | `amount` | decimal amount string (1.2) |
  | `uuid` | UUID v4 string (Dispatch wire 1.7) |
  | `list-version` | required positive `generation` and `counter` integers, compared generation first, then counter (4.13) |

  The other types are the Dispatch's (Dispatch wire, Conventions).
- **Absent and null.** "Req: no" means the member may be missing. Writers omit absent optional members, except where a table says the member is always written, and except in the shared record values of Section 5, whose writers may write every member with `null` for absent. Readers treat `null` like a missing member, unless the table says `null` has its own meaning, and except `lost_to_delete`, whose writers always omit it when absent and whose `null` is a present member that isn't a valid value (5, Deletion data that doesn't read).
- Examples of signed documents show the decoded body. Long base64url values are cut short with `...` in a few examples; the vector files have them in full.
- **Rejection vectors.** Where a vector file lists inputs to refuse, what is normative is that the input is refused, and the HTTP status and `code` where this document fixes one. An `error` or `message` text in a vector, such as "incomplete batch: <id>" in the Dispatch set's [`chain.json`](https://github.com/pubky/pubky-dispatch/blob/5f3209aae28d1b538a31c8ce43506b39df828b61/spec/vectors/chain.json), is that of the code that generated the case, which the vectors READMEs name, and only informative.

## 1. Encodings

### 1.1 JSON in Paykit's documents

Paykit's documents follow the JSON rules of the Dispatch (Dispatch wire 1.1). What Paykit adds:

- **Integers** follow the Dispatch's rule (Dispatch wire 1.1), outside the opaque members below. Paykit documents carry no other numbers. Times are never integers on the wire; period starts inside derivations use the specified integer encoding. A Private Payment List version is an ordering pair (4.13), and neither member is a time.
- **Opaque members** of Paykit's message bodies, besides the Dispatch's:
  - a message body's `metadata` (Dispatch wire 5.1, Sections 4.5 and 4.11);
  - the `proof` of a `paykit.payment_proof` body (4.10);

  Every other member is checked, whatever its name: a `metadata` elsewhere in a message, or a `proof` holding a transition proof or a Stored message. An extension kind's body (4.3) is no exception: only its `metadata` is opaque.

### 1.2 Amounts

Amounts are decimal strings: `amount.value` of requests and proofs, every amount in Allowance terms, and the values and totals of Allowance ledgers.

- **Grammar** (v0.1 `PaymentAmount.value`): `[0-9]+(\.[0-9]*)?` or `\.[0-9]+`. ASCII digits, at most one `.`, at least one digit. No sign, exponent, grouping separator or whitespace. `.5`, `10.` and leading or trailing zeros are valid.
- **Bounds:** at most 18 digits after the `.`, and the value times 10^18 below 2^128. The largest valid value is `340282366920938463463.374607431768211455`. Writers should keep the integer part to 20 digits.
- **Arithmetic** is exact: implementations compare and add amounts as integers scaled by 10^18. An addition that overflows makes an Allowance evaluation fail closed (spec 8.3). The original spelling is kept and signed; two spellings of one value are equal when compared.
- **Computed amounts** (an Allowance ledger's `totals.spent`, 5.5) are written in the normalized spelling of `encoding.json` `amounts`: no leading zeros before a non-zero integer digit, no trailing fraction zeros, no `.` without a fraction, and `0` for zero. A ledger that was never compacted carries `totals.spent` as `""`, which reads as zero; compaction writes the normalized sum, `0` when nothing was paid.
- **Asset** (`asset`): non-empty, no control characters, compared exactly and case-sensitively. By convention it is the lower-case asset segment of the Payment Endpoint Identifiers, such as `btc`, `usd` or `usdt`.
- **Scale:** an amount is in whole units of its asset (`0.00025` `btc`, not satoshis). Paykit fixes no per-asset scale. A payment method rounds up to its endpoint's smallest unit, as `specs/payment-conversion.md` says; 18 fraction digits cover every common asset.
- **Rates** (`conversion.rates`, period details): the same grammar, and the value must be above zero.
- Vectors: `encoding.json` `amounts`, with the normalized value of each valid amount.

### 1.3 Identifiers

Paykit's identifiers, beside the Dispatch's (Dispatch wire 1.7):

| Value | Form | Length | Notes |
|---|---|---|---|
| `allowance_id` | UUID v4 | 36 | the form of the Dispatch's `msg_id`: lower-case hex, `8-4-4-4-12`, version nibble 4, variant bits `10`; a MUST: readers reject upper case and other versions |
| `attempt_id` | `b64(16)` | 22 | random; the claim's attempt ID (Dispatch wire 6.2) is the payment's |

### 1.4 Payment Endpoint Identifiers and payloads

- A **Payment Endpoint Identifier** is a string as `specs/payment-endpoint-identifier.md` defines it: ASCII letters and digits, `-`, `_` and `.`, at most 64 characters, not `.`, `..` or `private`. The recommended form is three lower-case segments, `<asset>-<rail>-<format>`, such as `btc-lightning-bolt11`. Requests with conversion terms must use that form.
- A **Payment Endpoint Payload** is a non-empty UTF-8 string. Paykit never interprets it.

### 1.5 Derivations

- Paykit's derivations use the Dispatch's `hash()` and `derive()` (Dispatch wire 1.5). The table gives the obligation labels and exact field order. Settlement tags and buckets use the domain and fields specified below.

| Value | Computation |
|---|---|
| Message hash (`refs`, request hash) | `hash(message JSON bytes)` |
| Terms hash | `hash(lp(proposal message JSON bytes, acceptance message JSON bytes))` |
| Obligation of a request | `derive("paykit/v1/obligation", "request", blind(payee identity), blind(request hash), u64(period start in Unix seconds, or 0 for a one-time request))` |
| Obligation of a payment without a request | `derive("paykit/v1/obligation", "payment", blind(attempt ID))`, where the input to `blind` is the original 16 attempt-ID bytes and the naming key is the payer's (Dispatch wire 1.5). No other derivation names this claim |

**Natural keys** (Dispatch spec 13.3), each an `lp()`:

| Collection | Record | Natural key |
|---|---|---|
| `payer_activity`, `payee_activity` | request record | `lp(payee identity, request hash)` |
| `payer_activity`, `payee_activity` | period record | `lp(payee identity, request hash, u64(period start in Unix seconds))` |
| `payer_activity` | payment without a request | `lp(own identity, attempt ID)` |
| `payee_activity` | payment without a request | `lp(Payment Endpoint Identifier text, settlement ID)`, below |
| `payment_lists` | one per payee app | `lp(payee identity, publishing app ID (16 bytes))` |
| `private_list_publications` | one per publishing app, recipient and generation | `lp(publishing app ID (16 bytes), recipient identity (32 bytes), u64(publishing identity generation))` |
| `recovery_uploads` | one per original app and recovery opening | `lp(original App ID (16 bytes), opening identity document hash (32 bytes))` |
| `allowance_proposals` | one per pending Allowance proposal | `lp(counterparty identity, allowance_id text)` |
| `allowances`, `allowance_ledgers` | one per Allowance | `lp(counterparty identity, allowance_id text)` |
| `block_floor` | the one record | `lp(own identity)` |
| `settlements` | one per bucket of settlements | `lp(u64(bucket))`, the bucket a number from 0 to 1023, below |

- **Whose naming key** is the Dispatch's rule (Dispatch wire 1.5), for obligations too: the payer's, whose Dispatch holds the claim, whatever the destination. The claim's attempt, the payment method's idempotency key, the proof and the receipt carry the attempt ID itself. Only a holder of the payer's naming key can compute the claim key from it. The service sees both the obligation and the attempt ID in the claim, so blinding keeps the claim key from those who hold only the attempt ID, not from the service. The obligation names no payee, so a holder of the naming key can't test guesses of the payee against it (Dispatch spec 14, 15).
- **Period start** in a derivation is the Billing Period's `starts_at` in Unix seconds, rounded down; periods start on whole seconds whenever the recurrence's `anchor` does, and an anchor with milliseconds is rounded down the same way.
- **Settlement ID** (spec 8.4): the bytes the payment method defines to name one settlement, the same in every proof of it. The payee's payment method works it out from the proof's `proof` object and the `payment_endpoint_identifier` (4.10). Paykit defines no settlement ID and no proof format: both are the payment method's. So is whether a method names a settlement by a whole transaction or by one part of it, such as one output; a method that names them apart lets one transaction carry several payments. For a payment without a request at the same endpoint, a second proof of that settlement names the same payee record. Its valid prior receipt makes it a replay. The settlement's mark (5.7) derives from the settlement ID alone, whatever the endpoint, the counterparty or the occurrence. Among credits that write a mark, only one can apply across them all. For a hand-confirmed credit without a mark, each app checks the receipts it can verify (spec 8.1 step 4). Settlement IDs must be distinct across methods. A method whose IDs could coincide with another's prefixes them with its own name. A method gives each settlement exactly one ID, so the same settlement written another way can't name a second record. A payee whose payment method names no settlement for a proof credits no payment without a request by it. An otherwise valid proof for a request or Billing Period whose method names no settlement can be credited only by hand under spec 8.1 step 4. A proof the payee's method finds invalid is refused, including for a request or Billing Period.
- **Settlement tags and buckets** (5.7): a settlement's tag is `derive("paykit/v1/settlement", "tag", blind(lp(settlement ID)))`. Its bucket is `u16_be(tag[0..2]) >> 6`, 0 to 1023, and a mark stores bytes 2 to 13; bits 10 to 15 are unused. An occurrence's tag is the first 8 bytes of `derive("paykit/v1/settlement", "occurrence", blind(natural key))`, the natural key of the occurrence's `payee_activity` record (above), which is the same under every label's policy. Both are blinded under the naming key, so no payer can aim a settlement at a bucket or a tag, and no app that can't name a settlement or an occurrence can tell which a tag is.

Vectors: `identifiers.json` `request_hash`, `obligations` with the blinded inputs of each, and `terms_hash`. The record keys of these natural keys are `identifiers.json` `record_keys`. Settlement tags, bucket key derivation and the permanent settlement policy are included in `identifiers.json` `record_keys` and `policies`; payee scope requirements are covered by `documents.json` `payee_certificates` (3.1, 5.7). For publication counters (5.8), `identifiers.json` `record_keys` gives the named-generation policy and derived record key. The signed writes and their binding results are in `sealed.json` `private_list_publications`. The `allowance_ledgers` cases in `identifiers.json` give the permanent ledger policy and its key (5.1).

## 2. Signatures

Paykit's own format signs under a `paykit/v1/` label, with the Dispatch's algorithm and signature inputs (Dispatch wire 2.1, 2.3):

| Object | Label | Signed by | Fields after the label | Where the signature travels |
|---|---|---|---|---|
| Executor grant | `paykit/v1/executor-grant` | executor app signing key | identity, counterparty identity, `allowance_id`, terms hash, `u64(confirmed_generation)`, `u64(generation)` and `u64(seq)` of the Allowance record version its write builds on (0 and 0 for a new record) | `sig` of the grant (3.3) |
| Ledger handoff | `paykit/v1/ledger-handoff` | previous executor app signing key | identity, counterparty identity, `allowance_id`, `u64(generation)` and `u64(version)` of the grant history entry that handed the Allowance on, `u64(generation)` and `u64(version)` of the previous executor's last ledger version, and that write's item hash | `sig` of `ledger_handoff` (5.4) |
| Recovery report | `paykit/v1/recovery-upload` | original app signing key | the single length-prefixed field in 5.10 | `signature` in the report |

- Vectors: `signatures.json`, the executor grant's and ledger handoff's cases, generated and checked by `paykit-lib` on `v0.2`, as the Dispatch set's [`signatures.json`](https://github.com/pubky/pubky-dispatch/blob/5f3209aae28d1b538a31c8ce43506b39df828b61/spec/vectors/signatures.json) gives those of the Dispatch's labels.

## 3. Paykit's records

### 3.1 App certificates

A Paykit App's certificate is the Dispatch's (Dispatch wire 3.1). Paykit fixes its scopes and checks them:

- **`scopes`:** for Paykit, scope names of spec 3.1, each with the scopes it requires; extension scopes `paykit_ext:<prefix>`, at most 8; labelled selling scopes `paykit_request:<label>`, at most 8. A scope's root is its `name`, the part before any `:`. Paykit's roots are `paykit` and every root that is `paykit_` or `paykit-` followed by at least one more character (Dispatch wire 3.1, "Scope roots"), and a scope with any other root, such as `chat`, `pay` or `request`, is another protocol's (Several protocols, below). Paykit's readers ignore the other well-formed names of its roots they don't know: names that aren't a scope of spec 3.1 and start with neither `paykit_ext:` nor `paykit_request:`. That includes a sub-scope of another Paykit scope, such as `paykit_pay:x` or `paykit_contacts:x`, a plain `paykit_ext`, a plain `paykit`, and a root such as `paykit_future`: Paykit gives them no meaning, so they grant no permissions and have no keys, and the signer refuses an app request naming one. An extension scope's `<prefix>` is `paykit-` followed by 1 to 25 characters of `a-z`, `0-9` and `-`, 32 characters at most, such as `paykit-cap`: a kind's prefix is its protocol's name, or that name followed by `-` and more characters (Dispatch wire 3.1, "Scope roots"), so every extension kind is Paykit's, and none is taken for `paykit.<name>`, whose prefix is the name alone. `paykit_ext:paykit` and `paykit_ext:cap` are malformed. It authorizes receiving the extension kinds `<prefix>.<name>` (4.3); sending them also requires explicit `paykit_contacts` (spec 3.1, Outbound authorization). A label follows the syntax of a sub-scope (Dispatch wire 3.1, "Scope names"), without the `paykit-` a prefix needs, and `paykit` is a label like any other: `paykit_request:<label>` acts as `paykit_request` for that label only (Labelled selling scopes, below)
- **Paykit's verifiers.** After the Dispatch's checks, Paykit's verifiers, at apps and at payers, also check Paykit's scope rules on Paykit's scopes: no name that starts with `paykit_ext:` but isn't a valid extension scope (such as `paykit_ext:paykit` or `paykit_ext:cap`), at most 8 extension scopes, a label of the sub-scope syntax, at most 8 labelled selling scopes, and every scope's required scopes (below), while they ignore the other names they don't know; and that `permissions` holds every permission the table below derives from Paykit's scopes, and, when the certificate holds no other protocol's scope, is exactly that list, with none missing and no other. A certificate that fails these is invalid at Paykit's verifiers, though the Dispatch takes it.

**Dispatch permissions from Paykit's scopes** (spec 3.1). The table's second column is Paykit's: the signer sets `permissions` from Paykit's scopes by it, and from another protocol's scopes by that protocol's table, and Paykit's verifiers derive the list again from the scopes they know; names they don't know, and other protocols' scopes, grant nothing in it. A scope may appear in several rows. The list holds each permission that one of the scopes grants, once, in the order below.

A well-formed list with a permission missing, or, in a certificate that holds no other protocol's scope, with one extra, is refused by Paykit's verifiers, while the Dispatch takes it and enforces exactly that list: the Dispatch no longer checks the signer's derivation, so a signer that grants too much is honoured by the Dispatch and refused by Paykit's apps, and the signer is trusted with everything anyway. A change to this table needs a new certificate version (`v`), since verifiers of this version would refuse a certificate carrying the new list. Vectors: `documents.json` `certificate_rejections`, which the Dispatch takes (`switch_accepts`); those every reader refuses are the Dispatch set's `permission_rejections`.

| Dispatch permission | Granted by any of |
|---|---|
| `claims.read` | `paykit_pay`, `paykit_allowance` |
| `claims.write` | `paykit_pay` |
| `lists.read` | `paykit_contacts` |
| `lists.manage` | `paykit_manage_contacts` |
| `reply.add` | `paykit_pay`, `paykit_request`, any labelled selling scope `paykit_request:<label>`, `paykit_allowance`, any extension scope `paykit_ext:<prefix>` |

Only valid names count in this table: a name that starts with `paykit_request:` or `paykit_ext:` but isn't a valid labelled selling scope or extension scope (above), such as `paykit_ext:paykit`, derives no permission. A certificate holding one is invalid at Paykit's verifiers anyway.

`paykit_pay` requires `paykit_contacts`, `paykit_allowance_execute` requires `paykit_allowance`, `paykit_pay` and `paykit_contacts`, `paykit_manage_contacts` requires `paykit_contacts`, and `paykit_request` and every `paykit_request:<label>` require `paykit_settlements` and `paykit_contacts` (spec 3.1); a certificate that breaks this is invalid. `paykit_settlements` grants no Dispatch permission: it reads and writes `settlements` (5.7). Vectors: `documents.json` `executor_certificates`, `payer_certificates` and `payee_certificates`. The last group includes valid plain and labelled selling certificates with `paykit_settlements` and `paykit_contacts`, and certificates refused for a missing required scope.

**Several protocols** (spec 3.2, Dispatch wire 3.1 "Several protocols"). A certificate may hold other protocols' scopes beside Paykit's, or only theirs. Paykit's verifiers apply the Dispatch's rule with Paykit as the protocol they know:

- `permissions` holds every permission Paykit's scopes need, by the table above. When the certificate holds another protocol's scope, the list may hold more, that protocol's; when it holds none, it is exactly Paykit's list. A `paykit_pay:x`, a plain `paykit_ext` or a plain `paykit` is Paykit's, not another protocol's, so it doesn't let the list hold more.
- Paykit's scope rules apply in full to Paykit's scopes, and the bounds of 8 extension scopes and 8 labelled selling scopes count only them.
- Another protocol's scopes, and the permissions they bring, authorize nothing in Paykit: check 8 (4.1) still needs the Paykit scope of each kind, and a certificate that holds no Paykit scope sends and receives no Paykit kind, though Paykit's verifiers take it.

Vectors: `documents.json` `multi_protocol_certificates` and `messages.json` `multi_protocol_senders`. The accepted selling certificate holds `paykit_request`, `paykit_settlements`, `paykit_contacts` and `chat`, with `lists.read`, `lists.manage` and `reply.add`; `chat` alone with `lists.read` is also accepted. The refused certificates omit `reply.add`, omit `paykit_contacts` from a selling certificate, or use the malformed extension scope `paykit_ext:cap`.

**Outbound authorization** (spec 3.1). Extension-only and Allowance-only certificates remain valid for receiving. Extension messages, Allowance proposals, acceptances and rejections, and `resend_request` require `paykit_contacts` in the sender's certificate as well as the kind's other scopes; a recipient checks that sender column in 4.3. This adds no implicit scope and changes no permission derivation above. Another protocol's `lists.read` supplies no Paykit authority. Missing scope refuses the send with `MissingScope` before signing, recording or queueing. With the scope, an unknown deny copy or a known block still prevents the send. Queued delivery and re-sends also require that current authorization and deny check. The user's initial `allowance_end` and own-app `executor_notice` keep their precise exceptions under spec 8.3; an end reflecting a crossing response is recorded at once but its message waits for contacts authorization and a permitting deny copy; a re-send to a counterparty does not inherit the initial-stop exception. Recovery marks waiting for authorization use spec 9's ordinary bounded pool. These authorization rules require implementing code and positive and negative cases; no vector support is claimed for them here.

**Labelled selling scopes** (spec 3.1). `paykit_request:<label>` has the Dispatch permissions of `paykit_request` (above) and sends and receives the payee's request-related messages for sales under its label (4.3). Private Payment List publication and list-request intake require plain `paykit_request`. It is one use of the scope rule (Dispatch wire 3.1, "Scope names"): plain `paykit_request` covers every `paykit_request:<label>`, a certificate holding `paykit_request` holds every label, and one holding `paykit_request:<label>` holds only that label. So a certificate holds plain `paykit_request` or labelled scopes, never both: every verifier refuses such a certificate, the Dispatch included, the signer never issues one, and it refuses an app request naming both (Dispatch wire 3.9). Vectors: `documents.json` `certificates`, among them the certificate of bob's hosted app `hosted`, holding `paykit_request:shop1`, and `labelled_scope_rejections` (a label of 33 characters, one in upper case, an empty one, nine labels, `paykit_request` beside a label; only the nine labels pass the Dispatch's rule).

### 3.2 App records

A Paykit App's outer record body is defined by [Dispatch wire 3.2](https://github.com/pubky/pubky-dispatch/blob/250cd60e98eecb2a0357e55257d5b0dad2d583a4/spec/wire-format.md#32-app-record), with the host-local display-name change adopted in the spec's introduction. Display names stay in the host's local state. Its `accepts` holds Paykit's kinds (`paykit.payment_request`) or extension kinds (`paykit-cap.offer`, 4.3) under an extension scope the certificate holds, and its Paykit member is `protocols.paykit` (Dispatch wire 3.2, "Protocols' members"), an object:

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `capabilities` | object | no | `{ "endpoint_identifiers": array<string> }` | default empty: the Payment Endpoint Identifiers (1.4) the app can pay to; a reader takes what a record lists (the Paykit SDK lists none at an app without `paykit_pay`, which pays nothing) |
| `public_endpoints` | object | no | Payment Endpoint Identifier to payload (1.4), each payload a string | default empty |

The example of Dispatch wire 3.2, `shop`'s record, carries both. These rules are Paykit's, and the Dispatch checks none of them:

- **No member:** a record without a `paykit` member, or with `null` there, has no capabilities and no public endpoints. A missing or `null` `capabilities`, `endpoint_identifiers` or `public_endpoints` is empty. Unknown members in it are ignored (Dispatch wire 1.1).
- **Malformed:** a Paykit reader refuses a record whose `paykit` member isn't of this form, as it refuses any invalid record: a `capabilities` or `public_endpoints` that isn't an object, an `endpoint_identifiers` that isn't an array of strings, or a payload that isn't a string. It refuses one whose identifiers or payloads break 1.4 too: an identifier in `endpoint_identifiers`, or a key of `public_endpoints`, that isn't a Payment Endpoint Identifier, such as `private`, `..` or one with a `/`, and an empty payload.
- **Publisher’s deadline** (spec 4). The signed Dispatch app record always includes `expires_at` (Dispatch wire 3.2). An app publishing public endpoints may request a finite deadline or no deadline from Paykit. Sign the earliest finite deadline requested by any protocol on the app, or `null` when there is none. A null value is valid with nonempty `public_endpoints`. The record age limit below applies in both cases.
- Vectors: `documents.json` `app_records`, `shop`'s record with its `paykit` member changed or removed and signed again: the stored cases include accepted Paykit members and refusals for malformed members and values that break 1.4. The accepted case `public endpoints with expires_at null` checks the endpoints the reader returns from that signed record. The form of `protocols` is the Dispatch set's [`documents.json`](https://github.com/pubky/pubky-dispatch/blob/250cd60e98eecb2a0357e55257d5b0dad2d583a4/spec/vectors/documents.json) `app_record_protocols`.
- **Expiry** (spec 4): if `expires_at` is finite, payers refuse its public endpoints once either their device clock or their highest verified service time is at or after that instant (spec 7.2). The service doesn't check `expires_at`; the directory serves the record as delivered.
- **Record age limit** (spec 4): a payer uses a record's public endpoints strictly before its `updated_at` plus the `record_max_age` in the identity's `identity.json`, or a finite signed deadline if earlier (Dispatch wire 3.3; 90 days when absent), by both its device clock and its highest verified service time (spec 7.2), and not at all when its `updated_at` is more than 5 minutes ahead of the payer's device clock. So a stolen app key can't sign a record that stays payable for long: the signer, which signs `identity.json`, sets the limit. An app renews its record within that limit too (Renewal, below).
- **Path** (Dispatch spec 4.2): a payer uses a record read from `/pub/pubky-dispatch/v1/apps/<app_id>.json` only if its `app_id` is that `<app_id>`.
- **Renewal** (spec 4): once less than half the record's effective lifetime remains, the app renews it at its next run. Measure that lifetime from `updated_at` to the earlier of the identity age ceiling and any finite signed deadline. For a null deadline, use the age ceiling alone. Sign the current endpoints with a new `updated_at` and each protocol's current deadline request. Republishing or renewing a shared record must preserve a fixed public payment cutoff, including after reopen. Once that cutoff has passed, omit the affected endpoints from the next record unless the publisher explicitly offers them again. If no public endpoints remain, Paykit requests no deadline and any other protocol's deadline still applies. A renewed record cannot extend the validity of the underlying payment method. The app delivers the new record with `app.put` and, once the service accepts it, republishes it on the Homeserver; while a publication fails, or the Homeserver copy differs from the newest record the service accepted, it publishes that newest record again, never an older one and never over a Homeserver copy with a later `updated_at`, until it deletes that record to leave, or leaves (Dispatch spec 4.2). A record with at least half its lifetime left does not need a rewrite solely for renewal. At each receive, an app that no longer holds `paykit_request`, or no longer publishes its record, signs its next record without public endpoints; a renewal the service hasn't accepted yet stays the app's pending record and goes again at the next read, and isn't counted as done; an app offline at the half-way point renews when it next runs, and an unrenewed public record becomes unusable at the end of its effective lifetime.
- **Withdrawing a destination:** removing an endpoint from the current record cannot recall a stale copy already within its trust window, so a payee whose destination is compromised removes it and revokes the app, or replaces it with a new one (Dispatch spec 4.2).

### 3.3 Executor grant

An executor app's signed grant, stored in the Allowance record value (5.4). Other own apps check it without asking the executor.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `app_id` | app-id | yes | the executor app | |
| `terms_hash` | `b64(32)` | yes | hash of the length-prefixed signed proposal and acceptance, in that order (Sections 1.5 and 4.16) | |
| `confirmed_generation` | int | yes | the generation in which the executor's user last confirmed the Allowance | signed, so no other app can raise it (spec 8.3) |
| `cert` | `signed<AppCert>` | yes | the executor's certificate, holding `paykit_allowance_execute` | |
| `sig` | `b64(64)` | yes | label `paykit/v1/executor-grant` over `lp(label, identity, counterparty identity, allowance_id text, terms hash, u64(confirmed_generation), u64(generation), u64(seq))` | `generation` and `seq`: the record version the grant's write builds on, below |

A grant counts only if `cert` verifies for the own identity, names `app_id` and holds `paykit_allowance_execute`; the signature verifies with the certificate's signing key for this identity, counterparty and `allowance_id`, at the record version its grant history entry names (5.4); and `terms_hash` equals the terms hash of the record's proposal and acceptance.

**The signed version.** The last two signed fields are the Allowance record version the grant's write builds on: the `expected_version` (generation, `seq`) of the `record.put` that adds the grant to the record's grant history (5.4), which is the version the executor read, or (0, 0) for a new record. Since a record's version is the `seq` of the write that produced it (Dispatch wire 6.2), the writer can't know its own new version before the service gives it; the version it read is fixed when it signs. The grant doesn't carry the numbers; a reader takes them from the grant's history entry. A grant moved to another place in the history therefore no longer verifies, so an own app with `paykit_allowance` can't make a ledger write look like the work of another executor by moving versions. A grant outside the history, such as a `grant` that isn't the history's last entry, counts for nothing.

Example (`documents.json` `executor_grant`, whose write built on version (1, 12); certificate cut short):

```json
{
  "app_id": "cw5ahtdkixcafna9zh1y5y5o1w",
  "cert": {
    "body": "eyJ2IjoxLCJraW5kIjoicHVia3ktc3dpdGNoLmFw...",
    "sig": "Ir-nJajpFEnwb23vssI3EeOQj5e1-q6-40FrkjXZb0f6irORauaGp_fi8YPsLbC3V153vNz1PfcjyPJI_DXODA"
  },
  "confirmed_generation": 1,
  "sig": "jKZ13zfRyNtQPpLEeOU2USP0AH5ryagD_QqudMVFPn2bi61kshMto6Uuqxohf38_ueLZXBivfe9V3j_CWacmBg",
  "terms_hash": "c6fSZwYRwRa_3yscaUapKYa_TKWdhfCEvJ5kzyMbauI"
}
```

## 4. Messages

### 4.1 Paykit's kinds in the common fields

Paykit's messages use the Dispatch's common fields and recipient checks (Dispatch wire 5.1). Paykit fixes:

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `kind` | string | yes | `paykit.` followed by a name of 4.3, or an extension kind `<prefix>.<name>` (4.3); each kind fixes the order of `refs` (4.3) | recipients drop kinds they don't know |

- **Sizes:** A kind that activity records store (4.3, "Recorded") is at most 16384 bytes of JSON (spec 5).
- **Check 8** for Paykit's kinds (Dispatch wire 5.1): the table is that of spec 3.3. Only Paykit's scopes count: another protocol's scopes in either certificate send and receive no Paykit kind (3.1, Several protocols). A `resend_request` names no kinds, so any current Paykit App of the counterparty holding `paykit_contacts` may send it, whatever other kinds it handles, and any Paykit App takes it; the answer is filtered instead (4.12). An extension kind `<prefix>.<name>` needs `paykit_ext:<prefix>` in both certificates. A labelled selling scope `paykit_request:<label>` authorizes request-related payee messages under the label rules (3.1). Check 8 requires plain `paykit_request` in the sender's signing certificate for every `private_payment_list` and in the recipient certificate for every `private_payment_list_request`, including one naming that app. A `payment_request` with `label` needs `paykit_request` or `paykit_request:<label>` for that label in the sender's certificate, and one without needs `paykit_request` (4.5). A payer's acceptance, rejection, cancellation or proof with `label` needs `paykit_request` or `paykit_request:<label>` for that label in the own certificate, and one without `label` needs `paykit_request`; a cancellation without `label` may also be the payee's (4.3, 4.8). A cancellation's recipient holds the other role than its sender: one from an app holding `paykit_pay` passes only at an app the label rule above lets take it, and one without `label` from an app holding `paykit_request` or a labelled selling scope only at an app holding `paykit_pay`, so `paykit_pay` to `paykit_pay` and `paykit_request` to `paykit_request` are dropped (vectors: `messages.json` `cancellation_roles`). For payee messages about a request, check the original signing certificate against the referenced request under spec 3.4 as soon as that request can be read. A receipt with no request reference requires plain `paykit_request`.
- **Check 10** for Paykit's kinds (Dispatch wire 5.1): the body is that of the kind's section (4.5 to 4.17). The members the recipient reads must have their types (an `attempt_id` of 16 bytes, payment endpoints mapping identifiers to non-empty payloads, times that parse), and the rules the kind's section states must hold: a request carries at most one of `payment_endpoints` and `required_app_id` (4.5), period details carry endpoints or rates (4.9), a `resend_request` selects something (4.12), Allowance terms are valid (4.15), an `executor_notice` names a known reason (4.17), and a `label`, in a request or a payer's reply (4.5 to 4.10), is a valid label (3.1). A kind the recipient doesn't handle fails this check, an extension kind included. An app that handles an extension kind passes its body to the extension as it is: it carries no Paykit payment semantics, and no shared record holds it.
- **Spam rule** (Dispatch spec 7): an app doesn't create an activity record for a message from a counterparty the service has flagged (`GET /allow`, Dispatch wire 7.4.6) or the user has blocked, and keeps only a count. A message counted because the user blocked its sender is dropped as one never processed: the commit that counts it also keeps a mark of that sender, with the earliest and latest times its inbox accepted such messages, and once the read is done the app forgets the message's pair (Pubky Dispatch spec, Section 11), so a re-sent copy is a new entry. At most one mark is kept per sender; a mark whose sender the deny list holds counts toward no bound, the deny list's own limit bounding those (Dispatch wire 9.1: `max_deny`, 100,000 by default), and the others are a bounded number in all (the Paykit SDK: 128; spec 9, Messages dropped for a bound). A mark that still names pairs to forget never goes for that bound; one that only a restore that isn't clean or a `missed` item made (spec 7.2) goes first, and any other only once an ask covering its latest time was delivered, or, with none such, the one whose latest time is earliest among those whose ask was refused or can't be sent yet, the host told (spec 9, Messages dropped for a bound). No message counted raises the identity's block floor: the unblock raises it (5.6), except under both holds (5.6, Unblocking under the divergence hold), as does the commit that applies the widening of a restore that isn't clean or of a `missed` item (spec 8.1, Blocked counterparties), so a mark that goes, for that bound or its own expiry, needs no raise of its own; a message whose mark already names as many pairs to forget as it may (the Paykit SDK: 64) is held until the pass after the read forgets them, so no required pair-forgetting operation is discarded. Payload recovery still follows the bounded, best-effort mark lifecycle of spec 9. The app asks the sender to re-send what its inbox accepted from the mark's earliest time to its latest (spec 9), the block floor raised at the unblock, or, under both holds, the unblock applied without the raise (5.6, Unblocking under the divergence hold), only once every pair the mark names is forgotten, no block or unblock of the sender that the app wrote is still to be shown by the lists, and a read of the lists it makes for the ask, after the messages it has taken by then, shows the sender neither blocked nor flagged. While the flagged senders aren't known, including past a bounded copy of them (the Paykit SDK: 4,096), or when that read fails, it waits. Once an ask for a mark is delivered, its window is done, and the next message counted, or the widening of a restore that isn't clean or of a `missed` item applied, starts a new one; it asks again at most once in any 24 hours while the mark lives, until 30 days after its latest time, as for every message dropped for a bound (spec 9, Messages dropped for a bound). What comes back is taken as new. A message counted only because the service flagged its sender is not marked, and its pair is kept. A message that updates an existing record (a receipt for a payment in progress) is applied, and so is one that opens a period record of a request the app holds and the payer has accepted, which updates that request's activity (spec 8.2). What binds an app depends on what it can know:
  - **An app holding `paykit_contacts`,** and so the Dispatch permission `lists.read` (spec 3.2), keeps a copy of the lists: the deny list and flagged senders `GET /allow` answers, and the feed's `allow.*` and `deny.*` items since (Dispatch wire 7.5). A sender its copy shows blocked or flagged gets only a count. A sender the app blocked itself gets only a count at once, before its copy shows the block; one it unblocked itself is judged by its copy alone. A sender whose state it doesn't know isn't clean: while its copy isn't known to be complete, because it never read the lists, was restored from a backup, followed a provider loss or failed to refresh them, the app creates no new activity record for that sender's message. It defers the message until it has read the lists, then handles it as the lists say, and whenever it holds no complete copy it reads the lists before it handles new activity. So such an app creates no new activity record until it has read the lists, whatever the reason it can't read them. Updates to records it already holds still apply.
  - **Every app that pays or receives as payee holds `paykit_contacts`:** `paykit_pay`, `paykit_request` and every labelled selling scope require it (spec 3.1), so the rule binds payer and payee apps alike, as in v0.1. An app holding only `paykit_allowance` or extension scopes needn't hold the lists, and the rule binds it only if it does: its intake is bounded by the Dispatch's allowlist (Dispatch spec 7), it pays nothing under a proposal, which no app accepts automatically (spec 8.3), and an extension kind creates no Paykit state (spec 3.3).

### 4.2 Changes from the v0.1 bodies

Historical v0.1 references use [Paykit revision 53b8c8cb](https://github.com/pubky/paykit-rs/tree/53b8c8cb2735c0845232fbdf34bc83052bf88404/specs). Bodies use the v0.1 definitions of `specs/payment-requests.md`, `specs/payment-conversion.md`, `specs/allowances.md` and `specs/paykit-sdk.md`, with these changes (spec 5):

| v0.1 | v0.2 |
|---|---|
| top-level `version`, `kind`, `app_id` | the common fields `v`, `kind`, `from.app_id` (Dispatch wire 5.1) |
| `event_id` | `msg_id` |
| `payment_request_id` | the request hash in `refs` |
| the `request` object of `paykit.payment_request` | the request terms are the `body` itself |
| `conversion_quote_id` of a proof | the period details hash in the proof's `refs` |
| `paykit.payment_conversion_quote` | `paykit.payment_period_details` (4.9) |
| `valid_from` of a quote | the period details' `created_at`, cut to whole seconds (4.9) |
| `proposal_event_id`, `acceptance_event_id` of Allowance messages | the proposal and acceptance hashes in `refs` |
| Receipt Access, Receipt Location, Delivery Confirmations | `paykit.payment_receipt` (4.11); no confirmations |
| the 1000-byte message limit | the envelope sizes of Dispatch wire 5.1 |
| closed-world objects, unknown members invalid | unknown members ignored (Dispatch wire 1.1) |
| `required_app_id` required, nullable | optional, only without `payment_endpoints` |
| Private Payment List endpoint values are payload strings | objects with `payload` and `single_use` (4.13) |
| Private Payment List: no list version; top-level `version` is the format version (first row) | a body member `version`, the publisher's `{generation, counter}` pair (4.13, spec 8.4.1) |
| an Allowance proposal names no generation | `allower_generation`, the Allower's verified identity generation (4.15) |

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
| `resend_request` | `paykit_contacts` | any Paykit scope; the answer re-sends only kinds the requester's certificate receives | 4.12 | none | no |
| `private_payment_list` | plain `paykit_request` | `paykit_pay` | 4.13 | none | in `payment_lists` |
| `private_payment_list_request` | `paykit_pay` | plain `paykit_request` | 4.14 | none | no |
| `allowance_proposal` | `paykit_allowance` and `paykit_contacts` | `paykit_allowance` | 4.15 | none | original in `allowance_proposals` before acceptance (5.4) |
| `allowance_acceptance` | `paykit_allowance` and `paykit_contacts` | `paykit_allowance` | 4.16 | proposal hash | matched with its proposal in permanent `allowances` at acceptance (5.4, 5.9) |
| `allowance_rejection` | `paykit_allowance` and `paykit_contacts` | `paykit_allowance` | 4.16 | proposal hash | ordinarily deletes its exact pending proposal. If an earlier distinct acceptance remains unplaced, the later rejection is kept as the stop for that pending lifetime (spec 8.3.1). A distinct response conflicting with a permanent acceptance belongs in `allowances.conflicted` (5.4) |
| `allowance_end` | `paykit_allowance` | `paykit_allowance` | 4.16 | proposal hash, then the acceptance hash for an accepted Allowance | deletes the current pending proposal on withdrawal, or is stored in the accepted `allowances` record (5.4) |
| `executor_notice` | `paykit_allowance` | `paykit_allowance_execute` | 4.17 | none | no |
| an extension kind `<prefix>.<name>` | `paykit_ext:<prefix>` and `paykit_contacts` | `paykit_ext:<prefix>` | defined by the extension | the extension's | no |

**Extension kinds** (spec 3.3). An extension kind is `<prefix>.<name>`: a prefix as for an extension scope (3.1), which starts with `paykit-`, then `.` and a name of 1 to 64 characters of `a-z`, `0-9` and `_`. Under the Dispatch's rule a kind whose prefix is `paykit` followed by `-` and more characters is Paykit's (Dispatch wire 3.1, "Scope roots"), so every extension kind is Paykit's, and the `-` keeps it apart from Paykit's own kinds, whose prefix is `paykit` alone: `paykit-cap.offer` is an extension kind, while `paykit.payment_request` is one of Paykit's own and `chat.message` another protocol's, which a Paykit app drops as a kind it doesn't know. One certificate scope `paykit_ext:<prefix>` covers every kind with that prefix, with `paykit_contacts` also required in the sender's certificate, and an app's record lists the kinds it handles in `accepts`, so a new kind needs no new certificate and nothing from the signer. Senders wrap one only to apps whose certificate holds the scope and whose record accepts the kind (Dispatch spec 3.4). Extension kinds carry no Paykit payment semantics: a payment never relies on one, no Paykit collection records one, and they name no destination. A recipient app that doesn't handle the kind drops it at check 10, keeping it as processed (Dispatch wire 5.1). The common fields of Dispatch wire 5.1 apply; the body and `refs` are the extension's. The body is parsed like every message (Dispatch wire 1.1, check 5), so a number outside its `metadata` must be an integer of Dispatch wire 1.1: an extension writes a decimal, such as a price, as a string, as Paykit writes amounts (1.2). A message whose body holds `2.5` outside `metadata` is dropped at check 5, before check 10 could pass it on. Vectors: `documents.json` `extension_app`, a certificate with `paykit_ext:paykit-cap` and a record accepting `paykit-cap.offer`.

**Labelled selling scopes** (spec 3.1, 3.4). A labelled selling scope `paykit_request:<label>` sends and receives the payee's request-related messages under its label. The app record advertises only kinds its scopes allow it to receive. An app with only labelled selling scopes omits `private_payment_list_request` from `accepts`. A request sent under it carries `label` (4.5), and the payer's acceptance, rejection, cancellation and proof copy the request's label (4.6 to 4.10), so the payee's apps find the record under that label's policy (5.1). **Targets:** the payer wraps those four kinds, about a labelled request, only to the payee's apps whose certificate holds `paykit_request` or `paykit_request:<label>` for that label, and about an unlabelled request, or a payment without one, only to those holding `paykit_request`. A cancellation from the payee carries no label and goes only to the payer's apps holding `paykit_pay`, since its recipient holds the other role (spec 3.3); a payer's cancellation follows the payer's targets above, so an app holding `paykit_pay` and no selling scope gets no payer's cancellation. Recipients apply the same rule at check 8 (Dispatch wire 5.1) and drop a reply that breaks it: an app whose only selling scope is `paykit_request:shop2` drops an acceptance labelled `shop1`, and one whose selling scopes are all labelled drops an acceptance without a label. A resend answer (4.12) follows the same targets. Vectors: `messages.json` `payment_request_labelled`, `payment_request_acceptance_labelled` and `label_rejections`.

An app's local display name grants no selling authority and supplies no message label. The certificate's `paykit_request:<label>` scope authorizes the seller, and the signed request's `label` determines the reply targets above. Plain `paykit_request` retains its broad selling authority; publishing public endpoints and sending Private Payment Lists still require that plain scope (spec 4 and 8.4).

`refs` name messages by hash. A reader that needs a referenced message (the request of an acceptance, the proof of a receipt) finds it by hash in its records. `executor_notice` is sent only between apps of one identity; every other kind between identities (Dispatch spec 7).

### 4.4 Shared body types

- **Amount:** `{ "value": amount, "asset": string }` (1.2).
- **Billing Period:** `{ "starts_at": time, "ends_at": time }`. It must equal a period the request's recurrence computes, compared as instants (spec 8.2; vectors in `periods.json`). The rules the vectors follow: boundary k is `anchor + k * every` units, in fixed seconds for `minute`, `hour`, `day` and `week`, and as a calendar step from the original anchor for `month` and `year`, clamped to the target month's last day; period k is `[boundary(k), boundary(k+1))`. The first period is the one containing `starts_at`; with `ends_at`, the last is the one containing the instant just before `ends_at`, whole (it isn't cut at `ends_at`). The period at a time t is the period k with `boundary(k) <= t < boundary(k+1)`, if k lies between the first and the last; else there is none. A named period is valid only if it equals one of those periods.
- **Payment endpoints:** an object mapping Payment Endpoint Identifiers to non-empty payload strings, with at least one member. The sending app owns them (`from.app_id`).
- **Rates:** a non-empty array of `{ "asset": string, "value": amount }`, with unique `asset` selectors (`specs/payment-conversion.md`).
- **Recurrence:** `{ "every": int (at least 1), "unit": "minute" | "hour" | "day" | "week" | "month" | "year", "starts_at": time, "anchor": time, "ends_at": time | null }`, `ends_at` after `starts_at` when set. Every member is present, `ends_at` as `null` for none; a recurrence without it is invalid.
- **Payment deadline:** `{ "type": "at", "timestamp": time }` on a one-time request, or `{ "type": "period_start", "seconds": int }` on a recurring one.
- **Conversion:** `{ "type": "fixed", "rates": rates }`, or `{ "type": "per_period" }` on a recurring request only. A request with conversion terms is in an asset of lower-case letters and digits, and each fixed rate's `asset` is `<asset>` or `<asset>-<rail>` of such segments, matching the asset, and the rail when it names one, of at least one identifier the request names in `accepted_payment_endpoint_identifiers` or `payment_endpoints`, when it names any; recipients drop a request that breaks this (vectors: `messages.json` `conversion_terms`).

The `conversion_amounts` cases in `messages.json` check rate selection and rounding to the payment method’s precision (spec 7.2 and wire 4.9).

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

**The attempt's deadline** (spec 7.1; the Dispatch's deadline check is Dispatch spec 10.2). An `attempt` on a one-time request carries `deadline` = `d + 1 ms` (Dispatch wire 6.2), where `d` is the earlier of `proposal_expires_at` and the `payment_deadline`'s `timestamp`; with only one of them, `d` is that one; with neither, the attempt carries no `deadline`. An `attempt` on a Billing Period of a recurring request carries the period's `starts_at` plus the payment deadline's `seconds`, plus 1 ms, or no `deadline` without a payment deadline; a recurring request's proposal expiry bounds only its acceptance (spec 8.2). `d` is taken at millisecond precision, further digits truncated (Dispatch wire 1.6), and `deadline` is written with three fraction digits: `.100` and `.1009` give `.101`, and `.999` gives `.000` of the next second. The service refuses an `attempt` from the `deadline` on (Dispatch wire 6.8), so the last instant it takes one is `d`. The acceptance of a recurring request is bounded the same way, by `server_time_in(null, proposal_expires_at + 1 ms)` (Dispatch wire 6.3, spec 8.2), and not at all when `proposal_expires_at` is `null`. Vectors: the Dispatch set's [`operations.json`](https://github.com/pubky/pubky-dispatch/blob/5f3209aae28d1b538a31c8ce43506b39df828b61/spec/vectors/operations.json) `attempt_deadlines`, which give `d + 1 ms` for a `d` with fractions of three and four digits, and none without a `d`; how Paykit computes `d` has no vector.

**The attempt's not-before** (spec 8.2). An `attempt` on a Billing Period carries the condition `server_time_in(s, null)` (Dispatch wire 6.3), where `s` is the period's `starts_at`, so the service refuses it with `409 conflict` and its `server_time` before the period starts on its own clock. No automatic payment carries anything else. A payment by hand before the start, after the user confirms paying that period early, carries as `s` the latest service time the app has verified when it builds the attempt, after that confirmation (spec 8.2, Not before its start). An attempt on a one-time request carries no not-before. An attempt on a Billing Period the request record doesn't list in `attempted_periods` also writes the request record, adding it (5.3).

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

Body: `{}`, or `{ "label": string }` for a labelled request, its `label` (4.5). `refs`: the request hash. v0.1 defined no other field. The example is in Dispatch wire 5.1.

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

Body as the rejection (`reason`, optional; a payer's cancellation of a labelled request carries its `label`, a payee's none). `refs`: the request hash. Either side may send it. The payer checks a payee cancellation against its authenticated parent under spec 3.4, including a cancellation kept before the request arrived. A payer records its cancellation of a recurring request in a batch conditioned on the claim of every Billing Period its request record lists in `attempted_periods`, and sends it after that batch succeeds (5.3, spec 8.2); a listed period with an open or `unknown` attempt refuses it.

### 4.9 `payment_period_details`

The payee's details for one Billing Period of a recurring request (spec 8.2).

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `billing_period` | Billing Period | yes | a period of the request | |
| `payment_endpoints` | Payment endpoints | no | | the period's destination |
| `rates` | Rates | no | only under `per_period` conversion terms; cross-asset only: no rate for the requested asset | as v0.1 quotes |
| `expires_at` | time | yes | | the payer uses only details whose `expires_at` is after both its device clock and its highest verified service time (spec 7.2); their rates apply to a payment verified from their start through it (below) |

- At least one of `payment_endpoints` and `rates` is present; recipients drop details with neither.
- `rates` only against a request whose conversion terms are `per_period` (4.4): details with `rates` are invalid against a request with fixed rates or without conversion terms, as v0.1's quote identifier is (`specs/payment-conversion.md`, "Payment Conversion Quote") (vectors: `messages.json` `conversion_terms`).
- The sender's original certificate must authorize the referenced request under spec 3.4.
- Details invalid against their request, for any reason of this section, the payer takes with no effect: it stores them in no period record and pays by none; a reader of a period record ignores stored details invalid against the record's request (5.3).
- Cross-asset only means no rate whose `asset`, or its asset segment, is the requested asset: a `usd` request's details with a `usd` or a `usd-polygon` rate are invalid against it. Each rate's `asset` is `<asset>` or `<asset>-<rail>` of lower-case letters and digits, as in conversion terms (4.4), or recipients drop the details; and it matches the asset, and the rail when it names one, of an endpoint the details' `payment_endpoints` or the request names, when they name any, or the details are invalid against the request (vectors: `messages.json` `conversion_terms`).
- Each details stays valid until its own `expires_at`: a later message for the same period replaces none (spec 8.2). Later means a later `created_at` instant, then, at the same instant, the greater `msg_id` compared as text; the latest that carries what the payment needs is the payer's default (spec 8.2). Details as late as those held, the same `created_at` and `msg_id`, are the same message only if their hash is; other details as late are an equivocation: the reader keeps the details it holds and stores no other. A period has at most 8 details: the payee sends no more, a reader that holds 8 stores no other (5.3), and the payer pays by none its period record doesn't hold.
- `refs`: the request hash.
- **Rates and payment time** (spec 8.2). The rates apply to a payment whose independently verified payment time, from the payment method's evidence, is at or after their start and at or before `expires_at`, all read at millisecond precision, further digits truncated (Dispatch wire 1.6). Their start is the details' `created_at` cut to whole seconds, v0.1's `valid_from`: `2026-02-28T00:00:00.999Z` starts at `2026-02-28T00:00:00.000Z`. The payee sets `created_at` to the time it signs the details and never backdates them, as in v0.1. The payment method defines what counts as verified payment time and documents its rule, as in v0.1 (`specs/payment-conversion.md`); one method may take a block's timestamp, another the time its node settled the payment. An evidence time in whole seconds is read as `.000`. The payee credits a payment at the rates only then; one verified outside that interval doesn't pay the period at those rates, and gets no receipt. The payer pays by the rates only while its device clock and its highest verified service time are both at or after their start and before `expires_at`, at planning and again after the attempt's answer, before money moves. Rates price a payment to an endpoint of another asset than the requested one, by the asset-rail rate, else the asset-wide one, as `specs/payment-conversion.md` says; a payment to an endpoint of the requested asset is paid 1:1 and isn't bound by them. Vectors: `periods.json` `details_rates`.

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
| `list_version` | list-version | when paid to a Private Payment List | the exact pair of that list (4.13), bound to its publishing app and this payer | |
| `amount` | Amount | for a payment without a request | | a request's amount comes from the request |
| `allowance_id` | uuid | for a payment under an Allowance | | omitted for every other payment, whether or not the user confirmed it (spec 8.2) |
| `label` | string | for a labelled request | the request's `label` (4.5) | absent for an unlabelled request and a payment without one |
| `payment_reference` | string | no | equals the request's | v0.1 required it; the request hash in `refs` now identifies the request |
| `proof` | object | yes | opaque, method-specific: an object whose members the payment method defines (`specs/payment-requests.md`); by convention `{ "type": string, ... }`. The proof types in this document's examples and in the vectors illustrate no defined method | |

`refs`: the request hash, then the hash of the period details the payment used, if any. A proof of a payment without a request has no `refs`; the payee records it by its settlement (1.5), so the same evidence sent again under a new `msg_id` is a replay. A proof with `refs` carries no `amount`; one without carries no `billing_period`, `label`, `payment_reference` or `allowance_id`, which name a request's context (an automatic payment under an Allowance pays a request, spec 8.3); and one that names the period details it used carries their `billing_period`. Recipients drop a proof that breaks this (vectors: `messages.json` `proof_members`).

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
      "type": "example"
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
  "list_version": { "generation": 1, "counter": 12 },
  "payment_app_id": "brrz3m6g5rci6iqnzke44q4djc",
  "payment_endpoint_identifier": "btc-bitcoin-p2wpkh",
  "proof": {
    "data": "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
    "type": "example"
  }
}
```

### 4.11 `payment_receipt`

The `receipt_members` cases in `messages.json` cover the receipt members for requested and unrequested payments.

| Field | Type | Req | Notes |
|---|---|---|---|
| `attempt_id` | `b64(16)` | yes | the attempt of the proof it confirms |
| `billing_period` | Billing Period | for a recurring request | the proof's |
| `metadata` | object | no | the payee's, opaque (1.1), as v0.1 `Receipt.metadata` |
| `payment_reference` | string | no | a payment without a request: the payee's reference for it, 1 to 256 characters, no control characters (4.5); recipients drop a receipt that carries it and a request hash, whose request's reference applies |

`refs`: the proof hash, then the request hash when there is a request. The payer's apps find the obligation and the record from the request hash, and may hold the receipt without the proof (Dispatch spec 10.2 row 3). Apply the original sender's selling scope under spec 3.4 before treating the receipt as credit or settling the attempt. A receipt with the proof hash alone names no Billing Period. A record that holds a receipt and its proof is invalid when their `attempt_id` or `billing_period` differ, and the record of a payment without a request when its receipt's `attempt_id` isn't the record's (5.3; vectors: `sealed.json` `activity_records`).

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
    "5i4nqp8KShw1uLnlx5y6UTsNNw8rNS9dt7n5OBbZieQ",
    "pukrdI5C7SfN7mlLheLGWRk2qzt8RTKXlXv4E1rRGKg"
  ],
  "body": {
    "attempt_id": "DgCVYQKL1eyDacZvG6YhZg"
  }
}
```

### 4.12 `resend_request`

Asks the counterparty to re-send messages it sent (spec 5, Dispatch spec 8.2, 12.6). Best effort.

| Field | Type | Req | Notes |
|---|---|---|---|
| `msg_ids` | `array<uuid>` | no | default `[]`; messages by ID |
| `accepted_from` | time | no | messages the recipient's inbox accepted at or after this time |
| `accepted_until` | time | no | ... and at or before this time; absent means no upper bound |
| `request_hashes` | `array<b64(32)>` | no | the requests named, and every message whose `refs` name one of them |
| `billing_period` | Billing Period | no | with `request_hashes`: only that period's messages |

- At least one of `msg_ids`, `accepted_from` and `request_hashes` is present, and the times parse; recipients drop any other (Dispatch wire 5.1 check 10).
- Times select by the acceptance times the sender kept (spec 9), at millisecond precision: the first one of each message or any later re-send's. To ask for an entry it can't open, an app sends `accepted_from` and `accepted_until` both equal to the entry's `accepted_at`, which returns every message that sender's inbox accepted in that millisecond; duplicates are dropped by `msg_id`.
- The sender re-seals each selected message it first accepted in the last 30 days to the current receive keys, with its original signed plaintext, and only messages of kinds that the requesting app's certificate allows it to receive (spec 9): a wallet with `paykit_pay` and `paykit_contacts` but no `paykit_allowance` gets back no Allowance messages. Of a payer's acceptances, rejections, cancellations and proofs, and a payee's cancellations, it re-sends only those check 8 lets the requesting app take under their label and role (Dispatch wire 5.1, Section 4.3): an app whose only selling scope is `paykit_request:shop2` gets back none labelled `shop1` and none without a label, and one holding `paykit_request` and not `paykit_pay` no payee's cancellation. A sender that keeps messages longer doesn't re-send older ones, since recipients keep processed IDs for 30 days. Retention runs from the first acceptance: the sender keeps one entry per `msg_id` with its first acceptance time and the later ones, and a re-send doesn't restart it.
- **Limits** (spec 9): the sender re-sends one message to the same requester identity at most once an hour (`RESEND_COOLDOWN_MS`), and at most a daily number of messages per requester identity, which it chooses; it leaves the rest out. Shifting a range by a millisecond selects the same messages, so it gets nothing new.
- **What an app asks for** (spec 9): the unopenable, unreadable and `missed` items that lead an app to ask come from the service, and the app can't check them. For an entry it can't open, an unreadable item and a `missed` item, it never asks for a time before its cursor's time less the clock tolerance (5 minutes): for the entry and the unreadable item, only if its `accepted_at` is no earlier than that, and for the `missed` item, from the later of its `accepted_from` and that time (Dispatch spec 8.2). For an unreadable item it also asks only after it has verified a departure of another own app in its chain (a `leave` or `revoke` transition, Dispatch wire 6.5) at or after the entry's `accepted_at`. It ignores an item whose times don't parse.
- A `resend_request` adds no reply grant (Dispatch spec 7): a forged entry from a stranger must not admit that stranger.
- `billing_period` is read only with `request_hashes`; without them it selects nothing, and senders don't send it alone. An empty array counts as absent.
- `request_hashes` selects each named request and every message whose `refs` name one of them. With `billing_period`, only the messages whose body names that Billing Period are selected; the request itself, which names none, is left out. Selectors combine as a union, and each message is re-sent once, whatever its selectors (duplicates are dropped by `msg_id`).
- Any own Paykit App holding `paykit_contacts` may ask for entries it can't open, whatever its other Paykit scopes; one without it keeps the recovery mark under spec 9's ordinary bounds and reports `MissingScope` without signing or queueing an ask.

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

The latest list of one publishing app for one recipient identity (spec 8.4). The publisher's signing certificate must hold plain `paykit_request`. The payer drops a list that fails this check before comparing versions or installing or withdrawing endpoints (4.1). An empty `payment_endpoints` object identifies a withdrawal. Every delivery of that signed message omits the publisher's reply grant write (spec 8.4.1). Nonempty lists follow the ordinary first-message rule.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `version` | list-version | yes | the pair reserved for this publishing app and recipient (spec 8.4.1); strictly greater than every earlier reservation of that tuple | compare the pair as specified below |
| `payment_endpoints` | object | yes | Payment Endpoint Identifier to endpoint | |
| `expires_at` | time | yes | chosen by the publishing app from actual current time, not from either version member; 30 days after publishing is recommended | a list without it, or with one that doesn't parse, is dropped (Dispatch wire 5.1 check 10). Payers use a list only while the earlier of `expires_at` and 30 days after its inbox accepted it (`accepted_at`) is after both their device clock and their highest verified service time (spec 7.2); once either clock is at or past it, it has expired, and they ask for a newer one (4.14) |

An endpoint:

| Field | Type | Req | Notes |
|---|---|---|---|
| `payload` | string | yes | non-empty |
| `single_use` | bool | no | default `false`, omitted when `false`; paying it consumes this list version |

A `list-version` is a JSON object with required `generation` and `counter`, each a Dispatch integer from 1 through 9007199254740991. The two members may be written in either order (Dispatch wire 1.1). Compare generation first, then counter, numerically; do not pack the pair into a number or compare JSON text. Generation is the publishing identity's verified `identity.json.generation`, not its document version, data epoch, the recipient inbox generation or a local recovery number. Changing either member changes the version.

A received list is installed by its pair (below). A payer pays from a list only when the copy of the payee's `identity.json` it reads before paying (spec 8.4), verified and fresh, has a `generation` at least the list's: an app-signed number is no evidence of the identity's generation. A list of a higher generation than that copy plays no part in that payment; it may in a later one, once the verified generation has reached it. So an app that signs a list for a generation its identity has not reached makes every lower pair of its own ignored at that payer, also once the identity reaches that generation: catching up only makes the list it signed payable, and only a strictly greater pair replaces it, while an equal pair with another hash is an equivocation. For example, an app of an identity at generation 7 signs a nonempty `(8, 9)`; the app's first genuine list of generation 8, a withdrawal `(8, 1)`, is ignored, and once generation 8 is verified `(8, 9)`, while unexpired, becomes payable. The app recovers if it can validly reserve a greater pair and sends a list or withdrawal with it; at the maximum pair only another App ID can publish to that payer again. The payee's way out is to remove the app (a request whose `required_app_id` names that app then can't be paid, spec 8.1). Only that app's key can sign such a list, and only that app's lists are ordered against it: the lists of the payee's other apps, and of other payees, are ordered on their own. A legitimate list may have a lower generation than the current document: its ordinary certificate-at-acceptance and expiry checks still apply.

For the same publisher identity, publisher App ID and recipient identity, a greater pair replaces the held list, a lower pair does not, and the same pair with the same message hash is a duplicate. The same pair with another message hash is an equivocation: reject it, report the authenticated writer, and retain the held message. An empty greater list leaves no payable endpoint. Keep its highest pair and message hash even when empty or expired, and keep every `used` entry when replacing it (5.4). Expiry permits a request for a newer list (spec 8.4); it does not erase ordering or consumption evidence. Existing record-loss/deletion holds still apply when that evidence is unavailable. The recipient orders the lists using the signed version and does not read the publisher's private counter.

`created_at`, `expires_at`, accepted-at age and replay deadlines remain actual-time checks. A new UUID, changed expiry, renewal, changed endpoints or newly constructed withdrawal is a new publication and consumes another reservation; an unchanged retry keeps its pair and exact signed bytes. Pair fields impose no future-time exception. Sections 8.4 and 8.4.1 of the specification govern payment, including loss and deletion holds.

**Renewal** (Dispatch spec 4.2): the same rule as for records (3.2). Once less than half a list's lifetime (publishing to `expires_at`) remains, the publishing app sends the counterparty a newer list at its next run, with a higher `version` and a new `expires_at`; like any newer list it carries fresh single-use payloads from the permitted methods for the chosen recipient (spec 8.4). A list with more than half its lifetime left is not sent again.

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
  "version": { "generation": 1, "counter": 12 }
}
```

### 4.14 `private_payment_list_request`

| Field | Type | Req | Notes |
|---|---|---|---|
| `held_version` | `list-version \| null` | yes, always written | the complete pair the sender holds, `null` when none |
| `app_id` | app-id | no | the publishing app asked about |

```json
{
  "app_id": "brrz3m6g5rci6iqnzke44q4djc",
  "held_version": { "generation": 1, "counter": 12 }
}
```

The payer sends this request only to eligible apps holding plain `paykit_request`. When `app_id` is present, only the named app may answer it. A targeted or untargeted request gives a labelled selling app no publishing authority.

A `held_version` is a hint from the requester, never authority to advance the publisher's counter or choose a generation. An unexpectedly greater hint is held and reconciled with the actual signed offered message; it does not reset or fast-forward the publication record.

### 4.15 `allowance_proposal`

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `allowance_id` | uuid | yes | fresh | |
| `proposer_role` | string | yes | `allower` or `allowee` | the sender's role |
| `allower_generation` | int | yes | 1 through 9007199254740991 | the generation read from the Allower's verified identity document |
| `terms` | object | yes | the Allowance Terms defined below, every member present | |

**Allower generation.** The proposer obtains `allower_generation` through the existing identity discovery, using its verified own document when it is the Allower. Signed sender, recipient and `proposer_role` determine that identity. The Allower compares it with the current generation and provider-loss boundary before its acceptance batch (spec 8.3.1). A changed generation member changes the proposal hash and is another proposal for the same-ID conflict rules. No acceptance expiry or additional message is implied.

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

**Checking the terms:** apps check the whole table, amounts by 1.2 and times by Dispatch wire 1.6, when a proposal arrives, dropping one whose terms fail; when they verify an Allowance record, which is invalid with such terms (Dispatch spec 8.2); and before each automatic payment. A time that doesn't parse makes the terms invalid: it is never read as no limit. Senders don't propose invalid terms.

**Meaning of the terms** (v0.1 `specs/allowances.md`, restated here):

- `per_payment_amount` is an inclusive range: a payment is allowed when `minimum <= value <= maximum`.
- Limits are inclusive ceilings: with a candidate payment, a period's counted amount plus its value must be at most `amount_limit`, and its counted payments plus one at most `payment_count_limit`; the lifetime amount plus its value at most `lifetime_amount_limit`.
- A **monetary ceiling** is `per_payment_amount`, an `amount_limit` or `lifetime_amount_limit`; terms with none of them and `expires_at: null` are invalid.
- `active_from` is the first eligible instant and `expires_at` the first ineligible one, on the service's time of the executor batch.
- `allowed_payment_endpoint_identifiers`, when set, lists the only identifiers an executor may pay to.
- A request in another asset than `asset` is never paid under the Allowance (5.5).

**Unknown members in `terms`.** v0.1 made terms closed; v0.2 readers ignore unknown members (Dispatch wire 1.1). An executor that ignores a restriction it doesn't know pays more than the proposer meant, so a later version that adds a restriction must also change `v` or the kind.

```json
{
  "allowance_id": "d0efffb7-191b-4709-b6c7-e343af5500b4",
  "proposer_role": "allowee",
  "allower_generation": 7,
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
| `executor_app_id` | app-id | no | in an acceptance from the allower: the executor app at acceptance. Informational; in the first release only the approving Allower app can take the first grant (spec 8.3) |

- Acceptance and rejection: `refs` is the proposal hash. Only the proposal's recipient answers.
- End: `refs` is the proposal hash and the acceptance hash. A proposal withdrawal by its sender has only the proposal hash.
- The terms hash is `hash(lp(proposal JSON bytes, acceptance JSON bytes))` (Dispatch wire 1.5), including the proposal's required generation in its exact signed JSON.
- The Allower records the original signed acceptance and creates the ledger bound to its hash in one batch (5.9).
- An end of an accepted Allowance names exactly its proposal hash and acceptance hash. A pre-acceptance withdrawal names only the proposal hash and comes from its sender. Its exact pending write is deleted before sending. On the side already holding the matched permanent proposal and acceptance, a crossing withdrawal is reflected by its `msg_id` and prompts an end naming both, subject to the conflict and delivery rules in spec 8.3. The withdrawn proposal's sender checks any late acceptance against its retained signed proposal and the current pending and permanent keys (spec 8.3.1). A matching permanent acceptance is a repeat. When the reads establish withdrawal, the obligations and implementation status of spec 8.3.1 apply. Indeterminate reads leave the duty held. A crossing rejection and withdrawal leave the pending lifetime closed and create no permanent record. Only an acceptance from the proposal's recipient acting as Allower can carry `executor_app_id`. Recipients drop a rejection or end carrying it. Section 5.4 defines the stored record checks (`sealed.json` `allowance_records`).

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
    "9HiIEoWyxH5nFqUHR-xtwv2XgbbdUOIiWyQF-BpcwQU"
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
  "allowance_record": "kNgxYqkcwbzOglKDpn58fw-UwvdX7mAeDCJKhK1bBUA",
  "reason": "ended"
}
```

## 5. Shared record values

Own apps of one identity share state through sealed shared records (spec 6, Dispatch spec 10.3). Apps of different vendors on one identity read each other's records, so the plaintext of each Paykit collection is part of the wire format. A value is JSON (Dispatch wire 1.1), sealed as in Dispatch wire 6.2 under the collection's read scope. Readers verify every stored message on use (Dispatch spec 8.2) and work out status from the verified messages and the claim, never from a field alone, but for a member that only withholds, `manual_only` (5.3) and `lost_to_delete` (below), which grant nothing: an own app with the write scope can withhold as much by writing a stop. A writer that writes a record again keeps every member it doesn't know as it read it, in the value and in each object of it that it keeps, so a member a later version adds survives an older writer.

**Observing a deleted payee activity record** (spec 6). An own app that verifies another own app's deletion of a `payee_activity` record identifies it by the record key, generation, version and signed item hash. It commits the applicable local evidence and restrictions together. It can attribute the deletion to its signed writer. It can attribute the loss of a particular receipt to that writer only if the authenticated write on which the delete was based contained that receipt. An older copy may prove the receipt was issued. It cannot prove what the deleted version contained. A read of the current record alone supplies no feed-page time.

To show the original receipt, the app verifies the payee's receipt and its reference to the payer's signed proof, and binds both to the activity key and the occurrence, including the signed request where applicable. For a Billing Period the start and end must agree with the period of that request. The receipt counts as evidence that the payee identity has credited the settlement identified by its payment method. Another proof of that settlement cannot credit a different occurrence, whatever payer presents it (spec 8.1 step 4). An app with no bound copy retains the occurrence's credit hold and shows no receipt. An authenticated exact predecessor containing neither a receipt nor a deletion restriction permits ordinary cleanup when the app holds no other evidence requiring a hold. A later write at the key, acknowledgment of the warning or user confirmation alone cannot clear a known credit restriction.

Restoring in place preserves any independently verified issued receipt and applicable restriction that the installation still holds for the same identity and app, including when the selected backup predates the receipt and the service now shows a successor. An imported assertion about a writer, an exact predecessor or a feed-page time requires independent verification before it can be used as such. An app that retains the acknowledgment does not repeat the warning when it sees that deletion again. On an empty installation, the absence of receipt bytes from the backup and service proves nothing about prior credit. The credit rules of spec 6 and 8.1 still apply. Local observation neither writes a shared replacement nor satisfies the service-cursor requirement for a deletion that must be put back.

**Publishing an issued one-time payee receipt** (spec 6). In a single uninterrupted service generation, a writer of `payee_activity` may protect a receipt already signed for a one-time request. It verifies the signed delete, its application and policy, and the complete readable predecessor. The delete’s applied generation and sequence must equal its authenticated record version, and its signature must name that same current service generation. A later recovery upload of the same signed delete falls under the provider-loss rules. It cannot authorize this publication or lift a credit restriction. A verified transition proof or chain stub must bind the predecessor's item hash to the version the delete signed as its `expected_version`. It binds that activity to the record key and label and verifies the original signed request, the payer's proof of that request, and the payee's receipt referring to that proof and request. A seen value or version alone supplies insufficient evidence. It publishes the complete predecessor under its existing policy with a `record.put` naming the delete’s version and a `record_current` condition on the same key, exact delete application and `deleted: true` (Dispatch wire 6.2–6.3).

The service cursor stays below the delete until a qualifying protective write applies. At the same key and policy, the writer forms the union of the complete predecessor and the authenticated, readable successor. It preserves their required signed roles, payment occurrence, restrictions and every opaque member of each retained object, including nested objects. An opaque member present on only one side is preserved, equal values coalesce, and different values at the same member make the union unrepresentable. A successor containing the whole union satisfies the duty without another write. Otherwise the writer publishes the representable union with an exact condition on the live successor. Conflicting signed roles, an unrepresentable union or uncertainty about a restriction keep the duty outstanding. Quota refusal or a changing successor leaves the service cursor held during retry. Each later deletion has its own duty.

One `lost_to_delete` may be replaced in that union. The eager write must carry the hash of this same deletion and sign its exact deleted application and version as its base. The repair must sign the eager write’s exact live application and version as its base. A writer or reader may then use the marker from the complete predecessor, including its absence, upon independently verifying those applications in the original generation, the complete predecessor as defined above, and the full union with every independent restriction and retained member. A reader that verifies this exception takes the repair under the other ordinary read checks. Without that evidence it keeps the restriction it saw and applies the ordinary rule for a dropped marker. Independently verified signed receipts remain evidence of prior credit in either case.

A lost reply keeps the original signed submission subject to the Dispatch’s original-result rules. A separate complete publication can cover the deletion without proving the outcome of that submission. A notice acknowledgment cannot discharge the publication duty. Before another device uses the same App ID, its host transfers exclusive use of the current durable cursor state, outstanding submissions, duties and owner evidence. A host using separate stores must synchronize this state before handing over. With a fresh destination, missing receipt bytes cannot be reconstructed from a later stub. Provider loss remains subject to the recovery rules in spec 6 and 13. Publication reuses the issued receipt and neither signs another receipt nor grants a new credit.

**Incomplete protected-record contract.** Apart from Publishing an issued one-time payee receipt above, the positive paths in Put back over a delete below, including genuine first creation, exact-copy and older-copy put-back, and later authorization from an incarnation's provenance, are not defined interoperable operations in this revision. Dispatch revision `f0bbddd1` specified the generic origin, lineage and recovery evidence these paths require (its spec 10.3 and wire 6.9). The current Dispatch distinguishes absence from a retained delete at application (Dispatch wire 6.2 and 6.3), but supplies no such lineage proof. The Paykit mapping remains incomplete. The corresponding generic implementation and Paykit verification paths remain required. Ordinary `expected_version` `{0, 0}`, a preflight read, a signature or current version, an atomic `record.put` plus `cursor.set`, and user confirmation do not supply the missing guards. Even a proved copy of a delete's exact predecessor cannot make its put-back safe against an intervening different delete without the apply-time guard.

Until an adopted Dispatch revision and its Paykit mapping and implementation provide that evidence, an implementation MUST withhold these positive paths and any payment, credit, acceptance, grant or endpoint use that depends on them. It retains every known restriction and delete hold, and does not acknowledge the service cursor past a delete whose required protective publication cannot safely apply. The availability cost includes inability to create usable state at an apparently absent potentially protected key or complete a protective put-back. These requirements remain unresolved release blockers, not optional features. Completing them requires mapping and implementing the bounded proof and validation contract across later writes and readers without seen state, retention and forgetting, export/import, provider recovery, backup/restore and shared-cursor devices, with positive and rejection vectors and the corresponding Paykit integration. No lifetime history API or guessed Dispatch member is specified. The family-specific least-permissive rules and rejection requirements below remain mandatory; their positive transitions describe requirements for that complete integration, not permission to execute them through today's zero-version write. Defined unaffected formats remain applicable. The all-witnesses-removed limitation in spec 6 applies only to families without permanent origin slots; it is not a substitute for the missing mechanism.

**Put back over a delete** (spec 6, Putting back a deleted record). Subject to the incomplete-contract hold above, a record of a family spec 6 lists there, deleted by another app, is written back by every own app that holds it and whose certificate holds one of its `write_scopes`, as a `record.put` with the delete's version as `expected_version`, the record's policy and the operation members required by the adopted Dispatch wire 6.2. A write over an uploaded delete also requires `record_current` naming its application (Dispatch wire 6.2 and 6.3). The protective write must apply before or atomically with the `cursor.set` that acknowledges the delete to the service (Dispatch wire 6.4). The service-held cursor, used for forgetting deletions (Dispatch wire 9.1), must stay below the delete's sequence until the protective shared write has applied; a local receive position is separate. The app durably keeps the publication obligation before moving that local position. A queued write or acknowledged local duty is not publication. Quota, value-limit or other refusal holds the service cursor and may block receive acknowledgment indefinitely until the write can apply; the host is told, and the family's least permissive state remains. Local reads may continue only without acknowledging past the hold. Retry, reload, backup, restore and every device sharing that service cursor preserve the obligation. A concurrent write ends the hold only once verified to contain every restriction this app must preserve (spec 6).

- **A visible delete without a copy:** an authenticated current delete is distinct from a key absent at the service. An app with no prior value or seen state treats payer and payee activity and `payment_lists` keys as potentially protected (spec 6, Visible deletion without a copy); the service refuses deletion of permanent `block_floor`, `settlements`, Allowance and ledger records (5.1). It retains the delete's item hash and local hold durably, writes no replacement from a replay at `{0, 0}`, and authorizes no payment, credit, acceptance, grant or endpoint use from that missing state. A deleted recurring request holds its periods too. Only a protective put-back judged under these rules may replace the hold; a later unmarked create alone proves nothing about deleted content. If the app cannot prove an exact predecessor or verify the required least-permissive replacement, it keeps waiting, also across restart, backup and restore and while another app's repair is blocked by quota. Genuine absence after legitimate forgetting is the separate limit of spec 6; a visible delete never counts as that absence.
- **Creation and incarnation provenance:** a create of a potentially protected key requires authenticated absence when the create applies. A zero-version write requires the key to be absent then. A later reader must also authenticate that creation and its continuity to the current write. Creation over a retained delete follows the protective put-back rules instead and requires authenticated matching of that exact current delete item when the put-back applies. A put-back for one delete overtaken by another is refused and recomputed from the new state. Every later writer and reader, including one without seen state, preserves and verifies authenticated exact-predecessor continuity from that creation or proved put-back, with every required restriction retained. A recreation or later unmarked rewrite is not a new authority merely because its signature and current version verify. Missing or incomplete evidence holds payment, credit, acceptance, grants and endpoint use (spec 6, Creation and later writes). Retry, reload, backup, restore and provider recovery preserve or reauthenticate the retained restrictions and provenance, never reset them. This is part of the incomplete Paykit mapping above. These paths remain withheld until an adopted Dispatch revision and Paykit readers provide the required evidence and generated cases.
- **The copy deleted:** when the write the app holds is the one the delete was built on, the write at the version the delete's operation signed as its `expected_version`, matched by item hash (Dispatch wire 6.6) with the write the feed or the service's chain shows there, not by version alone, since after a provider loss two values can be uploaded at one version (spec 6, Divergent records), and an app that can't match it so, such as over a record taken from an upload, which left no chain item to match (Dispatch wire 7.4.14), writes back as from an older copy; the app writes that value as it holds it, members it doesn't know included, and names the delete's writer (Dispatch spec 8.2). Permanent `block_floor`, `settlements`, Allowance and ledger records cannot be deleted (5.1, 5.6, 5.7, 5.9).
- **An older copy:** every older copy of a `payee_activity` or `payment_lists` record belongs to its protected family (spec 6), regardless of whether the copy holds a receipt or `used` entry; content added in the unseen deleted write cannot be ruled out. The app writes the value it holds, with every message, `used` entry, mark, listed period and grant it has seen, and `lost_to_delete` set to the item hash (Dispatch wire 6.6) of the delete it answers, which a write-back held back for room keeps from its read, whatever the key holds of that delete later, and, by collection: in a payer's activity record, `manual_only: true` where the record takes it (5.3); in a payer's request record that lists a period, every Billing Period of the request whose claim the app reads with an open or `unknown` attempt added to `attempted_periods`, within its 16, dropping only listed periods whose claims it read with neither, each with its `claim_step` condition (5.3, Attempted periods). It names no app.
- **`lost_to_delete`:** the item hash (Dispatch wire 6.6) of the delete a write-back from an older copy answered, `b64(32)`, or absent, in an activity record or a `payment_lists` record. This scalar form is invalid in every other collection except `block_floor` and `settlements`, whose permanent records carry no `lost_to_delete` and treat it as a member they don't define (5.6, 5.7), and any other value is invalid in these listed collections. Settlement credit checks never release a payer or payee activity hold. It names which delete lost content by the bytes its writer signed: a version is the service's word, and after a provider loss two uploads can share one. A reader takes a record that carries it at its least permissive state (spec 6): a `payment_lists` record as one whose list is paid automatically never, and by hand only once its user has seen that the list may be stale, until its user confirms the record (Confirming a loss, below), every single-use endpoint of a list version at or below that of its list counting as used, confirmed or not; a payer's activity record as one whose occurrence, every Billing Period of the request for a recurring request's request record, is paid never, automatically or by hand; a payee's activity record as one whose occurrence takes no proof credited, automatically or by hand. For a payer's activity record, no missing, present or recreated claim, message date, retention interval, provider or account change, or user confirmation releases that hold (spec 6, Payment history lost), and an intended new payment requires a distinct new request or obligation, not a replay of the old one or a cleared or changed loss marker. Except for the proved repair in Publishing an issued one-time payee receipt, a reader that has seen it ignores, and shows with its writer, a later write that drops it (Dispatch spec 8.2); a write that changes it is taken, since another value only ends confirmations (Confirming a loss).
- **Deletion data that doesn't read:** in every collection whose records carry `lost_to_delete`, only an absent member means no restriction. A member that is present but isn't a valid value for its collection, such as `null`, a value of another type, a malformed or non-canonical hash or an unknown `kind`, never clears a restriction: the record is invalid, and a reader takes it at the least permissive state that collection gives a record carrying `lost_to_delete`, as above. Deletion data that doesn't read never relaxes any of those restrictions.
- **Confirming a loss:** a user's confirmation of a record carrying `lost_to_delete`, such as a `payment_lists` record, is of that put-back: the app keeps in its own state the record's collection and key, the `lost_to_delete` value, and both the item hash (Dispatch wire 6.6) and version of the write its user was shown. Repeated reads of that same authenticated write retain the confirmation. It advances to a successor only when the successor is a write, not a delete, carries the same `lost_to_delete`, names the kept version as its `expected_version`, and authenticated feed or chain evidence establishes the kept item hash as the exact preceding write of that record. Equality of versions is insufficient: after provider loss a different uploaded write may have the same version. Once that predecessor is proved, the app keeps the successor's item hash and version as the new anchor. The confirmation ends for good, and the app shows the record again, at the first delete, write on `{0, 0}`, different marker, missing record write or predecessor that cannot be authenticated, including an upload without the needed chain item or a different write at the kept version. An unresolved or incomplete predecessor proof never advances the anchor. A restore drops every such confirmation (spec 6, Putting back a deleted record). While the record's current write doesn't open or read, or is one the app ignores, the confirmation lifts nothing. Thus a later write cannot reuse a confirmation merely by repeating the version and marker: continuity must be proved by the exact authenticated writes. A missed write can have been a delete and its write-back, reaching a late reader only as stubs that name no record (Dispatch wire 7.5), so its lost continuity requires the user to confirm again.
- **Merging:** an app whose write-back fails with `409` takes the current write the answer carries (Dispatch wire 6.2, A conflict carries the current write) when it holds every message, `used` entry, mark, listed period and grant the app's own write would and the same `lost_to_delete` where that write sets one; otherwise it writes over it, at its version, the merge: the current write with what its own write adds, `lost_to_delete` as the current write carries it, or as the app's own write sets it where the current write carries none, the longer `grants` where one starts with the other (5.4, Grant history), and, where both hold a message in one role with different hashes, the one a later write may hold there (the `list` of the higher `version`, 5.4; the later period details, 5.2), else the current write's, and the other where the collection keeps a second (an `allowances` record's `conflicted`, 5.4; `earlier_details`, 5.3) or nowhere, as for a second end (spec 6, One message per role); `attempted_periods` stays within its 16 as an older copy's write-back keeps it. For the issued one-time payee receipt above, Merging preserves the full opaque-member union and applies its proved exception to the current marker. The app takes the merge as what it has seen there. A reader that has seen more than a write carrying `lost_to_delete` holds merges over it the same way and names nobody, rather than ignoring it for what it lacks (5.2, Seen roles). A merge only adds, so a write-back over it finds nothing more to add.
- **Repeated deletes:** each is put back the same way; the app names a deleting app once and tells its user once per deleting app, and, writing back an older copy, once per delete it answers, whose item hash its `lost_to_delete` names, so a confirmation of an earlier loss never covers a later one (Confirming a loss).

### 5.1 Fixed policies

| Collection | `read_scope` | `write_scopes` | Permanence | Policy hash in `identifiers.json` |
|---|---|---|---|---|
| `payer_activity` | `paykit_pay` | `["paykit_pay"]` | Ordinary | `policies` |
| `payee_activity` | `paykit_request` | `["paykit_request"]` | Ordinary | `policies` |
| `payee_activity`, a request sent under a label | `paykit_request:<label>` | `["paykit_request:<label>"]` | Ordinary | `policies` (label `shop1`) |
| `payment_lists` | `paykit_pay` | `["paykit_pay"]` | Ordinary | `policies` |
| `private_list_publications` | `paykit_request` | `["paykit_request"]` | Permanent for the named generation (5.8) | `record_keys` (shop publication for Alice) |
| `recovery_uploads` | `paykit_recovery` | `["paykit_recovery"]` | Ordinary | `policies` |
| `allowance_proposals` | `paykit_allowance` | `["paykit_allowance"]` | Ordinary | `policies` |
| `allowances` | `paykit_allowance` | `["paykit_allowance"]` | Permanent, `generation: null` | `policies` |
| `allowance_ledgers` | `paykit_allowance` | `["paykit_allowance_execute"]` | Permanent, `generation: null` | `policies` |
| `block_floor` | `paykit_pay` | `["paykit_pay"]` | Permanent, `generation: null` | `policies` |
| `settlements` | `paykit_settlements` | `["paykit_settlements"]` | Permanent, `generation: null` | `policies` |

Apps ignore a record in these collections whose policy differs. The permanent Allowance key guards pending proposals (spec 8.3.1). The ledger's fixed policy has read scope `paykit_allowance`, sole write scope `paykit_allowance_execute` and permanent generation `null`. The same write scope applies to the original creation and every successor. Derive the account's single ledger key with that policy and the named collection (Dispatch wire 1.5, 6.2). Creation requires absence at application. The service refuses deletion of the permanent record. Paykit separately verifies the sealed value, original Q and current executor authority. A labelled request's records, its request record and its period records, have its label's policy, are sealed with the label key (Dispatch wire 1.5) under the AAD of Dispatch wire 6.2 with `scope` `paykit_request:<label>`, and bind that label (5.3).

### 5.2 Stored message

A verifiable copy of one message (Dispatch spec 8.2):

| Field | Type | Req | Notes |
|---|---|---|---|
| `sender` | pubky | yes | the sender identity |
| `cert` | `signed<AppCert>` | yes | the sender's certificate, as the inbox entry carried it |
| `plaintext` | object | yes | `{ "sender_app": b64(16), "signature": b64(64), "json": b64 }`: the signed plaintext of Dispatch wire 4.5 without padding, the JSON as base64url of its exact bytes |
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
| `msg_ids` | `array<uuid>` | no | the `msg_id`s of the messages the record stores, and of at most 16 others it reflects, the newest (Dispatch spec 11) |
| `status` | string | no | informational, written by the last writer, never trusted |
| `allowance_id` | uuid | no | a payer's request record: the Allowance an executor accepted the request under, written in the acceptance's batch, conditioned on the Allowance record's version, or else, for a one-time request, the one its first automatic payment was made under, written in that attempt's batch; later automatic payments of the request and its periods use only that one, but for the periods `reassociations` moves (spec 8.2, 8.3) |
| `attempted_periods` | `array<int>` | no | a payer's request record of a recurring request: the start, in Unix seconds as in the obligation ID (Dispatch wire 1.5), of every Billing Period that may have an open or `unknown` attempt; at most 16, ascending, each once (spec 8.2). Anywhere else, or with more, a repeat or another order, the record is invalid |
| `reassociations` | `array<object>` | no | a payer's request record of a recurring request, beside `allowance_id`: `{ "allowance_id": uuid, "from_period": int }`, the Allowance the user moved the Billing Periods from `from_period` on to (spec 8.3); at most 16, `from_period` a period start as in `attempted_periods`, strictly ascending; the first names another Allowance than `allowance_id`, and each another than the entry before it |
| `manual_only` | bool | no | `true` or absent: a payer's request record, or a payer's period record; no app pays the occurrence automatically, an executor or an app paying a Billing Period without its user's confirmation; the batch of an attempt its user started by hand writes it too, on the occurrence's record, creating a period record the period lacks; and while the request record of a recurring request holds it no executor accepts that request and no app pays one of its Billing Periods automatically (spec 8.2, 8.3) |
| `lost_to_delete` | `b64(32)` | no | the item hash of the delete a write-back from an older copy answered (Section 5, Put back over a delete) |

`unrequested`: `{ "app_id": app-id, "endpoint_id": string, "payload": string, "source": "request" | "list" | "public", "list_version": list-version (for a list), "single_use": bool, "amount": Amount }`. `list_version` is present for a list and absent otherwise, and the stored proof's complete pair is the same (4.10). The payer journal destination, consumed-list keys and proof lookup retain both members with the publisher identity/App ID and recipient binding; equal counters from another generation or recipient do not identify the same destination. `amount` is the asset and amount being paid: every app writes it with the `attempt`, and the stored proof's `amount` is the same.

`request_hash`, `billing_period` and `earlier_details` appear only in a period record, `attempt_id` and `unrequested` only in that of a payment without a request, and `allowance_id` and `reassociations` only in a payer's request record, each by the record's binding (below), and `manual_only` only where the table says; a record that carries one anywhere else is invalid (vectors: `sealed.json` `activity_records`).

A payer's period record may hold no message: besides `v` and `counterparty`, only `request_hash`, `billing_period` and `manual_only`: the record a mark creates (spec 8.3), or, without `manual_only`, the one the batch of an automatic attempt on a Billing Period creates when the period has no record yet, since that batch writes the period record at the version it read (spec 8.2, 8.3); its key is derived from `counterparty`, `request_hash` and the start of `billing_period`, as for any period record, and such a record binds nothing a reader acts on but the mark it may hold.

A payment without a request to a public endpoint made while the payer's own Dispatch didn't answer has the same record and claim, under spec 8.4, Late recording. Recording never makes another payment call or changes the original attempt ID or obligation. Once success is authoritatively known, including from later method history, and reconciliation permits recording, the payer accounts for every possibly submitted batch and reads the claim before creating one. Only when no earlier recording batch applied and none can still apply does it prepare the batch of that original `attempt` and this record, with a `deadline` 10 minutes after a fresh verified service time. An expired unsubmitted batch or an established non-applied batch may be replaced under those same conditions; the Dispatch's first returning answer fixes no deadline. An unanswered batch stays unresolved and is retried unchanged or resolved by authenticated evidence under Dispatch wire 6.4 and 6.8 before a replacement is prepared. A restored journal, an absent claim after loss or forgetting, or expired batch evidence is no proof of non-application. If the attempt exists, the payer uses its actual step and pinned deadline, reads this record, and, once success is known, completes only a still open or unknown attempt with `done` and the record with its proof, even after that deadline. A done attempt is not completed twice; a failed or subsequently forgotten claim follows spec 7.2's payment-proof and receipt correction rules without creating a fresh attempt. The record and unresolved batches remain accounted for under the restore and provider-loss rules. The positive and negative runtime cases are required by spec 18.

A request record sits at the key derived from `blind(lp(payee, request hash))`, a period record at `blind(lp(payee, request hash, u64(period start)))` (Dispatch wire 1.5). Readers check that the stored request's hash matches the record key, recomputing it with their own naming key.

**Binding** (Dispatch spec 8.2): every stored message names the record's obligation by its own signed fields, never by the record's unsigned members:

- the request hash at its place in `refs`: first in an acceptance, rejection, cancellation, period details and proof, second in a receipt, after the proof hash it confirms (4.6 to 4.11);
- for a Billing Period, the signed `billing_period` of the period details, proof and receipt.

In a request record these must name its request, and none may name a Billing Period. **Labels** (spec 6): in a `payee_activity` record under a labelled policy, the stored request and every message from the payer (acceptance, rejection, the payer's cancellation, proof) carry that `label`; under the fixed policy none does. The payee's Period Details, receipts and payee cancellations carry no `label`. On both sides, readers check the original signing certificate against the signed request under spec 3.4. For a Billing Period this requires the signed request in its parent record. A period record holds no request, so under a labelled policy the payer's messages in it carry the label. Period details a period record holds are read against its request record's request: a reader ignores any invalid against it (4.9) and pays by none. A record that breaks this is invalid. The record key derives from the policy hash, so a labelled request's record sits at another key than an unlabelled one's. A `payer_activity` record keeps the fixed policy whatever the request's label. Vectors: `sealed.json` `labelled`, with its rejections (another label's policy, the fixed policy, an unlabelled request under a label). **Seen Allowance:** an app that has seen a request record's `allowance_id`, or an entry of its `reassociations`, ignores and flags a write that changes or drops it, or that adds an entry not after the last it saw (Dispatch spec 8.2). The `record.put` that adds an entry carries `server_time_in(null, s)`, `s` the entry's `from_period`, so the period hasn't started on the service's clock. A writer adds an entry only with a `from_period` later than every period in the `attempted_periods` of the record version its write builds on, and a reader that holds that version ignores and flags a write whose new entry's `from_period` is at or before one of them: a period attempted early stays under the Allowance it was attempted under (spec 8.2, 8.3). An attempt on a later period, early or not, counts under the Allowance of the last entry starting at or before it. **Seen marks:** an app ignores and flags a write that drops a `manual_only` it saw. A record without a `request` is a period record if any message in it names a request or a period, or `request_hash` or `billing_period` is set; then every message in it must name both, all the same ones, the key is recomputed from them, and `request_hash` and `billing_period`, if set, must equal them. A message missing a binding field makes the record invalid. A record whose messages name no request is that of a payment without a request.

**Attempted periods** (spec 8.2). The batches that read or write `attempted_periods`:

- **An attempt on a Billing Period not listed** carries, besides the `attempt` with its conditions (`record_version` of the request record, `server_time_in(s, null)`, 4.5), a `record.put` of the request record at its next version: the list with the period added and, for each listed period it drops, a `claim_step` condition on that period's claim (Dispatch wire 1.5), at the step the writer read, on the `record.put` itself. It drops only periods whose claim it read with no open or `unknown` attempt. With 16 listed and none to drop, the app makes no such attempt. An executor's batch writes no `allowance_id` there: a recurring request's record names its Allowance from its acceptance on (5.5).
- **An attempt on a listed period** doesn't write the request record; its `record_version` condition is enough.
- **Any other write** of the request record may drop listed periods the same way, each with its `claim_step` condition; one that doesn't drop keeps the list as it read it.
- **A payer's cancellation** (4.8) is a `record.put` of the request record at its next version, with a `claim_step` condition for the claim of every listed period, at the step read, none of which had an open or `unknown` attempt.
- **Conditions per operation:** these `record.put`s carry up to 18 conditions for the listed periods and reassociation, one `claim_step` for each of the at most 16 listed periods and, where one is carried, a `record_version` and the `server_time_in` of a write that adds a reassociation (Seen Allowance, above). A receipt batch also checks each write used to authorize the credit (5.7). For example, a receipt-only batch can require the bucket version and the exact current applications of both the recovered bucket and the recovered activity. A Billing Period also checks its parent request. The pinned Dispatch sets no separate limit on the number of conditions per operation.

A reader that holds the version a write was built on (its `expected_version`) checks every listed period the write drops: the `record.put` must carry a `claim_step` condition on that period's claim. One that doesn't is ignored and flagged, like a dropped role (Seen roles, below), and the reader keeps the period listed. The reader checks only that the condition is there, among the conditions the write's operation signed, which come with the write (Dispatch spec 8.2): the service enforced it against the claim as it stood when it applied the write, and it enforced a cancellation's conditions the same way, so readers don't check those. Readers check the list's shape: ascending, each start once, at most 16. A start that names no Billing Period of the request fences nothing. An empty list is the same as none, and writers leave it out. `status` never comes from the list (Dispatch spec 8.2).

**`msg_ids` is bounded** (Dispatch spec 11): a writer keeps the IDs of the messages the record stores and the 16 newest of the others it adds (`RECENT_MSG_IDS`), in order, and drops older ones; each app's processed pairs, kept 30 days, cover those. Each ID appears once, and a repeat is dropped at its later place before the 16 are taken, so the 16 are distinct; a reader reads a list that repeats an ID, or names more, as the list a writer would have written (vectors: `sealed.json` `msg_ids_bounds`). A reader treats a message the record stores as reflected whether or not the list names it. So a record stays bounded however many messages repeat a role it holds.

**Seen roles** (Dispatch spec 8.2): a record holds one message per role, so a reader that has seen a message in a role ignores, and shows with its writer, a later write that drops or replaces it. Period details it saw, in `period_details` or `earlier_details`, stay in one of them: a later write that drops one, or holds other details as late with another hash, is ignored the same way. A write may move `period_details` only to later details, except the write that stores the proof, which sets it to the details the proof names. A delete isn't such a write (spec 6), but the reader keeps what it saw, so a later write that brings the record back without it is one, but for a write-back carrying `lost_to_delete`, which it merges over (Section 5, Put back over a delete). A reader puts a deleted record of a family spec 6 lists back as that section says.

Activity-record writers may write absent optional members as `null` where Conventions allows it. Writers omit `allowance_id` and `lost_to_delete` when absent (a `null` `lost_to_delete` is a present member that doesn't read: Section 5, Deletion data that doesn't read), `attempted_periods`, `earlier_details` and `reassociations` when empty, and `manual_only` when false. On `v0.2`, `paykit-lib` reads and writes the activity record’s delete item hash and checks its valid and invalid encodings against the sealed-record vectors. The SDK uses this marker to preserve an applicable payee credit restriction, including during in-place restore. Shared replacement and the remaining protected-record recovery limits are listed in the [implementation table](implementation.md).

Example (alice's `payer_activity` record of bob's request, certificate and plaintext cut short):

```json
{
  "v": 1,
  "counterparty": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
  "request": {
    "sender": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
    "cert": {
      "body": "eyJ2IjoxLCJraW5kIjoicHVia3ktc3dpdGNoLmFw...",
      "sig": "4bAiKsZ2ITml9GP7Nqxgz3-lJ5WB25fIqYRarBxke89Mye8URlPFAM5qGTqWMxRcPS5lWLy1aDPt6IyRVQsoDA"
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

### 5.4 `payment_lists`, `allowance_proposals` and `allowances`

**`payment_lists`**, one per payee app:

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `payee` | pubky | yes | |
| `app_id` | app-id | yes | the publishing app |
| `list` | Stored message | yes | the latest `private_payment_list` |
| `used` | `array<object>` | no | single-use endpoints used: `{ "version": list-version, "endpoint": string, "attempt_id": b64(16) }`, each written in the batch of the `attempt` that pays it, whatever it pays: a request, a Billing Period, an automatic payment or none (spec 7.2) |
| `msg_ids` | `array<uuid>` | no | bounded as in 5.3: the list's own and the 16 newest others |
| `lost_to_delete` | `b64(32)` | no | the item hash of the delete a write-back from an older copy answered (Section 5, Put back over a delete) |

**Binding** (Dispatch spec 8.2), as for activity records: the reader recomputes the record key with its own naming key (Dispatch wire 1.5) and acts on the record only if

- the key is the one derived from `payee` and `app_id`;
- the stored list verifies, addressed to this identity, its sender identity is `payee` and its sender app is `app_id`.

A `used` entry consumes the entire pair for this publishing app and recipient, not merely its named endpoint. The pair comparison of 4.13 also governs every held-to-later and loss-floor comparison of these versions.

**Seen lists** (Dispatch spec 8.2; Section 4.13): a reader that holds a `payment_lists` record ignores, and shows with its writer, a later write whose list has a lower `version`, or the same `version` and another hash, and one that drops a `used` entry it saw, whatever the newer list's `version`: a newer list doesn't reset `used`.

**`allowance_proposals`**, one per pending Allowance proposal, sent or received (spec 8.3.1):

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | |
| `allowance_id` | uuid | yes | |
| `proposal` | Stored message | yes | the pending `allowance_proposal` |
| `conflicted` | Stored message | no | a second `allowance_proposal` under the same `allowance_id`, from either side, with another hash than `proposal`: the proposal is conflicted and never accepted (spec 8.3). Writers leave it out when absent |

**Binding.** Derive the pending record key from its fixed policy and `counterparty` and `allowance_id` with the own naming key. Verify each stored message, its certificate, parties, ID and proposal terms (4.15). A conflict holds a distinct signed proposal for the same parties and ID. A later live write preserves the proposal and every observed conflict. A verified current deletion ends the pending lifetime (spec 8.3.1).

**`allowances`**, one per accepted Allowance, on either side:

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | |
| `allowance_id` | uuid | yes | |
| `proposal` | Stored message | yes | |
| `response` | Stored message | yes | the acceptance |
| `end` | Stored message | no | |
| `conflicted` | Stored message | no | a second `allowance_proposal` under the record's `allowance_id`, from either side, with another hash than `proposal`, or a second `allowance_acceptance` or `allowance_rejection`, beside `response`, with another hash than it: the Allowance is conflicted (spec 8.3). Writers leave it out when absent |
| `grant` | Executor grant | no | the current grant (3.3): the last entry of `grants` |
| `grants` | `array<object>` | no | the grant history, below; default `[]` |
| `ledger_handoff` | object | no | the previous executor's last ledger version at a handoff (Ledger handoff, below). Writers leave it out when absent |
| `msg_ids` | `array<uuid>` | no | bounded as in 5.3: those of the messages it stores and the 16 newest others |

**Binding** (Dispatch spec 8.2): the reader recomputes the record key with its own naming key (Dispatch wire 1.5) and acts on the record only if

- the key is the one derived from `counterparty` and `allowance_id`;
- there is a `proposal` and a `response`; a record lacking either is invalid;
- each member holds its kind: `proposal` an `allowance_proposal` with a `proposer_role` of `allower` or `allowee` and valid terms (4.15), `response` an `allowance_acceptance`, `end` an `allowance_end`, and `conflicted` either an `allowance_proposal` as `proposal` is, with another hash than `proposal`, or, beside a `response`, an `allowance_acceptance` or `allowance_rejection` from the proposal's recipient that refers to the proposal, with another hash than `response`;
- every stored message names that `allowance_id` (the proposal's body, and the body of the acceptance, rejection or end, 4.15 and 4.16) and is between this identity and `counterparty`, with the sides its kind requires: the proposal, and `conflicted` holding a proposal, from either side, the response, and `conflicted` holding a response, from the proposal's recipient, the end from either side, each addressed to the other; the response, a conflicting response and the end refer to the proposal in `refs`.

**Permanent records.** Both roles use the fixed permanent policy in 5.1. A proposal or rejection sent before acceptance reads this key after the pending key to determine whether its original is still eligible for delivery (spec 8.3.1). Each live permanent value must carry the matched signed proposal and acceptance. A rejection can occupy only the subsequent conflict role.

A grant doesn't affect whether the record is valid. Grants are checked against the terms hash of the record's own proposal and acceptance (Dispatch wire 1.5). `grant` is the current grant only if it is the last entry of a usable history and verifies for this identity, `counterparty` and the Allowance at that entry's version (3.3), and the record holds no `conflicted`; otherwise the record has no current grant. A conflicted record still names the executor at each version (Grant history, below), by which its ledger's later writes are checked. A history entry that doesn't verify means "no valid grant from this version on" (Valid entries, below), and a history that isn't usable makes every ledger check against it fail closed.

**Seen roles** (Dispatch spec 8.2, spec 6): an `allowances` record holds one proposal, one response, one end and one `conflicted`, so a reader that has seen one of them ignores, and shows with its writer, a later write that drops or replaces it, by hash. So no write clears a conflict: once a reader has seen `conflicted`, it ignores every later write without it.

**Grant history** (spec 8.3 "Who wrote it"). `grants` holds every executor grant the record has held, oldest first, each with the record version the write adding it built on:

| Field | Type | Req | Notes |
|---|---|---|---|
| `generation` | int | yes | the generation of the version the adding `record.put` read (its `expected_version`), 0 for a new record |
| `version` | int | yes | that version's `seq`, 0 for a new record; with `generation`, signed by the grant (3.3) |
| `grant` | Executor grant | yes | 3.3 |

- **Writing.** An executor that writes its grant, on setting up, at a handoff, or when a confirmation signs it again (spec 8.3), appends an entry naming the version its write builds on, the one it read, and sets `grant` to the same grant. A write that fails with `409` is rebuilt from a fresh read, so an entry's version is always the one the adding write built on. Writers only append: every other write keeps `grants` as read.
- **A usable history** has versions that strictly increase, ordered by (generation, `seq`), and ends with an entry whose grant equals `grant`. Otherwise every ledger check against it fails closed.
- **Valid entries.** In a usable history an entry names an executor only if its grant verifies at the entry's version for this identity, counterparty and Allowance, and names the current terms hash (3.3). Any other entry means "no valid grant from this version on".
- **The executor at a version** v is the one named by the last entry whose version is older than v: the adding write built on that version, so the grant holds from the write's own version, which is newer. A ledger write (5.5) counts only if it carries `record_version` on the Allowance record at some v no newer than the Allowance record the app holds, and its writer is the executor at v. This holds for versions the app never saw: a version that is now a stub, an app added later, or a new executor checking its predecessor's writes.
- **Rewrites.** An app that has seen a history ignores and flags a write whose `grants` does not start with it, or whose `grant` is not the last entry's grant (Dispatch spec 8.2). An app that never saw it relies on the signed versions: a moved grant doesn't verify (3.3). Dropping an entry is still possible.
- **Records without a history** have no current grant, until the executor writes its grant again.
- **Size.** An entry with its certificate takes about 1 KiB, so a record whose stored messages are small holds about 200 grants within the 256 KiB cap, and fewer beside large ones: unlike the kinds activity records store (4.1), Allowance messages are bounded only by the envelope limit of 65536 bytes (Dispatch wire 4.4).

Example entry (`documents.json` `executor_grant_history_entry`: the record was created at (1, 12) and the executor's grant write built on it; certificate cut short):

```json
{
  "generation": 1,
  "grant": {
    "app_id": "cw5ahtdkixcafna9zh1y5y5o1w",
    "cert": {
      "body": "eyJ2IjoxLCJraW5kIjoicHVia3ktc3dpdGNoLmFw...",
      "sig": "Ir-nJajpFEnwb23vssI3EeOQj5e1-q6-40FrkjXZb0f6irORauaGp_fi8YPsLbC3V153vNz1PfcjyPJI_DXODA"
    },
    "confirmed_generation": 1,
    "sig": "jKZ13zfRyNtQPpLEeOU2USP0AH5ryagD_QqudMVFPn2bi61kshMto6Uuqxohf38_ueLZXBivfe9V3j_CWacmBg",
    "terms_hash": "c6fSZwYRwRa_3yscaUapKYa_TKWdhfCEvJ5kzyMbauI"
  },
  "version": 12
}
```

**Ledger handoff** (spec 8.3 Handoff). Only the previous executor writes `ledger_handoff`, at the record version it read, naming its last ledger version (5.5) for the grant that handed the Allowance on:

| Field | Type | Req | Notes |
|---|---|---|---|
| `app_id` | app-id | yes | the previous executor, the app of the grant history entry just before `grant` |
| `grant` | object | yes | `{ "generation": int, "version": int }`: the version of the grant history entry that handed the Allowance on |
| `ledger` | object | yes | `{ "generation": int, "version": int }`: the version of the previous executor's last ledger version |
| `item_hash` | `b64(32)` | yes | that ledger write's item hash (Dispatch wire 6.6) |
| `cert` | `signed<AppCert>` | yes | the previous executor's certificate, holding `paykit_allowance_execute` |
| `sig` | `b64(64)` | yes | label `paykit/v1/ledger-handoff` over `lp(label, identity, counterparty identity, allowance_id text, u64(grant generation), u64(grant version), u64(ledger generation), u64(ledger version), item hash)` (Section 2) |

- **Counts** only if `cert` verifies for the own identity, names `app_id` and holds `paykit_allowance_execute`; `sig` verifies with the certificate's signing key; the history is usable, `grant` is the version of an entry whose grant names another app than `app_id`, the entry just before it names `app_id`, and every later entry names the same app as that entry, so the handoff is the one to the current executor; and `app_id`'s app isn't removed (Dispatch spec 12.2). One that doesn't verify, or whose `app_id` isn't the app of the entry before `grant`, is ignored and flagged (Dispatch spec 8.2); one for an earlier handoff, or by a removed app, counts for nothing.
- **Writing.** The previous executor writes it only with complete usage authority (5.5), while the current ledger exactly matches its complete value and every ledger write it sent has a verified answer incorporated into that state, and writes none when it holds no ledger version, when the one it holds is one a restore installed, when it isn't one it wrote or took as its kept version by a `ledger_handoff` since its own grant (so a second handoff before the mark needs complete authority before repair), or once an entry of another app follows `grant`. A grant the same app signs again is no handoff. Other writes keep it as read; the record holds at most one, and a later handoff's replaces it: one that verifies (Counts, above, but for being to the current executor) and whose `grant` is a later entry of the record's grant history than the one it replaces. A reader that has seen a `ledger_handoff` ignores and flags (Dispatch spec 8.2) a later write that drops it, or replaces it by one for the same or an earlier grant, which could name an older ledger version; when the one it has seen does not read as a handoff, its `grant` member must still read as a record version and identify an entry of the preserved grant history before a replacement can be ordered after it. If that member cannot be read or placed, the reader takes no replacement. Keeping the unread value exactly is allowed; it never makes that handoff count.
- **Building on it** (5.5 Last ledger version), versions ordered by (generation, `version`): after an earlier grant, a new executor builds on no ledger except for complete-authority repair, and pays nothing automatically, until a valid `ledger_handoff` or successful repair establishes its current-term anchor. An earlier-term version alone (grant history N, P, N) does not establish that authority. A restored executor whose backup holds no accepted complete anchor for its current grant takes repair alone. The backup itself supplies no such anchor. Under the ordinary handoff path, a recipient obtains the complete matching value before payment or repair; holding only its version and item hash is insufficient. One that holds a version and has not written the ledger since the handoff takes the named write as its kept version unless the one it holds is later, and flags an earlier named write or another write at its version; one that has written the ledger since the handoff requires the named write to be the one its first ledger write since the handoff built on: a later one, another write at that version, or any named write where it built on a `404`, is a rollback (Dispatch spec 10.1), and an earlier one is flagged.
- A `ledger_handoff` doesn't affect whether the record is valid. Its vectors, its binding in `sealed.json` `allowance_records` (a record holding the executor's `ledger_handoff` for the new grant, and one naming a ledger version no write gives, which reads with it counting for nothing) and its signature in `signatures.json` (the label `paykit/v1/ledger-handoff`), are generated and checked by `paykit-lib` on `v0.2` and copied here.

### 5.5 `allowance_ledgers`

**Binding** (Dispatch spec 8.2): the reader recomputes the record key with its own naming key (Dispatch wire 1.5) and acts on the ledger only if the key is the one derived from its `counterparty` and `allowance_id`, and both fields equal those of the reader's `allowances` record for that Allowance. Its `acceptance` must equal the hash of the signed acceptance verified in that permanent record. Every successor, compaction and repair retains that hash. A ledger, Allowance record or `payment_lists` record that fails its binding is invalid: the reader doesn't act on it, flags it as Dispatch spec 8.2 says, and keeps the content it trusts. A reader that holds no `allowances` record for the Allowance yet, or one without an acceptance, doesn't act on the ledger either, but flags nothing: it keeps the ledger version it holds until it holds such a record with an acceptance (spec 8.3). In `sealed.json`, `allowance_initial_ledger` gives the canonical first value and refusals for a missing or different acceptance hash. The ledger in `binding_accepted` retains the original acceptance hash with entries and totals.

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | the allowee |
| `allowance_id` | uuid | yes | |
| `acceptance` | `b64(32)` | yes | hash of the original signed acceptance in the permanent Allowance |
| `entries` | `array<object>` | no | one per attempt under the Allowance, below |
| `totals` | object | no | `{ "spent": amount, "count": int, "failed": array<object> }`: paid amounts and payments of compacted entries, and the compacted `failed` entries, each `{ "obligation": b64(32), "attempt_id": b64(16), "value": amount }`, so a later `done` still counts once; `spent` may be `""` for none, and `failed` is left out when empty. |

An entry:

| Field | Type | Req | Notes |
|---|---|---|---|
| `obligation` | `b64(32)` | yes | the obligation ID of the request or, for a Billing Period, of that period, the claim key of its claim (Dispatch wire 6.2) |
| `attempt_id` | `b64(16)` | yes | |
| `value` | amount | yes | the amount the payment transfers in the requested asset, never less than the requested amount (spec 7.2 step 1, 8.2), and fees aren't counted |
| `asset` | string | yes | |
| `at` | time | yes | the time the executor stated, vouched for by `server_time_in(at, at + 60 s)` |
| `outcome` | `"done" \| "failed"` | no | the attempt's outcome, once known: `done` is final; `failed` is written only when the attempt's owner failed it or the user released it with durable never-handed-off evidence; it does not itself release capacity (Counting, below) |

- **Executor batch** (spec 8.3): the ledger write carries `record_version(Allowance record)` and `server_time_in(at, end)`, where `end` is `at + 60 s` cut at the next boundary of any anchored limit; the `attempt` carries the request's `deadline` and `record_version(request record)` (spec 7.2); a third operation writes the activity record showing the payment as being handled (spec 8.3 step 3); when the request record of a one-time request doesn't name the Allowance yet, that record carries `allowance_id` (5.3), and for a Billing Period not yet in `attempted_periods` a further write of the request record adds it (5.3); and one more writes the `payment_lists` record when a single-use endpoint is paid (5.4).
- **The amount transferred** (spec 7.2 step 1): before the batch, the executor has its payment method report what it will transfer to the destination; another amount or asset, or an amount it can't tell, makes the destination unusable for automatic payment; the fee is the payment method's, which bounds it (spec 7.2 step 1).
- **Last ledger version** (spec 8.3): the executor keeps the version and item hash (Dispatch wire 6.6) of the last ledger write it wrote or read with an authenticated version (Dispatch wire 6.2 Accepting a write). Before an automatic payment, the ledger write `GET /records` shows must be that one, by item hash, or one whose signed `expected_version` is that version or later. A verified write built on an earlier version, an authenticated deletion of this permanent ledger, or confirmed absence contradicting its kept version in the same uninterrupted history raises the rollback alarm (Dispatch spec 10.1). An unconfirmed response or unresolved history after provider loss holds spending. A write whose operation verifies with its writer's certificate (the record view's `cert`, the certificate the service checked the write against, Dispatch wire 6.2 step 1), that is the kept write or states `expected_version` at or after the kept version, but that fails a check of Dispatch wire 6.2 by policy (its writer's standing) is refused as untrusted ("Building on a version"), which pauses the Allowance. A write that fails the checks of Dispatch wire 6.2 is never handed to the protocol: signed evidence of misbehaviour (an item that doesn't match its stub, Dispatch wire 6.6, or a proof that names another operation or contradicts the verified chain) is the client's own rollback (Dispatch spec 10.1); one whose failure is only the service's word (an operation that doesn't verify with its writer's certificate, a missing date, the certificate the service chose, a version that fails Dispatch wire 6.2 step 6) is ignored, naming no app, raises no alarm, and leaves the executor's last ledger version as it was. The first grant may establish an initial checkpoint under Section 5.9. After an earlier grant a new executor establishes its accepted anchor through a valid handoff (the Allowance record's `ledger_handoff`, 5.4) or successful complete-authority repair under its current grant (5.4 Building on it and Successful repair below). A write following a handoff must be the named write or have a signed `expected_version` at or after the named version and pass the successor checks. Under the same uninterrupted account history and grant term, the same executor may resume from a previously accepted complete ledger under spec 8.3, Resuming the same executor. It must verify the complete signed service continuation through the current write and independently complete payment-method history through the reconciliation cutoff, including in-flight payments. A backup alone, a point lookup, a later version number or a current ledger read alone cannot establish that continuation. All admission and limit checks still apply. A changed executor or term requires the applicable handoff or repair. Incomplete evidence holds automatic payment. A confirmed attributable payment demonstrably omitted from the authenticated ledger, including compacted usage, and the relevant claims follows the rollback rule in spec 8.3. Every repair requires Complete usage authority below and never supplies an empty baseline for missing history (spec 8.3).
- **Counting:** an entry's own `done`, a claim showing its attempt `done`, or the executor's copy of the request's or Billing Period's payer record holding a proof of that attempt naming this Allowance spends, whichever executor made the attempt. An entry without an outcome whose claim shows `open` or `unknown` reserves. Every `failed` attempt counts as spent immediately, whether the entry itself holds `failed` or reads it from its claim, while the claim remains and after it is gone, whoever wrote the failure. It releases only when the executor's durable journal proves the attempt ended before the payment method's call and can never hand off; an entry at the call, an ambiguous result, a user's confirmation alone or a restored journal is no such evidence. A `done` or proof as above overrides that evidence and spends. A record the executor can't read, one without a proof, or a proof of another attempt or naming no Allowance or another one releases nothing. An entry without an outcome whose claim is gone or doesn't hold the attempt spends. A `totals.failed` item counts by the same rule, in the lifetime amount only, until its later `done` moves into `spent` and `count`, once (Outcomes written, below). An anchored limit counts entries whose `at` is in the period containing the evaluation time; a rolling limit of length L counts an entry while `t < at + 60 s + L`.
- **Outcomes written** (spec 8.3): the executor writes `done` from a claim, or from its copy of the payer record holding a proof of the attempt naming the Allowance, `failed` from a claim only when its `resolved_by` is the owner, `failed` for an amount the user released with the never-handed-off evidence above, and `done` over `failed` when a claim shows it later. For that it reads again the claims of `failed` entries and of compacted failed attempts; a compacted one whose claim shows `done` moves from `totals.failed` into `spent` and `count`, once. The service keeps those claims to their deadline plus 30 days (Dispatch wire 6.8), and a `failed` entry or `totals.failed` item stays at least as long, so a `done` a late receipt records in that window is seen. A receipt after the window can't be recorded on the claim, which is gone; an executor that gets one treats the attempt as `done` all the same and writes `done` over the entry's `failed` (or moves the compacted item into the totals).
- **Compaction:** entries with a final outcome and `at + longest period + 60 s < now` are dropped, `now` being the latest service time the executor has verified: a `done` one's value and count are added to `totals`, and a `failed` one is kept in `totals.failed`. The longest period of an anchored limit is the length of its period containing `now`. A ledger that can't take a new entry within the 256 KiB limit even after compaction, counted with an outcome (`failed`, the longer) in every entry that has none, pays nothing automatically (spec 8.3).
- **A valid ledger** (spec 8.3 Compaction): every entry's `asset` is the terms' `asset`, each `attempt_id` appears once among the entries and `totals.failed`, and every `value`, `at` and `totals.spent` parses (Dispatch wire 1.6, Section 1.2). Usage and compaction refuse a ledger that isn't valid; they never read it as smaller.
- **From one version to the next** (spec 8.3): a reader that holds a ledger version accepts a newer one only if it counts no less: every held entry is still there with the same `obligation`, `attempt_id`, `value`, `asset` and `at`, its `outcome` unchanged or gone from none to `done` or `failed`, or from `failed` to `done`; or it was compacted, its `at + longest period + 60 s` before some service time from the held version's to the newer write's (Dispatch wire 6.6 stub or Dispatch wire 6.5 proof, else the reader's latest service time), the longest period taken at that time: a held `done` entry into the totals, and a held entry without an outcome or `failed` into `totals.failed` with the same `obligation` and `value`, or into the totals, since it may have become `done` between the two versions, the longest period being that of the terms of the reader's `allowances` record (Binding, above) (what decides is the outcome it had when it was compacted, which the reader may not have seen); every held `totals.failed` item is still there unchanged or left into the totals; and `spent` and `count` grew by at least the values and number of what left into them. An attempt that left into the totals is in neither the newer `entries` nor its `totals.failed`, so it counts once. New entries are allowed. A newer version that breaks a rule is ignored and flagged (Dispatch spec 8.2).
- **Building on a version** (spec 8.3): the executor writes a ledger version, in any batch, only on a write that passes the kept-version check above, the `distrust_from` check (Dispatch spec 12.2 step 3) on the Allowance record and the ledger, "Who wrote it" (5.4) and the rules above against the last version it accepted; its `record.put` then carries that write's version as `expected_version`. The ledger's first write is the acceptance mechanism's (spec 8.3.1). On a refusal it writes no ledger version and the Allowance needs the user's confirmation again; an outcome batch carries the claim outcome alone.
- **A grant implies a ledger** (spec 8.3): on the Allower's account, the batch in Section 5.9 establishes the initial ledger. A grant requires a complete current anchor before payment. The initial checkpoint under Section 5.9, a valid handoff, a successful complete-authority repair, or the same executor's verified continuation in its uninterrupted term under Last ledger version above establishes that anchor. For continuation the current service and independently complete payment-method histories must both pass spec 8.3. All other admission checks still apply. `404`, unreadable state, a delete or recovered history never supplies zero. Reacceptance or a new grant cannot initialize again. The service refuses a ledger delete under its fixed policy (5.1). Loss of a previously observed permanent ledger in the same verified history raises the rollback alarm. A missing or uncertain ledger never supplies spending authority.
- **After a confirmation** (spec 8.3): the executor computes the rewrite and shows its figures (usage per period limit, lifetime amount, entries reserved, spent and released, amounts held reserved) before the user confirms; the confirmation computes it again and writes it only if the ledger and the version it builds on are the same as shown, else it refuses. Complete usage authority below is a prerequisite, even for an empty inventory. The rewrite starts from that complete accepted ledger value, retaining its cumulative totals, entries and `totals.failed` items as they were; adds from the ledger shown, when its value reads as the Allowance's ledger, the entries and `totals.failed` items it lacks (an entry's `outcome` only if `done`; entries of another asset, or that don't parse, are left out), a `done` for an entry it holds, and the higher `spent` and `count`; adds its journal's attempts under the Allowance; and adds every attempt on a claim of the counterparty's obligations whose `owner` is an app the grant history names, unless its verified proof names no `allowance_id` or another one, an attempt its owner failed included, which then counts as "Counting" says, while its claim remains and after it is gone, with its amount established by Amounts recovered below and the attempt's `opened_at` as `at`; a journal attempt takes its transfer-bound amount under that rule and its claim's `opened_at`, else the time it was journaled; a payment a payer or period record's proof shows made under the Allowance is `done`, also once its claim is gone, but its amount must be established separately under the same rule. An entry the last accepted version lists keeps its value and time. The rewrite is refused when the payer identity's own evidence shows that attempt at a larger value, or dated at or after the end of that entry's executor window (`at` + 60 s, cut at the next boundary of any anchored limit), which its executor batch lands before. Evidence within that window is the batch the entry already counts. For an attempt the version kept doesn't list, the entry takes the largest value any input shows, the latest `at` but no later than the repair's service time, the journal's or the claim's obligation, and `done` where any input shows it; a shown `failed` or `totals.failed` item is never taken for an attempt the journal or a claim shows. The obligations are those of one-time requests and the Billing Periods in `attempted_periods`, with a period record held, or from the first to the one containing the latest verified service time whose claim may still be held. The rewrite takes the outcomes the claims show and is compacted as an executor batch's ledger is. The confirmation's write also conditions every payer record the rewrite read at the version read, every period record absent from an obligation it walked at its absence, and every claim it read at its step. It carries `record_version(Allowance record)` and `server_time_in(at, end)` with an executor batch's `end`, and its `expected_version` is the exact current authoritative ledger version. The rewrite may count an attempt twice, never less.
- **Successful repair** (spec 8.3): only a verified applied result durably incorporated into complete authority ends the new-executor handoff hold. The executor retains the full result, authenticated version and item hash bound to its exact current grant as the accepted anchor; automatic payment may then proceed under every other check without waiting for a handoff. Confirmation, a pending or refused repair and an applied result not yet durably incorporated do not suffice. It separately retains the exact authenticated base version and item hash of its first repair since the handoff for 5.4's late-handoff checks; the result anchor never replaces that comparison base. A new term requires its own authority. This permits A to hand to B, B to write nothing and go offline, then A to take the grant back and repair its retained complete checkpoint when it is still exactly current in uninterrupted history. The repair's authenticated result establishes the current anchor. Late handoffs still face every current-hash and rollback check in 5.4.
- **Complete usage authority** (spec 8.3): every automatic payment requires this authority, and a later own rewrite cannot bypass its incomplete-history hold. Repair needs a full durable accepted ledger value, including `totals.spent`, `totals.count` and `totals.failed`, its authenticated version, item hash and executor term, and any exact prepared ledger batch whose result is not accounted for. After the initial ledger (spec 8.3.1), ordinary authority advances only from the exact current complete value; an own signed rewrite over an unaccounted gap cannot manufacture it. Each instance serializes its ledger writes per Allowance, storing the batch and proposed value before submission with the preceding complete ledger. Before another write, repair or handoff it durably incorporates a verified applied result or verifies definitive non-application. Lost, unknown and failed answers without that proof stay pending, including terminal-outcome, compaction, release, repair and put-back writes. Neither claim expiry nor a failed local commit clears them. The bounded state is one complete ledger and at most one prepared write; inability to retain it stops further ledger writes and automatic payments. Compaction preserves cumulative amounts, counts and failed items; an older accepted ledger without its complete accounted-for continuation is insufficient.
- **Current authority and concurrent instances** (spec 8.3): local serialization does not exclude another instance of the same executor. The current ledger being repaired must be the exact authenticated complete value held, or the exact prepared write whose verified application has now been incorporated. Repair uses that current `expected_version`; another instance's concurrent write fails the CAS. The anchor and current write must belong to the same uninterrupted account history; after provider loss, an older matching copy cannot exclude a later applied write by another instance in the lost history. Recovery upload and a new generation supply no such continuity. An unaccounted remote write, missing or deleted current value or older-copy put-back refuses repair. A higher version, same app or grant, or signed predecessor at or after the anchor supplies no missing continuation. No history or complete-inventory query is assumed. A valid handoff with the matching complete value can establish the anchor; an older handoff does not account for later writes. A previously complete checkpoint from an earlier term or backup can support repair only with its retained complete authority, exact current value/version/hash in that uninterrupted history and every known prepared write resolved. A value held only as an observer does not qualify. CAS fences still-unapplied writes, not writes already lost from history.
- **Incomplete usage** (spec 8.3): without complete authority the entire repair refuses without writing, including an empty inventory, and no automatic payment follows under the Allowance, for new obligations or later periods either. Claims and activity may expire after spending survives only in compacted totals. Surviving attempts, known amounts, an unaccepted shown ledger, confirmation, elapsed windows, a new grant/generation and older-copy put-back never replace missing usage. Recovery requires a retained complete value and accounted-for continuation under the current-write or handoff rules; loss of every such copy makes automatic use unavailable indefinitely. The host reports incomplete usage history. Separate by-hand payment or a distinct newly authorized Allowance obey existing occurrence/association rules and never reset this ledger. The durable state, runtime refusal checks and generated cases are required with the implementing code before release; no current repair API or vector establishes complete inventory.
- **Amounts recovered** (spec 8.3, Amounts recovered by a repair): each attempt included from a claim, a payer or period proof, or the journal requires durable evidence of its transferred amount, or exact amount bound before possible handoff, including rounding and excluding fees, bound to that attempt, obligation, Allowance and asset. A retained accepted ledger entry or `totals.failed` item's value is kept. The executor's own durable journal is sufficient only when it preserves that exact transfer bound; its nominal requested amount alone is insufficient. Neither the claim nor a request proof without an amount (4.10) supplies the missing value. The nominal one-time or Billing Period amount, current endpoint unit or rate, and user confirmation are not substitutes. A shown ledger that was not accepted can add a larger value but cannot alone establish a missing amount for one of these attempts. If that amount cannot be established, the entire repair is refused, writes nothing, and automatic payments stay paused; confirmation does not lift that hold. This applies before and after claim expiry and across restore/provider loss. Only established transfer amounts enter the conservative maximum above; an accepted larger value is never reduced. A `failed` attempt still counts as Counting requires and cannot be released by this reconstruction.
- **Rules checked by the vectors**:
  - **Order of refusal:** the per-payment range first, then each period limit in the order of `period_limits`, its amount before its count, then the lifetime limit; `refused_by` names the first that refuses.
  - **Rolling window:** an entry counts while `t < at + 60 s + L`, with no lower bound, so an entry dated after t (within the 60 s window) counts too.
  - **Lifetime and counts** include reserved entries as well as spent ones; compaction adds only `done` entries to `spent` and `count`, and keeps `failed` ones in `totals.failed`.
  - **Final** for compaction means the entry's own `outcome` is `done` or `failed`; an entry without one stays, whatever its claim shows.
  - **No period limits:** the longest period is 0, so final entries older than 60 s are compacted.
  - **Assets:** an executor refuses a request whose amount's `asset` differs from the terms' `asset` before evaluating limits, so a ledger holds entries of one asset; an entry of another asset makes the ledger invalid (above), and usage and compaction refuse it.
- Vectors: `allowance_usage.json` checks per-payment, anchored-window, rolling-window and lifetime limits, term formats, default-spent failures and compacted `totals.failed` items. Its `failure_evidence` cases supply already validated evidence of never-handed-off failures, with paid claims and proofs taking precedence. The SDK's evidence acquisition, validation and persistence, ledger reconstruction, reservation release and payment handoff remain runtime work (vectors README, "Allowance usage conformance").

### 5.6 `block_floor`

One record per identity, its block floor (spec 8.1, Blocked counterparties), under one permanent policy (`"permanent": {"generation": null}`), so the service refuses deletion in every generation (5.1; Dispatch spec 10.3.1):

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `floor` | time | yes | a one-time request whose signed `created_at` is earlier than it plus the clock tolerance is paid only by hand |

The `block_floor` record carries no `lost_to_delete`: a writer never originates it, a reader ignores it, and a rewrite keeps it as it keeps any member it doesn't know (Section 5; Dispatch wire 1.1).

**Binding** (Dispatch spec 8.2): the reader recomputes the record key with its own naming key and the permanent policy of 5.1 (Dispatch wire 1.5) and acts on the record only if it is the key derived from its own identity.

**Seen floor and permanence** (Dispatch spec 8.2, 10.1 and 10.3.1): a reader that holds a `block_floor` record ignores, and shows with its writer, a later write of a lower `floor`, so the floor it applies is the highest it has seen. A value that doesn't read keeps the floor seen; with none seen, the reader can't read the floor, and every one-time request counts as dated before it (spec 8.1). No Paykit app deletes `block_floor`, and under the permanent policy (`"permanent": {"generation": null}`) the service refuses deletion in every generation (5.1). Any delete on the permanent `block_floor` key whose version is authenticated by a current-service transition proof or verified stub raises the rollback alarm in every generation, whether or not the app previously saw a floor (Dispatch spec 10.1 and 10.3.1, Readers); an unauthenticated delete is ignored as the service's word, naming no app, is never treated as absence, keeps any highest floor seen, and with none seen counts as a floor the app can't read, so every one-time request counts as dated before it, an unblock the app writes and a widening that must raise the floor wait, and for an unblock the app didn't write it records it in its copy of the lists together with a pending raise for that sender that survives restart and restore, holds that sender's one-time requests and its asks until the raise is applied, and takes its other entries under their own rules (Raising, below), except under both holds (Unblocking under the divergence hold, below), and automatic one-time handoffs and repeats are withheld (spec 8.1). Within continuous account history (when the newest own `identity.json`'s `recovery_from` does not cover the last generation in which the app saw the floor live), a signed `404` from the hosting service to an authorized reader, a covering listing omitting the key, or an authenticated `{0, 0}` create while the app holds the floor live raises the rollback alarm (Dispatch spec 10.1 and 10.3.1, Absence); the app keeps the highest floor seen and holds every automatic one-time handoff and repeat, including across reopen. Absence covered by a declared provider loss (when `recovery_from` covers the last generation in which the app saw the floor live) leaves the separate provider-loss rules of spec 7.3 and the shared post-loss hold below in force, never lowers a previously seen floor, and leaves the app free to upload its retained floor under Dispatch spec 12.6. At final handoff authorization (after the durable attempt is recorded and immediately before each automatic handoff of a one-time request and each resume's repeat of a one-time payment, the repeat of an attempt its user started included), the app takes the newest own `identity.json` it has verified, kept current by its feed and revalidated as spec 7.2 requires (Dispatch spec 4.3), and reads `block_floor` again from its Dispatch, accepting only an answer signed with the service key and for the generation that document names (Dispatch spec 12.5, Fencing; Dispatch wire 7.1), and applying the higher of the floor that read shows, the one it holds and any raise it owes, waiting for no other app's raise; the shared post-loss hold (below), an own `identity.json` that can't be read or equivocates (Dispatch spec 4.3), continuity unverified (Dispatch spec 8.3), a `block_floor` answer not signed for the service and generation that document names, a failed `block_floor` read, a value that doesn't read or an unauthenticated delete with none seen, a divergent view or retained divergence hold on `block_floor` (Divergent floor, below), or a rollback alarm (Dispatch spec 10.1), one from a disappeared or deleted floor included, withholds the handoff or repeat (spec 8.1, 8.3).

**Shared post-loss hold** (Dispatch spec 4.3, 12.5, 12.6 step 5 and 12.7; Dispatch wire 3.3): after any declared provider loss, two apps at the replacement service can agree on a low `block_floor` write while a higher floor existed only at the lost provider or on an offline app, or had its divergence cleared before another app read it; a compare-and-set on the visible version, an empty upload, an elapsed wait or a signed app removal supplies no upper bound on prior raises. So each app applies the shared post-loss hold of spec 8.1, Blocked counterparties, a pause of the protocol's own that Dispatch spec 12.6 step 5 allows: once its verified own `identity.json` carries a non-null `recovery_from` (once set, `recovery_from` stays in every later version until a later loss replaces it, Dispatch spec 4.3, so a non-null value persists through ordinary updates, cooperative moves and key changes in place; `recovery_from == generation - 1` marks only the generation a loss began), or once it holds a recorded loss (spec 8.1), it makes no automatic payment of a one-time request and no resume's repeat of a one-time payment, the repeat of an attempt its user started included: no handoff to the payment method, while a history lookup by attempt ID, which only reads, still serves reconciliation (spec 7.2 and 7.3). A declared provider loss includes re-enrollment after an erasure, which Dispatch spec 12.7 makes a provider loss. A cooperative move or key change in place with `recovery_from: null` and no recorded loss causes no hold, and ordinary `block_floor` and payment authorization rules continue to apply. A recorded loss persists across restart, reopen, scope changes (including losing and later regaining `paykit_pay`) and in-place restore; having none gives no clearance, since the `identity.json` decides. Nothing in v0.2 releases the hold, and what still goes ahead under it is as spec 8.1 says. The app tells its host once, and again after each reopen, that automatic one-time payments for the identity are held for good after the provider loss and that each request can still be paid by hand. Not covered: an app that can read only a pre-loss `identity.json`, as when a stale Homeserver copy names the lost provider and that provider still answers, can't learn of the loss and may pay automatically there; this is the residual risk of Dispatch spec 12.6 (spec 13, an old provider that keeps authorizing attempts). A payment handed off before the app learned of the loss is not undone.

**Divergent floor** (Dispatch spec 12.6; Dispatch wire 7.4.9 and 7.4.14): after a provider loss, Dispatch can keep a lower write current while retaining a higher authenticated `block_floor` upload only as divergent; the record view exposes only the current operation and `divergent: true`, never the hidden upload, and a later write at the service clears the divergent uploads. An app takes `block_floor` as seen divergent whenever any view it reads (a record read, listing page or `409` answer) shows `divergent: true`, including when a later write read in the same pass of the feed cleared a view the app already read, or when the client delivers `RecordDivergent` for the identity's derived `block_floor` key (spec 6, Divergent records). For `RecordDivergent` matching the identity's derived `block_floor` key, the app always sets or retains its durable local divergence hold before acknowledging that event, even when it does not currently hold `block_floor` locally or the current view already shows `divergent: false`, and also under a refused certificate. The app retains its highest floor seen (updating it upward if a later valid write carries a higher floor, never lowering it), and the durable hold survives restart, reopen, restore, a `missed` item, a later provider loss, scope changes (including losing and later regaining `paykit_pay`) and a later write whose view shows `divergent: false`. A backup taken after observing divergence carries the hold across any restore; an in-place restore from an earlier backup preserves any `block_floor` divergence hold and highest floor seen that the installation already holds (in addition to preserved unacknowledged `RecordDivergent` duties), whereas a fresh-device restore from a pre-divergence backup has no local observation unless `divergent: true` is still live at the service when it lists. While `block_floor` is divergent or that hold stands: the app withholds every automatic one-time handoff and every resume repeat of a one-time payment (including when the user approved the original handoff manually), treats every incoming one-time request as dated before the floor (`manual_only`), writes no `block_floor` create or successor, and, under both holds only, applies an unblock or a widening that must raise `block_floor` without the raise (Unblocking under the divergence hold, below). Only authenticated evidence proving a successor includes the maximum of every authenticated divergent `block_floor` upload (and the app's highest floor seen, and, once any own app has applied an unblock or a widening without the raise, a floor at or past the service time of a verified read made after every own app stopped doing so, plus the clock tolerance) can lift this hold; user confirmation cannot lift or drop it because neither the app nor the user can read the hidden upload. Because current Dispatch APIs expose only the current write and `divergent: true` and supply no such proof, no existing evidence releases this hold, so every automatic one-time handoff and every resume repeat of a one-time payment stays held indefinitely once `block_floor` is seen divergent; ordinary manual one-time payments, recurring Billing Periods, payments without a request, stops and receipts continue under their own rules, and a manual payment never clears the hold. This hold is separate from the shared post-loss hold above. What remains uncovered under current Dispatch APIs for observed-divergence detection and `block_floor` writes (while the shared post-loss hold above still holds automatic one-time payments): (i) an in-flight compliant `block_floor` write prepared against a `divergent: false` view racing with a concurrent divergent upload, because Dispatch `record.put` and `record_current` have no `divergent: false` precondition and clear `divergent_uploads` on apply; and (ii) an app that never read a `divergent: true` view or received `RecordDivergent` before a clearing write landed (such as a lagging app whose post-upload reread sees `divergent: false`, a newly joined app, an app that gained `paykit_pay` only after the clearing write landed, or a fresh-device restore from a pre-divergence backup).

**Unblocking under the divergence hold:** an app is *under both holds* while it holds a `block_floor` divergence hold (Divergent floor, above) and the shared post-loss hold stands at it. Under both holds, an unblock it writes, an unblock it records from its read of the lists, and the widening of a restore that isn't clean or of a `missed` item (spec 7.2) apply without raising `block_floor`: the app writes its own unblock; for any unblock it takes that sender's entries and queues its asks; and it applies the widening's marks, whose asks wait for the counterparty's unblock as any block mark's do; each as once a raise has applied. Every other rule of this section and of spec 8.1 that holds an unblock the app writes, a sender's one-time requests, an ask or a widening until a raise applies, or that has an app raise the floor before or with them, applies except under both holds. This is safe because the raise exists only so that no own app pays automatically a one-time request dated before the unblock, and under the shared post-loss hold no own app pays any one-time request automatically: divergence arises only in a generation after a declared provider loss (Dispatch spec 12.6; Dispatch wire 7.4.14, Which stage and Members by stage), so the newest own `identity.json` carries a non-null `recovery_from` and the shared post-loss hold, above, holds every automatic one-time payment and every resume repeat of a one-time payment at every own app, an app that never saw the divergence included, and this app also treats every one-time request as dated before the floor. The app the shared post-loss hold leaves uncovered (Not covered, above) reads the lost provider, where a raise at the current service would not reach it either. The floor applies to one-time requests only (spec 8.1), so whether it is raised changes nothing for a Billing Period, under an Allowance or not, or for a payment without a request, each judged by its own rules (spec 8.2 to 8.4), and an executor pays a one-time request under an Allowance only past the same floor and shared post-loss hold checks (spec 8.3). Unblocking still authorizes no payment (spec 8.1). Nothing in v0.2 ends either hold, so the floor's missing raise never decides an automatic payment; a later version that could end them would first have to stop every own app from applying an unblock or a widening without the raise, and then raise the floor to at least the service time of a verified read made after that, plus the clock tolerance, which is past every raise an own app left unwritten, and to at least the highest floor it has seen and the maximum of every authenticated divergent upload, before any automatic one-time payment. A divergent `block_floor` view, a `RecordDivergent` for its key or a retained divergence hold while the shared post-loss hold doesn't stand at the app, which a service following Dispatch spec 12.6 never shows, applies nothing without a raise: an unblock the app writes or a widening waits as while the floor can't be read, and for an unblock the app didn't write it records it in its copy of the lists together with a pending raise for that sender that survives restart and restore, holds that sender's one-time requests and its asks until the raise is applied, and takes its other entries under their own rules (Raising, below; spec 8.1).

**Raising:** a writer writes the higher of the floor held and the time it raises to, only as a blocked window closes, by each app once, or as a widening applies; only an app holding `paykit_pay` writes it (5.1); an app without `paykit_pay` raises nothing and does not hold unblocks, a sender's one-time requests or widenings waiting for a floor raise (so unblocking need not wait for that app to raise the floor), while any recorded loss, `block_floor` divergence hold or highest floor it already observed stays durable across scope changes. When the app holds no `block_floor` divergence hold and `block_floor` is lawfully absent at the current service (never seen live, or absence covered by a declared provider loss whose `recovery_from` covers the last generation in which the app saw the floor live), the create carries `expected_version: {0, 0}`, the permanent policy of 5.1 and its `id_inputs`; for a current non-divergent record when the app holds no `block_floor` divergence hold, the write builds on its verified version (and names its exact current application with `record_current` over a non-divergent recovery upload, Dispatch wire 6.2 and 6.3) and preserves unknown members. An app writes no `block_floor` create or successor over a divergent `block_floor` or while holding a `block_floor` divergence hold; under both holds it then applies an unblock or a widening without the raise, and otherwise an unblock it writes or a widening waits, and for an unblock it didn't write it records it in its copy of the lists together with a pending raise for that sender that survives restart and restore, holds that sender's one-time requests and its asks until the raise is applied, and takes its other entries under their own rules (Unblocking under the divergence hold, above); no writer ever submits an empty sealed value (a delete) for `block_floor`. Except under both holds, an app holding `paykit_pay` that removes a sender from the deny list first raises it to the service time of its latest verified read plus the clock tolerance, and writes the unblock only once that write succeeds and not at all if it fails; for an unblock it didn't write, as its read of the lists shows it, it raises it to the same time: it records it in its copy of the lists together with a pending raise for that sender that survives restart and restore, holds that sender's one-time requests and its asks until the raise is applied, and takes its other entries under their own rules; and it raises it in the commit that applies the widening of a restore that isn't clean or of a `missed` item, to the time spec 7.2 gives. Never per message counted, and never later at an ask, so a copy that comes back unasked after the unblock finds the floor raised, or, under both holds, opens `manual_only` at the app under them, and is held from automatic payment at every own app by the shared post-loss hold (Unblocking under the divergence hold, above; spec 8.1, Blocked counterparties). After `409 conflict` it reads the record again and builds anew; until the raise is applied, the app holds that sender's one-time requests and asks nothing, while its other entries, such as period details, request acceptances, proofs, receipts, stops, Private Payment Lists and recurring requests, go on under their own rules (spec 8.1), and a widening whose raise isn't applied isn't applied either; the app tries the raise again, and applies a widening with it, as the retry rule below says: at its next receive after a try that meets `409 conflict` or a first try that leaves it unwritten, and after a second such try in a row once its wait ends. After a try that leaves the raise unwritten for a cause other than the divergence hold, the app tries it again at its next receive; after a second such try in a row, it waits before the next: one minute, doubling after each further such try, up to 10 minutes, the age the deny list may have before a payment the user starts (spec 8.1, How fresh the deny list is). Whatever the wait, it tries again at its next receive once the floor record it holds changes or the floor becomes readable, its payer records listed or the scope key it waits for held. A `409 conflict` neither starts nor ends a wait: the app has read the newer record and builds anew at its next receive. While the raise waits, the app reads neither the lists nor the floor for it, and tells its host. A raise the divergence hold keeps from being written is not tried again while that hold stands alone; it goes without the raise once the shared post-loss hold stands too (Unblocking under the divergence hold, above). While it owes a raise that isn't applied, an unblock's or a widening's, the app judges every one-time request against that raise's time as well, when it opens the request's record and before each automatic handoff and repeat, so one dated before it is paid only by hand at that app (spec 8.1). Under both holds the app applies the unblock and the widening without the raise (above).

Vectors: `identifiers.json` `policies` holds the permanent `block_floor` policy and its hash, and `paykit-lib` checks the value, the seen floor and the identity binding under that policy; the wallet checks are SDK runtime cases (`conformance.md`, Block floor). The SDK runtime cases cover the shared post-loss hold too, and unblocks applied without the raise under both holds.

### 5.7 `settlements`

The identity's settlement marks (spec 6, Settlement marks), in at most 1,024 bucket records, one per bucket (1.5): each holds the marks of the settlements of its bucket that any own payee app has credited, labelled or not, under one permanent policy, so the service refuses a bucket's delete (5.1).

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `marks` | b64 | yes | the bucket's marks, 21 bytes each, concatenated in ascending byte order: the settlement's tag (12 bytes), the occurrence's kind (1 byte) and the occurrence's tag (8 bytes) (1.5); at most 9,000 marks |

Buckets carry no `lost_to_delete`: a writer never originates it, a reader ignores it, and a rewrite keeps it as it keeps any member it doesn't know (Section 5; Dispatch wire 1.1).

**Kinds:** `1` a one-time request, `2` a Billing Period, `3` a payment without a request. A bucket whose `marks` isn't a whole number of marks, holds more than 9,000, isn't in strictly ascending order of settlement tag, so names a settlement twice, or holds a mark of another kind, which names an occurrence no writer produces, is invalid; so is a bucket with no mark, since a bucket is first written with the credit that adds its first mark and no mark is ever removed. The marks name no counterparty, amount, label or settlement ID, since every payee app reads every bucket (spec 3.1, 12).

**Writing** (spec 8.1 step 4): one batch writes the settlement bucket with every retained mark and the new mark, writes the signed receipt into its verified `payee_activity` record and, when the payee chooses to send it, queues the receipt in that batch. A stored-only receipt has no delivery queued, including after restart or restore. For a bucket read absent at the current service the put carries `expected_version: {0, 0}` and its `id_inputs`. For a current bucket the put names its verified version. Within one uninterrupted provider history the service cannot delete a permanent bucket. Absence after a provider loss leaves the independent prior-receipt, seen-mark and provider-loss checks in force (spec 6 and 8.1 step 4). For a recovery upload the batch also checks its exact current application with `record_current` (Dispatch wire 6.2 and 6.3). The activity and, for a Billing Period, its parent must still match the writes used to verify the proof. A conflicting batch applies none of these effects. The app reads the bucket and occurrence again and repeats the full credit decision, including method verification. A mark of this settlement prevents another credit. A mark of another settlement stays in the bucket when the app retries. Only an applied batch completes the credit.

**Reading:** before a credit, the app reads the settlement's bucket at its Dispatch (Dispatch wire 7.4.9), never deciding from its copy or a backup alone. The settlement is credited when the bucket holds its mark or the app has seen one, whatever its user confirms. An absent bucket supplies no mark and leaves the rollback, receipt and provider-loss checks of spec 6, Settlement marks, and spec 8.1 step 4 in force.

While the bucket's current write doesn't open or read, is invalid or divergent, or the read fails, the settlement may already be credited, and the app credits it only by hand once its user has been shown that it may be (`CreditNeedsUser`, spec 8.1 step 4).

**Seen marks** (Dispatch spec 8.2): a reader that has seen a mark ignores, and shows with its writer, a later write that drops or changes it; it keeps every mark it has seen. A full write that keeps every mark of the write it was built on drops nothing, though it lacks seen marks that didn't fit (Over capacity), and isn't shown. A reader holding a seen mark that a valid, opening current write lacks writes the bucket again at once, at that write's version, with the current marks and every seen mark. An invalid or unopened bucket stays as Repairing says.

Every rewrite and merge follows the same mark tie rule: where the marks it would write name one settlement with two occurrences, it keeps the lower mark in byte order and shows its user that the settlement may be credited twice. A reader takes a write replacing a seen mark with a lower mark of the same settlement as no change, and the higher mark still counts as seen. More than 9,000 marks follow Over capacity.

**Repairing:** a write over a divergent bucket holds every mark of its current write and every mark the app has seen in it; marks only an upload holds, which no app reads, are covered by the provider-loss rule of spec 8.1 step 4. Any other write over an invalid bucket, or one that doesn't open for good, is made only by hand, at its version, with every mark the app has seen, once its user has been shown that marks may be lost. Neither write holds more than 9,000 marks (Over capacity). A credit by hand over a bucket it can't write stores its receipt without a mark, and is complete only as Complete says.

**Full:** a bucket holds at most 9,000 marks, about 252,000 bytes of JSON, under the 262,140 bytes of the largest value (Dispatch wire 6.2). A credit whose bucket is full, or whose bucket write the service refuses for the storage quota or the value limit (`429` or `413`, Dispatch wire 7.6 and 9.2), is made only by hand, once its user has been shown that the settlement can't be marked and so may be credited again; it stores its receipt without a mark, complete only once that write applies (Complete), and the app tells its host once per bucket (`SettlementBucketFull`, spec 6). An app that credits only by hand for any other reason tells its host once per settlement (`CreditNeedsUser`, spec 8.1 step 4).

**Over capacity:** no write of a bucket, by hand or not, holds more than 9,000 marks. Where the marks a write would hold, each settlement counted once after the lower-mark rule (Seen marks), exceed 9,000, as a rewrite for a seen mark, a repair or a rewrite over another app's rewrite can when the apps' seen marks differ, that write isn't made: the app writes instead, at the same version, the marks it keeps of the current write, none over an invalid bucket, and as many of its other seen marks as fit in 9,000, in ascending byte order, so the bucket is full; when that write would hold just the current write's marks, nothing is written. Once a bucket holds 9,000 marks, no write adds a settlement to it. The app keeps every mark it has seen in its own state, those that didn't fit too, and tells its host once per bucket (`SettlementBucketFull`). The apps converge: each goes on taking every mark it has seen as a credit, whatever the bucket shows, so no app credits a settlement again whose mark it has seen; every rewrite only adds marks, or lowers one of a settlement (Seen marks), so the rewrites end, at 9,000 at the latest; and a settlement of a full bucket that the bucket holds no mark of, one whose mark didn't fit among them too, is credited by no app automatically, only by hand (Full).

**Complete:** a credit is complete, and shown and reported so, only once the batch that stores its receipt in its `payee_activity` record applies: with its mark in that batch, or, for a credit by hand over a bucket it can't write (Repairing and Full) or of a proof whose method gives no settlement ID, without one. A receipt-only batch for an identifiable settlement still checks the bucket version from its verified read, zero when absent, and its exact current application when it came from a recovery upload. Without that comparison the receipt waits. When the service refuses that batch for the storage quota or the value limit (`429 quota_exceeded` or `413 too_large`, Dispatch wire 7.6 and 9.2), also once the bucket's write is left out, the credit isn't made: no receipt is stored or sent, since the refused batch also held the receipt and any requested delivery; the proof stays pending, in its record without a receipt, or as a stored entry while its record write waits for room (spec 8.1, Stored entries); the app tells its host of the refusal, with the storage-capacity error the client returns; and once room frees it decides the credit again from a new read of the bucket if the payment method identifies the settlement, by hand again where it was by hand.

**Capacity:** tags are uniform, so the buckets fill evenly: the first is full at about 8.9 million marks, out of 9.2 million in all, in at most 1,024 live record keys. A mark takes 28 bytes of JSON. The `payee_activity` record each credit keeps for the identity's life reaches the storage quota (`record_quota_bytes`, 64 MiB by default, Dispatch wire 9.2) and the 100,000 live record keys long before the marks do; 100,000 marks take about 3 MB, and a credit then rewrites a bucket of about 3 KiB sealed.

**Collisions:** two settlements share a bucket and a tag with a chance of about n^2/2^107 for n marks, below 10^-18 for 10 million, and since the tags are blinded no payer can aim one. A collision only refuses a credit, which the user then sees; it never makes one. An occurrence tag only tells an app whether the credit it lost was of its own occurrence, and decides no credit.

Example (a bucket holding two marks, a one-time request's and a Billing Period's; stored in `sealed.json` as `settlement_marks.example`):

```json
{
  "v": 1,
  "marks": "fMwzZXxKU-r3TUswAdLhLnFb3qh7uLRaxJvmJjPm0NJwAoHqru5D8S_X"
}
```

`identifiers.json` gives the permanent policy hash and blinded bucket key. `sealed.json` `settlement_marks` gives the encoded marks, bound bucket values and sealed records. Selling-scope cases are in `documents.json` `payee_certificates`. The SDK tests in spec 18 exercise concurrent wallet writes and delivery.

### 5.8 `private_list_publications`

A publishing app's counter of the lists it sends one recipient in one generation of its identity (spec 8.4.1). For App ID A (16 decoded bytes), recipient R (32 decoded bytes) and generation g:

```text
natural_key = lp(A, R, u64(g))
policy = { read_scope: "paykit_request",
           write_scopes: ["paykit_request"],
           permanent: { generation: g } }
```

Derive the record key as in Section 1.5 with the permanent policy above. Both reads and writes require plain `paykit_request` in the app certificate. The policy and key have no labelled variant. The service accepts its creation only in generation g and refuses deletion until the account has passed g (Pubky Dispatch spec, Section 10.3.1). The natural key stays out of the cleartext policy and metadata.

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `publisher` | app-id | yes | A |
| `recipient` | pubky | yes | R |
| `generation` | int | yes | g, from 1 through 9007199254740991 |
| `counter` | int | yes | the counter of the newest list reserved, from 1 through 9007199254740991: that list's `version` is `{g, counter}` (4.13) |
| `msg_id` | uuid | yes | that list message's `msg_id` |
| `message_hash` | `b64(32)` | yes | the message hash (1.5) of that list |

**Binding** (Dispatch spec 8.2). The reader verifies the signed `record.put` and requires its authenticated writer App ID to equal `publisher`. It recomputes the record key with its naming key, the policy above and the value's `publisher`, `recipient` and `generation`. Both checks must pass before the value supplies a counter, a retained floor or a recipient for a restored withdrawal. A wrong writer keeps publication at that key on hold and is reported to the host, including on a restore without a prior observation (spec 8.4.1). Unknown members follow Dispatch wire 1.1.

**Seen counters:** retain only counters bound to the authenticated publisher above. A later bound write whose `counter` is lower than the held counter, or equal with another `msg_id` or `message_hash`, is reported and leaves the held floor intact (spec 8.4.1).

`identifiers.json` gives the permanent policy, its hash and the derived record key. In `sealed.json`, `private_list_publications` gives the signed record writes, their accepted or refused bindings and the later counter comparisons. The held value refers to the signed `private_payment_list` in `messages.json`.

### 5.9 Acceptance batch and initial ledger

The accepting and proposing parties each use one atomic batch on their own Dispatch when recording the original signed proposal P and acceptance Q (spec 8.3.1). Under the fixed policies of Section 5.1, the permanent records use a `record.put` with signed `expected_version` of `{0, 0}`. The service must hold nothing at each create key when applying the batch.

- The Allower creates the permanent `allowances` record containing P and Q, creates the permanent `allowance_ledgers` record, and deletes its current `allowance_proposals` record.
- The Allowee creates its permanent `allowances` record containing P and Q and deletes its current `allowance_proposals` record.

The delete names the exact version of P. A pending write from a recovery upload also requires `record_current` for its exact application. Both parties retain the original parties and Allowance ID. The first ledger write is a sealed non-delete with exactly the following UTF-8 bytes, substituting the encoded Allowee identity, Allowance ID and original Q hash.

```json
{"v":1,"counterparty":"<allowee pubky>","allowance_id":"<same UUID>","acceptance":"<hash of Q>"}
```

The initial value has no whitespace or additional members. Missing `entries` and `totals` in this value mean zero usage. The app placing Q verifies its own complete original batch, including the delete of its exact selected proposal, before delivering the answer. A Q received from the other account authenticates that sender's answer. A later copy of the zero, an upload or a missing ledger cannot establish an original application.

On the Allower's account the approving app is the authenticated sender of Q when the Allowee proposed, or of P when the Allower proposed. Before submitting the batch it durably retains its user's decision for this App ID, counterparty, Allowance ID, signed P hash, terms and Allower generation, together with its signed P or Q and the matching counterparty message. It also retains the selected signed pending record write, its writer certificate and its authenticated service application. It opens the write at the derived pending key, verifies its version and item hash against that application and verifies that it contains this exact signed P without a conflict. A pending stop or a prior rejection, withdrawal or discard precludes acceptance. The app retains its exact three prepared signed operations and their conditions so it can resolve an unanswered submission on reopen.

After application the app durably records all three operations, their authorized writer certificate and their ordered authenticated service proofs before retiring its pending submission. If the answer or the local commit is lost, it resolves that same submission. Until it can authenticate and record the full result, it holds the grant. It verifies the writer is its own App ID in the same batch and uninterrupted account generation. Each proof must bind that operation's item hash and resulting application. In order, the first two create the permanent P/Q at its derived key and the exact Q-bound zero at its ledger key, both from zero predecessors. The third must open as a deletion at the derived pending key for these parties and Allowance ID. Its signed expected version must match the proved selected pending write. Its proof must identify the delete immediately after both creates. If the selected write came from a recovery upload, the signed batch must also condition on that exact installed application through `record_current`. The app verifies the certificates' standing at the signed operations, including any removal cutoff. A renewed certificate for the same App ID can establish current authority independently.

Before writing the first grant, the same app verifies that it still holds executor authority, its matching local consent and the complete original evidence above. For an Allowee-proposed P the decision must match the approving app's own signed Q as well. It reads both permanent applications and the pending key with current authority. It conditions its grant on the two exact unchanged original permanent applications and the pending disposition. A retained original deletion requires `record_current` with that deletion's exact application and `deleted: true`. After collection, qualified current absence permits `record_version` zero only with the retained proof of the selected pending write and its original deletion. A live proposal, a different delete, a local conflict or stop, or uncertain evidence holds the grant. A concurrent change prevents this grant from applying.

The app durably records the applied grant before using the original ledger as its checkpoint. Its protected backup and reopened state must preserve the consent, original evidence and any unresolved result together with the existing grant and ledger state. A restored instance of the same App ID verifies its evidence and reconciles current service state before proceeding. It resolves an outstanding original grant without issuing a fresh grant on uncertainty. An earlier grant cannot initialize another zero. A different App ID, missing historical operands or proofs, loss of continuity or incomplete spending history leaves automatic payment held (spec 7.2 and 8.3).

### 5.10 Recovery reports (`recovery_uploads`)

In the generated vectors, the `recovery_uploads` case in `identifiers.json` gives the policy hash for the dedicated read and write scope. The `recovery_report` case in `signatures.json` gives the commitment preimages, the signed bytes, the original app’s signature and the rejection cases.

The identity-signed opening of the recovered generation supplies the account, its original App IDs, the lost generation (`recovery_from`), the recovered generation and its opening service key. Use the opening document’s authenticated hash for the record address in Section 1 and the ordinary fixed policy in 5.1. Count an explicit identity-signed removal only for the loss it covers.

The sealed JSON object has six required members. Read `v` as an unsigned JSON integer and require the value `1` before interpreting the record.

| Member | Value |
|---|---|
| `v` | `1`, the record schema version |
| `cutoff` | Base64url of the 32-byte cutoff digest below |
| `inventory` | Base64url of the 32-byte inventory digest below |
| `dispositions` | Base64url of the 32-byte dispositions digest below |
| `signature` | Base64url of the 64-byte Ed25519 signature made by the original app |
| `author_certificate` | The original app’s identity-signed Dispatch App Certificate, in its signed-document form |

**Disclosed evidence.** Use `lp` from Dispatch wire 1.4, with a four-byte unsigned big-endian length before each field, and `H` from Dispatch wire 1.5 (unkeyed BLAKE3, 32 output bytes). Quoted literals are their exact ASCII bytes. `u64` is eight unsigned big-endian bytes. An identifier or hash contributes its decoded bytes. `set(rows)` is `lp` of the complete encoded rows sorted in unsigned bytewise lexicographic order. The empty set is zero bytes. Reject duplicate rows. To check an encoded disclosure, require this ordering and consume every byte. To build one from typed inputs, sort before encoding. Use the exact disclosed bytes for signed bodies and local values. Re-serializing a parsed JSON value can change those bytes.

A witness is one of these encoded rows. Select the tag from the underlying evidence type.

| Evidence | Encoded row |
|---|---|
| Identity document, app certificate, signed operation, or service transition proof | `lp(tag, doc_bytes)` with tag `"identity"`, `"certificate"`, `"operation"`, or `"proof"` respectively. Dispatch wire 2.2 defines `doc_bytes = lp(exact signed body bytes, raw 64-byte signature)`. |
| Signed service head | `lp("head", u64(chain_version), u64(generation), u64(seq), chain_hash, sig)` using the 32-byte hash and 64-byte signature from Dispatch wire 6.7. |
| Local checkpoint or retained local evidence | `lp("local", scheme, key, value)`. `scheme` names the author’s format as `name/version`. The name is one or more lowercase ASCII letters, digits, dots or hyphens and the version is a positive decimal integer without leading zeroes (for example, `org.example.wallet/1`). `key` is the nonempty raw locator in that format and `value` is the exact retained byte string. A verifier can match these bytes across implementations. Interpreting their contents requires the named format. |

The subject of an inventory entry is `lp(kind, key)` for `"record"` (32-byte Dispatch record key), `"claim"` (32-byte Paykit obligation ID) or `"settlement"` (full 32-byte settlement tag from Section 1.5). A duty with only a local locator uses `lp("local", scheme, key)` with the same rules as a local witness. Use the same subject bytes in its disposition.

| Digest | Typed preimage for `H` |
|---|---|
| `cutoff` | `lp("paykit/v1/recovery-cutoff", set(anchors))`. Include the identity documents, verified old service heads or local checkpoints that delimit this original’s scan. Require at least one anchor. A local checkpoint used to claim an uninterrupted scan must have the same value before and after enumeration. |
| `inventory` | `lp("paykit/v1/recovery-inventory", cutoff, set(items))`. Each item is `lp(subject, set(witnesses))` for the records, claims, settlements and local duties the scan found. Keep a subject with incomplete evidence in the inventory. |
| `dispositions` | `lp("paykit/v1/recovery-dispositions", inventory, set(decisions))`. Each decision is `lp(subject, state, set(witnesses))`. `state` is `"held"` for unresolved work, `"represented"` for evidence the original found at the recovered service, `"transferred"` for protective custody accepted by another holder, or `"terminal"` for a final outcome supported by the witnesses. Require supporting witnesses for every state except `"held"`. |

Require distinct subjects in each set of items or decisions and exactly one decision for every inventory subject. Recompute the digests in table order and compare all three to the report. An empty inventory has an empty set of decisions. A digest match establishes which evidence and decisions the original signed. Verifying an operation, its historical authority, the completeness of the scan or the claimed outcome requires the underlying evidence and the rules for that item. An omission, a declared terminal state or a later relay cannot by itself release a payment or credit.

The app signs under `paykit/v1/recovery-upload` with **one field**. That field is `lp(u64(1), identity, original App ID, u64(lost generation), u64(recovered generation), opening service key, opening identity document hash, cutoff, inventory, dispositions)`. The first integer is the checked outer `v`. The identity, service key, document hash and commitments are raw 32-byte strings. The App ID is 16 bytes. Signature framing follows Section 1 and Dispatch wire 2.1. The original can give its report to a different app for publication. The inner signature covers this version, loss context and these three digests. The authenticated service write covers the sealed value, including the presented certificate and any other members. A reader holding only the report can check the origin and the committed digests. Disclosed inputs let it recompute those digests.

The reader needs a current recovery grant, its scope key, and the verified opening for the loss it follows. It verifies the current applied write at the derived key, the fixed policy, and the publishing app’s signing certificate and `paykit_recovery` grant. Independently it verifies `author_certificate` under the opening’s identity, checks that its App ID is the addressed opening member, and verifies `signature` under that certificate’s signing key. Each certificate must count in the authenticated identity directory at the write’s accepted time and be valid at that time. These checks also apply when the publisher is the original. Readers ignore unknown members. Rewriting a value preserves unknown members as in Section 5.

The exact service application identifies a publication of this report. A fresh application of the same signed report identifies its republication. Neither application establishes that the original’s historical commitments still cover every relevant action. An absent, deleted or unverified report supplies no commitments. An unreadable collection, an unconfirmed opening or a changed loss cannot supply an authoritative roster. Reading reports authorizes no payment or credit (spec 7.3).

## Appendix A. Choices this document made

Use these choices when serializing and validating the relevant fields.

| # | Detail | Choice |
|---|---|--- |
| A1 | Time comparisons | as instants at millisecond precision (Dispatch wire 1.6); period details ordered by `created_at` instant, then `msg_id` text (4.9) |
| A2 | Amount bounds | at most 18 fraction digits and value times 10^18 below 2^128, integer part of at most 20 digits advised; rates above zero (1.2) |
| A3 | `accepted_payment_endpoint_identifiers` | optional, since the destination already limits a request to the payee's own endpoints (spec 8.1), and an app that receives no payments can't know which identifiers the identity's other apps publish; narrows the identifiers, and holds every key of `payment_endpoints` when both are present (4.5) |
| A4 | `payment_reference` in a proof | optional; when present it equals the request's (4.10) |
| A5 | Period details | at least one of `payment_endpoints` and `rates`; v0.1's `valid_from` is `created_at` cut to whole seconds; each valid until its own `expires_at`, at most 8 per period (4.9) |
| A6 | `resend_request` selectors | `request_hashes` and `billing_period` added for the spec's "by request or Billing Period"; at least one selector; both range ends inclusive, an absent end open (4.12) |
| A7 | `reason` on rejection and cancellation | kept from v0.1, optional (4.7, 4.8) |
| A8 | Allowance terms and unknown members | ignored, as everywhere in v0.2; a later restriction needs a new `v` or kind (4.15) |
| A9 | Record expiry | optional for public endpoints, earliest protocol deadline or `null`, always subject to the identity’s record age limit (3.2); Private Payment Lists 30 days (4.13); renewed at the next run once less than half the lifetime remains |
| A10 | How many `msg_id`s a record keeps | those of the messages it stores and the 16 newest distinct others, each once; readers read a list in that form (5.3, 5.4) |
| A11 | Re-send limits | one message to one requester identity at most once an hour; a daily number of messages per requester identity the sender chooses (4.12) |
| A12 | What a compacted `failed` entry keeps | its `obligation`, `attempt_id` and `value` in `totals.failed`, so the executor can read its claim again and add a later `done` to the totals once (5.5) |
| A13 | Record age limit | `record_max_age` in whole seconds, at least 1, 90 days when absent; an `updated_at` more than 5 minutes ahead is refused (3.2, Dispatch wire 3.3) |
| A14 | Extension scopes and kinds | `paykit_ext:<prefix>`, prefix `paykit-` and 1 to 25 more of `a-z`, `0-9`, `-` (so its kinds are Paykit's, Dispatch wire 3.1 "Scope roots"); kind `<prefix>.<name>`, name 1 to 64 of `a-z`, `0-9`, `_`; at most 8 extension scopes per certificate; they give `reply.add` |
| A15 | Removed | Paykit defines no settlement ID: the payment method does (1.5) |
| A16 | Settlement marks | derived from the settlement ID alone, never the endpoint; 1,024 buckets by the first 10 bits of a blinded tag, a 21-byte mark (12-byte settlement tag, kind, 8-byte occurrence tag), at most 9,000 per bucket; written in the credit's batch at the bucket's version; the scope `paykit_settlements`, which every payee scope requires (1.5, 3.1, 5.7) |

The vector files follow these choices.
