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
- `fetch::FetchTransport`: the Switch client's `Transport`, on the browser's `fetch`. Before it
  sends anything, it checks the switch origin with the Switch's own check (Switch wire 7.1),
  and refuses a host name of one label, and `local`, `internal`, `home.arpa`, `lan`, `home`,
  `corp`, `private` and `intranet` and every name under them; `FetchTransport::new` also
  refuses `localhost`, `localdomain` and `localdomain6` and every name under them, such as
  `localhost.localdomain`. It follows no redirect, and it sends the signed request bytes
  exactly or refuses the request. A browser does not say whether a failed `fetch` left, so a
  rejected `fetch` is `TransportError::Other`, never `Connect`. The switch origin must answer
  CORS requests and expose the signed response headers.
  `FetchTransport::allowing_localhost_for_loopback_tests` also allows those names, for tests
  against a service on the local machine only. The type's docs list what it does and what it
  cannot do.

## Target

Single-threaded `wasm32-unknown-unknown`. A build with WebAssembly threads (the `atomics`
target feature) is refused at compile time.

## What this covers, and what it does not yet

- The `wasm32-unknown-unknown` dependency graph of `paykit-lib` and `paykit-sdk` builds.
- The tests listed under [Tests](#tests) run in a page and in a dedicated worker, in Chromium,
  Firefox and WebKit.
- The host pieces above are tested on their own. The no-network flows below give their apps
  `JsTimer` as their `Timer`, and start their test clock at `JsClock`'s reading. The network
  flow gives its apps `JsClock` as their clock and `JsTimer` as their `Timer`.
- A Payment Request flow of the SDK runs in one WebAssembly instance, with no network: a payer
  and a payee each join a Paykit App through their signer, on the Switch client's in-memory
  scripted switches. The payer invites the payee, the payee sends a one-time Payment Request,
  and the payer receives it and reads it back as open, with the amount, payment reference and
  expiry that were sent, its activity record holding the payee's signed request with its
  Payment Endpoint. The flow also shows a request expire, a rejection refused a second
  time, and a request that waits until the payer admits the payee.
- The pay and receipt flow runs the same way, with no network: the payer pays the open
  one-time Payment Request through the SDK's `FakeRail`, the payee receives the Payment Proof
  and confirms it with a Payment Receipt, and the payer stores the receipt with the request it
  pays. It also shows a payment the rail refuses or fails, paid again afterwards, a request
  refused once its expiry has passed, a payment that settles late resumed once, and a payment
  left in flight asked again under the same idempotency key after its app is loaded again.
- `FetchTransport` sends over a real network, to a local rig that terminates TLS and answers
  CORS (see [Run the network tests](#run-the-network-tests)). Bodies and the Switch's request
  headers arrive unchanged, answers come back with their status and exact body, a redirect is
  not followed, an answer over the size limit and a call past its timeout fail, a refused
  origin sends nothing, and an origin that does not answer reads as an unknown outcome.
- The one-time Payment Request flow runs against a real Pubky Switch service, through
  `FetchTransport`. A payer and a payee, enrolled on the service with the real signer, each
  join a Paykit App with `Paykit::join`. The payer invites the payee, and the payee sends a
  one-time Payment Request, which the service accepts. The payer reads it as open, with the
  amount, Payment Endpoint, payment reference and expiry that were sent, and pays it through
  `FakeRail`. The payee receives the Payment Proof and confirms it, and the payer stores the
  Payment Receipt. The flow also shows a payment the rail refuses, paid again afterwards; a
  request sent before the payer admitted the payee, which waits for admission and is
  delivered once the payer invites the payee; and a request to an identity that never
  enrolled, which stays queued.

The network tests have these limits:

- A browser gives no way to check the addresses a host name resolves to (Switch wire 8.2):
  `FetchTransport::new` refuses local machine names and local network names, but not a public
  name that resolves to a private address.
- `fetch` merges repeated response headers into one value.
- A redirect answer is `TransportError::Other`, where the native transport returns it as an
  answer with its 3xx status: with `redirect: "manual"`, `fetch` hides that status.
- Trusting the rig's self-signed certificate is a rig setting: the runner ignores
  certificate errors in a `--switch` run only.
- Trusted time is off in the rig's service configuration, a test setting.
- The rig raises the service's per-source and per-minute limits.
- Nothing moves the real service's clock, so expiry and period flows are covered only by the
  no-network targets.
- Playwright's WebKit build is not Safari.
- A payment without a request to a public Payment Endpoint while the payer's switch is
  unavailable is still unsupported: see the hazard below.
- A real Homeserver is not part of the rig: each test loads the `identity.json` documents
  into an in-memory one.

Not run in a browser yet:

- payments other than a one-time Payment Request paid with `Paykit::pay_request`:
  `Paykit::pay_request_automatically`, `Paykit::pay_without_request`, `Paykit::pay_period` and
  `Paykit::pay_period_automatically` dispatch through the same code, but no test here runs
  them, and `pay_without_request` to a public Payment Endpoint while the payer's switch is
  unavailable meets the hazard below;
- service workers and shared workers: only a page and a dedicated worker are tested.

On `wasm32-unknown-unknown` the standard library has no clock: `std::time::Instant::now()` and
`std::time::SystemTime::now()` panic. `paykit-sdk` takes the pay path's monotonic time from the
`web-time` crate, which reads `performance.now()` there and is `std::time::Instant` on every
other target, and the `wasm-check` job rejects the std clocks in the code of `paykit-lib`,
`paykit-sdk` and this crate that the browser build compiles (see
[Continuous integration](#continuous-integration)). One known run-time hazard remains, outside
the workspace:

- The Switch client calls `Instant::now()` in `FirstSend::new`. It builds a `FirstSend` only
  when it submits a batch that sets a first-send deadline (`first_send_before`). In
  `paykit-sdk`, only `record_offline_attempt` (`src/pay/offline.rs`) sets one: it records a
  payment that `Paykit::pay_without_request` made to a public endpoint while the payer's switch
  was unavailable. Other writes do not reach it, and the flows here do not run it.

## Stack

A `wasm32-unknown-unknown` build gets a 1 MiB stack by default, and a stack overflow there
traps with an out-of-bounds memory access. Measured with `-zstack-size`, in steps of 64 KiB for
the debug build and 32 KiB for the release build, in Chromium, Firefox and WebKit, in a page and
in a dedicated worker, with the same result in each:

| Target | Build | Smallest stack that passes | Largest stack that fails |
| --- | --- | --- | --- |
| `tests/request_flow/` | debug (`cargo test`) | 832 KiB | 768 KiB |
| `tests/request_flow/` | `--release` | 160 KiB | 128 KiB |
| `tests/pay_flow/` | debug (`cargo test`) | 1216 KiB | 1152 KiB |
| `tests/pay_flow/` | `--release` | 192 KiB | 160 KiB |
| `tests/switch_flow/` | debug (`cargo test`) | 1216 KiB | 1152 KiB |
| `tests/switch_flow/` | `--release` | 192 KiB | 160 KiB |

The debug builds of the pay flow and of the network flow do not fit the default, so `build.rs`
gives this crate's wasm32 test binaries a 2 MiB stack, about 68 percent above the largest need
measured. It sets nothing for the library or for native builds. Its linker argument comes after
those of `RUSTFLAGS` and `cargo rustc -- -C link-arg=...`, so it wins over them, as the initial
value of `__stack_pointer` in each measurement binary showed. To measure again, edit the
`TEST_STACK_BYTES` const in `build.rs` to each size and build. A web app that links Paykit
into its own WebAssembly module sets that module's stack size when it links it, and should
measure its own call paths.

## Tests

There are seven test targets. Two of them, `tests/fetch_transport/` and `tests/switch_flow/`,
are network targets: they need a real Switch service, and run only as
[Run the network tests](#run-the-network-tests) describes. The others need no network, no
database and no service.

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
  the local future when the returned future is dropped, and its returned future is `Send`;
- `FetchConfig`'s defaults are those of the native transport, and `FetchTransport` refuses,
  before it calls `fetch`: an origin outside the Switch's rules, `localhost`,
  `localhost.localdomain` and the other local machine names in every spelling, a host name of
  one label, a name that is or ends in `local`, `internal`, `home.arpa`, `lan`, `home`,
  `corp`, `private` or `intranet`, and a request `fetch` would not send as signed. The
  loopback test transport allows the local machine names, but only over `https`, on allowed
  ports, and never a private address literal or another local network name. No request in
  these tests reaches the network.

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

The tests in `tests/pay_flow/` run the pay and receipt flow described above, in the same world
as `tests/request_flow/` (`tests/world/`), so they also run only in a browser. They follow the
SDK's native flow tests (`paykit-sdk/tests/flows/one_time.rs` and `recovery.rs`). They check
that:

- a paid request reads as paid, with one execution on the rail; the payee's shop receives the
  Payment Proof, and confirming it sends a Payment Receipt; and the payer's wallet stores that
  receipt, with its attempt, payee, request and Payment Reference, and lists it;
- a payment the rail refuses, or fails, is reported failed and written `failed` (its journal
  entry finished, its attempt failed on the claim), a resume repeats nothing, no money moves,
  the request stays open, and a second payment then goes through;
- paying a request whose expiry has passed is refused (`Refusal::DeadlinePassed`) and moves no
  money;
- a payment the rail leaves in flight reads as being paid here, and once the rail settles it, a
  resume reports it paid, once;
- a payment left in flight, with a rail that keeps no history, is asked again under the same
  idempotency key when its app, loaded again from its store, resumes it, and money moves once.

The tests in `tests/fetch_transport/` send through `FetchTransport` to the rig's fixed routes,
over TLS and CORS, so they run only in a browser. They check that:

- a `GET` returns the status, the headers the rig exposes and the exact body, both with a
  `content-length` and streamed without one;
- a `POST` body with every byte value arrives byte for byte, at the path and query sent;
- the Switch's request headers arrive with the values sent, once each;
- an answer with a status outside 2xx is returned as an answer, not as an error;
- a redirect answer is `TransportError::Other`, and its target is never requested;
- an answer body over `max_response_bytes` is `TransportError::TooLarge`, counted as it
  streams, with a `content-length` and without one, and a body of exactly the limit is read;
- an answer body shorter than its `content-length` is `TransportError::Other`, also when that
  length is beyond what a 32-bit WebAssembly instance can hold, and when it is over the limit:
  the limit counts the bytes read, not the length announced;
- the answer to a `HEAD` is returned with no body, though its `content-length` is over the
  limit;
- a refused origin is `TransportError::Refused` and nothing reaches the rig: plain `http`, a
  port that is not allowed, an address literal, and `localhost` through `FetchTransport::new`;
- an origin that does not answer is `TransportError::Other`, never `Connect` or `Refused`;
- a call past its timeout is `TransportError::Timeout` as soon as the timeout passes.

The tests in `tests/switch_flow/` run the network flow described above, so they also run only
in a browser. Each test asks the rig for a world of its own, and joins its apps through
`FetchTransport` (`tests/switch_flow/switch_world.rs`, on the rig helpers in
`tests/switch_rig/`, shared with `tests/fetch_transport/`). They follow the
SDK's native flow tests (`paykit-sdk/tests/flows/one_time.rs`), with the real service's
answers in place of a scripted switch's. They check that:

- a one-time Payment Request is accepted by the service, received once, and reads as open,
  with the amount, payment reference and expiry that were sent, and that the payer's activity
  record stores the payee's signed request with its Payment Endpoint; paid through
  `FakeRail`, it reads as paid, with one execution on the rail; the payee receives the Payment
  Proof, and confirming it sends a Payment Receipt; and the payer stores that receipt with the
  request it pays, and lists it;
- a payment the rail refuses is reported failed and written `failed` (its journal entry
  finished, its attempt failed on the claim the service holds), a resume repeats nothing, no
  money moves, the request stays open, and a second payment then goes through;
- a request to a payer who has not admitted the payee is `MessageDelivery::WaitingForAdmission`
  (the service answers `403`), nothing reaches the payer, and once the payer invites the payee
  it is accepted and received;
- a request to an identity that never enrolled stays `MessageDelivery::Queued`, since no
  `identity.json` can be read for it, and the payee's app records it;
- the rig refuses `POST /rig/world` with a 403 without the run's token, and with a wrong one.

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
matches the filter passes, but a run in which no test target runs a test fails. This runs
every target except the network targets, and refuses a network target named with `--test`.
`test.sh` picks the test targets itself: `--test` is the only `cargo` target option it takes.

To run one engine and context without `test.sh`, set `rig/runner.mjs` as Cargo's wasm32 runner
as its header comment shows; that comment also lists the runner's environment variables and
their defaults.

### Run the network tests

`rig/test.sh --switch` runs the network targets against a real Pubky Switch service. It is
test rig code only, never for production. On the local machine, the run starts:

- the `pubky-switch` service binary, on a fresh database that the run creates and drops, with
  the rig's configuration in `rig/switch.toml.in`;
- `rig/switch-rig.mjs`, one Node.js process in front of the service. It terminates TLS on
  `https://localhost:<rig port>`, on both `127.0.0.1` and `::1`, with a self-signed
  certificate for `localhost` that it makes at start, answers CORS preflights, adds CORS
  headers to its answers, and forwards every other request to the service byte for byte. It
  answers CORS only for the test page's origin, `http://127.0.0.1`, `http://localhost` or
  `http://[::1]` on any port. It serves the routes under `/rig/` itself and never forwards
  them.

The routes under `/rig/` are fixed routes for the transport tests, and `POST /rig/world`. For
each test world, `/rig/world` runs the native `switch-enroll` example (`rig/switch_enroll.rs`)
once, with a fresh seed prefix. It requires the run's token in its query
(`/rig/world?token=<token>`), compared in constant time: `test.sh` makes a fresh random token
for each run, gives it to the rig in its environment and to the test build in
`PAYKIT_BROWSER_RIG_TOKEN`, so a page or program that does not know the token cannot make the
rig enrol. A request without that token, or with an `Origin` other than the test page's, gets
a 403 instead. The example enrolls the identities and approves their apps with the real
signer, which does not build for wasm32, and returns each identity's latest `identity.json`
and each app's request and approval. The browser test rebuilds the keys from the seed prefix,
loads the documents into an in-memory `TestHomeserver`, and joins each app with
`Paykit::join`.

The run passes the rig's origin to the test build in `PAYKIT_BROWSER_RIG_ORIGIN`, and the
token in `PAYKIT_BROWSER_RIG_TOKEN`. The runner
ignores certificate errors only when that variable is set, so only a `--switch` run accepts
the self-signed certificate. A network test never passes by skipping: built without the
origin, it panics, and `test.sh` fails with an error when the service or the rig does not
start, or stops during the tests. The run stops the service and the rig, and drops the
database, on every exit, including a failure and Ctrl-C.

You need what [Run the tests in a browser](#run-the-tests-in-a-browser) lists, and:

- PostgreSQL 14 or later, with `fsync` on (the service refuses to run with it off), and a
  user that may create databases. For example, a throwaway container; replace
  `<test password>` with a password for it:

  ```sh
  docker run --rm -d --name paykit-browser-pg -e POSTGRES_PASSWORD=<test password> \
    -p 127.0.0.1:5433:5432 postgres:17-alpine
  ```

  `docker stop paykit-browser-pg` stops and removes it;
- the `pubky-switch` binary at the commit pinned in the root `Cargo.toml`. Read the commit
  from `Cargo.lock` with `rig/switch-pin.sh`, and install the binary into a folder of your
  choice, `<dir>`:

  ```sh
  rev=$(paykit-browser/rig/switch-pin.sh --rev)
  cargo install --locked --git https://github.com/pubky/pubky-switch --rev "$rev" pubky-switch --root <dir>
  ```

  `test.sh` checks that the binary's crate version is the version of the `pubky-switch`
  package in `Cargo.lock`. The binary does not report the commit it was built from, so
  `test.sh` cannot check the commit;
- `psql`, `curl`, `jq` and `openssl` on `PATH`.

Then run, from the repository root:

```sh
export PAYKIT_BROWSER_SWITCH_BIN=<dir>/bin/pubky-switch
export PAYKIT_BROWSER_SWITCH_DATABASE_URL=postgres://postgres:<test password>@127.0.0.1:5433/postgres
paykit-browser/rig/test.sh --switch all both
```

The engine, context and `cargo test` arguments work as without `--switch`. Without `--test`,
the run runs the two network targets. Unless `PAYKIT_BROWSER_ENROLL_BIN` is set, it builds the
`switch-enroll` example natively first. `test.sh` reads these variables:

| Variable | What it sets | Default |
| --- | --- | --- |
| `PAYKIT_BROWSER_SWITCH_BIN` | The `pubky-switch` service binary. | Required |
| `PAYKIT_BROWSER_SWITCH_DATABASE_URL` | A `postgres://` URL. Each run creates its own database there and drops it. | Required |
| `PAYKIT_BROWSER_RIG_PORT` | The rig's TLS port, in the origin. | `18443` |
| `PAYKIT_BROWSER_SWITCH_PORT` | The service's API port. | `18480` |
| `PAYKIT_BROWSER_SWITCH_ADMIN_PORT` | The service's admin port. | `18490` |
| `PAYKIT_BROWSER_RIG_CLOSED_PORT` | A port nothing listens on, for the test of an origin that does not answer. The run passes it to the test build. | `18449` |
| `PAYKIT_BROWSER_RIG_LOG_DIR` | A folder that keeps the service and rig logs, replaced at each run. | A temporary folder, removed |
| `PAYKIT_BROWSER_ENROLL_BIN` | The native enrollment program the rig runs for `/rig/world`. | The `switch-enroll` example, built by the run |

Nothing may already listen on the four ports, on `127.0.0.1` or `::1`: a browser may resolve
`localhost` to either.

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

Its `switch-service` job reads the pinned `pubky-switch` commit from `Cargo.lock` with
`rig/switch-pin.sh`, which fails unless every Switch crate there comes from that commit, and
installs the service binary with
`cargo install --locked --git https://github.com/pubky/pubky-switch --rev <commit>`. The binary
is cached per commit, so it is built once, not once per engine, and the job installs the Rust
toolchain only when the cache has no binary for the commit. Its `switch-tests` job runs
once per engine (`chromium`, `firefox`, `webkit`), as three jobs, with a PostgreSQL 17 service
at its default settings. Each restores the binary, runs `rig/test.sh --switch` in a page and
then in a dedicated worker, and keeps the service and rig logs as an artifact when it fails.

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
