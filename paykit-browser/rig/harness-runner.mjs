#!/usr/bin/env node
// Runs the paykit-browser hosted test harness (rig/harness/) in browser engines through
// Playwright: the bundle in a dedicated module worker under a strict Content Security Policy,
// served over HTTPS by switch-rig.mjs on an origin that is not the Switch's, with the page and
// the worker exchanging only plain-data messages (rig/harness/www/page.mjs). Test rig code
// only, never for production.
//
//   node harness-runner.mjs <chromium|firefox|webkit|chrome>...
//
// `test.sh --harness` builds the bundle, starts the Switch service and the rig, and runs this.
// Environment:
//   PAYKIT_BROWSER_RIG_ORIGIN        the Switch origin, https://localhost:<rig port> (required)
//   PAYKIT_BROWSER_HARNESS_ORIGIN    the harness origin, https://localhost:<harness port>
//                                    (required)
//   PAYKIT_BROWSER_HARNESS_TIMINGS   a file to write the timings to, as JSON. Default: none;
//                                    the summary table is printed either way. A run stopped
//                                    early, by its time limit or a signal, writes what it
//                                    collected.
//   GITHUB_STEP_SUMMARY              when set, the summary table is also appended to this
//                                    file, as Markdown.
//   PAYKIT_BROWSER_HARNESS_TIMEOUT_SECS  hard limit for the whole run, at most 2147483.
//                                    Default 1200.
//
// Pages and dedicated workers accept the rig's self-signed certificate through Playwright's
// ignoreHTTPSErrors. Each run of the cases below prints PASS or FAIL; the run exits 1 when any
// failed, and 0 only when every case passed in every engine.
//
// Once, before the engines: the rig's headers. Every variant of /harness/<variant>/ sends
// exactly the policy expected on index.html and, apart, on worker.mjs and sw.mjs (none on the
// worker scripts of worker-no-header), and none on the other files; harness_bg.wasm is
// application/wasm; every file has cache-control: no-store; the rig's port serves the strict
// files under /rig/harness/strict/ the same way; and paths outside the file list, other
// variants and spellings with "..", get a 404.
//
// Per engine, in one persistent context (a profile in a new temporary folder, so the store
// survives a page reload), on the harness origin unless stated:
//   strict, cross-origin  worker:load; `pure`; `switch_reach`, which sends at least one
//                         preflight under its tag (the rig's /rig/count?tag=<tag>);
//                         `flow_pay`; a real page reload, then a new page instance and a new
//                         worker; `flow_read_back` with the ids flow_pay returned; `cleanup`.
//                         No Content Security Policy violation event in the page or a worker,
//                         no console error (where each engine also reports a violation raised
//                         while the page loads, before page.mjs listens for the events; what
//                         an engine reports on its own aside, see KNOWN_ENGINE_NOISE), no
//                         page error, and no console message about the .wasm's MIME
//                         type, from the start of the page's load on; every message both ways
//                         a plain object of the protocol whose values are strings or numbers,
//                         with no port. Then the strict worker refuses `new Function`.
//   same-origin           from the rig's own origin (/rig/harness/strict/): worker:load,
//                         `pure` and `switch_reach` pass, and `switch_reach` sends no
//                         preflight under its tag; the same checks of violations, console
//                         and messages.
//   no-wasm               worker:load fails with a CompileError (only the name is checked).
//   worker-no-header      the worker, served without a policy of its own, runs `new Function`.
//   no-switch             `switch_reach` fails with the code `switch`, and the rig counts no
//                         request under its tag: connect-src does not name the Switch. A
//                         connect-src violation naming the Switch is logged where the engine
//                         fires one.
//   panic                 `panic` never answers; the page sees the worker's error event (only
//                         the event is checked, its text differs per engine), ends the call,
//                         replaces the worker, and `pure` then passes on the new worker.
//   start failure         a worker whose script does not exist dies before it loads: its load
//                         ends with worker_died, the page records one start failure and starts
//                         no other worker, and a later call ends with worker_died too.
//   storage               after the cases, the origins' OPFS and IndexedDB hold nothing the
//                         cases left (an empty key database aside); whatever is there is
//                         removed, and a run that passed but left something fails.
// Then, in a separate browser launch with no profile:
//   service worker        the bundle loads and `pure` passes in a module service worker under
//                         the same policy, with no violation event and no console error or
//                         MIME message on the page. Playwright reports the service worker's
//                         own console in Chromium only, so only there is it checked too.
//
// The store of flow_pay per engine: "origin" (the origin-held wrapping key) in Chromium and
// Firefox; "host" (a host key kept in memory, so no CryptoKey is stored in IndexedDB) in
// WebKit on macOS, where Playwright's WebKit asks for Keychain access to store one and waits
// for an answer; "memory" where NO_OPFS (engines.mjs) names the engine and platform, checked from the
// harness page: flow_read_back and cleanup are then not run, with a printed warning. The run
// prints which engine ran which store.
//
// Timings, per engine: the worker's fetch, compile and instantiate of the bundle (init_ms,
// measured in the worker around the glue's init), again after the reload, on the rig's
// origin and in the service worker; the first scenario call, measured in the worker; and
// flow_pay's own timings.

