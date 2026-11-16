// THROWAWAY (#411): browser end-to-end driver for the paykit-wasm facade.
// Runs two Paykit identities in one page against a local Pubky StaticTestnet
// (pubky-testnet 0.16.2 on localhost: homeserver 6286, pkarr relay 15411,
// HTTP relay 15412). Each step logs PASS/FAIL; the page title becomes
// "e2e-done" when finished so a headless driver can collect the result.
import init, { PaykitWasmClient, sleep_ms } from "../pkg/paykit_wasm.js";

// Well-known StaticTestnet homeserver key (deterministic testnet keypair).
const HOMESERVER = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";
const AUTH_RELAY_INBOX = "http://localhost:15412/inbox";

// The scenario never touches the DOM, so it runs on the page's main thread
// (index.html) and inside a dedicated module worker (worker.html).
let results = [];
let sink = () => {};
function log(line) {
  results.push(line);
  sink(line);
  console.log(line);
}
async function step(name, fn) {
  const started = performance.now();
  try {
    const detail = await fn();
    log(`PASS ${name} (${Math.round(performance.now() - started)} ms)${detail === undefined ? "" : ": " + detail}`);
    return true;
  } catch (e) {
    log(`FAIL ${name} (${Math.round(performance.now() - started)} ms): ${e && (e.stack || e.message) || e}`);
    return false;
  }
}
function assert(cond, msg) { if (!cond) throw new Error(msg); }

