// THROWAWAY (#411): browser end-to-end driver for the paykit-wasm facade.
// Runs two Paykit identities in one page against a local Pubky StaticTestnet
// (pubky-testnet 0.16.2 on localhost: homeserver 6286, pkarr relay 15411,
// HTTP relay 15412). Each step logs PASS/FAIL; the page title becomes
// "e2e-done" when finished so a headless driver can collect the result.
//
// Round 2 adds Payment Requests, Receipts, Allowances and accounting, backup
// restore, concurrent IndexedDB writers, a WebCrypto-wrapped store, key
// rotation and request-timeout prototypes. The timeout steps need a TCP
// blackhole: run `python3 -I www/blackhole.py 15490` and open the page with
// `?bh=15490` (worker.html forwards the query to the worker); without it
// those steps are skipped.
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

// ---------------------------------------------------------------------------
// IndexedDB state stores
// ---------------------------------------------------------------------------

function openIdb(dbName) {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(dbName, 1);
    req.onupgradeneeded = () => { req.result.createObjectStore("state"); req.result.createObjectStore("keys"); };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}
function idbGet(db, storeName, key) {
  return new Promise((resolve, reject) => {
    const req = db.transaction(storeName, "readonly").objectStore(storeName).get(key);
    req.onsuccess = () => resolve(req.result ?? null);
    req.onerror = () => reject(req.error);
  });
}
function idbPut(db, storeName, key, value) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(storeName, "readwrite", { durability: "strict" });
    tx.objectStore(storeName).put(value, key);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}
// Compare-and-swap of the "sdk" row inside one readwrite transaction with
// strict durability. `row` is fully prepared before the transaction opens,
// because an IndexedDB transaction auto-commits across any non-IDB await
// (such as WebCrypto).
function idbCompareAndPut(db, expected, row) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction("state", "readwrite", { durability: "strict" });
    const store = tx.objectStore("state");
    let committed = false;
    const get = store.get("sdk");
    get.onsuccess = () => {
      const current = get.result ? get.result.revision : null;
      if (current !== expected) return;
      store.put(row, "sdk");
      committed = true;
    };
    tx.oncomplete = () => resolve(committed);
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}

// Test hook: the next `n` loads of database `dbName` (across every store
// object over it) wait until all `n` have started, so concurrent SDK
// transactions read the same revision and race on the compare-and-swap.
const loadBarriers = new Map();
function armLoadBarrier(dbName, n) { loadBarriers.set(dbName, { n, waiting: [] }); }
async function passLoadBarrier(dbName) {
  const barrier = loadBarriers.get(dbName);
  if (!barrier) return;
  await new Promise((resolve) => {
    barrier.waiting.push(resolve);
    if (barrier.waiting.length >= barrier.n) {
      loadBarriers.delete(dbName);
      barrier.waiting.forEach((r) => r());
    }
  });
}

// IndexedDB-backed PaykitStateStore with the SDK state blob in plaintext.
// THROWAWAY (#411): the blob holds Noise static and link secrets and
// Receipt keys; see encryptedIdbStateStore for the WebCrypto-wrapped variant.
function idbStateStore(dbName) {
  const dbPromise = openIdb(dbName);
  const stats = { loads: 0, saves: 0, conflicts: 0, lastBytes: 0 };
  return {
    dbName,
    stats,
    async rawRow() { return idbGet(await dbPromise, "state", "sdk"); },
    async load() {
      const db = await dbPromise;
      stats.loads++;
      await passLoadBarrier(dbName);
      return idbGet(db, "state", "sdk");
    },
    async saveIfRevision(expected, bytes) {
      const db = await dbPromise;
      const committed = await idbCompareAndPut(db, expected, { revision: crypto.randomUUID(), bytes: bytes.slice() });
      if (committed) { stats.saves++; stats.lastBytes = bytes.length; } else stats.conflicts++;
      return committed;
    },
  };
}