import { randomBytes } from "node:crypto";
import { appendFileSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import https from "node:https";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium } from "playwright";
import { ENGINES, errorText, noOpfsReason, sleep, staleNoOpfs } from "./engines.mjs";

const KEYCHAIN_REASON =
  "a host key kept in memory, as Playwright's WebKit on macOS asks for Keychain access to store a CryptoKey in IndexedDB";
// A console message about the bundle's content type: instantiateStreaming refusing it, or the
// glue's warning that it falls back to a slower path.
const WASM_MIME = /application\/wasm|MIME type/i;
// What an engine reports on its own, which says nothing about the harness, so it is not
// counted: `console` matches a console message's text, `violation` takes a Content Security
// Policy violation event. Applied where console messages are kept (watch) and where violation
// events are collected (collectViolations).
const KNOWN_ENGINE_NOISE = [
  {
    engines: ["firefox"],
    console: /^(?=.*favicon\.ico)(?=.*img-src)/s,
    violation: ({ directive, blocked }) => directive === "img-src" && pathOf(blocked) === "/favicon.ico",
    why:
      "Firefox asks for /favicon.ico on its own, and default-src 'none' blocks that as an img-src " +
      "violation: an event on the page and a console message. The harness's own code loads no image.",
  },
  {
    engines: ["webkit"],
    console: /^Failed to load resource: the server responded with a status of 404\b/,
    why:
      "The line WebKit itself logs as a console error for an answer with status 404. The Switch " +
      "answers 404 for a record that does not exist yet, such as a claim, which the client reads " +
      "as none. It says nothing about the page's own script: a harness file that does not load " +
      "fails worker:load instead.",
  },
];
const PROTOCOL_SHAPES = {
  request: ["args", "id", "scenario"],
  ok: ["id", "result", "status"],
  error: ["code", "id", "message", "status"],
};
const CALL_MS = 60_000;
const FLOW_MS = 240_000;
const PANIC_MS = 30_000;

const rigOrigin = process.env.PAYKIT_BROWSER_RIG_ORIGIN ?? "";
const harnessOrigin = process.env.PAYKIT_BROWSER_HARNESS_ORIGIN ?? "";
const timingsFile = process.env.PAYKIT_BROWSER_HARNESS_TIMINGS || undefined;
const timeoutSecs = Number(process.env.PAYKIT_BROWSER_HARNESS_TIMEOUT_SECS || 1200);
const LOCAL_ORIGIN = /^https:\/\/localhost:[1-9][0-9]{0,4}$/;

const results = [];
const timings = {};
const variants = [];
// What a stop must close, and the profile folders to remove.
const closeables = new Set();
const profileDirs = new Set();
let stopping = false;
let reported = false;

function log(line) {
  process.stdout.write(`[harness] ${line}\n`);
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

async function stop(code, why) {
  if (stopping) return;
  stopping = true;
  if (why) log(why);
  // A run cut short by its time limit or a signal still reports what it collected.
  try {
    report();
  } catch (error) {
    log(`cannot report the timings: ${error.message}`);
  }
  for (const closeable of closeables) {
    await Promise.race([closeable.close().catch(() => {}), sleep(5000)]);
  }
  process.exit(code);
}

process.on("exit", () => {
  for (const dir of profileDirs) {
    try {
      rmSync(dir, { recursive: true, force: true, maxRetries: 3 });
    } catch (error) {
      log(`cannot remove the profile folder ${dir}: ${error.message}`);
    }
  }
});
process.on("SIGINT", () => stop(130, "interrupted"));
process.on("SIGTERM", () => stop(143, "terminated"));

// A GET to the rig from Node: status, headers and body. The rig's certificate is self-signed
// and made at its start, so this loopback request alone does not check it. The raw path is
// sent as it is, ".." included.
function rigGet(origin, path) {
  const { hostname, port } = new URL(origin);
  return new Promise((resolve, reject) => {
    const request = https.request(
      { host: hostname, port, path, method: "GET", rejectUnauthorized: false, agent: false },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () =>
          resolve({ status: response.statusCode, headers: response.headers, body: Buffer.concat(chunks) }),
        );
        response.on("error", reject);
      },
    );
    request.on("error", reject);
    request.setTimeout(10_000, () => request.destroy(new Error(`GET ${origin}${path} timed out`)));
    request.end();
  });
}

// What the rig counted under `tag`: { requests, preflights, closed_early, open }.
async function rigCount(tag) {
  const answer = await rigGet(rigOrigin, `/rig/count?tag=${tag}`);
  assert(answer.status === 200, `GET /rig/count answered ${answer.status}`);
  return JSON.parse(answer.body.toString("utf8"));
}

