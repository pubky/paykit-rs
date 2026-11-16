# Browser (WASM) spike for issue #411

Throwaway branch `throwaway/wasm-browser-sdk-411`, based on `ed92032b`.
Do not merge this branch. Use it as evidence and as a source of small,
separately reviewed PRs. Every hack in the code carries a
`THROWAWAY (#411):` comment (`git grep -n "THROWAWAY (#411)"`).

## Result

The full Paykit SDK compiles for `wasm32-unknown-unknown` and runs in a
browser. Everything below was verified by running it against a local
`pubky-testnet` 0.16.2 StaticTestnet.

| What ran | Chrome 157 | Firefox 157 | WebKit 27.2 |
|---|---|---|---|
| Offline wasm suite (18 tests), page and worker | pass | pass | pass |
| Full SDK e2e via `paykit-wasm` facade (42 steps), page and worker | 42/42 | 41/42, 42/42 with `cache: "no-store"` | 42/42 |
| Generated ubrn TypeScript e2e (21 steps), page and worker | 21/21 | not run | not run |

The e2e covers sign-up, initialize, public Payment Endpoints, the
Encrypted Link Handshake, Private Payment Lists, private intake over SSE,
Payment Requests (accept, reject, cancel, conversion quote, proof),
Receipts and Receipt Access, Allowances and one accounting cycle, backup
export and restore, key rotation and link recovery, three storage modes
(in-memory, IndexedDB, Pubky shared state), and sign-out.

The native gate still passes on the branch: paykit-lib 415, paykit-sdk
lib 1047, testnet_e2e 185, paykit-ffi 156 + 4 (default) and 155 + 4 (all
features), doctests, clippy, rustdoc, MSRV 1.91.1, and iOS and Android
cross-checks. Native UniFFI Swift, Kotlin, C header and exported symbols
are byte-identical to master.

## Workarounds on this branch

### Dependencies

| Problem | Workaround | Production fix |
|---|---|---|
| getrandom 0.2, 0.3 and 0.4 have no browser backend by default | wasm32 target deps with `js` / `wasm_js` in paykit-lib and paykit-sdk; `getrandom_backend` cfg appended to `.cargo/config.toml` (probably unnecessary for 0.3.4+) | Keep, per crate |
| uuid needs a randomness source | `uuid` `js` feature on wasm32 | Keep |
| `ring` (C and asm) appears via snow | snow 0.10.0 `std` feature lists `ring/std` without `?`, which enables optional ring. Vendored snow with `ring?/std` | Upstream one-line fix to snow, then drop the vendor patch |
| chrono `Utc::now()` panics | chrono `wasmbind` on wasm32 | Keep. Workspace dev-deps hide this in `cargo check`; CI must check each crate alone |
| pubky-noise uses `Response::chunk()`, missing on wasm reqwest | Vendored pubky-noise, wasm-only `bytes_stream()` reader that keeps the size bound | Upstream to pubky-noise |

### paykit-lib and paykit-sdk

| Problem | Workaround | Production fix |
|---|---|---|
| `Response::chunk()` at 8 bounded-read sites | `paykit_lib::ResponseChunks`: `chunk()` native, `bytes_stream()` wasm. Bound still enforced | Keep as an internal helper |
| `reqwest::Error::is_connect()` missing | Returns false on wasm; error category becomes "transport failure" | Keep |
| ~70 `Send` errors: browser futures are `!Send` | `paykit_sdk::wasm_compat` (`MaybeSend`, `BoxFuture`, `async_trait(?Send)` on wasm32 only). 11 files, +77/-44 | Keep; native types unchanged |
| tokio timers, `Instant::now`, `tokio::spawn` panic at runtime | `paykit_lib::rt`: tokio re-export on native; `spawn_local` + `Abortable`, gloo-timers, `web_time::Instant`, suspend-aware lease clock on wasm | Keep, as an internal crate or `pub(crate)` |
| Threaded wasm would make the `?Send` relaxation unsound | `compile_error!` on `target_feature = "atomics"` | Keep |
| std `HashMap` seeds are memory addresses on wasm (HashDoS) | `paykit_lib::collections` aliases: std on native, getrandom-seeded SipHash-1-3 on wasm. 77 files converted | Keep, or use `BTreeMap` for attacker-keyed maps. Fail loudly if getrandom fails |
| Private intake SSE filter `/pub/paykit/v0/private` matched nothing (native bug too) | Trailing slash plus a testnet regression test | Separate PR to master after checking production homeserver versions |

### Bindings

