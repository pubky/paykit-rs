// THROWAWAY (#411): dedicated-worker entry for the paykit-web e2e.
import { runE2e } from "./e2e";
runE2e((line) => postMessage({ line }))
  .catch((e) => postMessage({ line: `FAIL uncaught: ${e}` }))
  .finally(() => postMessage({ done: true }));