// Runs `switch_reach` under a tag of its own and returns the rig's count for that tag.
async function reachCounted(engine, tag) {
  const reach = await run(engine.page, "switch_reach", { switch: rigOrigin, tag });
  expectOk(reach, "switch_reach");
  return rigCount(tag);
}

// The policy the harness is meant to run under, as the test expects it; switch-rig.mjs
// (harnessPolicy) is where it is defined.
function expectedPolicies() {
  const strict =
    `default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self'; ` +
    `connect-src 'self' ${rigOrigin}; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`;
  const noWasm = strict.replace("script-src 'self' 'wasm-unsafe-eval'", "script-src 'self'");
  const noSwitch = strict.replace(`connect-src 'self' ${rigOrigin}`, "connect-src 'self'");
  return {
    strict: { page: strict, worker: strict },
    "no-wasm": { page: noWasm, worker: noWasm },
    "worker-no-header": { page: strict, worker: undefined },
    "no-switch": { page: noSwitch, worker: noSwitch },
  };
}

// Checks the rig's answers for the harness files of one variant under `base`.
async function checkFiles(origin, base, want) {
  const files = {
    "index.html": { type: "text/html; charset=utf-8", policy: want.page },
    "page.mjs": { type: "text/javascript; charset=utf-8", policy: undefined },
    "worker.mjs": { type: "text/javascript; charset=utf-8", policy: want.worker },
    "sw.mjs": { type: "text/javascript; charset=utf-8", policy: want.worker },
    "harness.js": { type: "text/javascript; charset=utf-8", policy: undefined },
    "harness_bg.wasm": { type: "application/wasm", policy: undefined },
  };
  for (const [name, expected] of Object.entries(files)) {
    const where = `${origin}${base}${name}`;
    const answer = await rigGet(origin, `${base}${name}`);
    assert(answer.status === 200, `${where}: status ${answer.status}`);
    assert(answer.headers["content-type"] === expected.type, `${where}: content-type ${answer.headers["content-type"]}`);
    assert(answer.headers["cache-control"] === "no-store", `${where}: cache-control ${answer.headers["cache-control"]}`);
    const policy = answer.headers["content-security-policy"];
    assert(
      policy === expected.policy,
      `${where}: content-security-policy ${JSON.stringify(policy)}, expected ${JSON.stringify(expected.policy)}`,
    );
  }
}

async function policyHeaders() {
  const expected = expectedPolicies();
  for (const [variant, want] of Object.entries(expected)) {
    await checkFiles(harnessOrigin, `/harness/${variant}/`, want);
  }
  await checkFiles(rigOrigin, "/rig/harness/strict/", expected.strict);
  const refused = [
    [harnessOrigin, "/harness/loose/index.html"],
    [harnessOrigin, "/harness/strict/"],
    [harnessOrigin, "/harness/strict/switch-rig.mjs"],
    [harnessOrigin, "/harness/strict/../../switch-rig.mjs"],
    [harnessOrigin, "/harness/strict/%2e%2e/%2e%2e/switch-rig.mjs"],
    [harnessOrigin, "/harness/strict/www/index.html"],
    [harnessOrigin, "/rig/health"],
    [rigOrigin, "/rig/harness/no-wasm/index.html"],
    [rigOrigin, "/rig/harness/strict/../www/index.html"],
  ];
  for (const [origin, path] of refused) {
    const answer = await rigGet(origin, path);
    assert(answer.status === 404, `${origin}${path}: status ${answer.status}, expected 404`);
  }
}

function pathOf(url) {
  try {
    return new URL(url).pathname;
  } catch {
    return undefined;
  }
}

// Whether `entry`, a console message (kind "console") or a violation event (kind
// "violation"), is one that KNOWN_ENGINE_NOISE lists for this engine.
function isEngineNoise(engineName, kind, entry) {
  return KNOWN_ENGINE_NOISE.some(
    (noise) =>
      noise.engines.includes(engineName) &&
      noise[kind] !== undefined &&
      (kind === "console" ? noise.console.test(entry.text) : noise.violation(entry)),
  );
}

// Records what a page and its workers print, and its errors. Every message is logged; the
// engine's own noise is not kept for the checks. With `context`, also what the context's
// service workers print, which Playwright reports in Chromium only (its `serviceworker`
// event); `state.serviceWorkers` counts the service workers seen.
function watch(page, engineName, state, context = undefined) {
  const seen = { console: [], errors: [] };
  const keep = (from, msg) => {
    const entry = { from, type: msg.type(), text: msg.text() };
    log(`[${engineName} ${from} ${entry.type}] ${entry.text}`);
    if (!isEngineNoise(engineName, "console", entry)) seen.console.push(entry);
  };
  // A message from a worker may also come through the page's event: it is kept once, from
  // the worker's own.
  page.on("console", (msg) => {
    if (!msg.worker?.()) keep("page", msg);
  });
  page.on("worker", (worker) => {
    state.workers += 1;
    worker.on("console", (msg) => keep("worker", msg));
  });
  context?.on("serviceworker", (worker) => {
    state.serviceWorkers = (state.serviceWorkers ?? 0) + 1;
    worker.on("console", (msg) => keep("service worker", msg));
  });
  page.on("pageerror", (error) => {
    seen.errors.push(errorText(error));
    log(`[${engineName} page error] ${errorText(error)}`);
  });
  page.on("crash", () => seen.errors.push("the page crashed"));
  return seen;
}

