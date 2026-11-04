# Paykit v0.2 wire format (Path A)

**Status.** Complete; a review candidate, with the spec. The wire format of `specs/v0.2/spec.md` ("the spec"); a clean-room implementation written from this document alone reproduces every test vector; that implementation and its report are shared separately. The spec decides behaviour; this document decides bytes. It settles every wire detail the spec leaves open, taking the phase 2 prototype, the reference prototype, as the source of truth, so that a second team can build an interoperable implementation without reading the prototype. Where this document had to choose something the prototype doesn't fix, Appendix A says so.

**Test vectors.** `specs/v0.2/vectors/` holds shared test vectors for every encoding, signature, derivation, envelope and state transition below; its `README.md` lists the files. Every example in this document comes from those files and uses the fixed keys of `vectors/keys.json` (identities alice and bob, alice's apps `wallet`, `phone` and `executor`, bob's apps `shop` and `cap`), except the `429 quota_exceeded` body of 10.3, which is the prototype's answer and says so. `cargo test -p paykit-v02 --test vectors` recomputes every vector from the prototype's code.

**Review history.** The review records, shared separately, note where each review finding and clean-room gap was settled. Where the spec is silent, Appendix A records the choice this document made.

**Conventions.**
- "Section N" means this document; "spec N" means the v0.2 specification.
- Field tables have the columns Field, Type, Req (required), Constraints and Notes.
- Types:

  | Type | Meaning |
  |---|---|
  | `int` | JSON integer, 0 to 2^53 - 1 (1.1) |
  | `string`, `bool`, `object`, `array<T>` | JSON types |
  | `b64(n)` | base64url of exactly n bytes (1.2); `b64` is any length |
  | `pubky` | Pubky ID (1.3) |
  | `app-id` | App ID (1.3) |
  | `time` | RFC 3339 time (1.6) |
  | `amount` | decimal amount string (1.7) |
  | `uuid` | UUID v4 string (1.8) |
  | `signed<T>` | a signed document whose body is a T (2.2) |
  | `T \| null` | nullable |

- **Absent and null.** "Req: no" means the member may be missing. Writers omit absent optional members, except where a table says the member is always written, and except in the shared record values of Section 6, whose writers may write every member with `null` for absent (the prototype does). Readers treat `null` like a missing member, unless the table says `null` has its own meaning.
- Examples of signed documents show the decoded body. Long base64url values are cut short with `...` in a few examples; the vector files have them in full.
- **Rejection vectors.** Where a vector file lists inputs to refuse, what is normative is that the input is refused, and the HTTP status and `code` where this document fixes one. An `error` or `message` text in a vector, such as `chain.json`'s "incomplete batch: <id>", is the prototype's and only informative.

## 1. Encodings

### 1.1 JSON
- **Text.** Every JSON document is UTF-8 without a byte order mark, and is exactly one JSON value. JSON whitespace may follow the value; anything else after it makes the document invalid.
- **Duplicate member names** at any depth make a document invalid. Names are compared after unescaping, so `{"a":1,"\u0061":2}` is a duplicate. The service answers `400 bad_request`; apps drop the document.
- **Lone surrogates** (`\ud800` not followed by a low surrogate, or a low surrogate alone) make a document invalid.
- **Unknown members** are rejected in conditions (7.3), operation bodies (7.1, 7.2), app certificates (3.1), receive key records (3.4), app requests (3.10), export transfers (3.12) and the erasure body (8.4.16), so a future restriction there is never silently skipped (spec 14.1): the service answers `400 bad_request`, and a reader drops the document. This holds at every depth: an object nested in one of these refuses unknown members as its top level does, such as an operation's `batch`, `expected_version`, `value_sealed` and `id_inputs`, a condition and its inner object, the carrying form (2.2) of a signed document an operation carries (`record`, `cert`), the key bundle object of `key_bundle.put` (3.5), each `suite_keys` entry of a certificate or a receive key record (3.1, 3.4), and an export transfer's `from` and `to`. So does each entry of a `POST /ops` body (7.4): a signed operation's carrying form has `body` and `sig` only, and a `cursor.set` has `op` and `position` only. Only opaque members are exempt: the `context` of `invite_code.put` holds any JSON. A signed document carried inside keeps its own rule: a certificate refuses unknown members, an app record ignores them. Everywhere else readers ignore them: message bodies, records, `identity.json`, the key bundle plaintext, shared record values and export parts. See 5.15 for the one place where ignoring needs care. Vectors: `operations.json` `unknown_members`, one top-level case and nested ones, and in `batch_entries` members beside a batch entry's own.
- **Version.** Every document with a `v` member has `v: 1`. Readers reject other values.
- **Exact bytes.** Signatures and hashes cover the exact bytes a writer produced. Readers never re-serialize JSON to check a signature or a hash, and anything forwarded keeps its bytes: a signed document's `body`, a message's JSON inside its plaintext, a chain envelope. Writers may use any member order and whitespace. The prototype writes compact JSON; its message bodies have their members sorted by name.
- **Integers** are JSON numbers without sign, fraction or exponent, from 0 to 2^53 - 1 (9007199254740991). Every binding reads such a number exactly. Readers reject a larger or negative value, and a fraction or exponent, `1.0` and `1e3` included. The rule runs on the whole document when it is parsed, before any of its fields is read, and covers every number outside the opaque members below, in members readers ignore too: the service answers `400 bad_request`, and an app drops the document. Paykit documents carry no other numbers. With integers in this range, a duration added to a time in milliseconds (an `orphan_after` of 2^53 - 1 seconds, say) can't overflow 64 bits; a sum past the last time 1.6 can write means "never". Inside `lp()` an integer is 8 bytes big-endian (1.4). Times are never integers on the wire, except the Private Payment List `version` (Unix milliseconds), period starts inside derivations, and the `_ms` and `_s` members of export parts (Section 11).
- **Opaque members** are opaque to Paykit, which never reads them as numbers. They, and everything at any depth below them, may hold any JSON number. They are named by their path in the document that holds them:
  - a message body's `metadata` (5.1, 5.5);
  - the `proof` of a `paykit.payment_proof` body (5.10);
  - an invite code's `context`: in `invite_code.put` (7.2), in a conversation entry (the `conversation.add` transition of 7.5, the `conversations` of `GET /allow` in 8.4.6, and the export's `conversation` part in 11.3), and where a feed item (8.5) or an export `chain`, `entry` or `invite_code` part (11.3) repeats it.

  Every other member is checked, whatever its name: a `metadata` elsewhere in a message, or a `proof` holding a transition proof or a Stored message. An extension kind's body (5.3) is no exception: only its `metadata` is opaque. Vectors: `encoding.json` `json` (the cases with `read_as` are read as that document) and `operations.json` `integer_bounds`.
- **Strings** may use JSON escapes; comparisons use the decoded text. A field documented as "no control characters" rejects U+0000 to U+001F and U+007F.

### 1.2 Binary values
- Binary values in JSON and in headers are **base64url without padding** (RFC 4648 section 5).
- Decoders reject `=` padding, the characters `+` and `/`, whitespace, lengths of the form 4k + 1, and non-zero trailing bits. So each byte string has exactly one text.
- Lengths:

  | Bytes | Characters | Used for |
  |---|---|---|
  | 8 | 11 | kid |
  | 16 | 22 | `attempt_id`, batch ID, the nonces of enrollments, freezes and webhooks, `entry_id` |
  | 24 | 32 | XChaCha20-Poly1305 nonce of a sealed value |
  | 32 | 43 | keys, hashes, obligations, record keys, collection IDs |
  | 64 | 86 | Ed25519 signatures |

- Vectors: `encoding.json` `base64url`.

### 1.3 Pubky IDs and App IDs
- **z-base-32**, alphabet `ybndrfg8ejkmcpqxot1uwisza345h769`, bits taken most significant first, the last group filled with zero bits, no padding characters, lower case only.
- **Pubky ID:** the identity's 32-byte Ed25519 public key, 52 characters. The last character carries one bit, so it is `y` or `o`.
- **App ID:** the first 16 bytes of `BLAKE3(app signing public key)`, 26 characters.
- Decoders reject a wrong length, a character outside the alphabet (upper case included) and non-zero trailing bits, so each key has one ID.
- Vectors: `encoding.json` `zbase32`, `identifiers.json` `app_id`.

### 1.4 `lp()` and field encodings
`lp(x1, ..., xn) = u32_be(len(x1)) || x1 || ... || u32_be(len(xn)) || xn`. `lp()` of no parts is empty. Every signature input, derivation input, AAD and HPKE `info` uses it.

| Field | Bytes inside `lp` |
|---|---|
| Identity, counterparty, any Pubky ID | the 32-byte Ed25519 public key, decoded from z-base-32 |
| App ID | its 16 bytes |
| kid | its 8 bytes |
| Integer | `u64` big-endian, 8 bytes |
| Label, scope name, collection name, HTTP method, path and query, origin | UTF-8 text |
| `allowance_id`, `msg_id` | UTF-8 text of the UUID (36 bytes) |
| Request header values (`Paykit-Timestamp`, `Paykit-Identity`, `Paykit-App`, `Paykit-Cert`, `Paykit-Invite-Code`) | the exact header text, UTF-8, empty when the header is absent. The identity and app headers are signed as their z-base-32 text, not as key bytes |
| `Paykit-Server-Time` | its exact text |
| Hashes, obligations, record keys, keys | raw bytes |
| A document or message body | its exact bytes |
| An absent value | empty (length 0) |

Vectors: `encoding.json` `lp` and `u64_be`.

### 1.5 Hashes and `derive()`
- `hash(x)` is BLAKE3 with a 32-byte output.
- `derive(label, parts...) = BLAKE3-derive_key(context = label, key material = lp(parts...))`, 32 bytes. Every label is a fixed ASCII constant.
- Some values hash an `lp()` whose first part is a label, with plain BLAKE3: policy hashes, chain item hashes and chain hashes. Sealed values and HPKE put the label first in their `lp()` the same way.

| Value | Computation |
|---|---|
| kid | first 8 bytes of `derive("paykit/v1/kid", X25519 public key)` for a suite 1 key; for a key of a later suite `s` in `suite_keys` (3.1, 3.4), first 8 bytes of `derive("paykit/v1/kid", u64(s), key bytes)` (spec 14.3) |
| App ID | first 16 bytes of `hash(Ed25519 signing public key)` |
| Paykit root | `paykit_root = derive("paykit/v1/scope-root", identity secret)`, the identity secret being the 32-byte Ed25519 secret key (seed). Only the signer computes it; it is never granted or delivered |
| Scope key | `scope_key(e, s) = derive("paykit/v1/scope-key", paykit_root, u64(e), s)`, for data epoch `e` and scope name `s` |
| Label key | `label_key(e, label) = derive("paykit/v1/label-key", scope_key(e, "request"), label)`, the key of the labelled selling scope `request:<label>` (3.1) at data epoch `e`: its scope key wherever a key of that scope is meant (3.5, 7.2). An app holding `request` derives it; others get it in key bundles |
| Naming key | `naming_key = derive("paykit/v1/naming", paykit_root)`, one per identity; it never rotates |
| Blinded input | `blind(x) = derive("paykit/v1/name", naming_key, x)`, for the natural keys and obligation inputs below |
| Key commitment | `derive("paykit/v1/key-commit", content key)` |
| HPKE `info` | `lp("paykit/v1/hpke", recipient identity, kid)` (66 bytes) |
| Message hash (`refs`, request hash) | `hash(message JSON bytes)` |
| Terms hash | `hash(lp(proposal message JSON bytes, acceptance message JSON bytes))` |
| Document hash (certificate hash, freeze hash, enrollment hash) | `hash(signed document bytes)` (2.2) |
| Policy hash | `hash(lp("paykit/v1/policy", read_scope, u64(n), write_scope_1, ..., write_scope_n))`, write scopes sorted ascending by bytes, no duplicates, n at least 1 |
| Collection ID | `derive("paykit/v1/collection", collection name, identity)` |
| Record key | `derive("paykit/v1/record", collection name, blind(natural key), policy hash)`; the creating `record.put` carries `blind(natural key)` in `id_inputs` (7.2) |
| Obligation of a request | `derive("paykit/v1/obligation", "request", blind(payee identity), blind(request hash), u64(period start in Unix seconds, or 0 for a one-time request))` |
| Obligation of a payment without a request | `derive("paykit/v1/obligation", "payment", attempt ID (16 bytes))`; it names no payee, so a former holder of the naming key can't test guesses of the payee against it (spec 15, 16) |
| Chain item hash | `hash(lp("paykit/v1/item", item type, signed document bytes))` (7.6) |
| Chain hash | `hash(lp("paykit/v1/chain", previous chain hash, chain envelope bytes))` (7.6) |
| Invite code hash | `hash(code bytes)`, the code decoded from base64url (1.8) |

**Natural keys** (spec 14.3), each an `lp()`:

| Collection | Record | Natural key |
|---|---|---|
| `payer_activity`, `payee_activity` | request record | `lp(payee identity, request hash)` |
| `payer_activity`, `payee_activity` | period record | `lp(payee identity, request hash, u64(period start in Unix seconds))` |
| `payer_activity` | payment without a request | `lp(own identity, attempt ID)` |
| `payee_activity` | payment without a request | `lp(Payment Endpoint Identifier text, settlement ID)`, below |
| `payment_lists` | one per payee app | `lp(payee identity, publishing app ID (16 bytes))` |
| `allowances`, `allowance_ledgers` | one per Allowance | `lp(counterparty identity, allowance_id text)` |

- **Whose naming key.** Record keys and obligations are blinded with the naming key of the identity whose Pubky Switch holds them: a payer's records and claims with the payer's, a payee's records with the payee's. The obligation of a payment without a request is the exception: it isn't blinded, and is derived from the attempt ID alone (its row in the first table). Equal inputs give equal outputs within one identity, so all its apps find the same record and claim, and the stored keys name neither the counterparty nor the request. That protects stored state and exports, not against a live operator, which links records and claims to senders through batches and timing (spec 16).
- **Period start** in a derivation is the Billing Period's `starts_at` in Unix seconds, rounded down; periods start on whole seconds whenever the recurrence's `anchor` does, and an anchor with milliseconds is rounded down the same way.
- **Settlement ID** (spec 11.4): the bytes the payment method defines to name one settlement, the same in every proof of it, such as a Lightning payment hash or an on-chain transaction ID. The payee works it out from the proof's `proof` object and the `payment_endpoint_identifier` (5.10). Every proof of one payment then names one payee record, so a second one is a replay, credited once. A proof whose method gives no settlement ID isn't credited. Each proof `type` has one spelling of its settlement ID, so the same settlement written another way (an upper-case `txid`, say) can't name a second record. A proof whose members aren't in that spelling gives no settlement ID, so it isn't credited:

  | Proof `type` | Settlement ID |
  |---|---|
  | `bitcoin-txid` | the `txid` member, 64 lower-case hex characters, as its UTF-8 text (64 bytes). It has no output index: one transaction is one settlement to a Payment Endpoint, so a payer pays two payments without a request to one endpoint in two transactions. Other members, such as a `vout`, aren't part of it |
  | the prototype's fake rail (`{ "rail": "fake", "tx": ... }`, no `type`) | its `tx` text, not empty |
  | any other | defined by its payment method; a payee that doesn't know the method credits no payment without a request by it |

  The vectors' payment without a request uses `bitcoin-txid` (`identifiers.json`).

Vectors: `encoding.json` `derive`; `keys.json` the Paykit root and naming key of each identity, `scope_keys` and `label_keys`; `identifiers.json` `blinded_inputs` and every value of the table, with the blinded inputs of each obligation and record key.

### 1.6 Times
- **Grammar:** `YYYY-MM-DDTHH:MM:SS[.F]Z`, with a four-digit year, upper-case `T` and `Z`, and an optional fraction of 1 to 9 digits. Times are UTC. Times before 1970 and leap seconds (`:60`) are invalid.
- **Writers:**
  - every time in documents, messages, records and response bodies, and `Paykit-Server-Time`: exactly three fraction digits, for example `2026-01-01T00:00:00.000Z`;
  - `Paykit-Timestamp`: no fraction, for example `2026-01-01T00:05:00Z`.
- **Readers** accept every form of the grammar, fractions of 1 to 9 digits included, and reject a fraction of 0 or more than 9 digits and a leap second. The prototype's parser also accepts a space instead of `T` and numeric offsets such as `+01:00`; writers never produce them. They are a reader's choice, not part of the grammar, so a reader may reject them and loses nothing.
- **Comparisons** use the instant at millisecond precision; further digits are truncated, not rounded (`.9999` is 999 ms). Two spellings of one instant are equal, for example when a Billing Period is matched (spec 11.2). The spelling is still part of the signed bytes.
- Vectors: `encoding.json` `rfc3339`, with fractions of 1, 3, 4 and 9 digits and the lenient forms under `lenient_reader_only`.

### 1.7 Amounts
Amounts are decimal strings: `amount.value` of requests and proofs, every amount in Allowance terms, and the values and totals of Allowance ledgers.

- **Grammar** (v0.1 `PaymentAmount.value`): `[0-9]+(\.[0-9]*)?` or `\.[0-9]+`. ASCII digits, at most one `.`, at least one digit. No sign, exponent, grouping separator or whitespace. `.5`, `10.` and leading or trailing zeros are valid.
- **Bounds:** at most 18 digits after the `.`, and the value times 10^18 below 2^128. The largest valid value is `340282366920938463463.374607431768211455`. Writers should keep the integer part to 20 digits.
- **Arithmetic** is exact: implementations compare and add amounts as integers scaled by 10^18 (the prototype uses `u128`). An addition that overflows makes an Allowance evaluation fail closed (spec 11.3). The original spelling is kept and signed; two spellings of one value are equal when compared.
- **Computed amounts** (an Allowance ledger's `totals.spent`, 6.5) are written in the normalized spelling of `encoding.json` `amounts`: no leading zeros before a non-zero integer digit, no trailing fraction zeros, no `.` without a fraction, and `0` for zero. A ledger that was never compacted carries `totals.spent` as `""`, which reads as zero; compaction writes the normalized sum, `0` when nothing was paid.
- **Asset** (`asset`): non-empty, no control characters, compared exactly and case-sensitively. By convention it is the lower-case asset segment of the Payment Endpoint Identifiers, such as `btc`, `usd` or `usdt`.
- **Scale:** an amount is in whole units of its asset (`0.00025` `btc`, not satoshis). Paykit fixes no per-asset scale. A payment method rounds up to its endpoint's smallest unit, as `specs/payment-conversion.md` says; 18 fraction digits cover every common asset.
- **Rates** (`conversion.rates`, period details): the same grammar, and the value must be above zero.
- Vectors: `encoding.json` `amounts`, with the normalized value of each valid amount.

### 1.8 Identifiers

| Value | Form | Length | Notes |
|---|---|---|---|
| Pubky ID | z-base-32 | 52 | 1.3 |
| App ID | z-base-32 | 26 | 1.3 |
| kid | `b64(8)` | 11 | 1.5 |
| Obligation, record key, collection ID, hashes | `b64(32)` | 43 | 1.5 |
| `msg_id`, `allowance_id` | UUID v4 | 36 | lower-case hex, `8-4-4-4-12`, version nibble 4, variant bits `10`; a MUST: readers reject upper case and other versions |
| `attempt_id` | `b64(16)` | 22 | random |
| Batch ID | `b64(16)` | 22 | random, 7.4 |
| Nonces of enrollments, freezes and webhooks | `b64(16)` | 22 | random |
| `entry_id` | `b64(16)` | 22 | random, opaque to senders |
| Invite code | base64url of at least 16 random bytes | 22 or more | shown as `pubky://<id>?paykit=invite&code=<code>` |
| Invite code hash | `b64(32)` | 43 | `hash(code bytes)` |
| Push handle | string | any | chosen by the relay; the reference relay uses `b64(16)` |
| Push key | `b64(32)` | 43 | the public half of a registration's push key (9.1) |
| Feed position, cursor | `<generation>:<seq>` | | two decimal integers, for example `1:1042` |
| Record version | `{ "generation": int, "version": int }` | | the (generation, `seq`) of the write that produced it (7.2); `{0, 0}` names no record |
| Service time | `time` | | three fraction digits |

### 1.9 Payment Endpoint Identifiers and payloads
- A **Payment Endpoint Identifier** is a string as `specs/payment-endpoint-identifier.md` defines it: ASCII letters and digits, `-`, `_` and `.`, at most 64 characters, not `.`, `..` or `private`. The recommended form is three lower-case segments, `<asset>-<rail>-<format>`, such as `btc-lightning-bolt11`. Requests with conversion terms must use that form.
- A **Payment Endpoint Payload** is a non-empty UTF-8 string. Paykit never interprets it.

## 2. Signatures and signed documents

### 2.1 Algorithm
- Ed25519 (RFC 8032). Public keys are 32 bytes, signatures 64 bytes.
- **Verification** (spec 14.2). With A the 32-byte public key, R the signature's first 32 bytes, S its last 32 read as a little-endian integer, M the signature input below, B the base point and L the group order (2^252 + 27742317777372353535851937790883648493), a verifier accepts only if every step holds, in this order:
  1. S < L.
  2. A decodes to a curve point (RFC 8032 5.1.3) whose encoding (5.1.2) is again exactly A's 32 bytes; the same for R. This refuses the encodings y + p of a point, which some decoders reduce silently (ed25519-dalek 3's `VerifyingKey::from_bytes` takes them for y = 3, 4, 5, 6, 9, 10, 14, 15, 16 and 18), and a set sign bit with x = 0.
  3. Neither A nor R is of small order: `[8]A` and `[8]R` aren't the identity point.
  4. `k = SHA-512(R || A || M)`, read as a 64-byte little-endian integer and reduced mod L, and the encoding of `[S]B - [k]A` equals R's 32 bytes (cofactorless, compared as bytes).

  A mixed-order A (order 8L) is accepted: nothing requires it to be torsion-free. A verifier that multiplies by the cofactor (`[8][S]B = [8]R + [8][k]A`), or skips step 2 or 3, isn't compliant, since it accepts signatures this one refuses. ed25519-dalek's `verify_strict` does steps 1, 3 and 4 and step 2 for R only; the prototype adds the check of A (`wire::verify`).
- **Vectors:** `signatures.json` `rejections` has signatures a verifier must refuse: S not reduced (S + L), a small-order public key (the identity point) with R the identity point and S = 0 (which a non-strict verifier accepts for any message), a signature checked under another label, and the edge cases built from known scalars and the points of order 8 (in the manner of "Taming the many EdDSAs", Chalkias et al. 2020): the cofactored equation only (a mixed-order key with k = 1 mod 8), a small-order R for which the cofactorless equation holds (ed25519-dalek's non-strict `verify` takes it), the identity point written as y + p as R, the point y = 3 written as y + p as A, and a public key of order 8. Each says, in `reason`, the step that refuses it, as the prototype words it. `accepted` has the mixed-order key that step 3 lets through, with k a multiple of 8 so both equations hold. Every case is checked under one label; the label doesn't change which step refuses it.
- **Signature input:** `lp(label, field1, ..., fieldn)`, with the label and fields of 2.3. Nothing else is signed, and nothing is canonicalized.
- **Sign once** (spec 14.2). A writer signs each document, operation and message once, keeps the signed bytes and sends exactly those bytes again: on a retry, in a re-delivery, in a later batch's answer check, when publishing what it delivered. It never signs the same body again or re-serializes it. Batch retries (7.4), the no-ops (7.4), `PUT /identity` retries and compare-and-swap (8.4.2) and every document hash compare whole signed bytes, so a second signature of one body is another document there; only equivocation compares bodies (3.3). Deterministic RFC 8032 signing is recommended, not required: a signer with random nonces is compliant as long as it keeps the bytes. The prototype keeps a batch's signed operations with the batch (`client::Batch`), the signed `identity.json` of each version (`Signer::signed_doc`) and each receive key's signed record with the key, and its conformance test signs with random nonces throughout.
- **What the identity key signs** (spec 14.2): only the identity-key rows of 2.3 and Pubky's own formats for the same key. Every input of 2.3 starts with `00 00 00` and the label's length, then `paykit/v1/`; Pubky's auth tokens start their signed bytes with `PUBKY:AUTH`, its grants are JWS (base64url text) and PKARR packets start with `3:seq`, so no input of one is an input of another. Signers expose no signing of caller-supplied bytes.

### 2.2 Signed documents
- **Carrying form:** a JSON object `{ "body": "<base64url of the exact JSON bytes>", "sig": "<base64url of the signature>" }`. Readers ignore other members, except in a carrying form inside an operation body or as an entry of a `POST /ops` body (7.4), which has these two only (1.1).
- **Compact form:** `<body>.<sig>`, the two base64url texts joined by one `.`. It is used where a signed document travels in a header (`Paykit-Cert`). A reader splits at the first `.`.
- **Signed document bytes:** `lp(body bytes, signature bytes)`, that is `u32_be(len(body)) || body || 00 00 00 40 || signature`.
- **Document hash:** `hash(signed document bytes)`. It is the certificate hash in `identity.json`, the freeze hash, the enrollment hash and the item bytes of chain items (7.6). It never covers re-serialized JSON.
- **Reading:** verify the signature first, then parse the body strictly (1.1). For an app record, the key is inside the body: a reader parses the body only to find `signing_key`, verifies, and then checks the certificate (3.2).
- Vectors: `documents.json`, with the body, both forms, the document bytes and the hash of every document of Section 3.

### 2.3 Signature labels and inputs
Every signature of v0.2. Fields are encoded as in 1.4.

| Object | Label | Signed by | Fields after the label | Where the signature travels |
|---|---|---|---|---|
| App certificate | `paykit/v1/app-cert` | identity key | body | signed document |
| App record | `paykit/v1/app-record` | app signing key | body | signed document |
| Receive key record | `paykit/v1/receive-key` | app signing key | body | signed document |
| `identity.json` | `paykit/v1/identity` | identity key | body | signed document |
| Enrollment | `paykit/v1/enroll` | identity key | body | signed document |
| App request | `paykit/v1/app-request` | the requesting app's signing key | body | signed document (3.10) |
| Key bundle | `paykit/v1/key-bundle` | identity key | identity, target app ID, envelope bytes | `sig` of the key bundle (3.5) |
| Signer hand-over | `paykit/v1/signer-handover` | identity key | identity, the new signer's App ID, `u32_be(n)`, the bytes of each of the n envelopes, in order | `sig` of the hand-over (3.13) |
| Executor grant | `paykit/v1/executor-grant` | executor app signing key | identity, counterparty identity, `allowance_id`, terms hash, `u64(confirmed_generation)`, `u64(generation)` and `u64(seq)` of the Allowance record version its write builds on (0 and 0 for a new record) | `sig` of the grant (3.6) |
| Freeze | `paykit/v1/freeze` | identity key | body | signed document |
| Unfreeze | `paykit/v1/unfreeze` | identity key | identity, freeze hash | `sig` of the unfreeze (3.7) |
| Import discarded | `paykit/v1/discarded` | the next service's key, the one the freeze names | identity, freeze hash | `sig` of the discard answer (3.7) |
| Export transfer | `paykit/v1/export-transfer` | identity key | body | signed document (3.12) |
| Next service's export request | `paykit/v1/transfer-request` | the next service's key, the one the export transfer names | method, path and query, `hash(body)`, `Paykit-Timestamp`, switch origin (the old service's), `Paykit-Identity`, `Paykit-Transfer` | `Paykit-Signature` header (8.2) |
| Message | `paykit/v1/msg` | sender app signing key | recipient identity, message JSON bytes | the plaintext's signature (4.5) |
| Request to the switch | `paykit/v1/http` | app signing key, or the identity key | method, path and query, `hash(body)`, `Paykit-Timestamp`, switch origin, `Paykit-Identity`, `Paykit-App`, `Paykit-Cert`, `Paykit-Invite-Code` | `Paykit-Signature` header (8.1) |
| Response from the switch | `paykit/v1/http-response` | the account's service key, or the operator key | `u64(status)`, `hash(request signature bytes)`, `Paykit-Server-Time` text, `hash(body)` | `Paykit-Service-Signature` header (8.1) |
| Operation | `paykit/v1/op` | writer app, or the identity key | identity, body | signed document (7.1) |
| Service transition | `paykit/v1/service-op` | service key | identity, body | signed document (7.5) |
| Transition proof | `paykit/v1/proof` | service key | identity, body | signed document (7.5) |
| Signed head | `paykit/v1/head` | service key | identity, `u64(chain_version)`, `u64(generation)`, `u64(seq)`, chain hash | `sig` of the head (7.7) |
| Export manifest | `paykit/v1/export-manifest` | the old service's key | identity, body | signed document (11.2) |
| Push webhook | `paykit/v1/push` | the registration's push key, never the service key | relay origin, body bytes | `Paykit-Signature` header of the webhook (9.2) |

- **Identity** in a field list is always the account's identity: the identity whose inbox, records and chain these are. For a message it is the recipient's.
- **Request signature bytes** are the 64 bytes decoded from `Paykit-Signature`. When the request has no decodable signature (a `401 bad_signature`, or an unknown route), the field is `hash` of the empty string.
- Vectors: `signatures.json` has one case per label, with each field, the exact signed bytes and the signature, and the rejections of 2.1; `http.json` adds requests and responses of every caller kind; `export.json` the export manifest, signed with the fixed service key of `keys.json`.

## 3. Records

### 3.1 App certificate
Signed by the identity key (label `paykit/v1/app-cert`). The signer delivers it in the approval (3.11), the companion claim of the app's Pubky grant approval (spec 3.2). Apps send it in `Paykit-Cert` to other identities' switches, inside their app record, inside every inbox entry they send (the feed's `cert`), and inside executor grants.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.app_cert` | |
| `identity` | pubky | yes | the signer's identity | |
| `app_id` | app-id | yes | equals the App ID of `signing_key` | |
| `signing_key` | `b64(32)` | yes | Ed25519 public key | |
| `bundle_key` | `b64(32)` | yes | X25519 public key | key bundles are sealed to it (3.5) |
| `scopes` | `array<string>` | yes | scope names of spec 3.3, each with the scopes it requires, each once; or extension scopes `ext:<prefix>`, at most 8; or labelled selling scopes `request:<label>`, at most 8 and never beside `request` | readers ignore other names they don't know: names that aren't a scope of spec 3.3 and start with neither `ext:` nor `request:`. An extension scope's `<prefix>` is 1 to 32 characters of `a-z`, `0-9` and `-`, starting with a letter or digit, and isn't `paykit`; it sends and receives the extension kinds `<prefix>.<name>` (5.3). A label follows the syntax of a prefix, and `paykit` is a label like any other: `request:<label>` acts as `request` for that label only (Labelled selling scopes, below) |
| `inbox` | `array<string>` | yes | the app's switch permissions: exactly those that `scopes` grants, by the table below | the service authorizes by these, never by `scopes`, which it reads only to check this list and as policy strings |
| `issued_at` | time | yes | | |
| `expires_at` | time | yes | after `issued_at`, at most 730 days (2 years) after it | the signer sets it 365 days after `issued_at` by default (spec 3.2). Expired when the checker's clock is at or past it |
| `suite_keys` | `array<suite key>` | no | 1 to 4 entries, suites strictly ascending | bundle keys of later suites (spec 3.2); writers of this version omit it. A suite key is `{ "suite": int, "key": b64 }`, `suite` from 2 to 255 (the envelope suite byte, 4.1), `key` 1 to 4096 bytes; unknown members in an entry are refused (1.1). Readers ignore the keys of suites they don't support, which in this version is every suite; its kid is derived over suite and key (1.5) |

A verifier checks: the signature with the key of the identity it expects; `v` and `kind`; that the body has no unknown members (1.1); that `identity` is that identity; that `app_id` is derived from `signing_key`; that both keys are 32 bytes, that `bundle_key` isn't an X25519 key of low order (4.2), and both times parse; that `suite_keys`, if present, has the form above; that `scopes` holds no name twice, no name that starts with `ext:` but isn't a valid extension scope (such as `ext:Cap` or `ext:paykit`), at most 8 extension scopes, no name that starts with `request:` but isn't a valid labelled selling scope (such as `request:Shop1` or `request:`), at most 8 of those and none beside plain `request`, and every scope's required scopes (below), while it ignores the other names it doesn't know; that `inbox` is exactly the list the table below derives from `scopes`, each permission once and in the table's order, with none missing and no other; and that `expires_at` is present, not `null`, later than `issued_at` and at most 730 days after it. A certificate that fails any of these is invalid at every reader: apps, the service and payers. Expiry is checked against the verifier's clock (the service uses its own time, 8.2). For the claim rules an app whose certificate has expired isn't current, so its open attempts are orphaned (7.8).

**Switch permissions from scopes** (spec 3.3). The signer sets `inbox` from `scopes` by this table, and every verifier derives the list again from the scopes it knows; names it doesn't know grant nothing. A scope may appear in several rows. The list holds each permission that one of the scopes grants, once, in the order below. Any other `inbox`, with a permission extra, missing, repeated or out of order, makes the certificate invalid at every reader, so the service never grants a permission the scopes don't. A change to this table needs a new certificate version (`v`), since verifiers of this version would refuse a certificate carrying the new list. Vectors: `documents.json` `certificate_rejections`.

| Switch permission | Granted by any of |
|---|---|
| `claims.read` | `pay`, `allowance` |
| `claims.write` | `pay` |
| `lists.read` | `contacts` |
| `lists.manage` | `manage_contacts` |
| `conversation.add` | `pay`, `request`, any labelled selling scope `request:<label>`, `allowance`, any extension scope `ext:<prefix>` |

Only valid names count in this table: a name that starts with `request:` or `ext:` but isn't a valid labelled selling scope or extension scope (above), such as `request:Shop1`, derives no permission. A certificate holding one is invalid anyway.

`allowance_execute` requires `allowance` and `pay`, and `manage_contacts` requires `contacts` (spec 3.3); a certificate that breaks this is invalid.

**Labelled selling scopes** (spec 3.3). `request:<label>` has the switch permissions of `request` (above) and sends and receives the kinds of `request` (5.3), for the requests sent under its label. Wherever a scope is compared, for a record's `read_scope` and `write_scopes` (7.2, 8.4.9, 8.5) and for the keys an app holds (3.5), plain `request` covers every `request:<label>`: a certificate holding `request` holds every label, and one holding `request:<label>` holds only that label. That is the one rule of hierarchy among scopes; the switch applies it too. So a certificate holds plain `request` or labelled scopes, never both: holding both would say nothing more, and every verifier refuses such a certificate, the signer never issues one, and it refuses an app request naming both (3.10). Vectors: `documents.json` "hosted certificate, labelled selling scope request:shop1" and `labelled_scope_rejections` (a label of 33 characters, one in upper case, an empty one, nine labels, `request` beside a label).

Example (`wallet`):

```json
{
  "v": 1,
  "kind": "paykit.app_cert",
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "app_id": "wejf37ngr7qizej3eoijn3hq4c",
  "signing_key": "34ZGVmVZLEaW9LV-fvdhq5LWstPhljHGbJgcQVSR9Ek",
  "bundle_key": "iSFVOWWeuN4EuNB7jx0IcOChXdjFaFF0P9YZGeYLsmE",
  "scopes": [
    "pay",
    "contacts",
    "manage_contacts"
  ],
  "inbox": [
    "claims.read",
    "claims.write",
    "lists.read",
    "lists.manage",
    "conversation.add"
  ],
  "issued_at": "2025-12-01T00:00:00.000Z",
  "expires_at": "2026-12-01T00:00:00.000Z"
}
```

### 3.2 App record
Signed by the app's signing key (label `paykit/v1/app-record`). Delivered with `app.put` (7.2), served by the directory (8.4), and, for an app with public endpoints, published at `/pub/paykit/v1/apps/<app_id>.json` on the Homeserver as the carrying form of 2.2.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.app` | |
| `identity` | pubky | yes | | |
| `app_id` | app-id | yes | equals the certificate's | |
| `signing_key` | `b64(32)` | yes | equals the certificate's | the key that verifies this record |
| `cert` | `signed<AppCert>` | yes | carrying form, signed by `identity` | |
| `accepts` | `array<string>` | yes | full kind strings (`paykit.payment_request`), or extension kinds (`paykit-cap.offer`, 5.3) under an extension scope the certificate holds | may narrow the certificate's scopes, never widen them (spec 3.5). The app signs its record, so it adds an extension kind under a scope it holds without a new certificate |
| `capabilities` | object | no | `{ "endpoint_identifiers": array<string> }` | default empty; identifiers the app can pay to |
| `public_endpoints` | object | no | Payment Endpoint Identifier to payload | default empty |
| `label` | `string \| null` | no | | display only |
| `updated_at` | time | yes | | the later `updated_at` wins between two copies; two different bodies with one `updated_at` are a conflict (Equal times, below; spec 4.2) |
| `expires_at` | `time \| null` | yes, always written | required, not `null`, when `public_endpoints` is non-empty | chosen by the app, at most 30 days after `updated_at` recommended (the prototype: exactly 30 days); `null` for a record without public endpoints |

A verifier reads `signing_key` from the body, verifies the signature with it, then checks `v`, `kind` and `identity`, verifies `cert` for that identity, and checks that the certificate names the same `app_id` and `signing_key`. A record with public endpoints and no `expires_at` is invalid.

- **Expiry** (spec 4.2): payers refuse public endpoints from a record whose `expires_at` is at or before their clock. The service doesn't check `expires_at`; the directory serves the record as delivered.
- **Record age limit** (spec 4.2): a payer uses a record's public endpoints only until the earlier of its `expires_at` and its `updated_at` plus the `record_max_age` of the identity's `identity.json` (3.3; 90 days when absent), and not at all when its `updated_at` is more than 5 minutes ahead of the payer's clock. So a stolen app key can't sign a record that stays payable for long: the signer, which signs `identity.json`, sets the limit. An app renews its record within that limit too (Renewal, below).
- **Path** (spec 4.2): a payer uses a record read from `/pub/paykit/v1/apps/<app_id>.json` only if its `app_id` is that `<app_id>`.
- **Renewal** (spec 4.2): once less than half a record's lifetime (`updated_at` to the earlier of `expires_at` and `updated_at` plus `record_max_age`) remains, the app renews it at its next run: it signs the record again with a new `updated_at` and `expires_at`, delivers it with `app.put` and republishes it on the Homeserver. A record with more than half its lifetime left is not rewritten. The prototype checks after every feed read; an app offline at the half-way point renews when it next runs, and one that doesn't run before `expires_at` leaves payers refusing the record until it does.
- **Withdrawing a destination:** a stale copy can't be recalled before it expires, so a payee whose destination is compromised removes it and revokes the app, or replaces it with a new one (spec 4.2).
- **Equal times** (spec 4.2): each new record of an app has an `updated_at` later than every record of it the app delivered or published, on any of its devices. Two records of one app with the same `updated_at` and different body bytes are a conflict for every reader, wherever it read them. A payer uses neither record's endpoints and tells the user; it goes on using an earlier record of that app it holds, if any, and can still pay the identity's other apps. A record with a strictly later `updated_at` ends the conflict. The same body twice is one record. The switch refuses the second record of such a pair (7.2 `app.put`). A record whose `updated_at` is more than 5 minutes ahead of the reader's clock isn't used at all (Record age limit, above), so it takes part in no comparison. Vectors: `documents.json` `app_record_conflict`.

Example (`shop`, with a public endpoint and its 30-day `expires_at`):

```json
{
  "v": 1,
  "kind": "paykit.app",
  "identity": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
  "app_id": "brrz3m6g5rci6iqnzke44q4djc",
  "signing_key": "D9vXhyOi76BsSuPscqj3tU8t_i5rcRkdOkqPyMy1WWM",
  "cert": {
    "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcF9jZXJ0...",
    "sig": "lq3oXVnpAjL5Rdld6TiIZZVwGMhx5KnCaMQedsRxLjWGv6YESVe-cg8VtZwRxtMKtTKLUrfTfQUDJdt7GtdEAA"
  },
  "accepts": [
    "paykit.payment_request_acceptance",
    "paykit.payment_request_rejection",
    "paykit.payment_request_cancellation",
    "paykit.payment_proof",
    "paykit.private_payment_list_request",
    "paykit.resend_request",
    "paykit.allowance_proposal",
    "paykit.allowance_acceptance",
    "paykit.allowance_rejection",
    "paykit.allowance_end"
  ],
  "capabilities": {
    "endpoint_identifiers": [
      "btc-lightning-bolt11",
      "btc-bitcoin-p2wpkh"
    ]
  },
  "public_endpoints": {
    "btc-lightning-bolt12": "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzr"
  },
  "label": "bob shop",
  "updated_at": "2025-12-31T00:00:00.000Z",
  "expires_at": "2026-01-30T00:00:00.000Z"
}
```

### 3.3 `identity.json`
Signed by the identity key (label `paykit/v1/identity`). Published at `/pub/paykit/v1/identity.json` on the Homeserver as the carrying form of 2.2, delivered to the own switch with `PUT /v1/{id}/identity` and to other switches with `PUT /v1/{id}/peers`, and returned by the directory.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.identity` | |
| `identity` | pubky | yes | the signer | |
| `version` | int | yes | at least 1, increases on every update | |
| `generation` | int | yes | at least 1, increases on each provider change | |
| `inbox` | object | yes | `{ "url": origin, "service_key": b64(32) }` | `url` is the switch origin exactly as requests sign it: `https://host[:port]`, no path, no trailing slash. `service_key` is the account's key from enrollment. Clients connect to it only as 8.1 "Switch origins" allows |
| `policy` | object | yes | see below | |
| `apps` | `array<object>` | yes | `{ "app_id": app-id, "cert": b64(32) }`, at most the app limit (3.8) | `cert` is the document hash of the app's certificate |
| `removed` | `array<object>` | yes | `{ "app_id": app-id, "distrust_from": time }` | |
| `data_epoch` | int | yes | at least 1, only increases | |
| `recovery_from` | `int \| null` | yes, always written | the generation lost in the most recent provider loss | |
| `record_max_age` | int | no | at least 1; absent when the identity sets none | the longest time, in seconds, payers trust one of the identity's app records after its `updated_at` (3.2, spec 4.2); absent means 90 days (7776000) |
| `service_keys` | `array<object>` | no | `{ "generation": int, "key": b64(32) }` each, ascending by `generation`, each generation once, every one before this document's `generation`, at most 256; writers leave it out when empty, and readers treat a present `[]` as none | the service keys of earlier generations that current records' proofs need; the current one is `inbox.service_key` (below) |
| `renewed` | `array<object>` | no | below; absent when empty | earlier certificates of listed apps (3.9) |

`policy`:

| Field | Type | Req | Constraints |
|---|---|---|---|
| `admission` | string | yes | `contacts_and_conversations`, the only mode of v0.2 |
| `connect_requests` | object | yes | `{ "enabled": bool, "max_pending": int, "push": bool }`; defaults `false`, `50`, `false` |

**Service keys** (spec 4.3). Each generation has one service key (3.8). The switch reports, in `GET /apps` (8.4.12), `proof_generations`: every generation that some current record's kept proof names (8.4.9), its own included. In the document that starts the next generation, at every move and every key change in place, the signer lists in `service_keys` the key of every generation in the latest report other than the new one, the outgoing generation's included when it is reported, and drops only generations the report doesn't name. Without a report it drops nothing: it lists every key its last document listed, the outgoing generation's `inbox.service_key` added. It never drops a reported key: if the reported generations need more than 256 entries, or one whose key it doesn't hold, it refuses the move or key change, with an error that names the reason and an alarm shown to the user, until the records are rewritten or deleted, which moves their proofs to the current generation. The switch enforces the same at `PUT /identity` (below). A provider loss adds no key: the lost generation's proofs date nothing, so the report and the signer leave out every generation `recovery_from` declares lost. The identity key signs the list, so any reader, an app added after the change included, checks a transition proof of an earlier generation with it (7.2 step 2). Readers reject a document whose list breaks the constraints above. Vectors: `documents.json` "alice identity.json after the move to the next service", and `http.json` "signer: identity.json at generation 2 that leaves out generation 1's service key" with its `409 conflict`.

**Lists** (spec 4.3). An App ID appears at most once in `apps` and at most once in `removed`, and never in both; a reader rejects a document that breaks this. An App ID once in `removed` is never listed in `apps` again: an app holding a version that lists it as removed refuses a later version that lists it in `apps`, and alerts the user.

Rules the service enforces at `PUT /identity` (8.4.2):
- the document verifies for the account's identity;
- it lists at most the app limit announced at enrollment, else `400 bad_request`;
- it names the service key of every generation that a current record's kept proof names (Service keys, above), in `inbox.service_key` or `service_keys`: the key itself, so a generation counts as left out unless every kept proof of it verifies with the key the document gives for it. Else `409 conflict` with `current.proof_generations`, the generations it leaves out; generations the document declares lost, its `recovery_from` and earlier, are exempt, since their proofs date nothing;
- the first document of an account enrolled without one has `version` 1, `generation` 1 and `data_epoch` 1, or carries `recovery_from` (a provider loss), else `400 bad_request` (vectors: alice's version 1, delivered in `http.json` and chained at `chain.json` seq 1);
- **compare-and-swap** (spec 4.3): the delivery names, as `prev`, the document hash of the version it builds on, and that must be the newest version the service holds, else `409 stale_identity` with that copy (8.4.2); the first delivery to an account that holds none names `null`. The same bytes as the newest version held succeed again without a new chain item, whatever `prev` they name (a retry), and answer with the original delivery's transition proof (8.4.2); an older version delivered again is `409 stale_identity`;
- a later document has a higher `version` and no lower `generation`, else `409 stale_identity`;
- a new `generation` is accepted only by an account that the newest document doesn't name yet (a staged account), or in place at a serving account: the document is at the next generation exactly and names this service's origin and the pending key an enrollment here answered (3.8); else `400 bad_request`. With `recovery_from` naming the generation that ends, it is a provider loss at the same service: the account starts afresh with the recovery record (8.4.2), keeping only its service time and its revoked and departed App IDs and those the enrollments named. Without, it is a key rotation: the sequence and chain continue under the new generation, signed with the new key. Either way the old key signs nothing more;
- `apps` lists no App ID that the version held lists in `removed`, and no App ID the service revoked or saw depart unless the version held still lists it (a departure awaiting its rotation), else `400 bad_request`. The service keeps those App IDs for good (10.1), so a revoked app is never current again, even after the signer purges it from `removed`.

**The signer and the copies** (spec 4.3):
- **Deliver, then publish.** The signer delivers each new version to the service first and publishes it on the Homeserver only after the service accepts. If the Homeserver copy differs from the service's, the signer republishes the service's copy (the one `409 stale_identity` carries).
- **Never back** (spec 4.3). The signer keeps the generation, version and document hash of the newest version a service accepted from it, or that it adopted. It never adopts or republishes a copy with a lower version or generation, or another body at that version, whichever service or answer shows it: that is a rollback or an equivocation, which it shows the user as an alarm instead of building on it. A copy with a higher version, another signer's, it may adopt.
- **Equivocation.** Two documents with the same `version` and different body bytes are equivocation: an app that sees both, from any source, stops paying and alerts the user. Apps keep `hash(body bytes)` of each `(identity, version)` they have seen. The same body under another signature isn't equivocation: it shows a signer that signed one version twice, against 2.1, and the service still treats it as another document (8.4.2). A switch that holds one body refuses a peer delivery of another at that version and answers with its copy (8.4.5), so the sender holds both.
- **Two copies.** A payer that holds both the Homeserver copy and the copy in the payee's directory (8.4.10) uses the one with the higher `version`.
- **The switch's copy** (spec 4.3). A payer about to pay a public endpoint that holds no copy of the payee's document fresher than 10 minutes reads the Homeserver copy, then, in parallel with the app record reads that need only that document, the copy the payee's switch holds (8.4.15, at the `inbox.url` the Homeserver copy names). It waits for that answer at most until its record reads finish plus a short grace (500 ms in the prototype), and uses the higher `version`: the same body at an equal version, and another body there is equivocation (above). A lower copy changes nothing, and still counts as an answer. The switch's copy is fetched now, so it counts as a fresh copy (spec 4.3), for 10 minutes like any other, whether it came from the service or from a cache (8.4.15). An app that only the newer copy lists has its record read then, one more read in that case only.
  - **A freeze in the answer** (8.4.15): the payer checks it as a freeze (3.7) with the payee's identity key, then reads the public read once more, at the freeze's `inbox.url`, and uses the higher of all the copies it holds. That answer without a freeze completes the lookup; `503 staged` there means the move isn't published, and the first answer's copy stands; a second freeze isn't followed, and the lookup has failed. A freeze that doesn't verify fails the lookup.
  - **No answer:** a lookup that hasn't answered after the grace keeps running. If, when money is about to move, it has failed (no answer after the client's retries, an error status, a copy that doesn't verify) or still hasn't answered, a manual payment goes ahead only with the user's explicit confirmation that the payee's switch couldn't confirm the payee's apps, given before the attempt is written, and the app records an event for it; an answer that arrives first is used, before the destination check. An automatic payment waits for the lookup, with the client's usual request timeout (5 s in the prototype) and one retry, and isn't made if it fails; it never pays on the Homeserver copy alone. A failed lookup counts until the payer looks up again, once its copy is more than 10 minutes old.
- **Revocations reach counterparties** (spec 13.2 step 5): when an own app sees, in its feed, a delivery whose `removed` adds an app, or whose `renewed` sets an `until` (a closed renewal, 3.9), it delivers that document with `PUT /peers` (8.4.5) to the switches it sent messages to that were accepted in the last 30 days, skipping a switch whose `known_version` (8.1) is already that version or newer.

**`renewed`** lists the earlier certificates of apps in `apps`, during and after a renewal (3.9). An entry:

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `app_id` | app-id | yes | | an entry whose app isn't in `apps` counts for nothing: it goes with its app when the app moves to `removed` |
| `cert` | `b64(32)` | yes | the document hash of an earlier certificate of that app | |
| `until` | `time \| null` | yes, always written | `null` while the renewal is in progress | the time the service took the app's record carrying the new certificate (`record_at`, 8.4.12) plus the 5-minute clock tolerance (3.9 step 4); for a reduction, the time the service took the renewing `identity.json` plus the tolerance, set at once (3.9 "Reductions") |

- **Bytes.** Writers put `renewed` last, after `recovery_from` and `record_max_age`, and omit it when it is empty, so a document without renewals has the same bytes as one written before renewals existed. Likewise they omit `record_max_age` when the identity sets none. Readers accept it anywhere (1.1); the spec's schematic example places it elsewhere, which means the same document.
- **Entries.** One per earlier certificate: a second renewal of the same app adds an entry for the certificate it replaces and keeps the older ones. Each takes about 100 bytes.
- **Purging** is the signer's choice (spec 4.3): only once it expects no more messages carrying that certificate, as stored requests keep theirs in use. The prototype never purges.
- **Meaning** (3.9, check 5): while `until` is `null` the earlier certificate counts like the listed one; once `until` is set it counts only for messages an inbox accepted before `until`.
- **At `PUT /identity`** the service checks only that each `cert` decodes to 32 bytes and each `until` parses (`400 bad_request`).

Example of the record age limit: `documents.json` "alice identity.json, record age limit (version 6)" is alice's version 3 document at version 6 with `"record_max_age": 2592000` (30 days) after `recovery_from`.

Example: alice's version 4 during a renewal of `wallet` (`documents.json` "alice identity.json, wallet renewal in progress"). The renewal drops `manage_contacts` and expires earlier, so it is a reduction, and since `manage_contacts` has keys it rotates the data epoch (3.9 Reductions). Its members are those of the version 3 example below, except `version`, which is 4, `data_epoch`, which is 3, and the hash `apps` lists for `wallet`, which is now the renewed certificate's (`HOf_wTLoiqRRSexTClHoFvoUMVCTTczLUlQOt3Lfkho`). After `recovery_from` comes the `renewed` member below. Version 5 ("... renewal closed") is the same with `until` set to `2026-01-01T00:08:00.000Z`: the service took version 4 at `00:03:00`, and the signer closed the reduction at once, adding the 5-minute tolerance (3.9 Reductions).

```json
{
  "renewed": [
    {
      "app_id": "wejf37ngr7qizej3eoijn3hq4c",
      "cert": "zvqkN-k_E_KpAdgG34ePoG5SphTNlax00o6FjwKQjEg",
      "until": null
    }
  ]
}
```

Example (alice, version 3, with a removed app):

```json
{
  "v": 1,
  "kind": "paykit.identity",
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "version": 3,
  "generation": 1,
  "inbox": {
    "url": "https://inbox.example",
    "service_key": "L3ykcjHUhsMki_ANP51ny-bBnRwgQbpdDKNAs1BotNI"
  },
  "policy": {
    "admission": "contacts_and_conversations",
    "connect_requests": {
      "enabled": false,
      "max_pending": 50,
      "push": false
    }
  },
  "apps": [
    {
      "app_id": "wejf37ngr7qizej3eoijn3hq4c",
      "cert": "zvqkN-k_E_KpAdgG34ePoG5SphTNlax00o6FjwKQjEg"
    },
    {
      "app_id": "g7teif9k1ur75jrmidqk7z7u6w",
      "cert": "Jp-atZ2yZeVkJSaqnRONI04NyrQEPDxUE-VmmiFe0fM"
    },
    {
      "app_id": "cw5ahtdkixcafna9zh1y5y5o1w",
      "cert": "Z8K0OBPyT5X2vyjtQ7TePqA2QWI03QjG-sNmOrWU4sI"
    }
  ],
  "removed": [
    {
      "app_id": "dw883cp3afxuwbgqsnpdbns7ry",
      "distrust_from": "2025-12-15T00:00:00.000Z"
    }
  ],
  "data_epoch": 2,
  "recovery_from": null
}
```

### 3.4 Receive key record
Signed by the app's signing key (label `paykit/v1/receive-key`). Registered with `receive_key.put` (7.2) and served by the directory.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.receive_key` | |
| `identity` | pubky | yes | the app's identity | |
| `app_id` | app-id | yes | the signing app | |
| `n` | int | yes | 0 for an app's first key at a service, then exactly one more each time; after a provider loss, the first record at the new service has any `n` higher than every `n` the app has registered (7.2) | |
| `kid` | `b64(8)` | yes | equals the kid of `x25519` | |
| `x25519` | `b64(32)` | yes | X25519 public key, not of low order (4.2) | |
| `suite_keys` | `array<suite key>` | no | as in 3.1; no entry's kid equals `kid` | receive keys of later suites (spec 4.4); writers of this version omit it, readers ignore suites they don't support |

Senders check a record before wrapping to it, in the order of spec 8.1: its signature with the app record's `signing_key`; the app record's certificate; that the app is current with that certificate; that `identity` and `app_id` name that identity and app; that `n` isn't lower than the highest seen for that app; that `kid` is derived from `x25519`, which isn't of low order; and that `suite_keys`, if present, has its form. Unknown members make the record invalid (1.1). A record that fails is skipped: the sender seals the message to the identity's other apps (spec 8.1).

**Unexpected registrations** (spec 3.7). An app watches the `receive_key.put` items of its own app in its feed, and the current record a `409 conflict` names. A registration whose kid it doesn't know, and that no device of the app announced through the app's own device sync, is a compromise: someone holding the app's signing key, from a copied state or a backup, registered a key of its own. The app stops (no attempts, no further reads), alerts the user, calls `leave` at once (8.4.12) and asks the signer to revoke it with `distrust_from` at the `server_time` of the registration's chain item. The prototype keeps the kids it registered (`known_kids`); an app run on several devices holds an unknown registration as pending (`pending_registrations`) until its device sync shows whether one of its devices made it.

**After a restore** (spec 3.7, 13.4). The backup knows the app's kids up to its newest key, of number `b`. A registration with `n > b` whose kid the restored app doesn't know, up to the key the restored app registers itself, may be its own or a thief's: the app asks the user, makes no attempts until the answer, and on a "no" proceeds as above. A registration after its own new key that it doesn't know is a compromise at once. The prototype keeps `b` in `restored_n_floor` and the open question in `restore_unaccounted`.

Example (`wallet`, its first key):

```json
{
  "v": 1,
  "kind": "paykit.receive_key",
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "app_id": "wejf37ngr7qizej3eoijn3hq4c",
  "n": 0,
  "kid": "-isrM12gVAQ",
  "x25519": "hd7UWzOh1c6wf8IlsOPeUbRdpHO3-ldT01QdGK1MUCg"
}
```

### 3.5 Key bundle
A key bundle is a JSON object, not a signed document:

| Field | Type | Req | Notes |
|---|---|---|---|
| `envelope` | b64 | yes | an envelope (Section 4) with one recipient: the target app's bundle key |
| `sig` | `b64(64)` | yes | identity key signature, label `paykit/v1/key-bundle`, over `lp(label, identity, target app ID (16 bytes), envelope bytes)` |

- **Envelope:** recipient identity is the identity itself, so the HPKE `info` is `lp("paykit/v1/hpke", identity, kid of the bundle key)`; the kid is that of the bundle key; `notify` is 0.
- **Plaintext:** `u32_be(len(JSON)) || JSON || zero padding` (4.5), with this JSON:

  | Field | Type | Req | Notes |
  |---|---|---|---|
  | `v` | int | yes | 1 |
  | `kind` | string | yes | `paykit.key_bundle` |
  | `naming_key` | `b64(32)` | yes | the identity's naming key (1.5) |
  | `cert` | `b64(32)` | yes | the document hash (2.2) of the app certificate (3.1) the bundle or part was made for: the approval's certificate at approval (3.11), the new one at a renewal (spec 3.2 step 1), otherwise the app's current certificate |
  | `first_epoch` | int | yes | the first data epoch the bundle, or the part (below), holds keys for; 1 for a full bundle and its first part |
  | `last_epoch` | int | yes | the last data epoch the bundle or the part holds keys for; for a bundle in one part, or a bundle's last part, the data epoch it was made for; at least `first_epoch` |
  | `keys` | `array<scope key>` | yes | exactly one for each Paykit scope of the certificate `cert` names and each data epoch from `first_epoch` to `last_epoch`, and no other. A Paykit scope is one of spec 3.3 other than an extension scope: extension scopes have no scope keys, since no shared record holds an extension kind (5.3). A labelled selling scope is a Paykit scope, and its keys are its label keys (1.5). No certificate holds both `request` and a labelled scope (3.1), so this has one meaning: a part for a certificate holding `request` carries no label keys, which its app derives, and one for a certificate holding labels only carries exactly its labels' keys. A Paykit scope that a later version adds needs a new certificate version (`v`), as a change to the permission table does (3.1), so a reader never meets keys for a scope name it doesn't know |

  A scope key is `{ "data_epoch": int, "scope": string, "key": b64(32) }`: `scope_key(data_epoch, scope)` (1.5). A key outside `first_epoch..last_epoch`, a key for a scope that isn't a Paykit scope of that certificate, a `(scope, data_epoch)` pair twice, or a pair of the certificate's scopes and the range missing makes the bundle or part invalid, and the reader skips it (Reading, below). Unknown members are ignored (1.1). The bundle's signature covers the identity, the target and the envelope, not the epochs `key_bundle.put` and `GET /keys` name in the clear, so a reader compares those with these (Reading, below).
- **At approval** (spec 3.2, 3.6) the app gets its scope keys for every data epoch so far and the naming key as a full bundle of this format, sealed to its certified bundle key, inside the approval (3.11). No Pubky grant carries them, and the companion claim's plaintext holds none either. The prototype hands the approval over in process.
- **Incremental bundles** (spec 3.6). A bundle holds only the epochs the app lacks: those after the newest epoch through which the app holds every key (`keys_through` in `GET /apps`, 8.4.12), up to the current one. The signer reads `keys_through`; when the service shows none (a new app before any bundle, or after a provider loss), it stores a full bundle. A bundle that replaces one the app never fetched starts where the app's holdings end, so nothing is skipped.
- **Full bundles on request.** An app that, after fetching its bundle, still lacks a scope key for some data epoch up to the current one (after a restore from an old backup) asks once with `POST /v1/{id}/apps/{app_id}/keys` (8.4.11); the signer, when next online, stores a full bundle for each app with a pending request, made for the app's current certificate, in parts when it is large: the ranges from epoch 1 to the current one that the parts it stored for that certificate don't cover yet. The service shows only the ranges of the stored parts (`bundle_ranges`, 8.4.12), so the signer keeps which certificate each part it stored names; a range whose part names another certificate, or one it can't tell, counts as missing. The new parts replace every part made for another certificate, which the app skipped. When the app's record at the service carries a certificate other than the current one (`record_cert`, 8.4.12), the signer also stores the current certificate with `cert.put`, in the batch of the first part, as at a renewal (3.9 step 1): an app restored from a backup older than its renewal, or one at a new service after a provider loss, may never have seen that `cert.put`, and takes the parts once it holds the certificate. The request stays pending until the stored parts together cover every epoch from 1 to the current one.
- **Size:** each scope key takes about 90 bytes of compact JSON, so one 64 KiB envelope holds about 700 of them (714 with three scopes); incremental bundles stay small while the app keeps up.
- **Parts** (spec 3.6). A bundle whose envelope would exceed 64 KiB, such as a full bundle of three scopes over 300 data epochs, or the delta of an app offline through many rotations, is stored as several bundles, each a `key_bundle.put` (7.2) covering a contiguous range `[first_epoch, last_epoch]` and each within 64 KiB. Each part is signed and sealed on its own, with the naming key and the scope keys of its range only, so an app opens it without the others. The prototype seals the whole range, and halves a range whose envelope doesn't fit until each half does. The parts stored for an app never overlap; the app fetches them all (8.4.11) and merges their keys. A delivery cut short leaves whole parts stored, and the signer resumes it with what `GET /apps` shows is still missing (`bundle_ranges`, 8.4.12). Every renewal stores, with its `cert.put`, a full bundle made for the new certificate (spec 3.2 step 1): its range, from epoch 1 to the current one, or to the next one for a renewal that rotates the data epoch (3.9 Reductions), covers every part stored for the app, so its parts replace them all, and every part stored for an app names its newest certificate. A renewal cut short between two parts leaves parts made for the earlier certificate; the app skips them and asks for a full bundle. No part holds `paykit_root` or a key for an epoch after the one it was made for: the current one, or during a rotation the next one, whose parts the signer stores before it publishes the document that raises the epoch (spec 3.2 and 13.2 step 2).
- **Reading** (8.4.11): for each part the app verifies `sig` with the identity key, opens the envelope with its bundle secret, checks `v`, `kind` and the epoch range and that the naming key is the one it holds, and merges the keys into its state. The naming key never changes (spec 3.6): the app keeps the one it was approved with. `key_bundle.put` names `first_epoch` and `last_epoch` in the clear (7.2), since data epochs are public in `identity.json`. For the parts of a `GET /keys` answer:
  - The app refuses the whole answer when its parts, by the ranges they name in the clear, are out of `first_epoch` order or overlap, since merging them would pick between two keys. It takes no part from it. An empty `bundles` is none.
  - It skips a part that doesn't verify or open, and also:
    - a part whose plaintext `first_epoch` and `last_epoch` aren't the range it names in the clear, since the signature doesn't cover the clear epochs, so a service could relabel a part;
    - a part with a key for an epoch after the next one, the newest `data_epoch` the app knows plus one (Parts, above);
    - a part whose naming key differs from an earlier part's or from the one the app holds, since an identity has one naming key; the app alerts the user, since taking it would split its obligation IDs and record keys from the other apps';
    - a part whose `cert` names neither the certificate the app presents nor a renewal of it (a later certificate for the same App ID, signing key and bundle key, with a later `issued_at`, 3.9) that the app's newest `identity.json` lists for its App ID, whose body the app has from `cert.put` in its feed or from its own record (3.9 step 3): a stale part, or one the app can't check;
    - a part whose keys aren't exactly one for each Paykit scope of that certificate and each data epoch of its range: a key of another scope, a pair twice, or a pair missing. Taking it could give the app a key its certificate doesn't grant, or leave it to choose between two keys for one pair.

    The other parts still count. A part made for a renewal that the app's newest `identity.json` doesn't list yet is skipped like a stale one; the app reads it again when it next lacks a key, once it holds that `identity.json`.
  - A data epoch that no part the app takes covers isn't a reason to skip anything: after merging, its keys show as lacking, which leads to `POST /keys` (Full bundles on request, above).

  Vectors: `envelopes.json` `key_bundle` (a part naming the executor's certificate) and `key_bundle_rejections` (an extra scope, a pair twice with conflicting keys, a pair missing, and a certificate the reader doesn't hold).

Example plaintext JSON (the `executor` app's incremental bundle at data epoch 2, `envelopes.json` `key_bundle`; `cert` is the `doc_hash` of `documents.json`'s executor certificate, whose three scopes it holds a key each for):

```json
{
  "v": 1,
  "kind": "paykit.key_bundle",
  "naming_key": "8_LZZi-e8sr6IwiADof-aTp2MTGotOOz2M-12wiYtZk",
  "cert": "Z8K0OBPyT5X2vyjtQ7TePqA2QWI03QjG-sNmOrWU4sI",
  "first_epoch": 2,
  "last_epoch": 2,
  "keys": [
    {
      "data_epoch": 2,
      "scope": "pay",
      "key": "fAUihgWrknfLtvnbk41OxrhDHClY1iUNLJ-Yjr8j3Dg"
    },
    {
      "data_epoch": 2,
      "scope": "allowance",
      "key": "Kh4M9miM23oaJwtrLbnRXIJm1ydgqBDbTHn5MDOjkKU"
    },
    {
      "data_epoch": 2,
      "scope": "allowance_execute",
      "key": "_9ZpMc96gt7BrnKrEjXUmXMVL_wCuVjWGHGgmR06Ccs"
    }
  ]
}
```

### 3.6 Executor grant
An executor app's signed grant, stored in the Allowance record value (6.4). Other own apps check it without asking the executor.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `app_id` | app-id | yes | the executor app | |
| `terms_hash` | `b64(32)` | yes | the Allowance's terms hash (1.5) | |
| `confirmed_generation` | int | yes | the generation in which the executor's user last confirmed the Allowance | signed, so no other app can raise it (spec 11.3) |
| `cert` | `signed<AppCert>` | yes | the executor's certificate, holding `allowance_execute` | |
| `sig` | `b64(64)` | yes | label `paykit/v1/executor-grant` over `lp(label, identity, counterparty identity, allowance_id text, terms hash, u64(confirmed_generation), u64(generation), u64(seq))` | `generation` and `seq`: the record version the grant's write builds on, below |

A grant counts only if `cert` verifies for the own identity, names `app_id` and holds `allowance_execute`; the signature verifies with the certificate's signing key for this identity, counterparty and `allowance_id`, at the record version its grant history entry names (6.4); and `terms_hash` equals the terms hash of the record's proposal and acceptance.

**The signed version.** The last two signed fields are the Allowance record version the grant's write builds on: the `expected_version` (generation, `seq`) of the `record.put` that adds the grant to the record's grant history (6.4), which is the version the executor read, or (0, 0) for a new record. Since a record's version is the `seq` of the write that produced it (7.2), the writer can't know its own new version before the service gives it; the version it read is fixed when it signs. The grant doesn't carry the numbers; a reader takes them from the grant's history entry. A grant moved to another place in the history therefore no longer verifies, so an own app with `allowance` can't make a ledger write look like the work of another executor by moving versions. A grant outside the history, such as a `grant` that isn't the history's last entry, counts for nothing.

Example (`documents.json` `executor_grant`, whose write built on version (1, 12); certificate cut short):

```json
{
  "app_id": "cw5ahtdkixcafna9zh1y5y5o1w",
  "cert": {
    "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcF9jZXJ0...",
    "sig": "AZ6tpIavdHNXHKpYFmugAcARnBuZv7_7sf6Fx_ZbGYl3zIT0kMl5jDV9cF8JjEyNU7TCYRS8VAAd5UDfWHUMAw"
  },
  "confirmed_generation": 1,
  "sig": "TwAuCSVx5eKgCkbbyAxeqddsr0xSL5QebVA794GsLihQO5VxA0O_qUCAk30KFH2TNuVgcWqpORJMNWNEaaLGDw",
  "terms_hash": "PEj43Oz6GuVKcLXW3n5vIbBdSORajscVVsogpqd7Y9M"
}
```

### 3.7 Freeze and unfreeze
The freeze is a signed document (label `paykit/v1/freeze`), sent as the body of `POST /v1/{id}/freeze` (8.4).

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.freeze` | |
| `identity` | pubky | yes | the account's identity | |
| `inbox` | object | yes | `{ "url", "service_key" }` of the new service N | as in `identity.json` |
| `next_generation` | int | yes | the current generation plus one | |
| `issued_at` | time | yes | | |
| `nonce` | `b64(16)` | yes | random | |

Its **freeze hash** is the document hash (2.2).
- **Kept before it is sent:** the signer keeps the signed freeze before it sends it and sends those same bytes again until it has an answer, so a lost answer never leaves it without the freeze the service may have applied. The service answers the same freeze again with its original proof (8.4.13).
- **Used once:** a freeze an unfreeze cancelled is never applied again (`409 conflict`), so a replay can't freeze the account after an abort. A new move signs a new freeze, with a new `nonce`.
- **Shown to payers:** while the account is frozen, its public read of `identity.json` carries the freeze beside the document (8.4.15), so a payer holding an old document that names this service finds the next one.

The unfreeze, the body of `POST /v1/{id}/unfreeze`, is a JSON object:

| Field | Type | Req | Notes |
|---|---|---|---|
| `freeze_hash` | `b64(32)` | yes | the freeze it cancels |
| `sig` | `b64(64)` | yes | identity key, label `paykit/v1/unfreeze`, over `lp(label, identity, freeze hash)` |
| `discarded` | object | at the old service, required | N's discard answer, below; absent in the unfreeze sent to N |

**Discard answer** (spec 13.5 Abort). The signer sends the unfreeze to N first. N discards the import bound to that freeze and answers, in the body of its `200`, with a JSON object signed with N's service key, the key the freeze names:

| Field | Type | Req | Notes |
|---|---|---|---|
| `discarded` | `b64(32)` | yes | the freeze hash |
| `sig` | `b64(64)` | yes | label `paykit/v1/discarded`, over `lp(label, identity, freeze hash)` |

The signer then sends the unfreeze to O with that answer as `discarded`. O checks it with the service key the freeze names and resumes only then (`409 conflict` without it, or when it doesn't verify), so O and N never both accept writes (11.6).

Example freeze:

```json
{
  "v": 1,
  "kind": "paykit.freeze",
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "inbox": {
    "url": "https://inbox.next.example",
    "service_key": "AxJWQ859k9ruLxc2bKi32fEnDTjAaaD0wgWazuFYJ4w"
  },
  "next_generation": 2,
  "issued_at": "2026-04-11T00:00:00.000Z",
  "nonce": "jfL4l5QlRZJmCREH9UTEsg"
}
```

Example unfreeze (as sent to N; `documents.json` `discarded` has N's answer, signed with the key of seed `service/alice/next`, and the unfreeze carrying it):

```json
{
  "freeze_hash": "MaWc7XFH-6Dq4_YPp40ual3MTfGjaaKcBldIpCqy4_0",
  "sig": "Dq2gdAEOb3JQ0sDjx-F08XCfZYdSU4klKxLXQfB6cpgkwSEwD57Rv8jMvCFUSECE0E_d59Bq0mDGyDCU6TPKAA"
}
```

### 3.8 Enrollment request and answer
`POST /v1/enroll` (spec 4.3, 6.2). The request is signed by the identity key like any signer request (8.1), and its body carries a separately signed enrollment, so a retry sends the same enrollment under a new request signature.

**Enrollment** (signed document, label `paykit/v1/enroll`):

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.enroll` | |
| `identity` | pubky | yes | equals `Paykit-Identity` | |
| `origin` | string | yes | the service's origin | a service refuses an enrollment naming another origin |
| `nonce` | `b64(16)` | yes | random, fresh per enrollment | |
| `policy` | object | yes | as in `identity.json` | the policy of the delivered `identity.json` applies; this one is informational while the account is staged |
| `identity_doc` | `signed<IdentityDoc>` | no | the current `identity.json` | present for a new provider (spec 13.5); absent for a new identity and after a provider loss |
| `revoked_apps` | `array<app-id>` | no | unique, ascending by their text (as 11.3 sorts part keys, not by the bytes they decode to, since z-base-32's alphabet isn't in ASCII order); absent when empty | every App ID the signer revoked or saw depart, which it keeps for good (spec 13.5, 13.6). A new account keeps each as a tombstone from the start (10.1), dated by the enrollment, so it never serves or lists them, also after a provider loss, when no export carries them; an import's activation keeps them so (11.5) |

**Request body:** `{ "enrollment": signed<Enrollment> }`.

**Answer** (`200`), signed by the response signature with the new account's service key:

| Field | Type | Notes |
|---|---|---|
| `service_key` | `b64(32)` | the account's service key; the signer verifies the response signature with it |
| `limits` | object | see below |
| `identity` | pubky | the enrollment's |
| `origin` | string | the enrollment's |
| `nonce` | `b64(16)` | the enrollment's |
| `enrollment` | `b64(32)` | the document hash of the signed enrollment it answers; the signer checks it |

`limits`:

| Field | Type | Prototype default | Meaning |
|---|---|---|---|
| `app_limit` | int | 32 | most apps in `identity.json`, never above 255 |
| `max_envelope` | int | 65536 | envelope cap in bytes |
| `max_sealed_value` | int | 262144 | sealed value plaintext cap in bytes |
| `quota_bytes` | int | 67108864 | inbox quota (Section 10) |
| `max_batch` | int | 8 | entries per batch |
| `record_quota_bytes` | int | 67108864 | shared record quota (Section 10) |
| `max_import_bytes` | int | 4294967296 | the largest export this service imports (10.2); a signer moves an identity only to a service whose import limit is at least its current one's (8.4.13). An answer without it stands for the default |

**Further limits.** `limits` carries only the seven members above. The retention and quota limits of Section 10 (record keys, claims, attempts per claim, lists, invite codes, connect requests, retired receive keys, peer copies, enrollment answers, the operation rate, the chunk size) aren't announced yet. Until they are, a client assumes the defaults of Section 10.2. When a service announces them, it adds them to `limits` under the names of the Name column of 10.2 (Appendix A13); clients ignore members they don't use (1.1).

- The same enrollment again gets the same answer, byte for byte.
- Another enrollment for an existing account gets the same key and limits, with its own `nonce` and `enrollment`, and changes nothing, while the account is staged. At an account the newest `identity.json` names (a serving account), it gets instead a pending key for the next generation, the same for every such enrollment until a document names it (3.3), signed with that key; each service key serves one generation. That is how a provider loss is declared at the same service, and how the signer rotates the service key.
- The account stays staged (`503 staged`, 8.3) until it holds an `identity.json` whose `inbox.url` and `inbox.service_key` name it.
- Errors before an account exists are signed with the operator key (8.1).
- An enrollment whose `revoked_apps` holds a member that isn't an App ID, isn't unique and ascending by text, or is present but empty, is `400`. Another enrollment for an existing account changes nothing, its `revoked_apps` included: the account already keeps its own.

Example enrollment:

```json
{
  "v": 1,
  "kind": "paykit.enroll",
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "origin": "https://inbox.example",
  "nonce": "cyMjqSLMjQL1PfIN117ZVQ",
  "policy": {
    "admission": "contacts_and_conversations",
    "connect_requests": {
      "enabled": false,
      "max_pending": 50,
      "push": false
    }
  }
}
```

Example answer:

```json
{
  "service_key": "L3ykcjHUhsMki_ANP51ny-bBnRwgQbpdDKNAs1BotNI",
  "limits": {
    "app_limit": 32,
    "max_envelope": 65536,
    "max_sealed_value": 262144,
    "quota_bytes": 67108864,
    "max_batch": 8,
    "record_quota_bytes": 67108864,
    "max_import_bytes": 4294967296
  },
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "origin": "https://inbox.example",
  "nonce": "cyMjqSLMjQL1PfIN117ZVQ",
  "enrollment": "2hdb0VEHayybo_L8lkXzZ9Llue4xeoBaXepOBPyAGZk"
}
```

### 3.9 Certificate check order and renewal
**Check order** (spec 3.5). The service, recipients, senders and payers check an app in this order and stop at the first check that fails:

| # | Check | At the service | A recipient, sender or payer |
|---|---|---|---|
| 1 | The certificate verifies with the identity key (3.1) | `401 bad_signature` | drops the message, or skips the app |
| 2 | Its `identity` and `app_id` are the expected identity and app, `app_id` derives from `signing_key`, and that key is the one the request, message or record is checked with | `401 bad_signature` | as 1 |
| 3 | It hasn't expired: `expires_at` is later than the service's time for a request, the entry's `accepted_at` for a message, the reader's clock for sending or paying | `403 cert_expired` | as 1 |
| 4 | The request (8.1), message (4.5) or app record (3.2) signature verifies with `signing_key` | `401 bad_signature` | as 1 |
| 5 | The certificate's standing (below) in the newest `identity.json` held counts | own app: `403 not_current`; another identity's app: `409 identity_needed`, or `403 not_current` when the certificate's renewal has closed; an app that copy lists in `removed` is answered exactly like a stranger (8.2) | as 1 |
| 6 | The app record is signed with `signing_key` and carries this certificate, or an earlier one of the same app with the same `signing_key` and `bundle_key` | `403 not_current` | skips the app |

Then come scopes and switch permissions (`403 permission_missing`; recipients drop the message), admission (spec 7) and each endpoint's own checks. The service takes the checks in the order of 8.2: the timestamp first, and for an app of another identity check 3 after check 4, so that signature work comes before the checks that read its state; a request that passes every check is treated the same either way.

- **The timestamp** (8.1) is checked right after check 4, so a stale request is refused only once it is authenticated.
- **Why 4 before 5.** Checks 1 to 3 need only what the caller presents and a clock; check 4 authenticates the caller before the service consults anything it holds for the account. If check 5 came first, anyone holding an app's public certificate could learn, without that app's key, whether this switch holds its identity's `identity.json`, which shows that the two identities correspond.
- **Which certificate.** The service checks an own app against the renewed certificate it stored with `cert.put` once the newest `identity.json` lists that certificate's hash, and otherwise against the one in the app's record (from `app.put`, or from the `app.put` in the body of the app's first call, 8.2). For an app of another identity it checks the one in `Paykit-Cert`. A recipient checks the one the inbox entry carries (the feed's `cert`), a sender the one in the app record it read in a directory (8.4.10), a payer the one in the destination app's record.
- **Standing** of a certificate with hash h for app a in an `identity.json` D (check 5):

  | Standing | When | Counts for a message accepted at t | Makes the app current now |
  |---|---|---|---|
  | listed | D's `apps` lists a with h | yes | yes |
  | renewed, open | D's `apps` lists a with another hash, and `renewed` has an entry for a and h with `until: null` | yes | yes |
  | renewed, closed | as above, with `until` set | if t is more than 5 minutes before `until` (5.1 check 7) | no |
  | removed | D's `removed` lists a | if t is at least 5 minutes before `distrust_from` (5.1 check 7) | no |
  | none | otherwise | no | no |

  "Current now" decides wherever the app acts at present: the service serving its requests and accepting its appends, a sender wrapping to it, a payer paying its destinations. "Counts at t" decides for a message, with t its `accepted_at` (spec 12).
- **Check 6** applies where a record is involved: the record the service holds for each own app, the records a sender reads in a directory, and the public record a payer reads (spec 4.2).
- **Payers** refuse a destination whose app's listed certificate has expired (check 3 at the payer's clock).

**Renewal** (spec 3.2). A renewed certificate is a new certificate for the same app: the same `app_id`, `signing_key` and `bundle_key`, and a later `issued_at`; `scopes`, `inbox` and `expires_at` may change. Other keys make a new app (spec 13.1).
1. The signer stores the new certificate with `cert.put` (7.2), in the same batch as the first `key_bundle.put` of a full bundle made for it (3.5). The bundle's parts replace every part stored for the app, whether the renewal adds scopes, removes them or keeps them, so every stored part names the newest certificate.
2. It delivers and then publishes `identity.json` with a higher `version` (3.3), listing the new certificate's hash for the app and adding the earlier hash to `renewed` with `until: null` (3.3). From this delivery the service checks the app against the stored renewed certificate: its expiry, switch permissions and scopes apply at once, also to requests the app signed earlier.
3. The app reads `cert.put` in its feed (8.5) and takes the certificate once its own `identity.json` lists its hash. An app that holds an earlier certificate and no `cert.put` for the listed one, such as one restored from a backup older than a renewal it took (the `cert.put` keeps only its stub once the record carries the new certificate, 8.5), reads its own entry in `GET /directory` (8.4.10). It takes the record's certificate only if the record verifies with its own signing key (3.2) and the certificate verifies for the identity (3.1), names its own `app_id`, `signing_key` and `bundle_key`, has a later `issued_at` than the one it holds, and has the hash `identity.json` lists for the app; it takes that record as its own too, since the service already holds it. It does this before it reads its key bundle parts. From then on it presents it (`Paykit-Cert`, the `cert` its inbox entries carry, executor grants), follows its scopes, and delivers its record carrying it with `app.put`. An app with public endpoints republishes that record on the Homeserver.
4. `GET /apps` (8.4.12) shows, for each app, `record_cert`, the certificate hash of the record the service holds, and `record_at`, when the service took it. Once `record_cert` is the new hash, the signer sets the entry's `until` to `record_at` plus the clock tolerance, 5 minutes (spec 3.2 step 4), in its next `identity.json` and delivers it. A renewal isn't a distrust event, so the tolerance goes on the side that keeps the app's last messages: recipients count the earlier certificate for a message accepted more than 5 minutes before `until` (5.1 check 7), that is, before `record_at`, when the app stopped presenting it.

- **Reductions** (spec 3.2). A renewal is a reduction when the new certificate lacks a scope or a switch permission of the earlier one, or expires earlier. Its cutoff doesn't wait for the app: right after the delivery of step 2, the signer delivers a further `identity.json` that sets `until` of every renewed entry of the app still `null` to the service time at which the service took that delivery, plus the 5-minute tolerance, and then publishes it. The time is the `server_time` of the chain envelope in the delivery's transition proof (7.5). A retry of a delivery whose answer was lost gets that proof again (8.4.2), so the time is always the service's, never the signer's clock. Step 4 doesn't apply. Recipients count the earlier certificate for a message accepted more than the tolerance before `until`, so before that delivery (5.1 check 7). Renewals that only add scopes or extend the expiry follow steps 3 and 4.
- **A removed scope's keys rotate** (spec 3.2, 13.2). A reduction whose new certificate lacks a scope with keys of the earlier one rotates the data epoch from e to e+1: a Paykit scope, a labelled selling scope (its label keys), or `request` replaced by labels, whose key derives every label's. One that removes only extension scopes, a switch permission or expiry doesn't, and neither does dropping a label that `request` in the new certificate covers. The full bundle of step 1 runs from epoch 1 to e+1 and is made for the new certificate, so it holds no key of the removed scope; then the signer stores each other current app's bundle for e+1, with its own scopes' keys, as at a revocation (spec 13.2 step 2); then step 2's delivery lists the new certificate and raises `data_epoch` to e+1, in one `identity.json`, as a revocation's delivery moves the app and raises it. The closing delivery follows as above. From that delivery the service takes sealed fields only at e+1 (`409 stale_epoch`), so the reduced app holds no key for anything written under the removed scope after it, whoever hands it the ciphertext. Vectors: the wallet renewal of `documents.json`, which removes `manage_contacts`, raises `data_epoch` from 2 to 3 in version 4, and `envelopes.json` `reduction_key_bundle` is the wallet's full bundle for it.
- **A closing delivery owed.** A signer stopped between the renewing delivery and the closing one leaves the earlier certificate open until it returns. At its next run, before any other delivery and before a provider change, it finds the closing delivery it owes from the newest `identity.json` the service holds (`GET /apps`, 8.4.12): every `renewed` entry still `null` whose renewal was a reduction, which it judges from the certificates it issued. It delivers that version again, a retry: it is still the renewing delivery, so the answer carries that delivery's proof, and the signer closes from its time as above.
- **In between.** While `until` is `null` the earlier certificate counts wherever the app presents it: other inboxes accept appends carrying it, recipients accept its messages, and senders and payers use the record that carries it. Once `until` is set, a switch refuses a request presenting it (`403 not_current`), and recipients accept a message carrying it only if its inbox accepted it more than the 5-minute tolerance before `until` (5.1 check 7), so before `record_at`, or for a reduction before the renewing delivery. A sender skips an app whose record carries a certificate whose renewal has closed (check 5, "current now"), until the app delivers its record with the new certificate.
- **Directory caches** (spec 8.1): a sender's cached directory belongs to the `identity.json` version it was read under; once the sender holds a newer version, it reads the directory again before its next send, so an app given a scope receives its kinds as soon as its new record is delivered. An app that lost a receiving scope receives its kinds no longer from each sender's next send after that sender sees the closing version: through the Homeserver event stream, an append answer whose `directory_version` is newer (8.4.3), or its re-fetch of a copy more than 10 minutes old (spec 8.1). Until then the sender's appends may still wrap those kinds to the app, and the inbox can't refuse them, since kinds are sealed and `apps_version` doesn't change on a renewal. So a removed receiving scope stops at most the sender's directory cache lifetime after the cutoff. A removed sending scope has no such delay: recipients reject a message carrying the earlier certificate that their inbox accepted after the cutoff (5.1 check 7).
- **Earlier messages** keep the authority they were sent with: a request stored in a record still verifies and can be paid (spec 12).
- **Removing a scope** applies at once at the service and, since the reduction closes the earlier certificate in the same signer step, at recipients: a switch holding the closing `identity.json` refuses the earlier certificate (`403 not_current`), recipient apps checking against it reject messages accepted after the cutoff (5.1 check 7), and senders stop wrapping to the app from their next send after they see the closing document (Directory caches, above). Own apps deliver the closing document to the switches they wrote to in the last 30 days, as after a revocation (3.3). The app keeps that scope's keys for every data epoch so far, so the removal of a scope with keys also rotates the data epoch (A removed scope's keys rotate, above).
- **An expired certificate** fails check 3, so the service refuses every request of that app, its feed read included. A renewal the signer stores and lists brings it back without anything from the app, since the service then checks it against the renewed certificate.
- **At the service** a stored renewed certificate is dropped once the app's record carries it (its `cert.put` item keeps only its stub, 8.5), or replaced by a newer `cert.put`. At most one is kept per app (Section 10).

### 3.10 App request
How an app asks the signer to approve it (spec 3.2, 13.1 step 1). It travels with the app's Pubky auth request (`specs/pubky-auth-companion-claims.md`). It is a signed document (2.2), signed with the app's signing key (label `paykit/v1/app-request`), so it proves the app holds that key: nobody can ask for a certificate naming another app's signing key with a bundle key of their own.

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.app_request` | |
| `app_id` | app-id | yes | equals the App ID of `signing_key` | |
| `signing_key` | `b64(32)` | yes | Ed25519 public key | it verifies this request |
| `bundle_key` | `b64(32)` | yes | X25519 public key | |
| `scopes` | `array<string>` | yes | scopes as in 3.1: names of spec 3.3, extension scopes `ext:<prefix>` and labelled selling scopes `request:<label>` included, each once and each with the scopes it requires, and labelled scopes never beside `request` | a name the signer doesn't know is refused, though a certificate's reader would ignore it |
| `client_id` | string | yes | the auth request's client ID (`cid`) | |
| `client_key` | `b64(32)` | yes | the auth request's client key (`cpk`) | the key the Pubky grant is bound to |
| `caps` | string | yes | the auth request's capabilities, exactly as it carries them | |
| `secret_hash` | `b64(32)` | yes | `hash(secret)` of the auth request's 32-byte secret | binds the request to one handshake |

It names no identity: the user picks the identity in the signer. Unknown members make it invalid (1.1).

**The signer checks**, in this order, and refuses the request at the first failure:
1. It verifies with `signing_key`; `v` and `kind` are right; `app_id` derives from `signing_key`; both keys, `client_key` and `secret_hash` are 32 bytes; `bundle_key` isn't an X25519 key of low order (4.2), since every reader refuses a certificate naming one (3.1); the scopes are valid (3.1), plain `request` and a labelled selling scope aren't both named, and none is a name the signer doesn't know.
2. `client_id`, `client_key` and `caps` are the auth request's, and `secret_hash` is the hash of its secret.
3. `caps` names exactly the app's own paths (spec 3.5), each `path:rw`, joined by `,`, each once: its record `/pub/paykit/v1/apps/<app_id>.json`, always; its backup folder `/priv/paykit/v1/apps/<app_id>/`, optionally; `/pub/paykit/profile.json`, only if the user lets the app edit the Paykit Profile. Anything else is refused, and so is any path that covers `/pub/paykit/v1/identity.json`: that path or a folder it lies in, such as v0.1's `/pub/paykit/` or a root grant.
4. The App ID isn't one the signer revoked or saw depart, which it keeps for good (spec 13.2), and isn't in `removed`.
5. The App ID isn't listed, unless the request names exactly the `signing_key` and `bundle_key` it is listed with: approving it then renews the app's certificate (3.9). A listed App ID is never given other keys.
6. A new App ID fits the app limit (3.8).

The signer then shows the user the App ID, the scopes and the capabilities. On approval it issues the certificate, delivers and publishes the `identity.json` that lists it (3.3), grants the Pubky capabilities checked and nothing else, and answers with the approval (3.11).

Example (`wallet`, `documents.json` "wallet app request"; `documents.json` `auth_request` has the auth request it binds):

```json
{
  "v": 1,
  "kind": "paykit.app_request",
  "app_id": "wejf37ngr7qizej3eoijn3hq4c",
  "signing_key": "34ZGVmVZLEaW9LV-fvdhq5LWstPhljHGbJgcQVSR9Ek",
  "bundle_key": "iSFVOWWeuN4EuNB7jx0IcOChXdjFaFF0P9YZGeYLsmE",
  "scopes": [
    "pay",
    "contacts",
    "manage_contacts"
  ],
  "client_id": "wallet.example",
  "client_key": "4KsMX-bMv84j6kdgukbEEWdG0g3PGHhqPG6BljlANeg",
  "caps": "/pub/paykit/v1/apps/wejf37ngr7qizej3eoijn3hq4c.json:rw",
  "secret_hash": "U_Gr-hJ54h-OApK5mB3NTs4kjZ1sQlb9adQuvQSjPHk"
}
```

### 3.11 Approval
What the signer returns to an approved app, as the companion claim of its Pubky grant approval (spec 3.2, 3.6). It carries only public material and one sealed key bundle. Whoever holds the auth request's secret, and so can read the claim from the relay, learns no key: the secret protects only the handshake. A JSON object:

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `kind` | string | yes | `paykit.approval` |
| `request` | `b64(32)` | yes | the document hash (2.2) of the app request it answers (3.10) |
| `cert` | `signed<AppCert>` | yes | the app's certificate (3.1) |
| `identity` | `signed<IdentityDoc>` | yes | the `identity.json` version that lists the certificate, as delivered to the service (3.3) |
| `bundles` | `array<object>` | yes | the app's full key bundle (3.5), as parts `{ "bundle": key bundle, "first_epoch": int, "last_epoch": int }` in `first_epoch` order, from epoch 1 to the document's `data_epoch` |

Readers ignore unknown members (1.1). The bundle is the format of 3.5: sealed to the certified `bundle_key`, signed by the identity key, holding the naming key and the app's scope keys, in parts when it doesn't fit one envelope.

**The app checks**, and refuses the approval at the first failure:
1. `v` and `kind` are right, and `request` is the hash of the request it sent.
2. `cert` verifies for the identity (3.1) and names the app's own App ID, `signing_key` and `bundle_key`.
3. `identity` verifies for the identity and lists the certificate's hash for the app.
4. It reads `bundles` by the rules of 3.5 "Reading", as it reads every later bundle, holding no naming key yet: the first part it takes sets it, and every other part must carry the same one. Every part's `cert` must be the document hash of the approval's `cert`, and every part must be taken, so each holds exactly one key for each Paykit scope of the certificate (3.5: not its extension scopes, which have no key) and each data epoch of its range, and together they cover every data epoch from 1 to the document's `data_epoch`.

It then keeps the certificate, the scope keys and the naming key, and follows spec 13.1 step 2.

Example (`documents.json` `approval`, the wallet's at data epoch 2, its signed members shortened):

```json
{
  "v": 1,
  "kind": "paykit.approval",
  "request": "rCed7bp2J3K7IxqbsFu64zTyOWYoVTi-B9mKAEY1VkM",
  "cert": { "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcF9jZXJ0...", "sig": "6BstOq1C..." },
  "identity": { "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmlkZW50aXR5...", "sig": "vbF6al58..." },
  "bundles": [
    {
      "bundle": { "envelope": "UEtUMQEBAAHRihi8...", "sig": "eLlDN142..." },
      "first_epoch": 1,
      "last_epoch": 2
    }
  ]
}
```

### 3.12 Export transfer
The signer's authorization for the next service N to fetch one export's chunks from the old service O itself, so the export doesn't pass through the signer's device (spec 13.5 step 3). A signed document (2.2) under the identity key, label `paykit/v1/export-transfer`, signed over `lp(label, body)`. The signer signs it once per manifest and keeps it (2.1, Sign once).

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.export_transfer` | |
| `identity` | pubky | yes | the account's identity | |
| `freeze_hash` | `b64(32)` | yes | the document hash of the signed freeze (3.7) | |
| `manifest_hash` | `b64(32)` | yes | the document hash of O's signed manifest (11.2) | |
| `from` | object | yes | `{ "url", "service_key" }` of O: its origin and the account's key there | as in `identity.json` |
| `to` | object | yes | `{ "url", "service_key" }` of N, as the freeze names it | |
| `issued_at` | time | yes | | informational |

Unknown members are refused (1.1).

- **N presents it** to O in the header `Paykit-Transfer`, in compact form (2.2), on `GET /v1/{id}/export/chunks/{chunk_id}`, signing the request with `to.service_key` (label `paykit/v1/transfer-request`, 8.2). It takes the transfer only at `POST /import` (11.4).
- **O serves the chunk** only if the transfer verifies with the identity key, `identity` is the account, `freeze_hash` is the freeze the account is frozen with, `from` is O's own origin and the account's key, `to` is the next service the freeze names, `manifest_hash` is the hash of the manifest O built for that freeze, and the request verifies with `to.service_key` (8.2). Otherwise `401 bad_signature` (a signature) or `403 not_admitted` (a transfer that doesn't name this export). Under a transfer, a caller may fetch chunks and nothing else (`403 permission_missing`).
- It needs no expiry: it names one freeze and one manifest, and both end with an abort (11.6) or the move.
- Vectors: `documents.json` "alice export transfer", `signatures.json` (both labels), `http.json` "next service: export chunk fetch under the export transfer" and its answer.

### 3.13 Signer hand-over
How a signer hands the identity to a new signer (spec 13.8). The new signer is a current app of the identity, and the hand-over is sealed to the bundle key its certificate names, as a key bundle is (3.5). A JSON object, not a signed document:

| Field | Type | Req | Notes |
|---|---|---|---|
| `envelopes` | `array<b64>` | yes | 1 to 64 envelopes (Section 4), in order, each with one recipient: the new signer's certified bundle key. The recipient identity is the identity itself, so the HPKE `info` is `lp("paykit/v1/hpke", identity, kid of the bundle key)`; `notify` is 0 |
| `sig` | `b64(64)` | yes | identity key signature, label `paykit/v1/signer-handover`, over `lp(label, identity, the new signer's App ID (16 bytes), u32_be(n), envelope_1 bytes, ..., envelope_n bytes)`, n the number of envelopes, so it binds the count and the order |

- **Parts:** the JSON below is split into n parts, in order, and part i is the plaintext of envelope i: `u32_be(len(part)) || part || zero padding` (4.5). The sender splits where it likes, each part fitting one envelope: at most 65,364 bytes, the 65,368 bytes of plaintext a one-recipient envelope holds less the length. With at most 64 parts the JSON is at most 64 x 65,364 = 4,183,296 bytes.
- **The bound holds every state the other limits allow.** `identity_doc` fits `PUT /identity`'s 256 KiB body cap (8.1), so it is at most 262,119 bytes here as the `identity_doc` member. That document references at most 1,884 certificates: 255 apps, the most `app_limit` allows, and as many `renewed` entries as the rest of it holds. A certificate a signer of this version issues is at most 1,646 bytes here, with every scope but `request`, 8 extension scopes and 8 labels of 32 characters, and no `suite_keys`, which writers of this version omit: 3,101,064 bytes for all of them. `revoked_apps` fits an enrollment within the same body cap, since the signer names it in every enrollment (3.8): at most 6,767 App IDs, 196,243 bytes. With the other members the JSON is at most 3,559,956 bytes, 55 parts.
- **Plaintext:** the parts, concatenated in order, are this JSON:

  | Field | Type | Req | Notes |
  |---|---|---|---|
  | `v` | int | yes | 1 |
  | `kind` | string | yes | `paykit.signer_handover` |
  | `identity` | pubky | yes | the identity handed over |
  | `identity_secret` | `b64(32)` | yes | the identity key's 32-byte Ed25519 secret (seed) |
  | `identity_doc` | `signed<IdentityDoc>` | yes | the newest `identity.json` a service accepted from the old signer (3.3): its version, generation and document hash are the old signer's high-water mark, and its `inbox.service_key` and `service_keys` the service keys the signer keeps |
  | `revoked_apps` | `array<app-id>` | yes | every App ID the old signer revoked or saw depart (3.8), kept for good: unique and ascending by text, as in 3.8, and `[]` when there are none. The new signer reads it as a set |
  | `certs` | `array<signed<AppCert>>` | yes | the certificates `identity_doc` references, and no other: that of every app it lists in `apps` and each earlier one in its `renewed` |
  | `limits` | object | yes | the `limits` of the switch's enrollment answer (3.8) |

  Unknown members are ignored (1.1).
- **Only referenced certificates:** the new signer acts on certificates only through `identity.json`. It renews a listed app, closes a listed renewal and judges a reduction against the listed certificate (3.9). A certificate that `identity.json` no longer references plays no part in its decisions: a removed or revoked app is handled by App ID (`removed`, `revoked_apps`), and readers judge old writes and messages from the certificates the records and messages carry, never from the signer. So `certs` stays within the bound however long the identity lives.
- **The old signer** seals it only to an app its newest `identity.json` lists as current, with that app's certificate, once that version is delivered and no closing delivery is owed (3.9), and not during a move (Section 11). From then on it is handing over: it signs nothing for the identity but an abort (below), and keeps the key and these records. The hand-over takes effect when the new signer's delivery wins the compare-and-swap (below). The old signer deletes the key and these records once the switch holds a version newer than `identity_doc` that it didn't deliver (spec 13.8).
- **The new signer checks**, and refuses the hand-over at the first failure:
  1. `envelopes` holds 1 to 64 envelopes; `sig` verifies with the identity key over its own App ID, their number and their bytes, in order; every envelope opens with its bundle secret, and their parts, concatenated in order, parse as the JSON; `v`, `kind` and `identity` are right; and `identity_secret` is the secret of the identity's key. So parts swapped, a part missing or added, or a count that isn't the parts', refuse it.
  2. `identity_doc` verifies for the identity (3.3) and lists the new signer's app; every certificate it lists, in `apps` and `renewed`, is among `certs`, and each of `certs` verifies for the identity (3.1). A certificate of `certs` that `identity_doc` doesn't reference, which a sender of this version doesn't include, is ignored: it plays no part in the new signer's decisions.
  3. Neither the switch's copy of `identity.json` (`GET /apps` `identity`, 8.4.12) nor the Homeserver's is newer than `identity_doc`, by version or generation, or another document at its version.

  It then takes as revoked the App IDs of `revoked_apps`, of `identity_doc`'s `removed` and those `GET /apps` shows revoked or departed, and delivers the next version of `identity.json`, `identity_doc`'s version plus 1, with `prev` the hash of `identity_doc` (8.4.2), and publishes it. The hand-over takes effect when that delivery succeeds. A second hand-over of the same export, or one another signer got ahead of, fails step 3, or the compare-and-swap of that delivery. A delivery that gets `409 stale_identity` cancels the hand-over. A new signer that refuses a hand-over once an envelope has opened, at check 1, 2 or 3, for any reason, or whose delivery gets `409 stale_identity`, deletes `identity_secret` and the records it received, and tells the user that the hand-over was refused or cancelled. So whether check 3 or the compare-and-swap stops it after an abort that won, it keeps no copy of the identity key.
- **Abort** (spec 13.8): until the hand-over takes effect, the old signer may abort it, on the user's choice. It delivers `identity_doc`'s content again at `identity_doc`'s version plus 2, with `prev` the hash of `identity_doc`, and then publishes it. The new signer's first version is `identity_doc`'s plus 1 with the same content: an abort at plus 1 would have its bytes, so the switch would answer the later of the two as a retry (8.4.2) and both would think they won. At plus 2 exactly one wins the compare-and-swap. An abort that wins kills the export: the new signer fails check 3, or its delivery gets `409 stale_identity`, and either way deletes what it received (above). The old signer is the signer again, and may seal a new hand-over from the abort's version, to another app or the same one. An abort that loses gets `409 stale_identity` naming the new signer's version: the old signer has been succeeded and deletes the key and its records. An abort whose answer was lost is sent again, the same bytes, a retry; the old signer never takes its own abort, held at the switch, for the new signer's version.
- Vectors: `envelopes.json` `signer_handover` (alice's signer to her phone app, in two parts, with fixed randomness), `signer_handover_rejections` (the parts swapped, a part missing, and a signature whose count isn't the parts') and `signatures.json` (its label). The abort of that hand-over is `documents.json` "alice identity.json, hand-over abort (version 5)"; `http.json` "signer: hand-over delivery after an abort won" is the phone's take-over delivery of version 4 that it got ahead of, and its answer `409 stale_identity` names version 5.

## 4. The binary envelope

### 4.1 Layout
An envelope carries one plaintext to 1 to 255 recipient apps of one identity (spec 5.1). With `n` recipients:

| Offset | Length | Field | Value |
|---|---|---|---|
| 0 | 4 | magic | `PKT1` (`50 4b 54 31`) |
| 4 | 1 | version | 1 |
| 5 | 1 | suite | 1 |
| 6 | 1 | flags | bit 0 is `notify`; every other bit is 0 |
| 7 | 1 | recipient count `n` | 1 to 255 |
| 8 | 32 | key commitment | `derive("paykit/v1/key-commit", content key)` |
| 40 | 88 per recipient | recipient wraps | `kid (8) \|\| enc (32) \|\| wrapped content key (48)`, in the sender's order |
| 40 + 88n | 24 | nonce | random |
| 64 + 88n | the rest | ciphertext | XChaCha20-Poly1305 of the padded plaintext, with its 16-byte tag |

- The **header** is bytes 0 to 40 + 88n. The **HPKE AAD** is its first 40 bytes (magic through key commitment).
- The total length is exactly one of the **buckets** 1024, 4096, 16384 and 65536 bytes. The padded plaintext is `bucket - (40 + 88n) - 24 - 16` bytes long.
- A sender chooses the smallest bucket with `40 + 88n + 24 + len(plaintext) + 16 <= bucket`. A plaintext that fits none is too large: 65368 bytes is the most one recipient can carry.

### 4.2 Suite 1
- **Content:** a fresh random 32-byte content key; XChaCha20-Poly1305 with a random 24-byte nonce; AAD is the whole header.
- **Key wrap**, one per recipient app: HPKE (RFC 9180) base mode with DHKEM(X25519, HKDF-SHA256) (KEM `0x0020`), HKDF-SHA256 (KDF `0x0001`) and ChaCha20-Poly1305 (AEAD `0x0003`), single-shot:
  - recipient key: the app's current receive key, or its bundle key for a key bundle (3.5);
  - `info`: `lp("paykit/v1/hpke", recipient identity, kid)`, where the recipient identity is the message's `to`, or the identity itself for a key bundle;
  - `aad`: the first 40 bytes of the header;
  - plaintext: the 32-byte content key, so the wrapped content key is 32 + 16 = 48 bytes;
  - `enc`: the 32-byte ephemeral public key.
  - **All-zero abort** (RFC 9180 7.1.4): DHKEM(X25519) fails when the X25519 output is all zero. That happens exactly when the other side's key, or `enc` for a recipient, is of low order: a point of order 1, 2, 4 or 8 on the curve or its twist, whose u-coordinate, read as RFC 7748 decodes it (top bit cleared, then mod p), is 0, 1, p - 1 or one of the two points of order 8, `325606250916557431795983626356110631294008115727848805560023387167927233504` and `39382357235489614581723060781553021112529911719440698176882885853963445705823`. Since every clamped X25519 secret is a multiple of 8, X25519 with any secret is all zero for those keys and for no other. Such a key is refused at registration (7.2), makes a certificate invalid as its bundle key (3.1), and a sender skips its app (3.4); a recipient refuses an envelope whose `enc` for it is one of them.
- **Randomness:** the content key, the nonce, and per recipient the 32 bytes of ephemeral keying material `ikmE` from which HPKE derives the ephemeral key (`DeriveKeyPair`). Implementations draw all three from the operating system's secure random generator and take none from their callers. The vectors fix all three; in the prototype only its test-only `hazmat` module can (`hazmat::seal_with`).

### 4.3 Sealing and opening
**Sealing:**
1. Draw the content key; write the header's first 40 bytes with the key commitment.
2. For each recipient, append `kid`, then the HPKE `enc` and wrapped content key.
3. Pad the plaintext with zero bytes to the bucket's plaintext length and encrypt it under the content key, with the nonce and the whole header as AAD.

**Opening** (a recipient app with receive key `kid`):
1. Check magic, version 1, suite 1, no unknown flag bits, `n` at least 1, and that the length covers the header, the nonce and a tag.
2. Find the wrap whose `kid` is the app's; none means the envelope isn't for this key.
3. Open the HPKE wrap with the receive secret, the `info` for the own identity and that kid, and the first 40 header bytes as AAD. The result must be 32 bytes.
4. Reject the envelope if `derive("paykit/v1/key-commit", content key)` differs from the header's key commitment. This makes every recipient open the same plaintext or none.
5. Decrypt the body with the whole header as AAD.
6. Parse the plaintext (4.5) and check that its padding is all zeros.

The inbox reads only the header: it checks steps 1 and the size (4.4), and that every kid is the current receive key of a current app (8.4). A recipient doesn't check that the length is a bucket: the inbox refused anything else, and the key commitment and AEAD tag already bind every byte.

### 4.4 Size rules at the inbox
- More than 65536 bytes: `413 too_large`.
- A header that doesn't parse: `400 bad_request`.
- A length that isn't one of the four buckets: `400 bad_request`.
- One app wrapped to twice: `400 bad_request`.

### 4.5 Plaintext formats
- **Message** (spec 5.2):

  | Offset | Length | Field |
  |---|---|---|
  | 0 | 16 | sender App ID bytes |
  | 16 | 64 | signature, label `paykit/v1/msg`, over `lp(label, recipient identity, message JSON bytes)` |
  | 80 | 4 | `u32_be(len(JSON))` |
  | 84 | len | message JSON bytes (5.1) |
  | 84 + len | the rest | zero padding |

- **Key bundle:** `u32_be(len(JSON)) || JSON || zero padding` (3.5).
- A reader rejects a length that runs past the plaintext and any non-zero padding byte.

Vectors: `envelopes.json` seals messages from bob's `shop` to alice's apps, to one and to three recipients in every bucket (a `payment_request_cancellation` in the 1 KiB bucket, a `resend_request` padded with `msg_ids` in the others), with every input and intermediate value: content key, key commitment, nonce, each recipient's `ikmE`, `skE`, `info`, `enc` and wrapped key, the header, and the envelope. It also has the bucket boundaries, the key bundle, and three envelopes that must be rejected: a flipped `notify` bit, the wrong recipient identity, and a forged second content key that fails the key commitment.

## 5. Messages

### 5.1 Common fields
Every message is one JSON object (spec 5.3):

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `v` | int | yes | 1 | |
| `kind` | string | yes | `paykit.` followed by a name of 5.3, or an extension kind `<prefix>.<name>` (5.3) | recipients drop kinds they don't know |
| `msg_id` | uuid | yes | fresh per message | deduplication key with the sender identity (spec 12); a re-send keeps it |
| `created_at` | time | yes | | informational (spec 5.3), except for provider-loss recovery (spec 13.6) |
| `from` | object | yes | `{ "identity": pubky, "app_id": app-id }` | must match the certificate of the inbox entry and the plaintext's App ID |
| `to` | pubky | yes | the recipient identity | must be the identity the signature names |
| `refs` | `array<b64(32)>` | no, always written | message hashes (1.5) | default `[]`; each kind fixes its order (5.3) |
| `body` | object | yes | per kind | unknown members ignored |

- **Signed plaintext:** the sender app signs `lp("paykit/v1/msg", recipient identity, message JSON bytes)` and puts the signature in the plaintext (4.5). The message hash is `hash(message JSON bytes)`.
- **Sizes:** the plaintext must fit the envelope (4.1): with `n` recipients the JSON is at most `65536 - 40 - 88n - 24 - 16 - 84` bytes. A kind that activity records store (5.3, "Recorded") is at most 16384 bytes of JSON (spec 5.4).
- **Re-sends** reuse the signed plaintext, and so the `msg_id` and the exact bytes, sealed again to the current receive keys (spec 12). A re-sent copy is a new entry with a new `accepted_at`, and the checks below judge it at that time: only the signing app can send it again (check 6: the entry's certificate must be the plaintext's app), and a copy that a removed app sends after its `distrust_from` fails check 7 however old the message.

**Recipient checks**, in this order, which is the order of 3.9 for a message:
1. Open the envelope (4.3) and parse the plaintext (4.5).
2. Verify the inbox entry's certificate (`cert` in the feed item, 8.5) as a certificate of the entry's `sender` identity (3.1; 3.9 checks 1 and 2).
3. Check that it hadn't expired at the entry's `accepted_at` (check 3).
4. Verify the signature with the certificate's signing key over the own identity and the JSON bytes (check 4).
5. Parse the JSON strictly (1.1) and check `v`.
6. Check that the plaintext's App ID, `from.app_id` and the certificate's `app_id` are equal; that `from.identity` is the certificate's identity and the entry's `sender`; and that `to` is the own identity. A message from an own app must be an `executor_notice` (spec 7).
7. Check the certificate's standing in the sender identity's newest `identity.json` at the entry's `accepted_at` (check 5, spec 12), with the clock tolerance against the sender: listed; renewed and open; renewed and closed with `accepted_at + 5 min < until`; or removed with `accepted_at + 5 min <= distrust_from`. `distrust_from` and `until` are times on the sender's service, `accepted_at` on the recipient's, so a message accepted less than 5 minutes before the cut-off is rejected too ("after `distrust_from`" in the spec is rejected; at it, minus the tolerance, still counts).
8. Check scopes (spec 3.4): the sender's certificate holds a scope that sends the kind, and the own certificate a scope that receives it. A `resend_request` names no kinds, so any current app of the counterparty may send it; the answer is filtered instead (5.12). An extension kind `<prefix>.<name>` needs `ext:<prefix>` in both certificates. A labelled selling scope `request:<label>` counts as `request` for the kind (3.1), and labels then narrow it. A `payment_request` with `label` needs `request` or `request:<label>` for that label in the sender's certificate, and one without needs `request` (5.5). A payer's acceptance, rejection, cancellation or proof with `label` needs `request` or `request:<label>` for that label in the own certificate, and one without `label` needs `request`; a cancellation without `label` may also be the payee's, so it passes at an app holding `pay` too (5.3, 5.8).
9. Deduplicate by (sender identity, `msg_id`) with the message hash.
10. Parse the body as its kind's body (5.5 to 5.17), once; later steps use the parsed body. The members the recipient reads must have their types (an `attempt_id` of 16 bytes, payment endpoints mapping identifiers to non-empty payloads, times that parse), and the rules the kind's section states must hold: a request carries at most one of `payment_endpoints` and `required_app_id` (5.5), period details carry endpoints or rates (5.9), a `resend_request` selects something (5.12), Allowance terms are valid (5.15), an `executor_notice` names a known reason (5.17), and a `label`, in a request or a payer's reply (5.5 to 5.10), is a valid label (3.1). A kind the recipient doesn't handle fails this check, an extension kind included. An app that handles an extension kind passes its body to the extension as it is: it carries no Paykit payment semantics, and no shared record holds it.

- **A failed check** drops the entry. After check 10 the recipient also keeps the (sender identity, `msg_id`) pair with the message hash as processed (spec 12), so the cursor moves past the entry and the same message isn't checked again: one message that can't be used never holds up the feed.
- **A sender that can't be checked** (its `identity.json` unreachable) holds the entry, with later entries processed past it (spec 8.2). After a bounded wait, which the app chooses, it MUST store the entry locally and move its cursor past it, checking it later: the prototype waits 24 hours by default (`DEFAULT_PARK_UNVERIFIABLE_MS`).
- **The standing is checked once** (spec 8.2): when the message is recorded, against the sender identity's `identity.json` current then. A stored message isn't checked again against later versions, so a signer that purges `removed` or `renewed` entries doesn't invalidate stored evidence; paying still checks the destination's app against the current document (spec 10.4).
- **Spam rule** (spec 7): an app doesn't create an activity record for a message from a counterparty the service has flagged (`GET /allow`, 8.4.6) or the user has blocked, and keeps only a count. A message that updates an existing record (a receipt for a payment in progress) is applied. The prototype reads the flagged senders at most every 10 minutes.
- **A write the service refuses** for a per-kind limit or the storage quota (`429 quota_exceeded`), or because it would take the record over the 256 KiB value limit (`413 too_large`, 10.2), MUST NOT stall the feed (spec 8.2): the app stores the entry locally, moves its cursor past it, and writes the record once room frees.

Example (an acceptance):

```json
{
  "v": 1,
  "kind": "paykit.payment_request_acceptance",
  "msg_id": "dd9fb672-ca40-418c-ac4d-a73a010186d5",
  "created_at": "2026-01-01T00:03:00.000Z",
  "from": {
    "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
    "app_id": "wejf37ngr7qizej3eoijn3hq4c"
  },
  "to": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
  "refs": [
    "pukrdI5C7SfN7mlLheLGWRk2qzt8RTKXlXv4E1rRGKg"
  ],
  "body": {}
}
```

### 5.2 Changes from the v0.1 bodies
Bodies use the v0.1 definitions of `specs/payment-requests.md`, `specs/payment-conversion.md`, `specs/allowances.md` and `specs/paykit-sdk.md`, with these changes (spec 5.4):

| v0.1 | v0.2 |
|---|---|
| top-level `version`, `kind`, `app_id` | the common fields `v`, `kind`, `from.app_id` (5.1) |
| `event_id` | `msg_id` |
| `payment_request_id` | the request hash in `refs` |
| the `request` object of `paykit.payment_request` | the request terms are the `body` itself |
| `conversion_quote_id` of a proof | the period details hash in the proof's `refs` |
| `paykit.payment_conversion_quote` | `paykit.payment_period_details` (5.9) |
| `proposal_event_id`, `acceptance_event_id` of Allowance messages | the proposal and acceptance hashes in `refs` |
| Receipt Access, Receipt Location, Delivery Confirmations | `paykit.payment_receipt` (5.11); no confirmations |
| the 1000-byte message limit | the envelope sizes of 5.1 |
| closed-world objects, unknown members invalid | unknown members ignored (1.1) |
| `required_app_id` required, nullable | optional, only without `payment_endpoints` |
| Private Payment List endpoint values are payload strings | objects with `payload` and `single_use` (5.13) |

New in v0.2: `per_period_endpoints`, `payment_period_details`, `resend_request`, `private_payment_list_request` and `executor_notice`.

### 5.3 Kinds

| Kind | Sender needs | Recipient needs | Body | `refs`, in order | Recorded |
|---|---|---|---|---|---|
| `payment_request` | `request` | `pay` | 5.5 | none | yes |
| `payment_request_acceptance` | `pay` | `request` | 5.6 | request hash | yes |
| `payment_request_rejection` | `pay` | `request` | 5.7 | request hash | yes |
| `payment_request_cancellation` | `request` or `pay` | the other one | 5.8 | request hash | yes |
| `payment_period_details` | `request` | `pay` | 5.9 | request hash | yes |
| `payment_proof` | `pay` | `request` | 5.10 | request hash, then the period details hash it used; none without a request | yes |
| `payment_receipt` | `request` | `pay` | 5.11 | proof hash, then the request hash when there is a request | yes |
| `resend_request` | any | any; the answer re-sends only kinds the requester's certificate receives | 5.12 | none | no |
| `private_payment_list` | `request` | `pay` | 5.13 | none | in `payment_lists` |
| `private_payment_list_request` | `pay` | `request` | 5.14 | none | no |
| `allowance_proposal` | `allowance` | `allowance` | 5.15 | none | in `allowances` |
| `allowance_acceptance`, `allowance_rejection` | `allowance` | `allowance` | 5.16 | proposal hash | in `allowances` |
| `allowance_end` | `allowance` | `allowance` | 5.16 | proposal hash, then the acceptance hash (absent for a withdrawn proposal) | in `allowances` |
| `executor_notice` | `allowance` | `allowance_execute` | 5.17 | none | no |
| an extension kind `<prefix>.<name>` | `ext:<prefix>` | `ext:<prefix>` | defined by the extension | the extension's | no |

**Extension kinds** (spec 3.4). An extension kind is `<prefix>.<name>`: a prefix as for an extension scope (3.1), then `.` and a name of 1 to 64 characters of `a-z`, `0-9` and `_`. One certificate scope `ext:<prefix>` covers every kind with that prefix, and an app's record lists the kinds it handles in `accepts`, so a new kind needs no new certificate and nothing from the signer. Senders wrap one only to apps whose certificate holds the scope and whose record accepts the kind (spec 3.5). Extension kinds carry no Paykit payment semantics: a payment never relies on one, no Paykit collection records one, and they name no destination. A recipient app that doesn't handle the kind drops it at check 10, keeping it as processed (5.1). The common fields of 5.1 apply; the body and `refs` are the extension's. The body is parsed like every message (1.1, check 5), so a number outside its `metadata` must be an integer of 1.1: an extension writes a decimal, such as a price, as a string, as Paykit writes amounts (1.7). A message whose body holds `2.5` outside `metadata` is dropped at check 5, before check 10 could pass it on. The vector `documents.json` has a certificate with `ext:paykit-cap` and a record accepting `paykit-cap.offer`.

**Labelled selling scopes** (spec 3.3, 3.5). A labelled selling scope `request:<label>` sends and receives the kinds that `request` does in the table above, and its record's `accepts` lists them. A request sent under it carries `label` (5.5), and the payer's acceptance, rejection, cancellation and proof copy the request's label (5.6 to 5.10), so the payee's apps find the record under that label's policy (6.1). **Targets:** the payer wraps those four kinds, about a labelled request, only to the payee's apps whose certificate holds `request` or `request:<label>` for that label, and about an unlabelled request, or a payment without one, only to those holding `request`. A cancellation from the payee carries no label and goes to the payer's apps holding `pay` or `request`, so an app holding `pay` gets every cancellation without a label, and a labelled one only if it also holds `request` or that label. Recipients apply the same rule at check 8 (5.1) and drop a reply that breaks it: an app holding only `request:shop2` drops an acceptance labelled `shop1`, and one holding only labelled scopes drops an acceptance without a label. A resend answer (5.12) follows the same targets. Vectors: `messages.json` `payment_request_labelled`, `payment_request_acceptance_labelled` and `label_rejections`.

`refs` name messages by hash. A reader that needs a referenced message (the request of an acceptance, the proof of a receipt) finds it by hash in its records. `executor_notice` is sent only between apps of one identity; every other kind between identities (spec 7).

### 5.4 Shared body types
- **Amount:** `{ "value": amount, "asset": string }` (1.7).
- **Billing Period:** `{ "starts_at": time, "ends_at": time }`. It must equal a period the request's recurrence computes, compared as instants (spec 11.2; vectors in `periods.json`). The rules the vectors follow: boundary k is `anchor + k * every` units, in fixed seconds for `minute`, `hour`, `day` and `week`, and as a calendar step from the original anchor for `month` and `year`, clamped to the target month's last day; period k is `[boundary(k), boundary(k+1))`. The first period is the one containing `starts_at`; with `ends_at`, the last is the one containing the instant just before `ends_at`, whole (it isn't cut at `ends_at`). The period at a time t is the period k with `boundary(k) <= t < boundary(k+1)`, if k lies between the first and the last; else there is none. A named period is valid only if it equals one of those periods.
- **Payment endpoints:** an object mapping Payment Endpoint Identifiers to non-empty payload strings, with at least one member. The sending app owns them (`from.app_id`).
- **Rates:** a non-empty array of `{ "asset": string, "value": amount }`, with unique `asset` selectors (`specs/payment-conversion.md`).
- **Recurrence:** `{ "every": int (at least 1), "unit": "minute" | "hour" | "day" | "week" | "month" | "year", "starts_at": time, "anchor": time, "ends_at": time | null }`, `ends_at` after `starts_at` when set.
- **Payment deadline:** `{ "type": "at", "timestamp": time }` on a one-time request, or `{ "type": "period_start", "seconds": int }` on a recurring one.
- **Conversion:** `{ "type": "fixed", "rates": rates }`, or `{ "type": "per_period" }` on a recurring request only.

### 5.5 `payment_request`

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `amount` | Amount | yes | | |
| `payment_reference` | string | yes | 1 to 256 characters, no control characters | copied into the payment where the method allows it |
| `proposal_expires_at` | `time \| null` | yes, always written | `null`: no expiry | bounds the acceptance of a recurring request, through a condition on its batch (spec 11.2); a one-time request is answered by paying it, so there it bounds the attempt, through its deadline (below) |
| `payment_deadline` | Payment deadline | no | the type matching the request | |
| `recurrence` | Recurrence | no | absent (or `null`) for a one-time request | |
| `payment_endpoints` | Payment endpoints | no | | the payer pays only to these |
| `required_app_id` | app-id | no | only without `payment_endpoints` | pay only to that payee app's endpoints (spec 11.1) |
| `per_period_endpoints` | bool | no | only with `recurrence` | default `false`; `payment_endpoints` then serve only the first period |
| `label` | string | no | a label (3.1); from a sender holding `request:<label>` or `request` | the selling scope it is sent under (spec 3.3); absent otherwise. A sender holding only labelled scopes always writes it |
| `accepted_payment_endpoint_identifiers` | `array<string>` | no | non-empty, unique | narrows the identifiers the payer may use; with `payment_endpoints`, every key of that map must be in it |
| `conversion` | Conversion | no | | as `specs/payment-conversion.md` |
| `metadata` | object | no | | opaque |

A request carrying both `payment_endpoints` and `required_app_id` is invalid: recipients drop it. Its request hash identifies it everywhere: obligations, activity records and `refs`.

**The attempt's deadline** (spec 10.2). An `attempt` on a one-time request carries `deadline` = `d + 1 ms` (7.2), where `d` is the earlier of `proposal_expires_at` and the `payment_deadline`'s `timestamp`; with only one of them, `d` is that one; with neither, the attempt carries no `deadline`. An `attempt` on a Billing Period of a recurring request carries the period's `starts_at` plus the payment deadline's `seconds`, plus 1 ms, or no `deadline` without a payment deadline; a recurring request's proposal expiry bounds only its acceptance (spec 11.2). `d` is taken at millisecond precision, further digits truncated (1.6), and `deadline` is written with three fraction digits: `.100` and `.1009` give `.101`, and `.999` gives `.000` of the next second. The service refuses an `attempt` from the `deadline` on (7.8), so the last instant it takes one is `d`. The acceptance of a recurring request is bounded the same way, by `server_time_in(null, proposal_expires_at + 1 ms)` (7.3, spec 11.2). Vectors: `operations.json` `attempt_deadlines`, among them a request whose payment deadline comes before its proposal expiry, and fractions of three and four digits.

**The attempt's not-before** (spec 11.2). An `attempt` on a Billing Period carries the condition `server_time_in(s, null)` (7.3), where `s` is the period's `starts_at`, so the service refuses it with `409 conflict` and its `server_time` before the period starts on its own clock. No automatic payment carries anything else. A payment by hand before the start, after the user confirms paying that period early, carries as `s` the latest service time the app had verified when the user confirmed. An attempt on a one-time request carries no not-before. An attempt on a Billing Period the request record doesn't list in `attempted_periods` also writes the request record, adding it (6.3).

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

### 5.6 `payment_request_acceptance`
Body: `{}`, or `{ "label": string }` for a labelled request, its `label` (5.5). `refs`: the request hash. v0.1 defined no other field. The example is in 5.1.

### 5.7 `payment_request_rejection`

| Field | Type | Req | Notes |
|---|---|---|---|
| `reason` | string | no | omitted when absent; `null` is treated as absent |
| `label` | string | no | a labelled request's `label` (5.5); absent otherwise |

`refs`: the request hash.

```json
{
  "reason": "user_rejected"
}
```

### 5.8 `payment_request_cancellation`
Body as the rejection (`reason`, optional; a payer's cancellation of a labelled request carries its `label`, a payee's none). `refs`: the request hash. Either side may send it. A payer records its cancellation of a recurring request in a batch conditioned on the claim of every Billing Period its request record lists in `attempted_periods`, and sends it after that batch succeeds (6.3, spec 11.2); a listed period with an open attempt refuses it.

### 5.9 `payment_period_details`
The payee's details for one Billing Period of a recurring request (spec 11.2).

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `billing_period` | Billing Period | yes | a period of the request | |
| `payment_endpoints` | Payment endpoints | no | | the period's destination |
| `rates` | Rates | no | cross-asset only: no rate for the requested asset | as v0.1 quotes |
| `expires_at` | time | yes | | the payer uses only unexpired details; their rates apply to a payment verified at or before it (below) |

- At least one of `payment_endpoints` and `rates` is present; recipients drop details with neither.
- A later message for the same period replaces an earlier one. Later means a later `created_at` instant, then, at the same instant, the greater `msg_id` compared as text. Details a proof already references are kept.
- `refs`: the request hash.
- **Rates and payment time** (spec 11.2). The rates apply to a payment whose independently verified payment time, from the payment method's evidence, is at or before `expires_at`, both read at millisecond precision, further digits truncated (1.6). The payment method defines what counts as verified payment time, as in v0.1 (`specs/payment-conversion.md`): for an ERC-20 transfer, the canonical block timestamp of its successful execution; a Bitcoin app documents its own rule, such as its zero-confirmation policy. An evidence time in whole seconds, such as a block timestamp, is read as `.000`. The payee credits a payment at the rates only then; one verified later doesn't pay the period at those rates, and gets no receipt. The payer pays by the rates only while its device clock and its highest verified service time are both before `expires_at`, at planning and again after the attempt's answer, before money moves. Rates price a payment to an endpoint of another asset than the requested one, by the asset-rail rate, else the asset-wide one, as `specs/payment-conversion.md` says; a payment to an endpoint of the requested asset is paid 1:1 and isn't bound by them. Vectors: `periods.json` `details_rates`.

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

### 5.10 `payment_proof`

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `attempt_id` | `b64(16)` | yes | the claim's attempt | |
| `billing_period` | Billing Period | for a recurring request | | |
| `payment_endpoint_identifier` | string | yes | the identifier paid | |
| `payment_app_id` | app-id | yes | the payee app whose endpoint was paid | |
| `list_version` | int | when paid to a Private Payment List | that list's `version` | |
| `amount` | Amount | for a payment without a request | | a request's amount comes from the request |
| `allowance_id` | uuid | for an automatic payment | | omitted for manual payments |
| `label` | string | for a labelled request | the request's `label` (5.5) | absent for an unlabelled request and a payment without one |
| `payment_reference` | string | no | equals the request's | v0.1 required it; the request hash in `refs` now identifies the request |
| `proof` | object | yes | opaque, method-specific: an object whose members the payment method defines (`specs/payment-requests.md`); by convention `{ "type": string, ... }`, such as `{ "type": "bitcoin-bolt11-preimage", "data": "<hex>" }` in the vectors | |

`refs`: the request hash, then the hash of the period details the payment used, if any. A proof of a payment without a request has no `refs`; the payee records it by its settlement (1.5), so the same evidence sent again under a new `msg_id` is a replay.

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

### 5.11 `payment_receipt`

| Field | Type | Req | Notes |
|---|---|---|---|
| `attempt_id` | `b64(16)` | yes | the attempt of the proof it confirms |
| `billing_period` | Billing Period | for a recurring request | the proof's |

`refs`: the proof hash, then the request hash when there is a request. The payer's apps find the obligation and the record from the request hash, and may hold the receipt without the proof (spec 10.2 row 3).

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

### 5.12 `resend_request`
Asks the counterparty to re-send messages it sent (spec 5.4, 8.2, 13.6). Best effort.

| Field | Type | Req | Notes |
|---|---|---|---|
| `msg_ids` | `array<uuid>` | no | default `[]`; messages by ID |
| `accepted_from` | time | no | messages the recipient's inbox accepted at or after this time |
| `accepted_until` | time | no | ... and at or before this time; absent means no upper bound |
| `request_hashes` | `array<b64(32)>` | no | the requests named, and every message whose `refs` name one of them |
| `billing_period` | Billing Period | no | with `request_hashes`: only that period's messages |

- At least one of `msg_ids`, `accepted_from` and `request_hashes` is present, and the times parse; recipients drop any other (5.1 check 10).
- Times select by the acceptance times the sender kept (spec 12), at millisecond precision: the first one of each message or any later re-send's. To ask for an entry it can't open, an app sends `accepted_from` and `accepted_until` both equal to the entry's `accepted_at`, which returns every message that sender's inbox accepted in that millisecond; duplicates are dropped by `msg_id`.
- The sender re-seals each selected message it first accepted in the last 30 days to the current receive keys, with its original signed plaintext, and only messages of kinds that the requesting app's certificate allows it to receive (spec 12): a wallet with `pay` alone gets back no Allowance messages. Of a payer's acceptances, rejections, cancellations and proofs, it re-sends only those check 8 lets the requesting app take under their label (5.1, 5.3): an app holding only `request:shop2` gets back none labelled `shop1` and none without a label. A sender that keeps messages longer doesn't re-send older ones, since recipients keep processed IDs for 30 days. Retention runs from the first acceptance: the sender keeps one entry per `msg_id` with its first acceptance time and the later ones, and a re-send doesn't restart it.
- **Limits** (spec 12): the sender re-sends one message to the same requester identity at most once an hour (`RESEND_COOLDOWN_MS`), and at most a daily number of messages per requester identity, which it chooses (the prototype's `DEFAULT_RESEND_BUDGET`, 500); it leaves the rest out. Shifting a range by a millisecond selects the same messages, so it gets nothing new.
- **What an app asks for** (spec 12): the unopenable, unreadable and `missed` items that lead an app to ask come from the service, and the app can't check them. For an entry it can't open and a `missed` item, it never asks for a time before its cursor's time less the clock tolerance (5 minutes): for the entry, only if its `accepted_at` is no earlier than that, and for the item, from the later of its `accepted_from` and that time. For an unreadable item, whose entry may be older than the cursor, it asks only after it has verified a departure of another own app in its chain (a `leave` or `revoke` transition, 7.5) at or after the entry's `accepted_at`. It ignores an item whose times don't parse.
- A `resend_request` adds no conversation entry (spec 7): a forged entry from a stranger must not admit that stranger.
- `billing_period` is read only with `request_hashes`; without them it selects nothing, and senders don't send it alone. An empty array counts as absent.
- `request_hashes` selects each named request and every message whose `refs` name one of them. With `billing_period`, only the messages whose body names that Billing Period are selected; the request itself, which names none, is left out. Selectors combine as a union, and each message is re-sent once, whatever its selectors (duplicates are dropped by `msg_id`).
- Any own app may ask for entries it can't open, whatever its scopes.

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

### 5.13 `private_payment_list`
The latest list of one publishing app (spec 11.4).

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `version` | int | yes | Unix milliseconds of publishing, higher than every earlier version from this app, even after a restore | |
| `payment_endpoints` | object | yes | Payment Endpoint Identifier to endpoint | |
| `expires_at` | time | yes | chosen by the publishing app from its current time, not from `version`; the prototype sets 30 days after publishing | a list without it, or with one that doesn't parse, is dropped (5.1 check 10). Payers use a list only until the earlier of `expires_at` and 30 days after its inbox accepted it (`accepted_at`), on their clock; past that it has expired, and they ask for a newer one (5.14) |

An endpoint:

| Field | Type | Req | Notes |
|---|---|---|---|
| `payload` | string | yes | non-empty |
| `single_use` | bool | no | default `false`, omitted when `false`; paying it consumes this list version |

A list with a lower or equal `version` than one held from the same app is ignored (spec 8.2). An expired list is treated as missing (spec 11.4): the payer sends `private_payment_list_request`.

**Renewal** (spec 4.2): the same rule as for records (3.2). Once less than half a list's lifetime (publishing to `expires_at`) remains, the publishing app sends the counterparty a newer list at its next run, with a higher `version` and a new `expires_at`; like any newer list it carries fresh single-use endpoints (spec 11.4). A list with more than half its lifetime left is not sent again.

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

### 5.14 `private_payment_list_request`

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

### 5.15 `allowance_proposal`

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

A period is `{ "kind": "anchored", "every": int, "unit": unit, "anchor": time }` or `{ "kind": "rolling", "every": int, "unit": "minute" | "hour" | "day" | "week" }`, with `every` at least 1. Terms configure at least one monetary ceiling or an expiry (`specs/allowances.md`).

**Checking the terms:** apps check the whole table, amounts by 1.7 and times by 1.6, when a proposal arrives, dropping one whose terms fail; when they verify an Allowance record, which is invalid with such terms (spec 8.2); and before each automatic payment. A time that doesn't parse makes the terms invalid: it is never read as no limit. Senders don't propose invalid terms.

**Meaning of the terms** (v0.1 `specs/allowances.md`, restated here):
- `per_payment_amount` is an inclusive range: a payment is allowed when `minimum <= value <= maximum`.
- Limits are inclusive ceilings: with a candidate payment, a period's counted amount plus its value must be at most `amount_limit`, and its counted payments plus one at most `payment_count_limit`; the lifetime amount plus its value at most `lifetime_amount_limit`.
- A **monetary ceiling** is `per_payment_amount`, an `amount_limit` or `lifetime_amount_limit`; terms with none of them and `expires_at: null` are invalid.
- `active_from` is the first eligible instant and `expires_at` the first ineligible one, on the service's time of the executor batch.
- `allowed_payment_endpoint_identifiers`, when set, lists the only identifiers an executor may pay to.
- A request in another asset than `asset` is never paid under the Allowance (6.5).

**Unknown members in `terms`.** v0.1 made terms closed; v0.2 readers ignore unknown members (1.1), and the prototype does. An executor that ignores a restriction it doesn't know pays more than the proposer meant, so a later version that adds a restriction must also change `v` or the kind.

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

### 5.16 `allowance_acceptance`, `allowance_rejection`, `allowance_end`

| Field | Type | Req | Notes |
|---|---|---|---|
| `allowance_id` | uuid | yes | the proposal's |
| `executor_app_id` | app-id | no | in an acceptance from the allower: the executor app at acceptance. Informational; the grant in the Allowance record decides (spec 11.3) |

- Acceptance and rejection: `refs` is the proposal hash. Only the proposal's recipient answers.
- End: `refs` is the proposal hash and the acceptance hash. A proposal withdrawal by its sender has only the proposal hash.
- The terms hash is `hash(lp(proposal JSON bytes, acceptance JSON bytes))` (1.5).

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

### 5.17 `executor_notice`
Between apps of one identity only, through the own inbox, wrapped only to the executor app (spec 11.3).

| Field | Type | Req | Constraints |
|---|---|---|---|
| `reason` | string | yes | `ended` or `handed_off` |
| `allowance_record` | `b64(32)` | yes | the record key of the Allowance record |

```json
{
  "allowance_record": "KBd1veFf0Bl3u8uXuLt-MXo5YB3klBkTlTLgn2FqUdc",
  "reason": "ended"
}
```

## 6. Shared record values
Own apps of one identity share state through sealed shared records (spec 8.3, 10.3). Apps of different vendors on one identity read each other's records, so the plaintext of each Paykit collection is part of the wire format. A value is JSON (1.1), sealed as in 7.2 under the collection's read scope. Readers verify every stored message on use (spec 8.2) and work out status from the verified messages and the claim, never from a field alone.

### 6.1 Fixed policies

| Collection | `read_scope` | `write_scopes` | Policy hash (vectors) |
|---|---|---|---|
| `payer_activity` | `pay` | `["pay"]` | `identifiers.json` `policies` |
| `payee_activity` | `request` | `["request"]` | |
| `payee_activity`, a request sent under a label | `request:<label>` | `["request:<label>"]` | `identifiers.json` `policies` (`shop1`) |
| `payment_lists` | `pay` | `["pay"]` | |
| `allowances` | `allowance` | `["allowance"]` | |
| `allowance_ledgers` | `allowance` | `["allowance_execute"]` | |

Apps ignore a record in these collections whose policy differs. A labelled request's records, its request record and its period records, have its label's policy, are sealed with the label key (1.5) under the AAD of 7.2 with `scope` `request:<label>`, and bind that label (6.3).

### 6.2 Stored message
A verifiable copy of one message (spec 8.2):

| Field | Type | Req | Notes |
|---|---|---|---|
| `sender` | pubky | yes | the sender identity |
| `cert` | `signed<AppCert>` | yes | the sender's certificate, as the inbox entry carried it |
| `plaintext` | object | yes | `{ "sender_app": b64(16), "signature": b64(64), "json": b64 }`: the signed plaintext of 4.5 without padding, the JSON as base64url of its exact bytes |
| `accepted_at` | `time \| null` | no | the acceptance time at the recipient's inbox, when known |

### 6.3 Activity records (`payer_activity`, `payee_activity`)
One message per role (spec 8.3).

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | the other identity |
| `request` | Stored message | no | the `payment_request` |
| `response` | Stored message | no | acceptance or rejection |
| `cancellation` | Stored message | no | from either side |
| `proof` | Stored message | no | |
| `receipt` | Stored message | no | |
| `period_details` | Stored message | no | period records; newer ones replace older ones unless the proof references them |
| `request_hash` | `b64(32)` | no | period records: the request's hash |
| `billing_period` | Billing Period | no | period records |
| `attempt_id` | `b64(16)` | no | a payment without a request |
| `unrequested` | object | no | a payment without a request: its destination, below |
| `msg_ids` | `array<uuid>` | no | the `msg_id`s of the messages the record stores, and of at most 16 others it reflects, the newest (spec 12) |
| `status` | string | no | informational, written by the last writer, never trusted |
| `allowance_id` | uuid | no | a payer's request record: the Allowance its first automatic payment was made under, written in that attempt's batch; later automatic payments of the request and its periods use only that one (spec 11.3) |
| `attempted_periods` | `array<int>` | no | a payer's request record of a recurring request: the start, in Unix seconds as in the obligation ID (1.5), of every Billing Period that may have an open attempt; at most 16, ascending, each once (spec 11.2). Anywhere else, or with more, a repeat or another order, the record is invalid |

`unrequested`: `{ "app_id": app-id, "endpoint_id": string, "payload": string, "source": "request" | "list" | "public", "list_version": int (for a list), "single_use": bool }`.

A payment without a request to a reusable public endpoint made while the payer's own switch didn't answer (spec 11.4) has the same record and claim, written once the switch answers: first the batch of its `attempt` (with a `deadline` 10 minutes after that write) and this record, then `done` and the record with its proof. The payer reads the payment's claim first: a lost answer may have had the first batch applied, and then only the record is read back.

A request record sits at the key derived from `blind(lp(payee, request hash))`, a period record at `blind(lp(payee, request hash, u64(period start)))` (1.5). Readers check that the stored request's hash matches the record key, recomputing it with their own naming key.

**Binding** (spec 8.2): every stored message names the record's obligation by its own signed fields, never by the record's unsigned members:
- the request hash at its place in `refs`: first in an acceptance, rejection, cancellation, period details and proof, second in a receipt, after the proof hash it confirms (5.6 to 5.11);
- for a Billing Period, the signed `billing_period` of the period details, proof and receipt.

In a request record these must name its request, and none may name a Billing Period. **Labels** (spec 8.3): in a `payee_activity` record under a labelled policy, the stored request and every message from the payer (acceptance, rejection, the payer's cancellation, proof) carry that `label`; under the fixed policy none does. The payee's own messages, period details, receipts and the payee's cancellation, carry no `label`, and readers don't check them for one. A period record holds no request, so under a labelled policy the payer's messages in it carry the label. A record that breaks this is invalid. The record key derives from the policy hash, so a labelled request's record sits at another key than an unlabelled one's. A `payer_activity` record keeps the fixed policy whatever the request's label. Vectors: `sealed.json` `labelled`, with its rejections (another label's policy, the fixed policy, an unlabelled request under a label). **Seen Allowance:** an app that has seen a request record's `allowance_id` ignores and flags a write that changes or drops it (spec 8.2). A record without a `request` is a period record if any message in it names a request or a period, or `request_hash` or `billing_period` is set; then every message in it must name both, all the same ones, the key is recomputed from them, and `request_hash` and `billing_period`, if set, must equal them. A message missing a binding field makes the record invalid. A record whose messages name no request is that of a payment without a request.

**Attempted periods** (spec 11.2). The batches that read or write `attempted_periods`:
- **An attempt on a Billing Period not listed** carries, besides the `attempt` with its conditions (`record_version` of the request record, `server_time_in(s, null)`, 5.5), a `record.put` of the request record at its next version: the list with the period added and, for each listed period it drops, a `claim_step` condition on that period's claim (1.5), at the step the writer read, on the `record.put` itself. It drops only periods whose claim it read with no open attempt. With 16 listed and none to drop, the app makes no such attempt. An executor's batch writes the request record once, with `allowance_id` too when it names the Allowance (6.5).
- **An attempt on a listed period** doesn't write the request record; its `record_version` condition is enough.
- **Any other write** of the request record may drop listed periods the same way, each with its `claim_step` condition; one that doesn't drop keeps the list as it read it.
- **A payer's cancellation** (5.8) is a `record.put` of the request record at its next version, with a `claim_step` condition for the claim of every listed period, at the step read, none of which had an open attempt.

A reader that holds the version a write was built on (its `expected_version`) checks every listed period the write drops: the `record.put` must carry a `claim_step` condition on that period's claim. One that doesn't is ignored and flagged, like a dropped role (Seen roles, below), and the reader keeps the period listed. The reader checks only that the condition is there: the service enforced it against the claim as it stood when it applied the write, and it enforced a cancellation's conditions the same way, so readers don't check those. Readers check the list's shape: ascending, each start once, at most 16. A start that names no Billing Period of the request fences nothing. An empty list is the same as none, and writers leave it out. `status` never comes from the list (spec 8.2).

**`msg_ids` is bounded** (spec 12): a writer keeps the IDs of the messages the record stores and the 16 newest of the others it adds (`RECENT_MSG_IDS`), in order, and drops older ones; each app's processed pairs, kept 30 days, cover those. A reader treats a message the record stores as reflected whether or not the list names it. So a record stays bounded however many messages repeat a role it holds.

**Seen roles** (spec 8.2): a record holds one message per role, so a reader that has seen a message in a role ignores, and shows with its writer, a later write that drops or replaces it, and one whose period details are older than those it saw (by `created_at`, then `msg_id`, 5.9) or gone. A delete isn't such a write (spec 8.3), but the reader keeps what it saw, so a later write that brings the record back without it is one.

The prototype writes every member of an activity record, with `null` for an absent one, as the example shows, except `allowance_id`, which it leaves out when absent, and `attempted_periods`, which it leaves out when empty; readers treat `null` like a missing member (Conventions).

Example (alice's `payer_activity` record of bob's request, certificate and plaintext cut short):

```json
{
  "v": 1,
  "counterparty": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
  "request": {
    "sender": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
    "cert": {
      "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcF9jZXJ0...",
      "sig": "lq3oXVnpAjL5Rdld6TiIZZVwGMhx5KnCaMQedsRxLjWGv6YESVe-cg8VtZwRxtMKtTKLUrfTfQUDJdt7GtdEAA"
    },
    "plaintext": {
      "sender_app": "CQl8r8bZGV9VwrqRrTtDSw",
      "signature": "mIYkUJh7uzIssG3zMqaOLwSXSL0XoBiW1Bo76mDhQ6O6an2CKnXOsZotrLkxXr3nGr8luHc-dRrxvbKRZqerBw",
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

### 6.4 `payment_lists` and `allowances`
**`payment_lists`**, one per payee app:

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `payee` | pubky | yes | |
| `app_id` | app-id | yes | the publishing app |
| `list` | Stored message | yes | the latest `private_payment_list` |
| `used` | `array<object>` | no | single-use endpoints used: `{ "version": int, "endpoint": string, "attempt_id": b64(16) }`, each written in the batch of the `attempt` that pays it, whatever it pays: a request, a Billing Period, an automatic payment or none (spec 10.4) |
| `msg_ids` | `array<uuid>` | no | bounded as in 6.3: the list's own and the 16 newest others |

**Binding** (spec 8.2), as for activity records: the reader recomputes the record key with its own naming key (1.5) and acts on the record only if
- the key is the one derived from `payee` and `app_id`;
- the stored list verifies, addressed to this identity, its sender identity is `payee` and its sender app is `app_id`.

**`allowances`**, one per Allowance, on either side:

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | |
| `allowance_id` | uuid | yes | |
| `proposal` | Stored message | no | |
| `response` | Stored message | no | acceptance or rejection |
| `end` | Stored message | no | |
| `grant` | Executor grant | no | the current grant (3.6): the last entry of `grants` |
| `grants` | `array<object>` | no | the grant history, below; default `[]` |
| `msg_ids` | `array<uuid>` | no | bounded as in 6.3: those of the messages it stores and the 16 newest others |

**Binding** (spec 8.2): the reader recomputes the record key with its own naming key (1.5) and acts on the record only if
- the key is the one derived from `counterparty` and `allowance_id`;
- there is a `proposal`; a record with a `response` or an `end` but no proposal is invalid;
- each member holds its kind: `proposal` an `allowance_proposal` with a `proposer_role` of `allower` or `allowee` and valid terms (5.15), `response` an `allowance_acceptance` or `allowance_rejection`, `end` an `allowance_end`;
- every stored message names that `allowance_id` (the proposal's body, and the body of the acceptance, rejection or end, 5.15 and 5.16) and is between this identity and `counterparty`, with the sides its kind requires: the proposal from either side, the response from the proposal's recipient, the end from either side, each addressed to the other; the response and the end refer to the proposal in `refs`.

A grant doesn't affect whether the record is valid. Grants are checked against the terms hash of the record's own proposal and acceptance (1.5); without an acceptance no grant is valid. `grant` is the current grant only if it is the last entry of a usable history and verifies for this identity, `counterparty` and the Allowance at that entry's version (3.6); otherwise the record has no current grant. A history entry that doesn't verify means "no valid grant from this version on" (Valid entries, below), and a history that isn't usable makes every ledger check against it fail closed.

**Grant history** (spec 11.3 "Who wrote it"). `grants` holds every executor grant the record has held, oldest first, each with the record version the write adding it built on:

| Field | Type | Req | Notes |
|---|---|---|---|
| `generation` | int | yes | the generation of the version the adding `record.put` read (its `expected_version`), 0 for a new record |
| `version` | int | yes | that version's `seq`, 0 for a new record; with `generation`, signed by the grant (3.6) |
| `grant` | Executor grant | yes | 3.6 |

- **Writing.** An executor that writes its grant, at acceptance, at a handoff, or when a confirmation signs it again (spec 11.3), appends an entry naming the version its write builds on, the one it read, and sets `grant` to the same grant. A write that fails with `409` is rebuilt from a fresh read, so an entry's version is always the one the adding write built on. Writers only append: every other write keeps `grants` as read.
- **A usable history** has versions that strictly increase, ordered by (generation, `seq`), and ends with an entry whose grant equals `grant`. Otherwise every ledger check against it fails closed.
- **Valid entries.** In a usable history an entry names an executor only if its grant verifies at the entry's version for this identity, counterparty and Allowance, and names the current terms hash (3.6). Any other entry means "no valid grant from this version on".
- **The executor at a version** v is the one named by the last entry whose version is older than v: the adding write built on that version, so the grant holds from the write's own version, which is newer. A ledger write (6.5) counts only if it carries `record_version` on the Allowance record at some v no newer than the Allowance record the app holds, and its writer is the executor at v. This holds for versions the app never saw: a version that is now a stub, an app added later, or a new executor checking its predecessor's writes.
- **Rewrites.** An app that has seen a history ignores and flags a write whose `grants` doesn't start with it (spec 8.2). An app that never saw it relies on the signed versions: a moved grant doesn't verify (3.6). Dropping an entry is still possible.
- **Records without a history** have no current grant, until the executor writes its grant again.
- **Size.** An entry with its certificate takes about 1 KiB, so a record within the 256 KiB cap holds about 200 grants.

Example entry (`documents.json` `executor_grant_history_entry`: the record was created at (1, 12) and the executor's acceptance write built on it; certificate cut short):

```json
{
  "generation": 1,
  "grant": {
    "app_id": "cw5ahtdkixcafna9zh1y5y5o1w",
    "cert": {
      "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcF9jZXJ0...",
      "sig": "AZ6tpIavdHNXHKpYFmugAcARnBuZv7_7sf6Fx_ZbGYl3zIT0kMl5jDV9cF8JjEyNU7TCYRS8VAAd5UDfWHUMAw"
    },
    "confirmed_generation": 1,
    "sig": "TwAuCSVx5eKgCkbbyAxeqddsr0xSL5QebVA794GsLihQO5VxA0O_qUCAk30KFH2TNuVgcWqpORJMNWNEaaLGDw",
    "terms_hash": "PEj43Oz6GuVKcLXW3n5vIbBdSORajscVVsogpqd7Y9M"
  },
  "version": 12
}
```

### 6.5 `allowance_ledgers`

**Binding** (spec 8.2): the reader recomputes the record key with its own naming key (1.5) and acts on the ledger only if the key is the one derived from its `counterparty` and `allowance_id`, and both fields equal those of the reader's `allowances` record for that Allowance; the ledger's checks below run against that record. A ledger, Allowance record or `payment_lists` record that fails its binding is invalid: the reader doesn't act on it, flags it as spec 8.2 says, and keeps the content it trusts. Vectors: `sealed.json` `binding_accepted` and `binding_rejections`.

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `counterparty` | pubky | yes | the allowee |
| `allowance_id` | uuid | yes | |
| `entries` | `array<object>` | no | one per attempt under the Allowance, below |
| `totals` | object | no | `{ "spent": amount, "count": int, "failed": array<object> }`: paid amounts and payments of compacted entries, and the compacted `failed` entries, each `{ "obligation": b64(32), "attempt_id": b64(16), "value": amount }`, so a later `done` still counts once; `spent` may be `""` for none, and `failed` is left out when empty |

An entry:

| Field | Type | Req | Notes |
|---|---|---|---|
| `obligation` | `b64(32)` | yes | the request's obligation ID, the claim key of its claim (7.2) |
| `attempt_id` | `b64(16)` | yes | |
| `value` | amount | yes | the amount paid; the payment method transfers exactly this (spec 10.4 step 1), and fees aren't counted |
| `asset` | string | yes | |
| `at` | time | yes | the time the executor stated, vouched for by `server_time_in(at, at + 60 s)` |
| `outcome` | `"done" \| "failed"` | no | the attempt's outcome, once known: `done` is final; `failed`, written only when the attempt's owner failed it or the user released the amount, releases it until a later `done` replaces it |

- **Executor batch** (spec 11.3): the ledger write carries `record_version(Allowance record)` and `server_time_in(at, end)`, where `end` is `at + 60 s` cut at the next boundary of any anchored limit; the `attempt` carries the request's `deadline` and `record_version(request record)` (spec 10.4); a third operation writes the activity record showing the payment as being handled (spec 11.3 step 3); at a request's first automatic payment that record, or for a Billing Period a further write of the request record, carries `allowance_id` (6.3), and for a Billing Period not yet in `attempted_periods` that one write of the request record also adds it (6.3); and one more writes the `payment_lists` record when a single-use endpoint is paid (6.4). `operations.json` shows the attempt and the ledger write.
- **The amount transferred** (spec 10.4 step 1): before the batch, the executor has its payment method report what it will transfer to the destination; another amount or asset, an amount it can't tell, or a fee above the app's cap makes the destination unusable for automatic payment.
- **Last ledger version** (spec 11.3): the executor keeps the version and item hash (7.6) of the last ledger write it wrote or read with an authenticated version (7.2 Accepting a write). Before an automatic payment, the ledger write `GET /records` shows must be that one, by item hash, or one whose signed `expected_version` is that version or later. Another write, `404`, or a write that fails the checks of 7.2 is a rollback (spec 10.1), except a `404` after a delete the executor holds at or after the kept version, and a write whose operation verifies with its writer's certificate, that is the kept write or states `expected_version` at or after the kept version, but that fails a check of 7.2 by policy (its writer's standing): those are refused as untrusted ("Building on a version"), which pauses the Allowance. An operation that doesn't verify with its writer's certificate, or doesn't match its stub (7.6), is a rollback even without a kept version.
- **Counting:** an entry's own `done` spends, and its own `failed` releases unless its attempt's claim now shows `done`, which spends. An entry without an `outcome` counts by its attempt's claim: `open` and `unknown` reserve, `done` spends, `failed` releases only when `resolved_by` is the attempt's owner (7.8) and otherwise reserves until the user releases it, and an entry whose claim is gone, or whose claim doesn't hold the attempt, spends. A `totals.failed` item counts as an entry's own `failed` does, in the lifetime amount only, since its `at` is past every period: it releases unless its attempt's claim now shows `done`, which adds its value to the lifetime amount until the executor moves it into `spent` and `count` (Outcomes written, below), so a later `done` counts once and never as less. An anchored limit counts entries whose `at` is in the period containing the evaluation time; a rolling limit of length L counts an entry while `t < at + 60 s + L`.
- **Outcomes written** (spec 11.3): the executor writes `done` from a claim, `failed` from a claim only when its `resolved_by` is the owner, `failed` for an amount the user released, and `done` over `failed` when a claim shows it later. For that it reads again the claims of `failed` entries and of compacted failed attempts; a compacted one whose claim shows `done` moves from `totals.failed` into `spent` and `count`, once. The service keeps those claims to their deadline plus 30 days (7.8), and a `failed` entry or `totals.failed` item stays at least as long, so a `done` a late receipt records in that window is seen. A receipt after the window can't be recorded on the claim, which is gone; an executor that gets one treats the attempt as `done` all the same and writes `done` over the entry's `failed` (or moves the compacted item into the totals).
- **Compaction:** entries with a final outcome and `at + longest period + 60 s < now` are dropped: a `done` one's value and count are added to `totals`, and a `failed` one is kept in `totals.failed`. The longest period of an anchored limit is the length of its period containing `now`.
- **A valid ledger** (spec 11.3 Compaction): every entry's `asset` is the terms' `asset`, each `attempt_id` appears once among the entries and `totals.failed`, and every `value`, `at` and `totals.spent` parses (1.6, 1.7). Usage and compaction refuse a ledger that isn't valid; they never read it as smaller.
- **From one version to the next** (spec 11.3): a reader that holds a ledger version accepts a newer one only if it counts no less: every held entry is still there with the same `obligation`, `attempt_id`, `value`, `asset` and `at`, its `outcome` unchanged or gone from none to `done` or `failed`, or from `failed` to `done`; or it was compacted, its `at + longest period + 60 s` before the newer write's service time (7.6 stub or 7.5 proof, else the reader's latest service time): a held `done` entry into the totals, and a held entry without an outcome or `failed` into `totals.failed` with the same `obligation` and `value`, or into the totals, since it may have become `done` between the two versions (what decides is the outcome it had when it was compacted, which the reader may not have seen); every held `totals.failed` item is still there unchanged or left into the totals; and `spent` and `count` grew by at least the values and number of what left into them. New entries are allowed. A newer version that breaks a rule is ignored and flagged (spec 8.2).
- **Building on a version** (spec 11.3): the executor writes a ledger version, in any batch, only on a write that passes the kept-version check above, the `distrust_from` check (spec 13.2 step 3) on the Allowance record and the ledger, "Who wrote it" (6.4) and the rules above against the last version it accepted; its `record.put` then carries that write's version as `expected_version`. On a refusal it writes no ledger version and the Allowance needs the user's confirmation again; an outcome batch carries the claim outcome alone.
- **A grant implies a ledger** (spec 11.3): the executor that writes an Allowance record's first grant then writes the empty ledger (`entries` and `totals` empty), conditioned on the new Allowance record version. Afterwards a `404`, or a write whose sealed value is empty, is "usage history lost" for the executor: untrusted, not paid on.
- **After a confirmation** (spec 11.3): the executor computes the rewrite and shows its figures (usage per period limit, lifetime amount, entries reserved, spent and released, amounts held reserved) before the user confirms; the confirmation computes it again and writes it only if the ledger and the version it builds on are the same as shown, else it refuses. The rewrite starts from the last ledger version it accepted, which it keeps with its content; adds from the ledger shown the entries and `totals.failed` items it lacks (an entry's `outcome` only if `done`; entries of another asset, or that don't parse, are left out), a `done` for an entry it holds, and the higher `spent` and `count`; adds its journal's attempts under the Allowance; and adds every attempt on a claim of the counterparty's obligations whose `owner` is an app the grant history names, unless that attempt's owner failed it or its verified proof names no `allowance_id` or another one, with the request's amount and the attempt's `opened_at` as `at`.
- **Rules the vectors settle**, as the prototype applies them:
  - **Order of refusal:** the per-payment range first, then each period limit in the order of `period_limits`, its amount before its count, then the lifetime limit; `refused_by` names the first that refuses.
  - **Rolling window:** an entry counts while `t < at + 60 s + L`, with no lower bound, so an entry dated after t (within the 60 s window) counts too.
  - **Lifetime and counts** include reserved entries as well as spent ones; compaction adds only `done` entries to `spent` and `count`, and keeps `failed` ones in `totals.failed`.
  - **Final** for compaction means the entry's own `outcome` is `done` or `failed`; an entry without one stays, whatever its claim shows.
  - **No period limits:** the longest period is 0, so final entries older than 60 s are compacted.
  - **Assets:** an executor refuses a request whose amount's `asset` differs from the terms' `asset` before evaluating limits, so a ledger holds entries of one asset; an entry of another asset makes the ledger invalid (above), and usage and compaction refuse it.
- Vectors: `allowance_usage.json` has every limit kind, the outcome table (a `failed` claim entry carries `resolved_by_owner`), a case for a `failed` by another app and for a ledger `failed` whose claim is now `done`, and a compaction that keeps a `failed` entry in `totals.failed`. `operations.json`'s executor batch pays the btc one-time request under the btc Allowance, so its ledger entry's `value` and `asset` are the request's own amount.

## 7. Operations, proofs and the chain

### 7.1 Signed operations
Every operation except `cursor.set` is a signed document (2.2) whose body names its type in `op`. The writer signs `lp("paykit/v1/op", identity, body)`: an app with its signing key, the signer with the identity key. A signed operation takes at most 524,288 bytes (512 KiB) in its carrying form, else `413 too_large` (7.4 step 2). The service measures the compact form, `{"body":"<body>","sig":"<sig>"}` with no whitespace: 20 bytes plus the lengths of the two base64url texts, whatever whitespace or escapes the request itself uses (1.1). So at the cap, beside a 64-byte signature (86 characters), the body's text is 524,182 characters. A `record.put` of a 256 KiB value takes about 456 KiB, so the cap bounds what else one operation carries, its conditions or an app record, and every chain item, record and claim entry fits one export chunk (11.3 Part sizes).

| Field | Type | Req | Constraints | Notes |
|---|---|---|---|---|
| `op` | string | yes | a type of 7.2 | |
| `identity` | pubky | yes | the account's identity | else `400` |
| `app_id` | `app-id \| null` | yes, always written | the calling app; `null` for the signer | else `400` |
| `generation` | int | yes | the account's current generation | else `409 stale_generation` |
| `batch` | object | yes | `{ "id": b64(16), "count": int }` | the same in every operation of the batch; `count` is the number of signed operations in it |
| `conditions` | `array<Condition>` | no | 7.3 | omitted when empty |
| ... | | | the type's fields, at the top level | |

Unknown members are `400 bad_request` at the top level and in every nested object: `batch`, each condition and its inner object, `expected_version`, `value_sealed`, `id_inputs`, the carrying form of `record` or `cert`, and `bundle` (1.1). The opaque `context` of `invite_code.put` is exempt.

Example (`operations.json` `claim attempt`: an `attempt` with a deadline, conditioned on the request record at version (1, 6), its create in `chain.json`):

```json
{
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "app_id": "wejf37ngr7qizej3eoijn3hq4c",
  "generation": 1,
  "batch": {
    "id": "X8mxmFl3oL9HfIUnsds0GQ",
    "count": 2
  },
  "conditions": [
    {
      "record_version": {
        "key": "ztlpSxU9RIODQQsXxNXqFRqz2PEo2zpGe2TYpadhrcg",
        "generation": 1,
        "version": 6
      }
    }
  ],
  "op": "claim",
  "claim_key": "gf5fn_W2IzE4B7i6A24-u5giYwJnARfTQxzPyx-ZQa0",
  "type": "attempt",
  "expected_step": 0,
  "attempt_id": "DgCVYQKL1eyDacZvG6YhZg",
  "orphan_after": 86400,
  "deadline": "2026-01-02T00:00:00.001Z"
}
```

### 7.2 Operation types

**`claim`** (spec 10.2). Needs `claims.write`.

| Field | Type | Req | Constraints |
|---|---|---|---|
| `claim_key` | `b64(32)` | yes | names the claim; opaque to the service |
| `type` | string | yes | `attempt`, `done`, `failed` or `unknown` |
| `expected_step` | int | yes | the claim's step: the number of entries so far, 0 for a claim key the service holds nothing for; a tombstone keeps its step |
| `attempt_id` | `b64(16)` | yes | every type names one |
| `orphan_after` | int | on `attempt`, required | seconds; any value |
| `deadline` | time | on `attempt` only, optional | `d + 1 ms`, with `d` as 5.5 defines it (spec 10.2); on another type `400` |
| `basis` | string | on `done`, required | `owner`, `evidence` or `reconcile` |
| `evidence_ref` | `b64(32)` | on `done` with basis `evidence`, required; with `owner` or `reconcile`, optional; on another type `400` | hash of the evidence: with `evidence` the counterparty's confirmation, with `owner` or `reconcile` the app's own proof |

A `done` names at most one piece of evidence, by `evidence_ref` (spec 10.2). A type other than `done` carrying it is `400`, and so is basis `evidence` without it. The service never interprets `attempt_id` or `evidence_ref`. Paykit's apps use one claim per obligation (a request, a Billing Period or a payment without a request): its claim key is the obligation ID (1.5), blinded for a request or a Billing Period, and derived from the attempt ID alone, without blinding, for a payment without a request; `done` means paid. With basis `evidence`, `evidence_ref` is the hash of the payee's receipt message; with `owner` or `reconcile`, of the payer's proof message. 7.8 gives the rules each type follows.

**`record.put`** (spec 10.3). Needs one of the record's `write_scopes`; plain `request` covers every `request:<label>` (3.1).

| Field | Type | Req | Constraints |
|---|---|---|---|
| `key` | `b64(32)` | yes | the record key (1.5) |
| `collection` | `b64(32)` | yes | the collection ID (1.5), not the name |
| `read_scope` | string | yes | |
| `write_scopes` | `array<string>` | yes | at least one, sorted ascending by bytes, no duplicates |
| `expected_version` | Record version | yes | `{ "generation", "version" }` of the version the writer read (1.8); `{0, 0}` to create |
| `value_sealed` | Sealed value | yes | below |
| `id_inputs` | object | when `expected_version` is `{0, 0}`, and only then | `{ "collection": collection name, "blinded_key": b64(32) }`, the blinded natural key `blind(natural key)` (1.5). On any other write it is refused, `400 bad_request`, as part of the operation's shape (7.4 step 9), so before the compare-and-swap |

- **Versions** (spec 10.3): a record's version is the (generation, `seq`) of the write that produced it, given by the service; the transition proof's `state` shows it (7.5). Versions are ordered by generation, then `seq`, so they only grow across the account, whoever writes. A writer can't know its new version before the service gives it.
- **Compare-and-swap:** `expected_version` must be the record's current version, else `409 conflict` (8.6). `{0, 0}` matches a record that is absent, forgotten or deleted, so a writer that holds a deleted record creates it again the same way, never needing to know whether the service has forgotten the key.
- **At creation** (`expected_version` `{0, 0}`) the service recomputes `key` from `id_inputs` and the operation's policy hash, and `collection` from the name and the identity; a mismatch is `400`. The service sees only the blinded natural key, so the key names neither the counterparty nor the request; a live operator still links it to a sender through the batch and its timing (spec 16).
- **Later writes** repeat `collection`, `read_scope` and `write_scopes` exactly; a difference is `400`. A write to a deleted record that names its delete's version continues it too.
- **One write per record per batch** (spec 14.5): a batch that writes one record twice is `400 bad_request`, since the second write couldn't name the first one's version.
- **A conflict carries the current write**: when the writer holds the record's read scope, the `409` answer's `current` holds `record` (`key`, `generation`, `version`) and `record_view`, the current write as `GET /records/{key}` returns it (8.4.9). The writer drops or redoes its write without reading the record again. A writer without the read scope gets `current: {}`.
- **Sealed value:**

  | Field | Type | Req | Notes |
  |---|---|---|---|
  | `data_epoch` | int | yes | the current data epoch, else `409 stale_epoch` (outside recovery uploads) |
  | `scope` | string | yes | equals `read_scope`, else `400` |
  | `nonce` | `b64(24)` | yes | random |
  | `ct` | b64 | yes | ciphertext and 16-byte tag: 16 bytes (a delete), or a bucket below plus 16; more than 262160 bytes is `413`, any other length `400` |

  The cipher is XChaCha20-Poly1305 under `scope_key(data_epoch, scope)` (1.5, spec 3.6), the label key for a labelled selling scope, with AAD `lp("paykit/v1/sealed", identity, scope, record key)`.
- **Padding** (spec 14.4). The plaintext sealed is `u32_be(len(JSON)) || JSON || zero padding`, as long as the smallest of these buckets that holds `4 + len(JSON)`: 512, 768, 1024, 1536, 2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072, 196608 and 262144 bytes (2^k for k from 9 to 18, and 3 * 2^k for k from 8 to 16). Each bucket is at most half again the one below, so padding adds at most half a value's size, under a fifth on average, and at least 512 bytes are stored per value. A value is therefore at most 262140 bytes of JSON; a larger one fits no bucket, and the service answers it `413`. Readers check, after opening, that the length fits and the padding is all zeros; a value that fails is a value that "doesn't open" (step 7 below). The stored length shows only the bucket (spec 16). Vectors: `sealed.json` `padded_len`.
- **Delete:** a write whose sealed value is a sealed empty plaintext, unpadded, so `ct` is the 16-byte tag only. It passes every check of a write, and gets a newer version like any write. The key keeps its policy while the service remembers it.
- **Forgotten deletes** (spec 10.3): the service may forget a deleted key once nothing refers to it (10.1). A forgotten key's delete keeps only its chain stub, and a later create at the key, with `{0, 0}`, gets a version newer than any the key had, since versions are `seq`s.
- **Accepting a write** (spec 10.3): an app acts on a `record.put` it reads, from the feed (8.5), `GET /records`, a list page or a `409`'s `record_view`, only after these checks:
  1. The writer's certificate: one that verifies for the identity (3.1) and names the operation's `app_id`, from the record view's `cert` (8.4.9), the certificate the service checked the write against, or else the app's own, or the app records it keeps from the directory (8.4.10) and `app.put` items. The operation's signature verifies with its `signing_key`, and `identity` is the account's. Without a certificate, or with a writer the app's `identity.json` doesn't name, the app first reads its `identity.json` and directory again.
  2. A feed item matches its stub (7.6); a stub the app verified in its chain dates the write and gives its version, including the stubs of the page just read whose hashes the page's checkpoint prunes. A transition proof that verifies with the current service key must have a stub whose `item_hash` is the operation's, an `object` that is `key`, a `state` whose `generation` and `version` are the stub's, and must not contradict a chain hash the app verified; it then gives the version if no stub did. A proof of an earlier generation, as a switch keeps it with a record after a key change or carries it after a move, gives no version, but dates the write for step 4 when it verifies with the key the newest own `identity.json` lists for the generation its stub names in `service_keys` (3.3), and its stub's `item_hash` is the operation's. A generation the list doesn't name dates nothing; an honest signer lists every generation a current record's proof names (3.3).
  3. `key` is the one read; `collection` is a Paykit collection (6.1) and the policy one of its policies there; `value_sealed.scope` is `read_scope`; a create's `key` derives from `id_inputs` (1.5).
  4. Standing: in the newest own `identity.json` the app holds, the certificate is listed; or it is an earlier certificate of a listed app in `renewed`, with `until` `null` or after the write's time; or the app is in `removed` and the write's time is at or before `distrust_from`. Only a proof (step 2) or a verified stub gives the write's time. An earlier certificate whose `until` is set counts only when one does and puts the write before `until`; without one the write has no standing (spec 8.2), since the service keeps the proof of every current write (8.4.9). A removed app's write without a time counts as doubtful (spec 13.2 step 3). A certificate expired at the write's time doesn't count.
  5. The certificate holds one of `write_scopes`.
  6. The version: from step 2 when a proof or a verified stub gives it (authenticated); otherwise the record view's (unauthenticated). Either way its generation is the operation's `generation` and it is after `expected_version`.
  7. The content: `value_sealed` opened with the scope key of its `data_epoch` (1.5, spec 14.4), and its padding removed (above). An empty plaintext is a delete, and nothing else is: a `deleted` flag or a 16-byte `ct` alone proves nothing. Without that epoch's key the value is unreadable; a value that doesn't open with the key, whose length or padding is wrong, or that isn't JSON, fails.

  Signed evidence of misbehaviour (an item that doesn't match its stub, a proof that names another operation or object, or contradicts the verified chain) is a rollback (spec 10.1); any other failure leaves the write ignored and shown with the app it names (spec 8.2). The app takes an accepted write in place of the one it holds only if it is newer: with both versions authenticated, the higher one; with the new version unauthenticated, only if its `expected_version` is the held version or later; with the held version unauthenticated, the held write counts as just after its own `expected_version`. The same write again, with an authenticated version, makes the held one authenticated. The AAD stays `lp("paykit/v1/sealed", identity, scope, record key)`: the operation's signature binds the writer.

Example (`operations.json` `record.put create`, which `chain.json` applies at `seq` 6, giving the record version (1, 6); `ct`, cut short, seals the payer request record of 6.3 holding bob's request):

```json
{
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "app_id": "wejf37ngr7qizej3eoijn3hq4c",
  "generation": 1,
  "batch": {
    "id": "RlS8dXaU6JBOESpdL12e5A",
    "count": 1
  },
  "op": "record.put",
  "key": "ztlpSxU9RIODQQsXxNXqFRqz2PEo2zpGe2TYpadhrcg",
  "collection": "vAdq7de2YcjuIim-ag5zW6vixdi23ORiOV8-J2P_BsE",
  "read_scope": "pay",
  "write_scopes": [
    "pay"
  ],
  "expected_version": {
    "generation": 0,
    "version": 0
  },
  "value_sealed": {
    "data_epoch": 2,
    "scope": "pay",
    "nonce": "WDEyIeaJ1mScEvDVOP66KIv2LyuUnfy2",
    "ct": "cIqaJvKEJQ6_zJPWfq0biBxy0zmedIsDNed1yQ53dgHlm9ad_bN7qMC..."
  },
  "id_inputs": {
    "collection": "payer_activity",
    "blinded_key": "wa-Ut6gkPKJjWFeCMeUGMsbRwiOqaOF4bOfx8VVjUVw"
  }
}
```

**`allow.put`, `allow.delete`** (spec 7): `{ "pubky": pubky, "kind": "contact" | "conversation" }`. Contacts need `lists.manage`; adding a conversation entry needs `conversation.add`, removing one `lists.manage`. Adding an entry that exists changes nothing (a conversation entry keeps its first writer) but is still chained. Adding a conversation entry for a pending connect request removes the request.

**`deny.put`, `deny.delete`**: `{ "pubky": pubky }`. Need `lists.manage`. Denying removes a pending connect request.

**`invite_code.put`** (spec 7). Needs `conversation.add`.

| Field | Type | Req | Constraints |
|---|---|---|---|
| `code_hash` | `b64(32)` | yes | `hash(code bytes)` (1.8); registering it again is `409 conflict` |
| `expires_at` | time | yes | |
| `max_uses` | int | yes | 1 to the service's `max_invite_uses` (100 by default, 10.2), else `400` (spec 7): a code for one order or counterparty has 1 |
| `context` | any JSON | yes | at most 1024 bytes as the writer serialized it, else `413`; stored and shown in the clear, so the operator reads it (spec 7, 16): an opaque reference, never personal data |

**`invite_code.delete`**: `{ "code_hash": b64(32) }`. Needs `conversation.add`. An unknown code is `404`.

**`app.put`** (spec 4.2, 13.1): `{ "record": signed<AppRecord> }`. For the calling app only. Its certificate must be the one the newest `identity.json` lists, else `403 not_current`. The record the service already holds, byte for byte, is unchanged (7.4 no-ops). Any other record must have an `updated_at` later than the held record's, else `409 conflict` with `current: { "record": <the held signed record> }`. So a replayed or delayed older record, such as one naming a destination since withdrawn, never replaces the current one, and the switch never holds a second record at a time readers have seen another body at (3.2 Equal times). A device of the app that gets this answer for a record of its own writes it again with an `updated_at` later than the held record's. Readers apply the same rule (3.2: the later `updated_at` wins). Vectors: the `app_put` steps of `claims.json`.

**`receive_key.put`** (spec 3.7): `{ "record": signed<ReceiveKeyRecord> }`. For the calling app only, after its `app.put` (earlier in the same batch, or before), else `400`.
- The first record of an app at a service has `n` 0; at an account recovering from a provider loss, any `n`, and apps send one higher than every `n` they have registered (spec 3.7), so they register a new record rather than re-deliver their current one. Each later record has `n` exactly one more than the current one. The current record again, byte for byte, is unchanged (7.4 no-ops). Anything else is `409 conflict` with `current.receive_key`, which the app checks for an unexpected registration (3.4).
- A kid already registered, by any app, is `400` (spec 3.7: kids are unique in the account).
- A key of low order (4.2), a `suite_keys` of the wrong form (3.4) or a record that fails 3.4's checks otherwise is `400`.

**`key_bundle.put`** (spec 3.6): `{ "target_app_id": app-id, "bundle": key bundle (3.5), "first_epoch": int, "last_epoch": int }`. Signer only (`app_id: null`), else `403 permission_missing`. The target must be current, else `403 not_current`. A bundle whose signature fails is `401 bad_signature`. `first_epoch` and `last_epoch` repeat, in the clear, the epochs the sealed bundle holds (3.5; `first_epoch` at most `last_epoch`, `400` otherwise); the service uses them for `keys_through` (8.4.12) and clears a pending full-bundle request once the parts it stores for the target cover every data epoch from 1 to the current one. A bundle may be one part of several (3.5). The service stores the target's parts so that they never overlap: a new part replaces every stored part whose range it overlaps, and, unless a full-bundle request is pending, the parts the target's cursor has passed and that `keys_through` covers go. A part identical to one stored, with the same epochs, changes nothing (7.4 no-ops).

**`cursor.set`** (spec 8.4): `{ "op": "cursor.set", "position": "<generation>:<seq>" }`, unsigned, not a signed document, and outside the batch count. The request signature covers it. Apps only; at most one per batch. The position's generation is the current one (or, after a cooperative move, the one the sequence continues from), and its `seq` is at most the newest; else `400`.

**`cert.put`** (spec 3.2; 3.9): `{ "target_app_id": app-id, "cert": signed<AppCert> }`, the target's renewed certificate. Signer only (`app_id: null`), else `403 permission_missing`.
- The certificate must verify for the identity and name `target_app_id` (`400`).
- The target must be known to the service, not revoked or departed, and have delivered its record (`404`).
- It keeps the record's certificate's `signing_key` and `bundle_key` (`400`: other keys make a new app).
- Its `issued_at` is later than that of the record's certificate and of any renewed certificate already stored for the app (`409 conflict`); the stored renewed certificate, or without one the certificate the record carries, again byte for byte, is unchanged (7.4 no-ops).
- It replaces a renewed certificate stored earlier, whose chain item then keeps only its stub. It is a control operation (Section 10).

Example (`operations.json` `cert.put`; certificate cut short):

```json
{
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "app_id": null,
  "generation": 1,
  "batch": {
    "id": "9G8n3RwiijmAxZku6M5rDA",
    "count": 1
  },
  "op": "cert.put",
  "target_app_id": "wejf37ngr7qizej3eoijn3hq4c",
  "cert": {
    "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcF9jZXJ0...",
    "sig": "NxeOgFzQFaAWSiJdJPhpshRB0Cxyq34q0jw0F_4E_Qe1mUYFGv_Qi906LD7QsjSp2xOYOZPZTPBw6MlTknM4BQ"
  }
}
```

### 7.3 Conditions
Each condition is an object with one member, checked against the state before the batch (spec 10.1):

| Condition | Form | Holds when |
|---|---|---|
| `record_version` | `{ "record_version": { "key": b64(32), "generation": int, "version": int } }` | the record's current version is (`generation`, `version`), its version being the (generation, `seq`) of its current write (7.2). A record that doesn't exist, or whose key the service forgot, is at version 0, whatever the generation named, and `version: 0` holds only then |
| `claim_step` | `{ "claim_step": { "claim_key": b64(32), "step": int } }` | the claim's step is `step` (0 when the service holds nothing for it) |
| `server_time_in` | `{ "server_time_in": { "start": time \| null, "end": time \| null } }` | the service's time when it applies the batch is in `[start, end)`; a `null` bound is open |

A failed condition is `409 conflict`, with the current value the caller may read (8.6). Unknown members, in the condition object or inside it, are `400`.

### 7.4 Batches
- **Request:** `POST /v1/{id}/ops` with body `{ "ops": [entry, ...] }`, 1 to 8 entries, each a signed operation in carrying form or one `cursor.set`. The cursor counts toward the 8. An entry has only its own members: `body` and `sig`, or `cursor.set`'s `op` and `position` (1.1).
- **Processing order:**
  1. Callers: own apps and the signer; apps of other identities get `403 not_admitted`.
  2. Parse; 0 or more than 8 entries, or an entry with a member besides its own, is `400`, and a signed operation over 512 KiB is `413 too_large`, before any signature is checked. The size is that of the operation's compact carrying form (7.1), 20 bytes plus its `body` and `sig` texts, not of the bytes the request carries for it.
  3. For each signed operation: its signature with the caller's key (`401 bad_signature`), then `identity` and `app_id`, and no unknown members at any depth (`400`, 7.1).
  4. Batch: no record written twice (7.2); `id` is 16 bytes; every operation has the same `id` and `count`, and `count` is the number of signed operations (`400`).
  5. Retry: a batch whose ID the service holds with the same signed operations returns the original proofs and the newest head while the service keeps them, `batch_proofs_ms` (30 days, Section 10). After that, and with other operations at any time, it is `409 conflict`. The service holds a batch's ID and operation hash as long as the proofs, and also while any of the batch's chain stubs is kept (10.1). The batch record goes once both have ended: its proofs are gone and none of its stubs is kept. An app's verifier takes a new batch for a repeat when its ID is that of an earlier batch and `t_new - t_old < 30 days` (2,592,000,000 ms), `t_new` and `t_old` being the `server_time` of each batch's first stub (7.6). The service holds a batch's ID at least until 30 days after its first stub's `server_time`, and refuses the ID while it holds it. So the service never accepts a batch an honest app would take for a repeat, and an app never compares an ID the service has forgotten. Nobody can replay a batch, list operations included, which carry no expected version (`allow.*`, `deny.*`, `invite_code.*`): a request is accepted only within 5 minutes of its signed timestamp (8.1), so a batch can come again later only in a new request, which only its app can sign, and apps resend a batch only in their immediate retries.
  6. Generation: an operation naming another generation is `409 stale_generation`, with `current.generation`. A retry recognized in step 5 never reaches this check.
  7. No-ops, time and rates: a batch without `cursor.set` whose every signed operation is unchanged (below) is a no-op; without trusted time, a batch that depends on time is `503 unavailable` (8.3); then the operation and control rates (below): `429 rate_limited`. The no-op decision comes before the conditions (step 8) and before each operation's permission and rules (step 9), since a no-op changes nothing and answers only `{ "proofs": [], "head" }`, which no caller reads as a guard having held. So a no-op batch whose operations would fail there gets that answer, such as one carrying a failing condition. (An `app.put` whose certificate the newest `identity.json` no longer lists never gets this far: authentication checks a batch carrying the caller's `app.put` against that record's certificate, 8.2.) A batch carrying `cursor.set` is never a no-op, since the cursor move changes something: its operations go through steps 8 and 9 whether unchanged or not.
  8. Conditions of every operation, against the state before the batch.
  9. Operations in order, each against the state the earlier ones left, so one batch can write two entries of one claim (but not two writes of one record, 7.2). For each operation, in this order: its permission (`403 permission_missing`), its shape (`400`: field lengths, a claim's `deadline` and `evidence_ref`), the per-kind limits (`429 quota_exceeded`, Section 10), then its own rules (7.2, 7.8; `409`).
  10. The cursor, only once every condition and operation has passed. When every signed operation of the batch is unchanged, it then adds no chain item and no batch record, and the answer is `{ "proofs": [], "head" }`.
  11. Commit, or nothing at all.
- **Response:** `{ "proofs": [transition proof, ...], "head": signed head }`, one proof per signed operation, in order. For example (two proofs, cut short):

```json
{
  "proofs": [
    {
      "body": "eyJjaGFpbiI6eyJlbnZlbG9wZSI6ImV5SmphR0Zw...",
      "sig": "9SdlSo7HN1yrd-XQ09LgtFaX7k40vzSMGduCzSU1-kjFvg8F2n_WsiqjKuwaXQUdacChwz7PHZb_IzqLwkLYCA"
    },
    {
      "body": "eyJjaGFpbiI6eyJlbnZlbG9wZSI6ImV5SmphR0Zw...",
      "sig": "YcI39K84Lhe_sPZ7LCtE8FE8Z7Ai3Rt1Jho3hoKRqPcPFPw8gDthsROJTRnH6-7fZ3FCB9ap_QRwfJcWAULMCw"
    }
  ],
  "head": {
    "chain_version": 1,
    "generation": 1,
    "seq": 9,
    "chain_hash": "Rw2wSFLDi3ZcYiO4nQ6LR0c2OQaJlJxxvlQ8WxM3IVE",
    "sig": "CBiJ7qLNo481i40mFyb7KOjr7NaYMxySzGX4btJK5S72NQ5VF-RZxAEd9YJ00kJNSXwZm-lxe6vByX-dD0hqBQ"
  }
}
```

- A batch of only `cursor.set` has no batch ID, takes no `seq` and is applied again on every retry.
- The operations of one batch take consecutive `seq`s, and their chain envelopes carry the batch ID and count (7.6).
- **No-ops** (spec 6.3). An `app.put` of the record the service holds for the app, a `receive_key.put` of the app's current receive key record, a signer's `cert.put` of the renewed certificate the service stores (or of the certificate the app's record carries), and a signer's `key_bundle.put` of a part it stores with the same epochs, each byte for byte, are unchanged. A batch whose every signed operation is unchanged, an **unchanged batch**, creates no chain item and no batch record and counts against no rate, and its answer, when it succeeds, is `{ "proofs": [], "head" }`. Without `cursor.set` it is a no-op, answered before the conditions and checks (step 7). With `cursor.set` it isn't a no-op: its conditions and operations are checked first, as in any batch, and the cursor moves only if they all pass (step 10); a failing condition is `409 conflict`, and the cursor stays. Vectors: the `noop_batch` steps of `claims.json`. In a batch with any other operation, an unchanged one is chained like the others, since the batch's `count` names it, and changes nothing: an unchanged `receive_key.put` registers nothing, and its proof shows the current `n` and `kid`.
- **Operation and control rates** (Section 10). A batch from an own app that carries an ordinary operation, or a repeatable control mutation, counts against that app's operation rate: `ops_per_min`, 3,000 batches a minute by default. A batch that carries a repeatable control mutation also counts against its control rate, `control_per_min`, 60 a minute by default. The repeatable control mutations are an `app.put` or a `receive_key.put` that changes what the service holds (a new app record, a new registration), and a `record.put` that deletes, an `allow.delete` or a `deny.delete` of something that isn't there. An unchanged `app.put` or `receive_key.put` (No-ops, above) counts against no rate, also in a batch with other operations, where it is chained like them. Both "unchanged" and "something that isn't there" are judged against the state before the batch, as the conditions are (step 8), so an `allow.put` and an `allow.delete` of one entry in one batch count the delete as removing nothing. Both rates use fixed one-minute windows of service time; beyond one the batch is `429 rate_limited`, with `retry_after` the seconds to the next window. The other control operations never count: `cursor.set`, a claim entry other than `attempt`, a `record.put`, `allow.delete` or `deny.delete` that removes something, and `invite_code.delete`. Signer batches never count. Together the two rates bound how fast chain stubs and batch records grow, while a full inbox can still be drained, an existing payment reconciled and an abusive app revoked.

Example (the registration batch of spec 13.1 step 2: `app.put`, the first `receive_key.put` and the cursor):

```json
{
  "ops": [
    {
      "body": "eyJpZGVudGl0eSI6IjZmcmFvOHc5c3M1Y241NDRx...",
      "sig": "LNyTBxdBqrzhhnZsLDFK4IckHBScMVclt86dSAqkmU7snEnrbM1pgvUXoqkRVgcA8GC20x0Ci42-zeSppM4jCA"
    },
    {
      "body": "eyJpZGVudGl0eSI6IjZmcmFvOHc5c3M1Y241NDRx...",
      "sig": "ikD0Pr8HcVxwHCCoY-v4J2wJtggZdDs38D09MG_DmP6McoUcADtBmgXztYysGm8ekLaXVIXi6CDmkl5zPwxEBA"
    },
    {
      "op": "cursor.set",
      "position": "1:0"
    }
  ]
}
```

### 7.5 Transition proofs and service transitions
**Transition proof:** a signed document (label `paykit/v1/proof`, fields: identity, body) with this body:

| Field | Type | Req | Notes |
|---|---|---|---|
| `chain` | stub | yes | the item's chain stub (7.6) |
| `object` | string | yes | per item, below |
| `step` | int | no | claims: the claim's step after the entry |
| `version` | int | no | per item, below |
| `state` | object | yes | per item, below |

| Item | `object` | `step` / `version` | `state` |
|---|---|---|---|
| `claim` | the claim key | `step` | the claim view (7.8) |
| `record.put` | the record key | `version`: the write's `seq`, the record's new version | `{ "generation", "version", "deleted" }`: the new version (7.2) |
| `allow.put`, `allow.delete` | the Pubky ID | | `{ "kind", "present" }` |
| `deny.put`, `deny.delete` | the Pubky ID | | `{ "denied" }` |
| `invite_code.put` | the code hash | | `{ "max_uses", "uses": 0 }` |
| `invite_code.delete` | the code hash | | `{ "withdrawn": true }` |
| `app.put` | the App ID | | `{ "app_id" }` |
| `receive_key.put` | the App ID | `version`: `n` | `{ "n", "kid" }` |
| `key_bundle.put` | the target App ID | | `{ "bundle_seq" }` |
| `cert.put` | the target App ID | | `{ "cert" }`: the renewed certificate's document hash |
| `identity.json` delivery | the literal string `identity` | `version`: the document's | `{ "version", "generation", "data_epoch" }` |
| service transition | per transition, below | | the transition body |

**Binding** (spec 10.4): an answer to `POST /ops` is verified only when its proofs bind the batch: one proof per signed operation, in order, or none for an unchanged batch (7.4): a no-op, or a batch with `cursor.set` whose unchanged operations passed their checks. Only `app.put`, `receive_key.put`, `key_bundle.put` and `cert.put` can be unchanged. Each proof verifies with the service key, its stub's `item_hash` is that of its operation, and its `object` is the operation's per the table above. Before paying, the proof of the app's `attempt` must also show `step` one more than the `expected_step`, and a claim view with `state` `ATTEMPTING`, `owner` and `attempt_id` the app's own, and that attempt `open`; it verifies with the service key the app's own `identity.json` names, and its stub's `generation` is the one that document names (spec 13.5 Fencing). An answer that fails these checks is unverified, so no money moves on it, and as a signed contradiction it is misbehaviour, handled as a rollback (spec 10.1); an `attempt` whose proof binds the operation but shows another state is written `failed`. A proof that comes with a feed item or a record view is checked the same way (7.2 Accepting a write).

**Service transition:** a signed document (label `paykit/v1/service-op`, fields: identity, body), a chain item of type `service` that the service writes itself. Bodies:

| `type` | Other fields | Proof `object` | Written by |
|---|---|---|---|
| `revoke` | `app_id`, `at` | the App ID | `POST /apps/{app_id}/revoke` |
| `leave` | `app_id`, `at` | the App ID | `POST /apps/{app_id}/leave` |
| `freeze` | `freeze_hash`, `next_generation`, `inbox`, `at` | `freeze` | `POST /freeze` |
| `unfreeze` | `freeze_hash`, `at` | `unfreeze` | `POST /unfreeze` |
| `recovery` | `recovery_from`, `at` | `recovery` | the first item at a new account after a provider loss |
| `admission` | `by: "signer"`, `at` | `admission` | `POST /recovery/admission` |
| `upload` | `by` (App ID), `changes` (8.4), `proofs` (count), `at` | `upload` | `POST /recovery/upload` |
| `conversation.add` | `pubky`, `code_hash`, `context`, `at` | (no proof is returned) | the first append admitted by an invite code |

**Import.** A provider move writes `freeze` (and, on an abort, `unfreeze`) at O; the import writes no transition of its own (Section 11). N's imported chain is O's chain, ending at O's `freeze` transition, which the manifest's head names. N's first own item is the delivery of the `identity.json` naming it, at generation g+1: it continues O's `seq` and chain hash, so an app's cursor and newest head stay valid (8.4.4). An abort at N writes nothing.

Example proof (`chain.json` "claim attempt (seq 7)", the `attempt` of 7.1; the claim view shows the deadline it pinned):

```json
{
  "chain": {
    "envelope": "eyJjaGFpbl92ZXJzaW9uIjoxLCJnZW5lcmF0aW9u...",
    "chain_hash": "_zpF2N6WLmIE58x2YP6HaiBCM6leO83GPX5Hkyz6wOI"
  },
  "object": "gf5fn_W2IzE4B7i6A24-u5giYwJnARfTQxzPyx-ZQa0",
  "step": 1,
  "state": {
    "attempt_id": "DgCVYQKL1eyDacZvG6YhZg",
    "attempts": [
      {
        "attempt_id": "DgCVYQKL1eyDacZvG6YhZg",
        "duplicate": false,
        "opened_at": "2026-01-01T00:00:03.000Z",
        "orphan_after": 86400,
        "orphaned": false,
        "outcome": "open",
        "owner": "wejf37ngr7qizej3eoijn3hq4c",
        "possibly_duplicate": false
      }
    ],
    "claim_key": "gf5fn_W2IzE4B7i6A24-u5giYwJnARfTQxzPyx-ZQa0",
    "deadline": "2026-01-02T00:00:00.001Z",
    "divergent": false,
    "next_possibly_duplicate": false,
    "owner": "wejf37ngr7qizej3eoijn3hq4c",
    "state": "ATTEMPTING",
    "step": 1,
    "tombstone": false
  }
}
```

### 7.6 Chain envelopes and hashes
Every signed operation, every `identity.json` delivery and every service transition is a chain item (spec 8.4, 14.6). Inbox entries and cursor moves are not, so the `seq`s of chain items may have gaps.

- **Item hash:** `hash(lp("paykit/v1/item", item type, item bytes))`, with item type `op`, `identity` or `service`, and item bytes the signed document bytes (2.2).
- **Chain envelope**, written by the service and kept as exact bytes:

  | Field | Type | Req | Notes |
  |---|---|---|---|
  | `chain_version` | int | yes | 1 |
  | `generation` | int | yes | |
  | `seq` | int | yes | the item's position in the sequence |
  | `prev_chain_hash` | `b64(32)` | yes | the previous chain item's chain hash; 32 zero bytes before the first item |
  | `server_time` | time | yes | when the service applied it |
  | `item_type` | string | yes | `op`, `identity` or `service` |
  | `item_hash` | `b64(32)` | yes | |
  | `batch_id` | `b64(16)` | operations only | |
  | `batch_count` | int | operations only | |

- **Chain hash:** `hash(lp("paykit/v1/chain", prev_chain_hash, envelope bytes))`.
- **Stub:** `{ "envelope": "<base64url of the exact envelope bytes>", "chain_hash": b64(32) }`.
- **Checks** an app makes on each stub, in feed order: the chain hash follows from the envelope; `seq` is above the last one; `prev_chain_hash` is the last chain hash; the generation is the one followed, or higher after a cooperative move; `server_time` isn't before the previous stub's, else it is a rollback, since the service never stamps earlier than the last time it committed (spec 10.1), though equal times are fine, as a batch's stubs may share one; a batch's items have consecutive `seq`s with one ID and count until `count` items are seen; a new batch's ID isn't that of an earlier batch with `t_new - t_old < 30 days`, exactly 2,592,000,000 ms, `t_new` and `t_old` being the `server_time` of each batch's first stub (the prototype's `BATCH_IDS_COMPARED_FOR_MS`); at 30 days or more the earlier batch isn't compared, and the app forgets it, whatever checkpoints it sees (7.4 step 5, spec 14.5). `chain.json` `batch_id_window` has the cases 1 ms before, at and 1 ms after 30 days. Right after an app continues from a checkpoint it has no previous time, so the next stub's isn't compared (8.4.4). A full item must match its stub's `item_type` and `item_hash`; one that doesn't is a rollback (spec 10.1), and the app doesn't act on it.

Example envelope (the `attempt` of 7.1, at `seq` 7: an inbox entry at `seq` 5, the record's create at `seq` 6):

```json
{
  "chain_version": 1,
  "generation": 1,
  "seq": 7,
  "prev_chain_hash": "S6AUZQIwgoWFs_81MXoXvwOGn_sZOVuk-cdZ7SP85EA",
  "server_time": "2026-01-01T00:00:03.000Z",
  "item_type": "op",
  "item_hash": "MFpMs1zhSS6MjJWM_G0uxBeWRQPb8Zvcaek5OMF0hZg",
  "batch_id": "X8mxmFl3oL9HfIUnsds0GQ",
  "batch_count": 2
}
```

### 7.7 Signed heads
A signed head is a JSON object (not a signed document):

| Field | Type | Notes |
|---|---|---|
| `chain_version` | int | 1 |
| `generation` | int | |
| `seq` | int | the newest chain item's `seq`, 0 before any |
| `chain_hash` | `b64(32)` | its chain hash; 32 zero bytes before any item |
| `sig` | `b64(64)` | service key, label `paykit/v1/head`, over `lp(label, identity, u64(chain_version), u64(generation), u64(seq), chain hash)` |

- In a header (`Paykit-Head`) the head travels as base64url of its JSON bytes.
- An app keeps the newest head it has seen. A head with a lower `seq` must match the chain hash the app verified at that `seq`; a head beyond the chain verified so far is kept until the stubs reach it, and is a rollback if a read that reaches the end of the feed still hasn't reached it (spec 10.1).

```json
{
  "chain_hash": "Rw2wSFLDi3ZcYiO4nQ6LR0c2OQaJlJxxvlQ8WxM3IVE",
  "chain_version": 1,
  "generation": 1,
  "seq": 9,
  "sig": "CBiJ7qLNo481i40mFyb7KOjr7NaYMxySzGX4btJK5S72NQ5VF-RZxAEd9YJ00kJNSXwZm-lxe6vByX-dD0hqBQ"
}
```

Vectors: `chain.json` builds a chain of eight items with a gap (the first is the account's first `identity.json`, 3.3), their envelopes, hashes and proofs, two heads, and chains the verifier must reject: a partial batch and a repeated batch ID, both with intact links, so only the batch rules reject them; a batch cut off at the end of a page; a gap that breaks the link; and a forked head. A verifier checks in the order of the list above, so a broken link is reported before a batch rule.

### 7.8 Claim states
The **claim view** is the `state` of claim proofs, the `claim` of `GET /claims` and the `current.claim` of conflicts:

| Field | Type | Notes |
|---|---|---|
| `claim_key` | `b64(32)` | |
| `state` | string | `FREE`, `ATTEMPTING` or `SETTLED` |
| `owner` | app-id | while `ATTEMPTING`: the current attempt's owner |
| `attempt_id` | `b64(16)` | while `ATTEMPTING`: the current attempt |
| `step` | int | the number of entries |
| `attempts` | `array<object>` | every attempt, oldest first, below |
| `tombstone` | bool | a compacted finished claim |
| `deadline` | time | the deadline the first `attempt` pinned, on a claim or a tombstone; absent when none was pinned or the pin is "none" |
| `divergent` | bool | uploads after a provider loss conflict (spec 13.6); default `false` |
| `next_possibly_duplicate` | bool | the next `attempt` would be flagged `possibly_duplicate` (row 1), so no app attempts it automatically; default `false`. Always `false` on a SETTLED claim, live or a tombstone, since row 7 refuses every later attempt |

An attempt: `attempt_id`, `owner` (app-id), `outcome` (`open`, `done`, `failed` or `unknown`), `opened_at` (time of its `attempt` entry), `orphan_after` (seconds), `orphaned` (bool, at the time of the answer), `possibly_duplicate` (bool), `duplicate` (bool), and `resolved_by` (app-id, absent while open): the app that wrote its outcome.

The rules (spec 10.2):

| # | Entry | Applies when | Writer | Effect |
|---|---|---|---|---|
| 1 | `attempt` | no attempt is open and not SETTLED; `attempt_id` new for the claim; the service's time is before `deadline`; `deadline` equals the pinned one, if any | any app with `claims.write` | a new open attempt, current; the first one pins the claim's deadline, "none" included; `possibly_duplicate` if an earlier attempt is `unknown`, or its `failed` or `unknown` was written by an app other than its owner. A `done` attempt, by any basis, settles the claim, so no attempt follows it (row 7) |
| 2 | `done`, basis `owner` | the attempt is `open` or `unknown`; or `failed` on a claim rebuilt from uploads after a provider loss | its owner | done; SETTLED; `duplicate` if another attempt is done |
| 3 | `done`, basis `evidence` | the attempt is `open`, `unknown` or `failed`, on a claim or a tombstone | any app with `claims.write` | as row 2 |
| 4 | `done`, basis `reconcile` | the attempt is `open` and orphaned | any app with `claims.write` | as row 2 |
| 5 | `failed` | the attempt is `open` or `unknown` | its owner, or any app once the owner isn't current | failed; FREE if it was the current attempt |
| 6 | `unknown` | the attempt is `open` and orphaned, not SETTLED | any app with `claims.write` | unknown; FREE |
| 7 | `attempt` or `unknown` | SETTLED | | `409 conflict` |

- **Orphaned:** an open attempt whose owner isn't current (revoked, departed, not listed, or with an expired certificate), or whose `orphan_after` has passed (`now >= opened_at + orphan_after`), or which came from a recovery upload.
- **Conflicts** (`409 conflict` with `current.claim`): a stale `expected_step`; an `attempt` while another is open; an `attempt` naming another deadline than the pinned one, or none where one is pinned; a writer the row doesn't allow; an entry for an unknown `attempt_id`; a repeated `done` for a done attempt, which also stops the rest of its batch; any entry on a tombstone other than row 3.
- **Deadline:** an `attempt` at or after its `deadline` on the service clock is `409 conflict` with `current.server_time` and `current.claim`, whatever the claim's state. This check comes first, before `expected_step` and the row rules. Apps don't send an `attempt` once its deadline is at or before the highest service time they have verified, and a transition proof stamping an `attempt` (7.6 `server_time`) at or after its deadline is misbehaviour (spec 10.2).
- **Pinned deadline and evidence** (spec 10.2, 10.4): an app keeps evidence for an obligation, its SETTLED and tombstone proofs, until the later of the bounds that apply:
  - the later of the pinned `deadline` and the one it computes itself, where there is one, plus the 30-day reconciliation window during which the service keeps the tombstone (below);
  - with a pin to "none", `max_claim_age` (400 days) after the proof's time, the time its stub carries, as the service keeps that tombstone 400 days after its last entry, which is no earlier.

  So with a pin to "none" beside a deadline of its own, it keeps the evidence to the later of its deadline plus 30 days and the proof's time plus 400 days. A pinned deadline where it computes none (it holds the request, which sets no deadline) is an early pin (below), and it keeps that evidence for good: it may still pay the obligation, and the evidence shows it paid once the service has deleted the tombstone. It drops the evidence only once every bound lies before the earlier of the highest verified service time and its device clock plus 24 hours: it is kept at the boundary itself and dropped from the next millisecond. Until then it compares the evidence with the claim and uploads it in a move (8.4.14) or after a provider loss. A service that shows nothing for it is a rollback (spec 10.4), except from the moment the service may delete the tombstone, the pinned deadline plus 30 days, or with a pin to "none" the proof's time plus 400 days, though the app still holds the evidence at that instant. From then until the app drops the evidence, the pin differs from the app's own deadline, early or "none" beside a deadline: the app alerts, and the evidence still shows the obligation paid, so it doesn't pay it again. A pin earlier than the app's own deadline is an alert and makes the obligation doubtful.
- **Order of the checks** for one entry: permission and shape (7.4 step 9), then for an `attempt` the claim limits (Per app, below), then the deadline, then `expected_step`, then the row rules in table order.
- **A SETTLED claim with an open attempt** (evidence settled another attempt while one was open) keeps the open attempt: its owner may still report `done` (flagged `duplicate`) or `failed`, and it isn't compacted until every attempt is `done` or `failed`.
- **Malformed** (`400`): `attempt` without `orphan_after`; `done` without `basis`; basis `evidence` without `evidence_ref`; `evidence_ref` on a type other than `done`; `deadline` on another type; a claim key or attempt ID of the wrong length.
- **Tombstones** (spec 6.3): a claim that is SETTLED with every attempt `done` or `failed` is compacted at once. It keeps its step and attempts; its deadline is the pinned deadline. An `attempt` is refused from that deadline itself (above), but the service keeps the tombstone for the reconciliation window, 30 days (2,592,000,000 ms; the prototype's `CLAIM_RECONCILIATION_MS`), and deletes it once the time base, with trusted time, reaches the deadline plus that window (8.3 step 4, Appendix A); then `GET /claims` is `404`. A tombstone pinned to "none" is kept until the time base, with trusted time, reaches its last entry plus `max_claim_age` (34,560,000,000 ms, 400 days; the prototype's `MAX_CLAIM_AGE_MS`), and deleted the same way.
- **Failed claims** (spec 6.3): a claim that never settled, whose every attempt is `failed`, is kept the same way: to its pinned deadline plus 30 days, so a late receipt can still record `done` on a `failed` attempt (row 3) and an Allowance ledger still sees it; then it is deleted. One pinned to "none" is kept 400 days after its last entry, then deleted. A claim with an open or `unknown` attempt is never deleted by age. After the window a late receipt can't be recorded: the app that gets it shows the user that a payment marked failed was paid, and an executor marks its own ledger entry `done` (6.5).
- **Doubtful claims:** apps never attempt automatically a claim with an `unknown` or orphaned attempt, or with `next_possibly_duplicate` (spec 10.2); executors treat them as doubtful (spec 11.3).
- **Per app** (spec 6.3, 10.2): the app whose `attempt` creates a claim is its creator, which the service records and an export carries (11.3): the owner of the claim's first attempt, also for a claim rebuilt from uploads. The claims one app created, live and tombstones, are at most its **claim share**, `floor(max_claims * claim_share_percent / 100)` claims (250,000 at the defaults; 3 for `max_claims` 12, and 2 for `max_claims` 10). The attempts one app owns that are `open` or `unknown` are at most `open_attempts_per_app` (1,000), counting every live claim. Outcomes are never refused. An `attempt` is checked against the claim limits in this order, after its permission and shape and before the deadline and the claim rules (7.4 step 9), and the first it would exceed answers `429 quota_exceeded` (10.3) with that limit in `current.limit`:
  1. `claims`: it would create a claim (its key is neither a live claim nor a tombstone), and the live claims and tombstones number `max_claims`;
  2. `claim_share`: it would create a claim, and the claims its app created number its claim share;
  3. `attempts`: the live claim holds `max_attempts_per_claim` (64) attempts;
  4. `open_attempts`: its app owns `open_attempts_per_app` attempts `open` or `unknown`.

  So a new claim within its app's share gets `open_attempts` when the app is at that limit, and `claim_share_percent` 100 turns the share off, since `claims` answers first. A revoked app's open attempts stay, orphaned, until an own app resolves each (rows 3 to 6, spec 10.5).
- **The record shows it paid** (spec 10.4): a claim pinned to "none" is gone 400 days after its last entry, so a re-sent copy of its request finds the claim FREE. An app never attempts an obligation whose activity record it holds carries the proof of a payment (6.3), so the record, not the claim, stops a second payment from then on. A payer therefore never deletes its record of a paid obligation without a deadline (spec 8.3, 10.2): the user may archive it, which hides it in the app, and the record stays whole.

Vectors: `claims.json` runs 69 steps through every row above, the deadline pin and the reconciliation window (a failed claim kept past its deadline, settled by a late receipt, and deleted at the deadline plus 30 days), with the expected claim view or error after each one; its `noop_batch` steps are the no-op cases of 7.4, and its `app_put` steps those of `app.put` (7.2). Its `quota` steps run the two per-app limits on a switch of their own, with `max_claims` 12 and `open_attempts_per_app` 2: an app's third open attempt is `429 open_attempts`, its fourth claim `429 claim_share`, and another app creates that claim.

## 8. HTTP API

### 8.1 Requests and responses
**Transport.** HTTPS. Paths start with `/v1/`; `{id}` is the account's Pubky ID. JSON bodies are sent as `application/json`; the body of an append is the raw envelope (`application/octet-stream`). Header names are case-insensitive; the prototype sends them in lower case.

**Switch origins** (spec 6.1). A client connecting to a switch origin it read from an `identity.json` checks it with the classifier of 9.2: `https` only, no user info, path, query or fragment; port 443, or another port only where the client's operator allows it; an address literal, and every address the host resolves to, globally reachable (9.2); the connection goes to an address it checked, and redirects aren't followed. The prototype's `transport::Http::public_only` does this (its resolver refuses a host with any address that isn't public); `Http::new` doesn't, for runs on loopback.

**Query parameters** are `name=value` pairs joined by `&`. The service percent-decodes names and values. The request signature covers the path and query exactly as sent, so a client builds the query once and doesn't re-encode it.

**Request headers:**

| Header | Sent by | Value |
|---|---|---|
| `Paykit-Identity` | every caller | the caller's identity, as a Pubky ID |
| `Paykit-App` | apps | the calling App ID; absent for signer calls |
| `Paykit-Cert` | apps calling another identity's account, and the staged head upload of 8.2 | the app's certificate in compact form (2.2) |
| `Paykit-Invite-Code` | optional (spec 7) | an invite code (1.8) |
| `Paykit-Transfer` | the next service fetching export chunks | the signer's export transfer in compact form (2.2, 3.12) |
| `Paykit-Timestamp` | every caller | the signing time, `YYYY-MM-DDTHH:MM:SSZ` (1.6) |
| `Paykit-Signature` | every caller | `b64(64)`, the request signature |

**Request signature** (label `paykit/v1/http`): `lp(label, method, path and query, hash(body), Paykit-Timestamp, switch origin, Paykit-Identity, Paykit-App, Paykit-Cert, Paykit-Invite-Code)`. A next service's request carries `Paykit-Transfer` and no `Paykit-App`, and is signed over `lp("paykit/v1/transfer-request", method, path and query, hash(body), Paykit-Timestamp, switch origin, Paykit-Identity, Paykit-Transfer)`.
- The method is upper case; the body hash is over the raw body bytes, so a request without a body signs `hash` of the empty string.
- The **switch origin** is the service's origin exactly as `identity.json` names it in `inbox.url` (for an enrollment, the origin the signer enrolls at), for example `https://inbox.example`. It isn't sent; both sides know it.
- Header values are signed as their exact text, and an absent header as an empty field.

**Timestamp:** an unparsable or missing `Paykit-Timestamp` is `400 bad_request`; one more than 5 minutes from the service's time base (8.3) is `401 stale_timestamp`. The time base is trusted time, or the wall clock while it agrees with trusted time; not the identity's stamped time, which never goes back, so neither a jump of the service's wall clock nor a corrected one locks out apps with correct clocks (spec 10.1).

**Response headers:**

| Header | Present | Value |
|---|---|---|
| `Paykit-Service-Signature` | every response | `b64(64)`, the response signature |
| `Paykit-Server-Time` | every response | the identity's service time, three fraction digits (1.6) |
| `Paykit-Head` | responses to own apps and the signer, except to an enrollment (8.4.1) and an erasure (8.4.16) | base64url of the newest signed head's JSON (7.7) |
| `Retry-After` | `429` | seconds |
| `Content-Type` | every response | `application/json` |

**Response signature** (label `paykit/v1/http-response`): `lp(label, u64(status), hash(request signature bytes), Paykit-Server-Time text, hash(body))`.
- The request signature bytes are the 64 bytes decoded from the request's `Paykit-Signature`, or empty when it has none that decodes (2.3).
- It is made with the account's service key. A response the service can't tie to an account (an unknown account or route, or an enrollment error before the account exists) is signed with an operator key no client knows, and clients treat it as unauthenticated.
- The signature covers the server time but not `Paykit-Head`, which carries its own signature, nor `Retry-After`, whose value is repeated in the body.
- A client checks the signature before it uses anything in the response. An executor takes `at` from `Paykit-Server-Time` only once it has checked it (spec 11.3).
- **Pinned key** (spec 6.1): a client checks with the account's service key it pinned, from the `identity.json` naming the service or, for the signer, from its enrollment answer (3.8) or the freeze naming the next service (3.7). A client without a pinned key sends only an enrollment; any other call fails closed before it is sent, and no answer is ever used unchecked. The prototype's `InboxClient` answers such a call with `ClientError::Unpinned`.

**Body caps.** Each call takes a body of at most its cap; a larger one is `413 too_large`, before the body is read or anything else is checked:

| Calls | Cap |
|---|---|
| `POST /inbox` | 65536 bytes (the envelope cap) |
| `POST /ops`, `POST /import` | 4 MiB |
| `POST /enroll`, `PUT /identity`, `PUT /peers` | 256 KiB |
| `PUT /import/chunks/{chunk_id}` | 1 MiB (the chunk size) |
| `POST /recovery/upload` | 16 MiB |
| `POST /freeze`, `POST /unfreeze`, `POST /import/activate`, `POST /recovery/admission`, `POST /apps/{app_id}/revoke`, `POST /apps/{app_id}/leave`, `POST /apps/{app_id}/keys`, `PUT /push`, `POST /erase` | 16 KiB |
| every `GET`, `POST /connect`, `DELETE /connect/{pubky}` | none (0 bytes) |

**Body rules:**
- Every body is a JSON object.
- Responses to apps of other identities never carry a `seq`, a head or other activity data. A `2xx` answer and a `409 identity_needed` get a member `known_version`: the `version` of the caller's identity's `identity.json` that the service holds, or `null` (spec 6.1). Other errors don't, so they tell the caller nothing of what the service holds for its identity. An app that copy lists in `removed` gets `null`, or the version it delivered (8.4.5), as a stranger would (8.2).
- Error bodies are `{ "code": string, "message": string, "current": object (where 8.6 says), "known_version": ... (only in `409 identity_needed`), "retry_after": int (on `429 rate_limited`) }`.
- Writes by own apps and the signer return `{ "proofs": [...], "head": signed head }`, the transition proofs of the chain items they wrote, in order.

Vectors: `http.json` has signed requests from an own app, an app of another identity (with `Paykit-Cert`, an invite code and `apps_version`) and the signer, and signed responses: two feed pages, a directory, a write answer, a `409 stale_key` and a `429` to another identity's app, the enrollment answer, and an operator-signed `404` to a request without a signature. The signer's erasure and its answer (8.4.16) are among them, and a key change that leaves out a needed service key with its `409 conflict` (8.4.2).

Example request headers (an append from bob's `shop` to alice's inbox):

```
POST /v1/6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy/inbox?apps_version=4
Paykit-Identity: 7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo
Paykit-App: brrz3m6g5rci6iqnzke44q4djc
Paykit-Cert: eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcF9jZXJ0...
Paykit-Invite-Code: IRrC78oNIjZ4hcet2fYnJQ
Paykit-Timestamp: 2026-01-01T00:05:01Z
Paykit-Signature: S_1i4_OePZHa2Bmoara9n18_aTMzMOEyZzsZ-Ugq3o14yVaWKpkMqZR51fzK680NeBoI-4NOmy7xrAGU-Y0kDQ
```

### 8.2 Callers and authentication
The service tells callers apart by the headers:

| `Paykit-Identity` | `Paykit-App` | Caller | Verified with |
|---|---|---|---|
| the account's identity | absent | the signer | the identity key |
| the account's identity | present | an own app | the signing key of the app record it delivered with `app.put` |
| another identity | present | an app of another identity | the signing key of the certificate in `Paykit-Cert` |
| another identity | absent | | `403 not_admitted` |
| the account's identity, with `Paykit-Transfer` | absent | the next service of a cooperative move, fetching export chunks | the next service's key the export transfer names (3.12) |
| absent | absent | anyone, for `GET /identity` only (8.4.15) | nothing |

- **An own app's first call** is a `POST /ops` whose batch carries its `app.put` (spec 13.1). The service looks only at the batch's first `app.put` naming the caller (a body of 1 to 8 entries), checks that its record's `signing_key` gives the caller's App ID (1.3) and that the request verifies with that key, and only then verifies the app record, its certificate and the operation's signature (`401 bad_signature` at the first that fails; a later `app.put` naming the caller doesn't count). Any other first call is `403 not_current`.
- **An own app** must be current: not revoked or departed, and listed with its certificate in the newest `identity.json` the service holds (`403 not_current`); its certificate must not have expired (`403 cert_expired`).
- **An app of another identity** must send `Paykit-Cert` (`400`). The certificate must verify for `Paykit-Identity` and name `Paykit-App` (`401 bad_signature`), the request must verify with its key (`401`), and it must not have expired (`403 cert_expired`). Admission (spec 7) and the listing check (`409 identity_needed`, 8.6) apply per endpoint.
- **A removed app is answered like a stranger** (spec 6.1). An app of another identity that the service's copy of that identity's `identity.json` lists in `removed` (and, at `PUT /peers`, one the delivered document lists there) gets, at every endpoint it may call, exactly the answer an app of an identity the service doesn't admit and holds no copy of would get: admission counts only an invite code, never a contact or conversation entry; appends and connect requests then get `409 identity_needed` with `current: { "version": null }` and `known_version: null`; `PUT /peers` answers as 8.4.5 says. So a revoked app's key can't learn which switches hold its identity's document, and so which identities correspond with it. Residual: a switch that still holds an older document listing the app treats it as current until it gets the newer one (spec 13.2 step 5); certificate expiry (3.1) bounds that too.
- **A staged head upload** (spec 13.5 step 3): before an import, a staged service holds no app records. An app then authenticates `POST /recovery/upload` with `Paykit-Cert`: the certificate must verify for the identity, name the app and its signing key, and be listed in the `identity.json` the service was enrolled with.
- **Invite codes** are checked and used only when nothing else admits the sender, and only an admitted append uses one; directory reads and `PUT /peers` use none.

**Order** (3.9). Cheap checks come before signature work, and signature work before anything that needs the account's state, so an unauthenticated request costs little and holds no lock: first the body cap (8.1), then, at an account the service holds, the network source's failed-authentication limit (`429 rate_limited`, below), then `Paykit-Identity` and `Paykit-Signature` decode (`400`, `401`) and the timestamp (`400`, `401 stale_timestamp`), then for every caller the order below, stopping at the first failure:
- **Signer:** the request signature with the identity key (`401 bad_signature`).
- **Own app:** for a batch carrying its `app.put`, the first-call order above; otherwise check 4 with the `signing_key` of the certificate it checks the app against (a stored renewed certificate once the newest `identity.json` lists it, else the record's). Then checks 1 to 3 on that certificate, not revoked or departed, and the certificate listed or in an open renewal (check 5, `403 not_current`), then check 6 on the record it holds (`403 not_current`).
- **App of another identity:** checks 1 and 2 on `Paykit-Cert` and check 4, then check 3 at the identity's service time. Check 5, against the service's copy of that identity's `identity.json`, comes per endpoint, for appends and connect requests (8.4.3, 8.4.7); an app whose renewal has closed gets `403 not_current` there. An app that copy lists in `removed` is answered like a stranger at every endpoint, before admission (above).
- **Staged head upload:** checks 1 to 4 on `Paykit-Cert`, then check 5 against the `identity.json` the service was enrolled with.
- **Next service:** `Paykit-Identity` is the account's and `Paykit-App` absent (`400` otherwise); the export transfer in `Paykit-Transfer` verifies with the identity key (`401 bad_signature`), then the request with the transfer's `to.service_key` over `paykit/v1/transfer-request` (`401 bad_signature`). Under the lock O checks that the transfer names this export (3.12, `403 not_admitted`); the call must be a chunk fetch (`403 permission_missing`). Its failed authentications are counted as a claim of their own, for monitoring.
- **Public read:** `GET /identity` (8.4.15) is answered with no authentication at all, after the body cap, before step 5 of 8.3, limited per network source and per account (10.4).
- **Failed authentications** (10.4): until a request verifies, nothing it claims is trusted, so the service limits it only by network source, an IPv4 address or an IPv6 /64, an IPv4 address mapped into IPv6 counting as the IPv4 address. Once a source has had `auth_failures_per_source_per_min` failed authentications in the current minute, at any accounts, the service answers every further authenticating request from it in that minute with `429 rate_limited` (so with the default 60, the 61st is refused). The check comes right after the body cap and the account lookup (8.3 steps 1 and 2), before any header is decoded, and applies to every call but the enrollment and the public read (8.4.15). Every answer `401` counts, whatever the step: a malformed `Paykit-Signature`, `401 stale_timestamp`, a signature that doesn't verify, the first-call checks and the next service's `401`s included; `400`, `403` and `429` answers don't count. The minutes are fixed minutes of the time base (8.3 Trusted time), the same for all accounts, not an account's stamped service time. The service also counts failed authentications per claimed caller (its `Paykit-Identity` and `Paykit-App`) at each account, for its operator's monitoring, but never refuses a request because of that count: anyone can claim any App ID, so such a refusal would let a stranger lock a victim app out. The limits per caller (10.4) apply only once a request has verified. A service that can't tell the source, such as the prototype's in-process transport, applies no limit here; behind a reverse proxy the source is the address the proxy reports.
- **Outside the lock:** everything up to the request signature needs no account state but an own app's key, so the prototype runs it before taking the service's state lock, looking up only that key under a brief lock, and checks under the lock that the key it verified with is still the one it holds.

Scopes and switch permissions come after authentication, then the endpoint's checks. Renewal (3.9) changes which certificate an own app is checked against from the delivery of the `identity.json` that lists the renewed one.

### 8.3 Processing order
For every request the service:
1. Routes it; an unknown route is `404`, signed with the operator key.
2. Finds the account; an unknown account is `404`, signed with the operator key. `POST /v1/enroll` is handled here (8.4).
3. Takes the identity's service time: the time base (below), or the last time it stamped if the time base is earlier (spec 10.1).
4. **With trusted time only** (below): deletes tombstones and failed claims whose deadline plus the reconciliation window has passed (7.8), by the time base, not by the stamped time of step 3, which a reading without trusted time may have moved ahead and which never goes back.
5. Authenticates the caller (8.2).
6. Records an own app's last-seen time.
7. **Staged:** an account that the newest `identity.json` doesn't name answers everything except `PUT /identity`, `POST /import`, `PUT /import/chunks/{chunk_id}`, `POST /import/activate`, `POST /unfreeze` (an abort, 11.6), `POST /recovery/upload` and `POST /erase` (8.4.16) with `503 staged`.
8. **Frozen:** a frozen account answers every write with `410 moved`, whose `current.freeze` is the signed freeze. Writes are `POST /inbox`, `PUT /peers`, `POST /connect`, `DELETE /connect/{pubky}`, `POST /ops` (including the record and cursor batch of receiving), `POST /apps/{app_id}/leave`, `POST /apps/{app_id}/revoke`, `POST /recovery/upload` and `PUT /push`. Reads go on, and so do the export (`GET /export`, `GET /export/chunks/{chunk_id}`), `POST /unfreeze` and `POST /erase` (8.4.16). Feed pages of a frozen account carry the freeze (8.4.4), so an app that only reads learns of the move.
9. Runs the endpoint.
10. **Durable, then answered** (spec 6.5): an answer carrying a `seq`, a transition proof or a signed head goes out only once what the request committed is durable (synced, or committed synchronously to every replica that could be promoted). The account's service key keeps, apart from the restorable state, the newest head it signed (generation, `seq`, chain hash); for a serving account whose state is behind that head, or at that `seq` with another chain hash, the service answers every request with `503 unavailable` (its message saying a new generation is needed), signed with the account key and carrying no head, except the signer's enrollment (answered with the pending key of the next generation, 3.8), the `PUT /identity` that delivers the next generation naming that key (3.3) and an erasure (8.4.16), whose answer carries no head. An erasure's answer carries none of these, and still goes out only once the deletion it confirms is durable (8.4.16). The prototype checks the mark before step 5 and again before signing the answer, and only for serving accounts: a staged account acknowledges no app operation.

**Trusted time** (spec 10.1). The service keeps its newest authenticated time sample, from the operator's choice of source, such as NTS-secured NTP or Roughtime, taken at least hourly, with the reading of its monotonic clock at that moment. Trusted time is the sample plus the monotonic time elapsed since; its uncertainty is the sample's own plus the elapsed time times the drift bound (`trusted_time.drift_ppm`, 100 ppm by default). There is trusted time while the sample is at most `trusted_time.max_sample_age_ms` old (24 hours) and the uncertainty at most 5 minutes.
- **Time base.** While the wall clock agrees with trusted time within 5 minutes, the time base is the wall clock; otherwise it is trusted time, and the service alerts its operator. Request timestamps are checked against the time base (8.1), and step 3 stamps it, never going back per identity. A wall-clock jump of more than 5 minutes either way therefore moves neither the service time nor the timestamp check. A smaller jump stays within the agreement, so it moves the time base, and with it the timestamp check, by up to 5 minutes; stamped time still never goes back. Service time keeps moving between samples.
- **Without trusted time** (no sample within the maximum age, or an uncertainty above 5 minutes), the service answers `503 unavailable` to exactly these requests, whose outcome depends on its time:
  - a batch with an `attempt`, or with a `server_time_in` condition (7.3);
  - a batch with an outcome that only an attempt's orphan age or its owner's certificate expiry allows: an outcome by an app other than the attempt's owner while the owner is current (7.8). A certificate's expiry can't be judged without trusted time, so "current" here means that the newest `identity.json` makes the owner current (3.9: listed, or renewed and still open) and that it is neither revoked nor departed;
  - an append, directory read or `PUT /peers` admitted only by an invite code, whose expiry is a time.

  Deletions at a deadline (10.1 sweeps) wait. Everything else is served as usual: reads, cursor moves, appends and `PUT /peers` admitted otherwise, record writes without a time condition, list and invite code changes, the owner's outcomes and `done` by evidence, outcomes for an attempt whose owner isn't current in the sense above or that came from uploads, deletes and removals, and revocation. Request timestamps, certificate expiry and rate windows are then checked against the time base, which is the wall clock, corrected by the last sample where they disagree by more than 5 minutes.
- The prototype implements this as an opt-in (`trusted_time.required`), since it has no time source of its own; tests give it samples.

### 8.4 Endpoints

| Call | Caller | Section |
|---|---|---|
| `POST /v1/enroll` | signer | 8.4.1 |
| `PUT /v1/{id}/identity` | signer | 8.4.2 |
| `POST /v1/{id}/inbox` | admitted app of another identity; own app for `executor_notice` | 8.4.3 |
| `GET /v1/{id}/feed` | own app | 8.4.4 |
| `PUT /v1/{id}/peers` | admitted app of another identity, for its own identity; own app, for a counterparty | 8.4.5 |
| `GET /v1/{id}/allow` | own app with `lists.read` | 8.4.6 |
| `POST /v1/{id}/connect` | app of another identity, where the policy allows it | 8.4.7 |
| `GET /v1/{id}/connect` | own app with `lists.read` | 8.4.7 |
| `DELETE /v1/{id}/connect/{pubky}` | own app with `conversation.add` | 8.4.7 |
| `POST /v1/{id}/ops` | own app, signer | 7.4 |
| `GET /v1/{id}/claims/{claim_key}` | own app with `claims.read` | 8.4.8 |
| `GET /v1/{id}/records/{key}`, `GET /v1/{id}/records` | own app | 8.4.9 |
| `GET /v1/{id}/directory` | own app, admitted app of another identity | 8.4.10 |
| `GET /v1/{id}/apps/{app_id}/keys` | that app | 8.4.11 |
| `POST /v1/{id}/apps/{app_id}/keys` | that app | 8.4.11 |
| `PUT /v1/{id}/push` | own app | 9.1 |
| `POST /v1/{id}/apps/{app_id}/leave` | that app | 8.4.12 |
| `GET /v1/{id}/apps` | signer | 8.4.12 |
| `POST /v1/{id}/apps/{app_id}/revoke` | signer | 8.4.12 |
| `POST /v1/{id}/freeze` | signer | 8.4.13 |
| `POST /v1/{id}/unfreeze` | signer, at the old service and at the next one | 8.4.13, 11.6 |
| `GET /v1/{id}/export` | signer, at a frozen account | 11.3 |
| `GET /v1/{id}/export/chunks/{chunk_id}` | signer, or the next service under the signer's export transfer, at a frozen account | 11.3 |
| `POST /v1/{id}/import` | signer, at a staged account | 11.4 |
| `PUT /v1/{id}/import/chunks/{chunk_id}` | signer, at a staged account | 11.4 |
| `POST /v1/{id}/import/activate` | signer, at a staged account | 11.5 |
| `POST /v1/{id}/recovery/admission` | signer | 8.4.14 |
| `POST /v1/{id}/recovery/upload` | own app | 8.4.14 |
| `GET /v1/{id}/identity` | anyone, unauthenticated | 8.4.15 |
| `POST /v1/{id}/erase` | signer | 8.4.16 |

#### 8.4.1 `POST /v1/enroll`
- **Body:** `{ "enrollment": signed<Enrollment> }` (3.8). Signed by the identity key, without `Paykit-App` (`403 permission_missing` otherwise).
- **Answer:** the enrollment answer of 3.8, signed with the key it names, without `Paykit-Head`: a new or staged account serves no chain yet (8.3 step 7), and at a serving account the answer names a pending key (3.8), which has signed no head. The signer gets the first head with its next answer, such as the one to the `identity.json` delivery that makes the account serve (8.4.2).
- **Errors**, signed with the operator key: `400` for a malformed body, an enrollment naming another identity or origin, a nonce that isn't 16 bytes, or an `identity_doc` that doesn't verify; `401 bad_signature` for a request or enrollment the identity didn't sign; `401 stale_timestamp`.

#### 8.4.2 `PUT /v1/{id}/identity`
- **Body**: `{ "identity": signed<IdentityDoc>, "prev": b64(32) | null }`. `prev` is the document hash (2.2) of the version this one builds on, the newest the signer knows the service holds; `null` for the first delivery to an account that holds none.
- **Answer:** `{ "proofs": [the delivery's proof], "head" }`. The same document bytes again, whatever `prev` names, are a retry: they change nothing and answer with the original delivery's proof and the newest head, as a batch retry does (7.4 step 5), so a signer whose answer was lost still learns the service time at which its delivery was taken (3.9 Reductions). The service keeps that proof with the version, while it is the newest it holds. A version it didn't take by this call, the one it was enrolled or imported with, has none, and a retry of it answers `{ "proofs": [], "head" }`; a signer closes any reduction it owes before it enrolls a new service (3.9), so it never needs one.
- **Compare-and-swap** (spec 4.3): the service accepts a new version only if `prev` is the hash of the newest version it holds. Otherwise, and for a version that isn't higher or a lower generation, it answers `409 stale_identity` with `current: { "version", "generation", "hash", "identity" }`: the copy it holds, its document hash and the signed document, so the signer can republish that copy and build its next version on it. A `prev` at an account that holds nothing is `409 stale_identity` with `current: null`.
- **Then publish.** The signer publishes the document on the Homeserver only after this call succeeds (3.3).
- **Errors:** `403 permission_missing` for a caller that isn't the signer; `400` for a document that doesn't verify, too many apps, a wrong first document, or a new generation at an account the newest document already names, unless it names that account's pending key at the next generation (3.3); `409 stale_identity` as above; `409 conflict` with `current: { "proof_generations": [int] }` for a document that leaves out the service key of a generation a current record's kept proof still names, or gives it a key those proofs don't verify with (3.3 "Service keys"), listing those generations, ascending, so the signer adds their keys and delivers again; at a staged account in a cooperative move, `409 conflict` while an uploaded head shows that the import doesn't extend the chain an app saw.
- A delivery that names this service and a generation with `recovery_from` = generation - 1, at an empty account, starts the recovery: the new chain's first item is the `recovery` transition (7.5). So does one at a serving account that names its pending key (3.3, 3.8); the account starts afresh first.
- Vectors: `http.json` "signer: identity.json delivery" has this body for the account's first delivery, `prev` `null`: alice's version 1, since she enrolled without a document (3.3). `chain.json` chains it at seq 1, and her version 3 at seq 2. "signer: identity.json at generation 2 that leaves out generation 1's service key" is a key change in place whose version 4 names her pending key but not the key of generation 1, which the proofs of `chain.json`'s records name; its answer, "signer: 409 conflict, a needed service key left out", carries `current: { "proof_generations": [1] }`. The pending key comes from the seed label `service/alice/pending`, by the rule of `keys.json`; the generator replays the delivery on the prototype service, whose keys are drawn at random, and takes the body from its answer.
- An app dropped from `apps` stops being current at once, which may release inbox entries (spec 6.3).

#### 8.4.3 `POST /v1/{id}/inbox`
- **Query:** `apps_version=<int>`, required: the recipient's `apps_version` (8.4.10) the envelope was sealed for. An append without it is `400 bad_request`.
- **Body:** the envelope bytes.
- **Answer:** `{ "entry_id": b64(16), "accepted_at": time, "directory_version": int, "known_version": int | null }`.
- **Checks, in order:**
  1. Retry by content: an envelope the service still holds from the same sender identity returns the stored original answer, before any other check.
  2. For an app of another identity: admission (spec 7; `403 not_admitted`, or `503 staged` while a recovering account restores its allowlist), then the listing of the app with its certificate in the service's copy of its identity's `identity.json` (`409 identity_needed`, or `403 not_current` when its renewal has closed). An app that copy lists as removed is admitted only by an invite code and then gets `409 identity_needed` as if no copy were held, like a stranger (8.2).
  3. Size and header (4.4).
  4. Rate limits per sender identity and per sender app, with a tighter one per identity when `notify` is set (`429 rate_limited`); a hit on the per-app limit flags the sender identity like any other (8.4.6).
  5. Targets: every kid must be the current receive key of a current app, and with `apps_version`, every app added since must be among the targets; else `409 stale_key` (8.6).
  6. The inbox quota, then the sender's share of it (`429 quota_exceeded`, `current.limit` `inbox` or `sender_share`; Section 10). One sender identity's unread entries, envelope and overhead, hold at most `sender_share_percent` of `quota_bytes`; beyond it only that sender is refused.
  7. An invite code that admitted the sender is used, atomically with the append, and creates a conversation entry written by the service (the `conversation.add` transition, 7.5).
- An own app may append only `executor_notice` messages to its own inbox; recipients drop other kinds from an own app (spec 7). Own apps aren't rate-limited per app.
- **After a revocation** (spec 13.2): the revoked app's invite codes are withdrawn, and a switch that received the sender identity's newer `identity.json` (3.3) refuses the revoked app as it refuses a stranger (8.2). Own apps treat every chain item a removed app wrote after its `distrust_from` as doubtful: no automatic payment relies on it.

#### 8.4.4 `GET /v1/{id}/feed`
- **Query:**

  | Parameter | Default | Meaning |
  |---|---|---|
  | `after` | the start | a position `<generation>:<seq>`; the page starts after it. A position from another sequence (a lost generation) reads from the start of the current one; after a cooperative move the previous generation's positions stay valid |
  | `limit` | 100 | items per page, 1 to 500 |
  | `wait` | 0 | long-poll seconds, at most 25 (below) |

- **Answer:**

  | Field | Type | Notes |
  |---|---|---|
  | `items` | `array<feed item>` | 8.5 |
  | `next` | position | `scanned_through` as a position, to pass as the next `after` |
  | `scanned_through` | int | the last `seq` this page examined, shown or not |
  | `more` | bool | whether items follow `scanned_through` |
  | `head` | signed head | the newest one, also in `Paykit-Head` |
  | `checkpoint` | signed head | the service's latest checkpoint (below); absent when none |
  | `freeze` | `signed<Freeze>` | the freeze of a frozen account (13.5 step 2), absent otherwise; an app that sees it treats its writes as `410 moved` would |

- **Paging:** a page ends after `limit` items or after examining 1000 positions, whichever comes first, but never inside a batch: it runs on to the batch's last item (103). An app always moves its position to `next`, so a page of hidden items still makes progress.
- **Long-poll:** with `wait` above 0, the service answers at once when the page has an item or `more` is set; otherwise it waits until the account changes or `wait` seconds pass, and then answers with the page as it is then, possibly empty. A foreground app reads again after each answer.
- **Start of a new app:** the first `seq` of the batch that registered its first receive key (spec 8.4).
- **Cursor moves** (spec 8.2): an app may defer a cursor move that applies nothing and send it with its next batch, for at most a few seconds while it runs, and sends a deferred move before it stops reading (for example when a phone app goes to the background). The prototype defers at most `cursor_defer_ms` (2 s) and `suspend()` sends the move.
- **Passing own writes**: every transition proof carries its operation's chain stub (7.5). When the `seq` of a write the app made directly follows its read position, and the stub extends the chain the app verified, the app may check the stub as it checks feed stubs and move its read position past it without reading the feed; with anything in between, or a stub that doesn't extend the chain, it reads the feed as before.
- **Checkpoints** (spec 8.4): the service may drop chain stubs that are older than a checkpoint, a signed head (7.7) it keeps for that purpose, and that the cursor of every current app has passed or that are at least 30 days old in service time (`cursor_hold_ms`), by the `server_time` of their envelopes (7.6). It drops them oldest first, so the stubs it keeps run unbroken from the oldest one to the head, and it keeps the `seq` and chain hash of the newest stub it dropped (10.1), which an export's checkpoint signs (11.2). Reads alone hold no stub back, only cursors, and only for 30 days. So the stubs from an app's verified position are kept while the chain item there is less than 30 days old, since every later stub is younger, and an app that reads without moving its cursor, or stays more than 30 days behind, doesn't pin chain retention. Feed pages carry the latest checkpoint. An app checks its signature with the account's service key, and then:
  - behind the checkpoint the app accepted before from the same service, it can't be matched, since the app keeps no chain hashes behind that one: it is a rollback (spec 10.1). After a cooperative move the app compares N's checkpoints only with those it accepted from N, since N's first is the export's (11.2), which may lie behind one it accepted from O;
  - at or behind its verified position, and not behind a checkpoint it accepted from an earlier service, the checkpoint must match the chain hash the app verified at that `seq`, else it is a rollback. Behind that one the app keeps no chain hash, so the checkpoint can't be matched and isn't compared. That holds for N's first checkpoint, the export's, and for any later one N signs behind the one the app accepted from O;
  - beyond it, the app accepts it only once the page's stubs extend its verified chain through the checkpoint's `seq` and the checkpoint matches the chain hash there (a mismatch is a rollback); while they extend it and `more` is set before the checkpoint, the checkpoint waits for the next page;
  - beyond it without those stubs (the first stub after its position doesn't link to its chain, or the page reaches the checkpoint's position with none), it depends on the app. For an app whose verified chain item is less than 30 days old, by the `server_time` of its envelope on the app's clock less the 5-minute clock tolerance (the prototype's `STUBS_KEPT_FOR_MS`), and that wasn't restored since, the service rule above keeps the stubs from its position, so their absence is evidence of a rollback. How often the app reads doesn't matter. After a restore the service may hold the app's cursor past the backup's position, so the rule holds again only once the service holds a cursor the restored app moved and the app has read a page after that. The same holds for an app more than 30 days behind that holds a signed head or a transition proof of its own write at a `seq` in (its verified position, the checkpoint], from an answer whose `Paykit-Server-Time` is less than 30 days old by the same measure: the service keeps that item's stub behind the app's cursor, so its absence is a rollback, not a gap to continue over. Not for an app whose devices share its cursor (another device may have moved it past that item; such apps sync their chain views) or that was restored since. The prototype keeps up to 256 such marks (`ChainView::note_held`). For an app more than 30 days behind, or restored, the stubs may otherwise be gone honestly, and the app has no evidence of continuity, since the checkpoint's signer is the service being checked. It enters "continuity unverified", which is reconcile mode (spec 13.4): no automatic payments, every obligation checked against the SETTLED proofs and claims it keeps before attempting, shown to the user. Only as that recovery decision does it continue from the checkpoint's chain hash, with no time for its verified item until the next stub; reconciliation ends the state, but doesn't prove the missing chain's continuity. A new app before its first read takes this path too.

  Behind the checkpoint an app keeps no chain hashes, and heads at or before it are no longer compared; it drops claim evidence behind it as 13.6 allows (11.6). Batch IDs don't follow checkpoints: the app compares each for 30 days of the chain, by its stubs' `server_time`, and then forgets it, whatever checkpoints it sees (7.6). How often the service signs a checkpoint, and at which item, is the operator's choice (spec 8.4), since apps verify from their cursor whatever the checkpoint: whenever the stubs it could drop would free a useful amount. So a checkpoint may lie ahead of stubs the service still keeps, and an export carries those too (11.2). The prototype signs one once `checkpoint_behind` chain items may go, at the newest of them, and drops every stub behind it (opt-in, off by default). With `checkpoint_at_head` it signs one at the newest chain item once `checkpoint_behind` items were added since the last, and drops behind it only what the rule above lets go. After a cooperative move N signs the export's checkpoint again with its own key (11.5).
- **Lagging apps** (spec 6.3): an app whose unread entries exceed its share of the inbox quota (the quota divided among current apps) is marked lagging (8.4.12), and entries no longer wait for it, but only while another current app is reading: when every current app would be lagging, none is marked and entries wait as before, so the quota refuses senders. The mark ends when the app reads its feed.
- **`missed` items:** when the service deletes an entry a lagging target hasn't read, it records the miss in a `missed` item for that app, which takes a new `seq` like an unreadable entry, so the app's cursor reaches it whatever its position. A later miss replaces an item the app hasn't passed with one covering both. On a `missed` item the app asks the counterparties of its unfinished activity records to re-send what their inboxes accepted from the later of the item's `accepted_from` and its cursor's time, less the clock tolerance (5.12), and lists its collections again (8.4.9), since deleted keys may have been forgotten meanwhile. An item whose times don't parse is ignored.
- **Errors:** `403 permission_missing` for callers that aren't own apps; `400` for a malformed `after`.

Example page (alice's `wallet` after `1:4`: bob's entry at `seq` 5, then four chain items: the record's create, the attempt batch and a service transition; long values cut short):

```json
{
  "items": [
    {
      "type": "entry",
      "seq": 5,
      "entry_id": "FN-CtHkJNr1sjf6lqfIzqg",
      "sender": "7p4fn44qoshcwgfndz5grt7fyyz1eycqyduxdqe8o58of1byzodo",
      "cert": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcF9jZXJ0...",
        "sig": "lq3oXVnpAjL5Rdld6TiIZZVwGMhx5KnCaMQedsRxLjWGv6YESVe-cg8VtZwRxtMKtTKLUrfTfQUDJdt7GtdEAA"
      },
      "accepted_at": "2026-01-01T00:00:02.000Z",
      "envelope": "UEtUMQEBAQHRQjAQSlKcVIhq0yh7VAeYcMaS_bbO..."
    },
    {
      "type": "chain",
      "seq": 6,
      "stub": {
        "envelope": "eyJjaGFpbl92ZXJzaW9uIjoxLCJnZW5lcmF0aW9u...",
        "chain_hash": "S6AUZQIwgoWFs_81MXoXvwOGn_sZOVuk-cdZ7SP85EA"
      },
      "item_type": "op",
      "item": {
        "body": "eyJpZGVudGl0eSI6IjZmcmFvOHc5c3M1Y241NDRx...",
        "sig": "89n6NGe-p822GbF_EADBX28XH8wnJLnDxb4uN2bty9pZFYAnwW_DUJOlpXq79qjlkLBaxxs4O_8_stktlk5nDQ"
      },
      "proof": {
        "body": "eyJjaGFpbiI6eyJlbnZlbG9wZSI6ImV5SmphR0Zw...",
        "sig": "PtygYBBHaTN3ftB1d0GkdHxhTnpBDik2C0hs-lWYaxLeMKOBVIiVG1wVnI38ViFnUEqM3ArsJXs3BFRHqfVPCA"
      }
    },
    {
      "type": "chain",
      "seq": 7,
      "stub": {
        "envelope": "eyJjaGFpbl92ZXJzaW9uIjoxLCJnZW5lcmF0aW9u...",
        "chain_hash": "_zpF2N6WLmIE58x2YP6HaiBCM6leO83GPX5Hkyz6wOI"
      },
      "item_type": "op",
      "item": {
        "body": "eyJpZGVudGl0eSI6IjZmcmFvOHc5c3M1Y241NDRx...",
        "sig": "WDXfXbjTgd4v8Dl1ayX26u3EXNDkc6d-TjSZ6eaNFyFBFVbn7eb3asAeklc6zEa3hSDHrw8yBHEpIVGI8nDCAQ"
      },
      "proof": {
        "body": "eyJjaGFpbiI6eyJlbnZlbG9wZSI6ImV5SmphR0Zw...",
        "sig": "9SdlSo7HN1yrd-XQ09LgtFaX7k40vzSMGduCzSU1-kjFvg8F2n_WsiqjKuwaXQUdacChwz7PHZb_IzqLwkLYCA"
      }
    },
    {
      "type": "chain",
      "seq": 8,
      "stub": {
        "envelope": "eyJjaGFpbl92ZXJzaW9uIjoxLCJnZW5lcmF0aW9u...",
        "chain_hash": "VhiXpVnG2HA-F5fe71UaNUy25Zr_6Yoe5PnZnV1o2Rg"
      },
      "item_type": "op",
      "item": {
        "body": "eyJpZGVudGl0eSI6IjZmcmFvOHc5c3M1Y241NDRx...",
        "sig": "3yOTlodOg7C644jsG5T6bwD3f7Hm_g3wvfQr-rfPP-y6JnfXFOaxrNSlno1iLwIQgPKp6srwsJDfFCT014Q0Bg"
      },
      "proof": {
        "body": "eyJjaGFpbiI6eyJlbnZlbG9wZSI6ImV5SmphR0Zw...",
        "sig": "YcI39K84Lhe_sPZ7LCtE8FE8Z7Ai3Rt1Jho3hoKRqPcPFPw8gDthsROJTRnH6-7fZ3FCB9ap_QRwfJcWAULMCw"
      }
    },
    {
      "type": "chain",
      "seq": 9,
      "stub": {
        "envelope": "eyJjaGFpbl92ZXJzaW9uIjoxLCJnZW5lcmF0aW9u...",
        "chain_hash": "Rw2wSFLDi3ZcYiO4nQ6LR0c2OQaJlJxxvlQ8WxM3IVE"
      },
      "item_type": "service",
      "item": {
        "body": "eyJhcHBfaWQiOiJnN3RlaWY5azF1cjc1anJtaWRx...",
        "sig": "xAyHn80tZNAmFqc3Utdjp_2Scnp7HUg4BamMsTuNPhC48X1tdK8IOz1hKHSWrSyAA4GBRTYTACbH-87TSjkUCw"
      }
    }
  ],
  "next": "1:9",
  "scanned_through": 9,
  "more": false,
  "head": {
    "chain_version": 1,
    "generation": 1,
    "seq": 9,
    "chain_hash": "Rw2wSFLDi3ZcYiO4nQ6LR0c2OQaJlJxxvlQ8WxM3IVE",
    "sig": "CBiJ7qLNo481i40mFyb7KOjr7NaYMxySzGX4btJK5S72NQ5VF-RZxAEd9YJ00kJNSXwZm-lxe6vByX-dD0hqBQ"
  }
}
```

#### 8.4.5 `PUT /v1/{id}/peers`
- **Body:** a `signed<IdentityDoc>` of another identity.
- **Answer:** `{ "version": int }`, the version the service now holds, and `known_version` for apps of other identities.
- **Rules:** an app of another identity delivers only its own identity's document (`403 not_admitted` otherwise), and must be admitted, or asking to connect where the policy allows it (`403 not_admitted`; `503 staged` during a recovery's staged admission). An own app delivers counterparties' documents, never its own identity's (`400`). The service keeps one copy per identity per account, the highest version: a delivery at the version it holds is judged first, by the next bullet, whatever its generation; otherwise a lower version or generation is `409 stale_identity` with `current: { "version" }`.
- **The held version again** (3.3): a delivery at the version the service holds is a no-op only with the held copy's body bytes, whatever the signature: the service keeps its copy and answers its version. Another body at that version is equivocation. The service keeps its copy and answers `409 stale_identity` with `current: { "version", "identity" }`, the held signed document. The sender then holds both documents and acts as 3.3 says: it stops paying that identity, or pays nothing at all when the document is its own identity's, and alerts the user. This holds for an app of another identity delivering its own identity's document and for an own app delivering a counterparty's.
- **Limits** (10.4): a delivery admitted by an invite code counts against that code's rate, shared with directory reads admitted by it (`invite_reads_per_min`); one admitted only because connect requests are on counts against the account's stranger rate (`stranger_per_min`), shared with connect requests. Beyond either, `429 rate_limited`.
- **Floors** (10.1): when the service drops the copy of an admitted identity (a contact or conversation) beyond `max_peers`, it keeps that copy's version, generation and body hash. Until a copy of that identity is held again, a delivery below that version or generation, or another body at that version, is `409 stale_identity` naming the floor's version, `current: { "version" }`: the service keeps the floor's hash, not the document. That answer carries no document, so it shows the sender nothing to compare and proves nothing: the sender doesn't treat it as equivocation, and keeps paying on the documents it holds.
- **Denied senders** (spec 7): where connect requests are on, a denied sender's delivery is answered and kept exactly like a stranger's (a peer copy admits nothing), so its connect request is answered alike too (8.4.7); where they are off, both get `403 not_admitted`.
- **From a removed app** (8.2): an app that the held copy or the delivered document lists in `removed` is answered like a stranger. Admitted only by an invite code or where the policy allows connect requests (`403 not_admitted` otherwise), it gets `{ "version": v, "known_version": v }` with v the version it delivered, whatever the service holds. The service still keeps a delivered document that is newer than the copy it holds, or, holding none, one it would keep from a stranger; it never goes back. This answer compares no bodies: another body at the version held gets it too, and isn't kept.
- Vectors: `http.json` "app of another identity: identity.json at the version the switch holds, the same body" is alice's wallet delivering her version 3 (`documents.json` "alice identity.json") to bob's switch, which holds it, signed for bob's origin; its answer, "app of another identity: the version held again with the same body, a no-op", is `{"known_version":3,"version":3}`, signed with bob's service key. "..., another body" delivers her version 3 with connect requests on, signed with her key; its answer, "app of another identity: 409 stale_identity, another body at the version held, current the held copy", carries `current: { "version": 3, "identity" }` with the version 3 held, and no `known_version` (8.1).

#### 8.4.6 `GET /v1/{id}/allow`
- **Answer:**

  | Field | Type | Notes |
  |---|---|---|
  | `contacts` | `array<pubky>` | |
  | `conversations` | `array<object>` | `{ "pubky", "written_by": app-id or "service", "code_hash"?, "context"?, "at": time, "last_used"?: time }`. `last_used` is the service time of the last append the entry admitted, absent until one; apps with `lists.manage` prune idle entries by it (spec 7 "Pruning") |
  | `deny` | `array<pubky>` | |
  | `flagged` | `array<object>` | `{ "pubky", "hits": int }`: senders that hit rate limits at least 3 times (spec 7) |
  | `invite_codes` | `array<object>` | the live invite codes, `{ "code_hash", "expires_at": time, "max_uses": int, "uses": int, "written_by": app-id }`, without their contexts, so an app can compare them with the codes it registered after a move (spec 13.5 step 4) |

#### 8.4.7 Connect requests
- **`POST /v1/{id}/connect`** (an app of another identity), empty body. Answer `{ "pending": true, "known_version" }`, also to a denied sender, whose request isn't kept, so the answer doesn't reveal the deny list (spec 7). Errors: `403 not_admitted` when the policy turns connect requests off; `409 identity_needed` as for appends, also to an app the held copy lists as removed (8.2); `429 rate_limited`, per sender (`connect_per_min`) and per account for all such senders together (`stranger_per_min`), a denied sender counted alike; `429 quota_exceeded` with `current.limit` `connect_requests` when `max_pending` requests (the policy's, at most `max_connect_pending`) are pending and the sender has none, checked after the rate limits and before the deny list, so a denied sender gets this same answer while the list is full. One pending request per sender identity: a sender already pending is answered `pending`, full or not. The requests already pending stay; none is dropped to make room. With `policy.connect_requests.push`, the current apps holding `lists.read` get a push, queued like an entry's webhooks (9.2), at most once per app in 60 seconds of service time, however many requests arrive.
- **`GET /v1/{id}/connect`** (own app with `lists.read`). Answer `{ "requests": [ { "pubky", "at": time } ], "max_pending": int, "full": bool }`: the requests oldest first, the cap that applies, and whether the list is full, so the app can offer the user to clear it or block senders.
- **`DELETE /v1/{id}/connect/{pubky}`** (own app with `conversation.add`). Answer `{ "declined": pubky }`; `404` when no request is pending from it.

#### 8.4.8 `GET /v1/{id}/claims/{claim_key}`
- **Answer:** `{ "claim": claim view (7.8), "entries": [ { "step": int, "seq": int, "op": signed operation, "proof": transition proof } ] }`, an entry's `step` being the claim's step before it, its operation's `expected_step`. A tombstone has no entries. `404` when the service holds nothing for the claim key.
- **Readers** check the entries as any operation they read (7.2, 7.5): each is a `claim` operation of this claim key at its `step`, signed by its writer where the reader knows the writer's certificate, and its proof, when it verifies with the current service key, names it, with `step` one more. The claim's `step` is one more than the last entry's. A signed answer that fails these checks is misbehaviour (spec 10.1).

#### 8.4.9 `GET /v1/{id}/records/{key}` and `GET /v1/{id}/records`
- A **record view**:

  | Field | Type | Notes |
  |---|---|---|
  | `key` | `b64(32)` | |
  | `collection` | `b64(32)` | the collection ID |
  | `read_scope`, `write_scopes` | | the policy |
  | `generation`, `version` | int | the current version |
  | `seq` | int | the write's `seq` |
  | `deleted` | bool | the current write is a delete |
  | `writer` | app-id | |
  | `op` | signed operation | the `record.put` that wrote the current version, with its sealed value |
  | `divergent` | bool | uploads after a provider loss conflict with it (spec 13.6) |
  | `proof` | transition proof | the proof that gives this version and dates the write (spec 10.3), kept with the record for as long as this write is current, whatever its batch's proof retention (10.1), on every record view: `GET /records/{key}`, list pages and a `409`'s `record_view`. After a cooperative move or a key change it may be an earlier generation's proof, which N carried over (11.3). Absent for a record taken from a recovery upload (8.4.14) |
  | `cert` | `signed<AppCert>` | the writer's certificate the service checked the current write against, kept with the record (for an upload, the certificate it came with, 8.4.14); for a record written before the service kept it, the one the record the service holds for `writer` carries. A renewal or revocation later doesn't change it. Readers check the write with it (7.2 Accepting a write): an earlier certificate in `renewed` counts for a write `proof` dates before its `until`, and for none that nothing dates (step 4), so an app that reads later still takes the writer's earlier writes under a scope a renewal dropped, and no write after the cutoff |

- **`GET /records/{key}`** needs the record's `read_scope`, which `request` covers for a labelled one (3.1) (`403 permission_missing`); `404` for an unknown key. A deleted record is returned with `deleted: true`.
- **`GET /records?collection=<collection ID>&after=<key>&limit=<n>`**: the current records of the collection the caller may read, deleted ones left out, ordered by the base64url text of their keys compared as ASCII. `after` is exclusive; `limit` is 1 to 1000, default 1000. Answer `{ "records": [record view], "seq": int, "next": key | null }`: `seq` is the newest `seq` when the page was read; `next` is the last key of a full page that has more after it, to pass as `after`. `collection` is required (`400`).
- An app catching up lists every collection it may read, then follows the feed from its starting point; a record write it then sees applies only if it is newer (spec 8.4). An app that was lagging, or that finds a `missed` item, lists again (8.4.4).
- A forgotten key (7.2) is `404`, like a key that never existed.
- **Readers** take nothing from a record view on trust: the writer, the version, `deleted` and the content come from the checks of 7.2 (Accepting a write). A view without `proof`, or with an earlier service's, gives an unauthenticated version. The `409` answer's `record_view` (7.2) is a record view too, with `cert`.

#### 8.4.10 `GET /v1/{id}/directory`
- **Callers:** own apps, and apps of other identities admitted as in spec 7; an invite code admits a read without using it, and reads with one code are rate-limited per code (the prototype allows 30 a minute, `429 rate_limited` beyond). An app that the service's copy of its identity's `identity.json` lists as removed is admitted only by an invite code, like a stranger (8.2).
- **Answer:**

  | Field | Type | Notes |
  |---|---|---|
  | `identity` | `signed<IdentityDoc>` | the newest one the service holds |
  | `apps` | `array<object>` | the current apps: `{ "app_id", "record": signed<AppRecord>, "receive_key": signed<ReceiveKeyRecord> \| null }` |
  | `directory_version` | int | changes whenever the directory changes; reveals no `seq` |
  | `apps_version` | int | changes only when an app becomes reachable (current, with a receive key) or stops being reachable |

  Apps of other identities also get `known_version`. `404` before the account holds an `identity.json`.

- **Two copies of `identity.json`:** a payer holding both this copy and the Homeserver's uses the one with the higher `version`; two copies with one `version` and different body bytes are equivocation (3.3).
- **Records expire:** a sender or payer ignores the public endpoints of a record whose `expires_at` has passed (3.2).

Example (bob's `shop` reading alice's directory; signed documents cut short):

```json
{
  "apps": [
    {
      "app_id": "wejf37ngr7qizej3eoijn3hq4c",
      "receive_key": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LnJlY2VpdmVf...",
        "sig": "PAQP0CetoqOZhbkiBFIke9E6UCGt3YWW4_Uqp36lK0l7eadrEzbq7wRqjbx5qJLQtkCXyr6VzrHfGiD2gW62Dg"
      },
      "record": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcCIsImlk...",
        "sig": "b9RT0ONNIGKEr5zWCLeM72M7Ugpcd8cuGq3X7zW0AsEWhRqw68bEoxQ9Fv_85Bv90h9bYqKQnqFlQdxT79W1Cg"
      }
    },
    {
      "app_id": "g7teif9k1ur75jrmidqk7z7u6w",
      "receive_key": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LnJlY2VpdmVf...",
        "sig": "tnrMaOwQz7Du8G6Iq9HCeNwd6cau1QAT54CIKi1yHouaPo6775DQEMd7INjwq1Uu5A8y6ArDPSGY3l22_TL9DQ"
      },
      "record": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcCIsImlk...",
        "sig": "f5221fPvkkAnSTYSu0LiEqhTxRcc_uhOKWuVJGix0YndFOusAMl5WN7iQHQ36TSo8ZEybCyjXtjj3lisG0JQAg"
      }
    },
    {
      "app_id": "cw5ahtdkixcafna9zh1y5y5o1w",
      "receive_key": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LnJlY2VpdmVf...",
        "sig": "-uWFmsLrI5ByW4e4tCWapU8j_pKQ3-dkBKs6ENntTgWVOg93vZO7IOas4V28WXEIgF6zxHTFKnf6pNhHedf9Cg"
      },
      "record": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcCIsImlk...",
        "sig": "Qv6IQynbH8MXcUD_IrCxTmSBWbcTqb0JR27-SI8ag5Q5Ty1cBOWcYZx3f9o6p2dd6nnoAbzDcKFRiwiGmlT1Ag"
      }
    }
  ],
  "apps_version": 4,
  "directory_version": 7,
  "identity": {
    "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmlkZW50aXR5...",
    "sig": "vbF6al58cr2SbWUAU5b8w4H77bHNfv_HkZqOzDwskknnTjpODk2LG4qZILzzq9pzlBtZYmqtnDOfQXKM06qcCA"
  },
  "known_version": 1
}
```

#### 8.4.11 `GET` and `POST /v1/{id}/apps/{app_id}/keys`
- **`GET`:** only that app (`403 permission_missing`). Answer `{ "bundles": [ { "bundle": key bundle (3.5), "seq": int, "first_epoch": int, "last_epoch": int } ] }`: every part the service stores for the app, ordered by `first_epoch`, never overlapping; `seq` is that of the part's `key_bundle.put`. `404` when the service holds none. The app opens every part and merges the keys, by the rules of 3.5 "Reading": it refuses an answer whose parts are out of order or overlap; it skips a part that doesn't verify or open, whose plaintext range isn't the range it names in the clear, that holds a key after the next data epoch, whose naming key differs from an earlier part's, that names a certificate other than its own or a listed renewal of it, or whose keys aren't exactly those of that certificate's Paykit scopes over its range, and the others still count. A key that no part it takes holds shows as lacking afterwards, which leads to `POST`.
- **`POST`** (spec 3.6): only that app, empty body; it asks the signer for a full key bundle. The service records the time of the first request and shows it in `GET /apps` as `full_bundle_requested` until the parts it stores for the app cover every data epoch from 1 to the current one; a repeated request keeps the first time. Answer `{ "requested": time }`. An app asks once, when after fetching its current bundle it still lacks a scope key for some data epoch up to the current one.

#### 8.4.12 Apps: leave, list, revoke
- **`POST /v1/{id}/apps/{app_id}/leave`**: the app itself (`403 permission_missing` otherwise), empty body. The service records the departure and answers with the `leave` transition's proof.
- **`POST /v1/{id}/apps/{app_id}/revoke`**: the signer, empty body. Answers with the `revoke` transition's proof; revoking a revoked app again answers `{ "proofs": [], "head" }`; `404` for an app the service and `identity.json` don't know.
- **`GET /v1/{id}/apps`**: the signer. Answer `{ "apps": [status], "recovery": object | null, "proof_generations": [int] }`. `proof_generations` lists, ascending, every generation that some current record's kept proof names (8.4.9), but those the newest `identity.json` declares lost (`recovery_from` and earlier), absent when none: the generations whose service keys `identity.json` keeps (3.3). It is answered while the account is frozen too, so the signer reads it from O before a move; N derives the same set from the record parts' proofs once it has imported them (11.3). A status:

  | Field | Type | Notes |
  |---|---|---|
  | `app_id` | app-id | every app the service knows or `identity.json` lists, a purged app's kept App ID included (`current` false) |
  | `current` | bool | |
  | `last_seen` | `time \| null` | |
  | `revoked_at`, `departed_at` | `time \| null` | |
  | `awaiting_rotation` | bool | a departure whose rotation isn't done: done once a delivered `identity.json` lists the app in `removed` with a higher `data_epoch` than at its departure |
  | `open_attempts` | `array<object>` | `{ "claim_key", "attempt_id", "outcome": "open" \| "unknown" }` |
  | `unread_entries` | int | inbox entries targeting it that it hasn't read |
  | `sole_target_unread` | int | of those, the ones it is the only current target of |
  | `record_cert` | `b64(32) \| null` | the document hash of the certificate the app record the service holds carries (3.9 Renewal) |
  | `record_at` | `time \| null` | when the service took that record; the signer writes it plus the 5-minute clock tolerance as `renewed[].until` (3.9 step 4) |
  | `lagging` | bool | marked lagging (8.4.4); its entries no longer wait for it |
  | `keys_through` | `int \| null` | the newest data epoch through which the app holds every scope key: the epoch of the first `identity.json` listing it, raised to a part's `last_epoch` once the app's cursor has passed that part's item, for each part that starts at most one epoch after what the app holds (or at 1 when that is unknown); `null` when unknown (the signer then stores a full bundle). It is evaluated again at each cursor move over every part the cursor has passed, in `first_epoch` order, so a part passed before the gap below it was filled raises it once the gap is filled |
  | `full_bundle_requested` | `time \| null` | a pending `POST /keys` (8.4.11) |
  | `bundle_ranges` | `array<[int, int]>` | `[first_epoch, last_epoch]` of each key bundle part the service stores for the app, in order (3.5). A signer resumes a delivery cut short from `keys_through` + 1 to the epoch it delivers for, storing only what these ranges don't show yet as parts it stored for the app's current certificate (3.5). It starts from 1 only with a pending full-bundle request or without `keys_through`, since the parts the app has passed and holds are gone |
  | `cert_expires_at` | `time \| null` | the `expires_at` of the certificate the service checks the app against; signers warn before letting it lapse while the app owns open attempts (spec 10.5) |

  `recovery`: `{ "recovery_from": int, "declared_at": time, "admission_staged": bool }` during a recovery. The answer also carries `identity`, the newest `identity.json` the service holds, so the signer can build on it (3.3).

#### 8.4.13 Freeze and unfreeze
- **`POST /v1/{id}/freeze`**: body `signed<Freeze>` (3.7). Answer: the `freeze` transition's proof. The same freeze again, byte for byte, while the account is frozen by it, gets that same proof with the newest head; the signer resends its kept freeze after a lost answer (3.7). Errors: `401 bad_signature` when the identity didn't sign it; `400` for another identity or kind, or a `next_generation` other than the current one plus one; `409 conflict` with `current: { "freeze": signed<Freeze> }` when frozen by another freeze (a signer that signed it, another device of its, adopts it if it names the same next service and generation); `409 conflict` for a freeze an unfreeze cancelled (3.7: used once); `409 conflict` while admission is staged after a provider loss (8.4.14, spec 13.6 step 2), since an export doesn't carry staged admission, so the move waits until the lists are restored or the signer ends the staging.
- **While frozen,** the public read of `identity.json` answers with the account's last document and the freeze (8.4.15); after an unfreeze at the old service it carries no freeze again.
- **Before it freezes** the signer completes the rotations that departures still owe (spec 13.3), and checks that the next service's announced `max_import_bytes` (3.8) is at least the current one's; otherwise it doesn't freeze.
- **`POST /v1/{id}/unfreeze`**: body the unfreeze of 3.7. At a staged next service it is the abort of 11.6, sent first: N discards the import and answers with its signed discard answer (3.7). At the frozen old service the body must carry that answer as `discarded`, signed with the service key the freeze names (`409 conflict` without it or when it doesn't verify); the answer is the `unfreeze` transition's proof, and the same unfreeze again gets the same proof with the newest head. Errors: `401 bad_signature`; `409 conflict` for another freeze; `404` when not frozen and never frozen by that freeze.

#### 8.4.14 Recovery
- **`POST /v1/{id}/recovery/admission`**: the signer, empty body. Ends staged admission; answers with the `admission` transition's proof; `404` without a recovery.
- **`POST /v1/{id}/recovery/upload`**: an own app. Body:

  | Field | Type | Notes |
  |---|---|---|
  | `lost_identity` | `signed<IdentityDoc>` | an `identity.json` of the lost generation; it names the old service key that signed the proofs |
  | `records` | `array<uploaded op>` | `record.put` operations with their sealed values |
  | `claims` | `array<uploaded op>` | claim entries |
  | `heads` | `array<signed head>` | the old service's heads |
  | `proofs` | `array<signed<proof>>` | the old service's transition proofs |
  | `lists` | object | `{ "contacts": array<pubky>, "deny": array<pubky> }`: contacts from an app with `lists.manage`; deny entries also from an app with `lists.read`, since they only restrict admission |

  An uploaded op is `{ "op": signed operation, "cert": signed<AppCert> (its writer's certificate), "proof": transition proof (optional, the old service's) }`. An operation of an app now listed in `removed` counts only with a proof that dates it before the app's `distrust_from`.
- **Answer** after a provider loss: `{ "result": changes, "proof": the upload transition's proof, "head" }`, where `changes` is `{ "records": [key], "divergent": [key], "claims": [claim key], "divergent_claims": [claim key], "unverified": int, "distrusted": int, "refused": int, "heads": int, "lists": bool }`.
- **Writer rules for claims** (spec 13.6 step 3): an uploaded claim entry that carries no transition proof from O counts only if the live writer rules allow it: `done` with basis `owner`, and `failed`, only from the attempt's owner. Others are counted in `refused` and left out. Two outcomes for one attempt make the claim divergent instead of applying `failed`; a divergent claim clears once every attempt on it that isn't final is resolved by its owner, evidence or a user-confirmed outcome. On a claim rebuilt from uploads the owner may record `done` on a `failed` attempt (7.8 row 2).
- **Records** (spec 13.6 step 3): an uploaded record's version is the one O's transition proof gives it; without that proof, the version it built on plus one is a lower bound. It fills in a record N lacks; two different uploads that build on one version are divergent; a write at N conflicts with an upload newer than the uploaded version the writes at N built on, or with any upload if they built on none.
- **Recovery in place** (spec 13.6 step 1): N may be O's own operator and origin; the delivery of an `identity.json` with a new generation and `recovery_from` then starts the new generation in the existing account.
- **At a staged service in a cooperative move** (before the import, spec 13.5 step 3) the body carries `heads` and `proofs`: after reading O's feed to its end, an app uploads its newest head and the evidence it holds, the SETTLED and tombstone proofs it keeps and the transition proofs of the record versions it holds, each checked with the old service's key (`401 bad_signature` otherwise). An app whose continuity is unverified (8.4.4) uploads no head until it has reconciled, since it hasn't verified the chain the head would vouch for; it still uploads its evidence. The evidence includes the transition proofs of the app's own attempts still open. The answer is `{ "heads": int, "proofs": int }`, the proofs counting those kept. At activation N checks the import against them (11.5).
- **After the activation**, while the move is unpublished, N checks each upload at once, with the old service's key: a head at or after the imported checkpoint that the imported chain doesn't extend, a proof not bound to the imported chain (its stub after the imported head, or with another chain hash at a `seq` N holds), and a proof the imported state doesn't meet by the evidence rules of 11.5 are each an import conflict: `409 conflict`, with `current: { "conflicts": [string] }`, which also blocks the `identity.json` delivery that would activate the service until the signer aborts (11.6). An older head is ignored, as at activation. The answer is `{ "heads": int, "proofs": int }`, the proofs counting those checked.
- **After the move is published**, an app that holds the old service's SETTLED proofs uploads them again once it follows (spec 13.5 step 4), so the evidence of an app that missed the move still counts. N checks each with the old service's key, which it keeps from the activation, and merges the proofs bound to the imported chain under the rules of spec 13.6 step 3: a SETTLED proof for an obligation N holds nothing for rebuilds its tombstone; one for an obligation N holds open and unsettled makes it divergent (doubtful, no automatic payment); one N holds settled changes nothing. Any other proof the imported state doesn't meet, and a proof not bound to the imported chain, is refused: `409 conflict` with `current: { "conflicts": [string] }`, and nothing changes. When something merged, N appends an `upload` transition listing it, as after a provider loss. The answer is `{ "heads": int, "proofs": int, "merged": changes, "head" }`, with `changes` as above. N never answers success for a proof it didn't check or keep.
- An app sends its `app.put` and first `receive_key.put` before it uploads, and uploads before its first record write at the new service.

#### 8.4.15 `GET /v1/{id}/identity`
The newest `identity.json` the service holds for the account's own identity, as the signer delivered it (8.4.2, spec 4.3), so a payer can compare it with the Homeserver copy (3.3, "The switch's copy").
- **Request:** unauthenticated: no `Paykit-Identity`, `Paykit-App` or `Paykit-Signature`, so it names no payer; a client sends none. No body.
- **Answer:** `{ "identity": signed<IdentityDoc>, "freeze": signed<Freeze> }`, with `freeze` only while the account is frozen (3.7, 8.4.13), signed with the account's service key like every answer (the request signature field is `hash` of the empty string, 8.1), with no `Paykit-Head`, `seq` or `known_version`. The document and the freeze are signed by the identity key, so a reader verifies them as any `identity.json` (3.3) and any freeze (3.7) and needs nothing else from the answer. How a payer uses it: 3.3, "The switch's copy".
- **Caching:** a `200` carries `Cache-Control: public, max-age=60`. The answer is the same for every reader, since its signature covers the hash of the empty string as the request signature, so a cache may serve it for up to 60 seconds. Error answers carry none.
- **Limits** (10.4): per network source, an IPv4 address or an IPv6 /64, `identity_reads_per_source_per_min` reads a minute per account; and per account, from all sources together, `identity_reads_per_min`, a higher backstop. Reads that a source's limit refuses don't count against the account's. A service that can't tell the source, such as the prototype's in-process transport, applies only the account's limit; behind a reverse proxy the source is the address the proxy reports.
- **Errors:** `404` for an unknown account (operator key) or one holding no document; `503 staged` for an account the newest document doesn't name yet (8.3 step 7); `429 rate_limited` past either limit (10.4).
- **A frozen account** answers with its last document and the freeze. After a move the old service keeps both, and answers so, for at least the identity's record age limit (`record_max_age`, 90 days when absent) after the move, after which no app record from before the move is payable (3.2). It doesn't see the move published, so it counts from the last export call it answered, plus a day. The prototype deletes a frozen account only when the signer erases it (8.4.16). After an unfreeze (11.6) the answer carries no freeze.
- It reveals nothing the Homeserver doesn't publish. The service sees the reader's network address and when it looked the identity up, so it learns that someone is about to pay it (spec 16).
- Vectors: `http.json` "anyone: the public identity.json read" and "anyone: a frozen service's public identity.json read, with the freeze".

#### 8.4.16 `POST /v1/{id}/erase`
The signer erases the identity's account at this service (spec 13.7).
- **Request:** a signer call (8.2): signed with the identity key, without `Paykit-App`, over the switch origin and `Paykit-Timestamp` like every request. So an app's call is `403 permission_missing`, another identity's key `403 not_admitted`, a signature for another service's origin `401 bad_signature`, and a timestamp more than 5 minutes from the service's time `401 stale_timestamp`. Body `{ "service_key": b64(32) }`: the account's service key, the one the signer pinned. Any other member is `400 bad_request` (1.1). Another key is `409 conflict`, so a replay within those 5 minutes can't erase a later account of the identity here, which has a new key.
- **Refused during a move** (`409 conflict`): at a frozen account whose export isn't finished, that is, with no manifest built yet (11.2) or a chunk of it never fetched, by the next service or the signer (11.3); and at a staged account holding an import, partial or activated, until the signer aborts it (11.6), since the old service resumes only with this service's discard answer (3.7). The service doesn't wait for the pointer window of 8.4.15: once the export is finished it takes the erasure, and it is the signer that waits for the window (spec 13.7). After a move given up for a provider loss (spec 13.6), the signer finishes the old service's export by fetching its manifest and every chunk itself, then erases there.
- **Answer:** `{ "erased": true }`, signed with the account's service key. It is the last answer that key signs. No answer to an erasure, an error included, carries `Paykit-Head`. The service then deletes everything it holds for the identity: every kind of 10.1, "for good" ones included, the chain with its stubs, checkpoints and batch records, the freeze, the export or the staged import, the enrollment answers, the pending key of the next generation (3.8), the rate-limit and failed-authentication counts (10.4), and the account's service key with its mark (8.3 step 10). An account whose state is behind its key's mark still takes the erasure, since the answer carries no head.
- **Order:** the service signs the answer first, while it still holds the key; then it commits the deletion of all of it, the state, the service key and the mark, durably (8.3 step 10); only then does it release the answer. So a crash after the answer can't leave the erased account recoverable. If the deletion can't be committed, the service answers `503 unavailable`, signed with the account's service key, without a head, having deleted nothing, and the signer retries. A retry after a committed erasure finds no account: it gets the `404` of the next bullet, signed with the operator key. A signer whose `200` was lost takes that `404`, for the account it asked to erase, as the erasure done. It can't authenticate it, but a signed `200` proves no more: a service that keeps the data could sign one anyway (spec 13.7).
- **Afterwards** every request for the account is answered as for an account the service doesn't host: `404`, signed with the operator key (8.3 step 2), the public read and the erasure itself included. `POST /v1/enroll` creates a new, staged account with a new key. The signer names in it the App IDs it revoked or saw depart, as in every enrollment (3.8), and starts the account as after a provider loss: an `identity.json` at the next generation with `recovery_from` (8.4.2), so its version and generation stay above any a payer has seen.
- **Not erased:** other accounts' state that names the identity, such as their contacts, conversation entries, deny lists and peer copies of its `identity.json`, belongs to them.
- **Backups** (spec 6.5): the operator lets the account's data age out of its backups and erases it again after any restore from a backup taken before the erasure.
- Vectors: `http.json` "signer: account erasure, naming the account's service key" and its answer. Its `service_key` is `keys.json`'s fixed key for alice; the generator replays it on the prototype service, where the account's key is drawn at random, so there it names that key.

### 8.5 Feed items and visibility
Each feed item has a `type`:

| `type` | Fields | Shown to |
|---|---|---|
| `entry` | `seq`, `entry_id`, `sender` (pubky), `cert` (the sender app's certificate), `accepted_at`, `context` (the invite code's, if one admitted it), `envelope` (b64) | its target apps |
| `unreadable` | `seq`, `entry_id`, `sender`, `accepted_at` | the current apps, for an entry no current app can open after its targets departed; it takes a new `seq` at the departure |
| `missed` | `seq`, `entries` (int), `accepted_from`, `accepted_until` (times) | the lagging app it was made for: how many entries targeting it the service deleted before it read them, and the first and last of their acceptance times (8.4.4) |
| `chain` | `seq`, `stub`, `item_type`, `item` (the signed document), `context` (for a service-written conversation entry), `proof` (for a signed operation: its transition proof) | apps that may read the item, below, while the service keeps it |
| `stub` | `seq`, `stub` | apps that may not read the item, and every app once the item's document is deleted |

Who may read a chain item in full:

| Item | Readers |
|---|---|
| `identity.json` deliveries, `app.put`, `receive_key.put`, `cert.put`, service transitions other than `conversation.add` | every own app |
| `claim` | apps with `claims.read` |
| `record.put` | apps holding the record's `read_scope` |
| `allow.*`, `deny.*`, and the service's `conversation.add` | apps with `lists.read` |
| `invite_code.*` | apps with `lists.read` or `conversation.add` |
| `key_bundle.put` | the target app |

Superseded record writes and key bundles, superseded app records, renewed certificates replaced or carried by the app's record, and the entries of compacted claims lose their document and show as stubs.

### 8.6 Errors

| HTTP | `code` | When | `current` |
|---|---|---|---|
| 400 | `bad_request` | malformed request, document or operation; unknown members where 1.1 rejects them; a missing or unparsable `Paykit-Timestamp` or `Paykit-Identity`; an append without `apps_version`; an envelope size that isn't a bucket; a policy or collection that differs from the record's; a batch writing one record twice; an `evidence_ref` on a claim entry other than `done` | |
| 401 | `bad_signature` | a request, operation, certificate, freeze, unfreeze or key bundle signature that doesn't verify | |
| 401 | `stale_timestamp` | `Paykit-Timestamp` more than 5 minutes from the service's clock | |
| 403 | `not_admitted` | a sender not admitted or denied; a connect request where they are off; a signer of another identity. An admission refusal has one message, `7: sender not admitted`, whatever the reason (denied, unknown, an unknown, expired or used-up code) | |
| 403 | `permission_missing` | the caller's certificate lacks the switch permission or scope; a call only another caller may make | |
| 403 | `not_current` | the calling app is revoked, departed or not listed with its certificate, also once it is purged (its App ID is kept for good, 10.1); an app of another identity whose certificate's renewal has closed (3.9). An app the sender's `identity.json` lists as removed gets a stranger's answer instead (8.2) | |
| 403 | `cert_expired` | the caller's certificate has expired | |
| 404 | `not_found` | no such account, route, claim, record, key bundle, invite code, connect request or freeze | |
| 409 | `conflict` | a condition fails | `{ "record_version": { "key", "generation", "version" } }`, `{ "claim_step": { "claim_key", "step" }, "claim": view \| null }` or `{ "server_time": time }` |
| 409 | `conflict` | a claim entry is refused (7.8) | `{ "claim": view \| null }`, plus `"server_time"` for a passed deadline |
| 409 | `conflict` | a `record.put`'s `expected_version` isn't the current version | `{ "record": { "key", "generation", "version" }, "record_view": record view }` to a writer with the read scope, else `{}` |
| 409 | `conflict` | a receive key's `n` isn't the next one | `{ "receive_key": signed record \| null }` |
| 409 | `conflict` | a batch ID in the chain with other operations, or a retry whose proofs were dropped; an invite code registered twice; a `cert.put` not issued after the certificate it renews; a second freeze, or an unfreeze of another freeze; an unfreeze at the old service without N's discard answer (3.7); an import that doesn't extend an uploaded head, or that doesn't settle what an uploaded SETTLED proof shows or holds a record older than an uploaded version (11.5); the export and import errors of Section 11; an erasure naming another service key, or during a move (8.4.16) | `{ "chunk": chunk ref }` for a chunk that doesn't match the manifest; `{ "missing": [chunk id] }` for an activation with chunks missing (11.4, 11.5) |
| 409 | `conflict` | a delivered `identity.json` leaves out the service key of a generation a current record's kept proof names, or gives it a key those proofs don't verify with (`PUT /identity`, 3.3) | `{ "proof_generations": [int] }`, the generations it leaves out, ascending |
| 409 | `stale_epoch` | a sealed value at another data epoch | `{ "data_epoch": int }` |
| 409 | `stale_generation` | an operation naming another generation | `{ "generation": int }` |
| 409 | `stale_key` | an envelope wrapped to a receive key that isn't current, or sealed for an `apps_version` older than an app added since | below |
| 409 | `stale_identity` | a delivered `identity.json` that isn't newer (at `PUT /peers`, a lower version or generation, or another body at the version held), or (`PUT /identity`) doesn't build on the version the service holds | `{ "version", "generation", "hash", "identity" }`, the copy held (`PUT /identity`, `null` when it holds none); at `PUT /peers`, `{ "version" }`, or `{ "version", "identity" }` with the held signed document for another body at the version held (8.4.5) |
| 409 | `identity_needed` | the service's copy of the sender identity's `identity.json` doesn't list the app; also, as to a stranger, an app that copy lists as removed (8.2) | `{ "version": int \| null }`, the version it holds; `null` for an app the copy lists as removed |
| 410 | `moved` | the account is frozen | `{ "freeze": signed<Freeze> }` |
| 413 | `too_large` | an envelope over 64 KiB, a sealed value over its cap, a signed operation over 512 KiB, an invite code context over 1024 bytes, a chunk over 1 MiB, an export over the import limit | |
| 429 | `rate_limited` | too many requests from the sender identity or sender app, invite-code directory reads, or batches from the app (Section 10) | body `retry_after` and header `Retry-After` |
| 429 | `quota_exceeded` | a quota or a per-kind limit is full (Section 10) | `{ "limit": string }`, the budget that is full (10.3) |
| 503 | `staged` | the account isn't named by its `identity.json` yet, or a recovering account restores its allowlist | |
| 503 | `unavailable` | a temporary failure; without trusted time, a request whose outcome depends on the service's time (8.3); an account whose state is behind its key's mark, until a new generation (8.3 step 10) | |

- **What `current` reveals**: a record's version only to a caller holding its read scope, or when the record doesn't exist; a claim only to a caller with `claims.read`. Otherwise `current` is `{}`.
- **`stale_key`'s `current`**:

  | Field | Type | Notes |
  |---|---|---|
  | `receive_keys` | `array<object>` | `{ "app_id", "record": signed<ReceiveKeyRecord> }`: the current record of each target app whose key was stale |
  | `not_current` | `array<app-id>` | targets that are no longer current |
  | `unknown_kids` | `array<b64(8)>` | kids the service never registered |
  | `added_apps` | `array<object>` | `{ "app_id", "app_record": signed<AppRecord>, "record": signed<ReceiveKeyRecord> }`: apps reachable since `apps_version` that the envelope left out |
  | `apps_version` | int | the current one |

  The sender checks each record (3.4), seals the same signed plaintext again and re-sends.
- **Retry** column of spec 6.4 applies. `429 rate_limited` uses fixed one-minute windows, per sender identity, per sender app or per invite code, or per own app for batches (Section 10); `retry_after` is the seconds to the next window. `429 quota_exceeded` carries no `retry_after`, and frees up per limit (spec 6.4): the inbox quota as the recipient's apps read their feed; the storage quota after deletions; the record key limit after deletions, once the service forgets the deleted keys (7.2); other limits after a deletion or removal. A receiving app whose record write gets it stores the entry and moves on (5.1).

Example (`409 stale_key`, records cut short):

```json
{
  "code": "stale_key",
  "message": "3.7: envelope wrapped to a receive key that isn't current",
  "current": {
    "added_apps": [
      {
        "app_id": "g7teif9k1ur75jrmidqk7z7u6w",
        "app_record": {
          "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcCIsImlk...",
          "sig": "f5221fPvkkAnSTYSu0LiEqhTxRcc_uhOKWuVJGix0YndFOusAMl5WN7iQHQ36TSo8ZEybCyjXtjj3lisG0JQAg"
        },
        "record": {
          "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LnJlY2VpdmVf...",
          "sig": "tnrMaOwQz7Du8G6Iq9HCeNwd6cau1QAT54CIKi1yHouaPo6775DQEMd7INjwq1Uu5A8y6ArDPSGY3l22_TL9DQ"
        }
      }
    ],
    "apps_version": 5,
    "not_current": [],
    "receive_keys": [
      {
        "app_id": "wejf37ngr7qizej3eoijn3hq4c",
        "record": {
          "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LnJlY2VpdmVf...",
          "sig": "PAQP0CetoqOZhbkiBFIke9E6UCGt3YWW4_Uqp36lK0l7eadrEzbq7wRqjbx5qJLQtkCXyr6VzrHfGiD2gW62Dg"
        }
      }
    ],
    "unknown_kids": []
  }
}
```

## 9. Push

### 9.1 `PUT /v1/{id}/push`
An own app sets its own relay and handles in two steps (spec 9):

| Field | Type | Req | Constraints |
|---|---|---|---|
| `relay_url` | string | yes | `https://` URL on port 443, with no user info; its host must resolve only to public addresses |
| `handles` | `array<string>` | yes | at most 8, one per device; `[]` when starting a registration |
| `push_key` | `b64(32)` | no | absent to start a registration; present to set the handles of the registration it names |
| `replaces` | `b64(32)` | no | only on a start: the push key of the app's current registration, which the start replaces (a rotation or another relay); absent otherwise |

1. **Start a registration.** The app sends its relay URL, no handles and no `push_key`; a start with handles is `400`. A start is idempotent without a nonce or replay cache (spec 6.1, 9):
   - the app holds no registration: the service mints a new **push key**, an Ed25519 key for this registration that appears in no public document, and answers its public half; a start naming `replaces` is then `409 conflict` with `current: { "push_key": null }`;
   - the app holds one, and the start names no `replaces` and the same `relay_url`: the answer is the current registration, unchanged, with its handles;
   - the app holds one, and `replaces` is its push key: the service mints a new push key, replacing the registration and its handles;
   - otherwise (another `relay_url` without `replaces`, or `replaces` naming another key): `409 conflict` with `current: { "push_key": the current one }`.

   So a retried start, or a replay within the timestamp window, changes nothing, and a replay of an earlier rotation names a key that is no longer current.
2. **Register with the relay.** Each device sends its device token and that push key to the relay, which returns a handle bound to the push key (9.3).
3. **Set the handles.** The app sends the relay URL, the handles and the `push_key`. A `push_key` that isn't the current registration's, or another relay URL, is `409 conflict` with `current: { "push_key": the current one, or null }`; the app starts a new registration.

- **Answer:** `{ "relay_url": string, "handles": int, "push_key": b64(32) }`: the handles kept and the registration's push key.
- **Errors:** `400` for more than 8 handles, handles on a start, `replaces` with `push_key`, a URL that fails the checks, or a service that sends no pushes; `409 conflict` as above.
- A start naming the current push key in `replaces` is how an app rotates its push key or moves to another relay. The setting belongs to the calling app and is removed when the app stops being current. After a provider change, apps register again with the new service, which mints new push keys (spec 13.5, 13.6).
- **What the relay learns** (spec 16): which handles share a push key, so which devices belong to one app, but not the identity, since push keys appear in no public document.

### 9.2 Webhook
When a `notify` entry targets an app, the service sends one webhook per handle:

```
POST <relay_url>
Content-Type: application/json
Paykit-Signature: <b64(64)>

{"v":1,"handle":"<handle>","ts":"<time>","nonce":"<b64(16)>"}
```

| Field | Type | Notes |
|---|---|---|
| `v` | int | 1 |
| `handle` | string | the handle, and nothing about the message |
| `ts` | time | the service time when sent, three fraction digits |
| `nonce` | `b64(16)` | random, fresh per webhook |

- **Signature:** the registration's push key (9.1), never the service key, label `paykit/v1/push`, over `lp(label, relay origin, body bytes)`. The relay origin is `https://` followed by the URL's host, without port or path: `https://push.example` for `https://push.example/v1/hook`.
- **Delivery:** from a background queue, never during the append. Each connection resolves the host again, refuses the host if any address isn't public, and connects to the address it checked. Redirects aren't followed.
- **Public addresses** (spec 9; the prototype's `netcheck`), for relay URLs and switch origins (8.1) alike. Refused, from the IANA special-purpose registries: IPv4 `0.0.0.0/8`, `10/8`, `100.64/10`, `127/8`, `169.254/16`, `172.16/12`, `192.0.0.0/24` (except `.9` and `.10`), `192.0.2/24`, `192.88.99/24`, `192.168/16`, `198.18/15`, `198.51.100/24`, `203.0.113/24`, `224/4` (multicast), `240/4`, `255.255.255.255`; IPv6 `::/96` (unspecified, loopback and IPv4-compatible), `64:ff9b:1::/48`, `100::/64`, `100:0:0:1::/64`, `2001::/23` except its globally reachable blocks (`2001:1::1` to `::3`, `2001:3::/32`, `2001:4:112::/48`, `2001:20::/28`, `2001:30::/28`), so Teredo `2001::/32` with it, `2001:db8::/32`, `3fff::/20`, `5f00::/16`, `fc00::/7`, `fe80::/10`, `fec0::/10`, `ff00::/8`. Judged by the IPv4 address they embed: IPv4-mapped `::ffff:0:0/96` (the low 32 bits), NAT64 `64:ff9b::/96` (the low 32 bits), 6to4 `2002::/16` (bits 16 to 47). Everything else is public. A relay URL uses port 443 only. Timeout 5 s; a failure is retried 20 s later, at most 3 attempts within 1 minute of the entry.
- Any 2xx answer is a delivery; any other answer, a redirect or a timeout is a failure.

Example (vectors `http.json` `webhook`, signed there with the fixed service key of `keys.json` standing in for a push key, since the signature input is the same):

```
POST https://push.example/v1/hook
Content-Type: application/json
Paykit-Signature: Uw3B2cVrmy7Qd-viNkEvucTie8sKpNNyMt1KcCinF0WHmyVKmxHE0kfHaYP6_JK7vfsvztDmt772AqoZC8aUBw

{"v":1,"handle":"Kt8fSp-tnjGPMhobvyuT-Q","ts":"2026-01-01T00:06:00.000Z","nonce":"93mQsFU50c1_PkEDllOYNA"}
```

### 9.3 Relay rules
A relay registers a device token for one push key and returns a random handle bound to that key (spec 9 step 2). It accepts a webhook only when:
1. the body is a webhook with `v` 1 (else `400`);
2. the handle is known (else `404`);
3. `Paykit-Signature` verifies with the push key bound to the handle, over the relay's own origin and the body (else `401`);
4. `ts` is within 5 minutes of the relay's clock (else `401`);
5. the nonce wasn't seen in the last 10 minutes (else `409`);
6. the handle is under its per-minute push limit (else `429`).

Then it pushes to the device and answers `200`.

## 10. Retention and quotas
Spec 6.3. For every kind of state the service keeps, how long it keeps it, which budget it counts against and its limit. The prototype's table is `service::retention`. Defaults are for one identity on a shared service; an operator may raise them.

### 10.1 What the service keeps

| Kind | Kept for | Counts against | Default limit | When full |
|---|---|---|---|---|
| Inbox entry | until every current target's cursor has passed it, not waiting for targets marked lagging (8.4.4); no expiry | inbox quota: its envelope bytes plus `entry_overhead_bytes` (256) | `quota_bytes` 64 MiB; 64 KiB per envelope (4.4) | the append is `429 quota_exceeded` (`inbox`) |
| `missed` item (8.5) | until its app's cursor has passed it | nothing; never refused | one per lagging app; a later miss replaces it | - |
| Unreadable entry (8.5) | until every current app's cursor has passed it | inbox quota: `entry_overhead_bytes` only | - | never refused: the service creates it |
| Shared record value | while current, with the operation, certificate and transition proof of its current write (8.4.9); a superseded value is deleted at once | storage quota: its sealed `ct` bytes, ciphertext and tag as decoded from base64url (not the text), 0 for a delete; the certificate and proof, about 1.6 KB, are bounded by `max_record_keys`, not counted | `record_quota_bytes` 64 MiB; 256 KiB per value (7.2) | a `record.put` that grows the stored bytes is `429` (`storage`) |
| Shared record key | while live; once deleted, until the service forgets it (below) | record key limit: live keys only | `max_record_keys` 100,000 live keys | a `record.put` creating a key, or writing a deleted one again, is `429` (`record_keys`) |
| Live claim | until finished, then compacted to a tombstone at once (7.8) | claim limit, with tombstones; its creator's share of it (7.8) | `max_claims` 1,000,000; `max_attempts_per_claim` 64; `claim_share_percent` 25; `open_attempts_per_app` 1,000 | only `attempt` is refused: on a new claim key (`claims`, or `claim_share` beyond its app's share), beyond 64 on one (`attempts`), or beyond its app's open attempts (`open_attempts`) |
| Tombstone | until the time base, with trusted time, reaches its pinned deadline plus the 30-day reconciliation window (7.8), then deleted at a request to the account (8.3 step 4); when pinned to "none", until it reaches its last entry plus `max_claim_age_ms` (400 days), then deleted the same way | claim limit, its creator's share | as above | - |
| Failed claim (7.8) | a claim whose every attempt is `failed`: until the time base, with trusted time, reaches its pinned deadline plus the 30-day reconciliation window, then deleted like a tombstone; when pinned to "none", until its last entry plus `max_claim_age_ms`; a claim with an open or `unknown` attempt is never deleted by age | claim limit, its creator's share | as above | - |
| Chain stub, and the item's document while it is kept (8.5) | within the generation, until it is older than a checkpoint and either every current app's cursor has passed it or it is `cursor_hold_ms` (30 days) old (8.4.4); dropped oldest first, so the kept ones run unbroken to the head; a cooperative move carries every kept one on (11.2) | control reserve | grows only as fast as the operation and control rates allow (10.4); unchanged control operations add none (7.4); no cursor holds a stub back once it is 30 days old | - |
| Checkpoint (8.4.4) | the latest one | control reserve | one | replaced by a newer one |
| Newest dropped stub (8.4.4) | its `seq` and chain hash, until a newer stub is dropped; an export's checkpoint signs them (11.2) | control reserve | one | replaced by the next one dropped |
| Batch record | its transition proofs `batch_proofs_ms` (30 days), for retries (7.4); its ID and operation hash as long, and also while any of the batch's chain stubs is kept (row above) | control reserve | proofs: bounded by the rates over 30 days; IDs: one per batch of the last 30 days or with a chain stub kept, so bounded like the proofs and the stubs | - |
| Contact | until removed | list limit | `max_contacts` 100,000 | `allow.put` of a new contact is `429` (`contacts`) |
| Conversation entry | until removed | list limit | `max_conversations` 100,000 | `allow.put` of a new entry is `429` (`conversations`); an append admitted by an invite code is `429` (`conversations`) and waits |
| Deny list entry | until removed | list limit | `max_deny` 100,000 | `deny.put` of a new entry is `429` (`deny`) |
| Invite code | until it expires, runs out or is withdrawn | invite code limit, live codes only | `max_invite_codes` 1,000 | `invite_code.put` is `429` (`invite_codes`) |
| Connect request | until answered (a conversation entry, a deny list entry or `DELETE /connect`) | the policy's `max_pending` | `max_connect_pending` 1,000 caps `max_pending` | a sender with no pending request gets `429 quota_exceeded`, `connect_requests` (8.4.7); none drops |
| A frozen account's last `identity.json` and freeze | while frozen, and after a move at least `record_max_age` (8.4.15) | - | one of each | - |
| Sender flag | while among the most frequent | - | `max_flags` 1,000 | the least frequent drop |
| App record, current receive key record, key bundle parts | while the app is listed in `apps` or `removed`, or awaits its rotation | app limit (3.8) | one record and one current receive key record per app; key bundle parts that never overlap (7.2), so at most one key per scope and data epoch | - |
| Retired receive key record | until `max_retired_keys` newer ones exist | control reserve | 8 per app | the oldest drops; an envelope wrapped to it gets `409 stale_key` with its kid in `unknown_kids` |
| Renewed certificate (3.9) | until the app's record carries it, or a newer `cert.put` | control reserve | one per app | replaced |
| Departure, revoked app | its record, receive key records, key bundles, cursor and last-seen time until the signer purges it from `removed` (it is in neither list), after its rotation, and it owns no open or `unknown` attempt; its App ID with its revocation or departure time for good, exported as its `app` part (11.3); likewise each App ID an enrollment names in `revoked_apps` (3.8), dated by the enrollment | app limit, until purged; then nothing | - | - |
| Cursor, last-seen time | while the app is kept | control reserve | one per app | - |
| Push registration and its push key (9.1) | while the app is current; replaced by a new registration | - | one per app, 8 handles | `400` |
| Full-bundle request (8.4.11) | until the stored parts cover every data epoch from 1 to the current one | control reserve | one per app | the first time is kept |
| Evidence uploaded in a move (8.4.14) | until the `identity.json` naming the service arrives, or an abort | - | the proofs apps upload | duplicates are kept once |
| Peer copy of `identity.json` | the newest per identity | peer limit | `max_peers` 10,000 | the least recently delivered drops, never the one just delivered; its sender gets `409 identity_needed` and delivers it again. Strangers' copies (neither contact nor conversation) drop first; a stranger's new copy never pushes out an admitted identity's, it drops itself instead; a dropped admitted copy leaves a floor (version, generation, body hash, 8.4.5) until a copy is held again, one per admitted identity, bounded by the list limits. Floors are exported as `peer_floor` parts (11.3), so the next service keeps them |
| Enrollment answer | one per distinct enrollment | - | `max_enrollments` 16 per account | the oldest drops; that enrollment again gets the same answer, built again |
| Staged import (Section 11) | until the `identity.json` naming the service arrives, or an abort | import limit | `max_import_bytes` 4 GiB; `max_chunk_bytes` 1 MiB per chunk | `POST /import` is `413 too_large`; the signer aborts the move (11.6) |
| Uploaded head (8.4.14) | until the `identity.json` naming the service arrives, or an abort | - | the newest per app | replaced |
| Divergent upload (spec 13.6) | until a newer write at the service | - | `max_divergent_uploads` 8 per record | further uploads are dropped |
| Rate-limit window | the current minute | - | - | - |

**Erasure** (8.4.16, spec 13.7). Every kind in this table, "for good" ones included, goes with the account when the signer erases it, and so do the key's mark (8.3 step 10) and the failed-authentication counts (10.4).

**Forgetting deleted keys** (spec 10.3). The service may forget a deleted key once nothing refers to it: its current write is the delete; the cursor of every current app that isn't lagging is at or past the delete's `seq`; no divergent upload names the key; and the account isn't frozen (the export carries it). A forgotten key's delete keeps only its chain stub. The prototype forgets at every sweep. An app that was lagging lists its collections again on its return (8.4.4), since it may not have seen the delete.

**Sweeps.** Tombstones and failed claims past their deadline plus the reconciliation window, or pinned to no deadline and past their last entry plus `max_claim_age_ms`, are deleted at requests to the account, with trusted time only and by the time base (8.3 step 4), and so are deleted keys that may be forgotten; lagging marks are set, `missed` items written and checkpoints signed at the same time; then the stubs behind the latest checkpoint that the chain stub row lets go are dropped, oldest first, and with them each batch record whose proofs are gone and none of whose stubs is kept (7.4 step 5). At most once a minute of service time, also at a request, the service drops invite codes that expired or ran out, batch proofs past `batch_proofs_ms` (the whole record when none of the batch's stubs is kept any more), past rate-limit windows and flags beyond `max_flags`. Revoked and departed apps are purged when an `identity.json` delivery leaves them in neither list, down to their App ID and revocation or departure time, which stay for good; retired receive key records beyond the limit go when a new one is registered; peer copies and enrollment answers beyond their limits go when a new one is added.

### 10.2 Limits and their names

| Name | Default | Meaning | Announced at enrollment (3.8) |
|---|---|---|---|
| `app_limit` | 32 | most apps in `identity.json`, never above 255 | yes |
| `max_envelope` | 65536 | envelope cap in bytes | yes |
| `max_sealed_value` | 262144 | sealed value plaintext cap in bytes | yes |
| `max_batch` | 8 | entries per batch | yes |
| `quota_bytes` | 67108864 | inbox quota in bytes | yes |
| `record_quota_bytes` | 67108864 | storage quota in bytes | yes |
| `entry_overhead_bytes` | 256 | bytes each inbox entry counts besides its envelope | not yet |
| `max_record_keys` | 100000 | live shared record keys | not yet |
| `max_claims` | 1000000 | live claims plus tombstones | not yet |
| `max_attempts_per_claim` | 64 | attempts on one claim | not yet |
| `open_attempts_per_app` | 1000 | attempts one app holds `open` or `unknown` at once (7.8) | not yet |
| `claim_share_percent` | 25 | the share of `max_claims` that the claims one app created, live and tombstones, may hold: `floor(max_claims * claim_share_percent / 100)` claims (7.8); 100 turns it off | not yet |
| `max_claim_age_ms` | 34560000000 | how long a finished or failed claim pinned to no deadline is kept after its last entry (400 days); an operator may raise it but not lower it, since apps judge a missing claim by it (7.8) | not yet |
| `max_contacts`, `max_conversations`, `max_deny` | 100000 each | list entries | not yet |
| `max_invite_codes` | 1000 | live invite codes | not yet |
| `max_connect_pending` | 1000 | the highest `max_pending` a policy may name | not yet |
| `max_retired_keys` | 8 | retired receive key records per app | not yet |
| `max_peers` | 10000 | peers' `identity.json` copies | not yet |
| `max_enrollments` | 16 | enrollment answers kept | not yet |
| `max_flags` | 1000 | flagged senders kept | not yet |
| `batch_proofs_ms` | 2592000000 | how long a batch's proofs, and at least its ID, are kept (30 days); an operator may raise it but not lower it, since apps compare batch IDs over 30 days (7.6) | not yet |
| `ops_per_min` | 3000 | batches with an ordinary operation or a repeatable control mutation, per app per minute | not yet |
| `control_per_min` | 60 | batches with a repeatable control mutation, per app per minute (7.4) | not yet |
| `cursor_hold_ms` | 2592000000 | how long an app's cursor holds a chain stub back: a stub this old (30 days) may be dropped behind a checkpoint even if a cursor hasn't passed it (8.4.4) | not yet |
| `max_divergent_uploads` | 8 | divergent uploads per record | not yet |
| `max_import_bytes` | 4294967296 | bytes of one staged import; it covers the export of an account at every default limit, with room for the stubs and batch IDs it keeps (Appendix A47) | yes |
| `max_chunk_bytes` | 1048576 | bytes of one export chunk; fixed | not yet |
| `sender_per_min` | 120 | appends per sender identity per minute | not yet |
| `sender_app_per_min` | 120 | appends per sender app per minute | not yet |
| `invite_reads_per_min` | 30 | directory reads and peer deliveries with one invite code per minute | not yet |
| `auth_failures_per_source_per_min` | 60 | failed authentications per network source (an IPv4 address or an IPv6 /64) per minute, at all accounts together; the only limit before a request verifies (8.2) | not yet |
| `stranger_per_min` | 60 | connect requests and peer deliveries of senders admitted only because connect requests are on, per account per minute | not yet |
| `sender_share_percent` | 25 | the share of `quota_bytes` one sender identity's unread entries may hold (10.3); 100 turns it off | not yet |
| `max_invite_uses` | 100 | the most uses an `invite_code.put` may name (7.2); more is `400` | not yet |
| `identity_reads_per_min` | 6000 | public reads of `identity.json` (8.4.15) per account per minute, from all sources together: the backstop above the per-source limit | not yet |
| `identity_reads_per_source_per_min` | 60 | public reads of `identity.json` per network source (an IPv4 address or an IPv6 /64) per account per minute | not yet |
| `list_page` | 1000 | most records per `GET /records` page (8.4.9) | not yet |
| `checkpoint_behind` | off | chain items that may go before the service signs a checkpoint, or with `checkpoint_at_head` chain items added since the last one (8.4.4); the prototype's opt-in | not yet |
| `checkpoint_at_head` | off | sign each checkpoint at the newest chain item, and drop behind it only the stubs the chain stub row lets go (8.4.4); the prototype's opt-in | not yet |
| `mark_lagging` | on | mark lagging apps (8.4.4) | not yet |
| `trusted_time.required` | off | require trusted time: without it, refuse what depends on time with `503 unavailable` (8.3); the prototype's opt-in, since it has no time source of its own | not yet |
| `trusted_time.drift_ppm` | 100 | drift bound of the monotonic clock, parts per million (8.3) | not yet |
| `trusted_time.max_sample_age_ms` | 86400000 | the oldest time sample that still gives trusted time (24 hours) | not yet |
| `trusted_time.max_uncertainty_ms` | 300000 | the largest uncertainty trusted time may have (5 minutes) | not yet |
| `trusted_time.max_disagreement_ms` | 300000 | the most the wall clock may differ from trusted time before the service uses trusted time and alerts (5 minutes) | not yet |
| `notify_per_min` | 20 | appends with `notify` per sender identity per minute | not yet |
| `connect_per_min` | 5 | connect requests per sender identity per minute | not yet |

"Not yet": the prototype doesn't announce these; until a service does, clients assume these defaults. The names are the prototype's configuration names (Appendix A13).

### 10.3 Quotas, per-kind limits and the control reserve
- **Two quotas.** The **inbox quota** counts inbox entries, each with `entry_overhead_bytes` beside its envelope, so it bounds their number as well as their bytes; it blocks only appends. The **storage quota** counts current shared record values; it blocks only a `record.put` that makes them larger, so a deletion or a rewrite that doesn't grow always passes. They are separate, so a full inbox never blocks the record writes that drain it.
- **Each sender's share** (spec 6.3). The inbox quota is the identity's, so one admitted sender within its rate limit could fill it while the identity's apps are offline, and every other sender would then get `429`. So one sender identity's unread entries hold at most `sender_share_percent` of the inbox quota (a quarter by default): an append past it gets `429 quota_exceeded` with `current.limit` `sender_share`, for that sender only, retried as `inbox` is. Unreadable entries and `missed` items, which the service makes itself, count for no sender.
- **Per-kind limits** refuse only the write that would create something new: a live record key (a new one, or a deleted one written again), an `attempt` on a new claim key or beyond the limit on one, or beyond its app's share of the claim limit or its open attempts (7.8; an `attempt` beyond several of these gets the first of `claims`, `claim_share`, `attempts` and `open_attempts`, in that order), a contact, a conversation entry, a deny list entry, an invite code. A write to an existing object always passes, and so do outcomes, deletions and removals.
- **Receivers never stall** (spec 8.2): an app whose record write for an entry gets `429 quota_exceeded` for a per-kind limit or the storage quota stores the entry locally, moves its cursor past it, and writes the record once room frees (5.1).
- **Control reserve.** Control operations count against neither quota and no per-kind limit: cursor moves, deletions and removals, claim outcomes (`done`, `failed`, `unknown`), `app.put`, `receive_key.put`, `key_bundle.put`, `cert.put`, `identity.json` deliveries, revocation, leave, freeze, unfreeze, the export and the import, and recovery uploads. What they create is bounded as follows; the operator plans that capacity apart from the quotas:
  - an unchanged `app.put`, `receive_key.put`, `cert.put` or `key_bundle.put` creates nothing (7.4): a batch made only of them is a no-op, or with `cursor.set` it is checked and then moves only the cursor;
  - a repeatable control mutation (an `app.put` or `receive_key.put` that changes what the service holds, a delete or removal of something that isn't there before the batch, 7.4) counts against its app's control rate, 60 batches a minute, and its operation rate (10.4), so chain stubs grow at most that fast, and are dropped behind a checkpoint, oldest first, which no cursor holds back from a stub 30 days old (8.4.4);
  - batch proofs go after 30 days, and a batch record, ID included, goes once its proofs are gone and none of its stubs is kept (7.4 step 5);
  - receive key records, key bundle parts, cursors and push registrations are bounded per app (10.1).

  Cursor moves, claim outcomes on existing attempts, deletes and removals of something present, and revocation and leave are never rate-limited, so a full inbox can still be drained, an existing payment reconciled and an abusive app revoked.
- **`429 quota_exceeded`** names the full budget in `current.limit`: `inbox`, `sender_share`, `storage`, `record_keys`, `claims`, `claim_share`, `attempts`, `open_attempts`, `contacts`, `conversations`, `deny`, `invite_codes` or `connect_requests`. It carries no `retry_after`. Retry per limit (8.6): `inbox` and `sender_share` as the recipient's apps read their feed; `storage` after deletions; `record_keys` after deletions, once the service forgets the deleted keys; `connect_requests` once the recipient answers pending requests, which the sender can't see, so a sender asks again much later or not at all; `open_attempts` once the app's own attempts are resolved; `claim_share` once claims the app created are deleted, at their deadline plus 30 days or, without one, 400 days after their last entry; the others after a deletion or removal frees room. Vectors: the `quota` steps of `claims.json` hold the `open_attempts` and `claim_share` answers. The prototype's answer when the claim limit is full, which no vector holds:

  ```json
  {
    "code": "quota_exceeded",
    "message": "6.3: the identity holds its limit of claims",
    "current": {
      "limit": "claims"
    }
  }
  ```

### 10.4 Rate limits

| Limit | Counts | Per | Default | Answer |
|---|---|---|---|---|
| Appends | every `POST /inbox`, `executor_notice` from an own app included | sender identity | `sender_per_min` 120 a minute | `429 rate_limited` |
| Appends per app | every `POST /inbox` from an app of another identity | sender app | `sender_app_per_min` 120 a minute | `429 rate_limited` |
| Invite-code reads | `GET /directory` and `PUT /peers` admitted by an invite code | invite code | `invite_reads_per_min` 30 a minute | `429 rate_limited` |
| Strangers | `POST /connect`, and `PUT /peers` admitted only because connect requests are on, from all senders together | account | `stranger_per_min` 60 a minute | `429 rate_limited` |
| Notifying appends | appends with `notify` set, besides the above | sender identity | `notify_per_min` 20 a minute | `429 rate_limited` |
| Connect requests | `POST /connect` | sender identity | `connect_per_min` 5 a minute | `429 rate_limited` |
| Failed authentications | requests answered `401` (8.2 Order), checked before any signature work; counted per claimed caller too, for monitoring only | network source (an IPv4 address or an IPv6 /64), all accounts together | `auth_failures_per_source_per_min` 60 a minute | `429 rate_limited` |
| Public `identity.json` reads per source | `GET /identity` (8.4.15), unauthenticated | network source (an IPv4 address or an IPv6 /64) per account | `identity_reads_per_source_per_min` 60 a minute | `429 rate_limited` |
| Public `identity.json` reads, backstop | the same, a source's refused reads not counted | account, all sources together | `identity_reads_per_min` 6,000 a minute | `429 rate_limited` |
| Operations | `POST /ops` batches from an own app carrying an ordinary operation or a repeatable control mutation (7.4) | app | `ops_per_min` 3,000 a minute | `429 rate_limited` |
| Control mutations | `POST /ops` batches from an own app carrying a repeatable control mutation (7.4) | app | `control_per_min` 60 a minute | `429 rate_limited` |

- **Windows** are fixed minutes of service time; the failed-authentication limit, kept across accounts, uses fixed minutes of the time base (8.2). A limit of n a minute takes n requests in a window and refuses the next. `retry_after` in the body and the `Retry-After` header give the seconds to the next window.
- **Flags.** Each append or connect refusal counts a hit for the sender identity, a per-app refusal included; one with at least 3 hits (`flag_after`) shows in `GET /allow` `flagged` (8.4.6). The service never blocks a sender on its own (spec 7); apps apply the spam rule (5.1). Operation refusals aren't flagged.
- **Where they apply.** For an append, after admission, the listing and the size checks, before the targets and the quota (8.4.3). For a batch, after the retry and generation checks, before the conditions (7.4). Every limit keyed by a caller, an app, an invite code or an account applies only to a request that has verified. Before that only the failed-authentication limit applies, by network source (8.2), and the public read's limits (8.4.15), the one call without authentication.

### 10.5 The 429 answers of each call

| Call | `429 rate_limited` | `429 quota_exceeded`, `current.limit` |
|---|---|---|
| `POST /inbox` | appends, appends per app, notifying appends | `sender_share`, `inbox`; `conversations` for an append admitted by an invite code |
| `POST /connect` | connect requests | `connect_requests`, while `max_pending` requests are pending, to a sender with none (8.4.7) |
| `POST /ops` | operations, control mutations | `record_keys` (a `record.put` creating a key), `storage` (a `record.put` growing the stored bytes), `claims`, `claim_share`, `attempts` and `open_attempts` (`attempt`), `contacts` and `conversations` (`allow.put`), `deny` (`deny.put`), `invite_codes` (`invite_code.put`) |
| `GET /directory` | invite-code reads | never |
| `GET /identity` | public reads | never |
| every other call | never | never |

Any call that authenticates can also get `429 rate_limited` before its signature is checked, once its network source has had `auth_failures_per_source_per_min` failed authentications in the current minute (8.2).

### 10.6 Dropping is safe
- A sender whose peer copy dropped gets `409 identity_needed` at its next append and delivers its `identity.json` again, never lower than the dropped copy's floor (8.4.5).
- An enrollment repeated after its answer dropped gets the same answer, since the answer is built from the account and the enrollment alone.
- A batch retried within 30 days gets its original answer. After that its proofs are gone and it gets `409 conflict`, as does another batch carrying its ID, for as long as the service holds the ID: while any of the batch's chain stubs is kept. An app's verifier takes a batch for a repeat only when `t_new - t_old < 30 days`, by the `server_time` of each batch's first stub (7.6), and the service holds every ID at least that long, so no batch the service accepts makes an honest chain look rolled back. The service forgets the ID once both have ended, when none of the batch's stubs is kept; by then no app compares it. Requests are accepted only within 5 minutes of their signed timestamp, so nobody can replay a batch, list operations included, which carry no expected version and could otherwise undo a later change: it can come again only in a new request, which only its app can sign, and apps resend a batch only in their immediate retries.
- An envelope to a dropped retired receive key gets `409 stale_key`, and the sender seals it again.
- A tombstone or failed claim deleted 30 days after its deadline is safe because no compliant app can start an attempt once the service's time reaches the pinned deadline (7.8): every `attempt` carries the same deadline. This assumes the first `attempt` pinned the deadline every app computes from the request; an app that finds an earlier pin keeps its evidence to its own deadline plus 30 days and treats the obligation as doubtful (7.8). The 30 days are for late receipts (7.8 Failed claims). A payment without a request carries a deadline 10 minutes ahead (spec 11.4), so its tombstone goes 30 days after it settles rather than 400 days after its last entry.
- A finished or failed claim pinned to no deadline, deleted 400 days after its last entry, leaves its obligation FREE at the service, so a re-sent copy of its request could be attempted again. No compliant app does: it never attempts an obligation whose activity record carries the proof of a payment (spec 10.4), and it never deletes its record of a paid obligation without a deadline (spec 8.3). The trade-off: after 400 days the payer's record, not the claim, prevents a second payment, at the cost of one record per payment without a deadline for the identity's life. Claims with an open or `unknown` attempt are never deleted by age, so no unresolved obligation is lost, and an app keeps its evidence of such a claim as long as the service may keep the claim (7.8).
- A forgotten deleted key is safe because a record created there again gets a newer version than any the key had (7.2), and an app that may have missed the delete lists its collections again (8.4.4).
- Chain stubs dropped behind a checkpoint are safe because every current app's cursor has passed them, or they are 30 days old. An app whose verified chain item is less than 30 days old therefore still has the stubs after it, and reports a rollback if they are missing; an app more than 30 days behind, or restored from a backup, can't verify continuity across them and continues from the checkpoint head only as an explicit recovery in reconcile mode (8.4.4). A cooperative move drops none of them: the export carries every stub O keeps (11.2). In a move N ignores such an app's head when it is older than the checkpoint, and the app uploads none while its continuity is unverified (8.4.14, 11.5).

## 11. Export and import
Spec 13.5 step 3 and Abort. The frozen old service O exports the account as a manifest and chunks; the staged next service N fetches the chunks from O under the signer's export transfer (3.12), or the signer moves them as a fallback, and N activates the import in one step. Every step can be repeated with the same result, so an interrupted signer resumes by asking N what it lacks.

### 11.1 Overview
1. After the freeze (8.4.13) the signer reads O's manifest (`GET /export`) and checks it: O's service key signed it, and it names the signer's freeze.
2. It signs the export transfer (3.12) and starts the import at N with the manifest, the signed freeze and the transfer (`POST /import`). N fetches each chunk it lacks from O (`GET /export/chunks/{chunk_id}` under the transfer, 8.2), checks its length and hash against the manifest and stores it, then answers with the chunks it still lacks (11.4).
3. Fallback, for each chunk still missing (N couldn't reach O, or stopped at a failure): the signer fetches the chunk from O itself (`GET /export/chunks/{chunk_id}`), checks its length and hash against the manifest, and stores it at N (`PUT /import/chunks/{chunk_id}`), in any order, as often as needed. Without a transfer in the start, N fetches nothing and the signer moves every chunk.
4. It activates the import (`POST /import/activate`). N rebuilds and verifies the state, then makes it live.
5. It delivers the `identity.json` naming N, generation g+1, and then publishes it (spec 13.5 step 4). N refuses that document (`409 conflict`) before the activation.

After the freeze, apps read O's feed to its end (its pages carry the freeze, 8.4.4), then upload to N their newest signed head and the evidence they hold (8.4.14); the activation checks the chain against each head and the state against the evidence (11.5). An abort (11.6) cancels at both services, N first.

**What the export holds:** the account's state, as parts (11.3): its `identity.json`, sequence and directory counters, every chain item's stub with its document while O keeps it, pending inbox entries, apps (records, receive key records with their `seq`s, key bundles, cursors, last-seen times, departures, renewed certificates), live claims and tombstones, shared records with the transition proof of each current write, batch records (IDs, operation hashes and times, not their proofs), contacts, conversation entries, the deny list, invite codes, connect requests, flags, peers' `identity.json` copies, the floors dropped peer copies left, and divergent uploads. Each operation travels once, since a shared record names the chain part that holds its operation instead of carrying it again, and no batch proofs travel: they serve only a retry at O, and N couldn't serve them as its own. A record's proof does travel, since it dates the record's current write for readers (spec 10.3), who check it with its generation's key, O's or one `service_keys` lists (3.3). **Not exported:** O's service key (N has its own), enrollment answers, rate-limit windows, push registrations (apps register again with N, spec 9), peer delivery order and staged state. N rebuilds its kid index and reachable apps from the parts.

### 11.2 Manifest
A signed document (2.2) under O's service key, label `paykit/v1/export-manifest`, signed over `lp(label, identity, body)`. Its **manifest hash** is its document hash.

| Field | Type | Req | Notes |
|---|---|---|---|
| `v` | int | yes | 1 |
| `kind` | string | yes | `paykit.export_manifest` |
| `identity` | pubky | yes | the account's identity |
| `freeze_hash` | `b64(32)` | yes | the document hash of the signed freeze this export belongs to |
| `generation` | int | yes | g, the exported generation |
| `next_generation` | int | yes | g+1, as the freeze names it |
| `last_time` | time | yes | the last service time O committed for the identity when it built the export, so at or after the time of the freeze transition (the head's item), its `at`, which N checks (11.5); N continues from it (spec 10.1) |
| `head` | signed head | yes | O's head at the end of the export, at the freeze's own chain item: the `freeze` transition (7.5) of the freeze this export belongs to, which N checks (11.5) |
| `checkpoint` | `signed head \| null` | yes, always written | a head O signs for the newest stub it dropped, at the `seq` and chain hash it keeps for it (10.1). O drops stubs oldest first, so that is the chain item just before the oldest stub it keeps, and its chain hash is that stub's `prev_chain_hash`; the `chain` parts start at that stub, from this chain hash instead of 32 zero bytes. `null` when O keeps the whole chain: they start at the first item. It needn't be O's latest checkpoint (8.4.4), which may lie ahead of stubs O keeps. An export leaves out none of them, so every app whose stubs O's cursor rule keeps finds them at N |
| `parts` | int | yes | the number of parts |
| `bytes` | int | yes | the sum of the chunks' `len` |
| `chunks` | `array<object>` | yes | the chunks in part order, below |

A chunk reference:

| Field | Type | Notes |
|---|---|---|
| `id` | string | opaque, used in the chunk URLs; the prototype writes the chunk's index as five decimal digits (`00000`) |
| `len` | int | the chunk's byte length, at most `max_chunk_bytes` (1 MiB) |
| `hash` | `b64(32)` | `hash(chunk bytes)` |
| `first`, `last` | int | the indexes of its first and last part; the first chunk starts at 0, each next one at the previous `last` + 1, and the last ends at `parts` - 1 |

- **Once per freeze.** O builds the export at the first `GET /export` or chunk fetch after the freeze and keeps it while frozen: every retry gets the same manifest bytes and the same chunk bytes. An unfreeze discards it.
- **Packing.** Parts go into chunks in order, each chunk up to O's chunk size (1 MiB by default; 64 KiB in the prototype's tests); a part larger than that gets a chunk of its own. Every part fits one chunk of 1 MiB with the chunk's own members (11.3 Part sizes), so every account the other limits allow can move.
- **Vectors**: `export.json` is a complete export of alice's account, frozen right after `chain.json`'s last item, with a manifest signed with the fixed service key of `keys.json`. It activates under 11.5 at a next service enrolled with her current `identity.json` (version 3) whose trusted time is the freeze's. Its one chunk holds the meta part; the chain items of `chain.json`, the superseded `record.put` of `seq` 6 as a stub only; the freeze transition at `seq` 10, which the head names; an `app` part for each app the `identity.json` lists; the live claim of `seq` 7 and its one entry, a `claim_entry` part; the record at (1, 8), its `op` by reference; and a `batch` part, without proofs, for each of the three batches the stubs name. `last_time` is the freeze transition's time. A real service draws each account's key at random (11.7), so this is the only export with a reproducible signature.

### 11.3 Chunks, parts and the export calls
A **chunk** is the exact bytes of one JSON document (1.1):

| Field | Type | Notes |
|---|---|---|
| `v` | int | 1 |
| `kind` | string | `paykit.export_chunk` |
| `index` | int | its position in the manifest's `chunks` |
| `first` | int | the index of its first part, as the manifest says |
| `parts` | `array<part>` | consecutive parts |

A **part** is an object whose `part` member names its kind; the other members follow. N refuses an unknown kind (`400`).

**Part order.** Each kind is a group of its own, and the groups come in this order: `meta` (exactly one, first); `chain` (by `seq`); `entry` (by `seq`); `app` (by `app_id`), each followed by its `key_bundle` parts (by `first_epoch`); `claim` (by `claim_key`), each followed by its `claim_entry` parts (by `step`); `tombstone` (by `claim_key`); `record` (by `record.key`); `batch` (by `id`); `contact` (by `pubky`); `conversation` (by `pubky`); `deny` (by `pubky`); `invite_code` (by `code_hash`); `connect` (oldest first, by `at`); `flag` (by `pubky`); `peer` (by the `identity` its document names); `peer_floor` (by `identity`); `divergent` (by `key`, then `index`). So `claim` and `tombstone` are two groups, and so are `contact`, `conversation` and `deny`. Within a group a `seq` sorts as a number, and every other key by the bytes of its text as the part writes it (the base64url or z-base-32 string, compared as ASCII), not by the bytes it decodes to; keys ascend strictly, so each appears once. An app's `key_bundle` parts come right after its `app` part, before the next app's, with `first_epoch` strictly ascending; a live claim's `claim_entry` parts right after its `claim` part, with `step` strictly ascending; and the `divergent` parts of one record one after another, `index` 0, 1, 2 and so on. `connect` parts are in non-decreasing order of `at`, compared as instants (1.6). N refuses parts out of this order, a `key_bundle` or `claim_entry` part that doesn't follow its app's or claim's part, and a record's `divergent` parts whose indexes don't run 0, 1, 2 in order or that number more than 8 (`400 bad_request`, 11.5).

Parts carry the service's own state in the prototype's form: times in Unix milliseconds in members named `_ms`, durations in seconds in members named `_s`, and objects with the service's bookkeeping. A member without such a suffix that holds a time, such as `at` and `expires_at`, is a time of 1.6, an RFC 3339 string.

Every listed member is present, except where the table says otherwise: a member marked "or `null`" may also be absent, which reads as `null`, and one marked "absent otherwise" is present only in the case named.

| Part | Members |
|---|---|
| `meta` | `identity` (the newest signed `identity.json`), `generation` (g), `seq` (the last `seq` given out, entries and chain items), `directory_version`, `apps_version`, `app_added_at` (each app reachable now, current with a receive key (8.4.10), by App ID, to the `apps_version` at which it became reachable; at most the app limit, since the service keeps no entry for an app that isn't reachable) |
| `chain` | one for every stub O keeps, from the oldest (11.2): `seq`, `stub` (7.6), `item_type` (`op`, `identity` or `service`), `doc` (the signed document, or `null` once O dropped it, 8.5), `context` (the invite code context of a service-written conversation entry, or `null`) |
| `entry` | `seq`, `sender`, `cert` (the sender app's certificate), `accepted_at_ms`, `context` (or `null`), `envelope` (`b64`; empty for an unreadable entry), `targets` (App IDs), `result` (the original append answer `{ entry_id, accepted_at, directory_version }`, which a retry by content gets, 8.4.3), `unreadable` (bool), `missed` (`{ entries, from_ms, until_ms }` for a `missed` item, 8.5; absent otherwise) |
| `app` | `app_id`, `record` (signed app record, or `null`), `record_seq` and `record_at_ms` (its `app.put`, or `null`), `receive_keys` (pairs `[signed receive key record, seq]`, oldest first, the last current), `cursor` (a `seq`, 0 before the app's first cursor move), `last_seen_ms`, `revoked_at_ms`, `departed_at_ms`, `departure_epoch` (each `int` or `null`), `rotated` (bool), `renewed_cert` (`[signed certificate, seq]` of a `cert.put` the record doesn't carry yet, or `null`), `lagging` (bool), `keys_through` (`int` or `null`), `full_bundle_requested_ms` (`int` or `null`) (8.4.12). A purged app's App ID is exported the same way, as its tombstone: `revoked_at_ms` or `departed_at_ms` set, `record` and `record_seq` `null`, no receive keys, and no `key_bundle` parts (10.1). The app's stored key bundle parts are `key_bundle` parts of their own, right after this one; an `app` part with a `key_bundles` member, as an earlier form carried them, is refused (`400`), whatever the member holds, so an earlier exporter's parts never vanish unseen |
| `key_bundle` | `app_id`, `bundle` (one key bundle part stored for that app, 3.5), `seq` (its `key_bundle.put`), `first_epoch`, `last_epoch`: one part for each part the service stores for the app, so an app offline through many rotations never makes one export part large |
| `claim` | `claim_key`, `claim`: `step`, `attempts` (below), `current` (the current attempt ID, or `null`), `settled` (bool), `divergent` (bool), `pinned_deadline` (absent before any attempt, `null` for a pin to "none", else Unix milliseconds), `rebuilt` (bool: rebuilt from uploads after a provider loss), `creator` (the App ID whose `attempt` created the claim, 7.8: the owner of its first attempt, `null` only for a claim without one), `last_entry_ms` (the service time at which its last entry was applied, or `null`; for a claim not rebuilt, the `server_time` of its last `claim_entry` part's proof stub). N checks both (11.5). Its entries are `claim_entry` parts, right after it; a `claim` part with an `entries` member is refused (`400`), as an `app` part with `key_bundles` is |
| `claim_entry` | `claim_key`, `step` (the claim's step before the entry), `seq` (its chain item), `op` (the signed claim entry) and `proof` (its transition proof), both always present: one part for each entry of the live claim, since each proof carries the claim view and a claim near its attempt limit would not fit one part |
| `tombstone` | `claim_key`, `tombstone`: `step`, `attempts` (below), `deadline_ms` (the pinned deadline, or `null`), `creator` and `last_entry_ms` (as in `claim`; without a deadline the tombstone goes `max_claim_age_ms` after `last_entry_ms`, 7.8). N checks `creator` against the tombstone's first attempt, and takes `last_entry_ms` as written, since a tombstone keeps no entries (11.5) |
| `record` | `record`: `key`, `collection`, `read_scope`, `write_scopes`, `generation`, `version` (the `seq` of its current write in `generation`, 7.2), `op` (the signed `record.put` of the current version, with its sealed value, or `null` when the `chain` part at `seq` carries it; always carried for a record at or behind the `checkpoint` and one taken from a recovery upload. An exporter writes `null` there; N takes a part that carries it anyway when it is the chain part's operation byte for byte, and refuses another one, `409 conflict`), `seq` (the chain item at this service that holds `op`: equal to `version` for a write made here, and the upload's `seq` for a record taken from a recovery upload), `deleted`, `writer` (App ID), `from_upload` (bool, set by a recovery upload), `upload_base` (`[generation, version]`, the uploaded version that writes at N built on, 8.4.14, or `null`), `size` (its bytes for the storage quota, 10.1: its `ct` decoded, 0 for a delete), `cert` (the writer's certificate the service checked the write against, 8.4.9, or `null` when it is the one the record in the writer's `app` part carries), `proof` (the transition proof of the current write that O keeps with the record, 8.4.9, signed by O or, after an earlier move or key change, by the service of the write's generation; absent for a record taken from a recovery upload, and required otherwise, 11.5). N derives every member but `op`, `seq`, `upload_base`, `cert` and `proof` from the operation and checks they match (11.5) |
| `batch` | `id`, `record`: `ops_hash` (`hash(lp(signed document bytes of each operation, in order))`, which recognizes a retry, 7.4), `proofs` (always empty in an export: N answers a retry of the batch as one whose proofs are gone, 7.4; a batch part with proofs is refused, `400`), `at_ms` |
| `contact`, `deny` | `pubky` |
| `conversation` | `pubky`, `written_by` (App ID or `service`), `code_hash` and `context` (when an invite code created it; absent otherwise), `at` (time, 1.6: when the entry was written), `last_used` (time of the last append it admitted; absent until one) |
| `invite_code` | `code_hash`, `expires_at`, `expires_at_ms` (ignored on import: N recomputes it from `expires_at`), `max_uses`, `uses`, `context` (any JSON value, `null` included), `written_by` (the App ID that registered it) |
| `connect` | `pubky`, `at` (time, 1.6: when the request arrived) |
| `flag` | `pubky`, `hits` |
| `peer` | `doc`: the signed `identity.json` of another identity |
| `peer_floor` | `identity` (pubky), `version`, `generation`, `body_hash` (`b64(32)`): the floor a dropped peer copy left (8.4.5, 10.1). O holds no floor for an identity whose copy it holds, so a `peer_floor` beside a `peer` part of the same identity is refused (`400`). A floor is O's word, like the admission lists: one set too high makes N refuse that identity's older deliveries, as O would have |
| `divergent` | `key`, `index` (its place among the record's divergent uploads, from 0), `upload`: one uploaded signed `record.put` that conflicts with the record (spec 13.6). One part for each divergent upload, at most 8 for a record (10.1) |

An **attempt**, in claims and tombstones: `attempt_id`, `owner` (App ID), `outcome` (`open`, `done`, `failed` or `unknown`), `opened_at_ms`, `orphan_after_s`, `possibly_duplicate`, `duplicate`, `deadline_ms` (or `null`), `from_upload` (bool: rebuilt from a recovery upload, always orphaned), `resolved_by` (the App ID that wrote its outcome, or absent).

Deleted record keys are exported while the account is frozen (10.1), so N holds every delete an app may still need to see.

**Part sizes.** Every part fits one chunk of 1 MiB (1,048,576 bytes) with the chunk's own members, which take at most 97 bytes. The largest JSON of each kind, at the default caps (10.2), with every integer at 2^53 - 1 but a `divergent` part's `index`, which is at most 7 since a record holds at most 8 divergent uploads, and every certificate and receive key record carrying 4 suite keys of 4,096 bytes (30,918 and 29,693 bytes signed):

| Part | At most (bytes) | What bounds it |
|---|---|---|
| `meta` | 274,016 | its `identity.json` within `PUT /identity`'s 256 KiB body (262,119 bytes), and `app_added_at` for at most 255 reachable apps |
| `chain` | 919,240 | the stub (554), a context (1,024) and the document: an operation (512 KiB, 7.1), an `identity.json` (262,119), or a service transition, of which an `upload` is the largest: each item of the 16 MiB upload takes at least 1,122 bytes, so it names at most 14,952 keys (917,574) |
| `entry` | 127,138 | the envelope (64 KiB, 87,382 in base64url), the sender's certificate, 255 targets and a context |
| `app` | 691,703 | the app record, which its `app.put` bounds (392,924), the current and 8 retired receive key records and a renewed certificate |
| `key_bundle` | 87,645 | one key bundle part, an envelope of at most 64 KiB |
| `claim` | 19,558 | 64 attempts, each `unknown` with `resolved_by`, so none is current, and each after the first `possibly_duplicate` (7.8 row 1) |
| `tombstone` | 19,494 | 64 attempts, each `done` or `failed` with `resolved_by`, one at least `done` |
| `claim_entry` | 548,952 | the entry (512 KiB) and its proof, whose claim view holds at most 64 attempts (24,522, ATTEMPTING with 63 of them `unknown`) |
| `record` | 949,261 | its operation when carried (512 KiB; a 256 KiB value takes 466,897), its `read_scope` and `write_scopes` again (at most what the operation leaves, 392,563), the certificate and the proof (1,078) |
| `divergent` | 524,380 | one uploaded operation |
| `peer` | 262,153 | a peer's `identity.json` within `PUT /peers`' 256 KiB body |
| `batch`, `contact`, `conversation`, `deny`, `invite_code`, `connect`, `flag`, `peer_floor` | 1,292 | identifiers, times, counts and a context of at most 1,024 bytes |

The largest, a `record` part, takes 949,358 bytes of a chunk. Three kinds that could outgrow a chunk are split into parts of their own: an app's stored key bundle parts, which grow with the data epochs an offline app missed (`key_bundle`); a live claim's entries, up to four for each of 64 attempts, each proof carrying the claim view (`claim_entry`); and a record's divergent uploads, eight values of up to 256 KiB (`divergent`, one each).

**`GET /v1/{id}/export`** (signer). Answer `{ "manifest": signed<Manifest> }`. Errors: `403 permission_missing` for another caller; `409 conflict` when the account isn't frozen; `404` without an `identity.json`.

**`GET /v1/{id}/export/chunks/{chunk_id}`** (signer, or the next service under the export transfer). Answer `{ "id": string, "data": b64 }`, `data` the exact chunk bytes. Errors: as `GET /export`, `404` for a chunk ID the manifest doesn't list, and for the next service those of 3.12. Whoever fetches it checks `len` and `hash` before the chunk is stored at N. The answer to the next service carries no head.

### 11.4 Import
**`POST /v1/{id}/import`** (signer, at a staged account). Body `{ "manifest": signed<Manifest>, "freeze": signed<Freeze>, "transfer": signed<ExportTransfer> }`, `transfer` optional (3.12). N checks, in order:
1. The caller is the signer (`403 permission_missing`), and the account is staged (`409 conflict`).
2. The freeze verifies with the identity key (`401 bad_signature`) and names this identity and this service's origin and service key (`400`); its hash isn't one an abort cancelled here (`409 conflict`).
3. The manifest verifies with the service key of the `identity.json` N holds, the one it was enrolled with (`401 bad_signature`; `409 conflict` without one).
4. An import already in progress: the same manifest hash returns its status. Another manifest is refused (`409 conflict`) while the import is activated or bound to a freeze at least as new (`issued_at`); a manifest bound to a newer freeze replaces a partial import.
5. The manifest is bound to this move: its `identity`, its `freeze_hash` is the freeze's, its `generation` is the held `identity.json`'s generation g, and its `next_generation` and the freeze's are g+1 (`400`).
6. Its chunk list is well formed: unique IDs, part ranges as in 11.2, `bytes` the sum of `len` (`400`); no chunk is over 1 MiB, and `bytes` is within `max_import_bytes` (`413 too_large`).
7. A `transfer` verifies with the identity key (`401 bad_signature`) and is for this move: its `freeze_hash` is the freeze's, its `manifest_hash` the manifest's, its `from` the origin and key of O as the held `identity.json` names them, and its `to` this service and the account's key here (`400`). N keeps it for the import, a later start with a valid one replacing it, and before it answers fetches from O each chunk it lacks, under it, one at a time, storing each as `PUT /import/chunks` would; a failed fetch stops there. The state lock isn't held while it fetches. A production service may fetch in the background instead, once it has answered; the signer then polls by sending the same start again, which step 4 answers with the import status. The prototype fetches before it answers (A50).

Answer, the **import status**:

| Field | Type | Notes |
|---|---|---|
| `manifest` | `b64(32)` | the manifest hash |
| `chunks` | int | the number of chunks in the manifest |
| `missing` | `array<string>` | the IDs of the chunks N doesn't hold yet, in manifest order |
| `activated` | bool | |
| `pulled` | int | how many chunks N fetched from O under the transfer, 0 without one |
| `pull_error` | string | present when N's last fetch from O stopped: which chunk and why, so the signer moves what is missing itself |

**`PUT /v1/{id}/import/chunks/{chunk_id}`** (signer, at a staged account). Body: the exact chunk bytes. Checks: the body is at most 1 MiB (`413`); an import is in progress (`409 conflict`); the manifest lists the chunk (`404`); its length and hash are the manifest's (`409 conflict`, `current: { "chunk": chunk reference }`, and nothing changes); a chunk not held yet parses as a chunk whose `index`, `first` and number of parts are where the manifest puts it (`400`). A chunk already held is a no-op. Answer: the import status. Nothing is live yet.

### 11.5 Activation
**`POST /v1/{id}/import/activate`** (signer, at a staged account). Body `{ "manifest": b64(32) }`, the manifest hash.
1. An import is in progress and is for that manifest, else `409 conflict`. Once it is activated, the same body gets the same answer.
2. Every chunk is held, else `409 conflict` with `current: { "missing": [chunk ID, ...] }`. N has trusted time (spec 10.1), else `503 unavailable`, since it bounds `last_time` by it.
3. N reads every part (`400` for one whose members don't read as 11.3 gives them, a listed member left out included). It then rebuilds the state from the parts, in order, gathering each app's `key_bundle` parts into its stored key bundle parts, each live claim's `claim_entry` parts into its entries and each record's `divergent` parts into its divergent uploads, as the parts of 11.3 hold them, and verifies it with the checks below, in the order listed, answering with the first that fails. A `chain` part out of `seq` order, which breaks both the order and the links, is therefore refused by the order check (`400`), before the link check (`409`). The checks:
   - the parts are in the order of 11.3 (`400`);
   - the first part, and only the first, is `meta`, and its `identity.json` verifies for the identity (`400`, or `401 bad_signature`);
   - that `identity.json` has the generation g and at least the version of the one N was enrolled with, and is that very document at that version (`409 conflict`), so O can't hand N an older version, such as one still listing an app the signer revoked, which the signer would be told to build on;
   - the manifest's head verifies with O's key (`401 bad_signature`), and so does its `checkpoint`, if any (`401 bad_signature`);
   - O chooses the checkpoint, so N checks where it lies: in generation g, or an earlier one the chain continued from, and before the head (`409 conflict`);
   - the meta part's `generation` is g and its `seq` is the head's (`409 conflict`): a frozen account gives out no `seq` after its freeze transition, and a `seq` behind it would make N give one out twice;
   - each `chain` part's chain hash follows from its envelope, its `seq` is the part's and above the previous one, and its `prev_chain_hash` is the previous chain hash, from the checkpoint's chain hash, or 32 zero bytes without one; the parts pass the other checks an app makes on each stub (7.6), the stamp order and the batch rules included, with no previous time right after the checkpoint, and generations that never go back and end at g (the stubs O keeps may begin in an earlier generation, after an earlier move), so the apps O kept stubs for read them at N without a rollback; a part with its document matches its stub's item type and hash (`409 conflict`);
   - the chain ends at the head: its last chain hash and `seq` are the head's, and the head's generation is g (`409 conflict`);
   - the head is the freeze transition: the last `chain` part, at the head's `seq`, is a `service` item with its document, a `freeze` transition (7.5) that verifies with O's key and names the manifest's `freeze_hash` (`409 conflict`). An export that ends earlier could leave out what O chained before the freeze;
   - the meta part's `identity.json` is the newest `identity` item of the `chain` parts, by item hash, when they hold one (`409 conflict`);
   - the manifest's `last_time` is at or after that transition's time, its `at` (7.5), else `409 conflict`. O sets `at` to the service time at which it chains the transition, so it equals the `server_time` of the transition's chain envelope (7.6). N continues from `last_time`, so an earlier one would put its time before what O chained last;
   - neither `last_time` nor that `at` is more than the clock tolerance (5 minutes) ahead of N's own time (`409 conflict`). N continues from `last_time`, so a time further ahead would move its clock ahead for good: certificates expiring, deadlines passing and sweeps deleting early;
   - every head an app uploaded at or after the checkpoint is at or before that end and has the chain hash the chain has at its `seq` (`409 conflict`). A head older than the checkpoint can't be checked against the chain parts, which start after it, so N ignores it: it is neither evidence for nor against the import. An honest app holds one only if O dropped the stubs after its position, as O may for an app more than 30 days behind, or for one restored from a backup whose position its cursor at O had passed (8.4.4). Such an app uploads no head while its continuity is unverified (8.4.14);
   - the apps: there is an `app` part for every app the imported `identity.json` lists in `apps`, and for every app whose `app.put` or `receive_key.put` a `chain` part holds with its document (`409 conflict`), so O can't leave an app out; app records, receive key records and renewed certificates verify (`400`, or `401 bad_signature`); the record and the renewed certificate name the part's App ID (`400`), and each key bundle part carries the identity's signature (`401 bad_signature`); an app's receive key records come in strictly increasing `n` and `seq`, each one after the checkpoint is the `receive_key.put` chain item at its `seq` with that record, and the newest `receive_key.put` the chain parts hold for the app is its last one, unless the app is a purged app's tombstone that the imported `identity.json` doesn't list (`409 conflict`), so O can't make an older key current; a `leave` or `revoke` transition in the chain parts makes the app departed or revoked at N, whatever its part says: N adds those flags from the chain and never clears one, and a departed app still listed awaits its rotation;
   - every shared record is derived from its signed operation, its `op` or the `chain` part at its `seq`, never taken as written. The operation verifies with the writer's certificate, the part's `cert` or else the one in the writer's `app` part (`401 bad_signature`; neither is there, `409 conflict`), and that certificate holds one of its write scopes; `key`, `collection`, the policy, `writer`, `deleted` and `size` are the operation's, and `generation` is the operation's, at most g (`409 conflict`). A write made at O after the checkpoint is the full chain item at its `seq` (a part that carries `op` as well carries those very bytes), in that item's generation, its `version` is that `seq`, and no later `record.put` of its key is in the chain parts; one at or behind the checkpoint, which no chain part covers, has a version at or before the checkpoint and no `record.put` of its key in the chain parts; one O took from a recovery upload (spec 13.6 step 3, when O was itself a recovering service) is of an earlier generation and listed by the `upload` transition at its `seq`. And every `record.put` the chain parts hold with its document is the current write of its key, since a superseded write keeps only its stub (`409 conflict`). Every part but one taken from a recovery upload carries `proof`, and an upload's carries none (`409 conflict` otherwise). The proof names that write: its stub's `item_hash` is the operation's at the part's `seq` in the record's generation, its `object` is `key` and its state shows the part's `generation` and `version`; and it verifies with that generation's service key: O's for g, else the one the imported `identity.json` lists for it in `service_keys` (3.3) (`409 conflict` for either). A proof of a generation that list doesn't name is refused too (`409 conflict`): an honest signer lists every generation that O reports a current record's proof names (3.3). The one exception is a generation the imported `identity.json` declares lost (`recovery_from` and earlier): its proofs date nothing, so N keeps the record without one. Vectors: `export.json` `record_rejections` (no proof, a signature that doesn't verify, the proof of another write). So O can't relabel a record's version, generation, policy or size, or pass an older write off as current while the chain holds the newer one in full;
   - every live claim is replayed from its entries: their steps strictly increase; each entry's proof verifies with O's key (an entry of an earlier generation carries the proof of the service that applied it, and is bound by its chain item instead), names the claim, and is that entry's (its stub's item hash is the entry's operation, at the entry's `seq`, and its `step` the entry's plus one); after the checkpoint each entry is the chain item at its `seq`, with the chain hash the chain parts give it. The claim's `step` is after its last entry and, unless the claim was rebuilt from recovery uploads (`rebuilt`, which add state without entries), exactly one after it, and the claim's state is the state that entry's proof shows, leaving out what depends on when it is read (`orphaned`, `next_possibly_duplicate`) (`409 conflict`). So an open attempt can't be exported as `failed`. The claim's `creator` is the owner of its first attempt, and unless it was rebuilt its `last_entry_ms` is the service time of its last entry, the `server_time` that entry's proof stub carries (`409 conflict`), so O can't charge another app's share of the claim limit, nor bring forward the deletion of a claim pinned to "none". A rebuilt claim's `last_entry_ms` is taken as written, since uploads change it without an entry; so is a tombstone's, which keeps no entries, while each tombstone's `creator` must be the owner of its own first attempt (`409 conflict`). Vectors: `export.json` `claim_rejections` (another creator, another last entry time);
   - each inbox entry's `seq` is one O gave out (at most the meta part's `seq`), no chain item's, and appears once; every batch ID a kept stub names has its `batch` part, since O keeps a batch's ID while any of its stubs is kept (7.4) (`409 conflict`);
   - **the uploaded evidence** (spec 13.5 step 3): a claim an uploaded proof shows SETTLED is SETTLED in the import, live or as a tombstone, unless the proof's deadline plus the 30-day reconciliation window (7.8) is at or before `last_time`, or, for a proof without a deadline, its time plus `max_claim_age_ms` is; a claim an uploaded proof shows with an attempt (the proof of an app's own open attempt) is held, live or as a tombstone, with that attempt, at that step or a later one; a record an uploaded proof shows at version v is that very write, by the proof's item hash, or one whose signed `expected_version` is v or later, or absent when v is a delete (`409 conflict`, naming the app whose evidence fails). The checks compare what the operations signed, not version numbers O wrote;
   - peers' `identity.json` copies verify (`400`, or `401 bad_signature`); a `peer_floor` names a Pubky ID with no `peer` part and a 32-byte body hash (`400`).
4. Only then N swaps the state in, in one step: O's sequence, chain, records, claims, tombstones, entries, apps and lists. By the checks above, the `identity.json` N holds never goes back on the one it was enrolled with. The App IDs its enrollment named in `revoked_apps` (3.8) stay tombstones, whatever the export carries for them. It signs the manifest's checkpoint again with its own service key, at the same `seq` and chain hash, since apps verify feed pages with the key of the `identity.json` naming N, and keeps them as those of its newest dropped stub (10.1). That is N's latest checkpoint, which may lie behind one an app accepted from O; such an app keeps no chain hash there, so it doesn't compare it (8.4.4). Its service time continues from at least `last_time`. It stays staged until the `identity.json` naming it arrives, which continues O's `seq` and chain under generation g+1 (7.5).

**Residual risk** (spec 13.5): N checks the import against the chain and the evidence apps uploaded. A faulty or malicious O that omits state, such as a tombstone or a ledger record, is detected only by apps holding evidence of it: their uploads, before the activation and after it (8.4.14, where a SETTLED proof rebuilds an omitted tombstone), the SETTLED proofs they compare before attempting (spec 10.4), and an executor's last ledger version (spec 11.3). Admission state an app can't vouch for, such as a contact no own app was seen adding, is shown to the user, not removed (spec 13.5 step 4).

Answer:

| Field | Type | Notes |
|---|---|---|
| `activated` | `b64(32)` | the manifest hash |
| `imported` | int | the last `seq` imported |
| `head` | signed head | N's head over the imported chain, under N's service key (generation g) |

After the activation an app's upload is checked against the imported chain and state at once, and a mismatch is an import conflict; once the move is published, N still merges the old service's SETTLED proofs an app uploads (8.4.14). When the `identity.json` naming N arrives, N drops the staged chunks, the uploaded heads and the uploaded evidence; it keeps the old service's key, the generation and the head the import ended at, to check later evidence.

### 11.6 Abort
While N holds no `identity.json` naming it, the signer may cancel the move with the unfreeze of 3.7 (`POST /v1/{id}/unfreeze`), sent to N first (spec 13.5 Abort):
- **At N**, a staged account: the unfreeze must verify with the identity key (`401 bad_signature`); N must not be named yet (`409 conflict`, the move is published); an import here must belong to the freeze it names (`409 conflict`). N discards that import, partial or activated, and puts back the state it was enrolled with; it forgets the uploaded heads and evidence, and refuses any manifest bound to that freeze from then on (11.4 step 2). Answer: the discard answer of 3.7, `{ "discarded": "<freeze hash>", "sig": "<N's signature>" }`, signed with N's service key; the same unfreeze again gets the same answer. N writes no chain item.
- **At O**, with N's answer as `discarded`: it is the unfreeze of 8.4.13. O checks the answer with the service key the freeze names and only then resumes, so O and N never both accept writes; apps submit their queued writes there. From then on its public read of `identity.json` carries no freeze (8.4.15). The cancelled freeze is used up: sent again, O refuses it (3.7).
- **N unreachable:** the move can't be cancelled; the signer completes it to N, or, if N is gone, declares a provider loss (the prototype doesn't automate either).
- **Pruning behind the checkpoint**: apps keep claim evidence and chain bookkeeping only as 8.4.4 says; they upload what they kept.

### 11.7 Examples
The examples of this section are taken from `export.json`: alice's account frozen right after `chain.json`'s last item, exported as one chunk of 19 parts. Each is the vector's JSON pretty-printed, its members in the vector's order. A value longer than 64 characters is cut short to its first 40 and `...`, and the parts left out are named. `export.json` has no `entry` or `tombstone` part: its one inbox entry was read and deleted before the freeze, and its one claim is live. 11.3 gives their members.

The manifest body:

```json
{
  "v": 1,
  "kind": "paykit.export_manifest",
  "identity": "6frao8w9ss5cn544qd64hynikrbr97ae1s5941c7ma5e1epifhgy",
  "freeze_hash": "MaWc7XFH-6Dq4_YPp40ual3MTfGjaaKcBldIpCqy4_0",
  "generation": 1,
  "next_generation": 2,
  "last_time": "2026-04-11T00:00:01.000Z",
  "head": {
    "chain_version": 1,
    "generation": 1,
    "seq": 10,
    "chain_hash": "DwFi5VqCio0iM75IQ0gLhkBVIwSmZ1-f9yLwa5U3TDE",
    "sig": "trHjBY7K70enKVOMIvQia8nBVozxb09IUKwBi8uG..."
  },
  "checkpoint": null,
  "parts": 19,
  "bytes": 28923,
  "chunks": [
    {
      "id": "00000",
      "len": 28923,
      "hash": "nIwZbNnffMjyInW8Le3wlXVt4ZBa8g5Gmr7JIfRIzbg",
      "first": 0,
      "last": 18
    }
  ]
}
```

The chunk, with its first two parts, the `meta` part and the `chain` part at `seq` 1 (the other 17 left out):

```json
{
  "v": 1,
  "kind": "paykit.export_chunk",
  "index": 0,
  "first": 0,
  "parts": [
    {
      "part": "meta",
      "identity": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmlkZW50aXR5...",
        "sig": "vbF6al58cr2SbWUAU5b8w4H77bHNfv_HkZqOzDws..."
      },
      "generation": 1,
      "seq": 10,
      "directory_version": 7,
      "apps_version": 4,
      "app_added_at": {
        "wejf37ngr7qizej3eoijn3hq4c": 1
      }
    },
    {
      "part": "chain",
      "seq": 1,
      "stub": {
        "envelope": "eyJjaGFpbl92ZXJzaW9uIjoxLCJnZW5lcmF0aW9u...",
        "chain_hash": "uDgFSnfYLnUHymmlSxFPWBZ9bwA_-D7do4Kbney1cWI"
      },
      "item_type": "identity",
      "doc": {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmlkZW50aXR5...",
        "sig": "SsN53u95-MeOq1XLQdhntmCyIt5ue0VOstT2zCrm..."
      },
      "context": null
    }
  ]
}
```

The `claim` part, the live claim of `seq` 7, and its one `claim_entry` part, the `attempt` of `seq` 7 with its proof, which follows it:

```json
{
  "part": "claim",
  "claim_key": "gf5fn_W2IzE4B7i6A24-u5giYwJnARfTQxzPyx-ZQa0",
  "claim": {
    "attempts": [
      {
        "attempt_id": "DgCVYQKL1eyDacZvG6YhZg",
        "deadline_ms": 1767312000001,
        "duplicate": false,
        "from_upload": false,
        "opened_at_ms": 1767225603000,
        "orphan_after_s": 86400,
        "outcome": "open",
        "owner": "wejf37ngr7qizej3eoijn3hq4c",
        "possibly_duplicate": false
      }
    ],
    "creator": "wejf37ngr7qizej3eoijn3hq4c",
    "current": "DgCVYQKL1eyDacZvG6YhZg",
    "divergent": false,
    "last_entry_ms": 1767225603000,
    "pinned_deadline": 1767312000001,
    "rebuilt": false,
    "settled": false,
    "step": 1
  }
}
```

```json
{
  "part": "claim_entry",
  "claim_key": "gf5fn_W2IzE4B7i6A24-u5giYwJnARfTQxzPyx-ZQa0",
  "step": 0,
  "seq": 7,
  "op": {
    "body": "eyJpZGVudGl0eSI6IjZmcmFvOHc5c3M1Y241NDRx...",
    "sig": "WDXfXbjTgd4v8Dl1ayX26u3EXNDkc6d-TjSZ6eaN..."
  },
  "proof": {
    "body": "eyJjaGFpbiI6eyJlbnZlbG9wZSI6ImV5SmphR0Zw...",
    "sig": "9SdlSo7HN1yrd-XQ09LgtFaX7k40vzSMGduCzSU1..."
  }
}
```

The `record` part, with the proof of its current write (`chain.json` `seq` 8); `op` and `cert` are `null`, since the `chain` part at `seq` 8 and the wallet's `app` part carry them:

```json
{
  "part": "record",
  "record": {
    "cert": null,
    "collection": "vAdq7de2YcjuIim-ag5zW6vixdi23ORiOV8-J2P_BsE",
    "deleted": false,
    "from_upload": false,
    "generation": 1,
    "key": "ztlpSxU9RIODQQsXxNXqFRqz2PEo2zpGe2TYpadhrcg",
    "op": null,
    "proof": {
      "body": "eyJjaGFpbiI6eyJlbnZlbG9wZSI6ImV5SmphR0Zw...",
      "sig": "YcI39K84Lhe_sPZ7LCtE8FE8Z7Ai3Rt1Jho3hoKR..."
    },
    "read_scope": "pay",
    "seq": 8,
    "size": 4112,
    "upload_base": null,
    "version": 8,
    "write_scopes": [
      "pay"
    ],
    "writer": "wejf37ngr7qizej3eoijn3hq4c"
  }
}
```

The wallet's `app` part:

```json
{
  "part": "app",
  "app_id": "wejf37ngr7qizej3eoijn3hq4c",
  "record": {
    "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LmFwcCIsImlk...",
    "sig": "b9RT0ONNIGKEr5zWCLeM72M7Ugpcd8cuGq3X7zW0..."
  },
  "record_seq": 3,
  "record_at_ms": 1767225601000,
  "receive_keys": [
    [
      {
        "body": "eyJ2IjoxLCJraW5kIjoicGF5a2l0LnJlY2VpdmVf...",
        "sig": "PAQP0CetoqOZhbkiBFIke9E6UCGt3YWW4_Uqp36l..."
      },
      4
    ]
  ],
  "cursor": 9,
  "last_seen_ms": 1767225604000,
  "revoked_at_ms": null,
  "departed_at_ms": null,
  "departure_epoch": null,
  "rotated": false,
  "renewed_cert": null,
  "lagging": false,
  "keys_through": 1,
  "full_bundle_requested_ms": null
}
```

The first `batch` part:

```json
{
  "part": "batch",
  "id": "RlS8dXaU6JBOESpdL12e5A",
  "record": {
    "at_ms": 1767225602500,
    "ops_hash": "bDUvg0MPxhdjDCBCRvo6KArkVCrMXE-bdiTvhRDdJ5s",
    "proofs": []
  }
}
```

## Appendix A. Choices this document made
The prototype fixes every other detail above. These it leaves open, so this document chose:

| # | Detail | Choice | Prototype |
|---|---|---|---|
| A1 | Range of JSON integers | 0 to 2^53 - 1, larger values rejected (1.1) | `u64`, unchecked |
| A2 | Time spellings | writers: three fraction digits, `Paykit-Timestamp` none; readers: every form of the grammar, fractions of 1 to 9 digits; accepting offsets or a space for `T` is the reader's choice, and no writer produces them (1.6) | writes these forms; its parser also accepts offsets and a space |
| A3 | Time comparisons | as instants at millisecond precision (1.6); period details ordered by `created_at` instant, then `msg_id` text (5.9) | compares milliseconds; orders period details by the `created_at` text, the same order for the fixed form |
| A4 | Amount bounds | at most 18 fraction digits and value times 10^18 below 2^128, integer part of at most 20 digits advised; rates above zero (1.7) | the same bounds through its `u128` decimal; checks that a rate is above zero only when it prices a payment |
| A5 | `inbox.url` | the origin `https://host[:port]`, no path or trailing slash (3.3) | compares the text with its configured origin |
| A6 | `accepted_payment_endpoint_identifiers` | optional; narrows the identifiers, and holds every key of `payment_endpoints` when both are present (5.5) | ignores it |
| A7 | `payment_reference` in a proof | optional; when present it equals the request's (5.10) | doesn't write it |
| A8 | Period details | at least one of `payment_endpoints` and `rates`; v0.1's `valid_from` dropped (5.9) | doesn't check |
| A9 | `resend_request` selectors | `request_hashes` and `billing_period` added for the spec's "by request or Billing Period"; at least one selector; both range ends inclusive, an absent end open (5.12) | implements and tests every selector |
| A10 | `reason` on rejection and cancellation | kept from v0.1, optional (5.7, 5.8) | sends `{}` |
| A11 | `msg_id` and `allowance_id` | lower-case UUID v4 with version and variant bits (1.8); the spec now makes it a MUST | checks the form (`wire::check_uuid_v4`) |
| A12 | Allowance terms and unknown members | ignored, as everywhere in v0.2; a later restriction needs a new `v` or kind (5.15) | ignores them |
| A13 | Names of the further limits in the enrollment answer | the configuration names of 10.2, as `limits` members, when a service announces them (3.8) | doesn't announce them |
| A14 | Chunk IDs | opaque path-safe strings that N takes from the manifest (11.2) | writes the chunk's index as five decimal digits |
| A15 | Paykit root input | the identity's 32-byte Ed25519 secret key (seed), not its expanded form (1.5) | the same |
| A16 | `PUT /identity` body | `{ "identity", "prev" }`, `prev` the document hash or `null`; a retry of the same bytes passes whatever `prev` it names and gets the original delivery's proof; the `409 stale_identity` carries the held copy and its hash (8.4.2) | the same |
| A17 | Key bundle plaintext | flat `keys` list with `naming_key`, `first_epoch` and `last_epoch`; the old nesting (`keys.keys`) dropped (3.5) | the same |
| A18 | Asking for a full bundle | `POST /v1/{id}/apps/{app_id}/keys`, empty body; `full_bundle_requested` and `keys_through` in `GET /apps` (8.4.11, 8.4.12) | the same |
| A19 | Discard answer | `{ "discarded", "sig" }` under N's service key, label `paykit/v1/discarded`, carried as `discarded` in the unfreeze at O (3.7) | the same |
| A20 | Push registration | two `PUT /push` calls: without `push_key` to start one, with it to set handles; `409 conflict` for another key or relay (9.1) | the same |
| A21 | `missed` feed item | `{ entries, accepted_from, accepted_until }`, one per lagging app, replaced while unread; it waits for its target even while lagging (8.4.4, 8.5) | the same |
| A22 | Lagging when every app is idle | no app is marked; entries wait and the quota refuses senders (8.4.4) | the same |
| A23 | Checkpoint | the latest one in each feed page; N signs the export's checkpoint again with its own key after an import (8.4.4, 11.5); signed once `checkpoint_behind` items may go, behind every cursor or 30 days old | opt-in, off by default; with `checkpoint_at_head`, signed at the newest item, keeping behind it the stubs a cursor holds |
| A24 | Feed of a frozen account | carries the signed `freeze` (8.4.4) | the same |
| A25 | Record expiry | at most 30 days after `updated_at`; required with public endpoints, `null` without (3.2); Private Payment Lists 30 days (5.13); renewed at the next run once less than half the lifetime remains | 30 days; renews after each feed read once past half the lifetime (`renew_destinations`) |
| A26 | Unverifiable sender | store locally and move on after 24 hours (5.1) | `DEFAULT_PARK_UNVERIFIABLE_MS` |
| A27 | "Counterparties recently written to" | switches whose inbox accepted a message from the app in the last 30 days, skipped when their `known_version` is already current (3.3) | the same |
| A28 | Clock tolerance comparisons | removed: counts when `accepted_at + 5 min <= distrust_from`; renewed and closed: when `accepted_at + 5 min < until` (5.1) | the same |
| A29 | Tombstone deletion "passes" | at the deadline plus the 30-day reconciliation window (`now >= deadline + 2592000000 ms`, `now` the time base with trusted time, 8.3 step 4), for tombstones and failed claims alike; an `attempt` is refused from the deadline itself (7.8) | the same |
| A30 | `evidence_ref` | one field for either piece of evidence: required with basis `evidence`, optional with `owner` or `reconcile`, refused on other types (7.2) | the same, `400` otherwise |
| A31 | Cursor deferral bound | at most 2 s while running, sent before the app stops reading (8.4.4) | `cursor_defer_ms`, `suspend()` |
| A32 | Order of per-operation checks | permission, shape, per-kind limit, then the type's rules; for a claim the deadline before `expected_step` (7.4, 7.8) | the same |
| A33 | Trusted time | an authenticated sample anchored to the monotonic clock, with a drift bound and a maximum sample age; the wall clock while it agrees within 5 minutes; `503 unavailable` for what depends on time without it (spec 10.1, 8.3) | opt-in (`trusted_time.required`), as the prototype has no time source; tests give samples (`note_time_sample`), and the test clock's wall reading can jump without moving its monotonic one |
| A34 | How a record view gives readers the writer's certificate | one optional `cert` member, the certificate of the record the service holds for the writer, also after a revocation; the reader checks the write's standing with it at the write's time, and a renewal's earlier certificate is replaced by the record's new one (8.4.9, 7.2). A per-write certificate or a cert hash in the chain envelope was not chosen: one more member in the view is the smallest change, and feed readers already hold the records of the identity's apps | the same |
| A35 | How many `msg_id`s a record keeps | those of the messages it stores and the 16 newest others (6.3, 6.4) | the same (`RECENT_MSG_IDS`) |
| A36 | Re-send limits | one message to one requester identity at most once an hour; a daily number of messages per requester identity the sender chooses (5.12) | 500 a day (`DEFAULT_RESEND_BUDGET`, `AppConfig.resend_budget`) |
| A37 | What a compacted `failed` entry keeps | its `obligation`, `attempt_id` and `value` in `totals.failed`, so the executor can read its claim again and add a later `done` to the totals once (6.5) | the same (`CompactedFailed`) |
| A38 | The O/N clock offset in the residual window | the difference between (last service time verified from O minus the device clock then) and (the first from N minus the device clock then); twice the clock tolerance without one from O (spec 13.6) | the same (`RecoveryState.clock_offset_ms`) |
| A39 | Record age limit | `record_max_age` in whole seconds, at least 1, 90 days when absent; an `updated_at` more than 5 minutes ahead is refused (3.2, 3.3) | `DEFAULT_RECORD_MAX_AGE_S`, `AppRecord::payable_until` |
| A40 | App request and approval | the fields of 3.10 and 3.11; the request is a signed document under its own label; the approval names the request by document hash and carries the bundle as `GET /keys` parts without `seq` | the same; the Pubky auth request is modelled (`records::AuthRequest`) and the approval is handed over in process |
| A41 | When the signer renews a certificate before it expires | at its first run once less than half the certificate's lifetime remains, with the default lifetime from then (3.1, spec 3.2) | `Signer::renew_expiring` |
| A42 | Certificate lifetime bounds | `expires_at` later than `issued_at` and at most 730 days after it; "1 year" is 365 days (3.1) | `records::CERT_LIFETIME_MS`, `MAX_CERT_LIFETIME_MS` |
| A43 | Sealed value buckets | 2^k bytes for k from 9 to 18 and 3 * 2^k for k from 8 to 16, of `u32_be(len) || JSON || zeros`; a delete unpadded; the service refuses any other length with `400`, more than 256 KiB with `413` (7.2) | `sealed::BUCKETS`, `pad`, `unpad`, `SealedValue::check_shape` |
| A44 | The reserved `suite_keys` member | 1 to 4 entries `{ suite, key }`, suites 2 to 255 strictly ascending, keys 1 to 4096 bytes; in a receive key record no entry's kid equals the record's; readers ignore unsupported suites (3.1, 3.4) | `records::SuiteKey`, `check_suite_keys` |
| A45 | The kid of a later suite's key | first 8 bytes of `derive("paykit/v1/kid", u64(suite), key)`; suite 1 keeps `derive("paykit/v1/kid", key)`, so no existing kid changes (1.5) | `wire::suite_kid` |
| A46 | Ed25519 verification and low-order X25519 keys | the four steps of 2.1; a key is of low order when X25519 with a clamped secret gives all zeros (4.2) | `wire::verify`, `wire::x25519_low_order` |
| A47 | The import limit | 4 GiB. Measured part sizes, each operation once and no batch proofs: a shared record about 1.2 KB with the transition proof of its current write (0.9 KB), plus its chain item with its operation about 2.5 KB (at the smallest sealed value), a tombstone with one attempt 0.4 KB, a live claim 2.5 KB, a chain stub alone about 0.5 KB, a batch part 0.15 KB. An account at every default limit (100,000 records within the 64 MiB record quota, 1,000,000 claims and tombstones, a full 64 MiB inbox, 100,000 entries in each list, 10,000 peer copies) exports under 1 GiB; the other 3 GiB hold about 5 million kept chain stubs with their batch parts. A service whose rates and stub retention keep more raises it, and a signer moves an identity only to a service whose limit is at least its current one's (8.4.13) | `retention::KindLimits::max_import_bytes`, `lifecycle::Limits` |
| A48 | The clock bound for automatic payments (spec 10.1) | 5 minutes, the clock tolerance, either way: the offset between the service's signed time and the device clock at the latest verified answer, or the highest verified service time ahead of the device clock now | `App::clock_skew_ms`, `check_clock`: an automatic payment is refused, a manual one goes on with the event `clock_skew_warning` |
| A49 | Extension scopes and kinds | `ext:<prefix>`, prefix 1 to 32 of `a-z`, `0-9`, `-`, not starting with `-`, not `paykit`; kind `<prefix>.<name>`, name 1 to 64 of `a-z`, `0-9`, `_`; at most 8 extension scopes per certificate; they give `conversation.add` | `records::extension_scope`, `check_scopes`; received extension messages are handed over in `ReceiveReport::extensions` |
| A50 | How the next service fetches the export | during `POST /import`, before it answers, one chunk at a time, stopping at the first failure; the status counts `pulled` and names `pull_error`; the signer moves what is missing | `InboxService::serve`, `set_peer_transport`; `Signer::transfer` (direct, with the fallback) and `transfer_relayed` |
| A51 | The sender's share of the inbox quota | `sender_share_percent` 25 | the same |
| A52 | The switch's copy of `identity.json` | the grace after the Homeserver reads: 500 ms, after which the lookup keeps running and a manual payment it hasn't answered for needs the user's confirmation before the attempt; an automatic payment's lookup: 5 s per try and one retry; `identity_reads_per_source_per_min` 60 per IPv4 address or IPv6 /64, `identity_reads_per_min` 6,000 per account; `Cache-Control: public, max-age=60`; a failed lookup counts until the payer's copy is 10 minutes old | `pay::INBOX_IDENTITY_GRACE_MS`, `INBOX_LOOKUP_TIMEOUT_MS`, `with_grace`, `lookup_with_retry`, `App::settle_lookup`; `execute::check_unconfirmed` (event `identity_unconfirmed`); `directory::source_key`, `IDENTITY_CACHE_CONTROL` |
| A55 | How long a frozen service answers the public read with its freeze | while frozen, and after a move at least `record_max_age` counted from the last export call it answered, plus a day; the payer follows a freeze once | the prototype deletes a frozen account only when the signer erases it (8.4.16, `directory::public_identity`); `pay::inbox_identity` |
| A56 | A full connect request list | refused with `429 quota_exceeded` `connect_requests`, checked before the deny list; `GET /connect` reports `max_pending` and `full` | `admission::post_connect`, `get_connect`, `ConnectList` |
| A53 | The cap on an invite code's uses | `max_invite_uses` 100 | the same |
| A54 | The settlement ID of a `bitcoin-txid` proof | its `txid`, 64 lower-case hex characters, as text, without an output index; a proof in another spelling isn't credited (1.5) | `wire::settlement_id`, which the rail uses for proofs with a `type` |

The vector files follow these choices wherever the prototype has a value to compute.
