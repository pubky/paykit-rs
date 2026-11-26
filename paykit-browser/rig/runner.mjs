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
//   PAYKIT_BROWSER_TIMEOUT_SECS  hard limit for the whole run, at most 2147483 (the longest
//                                timer Node accepts). Default 300.
//   WASM_BINDGEN_TEST_RUNNER     the wasm-bindgen-test-runner binary. Default: from PATH.
//
// wasm-bindgen-test-runner runs with NO_HEADLESS=1, so it only generates the bindings and
// serves the test page on a free loopback port. This script opens that page in the engine,
// relays console output and errors, waits for the harness's "test result:" line, and exits 0
// only when at least one test ran and none failed. The runner and the browser are always
// stopped, on timeout and on SIGINT or SIGTERM too. Given `--list`, it runs only
// wasm-bindgen-test-runner, which prints the test names, and exits with its exit code.

import { spawn } from "node:child_process";
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
// Matches only a complete line, so a URL split across output chunks is never taken early.
const SERVING_LINE = /available at (http:\/\/\S+)\r?\n/;

const engineName = process.env.PAYKIT_BROWSER_ENGINE || "chromium";
const contextName = process.env.PAYKIT_BROWSER_CONTEXT || "page";
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
let browser;
let finished = false;

function log(line) {
  process.stdout.write(`${line}\n`);
}

// Stop the runner and the browser, then exit. Safe to call more than once.
async function finish(code, why) {
  if (finished) return;
  finished = true;
  if (why) log(`[runner] ${why}`);
  log(`[runner] elapsed ${((Date.now() - started) / 1000).toFixed(1)}s`);
  if (browser) {
    await Promise.race([browser.close().catch(() => {}), sleep(5000)]);
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
process.on("SIGTERM", () => finish(143, "terminated"));
setTimeout(
  () => finish(124, `timed out after ${timeoutSecs}s without a test result`),
  timeoutSecs * 1000,
);

// Start wasm-bindgen-test-runner in serve-only mode, and resolve with the page URL.
function serveTests(args) {
  const env = { ...process.env, NO_HEADLESS: "1", WASM_BINDGEN_TEST_ADDRESS: "127.0.0.1:0" };
  // The runner refuses more than one mode variable: set exactly the one asked for.
  for (const name of MODE_VARIABLES) delete env[name];
  env[CONTEXTS[contextName]] = "1";

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
        return;
      }
      reject(new Error(`${runnerBin} exited before serving (code ${code}, signal ${signal})`));
    });
  });
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

async function main() {
  // Own properties only, so a name such as "toString" is not taken from Object.prototype.
  if (!Object.hasOwn(ENGINES, engineName)) {
    return finish(2, `unknown PAYKIT_BROWSER_ENGINE "${engineName}"`);
  }
  if (!Object.hasOwn(CONTEXTS, contextName)) {
    return finish(2, `unknown PAYKIT_BROWSER_CONTEXT "${contextName}"`);
  }
  const engine = ENGINES[engineName];

  const args = process.argv.slice(2);
  if (args.includes("--list")) return listTests(args);
  const url = await serveTests(args);
  browser = await engine.type.launch({ headless: true, ...engine.options });
  log(`[runner] engine ${engineName} ${browser.version()}, context ${contextName}, ${url}`);

  const page = await browser.newPage();
  let workers = 0;
  page.on("console", (msg) => {
    // The wasm-bindgen test harness forwards the test worker's console output to the page, so
    // it is logged here. A message the engine reports from the worker itself, such as its
    // uncaught error in some engines, is logged by the worker's own listener below.
    if (!msg.worker?.()) log(`[page ${msg.type()}] ${msg.text()}`);
  });
  // The first uncaught error of the page or the worker, such as a test module that fails to
  // load. It means no result is coming, so the poll below stops on it.
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
      // The harness replaces the worker's console methods and sends their output to the page,
      // so an error on the worker's own console comes from the engine: an uncaught error.
      if (msg.type() === "error") uncaught ??= `worker error: ${msg.text()}`;
    });
  });
  await page.goto(url);

  // The harness writes its report into #output and ends it with the "test result:" line.
  let printed = "";
  for (;;) {
    const text = await page.evaluate(() => document.getElementById("output")?.textContent ?? "");
    if (text.startsWith(printed)) {
      process.stdout.write(text.slice(printed.length));
    } else {
      process.stdout.write(`\n${text}`);
    }
    printed = text;
    if (text.includes("test result: ")) break;
    // Fail now rather than at the timeout. The harness page also reports an error event of its
    // dedicated worker, as a "Worker error:" line in #output.
    if (text.includes("\nWorker error: ")) uncaught ??= "worker error, reported by the page";
    if (uncaught) return finish(1, `${uncaught} (before a test result)`);
    await sleep(100);
  }
  // Let late console messages arrive before the summary.
  await sleep(200);

  const results = [...printed.matchAll(RESULT_LINE)];
  const last = results.at(-1);
  if (!last) return finish(1, "no parsable test result line");
  const [, status, passed, failed] = last;
  if (contextName === "worker" && workers === 0) {
    return finish(1, "the tests reported, but no dedicated worker was seen");
  }
  const ok = status === "ok" && Number(failed) === 0 && Number(passed) > 0;
  const summary = `${engineName} ${browser.version()} ${contextName}: ${passed} passed, ${failed} failed`;
  return finish(ok ? 0 : 1, ok ? `PASS ${summary}` : `FAIL ${summary}`);
}

main().catch((error) => finish(1, errorText(error)));
