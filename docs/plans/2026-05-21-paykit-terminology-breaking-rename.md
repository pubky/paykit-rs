# Paykit Terminology Breaking Rename Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Rename Paykit's developer-, user-, and protocol-visible code surfaces to match `docs/THESAURUS.md`, `CONTEXT.md`, and `docs/adr/0001-defer-public-api-terminology-renames.md` in one deliberate breaking-change pass.

**Architecture:** This is an intentional hard break across Rust public API, transport traits, FFI DTOs/functions, generated bindings, runtime strings, serialized JSON fields, and protocol message kinds. Preserve current validation semantics unless the ADR explicitly changes them. Keep public Payment List map storage in Rust core, but use vector DTOs in FFI/platform bindings.

**Tech Stack:** Rust 2021 workspace, `paykit-lib`, `paykit-ffi`, UniFFI bindings, Pubky transport adapters, `pubky-noise`, Cargo test/doc/clippy/fmt, FFI build script.

---

## Ground Rules

- Treat `docs/THESAURUS.md` as canonical language.
- Treat `CONTEXT.md` as domain glossary, not an implementation spec.
- Treat `docs/adr/0001-defer-public-api-terminology-renames.md` as the implementation decision record for this breaking rename.
- Do not preserve legacy aliases unless a task explicitly says to.
- Do not add backward read compatibility for renamed wire/protocol message kinds or serialized JSON field names.
- Keep current validation behavior unless explicitly changed below.
- After each code task, run the smallest relevant focused test first, then the broader command listed in the verification task.

## Canonical Rename Map

| Legacy | Canonical |
|---|---|
| `MethodId` | `PaymentEndpointIdentifier` |
| `METHOD_ID_MAX_LEN` | `PAYMENT_ENDPOINT_IDENTIFIER_MAX_LEN` |
| `METHOD_ID_RESERVED_PRIVATE` | `PAYMENT_ENDPOINT_IDENTIFIER_RESERVED_PRIVATE` |
| `EndpointData` | `PaymentEndpointPayload` |
| `SupportedPayments` | `PaymentList` |
| `SupportedPayments.entries` | `PaymentList.endpoints` |
| `PrivatePaymentsPayload` | `PrivatePaymentEnvelope` |
| `PrivatePaymentsPayload.entries` | private `PrivatePaymentEnvelope.endpoints` |
| `fetch_supported_payments` | `fetch_payment_list` |
| `set_private_payments` | `set_private_payment_envelope` |
| `get_private_payments` | `get_private_payment_envelope` |
| `paykit.private_payments` | `paykit.private_payment_envelope` |
| `payment_method` receipt field | `payment_endpoint_identifier` |
| `method_id` FFI/platform field | `payment_endpoint_identifier` |
| `endpoint_data` FFI/platform field | `payment_endpoint_payload` |
| `FfiPaymentEntry` | `FfiPaymentEndpoint` |
| `FfiPrivatePaymentsPayload` | `FfiPrivatePaymentEnvelope` |

## Desired Core Shapes

```rust
pub struct PaymentEndpointIdentifier(String);

pub struct PaymentEndpointPayload(String);

pub struct PaymentEndpoint {
    pub identifier: PaymentEndpointIdentifier,
    pub payload: PaymentEndpointPayload,
}

pub struct PaymentList {
    pub endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

pub struct PrivatePaymentEnvelope {
    pub reference: PaymentReference,
    endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}
```

`PrivatePaymentEnvelope` must provide at least:

```rust
impl PrivatePaymentEnvelope {
    pub fn new(
        reference: PaymentReference,
        endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Result<Self>;

    pub fn endpoints(&self) -> &HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>;

    pub fn into_endpoints(self) -> HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>;

    pub fn get(&self, identifier: &PaymentEndpointIdentifier) -> Option<&PaymentEndpointPayload>;
}
```

`PaymentEndpointIdentifier::new()` must keep current `MethodId::new()` semantics:
- preserve input exactly;
- allow ASCII alphanumeric, `-`, `_`, and `.`;
- reject empty;
- reject max length over current limit;
- reject `private`;
- reject `.`, `..`, slashes, backslashes, null bytes, spaces, unicode, and other currently forbidden characters;
- do not enforce `asset-rail-endpoint-format`.

`PaymentEndpointPayload::new()` must keep current `EndpointData::new()` semantics:
- generic UTF-8 wrapper;
- no identifier-aware validation;
- no generic max size.

---

