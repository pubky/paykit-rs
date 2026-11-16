// THROWAWAY (#411): does a browser serve Paykit a stale copy of a mutable
// homeserver resource from its HTTP cache?
//
// The homeserver answers GETs with `last-modified`, `etag` and
// `cache-control: private, must-revalidate` but no max-age, so RFC 9111
// heuristic freshness (typically 10% of the resource's age) applies, and
// reqwest's wasm backend calls fetch() with the default cache mode.
// alice publishes v1, waits AGE_MS so the resource has an age, bob reads it,
// alice publishes v2, bob reads again at once. With `?nostore=1` the page
// forces `cache: "no-store"` on every fetch as the candidate fix.
import init, { PaykitWasmClient, sleep_ms } from "../pkg/paykit_wasm.js";

const HOMESERVER = "8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo";
const AUTH_RELAY_INBOX = "http://localhost:15412/inbox";
const params = new URLSearchParams(location.search);
const AGE_MS = Number(params.get("age") ?? 30000);
const noStore = params.get("nostore") === "1";
const realFetch = globalThis.fetch.bind(globalThis);
if (noStore) globalThis.fetch = (input, init) => realFetch(new Request(new Request(input, init), { cache: "no-store" }));

export async function runProbe(log) {
  log(`INFO ${navigator.userAgent}; noStore=${noStore}; age=${AGE_MS} ms`);
  await init();
  const alice = await PaykitWasmClient.signUpOnTestnet("probe-alice", HOMESERVER, AUTH_RELAY_INBOX, "memory");
  const bob = await PaykitWasmClient.signUpOnTestnet("probe-bob", HOMESERVER, AUTH_RELAY_INBOX, "memory");
  const read = async () => JSON.parse(await bob.readPublicPaymentList(alice.publicKey, alice.appId))["btc-onchain"];
  alice.setPublicDetails(JSON.stringify({ "btc-onchain": "v1" }));
  await alice.syncPublicEndpoints();
  await sleep_ms(AGE_MS);
  const first = await read();
  alice.setPublicDetails(JSON.stringify({ "btc-onchain": "v2" }));
  await alice.syncPublicEndpoints();
  const second = await read();
  await sleep_ms(AGE_MS / 10 + 2000);
  const later = await read();
  const stale = second !== "v2";
  log(`${stale ? "STALE" : "FRESH"} first=${first} immediatelyAfterUpdate=${second} after${AGE_MS / 10 + 2000}ms=${later}`);
  return stale;
}
