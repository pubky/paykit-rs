// THROWAWAY (#411): orchestrate the browser lifecycle experiments against
// paykit-wasm/www/lifecycle.html on a local Pubky testnet.
// Usage: node lifecycle.mjs <engine> <scenario> [baseUrl]
//   engine: chromium (Google Chrome) | firefox | webkit
//   scenario: multitab | reload | freeze | panic
import { chromium, firefox, webkit } from "playwright";

const [,, engine, scenario, base = "http://127.0.0.1:8410/www/lifecycle.html"] = process.argv;
const types = { chromium, firefox, webkit };
const opts = engine === "chromium" ? { executablePath: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" } : {};
const browser = await types[engine].launch({ headless: true, ...opts });
const T0 = Date.now();
const ts = () => `+${((Date.now() - T0) / 1000).toFixed(1)}s`;
const out = (label, value) => console.log(`${ts()} ${label}: ${typeof value === "string" ? value : JSON.stringify(value)}`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const HOMESERVER = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";
let failures = 0;
// debugPutTestFile resolves to JSON {ok, status, error}; a call wrapper adds its own ok.
const putOk = (r) => !!(r && r.ok && JSON.parse(r.value).ok);
const check = (cond, label) => { out(cond ? "CHECK ok" : "CHECK FAILED", label); if (!cond) failures++; };

async function lcPage(ctx, name) {
  const page = await ctx.newPage();
  page.on("console", (m) => { if ((m.type() === "error" && !m.text().startsWith("Failed to load resource")) || m.text().includes("panicked") || m.text().includes("became")) out(`[${name} console.${m.type()}]`, m.text().slice(0, 400)); });
  page.on("pageerror", (e) => out(`[${name} pageerror]`, e.message.slice(0, 300)));
  page.on("worker", (w) => w.on("console", (m) => out(`[${name} worker console.${m.type()}]`, m.text().slice(0, 400))));
  await page.goto(base);
  await page.waitForFunction(() => document.title === "lc-ready", null, { timeout: 30000 });
  return page;
}
const ev = (page, fn, arg) => page.evaluate(fn, arg);

// ---------------------------------------------------------------------------
async function multitab() {
  // b1. Reproduce: a second context restoring the same grant logs the first out.
  const ctx = await browser.newContext();
  const db = `lc-mt-${Date.now()}`;
  const A = await lcPage(ctx, "A");
  out("A signUp", await ev(A, (db) => lc.signUp("lc-mt-app", db), db));
  const a1 = await ev(A, () => lc.call("debugPutTestFile", "mt", "A-before"));
  out("A put before B restores the grant", a1);
  await ev(A, () => lc.client.setPublicDetails(JSON.stringify({ "btc-onchain": "bc1q-mt" })));
  const a1s = await ev(A, () => lc.call("syncPublicEndpoints"));
  out("A syncPublicEndpoints before", a1s);
  const B = await lcPage(ctx, "B");
  out("B open (import_session of the SAME exported grant)", await ev(B, (db) => lc.open(db, true).then(() => "ok", (e) => String(e)), db));
  const a2 = await ev(A, () => lc.call("debugPutTestFile", "mt", "A-after"));
  out("A put after B restored the grant", a2);
  await ev(A, () => lc.client.setPublicDetails(JSON.stringify({ "btc-onchain": "bc1q-mt-2" })));
  const a2s = await ev(A, () => lc.call("syncPublicEndpoints"));
  out("A syncPublicEndpoints after", a2s);
  const b1 = await ev(B, () => lc.call("debugPutTestFile", "mt", "B"));
  out("B put", b1);
  check(putOk(a1) && JSON.parse(a1s.value).failed === 0 && !putOk(a2) && JSON.parse(a2s.value).failed > 0 && putOk(b1), "restoring the same grant in tab B makes tab A's writes fail");
  const C = await lcPage(ctx, "C");
  out("C open (root key sign_in: a separate grant)", await ev(C, (db) => lc.open(db, false).then(() => "ok", (e) => String(e)), db));
  const b2 = await ev(B, () => lc.call("debugPutTestFile", "mt", "B-after-C"));
  const c1 = await ev(C, () => lc.call("debugPutTestFile", "mt", "C"));
  out("B put after C signed in separately", b2);
  out("C put", c1);
  check(putOk(b2) && putOk(c1), "a separate grant per tab does not invalidate other tabs (needs the root key in every tab)");
  await ctx.close();

  // b2. Mitigation: one SDK owner per origin (Web Locks) + BroadcastChannel.
  const ctx2 = await browser.newContext();
  const db2 = `lc-cluster-${Date.now()}`;
  const S = await lcPage(ctx2, "setup");
  out("setup signUp", await ev(S, (db) => lc.signUp("lc-cluster-app", db), db2));
  await S.close();
  const P1 = await lcPage(ctx2, "P1");
  out("P1 joinCluster", await ev(P1, (db) => lc.joinCluster(db), db2));
  const P2 = await lcPage(ctx2, "P2");
  out("P2 joinCluster", await ev(P2, (db) => lc.joinCluster(db), db2));
  out("P2 Web Lock view", await ev(P2, () => lc.webLockState()));
  const r1 = await ev(P2, () => lc.rpc("debugPutTestFile", "cl", "via-owner"));
  const r2 = await ev(P1, () => lc.rpc("debugPutTestFile", "cl", "owner-direct"));
  const r3 = await ev(P2, () => lc.rpc("setPublicDetails", JSON.stringify({ "btc-onchain": "bc1q-cluster" })));
  const r4 = await ev(P2, () => lc.rpc("syncPublicEndpoints"));
  const r5 = await ev(P2, (k) => lc.rpc("saveContact", k, "saved-through-owner"), HOMESERVER);
  const r6 = await ev(P1, () => lc.rpc("debugPutTestFile", "cl", "owner-direct-2"));
  for (const [k, v] of Object.entries({ r1, r2, r3, r4, r5, r6 })) out(`cluster call ${k}`, v);
  check([r1, r2, r6].every(putOk) && [r3, r4, r5].every((r) => r.ok) && r1.via === "owner" && r2.via === "self", "follower calls go through the owner; no tab loses its bearer");
  const closedAt = Date.now();
  await P1.close();
  const became = await ev(P2, () => lc.waitLeader(20000));
  const leaderAt = await ev(P2, () => lc.cluster.becameLeaderAt);
  out("P2 took over after P1 closed", { became, takeoverMs: leaderAt - closedAt });
  const r7 = await ev(P2, () => lc.rpc("contactKeys"));
  const r8 = await ev(P2, () => lc.rpc("debugPutTestFile", "cl", "after-takeover"));
  out("P2 contactKeys (from IndexedDB)", r7);
  out("P2 put after takeover", r8);
  const P3 = await lcPage(ctx2, "P3");
  out("P3 joinCluster", await ev(P3, (db) => lc.joinCluster(db), db2));
  const r9 = await ev(P3, () => lc.rpc("debugReadTestFile", "cl"));
  out("P3 read through new owner", r9);
  check(became && r7.ok && r7.value.includes(HOMESERVER) && putOk(r8) && r9.ok && r9.value === "after-takeover", "ownership moves to the next tab with state intact");
  await ctx2.close();
}

// ---------------------------------------------------------------------------
async function reload() {
  for (const variant of ["baseline", "pagehide", "weblock"]) {
    const ctx = await browser.newContext();
    const db = `lc-reload-${variant}-${Date.now()}`;
    const R = await lcPage(ctx, `R-${variant}`);
    out(`[${variant}] signUp`, await ev(R, (db) => lc.signUp("lc-reload-app", db), db));
    const started = variant === "weblock"
      ? await ev(R, () => lc.startIntakeWithWebLock())
      : await ev(R, () => lc.call("startPrivateIntake"));
    out(`[${variant}] intake started`, started);
    if (variant === "pagehide") out(`[${variant}] pagehide hook`, await ev(R, () => lc.installPagehideRelease()));
    out(`[${variant}] lease before reload`, await ev(R, () => lc.lease()));
    await sleep(5000);
    const reloadAt = Date.now();
    await R.reload();
    await R.waitForFunction(() => document.title === "lc-ready", null, { timeout: 30000 });
    out(`[${variant}] reloaded in ${Date.now() - reloadAt} ms; pagehide-at`, await ev(R, () => sessionStorage.getItem("pagehide-at")));
    await ev(R, (db) => lc.open(db, true), db);
    out(`[${variant}] lease after reload`, await ev(R, () => lc.lease()));
    if (variant === "weblock") {
      // A second tab must not steal intake while the reloaded owner holds the Web Lock.
      const r = await ev(R, () => lc.startIntakeWithWebLock());
      out(`[${variant}] restart after reload`, { ...r, sinceReloadMs: Date.now() - reloadAt });
      const T = await lcPage(ctx, "T");
      await ev(T, (db) => lc.open(db, false), db);
      const t = await ev(T, () => lc.startIntakeWithWebLock(3000));
      out(`[${variant}] second tab while owner holds the Web Lock`, t);
      check(r.ok && !t.ok, `[${variant}] reload restarts intake at once; a live owner is not displaced`);
    } else {
      const r = await ev(R, () => lc.startIntakeUntilOk(170000, 2000));
      out(`[${variant}] restart after reload`, { ...r, sinceReloadMs: Date.now() - reloadAt });
      check(r.ok, `[${variant}] intake restarts eventually`);
    }
    await ctx.close();
  }
}

// ---------------------------------------------------------------------------
async function freeze() {
  if (engine !== "chromium") throw new Error("freeze needs Chrome DevTools Protocol");
  const ctx = await browser.newContext();
  const db = `lc-freeze-${Date.now()}`;
  const A = await lcPage(ctx, "A");
  out("A signUp", await ev(A, (db) => lc.signUp("lc-freeze-app", db), db));
  const B = await lcPage(ctx, "B");
  out("B open (separate grant via sign_in, same IndexedDB)", await ev(B, (db) => lc.open(db, false), db));
  const cdp = await ctx.newCDPSession(A);
  const freezeFor = async (ms, during) => {
    const before = await ev(A, () => lc.clocks());
    await cdp.send("Page.setWebLifecycleState", { state: "frozen" });
    const frozenAt = Date.now();
    const duringResult = during ? await during(frozenAt) : null;
    const rest = ms - (Date.now() - frozenAt);
    if (rest > 0) await sleep(rest);
    await cdp.send("Page.setWebLifecycleState", { state: "active" });
    const after = await ev(A, () => lc.clocks());
    out("A clocks across the freeze", {
      frozenMs: Date.now() - frozenAt,
      leaseClockAdvancedMs: Math.round(after.leaseClockMs - before.leaseClockMs),
      performanceNowAdvancedMs: Math.round(after.performanceNowMs - before.performanceNowMs),
      wallAdvancedMs: after.wallMs - before.wallMs,
    });
    return duringResult;
  };
  // Is the page really frozen? A timer must not fire while frozen.
  await ev(A, () => { globalThis.ticks = 0; globalThis.tickTimer = setInterval(() => globalThis.ticks++, 100); });
  await freezeFor(3000);
  const ticks = await ev(A, () => { clearInterval(globalThis.tickTimer); return globalThis.ticks; });
  out("A 100 ms interval ticks during a 3 s freeze (plus a few before/after)", ticks);

  // d1. Paykit with_write_lock: holder frozen past its 60 s lock.
  await ev(A, () => lc.start("hold", "debugHoldWriteLock", "lk1", 5000, "A-value"));
  await sleep(1000);
  const bHold = await freezeFor(75000, async (frozenAt) => {
    await sleep(65000 - (Date.now() - frozenAt));
    return ev(B, () => lc.call("debugHoldWriteLock", "lk1", 0, "B-value"));
  });
  out("d1 B writes under lock while A is frozen (65 s)", bHold);
  const aHold = await ev(A, () => lc.result("hold"));
  out("d1 A with_write_lock result after unfreeze", aHold);
  const lk1 = await ev(B, () => lc.call("debugReadTestFile", "lk1"));
  out("d1 lk1 content", lk1);
  const aTimeline = aHold.ok ? JSON.parse(aHold.value) : null;
  check(bHold.ok && aTimeline && !aTimeline.result.ok && !aTimeline.put && lk1.value === "B-value", "d1 stale Paykit lock holder does not write after its lease expired; the other grant's write stands");

  // d2. Raw pubky lock (no Paykit lease clock), with and without a competitor.
  await ev(A, () => lc.start("raw", "debugRawLockedPut", "lk2", 60, 5000, "A-stale"));
  await sleep(1000);
  const bRaw = await freezeFor(75000, async (frozenAt) => {
    await sleep(65000 - (Date.now() - frozenAt));
    return ev(B, () => lc.call("debugHoldWriteLock", "lk2", 0, "B2-value"));
  });
  out("d2 B writes lk2 under lock while A is frozen", bRaw);
  const aRaw = await ev(A, () => lc.result("raw"));
  out("d2 A raw put_locked with its expired token", aRaw);
  out("d2 lk2 content", await ev(B, () => lc.call("debugReadTestFile", "lk2")));
  await ev(A, () => lc.start("raw2", "debugRawLockedPut", "lk3", 60, 5000, "A-stale-alone"));
  await sleep(1000);
  await freezeFor(70000);
  const aRaw2 = await ev(A, () => lc.result("raw2"));
  out("d2b A raw put_locked with expired token, no competitor", aRaw2);
  out("d2b lk3 content", await ev(B, () => lc.call("debugReadTestFile", "lk3")));

  // d3. Private intake: owner frozen past the 2-minute supervisor lease.
  const aIntake = await ev(A, () => lc.call("startPrivateIntake"));
  out("d3 A startPrivateIntake", aIntake);
  out("d3 lease (A)", await ev(A, () => lc.lease()));
  out("d3 B start while A runs", await ev(B, () => lc.call("startPrivateIntake")));
  await sleep(2000);
  const bIntake = await freezeFor(150000, async () => ev(B, () => lc.startIntakeUntilOk(145000, 5000)));
  out("d3 B start attempts while A is frozen", bIntake);
  out("d3 lease after B started", await ev(B, () => lc.lease()));
  let finished = null;
  const thawAt = Date.now();
  for (let i = 0; i < 25; i++) {
    finished = await ev(A, () => lc.client.privateIntakeFinished());
    if (finished) break;
    await sleep(2000);
  }
  out("d3 A supervisor finished on its own after thaw", { finished, afterMs: Date.now() - thawAt, health: await ev(A, () => lc.client.privateIntakeHealth()) });
  out("d3 lease at end", await ev(B, () => lc.lease()));
  check(bIntake.ok && finished === true, "d3 frozen intake owner loses the lease; another runtime takes over; the thawed owner stops itself");
  await ctx.close();
}

// ---------------------------------------------------------------------------
async function panic() {
  const ctx = await browser.newContext();
  const db = `lc-panic-${Date.now()}`;
  const P = await lcPage(ctx, "P");
  out("P signUp", await ev(P, (db) => lc.signUp("lc-panic-app", db), db));
  out("P saveContact", await ev(P, (k) => lc.call("saveContact", k, "before-panic"), HOMESERVER));
  const identity = await ev(P, () => localStorage.getItem("paykit-lifecycle-identity"));
  const w = (op, args, t) => ev(P, ([op, args, t]) => lc.workerCall(op, args, t), [op, args, t]);
  const results = {};
  for (const kind of ["async", "task", "transaction"]) {
    await ev(P, () => lc.startWorker());
    results[`${kind}.open`] = await w("open", [identity, db]);
    results[`${kind}.contactKeysBefore`] = await w("call", ["contactKeys"]);
    results[`${kind}.panic`] = await w("panic", [kind], 5000);
    results[`${kind}.contactKeysAfter`] = await w("call", ["contactKeys"], 5000);
    results[`${kind}.putAfter`] = await w("call", ["debugPutTestFile", "pn", `after-${kind}-panic`], 5000);
    results[`${kind}.staticAfter`] = await w("static", ["debugClocks"], 5000);
    if (kind === "transaction") {
      // Same worker, same wasm instance, a NEW SDK runtime (new JsBlobStorage mutex).
      results[`${kind}.reopenSameInstance`] = await w("open", [identity, db], 20000);
      results[`${kind}.contactKeysNewRuntimeSameInstance`] = await w("call", ["contactKeys"], 5000);
    }
    results[`${kind}.workerErrors`] = await ev(P, () => lc.workerErrors.splice(0));
    for (const [k, v] of Object.entries(results)) if (k.startsWith(kind)) out(`worker ${k}`, v);
    // Recovery: terminate, new worker, reopen identity and state from IndexedDB.
    const t = Date.now();
    await ev(P, () => lc.startWorker());
    const reopen = await w("open", [identity, db], 20000);
    const keys = await w("call", ["contactKeys"]);
    const put = await w("call", ["debugPutTestFile", "pn", `recovered-${kind}`]);
    out(`recovery after ${kind} panic`, { reopen, keys, put, recoveryMs: Date.now() - t });
    const panicked = results[`${kind}.panic`];
    if (kind !== "task") check(!panicked.ok && /no reply/.test(panicked.error), `${kind}: the panicking call's promise never settles`);
    if (kind === "transaction") {
      check(!results[`${kind}.contactKeysAfter`].ok && /no reply/.test(results[`${kind}.contactKeysAfter`].error), "transaction: every later storage call on that runtime hangs (mutex guard never dropped)");
      check(results[`${kind}.contactKeysNewRuntimeSameInstance`].ok, "transaction: a new runtime in the same trapped instance works again (only the held state is poisoned)");
    } else {
      check(results[`${kind}.contactKeysAfter`].ok && putOk(results[`${kind}.putAfter`]), `${kind}: OBSERVED the trapped instance keeps answering unrelated calls`);
    }
    check(reopen.ok && keys.ok && keys.value.includes(HOMESERVER) && putOk(put), `${kind}: new worker restores a working SDK from IndexedDB`);
  }
  await ev(P, () => lc.terminateWorker());
  await ctx.close();
}

try {
  out("browser", `${engine} ${browser.version()}`);
  await ({ multitab, reload, freeze, panic })[scenario]();
} catch (e) {
  failures++;
  out("SCENARIO ERROR", e.stack || String(e));
} finally {
  out("DONE", { scenario, engine, failures });
  await browser.close();
  process.exit(failures ? 1 : 0);
}
