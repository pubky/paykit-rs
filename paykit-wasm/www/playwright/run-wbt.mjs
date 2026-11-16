// THROWAWAY (#411): open a wasm-bindgen-test-runner NO_HEADLESS page in a
// Playwright engine and print the test output.
// Usage: node run-wbt.mjs <chromium|firefox|webkit> <url> [timeoutSec]
import { chromium, firefox, webkit } from "playwright";
const [,, engine, url, timeoutSec = "180"] = process.argv;
const types = { chromium, firefox, webkit };
const opts = engine === "chromium" ? { executablePath: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" } : {};
const browser = await types[engine].launch({ headless: true, ...opts });
const page = await browser.newPage();
const logs = [];
page.on("console", (m) => logs.push(`console.${m.type()}: ${m.text()}`));
page.on("pageerror", (e) => logs.push(`pageerror: ${e.message}`));
await page.goto(url);
const deadline = Date.now() + Number(timeoutSec) * 1000;
let text = "";
while (Date.now() < deadline) {
  text = await page.evaluate(() => (document.getElementById("output") || document.body).textContent);
  if (/test result: (ok|FAILED)/.test(text)) break;
  await new Promise((r) => setTimeout(r, 500));
}
console.log(`=== ${engine} ${browser.version()} ===`);
console.log(text);
if (!/test result: ok/.test(text)) console.log("=== console ===\n" + logs.join("\n"));
await browser.close();
process.exit(/test result: ok/.test(text) ? 0 : 1);