// THROWAWAY (#411): WebCrypto-wrapped IndexedDB PaykitStateStore.
// - Key: AES-GCM-256 from generateKey(extractable=false), stored as a
//   CryptoKey in the same database (structured clone). Script cannot export
//   the raw key bytes; it can still ask the browser to encrypt/decrypt.
// - Each write: fresh random 96-bit IV, new random revision, AAD =
//   "paykit-state-v1|<dbName>|<revision>", so a ciphertext only decrypts
//   under the revision (and database) it was written for.
// - Row: {revision, iv, ct}. Plaintext never reaches IndexedDB.
// What it does NOT protect against: script running in this origin (XSS,
// a malicious dependency) can call decrypt or simply drive the SDK; the
// CryptoKey is stored in the same browser profile as the ciphertext, so
// profile/disk theft gets both; rollback by replaying an older complete row
// {revision, iv, ct} still decrypts (no monotonic counter outside the store).
function encryptedIdbStateStore(dbName) {
  const dbPromise = openIdb(dbName);
  // Get-or-create must be atomic across tabs/store objects: a naive
  // "get, generate if missing, put" lets two openers each install their own
  // key, and whichever loses can no longer decrypt (seen in this spike). The
  // candidate key is generated before the transaction (WebCrypto awaits
  // would auto-commit it); the transaction keeps an existing key.
  const keyPromise = (async () => {
    const db = await dbPromise;
    const candidate = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
    return new Promise((resolve, reject) => {
      const tx = db.transaction("keys", "readwrite", { durability: "strict" });
      const keys = tx.objectStore("keys");
      let chosen = candidate;
      const get = keys.get("state-key");
      get.onsuccess = () => {
        if (get.result) chosen = get.result;
        else keys.add(candidate, "state-key");
      };
      tx.oncomplete = () => resolve(chosen);
      tx.onerror = () => reject(tx.error);
      tx.onabort = () => reject(tx.error);
    });
  })();
  const aad = (revision) => new TextEncoder().encode(`paykit-state-v1|${dbName}|${revision}`);
  const stats = { loads: 0, saves: 0, conflicts: 0, lastBytes: 0, decryptFailures: 0 };
  return {
    dbName,
    stats,
    async key() { return keyPromise; },
    async rawRow() { return idbGet(await dbPromise, "state", "sdk"); },
    async putRawRow(row) { return idbPut(await dbPromise, "state", "sdk", row); },
    async load() {
      const db = await dbPromise;
      stats.loads++;
      await passLoadBarrier(dbName);
      const row = await idbGet(db, "state", "sdk");
      if (!row) return null;
      let plain;
      try {
        plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv: row.iv, additionalData: aad(row.revision) }, await keyPromise, row.ct);
      } catch (e) {
        stats.decryptFailures++;
        throw new Error(`state blob failed authentication (${e.name})`);
      }
      return { revision: row.revision, bytes: new Uint8Array(plain) };
    },
    async saveIfRevision(expected, bytes) {
      const db = await dbPromise;
      const revision = crypto.randomUUID();
      const iv = crypto.getRandomValues(new Uint8Array(12));
      const ct = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: aad(revision) }, await keyPromise, bytes));
      const committed = await idbCompareAndPut(db, expected, { revision, iv, ct });
      if (committed) { stats.saves++; stats.lastBytes = bytes.length; } else stats.conflicts++;
      return committed;
    },
  };
}

function bytesContainAscii(bytes, text) {
  const needle = new TextEncoder().encode(text);
  outer: for (let i = 0; i + needle.length <= bytes.length; i++) {
    for (let j = 0; j < needle.length; j++) if (bytes[i + j] !== needle[j]) continue outer;
    return true;
  }
  return false;
}

// ---------------------------------------------------------------------------
// THROWAWAY (#411): fetch shim for the request-timeout prototype.
// reqwest's wasm backend calls the global `fetch` at request time, so this
// wrapper sees every Pubky/Paykit HTTP request. It is inert unless a test
// turns one of its modes on:
// - blackholeTo: rewrite requests for the testnet homeserver (port 6286) to a
//   local TCP blackhole that accepts and never answers (a hung homeserver).
// - headersTimeoutMs: abort the fetch (AbortSignal) when response headers do
//   not arrive in time. Cleared once headers arrive, so long-lived event
//   streams are not cut; a body that stalls after the headers is NOT bounded.
// ---------------------------------------------------------------------------
const fetchShim = { blackholeTo: null, headersTimeoutMs: null, rewrites: 0, headerTimeouts: 0, lastRewritten: null };
const realFetch = globalThis.fetch.bind(globalThis);
globalThis.fetch = (input, init) => {
  if (!fetchShim.blackholeTo && !fetchShim.headersTimeoutMs) return realFetch(input, init);
  let req = new Request(input, init);
  if (fetchShim.blackholeTo && /^https?:\/\/(localhost|127\.0\.0\.1):6286\//.test(req.url)) {
    const u = new URL(req.url);
    req = new Request(`http://127.0.0.1:${fetchShim.blackholeTo}${u.pathname}${u.search}`, req);
    fetchShim.rewrites++;
    fetchShim.lastRewritten = `${req.method} ${u.pathname}`;
  }
  if (!fetchShim.headersTimeoutMs) return realFetch(req);
  const ctrl = new AbortController();
  const timer = setTimeout(() => {
    fetchShim.headerTimeouts++;
    ctrl.abort(new DOMException("response headers timeout", "TimeoutError"));
  }, fetchShim.headersTimeoutMs);
  return realFetch(new Request(req, { signal: AbortSignal.any([req.signal, ctrl.signal]) })).finally(() => clearTimeout(timer));
};

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

  // Round 2 flows. Each section mirrors a native scenario in
  // paykit-sdk/tests/testnet_e2e. They return the (possibly replaced) bob.
  bob = await paymentRequestFlows(alice, bob);
  await receiptFlows(alice, bob);
  const allowanceId = await allowanceFlows(alice, bob);
  bob = await backupRestoreFlow(alice, bob, bobStore, allowanceId);
  await concurrentWriterFlows(alice, carol);
  await keyRotationFlow(alice, bob);
  await timeoutFlows(alice, carol);

  await step("alice sign_out (grant revocation via restore + signout)", async () => {
    const cap = await alice.signOut();
    assert(cap === "SignedOut", cap);
    return cap;
  });
}

