// THROWAWAY (#411): dedicated worker that hosts one Paykit SDK instance for
// the panic-recovery experiment. Messages: {id, op, args} with op "open"
// ([identityJson, dbName]), "call" ([method, ...args]) or "panic" ([kind]).
// Replies {id, ok, value | error}. A Rust panic traps the wasm instance
// (panic = "abort"); the page recovers by terminating this worker and
// starting a new one that reopens the identity from IndexedDB.
import init, { PaykitWasmClient } from "../pkg/paykit_wasm.js";
import { idbStateStore, errorText } from "./e2e.js";

const ready = init();
let client = null;
const errors = [];
self.addEventListener("error", (e) => errors.push(`error event: ${e.message}`));
self.addEventListener("unhandledrejection", (e) => errors.push(`unhandledrejection: ${e.reason && (e.reason.message || e.reason)}`));

onmessage = async (ev) => {
  const { id, op, args } = ev.data;
  const reply = (r) => postMessage({ id, ...r, errorsSeen: errors.slice() });
  try {
    await ready;
    if (op === "open") {
      const [identity, dbName] = args;
      const ident = JSON.parse(identity);
      client = await PaykitWasmClient.openOnTestnet(ident.appId, ident.secretKeyHex, ident.sessionSecret, "js", idbStateStore(dbName));
      return reply({ ok: true, value: client.publicKey });
    }
    if (op === "call") {
      const [method, ...rest] = args;
      return reply({ ok: true, value: await client[method](...rest) });
    }
    if (op === "panic") {
      return reply({ ok: true, value: await client.debugPanic(args[0]) });
    }
    if (op === "static") {
      const [method, ...rest] = args;
      return reply({ ok: true, value: await PaykitWasmClient[method](...rest) });
    }
    reply({ ok: false, error: `unknown op ${op}` });
  } catch (e) {
    reply({ ok: false, error: errorText(e).split("\n").slice(0, 2).join(" | ") });
  }
};