## Task 1: Snapshot Current API Surface

**Objective:** Capture current legacy symbols and establish a baseline before renaming.

**Files:**
- Read/search only: repository root

**Step 1: Search legacy Rust symbols**

Run:
```bash
rg -n "\b(MethodId|EndpointData|SupportedPayments|PrivatePaymentsPayload)\b|fetch_supported_payments|get_private_payments|set_private_payments|payment_method|method_id|endpoint_data|paykit\.private_payments" . --glob '!target/**' --glob '!docs/adr/0001-defer-public-api-terminology-renames.md' --glob '!docs/THESAURUS.md' --glob '!CONTEXT.md'
```

Expected: existing matches in `paykit-lib`, `paykit-ffi`, README files, and tests.

**Step 2: Record any unexpected generated/binding files**

If generated Swift/Kotlin files are committed, list them. If not, note that binding verification happens through `paykit-ffi && ./build.sh all`.

**Step 3: Commit not required**

This is reconnaissance only.

---

## Task 2: Rename Core Identifier Type

**Objective:** Rename `MethodId` to `PaymentEndpointIdentifier` while preserving validation semantics exactly.

**Files:**
- Modify: `paykit-lib/src/lib.rs`
- Modify: `paykit-lib/src/transport/traits.rs`
- Modify: `paykit-lib/src/transport/pubky/authenticated_transport.rs`
- Modify: `paykit-lib/src/transport/pubky/unauthenticated_transport.rs`
- Modify: `paykit-lib/src/transport/pubky/mod.rs`

**Step 1: Rename the type and constants in `paykit-lib/src/lib.rs`**

Replace:
```rust
pub struct MethodId(String);
```
with:
```rust
pub struct PaymentEndpointIdentifier(String);
```

Rename constants:
```rust
METHOD_ID_MAX_LEN -> PAYMENT_ENDPOINT_IDENTIFIER_MAX_LEN
METHOD_ID_RESERVED_PRIVATE -> PAYMENT_ENDPOINT_IDENTIFIER_RESERVED_PRIVATE
```

Update `impl`, `Display`, `AsRef<str>`, examples, rustdoc, and test names.

**Step 2: Preserve validation error semantics except type name**

Runtime strings should use canonical language after this breaking pass. Example:
```rust
return Err(PaykitError::Validation(
    "PaymentEndpointIdentifier must not be empty".into(),
));
```

**Step 3: Update all transport trait signatures**

Example:
```rust
async fn fetch_payment_endpoint(
    &self,
    payee: &PublicKey,
    identifier: &PaymentEndpointIdentifier,
) -> Result<Option<PaymentEndpointPayload>>;
```

**Step 4: Run focused tests**

Run:
```bash
cargo test -p paykit-lib payment_endpoint_identifier -- --nocapture
```

Expected: tests compile and pass after test names are updated.

**Step 5: Commit**

```bash
git add paykit-lib/src

git commit -m "refactor: rename MethodId to PaymentEndpointIdentifier"
```

---

## Task 3: Rename Core Payload Type

**Objective:** Rename `EndpointData` to `PaymentEndpointPayload` while preserving generic wrapper behavior.

**Files:**
- Modify: `paykit-lib/src/lib.rs`
- Modify: `paykit-lib/src/transport/traits.rs`
- Modify: `paykit-lib/src/transport/pubky/authenticated_transport.rs`
- Modify: `paykit-lib/src/transport/pubky/unauthenticated_transport.rs`

**Step 1: Rename the type in `paykit-lib/src/lib.rs`**

Replace:
```rust
pub struct EndpointData(String);
```
with:
```rust
pub struct PaymentEndpointPayload(String);
```

Update `impl`, `Display`, `AsRef<str>`, `new`, `as_str`, `into_inner`, docs, examples, and tests.

**Step 2: Do not add identifier-aware validation**

Keep constructor shape:
```rust
impl PaymentEndpointPayload {
    pub fn new(data: impl Into<String>) -> Self { ... }
}
```

**Step 3: Run focused tests**

Run:
```bash
cargo test -p paykit-lib payment_endpoint_payload -- --nocapture
```

Expected: payload accessor tests pass.

**Step 4: Commit**

```bash
git add paykit-lib/src

git commit -m "refactor: rename EndpointData to PaymentEndpointPayload"
```

---

## Task 4: Introduce `PaymentEndpoint` and Rename `SupportedPayments` to `PaymentList`

**Objective:** Make the public Payment List model match the thesaurus.

