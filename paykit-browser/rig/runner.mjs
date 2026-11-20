#!/usr/bin/env node
// Runs one wasm-bindgen test binary in a browser engine driven by Playwright.
//
// Cargo uses it as the wasm32 runner and passes the test .wasm file and the test arguments:
//
//   CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER="node /abs/path/paykit-browser/rig/runner.mjs" \
//   PAYKIT_BROWSER_ENGINE=firefox PAYKIT_BROWSER_CONTEXT=worker \
//   cargo test -p paykit-browser --target wasm32-unknown-unknown --tests
//
// Environment:
//   PAYKIT_BROWSER_ENGINE   chromium | firefox | webkit | chrome (the installed Google Chrome,
//                           through Playwright's chromium with channel "chrome"). Default chromium.
//   PAYKIT_BROWSER_CONTEXT  page | worker (a dedicated worker). Default page.
//   PAYKIT_BROWSER_CONTEXTS every context the caller runs the tests in, separated by spaces,
//                           as test.sh sets it (see "Target modes"). Default: the one above.
//   PAYKIT_BROWSER_TIMEOUT_SECS  hard limit for the whole run, at most 2147483 (the longest
//                                timer Node accepts). Default 300.
//   WASM_BINDGEN_TEST_RUNNER     the wasm-bindgen-test-runner binary. Default: from PATH.
//   PAYKIT_BROWSER_RIG_ORIGIN    set by `test.sh --switch` for the network tests, whose rig
//                                (switch-rig.mjs) serves a self-signed certificate. When it is
//                                set, the page and its worker ignore certificate errors.
//   PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS  1 leaves the tests named in KEYCHAIN_TESTS below out of
//                                WebKit runs, for local runs on macOS only, and is refused on
//                                any other platform; 0 or unset runs every test. Playwright's
//                                WebKit on macOS protects a CryptoKey stored in IndexedDB with a
//                                key from the user's Keychain, and asks for Keychain access in a
//                                dialog that stops the test until someone answers it. Each test
//                                left out is printed as "[runner] NOT RUN <target>::<test>:
//                                needs Keychain access ...", and test.sh counts them. CI never
//                                sets it. It is separate from NO_OPFS below, which leaves out
//                                whole targets on Linux and is reported on its own.
//
// wasm-bindgen-test-runner runs with NO_HEADLESS=1, so it only generates the bindings and
// serves the test page on a free loopback port. This script opens that page in the engine,
// relays console output and errors, waits for the harness's "test result:" line, and exits 0
// only when at least one test ran and none failed. When a test name filter or `--skip` given to
// `cargo test` leaves no test of a test target to run, wasm-bindgen-test-runner says "no tests
// to run!" and exits without serving; given a filter, this script then exits 0 as well, so the
// filter can pick tests of one target (`test.sh` still fails an engine where no target ran a
// test). The runner and the browser are always stopped, on timeout and on SIGINT or SIGTERM
// too. Given `--list`, it runs only wasm-bindgen-test-runner, which prints the test names, and
// exits with its exit code.
//
// Target modes. A test target named in TARGET_MODES below runs differently; the name comes from
// the .wasm path cargo passes (`<target>-<hash>.wasm`), and every other target runs as above,
// in a new browser context with no profile on disk.
//   context     the only context the target runs in (any value but page or worker stops every
//               run at startup). Its `wasm_bindgen_test_configure!` fixes the context and
//               overrides PAYKIT_BROWSER_CONTEXT. Asked for the other context, this script runs
//               the target in its own context when PAYKIT_BROWSER_CONTEXTS does not include
//               that one, so a run of one context still covers it. When it does, the target
//               runs in its own context's run instead: this script prints
//               "[runner] SKIP <target>: ..." and exits 0, printing no PASS line, so a target is
//               never counted twice.
//   persistent  the browser runs with a profile in a new temporary folder (Playwright's
//               launchPersistentContext), deleted on every exit. The Origin Private File System
//               needs it: in a context without a profile, WebKit refuses it and Chromium keeps
//               it in memory only. On every exit with the test page still served, this script
//               closes the page, so its worker's file locks and database connections are
//               released, and removes every entry left in the origin's OPFS root and every
//               IndexedDB database of the origin. The origin, a random loopback port, is this
//               run's alone. The tests remove what they create, so a passing run that left an
//               entry or an IndexedDB record, or whose leftovers cannot be removed, fails (an
//               empty database does not: the store keeps its key database); a failed test
//               cannot, and this keeps its files from staying on the machine (Playwright's
//               WebKit on macOS keeps OPFS and IndexedDB in the user's Library).
//   reload      after the first test result, the page is reloaded and the tests run again in a
//               new WebAssembly instance of the same origin and profile. The run passes only if
//               both results pass and the tests printed RELOAD_MARKERS[0] before the reload and
//               RELOAD_MARKERS[1] after it (on the console or in the report), so tests that skip
//               either step fail. The tests pick their step from what they stored.
//   Where NO_OPFS below names the engine and platform, an engine build known to have no Origin
//   Private File System, a persistent target is not run: before the tests load, this script
//   checks from a page of the test origin that the engine still has no `navigator.storage`,
//   prints "[runner] EXCLUDED <target>: <reason>" and exits 0, printing no PASS line; test.sh
//   lists these at the end, as a warning. If the engine has it after all, the run fails, so the
//   entry is removed and the target runs again.
// A table entry whose test target does not exist under tests/ stops every run at startup, and
// a target whose name starts with "store" but has no entry stops its run, so a renamed or new
// store target never runs without its mode.

