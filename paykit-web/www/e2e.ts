// THROWAWAY (#411): browser end-to-end check of the GENERATED ubrn web
// bindings (src/generated/paykit.ts) against a local pubky-testnet 0.16.2
// (homeserver 6286, pkarr relay 15411, HTTP relay 15412 on localhost).
//
// alice uses the existing synchronous callback API with Pubky shared-state
// storage (no platform blob store). bob uses the browser-only async callback
// prototype (`web` feature): an IndexedDB state store wrapped with a
// non-extractable WebCrypto AES-GCM key, and an async session provider that
// keeps the Pubky grant secret encrypted in IndexedDB.
//
// Every step logs PASS/FAIL. The run ends with a SUMMARY line.
//
// Run: ./build.sh, start pubky-testnet (static testnet) and serve this
// package directory, e.g. `python3 -m http.server 18741 --bind 127.0.0.1`,
// then open /www/index.html (main thread) or /www/worker.html (dedicated
// worker). Headless: node ../paykit-wasm/www/run-chrome.mjs <chrome>
// <fresh-profile-dir> http://127.0.0.1:18741/www/index.html 9461 240
import {
  uniffiInitAsync,
  FfiPaykitSdk,
  FfiPaykitSdkInterface,
  FfiPaymentPayload,
  FfiPubkyClientConfig,
  FfiPubkyIdentityCapability,
  FfiPubkyLocalSecretKey,
  FfiPubkySessionAccess,
  FfiPubkySessionAccessInterface,
  FfiPubkySessionBootstrap,
  FfiPubkySessionBootstrapResult,
  FfiSdkAsyncPubkySessionProvider,
  FfiSdkAsyncStateBlobStore,
  FfiSdkPaymentAdapter,
  FfiSdkPubkySessionProvider,
  FfiSdkStateBlob,
  FfiSdkStateBlobInterface,
  FfiSdkStateBlobSnapshot,
  FfiLinkedPeerState,
  FfiPrivateReceivingDetail,
  FfiPrivateReceivingDetailReservationResponseKind,
  FfiPublicPaymentResolutionStatus,
  FfiPublicReceivingDetail,
  PaykitFfiError,
  defaultConfig,
  paykitAuthorizerSessionCapabilities,
  webInstallPanicHook,
} from "../src/index.web";

// Well-known StaticTestnet homeserver key (deterministic testnet keypair).
const HOMESERVER = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";
const CLIENT_ID = "paykit-web.spike";
const CLIENT_CONFIG: FfiPubkyClientConfig = {
  // Ignored on wasm32: pubky has no request timeout in the browser.
  requestTimeoutSecs: 30n,
  localTestnetHost: "localhost",
  authRelayUrl: undefined,
};

type Log = (line: string) => void;
type Step = (name: string, f: () => Promise<unknown>) => Promise<boolean>;

class AssertionError extends Error {}
function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new AssertionError(msg);
}
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const bigintSafe = (_k: string, v: unknown) => (typeof v === "bigint" ? v.toString() : v);
const j = (v: unknown) => JSON.stringify(v, bigintSafe);

function describeError(e: any): string {
  if (e && typeof e === "object" && "tag" in e && "inner" in e) {
    return `PaykitFfiError.${e.tag}(${j(e.inner)})`;
  }
  return `${e?.name ?? "Error"}: ${e?.message ?? String(e)}`;
}

// ---------------------------------------------------------------------------
// IndexedDB + WebCrypto helpers
// ---------------------------------------------------------------------------

function idbOpen(name: string): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(name, 1);
    req.onupgradeneeded = () => req.result.createObjectStore("kv");
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function idbGet<T>(db: IDBDatabase, key: string): Promise<T | undefined> {
  return new Promise((resolve, reject) => {
    const req = db.transaction("kv", "readonly").objectStore("kv").get(key);
    req.onsuccess = () => resolve(req.result as T | undefined);
    req.onerror = () => reject(req.error);
  });
}

