// The worker side of the paykit-browser hosted test harness. Test rig code only: it is not a
// proposal for the SDK's JavaScript surface. page.mjs starts it as a dedicated module worker;
// switch-rig.mjs serves it with its own Content Security Policy header, since a worker runs
// under its script response's policy, not its page's. sw.mjs imports `answer` from it.
//
// It answers the requests of the protocol in page.mjs, each exactly once. The names starting
// with "worker:" are answered here:
//   worker:load            fetches, compiles and instantiates the bundle (harness_bg.wasm),
//                          and answers { init_ms }, the time that took in this worker. A
//                          failure, such as a CompileError under a policy without
//                          'wasm-unsafe-eval', is answered with the error's name as the code.
//   worker:timings         { init_ms, first_call: { scenario, ms } or null }: the time of the
//                          first bundle scenario call, measured here.
//   worker:violations      { events: [...] }, the Content Security Policy violations this
//                          worker saw.
//   worker:probe_function  { ran: 1 } when `new Function` runs here, { ran: 0, error } when
//                          the worker's policy blocks it.
// Every other name goes to the bundle's runScenario(name, args). A scenario's error, whose
// message is "<code>: <text>", is answered with that code and text; any other error with its
// name, such as RuntimeError for a WebAssembly trap, and its message.

import init, { runScenario } from "./harness.js";

// The bundle's URL, given to the glue explicitly rather than taken from import.meta.url.
const WASM_URL = new URL("harness_bg.wasm", self.location.href);
const MAX_MESSAGE = 4096;
const CODED_MESSAGE = /^([a-z][a-z_]{0,31}): /;

const violations = [];
self.addEventListener("securitypolicyviolation", (event) => {
  violations.push({
    directive: event.effectiveDirective,
    blocked: event.blockedURI,
    source: event.sourceFile ?? "",
    disposition: event.disposition,
  });
});

const timings = { init_ms: null, first_call: null };
// The bundle's load, started by the first worker:load.
let loading = null;

function load() {
  loading ??= (async () => {
    const started = performance.now();
    await init({ module_or_path: WASM_URL });
    timings.init_ms = performance.now() - started;
  })();
  return loading.then(() => ({ init_ms: timings.init_ms }));
}

function probeFunction() {
  try {
    // Only to show whether this worker's policy allows JavaScript eval; nothing else runs it.
    const made = new Function("return 7");
    return { ran: made() === 7 ? 1 : 0 };
  } catch (error) {
    return { ran: 0, error: String(error?.name ?? "Error") };
  }
}

const STEPS = {
  "worker:load": load,
  "worker:timings": async () => timings,
  "worker:violations": async () => ({ events: violations }),
  "worker:probe_function": async () => probeFunction(),
};

function isRequest(message) {
  return (
    message !== null &&
    typeof message === "object" &&
    Object.getPrototypeOf(message) === Object.prototype &&
    Number.isSafeInteger(message.id) &&
    typeof message.scenario === "string" &&
    typeof message.args === "string"
  );
}

function failure(id, code, message) {
  return { id, status: "error", code, message: message.slice(0, MAX_MESSAGE) };
}

// The code and text of an error: the scenario's own code for a bundle error, or the name.
function failureOf(id, error) {
  const name = typeof error?.name === "string" ? error.name : "Error";
  const message = typeof error?.message === "string" ? error.message : String(error);
  const coded = name === "Error" ? CODED_MESSAGE.exec(message) : null;
  if (coded) return failure(id, coded[1], message.slice(coded[0].length));
  return failure(id, name, message);
}

// The reply to one request: strings and numbers only. `allowed`, when given, lists the only
// scenario names answered.
export async function answer(message, allowed = null) {
  if (!isRequest(message)) {
    const id = Number.isSafeInteger(message?.id) ? message.id : -1;
    return failure(id, "bad_request", "need { id: <integer>, scenario: <string>, args: <string> }");
  }
  const { id, scenario, args } = message;
  if (allowed !== null && !allowed.includes(scenario)) {
    return failure(id, "not_allowed", `this worker does not run ${scenario}`);
  }
  try {
    if (Object.hasOwn(STEPS, scenario)) {
      return { id, status: "ok", result: JSON.stringify(await STEPS[scenario]()) };
    }
    if (loading === null) return failure(id, "not_loaded", "send worker:load first");
    await loading;
    const started = performance.now();
    const result = await runScenario(scenario, args);
    timings.first_call ??= { scenario, ms: performance.now() - started };
    if (typeof result !== "string") return failure(id, "bad_result", "the scenario returned no string");
    return { id, status: "ok", result };
  } catch (error) {
    return failureOf(id, error);
  }
}

// In a dedicated worker, answer the page; sw.mjs installs its own listener.
if (typeof DedicatedWorkerGlobalScope !== "undefined" && self instanceof DedicatedWorkerGlobalScope) {
  self.addEventListener("message", (event) => {
    answer(event.data).then((reply) => self.postMessage(reply));
  });
}