**Files:**
- Modify: `paykit-lib/src/lib.rs`
- Modify: call sites in `paykit-lib/src/transport/**`

**Step 1: Add `PaymentEndpoint`**

Add near the payload/list types:
```rust
/// A whole payee-owned entry in a Payment List.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentEndpoint {
    /// Machine-readable type identifier for this Payment Endpoint.
    pub identifier: PaymentEndpointIdentifier,
    /// Payee-owned receiving payload for this Payment Endpoint.
    pub payload: PaymentEndpointPayload,
}
```

**Step 2: Rename `SupportedPayments`**

Replace:
```rust
pub struct SupportedPayments {
    pub entries: HashMap<MethodId, EndpointData>,
}
```
with:
```rust
pub struct PaymentList {
    pub endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}
```

Do not create `SupportedPaymentList` yet.

**Step 3: Update constructors/accessors/tests**

Update any `.entries` access to `.endpoints`.

**Step 4: Run focused tests**

Run:
```bash
cargo test -p paykit-lib payment_list -- --nocapture
```

If no focused tests exist yet, add one that constructs an empty `PaymentList` and asserts it is valid.

**Step 5: Commit**

```bash
git add paykit-lib/src

git commit -m "refactor: rename SupportedPayments to PaymentList"
```

---

## Task 5: Rename Transport Trait Methods

**Objective:** Rename public transport trait methods to canonical names while preserving `fetch_*` for transport-level network retrieval.

**Files:**
- Modify: `paykit-lib/src/transport/traits.rs`
- Modify: `paykit-lib/src/transport/pubky/unauthenticated_transport.rs`
- Modify: all mocks/tests implementing `UnauthenticatedTransportRead`

**Step 1: Rename trait method**

Replace:
```rust
async fn fetch_supported_payments(...) -> Result<SupportedPayments>;
```
with:
```rust
async fn fetch_payment_list(...) -> Result<PaymentList>;
```

**Step 2: Keep high-level helper named `get_payment_list`**

Update implementation:
```rust
reader.fetch_payment_list(payee).await
```

**Step 3: Update debug logs/runtime labels**

Replace legacy runtime strings such as `list supported payments` and `supported payments collected` with Payment List / Payment Endpoint language.

**Step 4: Run tests**

Run:
```bash
cargo test -p paykit-lib get_payment_list -- --nocapture
```

**Step 5: Commit**

```bash
git add paykit-lib/src

git commit -m "refactor: rename transport payment-list fetch"
```

---

## Task 6: Rename Private Envelope Core Type and Wire Kind

**Objective:** Rename `PrivatePaymentsPayload` to `PrivatePaymentEnvelope` and hard-break the message kind.

**Files:**
- Modify: `paykit-lib/src/lib.rs`

**Step 1: Rename message kind**

Replace:
```rust
"paykit.private_payments"
```
with:
```rust
"paykit.private_payment_envelope"
```

Do not accept the old kind.
Do not bump envelope version solely because of this rename.

**Step 2: Rename type and fields**

Replace:
```rust
pub struct PrivatePaymentsPayload {
    pub reference: PaymentReference,
    pub entries: HashMap<MethodId, EndpointData>,
}
```
with:
```rust
pub struct PrivatePaymentEnvelope {
    pub reference: PaymentReference,
    endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}
```

**Step 3: Make constructor validate non-empty endpoints**

Implement:
```rust
pub fn new(
    reference: PaymentReference,
    endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
) -> Result<Self> {
    if endpoints.is_empty() {
        return Err(PaykitError::Validation(
            "PrivatePaymentEnvelope endpoints must not be empty".into(),
        ));
    }
    Ok(Self { reference, endpoints })
}
```

Adjust call sites for `Result<Self>`.

**Step 4: Add accessors**

```rust
pub fn endpoints(&self) -> &HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>;
pub fn into_endpoints(self) -> HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>;
pub fn get(&self, identifier: &PaymentEndpointIdentifier) -> Option<&PaymentEndpointPayload>;
```

**Step 5: Rename JSON helpers**

Rename functions:
```rust
parse_private_payments_json -> parse_private_payment_envelope_json
serialize_private_payments_json -> serialize_private_payment_envelope_json
```

Serialized field should become `endpoints`, not `entries`.

**Step 6: Run focused tests**

Run:
```bash
cargo test -p paykit-lib private_payment_envelope -- --nocapture
```