| Problem | Workaround | Production fix |
|---|---|---|
| paykit-ffi does not build for wasm32 | `web` cargo feature: tokio `rt-multi-thread` native-only, `async_trait(?Send)`, `uniffi/wasm-unstable-single-threaded`, no `async_runtime = "tokio"`, grant logout gated, chrono `wasmbind` | Keep the feature. `--all-features` now builds the web flavor, so CI tests default features separately (done in ci.yml) |
| UniFFI foreign traits are sync; IndexedDB, WebCrypto and fetch are async-only | Web-only `FfiSdkAsyncStateBlobStore` and `FfiSdkAsyncPubkySessionProvider` and constructors `withAsyncCallbacks`, `withAsyncSessionProviderAndPubkySharedState` | Decision: web-only traits or async everywhere |
| ubrn 0.29.3-1 + wasm-bindgen 0.2.115 merge the 13 `JsCallbackFn` completion classes; value-returning async callbacks crash | Post-generation shim patch in `paykit-web/` | Upgrade to UniFFI/ubrn 0.31 (fixed upstream, not run) or pin wasm-bindgen in the generated crate |
| ubrn generated crate misses workspace patches, lock and resolver 2 | `paykit-web/cargo-patch.toml`, copied lockfile, prettier | Mostly fixed by ubrn 0.31 wasm2 |

### Browser runtime behaviour

| Problem | Workaround | Production fix |
|---|---|---|
| Pubky `request_timeout` is a no-op on wasm; hung homeserver hangs forever | Facade deadline via `rt::timeout` (aborts the fetch); fetch-shim header timeout | Decision: binding-layer deadline vs pubky upstream |
| IndexedDB blob held plaintext secrets | AES-GCM-256 non-extractable CryptoKey, fresh IV, revision as AAD | Protects at rest only, not against XSS or extensions |
| Two runtimes on one IndexedDB | CAS revision in one readwrite transaction; `ConcurrentUpdate` then retry | Keep; use strict durability |
| Second tab restoring the grant logs the first tab out | One SDK owner per origin via Web Locks, others RPC over BroadcastChannel (works in all 3 engines) | Decision: enforce in binding or leave to apps |
| Reload during intake locks intake out for ~116 s | Web-Lock-gated takeover of a stale lease: 54-118 ms | SDK API for host-signalled takeover |
| Firefox serves stale homeserver reads from HTTP cache | `cache: "no-store"` on every fetch | pubky client cache mode or homeserver `Cache-Control: no-cache` |
| Firefox resolves SSE fetch on first body byte; homeserver first keep-alive at 15 s | None | Homeserver sends an initial SSE comment |
| A wasm panic leaves the call pending and a held storage lock poisoned | Terminate the worker and reopen from storage (36-118 ms) | Run the SDK in a worker; ship the panic hook |

## Needs a human decision

1. Browser identity model. Paykit has no supported way to deliver the
   delegated Paykit Identity Secret to a browser. The spike signs up with
   a root key generated in wasm, which a real app should not do.
2. Where browser secrets live and the threat model. There is no
   Keychain or Keystore. Noise and Paykit secrets must exist as raw bytes in
   wasm memory. WebCrypto wrapping only protects at rest.
3. Transport trust drops from PubkyTLS to WebPKI in a browser.
4. Async foreign traits: web-only or for every platform.
5. Multi-tab policy and intake lease takeover.
6. Request timeout policy on wasm.
7. Binding path: ubrn-generated TypeScript (recommended; same API as React
   Native) or the hand-written facade.

## Not covered

- A real HTTPS homeserver and production pkarr relays.
- Sign-in through an external signer such as Pubky Ring.
- Real tab freeze, discard, OS sleep, intensive background throttling.
- Safari itself (WebKit was Playwright's build), mobile browsers.
- IndexedDB eviction, ITP, private mode, rollback protection.
- Strict Content-Security-Policy.
- Profiles and avatar upload, Public Contact Markers, contact removal and
  blocking, app removal, reservations.
- The generated ubrn TypeScript only covers public endpoints, Encrypted
  Links and Private Payment Lists.
- `.github/workflows/wasm.yml` has never run.

## Reproduce

See `paykit-wasm/build.sh`, `paykit-wasm/www/run-chrome.mjs`,
`paykit-web/build.sh`, and `.github/workflows/wasm.yml`. The local
testnet needs `pubky-testnet` 0.16.2 and a postgres reachable through
`TEST_PUBKY_CONNECTION_STRING`. Use a fresh Chrome profile per run.
