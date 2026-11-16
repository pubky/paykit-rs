// THROWAWAY (#411): drive headless Chrome over the DevTools protocol (no
// puppeteer). Launches Chrome with an isolated profile, loads the page, waits
// for document.title === "e2e-done", prints #out and every console message.
// Usage: node run-chrome.mjs <chrome> <profileDir> <url> <debugPort> [timeoutSec]
import { spawn } from "node:child_process";
const [,, chromePath, profileDir, url, port, timeoutSec = "180"] = process.argv;
const chrome = spawn(chromePath, [
  "--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${profileDir}`,
  "--no-first-run", "--no-default-browser-check", "about:blank",
], { stdio: "ignore" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let target;
for (let i = 0; i < 50 && !target; i++) {
  await sleep(200);
  try { target = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find((t) => t.type === "page"); } catch {}
}
if (!target) { console.log("no chrome target"); chrome.kill(); process.exit(2); }
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((r) => ws.addEventListener("open", r, { once: true }));
let id = 0; const pending = new Map(); const logs = [];
const t0 = Date.now();
const stamp = () => `+${((Date.now() - t0) / 1000).toFixed(1)}s`;
ws.addEventListener("message", (ev) => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  if (m.method === "Runtime.consoleAPICalled") logs.push(`${stamp()} console.${m.params.type}: ` + m.params.args.map((a) => a.value ?? a.description).join(" "));
  if (m.method === "Runtime.exceptionThrown") logs.push(`${stamp()} exception: ` + (m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text));
  if (m.method === "Log.entryAdded") logs.push(`${stamp()} log.${m.params.entry.level}: ${m.params.entry.text}${m.params.entry.url ? " " + m.params.entry.url : ""}`);
});
const send = (method, params = {}) => new Promise((r) => { const i = ++id; pending.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
await send("Runtime.enable"); await send("Log.enable");
const version = await send("Runtime.evaluate", { expression: "navigator.userAgent", returnByValue: true });
await send("Page.navigate", { url });
let done = false;
const deadline = Date.now() + Number(timeoutSec) * 1000;
while (!done && Date.now() < deadline) {
  await sleep(500);
  const r = await send("Runtime.evaluate", { expression: "document.title", returnByValue: true });
  done = r.result?.result?.value === "e2e-done";
}
const out = await send("Runtime.evaluate", { expression: "document.getElementById('out')?.textContent", returnByValue: true });
console.log(`=== RESULT (done=${done}, ${stamp()}) ${version.result?.result?.value} ===\n` + out.result?.result?.value);
console.log("=== CONSOLE / LOG ===\n" + logs.join("\n"));
ws.close(); chrome.kill(); process.exit(done ? 0 : 1);