Add tests for:
- new kind is `paykit.private_payment_envelope`;
- old kind is unsupported;
- empty endpoints are rejected;
- latest queued envelope wins globally per Known Peer, not per Payment Reference.

**Step 7: Commit**

```bash
git add paykit-lib/src

git commit -m "refactor: rename private payments to PrivatePaymentEnvelope"
```

---

## Task 7: Rename Private Envelope Public Functions

**Objective:** Replace plural private-payments APIs with singular Private Payment Envelope APIs.

**Files:**
- Modify: `paykit-lib/src/lib.rs`
- Modify: docs/examples in rustdoc

**Step 1: Rename send function**

Replace:
```rust
set_private_payments(link, payload)
```
with:
```rust
set_private_payment_envelope(link, envelope)
```

**Step 2: Rename receive function**

Replace:
```rust
get_private_payments(link) -> Result<Option<PrivatePaymentsPayload>>
```
with:
```rust
get_private_payment_envelope(link) -> Result<Option<PrivatePaymentEnvelope>>
```

It returns only the latest currently available Private Payment Envelope.

**Step 3: Update docs and runtime labels**

Use singular envelope language everywhere.

**Step 4: Run tests**

Run:
```bash
cargo test -p paykit-lib get_private_payment_envelope set_private_payment_envelope -- --nocapture
```

**Step 5: Commit**

```bash
git add paykit-lib/src

git commit -m "refactor: rename private envelope APIs"
```

---

## Task 8: Rename Receipt Field and Serialized JSON

**Objective:** Replace receipt `payment_method` with `payment_endpoint_identifier` across structs and wire JSON.

**Files:**
- Modify: `paykit-lib/src/lib.rs`
- Modify: `paykit-ffi/src/lib.rs`
- Modify: receipt tests in `paykit-lib/src/lib.rs`

**Step 1: Rename Rust struct fields**

Replace fields such as:
```rust
pub payment_method: Option<MethodId>
```
with:
```rust
pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>
```

Apply to `ReceiptDraft`, `IssuedReceipt`, `Receipt`, and any wire structs.

**Step 2: Rename serialized JSON field**

Replace JSON key:
```json
"payment_method"
```
with:
```json
"payment_endpoint_identifier"
```

Do not support old key.

**Step 3: Update tests**

Add/modify tests asserting serialized JSON contains `payment_endpoint_identifier` and does not contain `payment_method`.

**Step 4: Run focused tests**

Run:
```bash
cargo test -p paykit-lib receipt -- --nocapture
```

**Step 5: Commit**

```bash
git add paykit-lib/src paykit-ffi/src

git commit -m "refactor: rename receipt endpoint identifier field"
```

---

## Task 9: Rename FFI DTO Fields and Types

**Objective:** Align FFI Rust boundary names while keeping vector DTO representations.

**Files:**
- Modify: `paykit-ffi/src/lib.rs`

**Step 1: Rename `FfiPaymentEntry`**

Replace:
```rust
pub struct FfiPaymentEntry {
    pub method_id: String,
    pub endpoint_data: String,
}
```
with:
```rust
pub struct FfiPaymentEndpoint {
    pub payment_endpoint_identifier: String,
    pub payment_endpoint_payload: String,
}
```

**Step 2: Rename private envelope DTO**

Replace `FfiPrivatePaymentsPayload` with:
```rust
pub struct FfiPrivatePaymentEnvelope {
    pub reference: String,
    pub endpoints: Vec<FfiPaymentEndpoint>,
}
```

**Step 3: Keep vector representation**

Do not expose HashMap in FFI DTOs.

**Step 4: Update conversion helpers**

Rename helper functions and fields:
```rust
entries_to_map -> endpoints_to_map
map_to_entries -> payment_list_to_ffi_endpoints
private_payload_to_ffi -> private_payment_envelope_to_ffi
```

Use the exact canonical names where practical.

**Step 5: Run FFI Rust tests**

Run:
```bash
cargo test -p paykit-ffi --lib -- --nocapture
```

**Step 6: Commit**

```bash
git add paykit-ffi/src/lib.rs

git commit -m "refactor: rename FFI payment endpoint DTOs"
```

---

## Task 10: Rename FFI Functions While Preserving High-Level `get_*`

**Objective:** Align FFI functions with Rust high-level helpers and private envelope naming.

**Files:**
- Modify: `paykit-ffi/src/lib.rs`
- Modify: generated binding config if present

**Step 1: Keep public payment functions' high-level names**