// ---------------------------------------------------------------------------
// Round 2 flows
// ---------------------------------------------------------------------------

const ENDPOINT = "btc-lightning-bolt11";
const isoSeconds = (ms) => new Date(Math.floor(ms / 1000) * 1000).toISOString().replace(".000Z", "Z");

// process_outbound_private_messages on `from`, then receive_private_messages
// on `to` (the native harness's `deliver`).
async function deliver(from, to) {
  const sent = JSON.parse(await from.deliverTo(to.publicKey));
  assert(sent.sent > 0 && sent.failed === 0, `deliver send ${JSON.stringify(sent)}`);
  const got = JSON.parse(await to.receiveFrom(from.publicKey));
  assert(got.items > 0 && got.conflicts === 0, `deliver receive ${JSON.stringify(got)}`);
  return `sent=${sent.sent} received=${got.items}`;
}
async function request(client, peer, id) {
  const all = JSON.parse(await client.paymentRequestsWith(peer.publicKey));
  const record = all.find((r) => r.payment_request_id === id);
  assert(record, `no Payment Request ${id} on ${client.publicKey.slice(0, 8)}`);
  return record;
}
async function expectError(promise, marker) {
  try {
    await promise;
  } catch (e) {
    const text = String(e && (e.message || e));
    assert(text.includes(marker), `expected ${marker}, got: ${text}`);
    return text;
  }
  throw new Error(`expected ${marker}, but the call succeeded`);
}

