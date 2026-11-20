// The page side of the paykit-browser hosted test harness. Test rig code only: it is not a
// proposal for the SDK's JavaScript surface. The bundle runs in a dedicated module worker
// (worker.mjs); this page only exchanges plain-data messages with it:
//
//   request  { id: <number>, scenario: "<name>", args: "<JSON object>" }
//   reply    { id, status: "ok", result: "<JSON>" }
//          | { id, status: "error", code: "<code>", message: "<text>" }
//
// Only strings and numbers cross, never a key, a buffer, a port or any other object. The page
// records every message it posts and receives, with a description of each value's type, so
// that harness-runner.mjs can check that. Each call has an id, a pending entry and a timeout.
// A worker that fires an `error` event, or answers a call with a `RuntimeError` (a WebAssembly
// trap, such as a panic), is treated as dead: the page terminates it, rejects its pending
// calls, and starts and loads a new one. A panic in an async step never answers its call, so
// the `error` event is what ends it. A worker that dies before its load succeeded is not
// replaced, as a new one would fail the same way (a script or bundle that does not load): its
// death is the outcome of its load, and of every later call, until startWorker() is called.
//
// harness-runner.mjs drives the page with Playwright's page.evaluate, through the functions of
// window.paykitHarness at the end of this file. None of them throws: each returns plain data.

const DEFAULT_TIMEOUT_MS = 120_000;
const LOAD_TIMEOUT_MS = 60_000;
// A new id for each load of the page, so that the runner can tell a reload happened.
const pageInstance = crypto.randomUUID();
// Every message posted to or received from a worker, in order.
const messageLog = [];
// What happened to the workers: started, error, messageerror, replaced, start failed,
// stopped, loaded.
const workerEvents = [];
// The Content Security Policy violations the page saw.
const pageViolations = [];
let nextId = 1;
let generation = 0;
// The current worker: { generation, post, pending: Map(id -> { resolve, reject, timer }),
// dead, terminate, loaded: a promise of its load outcome, ready: whether that load passed }.
let current = null;

document.addEventListener("securitypolicyviolation", (event) => {
  pageViolations.push({
    directive: event.effectiveDirective,
    blocked: event.blockedURI,
    source: event.sourceFile ?? "",
    disposition: event.disposition,
  });
});

// The worker died: its calls never get a reply.
class WorkerDied extends Error {
  name = "WorkerDied";
}

class CallTimeout extends Error {
  name = "CallTimeout";
}

function tagOf(value) {
  return Object.prototype.toString.call(value);
}

// The type of a message and of each of its values, as the runner checks them. A message of
// the protocol is a plain object whose values are all strings or numbers.
function describe(value) {
  const plain =
    value !== null && typeof value === "object" && Object.getPrototypeOf(value) === Object.prototype;
  const fields = plain
    ? Reflect.ownKeys(value).map((key) => ({
        key: String(key),
        type: typeof value[key],
        tag: tagOf(value[key]),
      }))
    : [];
  return { tag: tagOf(value), plain, fields };
}

function record(direction, channel, data, ports) {
  messageLog.push({
    direction,
    channel,
    ports,
    id: typeof data?.id === "number" ? data.id : null,
    shape: describe(data),
  });
}

// Settles the pending call a reply answers.
function settle(target, reply) {
  const call = target.pending.get(reply?.id);
  if (call === undefined) {
    workerEvents.push({ type: "unmatched reply", generation: target.generation });
    return;
  }
  target.pending.delete(reply.id);
  clearTimeout(call.timer);
  call.resolve(reply);
  if (reply.status === "error" && reply.code === "RuntimeError") {
    replace(target, "RuntimeError reply");
  }
}

// Ends `target` for good and rejects its pending calls.
function retire(target, type, reason) {
  if (target.dead) return;
  target.dead = true;
  target.terminate();
  for (const call of target.pending.values()) {
    clearTimeout(call.timer);
    call.reject(new WorkerDied(`worker ${target.generation}: ${reason}`));
  }
  target.pending.clear();
  workerEvents.push({ type, generation: target.generation, reason });
}

// Replaces a dead current worker with a new, loaded one, unless it died before its load
// passed: then it stays the current worker, dead, so that its start-up failure is reported
// once, rather than a new worker started for each failure.
function replace(target, reason) {
  if (target.dead) return;
  if (!target.ready) {
    retire(target, "start failed", reason);
    return;
  }
  retire(target, "replaced", reason);
  if (current === target) spawn();
}

