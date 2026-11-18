# Paykit Capabilities, Swap Integration, and Route Solving (v0.2)

**Status.** Draft architecture and specification breakdown for
[pubky/paykit-rs#448](https://github.com/pubky/paykit-rs/issues/448) under
[pubky/paykit-rs#430](https://github.com/pubky/paykit-rs/issues/430). Not frozen; no library or SDK
code is implemented from this document yet. Implementation is gated on three upstream items:

1. **Paykit Inbox v0.2 core freeze** ([pubky/paykit-rs#426](https://github.com/pubky/paykit-rs/issues/426),
   [pubky/paykit-rs#446](https://github.com/pubky/paykit-rs/issues/446)): freezes per-app keys,
   signed Shared records, Envelopes, and the `paykit-lib` / `paykit-sdk` surface in `specs/v0.2/`.
2. **Capabilities and Switch `swap` protocol sign-off**
   ([pubky/paykit-rs#229](https://github.com/pubky/paykit-rs/issues/229),
   [pubky/paykit-rs#302](https://github.com/pubky/paykit-rs/issues/302),
   [coreyphillips/pubky-swap#142](https://github.com/coreyphillips/pubky-swap/pull/142)): settles
   App record static capability fields, the generic `paykit_ext:<prefix>` SDK boundary, and the
   sibling `swap` Protocol on the Switch.
3. **Route solver preference-mode specification**
   ([pubky/paykit-rs#449](https://github.com/pubky/paykit-rs/issues/449)): defines exact scoring
   weights, tie-breakers, and amount/fee thresholds for `cheapest`, `fastest`, and `most_private`.

---

## 1. Architectural Boundary and Repository Split

Paykit never holds or moves funds and never interprets a Payment Endpoint Payload (Paykit spec 1,
7.2; Paykit wire 1.4). Route solving, swap negotiation engines, bridge APIs, and chain or L2
settlement logic stay outside `paykit-lib` and `paykit-sdk`. They integrate with Paykit through
three clean seams:

1. **Paykit Protocol, Paykit Library (`paykit-lib`), and Paykit SDK (`paykit-sdk`) in
   `pubky/paykit-rs`:**
   - **Multi-protocol App certificates (`ProtocolSet`):** one wallet App ID holds Paykit scopes
     beside sibling Switch protocol scopes such as `swap` (Paykit spec 3.2, "Several protocols in
     one certificate" and "Own-app lifecycle"; Switch spec 3.2, 8.3).
   - **Static Paykit capability discovery:** `protocols.paykit.capabilities` on an App record
     advertises payable `endpoint_identifiers` and optional Paykit feature declarations such as
     `paykit-features/1` (Paykit spec 4; Paykit wire 3.2).
   - **Thin generic Extension kind transport:** `paykit_ext:<prefix>` and `<prefix>.<name>` carry
     authenticated, non-payment capability negotiation between admitted Counterparties, with
     outbound `paykit_contacts` and Deny list checks and a bounded 64-per-sender / 128-total unread
     pool (Paykit spec 3.1, 3.3; Paykit wire 4.1, 4.3).
   - **Paying order and Allowance coordination for routed fills:** Attempt lifetime (`orphan_after`),
     exact-delivery transfer checks, duplicate-payment protection, `routed-fill-v1` proof
     verification, and Executor coordination (`paykit_allowance_execute`) across direct and routed
     payments (Paykit spec 7.2, 7.3, 8.1, 8.3).
2. **Sibling Protocols on the Switch (such as `swap` in `coreyphillips/pubky-swap`):**
   - Extension kinds carry no Paykit payment semantics, name no destination, and may be evicted from
     the bounded unread pool under best-effort recovery (Paykit spec 3.3). Swap commitments and
     contracts carry invoices and HTLC lockup terms and require durable recovery records.
   - Following the `#302` design direction, hash- and point-locked swaps run as their own `swap`
     Protocol on the Switch (`swap` and `swap_provide` scopes, `protocols.swap` App record member,
     `swap.*` message kinds, and an owner-scoped `swaps` Collection), while Paykit defines only how
     a Paykit Attempt links to and settles through a swap (Section 4.2).
3. **Wallet Solver, Router, and Route Adapters (such as `synonymdev/bitkit-core`):**
   - Deterministic wallet-local route solver and router (`cheapest`, `fastest`, `most_private`) and
     concrete route adapters for direct rails, Allowances, Pubky swaps, Orchestra swaps, Spark
     service legs, and Arkade swaps, exposed to Paykit through [`Rail`] (`paykit-sdk/src/rail.rs`)
     and the host app's coordinator.

| Component | Normative / Code Home | Owns |
|---|---|---|
| Core payment coordination, Allowances, App record Paykit member, `paykit_ext:<prefix>` transport, swap-attempt link rules | `pubky/paykit-rs` (`specs/v0.2/`, `paykit-lib`, `paykit-sdk`) | Scopes, claims, `payer_activity` / `payee_activity` / `allowances` / `allowance_ledgers` / `settlements`, Paying order, `Rail` trait |
| `swap` Protocol on the Switch and HTLC engine | `coreyphillips/pubky-swap` | `protocols.swap` offers, direct RFQ/quote + one-use Invite code, `swap.*` messages, `swaps` Collection, claim/refund execution, seed restore |
| Multi-rail solver, router, and wallet route adapters | `synonymdev/bitkit-core` (`paykit_route`, `pubky_swaps`, `usdt`, `arkade`) | `cheapest` / `fastest` / `most_private` solver, RFQ fan-out, quote ranking, Orchestra / Spark / Arkade leg execution |
| Seller feature advertisement and routed proof verification | `pubky/paykit-server` | Publishing `paykit-features/1` in seller App records, verifying `routed-fill-v1` proofs |

---

## 2. Capability Discovery, Extension Transport, and Multi-Protocol Composition

### 2.1 One App Certificate Across Paykit and Sibling Protocols

A wallet running Paykit, swaps, and capability profiles uses one App ID and one App certificate
listing all required scopes explicitly (Paykit spec 3.1, 3.2; Switch spec 3.2):

- Paykit scopes: `paykit_pay`, `paykit_request`, `paykit_settlements`, `paykit_contacts`,
  `paykit_manage_contacts`, `paykit_allowance`, `paykit_allowance_execute`, and optional
  `paykit_ext:paykit-cap` (up to 8 extension scopes).
- Sibling protocol scopes: `swap` (requiring `claims.read` and `claims.write` on the Switch) for
  swap owners, or `swap_provide` (requiring `conversation.add`) for swap providers.

`Paykit::join`, `Paykit::load`, and `Paykit::restore` accept the host's complete `ProtocolSet` (or
a Switch `App` constructed from it) so Paykit validates its own scope and permission rules while
retaining every sibling protocol owner without granting foreign protocols authority over Paykit
kinds, collections, or duties (Paykit spec, "What this pin adopts: Protocol ownership and
lifecycle", and Section 3.2).

### 2.2 Static and Dynamic Capability Discovery

Paykit v0.2 replaces separate capability signing-key delegations and `/pub/paykit/ext/` Homeserver
manifests with the certified App Signing key and App record (`/pub/pubky-switch/v1/apps/<app_id>.json`
and Switch Directory; Switch spec 4.2, Paykit spec 4):

1. **Public static Paykit declarations (`protocols.paykit.capabilities`):**
   - Lists `endpoint_identifiers` (the Payment Endpoint Identifiers the app can pay to; Paykit wire
     3.2) and, once `#229` settles the wire extension, static Paykit profile declarations such as
     `paykit-features/1` (supported optional request members, rate selectors, and proof profiles
     such as `routed-fill-v1`).
   - **Spec change required when `#229` freezes:** widen `protocols.paykit.capabilities` in
     `specs/v0.2/wire-format.md` Section 3.2 beyond `{ "endpoint_identifiers": array<string> }` (or
     ignore unknown capability members as open-world profile entries with closed-world profile
     bodies) and define expiry rules for capability-only App records published without
     `public_endpoints`.
2. **Public swap provider offers (`protocols.swap`):**
   - Published on the provider's App record and Homeserver under `protocols.swap`: network, direct
     quote channel (HTTPS or `iroh:pubky-swap/direct/1`), indicative `edges` (`from` and `to` as
     exact Payment Endpoint Identifiers, `min`, `max`, optional indicative `fee` `{base, ppm,
     onchain}`, `fund_within_secs`, `lock_profiles`, `atomicity`), `htlc_timeout_blocks`,
     `required_confirmations`, and supported features such as `swap-list-v1`.
   - **Contacts-only swap providers:** omit public fees or omit Homeserver publication altogether;
     admitted Contacts receive the App record through the Switch Directory and obtain contact-scoped
     quotes directly.
3. **Private dynamic capability offers (`paykit_ext:paykit-cap`):**
   - Capabilities that must not be public (contact-specific pricing, `allowance-route/1` solver
     availability, `payjoin/1`, `credit-route/1` headroom) are exchanged only with admitted
     Counterparties as Extension kinds under `paykit_ext:paykit-cap`, narrowed by each app's
     self-signed `accepts` list so adding a new profile kind requires no new Signer approval
     (Paykit spec 3.1, 3.3).

### 2.3 Thin Generic Extension Transport in `paykit-sdk`

`paykit-lib` on `v0.2` already implements `ExtensionPrefix`, `ExtensionKind`, Check 8 scope
validation, Check 10 `check_body_with_extensions`, and `AppScopes::extension`. Once `#426` freezes,
`paykit-sdk` exposes a thin, profile-agnostic extension API:

- **Registration:** the host declares the exact `ExtensionKind`s this app accepts; `paykit-sdk`
  includes them in the App record's `accepts` when the certificate holds matching
  `paykit_ext:<prefix>`.
- **Outbound send:** sending any `<prefix>.<name>` message requires `paykit_ext:<prefix>` and
  explicit `paykit_contacts` in the sender's current App certificate, plus a complete Deny list
  copy showing the recipient is not blocked (Paykit spec 3.1, "Outbound authorization"; Paykit wire
  3.1, 4.1). Missing `paykit_contacts` fails closed before signing or queueing with `MissingScope`.
- **Bounded intake and eviction:** unread extension messages sit in a local transactional pool
  bounded at 64 per sender and 128 total (Paykit spec 3.3). Eviction atomically removes the oldest
  eligible message, forgets its processed pair, and records a `DropMark` for best-effort
  `paykit.resend_request` recovery; if the required victim has an uncertain effect, intake requests
  the client admission wait instead of exceeding either bound.
- **Adapter ownership:** profile adapters consume raw verified extension messages and keep their
  own domain state, retries, and status machines outside Paykit's collections.

---

## 3. Route Solver and Router Model

The route solver and router live in the wallet (such as `bitkit-core`) and generalize deterministic
route selection across every rail and capability Paykit can use (`#430`, `#449`).

### 3.1 Solver Inputs

```text
solve(payment_target, preference_mode, wallet_state, policy):
  payment_target:
    counterparty          : Identity
    obligation_context    : OneTimeRequest | BillingPeriod | AllowanceAutoPay | Unrequested
    candidate_endpoints   : Vec<VerifiedPaymentEndpoint>   // Paykit spec 4, 7.2, 8.1, 8.4
    requested_amount      : PaymentAmount                  // Paykit wire 1.2
    conversion_rates      : Option<ConversionRates>        // Request fixed rates or Period details
    payment_deadline_ms   : Option<u64>
    active_allowance      : Option<AllowanceContext>       // Terms, ceilings, allowed identifiers
  preference_mode:
    Cheapest | Fastest | MostPrivate
  wallet_state:
    spendable_balances    : Map<SourceBalanceId, Amount>   // Lightning, Spark, Bitcoin, USDT0, Arkade
    contact_capabilities  : VerifiedContactOffers          // Contacts' swap records & private offers
    public_swap_providers : VerifiedSwapRecords            // Public protocols.swap App records
    service_adapters      : [SparkAdapter, OrchestraAdapter, ArkadeAdapter, AllowanceRouteAdapter]
    device_liveness       : ForegroundSession | BackgroundWake
```

### 3.2 Hard Eligibility Gates (All Preference Modes)

Before scoring or sending any RFQ, a candidate route MUST pass every hard gate:

1. **Verified destination:** the Payee's Payment Endpoint comes from a verified, unexpired Payment
   Request, Period details, Private Payment List, or current App record `public_endpoints`, and its
   publishing app is current in the Payee's `identity.json` with `paykit_request` or matching
   `paykit_request:<label>` (Paykit spec 4, 7.2).
2. **Exact Payee delivery:** the route delivers the exact `PaymentAmount` computed by Paykit spec
   7.2 step 1 at the destination Payment Endpoint Identifier's precision, net of all provider,
   network, and claim fees. Any fee bump on a claim transaction MUST come from a change output or a
   pre-reserved fee budget, never out of the Payee's output (`#302` open question 4).
3. **Network and chain match:** on-chain Bitcoin identifiers match the wallet's network (`bitcoin`,
   `testnet`, `signet`, `regtest`), BOLT11 invoices match the network, unexpired expiry, and exact
   or amountless value, and EVM/token identifiers match the exact chain and token contract.
4. **Atomicity floor:** route atomicity (`min(leg.atomicity)`) meets `policy.atomicity_min` (with
   stronger floors configurable above user amount thresholds). Ordering from strongest to weakest:
   `direct` > `point` > `hash` > `escrow` > `attested` > `operator` > `credit_bounded` >
   `provider_trusted` > `fiat_attested`. Unknown classes are rejected.
5. **Liveness fit:** routes requiring background wake-ups (such as reverse HTLC claims) require
   push wake support; background Allowance auto-pay (`BackgroundWake`) excludes routes that require
   interactive foreground multi-step user confirmation.
6. **Fee cap and leg cap:** indicative and quoted fees stay within the profile and policy cap (for
   example 5% on `swap`, and the payer's automatic-payment fee cap under `Paykit spec 7.2`); at
   most one leg initially (`allow_two_leg = false` until single-leg real-device recovery metrics
   are established).
7. **Allowance and Deny list compliance:** Counterparty and any intermediate provider are absent
   from the Deny list; for `AllowanceAutoPay`, the Request is in the Allowance's Asset, uses an
   allowed Payment Endpoint Identifier, and fits remaining period, per-payment, and lifetime limits
   (Paykit spec 8.3).

### 3.3 Preference Modes (`#430`, `#449`)

Once hard gates filter the candidate set, the solver pre-ranks edges, fans out RFQs up to
`policy.fanout_budget` (default 3; reduced in `MostPrivate`), validates binding quotes (`valid_until`
at least 5 s ahead, `debit + max_fee <= spendable_balance`), and selects one winner:

| Mode | Primary Objective | Tie-Breakers (in order) | Fan-out / Discovery Policy |
|---|---|---|---|
| `Cheapest` | Minimize total effective cost in source units (`service_fee + onchain_fee + claim_fee_budget`, or maximize `credit_min / (debit + max_fee)` across priced endpoints) | 1. Stronger atomicity class<br>2. Direct > Contact provider > Public provider<br>3. Faster expected finality | Queries direct rails, local service adapters, Contact providers, and top `fanout_budget` public providers |
| `Fastest` | Minimize expected time to Payee settlement evidence (`0` round-trip instant off-chain rails such as Lightning / Spark / Arkade VTXO transfer < fast single-step service legs < multi-confirmation HTLC swaps) within `policy.fast_mode_max_fee_bps` | 1. Lower total effective cost<br>2. Stronger atomicity class<br>3. Fewer external round trips | Skips multi-block lockup routes when an instant direct or fast service route is within the fee cap |
| `MostPrivate` | Maximize privacy class (minimize chain linkability, third-party custody/KYC exposure, and RFQ metadata leakage):<br>1. Single-use Private Payment List endpoint / Payjoin / blinded or `point`-locked direct leg<br>2. Reusable direct off-chain endpoint<br>3. Admitted Contact swap provider<br>4. Public `hash`-locked swap provider<br>5. External `provider_trusted` gateway/bridge | 1. Stronger atomicity class<br>2. Lower total effective cost<br>3. Faster expected finality | Restricts RFQ fan-out (`fanout_budget = 1` sequential or Contact-only when possible) so non-winning providers do not learn payment amounts and timing |

**Frozen selection invariant.** Once the solver selects a winning route and writes Paykit's
`attempt`, execution and Reconciliation never silently re-route to another leg or provider (Paykit
spec 7.2, "No second execution"). Before `attempt` is written, if a provider refuses `swap.commit`
with a non-fading capacity refusal (`liquidity_unavailable`, `rate_limited`, `access_denied`), the
solver may commit the next ranked valid quote from the same solve.

---

## 4. Capability Profiles and Route Mechanics

### 4.1 Contacts Allowances

Contacts Allowances cover two layers: native same-asset Paykit Allowances (`Paykit spec 8.3`) and
allowance-funded cross-asset/cross-rail routing (`allowance-route/1`).

#### 4.1.1 Direct Contact Allowances (Paykit v0.2 Native)

- **Authorization and state:** managed entirely in `paykit-lib` and `paykit-sdk` through
  `allowances` and `allowance_ledgers` Shared records, the Executor's signed `Executor grant`
  (`paykit/v1/executor-grant`), its local durable `Consent list`, and `executor_notice` messages
  between own apps (Paykit spec 8.3; Paykit wire 3.3, 4.15-4.17, 5.4, 5.5).
- **Solver role during automatic execution:**
  1. When the Executor receives a qualifying `paykit.payment_request` or Billing Period occurrence
     from an Allowee Contact, `paykit-sdk` verifies every Section 8.3 precondition (active grant,
     Consent list, complete non-blocking Deny list copy, `block_floor` check, non-divergent records,
     no `manual_only` mark, complete authoritative ledger anchor, requested Asset == Allowance
     Asset, within all ceilings).
  2. `paykit-sdk` hands the allowed candidate Payment Endpoints (intersected with the Allowance
     terms' allowed `endpoint_identifiers`) to `Rail::select_endpoints` / `Rail::transfer`.
  3. The wallet's solver evaluates direct and background-safe single-leg routes under the user's
     Allowance route preference (`Cheapest` by default) and enforces the payer's automatic-payment
     fee cap (Paykit spec 7.2 step 1). Fees do not count against Allowance limits, which count the
     transferred `PaymentAmount` in the requested Asset.
  4. Manual override safety: if the user initiates a manual payment for the same request or Billing
     Period (for example choosing a `Fastest` swap route by hand), step 1's batch atomically writes
     `manual_only` on the occurrence record so no Executor can double-pay it (Paykit spec 8.3).
- **Remaining `paykit-rs` prerequisite on `v0.2`:** specifying and implementing Section 8.3.1
  (`accept_allowance` and initial ledger creation, currently refused with
  `AllowanceAcceptanceUnavailable`) and the runtime ledger handoff/repair flow.

#### 4.1.2 Allowance-Funded Cross-Asset Routing (`allowance-route/1`)

- **Purpose:** lets an Allower auto-pay cross-asset requests (for example a `usdt` invoice funded
  from a `btc` Allowance) without widening Allowance terms V1 (`Paykit spec 16`).
- **Flow:**
  1. The owner grants a standard same-asset Paykit Allowance (for example in `btc`) to a Contact or
     designated solver `S`.
  2. For a cross-asset payment, the wallet negotiates an RFQ, signed quote, and commit with `S`
     over `paykit_ext:paykit-cap` (`profile: "allowance-route/1"`), bounded by wallet policy
     `max_cost_bps`.
  3. `S` sends a standard `paykit.payment_request` in `btc` carrying `{"paykit_cap": {"flow_id",
     "contract_hash"}}` in its open `metadata` member.
  4. The Executor pays `S`'s `btc` request under unchanged Section 8.3 Allowance rules.
  5. `S` delivers the target Asset to the Payee and returns a `routed-fill-v1` proof (`contract_hash`,
     delivery settlement reference, solver signature, payer signature) which the payer sends in
     `paykit.payment_proof`.
- **Atomicity:** `provider_trusted` (`S` holds value between funding and target delivery).

### 4.2 Contacts Pubky Swaps (and Public `swap` Providers)

Builds on the route groundwork in `synonymdev/bitkit-core#182` and transitions from interim iroh
`direct/1` + delegated subkeys to the Switch `swap` protocol (`#302`,
`coreyphillips/pubky-swap#142`).

- **Contact vs. public providers:**
  - **Contacts Pubky swaps:** a Contact running a `pubky-swap` provider (`swap_provide`) is
    discovered through the Contact's App record (`protocols.swap`). Because the owner wallet
    authenticates on the Switch with its real Pubky Identity (`swap` scope under its wallet App ID,
    replacing `bitkit-core#182`'s unlinked seed-derived iroh key), Contact providers can restrict
    service to their Switch Allowlist, omit public pricing, and return contact-specific fees in
    `swap.quote`.
  - **Public swap providers:** discovered via Homeserver App records (`protocols.swap`). `swap.rfq`
    and `swap.quote` run over the provider's low-latency direct channel (HTTPS or iroh), and
    `swap.quote` returns a short-lived, single-use Switch `Invite code` so only quoted owners can
    append `swap.commit` to the provider's Inbox (`Switch spec 7`).
- **Directions and Atomicity (`hash`):**
  - **Reverse swap (`btc-lightning-bolt11` -> `btc-{network}-{p2tr|p2wpkh|p2sh|p2pkh}`):** owner
    pays the provider's Lightning hold invoice; provider locks on-chain HTLC (`taproot_boltz` or
    `p2wsh`) within `fund_within_secs`; at `required_confirmations`, provider sends `swap.status`
    (`claimable: true`, `notify: true`); owner wakes and broadcasts the preimage claim paying the
    Payee's exact on-chain output. While the Lightning payment is unsettled, claims stop at
    `CLAIM_ABORT_MARGIN` (6 blocks before timeout); once settled on the owner's node, claims keep
    bumping past timeout until the claim or a provider refund confirms.
  - **Submarine swap (`btc-{network}-*` -> `btc-lightning-bolt11`):** owner funds on-chain HTLC
    lockup naming the Payee's BOLT11 invoice hash in `swap.commit`; after `required_confirmations`,
    provider pays the Payee's BOLT11 invoice and claims the lockup, revealing the preimage.
- **Paying order and recovery link (`Paykit spec 7.2, 7.3`):**
  1. **Write `attempt` before `swap.commit`:** `orphan_after` MUST cover the longest swap horizon
     (reverse: hold-invoice maximum lock window, ~216 blocks / 36 hours, plus dispatch margin;
     submarine: `timeout_block_height` + claim margin).
  2. **Persist swap state before sending `swap.commit`:** write the owner's `swaps` Shared record
     containing `{quote, commit, owner_ref, paykit: {attempt_id}}`, where `owner_ref` encodes the
     32-byte seed derivation index (`bitkit-swap-v4:<58-char base64url>`). Record `{swap_id,
     provider}` in Paykit's local payment journal for the Attempt.
  3. **Funding is not completion:** paying the hold invoice or confirming the on-chain lockup keeps
     the Paykit Attempt open (`ATTEMPTING`) so no other own app can double-pay the Request.
  4. **Completion evidence:** reverse swaps complete only when the claim transaction paying the
     Payee's exact address and amount confirms (producing an ordinary on-chain `payment_proof`);
     submarine swaps complete only when the preimage matching the Payee's BOLT11 payment hash is
     verified (producing an ordinary Lightning `payment_proof`). The Payee needs no swap-specific
     verification logic.
  5. **Definitive failure:** mark the Attempt `failed` only when the swap terminates without paying
     the Payee (for example cancelled hold invoice before claim, or confirmed submarine refund).
  6. **Seed restore:** a restored wallet reads its `swaps` Collection (falling back to
     `swap.list_request` for uncommitted records), re-derives per-commit keys and preimages from
     `owner_ref` + seed, and reconciles the linked Paykit `attempt_id`.

### 4.3 Orchestra Swaps

Integrates Flashnet Orchestra cross-chain / cross-layer conversion routes (building on
`synonymdev/bitkit-core#165`, `modules/usdt/orchestra.rs`, and `bitkit-usdt-service`).

- **Capabilities covered:** cross-asset and cross-chain routes (such as EVM/Tron USDT <-> Arbitrum
  USDT0 and Bitcoin/Lightning/Spark <-> stablecoin corridors supported by Orchestra), modeled as a
  wallet-local service adapter or advertised by a solver under `cross-asset/1`.
- **Atomicity and trust:** `provider_trusted` (funds are sent to a provider-bound deposit address
  or bridge contract; Orchestra executes conversion/delivery or refunds to a verified source-chain
  address).
- **Protocol and solver rules:**
  1. **Exact-out requirement for direct Payee delivery:** to pay a Payee's Payment Endpoint directly
     in one leg, the Orchestra quote MUST guarantee an exact delivered `PaymentAmount` after
     provider fees (such as fixed-output order modes). Standing-address deposit routes whose output
     is only an indicative estimate (or batched) MUST NOT auto-pay a Payee Request directly unless
     wrapped in an exact-out solver quote or confirmed as a wallet self-transfer first (Paykit spec
     7.2 step 1).
  2. **Strict chain and token pinning:** source and destination addresses and unit scaling (such as
     6-decimal Arbitrum USDT0 vs. 18-decimal BSC USDT, normalized to Paykit's exact decimal
     `PaymentAmount`) are validated before `attempt`; an EVM address is never reused on an unlisted
     chain.
  3. **Paying order and completion evidence:** write `attempt` with `orphan_after` covering the
     provider's maximum fill + refund SLA before funding the deposit. Provider order status (`in_transit`,
     `completed`) is treated as a hint only: the Attempt is marked `done` only when destination-chain
     settlement to the Payee's endpoint is verified (producing the v0.2 ERC-20 / chain proof or
     `routed-fill-v1`), and marked `failed` only upon verified source-chain refund evidence proving
     non-delivery.

### 4.4 Arkade Swaps

> **Bitkit product note:** Bitkit parked its Arkade runtime in
> [synonymdev/bitkit-core#185](https://github.com/synonymdev/bitkit-core/pull/185) and
> [synonymdev/bitkit-android#1947](https://github.com/synonymdev/bitkit-android/issues/1947) after
> selecting Spark for Lightning. Arkade remains specified in this capability and solver architecture
> so `#448`'s full route matrix is covered and any Arkade-enabled Paykit wallet or solver can plug
> in without protocol changes.

Arkade (`synonymdev/bitkit-core#183`, `#185`) contributes two distinct route shapes to the solver:

1. **Arkade Collaborative Batch Offboard (VTXO -> On-Chain Bitcoin Payee Endpoint):**
   - **Mechanics:** spends wallet Arkade VTXOs into the operator's next commitment batch paying
     exact `amount_sat` to the Payee's `btc-{network}-*` address, with the operator's exit fee
     charged on top (`debit_sat = amount_sat + exit_fee_sat`, matching `arkade_offboard_exact`).
   - **Atomicity:** `operator` (requires the Ark operator's co-signature to include the offboard in
     a batch; VTXOs retain unilateral on-chain exit if the operator stalls).
   - **Completion evidence:** confirmed batch transaction containing the Payee's exact on-chain
     output (ordinary Bitcoin `payment_proof`).
2. **Arkade Intents / VHTLC Swaps (VTXO <-> Lightning BOLT11, or `swap` Protocol over Arkade):**
   - **Mechanics:** quotes an Arkade solver (over Nostr NIP-44 kind 24859 today, or natively over
     the Switch `swap` protocol), funds a VHTLC v2
     covenant lockup from VTXOs, and settles when the solver pays the Payee's BOLT11 invoice and
     reveals the preimage (or claims into VTXOs on reverse receive).
   - **Atomicity:** `hash` (VHTLC preimage lock with `refund_locktime`) or `point` (adaptor
     transfers).
   - **Attempt horizon and durable backup requirement:** if a send fails without cooperative solver
     refund, VHTLC funds remain locked until `refund_locktime` (~7 days / 612,000 s). Paykit's
     `attempt` `orphan_after` MUST cover the time until the Lightning invoice expires and non-payment
     is authoritative, and the wallet MUST persist the VHTLC swap record (quote/RFQ IDs, preimage
     salt when non-HD, covenant scripts, `refund_locktime`, and linked `attempt_id`) in its durable
     encrypted backup (`swaps` Shared record or Switch `AppBackup`) *before* funding, because the
     Arkade indexer alone cannot reconstruct Lightning VHTLC swaps from seed.

---

## 5. Route Capability Matrix Across Solver Modes

| Route / Capability | Source -> Payee Endpoint | Atomicity Class | Typical Finality | Privacy Profile | `Paykit` Completion Evidence | Attempt `orphan_after` Horizon |
|---|---|---|---|---|---|---|
| **Direct Off-Chain** (Lightning / Spark LN / Direct VTXO) | LN / Spark -> `btc-lightning-*` | `direct` | Seconds (1 foreground step) | High (especially single-use Private Payment List endpoint) | BOLT11 preimage / rail settlement ID | Minutes (invoice / payment timeout) |
| **Direct On-Chain** (Bitcoin / Arbitrum USDT0) | BTC / USDT0 -> `btc-*` / `usdt-arbitrum-*` | `direct` | 10-60 min (BTC) / seconds (Arb) | Medium-High (highest with `payjoin/1` or single-use address) | Confirmed txid + output proof | Mempool / confirmation horizon |
| **Contacts Allowance** (`paykit_allowance_execute`) | Allowed payer rail -> Allowee Request endpoint | Underlying rail's class | Background automatic (0 user prompts) | High (same Counterparty, no third party) | Underlying rail's `payment_proof` | Underlying rail's `orphan_after` |
| **Allowance-Routed Solver** (`allowance-route/1`) | Payer Allowance Asset -> Solver -> Payee Asset | `provider_trusted` | Seconds to minutes (background wake) | Medium (solver learns Payee delivery) | `routed-fill-v1` in `payment_proof` | Solver quote + delivery SLA |
| **Contacts Pubky Swap** (`swap` Protocol, Contact provider) | LN <-> `btc-{net}-*` | `hash` | Sub: ~20 min; Rev: ~20-30 min (+ wake) | Medium-High (trusted Contact, no public RFQ fan-out) | Confirmed claim txid (rev) or BOLT11 preimage (sub) | Rev: ~36h (216 blocks); Sub: timeout + margin |
| **Public Pubky Swap** (`swap` Protocol, Public provider) | LN <-> `btc-{net}-*` | `hash` | Sub: ~20 min; Rev: ~20-30 min (+ wake) | Medium (RFQ reveals amount/timing to queried providers) | Confirmed claim txid (rev) or BOLT11 preimage (sub) | Rev: ~36h (216 blocks); Sub: timeout + margin |
| **Orchestra Swap** (`cross-asset/1` / Service leg) | Multi-chain USDT / BTC / LN -> target endpoint | `provider_trusted` | Seconds to minutes (chain-dependent) | Low-Medium (companion gateway + cross-chain footprint) | Verified destination transfer proof / `routed-fill-v1` | Provider fill + source refund SLA |
| **Arkade Offboard** (`arkade_offboard_exact`) | Arkade VTXO -> `btc-{net}-*` | `operator` | Next Ark batch round (seconds to minutes) | Medium (operator co-signs batch) | Confirmed Ark batch txid + Payee output | Batch round timeout + margin |
| **Arkade VHTLC Swap** (Intents / `swap` over Arkade) | Arkade VTXO <-> `btc-lightning-bolt11` | `hash` / `point` | Seconds (send) to minutes | Medium (solver sees invoice) | BOLT11 preimage (send) / claimed VTXO | Invoice expiry / VHTLC `refund_locktime` (~7d) |

---

## 6. Implementation Order and Prerequisites

Once `#426` (Inbox v0.2 freeze), `#229` / `#302` (Capabilities + Switch `swap` spec), and `#449`
(solver preference spec) settle, implementation proceeds in five PR-sized slices:

1. **Slice 1 -- `paykit-rs` (`v0.2`) App Record Capabilities & Spec Vectors:**
   - Extend `specs/v0.2/spec.md` (Sections 4 and 7.2) and `specs/v0.2/wire-format.md` (Section 3.2)
     with `paykit-features/1` static declarations, `routed-fill-v1` proof rules, and Section 9 swap
     attempt rules from `#302`.
   - Update `paykit-lib/src/app_record.rs` and regenerate `specs/v0.2/vectors/documents.json` with
     positive and malformed capability cases.
2. **Slice 2 -- `paykit-rs` (`v0.2`) Thin Extension SDK & Multi-Protocol `ProtocolSet`:**
   - Wire `ProtocolSet` composition through `Paykit::join`, `Paykit::load`, and `Paykit::restore`
     (`paykit-sdk/src/paykit.rs`, `protocol.rs`).
   - Implement outbound `paykit_contacts` + Deny list enforcement for `paykit_ext:<prefix>` sends
     and the 64/128 transactional unread extension pool with `DropMark` eviction and admission-wait
     fallback (`paykit-sdk/src/receive.rs`, `outbound.rs`), with positive and failure-path tests.
3. **Slice 3 -- `paykit-rs` (`v0.2`) Contacts Allowances Completion (`accept_allowance` & Executor):**
   - Land Section 8.3.1 initial ledger creation and `Paykit::accept_allowance`, plus Executor
     dispatch integration with `Rail` fee caps and `allowance-route/1` metadata correlation.
4. **Slice 4 -- `pubky-swap` & `bitkit-core` Switch `swap` Protocol + Contacts Pubky Swaps:**
   - Implement the `swap` Switch `Protocol` (`protocols.swap`, `swap.*` messages, `swaps`
     Collection, Invite-code first contact, exact-output reverse claim fee budgeting) and migrate
     `bitkit-core/src/modules/pubky_swaps` onto the wallet's real Pubky Identity for Contacts and
     public providers, including "phone disappeared" kill/restore tests.
5. **Slice 5 -- `bitkit-core` Unified Solver (`cheapest`, `fastest`, `most_private`) + Orchestra & Arkade Adapters:**
   - Upgrade `bitkit-core/src/modules/paykit_route` into the multi-mode solver (`#449`), plugging in
     Contacts Allowances, Contacts/Public Pubky swaps, Orchestra exact-out routes, Spark service
     legs, and the Arkade route trait adapter (kept feature-gated while parked in Bitkit per
     `bitkit-android#1947`).