// a. Payment Requests: bob is the payee, alice the payer.
async function paymentRequestFlows(alice, bob) {
  let firstId;
  await step("PR: bob proposes, alice claims + accepts, bob sees Accepted", async () => {
    const proposed = JSON.parse(await bob.proposePaymentRequest(alice.publicKey, "0.001", "btc", "wasm-invoice-1", ENDPOINT, undefined));
    assert(proposed.state === "Proposed", proposed.state);
    firstId = proposed.payment_request_id;
    const d1 = await deliver(bob, alice);
    const received = await request(alice, bob, firstId);
    assert(received.state === "Proposed" && received.local_role === "Payer", `${received.state} ${received.local_role}`);
    const accepted = JSON.parse(await alice.claimAndAcceptPaymentRequest(bob.publicKey, firstId));
    assert(accepted.state === "Accepted", accepted.state);
    const d2 = await deliver(alice, bob);
    const seen = await request(bob, alice, firstId);
    assert(seen.state === "Accepted", seen.state);
    return `id=${firstId}; ${d1}; ${d2}; payee state=${seen.state}`;
  });
  await step("PR: second request rejected by alice, bob sees Rejected", async () => {
    const id = JSON.parse(await bob.proposePaymentRequest(alice.publicKey, "0.002", "btc", "wasm-invoice-2", ENDPOINT, undefined)).payment_request_id;
    await deliver(bob, alice);
    const rejected = JSON.parse(await alice.rejectPaymentRequest(bob.publicKey, id, "not today"));
    assert(rejected.state === "Rejected", rejected.state);
    await deliver(alice, bob);
    const seen = await request(bob, alice, id);
    assert(seen.state === "Rejected", seen.state);
    return `payer=${rejected.state} payee=${seen.state}`;
  });
  await step("PR: third request cancelled by bob, alice sees Canceled", async () => {
    const id = JSON.parse(await bob.proposePaymentRequest(alice.publicKey, "0.003", "btc", "wasm-invoice-3", ENDPOINT, undefined)).payment_request_id;
    await deliver(bob, alice);
    const canceled = JSON.parse(await bob.cancelPaymentRequest(alice.publicKey, id, "issued in error"));
    assert(canceled.state === "Canceled", canceled.state);
    await deliver(bob, alice);
    const seen = await request(alice, bob, id);
    assert(seen.state === "Canceled", seen.state);
    return `payee=${canceled.state} payer=${seen.state}`;
  });
  await step("PR: alice submits a Payment Proof for the accepted request", async () => {
    const proof = JSON.stringify({ test_evidence: "wasm-preimage-0001" });
    const submitted = JSON.parse(await alice.submitPaymentProof(bob.publicKey, firstId, bob.appId, ENDPOINT, undefined, undefined, undefined, undefined, proof));
    await deliver(alice, bob);
    const seen = await request(bob, alice, firstId);
    assert(seen.payment_proofs.length === 1 && seen.payment_proofs[0].event_id === submitted.payment_proofs[0].event_id, JSON.stringify(seen.payment_proofs));
    assert(seen.payment_proofs[0].proof.test_evidence === "wasm-preimage-0001", "proof object mismatch");
    return `payer=${submitted.state} payee=${seen.state}; proof event ${seen.payment_proofs[0].event_id}`;
  });
  await step("PR: recurring request with per-period conversion quote and quoted proof", async () => {
    const now = Date.now();
    const start = isoSeconds(now), end = isoSeconds(now + 30 * 86400e3), expires = isoSeconds(now + 3600e3);
    const id = JSON.parse(await bob.proposePaymentRequest(alice.publicKey, "10", "usd", "wasm-monthly", "usdt-arbitrum-address", start)).payment_request_id;
    await deliver(bob, alice);
    await alice.claimAndAcceptPaymentRequest(bob.publicKey, id);
    await deliver(alice, bob);
    await expectError(alice.quotePaymentRequest(bob.publicKey, id, start, end, "usdt", "1", expires), "[Policy]");
    const quoted = JSON.parse(await bob.quotePaymentRequest(alice.publicKey, id, start, end, "usdt", "1", expires));
    const quoteId = quoted.conversion_quotes[0].event_id;
    await deliver(bob, alice);
    const proof = JSON.stringify({ test_evidence: "quoted-payment" });
    await alice.submitPaymentProof(bob.publicKey, id, bob.appId, "usdt-arbitrum-address", start, end, quoteId, undefined, proof);
    await deliver(alice, bob);
    const seen = await request(bob, alice, id);
    assert(seen.state === "ActiveRecurring", seen.state);
    assert(seen.payment_proofs[0].conversion_quote_id === quoteId, JSON.stringify(seen.payment_proofs[0]));
    return `payee state=${seen.state}; quote ${quoteId}; payer quote refused with Policy`;
  });
  return bob;
}

// b. Receipts: bob (payee) issues a Receipt to alice and grants Receipt
// Access over the Encrypted Link; alice fetches the encrypted Receipt from
// bob's homeserver (the wasm bounded Response read in receipts/access.rs)
// and decrypts it.
async function receiptFlows(alice, bob) {
  await step("Receipt: bob issues, Receipt Access delivered, alice retrieves and decrypts", async () => {
    const issued = JSON.parse(await bob.issueReceipt(alice.publicKey, "wasm-invoice-1", "0.001", "btc"));
    assert(issued.status === "AccessQueued", issued.status);
    const d = await deliver(bob, alice);
    const views = JSON.parse(await bob.issuedReceiptsTo(alice.publicKey));
    const view = views.find((v) => v.receipt_id === issued.receipt_id);
    assert(view && view.status === "AccessSent", JSON.stringify(views.map((v) => v.status)));
    const receipt = JSON.parse(await alice.retrieveReceipt(bob.publicKey, issued.receipt_id));
    assert(receipt.payment_reference === "wasm-invoice-1", receipt.payment_reference);
    assert(receipt.issuer === bob.publicKey && receipt.recipient_public_key === alice.publicKey, "receipt parties");
    assert(JSON.stringify(receipt.amount).includes("0.001"), JSON.stringify(receipt.amount));
    return `${d}; issuer status=${view.status}; decrypted receipt ${receipt.receipt_id} at ${receipt.location}`;
  });
}