import { execFile, spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, firefox, webkit } from "playwright";

const ENGINES = {
  chromium: { type: chromium, options: {} },
  firefox: { type: firefox, options: {} },
  webkit: { type: webkit, options: {} },
  chrome: { type: chromium, options: { channel: "chrome" } },
};
// The wasm-bindgen-test-runner variable that selects each context.
const CONTEXTS = {
  page: "WASM_BINDGEN_USE_BROWSER",
  worker: "WASM_BINDGEN_USE_DEDICATED_WORKER",
};
// Every mode variable wasm-bindgen-test-runner 0.2.130 reads (TestMode::env in
// src/wasm_bindgen_test_runner.rs). Other WASM_BINDGEN_USE_ variables, such as
// WASM_BINDGEN_USE_NO_MODULE, are options and pass through.
const MODE_VARIABLES = [
  "WASM_BINDGEN_USE_DENO",
  "WASM_BINDGEN_USE_BROWSER",
  "WASM_BINDGEN_USE_DEDICATED_WORKER",
  "WASM_BINDGEN_USE_SHARED_WORKER",
  "WASM_BINDGEN_USE_SERVICE_WORKER",
  "WASM_BINDGEN_USE_NODE_EXPERIMENTAL",
];
const RESULT_LINE = /test result: (ok|FAILED)\. (\d+) passed; (\d+) failed;/g;
// wasm-bindgen-test-runner prints this, and exits 0, when no test matches the filter.
const NO_TESTS_LINE = /^no tests to run!\r?$/m;
// Matches only a complete line, so a URL split across output chunks is never taken early.
const SERVING_LINE = /available at (http:\/\/\S+)\r?\n/;
// The test targets that need more than a new page, by target name (see "Target modes" above).
const TARGET_MODES = {
  store: { context: "worker", persistent: true },
  store_flow: { context: "worker", persistent: true },
  store_reload: { context: "worker", persistent: true, reload: true },
  store_page: { context: "page", persistent: true },
};
const DEFAULT_MODE = { context: undefined, persistent: false, reload: false };
// Engine builds with no Origin Private File System, by engine and Node's process.platform, and
// why. Playwright's WebKit for Linux exposes no `navigator.storage` in a page and no
// `FileSystemSyncAccessHandle` in a dedicated worker, also in a persistent context.
const NO_OPFS = {
  webkit: {
    linux: "Playwright's WebKit for Linux has no navigator.storage, so no Origin Private File System",
  },
};
// The tests that store a wrapping CryptoKey in IndexedDB, by test target and test name: every
// test that opens a store with `WrappingKey::Origin`, which is all of `store_flow` and
// `store_reload`. In `store`, the host-key tests, the in-memory tests and the invalid-name test
// store none. See PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS.
const KEYCHAIN_TESTS = {
  store: /^(on_opfs::|opening::(?!test_store_opfs_invalid_name_creates_nothing$)|sealing::test_store_seal_)/,
  store_flow: /^/,
  store_reload: /^/,
};
const KEYCHAIN_REASON = "needs Keychain access for Playwright's WebKit on macOS";
const skipKeychainTests = process.env.PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS || "0";
if (skipKeychainTests !== "0" && skipKeychainTests !== "1") {
  log(`[runner] PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS must be 0 or 1, not "${skipKeychainTests}"`);
  process.exit(2);
}
if (skipKeychainTests === "1" && process.platform !== "darwin") {
  log(`[runner] PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS=1 is for macOS only, not ${process.platform}`);
  process.exit(2);
}
// What a reload target prints before the reload and after it.
const RELOAD_MARKERS = ["BEFORE RELOAD DONE", "AFTER RELOAD DONE"];
const CONTEXT_WORDS = { page: "a page", worker: "a dedicated worker" };
// Cargo's test targets of this crate: tests/<target>.rs and tests/<target>/main.rs.
const TESTS_DIR = join(dirname(fileURLToPath(import.meta.url)), "..", "tests");
for (const [target, mode] of Object.entries(TARGET_MODES)) {
  if (mode.context !== undefined && !Object.hasOwn(CONTEXTS, mode.context)) {
    log(`[runner] TARGET_MODES.${target}.context "${mode.context}" is not page or worker`);
    process.exit(2);
  }
  if (!existsSync(join(TESTS_DIR, `${target}.rs`)) && !existsSync(join(TESTS_DIR, target, "main.rs"))) {
    log(`[runner] TARGET_MODES.${target} names no test target under ${TESTS_DIR}`);
    process.exit(2);
  }
}