// A mark of what a page has printed so far, and the check of what came after it.
function mark(seen) {
  return { console: seen.console.length, errors: seen.errors.length };
}

// Fails on a message about the .wasm's type, a page error and, unless `allowErrors`, a
// console error, among what came after `since`. A console message that `expected` matches is
// passed over.
function checkConsole(engineName, seen, since, { allowErrors = false, expected = undefined } = {}) {
  const problems = [];
  for (const message of seen.console.slice(since.console)) {
    if (expected?.test(message.text)) continue;
    if (WASM_MIME.test(message.text)) problems.push(`a message about the .wasm's type: ${message.text}`);
    else if (message.type === "error" && !allowErrors) problems.push(`a console error: ${message.text}`);
  }
  for (const error of seen.errors.slice(since.errors)) problems.push(`a page error: ${error}`);
  assert(problems.length === 0, problems.join("\n"));
}

// Calls a function of window.paykitHarness in the page.
function api(page, name, ...args) {
  return page.evaluate(([fn, values]) => window.paykitHarness[fn](...values), [name, args]);
}

// Waits until page.mjs has set window.paykitHarness.
async function waitForHarness(page) {
  await page.waitForFunction(() => window.paykitHarness !== undefined, null, { timeout: 30_000 });
}

async function openHarness(page, url) {
  await page.goto(url);
  await waitForHarness(page);
}

function expectOk(outcome, what) {
  assert(outcome?.outcome === "ok", `${what}: ${JSON.stringify(outcome)}`);
  return outcome.result;
}

async function run(page, scenario, args = {}, timeoutMs = CALL_MS) {
  return api(page, "run", scenario, args, timeoutMs);
}

// The Content Security Policy violation events of the page and of a worker, from
// `fromWorker`, the outcome of its worker:violations call; the engine's own noise left out.
async function collectViolations(page, engineName, fromWorker) {
  const events = [
    ...(await api(page, "violations")).map((event) => ({ where: "page", ...event })),
    ...expectOk(fromWorker, "worker:violations").events.map((event) => ({ where: "worker", ...event })),
  ];
  return events.filter((event) => !isEngineNoise(engineName, "violation", event));
}

// The violation events of the page and its current worker.
async function violations(page, engineName) {
  return collectViolations(page, engineName, await run(page, "worker:violations"));
}

// Every message the page posted and received: a plain object of one protocol shape, every
// value a string or a number, and no port.
async function checkProtocol(page) {
  const log = await api(page, "messages");
  const problems = [];
  for (const entry of log) {
    const where = `${entry.direction} on ${entry.channel} (id ${entry.id})`;
    if (entry.ports !== 0) problems.push(`${where}: carried ${entry.ports} ports`);
    if (!entry.shape.plain) {
      problems.push(`${where}: a ${entry.shape.tag}, not a plain object`);
      continue;
    }
    for (const field of entry.shape.fields) {
      const typed =
        (field.type === "string" && field.tag === "[object String]") ||
        (field.type === "number" && field.tag === "[object Number]");
      if (!typed) problems.push(`${where}: ${field.key} is a ${field.type} ${field.tag}`);
    }
    const keys = entry.shape.fields.map((field) => field.key).sort().join(",");
    const allowed =
      entry.direction === "sent"
        ? [PROTOCOL_SHAPES.request]
        : [PROTOCOL_SHAPES.ok, PROTOCOL_SHAPES.error];
    if (!allowed.some((shape) => shape.join(",") === keys)) problems.push(`${where}: fields ${keys}`);
  }
  assert(log.some((entry) => entry.direction === "sent"), "the page posted no message");
  assert(log.some((entry) => entry.direction === "received"), "the page received no message");
  assert(problems.length === 0, `messages outside the protocol:\n${problems.join("\n")}`);
  return log;
}

// The ids flow_pay returned, which flow_read_back takes back: every member named *_id.
function idsOf(result) {
  return Object.fromEntries(
    Object.entries(result).filter(
      ([key, value]) => key.endsWith("_id") && (typeof value === "string" || typeof value === "number"),
    ),
  );
}

// The store of flow_pay in this engine, from the page's OPFS check (see the header).
function storeFor(engineName, hasOpfs) {
  const noOpfs = noOpfsReason(engineName);
  if (noOpfs !== undefined) {
    assert(!hasOpfs, staleNoOpfs(engineName));
    return { store: "memory", why: noOpfs };
  }
  assert(hasOpfs, `${engineName} on ${process.platform} has no Origin Private File System, and NO_OPFS does not say so`);
  if (engineName === "webkit" && process.platform === "darwin") return { store: "host", why: KEYCHAIN_REASON };
  return { store: "origin", why: "the origin-held wrapping key" };
}