// c. Allowances: alice (payer) allows bob (payee); then one automatic
// payment runs through allowance accounting to a Succeeded attempt.
async function allowanceFlows(alice, bob) {
  let allowanceId;
  await step("Allowance: alice proposes as Allower, bob accepts, both see Accepted", async () => {
    const proposed = JSON.parse(await alice.proposeAllowance(bob.publicKey, "allower", "btc", "0.0001", "0.01", "0.10"));
    assert(proposed.state === "Proposed" && proposed.local_role === "Allower", `${proposed.state} ${proposed.local_role}`);
    allowanceId = proposed.allowance_id;
    await deliver(alice, bob);
    const accepted = JSON.parse(await bob.acceptAllowance(alice.publicKey, allowanceId));
    await deliver(bob, alice);
    const a = JSON.parse(await alice.allowanceRecord(bob.publicKey, allowanceId));
    const b = JSON.parse(await bob.allowanceRecord(alice.publicKey, allowanceId));
    assert(a.state === "Accepted" && b.state === "Accepted", `${a.state} ${b.state}`);
    assert(a.acceptance_event_id && a.acceptance_event_id === b.acceptance_event_id, "acceptance event ids differ");
    return `allowance ${allowanceId}; bob local=${accepted.state}; history=${a.history_status}/${b.history_status}`;
  });
  if (!allowanceId) return allowanceId;
  await step("Allowance accounting: automatic acceptance, reserve, execute, record Succeeded, proof", async () => {
    const id = JSON.parse(await bob.proposePaymentRequest(alice.publicKey, "0.001", "btc", "wasm-allowance-invoice", ENDPOINT, undefined)).payment_request_id;
    await deliver(bob, alice);
    await alice.claimPaymentRequest(bob.publicKey, id);
    const init = JSON.parse(await alice.initializeAllowanceAccounting());
    assert(!init.state.requires_reconciliation, JSON.stringify(init.state));
    const sel = JSON.parse(await alice.selectAllowanceAndAccept(bob.publicKey, id, allowanceId, ENDPOINT, "0.001", "btc"));
    assert(sel.candidates.some((c) => c.allowanceId === allowanceId && !c.blocked), JSON.stringify(sel.candidates));
    await deliver(alice, bob);
    const payee = await request(bob, alice, id);
    assert(payee.state === "Accepted", payee.state);
    const reserved = JSON.parse(await alice.reserveAutomaticPayment(bob.publicKey, id, sel.revision, ENDPOINT, "0.001", "btc"));
    const prepared = reserved.Ready && reserved.Ready.attempt;
    assert(prepared && prepared.status === "Prepared" && prepared.allowance_id === allowanceId, JSON.stringify(reserved));
    const begun = JSON.parse(await alice.beginPaymentExecution(prepared.attempt_id, ENDPOINT, "0.001", "btc"));
    assert(begun.Ready && begun.Ready.attempt.status === "Submitted", JSON.stringify(begun));
    const outcome = JSON.parse(await alice.recordPaymentOutcome(prepared.attempt_id, "succeeded"));
    assert(outcome.status === "Succeeded", outcome.status);
    const state = JSON.parse(await alice.allowanceAccountingState());
    const attempts = state.history.occurrences.flatMap((o) => o.attempts);
    assert(attempts.some((a) => a.attempt_id === prepared.attempt_id && a.status === "Succeeded"), JSON.stringify(attempts.map((a) => a.status)));
    const proof = JSON.stringify({ test_evidence: "allowance-payment" });
    await alice.submitPaymentProof(bob.publicKey, id, bob.appId, ENDPOINT, undefined, undefined, undefined, allowanceId, proof);
    await deliver(alice, bob);
    const seen = await request(bob, alice, id);
    assert(seen.payment_proofs[0].allowance_id === allowanceId, JSON.stringify(seen.payment_proofs[0]));
    return `accounting ${init.action} (epoch ${init.state.epoch}, revision ${init.state.revision}); association rev ${sel.revision}; attempt ${prepared.attempt_id} Prepared->Submitted->Succeeded; accounting revision ${state.revision}; payee saw proof with allowance_id`;
  });
  return allowanceId;
}