const engineName = process.env.PAYKIT_BROWSER_ENGINE || "chromium";
// A target that runs in one context only may change it (see "Target modes").
let contextName = process.env.PAYKIT_BROWSER_CONTEXT || "page";
const requestedContexts = process.env.PAYKIT_BROWSER_CONTEXTS?.split(/\s+/) ?? [contextName];
const timeoutSecs = Number(process.env.PAYKIT_BROWSER_TIMEOUT_SECS || 300);
// Node fires a timer longer than 2^31-1 ms after 1 ms instead, which would end the run at once.
const MAX_TIMEOUT_SECS = 2147483;
if (!(timeoutSecs > 0 && timeoutSecs <= MAX_TIMEOUT_SECS)) {
  log(`[runner] PAYKIT_BROWSER_TIMEOUT_SECS must be a positive number, at most ${MAX_TIMEOUT_SECS}`);
  process.exit(2);
}
const runnerBin = process.env.WASM_BINDGEN_TEST_RUNNER || "wasm-bindgen-test-runner";
const started = Date.now();

let child;
// What finish() closes: the browser, or a target's persistent context.
let closeable;
// A persistent context's temporary profile folder, removed on exit.
let profileDir;
// A persistent target's test page URL, whose origin's OPFS and IndexedDB finish() empties.
let sweepUrl;
let finished = false;

function log(line) {
  process.stdout.write(`${line}\n`);
}