async function runCase(engineName, name, body) {
  const started = Date.now();
  try {
    await body();
    results.push({ engine: engineName, name, ok: true });
    log(`PASS ${engineName} ${name} (${((Date.now() - started) / 1000).toFixed(1)}s)`);
  } catch (error) {
    results.push({ engine: engineName, name, ok: false });
    log(`FAIL ${engineName} ${name}: ${errorText(error)}`);
  }
}

// Opens `url` in the engine's page and starts a worker there, runs `body` with the worker's
// load outcome, then checks every message the page posted and received and what the page and
// its workers printed from the start of the page's load. `allowErrors` lets console errors
// pass, and a console message that `expected` matches passes.
async function inHarness(engine, url, { allowErrors = false, expected = undefined }, body) {
  const { page, seen, name } = engine;
  // Leave the previous case's page first, so that nothing it prints late counts here.
  await page.goto("about:blank");
  const since = mark(seen);
  await openHarness(page, url);
  await body(await api(page, "startWorker"));
  await checkProtocol(page);
  checkConsole(name, seen, since, { allowErrors, expected });
}

async function strictCrossOrigin(engine) {
  const { page, name, state } = engine;
  assert(engine.store !== undefined, `no store: ${engine.storeError}`);
  await inHarness(engine, `${harnessOrigin}/harness/strict/index.html`, {}, async (load) => {
    const found = [];
    timings[name].load_ms = expectOk(load, "worker:load").init_ms;
    expectOk(await run(page, "pure"), "pure");
    timings[name].first_call = expectOk(await run(page, "worker:timings"), "worker:timings").first_call;
    state.crossPreflights = (await reachCounted(engine, `reach-${engine.run}-cross`)).preflights;
    assert(state.crossPreflights >= 1, "switch_reach from the harness origin sent no preflight");

    const runArgs = { run: engine.run, store: engine.store, switch: rigOrigin };
    // From here on, the run's stores may exist, even when flow_pay fails part way.
    state.storesMayExist = engine.store !== "memory";
    const pay = expectOk(await run(page, "flow_pay", runArgs, FLOW_MS), "flow_pay");
    assert(typeof pay.timings?.pay === "number", `flow_pay returned no pay timing: ${JSON.stringify(pay.timings)}`);
    timings[name].flow_pay = pay.timings;
    found.push(...(await violations(page, name)));
    // The reload starts a new message log, so this load's messages are checked now.
    await checkProtocol(page);

    const instance = await api(page, "pageInstance");
    const workersBefore = state.workers;
    log(`${name}: reloading the page`);
    await page.reload();
    await waitForHarness(page);
    assert((await api(page, "pageInstance")) !== instance, "the reload did not load the page again");
    const reloaded = expectOk(await api(page, "startWorker"), "worker:load after the reload");
    assert(state.workers > workersBefore, "no new worker started after the reload");
    timings[name].reload_load_ms = reloaded.init_ms;

    if (engine.store === "memory") {
      log(`WARNING ${name}: flow_read_back and cleanup not run: the in-memory store does not survive a reload (${engine.storeWhy})`);
    } else {
      expectOk(await run(page, "flow_read_back", { ...runArgs, ...idsOf(pay) }, FLOW_MS), "flow_read_back");
      expectOk(await run(page, "cleanup", { run: engine.run }, FLOW_MS), "cleanup");
      state.storesMayExist = false;
    }
    found.push(...(await violations(page, name)));
    assert(found.length === 0, `Content Security Policy violations: ${JSON.stringify(found)}`);
  });

  // Last, as a blocked eval is itself reported as a violation and, in some engines, on the
  // console: the strict worker's own policy refuses JavaScript eval.
  const probe = expectOk(await run(page, "worker:probe_function"), "worker:probe_function");
  assert(probe.ran === 0, `the strict worker ran new Function: ${JSON.stringify(probe)}`);
  await checkProtocol(page);
}

async function sameOrigin(engine) {
  const { page, name, state } = engine;
  await inHarness(engine, `${rigOrigin}/rig/harness/strict/index.html`, {}, async (load) => {
    timings[name].same_origin_load_ms = expectOk(load, "worker:load").init_ms;
    expectOk(await run(page, "pure"), "pure");
    const tag = `reach-${engine.run}-same`;
    const counted = await reachCounted(engine, tag);
    assert(counted.requests >= 1, `the rig counted no request under ${tag}`);
    assert(counted.preflights === 0, `the same-origin calls sent ${counted.preflights} preflights`);
    assert(
      state.crossPreflights >= 1,
      "no cross-origin preflight count to compare with: the strict, cross-origin case did not get that far",
    );
    const found = await violations(page, name);
    assert(found.length === 0, `Content Security Policy violations: ${JSON.stringify(found)}`);
  });
}

