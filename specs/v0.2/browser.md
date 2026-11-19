# Paykit SDK in the browser

Status: draft for review. It targets Paykit v0.2, which is itself a draft and
not frozen (`v02:specs/v0.2/README.md:3`). This document governs nothing in
v0.2 yet; it records what a browser needs from the v0.2 and Switch documents.

This document plans how the Paykit SDK runs in a web browser, compiled to
WebAssembly (`wasm32-unknown-unknown`). It tracks
[#411](https://github.com/pubky/paykit-rs/issues/411) and replaces the v0.1
proposal in [#437](https://github.com/pubky/paykit-rs/pull/437). The browser
constraints that v0.2 should settle before its freeze are raised in
[#537](https://github.com/pubky/paykit-rs/issues/537).

A v0.1 spike ran most of the v0.1 SDK in Chrome, Firefox, and Playwright
WebKit ([Evidence](#evidence)). That is evidence about the browser platform,
not about v0.2. The v0.2 crates compile for the browser with build changes
only, but the payment path would trap at run time (*inferred*, D8), and the
Switch contracts they rely on were not designed for a browser. The rest is
product, security, and API decisions.
[Summary of decisions](#summary-of-decisions) lists what this document
proposes and who decides each item.

## How to review this document

- Each entry under [Decisions](#decisions) ends with a **Proposal** and one or
  more **Decision needed** lines. Comment on that section.
- Owners, by role:
  - **Paykit**: the Paykit maintainer (this plan, the package, the binding).
  - **Paykit v0.2**: the v0.2 author (the v0.2 spec, `paykit-lib` and
    `paykit-sdk` on `v0.2`, and Paykit's team decisions).
  - **Pubky Switch**: the Switch maintainers (spec, client, and service).
  - **Pubky**: the Pubky maintainers (Homeserver, client, key derivation).
- Before this document merges, every **Decision needed** line owned by Paykit
  becomes **Decided** with the outcome, or **Deferred** with an owner and a
  date. A line owned by anyone else becomes **Asked**, with a link, and keeps
  its fallback. The link points to its #537 item. Asks marked *not yet filed*
  are the [Pubky asks](#pubky-asks-that-do-not-wait-on-the-freeze), each
  raised in a new Pubky issue. This document records Paykit's position only.
- Items that wait on the v0.2 freeze are open notes N1 to N9
  ([Open notes and questions](#open-notes-and-questions)). They may stay open
  when this document merges.
- Evidence labels:
  - *Verified (v0.1 spike)*: ran in a browser on v0.1.
  - *Verified (v0.2 build)*: `cargo check --target wasm32-unknown-unknown` of
    the v0.2 crates (some rows with patched manifests), a scratch crate that
    implements the Switch client's host traits at the v0.2 pin, or a
    standalone wasm32 probe of `std` run in Node.js.
  - *Verified (probe)*: a network check on 2026-11-19.
  - *Read*: from source, a specification, or vendor documentation; not run.
  - *Inferred*: reasoned from read sources; not run.
  - *Untested*: an expectation.

## Terms

- *Host app*: the web application that embeds the SDK.
- *Web app as signer*: the host app acting as its identity's signer (#537 item
  4). Elsewhere *web app* has its usual meaning.
- *App*: on its own, always an app of the Switch, with its own keys and app
  certificate. A *Paykit App* is an app holding Paykit scopes.
- *Browser partition*: one origin, in one browser profile, under one top-level
  site. Partitions share no storage, Web Locks, or channels
  ([What a browser changes](#what-a-browser-changes)).
- *Owner context*: the one context (tab or worker) in a partition that runs
  the SDK for an app and holds its store. "(Owner: ...)" instead names a
  decision owner.
- *Store namespace*: the key prefix in the host's `StateStore` that one crate
  owns and writes alone, such as `signer/<identity>` for the signer or
  `client/<app_id>` for one app (D3). The store's ownership API (`acquire`,
  `commit_if`) gives one holder exclusive use of a namespace (D4).
- *First release*: the pre-release at the end of [rollout](#rollout) step 3.
- *Trap*: a WebAssembly run-time abort (`RuntimeError: unreachable`) that ends
  the call.
- *Spike*: the throwaway #411 branch that ran v0.1 in browsers.
- *CORS*: Cross-Origin Resource Sharing. *OPFS*: the Origin Private File
  System. *AEAD*: authenticated encryption with associated data. *PRF*: the
  WebAuthn pseudo-random function extension. *VAPID*: Voluntary Application
  Server Identification, the server key scheme of Web Push.
  *APNs*, *FCM*: Apple's and Google's push services. *BIP32*, *BIP39*,
  *BIP85*: Bitcoin standards for hierarchical key derivation, mnemonic seed
  phrases, and child mnemonics derived from a master seed.
- *WebPKI*: the public certificate authorities a browser trusts for HTTPS.
  *PubkyTLS*: TLS authenticated by the Homeserver's Pubky key instead.
  *DHT*: here, the Mainline distributed hash table. *pkarr*: signed
  DNS-like records for a Pubky key, stored on the DHT. Native clients can
  query the DHT directly; a browser reaches pkarr records only through HTTP
  relays.
- Other v0.2 terms (identity key, signer, App ID, app certificate, key bundle,
  switch, approval) are as in `v02:THESAURUS.md` and the v0.2 and Switch
  specs.

## Citations

| Prefix | Repository and commit | Permalink base |
| --- | --- | --- |
| `v02:` | paykit-rs `v0.2` at `7aa75fe4048e7e7d2d17d0aded544c02da53b103` (not master's older `specs/v0.2/`) | `https://github.com/pubky/paykit-rs/blob/7aa75fe4048e7e7d2d17d0aded544c02da53b103/` |
| `sw:` | pubky-switch at `a9d7261820ee8fd8fc056ac111d04ae683f2c206`, the pin in `v02:Cargo.toml:21-24`, which open pull requests into `v0.2` may move. The v0.2 spec's normative baseline is Switch `5f3209aa` (`v02:specs/v0.2/spec.md:9`) | `https://github.com/pubky/pubky-switch/blob/a9d7261820ee8fd8fc056ac111d04ae683f2c206/` |
| `m:` | paykit-rs `master` at `e047abd2e3a46674a4c02761488e7f4a6ee2ceb8` | `https://github.com/pubky/paykit-rs/blob/e047abd2e3a46674a4c02761488e7f4a6ee2ceb8/` |
| `spike:` | paykit-rs `throwaway/wasm-browser-sdk-411` at `b74712c4d346df124f2e9b4cbd4c0443d63a41a0` | `https://github.com/pubky/paykit-rs/blob/b74712c4d346df124f2e9b4cbd4c0443d63a41a0/` |

`sw:spec/spec.md:98` means line 98 at that commit (for Markdown, append
`?plain=1#L98`); a bare `:N` repeats the previous file, and a short path
such as `store/mod.rs` or `app/STATE.md` is under
`sw:crates/pubky-switch-client/src/`. "Paykit spec N", "Paykit wire N",
"Switch spec N", and "Switch wire N" name sections. "Switch decision N" is an
item in Switch spec 19. Paykit spec 17 has a single numbered list: "Paykit
decision N" is an item under its "Further team decisions" heading, and
"Paykit open question N" is any other item.

## Summary of decisions

*Blocks* names the first [rollout](#rollout) step that needs the decision.

| ID | Decision | Owner | Proposal in one line | Waiting on | Blocks |
| --- | --- | --- | --- | --- | --- |
| [D1](#d1-identity-and-the-signer) | Identity and the signer | Paykit, Paykit v0.2, Pubky | First release: an app approved by a signer elsewhere; a web app as signer only if #537 item 4 allows it; each browser install its own app | #537 items 4 and 6 (N3); seed derivation (#537 "can wait") | Step 3 |
| [D2](#d2-secret-custody-and-threat-model) | Secret custody | Paykit, Pubky Switch | Same-origin script is trusted; sealed storage is the minimum; the host may supply the wrapping key; the backup key survives eviction | #537 item 1 | Step 3; a stable release (step 4) needs item 1 answered yes |
| [D3](#d3-the-state-store) | Where app state lives | Paykit, Pubky Switch | One store per app and partition; a reference store on OPFS unless an async store seam lands | #537 items 1 and 3; N4, N5 | Step 3 |
| [D4](#d4-tabs-and-store-ownership) | Several tabs | Paykit, Pubky Switch | One live owner context per store namespace and partition; ask for a safe release of store ownership | #537 item 3 | Step 3 |
| [D5](#d5-transport-trust-and-cors) | Transport trust and CORS | Paykit, Pubky, Pubky Switch | WebPKI for Homeserver traffic; Switch traffic needs normative CORS and a browser address rule | #537 items 1 and 2 (N6); WebPKI (not yet filed) | Step 3; step 4 needs CORS deployed |
| [D6](#d6-fresh-reads-and-http-caching) | Fresh reads | Paykit, Pubky | Homeserver and relay reads bypass the HTTP cache; a first-release blocker | `no-cache` on `/pub` and `NetworkOnly` (not yet filed) | Step 3 |
| [D7](#d7-receiving-and-push) | Receiving | Paykit, Pubky Switch | Foreground long-poll first, no event stream; Web Push later | N7, N8 | Step 3; Web Push in step 5 |
| [D8](#d8-clocks-timers-and-deadlines) | Clocks and deadlines | Paykit, Pubky Switch | A host monotonic clock in both repos; the binding's transport applies the reference timeouts | #537 "can wait" (`FirstSend`); `pay.rs` `Instant::now()` (#537 "cheaper now"); "last cancellable handoff" (#537 item 1) | Step 3 |
| [D9](#d9-where-the-sdk-runs) | Execution context | Paykit | Dedicated worker; service worker after the async store seam and its own test run | N1 | Step 3 |
| [D10](#d10-host-interfaces-and-send) | Host interfaces and `Send` | Paykit, Pubky Switch | Store, clock, timer, transport, and reader in Rust; only `Rail` crosses to JavaScript | #537 "cheaper now" (`MaybeSend`) | Step 3 |
| [D11](#d11-toolchain-and-package) | Toolchain and package | Paykit | Generator chosen with the v0.2 bindings workstream; separate npm package | N9 | Step 2 |
| [D12](#d12-supported-browsers-and-environments) | Supported browsers | Paykit | A version window per engine; Lockdown Mode unsupported | None | Step 4 |
| [Groundwork](#groundwork-on-the-v02-crates) | Groundwork placement | Paykit, Pubky Switch | Each fix lands in the crate that owns the code; no public helper API; browser code stays out of protocol logic | `pubky-switch-signer` forcing `http` (#537 "cheaper now") | Step 1 |
| [Rollout](#rollout) | Real funds on the pre-release | Paykit | No, until step 4's security review is done | None | Step 3 |

## Scope

In scope:

- Building `paykit-lib`, `paykit-sdk`, and the Switch crates they use
  (`pubky-switch-core`, `pubky-switch-client`; `pubky-switch-signer` only for
  a web app as signer, D1) for `wasm32-unknown-unknown`, single-threaded. The
  Switch crates belong to Pubky.
- A TypeScript package for the v0.2 SDK surface (join, load, restore, pay,
  receive, and the rest of `v02:docs/paykit-host-guide.md`), listing every
  exported call and whether it ran in a browser.
- The browser rules a host app must follow: signer role, storage, secrets,
  tabs, Content Security Policy (CSP), push, and supported browsers.

Out of scope: Node.js, Deno, and other non-browser runtimes; WebAssembly
threads (if the build relaxes `Send`, it must fail to compile with the
`atomics` target feature); Paykit wire, path, kind, or scope changes (the plan
does ask the Pubky Switch for a browser clause and normative CORS); user
interface.

## Why v0.2, and when

After a validated audit of #437, the team decided that the browser SDK
targets v0.2 and that the first integrator waits for the v0.2 release. v0.2
replaces v0.1 with no migration: payer and payee both update, v0.1 and v0.2
apps cannot pay each other, and Allowances and recurring requests do not carry
over (`v02:specs/v0.2/spec.md:670-675`). It removes what made a v0.1 browser
binding risky: the shared-state blob, Homeserver locks, leases, Noise links,
and polling each contact (`:640-657`). The `v0.2` branch has only `paykit-lib`
and `paykit-sdk` 0.2.0-alpha.0 on pinned Switch crates
(`v02:Cargo.toml:3-24`), unpublished until the freeze; language bindings
"come later" (`v02:README.md:35`). The Switch spec defers "browser and WASM
support" (`sw:spec/spec.md:1251`), and the Paykit spec does not mention
browsers.

v0.2 freezes when five items are done (`v02:specs/v0.2/spec.md:3`,
`:984-988`):

1. The team approves the spec, including Paykit decision 22 (labelled selling
   scopes; no browser impact found) and Switch decision 24 (the signer
   hand-over, D1).
2. A production switch has persistent storage.
3. The retention rules get "a last check". A restoring browser app relies on
   them for the 30-day re-send and stub windows.
4. The measurements run on real infrastructure.
5. An independent human crypto review is done.

**Open note N2 (waiting on v0.2): the freeze items.** Undecided: the outcomes
of the five items above. Resolved by: the team's freeze review. Blocks:
rollout steps 1 to 3.

The release date is not set. It waits on the freeze items (N2) and the dates in
Paykit decision 20's phased plan (N1). Rollout also has gates on #537
([Before the v0.2 freeze](#before-the-v02-freeze), [Rollout](#rollout)). The
first integrator also depends on when the payees its users pay move to v0.2.
Each payee's app moves on its own schedule, and no Paykit plan sets it. Under
the [Rollout](#rollout) proposal, users' funds move only after rollout step 4,
which also needs a production switch that serves CORS (#537 item 2) and the
D2 security review.

**Open note N1 (waiting on v0.2): the SDK surface.** Paykit decision 20 drafts
the v0.2 SDK surface and maps "the host app's calls" onto it before the
freeze, then sets a phased plan with owners and dates
(`v02:specs/v0.2/spec.md:712`). Undecided: the final surface and dates.
Resolved by: Paykit decision 20's draft, which should map a browser host's
calls too. Blocks: the page-to-worker API (D9) and the surface to bind (N9).

## Evidence

### What ran on v0.1 (the #411 spike)

The spike branch is based on `ed92032b`. Do not merge it. Its log is
`spike:docs/wasm-spike-411.md`, and every temporary change is marked
`THROWAWAY (#411)`. Setup: a local `pubky-testnet` 0.16.2 over plain HTTP,
Chrome 157, Playwright Firefox 157, and Playwright WebKit 27.2, headless
("all three" below). All parties ran in one page, one profile, and one
Homeserver: no counterparty on a second Homeserver, no live native peer, no
bearer refresh, and no background throttling. "Worker" means a dedicated
worker. The spike did not run profiles and avatar upload, Public Contact
Markers, contact removal and blocking, app removal, reservations, or sign-in
through an external signer.

| Run (*verified (v0.1 spike)*) | Result | See |
| --- | --- | --- |
| Offline wasm suite, 18 tests, page and worker | Pass in all three | D11 |
| Full SDK flow through a hand-written `wasm-bindgen` facade, 42 steps | 42/42 in Chrome and WebKit; Firefox 41/42, and 42/42 with `cache: "no-store"` | D6 |
| Generated UniFFI TypeScript, 21 steps | 21/21 in Chrome; not run elsewhere | D11 |
| A second tab restores the same grant; the first tab's writes then fail with HTTP 401 | Reproduced in all three | D4 |
| Single owner elected with a Web Lock, called over `BroadcastChannel` | Pass in all three | D4 |
| IndexedDB compare-and-swap, strict durability, two racing runtimes in one context | Pass in all three | D3 |
| IndexedDB row sealed with a non-extractable AES-GCM-256 `CryptoKey`; tampering fails | Pass in all three | D2 |
| Intake restart after a reload, blocked by the v0.1 lease | About 116 s; per-engine figures not logged | D4 (v0.1 only) |
| Panic, then rebuild the worker from IndexedDB | 36 to 118 ms across engines; per-engine figures not logged | D9 |

The lifecycle rows ran from a committed harness
(`spike:paykit-wasm/www/playwright/lifecycle.mjs`). The event-stream intake
ran with the spike's trailing-slash fix (`91b235e8`);
[#435](https://github.com/pubky/paykit-rs/issues/435) is still open.

### What was checked on v0.2

On `v02` and `sw` as cited, with rustc 1.96.0. Nothing ran in a browser.

| Check (*verified (v0.2 build)*) | Result |
| --- | --- |
| `cargo check --target wasm32-unknown-unknown -p paykit-lib`, unchanged | Fails: `getrandom` 0.2 needs `js` (through `chacha20poly1305`, `sw:Cargo.toml:36`) |
| `-p paykit-sdk`, with the 0.2 backend | Fails: `mio` (Tokio `net`) and `ring` 0.17.14's C build, from the client's default `http` feature (`sw:crates/pubky-switch-client/Cargo.toml:12-18`); `getrandom` 0.3 needs its backend |
| `-p paykit-sdk`, `http` off, both `getrandom` backends | Passes, with no source change |
| `-p paykit-lib --features signer`, same | Fails: the Switch workspace takes the client with default features (`sw:Cargo.toml:29`); passes with that line patched |
| A standalone std-only probe (a cdylib, not the v0.2 crates) calling `Instant::now()`, `SystemTime::now()`, `std::thread::spawn` on wasm32, run in Node.js with no imports | Each traps: `RuntimeError: unreachable`. The v0.2 call sites (D8) are *inferred* to trap the same way; no v0.2 code ran |
| A store holding a `FileSystemSyncAccessHandle`; a `Date.now()` clock | Compile; `JsValue` is `Send + Sync` without `atomics` |
| A `fetch` transport awaiting `JsFuture` inside `BoxFuture` | Fails: `JsFuture` is not `Send` |
| The same via `spawn_local`, returning a future that awaits a `oneshot` receiver | Compiles, with no `unsafe` |

*Verified (probe)*, 2026-11-19: the production and staging pkarr records name
`homeserver.pubky.app` and `homeserver.staging.pubky.app`. Both relays
(`pkarr.pubky.app`, `pkarr.pubky.org`) allow any origin: a GET echoes the
request's `Origin` in `Access-Control-Allow-Origin` with
`Access-Control-Allow-Credentials: true`, and an OPTIONS preflight answers
`Access-Control-Allow-Origin: *`. The SDK fetches with `credentials: "omit"`
(D5), so the credentials header has no effect.

### What did not run

- Any v0.2 code in a browser; any real Switch service (it has no CORS, D5);
  approval by a signer, the main v0.2 path (D1). No wasm CI job exists on
  `v0.2` or in pubky-switch.
- An SDK or browser run against a real HTTPS Homeserver or the production
  pkarr relays. The probe only fetched relay records and read response headers
  (CORS, `Cache-Control`).
- A service worker or shared worker; Safari itself; any mobile browser; real
  freezing, discarding, sleep, and intensive throttling (freezes were a busy
  loop); eviction, Safari's 7-day deletion, private windows; a CSP; the Paykit
  Profile and the files its fields reference (such as `image_uri`).

## What a browser changes

*Read*, except where marked.

| Difference | Effect on v0.2 | See |
| --- | --- | --- |
| No threads and no Tokio runtime | Futures that await JavaScript (such as `JsFuture`) are not `Send`; timers and tasks come from the event loop | D8, D10 |
| No operating-system key store | WebCrypto keys can be non-extractable, for WebCrypto's own algorithms only, but any origin script can use them; no engine ships ChaCha20-Poly1305 in WebCrypto | D2 |
| Storage is mostly asynchronous and evictable | The only suitable synchronous durable store is an OPFS `FileSystemSyncAccessHandle`, in dedicated workers only; eviction deletes a whole origin | D3 |
| Networking is `fetch` | Browser TLS and certificate authorities; no raw socket, custom certificate check, DHT, or view of resolved addresses; CORS on every cross-origin call | D5 |
| The context can stop at any time | A tab can be reloaded, frozen, discarded, or closed mid-call. v0.2 signs an artifact, commits it, then resends the same bytes until answered, so a cut-off call resumes on the next start (`store/mod.rs:8-12`) | D4, D9 |
| Partitions | Storage, Web Locks, and channels are per partition (below) | D3, D4 |
| A secure context is required | For WebCrypto, Web Locks, OPFS sync handles, persistent storage, and service workers | D12 |

**Partitions.** Tabs, workers, and a service worker of one origin in one
profile share storage, Web Locks, and `BroadcastChannel`. Another browser,
another profile, a private window, and a cross-site frame (Chrome 115+,
Firefox 103+, WebKit) are separate partitions. Each iOS and iPadOS Home
Screen icon has its own storage, even for one site; so does a macOS Safari
Dock web app. An installed Chromium web app shares the profile (*untested*; no
Chrome document states it). On iOS, `SFSafariViewController` keeps its own
data. Other in-app browsers (WebViews, Android Custom Tabs) are *untested*.

## Host contracts in a browser

A v0.2 host builds an `AppEnv` (`app/mod.rs:286-301`) and passes a `Rail` and
a `Timer` to `Paykit::load` (`v02:paykit-sdk/src/paykit.rs:570`). Every trait
is object-safe and `Send + Sync`; async methods return `BoxFuture`, a `Send`
future (`transport/mod.rs:24`). `paykit-sdk` names Tokio only as a
dev-dependency (`v02:paykit-sdk/Cargo.toml:33`), but Tokio still reaches its
default build through the client's default `http` feature, which a browser
build must turn off ([Groundwork](#groundwork-on-the-v02-crates), `http` row).

| Contract | v0.2 requirement | Browser plan | Status |
| --- | --- | --- | --- |
| `StateStore` (`store/mod.rs:84`) | Synchronous; guarantees 1 to 7 (`store/mod.rs:60-83`) | OPFS in a dedicated worker, or IndexedDB behind an async seam (D3) | Partly blocked: #537 items 1 and 3 |
| `StoreHandle` (`store/handle.rs:9-59`) | Inline, or Tokio `spawn_blocking` | Inline only | Fits; no async-store mode |
| Ownership (`store/mod.rs:116-143`) | No timeout or preemption | Web Lock plus the OPFS file lock (D4) | Open: #537 item 3 |
| `Transport::send` (`transport/mod.rs:212-222`) | No redirects; switch origin rules (`sw:spec/wire-format.md:1593`) | `fetch` with `redirect: "manual"`, `credentials: "omit"`, reference timeouts (D5, D8) | Open: address check (#537 item 1) and CORS (item 2) |
| `Transport::send_before` (`transport/mod.rs:223-229`) | First-handoff deadline at the last cancellable handoff | Not implemented at first (D8) | Open |
| `Clock` (`clock.rs:7-10`) | Unix milliseconds | `Date.now()`; never `SystemClock`, which traps | Fits |
| `HomeserverReader` (`app/identity.rs:180-191`) | On the host's Pubky client | `fetch` with `cache: "no-store"`, or the host's Pubky JavaScript client (D6, D10) | Fits; cache rule is a blocker |
| `HomeserverWriter` (`app/own_record.rs:62`) | The app grant's Pubky session; optional | Owner context only (D4) | Fits |
| `PushRelay` (`app/push.rs:70-84`) | Relay URL, device token, push key | A Web Push relay; token is a `PushSubscription` (D7) | Open: no relay |
| `Timer` (`v02:paykit-sdk/src/timer.rs:12-15`) | Bounds the payee switch lookup | `setTimeout` | Fits; throttled when hidden |
| `Rail` (`v02:paykit-sdk/src/rail.rs:393`) | Six synchronous methods, three async | The wallet's payment method in JavaScript (D10) | Fits with a channel shim |
| Randomness | `getrandom` 0.2 and 0.3 in the wasm build (0.4 only in dev and native dependencies) | `crypto.getRandomValues` via each browser backend | Fits (Groundwork) |
| Signer `SecretVault` (`sw:crates/pubky-switch-signer/src/key.rs:105`) | Synchronous; device-bound, not backed up (`sw:docs/signer-host-guide.md:11`) | Only for a web app as signer (D1) | Open: #537 item 4 |

## First integrator

One host app is expected to integrate first: a self-custodial progressive
web app (PWA) wallet whose engine runs in a service worker. The design must
not depend on this one host app, so this section lists host properties the
design must support (*read* from public sources, not run).

| A host may | Gap in the first release | See |
| --- | --- | --- |
| Run its engine in one service worker per origin, as a single bundled classic script that pages talk to by messages and restart when the browser stops it, with no active fetch handler | The service worker is deferred | D9, D6 |
| Be web-only, with React Native and Expo entry points in a separate TypeScript SDK library | None | D11 |
| Keep the wallet seed under no user secret, so it starts without an unlock step | None | D1, D2 |
| Accept a raw 32-byte secp256k1 key with no mnemonic, and use one key for signing and backup | None | D1, D2 |
| Use Web Locks so that one tab drives certain work | None | D4 |
| Ship a CSP without `'wasm-unsafe-eval'` | The SDK worker's CSP must allow it | D12 |
| Run as a PWA install and in a browser tab, or in two browsers | Two browsers are always separate partitions. A PWA install and a tab are separate partitions, and so separate apps, on iOS, iPadOS, and Safari on macOS; an installed Chromium web app shares the profile with its tabs (*untested*) | D3 |
| Be its own signer: Switch decision 24 lets any app be its identity's signer, so the wallet would hold the identity key in its origin (*inferred*) | Waits on #537 item 4 | D1 |

Also, the first release receives nothing while no page is open (D7).

## Decisions

### D1. Identity and the signer

**Context.** *Read*.

- The signer holds the identity key, approves apps, signs `identity.json`, and
  writes key bundles (`sw:spec/spec.md:76`). The identity key derives
  `switch_root`, so it reaches all the identity's sealed data, past and future
  (`:89`).
- Every other app runs on its own signing, bundle, and receive keys. Its
  approval (app certificate, the `identity.json` that lists it, a key bundle)
  rides on its Pubky grant as a Pubky auth "companion claim", not a Switch
  claim (`sw:spec/spec.md:104`, `:118`). The grant covers only the app's record,
  its private backup folder, and paths the user allows (`:157-164`); for
  Paykit, the Paykit Profile (`v02:specs/v0.2/spec.md:145-146`). This is the
  delegation v0.1 lacked.
- The approval crosses the Pubky HTTP auth relay (default
  `https://httprelay.pubky.app/inbox`) encrypted: an availability dependency,
  not a trust hole. The relay answers with credentialed CORS (*verified
  (probe)*); a self-hosted one needs CORS configured. What a signer shows
  about a browser app's requesting origin is *untested*.
- Paykit decision 19 makes "the wallet app" the signer for now, with one Rust
  signer core for both its platforms (`v02:specs/v0.2/spec.md:711`). Switch
  decision 24 lets any app be its identity's signer (`sw:spec/spec.md:1301`).
- On 2026-11-11 Pubky agreed that key generation, backup, and recovery live
  only in dedicated signers (Ring, Bitkit, Passport), and `pubky-app` removed
  in-browser key creation
  ([pubky-app#3142](https://github.com/pubky/pubky-app/pull/3142)) and
  root-key sign-in
  ([pubky-app#3201](https://github.com/pubky/pubky-app/pull/3201)). The
  agreement is recorded only in pubky-app#3142. Passport is a web
  signer that keeps the key in browser storage, and Bitkit is a wallet. So the
  agreement limits which apps hold keys; it does not ban browser keys or
  settle whether a web app may be a signer. #537 item 4 stays open.
- A signer can later hand the identity to a dedicated signer through the
  switch, with no deep link (`sw:spec/spec.md:1005-1007`). That does not take
  back a key the origin already exposed. Switch spec 18 still lists the
  hand-over as support Ring and wallet signers need (`:1271`).
- A signer needs a `SecretVault` in device-bound storage that is not backed
  up, a Homeserver session, and `Signer::run_due` at each start and on a
  schedule (`sw:docs/signer-host-guide.md:11-16`). Whether those runs need the
  identity key unlocked is not checked.
- `pubky-core-ffi` `mnemonic_to_secret_key` takes the first 32 bytes of the
  64-byte BIP39 seed, with an empty passphrase
  ([`src/keypair.rs:51-62`](https://github.com/pubky/pubky-core-ffi/blob/cecd67c161ba9d2a9ada1a131a2df9279fde882a/src/keypair.rs#L51-L62)
  at `cecd67c1`). Nothing fixes how a wallet derives the identity key or,
  under Paykit decision 18, its app keys (`v02:specs/v0.2/spec.md:710`;
  non-normative profile at `sw:spec/spec.md:95-96`).

**Options.**

1. **Approved by a signer elsewhere** (Ring, or a mobile wallet acting as
   signer). The browser never holds the identity key.
2. **Web app as signer.** It approves a separate Paykit App for itself and
   runs the SDK as that app, in the same origin, so origin compromise reaches
   the identity key. A signer's `SecretVault` is synchronous like the store
   (D3).

**Proposal.**

- The first release supports option 1 only. Its end-to-end test joins through
  a signer, including a mobile tab suspended during approval.
- Option 2 only if #537 item 4 says yes, with these costs stated:
  - Script in the origin can take the identity and every app on it, past and
    future; revoking apps does not take the key back
    (`sw:docs/signer-host-guide.md:241`). With no unlock step nothing limits
    exposure, and `run_due` may need the key unattended.
  - The signer's Homeserver session must read and write
    `/pub/pubky-switch/v1/` (which holds `identity.json`) and the signer
    backup, which a root session gives (`sw:docs/signer-host-guide.md:14`).
    So the origin holds a credential that can rewrite `identity.json`.
  - Sign-up also runs in the browser. It posts a root grant with a 5-minute
    lifetime, and its proof, to `/auth/grant/signup` (pubky 0.18.0 and
    0.16.2, `src/actors/signer/session.rs:29-30` sets the lifetime,
    `:274-279` builds the grant, `:303-306` gives it the root capability;
    `src/actors/auth/grant/grant_exchange.rs:60-87`). A party that can read
    Homeserver TLS traffic (D5) and withholds the request can redeem it once
    for a root bearer good for at most about 5 minutes. For a new account it
    must also forward the retry; for an existing one, withholding suffices.
- Recommendation to Pubky for a wallet that derives Pubky keys (identity or
  app keys) from its own seed. Not host guidance until Pubky decides; the
  first release (option 1, random app keys) does not need it.
  - Do not feed the wallet mnemonic to the first-32-bytes convention. A leaked
    identity key would disclose half of the seed behind the BIP32 master key
    (not a known break); the identity and wallet keys could not be replaced
    separately; a BIP39 passphrase would not protect the identity key.
  - Derive a 12-word child mnemonic with BIP85 at
    `m/83696968'/39'/0'/{words}'/{index}'`, index 0, unless Pubky decides
    otherwise. Ring and Passport import exactly 12 words, so a 24-word child
    could not be imported into them by phrase.
  - A wallet with only a raw 32-byte key gets neither; reusing that key
    exposes the spending key on a leak.
  - Costs: the wallet mnemonic gives a different identity in other Pubky
    apps, a host uses one derivation on all platforms, and an existing
    identity keeps its own.
- Each browser install is its own app, with random app keys by default. A host
  that derives app keys from a seed gives each install its own index and
  approval (`sw:spec/spec.md:96`). Partitions never share app keys (D3).

**Decision needed (Owner: Paykit).** Ship the first release with option 1
only, whatever #537 item 4 answers? Cost: a user whose only wallet is the
browser wallet needs Ring or a mobile wallet as signer before joining. If
item 4 says yes, option 2 comes later with the costs above.

**Decision needed (Owner: Pubky, with Paykit v0.2; #537 item 4; N3).** May a
web app be its identity's signer, and do `run_due` runs need the identity key
unlocked? Fallback: no; the first release supports option 1 only.

**Decision needed (Owner: Pubky; #537 "can wait").** A standard derivation of
the identity key and app keys from a wallet seed (scheme, path, word count,
index). Not on the browser critical path. Fallback: none. Until Pubky decides,
the package gives the BIP85 proposal only as Paykit's recommendation to
Pubky, not as host guidance, and warns hosts against the first-32-bytes
convention.

**Decision needed (Owner: Paykit v0.2, with Paykit; #537 item 6).** Confirm
that browser installs are separate apps, an exception to Paykit decision 18's
seed-derived keys that make devices one app. Each install then needs its own
approval, the cost decision 18 avoided. D3 explains why two partitions cannot
be one app.

### D2. Secret custody and threat model

**Context.** A browser host holds these secrets (*read*):

| Secret | Where | Notes |
| --- | --- | --- |
| Identity key | The signer's `SecretVault` | In a browser only for a web app as signer (D1). Gives everything, past and future |
| App state | Store namespace `client/<app_id>` (D3) | Signing and bundle secrets, every held receive key, scope keys, journal, message state (`app/STATE.md:263`, `:277`). A leaked signing key allows impersonation and reading new messages until revocation |
| Homeserver session | The host's Pubky client, for `HomeserverWriter` | A bearer for the app's record, backups, and Paykit Profile paths. The approving signer sets the grant's `exp`; neither the Switch spec nor Paykit does. Pubky's own signer code defaults to two years unless revoked (`DEFAULT_GRANT_LIFETIME_SECS`, pubky 0.18.0 `src/actors/auth/grant/constants.rs:2`, used at `src/actors/signer/auth.rs:273`; 0.16.2 uses it at `auth.rs:258`). Each restore mints a new bearer, so the session secret is a long-lived credential, not a short-lived bearer. A web signer also holds its session over `/pub/pubky-switch/v1/` and the signer backup |
| App backup (`PaykitBackup`) | Wherever the host keeps it | `PaykitBackup::to_bytes` returns plaintext that always holds the app's signing key (`v02:paykit-sdk/src/paykit.rs:168-200`), so a leaked copy can act as the app until it is revoked. The host encrypts it under a key the app or user holds, never one from a Shared Data Key. Switch spec 12.4 asks to keep it out of routine backups (automatic device or cloud backups) or encrypt it under a separate key the user holds (`sw:spec/spec.md:907`). Refresh it at each receive-key rotation. Keep a copy outside the origin's evictable storage, such as the app's private Homeserver folder |
| Pending approval | Host storage until the signer answers | The generated keys and the auth request secret. Persist them only if the tab may be lost before approval. The host documents how long it keeps them |
| Push subscription | The service worker | Its endpoint is a capability URL (D7) |

All of these are raw bytes in wasm memory while used: v0.2's Ed25519, X25519,
HPKE, and ChaCha20-Poly1305 run synchronously in Rust, and WebCrypto is
async. The Rust key types zeroize on drop
(`sw:crates/pubky-switch-core/src/keys.rs:4`), though the Switch spec still
lists this as not done (`sw:spec/spec.md:98`). Copies they do not own are not
wiped: the `Vec<u8>` from `PaykitBackup::to_bytes`, serialized store values,
and anything in JavaScript, where a binding object is freed only by its
destroy call or a late finalizer. Platform keystores remain production work
(`sw:spec/spec.md:98`).

At rest, the spike sealed IndexedDB rows with a non-extractable `CryptoKey` in
the same database (*verified (v0.1 spike)*). Whether that protects a copied
profile depends on the engine (*read*): Chromium and Firefox write the raw
key bytes into the profile; WebKit on Apple platforms wraps them with a
Keychain master key; WebKitGTK and WPE (so Playwright WebKit on Linux) use an
empty key. The Pubky client's `#[doc(hidden)]` proof-of-possession hook takes
a signing callback from the host, not a key (`delegated_client_signer`, pubky
0.18.0 `src/actors/auth/grant/builder.rs:141-151`; `DelegatedSignFn`
re-exported `#[doc(hidden)]` at `src/lib.rs:84-85`). The host can back that
callback with a non-extractable WebCrypto key, but pubky does not enforce it
(`src/pubky.rs:420-430` documents "a browser-held non-extractable key"; also
in 0.16.2; *read*). A non-extractable key stops script exporting the key, not
a copied profile; it is a host option.

**Options for protection at rest.**

1. Seal values with a data key wrapped by a non-extractable WebCrypto key in
   IndexedDB. The data key is raw in wasm memory while the store is open (the
   store is synchronous, D3; WebCrypto is async). Sealing uses an existing
   AEAD crate, not new crypto.
2. Seal with a key from a user secret (password, or passkey through WebAuthn
   PRF). Protects against profile theft; needs an unlock step; PRF support is
   uneven.
3. The host supplies the wrapping key or the whole store.
4. Run the SDK in a separate origin in an iframe: a separate partition and a
   second deployment, gaining nothing for a host whose seed lives in the page
   origin.

**Proposal.**

- State the threat model plainly. **Any script that runs in the origin is
  fully trusted**, including cross-site scripting, a malicious dependency, and
  an extension with page access. The SDK cannot defend against it. What it
  does defend against:

  | Attacker | Can | Cannot | Relies on |
  | --- | --- | --- | --- |
  | Script in the origin | Anything the SDK can do | Nothing is withheld | The host's CSP and supply chain |
  | Someone using the unlocked device or profile | Anything the SDK can do, with no unlock step | Nothing, unless the host adds an unlock step (option 2 or 3) | The host's unlock step |
  | A copied profile | Read sealed values in Chromium and Firefox under option 1 | Read them under option 2, option 3 with a key kept outside the profile, or option 1 on WebKit on Apple platforms | Engine key wrapping, or a user secret |
  | A phishing origin | Ask a signer to approve its own app with Paykit scopes | Use another app's keys | The signer showing the requested scopes and client; what it shows about the requesting origin is *untested* (D1) |
  | Network, or a mis-issued certificate | For Homeserver traffic: withhold, serve stale signed copies, and with the app's session delete the record or backups (D5) | Alter signed records or Switch traffic | Signatures |
  | The switch operator | As Switch spec 14 (`sw:spec/spec.md:1195-1196`) | As Switch spec 14 | Switch spec 14's protections; not specific to browsers |
  | Malicious counterparty | Send untrusted strings; choose the hosts the client contacts | Pass signature checks | The rendering rules below; the D5 address rule |
  | Other origins and partitions | Nothing | - | Browser isolation |

- Serve the wallet from its own origin, shared with no other app or user
  content: any same-origin page can call the owner context's worker.
- The package is part of the trusted base: publish it with npm provenance;
  the host serves the wasm and glue from its own origin, pinned by version
  and hash.
- Option 1 keeps plaintext out of the files and detects tampering. It stops a
  copied profile only where the engine wraps stored keys, and never stops
  code. Propose it as the browser minimum for Switch "Where keys live" and
  `StateStore` guarantee 5 (#537 item 1; limits in D3).
- Make the wrapping key replaceable (option 3), so a host that needs
  protection against profile theft in Chrome, Edge, or Firefox supplies its
  own. A key behind a user unlock step rules out unattended start-up (reload,
  browser restart, push wake-up with no page); panic recovery still works, as
  the page can post the `CryptoKey` to the new worker.
- The backup key must survive eviction, so it comes from outside the origin's
  storage: the user's password or passkey (option 2), the wallet seed through
  the host's derivation, or a key the host keeps elsewhere. The seed counts
  only if the user also holds it outside this browser (a written mnemonic or
  the wallet's own backup). A backup sealed only under the origin's
  non-extractable key cannot be restored after eviction; the package says so.
- The host seals the Homeserver session the same way, or keeps it in the
  binding's store. A signer's identity key stays behind the user's unlock step
  and never reaches the Paykit SDK.
- Require a CSP that blocks inline script and limits script sources (D12). It
  lowers the chance of injection but does not contain one: injected script
  can still exfiltrate by navigation and drive the worker.
- Every string from a counterparty-signed record or message is untrusted:
  Paykit Profile fields (`v02:specs/v0.2/spec.md:170`), app record labels,
  Payment Request and Allowance text, endpoint values, and error text. Render
  it as text, never HTML; allow only `https:` URLs. Fetch `image_uri` only
  when the user acts, since the fetch reveals the viewer's IP address and when
  they looked. The `Rail` checks endpoint values before paying. Strip
  bidirectional control characters, and show the payee's Pubky ID (or a short
  form) next to any counterparty-chosen name on every confirmation screen. A
  display name never identifies a payee on its own.
- Claim no zeroization for anything that crosses into JavaScript; hosts call
  destroy on secret-bearing handles such as a `PaykitBackup`.
- On suspected leak of a store, backup, or sealed row with its key, revoke the
  app (`sw:spec/spec.md:877`) and approve the device as a new app.
- "Remove from this browser" defaults to an immediate leave: delete the app's
  Homeserver record, call `leave`, then delete the store namespace, its
  sealing key, and the Homeserver session. Warn that counterparties may act on
  the record until their cached copies expire (`sw:spec/spec.md:898`). Switch
  spec 12.3 prefers a two-step leave that deletes keys only after the signer
  publishes the removal; a host may offer it only if it states that secrets
  stay in this browser until the next run after that. Offline, the host still
  deletes local state and tells the user to remove the app in the signer. A
  `PaykitBackup` kept elsewhere survives, and account erasure does not reach
  the device; say so. `Clear-Site-Data: "storage"` removes all the origin's
  storage, including any wallet seed there; send it only when nothing else in
  the origin must survive.
- A security review of hand-written browser code (store, owner helper,
  transport) gates real funds and the first stable release (rollout step 4).
  The v0.2 external audit's listed scope does not include it
  (`v02:specs/v0.2/spec.md:1001`).

**Decision needed (Owner: Paykit).** Is sealing with a data key wrapped by a
non-extractable WebCrypto key, raw in wasm memory while the store is open,
acceptable as the minimum? Is "same-origin script is trusted" an acceptable
written threat model for a payments SDK? Which backup key source does the
reference flow use: (a) a key derived from a wallet seed the user holds
outside the browser, (b) a passkey through WebAuthn PRF, or (c) a password?

**Decision needed (Owner: Pubky Switch; #537 item 1).** Is a browser an
acceptable platform under Switch spec 3.1, at a stated lower assurance?
Fallback: the package documents the lower assurance and ships as a
pre-release only.

### D3. The state store

**Context.** *Read*, except where marked.

- One app's state is one `StateStore` namespace, `client/<app_id>`, one record
  per key, never a blob rewritten whole (`app/STATE.md:3-4`); a signer uses
  `signer/<identity>` (`sw:crates/pubky-switch-signer/src/state.rs:38-41`).
  The trait is synchronous so Swift and Kotlin hosts avoid async callbacks
  (`store/mod.rs:4-6`), and `StoreHandle` cannot await an async store.
- An async store needs more than a `StoreHandle` change. Every protocol
  callback is synchronous and may read the host's store on the app's task
  (`app/protocol.rs:910-915`). The receive step reads through
  `ReceiveContext`, and `sends_to` and `provider_lost` through `LocalView`;
  each holds a raw `&dyn StateStore` (`app/context.rs:99-104`, `:214-219`).
  `may_send` gets a `LocalView` inside its `DeliveryView`
  (`app/send.rs:243-248`). Helpers such as `app/kept.rs:186`,
  `app/records/previous.rs:18`, and `app/context.rs:128` take a raw store too.
  The app builds the callbacks' views from `StoreHandle::store()`
  (`store/handle.rs:76`), which returns the raw synchronous store, for example
  at `app/send.rs:3260` and `app/records.rs:1486`. So app code, not only
  callbacks, reads the store outside `StoreHandle`. #537 item 3 names only the
  first two helpers; the `Protocol` callback contract must change too.
- A browser can encrypt at rest, but cannot promise exclusion from backups
  (guarantee 5; computer backups include the profile) or physical deletion
  (guarantee 6). The only durable synchronous storage that suits the store is
  an OPFS sync access handle in a dedicated worker: it opens asynchronously,
  then reads, writes, and flushes synchronously, with an exclusive lock per
  file and no transactions. `localStorage` does not fit (`Window` only,
  strings only, small quota, no transactions). So a page or service worker
  needs an async store.
- **Eviction.** Under storage pressure the browser deletes whole origins,
  least recently used first, unless storage is persistent. Safari deletes all
  script-writable storage after 7 days of use without interaction with the
  site (OPFS coverage undocumented); a Home Screen web app has its own counter,
  and WebKit does not expect its data deleted. Firefox 115+ deletes IndexedDB
  when a private window ends; Chrome and Safari are unconfirmed.
- **What eviction loses:** the app's keys, cursor, outbox, and journal; lasting
  effects of processed messages stay recoverable at the switch
  (`sw:spec/spec.md:933-934`). With random keys (the D1 default) and no
  backup, the app joins again as a new app (`:907`). With a backup it
  restores, asks whether the previous device is still the user's (`:908`),
  and makes no new attempts until reconcile has run (`:928`). After a restore
  from a backup that is not clean, it holds every open Payment Request and
  Allowance until the record ends it or the user confirms; Paykit open
  question 28 asks how to lift that hold without the user
  (`v02:specs/v0.2/spec.md:690`). Missing app metadata is
  `Refusal::NotJoined` (`sw:docs/app-host-guide.md:17`); whether
  `Paykit::load` surfaces it on an emptied store is *untested*.
- **Seed-derived keys** (Paykit decision 18). Restoring the seed brings back
  the signing and bundle keys without a new approval
  (`v02:specs/v0.2/spec.md:710`; `sw:spec/spec.md:95`); receive keys are
  random. A fresh join with those keys names the same App ID, and the signer
  renews its certificate unless that App ID is listed with other keys,
  removed, or revoked (`sw:spec/spec.md:118`); after revocation the app joins
  as a new app at the next index (`:96`). State needs a backup:
  `Paykit::restore` takes an authorized `PaykitBackup`
  (`v02:docs/paykit-host-guide.md:13-14`), or the cursor, outbox, and journal
  are lost. Whether `Paykit::join` accepts an approval that renews a listed
  App ID is *untested*.
- **Older backup.** Entries sealed to a receive key rotated after the backup
  cannot be opened; the app skips them and sends each sender a
  `resend_request` (`sw:spec/spec.md:631`). The backup carries unacknowledged
  unknown-delivery duties that `paykit-sdk` does not yet handle, so the host
  must record possible deliveries and keep restored work it cannot verify out
  of automatic delivery, including SDK calls that can flush it
  (`v02:specs/v0.2/spec.md:35`; N5).
- **Rollback** (a profile restored from a computer backup). Envelope keys and
  nonces are fresh random values (`sw:spec/spec.md:320-323`), and claims and
  Allowance records live at the switch. The journal and newest signed head
  roll back, so rollback detection anchors on an older head (`:745`). A host
  that knows its storage was restored or copied calls `Paykit::restore`
  (`v02:paykit-sdk/src/paykit.rs:703`). Effects in shared records are
  recognized by `msg_id` (`sw:spec/spec.md:619`); messages that left no record
  effect may be processed again (*inferred*). A malicious switch can serve old
  state to an app that has not seen a newer one (`:1196`); split-view
  detection is deferred (`:1246`). Not specific to browsers.
- **Several installs, one app.** Under Paykit decision 18, devices restored
  from one secret are one app that must keep its state consistent
  (`v02:specs/v0.2/spec.md:710`; `sw:spec/spec.md:100`, "One app per device is
  recommended"). Two partitions restored from one mnemonic would be one app
  that a browser cannot keep consistent, and "no duplicate payment" excludes
  an attempt carried out twice on two devices of one app
  (`v02:specs/v0.2/spec.md:608`). Not browser-specific; the double payment is
  *inferred*, not run. Hence D1's one app per install.
- **Mixed builds after a deploy** share one store. An older build must fail
  distinctly on a newer format; a rollback across a format bump locks out
  users the newer build wrote for; an open IndexedDB connection blocks an
  upgrade unless it handles `versionchange`.
- **WebKit lost connection.** Before Safari 26.5, IndexedDB could fail until
  reload after a network-process crash
  ([bug 309386](https://bugs.webkit.org/show_bug.cgi?id=309386)); the worker
  case was fixed later
  ([bug 314932](https://bugs.webkit.org/show_bug.cgi?id=314932)).
- *Verified (v0.1 spike)*: the spike's second store used default durability, a
  weak additional authenticated data (AAD), a racy key creation, and ignored
  `abort`, which shaped the requirements below.

**Options.**

1. The host implements the store to the Switch contract.
2. Paykit ships a reference store on OPFS in a dedicated worker, which fits
   the synchronous trait today.
3. The same on IndexedDB, once the Switch client adds an async store seam
   behind `StoreHandle`; this also allows a page or service worker host. An
   async store must keep each commit atomic when its future is dropped; with
   the synchronous store and `StoreHandle::inline` a drop cannot split a
   commit (*read*, not run).

**Proposal.**

- One store per app and partition; each partition is its own app (D1) and
  shares state only through the switch.
- Ship one replaceable reference store, because it is small and easy to get
  wrong: option 2 unless option 3's seam lands first.
- Any browser store meets these requirements:

  | Requirement | Browser form |
  | --- | --- |
  | Atomic (guarantee 1) | One IndexedDB `readwrite` transaction per `commit` and `commit_if`, or A/B slots or a log on OPFS. `commit_if` is never a read followed by a commit (`store/mod.rs:133`) |
  | Durable before `Ok` (2) | `durability: "strict"` on every IndexedDB transaction (no engine defaults to strict: Chrome 121 and later default to relaxed, Safari treats the default like relaxed, and Firefox's default is a weaker SQLite sync level than strict); `flush()` on OPFS |
  | Ordered (3) | Reads see every returned commit, across handles; scans in ascending byte order |
  | Exclusive (4) | D4 |
  | Encrypted (5) | AEAD per value with AAD binding store identity, App ID, store namespace, and key; atomic sealing-key creation |
  | Deletion (6) | `DeletePrefix` plus deleting the sealing key |
  | Sizes (7) | 16 MiB values and 32 MiB batches within quota |
  | Errors | Reject on `abort` as well as `error`. An OPFS `NoModificationAllowedError` from a second opener (Firefox and Safari have no `mode` option) maps to `InUse`. Map the rest as in [Host-visible errors](#host-visible-errors) |
  | Lifecycle | Handle `versionchange`, `blocked`, and `close`; version the format |

- The page calls `navigator.storage.persist()` after a user action, even for a
  worker store (`persist()` is Window-only). Firefox prompts; others decide
  silently; refusal is normal. Nothing documents that a grant stops Safari's
  7-day deletion, so it never replaces backups. Recommend Home Screen install
  on iOS.
- An empty store is ambiguous at start-up: a first install and an evicted one
  look alike, so `NotJoined` may never appear. Before a fresh join, the host
  asks whether this browser ran the wallet before and offers restore. A host
  whose own state is evicted with the origin offers the Paykit restore in its
  own recovery step. `identity.json` lists current App IDs, and a published
  app record may carry a display `label`, which can help the user choose.
- After a join without restore, the host asks the user to remove the lost app
  in the signer (Switch spec 12.3, "Reinstalling", `sw:spec/spec.md:903`).
  Until then payers may use its public endpoints until `expires_at` or
  `record_max_age` (90 days when absent; `v02:specs/v0.2/spec.md:160-161`), so
  a publishing browser app keeps `expires_at` short and its endpoints payable
  to wallet keys that survive eviction. Each such join costs a new App ID,
  kept for good, that counts toward the default of 32 apps until the lost one
  is removed (`v02:specs/v0.2/spec.md:681`), and removal is a revocation that
  leads to a rotation (`sw:spec/spec.md:898`, `:901`). So frequent eviction
  is an identity-wide cost, another reason for a backup outside the origin.
- Measure store cost up to a backup-sized value (11,185,390 bytes,
  `store/mod.rs:80-83`).

**Decision needed (Owner: Paykit).** Ship a reference store, on OPFS now or
IndexedDB after a seam? Assurance is decided in D2.

**Decision needed (Owner: Pubky Switch; #537 item 3).** Keep an async store
possible behind `StoreHandle`, and keep new store access behind it? Fallback:
OPFS in a dedicated worker, and no page or service worker host.

**Open note N4 (waiting on v0.2): eviction, rollback, and store formats.**
Undecided: whether a current App ID may join again without a backup or must
restore (seed-derived app keys only, not the D1 default); whether Private
Payment List reservations (Paykit spec 8.4.1) hold for a rolled-back store
that never ran restore; how the client and Paykit version their store
formats. Resolved by: a Paykit v0.2 or Pubky Switch answer. Blocks: the
rollout step 3 eviction and store-format tests.

**Open note N5 (waiting on v0.2): unknown deliveries after restore.**
Undecided: whether `paykit-sdk` takes over the `DeliveryUnknown` duty before
the browser release. Resolved by: SDK handling of `DeliveryUnknown`. Blocks:
the restore flow above and the rollout step 3 backup-and-restore test.

### D4. Tabs and store ownership

**Context.** *Read*, except where marked.

- v0.2 requires one writer per store namespace: the host serializes writers
  across threads, extensions, and processes, and `App` uses ordinary commits
  (`sw:docs/app-host-guide.md:9`; `v02:docs/paykit-host-guide.md:7`). Two
  unserialized runtimes break guarantee 4, with an undefined result.
- `acquire` grants a `StoreLease`, the client's token of store ownership (not
  the v0.1 intake lease); `commit_if` from a holder that lost ownership
  returns `NotOwner` and applies nothing (`store/mod.rs:116-143`). A store
  lease never expires and has no explicit unlock; it ends when its last
  holder drops it or the process dies, and "a clock or a missing heartbeat
  does not prove that death" (`store/ownership.rs:34-45`). No `App` code calls
  `acquire` or `commit_if` at this pin, so nothing fences a resumed old owner
  context today, and preemption is not yet safe.
- A Web Lock is the browser form of a store lease, released when its document
  or worker ends, but engines deviate. Chrome's Energy Saver (desktop, Chrome
  133) does not freeze a page that holds a Web Lock: safe, but it can block
  other tabs. Other freeze paths and Chrome on Android are undocumented.
  WebKit drops a page's locks when it suspends it or caches it for
  back/forward navigation, so the page can resume with its state but without
  the lock while a second owner context runs. Firefox has no freeze; whether
  Firefox and Chromium cache a lock-holding page is unsettled. A dedicated
  worker freezes with its page, and `steal: true` does not stop the old
  holder's code.
- So "a frozen tab keeps its locks" holds only in part. The hazards are a
  live but throttled owner context, a resumed WebKit page that believes it
  owns, and Chrome on Android, whose rules are unknown.
- *Verified (v0.1 spike)*: Web Lock election with calls relayed over
  `BroadcastChannel` worked; takeover was tested only after a close. The v0.1
  intake lease blocked a restarted context for about 116 seconds. An owner
  context that stops without dying is the same hazard for a `StoreLease`,
  open until #537 item 3 is answered.
- Bearer replacement between tabs is gone only for Switch calls, which carry no
  session (`sw:spec/spec.md:357`). Two contexts that restore one Pubky session
  still replace each other's `HomeserverWriter` bearer (*verified (v0.1 spike)*,
  pubky 0.16.2). pubky 0.18.0 resends a replayable request once after a 401 and
  exchanges the grant only with 150 seconds or less left, so an idle tab does
  not interfere (*read*). Paykit v0.2 has no `pubky` dependency; the host's
  client version decides.
- Pubky's grant-session coordinator can share one bearer across tabs under a
  lock, but is still `#[doc(hidden)]` in 0.18.0
  (`src/actors/auth/grant/shared_session.rs:36-50`). Low-priority Pubky ask:
  make it and the delegated proof-of-possession signer (D2) stable.

**Options.**

1. Document the rule; enforcement is the host's.
2. One owner context per store namespace and partition, with an optional
   TypeScript owner helper.
3. A SharedWorker per origin as the owner context (versions in D12). It
   cannot hold an OPFS sync handle, and Android may end it under memory
   pressure. *Untested*.
4. A separate app per tab, each approved. Not viable: apps accumulate in
   `identity.json`, 32 per identity by default (`v02:specs/v0.2/spec.md:681`).

**Proposal.**

- At most one live owner context per store namespace and partition. Back the
  store lease with a Web Lock, plus the OPFS file's exclusive handle if D3
  picks OPFS. Map `AcquireOutcome::Busy` and `StoreError::InUse` to a distinct
  host code ([Host-visible errors](#host-visible-errors)).
- Ship option 2's helper as optional, with these rules. The owner context
  stops its runtime and waits for in-flight store calls and sends before it
  releases the lock. It keeps the lock while the page is hidden: releasing
  gives up Chrome's freeze exemption, and `freeze` fires only in Chromium. A
  page restored from the back/forward cache takes the lock again before
  acting as owner context. A relayed call whose owner context goes away has an
  uncertain outcome: report it and never resend a state-changing call, as
  v0.2 never re-queues a given-up delivery (`v02:specs/v0.2/spec.md:35`).
  Hidden tabs' timers may fire a minute late, so liveness timeouts are
  advisory. Version the helper's messages across deploys.
- Secrets never cross `BroadcastChannel`: join requests (they carry app keys),
  backup, restore, and push tokens run in the owner context only. A
  `MessagePort` cannot travel over `BroadcastChannel`, so a private port needs
  a broker (service worker or SharedWorker) or a window reference; it avoids
  broadcasting secrets but adds no confidentiality against same-origin script.
- Ask the Switch client to fence the app with `commit_if`, or to let a host
  take store ownership away from a stopped owner context safely (#537 item 3),
  so a resumed WebKit page cannot write.
- Only the owner context restores the Homeserver session.

**Decision needed (Owner: Paykit).** Is "one owner context per store
namespace and partition, with option 2's optional helper" the first-release
shape, with a SharedWorker owner later (rollout step 5)?

**Decision needed (Owner: Pubky Switch; #537 item 3).** Confirm that the
client handles losing ownership safely, and allow a host-initiated release of
store ownership. Fallback: the binding's store fences itself. When the binding
takes the Web Lock and opens the store, it raises an owner epoch kept in the
store. Every `commit` and `commit_if` checks that epoch in the same
transaction (IndexedDB) or under the file's exclusive handle (OPFS). A stale
`commit` fails with `StoreError::InUse` and a stale `commit_if` returns
`CommitOutcome::NotOwner`, and neither applies anything. A page restored from
the back/forward cache takes the lock again and raises the epoch before it
writes.

### D5. Transport trust and CORS

**Context.** *Read*, except where marked.

- **Homeserver traffic** (reading `identity.json`, app records, and endpoints;
  writing the own record and backups) goes through the host's Pubky client.
  With no DHT access, a browser finds the Homeserver through pkarr HTTP relays
  and uses an HTTPS record naming an ICANN domain. It trusts WebPKI (the
  public certificate authorities browsers trust), never PubkyTLS (TLS
  authenticated by the Homeserver's Pubky key). Native prefers PubkyTLS but
  falls back to the ICANN domain after a 1.5-second TCP probe, so an on-path
  party or a blocked port 6287 forces it onto WebPKI too; pubky 0.16.2 and
  0.18.0 have no strict mode.
- Anyone with a valid certificate for the domain can read and change that
  traffic: a mis-issuing certificate authority, a domain or DNS takeover, the
  TLS-terminating proxy, or a root installed on the device (enterprise
  inspection, antivirus). Browsers enforce Certificate Transparency only for
  public roots. Such a party cannot substitute v0.2 Payment Endpoints, which
  sit in the app's signed record with a signed `expires_at`
  (`v02:specs/v0.2/spec.md:160-166`). Like the Homeserver operator
  (`sw:spec/spec.md:1198`), it can withhold or serve stale signed copies, and
  with the app's session delete the record or backups, never `identity.json`
  (`:164`).
- pkarr verifies signatures in wasm. A relay can withhold, serve an older
  record of any age (pkarr bounds no packet's signed age), and see lookups;
  both default relays are Pubky's. `homeserver.pubky.app` and the default
  relays speak HTTP/1.1 only, and browsers open about 6 HTTP/1.1 connections
  per host. The Switch spec asks for HTTP/2 and a persistent pkarr cache in
  the Pubky client (`sw:spec/spec.md:1273`).
- A browser reaches a Homeserver only with an ICANN domain, a public
  certificate, and CORS kept through any proxy (0.16.2 and 0.18.0 set no
  preflight max age). A payee on an unreachable Homeserver cannot be paid
  through public endpoints from a browser.
- **Switch traffic** goes to the switch origin named in a signed
  `identity.json`: the identity's own and each recipient's
  (`sw:spec/wire-format.md:1615`). Calls are app-signed with a timestamp, and
  responses are signed and bound to the exact request
  (`sw:spec/spec.md:357-368`). A network attacker can see TLS metadata and
  drop traffic, not read, alter, or replay it (`:1199`), so WebPKI trust
  changes nothing here.
- **CORS.** Neither Switch spec mentions CORS, and the service has none (no
  `cors` or `access-control` in the repository at the pin). `Switch-*`
  request headers force a preflight on every call, cached 5 seconds unless
  the server sets `Access-Control-Max-Age`. The client must read
  `Switch-Head`, `Switch-Server-Time`, and `Switch-Service-Signature`, hidden
  unless exposed (#537 item 2). Cold preflights add a round trip to the
  "about one round trip per action" target (`v02:specs/v0.2/spec.md:57`); not
  measured.
- **Address check.** Switch origins need every resolved address checked as
  globally reachable and the connection made to it
  (`sw:spec/wire-format.md:1593`). `fetch` cannot do this (#537 item 1), and
  counterparty-signed records choose the host. The same rule for relay URLs
  binds the switch when it sends webhooks (`sw:spec/spec.md:712`), not the
  browser.
- An unreachable counterparty looks like an outage: browsers hide why `fetch`
  failed, and the host sees `PaykitError::Homeserver` or
  `PublicAppFailureReason::Homeserver`, both with retry advice
  (`v02:docs/paykit-host-guide.md:251`, `:260`), so it may retry forever.
- The public `identity.json` read is rate-limited per IPv4 address or IPv6 /64
  (`sw:spec/spec.md:546`), so users behind one NAT share it (*inferred*).

**Proposal.**

- Accept WebPKI for Homeserver traffic, and document in the package README
  what it exposes and the Homeserver requirements. The pkarr relay list is the
  host's Pubky client setting, not a Paykit field.
- The browser `Transport` and `HomeserverReader` use `credentials: "omit"`,
  `redirect: "manual"`, and HTTPS only, and read bodies as a stream within the
  reference cap of 32 MiB (`transport/http.rs:59-61`, `:71`). pkarr
  lookups bypass the HTTP cache (D6).
- Report "unreachable from a browser" as a distinct reason where the binding
  can tell ([Host-visible errors](#host-visible-errors)).
- Ask the Pubky Switch maintainers for normative CORS on every service: answer
  preflights for any origin, allow the `Switch-*` request headers, expose the
  `Switch-*` response headers and `Retry-After`, and set
  `Access-Control-Max-Age`. Ask them too for a browser address rule: `https`,
  port 443, no redirects, and the browser's own private-network protections,
  which vary by engine.

**Decision needed (Owner: Paykit).** Ship a browser SDK on WebPKI trust for
Homeserver traffic? The alternative is no browser SDK; the residual risk is
withholding, stale signed copies, and deletion with the app's session.

**Decision needed (Owner: Pubky; not yet filed).** Is WebPKI acceptable for
browser Homeserver traffic? Fallback: document the exposure.

**Decision needed (Owner: Pubky Switch; #537 items 1 and 2).** Normative
CORS, a browser address rule, and when deployed switches serve CORS. No
fallback: without CORS a browser cannot call a switch, so rollout step 4 waits
on a production switch that serves it (N6).

### D6. Fresh reads and HTTP caching

**Context.**

- Before paying, an app revalidates its own `identity.json` unless it did so
  in the last 60 seconds; at dispatch its age, "measured from the start of the
  Homeserver request that supplied it", must be under 60 seconds
  (`v02:specs/v0.2/spec.md:169`; Paykit open question 12 at `:684`). Payee
  `identity.json`, app records, and endpoints come through the same reader.
- Homeserver `/pub` reads send `Cache-Control: private, must-revalidate` with
  `Last-Modified` and no `max-age`. Browsers may treat them as fresh for a
  heuristic time (*read* from engine source): Firefox 10% of the age since
  `Last-Modified`, capped at one week; WebKit 10% with no cap, ignoring
  `must-revalidate` for this; Chromium skips the heuristic when
  `must-revalidate` is present. Responses without `Last-Modified` (directory
  listings) and 404s are not served stale.
- pkarr: `ResolvePolicy::NetworkOnly` asks every relay, but Homeserver lookups
  use `CacheFirst`, and relays send `Cache-Control: public, max-age=<TTL>`
  (3600 seconds in production, *verified (probe)*), so the browser cache
  answers after a reload.
- *Verified (v0.1 spike)*: Firefox served the old value right after an update
  and the new value 5 seconds later; `cache: "no-store"` fixed it. So a cached
  copy can pass the 60-second check without reaching the Homeserver
  (*inferred* on v0.2). Counterparty Homeservers are not the host's to fix, so
  a client cache mode is the required layer.
- Playwright WebKit passed the same run, but that says nothing about Safari's
  cache (*read*): the spike drivers use `browser.newContext()`, which in
  Playwright WebKit is an ephemeral session with no HTTP disk cache.
- The Switch's public `identity.json` read is
  `public, max-age=60, must-revalidate` by design
  (`sw:spec/wire-format.md:2305`). Authenticated answers verify only for the
  exact request (`:1650`) and carry no validators.
- A host service worker with a fetch handler sits in the path of the page and
  the dedicated workers it controls. The Cache API ignores HTTP caching
  headers and the request's cache mode, so a runtime-caching rule for these
  URLs defeats `no-store`. A page can see that a service worker controls it
  (`navigator.serviceWorker.controller`), but not how it caches. In a
  dedicated worker, where the first release runs (D9), Chrome and Edge do not
  expose `navigator.serviceWorker`, so the package cannot see even that
  unless the page tells it.

**Proposal.**

- The browser `HomeserverReader` and the binding's pkarr lookups send every
  request with `cache: "no-store"`, and the first pkarr lookup after a load is
  network-only. The rule lives in binding code, so it also covers Homeservers
  that never add `no-cache`, and it does not replace the global `fetch`. A
  reader built on a Pubky client needs a per-request cache mode there. Hosts
  never route Homeserver, relay, or switch URLs through a caching service
  worker handler.
- Ask Pubky for `Cache-Control: no-cache` on `/pub` reads as defense in depth.
- Treat this as a first-release blocker. Test it with a persistent profile and
  the writer in another profile or device.

**Decision needed (Owner: Paykit).** Confirm `no-store` on Homeserver reads as
a first-release blocker.

**Decision needed (Owner: Pubky; not yet filed).** Add `no-cache` to
Homeserver `/pub` reads? Should a browser client resolve with `NetworkOnly` on
the first lookup after a load? Fallback: the client rule alone.

### D7. Receiving and push

**Context.** *Read*, except where marked.

- v0.2 receives by long-polling the feed on the identity's own switch,
  `Paykit::receive(wait)` (`v02:paykit-sdk/src/paykit.rs:928`), with `wait` at
  most 25 seconds (`sw:spec/wire-format.md:1921`). The cost does not grow with
  the number of contacts: one request every 25 seconds in the foreground, none
  in the background. A new message shows in under 1 second
  (`v02:specs/v0.2/spec.md:623`, `:630`).
- Switch spec 4.3 has a running app subscribe to the Homeserver
  `/events-stream` for its own and its counterparties' `identity.json`
  (`sw:spec/spec.md:292`). Neither the Switch client nor `paykit-sdk`
  implements it at this pin. The client has no idea of a watched document: it
  applies the fixed 10-minute cache (`DOC_CACHE_MS`, `app/identity.rs:34`) to
  every copy (`app/feed.rs:3282`, `app/state.rs:550`), so every document keeps
  the 10-minute re-fetch rule. A host can open the stream today (filter
  `/pub/pubky-switch/v1/identity.json`) and re-fetch sooner when an event
  names a document: `App::refresh_identity` for its own document, and
  `App::identity_of(pubky, true)` (`app/directory.rs:472`) for a
  counterparty's, both through `Paykit::app_mut`
  (`v02:paykit-sdk/src/paykit.rs:779`). That only adds re-fetches; it cannot
  lift the 10-minute rule. Who implements the stream long term, and whether
  one request can watch several users, is open note N7.
- *Verified (v0.1 spike)*: Firefox resolves the event-stream request only at
  the first bytes, and the Homeserver sends its first keep-alive after 15
  seconds. The Switch spec already asks Pubky for event-stream keep-alives
  (`sw:spec/spec.md:1274`).
- Background receive is push: the switch calls the vendor's relay webhook, and
  the relay sends APNs or FCM (`sw:spec/spec.md:80`, `:679-725`). Registration
  is generic, and push is optional per app (`:1285`). Web Push needs a relay
  that speaks it (with VAPID) and a browser delivery rule, but no service
  change. None exists.
- Push reaches a browser only in a service worker. iOS and iPadOS deliver Web
  Push only to Home Screen web apps, from 16.4, after a user gesture; Chrome
  and Edge require `userVisibleOnly: true`; Chromium cuts a push event at 90
  seconds; Background Sync is Chromium only. A push-woken worker needs the
  signing and receive keys, like the iOS push extension
  (`sw:spec/spec.md:691-700`), which a key behind a user unlock step rules out
  (D2).
- So with no open page a browser wallet receives nothing, shows no Payment
  Request, and makes no automatic acceptance until the user reopens it.

**Proposal.**

- First release: foreground long-poll only, in the dedicated worker while a
  page is open, using the client's `feed_wait` and `WaitPace`
  (`sw:docs/app-host-guide.md:105`; `feed_wait` changes nothing even when
  cancelled). The package states what is missed while closed.
- No event stream in the first release, so the 10-minute re-fetch rule
  applies. The binding adds the stream once N7 names an owner.
- Web Push needs a service worker, so it follows the async store seam and the
  service worker run (D9, rollout step 5). Add the Firefox first-byte delay to
  the Switch spec's existing keep-alive ask to Pubky.

**Decision needed (Owner: Paykit).** Is foreground-only receive, with no event
stream, acceptable for the first release, with Web Push later?

**Open note N7 (waiting on v0.2): the Homeserver event stream.** Undecided:
who implements it (the Switch client, `paykit-sdk`, or the host), and whether
one request can watch several users. Resolved by: an owner named in the SDK
surface (N1). Blocks: the binding's event stream.

**Open note N8 (waiting on v0.2): Web Push.** Undecided: a Web Push relay, its
owner, and a browser delivery rule. Resolved by: a Web Push relay with a named
owner. Blocks: Web Push and its push measurements in the service worker run
(rollout step 5).

### D8. Clocks, timers, and deadlines

**Context.**

- **Run-time traps.** `Instant::now()` and `SystemTime::now()` trap on wasm32
  (std behavior verified by a standalone probe; v0.2 call sites *inferred*):
  - `paykit-sdk` calls `Instant::now()` before sending every attempt batch and
    before an idempotent repeat's claim read
    (`v02:paykit-sdk/src/pay.rs:2576`, `:3884`), so every payment attempt
    would trap (#537 "cheaper now").
  - The Switch client's `FirstSend::new` (`transport/mod.rs:160`), reached
    when a batch has a first-send deadline, as Paykit's offline public payment
    does (`v02:paykit-sdk/src/pay/offline.rs:279`).
  - `SystemClock` (`clock.rs:71`).
- **CI must run code.** At `7aa75fe4`, `paykit-lib` and `paykit-sdk` pass
  `cargo check` for wasm32 with build edits only
  ([What was checked on v0.2](#what-was-checked-on-v02)), yet the payment path
  would trap (*inferred*: the std probe traps, and `paykit-sdk` itself did not
  run). So a `cargo check` job would not catch this; the CI job must run code
  in a browser (Groundwork, CI row).
- **Clocks.** `Clock` is wall time, and `Date.now()` can be set back. A
  browser's monotonic source, `performance.now()`, behaves differently across
  sleep by engine. v0.2 discards an identity observation when the clock moves
  back (`v02:specs/v0.2/spec.md:169`; `IdentityRefresh::ClockMovedBack`,
  `sw:docs/app-host-guide.md:21`).
- **First handoff.** `Transport::send_before` defaults to
  `FirstSendUnavailable`, leaving the request unsent
  (`transport/mod.rs:223-229`). Paykit needs it only to pay a manually
  selected public endpoint while the payer's switch is down. Whether a check
  just before `fetch()` meets "the transport's last cancellable handoff"
  (`sw:spec/spec.md:740`) is undefined.
- **Timers.** The `Timer` bounds the 2-second payee lookup grace
  (`v02:paykit-sdk/src/destination.rs:72`). Hidden pages throttle timers: once
  per second in Chrome and Firefox desktop; once per minute under Chrome's
  intensive throttling (a page hidden 5 minutes and silent 30 seconds, chained
  timers); once per 15 minutes in Firefox for Android. Dedicated workers and
  Safari are unknown. A late timer delays but is not unsafe: payments stop
  when the 60-second check lapses.
- **Deadlines.** Transport timeouts are the caller's (`v02:AGENTS.md:57`). The
  client tags each request `Ordinary`, `Bulk`, or `LongPoll { wait_s }`
  (`transport/mod.rs:60-84`); the reference transport gives them 10 seconds,
  40 seconds, and the wait plus 40 seconds, with a 10-second connect timeout
  (`transport/http.rs:48-73`). `fetch` has no separate connect timeout. The
  10-minute pre-dispatch budget assumes those bounds
  (`v02:paykit-sdk/src/paykit.rs:37-45`), and `Paykit` calls take
  `&mut self`, so one held call blocks the handle. A deadline must not cut a
  stream: the feed long-poll answers with a whole feed page, but the event
  stream's deadline ends at the response headers.
- A reader built on the Pubky client (D10 option 3) gets a deadline from
  Pubky only in a release that ships
  [pubky-homeserver#725](https://github.com/pubky/pubky-homeserver/pull/725)
  (merged 2026-11-10, not yet released; pubky 0.18.0 is the latest release).
  #725 also aborts event streams and relay long-polls, which breaks the
  event-stream rule above.
- An aborted call that reached `Rail::pay` has an uncertain outcome; only
  `RailError::Refused` means nothing was submitted
  (`v02:paykit-sdk/src/rail.rs:492-499`). The host resolves it through
  `Rail::payment` and the journal.
- *Verified (v0.1 spike)*: dropping the Rust future closed the connection.

**Proposal.**

- Take monotonic time from the host (or `web-time`) for the `pay.rs` calls and
  `FirstSend`; never `SystemClock` on wasm. The pre-dispatch budget counts on
  both the wall clock and the monotonic clock
  (`v02:paykit-sdk/src/pay.rs:2572-2576`), so where an engine pauses
  `performance.now()` during sleep, the wall-clock count bounds the budget
  (*inferred*). The release clock review confirms this per engine.
- The binding's `Transport` applies the `TimeoutClass` deadlines with
  `AbortSignal.timeout`, matching the reference defaults; the host may raise
  them. The `HomeserverReader` has its own deadline; the event stream's ends
  at the headers.
- Prefer per-request deadlines to aborting whole SDK calls: a request that
  fails inside the SDK lets cleanup run, and a dropped call may skip it.
  Record which Pubky release ships #725, and whether it exempts streams.
- No `send_before` in the first release, so the offline public-endpoint
  payment is unavailable, and the package says so.
- Test: drop every exported future at each await point on wasm and check that
  state reloads consistently. Review the browser clock before release.

**Decision needed (Owner: Paykit).** Do browser deadlines match the reference
defaults (10 seconds, 40 seconds, and the wait plus 40 seconds), with host
override? Is leaving out the offline payment path acceptable?

**Decision needed (Owner: Pubky Switch; `FirstSend` in #537 "can wait"; the
handoff in #537 item 1).** Take `FirstSend`'s monotonic time from the host, and
define "last cancellable handoff" over `fetch`? Fallback: the Paykit fix alone
and no `send_before`.

### D9. Where the SDK runs

**Context.**

- *Verified (v0.1 spike)*: the SDK ran on the main thread and in a dedicated
  worker; replacing a worker after a panic took 36 to 118 ms. *Read*: under
  `panic=abort` a panic leaves an async export's promise pending and makes a
  sync export throw. The instance can keep answering other calls, writes
  included, with destructors not run, so the host must replace it. A panic
  hook does not run on allocation failure. In ubrn
  (uniffi-bindgen-react-native), the `wasm2` mode's `open()` installs its own
  panic hook, which only logs to the console. Panic text is unredacted.
- **Loading in a service worker** (*read*): dynamic `import()` is not allowed.
  A classic service worker has no `import.meta` or static `import`, and
  `importScripts()` loads only scripts fetched at install. A module service
  worker (Chrome 91, Firefox 147, Safari 15) has no top-level `await`.
- **Service worker lifetime** (*read*; Firefox from its source at
  `615217b20e60e956769e7876514f920c339e9a76`:
  `modules/libpref/init/all.js:119-124`;
  `dom/serviceworkers/ServiceWorkerPrivate.cpp:1674-1712`, `:1732-1782`):

  | Limit | Chromium | Firefox | WebKit |
  | --- | --- | --- | --- |
  | Idle stop | 30 s | 30 s after the last event | 10 s after the last page closes; 2 s after an event with no page; at once under memory pressure |
  | Event cut | 5 min (push: 90 s, D7) | 30 s more after the idle stop (about 60 s after the last event); a new event restarts the idle timer | unknown |

  Spawned tasks must sit in `event.waitUntil`. A service worker cannot create
  a dedicated worker, and Chrome hides `navigator.serviceWorker` in dedicated
  workers, but the page can create a `MessageChannel` and give one port to
  each.
- No API exists for callers outside the SDK's context: generated object
  handles sit under a Symbol key that structured clone drops.

**Proposal.**

- First release: a dedicated worker only (required for the OPFS store). The
  main thread follows the async store (rollout step 5). Install a
  host-reporting panic hook after the binding's `open()`; the host replaces
  the worker. Apply the bindings' redaction rule to panic text.
- An interrupted call is crash recovery: the next owner context loads the
  store, and uncertain payments resolve through the journal and
  `Rail::payment` (D8).
- The service worker is unsupported until the async store seam (D3) lands and
  its own test run passes (rollout step 5); that run also covers push (D7).
  Until then a host whose engine lives in a service worker runs the SDK in a
  page-owned dedicated worker and relays over a `MessageChannel`. `Rail` then
  runs in the dedicated worker: its six synchronous methods answer from data
  the engine has copied there, and only `pay`, `payment`, and `verify` go to
  the service worker over the port (D10). Paykit checks this relay shape with
  the first integrator in rollout step 4. If it does not work, the service
  worker run waits on #537 item 3's async seam; it cannot move earlier.
- Define a page-to-worker message API sized to the `Paykit` calls a wallet UI
  makes (N1).
- The wasm loader accepts a URL, bytes, a `Response`, or a compiled module,
  never relies on `import.meta.url`, and falls back to bytes when the server
  does not send `Content-Type: application/wasm`.

**Decision needed (Owner: Paykit).** Is "dedicated worker, service worker
later" acceptable for the first release, given the first integrator's design?

### D10. Host interfaces and Send

**Context.** *Verified (v0.2 build)* for the shapes; *read* otherwise.

- Synchronous host traits can hold JavaScript handles. Each async method needs
  a small shim (`spawn_local` plus a `oneshot` receiver, no `unsafe`), not a
  redesign ([What was checked on v0.2](#what-was-checked-on-v02)).
- A `MaybeSend` alias (`Send` on native, nothing on wasm) would remove the
  shim. It belongs in the Switch crates, where `BoxFuture` lives;
  `public_futures_are_send` (`v02:paykit-sdk/src/paykit.rs:1239`), which
  asserts every public future is `Send`, would stay native-only. #537 lists it
  as "cheaper now".
- `Rail` has six synchronous methods (`declaration`, `select_endpoints`,
  `endpoint_identifiers`, `transfer`, `fraction_digits`, `settlement_id`;
  `v02:paykit-sdk/src/rail.rs:395-527`) that must answer from data in memory.
  ubrn's worker package fails on any synchronous foreign method (D11).
- A Rust `HomeserverReader` needs pkarr resolution on wasm, through the pkarr
  crate over `fetch` or the host's Pubky client (option 3). *Untested*.

**Options.**

1. Implement every contract except `Rail` (and later `PushRelay`) in Rust
   inside the binding, as in the Browser plan column of
   [Host contracts](#host-contracts-in-a-browser).
2. Expose every interface to JavaScript as a foreign trait.
3. Option 1, with `HomeserverReader` and `HomeserverWriter` backed by the
   host's Pubky JavaScript client, so the host runs one Pubky client.

**Proposal.** Option 1, with option 3 as a host choice. The store and
transport stay out of JavaScript, the foreign surface is `Rail`, and Paykit
owns the cache and deadline rules. Use the channel shim now; adopt `MaybeSend`
if it lands, and then add `compile_error!` for wasm with `atomics`. Document
that public futures are not `Send` on wasm. Decide before the v0.2 bindings
workstream fixes the FFI surface.

**Decision needed (Owner: Paykit).** Which host interfaces does the binding
implement in Rust, and which cross to JavaScript?

**Decision needed (Owner: Pubky Switch; #537 "cheaper now").** Add
`MaybeSend` before the freeze? Fallback: the channel shim.

### D11. Toolchain and package

**Context.** *Read*.

- v0.2 bindings come "after the Rust crates" (`v02:THESAURUS.md:37-38`).
  *Verified (v0.1 spike)*: on master's pins (UniFFI 0.29.4,
  `m:paykit-ffi/Cargo.toml:24`; ubrn 0.29.3-1,
  `m:paykit-react-native/package.json:125`) with `wasm-bindgen` 0.2.115, an
  async callback that returns a value crashes. With a post-generation patch to
  the generated shim crate, the ubrn-generated TypeScript passed its 21-step
  run in Chrome, page and worker; Firefox and WebKit did not run it. *Read*:
  ubrn 0.31 is said to fix the crash (0.31.0-3); the spike did not run it.
- ubrn 0.31.0-5 added `wasm2`: no generated Rust crate, the interface read
  from the `.wasm`, and the module taken as a URL, bytes, a `Response`, or a
  compiled module. It still needs the exact `wasm-bindgen` CLI version, a
  `_bg.js` glue file shipped with the `.wasm`, the `uniffi-runtime-wasm`
  crate, and UniFFI's `wasm-unstable-single-threaded` feature, and it changed
  incompatibly in 0.31.0-7. It does not strip or optimize the module, and
  `@ubjs/wasm` takes a caret pre-release peer range on `@ubjs/core`. Under a
  strict CSP its `new Function` probe falls back to an interpreter and raises
  only a violation report. ubrn's worker package makes every web function a
  Promise and fails on any synchronous foreign method.
- ubrn 0.31 pins UniFFI `=0.31`, so any 0.31.x. Paykit should set its own
  floor at 0.31.2. UniFFI 0.31.2 fixed a bug in its Kotlin (JNA) generator:
  direct-mapped `u8`/`u16` returns broke checksum checks on Android ARM32,
  which master's Android build ships (armeabi-v7a,
  `m:paykit-ffi/build_android.sh:156`). The fix is not in ubrn or React
  Native, and helps only Kotlin from UniFFI's generator. Master builds Kotlin
  with a Gobley fork on UniFFI 0.29 (`:16-18`), so a Gobley port needs the
  same fix; Gobley 0.30 and 0.31 support exists only as open pull requests,
  and one FFI crate has one UniFFI version. Ways out: port Gobley to UniFFI
  0.31, use another Kotlin generator, or split the FFI crates.
- The React Native package has `react` and `react-native` peers and no browser
  entry. v0.1 size, measured once: 5.36 MB raw, 2.03 MB gzip, plus 55 KB gzip
  of glue; v0.2's tree is smaller (no `pubky`, no `reqwest`). Wasm memory
  never shrinks.
- Master's npm workflow takes `vX.Y.Z` or `vX.Y.Z-rcN` tags matching
  `package.json` and sends only pre-releases to `next`
  (`m:.github/workflows/npm-publish.yml:41`, `:160-166`, `:195-199`), so a
  browser package on that train would go to `latest` on the next final tag.
  npm is targeting January 2027 to stop bypass-2FA granular access tokens
  from publishing directly
  ([2026-07-31 changelog](https://github.blog/changelog/2026-07-31-restricting-npm-bypass-2fa-granular-access-tokens)).
  After that, automation publishes through trusted publishing (OIDC), or a
  stage-only token stages a version for a maintainer to approve with 2FA
  ([2026-09-18 changelog](https://github.blog/changelog/2026-09-18-stage-only-npm-tokens-for-safer-automation)).
  This affects the first-publish route.

**Proposal.**

- Choose the generator with the v0.2 bindings workstream, with the browser in
  mind: ubrn `wasm2`, with its older web mode as fallback if a `wasm2` probe
  with v0.2 (on a throwaway branch) fails. Not 0.29, and no hand-written
  facade. A toolchain counts as verified once Swift and Kotlin libraries load
  at run time, not just compile.
- Publish a separate npm package. The two-package cost falls on the first
  integrator's separate SDK library, not its web app.
- Add a CI check listing exports that differ between web and React Native.
  Use a target condition, not a cargo feature, for browser-only exports: v0.2
  CI runs `--all-features` (`v02:.github/workflows/ci.yml:42`).
- Re-measure size on v0.2 with a committed build profile. Decide the package
  name, scope, version line, dist-tag, and first-publish route before step 3.

**Decision needed (Owner: Paykit).** Separate package, or one?

**Open note N9 (waiting on v0.2): bindings.** Deferred (Owner: Paykit v0.2;
date: with Paykit decision 20's phased plan, and before rollout step 2).
Undecided: the generator, its UniFFI version, and the surface to bind (N1).
Paykit's browser constraint: ubrn `wasm2` (UniFFI 0.31.x). The 0.31.2 floor
is for Kotlin on ARM32. Resolved by: the v0.2 bindings workstream. Blocks:
rollout step 2 and Paykit decision 20's phased plan.

### D12. Supported browsers and environments

**Context.** *Read*.

- Features the build may depend on, with first versions from MDN
  browser-compat-data 8.1.9 (dependencies, not the support floor):

  | Feature | Chrome | Firefox | Safari | Needed for |
  | --- | --- | --- | --- | --- |
  | `'wasm-unsafe-eval'` in CSP | 97 | 102 | 16 | always |
  | Module dedicated worker | 80 | 114 | 15 | always |
  | Web Locks | 69 | 96 | 15.4 | always (D4) |
  | `FileSystemSyncAccessHandle` | 102 (Android 109) | 111 | 15.2 | OPFS store |
  | IndexedDB strict durability | 83 | 126 | 15 | IndexedDB store |
  | `BroadcastChannel` | 54 | 38 | 15.4 | owner helper |
  | SharedWorker | 5 (Android 148) | 29 | 16 | SharedWorker owner |

- CSP: the SDK context's policy needs `'wasm-unsafe-eval'` and, since relays,
  Homeservers, and switches come from signed records, `connect-src https:`. A
  worker loaded from a URL takes its own response's CSP, so these can sit on
  the SDK worker while the page stays strict; a `blob:` or `data:` worker
  inherits the page policy. The page's `worker-src` (falling back to
  `child-src`, then `script-src`) must allow the worker's URL.
- Lockdown Mode on Apple devices turns off WebAssembly, IndexedDB, OPFS, the
  Cache API, service workers, Web Locks, and Push; WebAssembly may stay
  blocked even for an excluded site
  ([WebKit bug 273824](https://bugs.webkit.org/show_bug.cgi?id=273824), open).
- Chrome ships a major version every two weeks since 153, with an eight-week
  Extended Stable; Firefox every two weeks since 155. Playwright covers only
  recent Firefox, Chrome Stable and Beta, and WebKit main; Firefox ESR, Chrome
  Extended Stable, and the previous Safari need other tooling, such as
  geckodriver.

**Options.**

1. **A feature floor.** Any browser that passes the support check, tested on
   the latest stable Chrome, Firefox, and Safari.
2. **A version window per engine.** Chromium current Stable and Extended
   Stable; Firefox current and ESR; Safari, and every iOS and iPadOS browser,
   current and previous major.

**Proposal.**

- Option 2. Mobile browsers count as supported only after the
  [rollout](#rollout) step 4 runs on them. CI tests Chrome, Firefox, and
  WebKit; Safari itself before stable.
- Export a support check for a secure context, WebAssembly, WebCrypto, Web
  Locks, module workers, and the chosen store backing. It compiles a tiny
  module, because under a CSP without `'wasm-unsafe-eval'`
  `WebAssembly.validate()` passes and compilation throws. It reports what is
  missing. Lockdown Mode is unsupported.
- Document the CSP requirements, private windows (state may vanish at session
  end), and separate partitions (cross-site frames, Home Screen icons; in-app
  browsers untested).

**Decision needed (Owner: Paykit).** Confirm the support policy.

## Host-visible errors

**Proposal.** The binding reports each condition below with a stable code, so
a host can tell "retry later" from a permanent refusal. D3, D4, D5, and D8
share this table.

| Condition | Source | What the host sees |
| --- | --- | --- |
| Store owned elsewhere | `AcquireOutcome::Busy`, `StoreError::InUse` | A distinct code: "open in another tab or window" (D4) |
| Store full, lost, or unknown | Quota; a lost connection; "cannot tell" (`sw:crates/pubky-switch-client/src/error.rs:15-49`) | `Full`; `Unavailable` plus a "reload needed" signal; `Unknown` (D3). The client also reports `AppEvent::StoreFailing` and `ParkReason::StoreUnavailable` (`sw:docs/app-host-guide.md:46`, `:64`) |
| Own record superseded | `Publication::Superseded` (`sw:docs/app-host-guide.md:273`) | Leave the newer copy; in a browser it can mean another partition runs the same app (D3) |
| Switch refusal | `401 bad_signature`, `403 cert_expired`, `403 not_current` (`sw:spec/spec.md:171-176`); `401 stale_timestamp` (`sw:spec/wire-format.md:1618`) | Already distinct |
| Homeserver 401 | Several causes | A distinct code, never folded into retry advice (the #437 principle; v0.1 follow-up in [#523](https://github.com/pubky/paykit-rs/issues/523)) |
| Counterparty unreachable from a browser | For example, a pkarr record with no HTTPS endpoint | A distinct reason where the binding can tell, not retry advice (D5) |
| Call aborted after `Rail::pay` | `v02:paykit-sdk/src/rail.rs:492-499` | Uncertain; resolve through `Rail::payment` and the journal (D8) |
| Relayed call whose owner context went away | The owner helper (D4) | Uncertain; never resent |

## Groundwork on the v0.2 crates

Changes that let the v0.2 crates build and run on wasm. Each keeps native
behavior unchanged and lands in the crate that owns the code. Browser-specific
code lives only in the binding, its host-interface implementations (store,
transport, reader, clock, timer), and the TypeScript package. Protocol logic
in `paykit-lib`, `paykit-sdk`, and the Switch crates does not branch on the
target; `paykit-lib` already does no I/O and reads no clock
(`v02:README.md:31`). Items cheaper before the freeze are in
[Before the v0.2 freeze](#before-the-v02-freeze).

| Item | Where | Owner | Change |
| --- | --- | --- | --- |
| `http` feature | `v02:Cargo.toml:22`; `sw:Cargo.toml:29`; `sw:crates/pubky-switch-client/Cargo.toml:12-18` | Paykit, Pubky Switch | The client's default `http` feature pulls in Tokio. Browser builds take `pubky-switch-client` with `default-features = false`; the Switch workspace does too, or `pubky-switch-signer` stops forcing `http`. This is the production plan's crate-split item (`v02:specs/v0.2/spec.md:995`) |
| Randomness | `sw:Cargo.toml:36`, `:42`; `v02:Cargo.lock` | Paykit | `getrandom` 0.2 with `js`; 0.3 with `wasm_js`, version 0.3.4 or later (0.3.0 to 0.3.3 also need `--cfg getrandom_backend="wasm_js"`; *read* from source). 0.4 is not in the wasm build at `7aa75fe4` (only `tempfile` via the dev-only `pubky-switch-harness`, and `quinn-proto` and `rand` 0.10 via `reqwest`'s native-only path); add its `wasm_js` backend if it ever enters the browser tree. Matches "a getrandom backend per target" (`v02:specs/v0.2/spec.md:994`) |
| `Instant::now()` | `v02:paykit-sdk/src/pay.rs:2576`, `:3884`; `sw:crates/pubky-switch-client/src/transport/mod.rs:160` | Paykit, Pubky Switch | A host monotonic source or `web-time` (D8) |
| `SystemClock` | `sw:crates/pubky-switch-client/src/clock.rs:71` | Pubky Switch | Document "never on wasm" |
| `Send` | `sw:crates/pubky-switch-client/src/transport/mod.rs:24` | Pubky Switch | `MaybeSend`, or the channel shim (D10) |
| `chrono` | `sw:Cargo.toml:37` | Paykit | Built without `clock`, so no `wasmbind`; CI fails if a crate enables `chrono/clock` |
| Hash maps | `v02:paykit-sdk/src/periods.rs:369-375`; `v02:paykit-lib/src/allowances.rs:831`, `:1131`; `v02:paykit-sdk/src/paykit.rs:348`; Switch client not surveyed | Paykit | `std` maps are seeded predictably on wasm. Seed internally from `getrandom`, or use ordered maps where a counterparty chooses keys; public types keep `std` maps or move to `BTreeMap`. Which keys a counterparty controls is not yet checked |
| CI | paykit-rs `v0.2`, pubky-switch | Paykit, Pubky Switch | A wasm32 job that runs the v0.2 vectors (`v02:specs/v0.2/vectors/`) in a headless browser, not only `cargo check` (D8), plus a guard against `std::time::Instant`, `SystemTime::now`, and Tokio in non-test code of browser crates |

**Proposal.**

- Paykit adds no public helper API. For anything both Paykit crates share, use
  a small unpublished internal crate (cheap now, because both v0.2 crates are
  unpublished), or `pub` plus `#[doc(hidden)]` with no semver promise.
- Direct Rust use of `paykit-lib` and `paykit-sdk` on wasm32 is supported with
  the `http` feature off, as built in CI, with no semver promise for wasm-only
  items. This is a scope statement, not a feature gate.

**Decision needed (Owner: Paykit).** An internal helper crate, or
`#[doc(hidden)]` items, for anything both Paykit crates share? Proposal: an
internal crate, while both crates are unpublished.

**Decision needed (Owner: Paykit).** Is direct Rust use of `paykit-lib` and
`paykit-sdk` on wasm32 supported, and with which features?

## Before the v0.2 freeze

Each ask is cheap before the freeze and costly after it. The pull request that
implements a decision updates the passage it amends.

| Ask | Owner | Passage it amends | See |
| --- | --- | --- | --- |
| #537 item 1: a browser clause for the address check (Switch wire 7.1 and 8.2), where keys live (Switch spec 3.1), store guarantees 5 and 6, and "last cancellable handoff" over `fetch`; and moving "browser and WASM support" out of Switch spec 17's "Decided later" list | Pubky Switch | `sw:spec/spec.md:98`, `:367`, `:740`, `:1251`; `sw:spec/wire-format.md:1593`, `:2509`; `sw:crates/pubky-switch-client/src/store/mod.rs:75-79` | D2, D3, D5 |
| #537 item 2: normative CORS | Pubky Switch (spec and service) | None yet | D5 |
| #537 item 3: keep an async store possible behind `StoreHandle`, keep its inline mode, and keep new store access behind it. Also move existing raw `&dyn StateStore` access behind it: `StoreHandle::store()`, every protocol callback (`app/protocol.rs:910-915`), and every helper that takes one (D3); fence `App` writes with the lease, and allow a host-initiated release (D4). Proposed contract text: "A host may revoke a `StoreLease`. After revocation, every `commit_if` under the old lease returns `NotOwner`, every `commit` to the namespace from the old holder returns `InUse`, and the client reloads state before acting on either." | Pubky Switch (client) | `store/mod.rs:116-127`; `app/protocol.rs:910-915`; `app/kept.rs:186`; `app/records/previous.rs:18` | D3, D4 |
| #537 item 4: may a web app be its identity's signer? The 2026-11-11 agreement does not settle it (D1) | Pubky, with Paykit v0.2 | `sw:spec/spec.md:1301` | D1 |
| #537 item 5: one line in the v0.2 plan saying browser support follows v0.2, with the other items open for it | Paykit v0.2 | `v02:specs/v0.2/spec.md:993`, `:995`, `:1000`; `v02:THESAURUS.md:37-38` | Rollout |
| #537 "cheaper now": `MaybeSend` | Pubky Switch | `sw:crates/pubky-switch-client/src/transport/mod.rs:24` | D10 |
| #537 "what already fits": keep `paykit-sdk` free of a Tokio runtime, Tokio and `http` optional in `pubky-switch-client`, and `Clock`, `Transport`, and the store supplied by the host. Bearer replacement is gone for Switch calls only; the `HomeserverWriter` bearer still needs one owner context (D4) | Paykit v0.2, Pubky Switch | None; keep as is | D3, D4, D10 |
| #537 "can wait": `FirstSend`'s monotonic time (fine after the freeze; it is not API). Also a standard seed derivation (Pubky; D1) and a wasm32 CI job that runs code in a browser, since `cargo check` would not catch the `Instant::now()` traps (D8) | Pubky Switch; Pubky (derivation); Paykit v0.2 and Pubky Switch (CI) | `sw:crates/pubky-switch-client/src/transport/mod.rs:160`; `sw:spec/spec.md:95-96` | D1, D8 |
| #537 "cheaper now": the two `Instant::now()` calls in `pay.rs` (before every attempt batch and before an idempotent repeat's claim read); every payment attempt would trap (*inferred*, D8) | Paykit v0.2 | `v02:paykit-sdk/src/pay.rs:2576`, `:3884` | D8 |
| #537 "cheaper now": `pubky-switch-signer` forcing `http` | Pubky Switch | `sw:Cargo.toml:29` | Groundwork |
| #537 item 7: a browser host in Paykit decision 20's mapping | Paykit v0.2 | `v02:specs/v0.2/spec.md:712` | N1 |
| #537 item 6: browser installs as separate apps, not Paykit decision 18's one app across devices | Paykit v0.2 | `v02:specs/v0.2/spec.md:710` | D1, D3 |

Push delivery names only APNs and FCM (`sw:spec/spec.md:80`, `:686-689`);
that waits on open note N8.

### Pubky asks that do not wait on the freeze

| Ask | Owner | Status | Fallback | See |
| --- | --- | --- | --- | --- |
| `Cache-Control: no-cache` on `/pub` reads; `NetworkOnly` on the first pkarr lookup after a load | Pubky | Not yet filed | `no-store` and a network-only first lookup in the binding | D6 |
| WebPKI for browser Homeserver traffic | Pubky | Not yet filed | Document the exposure | D5 |
| Event-stream keep-alives with a first byte at once, so Firefox resolves the request (the Switch spec asks for keep-alives at `sw:spec/spec.md:1274`) | Pubky | Not yet filed; merge with the Switch spec's ask | No event stream in the first release | D7 |
| A Pubky release that ships pubky-homeserver #725, with streams exempt | Pubky | #725 merged 2026-11-10, unreleased (latest release v0.18.0, 2026-11-09); stream exemption not yet filed ([pubky-homeserver#817](https://github.com/pubky/pubky-homeserver/issues/817) records that #725 aborts event streams, but asks for a bound on the session lease, not a stream exemption) | The binding's own reader deadline | D8 |
| A stable grant-session coordinator and delegated proof-of-possession signer | Pubky | Not yet filed; low priority | One owner context restores the session | D2, D4 |

## Rollout

Each step waits on the one before. Steps 1 to 3 wait on the v0.2 freeze, so
they build on a stable spec. Step 3 also waits on the v0.2 release. The asks
in [Before the v0.2 freeze](#before-the-v02-freeze) gate the steps as follows.
Gates on #537:

- Step 1 needs only the "cheaper now" item (`MaybeSend`) asked.
- Step 3 starts when #537 items 1, 3, and 4 each have a written answer on the
  issue, or when the v0.2 freeze passes without one. In that case the Paykit
  maintainer records on #537 that the fallback in D1, D2, D3, or D4 applies.
  Item 2 must be in the Switch spec text.
- Step 4 needs item 2 deployed on a production switch and item 1 answered
  yes. With item 1's fallback, step 4 ends in a documented lower-assurance
  pre-release, not a stable release.

1. **Groundwork** ([Groundwork on the v0.2 crates](#groundwork-on-the-v02-crates)).
   Exit: `paykit-lib` and `paykit-sdk` build for wasm32 with `http` off, and
   CI runs the v0.2 vectors and a smoke payment path in a headless browser.
2. **Toolchain.** A `wasm2` probe with v0.2 on a throwaway branch, then the
   bindings workstream's generator choice (D11). Exit: one UniFFI version
   every generator supports; Swift and Kotlin libraries loaded at run time;
   and the generated web binding loads and runs one call in a dedicated
   worker in Chrome, Firefox, and WebKit, under a CSP with
   `'wasm-unsafe-eval'` and no `'unsafe-eval'`, with no CSP violation report.
3. **Browser binding.** Build the D10 binding, the reference store (D3), the
   ownership rules (D4), the browser transport (D5, D8), `no-store` reads
   (D6), and the page-to-worker API (D9). Then run an end-to-end test on HTTPS
   under a CSP, against a local Switch service whose test harness adds the
   CORS rules that #537 item 2 specifies. The test joins through a signer and
   covers:
   - record publication, public endpoints, and the Paykit Profile with the
     files its fields reference;
   - a one-time Payment Request paid and answered with a Payment Receipt,
     Private Payment Lists, including paying a single-use endpoint
     (`v02:specs/v0.2/spec.md:272`), Recurring Payment Requests, and
     Allowances;
   - contacts and the deny list (`paykit_manage_contacts`,
     `v02:specs/v0.2/spec.md:89-90`);
   - receive by long-poll, re-sends, backup and restore, and an eviction that
     looks like a new install;
   - revoke and leave, and a deploy and rollback across a store format
     change.

   Exit: the first release, a pre-release, with every exported call either
   run in the test or marked untested. Its release notes name the paykit-rs
   revision, the Switch pin, and the Pubky client versions the test used for
   `HomeserverReader` and `HomeserverWriter`.
4. **Hardening.** A real Switch service with CORS, a real HTTPS Homeserver,
   Safari, mobile browsers, a persistent-profile cache test, a relay test of
   the D9 shape (a service-worker engine relaying to a page-owned dedicated
   worker), and the D2 security review of hand-written
   browser code. Exit: the first stable release.
5. **Later.** An IndexedDB store on an async seam (D3), then the service
   worker run (D9) and Web Push (D7), which need it; and a SharedWorker owner
   (D4).

**Decision needed (Owner: Paykit).** May a host ship the step 3 pre-release to
users with real funds? Proposal: no, until step 4's security review is done.

## Open notes and questions

### Waiting on v0.2

N3 and N6 mirror #537 items and are tracked on their **Decision needed**
lines; the others have an **Open note** paragraph in their section.

| ID | Topic | Resolved by | Where |
| --- | --- | --- | --- |
| N1 | v0.2 SDK surface, including a browser host's calls | Paykit decision 20's draft | [Why v0.2](#why-v02-and-when), [D9](#d9-where-the-sdk-runs) |
| N2 | Freeze items: Paykit decision 22, Switch decision 24, persistent storage in a production switch, the retention check, measurements, and the crypto review | Team review of the freeze items | [Why v0.2](#why-v02-and-when) |
| N3 | Web app as signer; `run_due` and the identity key | #537 item 4 | [D1](#d1-identity-and-the-signer), Decision (Pubky) |
| N4 | Eviction with seed-derived keys; rollback; store format versions | A Paykit v0.2 or Pubky Switch answer | [D3](#d3-the-state-store) |
| N5 | Unknown deliveries after restore | SDK handling of `DeliveryUnknown` | [D3](#d3-the-state-store) |
| N6 | Switch CORS and a browser address rule; when deployed switches serve CORS | #537 items 1 and 2 | [D5](#d5-transport-trust-and-cors), Decision (Pubky Switch) |
| N7 | Who implements the Homeserver event stream; one request for several users | An owner named in the SDK surface (N1) | [D7](#d7-receiving-and-push) |
| N8 | Web Push relay and browser delivery rule | A Web Push relay with a named owner | [D7](#d7-receiving-and-push) |
| N9 | Binding generator and UniFFI version | The v0.2 bindings workstream | [D11](#d11-toolchain-and-package) |

### Not evaluated

- WebTransport `serverCertificateHashes` (Chrome 100, Firefox 125, Safari
  26.4; certificates under two weeks) or WebRTC fingerprints to pin a server.
  Low priority now that Switch traffic is signed.

### Facts to measure

These need a run, not more reading.

- Production switches with CORS; the production Homeserver's CORS through its
  proxy.
- Whether a pending long-poll or event stream keeps a push-woken service
  worker alive in Firefox and Safari.
- Whether Safari serves heuristically fresh Homeserver reads (persistent
  profile, writer elsewhere).
- Dedicated-worker throttling when every page is hidden; whether Chrome on
  Android freezes or discards a lock-holding tab.
- Whether WebKit keeps a suspended page's worker's OPFS sync access handle;
  whether OPFS can lose its handle like WebKit's IndexedDB connection.
- Whether Safari's 7-day deletion covers OPFS, and whether browser profile
  data is in ordinary backups on each platform (#537 item 1).
- How an evicted install behaves at start-up (it looks like a new install;
  D3, rollout step 3).
- Cold-start join latency against production relays; preflights per user
  action; store cost for a backup-sized value; a wasm size budget.

## Changes from the v0.1 proposal (#437)

Nothing was dropped silently. Each line says what changed and why.

| #437 item | Change | Why |
| --- | --- | --- |
| D0, protocol version | Decided: v0.2 | Team decision after the audit; v0.2 removes the risks in #437's D1, D3, and D4 |
| `paykit-ffi` in scope | Dropped | v0.2 has no `paykit-ffi`; bindings come later |
| Paykit Identity Secret, its derivation and delegation carrier; root-holding host as the first model; `/pub/paykit/:rw` grant and Pubky Profile capability; key delivery during auth (`e` capability) | Replaced by D1 roles | A signer gives each v0.2 app its own keys and a grant for exactly its paths |
| Pubky shared state, vetted Homeservers, replay protection; Homeserver write locks (clock, 412, pending marker); slot search and key and nonce reuse after rollback; supervised intake, its lease, and takeover; Public Contact Markers, App Registry, Noise Key Authorization, Pubky Profiles | Dropped; lock clock concerns to D8, the lease lesson to D4 | Not in v0.2 or outside its SDK (`v02:specs/v0.2/spec.md:640-657`); fresh random keys and nonces |
| `RestoredLinkPolicy`, `RequireRecovery`, re-handshakes after a restore | Replaced by `Paykit::restore` and reconcile mode (D3) | No Noise links in v0.2 |
| #437 D5 option 3: a shared bearer through Pubky's coordinator; several runtimes on storage revisions | Dropped; one owner context per store namespace (D4); the coordinator stays a low-priority Pubky ask | Switch calls carry no session; no revisions, and `App` uses ordinary commits. The coordinator matters only for the `HomeserverWriter` session |
| Owner helper liveness timeout and lock release on `visibilitychange`, `freeze`, and `pagehide` (#437 reply) | Replaced by the D4 helper rules | Releasing on hide gives up Chrome's freeze exemption; `freeze` is Chromium only; release follows stopping the runtime |
| "Message slot writes run under the SDK's peer link lease, not in this lock scope" (#437 reply) | Not carried | Wrong on v0.1: slot writes were inside the lock scope, and the lease was not a separate fence; moot in v0.2 |
| Legacy polling per peer | Replaced by the feed long-poll (D7) | v0.2 removes polling each contact |
| Flows the spike did not run: contact removal and blocking, app removal, Payment Endpoint Reservations, Pubky Profiles | Mapped into the step 3 test (deny list, revoke and leave, single-use endpoints); Pubky Profiles dropped | v0.2 equivalents; Pubky Profiles are not in the v0.2 SDK |
| `request_timeout` ask, Paykit stopgap deadline, global `fetch` wrapper | Replaced by binding-owned deadlines (D8) | The host's transport is the caller; a wrapper replaces `fetch` for its whole context |
| Stale-read fix in Pubky and `pubky-noise` | Replaced by `no-store` in the binding's reader (D6) | No `pubky-noise`; counterparty Homeservers are not ours to fix |
| Relay list field on every binding; "no transport abstraction" | Relay list moved to the host's Pubky client; the other dropped | The host builds the reader and writer; v0.2 has a `Transport` trait |
| UniFFI 0.31 upgrade of existing bindings; 32-bit Android for React Native; async variants of the three v0.1 callbacks | Generator choice to D11; callbacks replaced by D10 | No v0.2 bindings to upgrade; those interfaces are not in v0.2 |
| 401 as an SDK error on every binding; `is_forbidden` | Dropped; principle kept in [Host-visible errors](#host-visible-errors) | v0.1 runtime; tracked in [#523](https://github.com/pubky/paykit-rs/issues/523) |
| `PubkyGrantLogout` on wasm | Dropped | `sign_out` already revoked on wasm; v0.2 revocation is the signer's |
| pubky 0.16.2 pin and its upgrade | Dropped; release notes name the host's Pubky client versions (Rollout) | Paykit v0.2 has no `pubky` dependency |
| v0.1 groundwork (`snow` and `ring`, `uuid`, `chrono` `wasmbind`, `is_connect`, bounded-read sites, runtime module) | Replaced by v0.2 Groundwork | v0.2 has no `snow`, `uuid`, `is_connect`, or runtime module. `chrono` comes in through `pubky-switch-core` with only `std` (no clock, so no `wasmbind`). `ring` comes in through the client's default `http` feature, which the browser build turns off (Groundwork, `http` row) |
| Whole-blob rewrite cost ([#188](https://github.com/pubky/paykit-rs/issues/188)) | Dropped; store cost is measured instead (Facts to measure) | v0.2 stores one record per key |
| v0.1 sign-up latency | Replaced by cold-start join latency (Facts to measure) | No v0.1 sign-up in v0.2 |
| Mobile bindings turning the DHT off; `sync_public_endpoints` failing silently; the FFI's 30-second default; storage lockup after a panic inside a transaction | Dropped | v0.1 FFI and runtime |
| v0.1 doc drift: stale-snapshot wording in the SDK spec and `backup.rs`; the sessions wording in `specs/paykit-sdk.md`; the `StateBlobStorageAdapter` heading; the CLAUDE.md deadline wording | Moved out; the deadline rule is now `v02:AGENTS.md:57` | Still on master (`m:specs/paykit-sdk.md:1888-1891`, `m:paykit-sdk/src/runtime/backup.rs:52`, `m:specs/paykit-sdk-bindings.md:85`); to be filed as an issue against master |
| Evidence tables; spike rebase note; "native suites still pass"; PR body "the code problems all had workarounds" | Tables kept, labelled v0.1, and corrected (lifecycle figures given across engines, not per engine; Playwright WebKit says nothing about Safari's cache, D6; SharedWorker on Android is in the window, D12). The frozen shared-state write test is dropped; its v0.2 analogue is a resumed owner context and `NotOwner` (D4). Notes dropped; claim withdrawn | Platform lessons still apply; the notes were about v0.1 groundwork; the claim contradicted the Firefox event-stream exception |