// Stop the runner and the browser, then exit. Safe to call more than once.
async function finish(code, why) {
  if (finished) return;
  finished = true;
  if (sweepUrl && closeable) {
    // `undefined` when the origin could not be emptied, which fails a run that passed: it
    // may leave files behind.
    const left = await Promise.race([
      sweepOrigin(closeable, sweepUrl).catch((error) => {
        log(`[runner] cannot empty the origin's storage: ${errorText(error)}`);
        return undefined;
      }),
      sleep(15000).then(() => {
        log("[runner] emptying the origin's storage timed out");
        return undefined;
      }),
    ]);
    if (left === undefined && code === 0) {
      code = 1;
      why = "FAIL: the Origin Private File System and IndexedDB could not be checked and emptied";
    } else if (left) {
      const kinds = [];
      if (left.entries.length > 0) {
        log(`[runner] removed what the tests left in the Origin Private File System: ${left.entries.join(", ")}`);
        kinds.push("entries in the Origin Private File System");
      }
      if (left.databases.length > 0) {
        const found = left.databases.map(({ name, records }) => `${name} (${records} records)`);
        log(`[runner] removed the origin's IndexedDB databases: ${found.join(", ")}`);
      }
      if (left.databases.some(({ records }) => records > 0)) kinds.push("records in IndexedDB");
      if (kinds.length > 0 && code === 0) {
        code = 1;
        why = `FAIL: the tests passed but left ${kinds.join(" and ")}`;
      }
    }
  }
  if (why) log(`[runner] ${why}`);
  log(`[runner] elapsed ${((Date.now() - started) / 1000).toFixed(1)}s`);
  if (closeable) {
    await Promise.race([closeable.close().catch(() => {}), sleep(5000)]);
  }
  if (child && child.exitCode === null && child.signalCode === null) {
    child.kill("SIGTERM");
    const exited = new Promise((resolve) => child.once("exit", resolve));
    if ((await Promise.race([exited.then(() => true), sleep(3000)])) !== true) {
      child.kill("SIGKILL");
    }
  }
  process.exit(code);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// Some engines leave the message out of the stack, so print both.
function errorText(error) {
  const message = error?.message ?? String(error);
  const stack = error?.stack ?? "";
  return stack.includes(message) ? stack : `${message}\n${stack}`.trimEnd();
}

process.on("SIGINT", () => finish(130, "interrupted"));
// The one place the profile folder is removed: every exit, finish()'s (on a result, a timeout,
// SIGINT or SIGTERM, after it has closed the browser) or a crash's.
process.on("exit", () => {
  try {
    if (profileDir) rmSync(profileDir, { recursive: true, force: true, maxRetries: 3 });
  } catch (error) {
    log(`[runner] cannot remove the profile folder ${profileDir}: ${error.message}`);
  }
});
process.on("SIGTERM", () => finish(143, "terminated"));
setTimeout(
  () => finish(124, `timed out after ${timeoutSecs}s without a test result`),
  timeoutSecs * 1000,
);

// Whether the arguments after the module path narrow the tests to run: a FILTER (a positional
// argument) or `--skip`. The value of `--format`, the runner's other option with a value, is
// not a filter.
function hasTestFilter(args) {
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--format") i += 1;
    else if (arg === "--skip" || arg.startsWith("--skip=") || !arg.startsWith("-")) return true;
  }
  return false;
}

// Start wasm-bindgen-test-runner in serve-only mode, and resolve with the page URL.
function serveTests(args) {
  const env = { ...process.env, NO_HEADLESS: "1", WASM_BINDGEN_TEST_ADDRESS: "127.0.0.1:0" };
  // The runner refuses more than one mode variable: set exactly the one asked for.
  for (const name of MODE_VARIABLES) delete env[name];
  env[CONTEXTS[contextName]] = "1";

  // Without a test name filter, a test target with no test to run still fails.
  const filtered = hasTestFilter(args.slice(1));

  return new Promise((resolve, reject) => {
    child = spawn(runnerBin, args, { env, stdio: ["ignore", "pipe", "pipe"] });
    // Kept only until the page URL is found; every chunk is relayed either way.
    let output = "";
    let served = false;
    const relay = (stream, out) => {
      stream.setEncoding("utf8");
      stream.on("data", (chunk) => {
        out.write(chunk);
        if (served) return;
        output += chunk;
        const url = output.match(SERVING_LINE);
        if (url) {
          served = true;
          resolve(url[1]);
        }
      });
    };
    relay(child.stdout, process.stdout);
    relay(child.stderr, process.stderr);
    child.on("error", (error) => reject(new Error(`cannot start ${runnerBin}: ${error.message}`)));
    child.on("exit", (code, signal) => {
      if (served) {
        finish(1, `${runnerBin} exited while serving (code ${code}, signal ${signal})`);
      }
    });
    // An exit before serving is judged on "close", which comes after both output streams have
    // ended: on "exit" the last chunk, with "no tests to run!", may not have arrived yet.
    child.on("close", (code, signal) => {
      if (served) return;
      if (code === 0 && filtered && NO_TESTS_LINE.test(output)) {
        finish(0, "no test matched the filter");
        return;
      }
      reject(new Error(`${runnerBin} exited before serving (code ${code}, signal ${signal})`));
    });
  });
}

// The test target's name from the test module's path, `<target>-<hash>.wasm`, and its mode.
function targetOf(modulePath) {
  const target = basename(modulePath ?? "", ".wasm").replace(/-[0-9a-f]{16}$/, "");
  return { target, mode: Object.hasOwn(TARGET_MODES, target) ? TARGET_MODES[target] : DEFAULT_MODE };
}