async function noWasm(engine) {
  // The engines report the refusal on the console in their own words.
  await inHarness(engine, `${harnessOrigin}/harness/no-wasm/index.html`, { allowErrors: true }, async (load) => {
    assert(
      load?.outcome === "error" && load.code === "CompileError",
      `worker:load without 'wasm-unsafe-eval' did not fail with a CompileError: ${JSON.stringify(load)}`,
    );
  });
}

async function workerNoHeader(engine) {
  await inHarness(engine, `${harnessOrigin}/harness/worker-no-header/index.html`, {}, async (load) => {
    expectOk(load, "worker:load");
    const probe = expectOk(await run(engine.page, "worker:probe_function"), "worker:probe_function");
    assert(
      probe.ran === 1,
      `the worker served without a policy did not run new Function: ${JSON.stringify(probe)}`,
    );
  });
}

function originOf(url) {
  try {
    return new URL(url).origin;
  } catch {
    return undefined;
  }
}

// The policy, not some other fault, must stop the call: it fails with the code `switch`, the
// rig counts no request under its tag (so nothing reached the rig, not even a preflight), and
// a connect-src violation naming the Switch is reported where the engine fires one.
async function noSwitch(engine) {
  const { page, name } = engine;
  await inHarness(engine, `${harnessOrigin}/harness/no-switch/index.html`, { allowErrors: true }, async (load) => {
    expectOk(load, "worker:load");
    const tag = `reach-${engine.run}-blocked`;
    const reach = await run(page, "switch_reach", { switch: rigOrigin, tag });
    assert(
      reach?.outcome === "error" && reach.code === "switch",
      `switch_reach under connect-src without the Switch did not fail with the code switch: ${JSON.stringify(reach)}`,
    );
    const counted = await rigCount(tag);
    assert(
      counted.requests === 0,
      `the rig counted ${counted.requests} requests under ${tag}: the policy did not stop the fetch`,
    );
    const blocked = (await violations(page, name)).filter(
      (event) => event.directive === "connect-src" && originOf(event.blocked) === rigOrigin,
    );
    const evidence =
      blocked.length > 0
        ? `a connect-src violation in the ${blocked[0].where} naming ${rigOrigin}, and no request at the rig`
        : "no violation event; no request at the rig";
    log(`${name}: switch_reach refused as expected (${reach.code}: ${reach.message}); ${evidence}`);
  });
}

async function panic(engine) {
  const { page } = engine;
  // The panic hook writes the panic to the console, and engines report the uncaught error.
  await inHarness(engine, `${harnessOrigin}/harness/strict/index.html`, { allowErrors: true }, async (load) => {
    expectOk(load, "worker:load");
    const call = await run(page, "panic", {}, PANIC_MS);
    assert(
      call?.outcome === "worker_died",
      `the panic call did not end with the worker's death within ${PANIC_MS} ms: ${JSON.stringify(call)}`,
    );
    const messages = await api(page, "messages");
    assert(
      !messages.some((entry) => entry.direction === "received" && entry.id === call.id),
      "the panic call was answered",
    );
    const events = await api(page, "workerEvents");
    const of = (type) => events.some((event) => event.type === type && event.generation === call.generation);
    assert(of("error"), `the page saw no error event of the worker: ${JSON.stringify(events)}`);
    assert(of("replaced"), `the page did not replace the worker: ${JSON.stringify(events)}`);
    const replacement = await api(page, "currentWorker");
    assert(
      replacement?.generation === call.generation + 1 && replacement.load?.outcome === "ok",
      `the replacement worker did not load: ${JSON.stringify(replacement)}`,
    );
    const pure = await run(page, "pure");
    expectOk(pure, "pure on the replacement worker");
    assert(pure.generation === call.generation + 1, `pure ran on worker ${pure.generation}`);
  });
}

// A worker that dies before its load passed is not replaced: its death is the outcome, once.
// The script does not exist, so the rig answers 404 and the worker fires its error event.
async function startFailure(engine) {
  const { page } = engine;
  const script = "missing-worker.mjs";
  // Engines report the failed script load on the console in their own words; Firefox names
  // the 404 answer's text/plain type, which the check for the .wasm's type would take.
  const expected = /missing-worker\.mjs/;
  await inHarness(engine, `${harnessOrigin}/harness/strict/index.html`, { allowErrors: true, expected }, async (load) => {
    expectOk(load, "worker:load");
    const failed = await api(page, "startWorker", script);
    assert(
      failed?.outcome === "worker_died",
      `the load on a worker whose script does not exist did not end with its death: ${JSON.stringify(failed)}`,
    );
    // A page that replaced such a worker would start one after another in this time.
    await sleep(1000);
    const later = await run(page, "pure");
    assert(later?.outcome === "worker_died", `a call after the start failure: ${JSON.stringify(later)}`);
    const events = (await api(page, "workerEvents")).filter((event) => event.generation >= failed.generation);
    const types = events.map((event) => event.type);
    assert(
      types.filter((type) => type === "started").length === 1 &&
        types.filter((type) => type === "start failed").length === 1 &&
        !types.includes("replaced"),
      `the page did not report one start failure without a new worker: ${JSON.stringify(events)}`,
    );
  });
}