function idbPut(db: IDBDatabase, key: string, value: unknown): Promise<void> {
  return new Promise((resolve, reject) => {
    const tx = db.transaction("kv", "readwrite");
    tx.objectStore("kv").put(value, key);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

function idbDelete(db: IDBDatabase, key: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const tx = db.transaction("kv", "readwrite");
    tx.objectStore("kv").delete(key);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

/// A non-extractable AES-GCM-256 key kept in IndexedDB (CryptoKey objects are
/// structured-cloneable, so the raw key bytes never reach JS).
async function wrappingKey(db: IDBDatabase): Promise<CryptoKey> {
  const existing = await idbGet<CryptoKey>(db, "kek");
  if (existing) return existing;
  const key = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, [
    "encrypt",
    "decrypt",
  ]);
  await idbPut(db, "kek", key);
  return key;
}

type SealedRow = { revision: string; iv: Uint8Array; ct: ArrayBuffer };

async function seal(key: CryptoKey, revision: string, plaintext: Uint8Array): Promise<SealedRow> {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ct = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv, additionalData: new TextEncoder().encode(revision) },
    key,
    plaintext,
  );
  return { revision, iv, ct };
}

async function unseal(key: CryptoKey, row: SealedRow): Promise<ArrayBuffer> {
  return crypto.subtle.decrypt(
    { name: "AES-GCM", iv: row.iv, additionalData: new TextEncoder().encode(row.revision) },
    key,
    row.ct,
  );
}

// ---------------------------------------------------------------------------
// bob's async callbacks
// ---------------------------------------------------------------------------

/// Async FfiSdkStateBlobStore over IndexedDB, sealed with WebCrypto.
///
/// Compare-and-swap: the blob is encrypted first (crypto.subtle cannot run
/// inside an IndexedDB transaction, which auto-commits once it has no pending
/// requests), then one readwrite transaction reads the current revision and
/// writes only if it still equals `expectedRevision`. IndexedDB serializes
/// readwrite transactions on a store, so the check and the write are atomic.
class IdbAsyncStateStore implements FfiSdkAsyncStateBlobStore {
  stats = { loads: 0, saves: 0, conflicts: 0, lastBlobBytes: 0, lastCiphertextBytes: 0 };
  // Test hook: make the next save throw this value instead of writing.
  failNextSaveWith: unknown = undefined;
  // Test only: last plaintext prefix, to check the row holds no plaintext.
  lastPlaintextProbe: Uint8Array | undefined;

  private constructor(private db: IDBDatabase, private key: CryptoKey) {}

  static async open(name: string): Promise<IdbAsyncStateStore> {
    const db = await idbOpen(name);
    return new IdbAsyncStateStore(db, await wrappingKey(db));
  }

  async rawRow(): Promise<SealedRow | undefined> {
    return idbGet<SealedRow>(this.db, "state");
  }

  async loadStateBlob(): Promise<FfiSdkStateBlobSnapshot | undefined> {
    this.stats.loads++;
    const row = await this.rawRow();
    if (!row) return undefined;
    const plaintext = await unseal(this.key, row);
    return { blob: new FfiSdkStateBlob(plaintext), revision: row.revision };
  }

  async saveStateBlobAtomically(
    blob: FfiSdkStateBlobInterface,
    expectedRevision: string | undefined,
  ): Promise<string> {
    if (this.failNextSaveWith !== undefined) {
      const err = this.failNextSaveWith;
      this.failNextSaveWith = undefined;
      throw err;
    }
    const plaintext = new Uint8Array(blob.exportBytes());
    const revision = crypto.randomUUID();
    const row = await seal(this.key, revision, plaintext);
    this.lastPlaintextProbe = plaintext.slice(0, 64);
    this.stats.lastBlobBytes = plaintext.length;
    this.stats.lastCiphertextBytes = row.ct.byteLength;
    plaintext.fill(0); // best effort: wasm memory and the GC still hold copies
    const committed = await new Promise<boolean>((resolve, reject) => {
      const tx = this.db.transaction("kv", "readwrite");
      const store = tx.objectStore("kv");
      let ok = false;
      const get = store.get("state");
      get.onsuccess = () => {
        const current = get.result as SealedRow | undefined;
        if ((current?.revision ?? undefined) === expectedRevision) {
          store.put(row, "state");
          ok = true;
        }
      };
      tx.oncomplete = () => resolve(ok);
      tx.onerror = () => reject(tx.error);
    });
    if (!committed) {
      this.stats.conflicts++;
      throw new PaykitFfiError.ConcurrentUpdate({
        code: "state_revision_changed",
        context: "IndexedDB state revision changed",
      });
    }
    this.stats.saves++;
    return revision;
  }
}

/// Async FfiSdkPubkySessionProvider: the grant secret and local Pubky key are
/// sealed with WebCrypto in IndexedDB, and decrypted on every load.
class IdbAsyncSessionProvider implements FfiSdkAsyncPubkySessionProvider {
  stats = { loads: 0, clears: 0 };
  private constructor(private db: IDBDatabase, private key: CryptoKey) {}

  static async open(name: string): Promise<IdbAsyncSessionProvider> {
    const db = await idbOpen(name);
    return new IdbAsyncSessionProvider(db, await wrappingKey(db));
  }

  async persist(access: FfiPubkySessionAccessInterface): Promise<void> {
    const local = access.exportLocalSecretKey();
    const record = {
      clientId: access.clientId(),
      sessionSecret: access.exportSessionSecret(),
      localSecretKey: local ? Array.from(new Uint8Array(local.exportBytes())) : null,
    };
    const plaintext = new TextEncoder().encode(JSON.stringify(record));
    await idbPut(this.db, "session", await seal(this.key, "session", plaintext));
    plaintext.fill(0);
  }

  async rawRow(): Promise<SealedRow | undefined> {
    return idbGet<SealedRow>(this.db, "session");
  }

  async loadSessionAccess(): Promise<FfiPubkySessionAccessInterface | undefined> {
    this.stats.loads++;
    const row = await this.rawRow();
    if (!row) return undefined;
    const record = JSON.parse(new TextDecoder().decode(await unseal(this.key, row)));
    const local = record.localSecretKey
      ? new FfiPubkyLocalSecretKey(new Uint8Array(record.localSecretKey).buffer)
      : undefined;
    return new FfiPubkySessionAccess(record.clientId, record.sessionSecret, local, undefined);
  }

  publicStorageAvailable(): boolean {
    return true;
  }

  async clearSessionAccess(): Promise<void> {
    this.stats.clears++;
    await idbDelete(this.db, "session");
  }
}

// ---------------------------------------------------------------------------
// Synchronous callbacks (the API every binding already has)
// ---------------------------------------------------------------------------

/// Synchronous session provider holding the bootstrap's live access in memory.
class MemorySessionProvider implements FfiSdkPubkySessionProvider {
  constructor(private access: FfiPubkySessionAccessInterface | undefined) {}
  loadSessionAccess() {
    return this.access;
  }
  publicStorageAvailable() {
    return true;
  }
  clearSessionAccess() {
    this.access = undefined;
  }
}

/// Synchronous payment adapter whose receiving details the test sets.
class TestPaymentAdapter implements FfiSdkPaymentAdapter {
  publicDetails: Array<[string, string]> = [];
  privateDetails: Array<[string, string]> = [];
  currentPublicReceivingDetails(): Array<FfiPublicReceivingDetail> {
    return this.publicDetails.map(([identifier, text]) => ({
      identifier,
      payload: new FfiPaymentPayload(text),
    }));
  }
  currentPrivateReceivingDetails(_counterparty: string): Array<FfiPrivateReceivingDetail> {
    return this.privateDetails.map(([identifier, text]) => ({
      identifier,
      payload: new FfiPaymentPayload(text),
    }));
  }
  reservePrivateReceivingDetails(_counterparty: string) {
    return {
      kind: FfiPrivateReceivingDetailReservationResponseKind.UseCurrentReceivingDetails,
      reservations: [],
    };
  }
  cancelPrivateReceivingDetailReservation() {}
  selectPublicPaymentEndpointIds(request: any): Array<string> {
    return request.candidates.map((c: any) => c.candidateId);
  }
  buildPublicPaymentTarget(endpoint: any) {
    return { payload: endpoint.payload };
  }
  selectPrivatePaymentEndpointIds(request: any): Array<string> {
    return request.candidates.map((c: any) => c.candidateId);
  }
  buildPrivatePaymentTarget(endpoint: any) {
    return { payload: endpoint.payload };
  }
}

async function signUp(): Promise<FfiPubkySessionBootstrapResult> {
  const bootstrap = FfiPubkySessionBootstrap.withPubkyClientConfig(CLIENT_ID, CLIENT_CONFIG);
  const secret = new FfiPubkyLocalSecretKey(crypto.getRandomValues(new Uint8Array(32)).buffer);
  return bootstrap.signUp(secret, HOMESERVER, undefined, paykitAuthorizerSessionCapabilities());
}

async function publishIdentity(sdk: FfiPaykitSdkInterface, name: string) {
  await sdk.publishPaykitNoiseKeyAuthorization();
  return sdk.publishPaykitApp(name, {
    privatePayments: true,
    paymentRequests: true,
    receipts: true,
    outgoingPayments: true,
  });
}

async function untilLinked(a: FfiPaykitSdkInterface, aKey: string, b: FfiPaykitSdkInterface, bKey: string) {
  const deadline = performance.now() + 20000;
  let sa = FfiLinkedPeerState.Linking;
  let sb = FfiLinkedPeerState.Linking;
  let rounds = 0;
  while (sa !== FfiLinkedPeerState.Linked || sb !== FfiLinkedPeerState.Linked) {
    assert(
      performance.now() < deadline,
      `handshake timed out (a=${FfiLinkedPeerState[sa]}, b=${FfiLinkedPeerState[sb]})`,
    );
    if (sa !== FfiLinkedPeerState.Linked) sa = (await a.advanceLinkHandshake(bKey)).state;
    if (sb !== FfiLinkedPeerState.Linked) sb = (await b.advanceLinkHandshake(aKey)).state;
    rounds++;
    await sleep(50);
  }
  return rounds;
}

function containsWindow(haystack: Uint8Array, needle: Uint8Array): boolean {
  outer: for (let i = 0; i + needle.length <= haystack.length; i++) {
    for (let k = 0; k < needle.length; k++) if (haystack[i + k] !== needle[k]) continue outer;
    return true;
  }
  return false;
}

export async function runE2e(log: Log): Promise<void> {
  let passed = 0;
  let failed = 0;
  const step: Step = async (name, f) => {
    const t0 = performance.now();
    try {
      const detail = await f();
      passed++;
      const extra = detail !== undefined ? ` :: ${typeof detail === "string" ? detail : j(detail)}` : "";
      log(`PASS ${name} (${Math.round(performance.now() - t0)} ms)${extra}`);
      return true;
    } catch (e) {
      failed++;
      log(`FAIL ${name} (${Math.round(performance.now() - t0)} ms) :: ${describeError(e)}`);
      return false;
    }
  };

  const context =
    typeof (globalThis as any).window === "undefined" ? "DedicatedWorkerGlobalScope" : "Window";
  log(`INFO userAgent ${navigator.userAgent}`);
  log(`INFO context ${context}; origin ${location.origin}`);
  const t0 = performance.now();
  try {
    await run(step);
  } finally {
    const secs = ((performance.now() - t0) / 1000).toFixed(1);
    log(`SUMMARY passed=${passed} failed=${failed} context=${context} (${secs} s)`);
  }
}

async function run(step: Step) {
  const loaded = await step("load wasm + verify every UniFFI checksum (uniffiInitAsync)", async () => {
    await uniffiInitAsync();
    webInstallPanicHook();
  });
  if (!loaded) return;

  // ---- alice: synchronous callbacks + Pubky shared-state storage ----------
  const aliceAdapter = new TestPaymentAdapter();
  let alice!: FfiPaykitSdkInterface;
  let aliceKey = "";
  const aliceUp = await step(
    "alice sign-up (FfiPubkySessionBootstrap.signUp) + withPaymentAdapterAndPubkySharedStateAndClientConfig + initialize",
    async () => {
      const result = await signUp();
      aliceKey = result.publicKey;
      alice = FfiPaykitSdk.withPaymentAdapterAndPubkySharedStateAndClientConfig(
        new MemorySessionProvider(result.sessionAccess),
        aliceAdapter,
        defaultConfig("web-alice-app"),
        CLIENT_CONFIG,
      );
      const status = await alice.initialize();
      assert(status.capability === FfiPubkyIdentityCapability.PrivateLinkCapable, j(status));
      return `${aliceKey} ${FfiPubkyIdentityCapability[status.capability]}`;
    },
  );
  if (!aliceUp) return;
  await step("alice publishPaykitNoiseKeyAuthorization + publishPaykitApp", async () => {
    const registry = await publishIdentity(alice, "Paykit web alice");
    assert(registry.apps.some((a) => a.appId === "web-alice-app"), j(registry));
    return `apps=${registry.apps.map((a) => a.appId).join(",")} keyGeneration=${registry.keyGeneration}`;
  });
  await step("alice syncPublicEndpoints publishes 2 public Payment Endpoints", async () => {
    aliceAdapter.publicDetails = [
      ["btc-lightning-bolt11", "lnbc-ubrn-alice"],
      ["btc-onchain", "bc1q-ubrn-alice"],
    ];
    const report = await alice.syncPublicEndpoints();
    assert(report.published.length === 2 && report.failed.length === 0, j(report));
    return `published=${report.published.map((c) => c.identifier).join(",")}`;
  });
  await step("alice stateRevision() reports the shared-state revision", async () => {
    const revision = alice.stateRevision();
    assert(typeof revision === "string" && revision.length > 0, String(revision));
    return revision;
  });

  // ---- bob: async callbacks (IndexedDB + WebCrypto) -----------------------
  const dbName = `paykit-web-bob-${Date.now()}`;
  const bobStore = await IdbAsyncStateStore.open(dbName);
  const bobSession = await IdbAsyncSessionProvider.open(dbName);
  const bobAdapter = new TestPaymentAdapter();
  let bob!: FfiPaykitSdkInterface;
  let bobKey = "";
  const bobUp = await step(
    "bob sign-up, seal the grant in IndexedDB, withAsyncCallbacks + initialize (async store + async session provider)",
    async () => {
      const result = await signUp();
      bobKey = result.publicKey;
      await bobSession.persist(result.sessionAccess);
      bob = FfiPaykitSdk.withAsyncCallbacks(
        bobStore,
        bobSession,
        bobAdapter,
        defaultConfig("web-bob-app"),
        CLIENT_CONFIG,
      );
      const status = await bob.initialize();
      assert(status.capability === FfiPubkyIdentityCapability.PrivateLinkCapable, j(status));
      assert(
        bobStore.stats.saves > 0 && bobSession.stats.loads > 0,
        j({ store: bobStore.stats, session: bobSession.stats }),
      );
      return `${bobKey}; store=${j(bobStore.stats)} session=${j(bobSession.stats)}`;
    },
  );
  if (!bobUp) return;
  await step("bob publishPaykitNoiseKeyAuthorization + publishPaykitApp", async () => {
    const registry = await publishIdentity(bob, "Paykit web bob");
    return `apps=${registry.apps.map((a) => a.appId).join(",")}`;
  });
  await step("bob reads alice's App Registry and public endpoints (resolvePublicContactPayment)", async () => {
    const registry = await bob.paykitAppRegistry(aliceKey);
    assert(registry?.apps.some((a) => a.appId === "web-alice-app"), j(registry));
    const r = await bob.resolvePublicContactPayment(aliceKey, undefined);
    const payloads = r.payableEndpoints.map((e) => e.payload.exportText()).sort();
    assert(
      r.status === FfiPublicPaymentResolutionStatus.Payable,
      `status=${FfiPublicPaymentResolutionStatus[r.status]}`,
    );
    assert(j(payloads) === j(["bc1q-ubrn-alice", "lnbc-ubrn-alice"]), j(payloads));
    return `status=${FfiPublicPaymentResolutionStatus[r.status]} payloads=${payloads.join(",")}`;
  });

  // ---- Encrypted Link + Private Payment List -------------------------------
  const linked = await step(
    "Encrypted Link Handshake alice<->bob (initiate/accept/advance until Linked)",
    async () => {
      const a = await alice.initiateLinkWithPeer(bobKey);
      const b = await bob.acceptLinkWithPeer(aliceKey);
      const rounds = await untilLinked(alice, aliceKey, bob, bobKey);
      return `initiate=${FfiLinkedPeerState[a.state]} accept=${FfiLinkedPeerState[b.state]} rounds=${rounds}`;
    },
  );
  if (!linked) return;
  await step("alice enqueuePrivatePaymentList(bob) + processPendingPrivateMessages", async () => {
    aliceAdapter.privateDetails = [["btc-lightning-bolt11", "ln-private-ubrn-alice-1"]];
    await alice.enqueuePrivatePaymentList(bobKey);
    const reports = await alice.processPendingPrivateMessages();
    const sent = reports.reduce((n, r) => n + (r.report?.sent.length ?? 0), 0);
    const errors = reports.filter((r) => r.error).map((r) => r.error!.code());
    assert(sent >= 1 && errors.length === 0, j({ sent, errors }));
    return `sent=${sent}`;
  });
  await step("bob receivePrivateMessages(alice) + currentPrivatePaymentLists(alice)", async () => {
    const intake = await bob.receivePrivateMessages(aliceKey);
    const views = await bob.currentPrivatePaymentLists(aliceKey);
    const view = views.find((v) => v.appId === "web-alice-app");
    const got = view?.paymentEndpoints.map((e) => `${e.identifier}=${e.payload.exportText()}`) ?? [];
    assert(got.includes("btc-lightning-bolt11=ln-private-ubrn-alice-1"), j(got));
    return `streamItems=${intake.streamItemIds.length} list=${got.join(",")}`;
  });
  await step("bob private list in the other direction (bob -> alice)", async () => {
    bobAdapter.privateDetails = [["btc-onchain", "bc1q-private-ubrn-bob"]];
    await bob.enqueuePrivatePaymentList(aliceKey);
    const reports = await bob.processPendingPrivateMessages();
    const sent = reports.reduce((n, r) => n + (r.report?.sent.length ?? 0), 0);
    await alice.receivePrivateMessages(bobKey);
    const views = await alice.currentPrivatePaymentLists(bobKey);
    const got = views.flatMap((v) => v.paymentEndpoints.map((e) => e.payload.exportText()));
    assert(sent >= 1 && got.includes("bc1q-private-ubrn-bob"), j({ sent, got }));
    return `sent=${sent} alice sees ${got.join(",")}`;
  });

  // ---- async store properties ---------------------------------------------
  await step("bob's IndexedDB rows are AES-GCM sealed (no plaintext window, +16 B tag)", async () => {
    const row = await bobStore.rawRow();
    assert(row && bobStore.lastPlaintextProbe, "no state row");
    const ct = new Uint8Array(row.ct);
    assert(
      !containsWindow(ct, bobStore.lastPlaintextProbe.slice(0, 16)),
      "plaintext window found in ciphertext",
    );
    assert(
      bobStore.stats.lastCiphertextBytes === bobStore.stats.lastBlobBytes + 16,
      j(bobStore.stats),
    );
    const session = await bobSession.rawRow();
    assert(session && !new TextDecoder().decode(session.ct).includes(CLIENT_ID), "session row holds plaintext");
    return `state=${ct.length} B, session=${session.ct.byteLength} B, stats=${j(bobStore.stats)}`;
  });
  await step("bob stateRevision() equals the IndexedDB row revision (last observed)", async () => {
    const row = await bobStore.rawRow();
    const revision = bob.stateRevision();
    assert(revision === row?.revision, `${revision} != ${row?.revision}`);
    return revision;
  });
  await step(
    "ConcurrentUpdate thrown by an async JS callback reaches the caller as PaykitFfiError.ConcurrentUpdate",
    async () => {
      bobStore.failNextSaveWith = new PaykitFfiError.ConcurrentUpdate({
        code: "test_conflict",
        context: "forced by test",
      });
      try {
        await bob.saveContact({ publicKey: aliceKey, label: "alice" });
      } catch (e: any) {
        assert(e?.tag === "ConcurrentUpdate", describeError(e));
        return describeError(e);
      }
      throw new AssertionError("saveContact did not fail");
    },
  );
  await step(
    "a non-Paykit rejection from an async callback maps to PaykitFfiError.Storage",
    async () => {
      bobStore.failNextSaveWith = new TypeError("boom from IndexedDB");
      try {
        await bob.saveContact({ publicKey: aliceKey, label: "alice" });
      } catch (e: any) {
        assert(e?.tag === "Storage", describeError(e));
        return describeError(e);
      }
      throw new AssertionError("saveContact did not fail");
    },
  );
  await step("after both failures the next write succeeds (async lock released)", async () => {
    const contact = await bob.saveContact({ publicKey: aliceKey, label: "alice" });
    return `${contact.publicKey.slice(0, 12)}... label=${contact.label}`;
  });
  await step("an already-aborted AbortSignal rejects a generated async method", async () => {
    const controller = new AbortController();
    controller.abort();
    try {
      await bob.linkedPeers({ signal: controller.signal });
    } catch (e: any) {
      return describeError(e);
    }
    throw new AssertionError("linkedPeers did not reject");
  });

  // ---- reload from the async store -----------------------------------------
  await step(
    "fresh bob runtime on the same IndexedDB (reload): identity, Linked peer and list restored",
    async () => {
      const loadsBefore = bobStore.stats.loads;
      const bob2 = FfiPaykitSdk.withAsyncCallbacks(
        bobStore,
        bobSession,
        new TestPaymentAdapter(),
        defaultConfig("web-bob-app"),
        CLIENT_CONFIG,
      );
      const status = await bob2.identityStatus();
      assert(status?.publicKey === bobKey, j(status));
      const peers = await bob2.linkedPeers();
      const peer = peers.find((p) => p.counterparty === aliceKey);
      assert(peer?.state === FfiLinkedPeerState.Linked, j(peers));
      const views = await bob2.currentPrivatePaymentLists(aliceKey);
      assert(views.length > 0, "private list lost");
      (bob2 as any).uniffiDestroy?.();
      return `loads +${bobStore.stats.loads - loadsBefore}, peer ${FfiLinkedPeerState[peer.state]}, lists=${views.length}`;
    },
  );

  // ---- sign-out ------------------------------------------------------------
  await step(
    "bob signOut revokes the grant and awaits clearSessionAccess (IndexedDB row deleted)",
    async () => {
      const status = await bob.signOut();
      assert(status.capability === FfiPubkyIdentityCapability.SignedOut, j(status));
      assert(
        bobSession.stats.clears === 1 && (await bobSession.rawRow()) === undefined,
        j(bobSession.stats),
      );
      return `${FfiPubkyIdentityCapability[status.capability]} clears=${bobSession.stats.clears}`;
    },
  );
  await step("alice signOut", async () => {
    const status = await alice.signOut();
    return FfiPubkyIdentityCapability[status.capability];
  });
}
