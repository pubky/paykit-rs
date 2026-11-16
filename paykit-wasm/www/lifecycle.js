// THROWAWAY (#411): page-side helpers for the browser lifecycle experiments
// (multi-tab, reload during private intake, frozen tabs, panic recovery).
// A Node/Playwright orchestrator (scratchpad pw/lifecycle.mjs) opens several
// pages of lifecycle.html in one browser context and calls `globalThis.lc.*`
// through page.evaluate; it also freezes pages and reloads them over CDP.
//
// Security: this file stores the Pubky root secret and the grant session
// secret in localStorage so other tabs and reloads can reopen the identity.
// That is a test shortcut only; see the module docs of src/web/lifecycle.rs.
import init, { PaykitWasmClient, sleep_ms } from "../pkg/paykit_wasm.js";
import { idbStateStore, errorText } from "./e2e.js";

const HOMESERVER = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";
const AUTH_RELAY_INBOX = "http://localhost:15412/inbox";
const IDENTITY_KEY = "paykit-lifecycle-identity";

const ready = init();
const lc = {
  client: null,
  dbName: null,
  role: "none",
  events: [],
  async ready() { await ready; return navigator.userAgent; },

  note(text) { const line = `${new Date().toISOString()} ${text}`; lc.events.push(line); console.log(line); },

  identity() { return JSON.parse(localStorage.getItem(IDENTITY_KEY)); },

  /// Sign up a fresh identity over IndexedDB database `dbName` and publish
  /// its secrets to localStorage for the other contexts (test-only).
  async signUp(appId, dbName) {
    await ready;
    lc.dbName = dbName;
    lc.client = await PaykitWasmClient.signUpOnTestnet(appId, HOMESERVER, AUTH_RELAY_INBOX, "js", idbStateStore(dbName));
    localStorage.setItem(IDENTITY_KEY, lc.client.exportIdentityForTest());
    return lc.client.publicKey;
  },

  /// Reopen the stored identity over `dbName`. `restoreGrant` restores the
  /// exported grant (rotates its bearer); otherwise the root key signs in to
  /// a separate grant.
  async open(dbName, restoreGrant) {
    await ready;
    const id = lc.identity();
    lc.dbName = dbName;
    lc.client = await PaykitWasmClient.openOnTestnet(id.appId, id.secretKeyHex, restoreGrant ? id.sessionSecret : undefined, "js", idbStateStore(dbName));
    return lc.client.publicKey;
  },

  /// Call a client method; never throws, returns {ok, value | error, ms}.
  async call(method, ...args) {
    const started = performance.now();
    try {
      const value = await lc.client[method](...args);
      return { ok: true, value, ms: Math.round(performance.now() - started) };
    } catch (e) {
      return { ok: false, error: errorText(e).split("\n")[0], ms: Math.round(performance.now() - started) };
    }
  },

  /// Start a call without awaiting it; `lc.pending[tag]` resolves to its result.
  pending: {},
  start(tag, method, ...args) { lc.pending[tag] = lc.call(method, ...args); return tag; },
  async result(tag) { return lc.pending[tag]; },

  async lease(dbName = lc.dbName) { return JSON.parse(await PaykitWasmClient.debugIntakeLease(idbStateStore(dbName))); },
  clocks() { return { ...JSON.parse(PaykitWasmClient.debugClocks()), performanceNowMs: performance.now() }; },

  /// Retry start_private_intake every `intervalMs` until it succeeds or
  /// `timeoutMs` passes.
  async startIntakeUntilOk(timeoutMs, intervalMs = 2000) {
    const started = performance.now();
    let attempts = 0, lastError = null;
    while (performance.now() - started < timeoutMs) {
      attempts++;
      const r = await lc.call("startPrivateIntake");
      if (r.ok) return { ok: true, attempts, elapsedMs: Math.round(performance.now() - started), lastError };
      lastError = r.error;
      await sleep_ms(intervalMs);
    }
    return { ok: false, attempts, elapsedMs: Math.round(performance.now() - started), lastError };
  },

  // -------------------------------------------------------------------------
  // Mitigation 1 (reload): best-effort release on pagehide.
  // stopPrivateIntake sends the stop signal; the topology task then releases
  // the lease with a storage transaction (IndexedDB get, wasm, CAS put). The
  // browser does not wait for any of that when the document goes away.
  // -------------------------------------------------------------------------
  installPagehideRelease() {
    addEventListener("pagehide", () => {
      sessionStorage.setItem("pagehide-at", String(Date.now()));
      lc.client?.stopPrivateIntake();
    });
    return true;
  },

  // -------------------------------------------------------------------------
  // Mitigation 2 (reload, tabs): one intake owner per origin via Web Locks.
  // The browser releases a Web Lock as soon as its holder's document or
  // worker is gone (reload, close, crash), unlike the 2-minute storage lease.
  // An owner that gets the Web Lock knows no live same-origin context runs
  // intake, so a stored lease left behind is stale and may be released.
  // THROWAWAY (#411): only sound for device-local storage (IndexedDB) where
  // every possible intake owner is a same-origin context in this browser.
  // -------------------------------------------------------------------------
  async startIntakeWithWebLock(timeoutMs = 10000) {
    const name = `paykit-intake:${lc.client.publicKey}`;
    const started = performance.now();
    return new Promise((resolve) => {
      navigator.locks.request(name, async () => {
        const lockAt = Math.round(performance.now() - started);
        let r = await lc.call("startPrivateIntake");
        let forced = null;
        if (!r.ok && r.error.includes("already running")) {
          forced = JSON.parse(await PaykitWasmClient.forceReleaseIntakeLease(idbStateStore(lc.dbName)));
          r = await lc.call("startPrivateIntake");
        }
        resolve({ ok: r.ok, error: r.error, lockAt, forcedRelease: forced, elapsedMs: Math.round(performance.now() - started) });
        // Hold the Web Lock for the life of this document.
        await new Promise(() => {});
      }).catch((e) => resolve({ ok: false, error: errorText(e) }));
      setTimeout(() => resolve({ ok: false, error: `Web Lock ${name} not granted within ${timeoutMs} ms` }), timeoutMs);
    });
  },
  async webLockState() {
    const q = await navigator.locks.query();
    return { held: q.held.map((l) => l.name), pending: q.pending.map((l) => l.name) };
  },

  // -------------------------------------------------------------------------
  // Multi-tab mitigation: one SDK owner per origin and identity.
  // The tab holding Web Lock `paykit-sdk-owner:<key>` restores the grant and
  // runs the only SDK instance; the other tabs send calls to it over a
  // BroadcastChannel. When the owner's document goes away the browser hands
  // the lock to the next waiting tab, which restores the grant and reopens
  // the SDK from IndexedDB.
  // -------------------------------------------------------------------------
  cluster: null,
  async joinCluster(dbName) {
    await ready;
    const id = lc.identity();
    const chan = new BroadcastChannel(`paykit-sdk:${id.publicKey}`);
    const waiters = new Map();
    let leaderReady;
    const leaderPromise = new Promise((r) => { leaderReady = r; });
    lc.cluster = { chan, waiters, leaderPromise, becameLeaderAt: null, served: 0 };
    lc.role = "follower";
    chan.onmessage = async (ev) => {
      const m = ev.data;
      if (m.type === "res" && waiters.has(m.id)) { waiters.get(m.id)(m); waiters.delete(m.id); return; }
      if (m.type === "req" && lc.role === "leader") {
        const r = await lc.call(m.method, ...m.args);
        lc.cluster.served++;
        chan.postMessage({ type: "res", id: m.id, ...r });
      }
    };
    const joinedAt = performance.now();
    const lockName = `paykit-sdk-owner:${id.publicKey}`;
    const ownerBody = async () => {
      const t = performance.now();
      await lc.open(dbName, true);
      lc.role = "leader";
      lc.cluster.becameLeaderAt = Date.now();
      lc.note(`became SDK owner (open took ${Math.round(performance.now() - t)} ms, ${Math.round(performance.now() - joinedAt)} ms after joining)`);
      leaderReady(true);
      // Hold the lock for the life of this document.
      await new Promise(() => {});
    };
    // Take ownership at once if nobody holds it; otherwise queue for it.
    const owner = await new Promise((resolve) => {
      navigator.locks.request(lockName, { ifAvailable: true }, (lock) => {
        resolve(!!lock);
        return lock ? ownerBody() : undefined;
      });
    });
    if (owner) await leaderPromise;
    else navigator.locks.request(lockName, ownerBody);
    return lc.role;
  },
  async waitLeader(timeoutMs) {
    return Promise.race([lc.cluster.leaderPromise.then(() => true), sleep_ms(timeoutMs).then(() => false)]);
  },
  /// Call through the SDK owner (directly when this tab is the owner).
  async rpc(method, ...args) {
    if (lc.role === "leader") return { via: "self", ...(await lc.call(method, ...args)) };
    const id = crypto.randomUUID();
    const reply = new Promise((resolve) => lc.cluster.waiters.set(id, resolve));
    lc.cluster.chan.postMessage({ type: "req", id, method, args });
    const timeout = sleep_ms(15000).then(() => ({ ok: false, error: "no SDK owner answered within 15 s" }));
    return { via: "owner", ...(await Promise.race([reply, timeout])) };
  },

  // -------------------------------------------------------------------------
  // Panic recovery: an SDK hosted in a dedicated worker (panic-worker.js).
  // -------------------------------------------------------------------------
  worker: null,
  workerSeq: 0,
  workerWaiters: new Map(),
  workerErrors: [],
  startWorker() {
    lc.worker?.terminate();
    lc.worker = new Worker(new URL("./panic-worker.js", import.meta.url), { type: "module" });
    lc.worker.onmessage = (ev) => { const w = lc.workerWaiters.get(ev.data.id); if (w) { lc.workerWaiters.delete(ev.data.id); w(ev.data); } };
    lc.worker.onerror = (ev) => { lc.workerErrors.push(ev.message); ev.preventDefault?.(); };
    return true;
  },
  terminateWorker() { lc.worker?.terminate(); lc.worker = null; return true; },
  async workerCall(op, args = [], timeoutMs = 10000) {
    const id = ++lc.workerSeq;
    const reply = new Promise((r) => lc.workerWaiters.set(id, r));
    lc.worker.postMessage({ id, op, args });
    return Promise.race([reply, sleep_ms(timeoutMs).then(() => ({ ok: false, error: `no reply within ${timeoutMs} ms` }))]);
  },
};
globalThis.lc = lc;
ready.then(() => { document.title = "lc-ready"; });