// Removes what the cases left in each origin's OPFS and IndexedDB, after a cleanup call when
// the run's stores may still exist. Fails when a run that passed left anything.
async function storage(engine) {
  const { page, name, state } = engine;
  const left = [];
  for (const url of [`${harnessOrigin}/harness/strict/index.html`, `${rigOrigin}/rig/harness/strict/index.html`]) {
    await openHarness(page, url);
    if (state.storesMayExist && url.startsWith(harnessOrigin)) {
      log(`${name}: a case stopped before cleanup; calling it now`);
      const load = await api(page, "startWorker");
      if (load?.outcome === "ok") {
        const cleaned = await run(page, "cleanup", { run: engine.run }, FLOW_MS);
        log(`${name}: cleanup: ${JSON.stringify(cleaned)}`);
      }
    }
    const found = await api(page, "clearStorage");
    assert(found.error === undefined, `${url}: cannot empty the origin's storage: ${found.error}`);
    if (found.entries.length > 0) left.push(`${url}: OPFS entries ${found.entries.join(", ")}`);
    const filled = found.databases.filter(({ records }) => records > 0);
    if (filled.length > 0) left.push(`${url}: IndexedDB records in ${filled.map((db) => db.name).join(", ")}`);
  }
  // Only a passing run must leave nothing; a failed case may well have left its stores.
  const failedBefore = results.some((result) => result.engine === name && !result.ok);
  if (left.length > 0) log(`${name}: removed what was left: ${left.join("; ")}`);
  assert(failedBefore || left.length === 0, `the cases passed but left: ${left.join("; ")}`);
}

async function serviceWorkerProbe(name, engineType) {
  // Chromium does not apply Playwright's ignoreHTTPSErrors to a service worker's script, so
  // this launch alone turns off its certificate checks; it reaches nothing but the rig.
  const args = engineType.type === chromium ? ["--ignore-certificate-errors"] : [];
  const browser = await engineType.type.launch({ headless: true, ...engineType.options, args });
  closeables.add(browser);
  timings[name].version = browser.version();
  try {
    const context = await browser.newContext({ ignoreHTTPSErrors: true });
    const page = await context.newPage();
    const state = { workers: 0, serviceWorkers: 0 };
    const seen = watch(page, name, state, context);
    const since = mark(seen);
    await openHarness(page, `${harnessOrigin}/harness/strict/index.html`);
    const probe = await api(page, "serviceWorkerProbe");
    assert(probe.error === undefined, `the service worker probe failed: ${probe.error}`);
    const load = expectOk(probe.load, "worker:load in the service worker");
    timings[name].service_worker_load_ms = load.init_ms;
    expectOk(probe.pure, "pure in the service worker");
    const found = await collectViolations(page, name, probe.violations);
    assert(found.length === 0, `Content Security Policy violations: ${JSON.stringify(found)}`);
    await checkProtocol(page);
    // Only Chromium reports a service worker's console to Playwright; elsewhere this checks
    // the page's console alone.
    if (engineType.type === chromium) {
      assert(state.serviceWorkers > 0, "Playwright reported no service worker, so its console was not watched");
    }
    checkConsole(name, seen, since);
  } finally {
    await browser.close();
    closeables.delete(browser);
  }
}

async function runEngine(name) {
  const engineType = ENGINES[name];
  timings[name] = { platform: process.platform };
  const state = { workers: 0, crossPreflights: 0, storesMayExist: false };
  const profileDir = mkdtempSync(join(tmpdir(), "paykit-browser-harness-profile-"));
  profileDirs.add(profileDir);
  const context = await engineType.type.launchPersistentContext(profileDir, {
    headless: true,
    ...engineType.options,
    ignoreHTTPSErrors: true,
  });
  closeables.add(context);
  try {
    const page = context.pages()[0] ?? (await context.newPage());
    const engine = {
      name,
      page,
      state,
      seen: watch(page, name, state),
      // Short, lower-case and unique per run: the scenarios name their stores from it.
      run: `h${randomBytes(4).toString("hex")}`,
    };
    try {
      await openHarness(page, `${harnessOrigin}/harness/strict/index.html`);
      const { store, why } = storeFor(name, await api(page, "hasOpfs"));
      engine.store = store;
      engine.storeWhy = why;
    } catch (error) {
      engine.storeError = error.message;
    }
    timings[name].store = engine.store ?? null;
    const readBack = engine.store !== undefined && engine.store !== "memory";
    variants.push(
      `${name} on ${process.platform}: store ${engine.store ?? "none"} (${engine.storeWhy ?? engine.storeError}); ` +
        `read-back after reload ${readBack ? "run" : "NOT run"}`,
    );
    log(`${name}: run ${engine.run}, ${variants.at(-1)}`);
    if (engine.store === "memory" && process.env.GITHUB_ACTIONS) {
      console.log(`::warning title=Harness read-back not run::${name}: ${engine.storeWhy}`);
    }

    await runCase(name, "strict, cross-origin", () => strictCrossOrigin(engine));
    await runCase(name, "same-origin, no preflight", () => sameOrigin(engine));
    await runCase(name, "control: no-wasm", () => noWasm(engine));
    await runCase(name, "control: worker-no-header", () => workerNoHeader(engine));
    await runCase(name, "control: no-switch", () => noSwitch(engine));
    await runCase(name, "panic", () => panic(engine));
    await runCase(name, "start failure", () => startFailure(engine));
    await runCase(name, "storage left", () => storage(engine));
  } finally {
    await Promise.race([context.close().catch(() => {}), sleep(10_000)]);
    closeables.delete(context);
    rmSync(profileDir, { recursive: true, force: true, maxRetries: 3 });
    profileDirs.delete(profileDir);
  }
  await runCase(name, "service worker probe", () => serviceWorkerProbe(name, engineType));
}

