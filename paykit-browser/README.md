# paykit-browser

`paykit-browser` holds the browser (`wasm32-unknown-unknown`) build support for the Paykit SDK,
the first host pieces a browser host gives the SDK and the Switch client, and the tests that
run Paykit v0.2 code in browser engines.

Depending on this crate selects the browser random number backends (`crypto.getRandomValues`)
for every getrandom version in a `wasm32-unknown-unknown` build of Paykit. On any other target
it builds as an empty crate, so workspace-wide commands keep working.

## Host pieces

On `wasm32-unknown-unknown` the crate holds these Rust items for a host written in Rust. Each
works in a page and in a dedicated worker, except `store::OpfsStore`, which works only in a
dedicated worker. None is exported to JavaScript. Other wasm32 targets, such as WASI, have no
JavaScript host, and do not get them.

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
- `store::OpfsStore`: the Switch client's `StateStore` on the browser's Origin Private File
  System, with every snapshot sealed. See [Durable store](#durable-store) for what sealing
  protects and what it does not.

## Durable store

`store::OpfsStore` is the Switch client's `StateStore` on the browser's Origin Private File
System (OPFS), so the SDK's state outlives the WebAssembly instance that wrote it. Its rustdoc
covers names, the contract's guarantees, errors and failure handling.

Every snapshot is sealed: encrypted and authenticated with XChaCha20-Poly1305 under the store's
data key, with a fresh random 24-byte nonce for every write. The files hold no plaintext:
namespaces, keys and values are all inside the sealed part. A snapshot that was changed,
written without the key, or moved to another store, generation or format version does not
open. A deleted value is overwritten in both files within two commits. Whether a commit's
`flush()`, or the key record's IndexedDB transaction, reaches the storage device before a power
loss is up to the engine, and cannot be tested here. The browser may evict the origin's storage
unless the origin was granted persistent storage (`navigator.storage.persist()`), which the
store does not ask for; a store whose files were evicted opens again as a new, empty store,
with no error.

Sealing does not protect against:

- **Script in the origin.** Any script running in the origin can do everything the SDK can.
  Sealing does not defend against it.
- **A copied browser profile.** Chromium and Firefox store the non-extractable wrapping key's
  raw bytes in the profile, so sealing does not stop someone who copies the whole profile
  there. WebKit on Apple platforms wraps stored keys with a Keychain key, and Playwright's
  WebKit on Linux uses an empty key. These three statements come from reading the engines, and
  the tests here do not verify them. A host key kept outside the profile is not in the copy.
- **Rollback.** Someone who can write the files can put back an older valid snapshot. Sealing
  does not detect that.
- **Memory.** The data key and all values are in WebAssembly memory in the clear while the
  store is open.

The data key is 32 random bytes, made in WebAssembly with the store, along with a 16-byte store
identifier. It is never written in the clear: a record in the origin's IndexedDB, under the
store's name, keeps it wrapped with AES-GCM under an AES-GCM wrapping key, with a fresh random
IV and the store identifier as associated data. The record is written in one transaction that
asks for strict durability, before the store's first snapshot. By default (`WrappingKey::Origin`)
the wrapping key is a non-extractable 256-bit `CryptoKey` that the record also holds, so the
store opens again with no step from the host. A host can pass its own `CryptoKey` instead
(`WrappingKey::Host`), for example one it keeps behind its own unlock step: the record then
holds no key, and every open needs that same key. `OpfsStore::destroy` deletes the record
first, so whatever is left of the files can no longer be opened (as long as the engine erases
the deleted record, which IndexedDB engines may keep on disk until they compact), then the
files. It refuses a store that is open, or that a lease it granted still keeps open
(`OpenError::InUse`). The key database, shared by the origin's stores, stays even when empty:
deleting it could delete the record of a store that another tab creates at the same moment.

What an open finds, and what it does (no failed open writes the files or the record):

| Files | No key record | Record that unwraps | Record that does not unwrap, or of the other mode |
| --- | --- | --- | --- |
| Both empty | New store: new identifier, data key and record | New store with the record's key and identifier | `OpenError::KeyLost` |
| Sealed slots | `OpenError::KeyLost` | The slot rules below | `OpenError::KeyLost` |
| A slot of the earlier unsealed format | `OpenError::UnsealedFormat` | `OpenError::UnsealedFormat` | `OpenError::UnsealedFormat` |
| A slot of a newer format | `OpenError::NewerFormat` | `OpenError::NewerFormat` | `OpenError::NewerFormat` |

A key record of a newer record format is `OpenError::NewerFormat` too, whatever the files
hold. Files that hold no complete sealed header (bytes of no format, or a header cut short)
are `OpenError::Corrupt` rather than `KeyLost` when the record does not give a key: there is no
sealed data to lose. A host key that is not AES-GCM, or lacks the `encrypt` or `decrypt` usage,
is refused with `OpenError::Unavailable` before anything is opened, as a store made with it
could never be opened again. `KeyLost`
means sealed data is present but its key is gone or wrong; the store is never replaced by a
new one over it. The earlier unsealed format is refused on a file's header and never read,
upgraded or overwritten. The rustdoc of `OpfsStore` has the complete table.

The store, the SDK and the Switch client run in the same dedicated worker: `StateStore` calls
are synchronous, and only a dedicated worker has synchronous file handles. Open the store there
once and hand it to the SDK.

The store is a directory of two slot files. Each slot holds a whole sealed snapshot: a header
(magic, format version, generation, store identifier, nonce, sealed length), then the encrypted
payload and its 16-byte Poly1305 tag, which also covers the header. A commit writes the new
state over the slot that is not current, cuts that file to the new length, flushes it, and only
then makes it current.

On open, the valid slot with the higher generation is current, and a torn or damaged slot next
to it is ignored. If the damaged slot held the newest state, the store opens without the last
commit that returned `Ok`. Files with no slot to trust fail the open with `OpenError::Corrupt`:
two invalid slots, a damaged slot next to an empty one, a slot that passes its tag but does not
decode, or two valid slots of one generation. A slot whose header names another store is
`OpenError::KeyLost`. Only a first commit that stopped part way, next to an empty file, opens
as a new store; so does the file of a store's only commit if it later loses its end, as the two
cannot be told apart. A slot of a newer format fails the open with
`OpenError::NewerFormat`, even next to a valid one. Opening writes nothing. The tag detects torn
and damaged writes and tampering, not rollback.

A real page reload is tested at the store level only: the payment tests reopen the store in
the same worker, as their in-memory test world would not survive a reload.

### Measurements

Measured on macOS arm64 with Playwright 1.62.1 (Chromium 151.0.7922.34, Firefox 153.0, WebKit
26.5), release build, in a dedicated worker of a persistent context, with the origin-held key in
Chromium and Firefox and a host key in WebKit. A commit is one 64-byte put into a store of the
given encoded state size; an open is the whole `OpfsStore::open`, the key record and its unwrap
included. Medians in milliseconds over 21 runs; in parentheses, the earlier unsealed store, with
a BLAKE3 tag and no encryption:

| Encoded state | Chromium commit | Chromium open | Firefox commit | Firefox open | WebKit commit | WebKit open |
| --- | --- | --- | --- | --- | --- | --- |
| 73.5 KiB | 0.45 (0.38) | 0.88 (0.66) | 2.12 (1.30) | 6.06 (3.90) | 0.42 (0.20) | 1.58 (0.62) |
| 1 MiB | 2.45 (1.57) | 4.69 (2.45) | 27.9 (16.5) | 59.3 (34.3) | 2.20 (2.22) | 5.32 (4.46) |
| 16 MiB | 32.6 (19.6) | 64.9 (33.5) | 446 (267) | 918 (527) | 35.9 (36.1) | 74.0 (66.8) |

Most of the time goes to the cipher: at
16 MiB, sealing takes 28.8 ms in Chromium, 33.3 ms in WebKit and 445 ms in Firefox, against
13.8, 29.8 and 250 ms for the BLAKE3 tag before; the write and the flush take about 2 ms each.
An open decrypts both slots. Reading the key record and unwrapping the data key takes about
0.1 ms in Chromium and WebKit and about 0.35 ms in Firefox.

Paying one Payment Request with `Paykit::pay_request` makes 27 commits to the payer's store
and writes about 1.25 MB, for a slot that grows from about 28 KB to about 65 KB. Sealing adds 24
bytes to each slot (a 68-byte header and a 16-byte tag, against 28 and 32 before). A snapshot
rewrites the whole live state on every commit. A log would write roughly the batches
themselves, about 192 KB, but it needs compaction and has more states to recover from.

## Target

Single-threaded `wasm32-unknown-unknown`. A build with WebAssembly threads (the `atomics`
target feature) is refused at compile time.

## What this covers, and what it does not yet

- The `wasm32-unknown-unknown` dependency graph of `paykit-lib` and `paykit-sdk` builds.
- The tests listed under [Tests](#tests) run in a page and in a dedicated worker, in Chromium,
  Firefox and WebKit. The store targets run in one of the two contexts only, and not in
  Playwright's WebKit for Linux, which has no OPFS (see
  [Run the tests in a browser](#run-the-tests-in-a-browser)): they run in WebKit on macOS
  only, and there without the tests that keep the wrapping key in IndexedDB (see the
  `OpfsStore` item below).
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
- `FetchTransport`, and the SDK on it, when the network misbehaves: a server that does not
  answer, a dropped call, cacheable answers, CORS preflights, long polls and the browser's
  connection limit, and lost or refused payment batches against the real service (see
  [When the network misbehaves](#when-the-network-misbehaves)).
- `OpfsStore` runs on real OPFS files in a dedicated worker, across a page reload, and under a
  payment that closes and opens it again part way, with its snapshots sealed (see
  [Tests](#tests)). The tests that keep the wrapping key in IndexedDB (`WrappingKey::Origin`,
  every `store_flow` and `store_reload` test included) run in Chromium and Firefox, locally
  and in CI. **In WebKit they run nowhere today:** on macOS, Playwright's WebKit asks for
  Keychain access to store that key, and local runs leave them out; Playwright's WebKit for
  Linux, which CI uses, has no OPFS (see
  [Run the tests in a browser](#run-the-tests-in-a-browser)). The store tests with a host's
  key run in WebKit on macOS, locally only. The library's unit tests of the sealed format,
  tampering and nonces run in all three engines, locally and in CI.

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

There are fourteen test targets, and the library has unit tests. Five of the targets,
`tests/fetch_transport/`, `tests/switch_flow/`, `tests/transport_faults/`,
`tests/switch_faults/` and `tests/switch_http/`, are network targets: they need a real Switch
service, and run only as [Run the network tests](#run-the-network-tests) describes. The others
need no network, no database and no service.

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
`tests/switch_rig/`, shared with the other network targets). They follow the
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

The tests in `tests/transport_faults/` send through `FetchTransport` to the rig's fault
routes, so they run only in a browser. The counts the engines differ on are printed, not
asserted (see [When the network misbehaves](#when-the-network-misbehaves)). They check that:

- a server that never answers ends an ordinary, a bulk and a long-poll call at the deadline
  of its timeout class with `TransportError::Timeout`, and so does a server that sends the
  head of its answer and then no body, for an ordinary and a bulk call; each time, the rig
  sees the browser close the connection;
- dropping the future `send` returned, before the head of the answer arrives or while the
  body is awaited, closes the connection, and the same transport then reads a whole answer;
- two calls to the same URL, whose answer says `cache-control: public, max-age=60`, both
  reach the rig, since the transport asks for `cache: "no-store"`;
- each call with a Switch header reaches the rig once, also on a path the rig forwards to
  the service, where each call reaches the service once; a URL not used before is always
  preflighted, also when only its query is new; and with `access-control-max-age: 0` every
  call is preflighted.

The tests in `tests/switch_faults/` pay a one-time Payment Request through the real service
while the rig faults the payer's operation batches (`POST /v1/{id}/ops`), and hold a public
identity read, so they also run only in a browser. They use the world of `tests/switch_flow/`, and follow the SDK's native
flow tests (`paykit-sdk/tests/flows/one_time.rs` and `unrequested.rs`), whose scripted switch
fails the same batches. They check that:

- when the answer to the payer's first batch, which records the attempt, is lost after the
  service applied it, or the batch is refused before it reaches the service, the call fails
  with the batch kept as sent and nothing paid; the next flush sends the same batch, and it
  is applied; a second payment is refused while the first is in progress
  (`Refusal::PaymentInProgress`), a resume writes `failed`, and the next payment pays once,
  with one execution on the rail, and the payee receives its Payment Proof;
- when the answer to the batch that writes the outcome is lost after the rail paid, the call
  reports `PayOutcome::PaidOutcomeUnwritten`, not an error, and a resume finishes the
  payment without paying again, with one execution on the rail;
- each batch reaches the rig as often as the flow sends it, and the rig sends it on to the
  service each time but when it refuses it;
- when the rig holds the payer's public identity read (`hold`) for 400 ms, the read returns
  the published `identity.json` no earlier than the hold; when it holds the read past a 1 s
  transport deadline, the read ends with `TransportError::Timeout` at the deadline, the
  rig sees the browser close the connection, with nothing left open, and the read never
  reaches the service.

The tests in `tests/switch_http/` check how the real service's long poll and caching behave
over `fetch`, with apps joined through `FetchTransport`, so they also run only in a browser.
They check that:

- a wait on the feed (Switch wire 7.4.4) wakes after a counterparty's message is sent, in
  less than half of its wait, and the next receive applies that message;
- the client cuts a wait asked for longer than the service's maximum, 25 s, to 25 s; and a
  long poll's deadline is its wait plus `long_timeout_ms`: with a 2 s `long_timeout_ms`, a
  4 s wait on a quiet feed returns empty once its wait has passed, not before and not cut,
  while a bulk call beside it times out;
- beside 4 held long polls, an ordinary call to the same origin completes, and a call that
  waits for a connection completes once a held poll ends or is dropped; the number of held
  polls at which a call starts to wait is printed;
- held polls that the app drops, or that the transport's deadline ends, while the service
  holds them, leave the rig and the service answering, also after the service's wait has
  passed;
- two public reads of an identity's `identity.json`, which the service answers with
  `max-age=60`, both reach the service.

The store's tests: the library's unit tests (`src/store/`) check its format and logic on
in-memory files, natively and in browsers. `tests/store/` runs the `StateStore` contract cases
on `MemoryStore` natively and on `OpfsStore` in a browser, and checks `OpfsStore` on its real
files; its module docs list the cases. `tests/store_reload/` opens the store again after a
page reload, `tests/store_page/` checks that a page cannot open it, and `tests/store_flow/`
closes and opens the payer's store again part way through a payment.

### Run the tests natively

```sh
cargo test -p paykit-browser
```

This also runs the library's unit tests and `tests/store/` on `MemoryStore`.

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
matches the filter passes, but an engine in which no test target runs a test, in any context,
fails. This runs the library's unit tests and every target except the network targets, and
refuses a network target named with `--test`. `test.sh` picks the test targets itself: `--test`
is the only `cargo` target option it takes.

The store targets run in one context only, with a browser profile on disk, and
`tests/store_reload/` across a page reload: see "Target modes" in `rig/runner.mjs`. Given one
context, `test.sh` still runs the targets of the other context, in their own context. After a
store target, the runner removes whatever is left in its origin's OPFS and IndexedDB, and fails
a passing run that left an OPFS entry or an IndexedDB record, or whose leftovers it cannot
remove: the tests remove what they create. An empty key database is not a failure, as
`OpfsStore::destroy` keeps it.

On macOS, Playwright's WebKit protects a `CryptoKey` stored in IndexedDB with a key from the
user's Keychain, and asks for Keychain access in a dialog; the test waits until someone answers
it. For a local run there, `PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS=1` leaves out of WebKit runs the
tests that store such a key (those that open a store with `WrappingKey::Origin`): each one is
printed as not run, and the summary counts them, so such a run does not cover WebKit fully.
The tests with a host's key, which the test keeps in memory, still run. The switch is refused
on any other platform, and CI never sets it. This was seen with Playwright's WebKit build;
Safari itself was not tried.

Playwright's WebKit for Linux has no OPFS: no `navigator.storage` in a page, and no
`FileSystemSyncAccessHandle` in a dedicated worker. There, the runner checks from a page of the
test origin that `navigator.storage` is still missing, does not run the store target, and
prints it as excluded; `test.sh` lists every excluded target at the end as a warning. If that
engine gains OPFS, the run fails until its entry in `NO_OPFS` (`rig/runner.mjs`) is removed.
The summary line names what did not run, each with its reason: targets excluded for want of
OPFS, and tests left out for the Keychain.

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

The routes under `/rig/` are fixed routes for the transport tests, fault routes, and
`POST /rig/world`. The header comment of `rig/switch-rig.mjs` describes each route. The fault
routes are these:

- `/rig/hang` reads the request and never answers, or sends the head of an answer and then
  no body.
- `/rig/cacheable` answers with `cache-control: public, max-age=60`, and counts its requests.
- `/rig/count` reports the requests that carry a given tag: how many there were, how many
  were preflights, how many the client closed early and how many are still open.
- `/rig/hits` reports, for one path the rig forwards to the service, and one method or all:
  `preflights`, the preflights for the path; `forwarded`, the requests the rig sent on to
  the service; `refused`, those a `refuse` fault kept from it; and `open`, those still in
  progress. A held request the client gives up before its hold ends is neither forwarded
  nor refused.
- `/rig/fault` arms one fault for the next forwarded request with a given method and exact
  path: the rig loses the service's answer (`lose`), forwards nothing (`refuse`), or
  forwards the request after a delay (`hold`). The fault fires only on a request that is the
  first its connection carries. With the optional `after`, from 0 (the default) to 16, the
  fault lets that many matching requests through first; only matching requests that are
  first on their connection count toward it. While a fault is armed, the rig keeps no
  browser connection alive: it closes the idle ones when the fault is armed, and each one in
  use, both ways, once its answer ends. `GET /rig/fault` reports whether a fault is armed,
  how many fired, and `resent`: how many later requests repeated the one the last fault
  fired on, with the same method, target, Switch request headers and body, as a browser's
  own resend would. Arming or disarming a fault sets `resent` back to 0.

The rig answers every CORS preflight itself and forwards none to the service, with
`access-control-max-age: 600`. A request whose query has `max_age=none` or `max_age=0`, on
any path, gets a preflight answer with no `access-control-max-age` or with 0.

For each test world, `POST /rig/world` runs the native `switch-enroll` example
(`rig/switch_enroll.rs`) once, with a fresh seed prefix. It requires the run's token in its
query (`/rig/world?token=<token>`), compared in constant time: `test.sh` makes a fresh random
token for each run, gives it to the rig in its environment and to the test build in
`PAYKIT_BROWSER_RIG_TOKEN`, so a page or program that does not know the token cannot make the
rig enrol. A request without that token, or with an `Origin` other than the test page's, gets
a 403 instead. `/rig/fault` checks the token and the `Origin` the same way. The example
enrolls the identities and approves their apps with the real signer, which does not build for
wasm32, and returns each identity's latest `identity.json` and each app's request and
approval. The browser test rebuilds the keys from the seed prefix, loads the documents into an
in-memory `TestHomeserver`, and joins each app with `Paykit::join`.

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
the run runs the five network targets. Unless `PAYKIT_BROWSER_ENROLL_BIN` is set, it builds the
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

## When the network misbehaves

The targets `tests/transport_faults/`, `tests/switch_faults/` and `tests/switch_http/` show
how `FetchTransport`, and the SDK on it, behave when an answer does not come, comes late or
is lost, and when long polls hold the browser's connections. The rig makes the faults (see
[Run the network tests](#run-the-network-tests)).

### What the tests prove

[Tests](#tests) lists each check. In short:

- A server that never answers, or sends the head of its answer and then no body, ends an
  ordinary, bulk or long-poll call at the deadline of its timeout class with
  `TransportError::Timeout`, and the browser closes the connection.
- Dropping the future `send` returned, before the head of the answer or during the body,
  closes the connection, and the same transport then completes a call.
- An answer marked `public, max-age=60` reaches the rig on every call, and so does the
  service's public identity read: the transport asks for `cache: "no-store"`.
- Each call reaches the rig once. A URL not used before always costs a preflight, also when
  only its query is new. An `access-control-max-age: 0` preflight is never reused. No
  preflight reaches the service, by the rig's design: the rig answers every preflight itself.
- A wait on the feed wakes once a counterparty's message arrives, and returns empty when its
  wait passes with nothing to show. The client asks for at most the service's maximum wait,
  25 s, and a long poll's deadline is its wait plus `long_timeout_ms`, so a
  `long_timeout_ms` shorter than the wait does not cut it.
- Beside 4 held long polls, an ordinary call to the same origin completes, and a call that
  waits for a connection completes once a held poll ends or is dropped.
- Held polls that the app drops, or that pass their deadline, while the service holds them,
  leave the rig and the service answering.
- A payment whose batch is lost or refused pays once: see [Payment faults](#payment-faults).
- A call the rig holds completes no earlier than the hold, and one held past the transport's
  deadline ends with `TransportError::Timeout` at the deadline: the browser closes the
  connection, and the call never reaches the service.

### Measured numbers

The numbers below were measured on one macOS machine, with Playwright's Chromium
151.0.7922.34, Firefox 153.0 and WebKit 26.5. A page and a dedicated worker gave the same
numbers. The tests print them in the run log; they assert only what
[What the tests prove](#what-the-tests-prove) lists.

Deadlines and dropped calls:

- A call to a server that never answers, or that stops after the head, ended with `Timeout`
  2 to 12 ms after its deadline, and the rig saw the browser close the connection within
  26 ms of that.
- After a call was dropped, before the head or during the body, the rig saw the connection
  close within 67 ms.
- A public identity read that the rig held for 400 ms took 411 to 437 ms. One held past a
  1 s deadline ended with `Timeout` 3 to 59 ms after the deadline, with nothing left open at
  the rig.

Caching:

- An answer with `cache-control: public, max-age=60` reached the rig on every call through
  the transport, in every engine. As a control, a plain `fetch` in the browser's default
  cache mode was served from Firefox's cache; Chromium and WebKit fetched it again.
- The service's public identity read answers with
  `cache-control: public, max-age=60, must-revalidate`, and every read reached the service.
  It is a plain `GET` with no Switch header, so it needs no preflight.

CORS preflights, as calls / preflights counted at the rig. Every call carries a Switch header,
so the browser preflights it unless it reuses a preflight it holds. `max-age` is the
`access-control-max-age` of the rig's preflight answer. A forwarded path is one the rig sends
on to the service.

| Calls | Chromium | Firefox | WebKit |
| --- | --- | --- | --- |
| Same URL twice, `max-age` 600 | 2 / 2 | 2 / 2 | 2 / 1 |
| Same URL twice, no `max-age` | 2 / 2 | 2 / 2 | 2 / 1 |
| Same URL three times, `max-age` 0 | 3 / 3 | 3 / 3 | 3 / 3 |
| Three different paths, `max-age` 600 | 3 / 3 | 3 / 3 | 3 / 3 |
| Same path, a new query each time (like the feed's `after=`), `max-age` 600 | 3 / 3 | 3 / 3 | 3 / 3 |
| Forwarded path, same URL twice, `max-age` 0 | 2 / 2 | 2 / 2 | 2 / 2 |
| Forwarded path, same URL twice, `max-age` 600 | 2 / 2 | 2 / 2 | 2 / 1 |
| Forwarded path, same URL twice, no `max-age` | 2 / 2 | 2 / 2 | 2 / 1 |
| Control: plain `fetch` in the default cache mode, same URL twice, `max-age` 600 | 2 / 1 | 2 / 1 | 2 / 1 |

Chromium and Firefox never reused a preflight for the transport's `cache: "no-store"`
requests, whatever `access-control-max-age` said; WebKit reused it for the same URL. A new
URL, including a new query, cost a preflight in every engine, so each page of a feed read
with a new `after=` waits for a preflight first.

Long polls:

- A 10 s wait woke about 0.2 to 0.6 s after a counterparty's message, sent about 1 s into
  the wait.
- With `long_timeout_ms` at 2 s, a 4 s wait completed after 4.02 to 4.07 s, while a bulk call
  beside it timed out after 2.00 to 2.01 s.

Connection limit, with the rig on HTTP/1.1. Each held long poll keeps a connection of its
own:

| Held long polls | An ordinary call to the same origin |
| --- | --- |
| 1 to 5 | completes in 2 to 14 ms |
| 6 | waits; once the first held poll is dropped, it completes within 11 ms |

The limit was 6 connections to one origin in Chromium, Firefox and WebKit, in a page and in a
dedicated worker.

### Payment faults

`tests/switch_faults/` faults the payer's operation batches while it pays a one-time Payment
Request through the real service. In every case below the rail executes one payment.

- The answer to the batch that records the attempt is lost after the service applied it, or
  the batch is refused before it reaches the service. The call fails with the batch kept as
  sent. The next flush sends it again, and the service applies it once. Paying again is
  refused while the payment is in progress, a resume writes it `failed`, and the next
  payment pays once.
- The answer to the batch that writes the outcome is lost. The call reports
  `PayOutcome::PaidOutcomeUnwritten`, and a resume finishes the payment without paying again.

A refused connection and a lost answer look the same to a browser's `fetch`: both are
`TransportError::Other`. So the client keeps a refused batch as sent, though the service never
saw it. The native case of a connection that fails before anything was sent, which the client
knows sent nothing (`TransportError::Connect`), cannot happen through `fetch`.

A browser may send a request again on its own when a connection it reused closes without a
byte of the answer, which would hide the fault. So a fault fires only on the first request of
a connection, and while one is armed the rig keeps no browser connection alive. That makes a
resend unlikely, not impossible, so each test also checks that the browser did not resend the
faulted request: the rig's `resent` count is 0 after the call. The service's batch IDs make
such a resend safe.

### Limits

- The rig speaks HTTP/1.1 only. HTTP/2, which carries many requests on one connection and
  has no limit of 6 connections, is not tested.
- Playwright's WebKit build is not Safari.
- All timings come from one local machine and vary from run to run.

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
installed at the version in `Cargo.lock`. Each store target runs in its one context there,
except in the `webkit` job: Playwright's WebKit for Linux has no OPFS, so the runner excludes
the store targets there, and the job shows a warning annotation for each. CI therefore runs no
store target in WebKit, and no test of the origin-held wrapping key in WebKit (see
[What this covers](#what-this-covers-and-what-it-does-not-yet)).
The runner stops a target whose name starts with `store` but has no mode, and a mode whose
target does not exist, so a store target never runs without its mode.

Its `switch-service` job reads the pinned `pubky-switch` commit from `Cargo.lock` with
`rig/switch-pin.sh`, which fails unless every Switch crate there comes from that commit, and
installs the service binary with
`cargo install --locked --git https://github.com/pubky/pubky-switch --rev <commit>`. The binary
is cached per commit, so it is built once, not once per engine, and the job installs the Rust
toolchain only when the cache has no binary for the commit. Its `switch-tests` job runs
once per engine (`chromium`, `firefox`, `webkit`), as three jobs, with a PostgreSQL 17 service
at its default settings. Each restores the binary, runs `rig/test.sh --switch` in a page and
then in a dedicated worker, and keeps the service and rig logs as an artifact when it fails.

`rig/test.sh --switch` runs all five network targets, so these jobs also run
`transport_faults`, `switch_faults` and `switch_http`. Measured on a local machine, per engine
and context, `transport_faults` takes about 4 s, `switch_faults` about 6 s (about 10 s in
Firefox) and `switch_http` about 15 to 17 s: about 26 to 32 s per context, so about 50 to 65 s
per engine job, plus the time to build their three test binaries. A CI runner's times may
differ.

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
