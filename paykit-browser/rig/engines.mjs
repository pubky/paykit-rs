// What runner.mjs and harness-runner.mjs share: the browser engines they drive through
// Playwright, the engine builds known to have no Origin Private File System, and two small
// helpers. Test rig code only. Importing it has no side effects.

import { chromium, firefox, webkit } from "playwright";

// The engines by name: Playwright's three, and "chrome", the installed Google Chrome through
// Playwright's chromium with channel "chrome".
export const ENGINES = {
  chromium: { type: chromium, options: {} },
  firefox: { type: firefox, options: {} },
  webkit: { type: webkit, options: {} },
  chrome: { type: chromium, options: { channel: "chrome" } },
};

// Engine builds with no Origin Private File System, by engine and Node's process.platform, and
// why. Playwright's WebKit for Linux exposes no `navigator.storage` in a page and no
// `FileSystemSyncAccessHandle` in a dedicated worker, also in a persistent context.
const NO_OPFS = {
  webkit: {
    linux: "Playwright's WebKit for Linux has no navigator.storage, so no Origin Private File System",
  },
};

// Why `engineName` on this platform has no Origin Private File System, or undefined when
// NO_OPFS does not name it.
export function noOpfsReason(engineName) {
  return Object.hasOwn(NO_OPFS, engineName) ? NO_OPFS[engineName][process.platform] : undefined;
}

// The failure when an engine build that NO_OPFS names has the Origin Private File System after
// all: its entry must go, so that what it excludes runs again.
export function staleNoOpfs(engineName) {
  return `${engineName} on ${process.platform} has the Origin Private File System: remove its NO_OPFS entry`;
}

// Some engines leave the message out of the stack, so print both.
export function errorText(error) {
  const message = error?.message ?? String(error);
  const stack = error?.stack ?? "";
  return stack.includes(message) ? stack : `${message}\n${stack}`.trimEnd();
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