// Sends request `id` to `target` and resolves with its reply, or rejects with WorkerDied or
// CallTimeout.
function request(target, id, scenario, args, timeoutMs) {
  if (target.dead) return Promise.reject(new WorkerDied(`worker ${target.generation} is dead`));
  const message = { id, scenario, args: JSON.stringify(args) };
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      target.pending.delete(id);
      reject(new CallTimeout(`no reply to ${scenario} (id ${id}) in ${timeoutMs} ms`));
    }, timeoutMs);
    target.pending.set(id, { resolve, reject, timer });
    record("sent", target.channel, message, 0);
    target.post(message);
  });
}

// A call's outcome as plain data: { outcome: "ok", result } with the result parsed, or
// { outcome: "error", code, message }, "worker_died", "timeout" or "bad_reply"; with the id,
// the worker's generation and the time from post to reply in ms.
async function outcomeOf(target, scenario, args, timeoutMs) {
  const id = nextId;
  nextId += 1;
  const started = performance.now();
  const base = () => ({ id, generation: target.generation, ms: performance.now() - started });
  try {
    const reply = await request(target, id, scenario, args, timeoutMs);
    if (reply.status === "ok") {
      try {
        return { outcome: "ok", result: JSON.parse(reply.result), ...base() };
      } catch {
        return { outcome: "bad_reply", message: "the result is not JSON", ...base() };
      }
    }
    return { outcome: "error", code: String(reply.code), message: String(reply.message), ...base() };
  } catch (error) {
    if (error instanceof WorkerDied) return { outcome: "worker_died", message: error.message, ...base() };
    if (error instanceof CallTimeout) return { outcome: "timeout", message: error.message, ...base() };
    return { outcome: "error", code: error?.name ?? "Error", message: String(error?.message ?? error), ...base() };
  }
}

// A worker the page calls: `post` sends it a message, `terminate` ends it.
function makeTarget(generation, channel, post, terminate) {
  return { generation, channel, pending: new Map(), dead: false, ready: false, post, terminate };
}

// Records each message `source` delivers and settles the call of `target` it answers.
function listen(source, target) {
  source.addEventListener("message", (event) => {
    record("received", target.channel, event.data, event.ports.length);
    settle(target, event.data);
  });
}

// Starts a new dedicated module worker from `script`, a file next to this page, and loads the
// bundle in it.
function spawn(script = "worker.mjs") {
  generation += 1;
  const handle = new Worker(new URL(script, location.href), {
    type: "module",
    name: `paykit-harness-${generation}`,
  });
  const target = makeTarget(
    generation,
    `worker-${generation}`,
    (message) => handle.postMessage(message),
    () => handle.terminate(),
  );
  listen(handle, target);
  handle.addEventListener("messageerror", () => {
    workerEvents.push({ type: "messageerror", generation: target.generation });
    replace(target, "messageerror event");
  });
  handle.addEventListener("error", (event) => {
    // Handled here, so that it is not reported as the page's own uncaught error. Its text
    // differs per engine and never holds the panic message; only the event is relied on.
    event.preventDefault();
    workerEvents.push({ type: "error", generation: target.generation, message: String(event.message ?? "") });
    replace(target, "error event");
  });
  current = target;
  workerEvents.push({ type: "started", generation: target.generation });
  target.loaded = outcomeOf(target, "worker:load", {}, LOAD_TIMEOUT_MS).then((outcome) => {
    target.ready = outcome.outcome === "ok" && !target.dead;
    workerEvents.push({ type: "loaded", generation: target.generation, outcome: outcome.outcome });
    return outcome;
  });
  return target;
}

// Stops the current worker, if any, without replacing it.
function stopWorker() {
  if (current !== null) retire(current, "stopped", "stopped by the runner");
  current = null;
}

// Starts a new worker, replacing any current one, and returns its load outcome. `script`
// names the worker's file next to this page: worker.mjs unless a test names another, such as
// one that does not exist, to see a worker that fails to start.
async function startWorker(script = "worker.mjs") {
  if (!/^[a-z-]+\.mjs$/.test(script)) return { outcome: "bad_script", message: "not a file name" };
  stopWorker();
  return spawn(script).loaded;
}

// Runs `scenario` with `args` (an object) on the current worker, once it has loaded.
async function run(scenario, args = {}, timeoutMs = DEFAULT_TIMEOUT_MS) {
  if (current === null) return { outcome: "no_worker", message: "call startWorker() first" };
  const target = current;
  await target.loaded;
  return outcomeOf(target, scenario, args, timeoutMs);
}

