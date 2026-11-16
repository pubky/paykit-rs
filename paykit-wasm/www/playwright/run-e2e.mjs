// THROWAWAY (#411): run paykit-wasm/www e2e pages in a Playwright engine.
// Usage: node run-e2e.mjs <chromium|firefox|webkit> <url> [timeoutSec]
// Waits for document.title === "e2e-done", prints #out and console output.
import { chromium, firefox, webkit } from "playwright";
const [,, engine, url, timeoutSec = "300"] = process.argv;
const types = { chromium, firefox, webkit };
const opts = engine === "chromium" ? { executablePath: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" } : {};
const browser = await types[engine].launch({ headless: true, ...opts });
const context = await browser.newContext();
const page = await context.newPage();
const t0 = Date.now();
const stamp = () => `+${((Date.now() - t0) / 1000).toFixed(1)}s`;
const logs = [];
page.on("console", (m) => logs.push(`${stamp()} console.${m.type()}: ${m.text()}`));
page.on("pageerror", (e) => logs.push(`${stamp()} pageerror: ${e.message}`));
page.on("worker", (w) => w.on("console", (m) => logs.push(`${stamp()} worker console.${m.type()}: ${m.text()}`)));
page.on("requestfailed", (r) => logs.push(`${stamp()} requestfailed: ${r.method()} ${r.url()} ${r.failure()?.errorText}`));
await page.goto(url);
let done = false;
const deadline = Date.now() + Number(timeoutSec) * 1000;
while (!done && Date.now() < deadline) {
  await new Promise((r) => setTimeout(r, 500));
  done = (await page.title()) === "e2e-done";
}
const out = await page.evaluate(() => document.getElementById("out")?.textContent);
console.log(`=== RESULT (done=${done}, ${stamp()}) ${engine} ${browser.version()} ===\n${out}`);
console.log("=== CONSOLE / LOG ===\n" + logs.join("\n"));
await browser.close();
process.exit(done && /SUMMARY passed=\d+ failed=0/.test(out) ? 0 : 1);
