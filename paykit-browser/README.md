# paykit-browser

`paykit-browser` holds the browser (`wasm32-unknown-unknown`) build support for the Paykit SDK,
the first host pieces a browser host gives the SDK and the Switch client, and the tests that
run Paykit v0.2 code in browser engines.

Depending on this crate selects the browser random number backends (`crypto.getRandomValues`)
for every getrandom version in a `wasm32-unknown-unknown` build of Paykit. On any other target
it builds as an empty crate, so workspace-wide commands keep working.

## Host pieces

On `wasm32-unknown-unknown` the crate holds these Rust items for a host written in Rust. Each
works in a page and in a dedicated worker. None is exported to JavaScript. Other wasm32 targets,
such as WASI, have no JavaScript host, and do not get them.

- `clock::JsClock`: the Switch client's `Clock`, on `Date.now()`. It is the browser's wall
  clock, which can jump when the time is changed. Use it instead of the Switch client's
  `SystemClock`, which panics on `wasm32-unknown-unknown`.
- `timer::JsTimer`: the SDK's `Timer`, on `setTimeout`. A sleep's delay counts from its first
  poll. It never ends before its delay as `performance.now()`, not the wall clock, reads it;
  where a browser coarsens that clock, it can end up to one step of it early in real time.
  Browsers can also fire timers late, most of all in hidden pages. A delay
  above `setTimeout`'s limit of `2^31 - 1` ms takes several timeouts, and dropping a sleep
  clears its timeout.
- `send::run_local`: starts a future that waits on the browser, which is not `Send`, on this
  thread's event loop, and returns a `Send` future for its output, as the host traits require.
  Dropping the returned future cancels the local one. The module docs say why this is sound.
- `panic::install_panic_hook`: reports a Rust panic, with its message and source location, to
  `console.error`, and then runs the panic hook installed before it.

## Target

Single-threaded `wasm32-unknown-unknown`. A build with WebAssembly threads (the `atomics`
target feature) is refused at compile time.

## What this covers, and what it does not yet