// Launch the engine and open a page: in a new browser, or, for a persistent target, in a
// context with a profile in a new temporary folder.
async function openPage(engine, persistent) {
  const options = { headless: true, ...engine.options };
  // Only the network tests reach the rig's self-signed certificate; every other run keeps the
  // engine's certificate checks. The page's setting covers its dedicated worker too.
  const pageOptions = { ignoreHTTPSErrors: Boolean(process.env.PAYKIT_BROWSER_RIG_ORIGIN) };
  if (!persistent) {
    const browser = await engine.type.launch(options);
    closeable = browser;
    return { browser, page: await browser.newPage(pageOptions) };
  }
  profileDir = mkdtempSync(join(tmpdir(), "paykit-browser-profile-"));
  const context = await engine.type.launchPersistentContext(profileDir, {
    ...options,
    ...pageOptions,
  });
  closeable = context;
  // Playwright's docs allow a persistent context without a browser object.
  return { browser: context.browser(), page: context.pages()[0] ?? (await context.newPage()) };
}

// Open a new page of the persistent context on `path` of `url`'s origin: an empty document
// Playwright answers itself, so no test runs and the test server is not asked.
async function emptyPage(context, url, path) {
  const page = await context.newPage();
  const emptyUrl = new URL(path, url).href;
  await page.route(emptyUrl, (route) =>
    route.fulfill({ status: 200, contentType: "text/html", body: "<!doctype html>" }),
  );
  await page.goto(emptyUrl);
  return page;
}

// Whether a page of `url`'s origin has the Origin Private File System.
async function hasOpfs(context, url) {
  const page = await emptyPage(context, url, "/opfs-probe");
  const has = await page.evaluate(() => typeof navigator.storage?.getDirectory === "function");
  await page.close();
  return has;
}

// Close the persistent context's test pages, which ends their workers and releases the file
// locks and database connections they hold, then remove every entry of the OPFS root of
// `url`'s origin and every IndexedDB database of that origin, from an empty page there. That
// page opens first: Playwright's Firefox cannot open a page in a persistent context whose last
// page has closed. Returns what it removed: { entries: names, databases: [{ name, records }] }.
async function sweepOrigin(context, url) {
  const tests = context.pages();
  const page = await emptyPage(context, url, "/opfs-cleanup");
  for (const test of tests) await test.close();
  const entries = await page.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    const names = [];
    for await (const name of root.keys()) names.push(name);
    for (const name of names) {
      // A closed worker's file locks may take a moment to be released: retry for about 5 s.
      for (let attempt = 1; ; attempt++) {
        try {
          await root.removeEntry(name, { recursive: true });
          break;
        } catch (error) {
          if (attempt === 25) throw error;
          await new Promise((resolve) => setTimeout(resolve, 200));
        }
      }
    }
    return names.sort();
  });
  // Each database is counted before it is deleted: the store's key database stays, empty, once
  // its last store is destroyed (src/store/keys.rs), so only records left fail a run. Counting
  // reads no record, so no stored CryptoKey is read.
  const databases = await page.evaluate(async () => {
    const found = [];
    for (const { name } of await indexedDB.databases()) {
      const records = await new Promise((resolve, reject) => {
        const request = indexedDB.open(name);
        request.onerror = () => reject(request.error);
        request.onsuccess = () => {
          const database = request.result;
          const storeNames = [...database.objectStoreNames];
          if (storeNames.length === 0) {
            database.close();
            resolve(0);
            return;
          }
          const transaction = database.transaction(storeNames, "readonly");
          let total = 0;
          for (const storeName of storeNames) {
            const counting = transaction.objectStore(storeName).count();
            counting.onsuccess = () => {
              total += counting.result;
            };
          }
          transaction.oncomplete = () => {
            database.close();
            resolve(total);
          };
          transaction.onabort = () => {
            database.close();
            reject(transaction.error);
          };
        };
      });
      found.push({ name, records });
      await new Promise((resolve, reject) => {
        const request = indexedDB.deleteDatabase(name);
        request.onsuccess = () => resolve();
        request.onerror = () => reject(request.error);
        // A connection still open delays the deletion until it closes: wait about 5 s for that.
        request.onblocked = () => {
          setTimeout(() => reject(new Error(`deleting the database ${name} stays blocked`)), 5000);
        };
      });
    }
    return found.sort((a, b) => (a.name < b.name ? -1 : 1));
  });
  return { entries, databases };
}

