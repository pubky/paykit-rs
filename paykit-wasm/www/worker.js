// THROWAWAY (#411): runs the same e2e scenario inside a dedicated module worker.
import { runE2e } from "./e2e.js";
runE2e((line) => postMessage({ line })).finally(() => postMessage({ done: true }));
