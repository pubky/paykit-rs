# paykit-browser

`paykit-browser` holds the browser (`wasm32-unknown-unknown`) build support for the Paykit SDK,
and the tests that run Paykit v0.2 code in browser engines.

Depending on this crate selects the browser random number backends (`crypto.getRandomValues`)
for every getrandom version in a `wasm32-unknown-unknown` build of Paykit. On any other target
it builds as an empty crate, so workspace-wide commands keep working.

## Target

Single-threaded `wasm32-unknown-unknown`. A build with WebAssembly threads (the `atomics`
target feature) is refused at compile time.

## What this covers, and what it does not yet

- The `wasm32-unknown-unknown` dependency graph of `paykit-lib` and `paykit-sdk` builds.
- The tests listed under [Tests](#tests) run in a page and in a dedicated worker, in Chromium,
  Firefox and WebKit.
- The SDK's flows are not run in a browser by this crate yet.
- Only a page and a dedicated worker are tested. Service workers and shared workers are not.

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

## Tests

The tests in `tests/conformance/` build for both the host and wasm32, with the same test
bodies, so a browser result is compared with the native result by construction. They cover:

- a subset of Paykit's normative vectors in `specs/v0.2/vectors/`, compared byte for byte
  (paykit-lib's native tests check the complete set);
- the operating system random generator, through both getrandom versions in the graph;
- signing and verifying, sealing and opening record values, and sealing and opening
  envelopes.

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
arguments go to `cargo test`, for example a test name filter.

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
also type-checks the build, then `rig/check-wasm-deps.sh`, which fails when the wasm32
dependency graph contains a native networking or TLS crate such as `mio`, `ring` or `tokio`.
Its `browser-tests` job runs once per engine (`chromium`, `firefox`, `webkit`), as three jobs;
each runs `rig/test.sh` in a page and then in a dedicated worker, with `wasm-bindgen-cli`
installed at the version in `Cargo.lock`.