Keep names like:
```rust
paykit_get_payment_list
paykit_get_payment_endpoint
paykit_set_payment_endpoint
paykit_remove_payment_endpoint
```

Update parameters:
```rust
method_id -> payment_endpoint_identifier
endpoint_data -> payment_endpoint_payload
```

**Step 2: Rename private functions**

Replace:
```rust
paykit_set_private_payments
paykit_get_private_payments
```
with:
```rust
paykit_set_private_payment_envelope
paykit_get_private_payment_envelope
```

**Step 3: Generated names policy**

If UniFFI config supports it cleanly:
- Strip `Ffi` from platform type names.
- Expose module-scoped names like `getPaymentList`, not `paykitGetPaymentList`.

If UniFFI cannot do this cleanly without extra complexity, document that limitation and do not hack around it in this pass.

**Step 4: Run binding generation/build**

Run:
```bash
cd paykit-ffi && ./build.sh all
```

Expected: all platform bindings build.

**Step 5: Commit**

```bash
git add paykit-ffi

git commit -m "refactor: rename FFI private envelope APIs"
```

---

## Task 11: Update README, Specs, Changelog, and Examples

**Objective:** Remove legacy language from user-facing docs outside intentional historical sections.

**Files:**
- Modify: `README.md`
- Modify: `paykit-lib/README.md`
- Modify: `paykit-ffi/README.md`
- Modify: `paykit-react-native/README.md`
- Modify: `specs/payment-endpoint-identifier.md`
- Modify: `CHANGELOG.md`
- Modify: `AGENTS.md`

**Step 1: Replace code examples**

Use canonical examples:
```rust
use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};

let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
let payload = PaymentEndpointPayload::new("lnbc1...");
```

**Step 2: Replace FFI examples**

Use:
```js
{
  payment_endpoint_identifier: 'btc-lightning-bolt11',
  payment_endpoint_payload: '{"value":"lnbc1..."}'
}
```

**Step 3: Keep historical changelog facts readable**

For old changelog entries, either:
- leave the historical old name with explicit historical framing; or
- add a new top entry explaining the breaking rename.

Do not rewrite old release history in a misleading way.

**Step 4: Search docs for forbidden terms**

Run:
```bash
rg -n "Supported Payments List|Payment Method List|Payment Option|Routing Network|Paykit SDK|Paykit PDK|\bMethodId\b|\bEndpointData\b|\bSupportedPayments\b|\bPrivatePaymentsPayload\b|method_id|endpoint_data|payment_method|private_payments|paykit\.private_payments" . --glob '*.md' --glob '!docs/THESAURUS.md' --glob '!CONTEXT.md' --glob '!docs/adr/0001-defer-public-api-terminology-renames.md'
```

Expected: only explicitly historical changelog entries, if any.

**Step 5: Commit**

```bash
git add README.md paykit-lib/README.md paykit-ffi/README.md paykit-react-native/README.md specs/payment-endpoint-identifier.md CHANGELOG.md AGENTS.md

git commit -m "docs: align Paykit terminology after breaking rename"
```

---

## Task 12: Update Tests Broadly

**Objective:** Rename all test names, fixtures, serialized JSON expectations, and assertions to canonical language.

**Files:**
- Modify: `paykit-lib/src/lib.rs` tests
- Modify: `paykit-ffi/src/lib.rs` tests if present
- Modify: any test fixtures in repository

**Step 1: Rename test functions**

Examples:
```rust
test_method_id_valid_simple_names -> test_payment_endpoint_identifier_valid_simple_names
test_endpoint_data_accessors -> test_payment_endpoint_payload_accessors
```

**Step 2: Update JSON fixtures**

Use:
```json
"kind": "paykit.private_payment_envelope"
```
and:
```json
"payment_endpoint_identifier": "btc-lightning-bolt11"
```

**Step 3: Ensure old wire names are rejected**

Add tests that old wire names do not parse as supported application messages.

**Step 4: Run tests**

Run:
```bash
cargo test -p paykit-lib -- --nocapture
cargo test -p paykit-ffi -- --nocapture
```

**Step 5: Commit**

```bash
git add paykit-lib/src paykit-ffi/src

git commit -m "test: update terminology rename coverage"
```

---

## Task 13: Run Workspace Verification

**Objective:** Prove the breaking rename compiles, tests, lints, documents, and builds bindings.

**Files:**
- No source changes expected unless fixing failures

**Step 1: Format**

Run:
```bash
cargo fmt
```

Expected: no output or files formatted.

