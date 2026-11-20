// The service-worker probe of the paykit-browser hosted test harness. Test rig code only: it
// is not a proposal for the SDK's JavaScript surface, and it makes no claim that the SDK
// supports service workers. It shows only that the bundle instantiates and the `pure`
// scenario runs in a module service worker under the same policy, which switch-rig.mjs sends
// on this script's own response. It imports the glue statically, through worker.mjs, and
// answers the protocol of page.mjs for the load, `pure` and the violation list only, through
// the client that sent the request. No store-backed scenario runs here.

import { answer } from "./worker.mjs";

const ALLOWED = ["worker:load", "worker:violations", "pure"];

self.addEventListener("install", (event) => event.waitUntil(self.skipWaiting()));
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
self.addEventListener("message", (event) => {
  event.waitUntil(answer(event.data, ALLOWED).then((reply) => event.source.postMessage(reply)));
});