// IndexedDB-backed PaykitStateStore: compare-and-swap on a random revision
// inside one readwrite transaction with strict durability. THROWAWAY (#411):
// the SDK state blob (Noise and link secrets included) is stored as
// plaintext; production needs WebCrypto wrapping (see the spike report).
function idbStateStore(dbName) {
  const open = () => new Promise((resolve, reject) => {
    const req = indexedDB.open(dbName, 1);
    req.onupgradeneeded = () => req.result.createObjectStore("state");
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  const dbPromise = open();
  const stats = { loads: 0, saves: 0, conflicts: 0, lastBytes: 0 };
  return {
    stats,
    async load() {
      const db = await dbPromise;
      stats.loads++;
      return new Promise((resolve, reject) => {
        const req = db.transaction("state", "readonly").objectStore("state").get("sdk");
        req.onsuccess = () => resolve(req.result ?? null);
        req.onerror = () => reject(req.error);
      });
    },
    async saveIfRevision(expected, bytes) {
      const db = await dbPromise;
      return new Promise((resolve, reject) => {
        const tx = db.transaction("state", "readwrite", { durability: "strict" });
        const store = tx.objectStore("state");
        let committed = false;
        const get = store.get("sdk");
        get.onsuccess = () => {
          const current = get.result ? get.result.revision : null;
          if (current !== expected) { stats.conflicts++; return; }
          store.put({ revision: crypto.randomUUID(), bytes: bytes.slice() }, "sdk");
          committed = true;
        };
        tx.oncomplete = () => {
          if (committed) { stats.saves++; stats.lastBytes = bytes.length; }
          resolve(committed);
        };
        tx.onerror = () => reject(tx.error);
        tx.onabort = () => reject(tx.error);
      });
    },
  };
}

async function untilLinked(alice, bob) {
  const deadline = performance.now() + 20000;
  let a = "Linking", b = "Linking", rounds = 0;
  while (a !== "Linked" || b !== "Linked") {
    assert(performance.now() < deadline, `handshake timed out (alice=${a}, bob=${b})`);
    if (a !== "Linked") a = await alice.advanceLink(bob.publicKey);
    if (b !== "Linked") b = await bob.advanceLink(alice.publicKey);
    rounds++;
    await sleep_ms(50);
  }
  return `${rounds} rounds`;
}

async function run() {
  log(`INFO userAgent ${navigator.userAgent}`);
  log(`INFO origin ${location.origin}; context ${typeof window === "undefined" ? "DedicatedWorkerGlobalScope" : "Window"}`);
  await step("load wasm module", async () => { await init(); });

  let alice, bob;
  const bobStore = idbStateStore(`paykit-spike-${Date.now()}`);
  if (!await step("alice sign-up + SDK initialize (InMemoryStorage)", async () => {
    alice = await PaykitWasmClient.signUpOnTestnet("wasm-alice-app", HOMESERVER, AUTH_RELAY_INBOX, "memory");
    return alice.publicKey;
  })) return;
  if (!await step("bob sign-up + SDK initialize (IndexedDB-backed JS store)", async () => {
    bob = await PaykitWasmClient.signUpOnTestnet("wasm-bob-app", HOMESERVER, AUTH_RELAY_INBOX, "js", bobStore);
    return `${bob.publicKey}; idb loads=${bobStore.stats.loads} saves=${bobStore.stats.saves} blob=${bobStore.stats.lastBytes} B`;
  })) return;

  const publicDetails = { "btc-lightning-bolt11": "lnbc-wasm-alice", "btc-onchain": "bc1q-wasm-alice" };
  await step("alice sync_public_endpoints publishes 2 Payment Endpoints", async () => {
    alice.setPublicDetails(JSON.stringify(publicDetails));
    const r = JSON.parse(await alice.syncPublicEndpoints());
    assert(r.published === 2 && r.failed === 0, JSON.stringify(r));
    return JSON.stringify(r);
  });
  await step("bob reads alice's public Payment List (paykit-lib get_payment_list)", async () => {
    const list = JSON.parse(await bob.readPublicPaymentList(alice.publicKey, alice.appId));
    assert(JSON.stringify(list) === JSON.stringify(publicDetails), JSON.stringify(list));
    return JSON.stringify(list);
  });
  await step("bob resolve_public_contact_payment(alice)", async () => {
    const r = JSON.parse(await bob.resolvePublicContactPayment(alice.publicKey));
    assert(r.payloads.includes("lnbc-wasm-alice") && r.payloads.includes("bc1q-wasm-alice"), JSON.stringify(r));
    return JSON.stringify(r);
  });

  if (!await step("Encrypted Link Handshake (initiate/accept/advance to Linked)", async () => {
    const a = await alice.initiateLink(bob.publicKey);
    const b = await bob.acceptLink(alice.publicKey);
    return `initiate=${a} accept=${b}; ${await untilLinked(alice, bob)}`;
  })) return;

  await step("alice sends Private Payment List over the Encrypted Link", async () => {
    alice.setPrivateDetails(JSON.stringify({ "btc-lightning-bolt11": "ln-private-alice-1" }));
    const r = JSON.parse(await alice.sendPrivatePaymentList(bob.publicKey));
    assert(r.sent === 1 && r.failed === 0, JSON.stringify(r));
    return JSON.stringify(r);
  });
  await step("bob receive_private_messages + current_private_payment_lists", async () => {
    const n = await bob.receivePrivateMessages(alice.publicKey);
    assert(n > 0, `no stream items received`);
    const views = JSON.parse(await bob.currentPrivatePaymentLists(alice.publicKey));
    const view = views.find((v) => v.appId === alice.appId);
    assert(view && view.endpoints["btc-lightning-bolt11"] === "ln-private-alice-1", JSON.stringify(views));
    return `${n} stream items; ${JSON.stringify(views)}`;
  });

  // Diagnostic: does the live, path-filtered homeserver event stream that
  // private intake uses deliver alice's next private write to the browser?
  // pubky-homeserver 0.16.2 treats a path filter without a trailing "/" as an
  // exact file match, so both forms are tried.
  for (const [label, path] of [["no trailing slash (as master's intake sends)", "/pub/paykit/v0/private"], ["trailing slash", "/pub/paykit/v0/private/"]]) {
    await step(`diagnostic: live /events-stream path=${path} [${label}]`, async () => {
      // Times are ms since subscribe; alice writes at about +1000 ms.
      const pending = bob.debugLiveStream(alice.publicKey, path, 100, 4000);
      await sleep_ms(1000);
      alice.setPrivateDetails(JSON.stringify({ "btc-lightning-bolt11": `ln-private-alice-diag-${path.length}` }));
      const r = JSON.parse(await alice.sendPrivatePaymentList(bob.publicKey));
      const events = JSON.parse(await pending);
      const n = await bob.receivePrivateMessages(alice.publicKey);
      // Only events that arrive after alice's write (about +1000 ms) are live;
      // earlier ones are history replayed because no cursor was given.
      const live = events.filter((e) => Number(/^\+(\d+)ms/.exec(e)?.[1] ?? 0) >= 900);
      if (!path.endsWith("/")) {
        // Expected: the homeserver treats this as an exact file filter.
        assert(events.length === 0, `expected no events for a file filter, got ${events.length}`);
        return `bug reproduced: 0 events (history or live) for a filter without trailing slash; explicit receive=${n}`;
      }
      assert(live.length > 0, `no live event within 3 s of alice's write (history replayed: ${events.length}; send=${JSON.stringify(r)}, explicit receive=${n})`);
      return `${live.length} live event(s) (history replayed: ${events.length - live.length}); first live: ${live[0].slice(0, 330)}; explicit receive=${n}`;
    });
  }

  let intakeStarted = false;
  await step("bob start_private_intake (live event stream + spawned tasks + timers)", async () => {
    await bob.startPrivateIntake();
    intakeStarted = true;
    // Wait past the first jittered audit (at most 5 s after connect). The
    // next audit is 60 s later, so delivery within 30 s below can only come
    // from the live SSE hint.
    await sleep_ms(6000);
    return await bob.privateIntakeHealth();
  });
  if (intakeStarted) {
    // Private intake only buffers ciphertext (SSE hint -> fetch next expected
    // slot -> durable buffer). Decryption happens in the explicit
    // receive_buffered_private_messages call, which reads no new slots.
    await step("private intake (SSE hint) buffers alice's next list; bob drains the buffer", async () => {
      alice.setPrivateDetails(JSON.stringify({ "btc-lightning-bolt11": "ln-private-alice-2" }));
      const r = JSON.parse(await alice.sendPrivatePaymentList(bob.publicKey));
      assert(r.sent === 1, JSON.stringify(r));
      const started = performance.now();
      const deadline = started + 30000;
      let drained = [];
      for (;;) {
        const report = JSON.parse(await bob.receiveBufferedPrivateMessages(alice.publicKey));
        if (report.items > 0) drained.push(report);
        const views = JSON.parse(await bob.currentPrivatePaymentLists(alice.publicKey));
        const view = views.find((v) => v.appId === alice.appId);
        if (view && view.endpoints["btc-lightning-bolt11"] === "ln-private-alice-2") {
          return `delivered after ${Math.round(performance.now() - started)} ms; drains=${JSON.stringify(drained)}; health=${await bob.privateIntakeHealth()}`;
        }
        assert(performance.now() < deadline, `not delivered in 30 s; last=${JSON.stringify(report)} health=${await bob.privateIntakeHealth()}`);
        await sleep_ms(250);
      }
    });
    await step("bob stop_private_intake", async () => { await bob.stopPrivateIntake(); return await bob.privateIntakeHealth(); });
  }

  // carol keeps SDK state on the homeserver (PubkySharedStateStorage):
  // XChaCha20-Poly1305 state blob behind WebDAV LOCK/UNLOCK write locks and
  // the wasm lease clock.
  let carol;
  if (await step("carol sign-up + SDK initialize (PubkySharedStateStorage)", async () => {
    carol = await PaykitWasmClient.signUpOnTestnet("wasm-carol-app", HOMESERVER, AUTH_RELAY_INBOX, "shared");
    return carol.publicKey;
  })) {
    await step("carol <-> alice Encrypted Link + Private Payment List (shared state)", async () => {
      await alice.initiateLink(carol.publicKey);
      await carol.acceptLink(alice.publicKey);
      const rounds = await untilLinked(alice, carol);
      alice.setPrivateDetails(JSON.stringify({ "btc-lightning-bolt11": "ln-private-for-carol" }));
      const r = JSON.parse(await alice.sendPrivatePaymentList(carol.publicKey));
      assert(r.sent === 1, JSON.stringify(r));
      const n = await carol.receivePrivateMessages(alice.publicKey);
      const views = JSON.parse(await carol.currentPrivatePaymentLists(alice.publicKey));
      const view = views.find((v) => v.appId === alice.appId);
      assert(view && view.endpoints["btc-lightning-bolt11"] === "ln-private-for-carol", JSON.stringify(views));
      return `${rounds}; received ${n}; ${JSON.stringify(views)}`;
    });
    await step("fresh carol runtime reloads shared state from the homeserver", async () => {
      const before = await carol.backupFingerprint();
      const reopened = await carol.reopenSharedState();
      const after = await reopened.backupFingerprint();
      const peers = JSON.parse(await reopened.linkedPeers());
      const views = JSON.parse(await reopened.currentPrivatePaymentLists(alice.publicKey));
      assert(before === after, `fingerprint changed: ${before} -> ${after}`);
      assert(peers[alice.publicKey] === "Linked", JSON.stringify(peers));
      assert(views.some((v) => v.endpoints["btc-lightning-bolt11"] === "ln-private-for-carol"), JSON.stringify(views));
      reopened.free();
      return `fingerprint ${after} matches; peers=${JSON.stringify(peers)}`;
    });
  }

  await step("backup fingerprints", async () => `alice=${await alice.backupFingerprint()} bob=${await bob.backupFingerprint()}`);
  await step("IndexedDB store holds bob's committed state", async () => {
    const row = await bobStore.load();
    assert(row && row.bytes && row.bytes.length > 0, "no state row");
    return `revision=${row.revision} bytes=${row.bytes.length}; stats=${JSON.stringify(bobStore.stats)}`;
  });
  await step("alice sign_out (grant revocation via restore + signout)", async () => {
    const cap = await alice.signOut();
    assert(cap === "SignedOut", cap);
    return cap;
  });
}

/// Run the scenario; `onLine` receives every log line. Resolves to the
/// summary line.
export async function runE2e(onLine) {
  results = [];
  sink = onLine;
  try {
    await run();
  } catch (e) {
    log(`FAIL driver: ${e && (e.stack || e.message) || e}`);
  }
  const failed = results.filter((l) => l.startsWith("FAIL")).length;
  const passed = results.filter((l) => l.startsWith("PASS")).length;
  const summary = `SUMMARY passed=${passed} failed=${failed}`;
  log(summary);
  return summary;
}