- The `wasm32-unknown-unknown` dependency graph of `paykit-lib` and `paykit-sdk` builds.
- The tests listed under [Tests](#tests) run in a page and in a dedicated worker, in Chromium,
  Firefox and WebKit.
- The host pieces above are tested on their own. The Payment Request flow below gives its apps
  `JsTimer` as their `Timer`, which it never sleeps on (the SDK waits on its timer only when it
  pays), and starts its test clock at `JsClock`'s reading.
- A Payment Request flow of the SDK runs in one WebAssembly instance, with no network: a payer
  and a payee each join a Paykit App through their signer, on the Switch client's in-memory
  scripted switches. The payer invites the payee, the payee sends a one-time Payment Request,
  and the payer receives it and reads it back as open, with the amount, payment reference and
  expiry that were sent, its activity record holding the payee's signed request with its
  Payment Endpoint. The flow also shows a request expire, a rejection refused a second
  time, and a request that waits until the payer admits the payee.

Not run in a browser yet:

- paying a Payment Request, and so Payment Proofs and Receipts (see the hazards below);
- a real network: the flow's apps reach the scripted switches in memory, and this crate holds
  no Switch transport;
- service workers and shared workers: only a page and a dedicated worker are tested.

Two known run-time hazards remain on `wasm32-unknown-unknown`, where the standard library has
no clock and `std::time::Instant::now()` panics:

- `paykit-sdk` calls `Instant::now()` in `execute` (`src/pay.rs`) before it submits a payment
  attempt. `Paykit::pay_request`, `Paykit::pay_request_automatically`,
  `Paykit::pay_without_request` (while the payer's switch is available), `Paykit::pay_period`
  and `Paykit::pay_period_automatically` dispatch their payments through it. It also calls
  `Instant::now()` in `resolve_dispatched` (`src/pay.rs`), before it reads the claim of a
  payment that was left in progress.
- The Switch client calls `Instant::now()` in `FirstSend::new`. It builds a `FirstSend` only
  when it submits a batch that sets a first-send deadline (`first_send_before`). In
  `paykit-sdk`, only `record_offline_attempt` (`src/pay/offline.rs`) sets one: it records a
  payment that `Paykit::pay_without_request` made to a public endpoint while the payer's switch
  was unavailable. Other writes do not reach it.

## Stack

A `wasm32-unknown-unknown` build gets a 1 MiB stack by default, and a stack overflow there
traps with an out-of-bounds memory access. The `tests/request_flow/` target passes on that
default. Measured with `-zstack-size`, in steps of 64 KiB for the debug build and 32 KiB for the
release build, in Chromium, Firefox and WebKit, in a page and in a dedicated worker, with the
same result in each:

| Build | Smallest stack that passes | Largest stack that fails |
| --- | --- | --- |
| debug (`cargo test`) | 832 KiB | 768 KiB |
| `--release` | 160 KiB | 128 KiB |

So the debug test build uses most of the default stack. This crate sets no stack size of its
own. A web app that links Paykit into its own WebAssembly module sets that module's stack size
when it links it, and should measure its own call paths.

## Tests

There are four test targets.

The tests in `tests/conformance/` build for both the host and wasm32, with the same test
bodies, so a browser result is compared with the native result by construction. They cover:

- a subset of Paykit's normative vectors in `specs/v0.2/vectors/`, compared byte for byte
  (paykit-lib's native tests check the complete set);
- the operating system random generator, through both getrandom versions in the graph;
- signing and verifying, sealing and opening record values, and sealing and opening
  envelopes.

The tests in `tests/host/` cover the host pieces, so they run only in a browser; natively the
target is empty. They check that:

- `JsClock` agrees with `Date.now()`;
- a `JsTimer` sleep lasts at least its delay, counted from its first poll, `sleep(0)` is ready
  at once, a sleep outlasts timers that fire early, and a dropped sleep clears its timeout;
- the steps of a sleep longer than `setTimeout`'s limit stay within it;
- `run_local` returns the output, starts the local future without a poll, cancels and drops
  the local future when the returned future is dropped, and its returned future is `Send`.

The test in `tests/panic_report.rs` checks that `install_panic_hook` writes a panic, with its
message and source location, to `console.error`, and then runs the panic hook installed before
it, also when it is called twice. It is a target of its own, so that its call is the first in
its WebAssembly instance, and so that the panic ends an instance no other test shares.

The tests in `tests/request_flow/` run the Payment Request flow described above, so they also
run only in a browser. They follow the SDK's native flow tests (`paykit-sdk/tests/flows/`),
on the Switch client's test API (`pubky_switch_client::testing`) and the SDK's `FakeRail`.
They check that:

- a one-time Payment Request is received once and reads as open, with the amount, payment
  reference and expiry that were sent, and that the payer's activity record stores the
  payee's signed request with its Payment Endpoint;
- a request reads as expired once the service's clock has passed its proposal expiry;
- a rejected request reads as rejected, a second rejection is refused (`Refusal::Rejected`),
  and the payee's shop sees the rejection;
- a request to a payer who has not admitted the payee waits for admission, and is accepted
  once the payer invites the payee.

### Run the tests natively

```sh
cargo test -p paykit-browser
```

### Run the tests in a browser

You need:

- the Rust target: `rustup target add wasm32-unknown-unknown`;
- `wasm-bindgen-test-runner`, from `wasm-bindgen-cli` at exactly the `wasm-bindgen` version in
  `Cargo.lock`:

  ```sh
  cargo install wasm-bindgen-cli --version <wasm-bindgen version in Cargo.lock> --locked
  ```

  The runner generates the JavaScript bindings the test module links against. A runner of any
  other version refuses the module, so `rig/test.sh` checks the version before it runs;
- Node.js 20 or later, and the rig's pinned Playwright with its browser builds:

  ```sh
  cd paykit-browser/rig
  npm ci --ignore-scripts
  npx playwright install chromium firefox webkit
  ```

Then run, from the repository root:

```sh
paykit-browser/rig/test.sh all both
```

The first argument is the engine: `chromium`, `firefox`, `webkit`, `chrome` or `all`. The second
is the context: `page`, `worker` (a dedicated worker) or `both`, which is the default. Further
arguments go to `cargo test`, for example a test name filter. A test target with no test that
matches the filter passes, but a run in which no test target runs a test fails.

To run one engine and context without `test.sh`, set `rig/runner.mjs` as Cargo's wasm32 runner
as its header comment shows; that comment also lists the runner's environment variables and
their defaults.

### The engines

`chromium`, `firefox` and `webkit` are the browser builds that Playwright pins for its version
in `rig/package-lock.json`. The WebKit build is Playwright's build of the WebKit engine, not
Safari itself. `chrome` is the Google Chrome installed on the machine, driven through
Playwright's Chromium support. `all` runs the three Playwright builds.

## Continuous integration

The `Browser` workflow (`.github/workflows/browser.yml`) runs on the same pushes and pull
requests as `CI`. Its `wasm-check` job runs `cargo clippy` for `wasm32-unknown-unknown`, which
also type-checks the build, with the clippy settings in `rig/wasm-clippy/`; `cargo check` of
the library alone, without the test-only features of the dev-dependencies; `cargo doc` for
wasm32, where the host pieces are; and then `rig/check-wasm-deps.sh`, which fails when the
wasm32 dependency graph contains a native networking or TLS crate such as `mio`, `ring` or
`tokio`.
Its `browser-tests` job runs once per engine (`chromium`, `firefox`, `webkit`), as three jobs;
each runs `rig/test.sh` in a page and then in a dedicated worker, with `wasm-bindgen-cli`
installed at the version in `Cargo.lock`.

The clippy settings in `rig/wasm-clippy/clippy.toml` reject `now` and `elapsed` of
`std::time::Instant` and of `std::time::SystemTime`, which compile for `wasm32-unknown-unknown`
and then panic there, and the Switch client's `SystemClock`, which reads `SystemTime::now()`
(use `clock::JsClock`). Only the `wasm-check` job reads them, through `CLIPPY_CONF_DIR`; native
clippy does not. With `-p paykit-browser`, clippy lints this crate and the workspace crates it
depends on, `paykit-lib` and `paykit-sdk`, as the browser build compiles them: code for other
targets and features that build leaves off are not checked, and neither is any dependency
from outside the workspace, such as the git dependency `pubky-switch-client`. To run the same
check from the repository root:

```sh
CLIPPY_CONF_DIR="$PWD/paykit-browser/rig/wasm-clippy" \
  cargo clippy --locked --target wasm32-unknown-unknown -p paykit-browser --all-targets -- -D warnings
```