// d. Backup: export bob's backup and restore it into a fresh runtime over a
// different backing (a WebCrypto-wrapped IndexedDB store). The original bob
// runtime is dropped: with Resume, two runtimes on one link would reuse a
// transport key and nonce.
async function backupRestoreFlow(alice, bob, bobPlainStore, allowanceId) {
  const encStore = encryptedIdbStateStore(`paykit-spike-enc-bob-${Date.now()}`);
  let restored = null;
  await step("Backup: export bob, restore into fresh runtime over encrypted IndexedDB (Resume)", async () => {
    const before = {
      requests: JSON.parse(await bob.paymentRequestsWith(alice.publicKey)).length,
      receipts: JSON.parse(await bob.issuedReceiptsTo(alice.publicKey)).length,
      lists: await bob.currentPrivatePaymentLists(alice.publicKey),
    };
    const backup = await bob.exportBackup();
    const fingerprint = backup.fingerprint();
    const summary = backup.summary();
    restored = await bob.restoreIntoFreshRuntime(backup, "js", encStore, true);
    const peers = JSON.parse(await restored.linkedPeers());
    assert(peers[alice.publicKey] === "Linked", JSON.stringify(peers));
    const requests = JSON.parse(await restored.paymentRequestsWith(alice.publicKey)).length;
    const receipts = JSON.parse(await restored.issuedReceiptsTo(alice.publicKey)).length;
    const lists = await restored.currentPrivatePaymentLists(alice.publicKey);
    assert(requests === before.requests && receipts === before.receipts && lists === before.lists, JSON.stringify({ before, requests, receipts, lists }));
    if (allowanceId) {
      const record = JSON.parse(await restored.allowanceRecord(alice.publicKey, allowanceId));
      assert(record && record.state === "Accepted", JSON.stringify(record));
    }
    return `backup ${fingerprint} ${summary}; restored: ${requests} Payment Requests, ${receipts} issued Receipts, Linked with alice, allowance kept; enc store saves=${encStore.stats.saves}`;
  });
  if (!restored) return bob;
  bob.free();
  await step("Backup: restored bob keeps using the resumed Encrypted Link both ways", async () => {
    alice.setPrivateDetails(JSON.stringify({ [ENDPOINT]: "ln-private-after-restore" }));
    const r = JSON.parse(await alice.sendPrivatePaymentList(restored.publicKey));
    assert(r.sent >= 1 && r.failed === 0, JSON.stringify(r));
    await restored.receivePrivateMessages(alice.publicKey);
    const views = JSON.parse(await restored.currentPrivatePaymentLists(alice.publicKey));
    assert(views.some((v) => v.endpoints[ENDPOINT] === "ln-private-after-restore"), JSON.stringify(views));
    const id = JSON.parse(await restored.proposePaymentRequest(alice.publicKey, "0.004", "btc", "wasm-after-restore", ENDPOINT, undefined)).payment_request_id;
    const d = await deliver(restored, alice);
    const seen = await request(alice, restored, id);
    return `alice->bob list delivered (outbound queue sent ${r.sent}); bob->alice request ${seen.state} (${d})`;
  });
  await step("Encrypted store: IndexedDB row holds no plaintext (oracle: plaintext store does)", async () => {
    const plainRow = await bobPlainStore.rawRow();
    const encRow = await encStore.rawRow();
    const plainHas = bytesContainAscii(plainRow.bytes, alice.publicKey);
    const encHas = bytesContainAscii(encRow.ct, alice.publicKey);
    assert(plainHas, "oracle failed: plaintext blob does not contain alice's key as ASCII");
    assert(!encHas && !encRow.bytes, "encrypted row leaks plaintext");
    const key = await encStore.key();
    assert(key.extractable === false, "key is extractable");
    let exportRefused = false;
    try { await crypto.subtle.exportKey("raw", key); } catch { exportRefused = true; }
    assert(exportRefused, "exportKey on the stored key succeeded");
    return `plaintext row contains alice's key: ${plainHas}; encrypted row: ${encHas}; iv ${encRow.iv.length} B, ct ${encRow.ct.length} B; key extractable=${key.extractable}, exportKey refused`;
  });
  return restored;
}