// Check the last test result in `text`: at least one test passed and none failed, and, given a
// marker, the tests printed it (on the console or in the report). `when` names the step for a
// reload target. Returns { counts } or { error }, the reason the run fails.
function checkResult(text, consoleText, marker, when = "") {
  const last = [...text.matchAll(RESULT_LINE)].at(-1);
  if (!last) return { error: `no parsable test result line${when}` };
  const [, status, passed, failed] = last;
  const counts = `${passed} passed, ${failed} failed`;
  if (!(status === "ok" && Number(failed) === 0 && Number(passed) > 0)) {
    return { error: `the tests${when} did not pass: ${counts}` };
  }
  if (marker && !consoleText.includes(marker) && !text.includes(marker)) {
    return { error: `the tests${when} passed without printing "${marker}"` };
  }
  return { counts };
}

// `--list` only prints the test names: the runner exits without serving a page, so no browser
// is started and the runner's own exit code is the result.
function listTests(args) {
  child = spawn(runnerBin, args, { stdio: "inherit" });
  child.on("error", (error) => finish(1, `cannot start ${runnerBin}: ${error.message}`));
  child.on("exit", (code) => {
    if (!finished) process.exit(code ?? 1);
  });
}

// The arguments with the tests of `target` that KEYCHAIN_TESTS names left out: list the tests
// the arguments select, print a NOT RUN line for each one left out, and add a `--skip` for it.
// When every test is left out, wasm-bindgen-test-runner finds none to run and the target passes
// without a PASS line, as for a filter that matches nothing.
async function withoutKeychainTests(target, args) {
  const listed = await new Promise((resolve, reject) => {
    execFile(runnerBin, [...args, "--list"], { maxBuffer: 16 << 20 }, (error, stdout) => {
      if (error) reject(new Error(`${runnerBin} --list failed: ${error.message}`));
      else resolve(stdout);
    });
  });
  const names = [...listed.matchAll(/^(.+): test\r?$/gm)].map((match) => match[1]);
  const excluded = names.filter((name) => KEYCHAIN_TESTS[target].test(name));
  // Without `--exact`, `--skip` leaves out every test whose name contains the given text.
  const left = new Set(excluded);
  const kept = names.filter((name) => !left.has(name));
  const clash = kept.find((name) => excluded.some((skip) => name.includes(skip)));
  if (clash) throw new Error(`leaving out the Keychain tests would also leave out ${clash}`);
  for (const name of excluded) log(`[runner] NOT RUN ${target}::${name}: ${KEYCHAIN_REASON}`);
  return [...args, ...excluded.flatMap((name) => ["--skip", name])];
}