**Step 2: Test**

Run:
```bash
cargo test
```

Expected: all tests pass.

**Step 3: Clippy**

Run:
```bash
cargo clippy --all-targets --all-features
```

Expected: no warnings/errors, or justified explicit allows.

**Step 4: Docs**

Run:
```bash
cargo doc --no-deps
```

Expected: docs build successfully.

**Step 5: Platform bindings**

Run:
```bash
cd paykit-ffi && ./build.sh all
```

Expected: iOS and Android bindings build; do not build only one target.

**Step 6: Commit fixes if needed**

```bash
git add .
git commit -m "fix: resolve terminology rename verification issues"
```

Only commit if verification required follow-up changes.

---

## Task 14: Final Audit for Legacy Terms

**Objective:** Ensure no legacy names remain outside intentional historical documentation.

**Files:**
- Whole repository except `target/`, `.git/`, ADR/history exceptions

**Step 1: Search all source/docs**

Run:
```bash
rg -n "\bMethodId\b|\bEndpointData\b|\bSupportedPayments\b|\bPrivatePaymentsPayload\b|fetch_supported_payments|get_private_payments|set_private_payments|method_id|endpoint_data|payment_method|private_payments|paykit\.private_payments|Supported Payments List|Payment Method List|Payment Option|Routing Network|Paykit SDK|Paykit PDK" . --glob '!target/**' --glob '!.git/**'
```

Expected: no matches except:
- intentional historical changelog notes;
- `docs/adr/0001-defer-public-api-terminology-renames.md` if retained as history;
- `docs/THESAURUS.md` legacy-term entries;
- `CONTEXT.md` flagged ambiguity entries if retained.

**Step 2: Verify generated binding names**

Inspect generated Swift/Kotlin outputs or package docs. Confirm platform consumers see names like:
- `PaymentEndpoint`
- `PrivatePaymentEnvelope`
- `getPaymentList`

not:
- `FfiPaymentEndpoint`
- `paykitGetPaymentList`

If UniFFI cannot strip prefixes cleanly, document the limitation in `paykit-ffi/README.md`.

**Step 3: Commit audit fixes**

```bash
git add .
git commit -m "chore: remove remaining legacy Paykit terminology"
```

Only commit if changes were needed.

---

## Task 15: PR Summary and Migration Notes

**Objective:** Prepare downstream users for the hard break.

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `README.md` if migration section is needed

**Step 1: Add breaking-change changelog entry**

Include:
- `MethodId` -> `PaymentEndpointIdentifier`
- `EndpointData` -> `PaymentEndpointPayload`
- `SupportedPayments` -> `PaymentList`
- `PrivatePaymentsPayload` -> `PrivatePaymentEnvelope`
- `paykit.private_payments` -> `paykit.private_payment_envelope`
- receipt JSON `payment_method` -> `payment_endpoint_identifier`
- no backward read compatibility for old wire names
- no envelope version bump solely for rename

**Step 2: Add migration examples**

Before/after snippets for Rust and FFI/platform bindings.

**Step 3: Final verification**

Run:
```bash
cargo fmt && cargo test && cargo clippy --all-targets --all-features && cargo doc --no-deps
cd paykit-ffi && ./build.sh all
```

**Step 4: Commit**

```bash
git add CHANGELOG.md README.md paykit-lib/README.md paykit-ffi/README.md paykit-react-native/README.md

git commit -m "docs: add migration notes for Paykit terminology rename"
```

---

## Acceptance Criteria

- `cargo fmt` passes.
- `cargo test` passes.
- `cargo clippy --all-targets --all-features` passes.
- `cargo doc --no-deps` passes.
- `cd paykit-ffi && ./build.sh all` succeeds.
- No source code uses `MethodId`, `EndpointData`, `SupportedPayments`, `PrivatePaymentsPayload`, `fetch_supported_payments`, `get_private_payments`, or `set_private_payments`.
- No active wire/runtime code emits or accepts `paykit.private_payments`.
- Receipt JSON uses `payment_endpoint_identifier`, not `payment_method`.
- Rust core uses `PaymentList.endpoints` as a public map.
- Rust core uses `PrivatePaymentEnvelope` with public `reference` and private non-empty `endpoints`.
- FFI uses vector DTOs for Payment Endpoints.
- Generated/platform APIs expose domain names if UniFFI permits clean mapping.
- Any remaining legacy terms are only in intentional historical docs or thesaurus/ADR legacy sections.