// f. Two SDK runtimes for one identity over one IndexedDB database: the
// load barrier makes both read the same revision; the compare-and-swap lets
// exactly one commit and the other fails with ConcurrentUpdate (no lost
// update). Run on the plaintext store, then on the encrypted store, which
// also gets a tamper and an AAD-swap check.
async function concurrentWriterFlows(alice, carol) {
  for (const [label, makeStore] of [["plaintext", idbStateStore], ["WebCrypto-wrapped", encryptedIdbStateStore]]) {
    const dbName = `paykit-spike-cas-${label.startsWith("plain") ? "plain" : "enc"}-${Date.now()}`;
    const storeA = makeStore(dbName);
    const storeB = makeStore(dbName);
    let first, second;
    if (!await step(`CAS (${label}): sign up + second runtime on the same IndexedDB database`, async () => {
      first = await PaykitWasmClient.signUpOnTestnet(`wasm-cas-${label.startsWith("plain") ? "plain" : "enc"}`, HOMESERVER, AUTH_RELAY_INBOX, "js", storeA);
      second = await first.openRuntimeOnStore(storeB);
      return `${first.publicKey}; db=${dbName}`;
    })) continue;
    await step(`CAS (${label}): concurrent writers -> one ConcurrentUpdate, retry, no lost update`, async () => {
      armLoadBarrier(dbName, 2);
      const results = await Promise.allSettled([
        first.saveContact(alice.publicKey, "alice"),
        second.saveContact(carol.publicKey, "carol"),
      ]);
      const ok = results.filter((r) => r.status === "fulfilled");
      const lost = results.map((r, i) => [r, i]).filter(([r]) => r.status === "rejected");
      assert(ok.length === 1 && lost.length === 1, JSON.stringify(results.map((r) => r.status === "fulfilled" ? "ok" : String(r.reason.message || r.reason))));
      const [[loss, lostIndex]] = lost;
      const reason = String(loss.reason.message || loss.reason);
      assert(reason.includes("[ConcurrentUpdate]"), reason);
      await (lostIndex === 0 ? first.saveContact(alice.publicKey, "alice") : second.saveContact(carol.publicKey, "carol"));
      const fromFirst = JSON.parse(await first.contactKeys()).sort();
      const fromSecond = JSON.parse(await second.contactKeys()).sort();
      const expected = [alice.publicKey, carol.publicKey].sort();
      assert(JSON.stringify(fromFirst) === JSON.stringify(expected) && JSON.stringify(fromSecond) === JSON.stringify(expected), JSON.stringify({ fromFirst, fromSecond }));
      return `runtime ${lostIndex + 1} lost: "${reason.slice(0, 120)}"; after retry both runtimes see 2 contacts; CAS refusals seen by the JS stores: ${storeA.stats.conflicts + storeB.stats.conflicts}`;
    });
    if (label.startsWith("WebCrypto")) {
      await step("Encrypted store: tampered ciphertext and swapped revision are rejected, original row still loads", async () => {
        const original = await storeA.rawRow();
        const flipped = new Uint8Array(original.ct); flipped[flipped.length - 1] ^= 1;
        await storeA.putRawRow({ ...original, ct: flipped });
        const tamper = await expectError(first.contactKeys(), "failed authentication");
        await storeA.putRawRow({ ...original, revision: crypto.randomUUID() });
        const swapped = await expectError(first.contactKeys(), "failed authentication");
        await storeA.putRawRow(original);
        const keys = JSON.parse(await first.contactKeys());
        assert(keys.length === 2, JSON.stringify(keys));
        return `tamper -> "${tamper.slice(0, 90)}"; revision swap rejected; restored row loads ${keys.length} contacts; decryptFailures=${storeA.stats.decryptFailures}`;
      });
    }
    second.free();
    first.free();
  }
}

// e. Key rotation: alice rotates the identity-wide Paykit key to generation
// 2; bob sees RecoveryRequired; both drive Encrypted Link recovery and a
// Private Payment List flows again.
async function keyRotationFlow(alice, bob) {
  let rotated = false;
  await step("Key rotation: alice rotates the Paykit identity key (generation 1 -> 2)", async () => {
    const r = JSON.parse(await alice.rotatePaykitIdentityKey());
    assert(r.toGeneration === 2 && r.registryKeyGeneration === 2, JSON.stringify(r));
    rotated = true;
    return JSON.stringify(r);
  });
  if (!rotated) return;
  await step("Key rotation: bob detects it (RecoveryRequired), recovery relinks, list flows again", async () => {
    const err = await expectError(bob.receiveFrom(alice.publicKey), "[RecoveryRequired]");
    const deadline = performance.now() + 30000;
    let a = "RecoveryRequired", b = "RecoveryRequired", rounds = 0;
    while (a !== "Linked" || b !== "Linked") {
      assert(performance.now() < deadline, `recovery timed out (alice=${a}, bob=${b})`);
      if (a !== "Linked") a = await alice.ensureLink(bob.publicKey);
      if (b !== "Linked") b = await bob.ensureLink(alice.publicKey);
      rounds++;
      await sleep_ms(50);
    }
    alice.setPrivateDetails(JSON.stringify({ [ENDPOINT]: "ln-private-after-rotation" }));
    const r = JSON.parse(await alice.sendPrivatePaymentList(bob.publicKey));
    assert(r.sent >= 1 && r.failed === 0, JSON.stringify(r));
    await bob.receivePrivateMessages(alice.publicKey);
    const views = JSON.parse(await bob.currentPrivatePaymentLists(alice.publicKey));
    assert(views.some((v) => v.endpoints[ENDPOINT] === "ln-private-after-rotation"), JSON.stringify(views));
    return `bob: "${err.slice(0, 100)}"; relinked in ${rounds} rounds; list after rotation delivered`;
  });
}

