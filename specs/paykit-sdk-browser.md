# Paykit SDK in the browser

Status: draft for review. The decisions in this document are open.

This document proposes how the Paykit SDK runs in a web browser, compiled to
WebAssembly (`wasm32-unknown-unknown`). It tracks
[issue #411](https://github.com/pubky/paykit-rs/issues/411).

A throwaway spike showed that the whole SDK compiles for the browser and runs
there against a local Pubky testnet. No compile or runtime problem blocked the
flow. Each has a workaround in the spike except one: in Firefox an
event-stream request takes 15 seconds to open, which needs a homeserver change
(D11). What remains is a set of product, security, and API decisions. This
document lists them, gives the evidence for each, and makes a proposal for
each.

## How to review this document

- Each entry under [Decisions](#decisions) ends with a **Proposal** and one or
  more **Decision needed** lines. Comment on that section to agree, disagree,
  or propose another option. The proposals are starting points, not
  conclusions.
- Before this document merges, every **Decision needed** line becomes either
  **Decided** with the outcome, or **Deferred** with an owner.
- Statements marked *verified* were run in a browser during the spike.
  Statements marked *read* come from reading source code or vendor
  documentation and were not run. Statements marked *untested* are
  expectations.
- Some decisions belong to Pubky, not Paykit. They are marked **Owner: Pubky**.
  This document records Paykit's position on them only.

## Summary of decisions

| ID | Decision | Owner | Proposal in one line |
| --- | --- | --- | --- |
| [D0](#d0-which-protocol-version-the-first-browser-release-targets) | Protocol version to target | Paykit | Target v0.1 as implemented; keep browser-specific code out of protocol logic |
| [D1](#d1-how-a-browser-app-gets-its-identity) | Identity in the browser | Paykit, Pubky | Support root-holding hosts first; do not define a delegation carrier on v0.1 |
| [D2](#d2-secret-custody-and-threat-model) | Secret custody and threat model | Paykit | Same-origin script is trusted; sealed storage is the minimum; the host may supply the wrapping key |
| [D3](#d3-state-backing) | Where SDK state lives | Paykit, Pubky | IndexedDB for an identity that lives in one browser profile; Pubky shared state only on vetted homeservers; ship one reference store |
| [D4](#d4-transport-trust) | Transport trust | Pubky, Paykit | Accept WebPKI for browsers; document what it exposes; add a relay list setting |
| [D5](#d5-several-tabs-and-one-grant) | Several tabs, one grant | Paykit, Pubky | One SDK runtime per origin, identity, and browser profile; adopt Pubky's shared bearer when it is stable |
| [D6](#d6-private-intake) | Private intake | Paykit | Legacy polling first, as on Swift and Kotlin; supervised intake later |
| [D7](#d7-request-deadlines) | Request deadlines | Pubky, Paykit | Fix `request_timeout` on wasm upstream; a Paykit deadline only as a stopgap |
| [D8](#d8-where-the-sdk-runs) | Execution context | Paykit | Dedicated worker or page; service worker only after its own test run |
| [D9](#d9-binding-toolchain-and-package) | Binding toolchain and package | Paykit | Generated TypeScript on UniFFI 0.31 and ubrn `wasm2`; separate npm package |
| [D10](#d10-async-host-callbacks) | Async host callbacks | Paykit | Async storage, session, and payment callbacks; decide the shape before the UniFFI upgrade |
| [D11](#d11-supported-browsers-and-unsupported-environments) | Supported browsers | Paykit, Pubky | A version window per engine; Lockdown Mode unsupported |

## Scope

In scope:

- Building `paykit-lib`, `paykit-sdk`, and `paykit-ffi` for
  `wasm32-unknown-unknown`, single-threaded.
- A TypeScript package for browser apps that exposes the same SDK workflows as
  the Swift, Kotlin, and React Native bindings.
- The browser-specific rules a host app must follow: storage, secrets, tabs,
  Content Security Policy, and supported browsers.

Out of scope:

- Node.js, Deno, and other non-browser JavaScript runtimes.
- WebAssembly threads. The build must fail when the `atomics` target feature
  is enabled, because the browser build relaxes `Send` bounds.
- Changes to the Paykit protocol or wire formats.
- User interface.

In this document, the *host app* is the web application that embeds the SDK.
The *first release* is the pre-release published at the end of
[rollout](#rollout) step 3.

## Relationship to existing specifications

This document adds a browser target to [`paykit-sdk.md`](paykit-sdk.md) and
[`paykit-sdk-bindings.md`](paykit-sdk-bindings.md). The bindings specification
describes itself as the mobile surface for Swift, Kotlin, and React Native.
Where a decision adds to or changes a passage in either document, the pull
request that implements the decision updates that passage:

- D2: what "platform-protected storage" means in a browser.
- D3: a first-party reference store, where the bindings specification treats
  storage as an app-provided integration point.
- D5: a browser rule of one runtime per origin and identity, where the
  bindings specification allows several runtimes on one blob.
- D6: the SDK specification's statement that supervisor lifecycle APIs are
  not exposed through `paykit-ffi`, once supervised intake is exposed.
- D7: the project rule that Paykit sets no deadline, if a stopgap is added.
- D10: async store callbacks, where the bindings specification describes the
  store callbacks as synchronous.

One existing passage is already out of date and is not caused by this
document. The Backup And Restore section of the SDK specification says that a
stale snapshot makes the next send reuse a transport key and nonce. Commit
`59ae3049` changed that behavior (see Rollback in D3). The passage needs its
own correction.

## Evidence

The spike is on the branch
[`throwaway/wasm-browser-sdk-411`](https://github.com/pubky/paykit-rs/tree/throwaway/wasm-browser-sdk-411).
It is based on `ed92032b`. Do not merge it. Its workaround log is
`docs/wasm-spike-411.md` on that branch, and every temporary change in the
code is marked `THROWAWAY (#411)`.

As of `0b9c63d2`, master is 17 commits past that base, with no change to
external dependency versions. A dry-run merge shows one textual conflict, in
`paykit-ffi/src/session.rs`.

### What ran

All runs used a local `pubky-testnet` 0.16.2 over plain HTTP, with Chrome 157,
Playwright Firefox 157, and Playwright WebKit 27.2. "Worker" means a dedicated
worker.

| Run | Chrome | Firefox | WebKit |
| --- | --- | --- | --- |
| Offline wasm suite, 18 tests, page and worker | pass | pass | pass |
| Full SDK flow through a hand-written wasm-bindgen facade, 42 steps, page and worker | 42/42 | 41/42, and 42/42 with `cache: "no-store"` | 42/42 |
| Generated UniFFI TypeScript, 21 steps, page and worker | 21/21 | not run | not run |
| A second tab that restores the same grant makes the first tab's requests fail with HTTP 401 | reproduced | reproduced | reproduced |
| Single-owner prototype (Web Lock and `BroadcastChannel`) | pass | pass | pass |
| Private intake restart after a page reload | 116.7 s | 116.0 s | 116.5 s |
| The same, with a prototype lease takeover | 54 ms | 118 ms | 68 ms |
| A context frozen past its 60-second shared-state write lock cannot overwrite newer state (freeze emulated with a busy loop) | pass | pass | pass |
| Panic, then rebuild the worker from IndexedDB | 36-52 ms | 96-118 ms | 61-73 ms |

Every row below the generated-TypeScript row ran through the hand-written
facade only.

The 42-step flow covers sign-up, initialization, public Payment Endpoints, the
Encrypted Link Handshake, Private Payment Lists, private intake over the event
stream and by polling, Payment Requests (accept, reject, cancel, conversion
quote, proof), Receipts and Receipt Access, Allowances with one accounting
cycle, backup export and restore, key rotation with link recovery, three state
backings (memory, IndexedDB, Pubky shared state), and sign-out.

The 21-step generated-TypeScript run covers public Payment Endpoints, the
Encrypted Link Handshake, Private Payment Lists, and async storage and session
callbacks.

The native test suites still pass on the spike branch.

### What did not run

- A real HTTPS homeserver, and the production pkarr relays.
- Pubky shared state on a homeserver with GCS storage. The testnet homeserver
  stores files in memory.
- A service worker or a shared worker.
- Safari itself, and any mobile browser.
- Real tab freezing, tab discarding, and operating-system sleep. Freezes were
  emulated with a busy loop.
- IndexedDB eviction, Safari's 7-day storage deletion, and private browsing.
- A Content Security Policy.
- Sign-in through an external signer such as Pubky Ring.
- Running the SDK without the Pubky identity secret, as a delegated app does.
- Paykit Profiles and Paykit Blob upload, Pubky Profiles, Public Contact
  Markers, contact removal and blocking, Paykit App removal, and Payment
  Endpoint Reservations.
- Through the generated TypeScript: Payment Requests, Receipts, Allowances,
  backup, key rotation, private intake, and every lifecycle run (two tabs,
  reload, freeze, panic, deadlines).
- UniFFI 0.31 and ubrn 0.31. The CI workflow on the spike branch has never
  run.

## What a browser changes

These are the platform facts that drive the decisions.

- **No threads and no Tokio runtime.** Futures are not `Send`. Timers and
  tasks come from the browser event loop.
- **No operating-system key store.** There is no Keychain or Keystore.
  WebCrypto can hold a non-extractable key, but only for the algorithms it
  implements, and any script in the origin can use that key.
- **Storage is asynchronous and can be evicted.** IndexedDB is the only
  durable store that fits. The browser may delete it.
- **Networking is `fetch`.** The browser does TLS with its own certificate
  authorities. `fetch` offers no raw socket, no custom certificate
  verification, and no DHT.
- **The context can stop at any time.** A tab can be reloaded, frozen,
  discarded, or closed mid-call. Nothing keeps running afterwards. A call
  that is cut off leaves its lease in the state: 60 seconds for a peer link
  operation, a Paykit App operation, or an outbound private send (*read*, not
  measured). Until the lease expires, the next context gets an "already in
  progress" error for that peer or Paykit App.
- **Several contexts share one origin, and some do not.** Tabs, workers, and
  a service worker in one browser profile can each start an SDK runtime over
  the same storage and the same grant. The same origin in another browser,
  another browser profile, a private window, or a web app added to the Home
  Screen has separate storage and shares no IndexedDB, Web Locks, or
  `BroadcastChannel`. Those contexts behave like separate devices.
- **A secure context is required.** WebCrypto, Web Locks, persistent storage,
  and service workers need HTTPS, or `localhost` during development.

## First integrator

One host app is expected to integrate first. Its design (*read* from its
public source, not run) has these properties:

- It is a self-custodial web wallet, installed as a PWA. It is a TypeScript
  app that also targets React Native (D9).
- Its wallet engine runs in one service worker per origin. The service worker
  is a single bundled classic script. Pages talk to it with messages, and the
  page restarts it when the browser stops it (D8).
- It already holds a BIP39 mnemonic in the browser, encrypted with a key
  derived from the user's password. Its threat model trusts page and worker
  memory fully (D1, D2).
- It uses the Web Locks API so that only one tab drives certain work (D5).
- Its Content Security Policy does not allow WebAssembly compilation today
  (D11).

The proposals are checked against these properties. The first release does
not meet all of them: the service worker is deferred (D8). The design must not
depend on this one host app.

## Groundwork

The spike found a workable answer for each of these. They can land as separate
pull requests while the other decisions are discussed, unless D0 chooses to
wait for v0.2. Each keeps native behavior unchanged.

| Area | Change |
| --- | --- |
| Dependencies | Browser randomness for `getrandom` and `uuid`; `chrono` `wasmbind`; all as `wasm32` target dependencies |
| `ring` | `snow` 0.10.0 enables `ring` by mistake (`ring/std` should be `ring?/std`). Fix upstream; no C toolchain is needed after that |
| Bounded reads | `reqwest` on wasm has no `Response::chunk()`. Read with `bytes_stream()` and keep the size bound. Six sites in Paykit, two in `pubky-noise` |
| Runtime | One internal module that re-exports Tokio on native and uses `spawn_local`, browser timers, and `web_time` on wasm |
| `Send` bounds | Relax `Send` on wasm only for boxed futures and for the storage, session, and payment traits. Add `compile_error!` for wasm with `atomics` |
| Hash maps | `std` hash maps are seeded from memory addresses on wasm, which is predictable. Either seed from `getrandom` through a shared alias and fail if it is unavailable, or use ordered maps where keys come from the network. This is a choice, and the first option was a large diff in the spike |
| CI | `cargo check --target wasm32-unknown-unknown` for each crate on its own, because workspace dev-dependencies hide missing browser features |

These helpers must not become public API. The runtime module, the bounded-read
helper, and the hash-map aliases are used by more than one crate, so
`pub(crate)` is not enough. Either they move to a small internal workspace
crate, or they stay `pub` and `#[doc(hidden)]` with a statement that they are
not covered by semver. On wasm, the futures returned by the public storage,
session, and payment traits are not `Send`. That is a visible difference
between targets, and the crate documentation must state it.

## Decisions

### D0. Which protocol version the first browser release targets

**Context.** All spike work used the v0.1 SDK, which is what the code
implements today. [`specs/v0.2`](v0.2/README.md) is a review candidate with no
freeze date. It replaces v0.1 "with no migration". It changes the parts this
document spends the most time on: the shared locked state file, the Noise
handshakes over homeserver files, and the identity model.

Some decisions here are about the browser and survive a protocol change:
D2, D4, D7, D8, D9, D10, and D11. D5 also survives unless v0.2 stops using one
homeserver bearer session per grant. Others are about v0.1 mechanisms and may
not survive: D1 (the Paykit Identity Secret), D3 (the state blob), and D6 (the
intake supervisor lease).

**Options.**

1. Target v0.1 now and accept that the v0.1-specific parts are replaced later.
2. Wait for v0.2 and build the browser binding on it.
3. Land the groundwork now, and build the binding once v0.2's timing is clear.

**Proposal.** Option 1, with a constraint: keep browser-specific code in the
runtime layer, the binding layer, and the TypeScript package. Protocol logic
must not branch on the target. Then the v0.1-specific answers stay small and
replaceable, and rollout step 3 can mark which deliverables are v0.1-only.

This choice rests on two facts that maintainers must confirm: the date by
which the first integrator needs a working SDK, and that v0.2 will not be
frozen and implemented before that date.

**Decision needed (Owner: Paykit).** Confirm the target, or say when v0.2
would make option 3 the better choice.

### D1. How a browser app gets its identity

A *root-holding host* is a host app that holds the Pubky identity secret.

**Context.** *Read* from master:

- The Paykit Identity Secret is 32 bytes, derived with BLAKE3 from the Pubky
  identity secret and a generation number. A root-holding host derives it
  locally. If session access includes the Pubky identity secret and no
  explicit Paykit Identity Secret, the SDK derives generation 1 itself.
- A delegated app must be given the 32 bytes and the generation. The SDK
  specification says "through a trusted channel" and defines none, on any
  platform.
- Completing a Pubky auth request, or importing a session, never returns a
  Paykit Identity Secret. A session from an external signer with no local key
  can only use public workflows until the app attaches a secret that it
  obtained some other way.
- Pubky 0.16.2 can deliver keys during auth (signed approvals with the `e`
  capability). Paykit turns this off: it requests bare grants and rejects the
  other URL form. Those Pubky keys are a different derivation with no
  rotation, so they cannot carry the Paykit Identity Secret as it is defined
  today.
- Companion claims are a channel from the approving identity to the
  requesting app. Paykit implements only the sending side.
- Paykit has `PubkyLocalSecretKey::from_bip39_mnemonic`, exported by every
  binding. It follows a convention shared by `pubky-core-ffi` and
  `pubky-app`: English word list, empty passphrase, and the first 32 bytes of
  the 64-byte BIP39 seed as the Ed25519 secret. Neither `pubky` nor
  `pubky-common` defines this.

The spike generated a random Pubky identity secret inside wasm and signed up
with it (*verified*). It never signed in through an external signer.

**Options.**

1. **The host app holds the Pubky identity secret.** It generates or derives
   the secret, signs up or signs in, and the SDK derives the Paykit Identity
   Secret. This works with today's API. The host then acts as the identity
   authorizer: its session needs the authorizer capabilities, and it
   publishes the Paykit Noise Key Authorization before private workflows, as
   the spike did.
2. **An external signer delegates to the browser.** The browser app receives a
   grant plus the Paykit Identity Secret. This needs a defined carrier, a
   receiving helper in Paykit, support in the signer, and a way to learn the
   new generation after a key rotation. None of these exist. A delegated app
   can be cut off by revoking its grant and rotating.
3. **Wait for v0.2**, where a signer issues each app its own keys.

**Proposal.**

- Support option 1 in the first release. It fits self-custodial hosts that
  already hold a seed in the browser, which includes the first integrator.
- State its cost. The Pubky identity secret then sits in a web origin. Under
  the threat model in D2, script that runs in the origin can take it, and
  with it the whole Pubky identity and every app on it, not only Paykit. This
  is permanent: the SDK specification states that rotation cannot revoke a
  holder of the Pubky identity secret.
- Limit that exposure. The SDK uses the Pubky identity secret only to sign a
  grant at sign-up or sign-in, to publish the Paykit Noise Key Authorization,
  and to rotate (*read*). The binding documentation should tell root-holding
  hosts to unlock it only for those steps. For normal operation the host
  derives the Paykit Identity Secret, signs in for a separate grant limited
  to `/pub/paykit/:rw`, exports that session secret, and runs the SDK with
  those two values and no Pubky identity secret, as a delegated app does.
  This mode is *untested* in the browser, and the browser end-to-end test
  must cover it.
- Do not define an option 2 carrier on v0.1. It is a protocol addition that
  v0.2 would replace. State in the binding documentation that delegated
  browser sign-in gives public workflows only.
- Add guidance for hosts that derive the Pubky identity secret from a wallet
  mnemonic. `from_bip39_mnemonic` always uses an empty BIP39 passphrase. For
  a wallet without a passphrase, feeding it the wallet's own mnemonic makes
  the Pubky identity secret equal to the first 32 bytes of the 64-byte seed
  that is also the input to the wallet's BIP32 master key. This is not a
  known break: Ed25519 hashes the secret before use, and the master key
  depends on all 64 bytes. But a leaked Pubky identity secret then discloses
  half of the wallet seed, and the two keys cannot be replaced separately.
  For a wallet with a passphrase, the passphrase does not protect the Pubky
  identity secret. Hosts should derive a dedicated secret instead. The BIP39
  application of BIP85 (`m/83696968'/39'/0'/{words}'/{index}'`) derives a
  child mnemonic that reveals nothing about the wallet seed, and another
  Pubky app that uses the same convention can import the identity from that
  child mnemonic.
- This guidance has a cost and applies on every platform. The wallet
  mnemonic itself then gives a different identity in other Pubky apps. A host
  must use one derivation on all its platforms, or the same seed gives two
  identities. An identity that already uses the direct derivation must keep
  it, because changing the derivation changes the public key. Paykit should
  not choose the word count and index alone.

**Decision needed (Owner: Paykit).** Is root-holding the only supported
browser identity model for the first release?

**Decision needed (Owner: Pubky).** Is there, or should there be, a standard
derivation of a Pubky identity secret from a wallet seed? If so, it must fix
the scheme, path, and index, and say what happens to identities already
derived with the first-32-bytes convention. Should delegation of Paykit key
material be defined before v0.2, and by which carrier?

### D2. Secret custody and threat model

**Context.** A browser host holds these secrets (*read*):

| Secret | Where it is kept | Notes |
| --- | --- | --- |
| Pubky identity secret | Host storage | Only for root-holding hosts (D1). Cannot be rotated or revoked if it leaks |
| Paykit Identity Secret and generation | Host storage, or derived from the Pubky identity secret | Needed for every private workflow |
| Session secret | Host storage | A string that contains the signed grant and the proof-of-possession key. Equivalent to a bearer credential |
| SDK state blob | The state backing (D3) | Plaintext when it reaches the host. Contains the Noise static secret, per-link ephemeral secrets, Receipt Decryption Keys, and private message plaintext |
| SDK backup blob | Wherever the host exports it | Unencrypted, with the same kinds of secrets as the state blob. The host must encrypt it and keep a copy outside the origin's evictable storage |

The state blob does not contain the Paykit Identity Secret, the Pubky identity
secret, or the session secret.

All of these must exist as raw bytes inside wasm memory while they are used,
because the Rust cryptography takes byte arrays. WebCrypto can run the X25519
step on a non-extractable key. It cannot run the rest of the Noise protocol
there: ChaCha20-Poly1305 is not available in every engine, and the WebCrypto
API is asynchronous while `snow` calls its primitives synchronously and takes
keys as bytes.

The session's proof-of-possession key is a partial exception. Pubky 0.16.2 has
a hook that lets a browser sign session proofs with a non-extractable
WebCrypto key (*read*). It is `#[doc(hidden)]`, and it exists only for
sessions approved through the grant auth flow by an external signer.
Root-holding sign-up and sign-in always create a local key. Paykit's session
access does not use the hook.

Zeroization is best effort. The SDK wipes its own wrapper types, and that
works in wasm memory. It does not wipe decoded state, `pubky-noise` state, or
`snow` state, on any platform. Anything that crosses into JavaScript, such as
a `Uint8Array` or a string, cannot be wiped reliably.

The spike sealed the IndexedDB row with a non-extractable AES-GCM-256
`CryptoKey` kept in the same database (*verified* in three engines). The
stored row has no plaintext, and tampering fails authentication. This does not
stop a script in the origin from using the key.

Whether the seal protects against a copy of the browser profile depends on the
engine (*read* from each engine's source, not run). Chromium and Firefox write
the raw key bytes into the IndexedDB files; `extractable: false` restricts
only the WebCrypto API. A copied profile there holds both the sealed row and
its key. WebKit on Apple platforms wraps stored keys with a master key in the
Keychain, so a copied profile alone is not enough there.

The existing bindings specification requires "platform-protected storage" for
a device-local blob and does not define it. The `paykit-ffi` README explains
it only by example: "Keychain- or Keystore-backed encryption or equivalent".

**Options for protection at rest.**

1. Seal with a non-extractable WebCrypto key stored in IndexedDB.
2. Seal with a key derived from a user secret: a password, or a passkey
   through the WebAuthn PRF extension. This protects against profile theft and
   needs an unlock step. PRF support is uneven across authenticators.
3. Let the host supply the wrapping key or the whole store.
4. Run the SDK and its storage in a separate origin, in an iframe, so that
   host-page script cannot read keys. Browsers partition storage in
   third-party frames, the host needs a second deployment, and a host whose
   wallet seed already lives in the page origin gains nothing.

**Proposal.**

- State the threat model plainly: **any script that runs in the origin is
  fully trusted.** This includes cross-site scripting, a malicious dependency,
  and a browser extension with access to the page. The SDK cannot defend
  against it.
- Say what protection at rest does. Option 1 keeps plaintext out of the
  database and detects tampering. It defends against a copied profile only in
  engines that wrap stored keys, and never against code.
- Define "platform-protected storage" for browsers as option 1 at minimum.
- Make the wrapping key replaceable (option 3), so a host with an unlock step
  can seal with its own key. A host that needs protection against profile
  theft in Chrome, Edge, or Firefox must do this. Option 2 then becomes a host
  choice, not an SDK feature.
- The Paykit Identity Secret and session secret get the same protection as the
  state blob. A root-holding host protects the Pubky identity secret the same
  way it protects its wallet seed, behind the user's unlock step, and does
  not pass it to the SDK for normal operation (D1).
- Require, in the binding documentation, a Content Security Policy that blocks
  inline script and limits script sources. This policy cannot also restrict
  outgoing connections (D11), so it lowers the chance of script injection and
  does not contain one.
- Do not claim zeroization for anything that crosses into JavaScript.

**Decision needed (Owner: Paykit).** Is sealing with a non-extractable
WebCrypto key acceptable as the minimum, given what it does not protect? Is
"same-origin script is trusted" an acceptable written threat model for a
payments SDK?

### D3. State backing

**Context.** The host contract is two calls over one opaque blob: load, and
save if the revision is unchanged. A stale save must fail with a conflict and
write nothing. The
[bindings specification](paykit-sdk-bindings.md#stateblobstorageadapter)
allows several runtimes for one identity, provided they resolve to the same
logical blob with checked revisions. Separate device-local blobs are not a
valid way to share state.

Two backings ran in the browser (*verified*):

- **IndexedDB.** Compare-and-swap is one `readwrite` transaction that reads
  the row, compares the revision, and writes on a match. With strict
  durability the transaction completes only after the data is flushed. Two
  runtimes that raced produced exactly one conflict, and the retry kept both
  changes. The race ran in one JavaScript context, not in two tabs.
- **Pubky shared state.** The SDK encrypts the state and stores it on the
  identity's homeserver. The browser keeps no state blob. It still keeps the
  session secret and the Paykit Identity Secret, and every state operation
  needs the network. Each transaction holds a homeserver write lock that is
  granted for 60 seconds.

These two backings are not interchangeable (*read*):

- An IndexedDB blob exists in one origin of one browser profile. It is a
  valid backing only while no other runtime uses the identity: no mobile app,
  no second browser or profile, no Home Screen copy of the same web app, and
  no second origin. If the identity also runs anywhere else, every runtime
  must use the same logical state, which today means Pubky shared state.
- Shared state is in contract only on a homeserver that the app has verified
  runs 0.16.2 or newer with GCS storage on every serving instance. The SDK
  does not check this, the homeserver's `/info` response does not report it,
  and the SDK README states that unverified, user-selected homeservers are
  unsupported. A browser host can therefore use shared state only for
  identities on homeservers it has vetted. The spike ran against a testnet
  that stores files in memory, so it shows that the code path works in a
  browser and nothing about the storage guarantees.

Risks that are specific to the browser (*read* or *untested*):

- **Eviction.** The browser may delete IndexedDB data. While cross-site
  tracking prevention is on, which is the default, Safari deletes all
  script-writable storage of a site after 7 days of browser use without user
  interaction on that site. A web app added to the Home Screen has its own
  counter, which advances only on days the web app is used. If the
  authoritative state and every backup are lost, private state cannot be
  rebuilt, as the bindings specification already says.
- **Rollback of a local blob.** Nothing detects that a local blob was
  replaced by an older copy. A browser profile restored from a backup is such
  a rollback. Since commit `59ae3049` the SDK moves past message slots that
  are already stored on its homeserver before it encrypts. A rolled-back link
  therefore does not reuse a Noise key and nonce, under two conditions: the
  homeserver reports its slots truthfully, and none of the link's stored
  slots has been deleted, because the search stops at a gap. Receiving has no
  equivalent check: the SDK reads again every peer message after the
  rolled-back position and accepts it as new, because the deduplication
  records rolled back too. Rollback also loses Allowance usage and Payment
  Request claims, so a Payment Request can be handled a second time and an
  Allowance can be overspent. The wallet's own payment records are then the
  only check. A host that knows its storage was restored or copied must treat
  that as a backup restore.
- **Replay of shared state.** Shared-state encryption does not detect a
  homeserver that serves an older valid blob. The same homeserver answers the
  slot checks that prevent key and nonce reuse. In a browser that trust
  extends to every party listed in D4.
- **The write-lock clock.** The shared-state write lock needs a clock that
  keeps counting while the context is suspended. Native builds have one.
  Browsers do not guarantee one. The spike combined `performance.now()` and
  `Date.now()` and marked the result as needing security review. The
  homeserver also rejects an expired lock token (*verified* in three engines,
  HTTP 412).
- **A backup is a partial answer to eviction.** Restore needs an empty state
  backing and an explicit restored link policy. The SDK also sends on its
  own, so a backup is usually older than the last send or receive. Such a
  backup must be restored with `RequireRecovery`, and then every Linked Peer
  needs a new Encrypted Link Handshake.
- **The two stores in the spike differed.** The second one used default
  durability, a weaker authenticated-data string, and a key creation step
  that could lose a key between two openers.

**Options.**

1. The host implements the store. Paykit specifies the contract.
2. Paykit ships one reference IndexedDB store in the TypeScript package.
3. Browser hosts must use Pubky shared state.

**Proposal.**

- Support both backings, and document the trade-off above. IndexedDB plus
  exported backups is for an identity that lives in this one origin and
  profile. Pubky shared state is required when the identity also runs in
  another app, browser, or profile, and only on a homeserver the host has
  vetted. A host that accepts any user's homeserver cannot make shared state
  its default today.
- Ship one reference IndexedDB store (option 2). It is small, and the spike
  showed it is easy to get wrong. It is TypeScript in the package, not Rust,
  and the host can replace it. This goes beyond the current statements: the
  SDK specification lists "First-party durable mobile storage helpers" among
  the areas that remain separate, and the bindings specification treats
  storage as an app-provided integration point, although it does allow a
  "built-in platform storage adapter".
- Require of any IndexedDB store: one transaction per compare-and-swap, strict
  durability, a revision that is never reused, authenticated data that binds
  the row to its revision and database, and atomic creation of the sealing
  key.
- Call `navigator.storage.persist()` after a user action such as creating or
  restoring an identity, not on page load: Firefox shows a permission prompt,
  and Chrome and Safari decide silently. Treat a refusal as normal. A grant
  protects against eviction under storage pressure. It is not documented to
  stop Safari's 7-day deletion, and it does not replace backups.
- Before shared state is recommended in Firefox, answer the open question on
  stale cached reads (D11).

**Decision needed (Owner: Paykit).** Should Paykit ship a reference store?
Which backing is the documented default? Is the homeserver's check of the
write lock enough, or must the client clock be reviewed before shared state
ships in browsers?

**Decision needed (Owner: Pubky).** Can a client learn at run time that a
homeserver meets the shared-state requirements? Does shared state need replay
protection before a browser relies on it over WebPKI (D4)?

### D4. Transport trust

**Context.** *Read* from `pubky` 0.16.2:

- In a browser the client finds a homeserver through pkarr HTTP relays, takes
  the first HTTPS record that names an ICANN domain, and lets the browser
  connect to that domain with its normal certificate checks.
- A browser can never use PubkyTLS, the mode that verifies the homeserver's
  key directly. Native clients use PubkyTLS when they can and already fall
  back to the ICANN domain when they cannot.
- The mobile bindings on master already turn the DHT off, so they also depend
  on relays. Relay dependence is not new to the browser.
- pkarr record signatures are still verified in wasm. A relay cannot forge a
  record. It can withhold a record, serve an older valid one, and see which
  keys a client looks up. The client asks every configured relay at once (two
  by default) and, for homeserver lookups, takes the first acceptable answer.
  After a page reload there is no cached record to compare against.
- A homeserver is reachable from a browser only if it publishes an ICANN
  domain, has a certificate from a public certificate authority, and keeps
  its CORS headers through any reverse proxy. TLS then ends at the operator's
  proxy, not at the process that holds the homeserver key.
- This applies to each counterparty's homeserver as well as the user's own. A
  browser user cannot resolve, link with, or receive from a counterparty
  whose homeserver does not meet these requirements. A native client can
  reach it over PubkyTLS.

So in a browser, anyone who can present a valid certificate for a homeserver's
domain can read and change that homeserver's traffic. That means a certificate
authority that mis-issues, a domain or DNS takeover, or the TLS-terminating
proxy. What that party can do (*read*):

- **Redirect payments.** Public Payment Endpoints and the Paykit App Registry
  are not signed; only the Paykit Noise Key Authorization is. A party in
  front of a payee's homeserver can serve its own Payment Endpoints to a
  browser payer.
- **Use the user's session.** The session bearer is sent with every
  authenticated request and is valid for up to one hour. It can write
  everything under `/pub/paykit/`. With it the party can replace the user's
  public Payment Endpoints, delete the user's outgoing private message slots,
  and delete the shared-state blob or put back an older copy. These changes
  remain after the bearer expires. The grant's proof-of-possession key and
  the Pubky identity secret never cross the wire, so the party cannot obtain
  new bearers after the interception ends.
- **Not read or forge private messages.** Private Paykit messages stay
  end-to-end encrypted with Noise, and a peer's Noise key is checked against
  a record that the Pubky identity signs. The party can still withhold or
  replay message slots. With Pubky shared state it can also serve an older
  state blob and report used slots as free, which defeats the slot check
  described under Rollback in D3 and can make the SDK reuse a Noise key and
  nonce.

On native with PubkyTLS, only the homeserver itself can do these things. The
browser adds certificate authorities, whoever controls the domain, and the
proxy operator.

WebTransport is the one browser API that can authenticate a server by
certificate hash instead of WebPKI (*read*). It is not `fetch`, the homeserver
would need a new endpoint and short-lived certificates, and Pubky does not use
it. This document does not evaluate it.

**Proposal.** Accept this for browsers. Document the list above in the package
README next to the homeserver requirements. Let the host configure the pkarr
relay list. This is a new setting: `FfiPubkyClientConfig` has no relay field
today, and the mobile bindings depend on relays in the same way, so add the
field for all bindings in one change. Do not add a transport abstraction to
Paykit.

**Decision needed (Owner: Pubky).** Is WebPKI trust acceptable for the
browser product? Should a browser client wait for answers from more than one
relay and keep the newest record, at least on the first lookup after a page
load?

**Decision needed (Owner: Paykit).** Does Paykit ship a browser SDK on WebPKI
trust? Is the pkarr relay list a new field on every binding, or a web-only
setting?

### D5. Several tabs and one grant

**Context.** A homeserver keeps one bearer session per grant. Restoring an
exported session secret always exchanges the grant for a new bearer, which
replaces the previous one (*read*). When a second tab restores the same
session secret, the first tab's requests fail with HTTP 401 (*verified* in
three engines).

The grant is not revoked and the first tab is not signed out. Without a
coordinator the Pubky client does not retry on 401. It gets a new bearer only
at its next scheduled refresh (bearers last one hour by default and are
refreshed five minutes before expiry), and that refresh in turn replaces the
other tab's bearer (*read*). Two live runtimes on one grant therefore keep
breaking each other, not only at restore time. Master already documents the
rule for mobile: use separate grants for independent handles, or share live
access.

In the spike, `sync_public_endpoints` in the first tab reported one failed
endpoint and raised no error.

*Read*: Pubky has a mechanism for this. A coordinator hook lets all tabs share
one bearer and refresh it under a lock, and the Pubky JavaScript SDK backs it
with Web Locks and IndexedDB. In the Rust crate the hook is `#[doc(hidden)]`.
The spike did not use it.

A host that also loads the Pubky JavaScript SDK runs two Pubky clients,
because the Paykit wasm contains its own. They cannot share a session object.
If both restore the same grant, they replace each other's bearer.

The spike prototyped a different answer (*verified*): one SDK owner per
identity, chosen with a Web Lock. Other tabs send their calls to the owner
over a `BroadcastChannel`. When the owner closes, another tab takes over in 16
to 25 ms with its state intact. The prototype had no acknowledgement and no
deduplication of requests, and takeover was tested only for an owner that
closed.

**Options.**

1. Document the rule and leave it to the host.
2. One SDK owner per origin and identity, with a helper in the TypeScript
   package.
3. Share one bearer between runtimes with Pubky's coordinator. Several
   runtimes then run at once and rely on storage revisions.
4. A separate grant for each tab. Only a root-holding host can do this, and
   grants accumulate.

**Proposal.** Option 2 as the documented model: **at most one live SDK
runtime per origin and identity in one browser profile.** It also removes
cross-tab storage conflicts, and it will match hosts that run their engine in
one worker once that worker type is supported (D8). Provide the owner election
as an optional TypeScript helper, not in Rust. Adopt option 3 as well when
Pubky makes the coordinator a stable API, because it fixes the 401 at its
source.

The rule is not enforced. A host that runs two runtimes on one grant sees HTTP
401 in the older one, and concurrent saves surface as revision conflicts.
Another browser profile, or a Home Screen copy of the web app, is a separate
device for this rule and needs its own grant.

The owner helper must define what a relayed call means when the owner goes
away. `BroadcastChannel` gives no delivery guarantee, so a caller whose owner
closed cannot tell "not run" from "run, reply lost". The helper must report
such a call as having an uncertain outcome and must not resend a call that
changes state. Every same-origin context can post to the channel and read its
replies, so secrets must not be call arguments or results. A frozen owner may
keep its Web Lock. That case is *untested* and must be covered before the
helper ships.

A related change for every binding: report a session that the homeserver
rejects with 401 as an error that the host can act on, not only as a failed
count. Under the one-owner rule, an unexplained 401 can mean that a copy of
the session secret is in use elsewhere, and the host should offer to revoke
the grant. This changes results on Swift, Kotlin, and React Native as well, so
it needs its own issue.

**Decision needed (Owner: Paykit).** Is "one runtime per origin, identity,
and profile" acceptable as a browser rule? The bindings specification allows
several runtimes elsewhere. Should a rejected session (HTTP 401) become an SDK
error on all bindings?

**Decision needed (Owner: Pubky).** Will the grant session coordinator and
the delegated proof-of-possession signer (D2) become stable APIs?

### D6. Private intake

**Context.** *Read* from master:

- Supervised private intake is the only long-running background work in the
  SDK. It holds open event streams, renews a lease every 30 seconds, and
  audits every 60 seconds by default.
- The lease lasts 2 minutes and is stored in the state blob. It is released by
  a storage transaction after the supervisor's tasks stop. That release is
  not retried. There is no takeover API.
- The supervisor is not exposed through `paykit-ffi`. Swift and Kotlin use
  legacy polling. The SDK specification says bindings need "explicit
  long-lived task, notification, stop, and health semantics" first.

*Verified* in the spike, through the hand-written facade only:

- Legacy polling (`receive_private_messages`) works in all three engines.
- Intake over the event stream works in all three engines.
- After a page reload, intake could not restart until the old lease expired.
  The measured wait was 116 s, because the page was reloaded 5 s after the
  lease was claimed. In general the wait is 90 to 120 s: each renewal sets
  the expiry to 2 minutes from that moment, and renewals are 30 seconds
  apart. Releasing the lease in a `pagehide` handler did not help.
- A prototype takeover restarted intake in 54 to 118 ms. The new page first
  takes a Web Lock, which proves that no other context in the origin is
  alive. This is sound only when the state is device-local. With Pubky shared
  state the lease may belong to another device.
- A context frozen for longer than the lease returns with a stopped
  supervisor.

*Read*: a service worker makes supervised intake harder. Chrome stops a
service worker 30 seconds after its last event finishes. An event kept open
with `waitUntil()` can run for up to 5 minutes, and a `fetch` keeps the worker
alive only while such an event is open. Firefox stops it about 30 seconds
after the last event, with at most 30 more for pending work. WebKit's source
has no fixed idle timer: it stops a worker about 10 seconds after the origin's
last page goes away, and it can also stop idle workers under memory pressure.
A poll that runs inside a wake-up event therefore has up to 5 minutes in
Chrome and about 60 seconds in Firefox.

The event-stream path filter bug in
[issue #435](https://github.com/pubky/paykit-rs/issues/435) affects native
intake too and is handled separately.

**Options.**

1. Legacy polling only. The host calls the receive API on its own schedule,
   for example on a timer while visible, and later on service worker wake-ups
   (when D8 supports them). Each poll makes at least one request to the
   homeserver of each Linked Peer.
2. Expose supervised intake in the browser binding, with a release or
   takeover API for device-local state.
3. Shorten the lease.

**Proposal.**

- First release: option 1. It matches Swift and Kotlin, needs no long-lived
  context, and works with every execution context in D8. Message latency is
  the host's polling interval.
- Later: expose supervised intake after the SDK specification defines its
  lifecycle, on all bindings at once so that the lifecycle is specified once.
  Add two things then: the lease expiry time in the "already running" error,
  so a host can wait a known time, and a takeover that the host may call only
  for device-local state when it can prove it is the only live context. A
  host on Pubky shared state cannot use the takeover and waits out the lease.

**Decision needed (Owner: Paykit).** Is polling-only intake acceptable for the
first browser release? Should supervised intake wait for all bindings, or come
to the browser binding first?

### D7. Request deadlines

**Context.** Project policy is that timeouts belong to the caller and the
Pubky client, and that the Pubky client's `request_timeout` is the tool. On
wasm, `request_timeout` and `read_timeout` are deprecated setters that do
nothing (*read*). The mobile binding's 30-second default therefore has no
effect in a browser.

The gap is narrower than "everything hangs" (*read*): pkarr relay requests
and the homeserver `/info` probe are already limited to 5 seconds on wasm.
Ordinary storage, session, and event-stream requests have no limit. These
include reads from counterparty homeservers, at a domain that the
counterparty's pkarr record names. A counterparty can therefore make a call
wait for as long as its server holds the connection open.

`reqwest` 0.13.2 supports a per-request timeout on wasm, built on
`AbortController`. The Pubky comment says it is waiting for a client-wide
timeout, which `reqwest` does not have on wasm.

A deadline must not cover a streaming response body, because aborting also
ends the stream. A deadline for an event stream has to end when the response
headers arrive.

*Verified*, in the facade only:

- Dropping the Rust future after a deadline rejected the call and closed the
  connection. The prototype covered three calls.
- A wrapper around the global `fetch` that aborts when the response does not
  start in time bounded a hung read with no Rust change, in three engines. It
  does not bound a body that stalls later, and it was not run against an
  event stream. It replaces `fetch` for everything in its context, so on a
  page it also affects the host's own requests.

**Options.**

1. Pubky implements `request_timeout` on wasm with per-request timeouts.
2. The Paykit binding adds a deadline around each call. This changes the
   policy, and a call that is dropped mid-transaction has an uncertain
   outcome.
3. The host cancels calls itself. A cancelled call has the same uncertain
   outcome as in option 2.
4. The TypeScript package offers an opt-in `fetch` wrapper with a
   time-to-response limit.

**Proposal.** Option 1. It keeps the policy intact and fixes every Pubky user
on wasm.

Until it ships, document the gap. Offer option 4 as opt-in for hosts that run
the SDK in a dedicated worker, where the wrapper is contained. Hosts may also
pass an `AbortSignal` to read-only calls. For a call that changes state or
runs the payment adapter, an abort leaves an uncertain outcome, and the host
must inspect durable records before it retries. The bindings specification
says that a cancelled call should still finish or fail its durable
transaction consistently. Whether the wasm build does so when its future is
dropped is *untested* and must be tested before the first release.

If the upstream fix cannot land before the first release, use option 2 as a
stopgap, at least for requests to counterparty homeservers.

**Decision needed (Owner: Pubky).** Will `request_timeout` be implemented on
wasm?

**Decision needed (Owner: Paykit).** If the upstream fix misses the first
release, may the browser binding add its own deadline as a temporary exception
to the no-deadline policy?

### D8. Where the SDK runs

**Context.**

- *Verified*: the SDK runs on the page's main thread and in a dedicated
  worker.
- *Verified*: a Rust panic leaves the calling promise pending forever. A panic
  inside a storage transaction also blocks every later storage call on that
  runtime. Ending the worker and opening a new runtime from IndexedDB took 36
  to 118 ms.
- *Untested*: a service worker. *Read*: no service worker, classic or module,
  can use dynamic `import()`. A classic one also cannot use `import.meta` or
  static `import`, and `importScripts()` only loads scripts that were fetched
  during installation. A module one cannot use top-level `await`, and Firefox
  supports module service workers only since version 147. So the wasm loader
  must take a URL, bytes, or a compiled module from the host, and each event
  handler must await wasm start-up. Idle limits are in D6.
- *Read*: a shared worker reached Chrome for Android only in version 148, and
  Android may end it under memory pressure.

**Proposal.**

- Support a dedicated worker (recommended) and the page's main thread.
- Recommend the worker because a panic can be recovered by replacing the
  worker, and because SDK work stays off the main thread.
- Install a panic hook that reports the panic to the host, so the host can
  replace the runtime.
- Document an interrupted call as crash recovery: the host inspects durable
  records, then retries after the lease expires. The bindings rule that a
  cancelled call still finishes or fails consistently cannot apply when the
  context is gone.
- Treat the service worker as unsupported until it has its own test run. With
  polling-only intake (D6) the SDK has no background work, which is the
  precondition for running in a context that the browser stops at will. The
  test run is part of rollout step 4, directly after the first release.
- Consequence: until that run passes, a host whose engine lives in a service
  worker must run the SDK in a page or a dedicated worker and use the owner
  helper (D5) across tabs. If that is not acceptable for the first
  integrator, move the service worker test run into rollout step 3 so that it
  gates the first release.
- The wasm loader must accept a URL, bytes, or a compiled module from the
  host. It must not rely on `import.meta.url`. When the host passes a URL,
  its server must send `Content-Type: application/wasm`, or the loader must
  fall back to fetching bytes.

**Decision needed (Owner: Paykit).** Is "worker or page, service worker
later" acceptable for the first release, given the first integrator's design?

### D9. Binding toolchain and package

**Context.**

- The spike has two bindings. One is a hand-written `wasm-bindgen` facade that
  the 42-step run used. The other is TypeScript generated from the existing
  UniFFI definitions by `uniffi-bindgen-react-native` (ubrn), the generator
  that already produces the React Native package.
- Master pins UniFFI 0.29 and ubrn 0.29.3-1. On that version, with
  `wasm-bindgen` 0.2.115, an async callback that returns a value crashes. The
  spike patched the generated code. ubrn fixed this in 0.31.0-3 (*read*).
- ubrn 0.31.0-5 added a second web mode, `wasm2` (*read*, *untested* with
  Paykit). It needs no generated Rust crate, reads the interface from the
  `.wasm` file, and takes the wasm from the host as a URL, bytes, a
  `Response`, or a compiled module. It adds two runtime packages to what
  Paykit ships, and it changed incompatibly in 0.31.0-7, so bindings and
  runtime packages must be upgraded together.
- ubrn 0.31.0-7 also adds a package that keeps the bindings on the page and
  runs the wasm in a worker (*read*, *untested*). A synchronous callback
  method cannot cross that channel, so using it requires the async callbacks
  in D10. The spike did not use it: it loaded the binding and the host
  callbacks inside the dedicated worker.
- ubrn 0.31 needs UniFFI 0.31. UniFFI 0.31 changes method checksums, so the
  Swift, Kotlin, and React Native bindings must be regenerated together. The
  same upgrade unblocks 32-bit Android for React Native.
- The generated web API and the React Native API come from the same
  definitions, so shared exports have the same names. In the spike, the web
  build added async callback types and a panic hook and left out grant
  logout.
- Size, measured once with size optimization: 5.36 MB raw and 2.03 MB gzip
  for the generated binding, and 55 KB gzip of TypeScript glue. This is not a
  floor. The build profile is not committed.
- A host that also loads the Pubky JavaScript SDK carries the Pubky client
  twice.

**Options.**

1. Ship the generated TypeScript on today's toolchain, with the patch.
2. Upgrade to UniFFI 0.31 and ubrn 0.31 first, then ship the generated
   TypeScript, with either (a) the existing web mode or (b) `wasm2`.
3. Ship the hand-written facade.

For packaging: a separate npm package, or a browser entry in the existing
React Native package.

**Proposal.**

- Option 2b, with 2a as the fallback if a test of `wasm2` with Paykit fails.
  One generator gives one API on web and React Native, and `wasm2` removes
  the spike's build workarounds and the loader problem in D8. Do the upgrade
  as its own change, verified on native first. Nothing here ran on 0.31, so
  run the `wasm2` test before committing to it.
- Do not ship the facade. Port its 42-step flow to the generated TypeScript as
  the browser end-to-end test.
- Publish a separate npm package. The React Native package declares `react`
  and `react-native` as peer dependencies, and its `exports` map has one
  module entry with no browser condition, which web-only consumers should not
  have to work around. The cost is that an app that targets both web and
  React Native from one codebase, as the first integrator does, imports two
  Paykit packages. Merging the two packages can be revisited later.
- Add a CI check that lists the exports that differ between the web and React
  Native TypeScript, so that differences are deliberate.
- Use a target condition, not a cargo feature, for browser-only exports if
  `wasm2` allows it. The spike's `web` feature made every `--all-features`
  build compile the browser variant.

**Decision needed (Owner: Paykit).** Upgrade first, or ship on 0.29 with the
patch? Separate package, or one package?

### D10. Async host callbacks

**Context.** Paykit's three host callback interfaces (state store, session
provider, payment adapter) have synchronous methods. UniFFI has supported
async methods on these since 0.27; the synchronous shape is Paykit's choice.

In a browser, IndexedDB, WebCrypto, and `fetch` are asynchronous only, so a
synchronous callback cannot read or write durable storage when it is called. A
durable synchronous state store is therefore not possible. The bindings
specification already notes the same limit for React Native stores. A
synchronous session provider works only if the host already holds session
access in memory. The spike ran one that way, with a synchronous payment
adapter, through the generated TypeScript (*verified* in Chrome). A payment
adapter in a web wallet is asynchronous in practice.

The spike added async versions of the store and the session provider for the
web build only (*verified* in Chrome through the generated TypeScript). The
payment adapter stayed synchronous. One of the two new constructors was never
run.

**Options.**

1. Async variants on web only. The web build then has callback types and
   constructors that no other platform has, to maintain and document.
2. Make the three interfaces async on every platform. This is a breaking
   change for Swift, Kotlin, and React Native.
3. Add async variants beside the synchronous interfaces on every platform.
   This is additive, so Swift and Kotlin do not break, and web and React
   Native keep one export set. React Native could then use asynchronous
   stores (*untested*). The spike did not try this.

**Proposal.** Async variants for all three interfaces. Option 1 is the
smallest change and what the spike ran. Option 3 gives one export set and
helps React Native. Decide between them before rollout step 2, because step 2
regenerates every binding and comes before the browser binding. This document
leans to option 3 if the `wasm2` test in D9 shows no obstacle.

**Decision needed (Owner: Paykit).** Web-only async callbacks, async variants
added on every platform, or one async shape for every platform?

### D11. Supported browsers and unsupported environments

**Context.** *Read*, except where marked:

- Features the browser build needs, with the first version that has each:

  | Feature | Chrome | Firefox | Safari |
  | --- | --- | --- | --- |
  | `'wasm-unsafe-eval'` in CSP | 97 | 102 | 16 |
  | Web Locks | 69 | 96 | 15.4 |
  | IndexedDB strict durability | 83 | 126 | 15 |
  | `BroadcastChannel` | 54 | 38 | 15.4 |
  | Module dedicated worker | 80 | 114 | 15 |

  The table records what the build depends on. It does not set the support
  floor.

- **Content Security Policy.** The policy that counts is the one on the
  context that runs the SDK. A dedicated worker or service worker loaded from
  a URL uses the `Content-Security-Policy` header of its own script response,
  not the page's policy. A policy that restricts scripts must include
  `'wasm-unsafe-eval'` in `script-src`. It allows WebAssembly compilation and
  does not allow JavaScript `eval`. A policy that sets `connect-src` must
  allow the pkarr relays, the user's homeserver, and the homeserver of every
  counterparty. Those domains come from pkarr records and cannot be listed in
  advance, so in practice the policy needs `connect-src https:`. That
  directive then no longer limits where injected script can send data.
- **Lockdown Mode** on Apple devices turns off WebAssembly, IndexedDB,
  service workers, and Web Locks in WebKit. The SDK cannot run there.
- **HTTP cache.** Homeserver public reads send `Cache-Control: private,
  must-revalidate` with `ETag` and `Last-Modified`, and no `max-age` or
  `Expires`. `must-revalidate` applies only after a response is stale, so a
  cache may still treat the response as fresh for a heuristic time. Firefox
  does this: it served the old value right after an update and the new value
  5 seconds later (*verified*). WebKit's source has the same heuristic, but
  Playwright WebKit returned fresh reads in the same probe (*verified*), so
  Safari itself is unconfirmed. Chromium skips the heuristic when
  `must-revalidate` is present and returned fresh reads (*verified*).
- Firefox resolves the event-stream request only when the first bytes
  arrive, and the homeserver sends its first keep-alive after 15 seconds
  (*verified*).

**Options for the support policy.**

- A. A feature floor: any browser that passes the support check, tested on
  the latest stable Chrome, Firefox, and Safari.
- B. A version window per engine. Chromium (Chrome and Edge): the current
  Stable and the current Extended Stable milestone. Chrome has shipped a new
  major version every two weeks since version 153, so "current and previous"
  alone would be a four-week window. Firefox: the current release and the
  current ESR. Safari, and every browser on iOS and iPadOS: the current and
  previous major version.

**Proposal.**

- Option B. Mobile browsers are in scope but untested; they count as
  supported only after the rollout step 4 runs pass. Test Chrome, Firefox,
  and WebKit in CI. Test Safari itself before the first stable release
  (rollout step 4).
- Export a support check that tests for a secure context, WebAssembly,
  WebCrypto, IndexedDB, and Web Locks, and reports which one is missing, so a
  host can explain why Paykit is unavailable. Lockdown Mode is unsupported.
- Document the Content Security Policy requirements.
- Ask Pubky to fix stale reads at the source, with `Cache-Control: no-cache`
  on homeserver public reads or a cache mode in the Pubky client. `reqwest`
  on wasm already has a per-request `fetch_cache_no_store()`. Until then the
  only workaround is to replace the global `fetch`, as the spike did, because
  the Pubky client and `pubky-noise` build the requests, not Paykit. In a
  dedicated worker that the SDK owns this is contained. On the page it
  changes `fetch` for the whole host app, so the package must not do it there
  without the host's consent. It can share the opt-in wrapper in D7.
- Ask Pubky for an initial comment on the event stream, which removes the
  15-second delay.

**Decision needed (Owner: Paykit).** Confirm the support policy.

**Decision needed (Owner: Pubky).** Where should the stale-read fix go, and
can it ship before the first browser release?

## Rollout

1. **Groundwork.** The changes in [Groundwork](#groundwork), one pull request
   each, plus the upstream fixes to `snow` and `pubky-noise`. After this,
   `paykit-lib` and `paykit-sdk` build for `wasm32-unknown-unknown` and CI
   keeps it that way. `paykit-ffi` follows in step 3.
2. **Toolchain.** UniFFI 0.31 and ubrn 0.31 for the existing bindings (D9).
3. **Browser binding.** The browser build of `paykit-ffi`, async callbacks
   (D10), the TypeScript package with the reference store (D3) and the owner
   helper (D5), and the browser end-to-end test. Published as a pre-release.
   This is the first release.
4. **Hardening.** A real HTTPS homeserver, Safari, mobile browsers, a Content
   Security Policy, and the service worker test run (D8). The first stable
   release follows this step.
5. **Later.** Supervised intake (D6), and whatever v0.2 changes (D0).

Step 1 depends only on D0: options 1 and 3 land it now, option 2 does not.
Step 2 depends on D9, and D10 must be settled before step 2 starts. Step 3
depends on every Paykit decision, and on Paykit's side of D4 (the relay list
setting) and D7 (the stopgap deadline).

## Open questions

These are not decisions yet, because the facts are missing.

- Do the production homeservers publish an ICANN domain, and keep CORS
  headers through their reverse proxy? Do they meet the shared-state contract
  (0.16.2 or newer, GCS on every instance)? The spike used a local testnet
  only.
- Does a streaming response keep a service worker alive in Firefox and
  Safari?
- Does cancelling a generated-binding call with an `AbortSignal` abort the
  `fetch` underneath?
- Does a stale cached read in Firefox (seen in the spike) or Safari (not seen
  in Playwright WebKit) break the compare-and-swap of Pubky shared state?
  This comes from reading the code and was not reproduced.
- How does a delegated app learn the new Paykit Identity Secret after a key
  rotation? This is unanswered on every platform.
- How long is a peer or Paykit App blocked after a reload in the middle of a
  call, and should the takeover in D6 also cover those leases?
- Sign-up plus initialization took either about 0.5 s or about 6.6 s in the
  spike. The split does not follow the binding or the engine. In the slow
  runs the client makes three pkarr relay lookups for the new, unpublished
  key, one after another, and the testnet relay takes about 2.1 s to answer
  each. Why the relay is sometimes slow is unknown. Measure a lookup of an
  unpublished key against the production relays, because it sets the sign-up
  time a user sees.
- What is a reasonable size budget for the wasm file?