async function main() {
  // Own properties only, so a name such as "toString" is not taken from Object.prototype.
  if (!Object.hasOwn(ENGINES, engineName)) {
    return finish(2, `unknown PAYKIT_BROWSER_ENGINE "${engineName}"`);
  }
  if (!Object.hasOwn(CONTEXTS, contextName)) {
    return finish(2, `unknown PAYKIT_BROWSER_CONTEXT "${contextName}"`);
  }
  const engine = ENGINES[engineName];

  let args = process.argv.slice(2);
  if (args.includes("--list")) return listTests(args);
  const { target, mode } = targetOf(args[0]);
  if (target.startsWith("store") && mode === DEFAULT_MODE) {
    return finish(2, `${target} looks like a store target but has no entry in TARGET_MODES`);
  }
  if (mode.context && mode.context !== contextName) {
    if (requestedContexts.includes(mode.context)) {
      return finish(0, `SKIP ${target}: runs only in ${CONTEXT_WORDS[mode.context]}`);
    }
    log(`[runner] ${target} runs only in ${CONTEXT_WORDS[mode.context]}: running it there`);
    contextName = mode.context;
  }
  const noOpfs = mode.persistent ? NO_OPFS[engineName]?.[process.platform] : undefined;
  // A target that NO_OPFS excludes runs no test at all, so none is reported as not run for the
  // Keychain: its EXCLUDED line below is the reason.
  const leaveOutKeychainTests =
    skipKeychainTests === "1" && engineName === "webkit" && Object.hasOwn(KEYCHAIN_TESTS, target);
  if (leaveOutKeychainTests && noOpfs === undefined) {
    args = await withoutKeychainTests(target, args);
  }
  const url = await serveTests(args);
  const { browser, page } = await openPage(engine, mode.persistent);
  if (noOpfs !== undefined) {
    if (!(await hasOpfs(closeable, url))) return finish(0, `EXCLUDED ${target}: ${noOpfs}`);
    return finish(1, `${engineName} on ${process.platform} has the Origin Private File System: remove its NO_OPFS entry`);
  }
  if (mode.persistent) sweepUrl = url;
  const version = browser?.version() ?? "(version unknown)";
  const run = `${engineName} ${version} ${contextName}`;
  log(`[runner] engine ${engineName} ${version}, context ${contextName}, ${url}`);

  let workers = 0;
  // A reload target's console text since the last load, where its marker is looked for.
  let consoleText = "";
  page.on("console", (msg) => {
    // The wasm-bindgen test harness forwards the test worker's console output to the page, so
    // it is logged here. A message the engine reports from the worker itself, such as its
    // uncaught error in some engines, is logged by the worker's own listener below.
    if (!msg.worker?.()) log(`[page ${msg.type()}] ${msg.text()}`);
    if (mode.reload) consoleText += `${msg.text()}\n`;
  });
  // The first uncaught error of the page or the worker since the last load, such as a test
  // module that fails to load. It means no result is coming, so the poll below stops on it.
  let uncaught;
  page.on("pageerror", (error) => {
    log(`[page error] ${errorText(error)}`);
    uncaught ??= `page error: ${error?.message ?? error}`;
  });
  page.on("crash", () => finish(1, "the page crashed"));
  page.on("worker", (worker) => {
    workers += 1;
    log(`[runner] dedicated worker started: ${worker.url()}`);
    worker.on("console", (msg) => {
      log(`[worker ${msg.type()}] ${msg.text()}`);
      if (mode.reload) consoleText += `${msg.text()}\n`;
      // The harness replaces the worker's console methods and sends their output to the page,
      // so an error on the worker's own console comes from the engine: an uncaught error.
      if (msg.type() === "error") uncaught ??= `worker error: ${msg.text()}`;
    });
  });
  await page.goto(url);

  // The harness writes its report into #output and ends it with the "test result:" line.
  let printed = "";
  // A reload target's counts before the reload, once it has reloaded.
  let beforeReload;
  for (;;) {
    const text = await page.evaluate(() => document.getElementById("output")?.textContent ?? "");
    if (text.startsWith(printed)) {
      process.stdout.write(text.slice(printed.length));
    } else {
      process.stdout.write(`\n${text}`);
    }
    printed = text;
    if (mode.reload && !beforeReload && text.includes("test result: ")) {
      await sleep(200);
      const step = checkResult(text, consoleText, RELOAD_MARKERS[0], " before the reload");
      if (step.error) return finish(1, `FAIL ${run}: ${step.error}`);
      if (contextName === "worker" && workers === 0) {
        return finish(1, "the tests reported before the reload, but no dedicated worker was seen");
      }
      beforeReload = step.counts;
      log("[runner] reloading the page");
      consoleText = "";
      workers = 0;
      uncaught = undefined;
      await page.reload();
      printed = "";
      continue;
    }
    if (text.includes("test result: ")) break;
    // Fail now rather than at the timeout. The harness page also reports an error event of its
    // dedicated worker, as a "Worker error:" line in #output.
    if (text.includes("\nWorker error: ")) uncaught ??= "worker error, reported by the page";
    if (uncaught) return finish(1, `${uncaught} (before a test result)`);
    await sleep(100);
  }
  // Let late console messages arrive before the summary.
  await sleep(200);

  if (contextName === "worker" && workers === 0) {
    return finish(1, "the tests reported, but no dedicated worker was seen");
  }
  if (mode.context === "page" && workers > 0) {
    return finish(1, `${target} runs only in a page, but a dedicated worker was seen`);
  }
  const step = mode.reload
    ? checkResult(printed, consoleText, RELOAD_MARKERS[1], " after the reload")
    : checkResult(printed, consoleText);
  if (step.error) return finish(1, `FAIL ${run}: ${step.error}`);
  const counts = mode.reload ? `${beforeReload} before the reload, ${step.counts} after it` : step.counts;
  return finish(0, `PASS ${run}: ${counts}`);
}

main().catch((error) => finish(1, errorText(error)));
