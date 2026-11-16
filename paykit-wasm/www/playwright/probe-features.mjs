// THROWAWAY (#411): browser feature probe for the lifecycle notes.
import { chromium, firefox, webkit } from "playwright";
const [,, engine] = process.argv;
const types = { chromium, firefox, webkit };
const opts = engine === "chromium" ? { executablePath: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" } : {};
const browser = await types[engine].launch({ headless: true, ...opts });
const page = await browser.newPage();
await page.goto("http://127.0.0.1:8410/");
const r = await page.evaluate(async () => {
  const db = await new Promise((res, rej) => { const q = indexedDB.open("probe-" + Date.now(), 1); q.onupgradeneeded = () => q.result.createObjectStore("s"); q.onsuccess = () => res(q.result); q.onerror = () => rej(q.error); });
  let durability;
  try { const tx = db.transaction("s", "readwrite", { durability: "strict" }); durability = tx.durability ?? "(property missing: option ignored)"; } catch (e) { durability = "throws " + e.name; }
  // LOCK preflight straight from the page (CORS for WebDAV methods).
  let lockPreflight;
  try { const res = await fetch("http://localhost:6286/pub/x", { method: "LOCK" }); lockPreflight = `reached server: ${res.status}`; } catch (e) { lockPreflight = "blocked: " + e.message; }
  return {
    webLocks: !!navigator.locks, broadcastChannel: typeof BroadcastChannel === "function",
    sharedWorker: typeof SharedWorker === "function", abortSignalAny: typeof AbortSignal.any === "function",
    storageBuckets: !!navigator.storageBuckets, persist: !!navigator.storage?.persist,
    idbDurability: durability, lockPreflight, crossOriginIsolated,
  };
});
console.log(engine, browser.version(), JSON.stringify(r));
await browser.close();