function number(value) {
  return typeof value === "number" ? value.toFixed(1) : "-";
}

// Prints the timings table and, when GITHUB_STEP_SUMMARY is set, appends it to that file as
// Markdown, so it shows on the run's summary page.
function printTimings() {
  const header = ["engine", "version", "store", "load", "first call", "pay", "flow total", "reload load", "same-origin load", "sw load"];
  const rows = Object.entries(timings).map(([name, t]) => [
    name,
    t.version ?? "-",
    t.store ?? "-",
    number(t.load_ms),
    t.first_call ? `${number(t.first_call.ms)} (${t.first_call.scenario})` : "-",
    number(t.flow_pay?.pay),
    number(t.flow_pay?.total),
    number(t.reload_load_ms),
    number(t.same_origin_load_ms),
    number(t.service_worker_load_ms),
  ]);
  const widths = header.map((title, i) => Math.max(title.length, ...rows.map((row) => row[i].length)));
  const line = (cells) => cells.map((cell, i) => cell.padEnd(widths[i])).join("  ");
  log("timings in ms (load = fetch + compile + instantiate in the worker):");
  for (const cells of [header, ...rows]) log(`  ${line(cells)}`);
  if (process.env.GITHUB_STEP_SUMMARY) {
    const markdown = [
      "### Hosted harness timings in ms",
      "",
      "load = fetch + compile + instantiate in the worker.",
      "",
      `| ${header.join(" | ")} |`,
      `| ${header.map(() => "---").join(" | ")} |`,
      ...rows.map((cells) => `| ${cells.join(" | ")} |`),
    ];
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${markdown.join("\n")}\n`);
  }
}

// Prints the timings and which engine ran which store, and writes the timings file, once,
// with what the engines run so far collected; nothing before the first engine.
function report() {
  if (reported || Object.keys(timings).length === 0) return;
  reported = true;
  printTimings();
  if (timingsFile) {
    const json = { switch_origin: rigOrigin, harness_origin: harnessOrigin, engines: timings };
    writeFileSync(timingsFile, `${JSON.stringify(json, null, 2)}\n`);
    log(`timings written to ${timingsFile}`);
  }
  log("which engine ran which store:");
  for (const line of variants) log(`  ${line}`);
}

async function main() {
  const engines = process.argv.slice(2);
  if (engines.length === 0 || !engines.every((name) => Object.hasOwn(ENGINES, name))) {
    return stop(2, `usage: harness-runner.mjs <${Object.keys(ENGINES).join("|")}>...`);
  }
  if (!LOCAL_ORIGIN.test(rigOrigin) || !LOCAL_ORIGIN.test(harnessOrigin) || rigOrigin === harnessOrigin) {
    return stop(
      2,
      "PAYKIT_BROWSER_RIG_ORIGIN and PAYKIT_BROWSER_HARNESS_ORIGIN must be two https://localhost:<port> origins",
    );
  }
  if (!(timeoutSecs > 0 && timeoutSecs <= 2147483)) {
    return stop(2, "PAYKIT_BROWSER_HARNESS_TIMEOUT_SECS must be a positive number, at most 2147483");
  }
  setTimeout(() => stop(124, `timed out after ${timeoutSecs}s`), timeoutSecs * 1000).unref();

  await runCase("rig", "policy headers", policyHeaders);
  for (const name of engines) {
    log(`== ${name}`);
    await runEngine(name);
  }

  report();
  const failed = results.filter((result) => !result.ok);
  if (failed.length > 0) {
    log(`FAILED ${failed.length} of ${results.length}: ${failed.map((r) => `${r.engine}: ${r.name}`).join("; ")}`);
    return stop(1);
  }
  log(`all ${results.length} passed: ${engines.join(", ")}`);
  return stop(0);
}

main().catch((error) => stop(1, `FAIL: ${errorText(error)}`));