// Registers sw.mjs as a module service worker, loads the bundle there and runs `pure`, then
// unregisters it. Returns the outcomes of the load, of `pure` and of the worker's
// violation list.
async function serviceWorkerProbe(timeoutMs = LOAD_TIMEOUT_MS) {
  const registration = await navigator.serviceWorker.register(new URL("sw.mjs", location.href), {
    type: "module",
    scope: "./",
  });
  try {
    const ready = await Promise.race([
      navigator.serviceWorker.ready,
      new Promise((_, reject) => setTimeout(() => reject(new CallTimeout("the service worker never became ready")), timeoutMs)),
    ]);
    const active = ready.active;
    const target = makeTarget(
      "service-worker",
      "service-worker",
      (message) => active.postMessage(message),
      () => {},
    );
    listen(navigator.serviceWorker, target);
    const load = await outcomeOf(target, "worker:load", {}, timeoutMs);
    const pure = await outcomeOf(target, "pure", {}, timeoutMs);
    const violations = await outcomeOf(target, "worker:violations", {}, timeoutMs);
    return { load, pure, violations };
  } finally {
    await registration.unregister();
  }
}

// Whether the page has the Origin Private File System.
function hasOpfs() {
  return typeof navigator.storage?.getDirectory === "function";
}

// The entries of the origin's OPFS root and the origin's IndexedDB databases, each with its
// record count. Counting reads no record, so no stored CryptoKey is read.
async function storage() {
  const entries = [];
  if (hasOpfs()) {
    const root = await navigator.storage.getDirectory();
    for await (const name of root.keys()) entries.push(name);
  }
  const databases = [];
  for (const { name } of await indexedDB.databases()) {
    databases.push({ name, records: await countRecords(name) });
  }
  return { entries: entries.sort(), databases };
}

function countRecords(name) {
  return new Promise((resolve, reject) => {
    const opening = indexedDB.open(name);
    opening.onerror = () => reject(opening.error);
    opening.onsuccess = () => {
      const database = opening.result;
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
}

// Stops the worker, so that its file locks and database connections are released, then
// removes every entry of the OPFS root and every IndexedDB database of the origin. Returns
// what was there, as storage() reports it.
async function clearStorage() {
  stopWorker();
  const found = await storage();
  if (hasOpfs()) {
    const root = await navigator.storage.getDirectory();
    for (const name of found.entries) {
      // A stopped worker's file locks may take a moment to be released: retry for about 5 s.
      for (let attempt = 1; ; attempt += 1) {
        try {
          await root.removeEntry(name, { recursive: true });
          break;
        } catch (error) {
          if (attempt === 25) throw error;
          await new Promise((resolve) => setTimeout(resolve, 200));
        }
      }
    }
  }
  for (const { name } of found.databases) {
    // A delete that a connection blocks, such as one of the stopped worker that is not closed
    // yet, waits for it to close. It fails only when it stays blocked for 30 s; a success or
    // an error before that ends the wait.
    await new Promise((resolve, reject) => {
      let blockedTimer;
      const deleting = indexedDB.deleteDatabase(name);
      deleting.onsuccess = () => {
        clearTimeout(blockedTimer);
        resolve();
      };
      deleting.onerror = () => {
        clearTimeout(blockedTimer);
        reject(deleting.error);
      };
      deleting.onblocked = () => {
        clearTimeout(blockedTimer);
        blockedTimer = setTimeout(() => reject(new Error(`deleting the database ${name} stays blocked`)), 30_000);
      };
    });
  }
  return found;
}

// Wraps a function so that it returns plain data and never throws to page.evaluate.
function safe(fn) {
  return async (...args) => {
    try {
      return await fn(...args);
    } catch (error) {
      return { error: `${error?.name ?? "Error"}: ${error?.message ?? error}` };
    }
  };
}

window.paykitHarness = Object.freeze({
  // The id of this load of the page.
  pageInstance: () => pageInstance,
  // Starts a new worker (stopping any current one) and resolves with its load outcome:
  // startWorker() for worker.mjs, startWorker(fileName) for another file next to the page.
  startWorker: safe(startWorker),
  // The current worker's generation, and its load outcome once loaded.
  currentWorker: safe(async () =>
    current === null ? null : { generation: current.generation, load: await current.loaded },
  ),
  // Runs a scenario on the current worker: run(name, argsObject, timeoutMs).
  run: safe(run),
  // Every message posted and received so far, with the type of each value.
  messages: () => structuredClone(messageLog),
  // The page's own Content Security Policy violations.
  violations: () => structuredClone(pageViolations),
  // The workers' events: started, error, messageerror, replaced, start failed, stopped,
  // loaded.
  workerEvents: () => structuredClone(workerEvents),
  // The service-worker probe.
  serviceWorkerProbe: safe(serviceWorkerProbe),
  // Whether the page has the Origin Private File System.
  hasOpfs,
  // Removes all that the origin's OPFS root and IndexedDB hold, and returns what was there.
  clearStorage: safe(clearStorage),
});