// g. Request timeouts. pubky's request_timeout is a no-op on wasm. A local
// TCP blackhole (www/blackhole.py) stands in for a hung homeserver; its
// status port reports accepted and peer-closed connections, which shows
// whether a cancelled call also aborted its fetch.
async function timeoutFlows(alice, carol) {
  const bh = Number(new URLSearchParams(globalThis.location.search).get("bh"));
  if (!bh) { log("SKIP request-timeout flows (no ?bh=<port> blackhole given)"); return; }
  const status = async () => (await realFetch(`http://127.0.0.1:${bh + 1}/`)).json();
  const outcome = (p) => p.then(() => "resolved", (e) => `rejected: ${e && (e.message || e)}`);
  await step("Timeout: without a deadline a read from a hung homeserver is still pending after 4 s", async () => {
    const s0 = await status();
    fetchShim.blackholeTo = bh;
    const dangling = outcome(carol.readPublicPaymentList(alice.publicKey, alice.appId));
    const r = await Promise.race([dangling, new Promise((res) => setTimeout(() => res("pending"), 4000))]);
    const s1 = await status();
    assert(r === "pending", r);
    assert(s1.accepted > s0.accepted, JSON.stringify({ s0, s1 }));
    return `still pending; blackhole accepted ${s1.accepted - s0.accepted} connection(s), rewrote ${fetchShim.lastRewritten}; left dangling`;
  });
  await step("Timeout: facade deadline (rt::timeout) bounds a hung SDK call and aborts its fetch", async () => {
    const s0 = await status();
    carol.setCallDeadlineMs(1500);
    const started = performance.now();
    const err = await expectError(carol.resolvePublicContactPayment(alice.publicKey), "[DeadlineExceeded]");
    const elapsed = Math.round(performance.now() - started);
    carol.setCallDeadlineMs(undefined);
    await new Promise((res) => setTimeout(res, 500));
    const s1 = await status();
    assert(elapsed >= 1400 && elapsed < 4000, `elapsed ${elapsed} ms`);
    assert(s1.closedByPeer > s0.closedByPeer, `fetch not aborted: ${JSON.stringify({ s0, s1 })}`);
    return `rejected after ${elapsed} ms: "${err.slice(0, 90)}"; blackhole saw ${s1.closedByPeer - s0.closedByPeer} connection(s) closed by the browser`;
  });
  await step("Timeout: fetch-shim headers timeout (AbortSignal) bounds a hung read with no Rust change", async () => {
    fetchShim.headersTimeoutMs = 1500;
    const before = fetchShim.headerTimeouts;
    const started = performance.now();
    const err = await expectError(carol.readPublicPaymentList(alice.publicKey, alice.appId), "get_payment_list");
    const elapsed = Math.round(performance.now() - started);
    fetchShim.headersTimeoutMs = null;
    assert(fetchShim.headerTimeouts > before, "shim timer did not fire");
    assert(elapsed >= 1400 && elapsed < 4000, `elapsed ${elapsed} ms`);
    return `rejected after ${elapsed} ms: "${err.slice(0, 120)}"`;
  });
  await step("Timeout: blackhole off, the same read succeeds under a 10 s deadline", async () => {
    fetchShim.blackholeTo = null;
    carol.setCallDeadlineMs(10000);
    const list = JSON.parse(await carol.readPublicPaymentList(alice.publicKey, alice.appId));
    carol.setCallDeadlineMs(undefined);
    assert(Object.keys(list).length === 2, JSON.stringify(list));
    return `${JSON.stringify(list)}; shim rewrites total=${fetchShim.rewrites}`;
  });
  fetchShim.blackholeTo = null;
  fetchShim.headersTimeoutMs = null;
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
